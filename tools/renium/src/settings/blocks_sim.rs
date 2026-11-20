//! Seeded simulation of the v12 block store, with v11 as its oracle.
//!
//! Each seed takes a document of the codec simulation, which reaches every
//! value path of the encoder, and gives some of its instances strings the
//! pool holds: shared payloads, texts, keys, attribute names and property
//! names, among them one of digits only and one a byte too short for the
//! pool. The seed writes it in blocks of a size it draws. Five laws hold:
//!
//! 1. Both encoders take the document or both refuse it, and neither reader
//!    takes the other's file.
//! 2. The v12 decode is the v11 decode, and the structure read is that
//!    document without values. A string-set search finds what v11 finds.
//! 3. Rows read alone are the rows of the whole decode, even from a file
//!    whose other blocks and pool frames are destroyed, and the laws of the
//!    row chunk and property edit readers hold for blocks of every size.
//! 4. A document has one file. After every step of a run of edits, the file
//!    is byte for byte the whole encode of the edited document, whose decode
//!    is the v11 decode of that document: an edit of some rows' values through
//!    `RetainedPropertyColumns` (adding, replacing and dropping strings the
//!    pool holds, shared with other blocks, a renamed record included), the
//!    edited document encoded whole over the file before it, a captured state
//!    of it written over that file, and rows added or taken out by a block
//!    boundary, written over the file. An edit of no rows gives the file back,
//!    and an edit that moves an instance to another row is refused.
//! 5. Each read of a truncated or damaged file refuses it or gives what the
//!    intact file gives.
//!
//! `crate::tests::support::run_seeds` names the variables that run more
//! seeds or replay one.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::property_edit::{decode_for_property_edit, decode_for_row_chunks};
use super::*;
use crate::settings::codec_sim::{captured_input, same_document};
use crate::tests::support::{SimRng, run_seeds};

/// One seed of each kind of the codec simulation's documents.
const SEEDS: u64 = 24;

/// A store version the simulations of the readers run on: its encoder, and
/// the version the documents its files decode to carry.
pub(crate) struct Format {
    pub(crate) version: u8,
    pub(crate) encode: fn(&SettingsBytecode) -> Result<Vec<u8>>,
}

pub(crate) const FORMATS: [Format; 2] = [
    Format {
        version: SETTINGS_COLUMN_VERSION,
        encode: encode_settings_bytecode_v11,
    },
    Format {
        version: SETTINGS_BINARY_VERSION,
        encode: encode_settings_bytecode,
    },
];

/// `document` as a store in blocks of `block_rows` rows.
pub(crate) fn in_blocks(document: &SettingsBytecode, block_rows: usize) -> Result<Vec<u8>> {
    encode_document_in_blocks(document, true, None, block_rows)
}

/// How many blocks the store `first` and the store `second` hold byte for
/// byte, as a comparison of the two takes them.
pub(crate) fn blocks_of_both(first: &[u8], second: &[u8]) -> usize {
    let (Ok(first), Ok(second)) = (BlockStore::open(first), BlockStore::open(second)) else {
        return 0;
    };
    let shared = first.blocks_held_by(&second);
    shared.into_iter().filter(|shared| *shared).count()
}

/// The whole encode of `document` in the blocks of `file`'s rows, which a
/// writer over a v12 file keeps: the file an edit of `file` gives.
pub(crate) fn encoded_as(file: &[u8], document: &SettingsBytecode) -> Result<Vec<u8>> {
    let held = BlockStore::open(file)
        .ok()
        .and_then(|store| store.blocks.first().map(|block| block.count));
    in_blocks(document, held.unwrap_or(BLOCK_ROWS))
}

/// The file after an edit of the values of the rows at `rows`, each as
/// `change` leaves it, through `RetainedPropertyColumns`; `reorder` then
/// swaps the first two rows the edit read.
fn edit_store(
    bytes: &[u8],
    rows: &[usize],
    mut change: impl FnMut(&mut SettingsBytecodeInstance),
    reorder: bool,
) -> Result<Vec<u8>> {
    let (mut document, mut columns) = decode_for_property_edit(bytes)?;
    let rows = chosen(rows);
    columns.materialize(&mut document, &rows)?;
    for row in &rows {
        change(&mut document.instances[*row]);
    }
    if reorder && rows.len() > 1 {
        document.instances.swap(rows[0], rows[1]);
    }
    columns.encode(&document)
}

/// The edit that gives a record another name or id, which an edit of values
/// may.
const RECORD_EDIT: usize = 7;
/// The edit that swaps two rows, which an edit of values refuses.
const REORDER: usize = 8;
/// The step that adds or takes out rows, which only a whole write may.
const ROWS: usize = 9;
/// The edit that gives a record another parent, which an edit of values may.
const MOVE: usize = 10;

/// Texts for the pool, but the first, a byte too short for it; one of them
/// a frame of its own, and enough that many buckets hold several.
fn long_texts(seed: u64) -> Vec<String> {
    let mut rng = SimRng(seed);
    let mut letters = |len: usize| {
        (0..len)
            .map(|_| char::from(b'A' + rng.below(26) as u8))
            .collect::<String>()
    };
    let mut texts = vec![
        letters(POOL_MIN_BYTES - 1),
        letters(POOL_MIN_BYTES),
        letters(3000),
        "7".repeat(POOL_MIN_BYTES + 44),
        "日本".repeat(POOL_MIN_BYTES / 3),
        letters(40_000),
    ];
    texts.extend((0..40).map(|index| letters(POOL_MIN_BYTES + 23 * index)));
    texts
}

/// The codec simulation's document of `seed`, some of its instances given
/// strings of `texts`.
fn document(seed: u64, texts: &[String]) -> SettingsBytecode {
    let mut rng = SimRng(seed ^ 0xb10c);
    let mut document = captured_input(seed);
    let names = (document.instances.iter())
        .map(|instance| instance.name.clone())
        .collect::<Vec<_>>();
    let root = (document.instances.iter()).position(|instance| instance.parent_index.is_none());
    for (index, instance) in document.instances.iter_mut().enumerate() {
        let choice = rng.below(8);
        let mut text = || texts[rng.below(texts.len())].clone();
        match choice {
            0 | 1 => {
                let payload = json!({"_type": "SharedString", "base64": text()});
                instance
                    .properties
                    .insert("PhysicalConfigData".into(), payload);
            }
            2 => {
                instance
                    .properties
                    .insert("Description".into(), json!(text()));
            }
            3 => {
                instance.attributes.insert("Blob".into(), json!(text()));
            }
            4 => {
                let nested = json!([text(), Value::Object(Map::from_iter([(text(), json!(1))]))]);
                instance.properties.insert("Nested".into(), nested);
            }
            5 => {
                instance.properties.insert(text(), json!(true));
                instance.attributes.insert(text(), json!(text()));
            }
            6 => {
                let gone = format!("gone-{}", instance.settings_id.len() % 4);
                let gone = json!({"_type": "Ref", "settingsId": gone});
                instance.properties.insert("Dangling".into(), gone);
                let far = &names[(index + names.len() / 2) % names.len()];
                let path = json!([names[root.unwrap_or(0)], far]);
                let lost = json!({"_type": "Ref", "pathSegments": path, "pathOrdinals": [1, 1]});
                instance.properties.insert("Lost".into(), lost);
            }
            _ => {}
        }
    }
    document
}

