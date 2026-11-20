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
/// The content bytes the frames read in this process hold.
#[cfg(test)]
static DECOMPRESSED: AtomicUsize = AtomicUsize::new(0);

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
fn marked(text: &str) -> Option<[u8; 16]> {
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
fn digest(parts: &[&[u8]]) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part);
    }
    half_digest(digest)
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
    let (structure, prepared) = rayon::join(
        || structure_content(document, lookup),
        || {
            ranges
                .par_iter()
                .map(|range| prepared(&document.instances[range.clone()], lookup))
                .collect::<Result<Vec<_>>>()
        },
    );
    encode_prepared(structure?, prepared?, lookup, previous)
}

/// The store of a captured state whose instances are `records`, with the
/// values of `replaced` in place of its instance's, over `previous` where the
/// store's file holds that.
pub(super) fn encode_state<'a>(
    state: &'a ServiceState,
    (records, lookup): (
        &'a [SettingsBinaryInstance<'a>],
        &SettingsBinaryInstanceLookup<'_>,
    ),
    replaced: Option<ReplacedRow<'a>>,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let ranges = block_ranges(records.len(), BLOCK_ROWS);
    let (structure, prepared) = rayon::join(
        || {
            let collected = collected_records(records, collect_settings_binary_record_strings);
            encode_prepared_service_payload(records, lookup, collected)
        },
        || {
            ranges
                .par_iter()
                .map(|range| {
                    let mut collected = SettingsBinaryCollection::default();
                    for (row, record) in records[range.clone()].iter().enumerate() {
                        let source = (state, lookup);
                        collect_settings_binary_record_values(
                            &mut collected,
                            source,
                            row,
                            record,
                            replaced,
                        )?;
                    }
                    Prepared::of(range.len(), collected)
                })
                .collect::<Result<Vec<_>>>()
        },
    );
    encode_prepared(structure?, prepared?, lookup, previous)
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

