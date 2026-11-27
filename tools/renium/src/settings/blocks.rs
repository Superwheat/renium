//! Store version 12: a store in sections that are each their own compressed
//! frame, so that reading the instances, reading a few rows or editing the
//! values of a few rows decompresses and rewrites only what it needs.
//!
//! A file holds the magic, the version byte, a head, the digest of all that,
//! and then the frames the head lists, in its order:
//! - the structure: the v11 payload of the instances without their values
//!   (ids, names, classes and parents), read as v11 reads it;
//! - the pool: every string of `POOL_MIN_BYTES` or more that a block names,
//!   once for the store, rising by the 16-byte hash of its text (the first
//!   half of its SHA-256). Each bucket of `POOL_BUCKET_BITS` leading hash bits
//!   that holds an entry shorter than `POOL_ALONE_BYTES` has one frame of
//!   those entries, followed by a frame for each of its longer entries. An
//!   entry is its length shifted left by one, the low bit set where it is
//!   held as the bytes of a base64 text that gives back exactly that text,
//!   then those bytes or the text;
//! - the value blocks, one for each run of consecutive rows: a string table
//!   of the texts shorter than `POOL_MIN_BYTES` by count and text, then the
//!   hash of each pool entry the block names, rising; the property names,
//!   short ones first in the same way; and the v11 property groups of those
//!   rows over row indices that start at 0 in each block.
//!
//! The head gives, as varints, the writer's revision and zstd version, then
//! each frame's content length and length, followed by the 16-byte digest of
//! its bytes: the structure's; the number of pool entries and, for each, its
//! hash and the number of blocks that name it shifted left by one, the low
//! bit set where it is a frame of its own; the pool frames in order; the
//! number of blocks and, for each, its row count and its frame. Every frame
//! carries zstd's checksum of its content and its length. A frame is read and
//! copied only where its bytes match their digest, so a damaged frame fails a
//! read and an edit alike, and a pool entry is read only where its text
//! matches its hash. A reference to an instance is its index in the store,
//! as in v11.
//!
//! Left as they are, since only a crafted file or text makes them matter:
//! the counts are not checked against the blocks; a text an edit brings whose
//! hash a pool entry it reads as a marker already has keeps that entry's
//! text; and a file that forges the writer's tag has its frames copied as
//! they stand. A zstd update changes the tag even where it gives the same
//! bytes, so the first write after one is whole.
//!
//! Every store is written in this version, and a document has one file: each
//! block's bytes come from its own rows, and the pool from the strings the
//! blocks name, neither depending on any text but the block's own short
//! ones. A writer that holds a file the same writer wrote copies each frame
//! of it that comes out the same instead of compressing it again. An edit of
//! some rows reads their blocks with a marker for each pool entry, writes
//! them, the frames whose entries changed and the counts in the head, which
//! is the file a whole encode of the edited document gives. An edit that
//! adds rows behind the last writes the blocks they fall in beside those; one
//! that takes rows out or puts them in another order writes every block from
//! the blocks that held its rows, a few at a time, and never holds the
//! document whole. A read of some
//! rows reads only the pool entries their values name. The encoder of
//! documents and the encoder of captured states both write it here, from the
//! same collections they wrote v11 from, so the two write the same bytes
//! where they did. The readers of `bytecode`, `property_edit` and
//! `row_chunks` hand a v12 file to this module and give what they give for
//! the v11 file of the same document.

use std::collections::BTreeMap;
use std::io::Read;
use std::ops::Range;
use std::sync::{LazyLock, OnceLock};

use super::captured::{Moved, half_digest};
use super::property_edit::{StoreRows, names_an_instance_by_id};
use super::*;
use crate::settings::equivalence::settle_forced_properties;
use crate::settings::instance::remap_ref_indices_in_value;

#[cfg(test)]
#[path = "blocks_sim.rs"]
pub(crate) mod sim;

#[path = "bulk_diff.rs"]
pub(crate) mod bulk_diff;

/// The rows of a value block; the last block holds what is left.
pub(super) const BLOCK_ROWS: usize = 4096;
/// From this length on a string is held in the pool, and below it in the
/// table of each block that holds it.
pub(super) const POOL_MIN_BYTES: usize = 256;
/// The leading bits of an entry's hash that give the bucket, and so the pool
/// frame, it is written in.
const POOL_BUCKET_BITS: u32 = 6;
/// From this length on a pool entry is a frame of its own, so that a
/// bucket's frame stays small however large the entries of the pool are.
const POOL_ALONE_BYTES: usize = 8 << 10;
/// The revision of this writer, raised whenever it gives a document other
/// bytes. A writer copies frames only from a file of its own revision and
/// zstd version, since another compressor may give one content other bytes.
const WRITER_REVISION: u64 = 1;
/// The content a frame's read may reserve before it comes out: as much as its
/// head gives, up to `FRAME_READ_RATIO` times its bytes or `FRAME_READ_START`,
/// whichever is more; a read that needs more grows with what comes out.
/// zstd's own buffer is at most the content, which a frame must state as its
/// head does.
const FRAME_READ_START: usize = 1 << 20;
const FRAME_READ_RATIO: usize = 64;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
/// The blocks an edit reads at once to write them or to learn whether they
/// come out otherwise.
const RESTRUCTURED_BATCH: usize = 4;
/// The blocks an edit reads at once to learn whether they stand as they are,
/// each read to its table and references alone.
const STANDING_BATCH: usize = 16;
/// The kinds of the property groups whose values may hold a reference by
/// position: arrays and objects, which nest values of any kind, and the two
/// kinds of reference.
const REFERRING_KINDS: [u8; 4] = [7, 8, 10, 17];
/// The content bytes the frames read in this process hold.
#[cfg(test)]
static DECOMPRESSED: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
thread_local! {
    /// The stores in blocks this thread decoded whole.
    pub(crate) static WHOLE_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The start of the text a decode gives a pool entry in place of its own: no
/// file can hold it, since every process draws it anew.
static MARK: LazyLock<String> = LazyLock::new(|| {
    let draw = |seed: u8| ahash::RandomState::new().hash_one(seed);
    format!("\u{1}pool:{:016x}{:016x}:", draw(1), draw(2))
});

/// The text that stands for the pool entry of hash `hash`.
fn marker(hash: &[u8; 16]) -> String {
    format!("{}{:032x}", *MARK, u128::from_be_bytes(*hash))
}

/// The hash of the pool entry `text` stands for, where it is a marker.
#[inline]
fn marked(text: &str) -> Option<[u8; 16]> {
    if text.as_bytes().first() != Some(&1) {
        return None;
    }
    let hex = text.strip_prefix(MARK.as_str())?;
    (hex.len() == 32)
        .then(|| u128::from_str_radix(hex, 16).ok())
        .flatten()
        .map(u128::to_be_bytes)
}

/// This writer's revision and the version of zstd it compresses with.
fn writer() -> [u64; 2] {
    [
        WRITER_REVISION,
        u64::from(zstd::zstd_safe::version_number()),
    ]
}

/// How many instances a store in blocks holds, from its header alone.
pub(crate) fn instance_count(bytes: &[u8]) -> Option<usize> {
    BlockStore::open(bytes).ok().map(|store| store.rows)
}

/// The bytes of a store file that hold its instances and values: past the
/// head of a v12 store, which grows with its blocks and pool entries.
pub(crate) fn store_payload_len(bytes: &[u8]) -> usize {
    match BlockStore::open(bytes) {
        Ok(store) => bytes.len() - store.structure.at,
        _ => bytes.len(),
    }
}

/// Whether a file is a v12 store.
pub(crate) fn is_block_store(bytes: &[u8]) -> bool {
    bytes
        .strip_prefix(SETTINGS_BINARY_MAGIC)
        .and_then(<[u8]>::first)
        == Some(&SETTINGS_BINARY_VERSION)
}

/// The first half of the SHA-256 of `parts`, one after another.
pub(super) fn digest(parts: &[&[u8]]) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part);
    }
    half_digest(digest)
}

const SHARDED_MAP_SHARDS: usize = 64;
const SHARDED_MAP_HASHER: ahash::RandomState = ahash::RandomState::with_seeds(
    0x517c_c1b7_2722_0a95,
    0x9e37_79b9_7f4a_7c15,
    0xbf58_476d_1ce4_e5b9,
    0x94d0_49bb_1331_11eb,
);

pub(super) struct ShardedMap<K, V> {
    shards: [std::sync::RwLock<AHashMap<K, V>>; SHARDED_MAP_SHARDS],
}

impl<K: Eq + std::hash::Hash, V: Copy> ShardedMap<K, V> {
    pub(super) fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| std::sync::RwLock::new(AHashMap::new())),
        }
    }

    #[inline]
    fn shard(&self, key: &K) -> &std::sync::RwLock<AHashMap<K, V>> {
        &self.shards[(SHARDED_MAP_HASHER.hash_one(key) as usize) & (SHARDED_MAP_SHARDS - 1)]
    }

    #[inline]
    pub(super) fn get(&self, key: &K) -> Option<V> {
        self.shard(key).read().unwrap().get(key).copied()
    }

    #[inline]
    pub(super) fn insert(&self, key: K, value: V) {
        self.shard(&key).write().unwrap().insert(key, value);
    }
}

#[derive(Clone, Copy)]
struct DigestBlockOpts<'a> {
    whole: bool,
    kept: Option<usize>,
    moved: Option<&'a Moved>,
    database: Option<&'a rbx_reflection::ReflectionDatabase<'a>>,
}

type StreamedRowDigest = ([u8; 16], [u8; 16], [u8; 16], Option<usize>);
type BlockGroupSpec<'a> = (&'a str, u8, usize, &'a [u8]);
type FastCompareOutcome = Option<(super::captured::ComparedPair, (bool, bool))>;

struct StreamDigestState {
    hashers: Vec<super::captured::RowHasher>,
    unique_ids: Vec<[u8; 16]>,
    attr_digests: Vec<[u8; 16]>,
    engine_hashers: Vec<Option<super::captured::RowHasher>>,
    named_flags: Vec<bool>,
    multi_slot: Vec<Option<[u8; 16]>>,
    string_digests: Vec<Option<[u8; 16]>>,
    enum_digests: Vec<Option<[u8; 16]>>,
    pair_obj_digests: AHashMap<(usize, usize, usize, usize), [u8; 16]>,
}

fn parse_block_group_specs<'a>(
    open: &'a OpenBlock,
    prop_names: &[&'a str],
) -> Result<(Vec<BlockGroupSpec<'a>>, Vec<usize>, Vec<usize>)> {
    let mut reader = BytecodeReader::new(&open.content[open.rest..]);
    let group_count = reader.read_collection_len("property group count")?;
    let mut specs: Vec<BlockGroupSpec<'a>> = Vec::with_capacity(reader.capacity(group_count, 4));
    for _ in 0..group_count {
        let property_id = reader.read_len("property id")?;
        let property_name = *prop_names
            .get(property_id)
            .with_context(|| format!("Invalid property id {property_id}"))?;
        let kind = reader.read_u8().context("Missing property kind")?;
        let value_count = reader.read_collection_len("property value count")?;
        let body_len = reader.read_len("property group byte length")?;
        let body = reader.read_bytes(body_len)?;
        if !SETTINGS_RECORD_FIELDS.contains(&property_name) {
            specs.push((property_name, kind, value_count, body));
        }
    }
    reader.finish()?;
    let mut attr_groups = Vec::new();
    let mut prop_order = Vec::with_capacity(specs.len());
    for (idx, &(name, kind, ..)) in specs.iter().enumerate() {
        if name == "Attributes" && kind == 8 {
            attr_groups.push(idx);
        } else {
            prop_order.push(idx);
        }
    }
    prop_order.sort_unstable_by(|&a, &b| specs[a].0.cmp(specs[b].0).then_with(|| a.cmp(&b)));
    Ok((specs, attr_groups, prop_order))
}

fn record_row_property_digest(
    state: &mut StreamDigestState,
    row: usize,
    class_name: &str,
    name: &str,
    is_unique_id: bool,
    val_digest: [u8; 16],
    opts: DigestBlockOpts<'_>,
) {
    use super::captured::{RowHasher, is_digested_with_db};
    use crate::settings::equivalence::digest_text;
    if is_unique_id {
        state.unique_ids[row] = val_digest;
    } else if opts.whole || is_digested_with_db(opts.database, class_name, name) {
        state.hashers[row].update(&[1]);
        digest_text(&mut state.hashers[row], name);
        state.hashers[row].update(&val_digest);
    } else {
        let eh = state.engine_hashers[row].get_or_insert_with(RowHasher::new);
        eh.update(&[1]);
        digest_text(eh, name);
        eh.update(&val_digest);
    }
}

fn try_digest_string_array_wire(
    reader: &mut BytecodeReader<'_>,
    str_items: &[super::captured::StrOrHash<'_>],
    charged: &mut usize,
) -> Option<[u8; 16]> {
    use super::captured::{RowHasher, StrOrHash};
    use crate::settings::equivalence::digest_text;
    let arr_len = reader.read_collection_len("array length").ok()?;
    let mut h = RowHasher::new();
    h.update(&[6]);
    h.update(&(arr_len as u64).to_le_bytes());
    let mut cost = arr_len.saturating_mul(VALUE_SLOT_BYTES);
    for _ in 0..arr_len {
        if reader.read_u8().ok()? != 6 {
            return None;
        }
        let id = reader.read_len("value string id").ok()?;
        let item = *str_items.get(id)?;
        h.update(&[5]);
        match item {
            StrOrHash::Text(t) => {
                digest_text(&mut h, t);
                cost = cost.saturating_add(t.len());
            }
            StrOrHash::Hash(hash) => {
                h.update_long_hash(&hash);
                cost = cost.saturating_add(16);
            }
        }
    }
    *charged = charged.saturating_add(cost);
    Some(h.finish())
}

fn try_digest_two_string_object_wire(
    reader: &mut BytecodeReader<'_>,
    str_items: &[super::captured::StrOrHash<'_>],
    whole: bool,
    charged: &mut usize,
    pair_obj_digests: &mut AHashMap<(usize, usize, usize, usize), [u8; 16]>,
) -> Option<[u8; 16]> {
    use super::captured::{StrOrHash, digest_two_string_object_with_hashes};
    let field_count = reader.read_collection_len("object field count").ok()?;
    if field_count != 2 {
        return None;
    }
    let k0_id = reader.read_len("object key string id").ok()?;
    if reader.read_u8().ok()? != 6 {
        return None;
    }
    let v0_id = reader.read_len("value string id").ok()?;
    let k1_id = reader.read_len("object key string id").ok()?;
    if reader.read_u8().ok()? != 6 {
        return None;
    }
    let v1_id = reader.read_len("value string id").ok()?;
    let StrOrHash::Text(k0) = *str_items.get(k0_id)? else {
        return None;
    };
    let StrOrHash::Text(k1) = *str_items.get(k1_id)? else {
        return None;
    };
    let v0 = *str_items.get(v0_id)?;
    let v1 = *str_items.get(v1_id)?;
    if whole
        && (matches!(k0, "settingsId" | "instanceId" | "debugId")
            || matches!(k1, "settingsId" | "instanceId" | "debugId"))
    {
        return None;
    }
    let v0_len = match v0 {
        StrOrHash::Text(t) => t.len(),
        StrOrHash::Hash(_) => 16,
    };
    let v1_len = match v1 {
        StrOrHash::Text(t) => t.len(),
        StrOrHash::Hash(_) => 16,
    };
    *charged = charged.saturating_add(2 * VALUE_SLOT_BYTES + k0.len() + v0_len + k1.len() + v1_len);
    Some(
        *pair_obj_digests
            .entry((k0_id, v0_id, k1_id, v1_id))
            .or_insert_with(|| digest_two_string_object_with_hashes(k0, v0, k1, v1)),
    )
}

fn skip_group_value_fast(
    reader: &mut BytecodeReader<'_>,
    kind: u8,
    cframe: &CFrameGroupDecoder,
    total_rows: usize,
    prev_ref: &mut usize,
) -> Result<()> {
    match kind {
        0..=2 => {}
        3 | 4 | 6 | 19 => {
            reader.read_var_u64()?;
        }
        5 | 11 | 13 => {
            reader.read_bytes(8)?;
        }
        20 => {
            reader.read_bytes(4)?;
        }
        12 | 15 => {
            reader.read_bytes(12)?;
        }
        14 | 18 => {
            reader.read_bytes(16)?;
        }
        16 => match cframe {
            CFrameGroupDecoder::Inline => {
                reader.read_bytes(48)?;
            }
            CFrameGroupDecoder::RotationTable(_) => {
                reader.read_bytes(12)?;
                reader.read_var_u64()?;
            }
        },
        17 => {
            let _ = decode_resolved_ref_target(reader, total_rows, prev_ref)?;
        }
        _ => skip_raw_value_payload_fast(reader, kind, 0)?,
    }
    Ok(())
}

/// The bucket of the pool that holds the entry of hash `hash`.
fn bucket(hash: &[u8; 16]) -> u8 {
    hash[0] >> (8 - POOL_BUCKET_BITS)
}

/// Where a frame lies in the file, the length of its content, and the digest
/// of its bytes.
#[derive(Clone, Copy)]
struct Frame {
    at: usize,
    len: usize,
    decoded: usize,
    digest: [u8; 16],
}

/// A frame of the pool and its entries, in order.
struct PoolFrame {
    frame: Frame,
    entries: Vec<usize>,
}

/// The entries of each pool frame, in the order of the frames: for each
/// bucket, a frame of its entries that are not `alone`, then a frame for
/// each that is.
fn frame_plan(hashes: &[[u8; 16]], alone: &[bool]) -> Vec<Vec<usize>> {
    let mut plan = Vec::new();
    let mut first = 0;
    while first < hashes.len() {
        let held = bucket(&hashes[first]);
        let end = first + hashes[first..].partition_point(|hash| bucket(hash) == held);
        let (single, shared): (Vec<_>, Vec<_>) = (first..end).partition(|entry| alone[*entry]);
        plan.extend((!shared.is_empty()).then_some(shared));
        plan.extend(single.into_iter().map(|entry| vec![entry]));
        first = end;
    }
    plan
}

/// A value block: `count` rows from the row `first` on.
struct Block {
    frame: Frame,
    first: usize,
    count: usize,
}

/// The pool frames a reader read, each by its place among the frames.
type PoolRead = Vec<Option<Arc<Vec<String>>>>;

/// A v12 store file with its head read. Each read decompresses only the
/// frames it needs, and a pool frame at most once.
pub(crate) struct BlockStore<'a> {
    bytes: Cow<'a, [u8]>,
    writer: [u64; 2],
    structure: Frame,
    pool: Vec<PoolFrame>,
    /// The hash of each pool entry, rising, the number of blocks that name
    /// it, whether it is a frame of its own, and its frame and place there.
    hashes: Vec<[u8; 16]>,
    counts: Vec<usize>,
    alone: Vec<bool>,
    slots: Vec<(usize, usize)>,
    blocks: Vec<Block>,
    rows: usize,
    budget: DecodeBudget,
    read_pool: Vec<Mutex<Option<Arc<Vec<String>>>>>,
    /// The most a read of one frame reserved.
    #[cfg(test)]
    reserved: AtomicUsize,
}

/// A value block's content with its string table and the ids of its
/// property names read; the property groups begin at `rest`.
struct OpenBlock {
    content: Vec<u8>,
    strings: Vec<BlockString>,
    properties: Vec<usize>,
    rest: usize,
}

enum BlockString {
    Own(String),
    Pool(usize),
}

/// A frame as it is written: the length of its content, the digest of its
/// bytes, and the bytes.
struct Section<'b> {
    decoded: usize,
    digest: [u8; 16],
    bytes: Cow<'b, [u8]>,
}

/// The pool a writer writes: each entry by its hash, with the number of
/// blocks that name it and its text where the writer holds that.
type PoolEntries<'t> = BTreeMap<[u8; 16], (usize, Option<&'t str>)>;

/// The blocks an edit writes, each where it is written anew, with the count
/// of each pool entry they leave and the text of each entry they bring.
type RewrittenBlocks<'s> = (
    BTreeMap<[u8; 16], usize>,
    BTreeMap<[u8; 16], String>,
    Vec<Option<Section<'s>>>,
);

/// A value block's strings, property names and property groups, in the
/// order the block holds them. A long string, or a marker, stands at the
/// table entry of its hash, as each other text of that hash does.
struct Prepared<'a> {
    rows: usize,
    strings: Table<'a>,
    properties: Table<'a>,
    groups: Vec<((&'a str, u8), Vec<SettingsBinaryValue<'a>>)>,
}

/// The texts of a table: the short ones by count and text, then one for each
/// hash, rising; and each other text that stands at one of those places.
struct Table<'a> {
    texts: Vec<SettingsString<'a>>,
    short: usize,
    hashes: Vec<[u8; 16]>,
    aliases: Vec<(SettingsString<'a>, usize)>,
}

/// The texts a decode of a block gives the pool entries the block names.
#[derive(Clone, Copy)]
enum Texts<'r> {
    /// Each entry's text, all of them read.
    All,
    /// A marker for each entry.
    Marked,
    /// A marker for each entry, then in the rows a read gives, the text of
    /// each entry the function takes.
    Resolved(&'r (dyn Fn(&[u8; 16]) -> bool + Sync)),
}

/// Where the instances of an edited document stand beside the rows of the
/// store it was read from.
#[derive(Clone, Copy)]
struct Placed<'p> {
    /// The row each instance was read at, none for one the edit made.
    from: &'p [Option<usize>],
    /// Where each row stands now, none for one the edit took out.
    to: &'p [Option<usize>],
    /// The instances whose values the document holds.
    held: &'p [bool],
}

/// What `BlockStore::rewrite` holds for every block it writes.
struct Rewrite<'r> {
    /// The edited document, settled.
    document: &'r SettingsBytecode,
    placed: Placed<'r>,
    /// The rows of each block of the file it writes.
    ranges: Vec<Range<usize>>,
    /// Whether every row stays at its place, in the block that held it.
    kept: bool,
    /// Whether every block is written from its rows, as a whole encode
    /// writes it, and none taken as it stands without a read.
    whole: bool,
    /// Whether, where rows moved, a block whose rows all stand where they
    /// stood is copied as it is (`BlockStore::standing`) rather than written
    /// from its rows, which gives its values the form a whole encode does.
    standing: bool,
    moved: AHashSet<&'r str>,
    /// The hash of each long string a reference may name an instance by.
    long: AHashSet<[u8; 16]>,
    dense: SettingsBinaryInstanceLookup<'r>,
    every: OnceLock<SettingsBinaryInstanceLookup<'r>>,
}

impl<'r> Rewrite<'r> {
    /// The lookup that finds the instances the references of `rows` name:
    /// by their positions alone where none of them needs another selector.
    fn lookup(&self, rows: &[SettingsBytecodeInstance]) -> &SettingsBinaryInstanceLookup<'r> {
        let selectors =
            settings_instance_reference_selectors(rows, self.dense.root_service.as_deref());
        match selectors.settings_id || selectors.path {
            true => (self.every)
                .get_or_init(|| build_bytecode_instance_lookup_by_every_selector(self.document)),
            false => &self.dense,
        }
    }
}

/// A block as `BlockStore::rewrite` wrote it.
struct Written<'s> {
    section: Section<'s>,
    /// The pool entries the block the store held at its place named.
    before: Vec<usize>,
    /// The hash of each pool entry it names, with the text of one the store
    /// does not hold.
    now: Vec<([u8; 16], Option<String>)>,
}

/// `document` as a store file, its references found by every selector its
/// values use or, where `by_selectors` says otherwise, by instanceIndex
/// alone, over `previous` where the store's file holds that. It refuses what
/// the v11 encoder refused.
pub(super) fn encode_document(
    document: &SettingsBytecode,
    by_selectors: bool,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    encode_document_in_blocks(document, by_selectors, previous, BLOCK_ROWS)
}

pub(super) fn encode_settled_document(
    document: &SettingsBytecode,
    by_selectors: bool,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let lookup = document_reference_lookup(document, by_selectors);
    encode_with_lookup(document, &lookup, previous, BLOCK_ROWS)
}

fn encode_document_in_blocks(
    document: &SettingsBytecode,
    by_selectors: bool,
    previous: Option<&[u8]>,
    block_rows: usize,
) -> Result<Vec<u8>> {
    let document = settled_settings_document(document)?;
    let lookup = document_reference_lookup(&document, by_selectors);
    encode_with_lookup(&document, &lookup, previous, block_rows)
}

/// A settled document as a store file in blocks of `block_rows` rows, its
/// references found by `lookup`, over `previous` where the store's file
/// holds that.
pub(super) fn encode_with_lookup(
    document: &SettingsBytecode,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    previous: Option<&[u8]>,
    block_rows: usize,
) -> Result<Vec<u8>> {
    let ranges = block_ranges(document.instances.len(), block_rows);
    let owned_entries = Mutex::new(BTreeMap::<[u8; 16], (usize, Option<String>)>::new());
    let (structure, contents) = rayon::join(
        || structure_content(document, lookup),
        || {
            ranges
                .par_iter()
                .map(|range| {
                    let block = prepared(&document.instances[range.clone()], lookup)?;
                    let content = block.content(lookup)?;
                    record_owned_entries(&owned_entries, &block)?;
                    Ok((block.rows, content))
                })
                .collect::<Result<Vec<_>>>()
        },
    );
    let owned_entries = owned_entries
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    encode_contents(structure?, contents?, &owned_entries, previous)
}

/// Optional hints from `changed_rows` so `encode_state_with_hint` can reuse the
/// already-opened previous store, copy unchanged structure/value blocks verbatim,
/// and avoid re-decompressing unchanged blocks just to count their pool entries.
pub(super) struct EncodeStateHint<'a> {
    pub(super) previous: Option<&'a BlockStore<'a>>,
    pub(super) structure_unchanged: bool,
    pub(super) unchanged_blocks: Option<&'a [bool]>,
    pub(super) block_pool_entries: Option<&'a [Vec<usize>]>,
    pub(super) long_hashes: Option<&'a ShardedMap<(usize, usize), [u8; 16]>>,
}

/// The store of a captured state whose instances are `records`, with the
/// values of `replaced` in place of its instance's, over `previous` where the
/// store's file holds that.
pub(super) fn encode_state<'a>(
    state: &'a ServiceState,
    records: (
        &'a [SettingsBinaryInstance<'a>],
        &SettingsBinaryInstanceLookup<'_>,
    ),
    replaced: Option<ReplacedRow<'a>>,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    encode_state_in_blocks_with_hint(state, records, replaced, previous, None, BLOCK_ROWS)
}

pub(super) fn encode_state_with_hint<'a>(
    state: &'a ServiceState,
    records: (
        &'a [SettingsBinaryInstance<'a>],
        &SettingsBinaryInstanceLookup<'_>,
    ),
    replaced: Option<ReplacedRow<'a>>,
    previous: Option<&[u8]>,
    hint: EncodeStateHint<'_>,
) -> Result<Vec<u8>> {
    encode_state_in_blocks_with_hint(state, records, replaced, previous, Some(hint), BLOCK_ROWS)
}

#[cfg(test)]
fn encode_state_in_blocks<'a>(
    state: &'a ServiceState,
    records: (
        &'a [SettingsBinaryInstance<'a>],
        &SettingsBinaryInstanceLookup<'_>,
    ),
    replaced: Option<ReplacedRow<'a>>,
    previous: Option<&[u8]>,
    block_rows: usize,
) -> Result<Vec<u8>> {
    encode_state_in_blocks_with_hint(state, records, replaced, previous, None, block_rows)
}

fn encode_state_in_blocks_with_hint<'a>(
    state: &'a ServiceState,
    (records, lookup): (
        &'a [SettingsBinaryInstance<'a>],
        &SettingsBinaryInstanceLookup<'_>,
    ),
    replaced: Option<ReplacedRow<'a>>,
    previous: Option<&[u8]>,
    hint: Option<EncodeStateHint<'_>>,
    block_rows: usize,
) -> Result<Vec<u8>> {
    let owned_prev = if hint.as_ref().and_then(|h| h.previous).is_some() {
        None
    } else {
        previous
            .and_then(|bytes| BlockStore::open(bytes).ok())
            .filter(|store| store.writer == writer())
    };
    let prev_store = hint
        .as_ref()
        .and_then(|h| h.previous)
        .filter(|store| store.writer == writer())
        .or(owned_prev.as_ref());

    if let Some(store) = prev_store
        && let Ok(bytes) = encode_state_streaming(
            state,
            (records, lookup),
            replaced,
            Some(store),
            hint.as_ref(),
            block_rows,
        )
    {
        return Ok(bytes);
    }
    encode_state_streaming(state, (records, lookup), replaced, None, None, block_rows)
}

type BlockEncoded<'a, 's> = (
    usize,
    Section<'s>,
    Vec<([u8; 16], Option<SettingsString<'a>>)>,
);

/// Streams `encode_state` block by block: each block collects, builds its table,
/// serializes its content, drops its intermediate tables, and compresses/copies its
/// frame before moving on, bounding peak memory and reusing zstd contexts per worker.
fn encode_state_streaming<'a, 's>(
    state: &'a ServiceState,
    (records, lookup): (
        &'a [SettingsBinaryInstance<'a>],
        &SettingsBinaryInstanceLookup<'_>,
    ),
    replaced: Option<ReplacedRow<'a>>,
    previous: Option<&'s BlockStore<'s>>,
    hint: Option<&EncodeStateHint<'_>>,
    block_rows: usize,
) -> Result<Vec<u8>> {
    let _total_span = crate::app::profile::span("encode_state total");
    let ranges = block_ranges(records.len(), block_rows);
    let structure_unchanged = previous.is_some() && hint.is_some_and(|h| h.structure_unchanged);
    let unchanged_blocks = previous
        .filter(|_| hint.is_some())
        .and_then(|_| hint.and_then(|h| h.unchanged_blocks));
    let block_pool_entries = hint.and_then(|h| h.block_pool_entries);
    let owned_long_hashes;
    let long_hashes: &ShardedMap<(usize, usize), [u8; 16]> = match hint.and_then(|h| h.long_hashes)
    {
        Some(existing) => existing,
        None => {
            owned_long_hashes = ShardedMap::new();
            &owned_long_hashes
        }
    };

    let encode_blocks = || -> Result<Vec<BlockEncoded<'a, 's>>> {
        ranges
            .par_iter()
            .enumerate()
            .map_init(FrameCodec::new, |codec, (at, range)| {
                if let Some(store) = previous
                    && unchanged_blocks.and_then(|u| u.get(at).copied()) == Some(true)
                    && let Some(prev_block) = store.blocks.get(at)
                    && prev_block.rows() == *range
                {
                    let section = store.raw(prev_block.frame)?;
                    let pooled = if let Some(entries) = block_pool_entries.and_then(|e| e.get(at)) {
                        entries
                            .iter()
                            .map(|&entry| (store.hashes[entry], None))
                            .collect()
                    } else if store.hashes.is_empty() {
                        Vec::new()
                    } else {
                        let open = store.open_block(at)?;
                        open.pool_entries()
                            .map(|entry| (store.hashes[entry], None))
                            .collect()
                    };
                    return Ok((range.len(), section, pooled));
                }

                let collected = collect_block_record_values_fast(
                    state,
                    lookup,
                    &records[range.clone()],
                    replaced,
                )?;
                let properties = Table::of_with_cache(collected.property_counts, long_hashes)?;
                let property_ids = properties.ids();
                let groups =
                    sorted_settings_property_groups(collected.property_groups, &property_ids);
                let strings = Table::of_with_cache(collected.string_counts, long_hashes)?;
                let string_ids = strings.ids();

                let mut content = Vec::with_capacity(range.len().saturating_mul(64));
                push_var_u64(&mut content, strings.texts.len() as u64);
                for text in &strings.texts[..strings.short] {
                    push_var_u64(&mut content, (text.len() as u64) << 1);
                    content.extend_from_slice(text.as_bytes());
                }
                for hash in &strings.hashes {
                    push_var_u64(&mut content, 1);
                    content.extend_from_slice(hash);
                }
                push_var_u64(&mut content, properties.texts.len() as u64);
                for name in &properties.texts {
                    let id = *string_ids
                        .get(name.as_ref())
                        .with_context(|| format!("Missing binary string id for {name:?}"))?;
                    push_var_u64(&mut content, id);
                }
                write_settings_binary_property_groups_serial(
                    &mut content,
                    &groups,
                    &property_ids,
                    &string_ids,
                    lookup,
                    &mut codec.body_scratch,
                )?;

                drop(string_ids);
                drop(property_ids);
                drop(properties);
                drop(groups);
                let pooled = strings
                    .texts
                    .into_iter()
                    .skip(strings.short)
                    .zip(strings.hashes)
                    .map(|(text, hash)| {
                        let held = marked(&text).is_none().then_some(text);
                        (hash, held)
                    })
                    .collect::<Vec<_>>();

                let held = previous.and_then(|store| Some((store, store.blocks.get(at)?.frame)));
                let section = framed_with(&content, held, codec)?;
                drop(content);
                Ok((range.len(), section, pooled))
            })
            .collect()
    };

    let (structure_section, encoded_blocks) = if structure_unchanged {
        let store = previous.expect("structure_unchanged requires previous store");
        (store.raw(store.structure), encode_blocks())
    } else {
        rayon::join(
            || -> Result<Section<'s>> {
                let collected = collected_records(records, collect_settings_binary_record_strings);
                let content = encode_prepared_service_payload(records, lookup, collected)?;
                let mut codec = FrameCodec::new();
                framed_with(
                    &content,
                    previous.map(|store| (store, store.structure)),
                    &mut codec,
                )
            },
            encode_blocks,
        )
    };
    let structure_section = structure_section?;
    let encoded_blocks = encoded_blocks?;

    let _pool_span = crate::app::profile::span("encode_state pool+assemble");
    let mut owned_entries: BTreeMap<[u8; 16], (usize, Option<SettingsString<'a>>)> =
        BTreeMap::new();
    let mut blocks = Vec::with_capacity(encoded_blocks.len());
    for (rows, section, pooled) in encoded_blocks {
        for (hash, text) in pooled {
            let (count, held) = owned_entries.entry(hash).or_default();
            *count += 1;
            anyhow::ensure!(
                held.as_deref()
                    .zip(text.as_deref())
                    .is_none_or(|(held, text)| held == text),
                "Two strings of a store share a hash"
            );
            if held.is_none() {
                *held = text;
            }
        }
        blocks.push((rows, section));
    }

    let entries: PoolEntries<'_> = owned_entries
        .iter()
        .map(|(hash, (count, text))| (*hash, (*count, text.as_deref())))
        .collect();
    let listed = listed(&entries, previous)?;
    let pool = pool_frames(&entries, &listed, previous)?;
    assemble(structure_section, &listed, &pool, &blocks)
}

type OwnedPoolEntries = BTreeMap<[u8; 16], (usize, Option<String>)>;

fn record_owned_entries(target: &Mutex<OwnedPoolEntries>, block: &Prepared<'_>) -> Result<()> {
    let mut guard = target.lock_recover();
    for (hash, text) in block.strings.pooled() {
        let (count, held) = guard.entry(hash).or_default();
        *count += 1;
        anyhow::ensure!(
            held.as_deref()
                .zip(text)
                .is_none_or(|(held, text)| held == text),
            "Two strings of a store share a hash"
        );
        if held.is_none()
            && let Some(text) = text
        {
            *held = Some(text.to_string());
        }
    }
    Ok(())
}

fn encode_contents(
    structure: Vec<u8>,
    contents: Vec<(usize, Vec<u8>)>,
    owned_entries: &OwnedPoolEntries,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let entries: PoolEntries<'_> = owned_entries
        .iter()
        .map(|(&hash, (count, text))| (hash, (*count, text.as_deref())))
        .collect();
    let previous = previous
        .and_then(|bytes| BlockStore::open(bytes).ok())
        .filter(|store| store.writer == writer());
    if let Some(store) = &previous
        && let Ok(bytes) = written(&structure, &contents, &entries, Some(store))
    {
        return Ok(bytes);
    }
    written(&structure, &contents, &entries, None)
}

/// A provable lower bound on the compressed v12 file size of `records` and its
/// pooled long strings: the sum of the compressed structure frame and the
/// compressed pool frames (each included verbatim in the assembled v12 file).
pub(super) fn min_encoded_file_bytes<'a>(
    records: &'a [SettingsBinaryInstance<'a>],
    lookup: &SettingsBinaryInstanceLookup<'_>,
    long_strings: &[&'a str],
) -> Result<usize> {
    let _span = crate::app::profile::span("captured store lower bound");
    let (structure_bytes, pool_bytes) = rayon::join(
        || -> Result<usize> {
            let collected = collected_records(records, collect_settings_binary_record_strings);
            let raw = encode_prepared_service_payload(records, lookup, collected)?;
            Ok(compressed(&raw)?.bytes.len())
        },
        || -> Result<usize> {
            let mut entries = PoolEntries::new();
            for &text in long_strings {
                let hash = digest(&[text.as_bytes()]);
                let (count, held) = entries.entry(hash).or_default();
                *count += 1;
                anyhow::ensure!(
                    held.is_none_or(|prev| prev == text),
                    "Two strings of a store share a hash"
                );
                *held = Some(text);
            }
            if entries.is_empty() {
                return Ok(0);
            }
            let listed = listed(&entries, None)?;
            let frames = pool_frames(&entries, &listed, None)?;
            Ok(frames.iter().map(|frame| frame.bytes.len()).sum())
        },
    );
    Ok(structure_bytes?.saturating_add(pool_bytes?))
}

/// The rows of each block of a store of `count` instances.
fn block_ranges(count: usize, block_rows: usize) -> Vec<Range<usize>> {
    (0..count)
        .step_by(block_rows)
        .map(|start| start..count.min(start + block_rows))
        .collect()
}

/// The strings `count` collects of every one of `items`, collected in
/// parallel.
fn collected_records<'a, T: Sync>(
    items: &'a [T],
    count: impl Fn(&mut SettingsBinaryCollection<'a>, &'a T) + Sync,
) -> SettingsBinaryCollection<'a> {
    items
        .par_chunks(BLOCK_ROWS)
        .map(|items| {
            let _span = crate::app::profile::span("store records chunk collected");
            let mut collected = SettingsBinaryCollection::default();
            for item in items {
                count(&mut collected, item);
            }
            collected
        })
        .reduce(SettingsBinaryCollection::default, |mut all, part| {
            merge_settings_binary_collection(&mut all, part);
            all
        })
}

/// The file of a structure's content, the blocks `contents` with their row
/// counts and the pool `entries`, each frame copied from `previous` where it
/// holds the same content at the same place.
fn written(
    structure: &[u8],
    contents: &[(usize, Vec<u8>)],
    entries: &PoolEntries<'_>,
    previous: Option<&BlockStore<'_>>,
) -> Result<Vec<u8>> {
    let listed = listed(entries, previous)?;
    let (structure, (blocks, pool)) = rayon::join(
        || {
            let mut codec = FrameCodec::new();
            framed_with(
                structure,
                previous.map(|store| (store, store.structure)),
                &mut codec,
            )
        },
        || {
            rayon::join(
                || {
                    (contents.par_iter().enumerate())
                        .map_init(FrameCodec::new, |codec, (at, (rows, content))| {
                            let held = previous
                                .and_then(|store| Some((store, store.blocks.get(at)?.frame)));
                            Ok((*rows, framed_with(content, held, codec)?))
                        })
                        .collect::<Result<Vec<_>>>()
                },
                || pool_frames(entries, &listed, previous),
            )
        },
    );
    assemble(structure?, &listed, &pool?, &blocks?)
}

/// `content` as a frame: the frame `held` of its store where that holds the
/// same content, else compressed, reusing `codec`'s zstd contexts and avoiding
/// a duplicate SHA-256 verification when copying `held`.
fn framed_with<'s>(
    content: &[u8],
    held: Option<(&'s BlockStore<'_>, Frame)>,
    codec: &mut FrameCodec,
) -> Result<Section<'s>> {
    if let Some((store, frame)) = held
        && frame.decoded == content.len()
        && let Some(bytes) = store.matches_content_with(frame, content, codec)?
    {
        return Ok(Section {
            decoded: frame.decoded,
            digest: frame.digest,
            bytes: Cow::Borrowed(bytes),
        });
    }
    compressed_with(content, codec)
}

fn compressed_with(content: &[u8], codec: &mut FrameCodec) -> Result<Section<'static>> {
    let _span = crate::app::profile::span("frame compressed");
    check_settings_payload_len(content.len(), "Decoded")?;
    let bytes = codec.compress_frame(content)?;
    Ok(Section {
        decoded: content.len(),
        digest: digest(&[&bytes]),
        bytes: Cow::Owned(bytes),
    })
}

/// `content` as a frame: the frame `held` of its store where that holds the
/// same content, else compressed.
fn framed<'s>(content: &[u8], held: Option<(&'s BlockStore<'_>, Frame)>) -> Result<Section<'s>> {
    if let Some((store, frame)) = held
        && frame.decoded == content.len()
        && store.content(frame)? == content
    {
        return store.raw(frame);
    }
    compressed(content)
}

/// The v11 payload of `document`'s instances without their values.
fn structure_content(
    document: &SettingsBytecode,
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<Vec<u8>> {
    let collecting = crate::app::profile::span("store records collected");
    let records = collected_records(&document.instances, collect_settings_record_strings);
    drop(collecting);
    encode_settings_bytecode_payload_collected(document, lookup, records)
}

fn prepared<'a>(
    rows: &'a [SettingsBytecodeInstance],
    lookup: &SettingsBinaryInstanceLookup<'_>,
) -> Result<Prepared<'a>> {
    let mut collected = SettingsBinaryCollection::default();
    for (row, instance) in rows.iter().enumerate() {
        collect_settings_instance_values(&mut collected, lookup, row, instance)?;
    }
    Prepared::of(rows.len(), collected)
}

impl<'a> Table<'a> {
    /// The table of the texts `counts` counts. Two texts of one hash are the
    /// same string: one of them a marker, or else the store is refused.
    fn of(counts: SettingsStringCounts<'a>) -> Result<Self> {
        Self::of_inner(counts, None)
    }

    fn of_with_cache(
        counts: SettingsStringCounts<'a>,
        long_hashes: &ShardedMap<(usize, usize), [u8; 16]>,
    ) -> Result<Self> {
        Self::of_inner(counts, Some(long_hashes))
    }

    fn of_inner(
        counts: SettingsStringCounts<'a>,
        long_hashes: Option<&ShardedMap<(usize, usize), [u8; 16]>>,
    ) -> Result<Self> {
        let mut short = Vec::with_capacity(counts.len());
        let mut long = BTreeMap::<[u8; 16], Vec<SettingsString<'a>>>::new();
        for (text, count) in counts {
            let hash = marked(&text).or_else(|| {
                (text.len() >= POOL_MIN_BYTES).then(|| {
                    if let Some(cache) = long_hashes
                        && let Cow::Borrowed(borrowed) = text
                    {
                        let key = (borrowed.as_ptr() as usize, borrowed.len());
                        if let Some(cached) = cache.get(&key) {
                            return cached;
                        }
                        let computed = digest(&[borrowed.as_bytes()]);
                        cache.insert(key, computed);
                        return computed;
                    }
                    digest(&[text.as_bytes()])
                })
            });
            match hash {
                Some(hash) => long.entry(hash).or_default().push(text),
                None => short.push((text, count)),
            }
        }
        short.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut texts = short.into_iter().map(|(text, _)| text).collect::<Vec<_>>();
        let (short, mut hashes, mut aliases) = (texts.len(), Vec::new(), Vec::new());
        for (hash, mut held) in long {
            held.sort_unstable_by_key(|text| marked(text).is_some());
            anyhow::ensure!(
                held.get(1).is_none_or(|text| marked(text).is_some()),
                "Two strings of a store share a hash"
            );
            let place = texts.len();
            let mut held = held.into_iter();
            texts.extend(held.next());
            aliases.extend(held.map(|text| (text, place)));
            hashes.push(hash);
        }
        Ok(Table {
            texts,
            short,
            hashes,
            aliases,
        })
    }

    fn ids(&self) -> SettingsStringIdMap<'_> {
        let mut ids = build_id_map(&self.texts);
        for (text, place) in &self.aliases {
            ids.insert(text.as_ref(), *place as u64);
        }
        ids
    }

    /// Each long string and its hash, with its text where it is no marker.
    fn pooled(&self) -> impl Iterator<Item = ([u8; 16], Option<&str>)> {
        (self.texts[self.short..].iter())
            .zip(&self.hashes)
            .map(|(text, hash)| (*hash, marked(text).is_none().then_some(text.as_ref())))
    }
}

impl<'a> Prepared<'a> {
    /// A block of `rows` rows whose values are `collected`, over row indices
    /// that start at 0. A property name is never a marker.
    fn of(rows: usize, collected: SettingsBinaryCollection<'a>) -> Result<Self> {
        let properties = Table::of(collected.property_counts)?;
        let groups = sorted_settings_property_groups(collected.property_groups, &properties.ids());
        Ok(Prepared {
            rows,
            strings: Table::of(collected.string_counts)?,
            properties,
            groups,
        })
    }

    /// The block's content, each long string named by its hash.
    fn content(&self, lookup: &SettingsBinaryInstanceLookup<'_>) -> Result<Vec<u8>> {
        let string_ids = self.strings.ids();
        let property_ids = self.properties.ids();
        let mut content = Vec::new();
        write_var_u64(&mut content, self.strings.texts.len() as u64)?;
        for text in &self.strings.texts[..self.strings.short] {
            write_var_u64(&mut content, (text.len() as u64) << 1)?;
            content.extend_from_slice(text.as_bytes());
        }
        for hash in &self.strings.hashes {
            write_var_u64(&mut content, 1)?;
            content.extend_from_slice(hash);
        }
        write_var_u64(&mut content, self.properties.texts.len() as u64)?;
        for name in &self.properties.texts {
            write_binary_string_id(&mut content, &string_ids, name)?;
        }
        write_settings_binary_property_groups(
            &mut content,
            &self.groups,
            &property_ids,
            &string_ids,
            lookup,
        )?;
        Ok(content)
    }
}

/// Each entry of `entries` by its hash, with the number of blocks that name
/// it and whether it is a frame of its own: an entry `entries` gives no text
/// of is as `previous` holds it.
fn listed(
    entries: &PoolEntries<'_>,
    previous: Option<&BlockStore<'_>>,
) -> Result<Vec<([u8; 16], usize, bool)>> {
    (entries.iter())
        .map(|(hash, (count, text))| {
            let alone = match (text, previous) {
                (Some(text), _) => text.len() >= POOL_ALONE_BYTES,
                (None, Some(store)) => store.alone[store.entry_of(hash)?],
                (None, None) => bail!("A pool entry has no text to write"),
            };
            Ok((*hash, *count, alone))
        })
        .collect()
}

/// The frames of the pool `entries`, in the order `frame_plan` gives for
/// `listed`. A frame whose entries are the ones a frame of `previous` holds
/// is copied; any other is written from the texts `entries` give and, for an
/// entry they give none of, the text `previous` holds.
fn pool_frames<'s>(
    entries: &PoolEntries<'_>,
    listed: &[([u8; 16], usize, bool)],
    previous: Option<&'s BlockStore<'_>>,
) -> Result<Vec<Section<'s>>> {
    let hashes = listed.iter().map(|(hash, ..)| *hash).collect::<Vec<_>>();
    let alone = listed.iter().map(|(.., alone)| *alone).collect::<Vec<_>>();
    frame_plan(&hashes, &alone)
        .into_par_iter()
        .map_init(FrameCodec::new, |codec, planned| {
            let held = previous.and_then(|store| {
                let (frame, _) = store.slots[store.entry_of(&hashes[planned[0]]).ok()?];
                let frame = &store.pool[frame];
                let same = (frame.entries.iter().map(|entry| &store.hashes[*entry]))
                    .eq(planned.iter().map(|entry| &hashes[*entry]));
                same.then_some((store, frame.frame))
            });
            if let Some((store, frame)) = held {
                return store.raw(frame);
            }
            let mut content = Vec::new();
            for hash in planned.iter().map(|entry| &hashes[*entry]) {
                match (entries[hash].1, previous) {
                    (Some(text), _) => write_pool_entry(&mut content, text)?,
                    (None, Some(store)) => {
                        let (frame, place) = store.slots[store.entry_of(hash)?];
                        write_pool_entry(&mut content, &store.pool_frame(frame)?[place])?;
                    }
                    (None, None) => bail!("A pool entry has no text to write"),
                }
            }
            compressed_with(&content, codec)
        })
        .collect()
}

/// Appends a pool entry of `text` to a pool frame's content.
fn write_pool_entry(content: &mut Vec<u8>, text: &str) -> Result<()> {
    let looks_base64 = text.len().is_multiple_of(4)
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if looks_base64
        && let Ok(bytes) = base64::decode(text)
        && base64::encode(&bytes) == text
    {
        write_var_u64(content, ((bytes.len() as u64) << 1) | 1)?;
        content.extend_from_slice(&bytes);
    } else {
        write_var_u64(content, (text.len() as u64) << 1)?;
        content.extend_from_slice(text.as_bytes());
    }
    Ok(())
}

fn compressed(content: &[u8]) -> Result<Section<'static>> {
    let _span = crate::app::profile::span("frame compressed");
    check_settings_payload_len(content.len(), "Decoded")?;
    let bytes = compress_store_frame(content, 1, true)?;
    Ok(Section {
        decoded: content.len(),
        digest: digest(&[&bytes]),
        bytes: Cow::Owned(bytes),
    })
}

/// A v12 file of these frames: the structure, the pool entries by hash with
/// the number of blocks that name each and whether it is a frame of its own,
/// the pool frames `frame_plan` gives for them, and the blocks with their
/// row counts.
fn assemble(
    structure: Section<'_>,
    entries: &[([u8; 16], usize, bool)],
    pool: &[Section<'_>],
    blocks: &[(usize, Section<'_>)],
) -> Result<Vec<u8>> {
    let sized = |head: &mut Vec<u8>, frame: &Section<'_>| {
        write_var_u64(head, frame.decoded as u64)?;
        write_var_u64(head, frame.bytes.len() as u64)?;
        head.extend_from_slice(&frame.digest);
        Ok::<_, anyhow::Error>(())
    };
    let hashes = entries.iter().map(|(hash, ..)| *hash).collect::<Vec<_>>();
    let alone = entries.iter().map(|(.., alone)| *alone).collect::<Vec<_>>();
    anyhow::ensure!(
        frame_plan(&hashes, &alone).len() == pool.len(),
        "A block store needs the pool frames its entries give"
    );
    let mut head = SETTINGS_BINARY_MAGIC.to_vec();
    head.push(SETTINGS_BINARY_VERSION);
    for part in writer() {
        write_var_u64(&mut head, part)?;
    }
    sized(&mut head, &structure)?;
    write_var_u64(&mut head, entries.len() as u64)?;
    for (hash, count, alone) in entries {
        head.extend_from_slice(hash);
        write_var_u64(&mut head, ((*count as u64) << 1) | u64::from(*alone))?;
    }
    for frame in pool {
        sized(&mut head, frame)?;
    }
    write_var_u64(&mut head, blocks.len() as u64)?;
    for (count, frame) in blocks {
        write_var_u64(&mut head, *count as u64)?;
        sized(&mut head, frame)?;
    }
    head.extend_from_slice(&digest(&[&head]));
    let frames = std::iter::once(&structure)
        .chain(pool)
        .chain(blocks.iter().map(|(_, frame)| frame));
    check_settings_payload_len(frames.clone().map(|frame| frame.decoded).sum(), "Decoded")?;
    let size = head.len() + frames.clone().map(|frame| frame.bytes.len()).sum::<usize>();
    check_settings_file_len(size)?;
    head.reserve_exact(size - head.len());
    for frame in frames {
        head.extend_from_slice(&frame.bytes);
    }
    Ok(head)
}

fn read_frame(reader: &mut BytecodeReader<'_>) -> Result<Frame> {
    let (decoded, len) = read_frame_lengths(reader)?;
    let digest = read_digest(reader)?;
    Ok(Frame {
        at: 0,
        len,
        decoded,
        digest,
    })
}

fn read_digest(reader: &mut BytecodeReader<'_>) -> Result<[u8; 16]> {
    Ok(reader.read_bytes(16)?.try_into()?)
}

type BlockUniqueIds = (Vec<Option<Box<str>>>, Vec<(usize, Value)>);
type BlockCompareRun<'a> = (&'a [(usize, usize, usize)], bool, u32, &'a mut [u32]);

impl<'a> BlockStore<'a> {
    pub(crate) fn open(bytes: impl Into<Cow<'a, [u8]>>) -> Result<Self> {
        let bytes = bytes.into();
        check_settings_file_len(bytes.len())?;
        let mut reader = BytecodeReader::new(&bytes);
        reader.read_magic()?;
        let version = reader
            .read_u8()
            .context("Missing settings bytecode version")?;
        if version != SETTINGS_BINARY_VERSION {
            bail!("Settings bytecode version {version} is no block store");
        }
        let writer = [
            reader
                .read_var_u64()
                .context("Missing block store writer")?,
            reader
                .read_var_u64()
                .context("Missing block store writer")?,
        ];
        let mut structure = read_frame(&mut reader)?;
        let entries = reader.read_collection_len("pool entry count")?;
        let mut hashes = Vec::with_capacity(reader.capacity(entries, 17));
        let mut counts = Vec::with_capacity(reader.capacity(entries, 17));
        let mut alone = Vec::with_capacity(reader.capacity(entries, 17));
        for _ in 0..entries {
            let hash = read_digest(&mut reader)?;
            anyhow::ensure!(
                hashes.last().is_none_or(|last| *last < hash),
                "The pool entries of the block store do not rise by hash"
            );
            let tag = reader.read_len("pool entry block count")?;
            anyhow::ensure!(
                tag >> 1 > 0,
                "The block store keeps a pool entry no block names"
            );
            hashes.push(hash);
            counts.push(tag >> 1);
            alone.push(tag & 1 == 1);
        }
        let mut slots = vec![(0, 0); hashes.len()];
        let mut pool = Vec::new();
        for entries in frame_plan(&hashes, &alone) {
            for (place, entry) in entries.iter().enumerate() {
                slots[*entry] = (pool.len(), place);
            }
            pool.push(PoolFrame {
                frame: read_frame(&mut reader)?,
                entries,
            });
        }
        let block_count = reader.read_collection_len("block count")?;
        let mut blocks = Vec::with_capacity(reader.capacity(block_count, 19));
        let mut rows = 0;
        for _ in 0..block_count {
            let count = reader.read_collection_len("block row count")?;
            anyhow::ensure!(count > 0, "A value block holds no rows");
            let frame = read_frame(&mut reader)?;
            blocks.push(Block {
                frame,
                first: rows,
                count,
            });
            rows = checked_settings_collection_len(rows + count, "instance count")?;
        }
        anyhow::ensure!(
            counts.iter().all(|count| *count <= blocks.len()),
            "A pool entry of the block store is named by more blocks than it has"
        );
        let head = reader.position();
        if read_digest(&mut reader)? != digest(&[&bytes[..head]]) {
            bail!("The head of the block store does not match its digest");
        }
        let mut at = reader.position();
        let mut decoded = 0_usize;
        for frame in std::iter::once(&mut structure)
            .chain(pool.iter_mut().map(|pool| &mut pool.frame))
            .chain(blocks.iter_mut().map(|block| &mut block.frame))
        {
            frame.at = at;
            at = at
                .checked_add(frame.len)
                .filter(|end| *end <= bytes.len())
                .context("The frames of the block store run past its end")?;
            decoded = decoded.saturating_add(frame.decoded);
        }
        anyhow::ensure!(
            at == bytes.len(),
            "The block store has {} bytes after its last frame",
            bytes.len() - at
        );
        // A store holds as much as a v11 store's one frame might.
        check_settings_payload_len(decoded, "Decoded")?;
        let read_pool = pool.iter().map(|_| Mutex::new(None)).collect();
        let budget = DecodeBudget::for_file(bytes.len());
        Ok(Self {
            bytes,
            writer,
            structure,
            pool,
            hashes,
            counts,
            alone,
            slots,
            blocks,
            rows,
            budget,
            read_pool,
            #[cfg(test)]
            reserved: AtomicUsize::new(0),
        })
    }

    /// The store whose whole reads hold each payload of some size once, by
    /// its handle (see `settings::held`).
    pub(crate) fn holding(mut self) -> Self {
        self.budget = DecodeBudget::for_file(self.bytes.len()).holding_payloads();
        self
    }

    /// Whether this writer wrote the file, whose frames it then copies.
    pub(crate) fn of_this_writer(&self) -> bool {
        self.writer == writer()
    }

    /// Whether this writer wrote the file in blocks of `block_rows` rows, so
    /// that a write over it gives the file a whole encode in such blocks
    /// gives.
    fn cut_in(&self, block_rows: usize) -> bool {
        let cut = block_ranges(self.rows, block_rows);
        self.of_this_writer() && self.blocks.iter().map(Block::rows).eq(cut)
    }

    /// `cut_in` the blocks `encode_settings_bytecode` cuts.
    pub(crate) fn cut_whole(&self) -> bool {
        self.cut_in(BLOCK_ROWS)
    }

    /// Whether every frame of the file is as its head says.
    pub(crate) fn intact(&self) -> bool {
        let frames = std::iter::once(self.structure)
            .chain(self.pool.iter().map(|pool| pool.frame))
            .chain(self.blocks.iter().map(|block| block.frame))
            .collect::<Vec<_>>();
        (frames.par_iter()).all(|frame| self.frame_bytes(*frame).is_ok())
    }

    /// Every instance with its id, name, class and parent, and no values.
    pub(crate) fn structure(&self) -> Result<SettingsBytecode> {
        self.structure_of(self.content(self.structure)?)
    }

    /// Whether the structure holds `text`, read without building an
    /// instance: where it does not, none has it as its id, name or class.
    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) fn structure_holds(&self, text: &str) -> Result<bool> {
        let content = self.content(self.structure)?;
        Ok(memmem::find(&content, text.as_bytes()).is_some())
    }

    fn structure_of(&self, content: Vec<u8>) -> Result<SettingsBytecode> {
        let mut reader = BytecodeReader::new(&content);
        let header = decode_settings_payload_header(&mut reader, &self.budget)?;
        anyhow::ensure!(
            header.instance_count == self.rows,
            "The structure of the block store holds {} instances and its blocks {}",
            header.instance_count,
            self.rows
        );
        anyhow::ensure!(
            header.properties.is_empty() && reader.read_len("property group count")? == 0,
            "The structure of the block store holds values"
        );
        reader.finish()?;
        Ok(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: header.instances,
        })
    }

    /// The whole document, as the v11 decode of the same document gives it.
    /// Blocks are opened and decoded in bounded batches so temporary block
    /// buffers and decoded column vectors stay bounded on very large stores.
    pub(crate) fn decode(&self) -> Result<SettingsBytecode> {
        const DECODE_BLOCKS_AT_ONCE: usize = 4;
        let structure = self.content(self.structure)?;
        #[cfg(test)]
        WHOLE_DECODES.set(WHOLE_DECODES.get() + 1);
        let mut document = self.structure_of(structure)?;
        let mut chunks = self.split(&mut document.instances);
        for (batch_idx, batch) in chunks.chunks_mut(DECODE_BLOCKS_AT_ONCE).enumerate() {
            let first_block = batch_idx * DECODE_BLOCKS_AT_ONCE;
            batch
                .par_iter_mut()
                .enumerate()
                .try_for_each(|(offset, rows)| {
                    let open = self.open_block(first_block + offset)?;
                    self.block_into(&open, rows, |_| true, Texts::All)
                })?;
        }
        Ok(document)
    }

    /// Streams the store block-by-block in bounded batches into a compact
    /// representation suitable for streamed identity alignment without ever
    /// holding all decoded properties in memory at once.
    pub(crate) fn compact(
        &self,
    ) -> Result<(crate::settings::equivalence::CompactDocument, Vec<String>)> {
        use crate::settings::equivalence::{
            CompactDocument, CompactRowMeta, compact_row, stabilize_instance_references,
        };
        const COMPACT_BLOCKS_AT_ONCE: usize = 4;
        let _span = crate::app::profile::span("store compact decode");
        self.budget.renew(self.bytes.len());
        let structure = self.content(self.structure)?;
        let mut document = self.structure_of(structure)?;
        let ids: Vec<String> = document
            .instances
            .iter()
            .map(|instance| instance.settings_id.clone())
            .collect();
        let count = document.instances.len();
        let mut unique_ids: Vec<Option<Box<str>>> = vec![None; count];
        let mut meta: Vec<CompactRowMeta> = vec![
            CompactRowMeta {
                exact_digest: [0; 16],
                full_digest: [0; 16],
                compared_digest: [0; 16],
                unique_id_digest: [0; 16],
                keys_hash: 0,
                strict_digest: 0,
                reflexive: true,
                has_untyped_enum: false,
                has_ref: false,
            };
            count
        ];
        let mut cameras: Vec<(usize, SettingsBytecodeInstance)> = Vec::new();
        let mut inst_chunks = self.split(&mut document.instances);
        let mut uid_chunks = self.split(&mut unique_ids);
        let mut meta_chunks = self.split(&mut meta);
        for (batch_idx, ((inst_batch, uid_batch), meta_batch)) in inst_chunks
            .chunks_mut(COMPACT_BLOCKS_AT_ONCE)
            .zip(uid_chunks.chunks_mut(COMPACT_BLOCKS_AT_ONCE))
            .zip(meta_chunks.chunks_mut(COMPACT_BLOCKS_AT_ONCE))
            .enumerate()
        {
            let first_block = batch_idx * COMPACT_BLOCKS_AT_ONCE;
            let batch_cameras: Vec<Vec<(usize, SettingsBytecodeInstance)>> = inst_batch
                .par_iter_mut()
                .zip(uid_batch.par_iter_mut())
                .zip(meta_batch.par_iter_mut())
                .enumerate()
                .map(|(offset, ((rows, uids), metas))| {
                    let block_index = first_block + offset;
                    let block_first = self.blocks[block_index].first;
                    let open = self.open_block(block_index)?;
                    self.block_into(&open, rows, |_| true, Texts::All)?;
                    let mut local_cameras = Vec::new();
                    for (row_offset, ((inst, uid_slot), meta_slot)) in rows
                        .iter_mut()
                        .zip(uids.iter_mut())
                        .zip(metas.iter_mut())
                        .enumerate()
                    {
                        stabilize_instance_references(inst, &ids);
                        let (uid, row_meta, cam) = compact_row(inst);
                        *uid_slot = uid;
                        *meta_slot = row_meta;
                        if let Some(cam) = cam {
                            local_cameras.push((block_first + row_offset, cam));
                        }
                    }
                    Ok(local_cameras)
                })
                .collect::<Result<Vec<_>>>()?;
            for local in batch_cameras {
                cameras.extend(local);
            }
        }
        Ok((
            CompactDocument {
                document,
                unique_ids,
                meta,
                cameras,
            },
            ids,
        ))
    }

    /// Decodes and stabilizes only the requested rows, reusing the store's
    /// already-decoded bare structure records and opening each needed block on
    /// demand.
    pub(crate) fn hydrate_rows(
        &self,
        bare: &[SettingsBytecodeInstance],
        ids: &[String],
        rows: &[usize],
    ) -> Result<Vec<(usize, SettingsBytecodeInstance)>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        self.budget.renew(self.bytes.len());
        let rows = self.chosen(rows)?;
        let groups: Vec<&[usize]> = rows
            .chunk_by(|left, right| self.block_of(*left) == self.block_of(*right))
            .collect();
        let read_group = |group: &[usize]| -> Result<Vec<(usize, SettingsBytecodeInstance)>> {
            let block_index = self.block_of(group[0]);
            let first = self.blocks[block_index].first;
            let open = self.open_block(block_index)?;
            let mut values = (0..self.blocks[block_index].count)
                .map(|_| DecodedRowValues::default())
                .collect::<Vec<_>>();
            let keep = |row: usize| group.binary_search(&(first + row)).is_ok();
            self.block_into(&open, &mut values, keep, Texts::Resolved(&|_| true))?;
            Ok(group
                .iter()
                .map(|&row| {
                    let b = &bare[row];
                    let row_vals = std::mem::take(&mut values[row - first]);
                    let mut inst = SettingsBytecodeInstance {
                        settings_id: b.settings_id.clone(),
                        name: b.name.clone(),
                        class_name: b.class_name.clone(),
                        parent_index: b.parent_index,
                        properties: row_vals.properties,
                        attributes: row_vals.attributes,
                    };
                    crate::settings::equivalence::stabilize_instance_references(&mut inst, ids);
                    (row, inst)
                })
                .collect())
        };
        let parts = if groups.len() >= 2 && rayon::current_num_threads() > 1 {
            groups
                .into_par_iter()
                .map(read_group)
                .collect::<Result<Vec<_>>>()?
        } else {
            groups
                .into_iter()
                .map(read_group)
                .collect::<Result<Vec<_>>>()?
        };
        Ok(parts.into_iter().flatten().collect())
    }

    /// The whole document of the structure's content and every block opened.
    fn decode_opened(
        &self,
        structure: Vec<u8>,
        opened: Vec<OpenBlock>,
    ) -> Result<SettingsBytecode> {
        #[cfg(test)]
        WHOLE_DECODES.set(WHOLE_DECODES.get() + 1);
        let mut document = self.structure_of(structure)?;
        self.split(&mut document.instances)
            .into_par_iter()
            .zip(opened)
            .try_for_each(|(rows, open)| self.block_into(&open, rows, |_| true, Texts::All))?;
        Ok(document)
    }

    /// The values of the rows at `rows`, in rising order and each once, as the
    /// whole decode gives them. Only the blocks of those rows are read, and of
    /// the pool only the frames that hold the entries those rows name. A read
    /// takes as many rows as the head gives each block, which only the
    /// structure checks: callers read the structure first.
    pub(crate) fn rows(&self, rows: &[usize]) -> Result<Vec<(usize, DecodedRowValues)>> {
        let rows = self.chosen(rows)?;
        let read = rows
            .chunk_by(|left, right| self.block_of(*left) == self.block_of(*right))
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|rows| {
                let index = self.block_of(rows[0]);
                let first = self.blocks[index].first;
                let mut mask = vec![false; self.blocks[index].count];
                for &row in rows {
                    mask[row - first] = true;
                }
                let keep = |row: usize| mask[row];
                let mut values = self.block_values(index, keep, Texts::Resolved(&|_| true))?;
                Ok(rows
                    .iter()
                    .map(|row| (*row, std::mem::take(&mut values[row - first])))
                    .collect::<Vec<_>>())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(read.into_iter().flatten().collect())
    }

    /// The file a whole encode gives of the document an edit leaves, whose
    /// instances are `document`'s. An instance `placed` marks as held holds
    /// its values in `document`; any other keeps the stored values of the
    /// row it was read at, each reference by position following its
    /// instance. `moved` gives each string a reference may name an instance
    /// by that the records give or take.
    ///
    /// Where every row stays at its place, after an edit of values or one
    /// that adds instances behind the last, only the blocks of the held
    /// instances are written, read with a marker for each pool entry, and
    /// the pool frames whose entries change; the counts of the head follow
    /// from what those blocks named before and name now, and each other
    /// block whose table holds one of `moved` is read and written where it
    /// comes out otherwise. Where rows moved, every block is written from
    /// the blocks that held its rows. Blocks are read a few at a time, each
    /// batch on a budget of its own, and every frame that comes out the same
    /// is copied. Where rows moved, the structure is written beside the
    /// blocks; else the few blocks written come first. `whole` writes every
    /// block and then the structure from what the store holds, also where the
    /// edit leaves them alone.
    fn rewrite(
        &self,
        document: &SettingsBytecode,
        placed: Placed<'_>,
        moved: &[&str],
        (whole, standing): (bool, bool),
    ) -> Result<Vec<u8>> {
        let settling = crate::app::profile::span("store document settled");
        let settled = settled_settings_document(document)?;
        drop(settling);
        let prepared = crate::app::profile::span("store rewrite prepared");
        let ranges = block_ranges(settled.instances.len(), self.block_rows());
        let kept = (placed.to.iter().enumerate()).all(|(row, now)| *now == Some(row))
            && self.blocks.len() <= ranges.len()
            && (self.blocks.iter().zip(&ranges))
                .all(|(block, range)| block.first == range.start && block.rows().end <= range.end);
        let long = (settled.instances.iter())
            .flat_map(|row| [row.settings_id.as_str(), row.name.as_str()])
            .chain(moved.iter().copied())
            .filter(|text| text.len() >= POOL_MIN_BYTES)
            .map(|text| digest(&[text.as_bytes()]))
            .collect();
        let edit = Rewrite {
            document: &settled,
            placed,
            ranges,
            kept,
            whole,
            standing,
            moved: moved.iter().copied().collect(),
            long,
            dense: document_reference_lookup(&settled, false),
            every: OnceLock::new(),
        };
        drop(prepared);
        let structure = || {
            let _structuring = crate::app::profile::span("store structure written");
            if whole {
                let content = structure_content(&settled, &edit.dense)?;
                framed(&content, Some((self, self.structure)))
            } else if kept && settled.instances.len() == self.rows && moved.is_empty() {
                self.raw(self.structure)
            } else {
                compressed(&structure_content(&settled, &edit.dense)?)
            }
        };
        let (written, structure) = match whole || (kept && settled.instances.len() < 4096) {
            true => (self.rewritten_blocks(&edit), structure()),
            false => rayon::join(|| self.rewritten_blocks(&edit), structure),
        };
        let (mut counts, brought, sections) = written?;
        let structure = structure?;
        let _assembled = crate::app::profile::span("store pool written");
        counts.retain(|_, count| *count > 0);
        let entries = (counts.iter())
            .map(|(hash, count)| (*hash, (*count, brought.get(hash).map(String::as_str))))
            .collect::<PoolEntries<'_>>();
        let listed = listed(&entries, Some(self))?;
        let pool = pool_frames(&entries, &listed, Some(self))?;
        let blocks = (sections.into_iter().zip(&edit.ranges).enumerate())
            .map(|(index, (section, range))| {
                let section = match section {
                    Some(section) => section,
                    None => self.raw(self.blocks[index].frame)?,
                };
                Ok((range.len(), section))
            })
            .collect::<Result<Vec<_>>>()?;
        assemble(structure, &listed, &pool, &blocks)
    }

    /// The blocks of the file `edit` writes, each where it is written anew,
    /// with the count of each pool entry they leave and the text of each
    /// entry they bring.
    fn rewritten_blocks(&self, edit: &Rewrite<'_>) -> Result<RewrittenBlocks<'_>> {
        let _written = crate::app::profile::span("store blocks rewritten");
        let mut counts = BTreeMap::new();
        if edit.kept {
            counts.extend(self.hashes.iter().copied().zip(self.counts.iter().copied()));
        }
        let mut brought = BTreeMap::<[u8; 16], String>::new();
        let mut sections = (edit.ranges.iter().map(|_| None)).collect::<Vec<Option<Section<'_>>>>();
        let indices = (0..edit.ranges.len()).collect::<Vec<_>>();
        let mut written = Vec::with_capacity(indices.len());
        let checking = crate::app::profile::span("store blocks checked");
        for batch in indices.chunks(STANDING_BATCH) {
            self.budget.renew(self.bytes.len());
            let _batch = crate::app::profile::span("store block batch checked");
            let standing = (batch.par_iter())
                .map(|index| self.standing(edit, *index))
                .collect::<Result<Vec<_>>>()?;
            written.extend(standing);
        }
        drop(checking);
        let rest = (indices.iter().copied())
            .filter(|index| written[*index].is_none())
            .collect::<Vec<_>>();
        let writing = crate::app::profile::span("store blocks written anew");
        for batch in rest.chunks(RESTRUCTURED_BATCH) {
            self.budget.renew(self.bytes.len());
            let _batch = crate::app::profile::span("store block batch written");
            for (index, block) in batch.iter().zip(self.rewritten(edit, batch)?) {
                written[*index] = Some(Some(block));
            }
        }
        drop(writing);
        let _counted = crate::app::profile::span("store pool entries counted");
        for (index, written) in written.into_iter().enumerate() {
            let Some(Some(written)) = written else {
                continue;
            };
            self.unname(&mut counts, written.before)?;
            for (hash, text) in written.now {
                *counts.entry(hash).or_default() += 1;
                let Some(text) = text else {
                    continue;
                };
                let held = brought.entry(hash).or_insert_with(|| text.clone());
                anyhow::ensure!(*held == text, "Two strings of a store share a hash");
            }
            sections[index] = Some(written.section);
        }
        Ok((counts, brought, sections))
    }

    /// Block `index` of the file `edit` writes where the block the store
    /// holds there gives it without a write from its rows: none where that
    /// block stands as it is, its rows holding their stored values at their
    /// places and its table none of the strings the edit moved. Where rows
    /// moved, a block whose rows all stand where they stood, whose references
    /// by position name the rows they named, and which names no instance of
    /// the store otherwise (`references_stay`) holds the same values; it is
    /// copied, with the pool entries it names. `None` for a block `rewritten`
    /// writes.
    fn standing(&self, edit: &Rewrite<'_>, index: usize) -> Result<Option<Option<Written<'_>>>> {
        let range = edit.ranges[index].clone();
        let Placed { from, to, held } = edit.placed;
        let untouched = !edit.whole && edit.kept && !range.clone().any(|row| held[row]);
        let stays = edit.standing
            && !edit.whole
            && !edit.kept
            && (self.blocks.get(index)).is_some_and(|block| block.rows() == range)
            && range
                .clone()
                .all(|row| !held[row] && from[row] == Some(row));
        if !untouched && !stays {
            return Ok(None);
        }
        if untouched && edit.moved.is_empty() {
            return Ok(Some(None));
        }
        let _checking = crate::app::profile::span("store block checked");
        let open = self.open_block(index)?;
        let names = |string: &BlockString| match string {
            BlockString::Own(text) => edit.moved.contains(text.as_str()),
            BlockString::Pool(entry) => edit.long.contains(&self.hashes[*entry]),
        };
        if open.strings.iter().any(names) {
            return Ok(None);
        }
        if untouched {
            return Ok(Some(None));
        }
        let root_service = edit.dense.root_service.as_deref();
        if !self.references_stay((&open, index), to, root_service)? {
            return Ok(None);
        }
        let named = open.pool_entries().map(|entry| self.hashes[entry]);
        let section = self.raw(self.blocks[index].frame)?;
        let now = (named.collect::<AHashSet<_>>().into_iter())
            .map(|hash| (hash, None))
            .collect();
        Ok(Some(Some(Written {
            section,
            before: Vec::new(),
            now,
        })))
    }

    /// The blocks `batch` of the file `edit` writes, from the blocks that
    /// held their rows and the values the document holds. Each block that
    /// held rows of the batch is read once, for all of them.
    fn rewritten(&self, edit: &Rewrite<'_>, batch: &[usize]) -> Result<Vec<Written<'_>>> {
        let Placed { from, to, held } = edit.placed;
        let mut sources = Vec::with_capacity(batch.len());
        for index in batch {
            let mut own = Vec::new();
            for row in edit.ranges[*index].clone().filter(|row| !held[*row]) {
                let stored =
                    from[row].context("An instance without its values is no row of the store")?;
                own.push(self.block_of(stored));
            }
            if edit.kept && *index < self.blocks.len() {
                own.push(*index);
            }
            own.sort_unstable();
            own.dedup();
            sources.push(own);
        }
        let mut read = sources.iter().flatten().copied().collect::<Vec<_>>();
        read.sort_unstable();
        read.dedup();
        let target =
            |now: usize| (batch.iter()).position(|index| edit.ranges[*index].contains(&now));
        let placed = |source: usize, row: usize| {
            to[self.blocks[source].first + row].filter(|now| !held[*now] && target(*now).is_some())
        };
        let resolved = |hash: &[u8; 16]| edit.long.contains(hash);
        let texts = match edit.long.is_empty() {
            true => Texts::Marked,
            false => Texts::Resolved(&resolved),
        };
        let opened = (read.par_iter())
            .map(|source| {
                let _reading = crate::app::profile::span("store block rows read");
                let open = self.open_block(*source)?;
                let mut values = (0..self.blocks[*source].count)
                    .map(|_| DecodedRowValues::default())
                    .collect::<Vec<_>>();
                let keep = |row: usize| placed(*source, row).is_some();
                self.block_into(&open, &mut values, keep, texts)?;
                Ok((open, values))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut rows = (batch.iter())
            .map(|index| edit.document.instances[edit.ranges[*index].clone()].to_vec())
            .collect::<Vec<_>>();
        let mut blocks = Vec::with_capacity(opened.len());
        for (source, (open, values)) in read.iter().zip(opened) {
            for (row, values) in values.into_iter().enumerate() {
                let Some(now) = placed(*source, row) else {
                    continue;
                };
                let place = target(now).context("A row left the blocks it was read for")?;
                let instance = &mut rows[place][now - edit.ranges[batch[place]].start];
                instance.properties = values.properties;
                instance.attributes = values.attributes;
            }
            blocks.push(open);
        }
        let open = |source: &usize| read.binary_search(source).ok().map(|place| &blocks[place]);
        (batch.par_iter().zip(rows).zip(&sources))
            .map(|((index, mut rows), sources)| {
                let _encoding = crate::app::profile::span("store block encoded");
                for (instance, now) in rows.iter_mut().zip(edit.ranges[*index].clone()) {
                    if held[now] {
                        continue;
                    }
                    if !edit.kept {
                        let values = instance.properties.values_mut();
                        for value in values.chain(instance.attributes.values_mut()) {
                            remap_ref_indices_in_value(value, to);
                        }
                    }
                    for value in instance.attributes.values_mut() {
                        if let Some(canonical) = canonical_attribute_value(value) {
                            *value = canonical;
                        }
                    }
                    settle_forced_properties(&mut instance.properties);
                }
                let lookup = edit.lookup(&rows);
                let block = prepared(&rows, lookup)?;
                let content = block.content(lookup)?;
                let stands = (sources.contains(index).then(|| open(index)).flatten())
                    .is_some_and(|open| open.content == content);
                let section = match stands {
                    true => self.raw(self.blocks[*index].frame)?,
                    false => compressed(&content)?,
                };
                let before = match edit.kept {
                    true => (sources.iter().filter_map(open))
                        .flat_map(OpenBlock::pool_entries)
                        .collect(),
                    false => Vec::new(),
                };
                let now = (block.strings.pooled())
                    .map(|(hash, text)| {
                        let brought = text.filter(|_| self.entry_of(&hash).is_err());
                        (hash, brought.map(str::to_string))
                    })
                    .collect();
                Ok(Written {
                    section,
                    before,
                    now,
                })
            })
            .collect()
    }

    /// The rows of each block of this store but the last, which a file that
    /// gains rows keeps.
    fn block_rows(&self) -> usize {
        match self.blocks.as_slice() {
            [first, _, ..] => first.count,
            _ => BLOCK_ROWS.max(self.rows),
        }
    }

    /// Takes from `counts` one count of each pool entry of `named`, a block
    /// that named them.
    fn unname(
        &self,
        counts: &mut BTreeMap<[u8; 16], usize>,
        named: impl IntoIterator<Item = usize>,
    ) -> Result<()> {
        let named = (named.into_iter())
            .map(|entry| self.hashes[entry])
            .collect::<AHashSet<_>>();
        for hash in named {
            let count = counts.get_mut(&hash).context("A pool entry is missing")?;
            *count = count
                .checked_sub(1)
                .context("A pool entry is named by more blocks than its count")?;
        }
        Ok(())
    }

    /// The rows `wanted` takes by their stored values that may hold a
    /// reference, a marker in place of each pool entry: of the blocks whose
    /// table holds one of `strings`, or of every block where there are none.
    /// Blocks are read a few at a time, each batch on a budget of its own.
    fn rows_where(
        &self,
        strings: Option<&AHashSet<&str>>,
        wanted: impl Fn(&DecodedRowValues) -> bool + Sync,
    ) -> Result<Vec<usize>> {
        let long = (strings.into_iter().flatten())
            .filter(|text| text.len() >= POOL_MIN_BYTES)
            .map(|text| digest(&[text.as_bytes()]))
            .collect::<AHashSet<_>>();
        let names = |string: &BlockString| match (string, strings) {
            (_, None) => true,
            (BlockString::Own(text), Some(strings)) => strings.contains(text.as_str()),
            (BlockString::Pool(entry), _) => long.contains(&self.hashes[*entry]),
        };
        let indices = (0..self.blocks.len()).collect::<Vec<_>>();
        let mut rows = Vec::new();
        for batch in indices.chunks(RESTRUCTURED_BATCH) {
            self.budget.renew(self.bytes.len());
            let _batch = crate::app::profile::span("store rows batch searched");
            let found = (batch.par_iter())
                .map(|index| {
                    let open = self.open_block(*index)?;
                    if strings.is_some() && !open.strings.iter().any(names) {
                        return Ok(Vec::new());
                    }
                    let block = &self.blocks[*index];
                    let mut values = (0..block.count)
                        .map(|_| DecodedRowValues::default())
                        .collect::<Vec<_>>();
                    let referring = |_: &str, kind: u8| REFERRING_KINDS.contains(&kind);
                    self.values(&open, &mut values, |_| true, referring, Texts::Marked)?;
                    let rows = block.rows().zip(&values);
                    Ok((rows.filter(|(_, values)| wanted(values)))
                        .map(|(row, _)| row)
                        .collect::<Vec<_>>())
                })
                .collect::<Result<Vec<_>>>()?;
            rows.extend(found.into_iter().flatten());
        }
        Ok(rows)
    }

    /// Whether the store holds every string of one of the sets in `required`:
    /// a string counts where the structure, a block or a pool entry holds it.
    /// Each frame is read by itself.
    pub(crate) fn holds_any(&self, required: &[Vec<String>]) -> Result<bool> {
        let needles = (required.iter().flatten())
            .map(String::as_str)
            .collect::<AHashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let found = |holds: &(dyn Fn(&str) -> bool + Sync)| {
            needles
                .iter()
                .map(|needle| holds(needle))
                .collect::<Vec<_>>()
        };
        let frames = std::iter::once(self.structure)
            .chain(self.blocks.iter().map(|block| block.frame))
            .collect::<Vec<_>>();
        let in_frames = (frames.par_iter()).map(|frame| {
            let content = self.content(*frame)?;
            Ok(found(&|needle| {
                memmem::find(&content, needle.as_bytes()).is_some()
            }))
        });
        let in_pool = (0..self.pool.len()).into_par_iter().map(|index| {
            let texts = self.pool_texts(index)?;
            Ok(found(&|needle| {
                texts.iter().any(|text| text.contains(needle))
            }))
        });
        let held = (in_frames.chain(in_pool)).try_reduce(
            || vec![false; needles.len()],
            |mut held: Vec<bool>, found| {
                (held.iter_mut().zip(found)).for_each(|(held, found)| *held |= found);
                Ok::<_, anyhow::Error>(held)
            },
        )?;
        let held = (needles.iter().zip(held))
            .filter_map(|(needle, held)| held.then_some(*needle))
            .collect::<AHashSet<_>>();
        Ok(required
            .iter()
            .any(|set| set.iter().all(|needle| held.contains(needle.as_str()))))
    }

    /// Whether an instance has one of `strings` as its id, or a block or a
    /// pool entry holds one of them as a whole string. Each frame is read by
    /// itself, and the pool only for a string as long as its entries.
    pub(crate) fn holds_any_whole(
        &self,
        strings: &std::collections::HashSet<&str>,
    ) -> Result<bool> {
        let instances = self.structure()?.instances;
        if instances
            .iter()
            .any(|row| strings.contains(row.settings_id.as_str()))
        {
            return Ok(true);
        }
        let in_blocks = (0..self.blocks.len()).into_par_iter().map(|index| {
            let open = self.open_block(index)?;
            Ok((open.strings.iter())
                .any(|string| matches!(string, BlockString::Own(text) if strings.contains(text.as_str()))))
        });
        let long = strings.iter().any(|text| text.len() >= POOL_MIN_BYTES);
        let in_pool = (0..self.pool.len())
            .into_par_iter()
            .filter(|_| long)
            .map(|index| {
                let texts = self.pool_texts(index)?;
                Ok(texts.iter().any(|text| strings.contains(text.as_str())))
            });
        let found = in_blocks.chain(in_pool).collect::<Result<Vec<bool>>>()?;
        Ok(found.contains(&true))
    }

    /// The place of the pool entry of hash `hash`.
    fn entry_of(&self, hash: &[u8; 16]) -> Result<usize> {
        (self.hashes.binary_search(hash).ok()).context("A pool entry the store keeps is missing")
    }

    /// Whether each block of this store is, byte for byte, the block `other`
    /// holds there, where the structures of the two are the same bytes too:
    /// an index and an id then name one instance in both, so a row of such a
    /// block holds in both the same values, the instances they name
    /// included. No block where the two are of other instances.
    pub(crate) fn blocks_held_by(&self, other: &BlockStore<'_>) -> Vec<bool> {
        let same = |mine: Frame, theirs: Frame| {
            mine.digest == theirs.digest
                && self.bytes[mine.at..mine.at + mine.len]
                    == other.bytes[theirs.at..theirs.at + theirs.len]
        };
        let alike =
            self.blocks.len() == other.blocks.len() && same(self.structure, other.structure);
        (self.blocks.iter().zip(&other.blocks))
            .map(|(mine, theirs)| {
                alike && mine.count == theirs.count && same(mine.frame, theirs.frame)
            })
            .collect()
    }

    /// `property_edit::row_digests`, or `whole_row_digests` where `whole`
    /// says so, of this store: each block decoded whole, digested and given
    /// back without its values. A block `shared` takes is one another store
    /// holds too (`blocks_held_by`), digested beside that store alone: each
    /// of its rows has the digest of the block and its place there, which is
    /// the same in both and no other row's, and none of them is given as
    /// naming an instance. `structure` is the instances of this store without
    /// their values, where they were read before.
    pub(crate) fn row_digests(
        &self,
        whole: bool,
        shared: &[bool],
        structure: Option<Vec<SettingsBytecodeInstance>>,
        plan: impl FnOnce(&[SettingsBytecodeInstance]) -> Option<(Option<usize>, Option<Moved>)>,
    ) -> Result<Option<StoreRows>> {
        self.row_digests_inner(whole, false, shared, structure, plan)
    }

    fn row_digests_inner(
        &self,
        whole: bool,
        keep_structure: bool,
        shared: &[bool],
        structure: Option<Vec<SettingsBytecodeInstance>>,
        plan: impl FnOnce(&[SettingsBytecodeInstance]) -> Option<(Option<usize>, Option<Moved>)>,
    ) -> Result<Option<StoreRows>> {
        let instances = match structure {
            Some(instances) if instances.len() == self.rows => instances,
            _ => self.structure()?.instances,
        };
        let Some((kept, moved)) = plan(&instances) else {
            return Ok(None);
        };
        self.row_digests_of_instances_inner(instances, whole, keep_structure, shared, kept, moved)
            .map(Some)
    }

    pub(crate) fn extract_unique_ids_and_root_camera(
        &self,
        instances: &mut [SettingsBytecodeInstance],
    ) -> Result<Vec<Option<Box<str>>>> {
        use super::captured::UNIQUE_ID;

        let extract_block = |index: usize| -> Result<BlockUniqueIds> {
            let block = &self.blocks[index];
            let open = self.open_block(index)?;
            let mut uids: Vec<Option<Box<str>>> = vec![None; block.count];
            let mut root_cameras: Vec<(usize, Value)> = Vec::new();
            let has_root = instances[block.rows()]
                .iter()
                .any(|inst| inst.parent_index.is_none());
            if has_root {
                let mut values = (0..block.count)
                    .map(|_| DecodedRowValues::default())
                    .collect::<Vec<_>>();
                self.values(
                    &open,
                    &mut values,
                    |local| instances[block.first + local].parent_index.is_none(),
                    |name, _| name == "CurrentCamera",
                    Texts::Resolved(&|_| true),
                )?;
                for (local, mut row_vals) in values.into_iter().enumerate() {
                    if let Some(cam) = row_vals.properties.remove("CurrentCamera") {
                        root_cameras.push((block.first + local, cam));
                    }
                }
            }
            let Some((str_opts, _, prop_names)) = build_block_string_meta(&open) else {
                return self.extract_block_unique_ids_fallback(index, &open, root_cameras);
            };
            let (specs, _, _) = parse_block_group_specs(&open, &prop_names)?;
            for (name, kind, value_count, body) in specs {
                if name != UNIQUE_ID {
                    continue;
                }
                if kind != 8 {
                    return self.extract_block_unique_ids_fallback(index, &open, root_cameras);
                }
                let mut grp = BytecodeReader::new(body);
                let mut prev_row = 0_usize;
                for position in 0..value_count {
                    let local = next_group_instance(
                        prev_row,
                        grp.read_len("property instance index")?,
                        position,
                    )?;
                    prev_row = local;
                    anyhow::ensure!(
                        local < block.count,
                        "A value block gives a value to a row it does not hold"
                    );
                    let Some((k0, v0_id, k1, v1_id)) =
                        read_two_string_object_header(&mut grp, &str_opts)
                    else {
                        return self.extract_block_unique_ids_fallback(index, &open, root_cameras);
                    };
                    let Some(uid) = extract_two_field_unique_id(&str_opts, k0, v0_id, k1, v1_id)
                    else {
                        return self.extract_block_unique_ids_fallback(index, &open, root_cameras);
                    };
                    let trimmed = uid.trim();
                    if !trimmed.is_empty() && !trimmed.chars().all(|c| c == '0') {
                        uids[local] = Some(trimmed.into());
                    }
                }
                grp.finish()?;
            }
            Ok((uids, root_cameras))
        };

        let blocks = (0..self.blocks.len())
            .into_par_iter()
            .map(extract_block)
            .collect::<Result<Vec<_>>>()?;
        let mut all_uids = Vec::with_capacity(self.rows);
        for (uids, cams) in blocks {
            all_uids.extend(uids);
            for (row, cam) in cams {
                instances[row]
                    .properties
                    .insert("CurrentCamera".into(), cam);
            }
        }
        Ok(all_uids)
    }

    fn extract_block_unique_ids_fallback(
        &self,
        index: usize,
        open: &OpenBlock,
        root_cameras: Vec<(usize, Value)>,
    ) -> Result<BlockUniqueIds> {
        use super::captured::UNIQUE_ID;

        let mut values = (0..self.blocks[index].count)
            .map(|_| DecodedRowValues::default())
            .collect::<Vec<_>>();
        self.values(
            open,
            &mut values,
            |_| true,
            |name, _| name == UNIQUE_ID,
            Texts::Resolved(&|_| true),
        )?;
        let uids = values
            .into_iter()
            .map(|vals| {
                vals.properties
                    .get(UNIQUE_ID)
                    .and_then(|v| {
                        v.as_object()
                            .filter(|o| o.get("_type").and_then(Value::as_str) == Some("UniqueId"))
                            .and_then(|o| o.get("value").and_then(Value::as_str))
                    })
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !s.chars().all(|c| c == '0'))
                    .map(Box::<str>::from)
            })
            .collect();
        Ok((uids, root_cameras))
    }

    pub(crate) fn row_digests_of_instances_with_structure(
        &self,
        instances: Vec<SettingsBytecodeInstance>,
        whole: bool,
        shared: &[bool],
        kept: Option<usize>,
        moved: Option<Moved>,
    ) -> Result<StoreRows> {
        self.row_digests_of_instances_inner(instances, whole, true, shared, kept, moved)
    }

    fn row_digests_of_instances_inner(
        &self,
        mut instances: Vec<SettingsBytecodeInstance>,
        whole: bool,
        keep_structure: bool,
        shared: &[bool],
        kept: Option<usize>,
        moved: Option<Moved>,
    ) -> Result<StoreRows> {
        use super::captured::{
            UNIQUE_ID, document_row_digest, value_digest, whole_document_row_digest,
        };
        let kept = kept.filter(|kept| *kept < instances.len());
        let digest_of: fn(&SettingsBytecodeInstance) -> ([u8; 16], [u8; 16]) = if whole {
            whole_document_row_digest
        } else {
            document_row_digest
        };
        // The rows of a shared block are not read, and neither is a pool
        // frame they alone name: a damaged one fails here as it fails their
        // read.
        if shared.contains(&true) {
            (self.pool.par_iter()).try_for_each(|pool| self.frame_bytes(pool.frame).map(drop))?;
        }
        let database = rbx_reflection_database::get().ok();
        let blocks = crate::app::profile::span("store blocks digested");
        let digested = self
            .split(&mut instances)
            .into_par_iter()
            .zip(&self.blocks)
            .enumerate()
            .map(|(index, (rows, block))| {
                let open = self.open_block(index)?;
                let pool_entries = if keep_structure {
                    open.pool_entries().collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                if whole && kept.is_none() && moved.is_none() && shared.get(index) == Some(&true) {
                    self.values(
                        &open,
                        rows,
                        |_| true,
                        |name, _| name == UNIQUE_ID,
                        Texts::Marked,
                    )?;
                    let block_rows = (0_u64..)
                        .zip(rows)
                        .map(|(place, instance)| {
                            (
                                digest(&[&block.frame.digest, &place.to_le_bytes()]),
                                (instance.properties.get(UNIQUE_ID)).map_or([0; 16], value_digest),
                                [0u8; 16],
                                None,
                            )
                        })
                        .collect();
                    return Ok((block_rows, pool_entries));
                }
                if let Some(streamed) = self.digest_block_streaming(
                    &open,
                    block,
                    rows,
                    DigestBlockOpts {
                        whole,
                        kept,
                        moved: moved.as_ref(),
                        database,
                    },
                )? {
                    return Ok((streamed, pool_entries));
                }
                self.block_into(&open, rows, |_| true, Texts::All)?;
                let mut digests = Vec::with_capacity(rows.len());
                for (row, instance) in (block.first..).zip(rows) {
                    let named = whole
                        && instance
                            .properties
                            .values()
                            .chain(instance.attributes.values())
                            .any(names_an_instance_by_id);
                    let orig_parent = instance.parent_index;
                    let (sum, unique_id, engine) = match &moved {
                        Some(moved) => {
                            moved.parent_in_state(instance);
                            let (properties, attributes) = (
                                moved.values_in_state(&instance.properties),
                                moved.values_in_state(&instance.attributes),
                            );
                            let properties = properties
                                .map(|placed| std::mem::replace(&mut instance.properties, placed));
                            let attributes = attributes
                                .map(|placed| std::mem::replace(&mut instance.attributes, placed));
                            let (sum, uid) = digest_of(instance);
                            let engine = if whole {
                                [0; 16]
                            } else {
                                engine_state_digest(database, instance)
                            };
                            if let Some(properties) = properties {
                                instance.properties = properties;
                            }
                            if let Some(attributes) = attributes {
                                instance.attributes = attributes;
                            }
                            if !whole {
                                instance.parent_index = orig_parent;
                            }
                            (sum, uid, engine)
                        }
                        None => {
                            let (sum, uid) = digest_of(instance);
                            let engine = if whole {
                                [0; 16]
                            } else {
                                engine_state_digest(database, instance)
                            };
                            (sum, uid, engine)
                        }
                    };
                    digests.push((sum, unique_id, engine, named.then_some(row)));
                    if kept == Some(row) {
                        instance
                            .properties
                            .values_mut()
                            .for_each(crate::settings::held::resolve_all);
                    } else {
                        instance
                            .properties
                            .retain(|name, _| whole && name == UNIQUE_ID);
                        instance.attributes.clear();
                    }
                }
                Ok((digests, pool_entries))
            })
            .collect::<Result<Vec<_>>>()?;
        drop(blocks);
        let mut digests = Vec::with_capacity(self.rows);
        let mut unique_ids = Vec::with_capacity(self.rows);
        let mut engine_digests = if whole {
            Vec::new()
        } else {
            Vec::with_capacity(self.rows)
        };
        let mut named = Vec::new();
        let mut block_pool_entries = if keep_structure {
            Vec::with_capacity(self.blocks.len())
        } else {
            Vec::new()
        };
        for (block_rows, pool_entries) in digested {
            if keep_structure {
                block_pool_entries.push(pool_entries);
            }
            for (sum, unique_id, engine, row) in block_rows {
                digests.push(sum);
                unique_ids.push(unique_id);
                if !whole {
                    engine_digests.push(engine);
                }
                named.extend(row);
            }
        }
        let kept = kept.map(|kept| {
            let mut instance = instances[kept].clone();
            if !whole && let Some(moved) = &moved {
                moved.parent_in_state(&mut instance);
            }
            if keep_structure && !whole {
                instances[kept].properties.clear();
                instances[kept].attributes.clear();
            }
            instance
        });
        let (ids, instances, retained_structure) = if whole {
            (Vec::new(), instances, None)
        } else if keep_structure {
            let ids = if instances.len() >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES {
                instances
                    .par_iter()
                    .map(|instance| instance.settings_id.clone())
                    .collect()
            } else {
                instances
                    .iter()
                    .map(|instance| instance.settings_id.clone())
                    .collect()
            };
            (ids, Vec::new(), Some(instances))
        } else {
            let ids = instances.into_iter().map(|instance| instance.settings_id);
            (ids.collect(), Vec::new(), None)
        };
        Ok(StoreRows {
            digests,
            unique_ids,
            engine_digests,
            kept,
            ids,
            structure: retained_structure,
            block_pool_entries,
            moved,
            instances,
            named,
        })
    }

    fn digest_block_streaming(
        &self,
        open: &OpenBlock,
        block: &Block,
        rows: &mut [SettingsBytecodeInstance],
        opts: DigestBlockOpts<'_>,
    ) -> Result<Option<Vec<StreamedRowDigest>>> {
        use super::captured::{
            EMPTY_ATTRIBUTES_DIGEST, RowHasher, StrOrHash, UNIQUE_ID, row_digest_header,
        };

        let eager_pool: std::cell::OnceCell<PoolRead> = std::cell::OnceCell::new();
        let prop_pool_read = self.load_digest_prop_pool(open, opts.whole, &eager_pool)?;

        let mut str_items: Vec<StrOrHash<'_>> = Vec::with_capacity(open.strings.len());
        let mut table_bytes = 0_usize;
        for string in &open.strings {
            match string {
                BlockString::Own(text) => {
                    table_bytes = table_bytes.saturating_add(text.len());
                    str_items.push(StrOrHash::Text(text.as_str()));
                }
                BlockString::Pool(entry) => {
                    table_bytes = table_bytes.saturating_add(16);
                    str_items.push(StrOrHash::Hash(self.hashes[*entry]));
                }
            }
        }
        let mut prop_names: Vec<&str> = Vec::with_capacity(open.properties.len());
        for &id in &open.properties {
            let text = match &open.strings[id] {
                BlockString::Own(text) => text.as_str(),
                BlockString::Pool(entry) => {
                    let pool = eager_pool
                        .get()
                        .or(prop_pool_read.as_ref())
                        .expect("pool loaded for pooled property name");
                    self.entry(pool, *entry)?
                }
            };
            table_bytes = table_bytes.saturating_add(text.len());
            prop_names.push(text);
        }
        self.budget.take(table_bytes)?;

        let (specs, attr_groups, prop_order) = parse_block_group_specs(open, &prop_names)?;
        let count = rows.len();
        let mut hashers: Vec<RowHasher> = Vec::with_capacity(count);
        for instance in rows.iter_mut() {
            let parent = match opts.moved {
                Some(m) => {
                    let placed = m.parent_index_in_state(instance.parent_index);
                    if opts.whole {
                        instance.parent_index = placed;
                    }
                    placed
                }
                None => instance.parent_index,
            };
            hashers.push(row_digest_header(
                &instance.name,
                &instance.class_name,
                parent,
            ));
        }

        let mut state = StreamDigestState {
            unique_ids: vec![[0u8; 16]; count],
            attr_digests: vec![*EMPTY_ATTRIBUTES_DIGEST; count],
            engine_hashers: if opts.whole {
                Vec::new()
            } else {
                (0..count).map(|_| None).collect()
            },
            named_flags: if opts.whole {
                vec![false; count]
            } else {
                Vec::new()
            },
            multi_slot: Vec::new(),
            string_digests: vec![None; str_items.len()],
            enum_digests: vec![None; str_items.len()],
            pair_obj_digests: AHashMap::new(),
            hashers,
        };

        let resolved_str_refs: std::cell::OnceCell<Vec<&str>> = std::cell::OnceCell::new();
        let get_resolved_refs = || -> Result<&[&str]> {
            if let Some(refs) = resolved_str_refs.get() {
                return Ok(refs.as_slice());
            }
            let pool = match eager_pool.get() {
                Some(p) => p,
                None => {
                    let loaded = self.pool_read(open.pool_entries())?;
                    let _ = eager_pool.set(loaded);
                    eager_pool.get().unwrap()
                }
            };
            let refs: Vec<&str> = open
                .strings
                .iter()
                .map(|s| match s {
                    BlockString::Own(t) => Ok(t.as_str()),
                    BlockString::Pool(entry) => self.entry(pool, *entry),
                })
                .collect::<Result<Vec<_>>>()?;
            let _ = resolved_str_refs.set(refs);
            Ok(resolved_str_refs.get().unwrap().as_slice())
        };
        let kept_local = opts
            .kept
            .and_then(|k| k.checked_sub(block.first).filter(|&l| l < count));

        if !attr_groups.is_empty() {
            self.digest_attr_groups(
                (open, &specs, &attr_groups),
                rows,
                &mut state,
                (opts, kept_local),
                &get_resolved_refs,
            )?;
        }

        let mut run_start = 0_usize;
        while run_start < prop_order.len() {
            let name = specs[prop_order[run_start]].0;
            let mut run_end = run_start + 1;
            while run_end < prop_order.len() && specs[prop_order[run_end]].0 == name {
                run_end += 1;
            }
            let multi = run_end - run_start > 1;
            if multi && state.multi_slot.len() != count {
                state.multi_slot.resize(count, None);
            }
            let is_unique_id = name == UNIQUE_ID;

            for &group_idx in &prop_order[run_start..run_end] {
                self.digest_property_group(
                    specs[group_idx],
                    rows,
                    &str_items,
                    &mut state,
                    (opts, kept_local, multi, is_unique_id),
                    &get_resolved_refs,
                )?;
            }

            if multi {
                for (row, instance) in rows.iter().enumerate() {
                    if let Some(val_digest) = state.multi_slot[row].take() {
                        record_row_property_digest(
                            &mut state,
                            row,
                            &instance.class_name,
                            name,
                            is_unique_id,
                            val_digest,
                            opts,
                        );
                    }
                }
            }
            run_start = run_end;
        }

        if let Some(local) = kept_local {
            rows[local]
                .properties
                .values_mut()
                .for_each(crate::settings::held::resolve_all);
        }

        let mut digests = Vec::with_capacity(count);
        for row in 0..count {
            let mut h = std::mem::replace(&mut state.hashers[row], RowHasher::new());
            h.update(&[0]);
            h.update(&state.attr_digests[row]);
            let engine = if opts.whole {
                [0; 16]
            } else {
                state.engine_hashers[row]
                    .take()
                    .map_or([0; 16], RowHasher::finish)
            };
            let named_row = (opts.whole && state.named_flags[row]).then_some(block.first + row);
            digests.push((h.finish(), state.unique_ids[row], engine, named_row));
        }
        Ok(Some(digests))
    }

    fn load_digest_prop_pool(
        &self,
        open: &OpenBlock,
        whole: bool,
        eager_pool: &std::cell::OnceCell<PoolRead>,
    ) -> Result<Option<PoolRead>> {
        if whole {
            let _ = eager_pool.set(self.pool_read(open.pool_entries())?);
            return Ok(None);
        }
        let prop_pool: Vec<usize> = open
            .properties
            .iter()
            .filter_map(|&id| match open.strings[id] {
                BlockString::Pool(entry) => Some(entry),
                BlockString::Own(_) => None,
            })
            .collect();
        if prop_pool.is_empty() {
            Ok(None)
        } else {
            Ok(Some(self.pool_read(prop_pool)?))
        }
    }

    fn digest_attr_groups<'b>(
        &self,
        (open, specs, attr_groups): (&OpenBlock, &[BlockGroupSpec<'_>], &[usize]),
        rows: &mut [SettingsBytecodeInstance],
        state: &mut StreamDigestState,
        (opts, kept_local): (DigestBlockOpts<'_>, Option<usize>),
        get_resolved_refs: &dyn Fn() -> Result<&'b [&'b str]>,
    ) -> Result<()> {
        use super::captured::attributes_digest;

        let count = rows.len();
        let mut full_needed = opts.whole || kept_local.is_some();
        if !full_needed {
            let marked_table: Vec<Cow<'_, str>> = open
                .strings
                .iter()
                .map(|s| match s {
                    BlockString::Own(t) => Cow::Borrowed(t.as_str()),
                    BlockString::Pool(entry) => Cow::Owned(marker(&self.hashes[*entry])),
                })
                .collect();
            let marked_refs: Vec<&str> = marked_table.iter().map(Cow::as_ref).collect();
            let scope = ValueScope::from_borrowed(&marked_refs, self.rows, &self.budget, None);
            'marked: for &group_idx in attr_groups {
                let (name, kind, value_count, body) = specs[group_idx];
                self.budget.property_group(value_count, name, kind)?;
                let mut reader = BytecodeReader::new(body);
                let _ = decode_cframe_group_header(kind, &mut reader)?;
                let mut prev_row = 0_usize;
                for position in 0..value_count {
                    let row = next_group_instance(
                        prev_row,
                        reader.read_len("property instance index")?,
                        position,
                    )?;
                    prev_row = row;
                    anyhow::ensure!(
                        row < count,
                        "A value block gives a value to a row it does not hold"
                    );
                    let attributes = decode_attributes_payload(&mut reader, scope)?;
                    if attributes.values().any(holds_marker)
                        || attributes.keys().any(|k| marked(k).is_some())
                    {
                        full_needed = true;
                        break 'marked;
                    }
                    state.attr_digests[row] =
                        match opts.moved.and_then(|m| m.values_in_state(&attributes)) {
                            Some(placed) => attributes_digest(&placed),
                            None => attributes_digest(&attributes),
                        };
                }
                reader.finish()?;
            }
        }
        if !full_needed {
            return Ok(());
        }
        let str_refs = get_resolved_refs()?;
        let scope = ValueScope::from_borrowed(str_refs, self.rows, &self.budget, None);
        for &group_idx in attr_groups {
            let (name, kind, value_count, body) = specs[group_idx];
            self.budget.property_group(value_count, name, kind)?;
            let mut reader = BytecodeReader::new(body);
            let _ = decode_cframe_group_header(kind, &mut reader)?;
            let mut prev_row = 0_usize;
            for position in 0..value_count {
                let row = next_group_instance(
                    prev_row,
                    reader.read_len("property instance index")?,
                    position,
                )?;
                prev_row = row;
                anyhow::ensure!(
                    row < count,
                    "A value block gives a value to a row it does not hold"
                );
                let attributes = decode_attributes_payload(&mut reader, scope)?;
                if opts.whole && attributes.values().any(names_an_instance_by_id) {
                    state.named_flags[row] = true;
                }
                state.attr_digests[row] =
                    match opts.moved.and_then(|m| m.values_in_state(&attributes)) {
                        Some(placed) => attributes_digest(&placed),
                        None => attributes_digest(&attributes),
                    };
                if kept_local == Some(row) {
                    rows[row].attributes = attributes;
                }
            }
            reader.finish()?;
        }
        Ok(())
    }

    fn digest_property_group<'b>(
        &self,
        (name, kind, value_count, body): BlockGroupSpec<'_>,
        rows: &mut [SettingsBytecodeInstance],
        str_items: &[super::captured::StrOrHash<'_>],
        state: &mut StreamDigestState,
        (opts, kept_local, multi, is_unique_id): (DigestBlockOpts<'_>, Option<usize>, bool, bool),
        get_resolved_refs: &dyn Fn() -> Result<&'b [&'b str]>,
    ) -> Result<()> {
        use super::captured::value_digest;

        let count = rows.len();
        self.budget.property_group(value_count, name, kind)?;
        let mut reader = BytecodeReader::new(body);
        let cframe = decode_cframe_group_header(kind, &mut reader)?;
        let mut prev_row = 0_usize;
        let mut prev_ref = 0_usize;
        let mut charged = 0_usize;

        for position in 0..value_count {
            let row = next_group_instance(
                prev_row,
                reader.read_len("property instance index")?,
                position,
            )?;
            prev_row = row;
            anyhow::ensure!(
                row < count,
                "A value block gives a value to a row it does not hold"
            );
            let keep_value = kept_local == Some(row) || (opts.whole && is_unique_id);
            let val_digest = if keep_value {
                let str_refs = get_resolved_refs()?;
                let scope = ValueScope::from_borrowed(str_refs, self.rows, &self.budget, None);
                let entry = decode_property_group_entry(
                    &mut reader,
                    name,
                    kind,
                    row,
                    scope,
                    &cframe,
                    &mut prev_ref,
                )?;
                let DecodedGroupEntry::Property(_, value) = entry else {
                    unreachable!("non-Attributes group decoded as Attributes");
                };
                if opts.whole && names_an_instance_by_id(&value) {
                    state.named_flags[row] = true;
                }
                let d = match opts.moved.and_then(|m| m.value_in_state(&value)) {
                    Some(placed) => value_digest(&placed),
                    None => value_digest(&value),
                };
                rows[row].properties.insert(name.to_string(), value);
                d
            } else {
                self.digest_wire_value_fast(
                    &mut reader,
                    (kind, row, &cframe, &mut prev_ref),
                    str_items,
                    state,
                    (&mut charged, opts),
                    get_resolved_refs,
                )?
            };

            if multi {
                state.multi_slot[row] = Some(val_digest);
            } else {
                record_row_property_digest(
                    state,
                    row,
                    &rows[row].class_name,
                    name,
                    is_unique_id,
                    val_digest,
                    opts,
                );
            }
        }
        self.budget.take(charged)?;
        reader.finish()?;
        Ok(())
    }

    fn digest_wire_value_fast<'b>(
        &self,
        reader: &mut BytecodeReader<'_>,
        (kind, row, cframe, prev_ref): (u8, usize, &CFrameGroupDecoder, &mut usize),
        str_items: &[super::captured::StrOrHash<'_>],
        state: &mut StreamDigestState,
        (charged, opts): (&mut usize, DigestBlockOpts<'_>),
        get_resolved_refs: &dyn Fn() -> Result<&'b [&'b str]>,
    ) -> Result<[u8; 16]> {
        use super::captured::{
            BOOL_FALSE_DIGEST, BOOL_TRUE_DIGEST, NULL_VALUE_DIGEST, StrOrHash,
            digest_cframe_f64_value, digest_enum_value, digest_f64_value,
            digest_fixed_floats_value, digest_i64_value, digest_long_string_hash,
            digest_ref_one_based, digest_string_value, digest_two_string_object_with_hashes,
            digest_u64_value, value_digest,
        };

        match kind {
            0 => Ok(*NULL_VALUE_DIGEST),
            1 => Ok(*BOOL_FALSE_DIGEST),
            2 => Ok(*BOOL_TRUE_DIGEST),
            3 => Ok(digest_i64_value(unzigzag_i64(reader.read_var_u64()?))),
            4 => Ok(digest_u64_value(reader.read_var_u64()?)),
            5 => Ok(digest_f64_value(read_f64(reader)?)),
            20 => Ok(digest_f64_value(f64::from(read_f32(reader)?))),
            6 => {
                let id = reader.read_len("value string id")?;
                let item = *str_items
                    .get(id)
                    .with_context(|| format!("Invalid value string id {id}"))?;
                Ok(match item {
                    StrOrHash::Text(text) => {
                        *charged = charged.saturating_add(text.len());
                        *state.string_digests[id].get_or_insert_with(|| digest_string_value(text))
                    }
                    StrOrHash::Hash(hash) => {
                        *charged = charged.saturating_add(16);
                        *state.string_digests[id]
                            .get_or_insert_with(|| digest_long_string_hash(&hash))
                    }
                })
            }
            19 => {
                let id = reader.read_len("enum item string id")?;
                let item = *str_items
                    .get(id)
                    .with_context(|| format!("Invalid enum item string id {id}"))?;
                Ok(match item {
                    StrOrHash::Text(text) => {
                        *charged = charged.saturating_add(text.len());
                        *state.enum_digests[id].get_or_insert_with(|| digest_enum_value(text))
                    }
                    StrOrHash::Hash(hash) => {
                        *charged = charged.saturating_add(16);
                        *state.enum_digests[id].get_or_insert_with(|| {
                            digest_two_string_object_with_hashes(
                                "_type",
                                StrOrHash::Text("EnumItem"),
                                "name",
                                StrOrHash::Hash(hash),
                            )
                        })
                    }
                })
            }
            11 | 13 => Ok(digest_fixed_floats_value(
                kind,
                &[read_f32(reader)?, read_f32(reader)?],
            )),
            12 | 15 => Ok(digest_fixed_floats_value(
                kind,
                &[read_f32(reader)?, read_f32(reader)?, read_f32(reader)?],
            )),
            14 | 18 => Ok(digest_fixed_floats_value(
                kind,
                &[
                    read_f32(reader)?,
                    read_f32(reader)?,
                    read_f32(reader)?,
                    read_f32(reader)?,
                ],
            )),
            16 => match cframe {
                CFrameGroupDecoder::Inline => {
                    let mut c = [0.0_f32; 12];
                    for slot in &mut c {
                        *slot = read_f32(reader)?;
                    }
                    Ok(digest_fixed_floats_value(16, &c))
                }
                CFrameGroupDecoder::RotationTable(rotations) => {
                    let pos = [read_f32(reader)?, read_f32(reader)?, read_f32(reader)?];
                    let rot_idx = reader.read_len("CFrame rotation index")?;
                    let rot = rotations
                        .get(rot_idx)
                        .with_context(|| format!("Invalid CFrame rotation index {rot_idx}"))?;
                    Ok(digest_cframe_f64_value(pos, rot))
                }
            },
            17 => {
                let target = decode_resolved_ref_target(reader, self.rows, prev_ref)?;
                let one_based = match opts.moved {
                    Some(m) => m.in_state(target).map_or(0, |idx| idx as u64 + 1),
                    None => target as u64 + 1,
                };
                Ok(digest_ref_one_based(one_based))
            }
            7 => {
                let save_pos = reader.position();
                if let Some(d) = try_digest_string_array_wire(reader, str_items, charged) {
                    return Ok(d);
                }
                reader.set_position(save_pos);
                self.digest_fallback_wire_value(reader, 7, row, state, opts, get_resolved_refs)
            }
            8 => {
                let save_pos = reader.position();
                if let Some(d) = try_digest_two_string_object_wire(
                    reader,
                    str_items,
                    opts.whole,
                    charged,
                    &mut state.pair_obj_digests,
                ) {
                    return Ok(d);
                }
                reader.set_position(save_pos);
                self.digest_fallback_wire_value(reader, 8, row, state, opts, get_resolved_refs)
            }
            _ => {
                let str_refs = get_resolved_refs()?;
                let scope = ValueScope::from_borrowed(str_refs, self.rows, &self.budget, None);
                let value = decode_raw_value_payload(reader, kind, scope, 0)?;
                if opts.whole && names_an_instance_by_id(&value) {
                    state.named_flags[row] = true;
                }
                Ok(match opts.moved.and_then(|m| m.value_in_state(&value)) {
                    Some(placed) => value_digest(&placed),
                    None => value_digest(&value),
                })
            }
        }
    }

    fn digest_fallback_wire_value<'b>(
        &self,
        reader: &mut BytecodeReader<'_>,
        kind: u8,
        row: usize,
        state: &mut StreamDigestState,
        opts: DigestBlockOpts<'_>,
        get_resolved_refs: &dyn Fn() -> Result<&'b [&'b str]>,
    ) -> Result<[u8; 16]> {
        use super::captured::value_digest;
        let str_refs = get_resolved_refs()?;
        let scope = ValueScope::from_borrowed(str_refs, self.rows, &self.budget, None);
        let value = decode_raw_value_payload(reader, kind, scope, 0)?;
        if opts.whole && names_an_instance_by_id(&value) {
            state.named_flags[row] = true;
        }
        Ok(match opts.moved.and_then(|m| m.value_in_state(&value)) {
            Some(placed) => value_digest(&placed),
            None => value_digest(&value),
        })
    }

    /// `rows` in rising order and each once, every one a row of the store.
    fn chosen(&self, rows: &[usize]) -> Result<Vec<usize>> {
        let mut rows = rows.to_vec();
        rows.sort_unstable();
        rows.dedup();
        if let Some(row) = rows.last().filter(|row| **row >= self.rows) {
            bail!("The store has no instance {row}");
        }
        Ok(rows)
    }

    fn block_of(&self, row: usize) -> usize {
        self.blocks.partition_point(|block| block.rows().end <= row)
    }

    /// `rows`, every row of the store, as the rows of each block.
    fn split<'r, T>(&self, mut rows: &'r mut [T]) -> Vec<&'r mut [T]> {
        self.blocks
            .iter()
            .map(|block| {
                let (part, rest) = std::mem::take(&mut rows).split_at_mut(block.count);
                rows = rest;
                part
            })
            .collect()
    }

    /// The bytes of a frame, which match their digest.
    fn frame_bytes(&self, frame: Frame) -> Result<&[u8]> {
        let bytes = &self.bytes[frame.at..frame.at + frame.len];
        anyhow::ensure!(
            digest(&[bytes]) == frame.digest,
            "A frame of the block store does not match its digest"
        );
        Ok(bytes)
    }

    /// A frame of this file as it stands, to be written again.
    fn raw(&self, frame: Frame) -> Result<Section<'_>> {
        Ok(Section {
            decoded: frame.decoded,
            digest: frame.digest,
            bytes: Cow::Borrowed(self.frame_bytes(frame)?),
        })
    }

    /// Verifies `frame` once and returns its raw compressed bytes if its decoded content
    /// equals `expected`, reusing `codec`'s zstd decompressor.
    fn matches_content_with(
        &self,
        frame: Frame,
        expected: &[u8],
        codec: &mut FrameCodec,
    ) -> Result<Option<&[u8]>> {
        let bytes = self.frame_bytes(frame)?;
        anyhow::ensure!(
            zstd::zstd_safe::find_frame_compressed_size(bytes) == Ok(bytes.len()),
            "A frame of the block store is not one zstd frame"
        );
        anyhow::ensure!(
            bytes[..4] == ZSTD_MAGIC && bytes[4] & 4 != 0,
            "A frame of the block store carries no checksum of its content"
        );
        anyhow::ensure!(
            matches!(
                zstd::zstd_safe::get_frame_content_size(bytes),
                Ok(Some(size)) if size == frame.decoded as u64
            ),
            "A frame of the block store does not state the length its head gives"
        );
        #[cfg(test)]
        DECOMPRESSED.fetch_add(frame.decoded, Ordering::Relaxed);
        self.budget.take(frame.decoded)?;
        let most = FRAME_READ_START.max(bytes.len().saturating_mul(FRAME_READ_RATIO));
        let content = if frame.decoded <= most {
            codec.decompress_exact(bytes, frame.decoded)?
        } else {
            let mut content = Vec::new();
            content.reserve_exact(most);
            zstd::stream::read::Decoder::with_buffer(bytes).and_then(|decoder| {
                let mut decoder = decoder.take(frame.decoded as u64 + 1);
                decoder.read_to_end(&mut content).map(drop)
            })?;
            content
        };
        #[cfg(test)]
        self.reserved
            .fetch_max(content.capacity(), Ordering::Relaxed);
        anyhow::ensure!(
            content.len() == frame.decoded,
            "A frame of the block store holds {} bytes, not the {} its head gives",
            content.len(),
            frame.decoded
        );
        Ok((content == expected).then_some(bytes))
    }

    /// What a frame holds, charged to the store's budget. A frame that does
    /// not state the length its head gives is refused before any of it is
    /// read; see `FRAME_READ_START` for what its read reserves.
    fn content(&self, frame: Frame) -> Result<Vec<u8>> {
        let bytes = self.frame_bytes(frame)?;
        anyhow::ensure!(
            zstd::zstd_safe::find_frame_compressed_size(bytes) == Ok(bytes.len()),
            "A frame of the block store is not one zstd frame"
        );
        anyhow::ensure!(
            bytes[..4] == ZSTD_MAGIC && bytes[4] & 4 != 0,
            "A frame of the block store carries no checksum of its content"
        );
        anyhow::ensure!(
            matches!(
                zstd::zstd_safe::get_frame_content_size(bytes),
                Ok(Some(size)) if size == frame.decoded as u64
            ),
            "A frame of the block store does not state the length its head gives"
        );
        #[cfg(test)]
        DECOMPRESSED.fetch_add(frame.decoded, Ordering::Relaxed);
        self.budget.take(frame.decoded)?;
        let most = FRAME_READ_START.max(bytes.len().saturating_mul(FRAME_READ_RATIO));
        let mut content = Vec::new();
        let read = if frame.decoded <= most {
            zstd::bulk::decompress(bytes, frame.decoded).map(|read| content = read)
        } else {
            content.reserve_exact(most);
            zstd::stream::read::Decoder::with_buffer(bytes).and_then(|decoder| {
                let mut decoder = decoder.take(frame.decoded as u64 + 1);
                decoder.read_to_end(&mut content).map(drop)
            })
        };
        #[cfg(test)]
        self.reserved
            .fetch_max(content.capacity(), Ordering::Relaxed);
        read?;
        anyhow::ensure!(
            content.len() == frame.decoded,
            "A frame of the block store holds {} bytes, not the {} its head gives",
            content.len(),
            frame.decoded
        );
        Ok(content)
    }

    /// The frames that hold the pool entries `entries`, each read at most once
    /// for this reader.
    fn pool_read(&self, entries: impl IntoIterator<Item = usize>) -> Result<PoolRead> {
        let mut needed = vec![false; self.pool.len()];
        for entry in entries {
            needed[self.frame_of(entry)] = true;
        }
        needed
            .into_par_iter()
            .enumerate()
            .map(|(index, needed)| needed.then(|| self.pool_frame(index)).transpose())
            .collect()
    }

    fn frame_of(&self, entry: usize) -> usize {
        self.slots[entry].0
    }

    fn entry<'p>(&self, read: &'p PoolRead, entry: usize) -> Result<&'p str> {
        let (frame, place) = self.slots[entry];
        read.get(frame)
            .and_then(Option::as_ref)
            .and_then(|texts| texts.get(place))
            .map(String::as_str)
            .with_context(|| format!("Pool entry {entry} was not read"))
    }

    /// The entries of pool frame `index`, read once for this reader.
    fn pool_frame(&self, index: usize) -> Result<Arc<Vec<String>>> {
        let mut read = self.read_pool[index].lock_recover();
        if let Some(texts) = &*read {
            return Ok(Arc::clone(texts));
        }
        let texts = Arc::new(self.pool_texts(index)?);
        *read = Some(Arc::clone(&texts));
        Ok(texts)
    }

    /// The entries of pool frame `index`, read from the file.
    fn pool_texts(&self, index: usize) -> Result<Vec<String>> {
        let frame = &self.pool[index];
        let content = self.content(frame.frame)?;
        let mut reader = BytecodeReader::new(&content);
        let mut texts = Vec::with_capacity(reader.capacity(frame.entries.len(), 1));
        for _ in 0..frame.entries.len() {
            let tag = reader.read_len("pool entry")?;
            let (len, binary) = (tag >> 1, tag & 1 == 1);
            check_settings_string_len(if binary { len.div_ceil(3) * 4 } else { len })?;
            let bytes = reader.read_bytes(len)?;
            let text = if binary {
                base64::encode(bytes)
            } else {
                String::from_utf8(bytes.to_vec()).context("Invalid UTF-8 pool entry")?
            };
            anyhow::ensure!(
                text.len() >= POOL_MIN_BYTES,
                "A pool entry is shorter than the pool holds"
            );
            texts.push(text);
        }
        reader.finish()?;
        let hashes = frame.entries.iter().map(|entry| &self.hashes[*entry]);
        anyhow::ensure!(
            (texts.iter().zip(hashes)).all(|(text, hash)| digest(&[text.as_bytes()]) == *hash),
            "A pool entry of the block store does not match its hash"
        );
        Ok(texts)
    }

    fn open_blocks(&self, blocks: Range<usize>) -> Result<Vec<OpenBlock>> {
        blocks
            .into_par_iter()
            .map(|index| self.open_block(index))
            .collect()
    }

    fn open_block(&self, index: usize) -> Result<OpenBlock> {
        let content = self.content(self.blocks[index].frame)?;
        let mut reader = BytecodeReader::new(&content);
        let count = reader.read_collection_len("string count")?;
        let mut strings = Vec::with_capacity(reader.capacity(count, 1));
        for _ in 0..count {
            let tag = reader.read_len("string")?;
            let (index, pooled) = (tag >> 1, tag & 1 == 1);
            if pooled {
                anyhow::ensure!(index == 0, "Invalid pooled string tag {tag}");
                let hash = read_digest(&mut reader)?;
                let entry = (self.hashes.binary_search(&hash).ok())
                    .context("A value block names a pool entry the pool lacks")?;
                strings.push(BlockString::Pool(entry));
                continue;
            }
            anyhow::ensure!(
                index < POOL_MIN_BYTES,
                "A value block holds a string the pool should hold"
            );
            let text = reader.read_bytes(index)?.to_vec();
            strings.push(BlockString::Own(
                String::from_utf8(text).context("Invalid UTF-8 settings string")?,
            ));
        }
        let count = reader.read_collection_len("property count")?;
        let mut properties = Vec::with_capacity(reader.capacity(count, 1));
        for _ in 0..count {
            let id = reader.read_len("property string id")?;
            anyhow::ensure!(id < strings.len(), "Invalid property string id {id}");
            properties.push(id);
        }
        let rest = reader.position();
        Ok(OpenBlock {
            content,
            strings,
            properties,
            rest,
        })
    }

    /// The name and kind of each property group of an opened block, each
    /// pair once, as the v11 groups name them; a name the pool holds is read
    /// from it. The names are copied on the budget.
    fn group_heads(&self, open: &OpenBlock) -> Result<Vec<(String, u8)>> {
        let pooled = open
            .properties
            .iter()
            .filter_map(|id| match open.strings[*id] {
                BlockString::Pool(entry) => Some(entry),
                BlockString::Own(_) => None,
            });
        let read = self.pool_read(pooled)?;
        let names = (open.properties.iter())
            .map(|id| match &open.strings[*id] {
                BlockString::Own(text) => Ok(text.as_str()),
                BlockString::Pool(entry) => self.entry(&read, *entry),
            })
            .collect::<Result<Vec<_>>>()?;
        let copied = names.iter().map(|name| name.len());
        self.budget.take(copied.fold(0, usize::saturating_add))?;
        let names = names.into_iter().map(str::to_string).collect::<Vec<_>>();
        let mut reader = BytecodeReader::new(&open.content[open.rest..]);
        let groups = read_settings_property_groups(&mut reader, &names)?;
        let heads = (groups.into_iter())
            .map(|(name, kind, ..)| (name.as_str(), kind))
            .collect::<AHashSet<_>>();
        let copied = heads.iter().map(|(name, _)| name.len());
        self.budget.take(copied.fold(0, usize::saturating_add))?;
        Ok((heads.into_iter())
            .map(|(name, kind)| (name.to_string(), kind))
            .collect())
    }

    /// The values of every row of block `index`, of the rows `keep` takes:
    /// the block is read before its rows are made.
    fn block_values(
        &self,
        index: usize,
        keep: impl Fn(usize) -> bool + Sync,
        texts: Texts<'_>,
    ) -> Result<Vec<DecodedRowValues>> {
        let open = self.open_block(index)?;
        let mut values = (0..self.blocks[index].count)
            .map(|_| DecodedRowValues::default())
            .collect::<Vec<_>>();
        self.block_into(&open, &mut values, keep, texts)?;
        Ok(values)
    }

    /// Gives `rows`, the rows of an opened block in order, the values the
    /// block holds for the rows `keep` takes, with `texts` for the pool
    /// entries they name.
    fn block_into<T: DecodedPropertyTarget>(
        &self,
        open: &OpenBlock,
        rows: &mut [T],
        keep: impl Fn(usize) -> bool + Sync,
        texts: Texts<'_>,
    ) -> Result<()> {
        self.values(open, rows, keep, |_, _| true, texts)
    }

    /// Whether the references block `index`, opened as `open`, holds are
    /// written as they stand once the rows stand where `to` puts them: each
    /// by position names the row it named, and none names an instance of the
    /// store of the root `root_service` by an id or a path, which a write
    /// finds again in the store as the edit leaves it (`references_alike`):
    /// a namesake removed, renamed or moved ahead of an instance changes what
    /// a path with ordinals names. Its rows then keep their values as they
    /// are.
    fn references_stay(
        &self,
        (open, index): (&OpenBlock, usize),
        to: &[Option<usize>],
        root_service: Option<&str>,
    ) -> Result<bool> {
        let mut rows = (0..self.blocks[index].count)
            .map(|_| DecodedRowValues::default())
            .collect::<Vec<_>>();
        let referring = |_: &str, kind: u8| REFERRING_KINDS.contains(&kind);
        self.values(open, &mut rows, |_| true, referring, Texts::Marked)?;
        let mut values = (rows.iter_mut()).flat_map(|row| {
            row.properties
                .values_mut()
                .chain(row.attributes.values_mut())
        });
        Ok(!values.any(|value| {
            remap_ref_indices_in_value(value, to) || !references_alike(value, root_service)
        }))
    }

    pub(crate) fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub(crate) fn visit_block_raw<R>(
        &self,
        index: usize,
        visit: impl FnOnce(
            usize,
            &[Vec<(&str, RawBlockValue<'_>)>],
            &[Option<Map<String, Value>>],
        ) -> Result<R>,
    ) -> Result<R> {
        let block = &self.blocks[index];
        let open = self.open_block(index)?;
        let read = self.pool_read(open.pool_entries())?;
        let table = open
            .strings
            .iter()
            .map(|string| match string {
                BlockString::Own(text) => Ok(text.as_str()),
                BlockString::Pool(entry) => self.entry(&read, *entry),
            })
            .collect::<Result<Vec<_>>>()?;
        let property_names = open.properties.iter().map(|id| table[*id]);
        self.budget.take(
            table
                .iter()
                .copied()
                .chain(property_names)
                .map(str::len)
                .fold(0, usize::saturating_add),
        )?;
        let properties = open
            .properties
            .iter()
            .map(|id| table[*id].to_string())
            .collect::<Vec<_>>();
        let mut reader = BytecodeReader::new(&open.content[open.rest..]);
        let specs = read_settings_property_groups(&mut reader, &properties)?;
        reader.finish()?;
        let scope = ValueScope::from_borrowed(&table, self.rows, &self.budget, None);
        let mut row_props: Vec<Vec<(&str, RawBlockValue<'_>)>> =
            (0..block.count).map(|_| Vec::new()).collect();
        let mut row_attrs: Vec<Option<Map<String, Value>>> =
            (0..block.count).map(|_| None).collect();
        let mut seen_names = AHashSet::with_capacity(specs.len());
        let mut has_dup = false;
        for &(name, ..) in &specs {
            if !seen_names.insert(name.as_str()) {
                has_dup = true;
                break;
            }
        }
        for &(name, kind, count, body) in &specs {
            let prop_name = name.as_str();
            match decode_property_group_raw(body, prop_name, kind, count, block.count, scope)? {
                DecodedRawGroup::Properties(name, entries) => {
                    if has_dup {
                        for (row, value) in entries {
                            if let Some(existing) =
                                row_props[row].iter_mut().find(|(k, _)| *k == name)
                            {
                                existing.1 = value;
                            } else {
                                row_props[row].push((name, value));
                            }
                        }
                    } else {
                        for (row, value) in entries {
                            row_props[row].push((name, value));
                        }
                    }
                }
                DecodedRawGroup::Attributes(entries) => {
                    for (row, attrs) in entries {
                        row_attrs[row] = Some(attrs);
                    }
                }
            }
        }
        visit(block.first, &row_props, &row_attrs)
    }

    pub(crate) fn renew_budget(&self) {
        self.budget.renew(self.bytes.len());
    }

    /// Gives `rows`, the rows of one block in order, the values the block
    /// holds for the rows `keep` takes of the property groups `groups` takes
    /// by name and kind, with `texts` for the pool entries they name; a
    /// property name is always its own text. The table and the property
    /// names are copied on the budget, as v11 copies its property names,
    /// before any copy is made.
    fn values<T: DecodedPropertyTarget>(
        &self,
        open: &OpenBlock,
        rows: &mut [T],
        keep: impl Fn(usize) -> bool + Sync,
        groups: impl Fn(&str, u8) -> bool + Sync,
        texts: Texts<'_>,
    ) -> Result<()> {
        let all = matches!(texts, Texts::All);
        let named = open.properties.iter().copied().collect::<AHashSet<_>>();
        let pooled = |id: usize, string: &BlockString| match string {
            BlockString::Pool(entry) if all || named.contains(&id) => Some(*entry),
            _ => None,
        };
        let read = self.pool_read(
            (open.strings.iter().enumerate()).filter_map(|(id, string)| pooled(id, string)),
        )?;
        let table = (open.strings.iter().enumerate())
            .map(|(id, string)| match (string, pooled(id, string)) {
                (BlockString::Own(text), _) => Ok(Cow::Borrowed(text.as_str())),
                (_, Some(entry)) => self.entry(&read, entry).map(Cow::Borrowed),
                (BlockString::Pool(entry), None) => Ok(Cow::Owned(marker(&self.hashes[*entry]))),
            })
            .collect::<Result<Vec<_>>>()?;
        let property_names = open.properties.iter().map(|id| &table[*id]);
        self.budget.take(
            (table.iter().chain(property_names))
                .map(|text| text.len())
                .fold(0, usize::saturating_add),
        )?;
        let string_refs = table.iter().map(Cow::as_ref).collect::<Vec<_>>();
        let properties = (open.properties.iter())
            .map(|id| string_refs[*id].to_string())
            .collect::<Vec<_>>();
        let mut reader = BytecodeReader::new(&open.content[open.rest..]);
        let specs = read_settings_property_groups(&mut reader, &properties)?;
        reader.finish()?;
        let specs = specs
            .into_iter()
            .filter(|(name, kind, ..)| groups(name, *kind))
            .collect::<Vec<_>>();
        // A marker is no payload: a store that holds payloads holds them once
        // the markers of the rows read have their texts. The decode charges
        // the store's own budget as it goes, so blocks read in parallel
        // together take no more than it holds.
        let marked_budget = (!all).then(|| self.budget.not_holding());
        let budget = marked_budget.as_ref().unwrap_or(&self.budget);
        let held_ids = budget.holds_payloads.then(|| {
            (0..string_refs.len())
                .map(|_| std::sync::atomic::AtomicU64::new(u64::MAX))
                .collect::<Vec<_>>()
        });
        let scope = ValueScope::from_borrowed(&string_refs, self.rows, budget, held_ids.as_deref());
        let held = rows.len();
        let groups = specs
            .par_iter()
            .map(|&(name, kind, count, body)| {
                decode_property_group_body_filtered(
                    body,
                    name,
                    kind,
                    count,
                    scope,
                    Some(held),
                    &keep,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        apply_decoded_property_groups_in_range(rows, 0, &specs, groups, 256);
        if all {
            return Ok(());
        }
        // A key stands where its text orders its object, so it is never a
        // marker.
        let text = |hash: [u8; 16], key: bool| -> Result<Option<String>> {
            if !key && !matches!(texts, Texts::Resolved(real) if real(&hash)) {
                return Ok(None);
            }
            self.pool_text(&hash).map(Some)
        };
        let has_pool = open.pool_entries().next().is_some();
        if has_pool || self.budget.holds_payloads {
            let resolve_shared =
                has_pool && matches!(texts, Texts::Resolved(_)) && self.budget.holds_payloads;
            let shared_handles: Mutex<AHashMap<[u8; 16], Value>> = Mutex::new(AHashMap::new());
            (rows.par_iter_mut().enumerate())
                .filter(|(row, _)| keep(*row))
                .try_for_each(|(_, row)| {
                    let (properties, attributes) = row.maps();
                    if has_pool {
                        if resolve_shared {
                            for value in properties.values_mut() {
                                if let Some(obj) = value.as_object()
                                    && obj.len() == 2
                                    && obj.get("_type").and_then(Value::as_str)
                                        == Some("SharedString")
                                    && let Some(b64) = obj.get("base64").and_then(Value::as_str)
                                    && let Some(hash) = marked(b64)
                                    && matches!(texts, Texts::Resolved(real) if real(&hash))
                                {
                                    let cached = shared_handles.lock_recover().get(&hash).cloned();
                                    let handle = match cached {
                                        Some(h) => h,
                                        None => {
                                            let real = self.pool_text(&hash)?;
                                            let h = self.budget.property(json!({
                                                "_type": "SharedString",
                                                "base64": real,
                                            }));
                                            shared_handles.lock_recover().insert(hash, h.clone());
                                            h
                                        }
                                    };
                                    *value = handle;
                                    continue;
                                }
                                unmark_value(value, &text)?;
                            }
                        } else {
                            unmark(properties, &text)?;
                        }
                        unmark(attributes, &text)?;
                    }
                    if self.budget.holds_payloads {
                        for value in properties.values_mut() {
                            if let Some(obj) = value.as_object()
                                && obj.len() == 2
                                && !crate::settings::held::is_handle(obj)
                                && obj.get("_type").and_then(Value::as_str) == Some("SharedString")
                            {
                                *value = self.budget.property(std::mem::take(value));
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })?;
        }
        Ok(())
    }

    /// The text of the pool entry of hash `hash`, copied on the budget.
    fn pool_text(&self, hash: &[u8; 16]) -> Result<String> {
        let (frame, place) = self.slots[self.entry_of(hash)?];
        let text = self.pool_frame(frame)?[place].clone();
        self.budget.take(text.len())?;
        Ok(text)
    }

    /// Gives `each`, in rising order, each row at `rows` with its values of
    /// the properties `names` takes, as the whole decode gives them, until
    /// `each` gives false. The blocks of those rows are decoded `at_once` at
    /// a time with a marker for each pool entry, and a row's markers take
    /// their texts as the row is given. A call is a pass on a budget of its
    /// own, as a whole decode is.
    fn read(
        &self,
        rows: &[usize],
        at_once: usize,
        names: impl Fn(&str) -> bool + Sync,
        mut each: impl FnMut(usize, DecodedRowValues) -> Result<bool>,
    ) -> Result<()> {
        self.budget.renew(self.bytes.len());
        let rows = self.chosen(rows)?;
        let blocks = rows
            .chunk_by(|left, right| self.block_of(*left) == self.block_of(*right))
            .collect::<Vec<_>>();
        let text = |hash: [u8; 16], _| self.pool_text(&hash).map(Some);
        for few in blocks.chunks(at_once) {
            let read = few
                .par_iter()
                .map(|rows| {
                    let index = self.block_of(rows[0]);
                    let first = self.blocks[index].first;
                    let open = self.open_block(index)?;
                    let mut values = (0..self.blocks[index].count)
                        .map(|_| DecodedRowValues::default())
                        .collect::<Vec<_>>();
                    let keep = |row: usize| rows.binary_search(&(first + row)).is_ok();
                    let named = |name: &str, _| names(name);
                    self.values(&open, &mut values, keep, named, Texts::Marked)?;
                    Ok((first, values))
                })
                .collect::<Result<Vec<_>>>()?;
            for ((first, mut values), rows) in read.into_iter().zip(few) {
                for row in *rows {
                    let mut given = std::mem::take(&mut values[row - first]);
                    unmark(&mut given.properties, &text)?;
                    unmark(&mut given.attributes, &text)?;
                    if !each(*row, given)? {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}

/// Gives each marker among the keys and texts of `map` the text `text` gives
/// it, told whether it is a key, where it gives one.
fn unmark(
    map: &mut Map<String, Value>,
    text: &impl Fn([u8; 16], bool) -> Result<Option<String>>,
) -> Result<()> {
    for value in map.values_mut() {
        unmark_value(value, text)?;
    }
    if map.keys().any(|key| marked(key).is_some()) {
        for (key, value) in std::mem::take(map) {
            let key = match marked(&key) {
                Some(hash) => text(hash, true)?.unwrap_or(key),
                None => key,
            };
            map.insert(key, value);
        }
    }
    Ok(())
}

fn unmark_value(
    value: &mut Value,
    text: &impl Fn([u8; 16], bool) -> Result<Option<String>>,
) -> Result<()> {
    match value {
        Value::String(held) => {
            if let Some(hash) = marked(held)
                && let Some(real) = text(hash, false)?
            {
                *held = real;
            }
        }
        Value::Array(items) => {
            for item in items {
                unmark_value(item, text)?;
            }
        }
        Value::Object(map) => unmark(map, text)?,
        _ => {}
    }
    Ok(())
}

/// Whether `value` holds, at any depth, a text that stands for a pool entry
/// (`Texts::Marked`).
pub(crate) fn holds_marker(value: &Value) -> bool {
    match value {
        Value::String(text) => marked(text).is_some(),
        Value::Array(items) => items.iter().any(holds_marker),
        Value::Object(map) => map.values().any(holds_marker),
        _ => false,
    }
}

impl Block {
    fn rows(&self) -> Range<usize> {
        self.first..self.first + self.count
    }
}

impl OpenBlock {
    fn pool_entries(&self) -> impl Iterator<Item = usize> + '_ {
        self.strings.iter().filter_map(|string| match string {
            BlockString::Pool(index) => Some(*index),
            BlockString::Own(_) => None,
        })
    }
}

/// `decode_if_contains_any_string_set` of a v12 file. A string counts where
/// the structure, a block or a pool entry a block names holds it.
pub(crate) fn decode_if_contains(
    bytes: &[u8],
    required: &[Vec<String>],
) -> Result<Option<SettingsBytecode>> {
    let store = BlockStore::open(bytes)?;
    let (structure, opened) = rayon::join(
        || store.content(store.structure),
        || store.open_blocks(0..store.blocks.len()),
    );
    let (structure, opened) = (structure?, opened?);
    if !required.is_empty() {
        let named = opened
            .iter()
            .flat_map(OpenBlock::pool_entries)
            .collect::<AHashSet<_>>();
        let read = store.pool_read(named.iter().copied())?;
        let holds = |needle: &String| {
            std::iter::once(&structure)
                .chain(opened.iter().map(|open| &open.content))
                .any(|content| memmem::find(content, needle.as_bytes()).is_some())
                || named.iter().any(|entry| {
                    store
                        .entry(&read, *entry)
                        .is_ok_and(|text| text.contains(needle.as_str()))
                })
        };
        if !required.iter().any(|needles| needles.iter().all(holds)) {
            return Ok(None);
        }
    }
    store.decode_opened(structure, opened).map(Some)
}

/// Whether any reference in a v12 block store has `pathSegments` matching
/// `matches`, scanning only blocks whose string table contains every exact
/// string of at least one set in `required` and decoding only property groups
/// that can hold a path reference.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn has_reference_path_matching(
    bytes: &[u8],
    required: &[Vec<String>],
    matches: impl Fn(&[String]) -> bool + Sync,
) -> Result<bool> {
    let _span = crate::app::profile::span("store reference path scan");
    let store = BlockStore::open(bytes)?;
    let long_hashes: AHashMap<&str, [u8; 16]> = required
        .iter()
        .flatten()
        .filter(|needle| needle.len() >= POOL_MIN_BYTES)
        .map(|needle| (needle.as_str(), digest(&[needle.as_bytes()])))
        .collect();
    let found = std::sync::atomic::AtomicBool::new(false);
    let indices = (0..store.blocks.len()).collect::<Vec<_>>();
    for batch in indices.chunks(STANDING_BATCH) {
        if found.load(Ordering::Relaxed) {
            return Ok(true);
        }
        store.budget.renew(store.bytes.len());
        batch.par_iter().try_for_each(|&index| -> Result<()> {
            if found.load(Ordering::Relaxed) {
                return Ok(());
            }
            let open = store.open_block(index)?;
            if !required.is_empty() {
                let mut own = AHashSet::with_capacity(open.strings.len());
                let mut pooled = AHashSet::new();
                for string in &open.strings {
                    match string {
                        BlockString::Own(text) => {
                            own.insert(text.as_str());
                        }
                        BlockString::Pool(entry) => {
                            pooled.insert(store.hashes[*entry]);
                        }
                    }
                }
                let block_matches = required.iter().any(|set| {
                    set.iter()
                        .all(|needle| match long_hashes.get(needle.as_str()) {
                            Some(hash) => pooled.contains(hash),
                            None => own.contains(needle.as_str()),
                        })
                });
                if !block_matches {
                    return Ok(());
                }
            }
            let block = &store.blocks[index];
            let mut values = (0..block.count)
                .map(|_| DecodedRowValues::default())
                .collect::<Vec<_>>();
            let ref_groups =
                |name: &str, kind: u8| matches!(kind, 7 | 8 | 10) && name != "UniqueId";
            store.values(&open, &mut values, |_| true, ref_groups, Texts::Marked)?;
            let mut scan_error: Option<anyhow::Error> = None;
            for value in values
                .iter()
                .flat_map(|row| row.properties.values().chain(row.attributes.values()))
            {
                if found.load(Ordering::Relaxed) || scan_error.is_some() {
                    break;
                }
                for_each_reference(value, &mut |reference| {
                    if found.load(Ordering::Relaxed) || scan_error.is_some() {
                        return;
                    }
                    let Some(raw_segments) =
                        reference.get("pathSegments").and_then(Value::as_array)
                    else {
                        return;
                    };
                    let mut segments = Vec::with_capacity(raw_segments.len());
                    for seg in raw_segments {
                        let Some(text) = seg.as_str() else {
                            return;
                        };
                        if let Some(hash) = marked(text) {
                            match store.pool_text(&hash) {
                                Ok(resolved) => segments.push(resolved),
                                Err(error) => {
                                    scan_error = Some(error);
                                    return;
                                }
                            }
                        } else {
                            segments.push(text.to_string());
                        }
                    }
                    if matches(&segments) {
                        found.store(true, Ordering::Relaxed);
                    }
                });
            }
            if let Some(error) = scan_error {
                return Err(error);
            }
            Ok(())
        })?;
    }
    Ok(found.load(Ordering::Relaxed))
}

/// Whether a whole decode refuses the file at `path` where a read of some
/// of its rows may not: a store in blocks with a frame that is not as its
/// head says, or a file that cannot be read. A command that read its store
/// whole before it read rows still decodes such a file whole.
pub(crate) fn damaged(path: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return true;
    };
    let intact = || BlockStore::open(bytes.as_slice()).is_ok_and(|store| store.intact());
    is_block_store(&bytes) && !intact()
}

/// The file a whole encode gives of the document a v12 file holds, written
/// from a few blocks at a time and never from the document. `None` for a
/// file another writer wrote or one cut into other blocks than this writer
/// cuts, which is decoded whole.
pub(crate) fn encode_again(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    encode_again_cut(bytes, BLOCK_ROWS)
}

/// `encode_again` for a whole encode in blocks of `block_rows` rows.
pub(crate) fn encode_again_cut(bytes: &[u8], block_rows: usize) -> Result<Option<Vec<u8>>> {
    let store = BlockStore::open(bytes)?;
    if !store.cut_in(block_rows) {
        return Ok(None);
    }
    let rows = (0..store.rows).map(Some).collect::<Vec<_>>();
    let placed = Placed {
        from: &rows,
        to: &rows,
        held: &vec![false; store.rows],
    };
    let written = store.rewrite(&store.structure()?, placed, &[], (true, false))?;
    Ok(Some(written))
}

/// `property_edit::decode_rows` of a v12 file: the instances at `rows`, each
/// as the whole decode gives it, and the id of every instance.
pub(crate) fn decode_rows(
    bytes: &[u8],
    rows: &[usize],
) -> Result<(Vec<SettingsBytecodeInstance>, Vec<String>)> {
    let store = BlockStore::open(bytes)?;
    let structure = store.structure()?.instances;
    let instances = rows_of(&store, &structure, rows)?;
    let ids = structure.into_iter().map(|row| row.settings_id);
    Ok((instances, ids.collect()))
}

/// The instances at `rows` of `store`, whose instances without their values
/// are `structure`.
fn rows_of(
    store: &BlockStore<'_>,
    structure: &[SettingsBytecodeInstance],
    rows: &[usize],
) -> Result<Vec<SettingsBytecodeInstance>> {
    let read = store.rows(rows)?;
    let whole = |row: usize, properties, attributes| SettingsBytecodeInstance {
        properties,
        attributes,
        ..structure[row].bare()
    };
    if rows.windows(2).all(|pair| pair[0] < pair[1]) {
        let read = read.into_iter();
        return Ok(read
            .map(|(row, values)| whole(row, values.properties, values.attributes))
            .collect());
    }
    let read = read.into_iter().collect::<AHashMap<_, _>>();
    let rows = rows.iter();
    Ok(rows
        .map(|row| {
            whole(
                *row,
                read[row].properties.clone(),
                read[row].attributes.clone(),
            )
        })
        .collect())
}

/// `decode_rows` of one v12 file a batch of rows at a time: its head, pool cache,
/// and instances are kept across all batches, and each batch runs on a renewed budget.
pub(crate) struct RowBatches<'a> {
    store: BlockStore<'a>,
    structure: Vec<SettingsBytecodeInstance>,
}

impl<'a> RowBatches<'a> {
    pub(crate) fn open_holding(bytes: &'a [u8]) -> Result<Self> {
        let store = BlockStore::open(bytes)?.holding();
        let structure = store.structure()?.instances;
        Ok(Self { store, structure })
    }

    pub(crate) fn from_opened(
        store: BlockStore<'a>,
        structure: Vec<SettingsBytecodeInstance>,
    ) -> Self {
        Self { store, structure }
    }

    pub(crate) fn block_of(&self, row: usize) -> usize {
        self.store.block_of(row)
    }

    pub(crate) fn structure(&self) -> &[SettingsBytecodeInstance] {
        &self.structure
    }

    pub(crate) fn into_store(self) -> BlockStore<'a> {
        let Self { store, structure } = self;
        if structure.len() >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES {
            structure.into_par_iter().for_each(drop);
        }
        store
    }

    /// The id of every instance of the file.
    pub(crate) fn ids(&self) -> Vec<String> {
        if self.structure.len() >= SETTINGS_BINARY_PARALLEL_MIN_INSTANCES {
            self.structure
                .par_iter()
                .map(|row| row.settings_id.clone())
                .collect()
        } else {
            self.structure
                .iter()
                .map(|row| row.settings_id.clone())
                .collect()
        }
    }

    pub(crate) fn read(&self, rows: &[usize]) -> Result<Vec<SettingsBytecodeInstance>> {
        self.store.budget.renew(self.store.bytes.len());
        rows_of(&self.store, &self.structure, rows)
    }

    pub(super) fn fast_compare_changed(
        &self,
        state: &ServiceState,
        records: &[SettingsBinaryInstance<'_>],
        lookup: &SettingsBinaryInstanceLookup<'_>,
        (changed, stored_at, moved): (&[usize], &[usize], Option<&super::captured::Moved>),
        root_service: Option<&str>,
        long_hashes: Option<&super::captured::LongHashMap>,
    ) -> Result<Vec<FastCompareOutcome>> {
        self.store.budget.renew(self.store.bytes.len());
        self.store.fast_compare_changed(FastCompareInput {
            structure: &self.structure,
            state,
            records,
            lookup,
            changed,
            stored_at,
            moved,
            root_service,
            long_hashes,
        })
    }
}

#[derive(Clone, Copy)]
enum FastCapVal<'a> {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(&'a str),
    Enum(&'a str),
    Vec2([f32; 2]),
    Vec3([f32; 3]),
    UDim([f32; 2]),
    UDim2([f32; 4]),
    Color3([f32; 3]),
    Rect([f32; 4]),
    CFrame(&'a [f32; 12]),
    Ref(usize),
    Payload { ty: &'a str, b64: &'a str },
    Json(&'a Value),
    Tags(&'a [Value]),
    SkipCompare,
}

#[inline]
fn is_known_default_fast(name: &str, val: FastCapVal<'_>) -> bool {
    matches!(
        (name, val),
        ("Archivable" | "CharacterAutoLoads", FastCapVal::Bool(true))
            | ("Sandboxed", FastCapVal::Bool(false))
    )
}

fn compare_number_default(
    a: f64,
    value: f64,
    uses_f32: bool,
    allow_nonfinite: bool,
) -> Option<bool> {
    use crate::settings::equivalence::{f32_values_definitely_differ, f32_values_match};
    if a == value || (uses_f32 && f32_values_match(a, value)) {
        Some(true)
    } else if f32_values_definitely_differ(a, value)
        || (!uses_f32 && (allow_nonfinite || a.is_finite()))
    {
        Some(false)
    } else {
        None
    }
}

fn compare_f32_slice_default(a: &[f32], b: &[f32]) -> Option<bool> {
    use crate::settings::equivalence::{f32_values_definitely_differ, f32_values_match};
    if a.iter()
        .zip(b)
        .all(|(&x, &y)| f32_values_match(f64::from(x), f64::from(y)))
    {
        Some(true)
    } else if a
        .iter()
        .zip(b)
        .any(|(&x, &y)| f32_values_definitely_differ(f64::from(x), f64::from(y)))
    {
        Some(false)
    } else {
        None
    }
}

fn fast_val_matches_one_sided(
    name: &str,
    val: FastCapVal<'_>,
    rule: &crate::settings::equivalence::OneSidedRule,
) -> Option<bool> {
    use crate::settings::equivalence::{
        FastDefaultVal, OneSidedRule, cframes_definitely_differ, cframes_match_reconciliation,
        f32_values_definitely_differ, f32_values_match,
    };

    if matches!(val, FastCapVal::SkipCompare) {
        return matches!(rule, OneSidedRule::Ignored).then_some(true);
    }
    match rule {
        OneSidedRule::Ignored => Some(true),
        OneSidedRule::NoDefault => Some(is_known_default_fast(name, val)),
        OneSidedRule::Default(def) => {
            if is_known_default_fast(name, val) {
                return Some(true);
            }
            match (val, def) {
                (FastCapVal::Bool(a), FastDefaultVal::Bool(b)) => Some(a == *b),
                (FastCapVal::F64(a), FastDefaultVal::Number { value, uses_f32 }) => {
                    compare_number_default(a, *value, *uses_f32, false)
                }
                (FastCapVal::I64(a), FastDefaultVal::Number { value, uses_f32 }) => {
                    compare_number_default(a as f64, *value, *uses_f32, true)
                }
                (FastCapVal::U64(a), FastDefaultVal::Number { value, uses_f32 }) => {
                    compare_number_default(a as f64, *value, *uses_f32, true)
                }
                (FastCapVal::Str(a), FastDefaultVal::Str(b)) => Some(a == b.as_str()),
                (FastCapVal::Enum(a), FastDefaultVal::Enum(b)) => {
                    if a == b.as_str() {
                        Some(true)
                    } else if a.parse::<u64>().is_err() && b.parse::<u64>().is_err() {
                        Some(false)
                    } else {
                        None
                    }
                }
                (FastCapVal::Vec2(a), FastDefaultVal::Vec2(b)) => compare_f32_slice_default(&a, b),
                (FastCapVal::Vec3(a), FastDefaultVal::Vec3(b)) => compare_f32_slice_default(&a, b),
                (FastCapVal::Rect(a), FastDefaultVal::Rect(b)) => compare_f32_slice_default(&a, b),
                (FastCapVal::UDim(a), FastDefaultVal::UDim(b)) => {
                    if f32_values_match(f64::from(a[0]), f64::from(b[0])) && a[1] == b[1] {
                        Some(true)
                    } else if f32_values_definitely_differ(f64::from(a[0]), f64::from(b[0]))
                        || f32_values_definitely_differ(f64::from(a[1]), f64::from(b[1]))
                    {
                        Some(false)
                    } else {
                        None
                    }
                }
                (FastCapVal::UDim2(a), FastDefaultVal::UDim2(b)) => {
                    if f32_values_match(f64::from(a[0]), f64::from(b[0]))
                        && a[1] == b[1]
                        && f32_values_match(f64::from(a[2]), f64::from(b[2]))
                        && a[3] == b[3]
                    {
                        Some(true)
                    } else if (0..4)
                        .any(|i| f32_values_definitely_differ(f64::from(a[i]), f64::from(b[i])))
                    {
                        Some(false)
                    } else {
                        None
                    }
                }
                (FastCapVal::Color3(a), FastDefaultVal::Color3(b)) => {
                    if (0..3).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i]))) {
                        Some(true)
                    } else if (0..3).any(|i| {
                        f32_values_definitely_differ(f64::from(a[i]), f64::from(b[i]))
                            && (a[i].clamp(0.0, 1.0) * 255.0).round() as u8
                                != (b[i].clamp(0.0, 1.0) * 255.0).round() as u8
                    }) {
                        Some(false)
                    } else {
                        None
                    }
                }
                (FastCapVal::CFrame(a), FastDefaultVal::CFrame(b)) => {
                    let a64 = a.map(f64::from);
                    if cframes_match_reconciliation(&a64, b) {
                        Some(true)
                    } else if cframes_definitely_differ(&a64, b) {
                        Some(false)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
    }
}

fn skip_raw_value_payload_fast(
    reader: &mut BytecodeReader<'_>,
    kind: u8,
    depth: usize,
) -> Result<()> {
    if depth > MAX_SETTINGS_VALUE_DEPTH {
        bail!("Settings bytecode value nesting exceeds safe depth of {MAX_SETTINGS_VALUE_DEPTH}");
    }
    match kind {
        0..=2 => Ok(()),
        3 | 4 | 6 | 17 | 19 => {
            reader.read_var_u64()?;
            Ok(())
        }
        5 => {
            reader.read_bytes(8)?;
            Ok(())
        }
        20 => {
            reader.read_bytes(4)?;
            Ok(())
        }
        11 | 13 => {
            reader.read_bytes(8)?;
            Ok(())
        }
        12 | 15 => {
            reader.read_bytes(12)?;
            Ok(())
        }
        14 | 18 => {
            reader.read_bytes(16)?;
            Ok(())
        }
        16 => {
            reader.read_bytes(48)?;
            Ok(())
        }
        9 => {
            let len = reader.read_len("numeric array length")?;
            let bytes = len
                .checked_mul(8)
                .context("Numeric array byte length overflow")?;
            reader.read_bytes(bytes)?;
            Ok(())
        }
        21 => {
            let len = reader.read_len("f32 numeric array length")?;
            let bytes = len
                .checked_mul(4)
                .context("f32 numeric array byte length overflow")?;
            reader.read_bytes(bytes)?;
            Ok(())
        }
        NESTED_ENUM_ITEM_KIND => {
            reader.read_var_u64()?;
            reader.read_var_u64()?;
            Ok(())
        }
        7 => {
            let len = reader.read_collection_len("array length")?;
            for _ in 0..len {
                let item_kind = reader.read_u8().context("Missing value kind")?;
                skip_raw_value_payload_fast(reader, item_kind, depth + 1)?;
            }
            Ok(())
        }
        8 => {
            let len = reader.read_collection_len("object field count")?;
            for _ in 0..len {
                reader.read_var_u64()?;
                let val_kind = reader.read_u8().context("Missing value kind")?;
                skip_raw_value_payload_fast(reader, val_kind, depth + 1)?;
            }
            Ok(())
        }
        10 => {
            let flags = reader.read_u8().context("Missing ref fallback flags")?;
            if flags & 1 != 0 {
                reader.read_var_u64()?;
            }
            if flags & 2 != 0 {
                reader.read_var_u64()?;
            }
            if flags & 4 != 0 {
                let len = reader.read_collection_len("ref path segment count")?;
                for _ in 0..len {
                    reader.read_var_u64()?;
                }
            }
            if flags & 8 != 0 {
                reader.read_var_u64()?;
            }
            if flags & 16 != 0 {
                reader.read_var_u64()?;
            }
            if flags & 32 != 0 {
                let len = reader.read_collection_len("ref path ordinal count")?;
                for _ in 0..len {
                    reader.read_var_u64()?;
                }
            }
            Ok(())
        }
        _ => bail!("Unknown settings value kind {kind}"),
    }
}

/// Scans a stored wire value payload in `fast_compare_changed` without allocating JSON.
/// Always consumes the complete payload from `reader` and returns `true` only when the
/// value is proven to contain no references, no `EnumItem`s, and nothing that fails
/// `values_encode_alike` (`references_alike`).
fn stored_value_is_safe(
    reader: &mut BytecodeReader<'_>,
    kind: u8,
    str_opts: &[Option<&str>],
    depth: usize,
) -> Result<bool> {
    if depth > MAX_SETTINGS_VALUE_DEPTH {
        bail!("Settings bytecode value nesting exceeds safe depth of {MAX_SETTINGS_VALUE_DEPTH}");
    }
    match kind {
        0..=2 => Ok(true),
        3 | 4 | 6 => {
            reader.read_var_u64()?;
            Ok(true)
        }
        5 | 11 | 13 => {
            reader.read_bytes(8)?;
            Ok(true)
        }
        20 => {
            reader.read_bytes(4)?;
            Ok(true)
        }
        12 | 15 => {
            reader.read_bytes(12)?;
            Ok(true)
        }
        14 | 18 => {
            reader.read_bytes(16)?;
            Ok(true)
        }
        16 => {
            reader.read_bytes(48)?;
            Ok(true)
        }
        9 => {
            let len = reader.read_len("numeric array length")?;
            let bytes = len
                .checked_mul(8)
                .context("Numeric array byte length overflow")?;
            reader.read_bytes(bytes)?;
            Ok(true)
        }
        21 => {
            let len = reader.read_len("f32 numeric array length")?;
            let bytes = len
                .checked_mul(4)
                .context("f32 numeric array byte length overflow")?;
            reader.read_bytes(bytes)?;
            Ok(true)
        }
        10 | 17 | 19 | NESTED_ENUM_ITEM_KIND => {
            skip_raw_value_payload_fast(reader, kind, depth)?;
            Ok(false)
        }
        7 => {
            let len = reader.read_collection_len("array length")?;
            let mut safe = true;
            for _ in 0..len {
                let item_kind = reader.read_u8().context("Missing value kind")?;
                if !stored_value_is_safe(reader, item_kind, str_opts, depth + 1)? {
                    safe = false;
                }
            }
            Ok(safe)
        }
        8 => {
            let len = reader.read_collection_len("object field count")?;
            let mut safe = true;
            for _ in 0..len {
                let key_id = reader.read_len("object key string id")?;
                let key = str_opts.get(key_id).copied().flatten();
                let val_kind = reader.read_u8().context("Missing value kind")?;
                match key {
                    Some(k) => {
                        if k == "Ref" || SETTINGS_REFERENCE_SELECTOR_KEYS[1..].contains(&k) {
                            safe = false;
                            skip_raw_value_payload_fast(reader, val_kind, depth + 1)?;
                        } else if k == "_type" {
                            if val_kind == 6 {
                                let val_id = reader.read_len("value string id")?;
                                match str_opts.get(val_id).copied().flatten() {
                                    Some(t) if t != "Ref" && t != "EnumItem" => {}
                                    _ => safe = false,
                                }
                            } else {
                                safe = false;
                                skip_raw_value_payload_fast(reader, val_kind, depth + 1)?;
                            }
                        } else if !stored_value_is_safe(reader, val_kind, str_opts, depth + 1)? {
                            safe = false;
                        }
                    }
                    None => {
                        safe = false;
                        skip_raw_value_payload_fast(reader, val_kind, depth + 1)?;
                    }
                }
            }
            Ok(safe)
        }
        _ => bail!("Unknown settings value kind {kind}"),
    }
}

struct FastCompareInput<'a> {
    structure: &'a [SettingsBytecodeInstance],
    state: &'a ServiceState,
    records: &'a [SettingsBinaryInstance<'a>],
    lookup: &'a SettingsBinaryInstanceLookup<'a>,
    changed: &'a [usize],
    stored_at: &'a [usize],
    moved: Option<&'a super::captured::Moved>,
    root_service: Option<&'a str>,
    long_hashes: Option<&'a super::captured::LongHashMap>,
}

struct FastCompareBlockState<'a> {
    row_to_slot: Vec<u32>,
    eligible: Vec<bool>,
    found_diff: Vec<bool>,
    all_same: Vec<bool>,
    holds_ref: Vec<bool>,
    stored_had_attrs: Vec<bool>,
    cap_unique_id: Vec<Option<&'a str>>,
    stored_unique_id_matched: Vec<Option<bool>>,
    cap_props: Vec<(&'a str, FastCapVal<'a>, bool)>,
    cap_ranges: Vec<(usize, usize)>,
    cap_canon_targets: AHashMap<usize, Vec<&'a str>>,
}

struct FastCompareShared<'a> {
    input: FastCompareInput<'a>,
    database: Option<&'a rbx_reflection::ReflectionDatabase<'a>>,
    long_hashes: &'a super::captured::LongHashMap,
    hull_fidelities: &'a ShardedMap<usize, Option<u8>>,
}

struct FastCompareGroupMeta<'r, 'b> {
    str_opts: &'r [Option<&'b str>],
    str_pool_entry: &'r [Option<usize>],
    block_prop_names: &'r AHashSet<&'b str>,
    count: usize,
}

struct FastCompareGroupMut<'r, 'a, 'b> {
    st: &'r mut FastCompareBlockState<'a>,
    cap_seen: &'r mut [bool],
    cursors: &'r mut [usize],
    stored_canon_targets: &'r mut AHashMap<usize, Vec<&'a str>>,
    scratch: &'r mut FastComparePropScratch<'a, 'b>,
}

#[derive(Default)]
struct FastComparePropScratch<'a, 'b> {
    class_engine_state: AHashMap<&'a str, bool>,
    class_asset_ref: AHashMap<&'a str, bool>,
    tag_left_buf: Vec<&'b str>,
    tag_right_buf: Vec<&'a str>,
}

#[derive(Clone, Copy)]
struct FastPropFlags {
    is_unique_id: bool,
    is_forced: bool,
    is_hull_prop: bool,
    is_meta_name: bool,
    is_source: bool,
    is_special: bool,
}

impl FastPropFlags {
    fn of(name: &str) -> Self {
        let is_unique_id = name == super::captured::UNIQUE_ID;
        let is_forced = name == "TextScaled" || name == "TextWrapped";
        let is_hull_prop = name == crate::rbx::encode::COLLISION_HULL_PROPERTY;
        let is_meta_name = matches!(name, "UniqueId" | "CurrentCamera" | "HistoryId");
        let is_source = name == "Source";
        let is_special = is_unique_id
            || is_forced
            || name == "Attributes"
            || is_meta_name
            || is_source
            || matches!(
                name,
                "Tags" | "CollisionFidelity" | "PhysicsGrid" | "TexturePack" | "TriangleCount"
            )
            || is_hull_prop;
        Self {
            is_unique_id,
            is_forced,
            is_hull_prop,
            is_meta_name,
            is_source,
            is_special,
        }
    }
}

fn fast_compare_skip_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "PackageLink" | "AnimationRigData" | "Terrain" | "HumanoidDescription" | "Lighting"
    )
}

fn fast_cap_val_from_source<'a>(
    name: &str,
    kind: u8,
    source: &SettingsBinaryValueSource<'a>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    shared_safe: &mut AHashMap<usize, bool>,
) -> Option<(FastCapVal<'a>, bool)> {
    if kind == 17 {
        let ref_val = match source {
            SettingsBinaryValueSource::Native(NativeSettingsValue::Ref(idx)) => {
                FastCapVal::Ref(*idx)
            }
            SettingsBinaryValueSource::Property(v) => ref_payload_object(v)
                .and_then(|r| resolve_ref_index(r, lookup).ok().flatten())
                .map_or(FastCapVal::SkipCompare, FastCapVal::Ref),
            _ => FastCapVal::SkipCompare,
        };
        return Some((ref_val, true));
    }
    if matches!(kind, 10 | NESTED_ENUM_ITEM_KIND) {
        return None;
    }
    if matches!(kind, 9 | 21) {
        return Some((FastCapVal::SkipCompare, false));
    }
    if kind == 7 {
        return fast_cap_val_array(name, source).map(|v| (v, false));
    }
    if kind == 8 {
        return fast_cap_val_object(source, shared_safe).map(|v| (v, false));
    }
    fast_cap_val_scalar(kind, source).map(|v| (v, false))
}

fn fast_cap_val_array<'a>(
    name: &str,
    source: &SettingsBinaryValueSource<'a>,
) -> Option<FastCapVal<'a>> {
    use crate::settings::equivalence::{
        reconciliation_value_contains_reference as holds_reference, value_contains_enum_item,
    };
    match source {
        SettingsBinaryValueSource::Property(v)
            if !holds_reference(v) && !value_contains_enum_item(v) =>
        {
            if name == "Tags"
                && let Some(items) = v.as_array()
                && items.iter().all(Value::is_string)
            {
                Some(FastCapVal::Tags(items.as_slice()))
            } else {
                Some(FastCapVal::SkipCompare)
            }
        }
        SettingsBinaryValueSource::Settled(v)
            if !holds_reference(v) && !value_contains_enum_item(v) =>
        {
            Some(FastCapVal::SkipCompare)
        }
        _ => None,
    }
}

fn fast_cap_val_object<'a>(
    source: &SettingsBinaryValueSource<'a>,
    shared_safe: &mut AHashMap<usize, bool>,
) -> Option<FastCapVal<'a>> {
    use crate::settings::equivalence::{
        reconciliation_value_contains_reference as holds_reference, value_contains_enum_item,
    };
    match source {
        SettingsBinaryValueSource::Native(NativeSettingsValue::Shared(arc)) => {
            let ptr = std::sync::Arc::as_ptr(arc) as usize;
            let safe = *shared_safe
                .entry(ptr)
                .or_insert_with(|| !holds_reference(arc) && !value_contains_enum_item(arc));
            if !safe {
                return None;
            }
            Some(extract_payload_from_value(arc).map_or_else(
                || FastCapVal::Json(arc.as_ref()),
                |(ty, b64)| FastCapVal::Payload { ty, b64 },
            ))
        }
        SettingsBinaryValueSource::Property(v) => {
            if let Some((ty, b64)) = extract_payload_from_value(v) {
                Some(FastCapVal::Payload { ty, b64 })
            } else if !holds_reference(v) && !value_contains_enum_item(v) {
                Some(FastCapVal::Json(v))
            } else {
                None
            }
        }
        SettingsBinaryValueSource::Settled(v)
            if !holds_reference(v) && !value_contains_enum_item(v) =>
        {
            Some(FastCapVal::SkipCompare)
        }
        _ => None,
    }
}

fn extract_payload_from_value(v: &Value) -> Option<(&str, &str)> {
    let o = v.as_object()?;
    if o.len() != 2 {
        return None;
    }
    let ty = o.get("_type").and_then(Value::as_str)?;
    if !matches!(ty, "SharedString" | "BinaryString") {
        return None;
    }
    let b64 = o.get("base64").and_then(Value::as_str)?;
    Some((ty, b64))
}

fn fast_cap_val_scalar<'a>(
    kind: u8,
    source: &SettingsBinaryValueSource<'a>,
) -> Option<FastCapVal<'a>> {
    match source {
        SettingsBinaryValueSource::Native(n) => match n {
            NativeSettingsValue::Bool(b) => Some(FastCapVal::Bool(*b)),
            NativeSettingsValue::Int(i) => Some(FastCapVal::I64(*i)),
            NativeSettingsValue::Float32(f) => Some(FastCapVal::F64(f64::from(*f))),
            NativeSettingsValue::Float64(f) => Some(FastCapVal::F64(*f)),
            NativeSettingsValue::String(s) => Some(FastCapVal::Str(s.as_str())),
            NativeSettingsValue::Enum(s) => Some(FastCapVal::Enum(s.as_str())),
            NativeSettingsValue::Vector2(v) => Some(FastCapVal::Vec2(*v)),
            NativeSettingsValue::Vector3(v) => Some(FastCapVal::Vec3(*v)),
            NativeSettingsValue::UDim(v) => Some(FastCapVal::UDim(*v)),
            NativeSettingsValue::UDim2(v) => Some(FastCapVal::UDim2(*v)),
            NativeSettingsValue::Color3(v) => Some(FastCapVal::Color3(*v)),
            NativeSettingsValue::Rect(v) => Some(FastCapVal::Rect(*v)),
            NativeSettingsValue::CFrame(cf) => Some(FastCapVal::CFrame(cf.as_ref())),
            NativeSettingsValue::Shared(_) => Some(FastCapVal::SkipCompare),
            NativeSettingsValue::UniqueId(_) | NativeSettingsValue::Ref(_) => None,
        },
        SettingsBinaryValueSource::Property(v) => match kind {
            0 | 11..=16 | 18 => Some(FastCapVal::SkipCompare),
            1 => Some(FastCapVal::Bool(false)),
            2 => Some(FastCapVal::Bool(true)),
            3 => Some(v.as_i64().map_or(FastCapVal::SkipCompare, FastCapVal::I64)),
            4 => Some(v.as_u64().map_or(FastCapVal::SkipCompare, FastCapVal::U64)),
            5 | 20 => Some(settings_number(v).map_or(FastCapVal::SkipCompare, FastCapVal::F64)),
            6 => Some(raw_string_payload(v).map_or(FastCapVal::SkipCompare, FastCapVal::Str)),
            19 => Some(raw_string_payload(v).map_or(FastCapVal::SkipCompare, FastCapVal::Enum)),
            _ => None,
        },
        SettingsBinaryValueSource::Settled(v) => match kind {
            0 | 6 | 19 | 11..=16 | 18 => Some(FastCapVal::SkipCompare),
            1 => Some(FastCapVal::Bool(false)),
            2 => Some(FastCapVal::Bool(true)),
            3 => Some(v.as_i64().map_or(FastCapVal::SkipCompare, FastCapVal::I64)),
            4 => Some(v.as_u64().map_or(FastCapVal::SkipCompare, FastCapVal::U64)),
            5 | 20 => Some(settings_number(v).map_or(FastCapVal::SkipCompare, FastCapVal::F64)),
            _ => None,
        },
        SettingsBinaryValueSource::Attributes(_) => None,
    }
}

fn decode_stored_scalar_val<'a>(
    reader: &mut BytecodeReader<'_>,
    kind: u8,
    str_opts: &[Option<&'a str>],
) -> Result<Option<Option<FastCapVal<'a>>>> {
    let val = match kind {
        0 => None,
        1 => Some(FastCapVal::Bool(false)),
        2 => Some(FastCapVal::Bool(true)),
        3 => Some(FastCapVal::I64(unzigzag_i64(reader.read_var_u64()?))),
        4 => Some(FastCapVal::U64(reader.read_var_u64()?)),
        5 => Some(FastCapVal::F64(read_f64(reader)?)),
        20 => Some(FastCapVal::F64(f64::from(read_f32(reader)?))),
        6 => {
            let id = reader.read_len("value string id")?;
            let entry = *str_opts
                .get(id)
                .with_context(|| format!("Invalid value string id {id}"))?;
            entry.map(FastCapVal::Str)
        }
        19 => {
            let id = reader.read_len("enum item string id")?;
            let entry = *str_opts
                .get(id)
                .with_context(|| format!("Invalid enum item string id {id}"))?;
            match entry {
                Some(s) => Some(FastCapVal::Enum(s)),
                None => return Ok(None),
            }
        }
        11 => Some(FastCapVal::Vec2([read_f32(reader)?, read_f32(reader)?])),
        12 => Some(FastCapVal::Vec3([
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
        ])),
        13 => Some(FastCapVal::UDim([read_f32(reader)?, read_f32(reader)?])),
        14 => Some(FastCapVal::UDim2([
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
        ])),
        15 => Some(FastCapVal::Color3([
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
        ])),
        18 => Some(FastCapVal::Rect([
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
        ])),
        _ => return Ok(None),
    };
    Ok(Some(val))
}

fn decode_cframe_f64(
    reader: &mut BytecodeReader<'_>,
    cframe: &CFrameGroupDecoder,
) -> Result<[f64; 12]> {
    match cframe {
        CFrameGroupDecoder::Inline => {
            let mut c = [0.0_f64; 12];
            for x in &mut c {
                *x = f64::from(read_f32(reader)?);
            }
            Ok(c)
        }
        CFrameGroupDecoder::RotationTable(rotations) => {
            let p0 = f64::from(read_f32(reader)?);
            let p1 = f64::from(read_f32(reader)?);
            let p2 = f64::from(read_f32(reader)?);
            let rot_idx = reader.read_len("CFrame rotation index")?;
            let rot = rotations
                .get(rot_idx)
                .with_context(|| format!("Invalid CFrame rotation index {rot_idx}"))?;
            Ok([
                p0, p1, p2, rot[0], rot[1], rot[2], rot[3], rot[4], rot[5], rot[6], rot[7], rot[8],
            ])
        }
    }
}

fn compare_f32_slices_pair(a: &[f32], b: &[f32], can_prove_diff: bool) -> (bool, bool) {
    use crate::settings::equivalence::{f32_values_definitely_differ, f32_values_match};
    if a.iter()
        .zip(b)
        .all(|(&x, &y)| f32_values_match(f64::from(x), f64::from(y)))
    {
        return (true, false);
    }
    let diff = can_prove_diff
        && a.iter()
            .zip(b)
            .any(|(&x, &y)| f32_values_definitely_differ(f64::from(x), f64::from(y)));
    (false, diff)
}

#[derive(Clone, Copy)]
enum FastNum {
    I64(i64),
    U64(u64),
    F64(f64),
}

impl FastNum {
    fn as_f64(self) -> f64 {
        match self {
            Self::I64(x) => x as f64,
            Self::U64(x) => x as f64,
            Self::F64(x) => x,
        }
    }
}

fn exact_fast_numbers_equal(left: FastNum, right: FastNum) -> bool {
    match (left, right) {
        (FastNum::I64(a), FastNum::I64(b)) => a == b,
        (FastNum::U64(a), FastNum::U64(b)) => a == b,
        (FastNum::I64(a), FastNum::U64(b)) | (FastNum::U64(b), FastNum::I64(a)) => {
            i128::from(a) == i128::from(b)
        }
        (FastNum::F64(a), FastNum::F64(b)) => a == b,
        (FastNum::F64(f), FastNum::I64(i)) | (FastNum::I64(i), FastNum::F64(f)) => {
            f.is_finite() && f.fract() == 0.0 && i128::from(i) == (f as i128)
        }
        (FastNum::F64(f), FastNum::U64(u)) | (FastNum::U64(u), FastNum::F64(f)) => {
            f.is_finite() && f.fract() == 0.0 && f >= 0.0 && i128::from(u) == (f as i128)
        }
    }
}

fn compare_fast_pair_values(
    class_name: &str,
    name: &str,
    (sv, cv): (FastCapVal<'_>, FastCapVal<'_>),
    (can_prove_diff, is_asset): (bool, bool),
    cache: &mut crate::settings::equivalence::CanonicalizeCache,
) -> (bool, bool) {
    use crate::settings::equivalence::{
        canonical_asset_text, f32_values_definitely_differ, f32_values_match,
    };

    if name == "TriangleCount"
        && matches!(
            class_name,
            "UnionOperation" | "NegateOperation" | "IntersectOperation"
        )
        && matches!(
            sv,
            FastCapVal::I64(_) | FastCapVal::U64(_) | FastCapVal::F64(_)
        )
        && matches!(
            cv,
            FastCapVal::I64(_) | FastCapVal::U64(_) | FastCapVal::F64(_)
        )
    {
        return (true, false);
    }
    if name == "CollisionFidelity"
        && crate::rbx::encode::is_solid_model_operation(class_name)
        && let (FastCapVal::Enum(a), FastCapVal::Enum(b)) = (sv, cv)
        && matches!(a, "Box" | "Default")
        && matches!(b, "Box" | "Default")
    {
        return (true, false);
    }
    match (sv, cv) {
        (FastCapVal::Bool(a), FastCapVal::Bool(b)) => (a == b, a != b && can_prove_diff),
        (
            FastCapVal::I64(_) | FastCapVal::U64(_) | FastCapVal::F64(_),
            FastCapVal::I64(_) | FastCapVal::U64(_) | FastCapVal::F64(_),
        ) => {
            let to_num = |v| match v {
                FastCapVal::I64(x) => FastNum::I64(x),
                FastCapVal::U64(x) => FastNum::U64(x),
                FastCapVal::F64(x) => FastNum::F64(x),
                _ => unreachable!(),
            };
            let (left, right) = (to_num(sv), to_num(cv));
            let (a, b) = (left.as_f64(), right.as_f64());
            if exact_fast_numbers_equal(left, right) {
                (true, false)
            } else if can_prove_diff && f32_values_definitely_differ(a, b) {
                (false, true)
            } else if cache.property_uses_f32(class_name, name) && f32_values_match(a, b) {
                (true, false)
            } else {
                (false, false)
            }
        }
        (FastCapVal::Str(a), FastCapVal::Str(b)) => {
            if a == b || (is_asset && canonical_asset_text(a) == canonical_asset_text(b)) {
                (true, false)
            } else {
                (false, can_prove_diff)
            }
        }
        (FastCapVal::Enum(a), FastCapVal::Enum(b)) => {
            if a == b {
                (true, false)
            } else {
                (
                    false,
                    can_prove_diff && a.parse::<u64>().is_err() && b.parse::<u64>().is_err(),
                )
            }
        }
        (FastCapVal::Vec2(a), FastCapVal::Vec2(b)) => {
            compare_f32_slices_pair(&a, &b, can_prove_diff)
        }
        (FastCapVal::Vec3(a), FastCapVal::Vec3(b)) => {
            compare_f32_slices_pair(&a, &b, can_prove_diff)
        }
        (FastCapVal::Rect(a), FastCapVal::Rect(b)) => {
            compare_f32_slices_pair(&a, &b, can_prove_diff)
        }
        (FastCapVal::UDim(a), FastCapVal::UDim(b)) => {
            if f32_values_match(f64::from(a[0]), f64::from(b[0])) && a[1] == b[1] {
                (true, false)
            } else {
                let diff = can_prove_diff
                    && (f32_values_definitely_differ(f64::from(a[0]), f64::from(b[0]))
                        || f32_values_definitely_differ(f64::from(a[1]), f64::from(b[1])));
                (false, diff)
            }
        }
        (FastCapVal::UDim2(a), FastCapVal::UDim2(b)) => {
            if f32_values_match(f64::from(a[0]), f64::from(b[0]))
                && a[1] == b[1]
                && f32_values_match(f64::from(a[2]), f64::from(b[2]))
                && a[3] == b[3]
            {
                (true, false)
            } else {
                let diff = can_prove_diff
                    && (0..4)
                        .any(|i| f32_values_definitely_differ(f64::from(a[i]), f64::from(b[i])));
                (false, diff)
            }
        }
        (FastCapVal::Color3(a), FastCapVal::Color3(b)) => {
            if (0..3).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i]))) {
                (true, false)
            } else {
                let diff = can_prove_diff
                    && (0..3).any(|i| {
                        f32_values_definitely_differ(f64::from(a[i]), f64::from(b[i]))
                            && (a[i].clamp(0.0, 1.0) * 255.0).round() as u8
                                != (b[i].clamp(0.0, 1.0) * 255.0).round() as u8
                    });
                (false, diff)
            }
        }
        _ => (false, false),
    }
}

fn read_inline_number(reader: &mut BytecodeReader<'_>, kind: u8) -> Option<f64> {
    match kind {
        3 => Some(unzigzag_i64(reader.read_var_u64().ok()?) as f64),
        4 => Some(reader.read_var_u64().ok()? as f64),
        5 => read_f64(reader).ok(),
        20 => read_f32(reader).ok().map(f64::from),
        _ => None,
    }
}

fn compare_inline_physical_properties(
    reader: &mut BytecodeReader<'_>,
    str_opts: &[Option<&str>],
    cap_obj: &Map<String, Value>,
) -> Option<bool> {
    use crate::settings::equivalence::{f32_values_definitely_differ, f32_values_match};

    let cap_vals = [
        cap_obj.get("density")?.as_f64()?,
        cap_obj.get("friction")?.as_f64()?,
        cap_obj.get("elasticity")?.as_f64()?,
        cap_obj.get("frictionWeight")?.as_f64()?,
        cap_obj.get("elasticityWeight")?.as_f64()?,
    ];
    let mut is_pp = false;
    let mut seen = 0u8;
    let mut stored_vals = [0.0f64; 5];
    for _ in 0..6 {
        let key_id = reader.read_len("object key string id").ok()?;
        let key = (*str_opts.get(key_id)?)?;
        let vk = reader.read_u8().ok()?;
        match key {
            "_type" if vk == 6 => {
                let sid = reader.read_len("value string id").ok()?;
                if (*str_opts.get(sid)?)? != "PhysicalProperties" {
                    return None;
                }
                is_pp = true;
            }
            "density" => {
                stored_vals[0] = read_inline_number(reader, vk)?;
                seen |= 1;
            }
            "friction" => {
                stored_vals[1] = read_inline_number(reader, vk)?;
                seen |= 2;
            }
            "elasticity" => {
                stored_vals[2] = read_inline_number(reader, vk)?;
                seen |= 4;
            }
            "frictionWeight" => {
                stored_vals[3] = read_inline_number(reader, vk)?;
                seen |= 8;
            }
            "elasticityWeight" => {
                stored_vals[4] = read_inline_number(reader, vk)?;
                seen |= 16;
            }
            _ => return None,
        }
    }
    if !is_pp || seen != 31 {
        return None;
    }
    if (0..5).all(|i| f32_values_match(stored_vals[i], cap_vals[i])) {
        Some(true)
    } else if (0..5).any(|i| f32_values_definitely_differ(stored_vals[i], cap_vals[i])) {
        Some(false)
    } else {
        None
    }
}

fn compare_inline_brick_color(
    reader: &mut BytecodeReader<'_>,
    str_opts: &[Option<&str>],
    cap_obj: &Map<String, Value>,
) -> Option<bool> {
    let cap_num = cap_obj.get("number")?.as_i64()?;
    let mut is_bc = false;
    let mut stored_num: Option<i64> = None;
    for _ in 0..2 {
        let key_id = reader.read_len("object key string id").ok()?;
        let key = (*str_opts.get(key_id)?)?;
        let vk = reader.read_u8().ok()?;
        match key {
            "_type" if vk == 6 => {
                let sid = reader.read_len("value string id").ok()?;
                if (*str_opts.get(sid)?)? != "BrickColor" {
                    return None;
                }
                is_bc = true;
            }
            "number" => {
                stored_num = match vk {
                    3 => Some(unzigzag_i64(reader.read_var_u64().ok()?)),
                    4 => i64::try_from(reader.read_var_u64().ok()?).ok(),
                    _ => return None,
                };
            }
            _ => return None,
        }
    }
    if !is_bc {
        return None;
    }
    Some(stored_num? == cap_num)
}

fn compare_inline_typed_object(
    reader: &mut BytecodeReader<'_>,
    str_opts: &[Option<&str>],
    cap_val: &Value,
) -> Option<bool> {
    let cap_obj = cap_val.as_object()?;
    let cap_ty = cap_obj.get("_type").and_then(Value::as_str)?;
    let len = reader.read_collection_len("object field count").ok()?;
    match cap_ty {
        "PhysicalProperties" if len == 6 => {
            compare_inline_physical_properties(reader, str_opts, cap_obj)
        }
        "BrickColor" if len == 2 => compare_inline_brick_color(reader, str_opts, cap_obj),
        _ => None,
    }
}

fn decode_inline_fast_object(
    reader: &mut BytecodeReader<'_>,
    str_opts: &[Option<&str>],
    depth: usize,
) -> Result<Option<Value>> {
    if depth > MAX_SETTINGS_VALUE_DEPTH {
        return Ok(None);
    }
    let len = reader.read_collection_len("object field count")?;
    let mut map = Map::with_capacity(len);
    for _ in 0..len {
        let key_id = reader.read_len("object key string id")?;
        let Some(key) = str_opts.get(key_id).copied().flatten() else {
            return Ok(None);
        };
        let val_kind = reader.read_u8().context("Missing value kind")?;
        let Some(val) = decode_inline_fast_value(reader, val_kind, str_opts, depth + 1)? else {
            return Ok(None);
        };
        map.insert(key.to_string(), val);
    }
    Ok(Some(Value::Object(map)))
}

fn decode_inline_fast_value(
    reader: &mut BytecodeReader<'_>,
    kind: u8,
    str_opts: &[Option<&str>],
    depth: usize,
) -> Result<Option<Value>> {
    if depth > MAX_SETTINGS_VALUE_DEPTH {
        return Ok(None);
    }
    let val = match kind {
        0 => Value::Null,
        1 => Value::Bool(false),
        2 => Value::Bool(true),
        3 => json!(unzigzag_i64(reader.read_var_u64()?)),
        4 => json!(reader.read_var_u64()?),
        5 => json_number_f64(read_f64(reader)?),
        20 => json_number_f64(f64::from(read_f32(reader)?)),
        6 => {
            let id = reader.read_len("value string id")?;
            let Some(s) = str_opts.get(id).copied().flatten() else {
                return Ok(None);
            };
            Value::String(s.to_string())
        }
        7 => {
            let len = reader.read_collection_len("array length")?;
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                let item_kind = reader.read_u8().context("Missing array item kind")?;
                let Some(item) = decode_inline_fast_value(reader, item_kind, str_opts, depth + 1)?
                else {
                    return Ok(None);
                };
                items.push(item);
            }
            Value::Array(items)
        }
        8 => return decode_inline_fast_object(reader, str_opts, depth),
        9 => {
            let len = reader.read_len("numeric array length")?;
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                items.push(json_number_f64(read_f64(reader)?));
            }
            Value::Array(items)
        }
        21 => {
            let len = reader.read_len("f32 numeric array length")?;
            let mut items = Vec::with_capacity(len);
            for _ in 0..len {
                items.push(json_number_f64(f64::from(read_f32(reader)?)));
            }
            Value::Array(items)
        }
        11 => decode_fixed_numeric_payload(reader, "Vector2", &["x", "y"])?,
        12 => decode_fixed_numeric_payload(reader, "Vector3", &["x", "y", "z"])?,
        13 => decode_fixed_numeric_payload(reader, "UDim", &["scale", "offset"])?,
        14 => decode_fixed_numeric_payload(
            reader,
            "UDim2",
            &["xScale", "xOffset", "yScale", "yOffset"],
        )?,
        15 => decode_fixed_numeric_payload(reader, "Color3", &["r", "g", "b"])?,
        16 => decode_cframe_payload(reader)?,
        18 => decode_fixed_numeric_payload(reader, "Rect", &["minX", "minY", "maxX", "maxY"])?,
        _ => return Ok(None),
    };
    Ok(Some(val))
}

fn read_two_string_object_header<'a>(
    reader: &mut BytecodeReader<'_>,
    str_opts: &[Option<&'a str>],
) -> Option<(&'a str, usize, &'a str, usize)> {
    if reader.read_collection_len("object field count").ok()? != 2 {
        return None;
    }
    let k0_id = reader.read_len("object key string id").ok()?;
    if reader.read_u8().ok()? != 6 {
        return None;
    }
    let v0_id = reader.read_len("value string id").ok()?;
    let k1_id = reader.read_len("object key string id").ok()?;
    if reader.read_u8().ok()? != 6 {
        return None;
    }
    let v1_id = reader.read_len("value string id").ok()?;
    Some((
        (*str_opts.get(k0_id)?)?,
        v0_id,
        (*str_opts.get(k1_id)?)?,
        v1_id,
    ))
}

fn check_stored_prop_canonical<'a, 'b>(
    cache: &mut crate::settings::equivalence::CanonicalizeCache<'a>,
    class_name: &str,
    name: &'b str,
    is_forced: bool,
    (slot, block_prop_names): (usize, &AHashSet<&'b str>),
    st: &mut FastCompareBlockState<'a>,
    stored_canon_targets: &mut AHashMap<usize, Vec<&'a str>>,
) -> bool {
    use crate::settings::equivalence::PropertyCanonical;
    if is_forced || name == "Attributes" {
        st.eligible[slot] = false;
        return false;
    }
    match cache.property_canonical(class_name, name) {
        PropertyCanonical::Unchanged => {
            if st
                .cap_canon_targets
                .get(&slot)
                .is_some_and(|ts| ts.contains(&name))
            {
                st.eligible[slot] = false;
            }
            false
        }
        PropertyCanonical::Renamed(target) => {
            let (cs, ce) = st.cap_ranges[slot];
            let conflict = block_prop_names.contains(target)
                || st.cap_props[cs..ce]
                    .binary_search_by_key(&target, |p| p.0)
                    .is_ok();
            let ts = stored_canon_targets.entry(slot).or_default();
            if conflict || ts.contains(&target) {
                st.eligible[slot] = false;
                false
            } else {
                ts.push(target);
                true
            }
        }
        PropertyCanonical::Migrated(targets) => {
            let (cs, ce) = st.cap_ranges[slot];
            let conflict = targets.iter().any(|target| {
                block_prop_names.contains(target)
                    || st.cap_props[cs..ce]
                        .binary_search_by_key(target, |p| p.0)
                        .is_ok()
            });
            let ts = stored_canon_targets.entry(slot).or_default();
            if conflict || targets.iter().any(|t| ts.contains(t)) {
                st.eligible[slot] = false;
                false
            } else {
                ts.extend_from_slice(targets);
                true
            }
        }
        PropertyCanonical::Complex => {
            st.eligible[slot] = false;
            false
        }
    }
}

fn advance_cap_cursor<'a>(
    st: &FastCompareBlockState<'a>,
    cap_seen: &mut [bool],
    cursors: &mut [usize],
    slot: usize,
    name: &str,
) -> Option<(FastCapVal<'a>, bool)> {
    let end = st.cap_ranges[slot].1;
    let mut c = cursors[slot];
    while c < end && st.cap_props[c].0 < name {
        c += 1;
    }
    cursors[slot] = c;
    if c < end && st.cap_props[c].0 == name {
        cap_seen[c] = true;
        Some((st.cap_props[c].1, st.cap_props[c].2))
    } else {
        None
    }
}

fn compare_stored_array_value<'a, 'b>(
    reader: &mut BytecodeReader<'_>,
    str_opts: &[Option<&'b str>],
    (slot, name, is_unique_id): (usize, &str, bool),
    cap_match: Option<(FastCapVal<'a>, bool)>,
    (st, scratch): (
        &mut FastCompareBlockState<'a>,
        &mut FastComparePropScratch<'a, 'b>,
    ),
) -> Result<()> {
    if is_unique_id {
        st.eligible[slot] = false;
        skip_raw_value_payload_fast(reader, 7, 0)?;
        return Ok(());
    }
    if name == "Tags" {
        if let Some((FastCapVal::Tags(cap_tags), _)) = cap_match {
            let save_pos = reader.position();
            let tags_ok = (|| -> Option<bool> {
                let arr_len = reader.read_collection_len("array length").ok()?;
                scratch.tag_left_buf.clear();
                for _ in 0..arr_len {
                    if reader.read_u8().ok()? != 6 {
                        return None;
                    }
                    let id = reader.read_len("value string id").ok()?;
                    scratch.tag_left_buf.push((*str_opts.get(id)?)?);
                }
                scratch.tag_right_buf.clear();
                for v in cap_tags {
                    scratch.tag_right_buf.push(v.as_str()?);
                }
                scratch.tag_left_buf.sort_unstable();
                scratch.tag_left_buf.dedup();
                scratch.tag_right_buf.sort_unstable();
                scratch.tag_right_buf.dedup();
                Some(scratch.tag_left_buf == scratch.tag_right_buf)
            })();
            match tags_ok {
                Some(true) => return Ok(()),
                Some(false) => {
                    st.found_diff[slot] = true;
                    st.all_same[slot] = false;
                    return Ok(());
                }
                None => reader.set_position(save_pos),
            }
        } else if cap_match.is_none() {
            let save_pos = reader.position();
            let non_empty_string_tags = (|| -> Option<bool> {
                let arr_len = reader.read_collection_len("array length").ok()?;
                for _ in 0..arr_len {
                    if reader.read_u8().ok()? != 6 {
                        return None;
                    }
                    let id = reader.read_len("value string id").ok()?;
                    let _ = (*str_opts.get(id)?)?;
                }
                Some(arr_len > 0)
            })();
            match non_empty_string_tags {
                Some(true) => {
                    st.found_diff[slot] = true;
                    st.all_same[slot] = false;
                    return Ok(());
                }
                Some(false) => {
                    st.all_same[slot] = false;
                    return Ok(());
                }
                None => reader.set_position(save_pos),
            }
        }
    }
    if !stored_value_is_safe(reader, 7, str_opts, 0)? {
        st.eligible[slot] = false;
    } else {
        st.all_same[slot] = false;
    }
    Ok(())
}

fn extract_two_field_unique_id<'a>(
    str_opts: &[Option<&'a str>],
    k0: &str,
    v0_id: usize,
    k1: &str,
    v1_id: usize,
) -> Option<&'a str> {
    if k0 == "_type" && str_opts.get(v0_id).copied().flatten() == Some("UniqueId") && k1 == "value"
    {
        str_opts.get(v1_id).copied().flatten()
    } else if k1 == "_type"
        && str_opts.get(v1_id).copied().flatten() == Some("UniqueId")
        && k0 == "value"
    {
        str_opts.get(v0_id).copied().flatten()
    } else {
        None
    }
}

fn extract_two_field_payload_id<'a>(
    str_opts: &[Option<&'a str>],
    k0: &str,
    v0_id: usize,
    k1: &str,
    v1_id: usize,
) -> Option<(&'a str, usize)> {
    let (ty_id, b64_id) = if k0 == "_type" && k1 == "base64" {
        (v0_id, v1_id)
    } else if k1 == "_type" && k0 == "base64" {
        (v1_id, v0_id)
    } else {
        return None;
    };
    str_opts
        .get(ty_id)
        .copied()
        .flatten()
        .filter(|t| matches!(*t, "SharedString" | "BinaryString"))
        .map(|ty| (ty, b64_id))
}

fn compare_stored_cframe_value(
    cache: &mut crate::settings::equivalence::CanonicalizeCache,
    (slot, class_name, name, is_special, is_stored_canon): (usize, &str, &str, bool, bool),
    a: &[f64; 12],
    cap_match: Option<(FastCapVal<'_>, bool)>,
    st: &mut FastCompareBlockState<'_>,
) {
    use crate::settings::equivalence::{
        FastDefaultVal, OneSidedRule, cframes_definitely_differ, cframes_match_reconciliation,
    };
    match cap_match {
        Some((FastCapVal::CFrame(b), _)) => {
            let b64 = b.map(f64::from);
            if !is_special && !is_stored_canon && cframes_definitely_differ(a, &b64) {
                st.found_diff[slot] = true;
                st.all_same[slot] = false;
            } else if !cframes_match_reconciliation(a, &b64) {
                st.all_same[slot] = false;
            }
        }
        None if !is_stored_canon => match cache.one_sided_rule(class_name, name) {
            OneSidedRule::Ignored => {}
            OneSidedRule::Default(FastDefaultVal::CFrame(ref def)) => {
                if !is_special && cframes_definitely_differ(a, def) {
                    st.found_diff[slot] = true;
                    st.all_same[slot] = false;
                } else if !cframes_match_reconciliation(a, def) {
                    st.all_same[slot] = false;
                }
            }
            OneSidedRule::NoDefault => {
                if !is_special {
                    st.found_diff[slot] = true;
                }
                st.all_same[slot] = false;
            }
            _ => st.all_same[slot] = false,
        },
        _ => st.all_same[slot] = false,
    }
}

fn apply_fast_scalar_comparison(
    cache: &mut crate::settings::equivalence::CanonicalizeCache,
    (slot, class_name, name, is_special, is_stored_canon, is_asset): (
        usize,
        &str,
        &str,
        bool,
        bool,
        bool,
    ),
    (stored_val, cap_match): (Option<FastCapVal<'_>>, Option<(FastCapVal<'_>, bool)>),
    st: &mut FastCompareBlockState<'_>,
) {
    match (stored_val, cap_match) {
        (Some(sv), Some((cv, _))) => {
            let can_prove_diff = !is_special && !is_stored_canon && !is_asset;
            let (same, diff) = compare_fast_pair_values(
                class_name,
                name,
                (sv, cv),
                (can_prove_diff, is_asset),
                cache,
            );
            if !same {
                st.all_same[slot] = false;
            }
            if diff {
                st.found_diff[slot] = true;
            }
        }
        (Some(sv), None) => {
            if is_stored_canon {
                st.all_same[slot] = false;
            } else {
                let rule = cache.one_sided_rule(class_name, name);
                match fast_val_matches_one_sided(name, sv, &rule) {
                    Some(true) => {}
                    Some(false) => {
                        st.all_same[slot] = false;
                        if !is_special && !is_asset {
                            st.found_diff[slot] = true;
                        }
                    }
                    None => st.all_same[slot] = false,
                }
            }
        }
        (None, _) => st.all_same[slot] = false,
    }
}

fn check_unmatched_captured_props(
    shared: &FastCompareShared<'_>,
    cache: &mut crate::settings::equivalence::CanonicalizeCache,
    block_items: &[(usize, usize, usize)],
    st: &mut FastCompareBlockState<'_>,
    cap_seen: &[bool],
) {
    use crate::settings::equivalence::is_asset_reference_property;
    for (slot, &(_, obs_row, _)) in block_items.iter().enumerate() {
        if !st.eligible[slot] {
            continue;
        }
        let class_name = shared.input.records[obs_row].class_name;
        let (start, end) = st.cap_ranges[slot];
        for (idx, &seen) in cap_seen.iter().enumerate().take(end).skip(start) {
            if seen {
                continue;
            }
            let (name, val, is_canon) = st.cap_props[idx];
            if is_canon {
                st.all_same[slot] = false;
                continue;
            }
            if name == "Source"
                && matches!(
                    val,
                    FastCapVal::Str(crate::settings::EXTERNAL_SOURCE_MARKER)
                )
            {
                continue;
            }
            if name == "Tags"
                && let FastCapVal::Tags(items) = val
            {
                st.all_same[slot] = false;
                if !items.is_empty() {
                    st.found_diff[slot] = true;
                }
                continue;
            }
            let rule = cache.one_sided_rule(class_name, name);
            if matches!(rule, crate::settings::equivalence::OneSidedRule::Ignored) {
                continue;
            }
            if let FastCapVal::Json(cap_val) = val {
                let is_special = matches!(
                    name,
                    "CurrentCamera"
                        | "HistoryId"
                        | "Source"
                        | "Tags"
                        | "CollisionFidelity"
                        | "PhysicsGrid"
                        | "TexturePack"
                        | "TriangleCount"
                ) || name == crate::rbx::encode::COLLISION_HULL_PROPERTY
                    || is_asset_reference_property(class_name, name);
                if !is_special
                    && crate::settings::equivalence::reconciliation_store_property_values_equal(
                        shared.input.root_service,
                        class_name,
                        name,
                        None,
                        Some(cap_val),
                    )
                {
                    continue;
                }
                st.all_same[slot] = false;
                continue;
            }
            match fast_val_matches_one_sided(name, val, &rule) {
                Some(true) => {}
                Some(false) => {
                    st.all_same[slot] = false;
                    let is_special = matches!(
                        name,
                        "CurrentCamera"
                            | "HistoryId"
                            | "Source"
                            | "Tags"
                            | "CollisionFidelity"
                            | "PhysicsGrid"
                            | "TexturePack"
                            | "TriangleCount"
                    ) || name == crate::rbx::encode::COLLISION_HULL_PROPERTY
                        || is_asset_reference_property(class_name, name);
                    if !is_special {
                        st.found_diff[slot] = true;
                    }
                }
                None => st.all_same[slot] = false,
            }
        }
    }
}

impl BlockStore<'_> {
    fn pool_hull_fidelity(
        &self,
        entry: usize,
        cache: &ShardedMap<usize, Option<u8>>,
    ) -> Result<Option<u8>> {
        if let Some(cached) = cache.get(&entry) {
            return Ok(cached);
        }
        let (frame, place) = self.slots[entry];
        let texts = self.pool_frame(frame)?;
        let code = crate::settings::equivalence::hull_fidelity_from_b64_prefix(&texts[place]);
        cache.insert(entry, code);
        Ok(code)
    }

    fn fast_compare_changed(&self, input: FastCompareInput<'_>) -> Result<Vec<FastCompareOutcome>> {
        use crate::settings::equivalence::CanonicalizeCache;

        let _span = crate::app::profile::span("changed rows fast compare");
        if input.changed.is_empty() {
            return Ok(Vec::new());
        }
        let database = rbx_reflection_database::get().ok();
        let mut indexed: Vec<(usize, usize, usize)> = input
            .changed
            .iter()
            .copied()
            .zip(input.stored_at.iter().copied())
            .enumerate()
            .map(|(pos, (obs_row, stored_row))| (pos, obs_row, stored_row))
            .collect();
        if !indexed.is_sorted_by_key(|&(_, _, stored_row)| stored_row) {
            indexed.sort_unstable_by_key(|&(_, _, stored_row)| stored_row);
        }
        let by_block: Vec<&[(usize, usize, usize)]> = indexed
            .chunk_by(|a, b| self.block_of(a.2) == self.block_of(b.2))
            .collect();
        let owned_long_hashes;
        let long_hashes = match input.long_hashes {
            Some(existing) => existing,
            None => {
                owned_long_hashes = ShardedMap::new();
                &owned_long_hashes
            }
        };
        let hull_fidelities = ShardedMap::<usize, Option<u8>>::new();
        let shared = FastCompareShared {
            input,
            database,
            long_hashes,
            hull_fidelities: &hull_fidelities,
        };

        let block_results = by_block
            .into_par_iter()
            .map_init(CanonicalizeCache::new, |cache, block_items| {
                self.fast_compare_block(&shared, cache, block_items)
            })
            .collect::<Result<Vec<_>>>()?;

        let mut out = vec![None; shared.input.changed.len()];
        for block in block_results {
            for (pos, pair) in block {
                out[pos] = pair;
            }
        }
        Ok(out)
    }

    fn fast_compare_block<'a>(
        &self,
        shared: &FastCompareShared<'a>,
        cache: &mut crate::settings::equivalence::CanonicalizeCache<'a>,
        block_items: &[(usize, usize, usize)],
    ) -> Result<Vec<(usize, FastCompareOutcome)>> {
        use super::captured::ComparedPair;
        use crate::settings::equivalence::is_engine_managed_attribute;

        let block_idx = self.block_of(block_items[0].2);
        let block = &self.blocks[block_idx];
        let mut st = self.fast_compare_collect_captured(shared, cache, block, block_items)?;
        if !st.eligible.iter().any(|&e| e) {
            return Ok(block_items.iter().map(|&(pos, _, _)| (pos, None)).collect());
        }

        let open = self.open_block(block_idx)?;
        let Some((str_opts, str_pool_entry, prop_names)) = build_block_string_meta(&open) else {
            return Ok(block_items.iter().map(|&(pos, _, _)| (pos, None)).collect());
        };
        let (specs, attr_groups, prop_order) = parse_block_group_specs(&open, &prop_names)?;
        if !attr_groups.is_empty() {
            self.fast_compare_attr_groups(
                shared,
                &open,
                &specs,
                &attr_groups,
                block_items,
                &mut st,
            )?;
        }
        let m = block_items.len();
        for (slot, &(_, obs_row, _)) in block_items.iter().enumerate() {
            if st.eligible[slot] && !st.stored_had_attrs[slot] {
                let cap_attrs = &shared.input.state.instances
                    [shared.input.records[obs_row].source_index]
                    .attributes;
                if cap_attrs.keys().any(|k| !is_engine_managed_attribute(k)) {
                    st.found_diff[slot] = true;
                    st.all_same[slot] = false;
                }
            }
        }

        let mut cap_seen = vec![false; st.cap_props.len()];
        let block_prop_names: AHashSet<&str> = prop_names.iter().copied().collect();
        self.fast_compare_prop_groups(
            shared,
            cache,
            (&str_opts, &str_pool_entry, &block_prop_names),
            (&specs, &prop_order, block.count),
            block_items,
            (&mut st, &mut cap_seen),
        )?;
        check_unmatched_captured_props(shared, cache, block_items, &mut st, &cap_seen);

        Ok((0..m)
            .map(|slot| {
                let pos = block_items[slot].0;
                let resolved = if !st.eligible[slot] {
                    None
                } else if st.found_diff[slot] || st.all_same[slot] {
                    let flags = if st.found_diff[slot] {
                        (false, false)
                    } else {
                        (true, true)
                    };
                    Some((
                        ComparedPair {
                            placed: true,
                            same_class: true,
                            written_as_today: true,
                            same_unique_id: st.stored_unique_id_matched[slot]
                                .unwrap_or(st.cap_unique_id[slot].is_none()),
                            holds_reference: st.holds_ref[slot],
                        },
                        flags,
                    ))
                } else {
                    None
                };
                (pos, resolved)
            })
            .collect())
    }

    fn fast_compare_collect_captured<'a>(
        &self,
        shared: &FastCompareShared<'a>,
        cache: &mut crate::settings::equivalence::CanonicalizeCache<'a>,
        block: &Block,
        block_items: &[(usize, usize, usize)],
    ) -> Result<FastCompareBlockState<'a>> {
        use super::captured::UNIQUE_ID;
        use crate::settings::equivalence::{
            PropertyCanonical, reconciliation_value_contains_reference as holds_reference,
        };

        let m = block_items.len();
        let mut st = FastCompareBlockState {
            row_to_slot: vec![u32::MAX; block.count],
            eligible: vec![true; m],
            found_diff: vec![false; m],
            all_same: vec![true; m],
            holds_ref: vec![false; m],
            stored_had_attrs: vec![false; m],
            cap_unique_id: vec![None; m],
            stored_unique_id_matched: vec![None; m],
            cap_props: Vec::with_capacity(m * 16),
            cap_ranges: Vec::with_capacity(m),
            cap_canon_targets: AHashMap::new(),
        };
        let mut slot_targets: Vec<&str> = Vec::new();
        let mut shared_safe: AHashMap<usize, bool> = AHashMap::new();

        for (slot, &(_, obs_row, stored_row)) in block_items.iter().enumerate() {
            st.row_to_slot[stored_row - block.first] = slot as u32;
            let row = &shared.input.structure[stored_row];
            let rec = &shared.input.records[obs_row];
            let stored_parent = match (row.parent_index, shared.input.moved) {
                (Some(parent), Some(moved)) => moved.in_state(parent),
                (parent, None) => parent,
                (None, Some(_)) => None,
            };
            if row.name != rec.name
                || row.class_name != rec.class_name
                || stored_parent != rec.parent_index
                || fast_compare_skip_class(rec.class_name)
            {
                st.eligible[slot] = false;
                st.cap_ranges.push((0, 0));
                continue;
            }

            let start = st.cap_props.len();
            slot_targets.clear();
            let mut ok = true;
            visit_settings_binary_properties(
                shared.input.state,
                rec,
                shared.input.lookup,
                |name, kind, source| {
                    if !ok || SETTINGS_RECORD_FIELDS.contains(&name) {
                        return Ok(());
                    }
                    if name == "TextScaled" || name == "TextWrapped" {
                        ok = false;
                        return Ok(());
                    }
                    if name == "Attributes" {
                        if kind != 8
                            || shared.input.state.instances[rec.source_index]
                                .attributes
                                .values()
                                .any(holds_reference)
                        {
                            ok = false;
                        }
                        return Ok(());
                    }
                    let is_canon = match cache.property_canonical(rec.class_name, name) {
                        PropertyCanonical::Unchanged => false,
                        PropertyCanonical::Renamed(target) => {
                            if slot_targets.contains(&target) {
                                ok = false;
                                return Ok(());
                            }
                            slot_targets.push(target);
                            true
                        }
                        PropertyCanonical::Migrated(targets) => {
                            for &target in targets {
                                if slot_targets.contains(&target) {
                                    ok = false;
                                    return Ok(());
                                }
                                slot_targets.push(target);
                            }
                            true
                        }
                        PropertyCanonical::Complex => {
                            ok = false;
                            return Ok(());
                        }
                    };
                    if name == UNIQUE_ID {
                        if st.cap_unique_id[slot].is_some() {
                            ok = false;
                            return Ok(());
                        }
                        let uid = match source {
                            SettingsBinaryValueSource::Native(NativeSettingsValue::UniqueId(s)) => {
                                Some(s.as_str())
                            }
                            SettingsBinaryValueSource::Property(v) => v
                                .as_object()
                                .filter(|o| {
                                    o.len() == 2
                                        && o.get("_type").and_then(Value::as_str)
                                            == Some("UniqueId")
                                })
                                .and_then(|o| o.get("value").and_then(Value::as_str)),
                            _ => None,
                        };
                        match uid {
                            Some(s) => st.cap_unique_id[slot] = Some(s),
                            None => ok = false,
                        }
                        return Ok(());
                    }
                    match fast_cap_val_from_source(
                        name,
                        kind,
                        &source,
                        shared.input.lookup,
                        &mut shared_safe,
                    ) {
                        Some((val, is_ref)) => {
                            if is_ref {
                                st.holds_ref[slot] = true;
                            }
                            st.cap_props.push((name, val, is_canon));
                        }
                        None => ok = false,
                    }
                    Ok(())
                },
            )?;

            let end = st.cap_props.len();
            if ok {
                st.cap_props[start..end].sort_unstable_by(|a, b| a.0.cmp(b.0));
                if st.cap_props[start..end]
                    .windows(2)
                    .any(|w| w[0].0 == w[1].0)
                {
                    ok = false;
                } else if !slot_targets.is_empty() {
                    if slot_targets.iter().any(|target| {
                        st.cap_props[start..end]
                            .binary_search_by_key(target, |p| p.0)
                            .is_ok()
                    }) {
                        ok = false;
                    } else {
                        st.cap_canon_targets.insert(slot, slot_targets.clone());
                    }
                }
            }
            st.eligible[slot] = ok;
            st.cap_ranges.push((start, end));
        }
        Ok(st)
    }

    fn fast_compare_attr_groups(
        &self,
        shared: &FastCompareShared<'_>,
        open: &OpenBlock,
        specs: &[BlockGroupSpec<'_>],
        attr_groups: &[usize],
        block_items: &[(usize, usize, usize)],
        st: &mut FastCompareBlockState<'_>,
    ) -> Result<()> {
        use crate::settings::equivalence::{
            reconciliation_store_values_map_equal,
            reconciliation_value_contains_reference as holds_reference, value_contains_enum_item,
        };

        let marked_strings: Vec<Cow<'_, str>> = open
            .strings
            .iter()
            .map(|s| match s {
                BlockString::Own(text) => Cow::Borrowed(text.as_str()),
                BlockString::Pool(entry) => Cow::Owned(marker(&self.hashes[*entry])),
            })
            .collect();
        let marked_refs: Vec<&str> = marked_strings.iter().map(Cow::as_ref).collect();
        let scope = ValueScope::from_borrowed(&marked_refs, self.rows, &self.budget, None);
        let empty_props = Map::new();
        let count = st.row_to_slot.len();
        for &group_idx in attr_groups {
            let (_, kind, value_count, body) = specs[group_idx];
            let mut reader = BytecodeReader::new(body);
            let _ = decode_cframe_group_header(kind, &mut reader)?;
            let mut prev_row = 0_usize;
            for position in 0..value_count {
                let row = next_group_instance(
                    prev_row,
                    reader.read_len("property instance index")?,
                    position,
                )?;
                prev_row = row;
                anyhow::ensure!(
                    row < count,
                    "A value block gives a value to a row it does not hold"
                );
                let slot_raw = st.row_to_slot[row];
                if slot_raw == u32::MAX || !st.eligible[slot_raw as usize] {
                    skip_raw_value_payload_fast(&mut reader, 8, 0)?;
                    continue;
                }
                let slot = slot_raw as usize;
                if st.stored_had_attrs[slot] {
                    st.eligible[slot] = false;
                    skip_raw_value_payload_fast(&mut reader, 8, 0)?;
                    continue;
                }
                let attributes = decode_attributes_payload(&mut reader, scope)?;
                if attributes
                    .values()
                    .any(|v| holds_marker(v) || holds_reference(v) || value_contains_enum_item(v))
                    || !values_encode_alike(&empty_props, &attributes, shared.input.root_service)
                {
                    st.eligible[slot] = false;
                    continue;
                }
                st.stored_had_attrs[slot] = true;
                let cap_attrs = &shared.input.state.instances
                    [shared.input.records[block_items[slot].1].source_index]
                    .attributes;
                if !reconciliation_store_values_map_equal(
                    shared.input.root_service,
                    &attributes,
                    cap_attrs,
                ) {
                    st.found_diff[slot] = true;
                    st.all_same[slot] = false;
                }
            }
            reader.finish()?;
        }
        Ok(())
    }

    fn fast_compare_prop_groups<'a, 'b>(
        &self,
        shared: &FastCompareShared<'a>,
        cache: &mut crate::settings::equivalence::CanonicalizeCache<'a>,
        (str_opts, str_pool_entry, block_prop_names): (
            &[Option<&'b str>],
            &[Option<usize>],
            &AHashSet<&'b str>,
        ),
        (specs, prop_order, count): (&[BlockGroupSpec<'b>], &[usize], usize),
        block_items: &[(usize, usize, usize)],
        (st, cap_seen): (&mut FastCompareBlockState<'a>, &mut [bool]),
    ) -> Result<()> {
        let m = block_items.len();
        let mut cursors: Vec<usize> = st.cap_ranges.iter().map(|&(s, _)| s).collect();
        let mut stored_canon_targets: AHashMap<usize, Vec<&str>> = AHashMap::new();
        let mut seen_in_run: Vec<u32> = Vec::new();
        let mut run_id: u32 = 0;
        let mut scratch = FastComparePropScratch::default();

        let mut run_start = 0_usize;
        while run_start < prop_order.len() {
            let name = specs[prop_order[run_start]].0;
            let mut run_end = run_start + 1;
            while run_end < prop_order.len() && specs[prop_order[run_end]].0 == name {
                run_end += 1;
            }
            let multi = run_end - run_start > 1;
            if multi {
                if seen_in_run.len() != m {
                    seen_in_run.resize(m, 0);
                }
                run_id += 1;
            }
            scratch.class_engine_state.clear();
            scratch.class_asset_ref.clear();

            let meta = FastCompareGroupMeta {
                str_opts,
                str_pool_entry,
                block_prop_names,
                count,
            };
            for &group_idx in &prop_order[run_start..run_end] {
                let mut state = FastCompareGroupMut {
                    st,
                    cap_seen,
                    cursors: &mut cursors,
                    stored_canon_targets: &mut stored_canon_targets,
                    scratch: &mut scratch,
                };
                self.fast_compare_single_group(
                    shared,
                    cache,
                    specs[group_idx],
                    &meta,
                    (block_items, multi, run_id, &mut seen_in_run),
                    &mut state,
                )?;
            }
            run_start = run_end;
        }
        Ok(())
    }

    fn fast_compare_single_group<'a, 'b>(
        &self,
        shared: &FastCompareShared<'a>,
        cache: &mut crate::settings::equivalence::CanonicalizeCache<'a>,
        (name, kind, value_count, body): BlockGroupSpec<'b>,
        meta: &FastCompareGroupMeta<'_, 'b>,
        (block_items, multi, run_id, seen_in_run): BlockCompareRun<'_>,
        state: &mut FastCompareGroupMut<'_, 'a, 'b>,
    ) -> Result<()> {
        use crate::settings::equivalence::{
            is_asset_reference_property, reconciliation_property_is_always_engine_state,
        };

        let flags = FastPropFlags::of(name);
        let mut reader = BytecodeReader::new(body);
        let cframe = decode_cframe_group_header(kind, &mut reader)?;
        let mut prev_row = 0_usize;
        let mut prev_ref = 0_usize;

        for position in 0..value_count {
            let row = next_group_instance(
                prev_row,
                reader.read_len("property instance index")?,
                position,
            )?;
            prev_row = row;
            anyhow::ensure!(
                row < meta.count,
                "A value block gives a value to a row it does not hold"
            );
            let slot_raw = state.st.row_to_slot[row];
            if slot_raw == u32::MAX || !state.st.eligible[slot_raw as usize] {
                skip_group_value_fast(&mut reader, kind, &cframe, self.rows, &mut prev_ref)?;
                continue;
            }
            let slot = slot_raw as usize;
            if multi {
                if seen_in_run[slot] == run_id {
                    state.st.eligible[slot] = false;
                } else {
                    seen_in_run[slot] = run_id;
                }
            }
            let class_name = shared.input.records[block_items[slot].1].class_name;
            let is_stored_canon = check_stored_prop_canonical(
                cache,
                class_name,
                name,
                flags.is_forced,
                (slot, meta.block_prop_names),
                state.st,
                state.stored_canon_targets,
            );
            if !state.st.eligible[slot] || matches!(kind, 10 | NESTED_ENUM_ITEM_KIND) {
                state.st.eligible[slot] = false;
                skip_group_value_fast(&mut reader, kind, &cframe, self.rows, &mut prev_ref)?;
                continue;
            }

            let cap_match = if flags.is_unique_id {
                None
            } else {
                advance_cap_cursor(state.st, state.cap_seen, state.cursors, slot, name)
            };
            if kind == 17 {
                self.fast_compare_ref_value(
                    &mut reader,
                    &mut prev_ref,
                    shared,
                    (slot, class_name, name, flags.is_meta_name, is_stored_canon),
                    cap_match,
                    (state.st, state.scratch),
                )?;
                continue;
            }
            if matches!(kind, 9 | 21) {
                skip_raw_value_payload_fast(&mut reader, kind, 0)?;
                state.st.all_same[slot] = false;
                continue;
            }
            if kind == 7 {
                compare_stored_array_value(
                    &mut reader,
                    meta.str_opts,
                    (slot, name, flags.is_unique_id),
                    cap_match,
                    (state.st, state.scratch),
                )?;
                continue;
            }
            if kind == 8 {
                self.fast_compare_object_value(
                    (&mut reader, cache),
                    shared,
                    meta,
                    (slot, class_name, name, flags, is_stored_canon),
                    cap_match,
                    (state.st, state.scratch),
                )?;
                continue;
            }
            if flags.is_unique_id {
                state.st.eligible[slot] = false;
                skip_raw_value_payload_fast(&mut reader, kind, 0)?;
                continue;
            }
            let is_engine = flags.is_meta_name
                || *state
                    .scratch
                    .class_engine_state
                    .entry(class_name)
                    .or_insert_with(|| {
                        reconciliation_property_is_always_engine_state(
                            shared.database,
                            class_name,
                            name,
                        )
                    });
            if kind == 16 {
                let a = decode_cframe_f64(&mut reader, &cframe)?;
                if !is_engine {
                    compare_stored_cframe_value(
                        cache,
                        (slot, class_name, name, flags.is_special, is_stored_canon),
                        &a,
                        cap_match,
                        state.st,
                    );
                }
                continue;
            }
            let Some(stored_val) = decode_stored_scalar_val(&mut reader, kind, meta.str_opts)?
            else {
                state.st.eligible[slot] = false;
                if kind != 19 {
                    skip_raw_value_payload_fast(&mut reader, kind, 0)?;
                }
                continue;
            };
            if is_engine || !state.st.eligible[slot] {
                continue;
            }
            if flags.is_source {
                let stored_ext = matches!(
                    stored_val,
                    Some(FastCapVal::Str(crate::settings::EXTERNAL_SOURCE_MARKER))
                );
                let cap_ext = matches!(
                    cap_match,
                    Some((FastCapVal::Str(crate::settings::EXTERNAL_SOURCE_MARKER), _))
                );
                if stored_ext && (cap_match.is_none() || cap_ext) {
                    continue;
                }
                if stored_ext || cap_ext {
                    state.st.all_same[slot] = false;
                    continue;
                }
            }
            let is_asset = *state
                .scratch
                .class_asset_ref
                .entry(class_name)
                .or_insert_with(|| is_asset_reference_property(class_name, name));
            apply_fast_scalar_comparison(
                cache,
                (
                    slot,
                    class_name,
                    name,
                    flags.is_special,
                    is_stored_canon,
                    is_asset,
                ),
                (stored_val, cap_match),
                state.st,
            );
        }
        reader.finish()?;
        Ok(())
    }

    fn fast_compare_ref_value<'a, 'b>(
        &self,
        reader: &mut BytecodeReader<'_>,
        prev_ref: &mut usize,
        shared: &FastCompareShared<'a>,
        (slot, class_name, name, is_meta_name, is_stored_canon): (usize, &'a str, &str, bool, bool),
        cap_match: Option<(FastCapVal<'a>, bool)>,
        (st, scratch): (
            &mut FastCompareBlockState<'a>,
            &mut FastComparePropScratch<'a, 'b>,
        ),
    ) -> Result<()> {
        use crate::settings::equivalence::reconciliation_property_is_always_engine_state;
        let target = decode_resolved_ref_target(reader, self.rows, prev_ref)?;
        st.holds_ref[slot] = true;
        if is_meta_name {
            return Ok(());
        }
        let is_engine = *scratch
            .class_engine_state
            .entry(class_name)
            .or_insert_with(|| {
                reconciliation_property_is_always_engine_state(shared.database, class_name, name)
            });
        if is_engine {
            return Ok(());
        }
        let target_in_state = match shared.input.moved {
            Some(moved) => moved.in_state(target),
            None => Some(target),
        };
        match cap_match {
            Some((FastCapVal::Ref(cap_target), _)) if target_in_state == Some(cap_target) => {}
            Some((FastCapVal::Ref(_), _)) => {
                st.all_same[slot] = false;
                if !is_stored_canon {
                    st.found_diff[slot] = true;
                }
            }
            _ => st.all_same[slot] = false,
        }
        Ok(())
    }

    fn fast_compare_object_value<'a, 'b>(
        &self,
        (reader, cache): (
            &mut BytecodeReader<'_>,
            &mut crate::settings::equivalence::CanonicalizeCache<'a>,
        ),
        shared: &FastCompareShared<'a>,
        meta: &FastCompareGroupMeta<'_, 'b>,
        (slot, class_name, name, flags, is_stored_canon): (
            usize,
            &'a str,
            &str,
            FastPropFlags,
            bool,
        ),
        cap_match: Option<(FastCapVal<'a>, bool)>,
        (st, scratch): (
            &mut FastCompareBlockState<'a>,
            &mut FastComparePropScratch<'a, 'b>,
        ),
    ) -> Result<()> {
        let (str_opts, str_pool_entry) = (meta.str_opts, meta.str_pool_entry);
        use super::captured::cached_long_hash;
        use crate::settings::equivalence::{
            OneSidedRule, hull_fidelity_from_b64_prefix,
            reconciliation_property_is_always_engine_state,
        };

        let save_pos = reader.position();
        if let Some((k0, v0_id, k1, v1_id)) = read_two_string_object_header(reader, str_opts) {
            if flags.is_unique_id {
                let uid = extract_two_field_unique_id(str_opts, k0, v0_id, k1, v1_id);
                match uid {
                    Some(s) => {
                        st.stored_unique_id_matched[slot] = Some(st.cap_unique_id[slot] == Some(s))
                    }
                    None => st.eligible[slot] = false,
                }
                return Ok(());
            }
            if let Some((stored_ty, b64_id)) =
                extract_two_field_payload_id(str_opts, k0, v0_id, k1, v1_id)
            {
                let is_engine =
                    *scratch
                        .class_engine_state
                        .entry(class_name)
                        .or_insert_with(|| {
                            reconciliation_property_is_always_engine_state(
                                shared.database,
                                class_name,
                                name,
                            )
                        });
                if !is_engine && st.all_same[slot] {
                    match cap_match {
                        Some((
                            FastCapVal::Payload {
                                ty: cap_ty,
                                b64: cap_b64,
                            },
                            _,
                        )) if stored_ty == cap_ty => {
                            let same_payload =
                                if let Some(stored_b64) = str_opts.get(b64_id).copied().flatten() {
                                    stored_b64 == cap_b64
                                } else if let Some(entry) =
                                    str_pool_entry.get(b64_id).copied().flatten()
                                {
                                    cap_b64.len() >= POOL_MIN_BYTES
                                        && self.hashes[entry]
                                            == cached_long_hash(cap_b64, shared.long_hashes)
                                } else {
                                    false
                                };
                            if !same_payload {
                                let hull_ok = flags.is_hull_prop
                                    && self.payload_hull_matches(
                                        cap_b64,
                                        str_opts.get(b64_id).copied().flatten(),
                                        str_pool_entry.get(b64_id).copied().flatten(),
                                        shared.hull_fidelities,
                                        hull_fidelity_from_b64_prefix,
                                    )?;
                                if !hull_ok {
                                    st.all_same[slot] = false;
                                }
                            }
                        }
                        None if !is_stored_canon
                            && matches!(
                                cache.one_sided_rule(class_name, name),
                                OneSidedRule::Ignored
                            ) => {}
                        _ => st.all_same[slot] = false,
                    }
                }
                return Ok(());
            }
        }
        reader.set_position(save_pos);
        if flags.is_unique_id || !stored_value_is_safe(reader, 8, str_opts, 0)? {
            st.eligible[slot] = false;
            return Ok(());
        }
        let end_pos = reader.position();
        if cap_match.is_none()
            && !is_stored_canon
            && matches!(
                cache.one_sided_rule(class_name, name),
                OneSidedRule::Ignored
            )
        {
            return Ok(());
        }
        self.compare_inline_object_value(
            reader,
            (save_pos, end_pos),
            (shared, str_opts),
            (slot, class_name, name, flags, is_stored_canon),
            cap_match,
            (st, scratch),
        )
    }

    fn compare_inline_object_value<'a, 'b>(
        &self,
        reader: &mut BytecodeReader<'_>,
        (save_pos, end_pos): (usize, usize),
        (shared, str_opts): (&FastCompareShared<'a>, &[Option<&'b str>]),
        (slot, class_name, name, flags, is_stored_canon): (
            usize,
            &'a str,
            &str,
            FastPropFlags,
            bool,
        ),
        cap_match: Option<(FastCapVal<'a>, bool)>,
        (st, scratch): (
            &mut FastCompareBlockState<'a>,
            &mut FastComparePropScratch<'a, 'b>,
        ),
    ) -> Result<()> {
        use crate::settings::equivalence::{
            is_asset_reference_property, reconciliation_property_is_always_engine_state,
            reconciliation_store_property_values_equal,
        };
        let is_engine = flags.is_meta_name
            || *scratch
                .class_engine_state
                .entry(class_name)
                .or_insert_with(|| {
                    reconciliation_property_is_always_engine_state(
                        shared.database,
                        class_name,
                        name,
                    )
                });
        if is_engine {
            return Ok(());
        }
        let is_asset = *scratch
            .class_asset_ref
            .entry(class_name)
            .or_insert_with(|| is_asset_reference_property(class_name, name));
        let cap_obj = match cap_match {
            Some((FastCapVal::Json(v), false)) => Some(v),
            _ => None,
        };
        if !st.found_diff[slot]
            && !flags.is_special
            && !is_stored_canon
            && !is_asset
            && let Some(cap_obj) = cap_obj
        {
            reader.set_position(save_pos);
            let fast = compare_inline_typed_object(reader, str_opts, cap_obj);
            reader.set_position(end_pos);
            if let Some(eq) = fast {
                if !eq {
                    st.all_same[slot] = false;
                    st.found_diff[slot] = true;
                }
                return Ok(());
            }
            reader.set_position(save_pos);
            let decoded = decode_inline_fast_object(reader, str_opts, 0)?;
            reader.set_position(end_pos);
            if let Some(stored_val) = decoded {
                let eq = reconciliation_store_property_values_equal(
                    shared.input.root_service,
                    class_name,
                    name,
                    Some(&stored_val),
                    Some(cap_obj),
                );
                if !eq {
                    st.all_same[slot] = false;
                    st.found_diff[slot] = true;
                }
                return Ok(());
            }
        }
        st.all_same[slot] = false;
        Ok(())
    }

    fn payload_hull_matches(
        &self,
        cap_b64: &str,
        stored_b64: Option<&str>,
        pool_entry: Option<usize>,
        hull_fidelities: &ShardedMap<usize, Option<u8>>,
        parse_fidelity: fn(&str) -> Option<u8>,
    ) -> Result<bool> {
        let cap_code = parse_fidelity(cap_b64);
        let stored_code = if let Some(b64) = stored_b64 {
            parse_fidelity(b64)
        } else if let Some(entry) = pool_entry {
            self.pool_hull_fidelity(entry, hull_fidelities)?
        } else {
            None
        };
        Ok(cap_code.is_some() && cap_code == stored_code)
    }
}

struct VerifyObsAttrBlocks<'a> {
    blocks_used: &'a [usize],
    opens: &'a [Arc<OpenBlock>],
    maps: &'a [Vec<u32>],
    specs: &'a [Vec<BlockGroupSpec<'a>>],
    attr_groups: &'a [Vec<usize>],
}

#[derive(Clone, Copy)]
enum FastVerifyVal<'a> {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(&'a str),
    PoolStr(usize),
    Enum(&'a str),
    Vec2([f32; 2]),
    Vec3([f32; 3]),
    UDim([f32; 2]),
    UDim2([f32; 4]),
    Color3([f32; 3]),
    Rect([f32; 4]),
    CFrame(usize),
    Ref(usize),
    Payload {
        ty: &'a str,
        own: Option<&'a str>,
        pool: Option<usize>,
    },
    Tags(usize, usize),
    RawComplex {
        kind: u8,
        bytes: &'a [u8],
        block: usize,
    },
    Unsupported,
}

#[inline]
fn color3_byte_round(c: f32) -> u8 {
    (c.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[inline]
fn verify_cframes_within_assembly_tolerance(a: &[f64; 12], b: &[f64; 12]) -> bool {
    (0..12).all(|i| {
        let tol = if i < 3 { 1e-3 } else { 1e-4 };
        a[i].is_finite() && b[i].is_finite() && (a[i] - b[i]).abs() <= tol
    })
}

fn verify_val_matches_default(
    val: FastVerifyVal<'_>,
    def: &crate::settings::equivalence::FastDefaultVal,
    cframes: &[[f64; 12]],
) -> bool {
    use crate::settings::equivalence::{
        FastDefaultVal, cframes_match_reconciliation, f32_values_match,
    };
    match (val, def) {
        (FastVerifyVal::Bool(a), FastDefaultVal::Bool(b)) => a == *b,
        (
            FastVerifyVal::I64(_) | FastVerifyVal::U64(_) | FastVerifyVal::F64(_),
            FastDefaultVal::Number { value, uses_f32 },
        ) => {
            let a = match val {
                FastVerifyVal::I64(x) => x as f64,
                FastVerifyVal::U64(x) => x as f64,
                FastVerifyVal::F64(x) => x,
                _ => unreachable!(),
            };
            a == *value || (*uses_f32 && f32_values_match(a, *value))
        }
        (FastVerifyVal::Str(a), FastDefaultVal::Str(b)) => a == b.as_str(),
        (FastVerifyVal::Enum(a), FastDefaultVal::Enum(b)) => a == b.as_str(),
        (FastVerifyVal::Vec2(a), FastDefaultVal::Vec2(b)) => {
            (0..2).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i])))
        }
        (FastVerifyVal::Vec3(a), FastDefaultVal::Vec3(b)) => {
            (0..3).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i])))
        }
        (FastVerifyVal::UDim(a), FastDefaultVal::UDim(b)) => {
            f32_values_match(f64::from(a[0]), f64::from(b[0])) && a[1] == b[1]
        }
        (FastVerifyVal::UDim2(a), FastDefaultVal::UDim2(b)) => {
            f32_values_match(f64::from(a[0]), f64::from(b[0]))
                && a[1] == b[1]
                && f32_values_match(f64::from(a[2]), f64::from(b[2]))
                && a[3] == b[3]
        }
        (FastVerifyVal::Color3(a), FastDefaultVal::Color3(b)) => {
            (0..3).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i])))
        }
        (FastVerifyVal::Rect(a), FastDefaultVal::Rect(b)) => {
            (0..4).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i])))
        }
        (FastVerifyVal::CFrame(idx), FastDefaultVal::CFrame(b)) => {
            cframes_match_reconciliation(&cframes[idx], b)
        }
        _ => false,
    }
}

type BlockStringMeta<'a> = (Vec<Option<&'a str>>, Vec<Option<usize>>, Vec<&'a str>);

fn build_block_string_meta(open: &OpenBlock) -> Option<BlockStringMeta<'_>> {
    let str_opts: Vec<Option<&str>> = open
        .strings
        .iter()
        .map(|s| match s {
            BlockString::Own(t) => Some(t.as_str()),
            BlockString::Pool(_) => None,
        })
        .collect();
    let str_pool: Vec<Option<usize>> = open
        .strings
        .iter()
        .map(|s| match s {
            BlockString::Own(_) => None,
            BlockString::Pool(e) => Some(*e),
        })
        .collect();
    let mut prop_names: Vec<&str> = Vec::with_capacity(open.properties.len());
    for &id in &open.properties {
        prop_names.push(str_opts[id]?);
    }
    Some((str_opts, str_pool, prop_names))
}

struct VerifyGroupSource<'a> {
    row_count: usize,
    total_rows: usize,
    str_opts: &'a [Option<&'a str>],
    str_pool: &'a [Option<usize>],
    row_map: Option<&'a [u32]>,
    block: usize,
}

struct VerifyGroupBuffers<'a, 'b> {
    vals: &'b mut [Option<FastVerifyVal<'a>>],
    cframes: &'b mut Vec<[f64; 12]>,
    tags: &'b mut Vec<&'a str>,
    touched: &'b mut Vec<usize>,
    touched_mark: &'b mut [u32],
    mark_id: u32,
}

fn decode_verify_group_into<'a>(
    (name, kind, value_count, body): BlockGroupSpec<'a>,
    src: &VerifyGroupSource<'a>,
    ok: &[bool],
    buf: &mut VerifyGroupBuffers<'a, '_>,
) -> Result<()> {
    let mut reader = BytecodeReader::new(body);
    let cframe = decode_cframe_group_header(kind, &mut reader)?;
    let mut prev_row = 0_usize;
    let mut prev_ref = 0_usize;
    for position in 0..value_count {
        let raw_row = next_group_instance(
            prev_row,
            reader.read_len("property instance index")?,
            position,
        )?;
        prev_row = raw_row;
        anyhow::ensure!(
            raw_row < src.row_count,
            "A value block gives a value to a row it does not hold"
        );
        let mapped = match src.row_map {
            Some(map) => {
                let target = map[raw_row];
                (target != u32::MAX).then_some(target as usize)
            }
            None => Some(raw_row),
        };
        let Some(row) = mapped.filter(|&r| ok[r]) else {
            skip_group_value_fast(&mut reader, kind, &cframe, src.total_rows, &mut prev_ref)?;
            continue;
        };
        let val = decode_verify_wire_val(
            &mut reader,
            body,
            (name, kind, &cframe, &mut prev_ref),
            src,
            buf.cframes,
            buf.tags,
        )?;
        if buf.touched_mark[row] != buf.mark_id {
            buf.touched_mark[row] = buf.mark_id;
            buf.touched.push(row);
        }
        buf.vals[row] = Some(val);
    }
    reader.finish()?;
    Ok(())
}

fn decode_verify_wire_val<'a>(
    reader: &mut BytecodeReader<'_>,
    body: &'a [u8],
    (name, kind, cframe, prev_ref): (&str, u8, &CFrameGroupDecoder, &mut usize),
    src: &VerifyGroupSource<'a>,
    cframes: &mut Vec<[f64; 12]>,
    tags: &mut Vec<&'a str>,
) -> Result<FastVerifyVal<'a>> {
    let val = match kind {
        0 => FastVerifyVal::Null,
        1 => FastVerifyVal::Bool(false),
        2 => FastVerifyVal::Bool(true),
        3 => FastVerifyVal::I64(unzigzag_i64(reader.read_var_u64()?)),
        4 => FastVerifyVal::U64(reader.read_var_u64()?),
        5 => FastVerifyVal::F64(read_f64(reader)?),
        20 => FastVerifyVal::F64(f64::from(read_f32(reader)?)),
        6 => {
            let id = reader.read_len("value string id")?;
            if let Some(s) = src.str_opts.get(id).copied().flatten() {
                FastVerifyVal::Str(s)
            } else if let Some(entry) = src.str_pool.get(id).copied().flatten() {
                FastVerifyVal::PoolStr(entry)
            } else {
                bail!("Invalid value string id {id}");
            }
        }
        19 => {
            let id = reader.read_len("enum item string id")?;
            match src.str_opts.get(id).copied().flatten() {
                Some(s) => FastVerifyVal::Enum(s),
                None => FastVerifyVal::Unsupported,
            }
        }
        11 => FastVerifyVal::Vec2([read_f32(reader)?, read_f32(reader)?]),
        12 => FastVerifyVal::Vec3([read_f32(reader)?, read_f32(reader)?, read_f32(reader)?]),
        13 => FastVerifyVal::UDim([read_f32(reader)?, read_f32(reader)?]),
        14 => FastVerifyVal::UDim2([
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
        ]),
        15 => FastVerifyVal::Color3([read_f32(reader)?, read_f32(reader)?, read_f32(reader)?]),
        18 => FastVerifyVal::Rect([
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
            read_f32(reader)?,
        ]),
        16 => {
            let idx = cframes.len();
            cframes.push(decode_cframe_f64(reader, cframe)?);
            FastVerifyVal::CFrame(idx)
        }
        17 => FastVerifyVal::Ref(decode_resolved_ref_target(
            reader,
            src.total_rows,
            prev_ref,
        )?),
        7 if name == "Tags" => {
            let save = reader.position();
            let start = tags.len();
            let parsed = (|| -> Option<()> {
                let arr_len = reader.read_collection_len("array length").ok()?;
                for _ in 0..arr_len {
                    if reader.read_u8().ok()? != 6 {
                        return None;
                    }
                    let id = reader.read_len("value string id").ok()?;
                    tags.push((*src.str_opts.get(id)?)?);
                }
                Some(())
            })();
            if parsed.is_some() {
                FastVerifyVal::Tags(start, tags.len())
            } else {
                tags.truncate(start);
                reader.set_position(save);
                skip_raw_value_payload_fast(reader, 7, 0)?;
                FastVerifyVal::RawComplex {
                    kind: 7,
                    bytes: &body[save..reader.position()],
                    block: src.block,
                }
            }
        }
        7 | 9 | 21 => {
            let save = reader.position();
            skip_raw_value_payload_fast(reader, kind, 0)?;
            FastVerifyVal::RawComplex {
                kind,
                bytes: &body[save..reader.position()],
                block: src.block,
            }
        }
        8 => {
            let save = reader.position();
            let parsed = read_two_string_object_header(reader, src.str_opts).and_then(
                |(k0, v0_id, k1, v1_id)| {
                    let (ty, b64_id) =
                        extract_two_field_payload_id(src.str_opts, k0, v0_id, k1, v1_id)?;
                    let own = src.str_opts.get(b64_id).copied().flatten();
                    let pool = src.str_pool.get(b64_id).copied().flatten();
                    (own.is_some() || pool.is_some()).then_some(FastVerifyVal::Payload {
                        ty,
                        own,
                        pool,
                    })
                },
            );
            match parsed {
                Some(v) => v,
                None => {
                    reader.set_position(save);
                    skip_raw_value_payload_fast(reader, 8, 0)?;
                    FastVerifyVal::RawComplex {
                        kind: 8,
                        bytes: &body[save..reader.position()],
                        block: src.block,
                    }
                }
            }
        }
        _ => {
            skip_raw_value_payload_fast(reader, kind, 0)?;
            FastVerifyVal::Unsupported
        }
    };
    Ok(val)
}

pub(crate) struct FastVerifyInput<'a> {
    pub(crate) desired_bytes: &'a [u8],
    pub(crate) observed_bytes: &'a [u8],
    pub(crate) desired_instances: &'a [SettingsBytecodeInstance],
    pub(crate) observed_instances: &'a [SettingsBytecodeInstance],
    pub(crate) places: &'a [usize],
    pub(crate) by_order: &'a [bool],
    pub(crate) skip: &'a [bool],
    pub(crate) has_before: bool,
}

struct FastVerifyStores<'a> {
    input: &'a FastVerifyInput<'a>,
    des: BlockStore<'a>,
    obs: BlockStore<'a>,
    database: Option<&'a rbx_reflection::ReflectionDatabase<'a>>,
    des_hulls: ShardedMap<usize, Option<u8>>,
    obs_hulls: ShardedMap<usize, Option<u8>>,
    obs_opened: Vec<Mutex<Option<Arc<OpenBlock>>>>,
    obs_remaining: Vec<AtomicUsize>,
}

const MAX_FAST_VERIFY_OBS_BYTES: usize = 32 * 1024 * 1024;

impl<'s> FastVerifyStores<'s> {
    fn obs_blocks_within_budget(&self, obs_blocks_used: &[usize]) -> bool {
        !obs_blocks_used.is_empty()
            && obs_blocks_used
                .iter()
                .try_fold(0_usize, |total, &ob| {
                    total
                        .checked_add(self.obs.blocks[ob].frame.decoded)
                        .filter(|&sum| sum <= MAX_FAST_VERIFY_OBS_BYTES)
                })
                .is_some()
    }

    fn open_obs_block(&self, idx: usize) -> Result<Arc<OpenBlock>> {
        let should_cache = {
            let slot = self.obs_opened[idx].lock_recover();
            if let Some(open) = slot.as_ref() {
                return Ok(Arc::clone(open));
            }
            self.obs_remaining[idx].load(Ordering::Acquire) > 1
        };
        let opened = Arc::new(self.obs.open_block(idx)?);
        if should_cache {
            let mut slot = self.obs_opened[idx].lock_recover();
            if let Some(existing) = slot.as_ref() {
                return Ok(Arc::clone(existing));
            }
            *slot = Some(Arc::clone(&opened));
        }
        Ok(opened)
    }

    fn release_obs_blocks(&self, obs_blocks_used: &[usize]) {
        for &ob in obs_blocks_used {
            if self.obs_remaining[ob].fetch_sub(1, Ordering::AcqRel) == 1 {
                *self.obs_opened[ob].lock_recover() = None;
            }
        }
    }

    fn verify_block(
        &self,
        cache: &mut crate::settings::equivalence::CanonicalizeCache,
        block_idx: usize,
        mut ok: Vec<bool>,
        obs_blocks_used: &[usize],
    ) -> Result<Vec<bool>> {
        let block = &self.des.blocks[block_idx];
        let first = block.first;
        let count = block.count;
        if !self.obs_blocks_within_budget(obs_blocks_used) {
            return Ok(vec![false; count]);
        }
        let _release =
            crate::system::files::OnDrop::new(|| self.release_obs_blocks(obs_blocks_used));

        let des_open = self.des.open_block(block_idx)?;
        let Some((des_str_opts, des_str_pool, des_prop_names)) = build_block_string_meta(&des_open)
        else {
            return Ok(vec![false; count]);
        };
        let mut obs_opens: Vec<Arc<OpenBlock>> = Vec::with_capacity(obs_blocks_used.len());
        for &ob in obs_blocks_used {
            obs_opens.push(self.open_obs_block(ob)?);
        }
        let mut obs_metas = Vec::with_capacity(obs_opens.len());
        for open in &obs_opens {
            let Some(meta) = build_block_string_meta(open) else {
                return Ok(vec![false; count]);
            };
            obs_metas.push(meta);
        }

        let obs_maps = self.build_obs_row_maps(first, &ok, obs_blocks_used);
        let (des_specs, des_attr_groups, des_prop_order) =
            parse_block_group_specs(&des_open, &des_prop_names)?;
        let mut obs_specs_all: Vec<Vec<BlockGroupSpec<'_>>> = Vec::with_capacity(obs_opens.len());
        let mut obs_attr_groups_all: Vec<Vec<usize>> = Vec::with_capacity(obs_opens.len());
        let mut obs_prop_orders_all: Vec<Vec<usize>> = Vec::with_capacity(obs_opens.len());
        let mut any_obs_attrs = false;
        for (k, open) in obs_opens.iter().enumerate() {
            let (specs, attr_groups, prop_order) = parse_block_group_specs(open, &obs_metas[k].2)?;
            any_obs_attrs |= !attr_groups.is_empty();
            obs_specs_all.push(specs);
            obs_attr_groups_all.push(attr_groups);
            obs_prop_orders_all.push(prop_order);
        }

        if !des_attr_groups.is_empty() || any_obs_attrs {
            self.verify_block_attrs(
                first,
                (&des_open, &des_specs, &des_attr_groups),
                &VerifyObsAttrBlocks {
                    blocks_used: obs_blocks_used,
                    opens: &obs_opens,
                    maps: &obs_maps,
                    specs: &obs_specs_all,
                    attr_groups: &obs_attr_groups_all,
                },
                &mut ok,
            )?;
        }

        let mut all_names: Vec<&str> = Vec::with_capacity(des_prop_order.len() + 32);
        for &idx in &des_prop_order {
            all_names.push(des_specs[idx].0);
        }
        for (k, order) in obs_prop_orders_all.iter().enumerate() {
            for &idx in order {
                all_names.push(obs_specs_all[k][idx].0);
            }
        }
        all_names.sort_unstable();
        all_names.dedup();

        let mut scratch = VerifyBlockScratch::new(count);
        let mut used_exact_special = vec![false; count];
        let mut had_one_sided = vec![false; count];
        let mut di = 0_usize;
        let mut ois: Vec<usize> = vec![0; obs_opens.len()];
        let des_src = VerifyGroupSource {
            row_count: count,
            total_rows: self.des.rows,
            str_opts: &des_str_opts,
            str_pool: &des_str_pool,
            row_map: None,
            block: 0,
        };

        for name in all_names {
            let des_start = di;
            while di < des_prop_order.len() && des_specs[des_prop_order[di]].0 == name {
                di += 1;
            }
            scratch.next_property();
            for &gidx in &des_prop_order[des_start..di] {
                let mut buf = VerifyGroupBuffers {
                    vals: &mut scratch.des_vals,
                    cframes: &mut scratch.des_cframes,
                    tags: &mut scratch.des_tags,
                    touched: &mut scratch.touched,
                    touched_mark: &mut scratch.touched_mark,
                    mark_id: scratch.mark_id,
                };
                decode_verify_group_into(des_specs[gidx], &des_src, &ok, &mut buf)?;
            }
            for k in 0..obs_opens.len() {
                let order = &obs_prop_orders_all[k];
                let specs = &obs_specs_all[k];
                let start = ois[k];
                while ois[k] < order.len() && specs[order[ois[k]]].0 == name {
                    ois[k] += 1;
                }
                if start == ois[k] {
                    continue;
                }
                let obs_src = VerifyGroupSource {
                    row_count: self.obs.blocks[obs_blocks_used[k]].count,
                    total_rows: self.obs.rows,
                    str_opts: &obs_metas[k].0,
                    str_pool: &obs_metas[k].1,
                    row_map: Some(&obs_maps[k]),
                    block: k,
                };
                for &gidx in &order[start..ois[k]] {
                    let mut buf = VerifyGroupBuffers {
                        vals: &mut scratch.obs_vals,
                        cframes: &mut scratch.obs_cframes,
                        tags: &mut scratch.obs_tags,
                        touched: &mut scratch.touched,
                        touched_mark: &mut scratch.touched_mark,
                        mark_id: scratch.mark_id,
                    };
                    decode_verify_group_into(specs[gidx], &obs_src, &ok, &mut buf)?;
                }
            }
            self.verify_property_rows(
                cache,
                first,
                name,
                &mut scratch,
                (&des_open, &obs_opens),
                (&mut used_exact_special, &mut had_one_sided),
                &mut ok,
            )?;
        }
        for row in 0..count {
            if used_exact_special[row] && had_one_sided[row] {
                ok[row] = false;
            }
        }
        Ok(ok)
    }

    fn eligible_rows_and_obs_blocks(&self, first: usize, count: usize) -> (Vec<bool>, Vec<usize>) {
        let mut ok = vec![false; count];
        let mut obs_blocks_used: Vec<usize> = Vec::new();
        for (row, slot) in ok.iter_mut().enumerate() {
            let global = first + row;
            if self.input.skip[global] {
                continue;
            }
            let obs_global = self.input.places[global];
            let ds = &self.input.desired_instances[global];
            let os = &self.input.observed_instances[obs_global];
            if ds.parent_index.is_some()
                && os.parent_index.is_some()
                && ds.name == os.name
                && ds.class_name == os.class_name
                && ds.parent_index.map(|p| self.input.places[p]) == os.parent_index
                && !fast_compare_skip_class(ds.class_name.as_str())
            {
                *slot = true;
                let ob = self.obs.block_of(obs_global);
                if !obs_blocks_used.contains(&ob) {
                    obs_blocks_used.push(ob);
                }
            }
        }
        obs_blocks_used.sort_unstable();
        (ok, obs_blocks_used)
    }

    fn build_obs_row_maps(
        &self,
        first: usize,
        ok: &[bool],
        obs_blocks_used: &[usize],
    ) -> Vec<Vec<u32>> {
        let mut obs_maps: Vec<Vec<u32>> = obs_blocks_used
            .iter()
            .map(|&ob| vec![u32::MAX; self.obs.blocks[ob].count])
            .collect();
        for (row, &eligible) in ok.iter().enumerate() {
            if !eligible {
                continue;
            }
            let obs_global = self.input.places[first + row];
            let ob = self.obs.block_of(obs_global);
            let pos = obs_blocks_used
                .binary_search(&ob)
                .expect("obs block recorded");
            obs_maps[pos][obs_global - self.obs.blocks[ob].first] = row as u32;
        }
        obs_maps
    }

    fn verify_block_attrs(
        &self,
        first: usize,
        (des_open, des_specs, des_attr_groups): (&OpenBlock, &[BlockGroupSpec<'_>], &[usize]),
        obs: &VerifyObsAttrBlocks<'_>,
        ok: &mut [bool],
    ) -> Result<()> {
        use crate::settings::equivalence::{
            is_engine_managed_attribute, reconciliation_values_equal,
        };

        let count = ok.len();
        let mut des_attrs: Vec<Option<Map<String, Value>>> = (0..count).map(|_| None).collect();
        let mut obs_attrs: Vec<Option<Map<String, Value>>> = (0..count).map(|_| None).collect();
        if !des_attr_groups.is_empty() {
            decode_verify_attrs_groups(
                &self.des,
                des_open,
                des_specs,
                des_attr_groups,
                (count, None),
                ok,
                &mut des_attrs,
            )?;
        }
        for (k, attr_groups) in obs.attr_groups.iter().enumerate() {
            if !attr_groups.is_empty() {
                decode_verify_attrs_groups(
                    &self.obs,
                    &obs.opens[k],
                    &obs.specs[k],
                    attr_groups,
                    (
                        self.obs.blocks[obs.blocks_used[k]].count,
                        Some(&obs.maps[k]),
                    ),
                    ok,
                    &mut obs_attrs,
                )?;
            }
        }
        for (row, slot) in ok.iter_mut().enumerate() {
            if !*slot {
                continue;
            }
            let global = first + row;
            let by_ord = self.input.by_order[global];
            let has_before = self.input.has_before;
            let matched = match (&des_attrs[row], &obs_attrs[row]) {
                (None, None) => true,
                (Some(da), Some(oa)) => {
                    !(by_ord && oa.keys().any(|k| !da.contains_key(k))
                        || has_before
                            && oa
                                .keys()
                                .any(|k| !da.contains_key(k) && !is_engine_managed_attribute(k)))
                        && da.iter().all(|(k, dv)| {
                            is_engine_managed_attribute(k)
                                || oa.get(k) == Some(dv)
                                || oa
                                    .get(k)
                                    .is_some_and(|ov| reconciliation_values_equal(dv, ov, false))
                        })
                }
                (Some(da), None) => da.keys().all(|k| is_engine_managed_attribute(k)),
                (None, Some(oa)) => {
                    !(by_ord && !oa.is_empty()
                        || has_before && oa.keys().any(|k| !is_engine_managed_attribute(k)))
                }
            };
            if !matched {
                *slot = false;
            }
        }
        Ok(())
    }

    fn verify_property_rows<'a>(
        &self,
        cache: &mut crate::settings::equivalence::CanonicalizeCache,
        first: usize,
        name: &str,
        scratch: &mut VerifyBlockScratch<'a>,
        blocks: (&OpenBlock, &[Arc<OpenBlock>]),
        (used_exact_special, had_one_sided): (&mut [bool], &mut [bool]),
        ok: &mut [bool],
    ) -> Result<()>
    where
        's: 'a,
    {
        use crate::settings::equivalence::{
            OneSidedRule, PropertyCanonical, reconciliation_property_is_always_engine_state,
            reconciliation_property_is_derived,
        };

        let is_unsupported_prop = matches!(name, "Attributes" | "CurrentCamera" | "PhysicsGrid");
        let is_exact_only_prop = matches!(name, "TextScaled" | "TextWrapped");
        let is_skipped_meta = matches!(name, "UniqueId" | "HistoryId" | "ScriptGuid")
            || reconciliation_property_is_derived(name);
        scratch.class_engine.clear();
        scratch.class_asset.clear();

        for idx in 0..scratch.touched.len() {
            let row = scratch.touched[idx];
            let dv = scratch.des_vals[row].take();
            let ov = scratch.obs_vals[row].take();
            if !ok[row] {
                continue;
            }
            if is_unsupported_prop {
                ok[row] = false;
                continue;
            }
            let global = first + row;
            let class_name = self.input.desired_instances[global].class_name.as_str();
            if is_exact_only_prop
                || !matches!(
                    cache.property_canonical(class_name, name),
                    PropertyCanonical::Unchanged
                )
            {
                let matched = match (dv, ov) {
                    (Some(d), Some(o)) => {
                        used_exact_special[row] = true;
                        self.verify_values_exact_match((d, o), scratch, blocks)?
                    }
                    _ => false,
                };
                if !matched {
                    ok[row] = false;
                }
                continue;
            }
            let skip_prop = is_skipped_meta
                || (name == "Source" && crate::editor::sync::is_lua_source_class(class_name))
                || *scratch.class_engine.entry(class_name).or_insert_with(|| {
                    reconciliation_property_is_always_engine_state(self.database, class_name, name)
                });
            if skip_prop {
                if dv.is_none() && ov.is_some() && self.input.by_order[global] {
                    ok[row] = false;
                }
                continue;
            }
            let matched = match (dv, ov) {
                (Some(d), Some(o)) => {
                    self.verify_values_pair_match(cache, class_name, name, (d, o), scratch, blocks)?
                }
                (Some(d), None) => {
                    had_one_sided[row] = true;
                    match cache.verify_one_sided_rule(class_name, name) {
                        OneSidedRule::Ignored => true,
                        OneSidedRule::Default(ref def) => {
                            verify_val_matches_default(d, def, &scratch.des_cframes)
                        }
                        OneSidedRule::NoDefault => false,
                    }
                }
                (None, Some(_)) => {
                    had_one_sided[row] = true;
                    !self.input.by_order[global] && !self.input.has_before
                }
                (None, None) => true,
            };
            if !matched {
                ok[row] = false;
            }
        }
        Ok(())
    }

    fn verify_values_exact_match<'a>(
        &self,
        (d, o): (FastVerifyVal<'a>, FastVerifyVal<'a>),
        scratch: &VerifyBlockScratch<'a>,
        (des_open, obs_opens): (&OpenBlock, &[Arc<OpenBlock>]),
    ) -> Result<bool> {
        let f32s = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(&x, &y)| f32_finite_bits_eq(x, y));
        let matched = match (d, o) {
            (FastVerifyVal::Null, FastVerifyVal::Null) => true,
            (FastVerifyVal::Bool(a), FastVerifyVal::Bool(b)) => a == b,
            (FastVerifyVal::I64(a), FastVerifyVal::I64(b)) => a == b,
            (FastVerifyVal::U64(a), FastVerifyVal::U64(b)) => a == b,
            (FastVerifyVal::F64(a), FastVerifyVal::F64(b)) => f64_finite_bits_eq(a, b),
            (FastVerifyVal::Str(a), FastVerifyVal::Str(b)) => a == b,
            (FastVerifyVal::PoolStr(e1), FastVerifyVal::PoolStr(e2)) => {
                self.des.hashes[e1] == self.obs.hashes[e2]
            }
            (FastVerifyVal::Enum(a), FastVerifyVal::Enum(b)) => a == b,
            (FastVerifyVal::Vec2(a), FastVerifyVal::Vec2(b)) => f32s(&a, &b),
            (FastVerifyVal::Vec3(a), FastVerifyVal::Vec3(b)) => f32s(&a, &b),
            (FastVerifyVal::UDim(a), FastVerifyVal::UDim(b)) => f32s(&a, &b),
            (FastVerifyVal::UDim2(a), FastVerifyVal::UDim2(b)) => f32s(&a, &b),
            (FastVerifyVal::Color3(a), FastVerifyVal::Color3(b)) => f32s(&a, &b),
            (FastVerifyVal::Rect(a), FastVerifyVal::Rect(b)) => f32s(&a, &b),
            (FastVerifyVal::CFrame(di), FastVerifyVal::CFrame(oi)) => scratch.des_cframes[di]
                .iter()
                .zip(&scratch.obs_cframes[oi])
                .all(|(&x, &y)| f64_finite_bits_eq(x, y)),
            (FastVerifyVal::Ref(t1), FastVerifyVal::Ref(t2)) => self.input.places[t1] == t2,
            (
                FastVerifyVal::Payload {
                    ty: t1,
                    own: o1,
                    pool: p1,
                },
                FastVerifyVal::Payload {
                    ty: t2,
                    own: o2,
                    pool: p2,
                },
            ) if t1 == t2 => match (o1, p1, o2, p2) {
                (Some(a), _, Some(b), _) => a == b,
                (_, Some(e1), _, Some(e2)) => self.des.hashes[e1] == self.obs.hashes[e2],
                _ => false,
            },
            (
                FastVerifyVal::RawComplex {
                    kind: dk,
                    bytes: db,
                    ..
                },
                FastVerifyVal::RawComplex {
                    kind: ok_kind,
                    bytes: ob,
                    block: ok_blk,
                },
            ) if dk == ok_kind => raw_complex_bytes_equal_no_refs(
                dk,
                db,
                ob,
                (&self.des, des_open),
                (&self.obs, &obs_opens[ok_blk]),
            )?,
            _ => false,
        };
        Ok(matched)
    }

    fn verify_values_pair_match<'a>(
        &self,
        cache: &mut crate::settings::equivalence::CanonicalizeCache,
        class_name: &'a str,
        name: &str,
        (d, o): (FastVerifyVal<'a>, FastVerifyVal<'a>),
        scratch: &mut VerifyBlockScratch<'a>,
        (des_open, obs_opens): (&OpenBlock, &[Arc<OpenBlock>]),
    ) -> Result<bool> {
        use crate::settings::equivalence::{
            canonical_asset_text, cframes_match_reconciliation, f32_values_match,
            hull_fidelity_from_b64_prefix, is_asset_reference_property,
        };

        if matches!(d, FastVerifyVal::Unsupported) || matches!(o, FastVerifyVal::Unsupported) {
            return Ok(false);
        }
        if matches!(
            name,
            "Velocity" | "RotVelocity" | "AssemblyLinearVelocity" | "AssemblyAngularVelocity"
        ) {
            return Ok(true);
        }
        if name == crate::roblox::schema::TEXTURE_PACK_PROPERTY
            && crate::roblox::schema::has_protected_texture_pack(class_name)
            && match o {
                FastVerifyVal::Str(s) => !s.is_empty(),
                FastVerifyVal::PoolStr(_) => true,
                _ => false,
            }
        {
            return Ok(true);
        }
        if name == "TriangleCount"
            && matches!(
                class_name,
                "UnionOperation" | "NegateOperation" | "IntersectOperation"
            )
            && matches!(
                d,
                FastVerifyVal::I64(_) | FastVerifyVal::U64(_) | FastVerifyVal::F64(_)
            )
            && matches!(
                o,
                FastVerifyVal::I64(_) | FastVerifyVal::U64(_) | FastVerifyVal::F64(_)
            )
        {
            return Ok(true);
        }
        if name == "CollisionFidelity"
            && crate::rbx::encode::is_solid_model_operation(class_name)
            && let (FastVerifyVal::Enum(a), FastVerifyVal::Enum(b)) = (d, o)
            && matches!(a, "Box" | "Default")
            && matches!(b, "Box" | "Default")
        {
            return Ok(true);
        }

        let matched = match (d, o) {
            (FastVerifyVal::Null, FastVerifyVal::Null) => true,
            (FastVerifyVal::Bool(a), FastVerifyVal::Bool(b)) => a == b,
            (
                FastVerifyVal::I64(_) | FastVerifyVal::U64(_) | FastVerifyVal::F64(_),
                FastVerifyVal::I64(_) | FastVerifyVal::U64(_) | FastVerifyVal::F64(_),
            ) => {
                let to_num = |v| match v {
                    FastVerifyVal::I64(x) => FastNum::I64(x),
                    FastVerifyVal::U64(x) => FastNum::U64(x),
                    FastVerifyVal::F64(x) => FastNum::F64(x),
                    _ => unreachable!(),
                };
                let (left, right) = (to_num(d), to_num(o));
                exact_fast_numbers_equal(left, right)
                    || (cache.property_uses_f32(class_name, name)
                        && f32_values_match(left.as_f64(), right.as_f64()))
            }
            (FastVerifyVal::Str(a), FastVerifyVal::Str(b)) => {
                a == b
                    || (*scratch
                        .class_asset
                        .entry(class_name)
                        .or_insert_with(|| is_asset_reference_property(class_name, name))
                        && canonical_asset_text(a) == canonical_asset_text(b))
            }
            (FastVerifyVal::PoolStr(e1), FastVerifyVal::PoolStr(e2)) => {
                self.des.hashes[e1] == self.obs.hashes[e2]
            }
            (FastVerifyVal::Enum(a), FastVerifyVal::Enum(b)) => a == b,
            (FastVerifyVal::Vec2(a), FastVerifyVal::Vec2(b)) => {
                (0..2).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i])))
            }
            (FastVerifyVal::Vec3(a), FastVerifyVal::Vec3(b)) => {
                (0..3).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i])))
            }
            (FastVerifyVal::UDim(a), FastVerifyVal::UDim(b)) => {
                f32_values_match(f64::from(a[0]), f64::from(b[0])) && a[1] == b[1]
            }
            (FastVerifyVal::UDim2(a), FastVerifyVal::UDim2(b)) => {
                f32_values_match(f64::from(a[0]), f64::from(b[0]))
                    && a[1] == b[1]
                    && f32_values_match(f64::from(a[2]), f64::from(b[2]))
                    && a[3] == b[3]
            }
            (FastVerifyVal::Color3(a), FastVerifyVal::Color3(b)) => {
                (0..3).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i])))
                    || (cache.property_is_color3uint8(class_name, name)
                        && (0..3).all(|i| color3_byte_round(a[i]) == color3_byte_round(b[i])))
            }
            (FastVerifyVal::Rect(a), FastVerifyVal::Rect(b)) => {
                (0..4).all(|i| f32_values_match(f64::from(a[i]), f64::from(b[i])))
            }
            (FastVerifyVal::CFrame(di), FastVerifyVal::CFrame(oi)) => {
                let da = &scratch.des_cframes[di];
                let oa = &scratch.obs_cframes[oi];
                cframes_match_reconciliation(da, oa)
                    || (name == "CFrame" && verify_cframes_within_assembly_tolerance(da, oa))
            }
            (FastVerifyVal::Ref(t1), FastVerifyVal::Ref(t2)) => self.input.places[t1] == t2,
            (
                FastVerifyVal::Payload {
                    ty: t1,
                    own: o1,
                    pool: p1,
                },
                FastVerifyVal::Payload {
                    ty: t2,
                    own: o2,
                    pool: p2,
                },
            ) if t1 == t2 => {
                let exact = match (o1, p1, o2, p2) {
                    (Some(a), _, Some(b), _) => a == b,
                    (_, Some(e1), _, Some(e2)) => self.des.hashes[e1] == self.obs.hashes[e2],
                    _ => false,
                };
                if exact {
                    true
                } else if name == crate::rbx::encode::COLLISION_HULL_PROPERTY {
                    let dc = match (o1, p1) {
                        (Some(s), _) => hull_fidelity_from_b64_prefix(s),
                        (_, Some(e)) => self.des.pool_hull_fidelity(e, &self.des_hulls)?,
                        _ => None,
                    };
                    let oc = match (o2, p2) {
                        (Some(s), _) => hull_fidelity_from_b64_prefix(s),
                        (_, Some(e)) => self.obs.pool_hull_fidelity(e, &self.obs_hulls)?,
                        _ => None,
                    };
                    dc.is_some() && dc == oc
                } else {
                    false
                }
            }
            (FastVerifyVal::Tags(ds, de), FastVerifyVal::Tags(os, oe)) => {
                scratch.tag_left_buf.clear();
                scratch
                    .tag_left_buf
                    .extend_from_slice(&scratch.des_tags[ds..de]);
                scratch.tag_left_buf.sort_unstable();
                scratch.tag_left_buf.dedup();
                scratch.tag_right_buf.clear();
                scratch
                    .tag_right_buf
                    .extend_from_slice(&scratch.obs_tags[os..oe]);
                scratch.tag_right_buf.sort_unstable();
                scratch.tag_right_buf.dedup();
                scratch.tag_left_buf == scratch.tag_right_buf
            }
            (
                FastVerifyVal::RawComplex {
                    kind: dk,
                    bytes: db,
                    ..
                },
                FastVerifyVal::RawComplex {
                    kind: ok_kind,
                    bytes: ob,
                    block: ok_blk,
                },
            ) => self.verify_raw_complex_pair(
                class_name,
                name,
                (dk, db),
                (ok_kind, ob, &obs_opens[ok_blk]),
                des_open,
            )?,
            _ => false,
        };
        Ok(matched)
    }

    fn verify_raw_complex_pair(
        &self,
        class_name: &str,
        name: &str,
        (dk, db): (u8, &[u8]),
        (ok_kind, ob, obs_open): (u8, &[u8], &OpenBlock),
        des_open: &OpenBlock,
    ) -> Result<bool> {
        use crate::settings::equivalence::{
            reconciliation_property_values_equal,
            reconciliation_value_contains_reference as holds_reference, value_contains_enum_item,
        };

        if dk == ok_kind
            && raw_complex_bytes_equal_no_refs(
                dk,
                db,
                ob,
                (&self.des, des_open),
                (&self.obs, obs_open),
            )?
        {
            return Ok(true);
        }
        let decode_one = |store: &BlockStore<'_>, open: &OpenBlock, kind: u8, bytes: &[u8]| {
            let marked: Vec<Cow<'_, str>> = open
                .strings
                .iter()
                .map(|s| match s {
                    BlockString::Own(t) => Cow::Borrowed(t.as_str()),
                    BlockString::Pool(e) => Cow::Owned(marker(&store.hashes[*e])),
                })
                .collect();
            let refs: Vec<&str> = marked.iter().map(Cow::as_ref).collect();
            let budget = store.budget.not_holding();
            let scope = ValueScope::from_borrowed(&refs, store.rows, &budget, None);
            let mut reader = BytecodeReader::new(bytes);
            let value = decode_raw_value_payload(&mut reader, kind, scope, 0)?;
            reader.finish()?;
            Ok::<_, anyhow::Error>(value)
        };
        let dv = decode_one(&self.des, des_open, dk, db)?;
        if holds_marker(&dv) || value_contains_enum_item(&dv) || holds_reference(&dv) {
            return Ok(false);
        }
        let ov = decode_one(&self.obs, obs_open, ok_kind, ob)?;
        if holds_marker(&ov) || value_contains_enum_item(&ov) || holds_reference(&ov) {
            return Ok(false);
        }
        Ok(reconciliation_property_values_equal(
            class_name,
            name,
            Some(&dv),
            Some(&ov),
        ))
    }
}

#[inline]
fn f32_finite_bits_eq(a: f32, b: f32) -> bool {
    a.is_finite() && a.to_bits() == b.to_bits()
}

#[inline]
fn f64_finite_bits_eq(a: f64, b: f64) -> bool {
    a.is_finite() && a.to_bits() == b.to_bits()
}

fn read_f32_lanes_exact_eq(
    ra: &mut BytecodeReader<'_>,
    rb: &mut BytecodeReader<'_>,
    lanes: usize,
) -> Result<bool> {
    for _ in 0..lanes {
        if !f32_finite_bits_eq(read_f32(ra)?, read_f32(rb)?) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn raw_complex_bytes_equal_no_refs(
    kind: u8,
    bytes_a: &[u8],
    bytes_b: &[u8],
    (store_a, open_a): (&BlockStore<'_>, &OpenBlock),
    (store_b, open_b): (&BlockStore<'_>, &OpenBlock),
) -> Result<bool> {
    let mut ra = BytecodeReader::new(bytes_a);
    let mut rb = BytecodeReader::new(bytes_b);
    if !raw_complex_values_equal_no_refs(
        kind,
        (&mut ra, store_a, open_a),
        (&mut rb, store_b, open_b),
        0,
    )? {
        return Ok(false);
    }
    ra.finish()?;
    rb.finish()?;
    Ok(true)
}

fn raw_complex_values_equal_no_refs(
    kind: u8,
    (ra, store_a, open_a): (&mut BytecodeReader<'_>, &BlockStore<'_>, &OpenBlock),
    (rb, store_b, open_b): (&mut BytecodeReader<'_>, &BlockStore<'_>, &OpenBlock),
    depth: usize,
) -> Result<bool> {
    if depth > 64 {
        return Ok(false);
    }
    let str_eq = |a: usize, b: usize| match (open_a.strings.get(a), open_b.strings.get(b)) {
        (Some(BlockString::Own(a)), Some(BlockString::Own(b))) => a == b,
        (Some(BlockString::Pool(a)), Some(BlockString::Pool(b))) => {
            store_a.hashes[*a] == store_b.hashes[*b]
        }
        _ => false,
    };
    match kind {
        0..=2 => Ok(true),
        3 | 4 => Ok(ra.read_var_u64()? == rb.read_var_u64()?),
        5 => Ok(f64_finite_bits_eq(read_f64(ra)?, read_f64(rb)?)),
        20 => Ok(f32_finite_bits_eq(read_f32(ra)?, read_f32(rb)?)),
        6 | 19 => Ok(str_eq(ra.read_len("string id")?, rb.read_len("string id")?)),
        NESTED_ENUM_ITEM_KIND => Ok(str_eq(
            ra.read_len("enum type string id")?,
            rb.read_len("enum type string id")?,
        ) && str_eq(
            ra.read_len("enum name string id")?,
            rb.read_len("enum name string id")?,
        )),
        11 | 13 => read_f32_lanes_exact_eq(ra, rb, 2),
        12 | 15 => read_f32_lanes_exact_eq(ra, rb, 3),
        14 | 18 => read_f32_lanes_exact_eq(ra, rb, 4),
        16 => read_f32_lanes_exact_eq(ra, rb, 12),
        9 | 21 => {
            let la = ra.read_len("numeric array length")?;
            if la != rb.read_len("numeric array length")? {
                return Ok(false);
            }
            if kind == 21 {
                return read_f32_lanes_exact_eq(ra, rb, la);
            }
            for _ in 0..la {
                if !f64_finite_bits_eq(read_f64(ra)?, read_f64(rb)?) {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        7 => {
            let la = ra.read_collection_len("array length")?;
            if la != rb.read_collection_len("array length")? {
                return Ok(false);
            }
            for _ in 0..la {
                let ka = ra.read_u8().context("Missing array item kind")?;
                if ka != rb.read_u8().context("Missing array item kind")?
                    || !raw_complex_values_equal_no_refs(
                        ka,
                        (ra, store_a, open_a),
                        (rb, store_b, open_b),
                        depth + 1,
                    )?
                {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        8 => {
            let la = ra.read_collection_len("object length")?;
            if la != rb.read_collection_len("object length")? {
                return Ok(false);
            }
            for _ in 0..la {
                let (ka, kb) = (
                    ra.read_len("object key string id")?,
                    rb.read_len("object key string id")?,
                );
                let Some(BlockString::Own(key)) = open_a.strings.get(ka).filter(|_| str_eq(ka, kb))
                else {
                    return Ok(false);
                };
                if matches!(
                    key.as_str(),
                    "instanceIndex"
                        | "settingsId"
                        | "instanceId"
                        | "debugId"
                        | "path"
                        | "pathSegments"
                        | "pathOrdinals"
                        | "ref"
                        | "enumType"
                        | "enumName"
                ) {
                    return Ok(false);
                }
                let va = ra.read_u8().context("Missing object value kind")?;
                if va != rb.read_u8().context("Missing object value kind")? {
                    return Ok(false);
                }
                if matches!(key.as_str(), "_type" | "type") {
                    if va != 6 {
                        return Ok(false);
                    }
                    let (sa, sb) = (ra.read_len("string id")?, rb.read_len("string id")?);
                    let Some(BlockString::Own(ty)) =
                        open_a.strings.get(sa).filter(|_| str_eq(sa, sb))
                    else {
                        return Ok(false);
                    };
                    if matches!(ty.as_str(), "InstanceRef" | "Ref" | "Instance" | "EnumItem") {
                        return Ok(false);
                    }
                    continue;
                }
                if !raw_complex_values_equal_no_refs(
                    va,
                    (ra, store_a, open_a),
                    (rb, store_b, open_b),
                    depth + 1,
                )? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

struct VerifyBlockScratch<'a> {
    des_vals: Vec<Option<FastVerifyVal<'a>>>,
    obs_vals: Vec<Option<FastVerifyVal<'a>>>,
    des_cframes: Vec<[f64; 12]>,
    obs_cframes: Vec<[f64; 12]>,
    des_tags: Vec<&'a str>,
    obs_tags: Vec<&'a str>,
    tag_left_buf: Vec<&'a str>,
    tag_right_buf: Vec<&'a str>,
    touched: Vec<usize>,
    touched_mark: Vec<u32>,
    mark_id: u32,
    class_engine: AHashMap<&'a str, bool>,
    class_asset: AHashMap<&'a str, bool>,
}

impl<'a> VerifyBlockScratch<'a> {
    fn new(count: usize) -> Self {
        Self {
            des_vals: vec![None; count],
            obs_vals: vec![None; count],
            des_cframes: Vec::new(),
            obs_cframes: Vec::new(),
            des_tags: Vec::new(),
            obs_tags: Vec::new(),
            tag_left_buf: Vec::new(),
            tag_right_buf: Vec::new(),
            touched: Vec::with_capacity(count),
            touched_mark: vec![0; count],
            mark_id: 0,
            class_engine: AHashMap::new(),
            class_asset: AHashMap::new(),
        }
    }

    fn next_property(&mut self) {
        self.mark_id = self.mark_id.wrapping_add(1);
        if self.mark_id == 0 {
            self.touched_mark.fill(0);
            self.mark_id = 1;
        }
        self.touched.clear();
        self.des_cframes.clear();
        self.obs_cframes.clear();
        self.des_tags.clear();
        self.obs_tags.clear();
    }
}

fn decode_verify_attrs_groups(
    store: &BlockStore<'_>,
    open: &OpenBlock,
    specs: &[BlockGroupSpec<'_>],
    attr_groups: &[usize],
    (row_count, row_map): (usize, Option<&[u32]>),
    ok: &mut [bool],
    out_attrs: &mut [Option<Map<String, Value>>],
) -> Result<()> {
    use crate::settings::equivalence::{
        reconciliation_value_contains_reference as holds_reference, value_contains_enum_item,
    };
    let marked_strings: Vec<Cow<'_, str>> = open
        .strings
        .iter()
        .map(|s| match s {
            BlockString::Own(t) => Cow::Borrowed(t.as_str()),
            BlockString::Pool(e) => Cow::Owned(marker(&store.hashes[*e])),
        })
        .collect();
    let refs: Vec<&str> = marked_strings.iter().map(Cow::as_ref).collect();
    let scope = ValueScope::from_borrowed(&refs, store.rows, &store.budget, None);
    for &gidx in attr_groups {
        let (_, kind, vc, body) = specs[gidx];
        let mut r = BytecodeReader::new(body);
        let _ = decode_cframe_group_header(kind, &mut r)?;
        let mut prev_row = 0_usize;
        for pos in 0..vc {
            let raw_row =
                next_group_instance(prev_row, r.read_len("property instance index")?, pos)?;
            prev_row = raw_row;
            anyhow::ensure!(raw_row < row_count, "Row out of block bounds");
            let mapped = match row_map {
                Some(map) => {
                    let target = map[raw_row];
                    (target != u32::MAX).then_some(target as usize)
                }
                None => Some(raw_row),
            };
            let Some(row) = mapped.filter(|&row| ok[row]) else {
                skip_raw_value_payload_fast(&mut r, 8, 0)?;
                continue;
            };
            if out_attrs[row].is_some() {
                ok[row] = false;
                skip_raw_value_payload_fast(&mut r, 8, 0)?;
                continue;
            }
            let attrs = decode_attributes_payload(&mut r, scope)?;
            if attrs.keys().any(|k| marked(k).is_some())
                || attrs
                    .values()
                    .any(|v| holds_marker(v) || holds_reference(v) || value_contains_enum_item(v))
            {
                ok[row] = false;
            } else {
                out_attrs[row] = Some(attrs);
            }
        }
        r.finish()?;
    }
    Ok(())
}

pub(crate) fn fast_verify_aligned_v12_rows(
    input: FastVerifyInput<'_>,
) -> Result<Option<Vec<bool>>> {
    use crate::settings::equivalence::CanonicalizeCache;

    let _span = crate::app::profile::span("fast verify aligned v12 rows");
    if !is_block_store(input.desired_bytes) || !is_block_store(input.observed_bytes) {
        return Ok(None);
    }
    let des = BlockStore::open(input.desired_bytes)?;
    let obs = BlockStore::open(input.observed_bytes)?;
    if des.rows != obs.rows
        || des.rows != input.desired_instances.len()
        || des.rows != input.observed_instances.len()
        || des.rows != input.places.len()
    {
        return Ok(None);
    }

    let obs_opened = (0..obs.blocks.len()).map(|_| Mutex::new(None)).collect();
    let obs_remaining: Vec<AtomicUsize> =
        (0..obs.blocks.len()).map(|_| AtomicUsize::new(0)).collect();
    let stores = FastVerifyStores {
        input: &input,
        des,
        obs,
        database: rbx_reflection_database::get().ok(),
        des_hulls: ShardedMap::new(),
        obs_hulls: ShardedMap::new(),
        obs_opened,
        obs_remaining,
    };
    let plans: Vec<(Vec<bool>, Vec<usize>)> = stores
        .des
        .blocks
        .iter()
        .map(|block| stores.eligible_rows_and_obs_blocks(block.first, block.count))
        .collect();
    for (_, used) in &plans {
        if stores.obs_blocks_within_budget(used) {
            for &ob in used {
                stores.obs_remaining[ob].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let block_results = plans
        .into_par_iter()
        .enumerate()
        .map_init(CanonicalizeCache::new, |cache, (block_idx, (ok, used))| {
            stores.verify_block(cache, block_idx, ok, &used)
        })
        .collect::<Result<Vec<_>>>()?;

    let mut verified = Vec::with_capacity(stores.des.rows);
    for block_ok in block_results {
        verified.extend(block_ok);
    }
    Ok(Some(verified))
}

/// Digest of the engine-state properties (skipped by `document_row_digest`) of an instance.
fn engine_state_digest(
    database: Option<&rbx_reflection::ReflectionDatabase<'_>>,
    instance: &SettingsBytecodeInstance,
) -> [u8; 16] {
    use super::captured::{RowHasher, UNIQUE_ID, is_digested_with_db, value_digest};
    use crate::settings::equivalence::digest_text;
    let mut eh: Option<RowHasher> = None;
    for (name, value) in &instance.properties {
        if name.as_str() != UNIQUE_ID && !is_digested_with_db(database, &instance.class_name, name)
        {
            let h = eh.get_or_insert_with(RowHasher::new);
            h.update(&[1]);
            digest_text(h, name);
            h.update(&value_digest(value));
        }
    }
    eh.map_or([0; 16], RowHasher::finish)
}

/// A v12 store that an edit reads and writes for `RetainedPropertyColumns`:
/// the instances it makes complete come from their blocks, and only the
/// blocks the edit changes are written anew.
impl BlockStore<'static> {
    pub(crate) fn settle_ids_and_rows(
        self,
        records: Vec<SettingsBytecodeInstance>,
        ids: &[String],
        written: &BTreeMap<usize, SettingsBytecodeInstance>,
    ) -> Result<Vec<u8>> {
        anyhow::ensure!(
            records.len() == ids.len(),
            "A captured store changed while it was verified; retry the sync"
        );
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: records.clone(),
        };
        for (instance, id) in document.instances.iter_mut().zip(ids) {
            instance.settings_id.clone_from(id);
        }
        let mut complete = vec![false; records.len()];
        for (index, verified) in written {
            let instance = &mut document.instances[*index];
            instance.properties.clone_from(&verified.properties);
            instance.attributes.clone_from(&verified.attributes);
            complete[*index] = true;
        }
        let edited = EditedStore {
            store: self,
            records,
            given: AHashMap::new(),
            complete,
            standing: true,
        };
        let encoded = edited.encode(&document);
        rayon::spawn(move || drop((document, edited)));
        encoded
    }
}

pub(crate) struct EditedStore {
    store: BlockStore<'static>,
    /// The instances as the store holds them, without values.
    records: Vec<SettingsBytecodeInstance>,
    /// The values the document is given beside its records, by row: the
    /// viewport camera its root names, which the camera checks read on any
    /// row, and each property `give` read.
    given: AHashMap<usize, Map<String, Value>>,
    complete: Vec<bool>,
    /// `Rewrite::standing` of the files it writes: an edit leaves the
    /// instances it does not touch as they are.
    standing: bool,
}

impl EditedStore {
    /// This store writing, where rows moved, every block that holds rows
    /// from its rows, as a merge does to give the whole encode of what it
    /// merged.
    pub(crate) fn rewriting_moved_blocks(mut self) -> Self {
        self.standing = false;
        self
    }

    /// The structure of a v12 file, with the root's viewport camera.
    pub(crate) fn decode(bytes: &[u8]) -> Result<(SettingsBytecode, Self)> {
        let _span = crate::app::profile::span("store structure read");
        let store = BlockStore::open(bytes.to_vec())?;
        let mut document = store.structure()?;
        let records = document.instances.clone();
        let root = records.iter().position(|row| row.parent_index.is_none());
        let mut edited = Self {
            store,
            complete: vec![false; records.len()],
            records,
            given: AHashMap::new(),
            standing: true,
        };
        edited.give(&mut document, root.as_slice(), "CurrentCamera")?;
        Ok((document, edited))
    }

    /// Gives the rows at `rows` their stored value of the property `name`
    /// beside their records, for an edit that reads it and leaves it alone.
    pub(crate) fn give(
        &mut self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        name: &str,
    ) -> Result<()> {
        let _span = crate::app::profile::span("store property read");
        self.in_place(document)?;
        let (given, complete) = (&mut self.given, &self.complete);
        let rows = (rows.iter().copied())
            .filter(|row| complete.get(*row) != Some(&true))
            .collect::<Vec<_>>();
        let named = |held: &str| held == name;
        self.store
            .read(&rows, READ_AHEAD_BLOCKS, named, |row, values| {
                let held = &mut document.instances[row].properties;
                held.extend(values.properties.clone());
                given.entry(row).or_default().extend(values.properties);
                Ok(true)
            })
    }

    /// Gives the instances at `indices` their stored values whole. Each call
    /// is a pass on a budget of its own, as `encode` is: a pass reads each
    /// block it needs once, and so takes no more than a whole decode. A
    /// document may hold new instances behind the store's rows.
    pub(crate) fn materialize(
        &mut self,
        document: &mut SettingsBytecode,
        indices: &[usize],
    ) -> Result<()> {
        let _span = crate::app::profile::span("store rows read");
        anyhow::ensure!(
            document.instances.len() >= self.records.len(),
            "An edit of values changed how many instances the store holds"
        );
        self.store.budget.renew(self.store.bytes.len());
        for (row, values) in self.store.rows(indices)? {
            document.instances[row].properties = values.properties;
            document.instances[row].attributes = values.attributes;
            self.complete[row] = true;
        }
        Ok(())
    }

    /// `materialize` of instances that may stand in every block, the
    /// instances of a few blocks at a time.
    pub(crate) fn materialize_in_passes(
        &mut self,
        document: &mut SettingsBytecode,
        indices: &[usize],
    ) -> Result<()> {
        let rows = self.store.chosen(indices)?;
        let pass = |row: &usize| self.store.block_of(*row) / READ_AHEAD_BLOCKS;
        let passes = (rows.chunk_by(|left, right| pass(left) == pass(right)))
            .map(<[usize]>::to_vec)
            .collect::<Vec<_>>();
        (passes.iter()).try_for_each(|rows| self.materialize(document, rows))
    }

    /// `materialize` of each instance that holds a value `wanted` takes,
    /// which sees only the values that may hold a reference (arrays, objects
    /// and references) and a marker in place of each pool entry: of the
    /// blocks that hold one of `strings`, or of every block where there are
    /// none.
    pub(crate) fn materialize_where(
        &mut self,
        document: &mut SettingsBytecode,
        strings: Option<&AHashSet<&str>>,
        wanted: impl Fn(&Value) -> bool + Sync,
    ) -> Result<()> {
        let _span = crate::app::profile::span("store rows found by values");
        let rows = self.store.rows_where(strings, |row| {
            let mut values = row.properties.values().chain(row.attributes.values());
            values.any(&wanted)
        })?;
        let rows = (rows.into_iter())
            .filter(|row| !self.complete[*row])
            .collect::<Vec<_>>();
        self.materialize(document, &rows)
    }

    /// Refuses a read into a document whose rows are no more the store's.
    fn in_place(&self, document: &SettingsBytecode) -> Result<()> {
        anyhow::ensure!(
            document.instances.len() == self.records.len(),
            "An edit of values changed how many instances the store holds"
        );
        Ok(())
    }

    /// The store with `document`'s records, and the document's values in
    /// the instances made complete. Any other instance keeps its stored
    /// values, and a document that changed one is refused. A record may take
    /// another id, name, class or parent; a document whose rows moved, which
    /// would give the stored values of one instance to another, is refused.
    /// A file another writer wrote is written whole, since none of its frames
    /// may be copied.
    pub(crate) fn encode(&self, document: &SettingsBytecode) -> Result<Vec<u8>> {
        self.in_place(document)?;
        let from = (0..self.records.len()).map(Some).collect::<Vec<_>>();
        self.written(document, &from, true)
    }

    /// `encode` of a document whose instances an edit added to, removed or
    /// put in another order: each instance is the row of the store with its
    /// id, wherever it stands now, and one with an id the store lacks is new
    /// and holds its values. A reference by position in the values an
    /// instance keeps follows its instance, and names nothing once that is
    /// gone.
    pub(crate) fn encode_restructured(&self, document: &SettingsBytecode) -> Result<Vec<u8>> {
        let placing = crate::app::profile::span("store rows placed");
        let rows = (self.records.iter().enumerate())
            .map(|(row, record)| (record.settings_id.as_str(), row))
            .collect::<AHashMap<_, _>>();
        let from = (document.instances.par_iter())
            .map(|instance| rows.get(instance.settings_id.as_str()).copied())
            .collect::<Vec<_>>();
        drop((placing, rows));
        self.written(document, &from, false)
    }

    /// The store of `document`, whose instance at each place is the row
    /// `from` gives there.
    fn written(
        &self,
        document: &SettingsBytecode,
        from: &[Option<usize>],
        in_place: bool,
    ) -> Result<Vec<u8>> {
        let _span = crate::app::profile::span("store blocks write");
        let mut to = vec![None; self.records.len()];
        for (now, row) in from.iter().enumerate() {
            if let Some(row) = row {
                anyhow::ensure!(
                    to[*row].replace(now).is_none(),
                    "Two instances of an edited store share an id"
                );
            }
        }
        let held = (from.iter())
            .map(|row| row.is_none_or(|row| self.complete[row]))
            .collect::<Vec<_>>();
        for (now, instance) in document.instances.iter().enumerate() {
            let Some(row) = from[now].filter(|_| !held[now]) else {
                continue;
            };
            let mut given = self.given.get(&row).cloned().unwrap_or_default();
            for value in given.values_mut() {
                remap_ref_indices_in_value(value, &to);
            }
            if !instance.attributes.is_empty() || instance.properties != given {
                bail!("An edit changed the values of instance {now} without reading them whole");
            }
        }
        let placed = Placed {
            from,
            to: &to,
            held: &held,
        };
        let moving = crate::app::profile::span("store moved strings");
        let moved = self.moved(document, placed, in_place)?;
        drop(moving);
        self.store.budget.renew(self.store.bytes.len());
        if self.store.writer != writer() {
            return self.whole(document, placed);
        }
        self.store
            .rewrite(document, placed, &moved, (false, self.standing))
    }

    /// The strings by which a reference may name an instance that the
    /// records of `document` give or take: the id and name of each new
    /// instance, each id a record takes, and the names a record takes and
    /// leaves with another name, class or parent.
    /// A document edited in place in which an instance took another id
    /// beside another name or class is refused, as one whose rows moved; one
    /// that took another id under its name and class, even that of another
    /// row, keeps the values of its row, as v11 kept them.
    fn moved<'d>(
        &'d self,
        document: &'d SettingsBytecode,
        placed: Placed<'_>,
        in_place: bool,
    ) -> Result<Vec<&'d str>> {
        let mut moved = Vec::new();
        for (row, now) in document.instances.iter().enumerate() {
            let Some(stored) = placed.from[row].map(|row| &self.records[row]) else {
                moved.extend([now.settings_id.as_str(), now.name.as_str()]);
                continue;
            };
            let parent = stored.parent_index.map(|parent| placed.to[parent]);
            let same_id = now.settings_id == stored.settings_id;
            let same_kind = now.name == stored.name && now.class_name == stored.class_name;
            anyhow::ensure!(
                !in_place || same_id || same_kind,
                "An edit of values moved instance {row}; a store edited in place keeps every instance at its row"
            );
            if !same_id {
                moved.push(now.settings_id.as_str());
            }
            if !same_kind || now.parent_index.map(Some) != parent {
                moved.extend([now.name.as_str(), stored.name.as_str()]);
            }
        }
        Ok(moved)
    }

    /// The store of `document` written whole, each instance not made
    /// complete with its stored values.
    fn whole(&self, document: &SettingsBytecode, placed: Placed<'_>) -> Result<Vec<u8>> {
        let mut stored = self.store.decode()?;
        let mut whole = document.clone();
        for (now, instance) in whole.instances.iter_mut().enumerate() {
            let Some(row) = placed.from[now].filter(|_| !placed.held[now]) else {
                continue;
            };
            instance.properties = std::mem::take(&mut stored.instances[row].properties);
            instance.attributes = std::mem::take(&mut stored.instances[row].attributes);
            let values = instance.properties.values_mut();
            for value in values.chain(instance.attributes.values_mut()) {
                remap_ref_indices_in_value(value, placed.to);
            }
        }
        encode_document(&whole, true, None)
    }
}

/// The blocks a read of consecutive rows decodes at once, in parallel.
const READ_AHEAD_BLOCKS: usize = 4;

/// A v12 store read for `RetainedRowColumns`, each block decoded whole when
/// the first of its rows is read, with the next few beside it.
pub(crate) struct RowBlocks {
    store: BlockStore<'static>,
    /// The blocks decoded and not passed yet, with the values of their rows
    /// not given yet.
    read: VecDeque<(usize, Vec<DecodedRowValues>)>,
    /// The values `references` read of the rows it was asked for and did not
    /// give them, which `select` gives.
    held_back: Vec<(usize, DecodedRowValues)>,
}

impl RowBlocks {
    pub(crate) fn decode(bytes: &[u8]) -> Result<(SettingsBytecode, Self)> {
        let blocks = Self::open(bytes)?;
        Ok((blocks.store.structure()?, blocks))
    }

    /// The blocks alone, for a reader that holds the store's structure.
    pub(crate) fn open(bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            store: BlockStore::open(bytes.to_vec())?.holding(),
            read: VecDeque::new(),
            held_back: Vec::new(),
        })
    }

    pub(crate) fn rows(&self) -> usize {
        self.store.rows
    }

    pub(crate) fn store(&self) -> &BlockStore<'static> {
        &self.store
    }

    pub(crate) fn into_store(self) -> BlockStore<'static> {
        self.store
    }

    /// `RetainedRowColumns::materialize_references` of this store: the rows
    /// `wanted` marks get the values of each property some block holds a
    /// value of one of `kinds` for, and their attributes where some block
    /// holds attributes, as the v11 columns give them; their other values
    /// wait for `select`. A reader of its own reads every block for the heads
    /// of its groups, and the blocks of those rows whole with the pool frames
    /// they name.
    pub(crate) fn references(
        &mut self,
        document: &mut SettingsBytecode,
        wanted: &[bool],
        kinds: &[u8],
    ) -> Result<()> {
        let store = BlockStore::open(&*self.store.bytes)?.holding();
        let headed = (0..store.blocks.len())
            .into_par_iter()
            .map(|index| {
                let open = store.open_block(index)?;
                let heads = store.group_heads(&open)?.into_iter();
                let names = heads
                    .filter(|(_, kind)| kinds.contains(kind))
                    .map(|(name, _)| name);
                let names = names.collect::<Vec<_>>();
                let read = wanted[store.blocks[index].rows()].contains(&true);
                Ok((names, read.then_some((index, open))))
            })
            .collect::<Result<Vec<_>>>()?;
        let (mut names, mut opened) = (AHashSet::new(), Vec::new());
        for (block_names, open) in headed {
            names.extend(block_names);
            opened.extend(open);
        }
        let read = opened
            .into_par_iter()
            .map(|(index, open)| {
                let first = store.blocks[index].first;
                let mut values = (0..store.blocks[index].count)
                    .map(|_| DecodedRowValues::default())
                    .collect::<Vec<_>>();
                let keep = |row| wanted[first + row];
                store.block_into(&open, &mut values, keep, Texts::Resolved(&|_| true))?;
                Ok((first, values))
            })
            .collect::<Result<Vec<_>>>()?;
        let attributes = names.contains("Attributes");
        for (first, values) in read {
            for (row, values) in (first..).zip(values).filter(|(row, _)| wanted[*row]) {
                let (now, later) = (values.properties.into_iter())
                    .partition(|(name, _)| names.contains(name.as_str()));
                let (given, held) = match attributes {
                    true => (values.attributes, Map::new()),
                    false => (Map::new(), values.attributes),
                };
                let now = DecodedRowValues {
                    properties: now,
                    attributes: given,
                };
                give(&mut document.instances[row], now);
                let later = DecodedRowValues {
                    properties: later,
                    attributes: held,
                };
                self.held_back.push((row, later));
            }
        }
        Ok(())
    }

    /// Gives `instances`, the rows at `rows`, their values.
    pub(super) fn fill<T: DecodedPropertyTarget>(
        &mut self,
        instances: &mut [T],
        rows: Range<usize>,
    ) -> Result<()> {
        for (instance, row) in instances.iter_mut().zip(rows) {
            let block = self.store.block_of(row);
            while self.read.front().is_some_and(|(at, _)| *at < block) {
                self.read.pop_front();
            }
            if self.read.is_empty() {
                let next = block..self.store.blocks.len().min(block + READ_AHEAD_BLOCKS);
                self.read = next
                    .into_par_iter()
                    .map(|at| {
                        let values = self.store.block_values(at, |_| true, Texts::All)?;
                        Ok((at, values))
                    })
                    .collect::<Result<_>>()?;
            }
            let (_, values) = self.read.front_mut().context("No block was read")?;
            give(
                instance,
                std::mem::take(&mut values[row - self.store.blocks[block].first]),
            );
        }
        Ok(())
    }

    /// `RetainedRowColumns::project_properties` of this store, read on a
    /// budget of its own as the v11 columns are. Only the named properties
    /// are decoded, and of the pool only the entries their values name.
    pub(crate) fn project(
        &self,
        document: &mut SettingsBytecode,
        names: impl Fn(&str) -> bool + Sync,
        project: impl Fn(&SettingsBytecodeInstance, &str, Value) -> Option<Value>,
    ) -> Result<()> {
        let store = &self.store;
        store.budget.renew(store.bytes.len());
        anyhow::ensure!(
            document.instances.len() == store.rows,
            "Store structure changed during row decoding; restart the store read"
        );
        let indices: Vec<usize> = (0..store.blocks.len()).collect();
        for batch in indices.chunks(STANDING_BATCH) {
            let decoded = batch
                .par_iter()
                .map(|&index| -> Result<(usize, Vec<DecodedRowValues>)> {
                    let block = &store.blocks[index];
                    let open = store.open_block(index)?;
                    let mut rows: Vec<DecodedRowValues> = (0..block.count)
                        .map(|_| DecodedRowValues::default())
                        .collect();
                    store.values(
                        &open,
                        &mut rows,
                        |_| true,
                        |name, _| names(name),
                        Texts::Resolved(&|_| true),
                    )?;
                    Ok((block.first, rows))
                })
                .collect::<Result<Vec<_>>>()?;
            for (first, rows) in decoded {
                for (offset, row_values) in rows.into_iter().enumerate() {
                    let row = first + offset;
                    for (name, value) in row_values.properties {
                        match project(&document.instances[row], &name, value) {
                            Some(value) => document.instances[row].properties.insert(name, value),
                            None => document.instances[row].properties.remove(&name),
                        };
                    }
                }
            }
        }
        store.budget.renew(store.bytes.len());
        Ok(())
    }

    /// Gives the rows `wanted` marks their values, from their blocks alone,
    /// read in parallel; `after_references` of the same rows, the values
    /// `references` held back.
    pub(crate) fn select(
        &mut self,
        document: &mut SettingsBytecode,
        wanted: &[bool],
        after_references: bool,
    ) -> Result<()> {
        let held_back = std::mem::take(&mut self.held_back);
        if after_references {
            for (row, values) in held_back {
                give(&mut document.instances[row], values);
            }
            return Ok(());
        }
        drop(held_back);
        let read = self
            .store
            .blocks
            .par_iter()
            .enumerate()
            .filter(|(_, block)| wanted[block.rows()].contains(&true))
            .map(|(index, block)| {
                let first = block.first;
                let keep = |row| wanted[first + row];
                let values = self
                    .store
                    .block_values(index, keep, Texts::Resolved(&|_| true))?;
                Ok((first, values))
            })
            .collect::<Result<Vec<_>>>()?;
        for (first, values) in read {
            for (row, values) in (first..).zip(values) {
                if wanted[row] {
                    give(&mut document.instances[row], values);
                }
            }
        }
        Ok(())
    }
}

/// A row's values given as the v11 columns give them: each property put in,
/// and the attributes in place of any there were.
fn give<T: DecodedPropertyTarget>(instance: &mut T, values: DecodedRowValues) {
    let (properties, attributes) = instance.maps();
    properties.extend(values.properties);
    if !values.attributes.is_empty() {
        *attributes = values.attributes;
    }
}

/// A v12 store a read command reads by its rows: its document holds every
/// instance without its values, and a row is given them, as the whole decode
/// gives them, only when asked for.
pub(crate) struct RowReader {
    store: BlockStore<'static>,
    /// The rows `fill` gave their values.
    filled: Vec<bool>,
}

impl RowReader {
    /// The instances of a v12 file without their values, and its reader.
    pub(crate) fn decode(bytes: &[u8]) -> Result<(SettingsBytecode, Self)> {
        let store = BlockStore::open(bytes.to_vec())?;
        let document = store.structure()?;
        let filled = vec![false; document.instances.len()];
        Ok((document, Self { store, filled }))
    }

    /// Gives the rows at `rows` their stored values, a few blocks at a time.
    pub(crate) fn fill(&mut self, document: &mut SettingsBytecode, rows: &[usize]) -> Result<()> {
        let rows = (rows.iter().copied())
            .filter(|row| self.filled.get(*row) != Some(&true))
            .collect::<Vec<_>>();
        let filled = &mut self.filled;
        self.store.read(
            &rows,
            READ_AHEAD_BLOCKS,
            |_| true,
            |row, values| {
                let instance = &mut document.instances[row];
                instance.properties = values.properties;
                instance.attributes = values.attributes;
                filled[row] = true;
                Ok(true)
            },
        )
    }

    /// Gives the rows at `rows` their stored value of the property `name`
    /// beside the values they hold.
    pub(crate) fn give(
        &self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        name: &str,
    ) -> Result<()> {
        self.store.read(
            rows,
            READ_AHEAD_BLOCKS,
            |held| held == name,
            |row, values| {
                document.instances[row].properties.extend(values.properties);
                Ok(true)
            },
        )
    }

    /// Calls `visit` on the rows at `rows` in rising order until it gives
    /// false, each row holding its stored values of the properties `names`
    /// takes while it is visited. One block is read at a time, and the
    /// document holds after what it held before.
    pub(crate) fn walk(
        &self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        names: impl Fn(&str) -> bool + Sync,
        mut visit: impl FnMut(&SettingsBytecode, usize) -> bool,
    ) -> Result<()> {
        self.store.read(rows, 1, names, |row, values| {
            let instance = &mut document.instances[row];
            let properties = std::mem::replace(&mut instance.properties, values.properties);
            let attributes = std::mem::replace(&mut instance.attributes, values.attributes);
            let more = visit(document, row);
            let instance = &mut document.instances[row];
            instance.properties = properties;
            instance.attributes = attributes;
            Ok(more)
        })
    }

    /// Refuses a store one of whose frames does not match its digest, as its
    /// whole decode does, before a row of it is read.
    pub(crate) fn check(&self) -> Result<()> {
        let store = &self.store;
        let frames = (store.pool.iter().map(|pool| pool.frame))
            .chain(store.blocks.iter().map(|block| block.frame))
            .collect::<Vec<_>>();
        (frames.par_iter()).try_for_each(|frame| store.frame_bytes(*frame).map(drop))
    }

    /// Calls `visit` on `rows` in the order given, `at_once` of them at a
    /// time, each row holding its stored values whole while its turn is
    /// visited. The blocks of a turn are read together, on a budget of their
    /// own, and the document holds after what it held before.
    pub(crate) fn walk_turns(
        &self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        at_once: usize,
        mut visit: impl FnMut(&SettingsBytecode, &[usize]) -> Result<()>,
    ) -> Result<()> {
        for turn in rows.chunks(at_once.max(1)) {
            self.store.budget.renew(self.store.bytes.len());
            let mut held = Vec::with_capacity(turn.len());
            for (row, values) in self.store.rows(turn)? {
                let instance = &mut document.instances[row];
                held.push((
                    row,
                    std::mem::replace(&mut instance.properties, values.properties),
                    std::mem::replace(&mut instance.attributes, values.attributes),
                ));
            }
            let visited = visit(document, turn);
            for (row, properties, attributes) in held {
                document.instances[row].properties = properties;
                document.instances[row].attributes = attributes;
            }
            visited?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod fast_path_regression_tests {
    use super::*;
    use crate::settings::equivalence::SettingsAlignment;
    use crate::snapshot::import::build_service_state_from_instances;
    use crate::snapshot::types::SnapshotInstance;

    fn snapshot_instance(
        id: &str,
        class_name: &str,
        name: &str,
        instance_index: usize,
        parent_index: Option<usize>,
        properties: Map<String, Value>,
    ) -> SnapshotInstance {
        let (path, path_segments, parent_path) = if parent_index.is_some() {
            (
                format!("Workspace.{name}"),
                vec!["Workspace".to_string(), name.to_string()],
                Some("Workspace".to_string()),
            )
        } else {
            (name.to_string(), vec![name.to_string()], None)
        };
        SnapshotInstance {
            path,
            path_segments,
            name: name.to_string(),
            class_name: class_name.into(),
            properties,
            parent_path,
            debug_id: Some(format!("debug:{instance_index}")),
            transported_settings_id: Some(id.to_string()),
            instance_index: Some(instance_index + 1),
            parent_index: parent_index.map(|parent| parent + 1),
            ..Default::default()
        }
    }

    fn doc_instance(
        id: &str,
        class_name: &str,
        name: &str,
        parent_index: Option<usize>,
        properties: Map<String, Value>,
    ) -> SettingsBytecodeInstance {
        let mut instance = SettingsBytecodeInstance::new(
            id.to_string(),
            name.to_string(),
            class_name.to_string(),
            parent_index,
        );
        instance.properties = properties;
        instance
    }

    #[test]
    fn fast_compare_and_verify_distinguish_large_int64_values() -> Result<()> {
        let uid = json!({"_type": "UniqueId", "value": "00000000000000000000000000000001"});
        let stored_value: i64 = 9_007_199_254_740_992;
        let changed_value: i64 = 9_007_199_254_740_993;

        let stored_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                doc_instance("1", "Workspace", "Workspace", None, Map::new()),
                doc_instance(
                    "iv",
                    "IntValue",
                    "Big",
                    Some(0),
                    Map::from_iter([
                        ("UniqueId".into(), uid.clone()),
                        ("Value".into(), json!(stored_value)),
                    ]),
                ),
            ],
        };
        let changed_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                doc_instance("1", "Workspace", "Workspace", None, Map::new()),
                doc_instance(
                    "iv",
                    "IntValue",
                    "Big",
                    Some(0),
                    Map::from_iter([
                        ("UniqueId".into(), uid.clone()),
                        ("Value".into(), json!(changed_value)),
                    ]),
                ),
            ],
        };
        let stored_bytes = encode_document(&stored_doc, false, None)?;
        let changed_bytes = encode_document(&changed_doc, false, None)?;

        let mut state = build_service_state_from_instances(
            "Workspace",
            None,
            vec![
                snapshot_instance("ws", "Workspace", "Workspace", 0, None, Map::new()),
                snapshot_instance(
                    "iv",
                    "IntValue",
                    "Big",
                    1,
                    Some(0),
                    Map::from_iter([
                        ("UniqueId".into(), uid),
                        ("Value".into(), json!(changed_value)),
                    ]),
                ),
            ],
            HashMap::new(),
            true,
        )?;
        state.dense_index_topology = true;
        let pulled = super::super::pulled_digest_from_service_state(&state)?;
        let (alignment, _) = super::super::captured::changed_document_rows(
            &state,
            &pulled.rows,
            &stored_bytes,
            None,
        )?
        .expect("changed_document_rows should compare single changed IntValue row");
        assert!(
            matches!(alignment, SettingsAlignment::Changed(_)),
            "fast compare dropped an Int64 change above 2^53"
        );

        let verified = fast_verify_aligned_v12_rows(FastVerifyInput {
            desired_bytes: &stored_bytes,
            observed_bytes: &changed_bytes,
            desired_instances: &stored_doc.instances,
            observed_instances: &changed_doc.instances,
            places: &[0, 1],
            by_order: &[false, false],
            skip: &[true, false],
            has_before: true,
        })?
        .expect("fast verify should handle aligned v12 stores");
        assert!(
            !verified[1],
            "fast verify accepted a different Int64 above 2^53"
        );
        Ok(())
    }

    #[test]
    fn fast_compare_detects_changed_inline_source_property() -> Result<()> {
        let uid = json!({"_type": "UniqueId", "value": "00000000000000000000000000000002"});
        let stored_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                doc_instance("1", "Workspace", "Workspace", None, Map::new()),
                doc_instance(
                    "sc",
                    "Script",
                    "Main",
                    Some(0),
                    Map::from_iter([
                        ("UniqueId".into(), uid.clone()),
                        ("Source".into(), json!("return 1")),
                    ]),
                ),
            ],
        };
        let stored_bytes = encode_document(&stored_doc, false, None)?;

        for captured_source in [
            json!("return 2"),
            json!(crate::settings::EXTERNAL_SOURCE_MARKER),
        ] {
            let mut state = build_service_state_from_instances(
                "Workspace",
                None,
                vec![
                    snapshot_instance("ws", "Workspace", "Workspace", 0, None, Map::new()),
                    snapshot_instance(
                        "sc",
                        "Script",
                        "Main",
                        1,
                        Some(0),
                        Map::from_iter([
                            ("UniqueId".into(), uid.clone()),
                            ("Source".into(), captured_source.clone()),
                        ]),
                    ),
                ],
                HashMap::new(),
                true,
            )?;
            state.dense_index_topology = true;
            let pulled = super::super::pulled_digest_from_service_state(&state)?;
            let (alignment, _) = super::super::captured::changed_document_rows(
                &state,
                &pulled.rows,
                &stored_bytes,
                None,
            )?
            .expect("changed_document_rows should compare Script row");
            assert!(
                matches!(alignment, SettingsAlignment::Changed(_)),
                "fast compare treated inline Source as metadata for captured={captured_source}"
            );
        }
        Ok(())
    }

    #[test]
    fn fast_compare_honours_one_sided_rules_for_inline_object_values() -> Result<()> {
        let uid = json!({"_type": "UniqueId", "value": "00000000000000000000000000000009"});
        let obj_val = json!({"_type": "NumberRange", "min": 1.0, "max": 2.0});

        let stored_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                doc_instance("1", "Workspace", "Workspace", None, Map::new()),
                doc_instance(
                    "p1",
                    "Part",
                    "P",
                    Some(0),
                    Map::from_iter([("UniqueId".into(), uid.clone())]),
                ),
            ],
        };
        let stored_bytes = encode_document(&stored_doc, false, None)?;
        let mut cap_state = build_service_state_from_instances(
            "Workspace",
            None,
            vec![
                snapshot_instance("ws", "Workspace", "Workspace", 0, None, Map::new()),
                snapshot_instance(
                    "p1",
                    "Part",
                    "P",
                    1,
                    Some(0),
                    Map::from_iter([
                        ("UniqueId".into(), uid.clone()),
                        ("FutureUnknownObjectProp".into(), obj_val.clone()),
                    ]),
                ),
            ],
            HashMap::new(),
            true,
        )?;
        cap_state.dense_index_topology = true;
        let pulled = super::super::pulled_digest_from_service_state(&cap_state)?;
        let (alignment, _) = super::super::captured::changed_document_rows(
            &cap_state,
            &pulled.rows,
            &stored_bytes,
            None,
        )?
        .expect("changed_document_rows must compare Part row");
        assert!(
            matches!(alignment, SettingsAlignment::Equivalent),
            "Settings 1: captured-only unknown object-typed property must compare Equivalent"
        );

        let stored_with_obj = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                doc_instance("1", "Workspace", "Workspace", None, Map::new()),
                doc_instance(
                    "p1",
                    "Part",
                    "P",
                    Some(0),
                    Map::from_iter([
                        ("UniqueId".into(), uid.clone()),
                        ("FutureUnknownObjectProp".into(), obj_val),
                    ]),
                ),
            ],
        };
        let stored_obj_bytes = encode_document(&stored_with_obj, false, None)?;
        let mut cap_empty = build_service_state_from_instances(
            "Workspace",
            None,
            vec![
                snapshot_instance("ws", "Workspace", "Workspace", 0, None, Map::new()),
                snapshot_instance(
                    "p1",
                    "Part",
                    "P",
                    1,
                    Some(0),
                    Map::from_iter([("UniqueId".into(), uid)]),
                ),
            ],
            HashMap::new(),
            true,
        )?;
        cap_empty.dense_index_topology = true;
        let pulled_empty = super::super::pulled_digest_from_service_state(&cap_empty)?;
        let (alignment_b, _) = super::super::captured::changed_document_rows(
            &cap_empty,
            &pulled_empty.rows,
            &stored_obj_bytes,
            None,
        )?
        .expect("changed_document_rows must compare Part row");
        assert!(
            matches!(alignment_b, SettingsAlignment::Equivalent),
            "Settings 1: stored-only unknown object-typed property must compare Equivalent"
        );
        Ok(())
    }

    #[test]
    fn compact_renews_decode_budget_and_matches_non_twin_refs() -> Result<()> {
        let ref_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                doc_instance("ws", "Workspace", "Workspace", None, Map::new()),
                doc_instance(
                    "t1",
                    "Part",
                    "Twin",
                    Some(0),
                    Map::from_iter([("Transparency".into(), json!(0.25))]),
                ),
                doc_instance(
                    "t2",
                    "Part",
                    "Twin",
                    Some(0),
                    Map::from_iter([("Transparency".into(), json!(0.25))]),
                ),
                doc_instance(
                    "w",
                    "WeldConstraint",
                    "Weld",
                    Some(0),
                    Map::from_iter([("Part0".into(), json!({"_type": "Ref", "settingsId": "t1"}))]),
                ),
            ],
        };
        let obs_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                doc_instance("o_ws", "Workspace", "Workspace", None, Map::new()),
                doc_instance(
                    "o_t2",
                    "Part",
                    "Twin",
                    Some(0),
                    Map::from_iter([("Transparency".into(), json!(0.25))]),
                ),
                doc_instance(
                    "o_t1",
                    "Part",
                    "Twin",
                    Some(0),
                    Map::from_iter([("Transparency".into(), json!(0.25))]),
                ),
                doc_instance(
                    "o_w",
                    "WeldConstraint",
                    "Weld",
                    Some(0),
                    Map::from_iter([(
                        "Part0".into(),
                        json!({"_type": "Ref", "settingsId": "o_t1"}),
                    )]),
                ),
            ],
        };
        let ref_bytes = encode_document(&ref_doc, false, None)?;
        let obs_bytes = encode_document(&obs_doc, false, None)?;
        let ref_store = BlockStore::open(&ref_bytes)?;
        let obs_store = BlockStore::open(&obs_bytes)?;

        for step in [1024 * 1024, 1024, 1] {
            while ref_store.budget.take(step).is_ok() {}
        }
        assert!(
            ref_store.budget.take(1).is_err(),
            "decode budget must be exhausted before testing compact() renewal"
        );
        let (compact_ref, ids_ref) = ref_store.compact()?;
        assert_eq!(compact_ref.meta.len(), 4);
        assert_eq!(ids_ref, vec!["ws", "t1", "t2", "w"]);

        let (aligned_ids, _) = crate::settings::equivalence::match_opened_block_stores_compact(
            &ref_store, &obs_store,
        )?
        .expect("compact matcher must align identical twins via non-twin Part0 ref");
        assert_eq!(aligned_ids, vec!["ws", "t2", "t1", "w"]);
        Ok(())
    }

    #[test]
    fn fast_verify_rejects_mismatched_types_nan_neg_zero_refs_and_one_sided_props() -> Result<()> {
        let uid = |n: u8| json!({"_type": "UniqueId", "value": format!("{n:032x}")});
        let doc = |instances| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        };
        let row = |id: &str, class: &str, name: &str, n: u8, extra: Vec<(&str, Value)>| {
            let mut props = Map::from_iter([("UniqueId".into(), uid(n))]);
            for (k, v) in extra {
                props.insert(k.into(), v);
            }
            doc_instance(id, class, name, Some(0), props)
        };
        let ws = || doc_instance("ws", "Workspace", "Workspace", None, Map::new());
        let run_verify = |des: &SettingsBytecode, obs: &SettingsBytecode| -> Result<Vec<bool>> {
            let (des_bytes, obs_bytes) = (
                encode_document(des, false, None)?,
                encode_document(obs, false, None)?,
            );
            let n = des.instances.len();
            let places: Vec<usize> = (0..n).collect();
            let (by_order, skip) = (vec![false; n], vec![false; n]);
            Ok(fast_verify_aligned_v12_rows(FastVerifyInput {
                desired_bytes: &des_bytes,
                observed_bytes: &obs_bytes,
                desired_instances: &des.instances,
                observed_instances: &obs.instances,
                places: &places,
                by_order: &by_order,
                skip: &skip,
                has_before: true,
            })?
            .expect("v12 stores must run fast verify"))
        };

        let res_types = run_verify(
            &doc(vec![
                ws(),
                row(
                    "p1",
                    "Part",
                    "A",
                    1,
                    vec![(
                        "CustomA",
                        json!({"_type": "Vector3", "x": 1.0, "y": 0.5, "z": 0.25}),
                    )],
                ),
                row(
                    "p2",
                    "Part",
                    "B",
                    2,
                    vec![(
                        "CustomB",
                        json!({"_type": "UDim", "scale": 1.0, "offset": 2.0}),
                    )],
                ),
                row(
                    "p3",
                    "Part",
                    "C",
                    3,
                    vec![("CustomC", json!({"_type": "Font", "family": "Plastic"}))],
                ),
            ]),
            &doc(vec![
                ws(),
                row(
                    "p1",
                    "Part",
                    "A",
                    1,
                    vec![(
                        "CustomA",
                        json!({"_type": "Color3", "r": 1.0, "g": 0.5, "b": 0.25}),
                    )],
                ),
                row(
                    "p2",
                    "Part",
                    "B",
                    2,
                    vec![("CustomB", json!({"_type": "Vector2", "x": 1.0, "y": 2.0}))],
                ),
                row(
                    "p3",
                    "Part",
                    "C",
                    3,
                    vec![(
                        "CustomC",
                        json!({"_type": "Font", "family": {"_type": "EnumItem", "enumType": "Material", "name": "Plastic"}}),
                    )],
                ),
            ]),
        )?;
        assert_eq!(&res_types[1..], &[false, false, false]);

        assert!(!f64_finite_bits_eq(f64::NAN, f64::NAN) && !f32_finite_bits_eq(f32::NAN, f32::NAN));
        assert!(!f64_finite_bits_eq(-0.0, 0.0) && !f32_finite_bits_eq(-0.0, 0.0));
        let empty_bytes = encode_document(&doc(vec![ws()]), false, None)?;
        let empty_store = BlockStore::open(&empty_bytes)?;
        let empty_open = empty_store.open_block(0)?;
        for (kind, left, right) in [
            (
                5_u8,
                f64::NAN.to_le_bytes().as_slice(),
                f64::NAN.to_le_bytes().as_slice(),
            ),
            (
                5_u8,
                (-0.0_f64).to_le_bytes().as_slice(),
                0.0_f64.to_le_bytes().as_slice(),
            ),
            (
                20_u8,
                f32::NAN.to_le_bytes().as_slice(),
                f32::NAN.to_le_bytes().as_slice(),
            ),
            (
                20_u8,
                (-0.0_f32).to_le_bytes().as_slice(),
                0.0_f32.to_le_bytes().as_slice(),
            ),
        ] {
            assert!(!raw_complex_bytes_equal_no_refs(
                kind,
                left,
                right,
                (&empty_store, &empty_open),
                (&empty_store, &empty_open),
            )?);
        }

        let path_ref = json!({"_type": "Ref", "path": "Workspace.Twin", "pathSegments": ["Workspace", "Twin"]});
        let res_ref = run_verify(
            &doc(vec![
                ws(),
                row("a", "Part", "Twin", 10, vec![]),
                row("b", "Part", "Twin", 11, vec![]),
                row(
                    "w1",
                    "WeldConstraint",
                    "WeldRef",
                    12,
                    vec![("Part0", json!({"_type": "Ref", "settingsId": "a"}))],
                ),
                row(
                    "w2",
                    "Part",
                    "PathObj",
                    13,
                    vec![("CustomRef", path_ref.clone())],
                ),
            ]),
            &doc(vec![
                ws(),
                row("a", "Part", "Twin", 10, vec![]),
                row("b", "Part", "Twin", 11, vec![]),
                row(
                    "w1",
                    "WeldConstraint",
                    "WeldRef",
                    12,
                    vec![("Part0", json!({"_type": "Ref", "settingsId": "b"}))],
                ),
                row("w2", "Part", "PathObj", 13, vec![("CustomRef", path_ref)]),
            ]),
        )?;
        assert_eq!((res_ref[3], res_ref[4]), (false, false));

        let res_one = run_verify(
            &doc(vec![
                ws(),
                row(
                    "d1",
                    "Part",
                    "DesOnly",
                    20,
                    vec![("Transparency", json!(0.5))],
                ),
                row("d2", "Part", "ObsOnly", 21, vec![]),
                row(
                    "d3",
                    "Script",
                    "MigratedOneSided",
                    22,
                    vec![("Disabled", json!(true))],
                ),
                row(
                    "d4",
                    "Script",
                    "MigratedPlusOneSided",
                    23,
                    vec![("Disabled", json!(true)), ("Enabled", json!(true))],
                ),
            ]),
            &doc(vec![
                ws(),
                row("d1", "Part", "DesOnly", 20, vec![]),
                row(
                    "d2",
                    "Part",
                    "ObsOnly",
                    21,
                    vec![("Transparency", json!(0.5))],
                ),
                row("d3", "Script", "MigratedOneSided", 22, vec![]),
                row(
                    "d4",
                    "Script",
                    "MigratedPlusOneSided",
                    23,
                    vec![("Disabled", json!(true))],
                ),
            ]),
        )?;
        assert_eq!(&res_one[1..], &[false, false, false, false]);
        Ok(())
    }
}