fn bare(document: &SettingsBytecode) -> SettingsBytecode {
    SettingsBytecode {
        version: document.version,
        instances: document
            .instances
            .iter()
            .map(SettingsBytecodeInstance::bare)
            .collect(),
    }
}

/// The rows `read` gives, each with its record from `whole`.
fn read_rows(whole: &SettingsBytecode, read: Vec<(usize, DecodedRowValues)>) -> SettingsBytecode {
    SettingsBytecode {
        version: whole.version,
        instances: read
            .into_iter()
            .map(|(row, values)| SettingsBytecodeInstance {
                properties: values.properties,
                attributes: values.attributes,
                ..whole.instances[row].bare()
            })
            .collect(),
    }
}

/// `rows` in rising order and each once.
fn chosen(rows: &[usize]) -> Vec<usize> {
    let mut rows = rows.to_vec();
    rows.sort_unstable();
    rows.dedup();
    rows
}

/// The rows at `rows` of `whole`, in rising order and each once.
fn rows_of(whole: &SettingsBytecode, rows: &[usize]) -> SettingsBytecode {
    SettingsBytecode {
        version: whole.version,
        instances: chosen(rows)
            .into_iter()
            .map(|row| whole.instances[row].clone())
            .collect(),
    }
}

/// Every byte of `frame` inverted.
fn destroy(bytes: &mut [u8], frame: Frame) {
    for byte in &mut bytes[frame.at..frame.at + frame.len] {
        *byte = !*byte;
    }
}

fn failed(law: &str) -> impl Fn(anyhow::Error) -> String + '_ {
    move |error| format!("{law}: {error:#}")
}

/// Law 3: the rows at `asked` read from the file with every block and pool
/// frame they do not need destroyed (a read takes the entries its blocks
/// name), and the structure read from the file with all of them destroyed.
fn rows_alone(
    bytes: &[u8],
    store: &BlockStore<'_>,
    whole: &SettingsBytecode,
    asked: &[usize],
) -> Result<(), String> {
    let expected = rows_of(whole, asked);
    let (mut damaged, mut bare_file) = (bytes.to_vec(), bytes.to_vec());
    let mut needed = vec![false; store.pool.len()];
    for (index, block) in store.blocks.iter().enumerate() {
        if asked.iter().any(|row| block.rows().contains(row)) {
            let open = store.open_block(index).map_err(failed("law 3"))?;
            for entry in open.pool_entries() {
                needed[store.frame_of(entry)] = true;
            }
        } else {
            destroy(&mut damaged, block.frame);
        }
        destroy(&mut bare_file, block.frame);
    }
    for (frame, needed) in store.pool.iter().zip(needed) {
        if !needed {
            destroy(&mut damaged, frame.frame);
        }
        destroy(&mut bare_file, frame.frame);
    }
    let read = BlockStore::open(&damaged)
        .and_then(|store| store.rows(asked))
        .map_err(failed("law 3: rows read alone"))?;
    if !same_document(&read_rows(whole, read), &expected) {
        return Err(format!(
            "law 3: rows {asked:?} read alone differ from the whole decode"
        ));
    }
    let structure = BlockStore::open(&bare_file)
        .and_then(|store| store.structure())
        .map_err(failed("law 3: the structure read alone"))?;
    if structure != bare(whole) {
        return Err("law 3: the structure read alone differs from the whole decode".into());
    }
    Ok(())
}

/// The edit `kind` of one row.
fn edited(
    kind: usize,
    rng: &mut SimRng,
    texts: &[String],
    target: &str,
    count: usize,
    instance: &mut SettingsBytecodeInstance,
) {
    let number = rng.next();
    let mut text = || texts[rng.below(texts.len())].clone();
    match kind {
        0 => {
            let value = json!((number % 64) as f64 / 8.0);
            instance.properties.insert("Transparency".into(), value);
        }
        1 => {
            let fresh = format!("{number:N>width$}", width = POOL_MIN_BYTES + 9);
            let payload = json!({"_type": "SharedString", "base64": fresh});
            instance.properties.insert("Fresh".into(), payload);
            let shared = json!({"_type": "SharedString", "base64": text()});
            instance
                .properties
                .insert("PhysicalConfigData".into(), shared);
        }
        2 => {
            instance.attributes.insert("Again".into(), json!(text()));
        }
        3 => {
            instance.properties.clear();
            instance.attributes.clear();
        }
        4 => {
            let link = json!({"_type": "Ref", "instanceIndex": 1 + number as usize % count});
            instance.properties.insert("Link".into(), link);
            let named = json!({"_type": "Ref", "settingsId": target});
            instance.properties.insert("Named".into(), named);
        }
        5 => {
            instance.properties.insert("Name".into(), json!("refused"));
        }
        6 => {
            instance.properties.insert(text(), json!(text()));
        }
        _ => instance.name.push('!'),
    }
}

/// Law 4: an edit of no rows, then three steps from the seed's store.
fn edits(
    store: &BlockStore<'_>,
    whole: &SettingsBytecode,
    block_rows: usize,
    texts: &[String],
    rng: &mut SimRng,
) -> Result<(), String> {
    if edit_store(&store.bytes, &[], |_| {}, false).map_err(failed("law 4"))? != *store.bytes {
        return Err("law 4: an edit of no rows changed the file".into());
    }
    let (mut file, mut whole) = (store.bytes.to_vec(), whole.clone());
    for _ in 0..3 {
        if let Some(edited) = step(&file, &whole, block_rows, texts, rng)? {
            (file, whole) = edited;
        }
    }
    Ok(())
}