/// The store file of a structure's content and the blocks `prepared`. Where
/// `previous` is a v12 store this writer wrote, each frame that comes out as
/// the one it holds at the same place is copied from it.
fn encode_prepared(
    structure: Vec<u8>,
    prepared: Vec<Prepared<'_>>,
    lookup: &SettingsBinaryInstanceLookup<'_>,
    previous: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let contents = prepared
        .par_iter()
        .map(|block| Ok((block.rows, block.content(lookup)?)))
        .collect::<Result<Vec<_>>>()?;
    let mut entries = PoolEntries::new();
    for block in &prepared {
        name_entries(&mut entries, block)?;
    }
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
        || framed(structure, previous.map(|store| (store, store.structure))),
        || {
            rayon::join(
                || {
                    (contents.par_iter().enumerate())
                        .map(|(at, (rows, content))| {
                            let held = previous
                                .and_then(|store| Some((store, store.blocks.get(at)?.frame)));
                            Ok((*rows, framed(content, held)?))
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
    let records = collected_records(&document.instances, collect_settings_record_strings);
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
        let mut short = Vec::new();
        let mut long = BTreeMap::<[u8; 16], Vec<SettingsString<'a>>>::new();
        for (text, count) in counts {
            let hash = marked(&text)
                .or_else(|| (text.len() >= POOL_MIN_BYTES).then(|| digest(&[text.as_bytes()])));
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

/// Counts `block` as a block that names each of its long strings, taking the
/// text of each that is no marker.
fn name_entries<'t>(entries: &mut PoolEntries<'t>, block: &'t Prepared<'_>) -> Result<()> {
    for (hash, text) in block.strings.pooled() {
        let (count, held) = entries.entry(hash).or_default();
        *count += 1;
        anyhow::ensure!(
            held.zip(text).is_none_or(|(held, text)| held == text),
            "Two strings of a store share a hash"
        );
        *held = held.or(text);
    }
    Ok(())
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
        .map(|planned| {
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
            compressed(&content)
        })
        .collect()
}

/// Appends a pool entry of `text` to a pool frame's content.
fn write_pool_entry(content: &mut Vec<u8>, text: &str) -> Result<()> {
    match base64::decode(text) {
        Ok(bytes) if base64::encode(&bytes) == text => {
            write_var_u64(content, ((bytes.len() as u64) << 1) | 1)?;
            content.extend_from_slice(&bytes);
        }
        _ => {
            write_var_u64(content, (text.len() as u64) << 1)?;
            content.extend_from_slice(text.as_bytes());
        }
    }
    Ok(())
}

fn compressed(content: &[u8]) -> Result<Section<'static>> {
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
    pub(crate) fn decode(&self) -> Result<SettingsBytecode> {
        let (structure, opened) = rayon::join(
            || self.content(self.structure),
            || self.open_blocks(0..self.blocks.len()),
        );
        self.decode_opened(structure?, opened?)
    }

    /// The whole document of the structure's content and every block opened.
    fn decode_opened(
        &self,
        structure: Vec<u8>,
        opened: Vec<OpenBlock>,
    ) -> Result<SettingsBytecode> {
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
                let keep = |row: usize| rows.binary_search(&(first + row)).is_ok();
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
    /// is copied. `whole` writes every block and the structure from what
    /// the store holds, also where the edit leaves them alone.
    fn rewrite(
        &self,
        document: &SettingsBytecode,
        placed: Placed<'_>,
        moved: &[&str],
        whole: bool,
    ) -> Result<Vec<u8>> {
        let settled = settled_settings_document(document)?;
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
            moved: moved.iter().copied().collect(),
            long,
            dense: document_reference_lookup(&settled, false),
            every: OnceLock::new(),
        };
        let mut counts = BTreeMap::new();
        if kept {
            counts.extend(self.hashes.iter().copied().zip(self.counts.iter().copied()));
        }
        let mut brought = BTreeMap::<[u8; 16], String>::new();
        let mut sections = (edit.ranges.iter().map(|_| None)).collect::<Vec<Option<Section<'_>>>>();
        let indices = (0..edit.ranges.len()).collect::<Vec<_>>();
        for batch in indices.chunks(RESTRUCTURED_BATCH) {
            self.budget.renew(self.bytes.len());
            let written = (batch.par_iter())
                .map(|index| self.rewritten(&edit, *index))
                .collect::<Result<Vec<_>>>()?;
            for (index, written) in batch.iter().zip(written) {
                let Some(written) = written else {
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
                sections[*index] = Some(written.section);
            }
        }
        counts.retain(|_, count| *count > 0);
        let entries = (counts.iter())
            .map(|(hash, count)| (*hash, (*count, brought.get(hash).map(String::as_str))))
            .collect::<PoolEntries<'_>>();
        let structure = if whole {
            let content = structure_content(&settled, &edit.dense)?;
            framed(&content, Some((self, self.structure)))?
        } else if kept && settled.instances.len() == self.rows && moved.is_empty() {
            self.raw(self.structure)?
        } else {
            compressed(&structure_content(&settled, &edit.dense)?)?
        };
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

    /// Block `index` of the file `edit` writes, or none where the block the
    /// store holds there stands as it is: its rows hold their stored values
    /// at their places, and its table none of the strings the edit moved.
    fn rewritten(&self, edit: &Rewrite<'_>, index: usize) -> Result<Option<Written<'_>>> {
        let range = edit.ranges[index].clone();
        let Placed { from, to, held } = edit.placed;
        let mut sources = Vec::new();
        for row in range.clone().filter(|row| !held[*row]) {
            let stored =
                from[row].context("An instance without its values is no row of the store")?;
            sources.push(self.block_of(stored));
        }
        if edit.kept && index < self.blocks.len() {
            sources.push(index);
        }
        sources.sort_unstable();
        sources.dedup();
        let untouched = !edit.whole && edit.kept && !range.clone().any(|row| held[row]);
        if untouched && edit.moved.is_empty() {
            return Ok(None);
        }
        let opened = (sources.iter())
            .map(|source| self.open_block(*source))
            .collect::<Result<Vec<_>>>()?;
        let names = |string: &BlockString| match string {
            BlockString::Own(text) => edit.moved.contains(text.as_str()),
            BlockString::Pool(entry) => edit.long.contains(&self.hashes[*entry]),
        };
        if untouched && !opened.iter().any(|open| open.strings.iter().any(names)) {
            return Ok(None);
        }
        let resolved = |hash: &[u8; 16]| edit.long.contains(hash);
        let texts = match edit.long.is_empty() {
            true => Texts::Marked,
            false => Texts::Resolved(&resolved),
        };
        let mut rows = edit.document.instances[range.clone()].to_vec();
        for (source, open) in sources.iter().zip(&opened) {
            let block = &self.blocks[*source];
            let placed =
                |row: usize| to[block.first + row].filter(|now| range.contains(now) && !held[*now]);
            let mut values = (0..block.count)
                .map(|_| DecodedRowValues::default())
                .collect::<Vec<_>>();
            self.block_into(open, &mut values, |row| placed(row).is_some(), texts)?;
            for (row, values) in values.into_iter().enumerate() {
                let Some(now) = placed(row) else {
                    continue;
                };
                let instance = &mut rows[now - range.start];
                instance.properties = values.properties;
                instance.attributes = values.attributes;
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
        }
        let lookup = edit.lookup(&rows);
        let block = prepared(&rows, lookup)?;
        let content = block.content(lookup)?;
        let stands = (sources.iter().zip(&opened))
            .any(|(source, open)| *source == index && open.content == content);
        let section = match stands {
            true => self.raw(self.blocks[index].frame)?,
            false => compressed(&content)?,
        };
        let before = match edit.kept {
            true => opened.iter().flat_map(OpenBlock::pool_entries).collect(),
            false => Vec::new(),
        };
        let now = (block.strings.pooled())
            .map(|(hash, text)| {
                let brought = text.filter(|_| self.entry_of(&hash).is_err());
                (hash, brought.map(str::to_string))
            })
            .collect();
        Ok(Some(Written {
            section,
            before,
            now,
        }))
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

    /// The rows `wanted` takes by their stored values, a marker in place of
    /// each pool entry: of the blocks whose table holds one of `strings`, or
    /// of every block where there are none. Blocks are read a few at a time,
    /// each batch on a budget of its own.
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
                    self.block_into(&open, &mut values, |_| true, Texts::Marked)?;
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
        use super::captured::{
            UNIQUE_ID, document_row_digest, value_digest, whole_document_row_digest,
        };
        let mut instances = match structure {
            Some(instances) if instances.len() == self.rows => instances,
            _ => self.structure()?.instances,
        };
        let Some((kept, moved)) = plan(&instances) else {
            return Ok(None);
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
        let blocks = crate::app::profile::span("store blocks digested");
        let digested = self
            .split(&mut instances)
            .into_par_iter()
            .zip(&self.blocks)
            .enumerate()
            .map(|(index, (rows, block))| {
                let open = self.open_block(index)?;
                if whole && kept.is_none() && moved.is_none() && shared.get(index) == Some(&true) {
                    self.values(
                        &open,
                        rows,
                        |_| true,
                        |name| name == UNIQUE_ID,
                        Texts::Marked,
                    )?;
                    return Ok((0_u64..)
                        .zip(rows)
                        .map(|(place, instance)| {
                            (
                                digest(&[&block.frame.digest, &place.to_le_bytes()]),
                                (instance.properties.get(UNIQUE_ID)).map_or([0; 16], value_digest),
                                None,
                            )
                        })
                        .collect());
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
                    let (sum, unique_id) = match &moved {
                        Some(moved) => {
                            // The instance as the state places it, its values
                            // that name one by its place given for the digest.
                            moved.parent_in_state(instance);
                            let (properties, attributes) = (
                                moved.values_in_state(&instance.properties),
                                moved.values_in_state(&instance.attributes),
                            );
                            let properties = properties
                                .map(|placed| std::mem::replace(&mut instance.properties, placed));
                            let attributes = attributes
                                .map(|placed| std::mem::replace(&mut instance.attributes, placed));
                            let digests = digest_of(instance);
                            if let Some(properties) = properties {
                                instance.properties = properties;
                            }
                            if let Some(attributes) = attributes {
                                instance.attributes = attributes;
                            }
                            digests
                        }
                        None => digest_of(instance),
                    };
                    digests.push((sum, unique_id, named.then_some(row)));
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
                Ok(digests)
            })
            .collect::<Result<Vec<_>>>()?;
        drop(blocks);
        let (mut digests, mut unique_ids, mut named) = (Vec::new(), Vec::new(), Vec::new());
        for (sum, unique_id, row) in digested.into_iter().flatten() {
            digests.push(sum);
            unique_ids.push(unique_id);
            named.extend(row);
        }
        let kept = kept.map(|kept| instances[kept].clone());
        let (ids, instances) = if whole {
            (Vec::new(), instances)
        } else {
            let ids = instances.into_iter().map(|instance| instance.settings_id);
            (ids.collect(), Vec::new())
        };
        Ok(Some(StoreRows {
            digests,
            unique_ids,
            kept,
            ids,
            moved,
            instances,
            named,
        }))
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
        self.values(open, rows, keep, |_| true, texts)
    }

    /// Gives `rows`, the rows of one block in order, the values the block
    /// holds for the rows `keep` takes of the properties `names` takes, with
    /// `texts` for the pool entries they name; a property name is always its
    /// own text. The table and the property names are copied on the budget,
    /// as v11 copies its property names, before any copy is made.
    fn values<T: DecodedPropertyTarget>(
        &self,
        open: &OpenBlock,
        rows: &mut [T],
        keep: impl Fn(usize) -> bool + Sync,
        names: impl Fn(&str) -> bool + Sync,
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
        let strings = table.into_iter().map(Cow::into_owned).collect::<Vec<_>>();
        let properties = (open.properties.iter())
            .map(|id| strings[*id].clone())
            .collect::<Vec<_>>();
        let mut reader = BytecodeReader::new(&open.content[open.rest..]);
        let specs = read_settings_property_groups(&mut reader, &properties)?;
        reader.finish()?;
        let specs = specs
            .into_iter()
            .filter(|(name, ..)| names(name))
            .collect::<Vec<_>>();
        // A marker is no payload: a store that holds payloads holds them once
        // the markers of the rows read have their texts. The decode charges
        // the store's own budget as it goes, so blocks read in parallel
        // together take no more than it holds.
        let marked = (!all).then(|| self.budget.not_holding());
        let scope = ValueScope {
            strings: &strings,
            instance_count: self.rows,
            budget: marked.as_ref().unwrap_or(&self.budget),
        };
        let held = rows.len();
        let groups = specs
            .par_iter()
            .map(|&(name, kind, count, body)| {
                let mut entries = decode_property_group_body(body, name, kind, count, scope)?;
                anyhow::ensure!(
                    entries
                        .last()
                        .is_none_or(|entry| entry.instance_index() < held),
                    "A value block gives a value to a row it does not hold"
                );
                entries.retain(|entry| keep(entry.instance_index()));
                Ok(entries)
            })
            .collect::<Result<Vec<_>>>()?;
        apply_decoded_property_groups_in_range(
            rows,
            0,
            &specs,
            groups,
            SETTINGS_BINARY_PARALLEL_CHUNK_SIZE,
        );
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
        // Each row takes its texts by itself; the payloads are held in the
        // order of the rows, which gives each its handle.
        if open.pool_entries().next().is_some() {
            (rows.par_iter_mut().enumerate())
                .filter(|(row, _)| keep(*row))
                .try_for_each(|(_, row)| {
                    let (properties, attributes) = row.maps();
                    unmark(properties, &text)?;
                    unmark(attributes, &text)
                })?;
        }
        if self.budget.holds_payloads {
            for row in rows.iter_mut().enumerate().filter(|(row, _)| keep(*row)) {
                let (properties, _) = row.1.maps();
                for value in properties.values_mut() {
                    *value = self.budget.property(std::mem::take(value));
                }
            }
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
                    self.values(&open, &mut values, keep, &names, Texts::Marked)?;
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
    let written = store.rewrite(&store.structure()?, placed, &[], true)?;
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
    // Rows asked in rising order come back as asked.
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

/// `decode_rows` of one v12 file a batch of rows at a time: its instances
/// are read once for all the batches, and each batch on a budget of its own.
pub(crate) struct RowBatches<'a> {
    bytes: &'a [u8],
    structure: Vec<SettingsBytecodeInstance>,
}

impl<'a> RowBatches<'a> {
    pub(crate) fn open(bytes: &'a [u8]) -> Result<Self> {
        let structure = BlockStore::open(bytes)?.structure()?.instances;
        Ok(Self { bytes, structure })
    }

    /// The id of every instance of the file.
    pub(crate) fn ids(&self) -> Vec<String> {
        let rows = self.structure.iter();
        rows.map(|row| row.settings_id.clone()).collect()
    }

    pub(crate) fn read(&self, rows: &[usize]) -> Result<Vec<SettingsBytecodeInstance>> {
        rows_of(&BlockStore::open(self.bytes)?, &self.structure, rows)
    }
}

/// A v12 store that an edit reads and writes for `RetainedPropertyColumns`:
/// the instances it makes complete come from their blocks, and only the
/// blocks the edit changes are written anew.
pub(crate) struct EditedStore {
    store: BlockStore<'static>,
    /// The instances as the store holds them, without values.
    records: Vec<SettingsBytecodeInstance>,
    /// The values the document is given beside its records, by row: the
    /// viewport camera its root names, which the camera checks read on any
    /// row, and each property `give` read.
    given: AHashMap<usize, Map<String, Value>>,
    complete: Vec<bool>,
}

impl EditedStore {
    /// The structure of a v12 file, with the root's viewport camera.
    pub(crate) fn decode(bytes: &[u8]) -> Result<(SettingsBytecode, Self)> {
        let store = BlockStore::open(bytes.to_vec())?;
        let mut document = store.structure()?;
        let records = document.instances.clone();
        let root = records.iter().position(|row| row.parent_index.is_none());
        let mut edited = Self {
            store,
            complete: vec![false; records.len()],
            records,
            given: AHashMap::new(),
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
    /// block it needs once, and so takes no more than a whole decode.
    pub(crate) fn materialize(
        &mut self,
        document: &mut SettingsBytecode,
        indices: &[usize],
    ) -> Result<()> {
        self.in_place(document)?;
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
    /// which sees a marker in place of each pool entry: of the blocks that
    /// hold one of `strings`, or of every block where there are none.
    pub(crate) fn materialize_where(
        &mut self,
        document: &mut SettingsBytecode,
        strings: Option<&AHashSet<&str>>,
        wanted: impl Fn(&Value) -> bool + Sync,
    ) -> Result<()> {
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
        let rows = (self.records.iter().enumerate())
            .map(|(row, record)| (record.settings_id.as_str(), row))
            .collect::<AHashMap<_, _>>();
        let from = (document.instances.iter())
            .map(|instance| rows.get(instance.settings_id.as_str()).copied())
            .collect::<Vec<_>>();
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
        let moved = self.moved(document, placed, in_place)?;
        self.store.budget.renew(self.store.bytes.len());
        if self.store.writer != writer() {
            return self.whole(document, placed);
        }
        self.store.rewrite(document, placed, &moved, false)
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
                    .map(|at| Ok((at, self.store.block_values(at, |_| true, Texts::All)?)))
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
        let store = BlockStore::open(&*self.store.bytes)?.holding();
        anyhow::ensure!(
            document.instances.len() == store.rows,
            "Store structure changed during row decoding; restart the store read"
        );
        let projected = |index: usize| -> Result<Vec<(usize, String, Value)>> {
            let open = store.open_block(index)?;
            let mut values = (0..store.blocks[index].count)
                .map(|_| DecodedRowValues::default())
                .collect::<Vec<_>>();
            store.values(
                &open,
                &mut values,
                |_| true,
                &names,
                Texts::Resolved(&|_| true),
            )?;
            Ok(store.blocks[index]
                .rows()
                .zip(values)
                .flat_map(|(row, values)| {
                    let properties = values.properties.into_iter();
                    properties.map(move |(name, value)| (row, name, value))
                })
                .collect())
        };
        let projected = (0..store.blocks.len())
            .into_par_iter()
            .map(projected)
            .collect::<Result<Vec<_>>>()?;
        for (row, name, value) in projected.into_iter().flatten() {
            match project(&document.instances[row], &name, value) {
                Some(value) => document.instances[row].properties.insert(name, value),
                None => document.instances[row].properties.remove(&name),
            };
        }
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