/// One step of law 4 from `file`, the store of `whole`, or from that file as
/// another writer would write it: the file it gives and its decode, or
/// `None` where both the step and the whole encode of the edited document
/// refuse it. An edit of a file another writer wrote writes it whole, in
/// blocks of `BLOCK_ROWS` rows.
fn step(
    file: &[u8],
    whole: &SettingsBytecode,
    block_rows: usize,
    texts: &[String],
    rng: &mut SimRng,
) -> Result<Option<(Vec<u8>, SettingsBytecode)>, String> {
    let foreign = rng.below(4) == 0;
    let file = &match foreign {
        true => foreign_file(file),
        false => file.to_vec(),
    };
    let count = whole.instances.len();
    let over = |document: &SettingsBytecode| {
        encode_document_in_blocks(document, true, Some(file), block_rows)
    };
    let kind = [
        0,
        1,
        2,
        2,
        3,
        4,
        5,
        6,
        RECORD_EDIT,
        RECORD_EDIT,
        REORDER,
        ROWS,
        MOVE,
        MOVE,
    ][rng.below(14)];
    let (by_blocks, changed, rows) = if kind == ROWS || count == 0 {
        let changed = rows_moved(whole, block_rows, texts, rng);
        (over(&changed), changed, Vec::new())
    } else {
        let mut rows = (0..1 + rng.below(3))
            .map(|_| rng.below(count))
            .collect::<Vec<_>>();
        let ids = match kind {
            RECORD_EDIT => new_ids(whole, &mut rows, rng),
            _ => HashMap::new(),
        };
        let lost = (kind == MOVE).then(|| lost_row(whole)).flatten();
        if let Some((row, _)) = lost {
            rows = vec![row];
        }
        let target = whole.instances[rng.below(count)].settings_id.clone();
        let seed = rng.next();
        let change = |edit: &mut SimRng, instance: &mut SettingsBytecodeInstance| match ids
            .get(&instance.settings_id)
        {
            Some(id) => instance.settings_id.clone_from(id),
            None => match lost {
                Some((row, root)) if whole.instances[row].settings_id == instance.settings_id => {
                    instance.parent_index = Some(root);
                }
                _ => edited(kind, edit, texts, &target, count, instance),
            },
        };
        let mut edit = SimRng(seed);
        let by_blocks = edit_store(file, &rows, |row| change(&mut edit, row), kind == REORDER);
        let mut edit = SimRng(seed);
        let mut changed = whole.clone();
        for row in chosen(&rows) {
            change(&mut edit, &mut changed.instances[row]);
        }
        if kind == REORDER {
            return match by_blocks {
                Ok(_) if chosen(&rows).len() > 1 => {
                    Err("law 4: an edit of values moved an instance to another row".into())
                }
                _ => Ok(None),
            };
        }
        (by_blocks, changed, rows)
    };
    let written_rows = match foreign && kind != ROWS && count > 0 {
        true => BLOCK_ROWS,
        false => block_rows,
    };
    let (by_blocks, fresh) = match (by_blocks, in_blocks(&changed, block_rows)) {
        (Ok(by_blocks), Ok(fresh)) => (by_blocks, fresh),
        (Err(_), Err(_)) => return Ok(None),
        (by_blocks, fresh) => {
            return Err(format!(
                "law 4: step {kind} by blocks {}, of the whole document {}",
                by_blocks.map_or_else(|error| format!("{error:#}"), |_| "taken".into()),
                fresh.map_or_else(|error| format!("{error:#}"), |_| "taken".into()),
            ));
        }
    };
    if Some(&by_blocks) != in_blocks(&changed, written_rows).ok().as_ref() {
        return Err(format!(
            "law 4: step {kind} of rows {rows:?} is not the whole encode of the edited document"
        ));
    }
    let got = BlockStore::open(&fresh)
        .and_then(|store| store.decode())
        .map_err(failed("law 4: the edited file"))?;
    let mut through_v11 = encode_settings_bytecode_v11(&changed)
        .and_then(|bytes| decode_settings_bytecode(&bytes))
        .map_err(failed("law 4: the v11 edit"))?;
    through_v11.version = SETTINGS_BINARY_VERSION;
    if !same_document(&got, &through_v11) {
        return Err(format!(
            "law 4: step {kind} of rows {rows:?} differs from the v11 edit"
        ));
    }
    if over(&changed).ok().as_ref() != Some(&fresh) {
        return Err(format!(
            "law 4: step {kind} of rows {rows:?} written whole over the file is another file"
        ));
    }
    let state = crate::settings::bytecode::captured_sim::captured_state(&changed, true);
    if let Ok(whole_state) = encode_service_settings_binary(&state)
        && encode_service_settings_binary_over(&state, Some(file)).ok() != Some(whole_state)
    {
        return Err(format!(
            "law 4: step {kind} of rows {rows:?} of a captured state written over the file is another file"
        ));
    }
    Ok(Some((fresh, got)))
}

/// The ids a record edit gives the rows at `rows`: another id; the id of
/// another row of the same name and class, whose row takes the first one's
/// and joins `rows`; or an id a reference names that no instance has, which
/// makes the blocks of that reference come out otherwise.
fn new_ids(
    whole: &SettingsBytecode,
    rows: &mut Vec<usize>,
    rng: &mut SimRng,
) -> HashMap<String, String> {
    let row = rows[0];
    let instance = &whole.instances[row];
    let twin = (whole.instances.iter().enumerate()).position(|(other, held)| {
        other != row && (&held.name, &held.class_name) == (&instance.name, &instance.class_name)
    });
    let ids = whole
        .instances
        .iter()
        .map(|held| held.settings_id.as_str())
        .collect::<HashSet<_>>();
    let dangling = (whole.instances.iter())
        .filter_map(|held| held.properties.get("Dangling")?.get("settingsId")?.as_str())
        .find(|id| !ids.contains(id));
    let id = instance.settings_id.clone();
    match (rng.below(4), twin, dangling) {
        (0, Some(twin), _) => {
            rows.push(twin);
            let other = whole.instances[twin].settings_id.clone();
            HashMap::from([(id.clone(), other.clone()), (other, id)])
        }
        (1 | 2, _, Some(dangling)) => HashMap::from([(id, dangling.to_string())]),
        _ => HashMap::from([(id.clone(), format!("{id}~"))]),
    }
}

/// A row that a reference names by a path under the first root and that
/// stands elsewhere, and that root: moved under it, the row makes the blocks
/// of that reference, which finds nothing there, come out otherwise.
fn lost_row(whole: &SettingsBytecode) -> Option<(usize, usize)> {
    let root = (whole.instances.iter()).position(|instance| instance.parent_index.is_none())?;
    let named = (whole.instances.iter())
        .filter_map(|held| {
            held.properties
                .get("Lost")?
                .get("pathSegments")?
                .get(1)?
                .as_str()
        })
        .collect::<HashSet<_>>();
    let row = (0..whole.instances.len()).find(|row| {
        let instance = &whole.instances[*row];
        *row != root
            && instance.parent_index != Some(root)
            && named.contains(instance.name.as_str())
    })?;
    Some((row, root))
}

/// `file` as another writer writes it: under another revision, every frame
/// compressed at another level.
fn foreign_file(file: &[u8]) -> Vec<u8> {
    let store = BlockStore::open(file).unwrap();
    let again = |frame: Frame| {
        let content = store.content(frame).unwrap();
        let bytes = compress_store_frame(&content, 3, true).unwrap();
        Section {
            decoded: content.len(),
            digest: digest(&[&bytes]),
            bytes: Cow::Owned(bytes),
        }
    };
    let entries = (store.hashes.iter().zip(&store.counts).zip(&store.alone))
        .map(|((hash, count), alone)| (*hash, *count, *alone))
        .collect::<Vec<_>>();
    let pool = store
        .pool
        .iter()
        .map(|frame| again(frame.frame))
        .collect::<Vec<_>>();
    let blocks = (store.blocks.iter())
        .map(|block| (block.count, again(block.frame)))
        .collect::<Vec<_>>();
    let structure = again(store.structure);
    let frames = std::iter::once(&structure)
        .chain(&pool)
        .chain(blocks.iter().map(|(_, frame)| frame))
        .map(|frame| frame.bytes.len())
        .sum::<usize>();
    let mut bytes = assemble(structure, &entries, &pool, &blocks).unwrap();
    bytes[SETTINGS_BINARY_MAGIC.len() + 1] += 1;
    let head = bytes.len() - frames - 16;
    let digest = digest(&[&bytes[..head]]);
    bytes[head..head + 16].copy_from_slice(&digest);
    bytes
}

/// `whole` with one to three rows added or taken out at a block boundary or
/// a row before it: new leaves under the root, some holding a string the
/// pool holds, or leaves no row is the parent of.
fn rows_moved(
    whole: &SettingsBytecode,
    block_rows: usize,
    texts: &[String],
    rng: &mut SimRng,
) -> SettingsBytecode {
    let mut document = whole.clone();
    let count = document.instances.len();
    let boundary = block_rows * (1 + rng.below(count / block_rows + 1));
    let at = boundary.saturating_sub(rng.below(2)).clamp(1, count.max(1));
    let moved = 1 + rng.below(3);
    let parents = (document.instances.iter())
        .filter_map(|instance| instance.parent_index)
        .collect::<HashSet<_>>();
    let leaves = (at..count.min(at + moved))
        .take_while(|row| !parents.contains(row))
        .count();
    if count > 1 && leaves > 0 && rng.below(2) == 0 {
        document.instances.drain(at..at + leaves);
        for instance in &mut document.instances {
            if let Some(parent) = &mut instance.parent_index
                && *parent >= at
            {
                *parent -= leaves;
            }
        }
        return document;
    }
    for instance in &mut document.instances {
        if let Some(parent) = &mut instance.parent_index
            && *parent >= at
        {
            *parent += moved;
        }
    }
    let added = (0..moved).map(|_| {
        let number = rng.next();
        let mut row = SettingsBytecodeInstance::new(
            format!("moved-{number}"),
            "Moved".into(),
            "Folder".into(),
            (count > 0).then_some(0),
        );
        if number.is_multiple_of(2) {
            let text = texts[number as usize % texts.len()].clone();
            let payload = json!({"_type": "SharedString", "base64": text});
            row.properties.insert("PhysicalConfigData".into(), payload);
        }
        row
    });
    let added = added.collect::<Vec<_>>();
    let at = at.min(count);
    document.instances.splice(at..at, added);
    document
}

/// Law 5 for a byte changed in the head and in the whole file at random
/// places, and for the file cut short in the head and at random lengths.
fn damaged(
    store: &BlockStore<'_>,
    whole: &SettingsBytecode,
    probe: &[usize],
    rng: &mut SimRng,
) -> Result<(), String> {
    let bytes = &*store.bytes;
    let head = store.structure.at;
    let mut positions = (0..48).map(|_| rng.below(head)).collect::<Vec<_>>();
    positions.extend((0..48).map(|_| rng.below(bytes.len())));
    let expected = rows_of(whole, probe);
    for at in positions {
        let mut bad = bytes.to_vec();
        bad[at] ^= 1 + rng.below(255) as u8;
        let Ok(store) = BlockStore::open(&bad) else {
            continue;
        };
        if store
            .decode()
            .is_ok_and(|document| !same_document(&document, whole))
        {
            return Err(format!(
                "law 5: damage at byte {at} decodes as another document"
            ));
        }
        if store
            .structure()
            .is_ok_and(|structure| structure != bare(whole))
        {
            return Err(format!(
                "law 5: damage at byte {at} reads as another structure"
            ));
        }
        if store
            .rows(probe)
            .is_ok_and(|read| !same_document(&read_rows(whole, read), &expected))
        {
            return Err(format!("law 5: damage at byte {at} reads as other rows"));
        }
    }
    let mut lengths = (0..(head + 32).min(bytes.len())).collect::<Vec<_>>();
    lengths.extend((0..32).map(|_| rng.below(bytes.len())));
    if let Some(len) = lengths
        .into_iter()
        .find(|len| BlockStore::open(&bytes[..*len]).is_ok())
    {
        return Err(format!("law 5: the file cut to {len} bytes opens"));
    }
    Ok(())
}

fn run_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let texts = long_texts(seed);
    let document = document(seed, &texts);
    let mut rng = SimRng(seed ^ 0x5eed);
    let block_rows = if document.instances.len() > 1000 {
        [64, 1000, BLOCK_ROWS][rng.below(3)]
    } else {
        [1, 2, 7, 64, BLOCK_ROWS][rng.below(5)]
    };
    log.push(format!(
        "{} instances in blocks of {block_rows}",
        document.instances.len()
    ));
    let (v11, bytes) = match (
        encode_settings_bytecode_v11(&document),
        in_blocks(&document, block_rows),
    ) {
        (Ok(v11), Ok(v12)) => (v11, v12),
        (Err(_), Err(_)) => return Ok(()),
        (v11, v12) => {
            return Err(format!(
                "law 1: v11 {} the document, v12 {}",
                if v11.is_ok() { "took" } else { "refused" },
                if v12.is_ok() { "took it" } else { "refused it" }
            ));
        }
    };
    if decode_settings_bytecode_container(&bytes).is_ok() || BlockStore::open(&v11).is_ok() {
        return Err("law 1: a reader of one version took the other's file".into());
    }
    let mut oracle = decode_settings_bytecode(&v11).map_err(failed("law 2"))?;
    oracle.version = SETTINGS_BINARY_VERSION;
    let store = BlockStore::open(&bytes).map_err(failed("law 2"))?;
    log.push(format!(
        "{} blocks, {} pool entries in {} frames",
        store.blocks.len(),
        store.hashes.len(),
        store.pool.len()
    ));
    let whole = store.decode().map_err(failed("law 2: the whole decode"))?;
    if !same_document(&whole, &oracle) {
        return Err("law 2: the v12 decode differs from the v11 decode".into());
    }
    if store.structure().map_err(failed("law 2"))? != bare(&oracle) {
        return Err("law 2: the structure differs from the v11 decode's instances".into());
    }
    let count = whole.instances.len();
    let mut asked = (0..rng.below(6))
        .map(|_| rng.below(count.max(1)))
        .filter(|row| *row < count)
        .collect::<Vec<_>>();
    if rng.below(4) == 0 {
        asked = (0..count).rev().collect();
    }
    log.push(format!("rows {asked:?}"));
    rows_alone(&bytes, &store, &whole, &asked)?;
    super::property_edit::sim::check_codec_document(&bytes)
        .map_err(failed("law 3: the row chunk and property edit laws"))?;
    // A string in the structure, in a block or in the pool is found as v11
    // finds it.
    let held = ["PhysicalConfigData", "Blob"].map(String::from);
    let first = whole
        .instances
        .first()
        .map(|instance| instance.name.clone());
    for text in texts.iter().chain(&first).chain(&held) {
        let sets = [vec![text.clone()]];
        let found = |bytes: &[u8]| {
            SettingsBytecode::decode_if_contains_any_string_set(bytes, &sets)
                .map(|found| found.map(|document| document.instances.len()))
                .map_err(failed("law 2: the string-set decode"))
        };
        if found(&bytes)? != found(&v11)? {
            return Err(format!(
                "law 2: the string-set decode differs for {} bytes",
                text.len()
            ));
        }
    }
    edits(&store, &whole, block_rows, &texts, &mut rng)?;
    damaged(&store, &whole, &asked[..asked.len().min(3)], &mut rng)
}

#[test]
fn block_stores_keep_the_laws_against_v11() {
    run_seeds(SEEDS, run_seed);
}

/// A few rows of a store of `count` instances, or every row.
fn asked(rng: &mut SimRng, count: usize) -> Vec<usize> {
    if rng.below(4) == 0 {
        return (0..count).rev().collect();
    }
    (0..rng.below(8)).map(|_| rng.below(count)).collect()
}

/// The row reader of the read commands against the whole decode, on the
/// documents of `run_seed` in blocks of a size each seed draws. The
/// structure is the whole decode without values; filled rows are those of
/// the whole decode, and the other rows hold none; a walk visits the rows it
/// is given in rising order, each with the whole decode's values, stops where
/// it is told and leaves the document as it was; a property given alone is
/// the whole decode's value of it. A batch of store queries over the file
/// answers as over the v11 file of the document, which is read whole.
fn run_row_reader_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    use clap::Parser;
    let texts = long_texts(seed);
    let document = document(seed, &texts);
    let mut rng = SimRng(seed ^ 0x40e5);
    let block_rows = [1, 2, 7, 64, BLOCK_ROWS][rng.below(5)];
    let count = document.instances.len();
    log.push(format!("{count} instances in blocks of {block_rows}"));
    let (Ok(v11), Ok(bytes)) = (
        encode_settings_bytecode_v11(&document),
        in_blocks(&document, block_rows),
    ) else {
        return Ok(());
    };
    if count == 0 {
        return Ok(());
    }
    let whole = decode_settings_bytecode(&bytes).map_err(failed("the whole decode"))?;
    let (mut read, mut reader) = RowReader::decode(&bytes).map_err(failed("the structure"))?;
    if read != bare(&whole) {
        return Err("the structure differs from the whole decode without values".into());
    }
    let mut filled = vec![false; count];
    for _ in 0..2 {
        let rows = asked(&mut rng, count);
        log.push(format!("filled {rows:?}"));
        reader.fill(&mut read, &rows).map_err(failed("a fill"))?;
        for row in rows {
            filled[row] = true;
        }
        let expected = SettingsBytecode {
            version: whole.version,
            instances: (whole.instances.iter().zip(&filled))
                .map(|(instance, filled)| match filled {
                    true => instance.clone(),
                    false => instance.bare(),
                })
                .collect(),
        };
        if !same_document(&read, &expected) {
            return Err("filled rows differ from the whole decode's, or others hold values".into());
        }
    }
    let rows = chosen(&asked(&mut rng, count));
    let stop = rng.below(rows.len() + 1);
    log.push(format!("walked {rows:?} to the {}th", stop + 1));
    let before = read.clone();
    let mut visited = Vec::new();
    reader
        .walk(
            &mut read,
            &rows,
            |_| true,
            |document, row| {
                visited.push((row, document.instances[row].clone()));
                visited.len() <= stop
            },
        )
        .map_err(failed("a walk"))?;
    let (visited_rows, visited): (Vec<_>, Vec<_>) = visited.into_iter().unzip();
    let expected = &rows[..rows.len().min(stop + 1)];
    if visited_rows != expected {
        return Err(format!("a walk visited {visited_rows:?}"));
    }
    let visited = SettingsBytecode {
        version: whole.version,
        instances: visited,
    };
    if !same_document(&visited, &rows_of(&whole, expected)) {
        return Err("a walk gave rows other values than the whole decode".into());
    }
    if !same_document(&read, &before) {
        return Err("a walk left the document otherwise".into());
    }
    let properties = (whole.instances.iter())
        .flat_map(|instance| instance.properties.iter())
        .collect::<Vec<_>>();
    let attributes = (whole.instances.iter())
        .flat_map(|instance| instance.attributes.keys())
        .collect::<Vec<_>>();
    let property = rng.pick(&properties);
    if let Some((name, _)) = property {
        let rows = asked(&mut rng, count);
        let mut given = bare(&whole);
        reader
            .give(&mut given, &rows, name)
            .map_err(failed("a property given"))?;
        let mut expected = bare(&whole);
        for row in rows {
            let value = whole.instances[row].properties.get(name);
            let properties = &mut expected.instances[row].properties;
            properties.extend(value.map(|value| (name.clone(), value.clone())));
        }
        if !same_document(&given, &expected) {
            return Err(format!("the property {name:?} given alone differs"));
        }
    }
    let row = rng.below(count);
    let instance = &whole.instances[row];
    let mut ops = vec![
        json!({"type": "instance", "index": row, "output": "full"}),
        json!({"type": "children", "index": row, "output": "full"}),
        json!({"type": "service", "fields": "id,props,attrs"}),
        json!({"type": "find", "className": &instance.class_name, "limit": 2, "output": "full"}),
        json!({"type": "search", "query": &instance.name, "limit": 3, "fields": "id,attrs"}),
    ];
    if let Some((name, value)) = property {
        ops.push(
            json!({"type": "find", "properties": [format!("{name}={value}")],
            "limit": 0, "output": "full"}),
        );
        ops.push(
            json!({"type": "search", "query": format!("{name}="), "limit": 0,
            "output": "full"}),
        );
    }
    if let Some(name) = rng.pick(&attributes) {
        ops.push(json!({"type": "find", "attributes": [name], "limit": 0, "fields": "id,attrs"}));
    }
    let request = json!({ "ops": ops }).to_string();
    let dir = crate::tests::support::temp_dir("row-reader");
    let file = dir.join("Store.renium");
    let answer = |bytes: &[u8]| {
        fs::write(&file, bytes).map_err(|error| error.to_string())?;
        let path = file.to_string_lossy();
        let args = crate::cli::BytecodeExplorerBatchArgs::try_parse_from([
            "bb", "-f", &path, "-j", &request,
        ])
        .map_err(|error| error.to_string())?;
        Ok::<_, String>(
            crate::bytecode::explorer::bytecode_explorer_batch_result(args)
                .unwrap_or_else(|error| json!(format!("{error:#}"))),
        )
    };
    let answers = answer(&v11).and_then(|first| Ok((first, answer(&bytes)?)));
    let _ = fs::remove_dir_all(&dir);
    let (from_whole, from_rows) = answers?;
    if from_whole != from_rows {
        let op = (0..ops.len()).find(|op| from_whole["rs"][op] != from_rows["rs"][op]);
        let kind = op.map(|op| &ops[op]["type"]);
        return Err(format!(
            "a batch answers otherwise read by rows, from op {op:?} {kind:?}"
        ));
    }
    Ok(())
}

#[test]
fn row_reads_give_what_the_whole_decode_gives() {
    run_seeds(SEEDS, run_row_reader_seed);
}

/// A frame of no content, with its checksum, under a digest that matches,
/// which claims `decoded` bytes of content.
fn claiming(decoded: usize) -> Section<'static> {
    let bytes = compress_store_frame(&[], 1, true).unwrap();
    Section {
        decoded,
        digest: digest(&[&bytes]),
        bytes: Cow::Owned(bytes),
    }
}

/// Blocks read in parallel charge the store's one budget as they decode, so
/// together they take no more than it holds: a block's values are charged
/// before its read goes on, and a read the budget cannot pay is refused.
#[test]
fn block_reads_charge_the_store_budget_as_they_decode() {
    let text = "b".repeat(POOL_MIN_BYTES - 1);
    let bytes = in_blocks(&in_rows(256, |_| Some(text.clone())), 32).unwrap();
    let store = BlockStore::open(&bytes).unwrap().holding();
    let left = || store.budget.remaining.load(Ordering::Relaxed);
    store.budget.remaining.store(1 << 30, Ordering::Relaxed);
    let most = AtomicUsize::new(0);
    let keep = |_| {
        most.fetch_max(left(), Ordering::Relaxed);
        true
    };
    store
        .block_values(1, keep, Texts::Resolved(&|_| true))
        .unwrap();
    assert!(most.into_inner() <= (1 << 30) - 32 * text.len());
    store
        .budget
        .remaining
        .store(((1 << 30) - left()) * 3 / 2, Ordering::Relaxed);
    let error = (store.rows(&(0..store.rows).collect::<Vec<_>>()).map(drop)).unwrap_err();
    assert!(format!("{error:#}").contains("more memory than a file of its size may take"));
}

/// A head with a valid digest may still claim anything: frames whose content
/// takes most of what a store may hold, more rows than the structure holds,
/// more pool entries than a frame holds, more than a store may hold in all.
/// Each read refuses it, and what a read reserves stays far below the claim.
#[test]
fn forged_heads_are_refused_without_taking_what_they_claim() {
    let document = document(0, &long_texts(0));
    let bytes = in_blocks(&document, 3).unwrap();
    let store = BlockStore::open(&bytes).unwrap();
    let pool = || {
        (store.pool.iter())
            .map(|frame| store.raw(frame.frame).unwrap())
            .collect::<Vec<_>>()
    };
    let entries = || {
        (store.hashes.iter().zip(&store.counts).zip(&store.alone))
            .map(|((hash, count), alone)| (*hash, *count, *alone))
            .collect::<Vec<_>>()
    };
    let blocks = || {
        (store.blocks.iter())
            .map(|block| (block.count, store.raw(block.frame).unwrap()))
            .collect::<Vec<_>>()
    };
    let structure = || store.raw(store.structure).unwrap();
    let claim = 100 << 20;
    let refused = |bytes: Vec<u8>, read: &dyn Fn(&BlockStore<'_>) -> Result<()>, why: &str| {
        let store = BlockStore::open(bytes).unwrap();
        let error = format!("{:#}", read(&store).unwrap_err());
        assert!(error.contains(why), "{error}");
        assert!(store.reserved.load(Ordering::Relaxed) <= FRAME_READ_START);
    };
    let edit = |store: &BlockStore<'_>| edit_store(&store.bytes, &[0], |_| {}, false).map(drop);
    // A structure, and then a block, that claim 100 MB of content.
    let short = "does not state the length its head gives";
    let file = assemble(claiming(claim), &entries(), &pool(), &blocks()).unwrap();
    refused(file.clone(), &|store| store.structure().map(drop), short);
    refused(file, &|store| store.decode().map(drop), short);
    let mut claimed = blocks();
    claimed[0].1 = claiming(claim);
    let file = assemble(structure(), &entries(), &pool(), &claimed).unwrap();
    refused(file.clone(), &|store| store.rows(&[0]).map(drop), short);
    refused(file, &edit, short);
    // Entries out of hash order, more blocks naming one than there are, and
    // a block naming an entry the pool lacks.
    let mut swapped = entries();
    swapped.swap(0, 1);
    let mut counted = entries();
    counted[0].1 = store.blocks.len() + 1;
    for (entries, why) in [(swapped, "rise by hash"), (counted, "more blocks than")] {
        let file = assemble(structure(), &entries, &pool(), &blocks()).unwrap();
        let error = format!("{:#}", BlockStore::open(&file).err().unwrap());
        assert!(error.contains(why), "{error}");
    }
    let named = (0..store.blocks.len())
        .find_map(|index| store.open_block(index).unwrap().pool_entries().next())
        .map(|entry| store.hashes[entry])
        .unwrap();
    let mut lacking = entries();
    lacking.retain(|(hash, ..)| *hash != named);
    let frames = (store.pool.iter())
        .filter(|frame| frame.entries.len() > 1 || store.hashes[frame.entries[0]] != named)
        .map(|frame| store.raw(frame.frame).unwrap())
        .collect::<Vec<_>>();
    let file = assemble(structure(), &lacking, &frames, &blocks()).unwrap();
    let error = format!(
        "{:#}",
        BlockStore::open(file).unwrap().decode().unwrap_err()
    );
    assert!(error.contains("the pool lacks"), "{error}");
    // A block that names a pool entry of the longest string a store may hold
    // as each of as many properties as it may hold: every copy of a name is
    // charged before it is made, by each read that names its properties.
    let text = "a".repeat(MAX_SETTINGS_STRING_BYTES);
    let hash = digest(&[text.as_bytes()]);
    let mut entry = Vec::new();
    write_var_u64(&mut entry, (text.len() as u64) << 1).unwrap();
    entry.extend_from_slice(text.as_bytes());
    let mut content = vec![1, 1];
    content.extend_from_slice(&hash);
    write_var_u64(&mut content, MAX_SETTINGS_COLLECTION_ITEMS as u64).unwrap();
    content.resize(content.len() + MAX_SETTINGS_COLLECTION_ITEMS + 1, 0);
    let root = in_blocks(&in_rows(0, |_| None), BLOCK_ROWS).unwrap();
    let root = BlockStore::open(&root).unwrap();
    let file = assemble(
        root.raw(root.structure).unwrap(),
        &[(hash, 1, false)],
        &[compressed(&entry).unwrap()],
        &[(1, compressed(&content).unwrap())],
    )
    .unwrap();
    let reads: [&dyn Fn() -> Result<()>; 3] = [
        &|| BlockStore::open(&file)?.decode().map(drop),
        &|| {
            let (mut document, mut columns) = decode_for_row_chunks(&file)?;
            columns.materialize_references(&mut document, &[true])
        },
        &|| {
            let (mut document, columns) = decode_for_row_chunks(&file)?;
            columns.project_properties(&mut document, |_| true, |_, _, value| Some(value))
        },
    ];
    for read in reads {
        let error = format!("{:#}", read().unwrap_err());
        assert!(
            error.contains("than a file of its size may take"),
            "{error}"
        );
    }
    // A pool frame that claims more entries than it holds.
    let mut entry = Vec::new();
    write_var_u64(&mut entry, (POOL_MIN_BYTES as u64) << 1).unwrap();
    entry.extend(std::iter::repeat_n(b'a', POOL_MIN_BYTES));
    let hashes = (0..MAX_SETTINGS_COLLECTION_ITEMS as u32)
        .map(|index| {
            let mut hash = [0; 16];
            hash[12..].copy_from_slice(&index.to_be_bytes());
            (hash, 1, false)
        })
        .collect::<Vec<_>>();
    let file = assemble(
        structure(),
        &hashes,
        &[compressed(&entry).unwrap()],
        &blocks(),
    )
    .unwrap();
    let read = |store: &BlockStore<'_>| store.pool_frame(0).map(drop);
    refused(file, &read, "pool entry");
    // A block whose frame carries no checksum of its content, and a block
    // of two frames.
    let content = store.open_block(0).unwrap().content;
    let unflagged = compress_store_frame(&content, 1, false).unwrap();
    let doubled =
        [&content[..], &[]].map(|content| compress_store_frame(content, 1, true).unwrap());
    for (bytes, why) in [
        (unflagged, "no checksum"),
        (doubled.concat(), "not one zstd frame"),
    ] {
        let mut changed = blocks();
        changed[0].1 = Section {
            decoded: content.len(),
            digest: digest(&[&bytes]),
            bytes: Cow::Owned(bytes),
        };
        let file = assemble(structure(), &entries(), &pool(), &changed).unwrap();
        refused(file, &|store| store.rows(&[0]).map(drop), why);
    }
    // One block more than the structure holds rows for.
    let mut more = blocks();
    more.push((1, store.raw(store.blocks[0].frame).unwrap()));
    let file = assemble(structure(), &entries(), &pool(), &more).unwrap();
    refused(
        file.clone(),
        &|store| store.structure().map(drop),
        "instances and its blocks",
    );
    refused(file, &edit, "instances and its blocks");
    // Frames that claim more than a store may hold in all are refused by the
    // writer and, in a head made by hand, by the reader.
    let past = [(1, claiming(MAX_SETTINGS_BYTECODE_BYTES))];
    assert!(assemble(structure(), &entries(), &pool(), &past).is_err());
    let frame = claiming(MAX_SETTINGS_BYTECODE_BYTES);
    let mut head = SETTINGS_BINARY_MAGIC.to_vec();
    head.push(SETTINGS_BINARY_VERSION);
    for part in writer() {
        write_var_u64(&mut head, part).unwrap();
    }
    let sized = |head: &mut Vec<u8>| {
        write_var_u64(head, frame.decoded as u64).unwrap();
        write_var_u64(head, frame.bytes.len() as u64).unwrap();
        head.extend_from_slice(&frame.digest);
    };
    sized(&mut head);
    for count in [0, 1, 1] {
        write_var_u64(&mut head, count).unwrap();
    }
    sized(&mut head);
    head.extend_from_slice(&digest(&[&head]));
    head.extend_from_slice(&frame.bytes.repeat(2));
    let error = format!("{:#}", BlockStore::open(&head).err().unwrap());
    assert!(error.contains("safe size limit"), "{error}");
}

/// The bytes this writer gives a fixed document. Other bytes are another
/// writer: raise `WRITER_REVISION`, so that neither copies frames of the
/// other's files, then give this test the new digest.
#[test]
fn a_fixed_document_keeps_its_bytes() {
    let payload = |index: usize| {
        let width = POOL_MIN_BYTES + index % 7;
        index
            .is_multiple_of(3)
            .then(|| format!("{:Q>width$}", index % 11))
    };
    let mut document = in_rows(9000, payload);
    let large = [8 << 10, 32 << 10, 128 << 10].into_iter();
    for (index, len) in large.flat_map(|len| [len - 1, len]).enumerate() {
        document.instances[1500 * index]
            .properties
            .insert("Large".into(), json!("L".repeat(len)));
    }
    let bytes = encode_settings_bytecode(&document).unwrap();
    let got = format!("{:032x}", u128::from_be_bytes(digest(&[&bytes])));
    assert_eq!(
        got, "643b44babe9b25b90409c8713050bac2",
        "this writer gives the fixed document other bytes: raise WRITER_REVISION, then set the digest to {got}"
    );
}

/// A flipped bit in a block or a pool frame an edit does not touch fails the
/// edit, as it fails a read: the edit writes nothing, so the file stays as it
/// was.
#[test]
fn an_edit_refuses_a_damaged_block_it_does_not_touch() {
    let payload = format!("{:>2048}", "").replace(' ', "q");
    let document = in_rows(12, |index| (index == 10).then(|| payload.clone()));
    let bytes = in_blocks(&document, 4).unwrap();
    let store = BlockStore::open(&bytes).unwrap();
    let edit = |bytes: &[u8]| {
        edit_store(
            bytes,
            &[1],
            |row| {
                row.properties.insert("Transparency".into(), json!(0.5));
            },
            false,
        )
    };
    assert!(edit(&bytes).is_ok());
    for frame in [store.blocks[2].frame, store.pool[0].frame] {
        let mut damaged = bytes.clone();
        damaged[frame.at + frame.len / 2] ^= 1;
        let error = format!("{:#}", edit(&damaged).unwrap_err());
        assert!(error.contains("does not match its digest"), "{error}");
    }
}

/// Two copies digested together refuse a damaged pool frame that only the
/// rows of a block both hold name, as each digested alone refuses it.
#[test]
fn copies_digested_together_refuse_a_damaged_pool_frame_of_a_block_both_hold() {
    let payload = format!("{:>2048}", "").replace(' ', "q");
    let mut document = in_rows(12, |index| (index == 10).then(|| payload.clone()));
    let files = in_blocks(&document, 4).unwrap();
    let edited = &mut document.instances[1].properties;
    edited.insert("Transparency".into(), json!(0.5));
    let studio = in_blocks(&document, 4).unwrap();
    assert_eq!(blocks_of_both(&files, &studio), 3);
    let digested = |files: &[u8]| super::property_edit::whole_row_digests_of_both(files, &studio).0;
    assert!(digested(&files).is_ok_and(|rows| rows.is_some()));
    let frame = BlockStore::open(&files).unwrap().pool[0].frame;
    let mut damaged = files.clone();
    damaged[frame.at + frame.len / 2] ^= 1;
    let error = format!("{:#}", digested(&damaged).err().unwrap());
    assert!(error.contains("does not match its digest"), "{error}");
}

/// A root and `count` parts under it, each with the payload `payload` gives
/// its place, where it gives one.
fn in_rows(count: usize, payload: impl Fn(usize) -> Option<String>) -> SettingsBytecode {
    let root = SettingsBytecodeInstance::new("root".into(), "Root".into(), "Folder".into(), None);
    let mut instances = vec![root];
    for index in 0..count {
        let mut part = SettingsBytecodeInstance::new(
            format!("part-{index}"),
            format!("Part{index}"),
            "Part".into(),
            Some(0),
        );
        if let Some(text) = payload(index) {
            let value = json!({"_type": "SharedString", "base64": text});
            part.properties.insert("PhysicalConfigData".into(), value);
        }
        instances.push(part);
    }
    SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances,
    }
}

/// The result of the last of three runs of `run`, and the least time a run
/// took, in milliseconds.
fn timed<T>(mut run: impl FnMut() -> T) -> (T, f64) {
    let mut best = f64::MAX;
    let mut last = None;
    for _ in 0..3 {
        drop(last.take());
        let start = Instant::now();
        last = Some(run());
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
    }
    (last.expect("three runs"), best)
}

/// Prints, for v11 and v12, the size of the store `BENCH_STORE` names and the
/// time of: its encode from its document and from a captured state built from
/// that document, the encode of that document over its v12 file, its whole
/// decode, a read of its structure and of one row, an edit of one property of
/// one row (a v11 file is converted by that edit, its first write), and an
/// edit that gives that row a payload no row holds. It checks that both
/// versions give one document each time, that the two v12 encoders write one
/// file and the encode over the file gives it back, and prints counts, sizes
/// and times only.
#[test]
#[ignore = "reads the store BENCH_STORE names"]
fn measure_block_store() -> Result<()> {
    use super::property_edit::{decode_for_property_edit, decode_rows};

    let v11 =
        std::fs::read(std::env::var_os("BENCH_STORE").context("BENCH_STORE names no store")?)?;
    let document = decode_settings_bytecode(&v11)?;
    let (_, v11_encode) = timed(|| encode_settings_bytecode_v11(&document));
    let (v12, v12_encode) = timed(|| encode_settings_bytecode(&document));
    let v12 = v12?;
    let (over, v12_over) = timed(|| encode_settings_bytecode_over(&document, false, Some(&v12)));
    anyhow::ensure!(over? == v12, "the encode over the file changed it");
    let state = crate::settings::bytecode::captured_sim::captured_state(&document, true);
    let row = document.instances.len() / 2;
    let name = document.instances[row]
        .properties
        .iter()
        .find(|(_, value)| value.is_f64())
        .map_or_else(|| "Transparency".to_string(), |(name, _)| name.clone());
    drop(document);
    let (_, v11_state) = timed(|| encode_service_settings_binary_v11(&state));
    let (from_state, v12_state) = timed(|| encode_service_settings_binary(&state));
    let encoders_agree = from_state? == v12;
    drop(state);
    let (whole, v11_decode) = timed(|| decode_settings_bytecode(&v11));
    let (decoded, v12_decode) = timed(|| decode_settings_bytecode(&v12));
    let (mut whole, mut decoded) = (whole?, decoded?);
    (whole.version, decoded.version) = (0, 0);
    anyhow::ensure!(
        same_document(&whole, &decoded),
        "v12 decodes as another document"
    );
    drop((whole, decoded));
    let (_, v11_structure) = timed(|| decode_settings_bytecode_structure(&v11));
    let (_, v12_structure) = timed(|| decode_settings_bytecode_structure(&v12));
    let (_, v11_row) = timed(|| decode_rows(&v11, &[row]));
    let (_, v12_row) = timed(|| decode_rows(&v12, &[row]));
    let edited = |bytes: &[u8], value: &Value| -> Result<Vec<u8>> {
        let (mut edited, mut columns) = decode_for_property_edit(bytes)?;
        columns.materialize(&mut edited, &[row])?;
        let properties = &mut edited.instances[row].properties;
        match value.is_f64() {
            true => properties.insert(name.clone(), value.clone()),
            false => properties.insert("PhysicalConfigData".into(), value.clone()),
        };
        columns.encode(&edited)
    };
    let number = json!(0.375);
    let (converted, first_write) = timed(|| edited(&v11, &number));
    let (edited_once, v12_edit) = timed(|| edited(&v12, &number));
    let (converted, edited_once) = (converted?, edited_once?);
    anyhow::ensure!(
        is_block_store(&converted),
        "the first write left a v11 store"
    );
    anyhow::ensure!(
        converted == edited_once,
        "the converting edit and the v12 edit give other stores"
    );
    let payload = json!({"_type": "SharedString", "base64": "Zm9v".repeat(1024)});
    let (_, payload_edit) = timed(|| edited(&v12, &payload));

    let store = BlockStore::open(&v12)?;
    let block = &store.blocks[store.block_of(row)];
    let counted = |run: &dyn Fn() -> Result<()>| {
        DECOMPRESSED.store(0, Ordering::Relaxed);
        run().map(|()| DECOMPRESSED.load(Ordering::Relaxed))
    };
    let decompressed = [
        counted(&|| BlockStore::open(&v12)?.rows(&[row]).map(drop))?,
        counted(&|| {
            BlockStore::open(&v12)?
                .rows(&block.rows().collect::<Vec<_>>())
                .map(drop)
        })?,
        counted(&|| edited(&v12, &number).map(drop))?,
    ];
    let mut pooled = Vec::new();
    for row in (0..200).map(|index| index * store.rows / 200) {
        let read = counted(&|| BlockStore::open(&v12)?.rows(&[row]).map(drop))?;
        let own = store.blocks[store.block_of(row)].frame.decoded;
        pooled.extend((read > own).then(|| read - own));
    }
    let frames =
        |frames: &mut dyn Iterator<Item = Frame>| frames.map(|frame| frame.len).sum::<usize>();
    println!(
        "{} instances in {} blocks; {} pool entries in {} frames, the largest {} bytes; the encoder of states writes the document's bytes: {encoders_agree}",
        store.rows,
        store.blocks.len(),
        store.hashes.len(),
        store.pool.len(),
        store
            .pool
            .iter()
            .map(|frame| frame.frame.len)
            .max()
            .unwrap_or(0),
    );
    println!(
        "v12 bytes: head {}, structure {}, pool {}, blocks {}",
        store.structure.at,
        store.structure.len,
        frames(&mut store.pool.iter().map(|frame| frame.frame)),
        frames(&mut store.blocks.iter().map(|block| block.frame)),
    );
    println!(
        "bytes decompressed: one row read {}, one block read {}, one-property edit {}",
        decompressed[0], decompressed[1], decompressed[2]
    );
    println!(
        "pool bytes a one-row read decompresses, {} of 200 rows naming an entry: mean {}, max {}",
        pooled.len(),
        pooled.iter().sum::<usize>() / pooled.len().max(1),
        pooled.iter().max().unwrap_or(&0),
    );
    println!("{:<40}{:>12}{:>12}", "", "v11", "v12");
    println!("{:<40}{:>12}{:>12}", "file bytes", v11.len(), v12.len());
    for (label, v11, v12) in [
        ("encode from the document ms", v11_encode, v12_encode),
        ("encode over the unchanged file ms", f64::NAN, v12_over),
        ("encode from a captured state ms", v11_state, v12_state),
        ("whole decode ms", v11_decode, v12_decode),
        ("structure read ms", v11_structure, v12_structure),
        ("one row read ms", v11_row, v12_row),
        (
            "one-property edit ms (v11: first write)",
            first_write,
            v12_edit,
        ),
        ("one-row new payload edit ms", f64::NAN, payload_edit),
    ] {
        println!("{label:<40}{v11:>12.1}{v12:>12.1}");
    }
    Ok(())
}
