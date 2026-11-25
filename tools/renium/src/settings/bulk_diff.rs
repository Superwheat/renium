use std::collections::{BTreeMap, BTreeSet};

use ahash::{AHashMap, AHashSet};
use anyhow::{Context, Result};
use rayon::prelude::*;
use rbx_dom_weak::types::VariantType as RbxVariantType;
use rbx_reflection::{
    DataType as RbxDataType, PropertyKind as RbxPropertyKind,
    PropertySerialization as RbxPropertySerialization, ReflectionDatabase,
    Scriptability as RbxScriptability,
};
use serde_json::{Value, json};

use super::super::captured::UNIQUE_ID;
use super::*;
use crate::editor::types::{
    BulkColumnKind, BulkPackageTarget, BulkPropertyColumn, BulkPropertyValues, BulkTreeNode,
    EditorBulkPropertyPatch,
};
use crate::settings::equivalence::{
    reconciliation_property_is_derived, reconciliation_property_values_equal,
};

type PropertyGroupSlice<'a> = (u8, usize, &'a [u8]);
type BlockPartition = (Vec<(usize, usize)>, Vec<(usize, usize)>);
#[cfg(any(windows, target_os = "macos", test))]
type RawBlockGroup<'a> = (usize, u8, usize, &'a [u8]);

#[derive(Clone)]
enum ExtractedValue {
    Bool(bool),
    Int(i64),
    F64(f64),
    String(String),
    Vec3([f32; 3]),
    Color3([f32; 3]),
    CFrame([f32; 12]),
    EnumItem(String),
    Json(Value),
}

enum BulkEdit {
    Property(usize, String, ExtractedValue),
    Attribute(usize, String, Option<Value>),
    #[cfg(any(windows, target_os = "macos", test))]
    AttributesReplace(usize, serde_json::Map<String, Value>),
    Tag(usize, String, bool),
}

impl BulkEdit {
    fn row(&self) -> usize {
        match self {
            Self::Property(r, _, _) | Self::Attribute(r, _, _) | Self::Tag(r, _, _) => *r,
            #[cfg(any(windows, target_os = "macos", test))]
            Self::AttributesReplace(r, _) => *r,
        }
    }
}

enum BlockResult {
    Fallback,
    Edits(Vec<BulkEdit>),
}

struct BulkDiffContext<'a> {
    desired: &'a BlockStore<'a>,
    observed: &'a BlockStore<'a>,
    desired_rows: &'a [SettingsBytecodeInstance],
    obs_to_des: &'a [usize],
    obs_to_des_opt: Vec<Option<usize>>,
    is_identity_perm: bool,
    is_twin: &'a [bool],
    database: &'a ReflectionDatabase<'a>,
    service: &'a str,
}

pub(crate) enum BulkDiffOutcome {
    Unchanged,
    Patch(EditorBulkPropertyPatch),
    Fallback,
}

struct CachedStoreTopology {
    key: (usize, usize, [u8; 16]),
    rows: Vec<SettingsBytecodeInstance>,
    sibling_ordinals: Vec<u32>,
    package_root_of: Vec<Option<usize>>,
    is_twin: Vec<bool>,
}

type SingleStoreTopology = (Vec<u32>, Vec<Option<usize>>, Vec<bool>);

fn compute_single_store_topology(rows: &[SettingsBytecodeInstance]) -> Option<SingleStoreTopology> {
    let row_count = rows.len();
    let mut sibling_ordinals = vec![1_u32; row_count];
    let mut name_counts: AHashMap<(Option<usize>, &str), u32> = AHashMap::with_capacity(row_count);
    let mut twin_first: AHashMap<(Option<usize>, &str, &str), usize> =
        AHashMap::with_capacity(row_count);
    let mut is_twin = vec![false; row_count];
    let mut is_package_root = vec![false; row_count];
    for (index, row) in rows.iter().enumerate() {
        if index > 0 && row.parent_index.is_none() {
            return None;
        }
        if let Some(parent) = row.parent_index {
            let entry = name_counts
                .entry((Some(parent), row.name.as_str()))
                .or_insert(0);
            *entry += 1;
            sibling_ordinals[index] = *entry;
            if row.class_name == "PackageLink" {
                is_package_root[parent] = true;
            }
        }
        if let Some(first) = twin_first.insert(
            (row.parent_index, row.name.as_str(), row.class_name.as_str()),
            index,
        ) {
            is_twin[first] = true;
            is_twin[index] = true;
        }
    }
    let mut package_root_of = vec![None; row_count];
    let mut visited = vec![false; row_count];
    let mut chain = Vec::with_capacity(64);
    for index in 0..row_count {
        if visited[index] {
            continue;
        }
        chain.clear();
        let mut cursor = Some(index);
        let mut parent_pkg = None;
        let mut parent_twin = false;
        while let Some(node) = cursor {
            if node >= row_count || chain.len() > row_count {
                return None;
            }
            if visited[node] {
                parent_pkg = package_root_of[node];
                parent_twin = is_twin[node];
                break;
            }
            chain.push(node);
            cursor = rows[node].parent_index;
        }
        for &node in chain.iter().rev() {
            if is_package_root[node] {
                parent_pkg = Some(node);
            }
            if is_twin[node] {
                parent_twin = true;
            }
            package_root_of[node] = parent_pkg;
            is_twin[node] = parent_twin;
            visited[node] = true;
        }
    }
    Some((sibling_ordinals, package_root_of, is_twin))
}

fn cached_store_topology(store: &BlockStore<'_>) -> Result<Option<Arc<CachedStoreTopology>>> {
    const MAX_CACHED_TOPOLOGY_ROWS: usize = 300_000;
    static CACHE: std::sync::OnceLock<Mutex<Option<Arc<CachedStoreTopology>>>> =
        std::sync::OnceLock::new();
    let key = (store.rows, store.structure.decoded, store.structure.digest);
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if store.rows <= MAX_CACHED_TOPOLOGY_ROWS {
        let entry = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hit) = entry.as_ref().filter(|e| e.key == key) {
            return Ok(Some(Arc::clone(hit)));
        }
    }
    let rows = store.structure()?.instances;
    if rows.len() != store.rows {
        return Ok(None);
    }
    let Some((sibling_ordinals, package_root_of, is_twin)) = compute_single_store_topology(&rows)
    else {
        return Ok(None);
    };
    let built = Arc::new(CachedStoreTopology {
        key,
        rows,
        sibling_ordinals,
        package_root_of,
        is_twin,
    });
    if store.rows <= MAX_CACHED_TOPOLOGY_ROWS {
        let mut entry = cache.lock().unwrap_or_else(|e| e.into_inner());
        *entry = Some(Arc::clone(&built));
    }
    Ok(Some(built))
}

pub(crate) fn diff_v12_bulk_plain_properties(
    desired_bytes: &[u8],
    observed_bytes: &[u8],
    service: &str,
    min_instances: usize,
) -> Result<BulkDiffOutcome> {
    if !is_block_store(desired_bytes) || !is_block_store(observed_bytes) {
        return Ok(BulkDiffOutcome::Fallback);
    }
    let (Ok(desired), Ok(observed)) = (
        BlockStore::open(desired_bytes),
        BlockStore::open(observed_bytes),
    ) else {
        return Ok(BulkDiffOutcome::Fallback);
    };
    if desired.rows == 0 || desired.rows != observed.rows {
        return Ok(BulkDiffOutcome::Fallback);
    }
    let same_structure_frame = desired.structure.decoded == observed.structure.decoded
        && desired.structure.digest == observed.structure.digest;
    if same_structure_frame {
        let Some(topo) = cached_store_topology(&desired)? else {
            return Ok(BulkDiffOutcome::Fallback);
        };
        if topo.rows[0].parent_index.is_some() || topo.rows[0].class_name != service {
            return Ok(BulkDiffOutcome::Fallback);
        }
        let same_block_layout = desired.blocks.len() == observed.blocks.len()
            && desired
                .blocks
                .iter()
                .zip(&observed.blocks)
                .all(|(d, o)| d.first == o.first && d.count == o.count);
        let identity_map = if !same_block_layout {
            (0..desired.rows).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        return diff_v12_bulk_with_topology(
            &desired,
            &observed,
            &topo.rows,
            &topo.sibling_ordinals,
            &identity_map,
            true,
            &topo.is_twin,
            &topo.package_root_of,
            service,
            min_instances,
        );
    }
    let (desired_struct, observed_struct) =
        rayon::join(|| desired.structure(), || observed.structure());
    let desired_rows = desired_struct?.instances;
    let observed_rows = observed_struct?.instances;
    if !bulk_roots_match(&desired_rows, &observed_rows, desired.rows, service) {
        return Ok(BulkDiffOutcome::Fallback);
    }

    let Some(topology) = build_bulk_topology(&desired_rows, &observed_rows, service) else {
        return Ok(BulkDiffOutcome::Fallback);
    };
    diff_v12_bulk_with_topology(
        &desired,
        &observed,
        &desired_rows,
        &topology.sibling_ordinals,
        &topology.obs_to_des,
        topology.is_identity_perm,
        &topology.is_twin,
        &topology.package_root_of,
        service,
        min_instances,
    )
}

#[allow(clippy::too_many_arguments)]
fn diff_v12_bulk_with_topology(
    desired: &BlockStore<'_>,
    observed: &BlockStore<'_>,
    desired_rows: &[SettingsBytecodeInstance],
    sibling_ordinals: &[u32],
    obs_to_des: &[usize],
    is_identity_perm: bool,
    is_twin: &[bool],
    package_root_of: &[Option<usize>],
    service: &str,
    min_instances: usize,
) -> Result<BulkDiffOutcome> {
    let (aligned_blocks, permuted_pairs) =
        partition_bulk_blocks(desired, observed, obs_to_des, is_identity_perm, service);

    let database = rbx_reflection_database::get()?;
    let obs_to_des_opt = if is_identity_perm {
        Vec::new()
    } else {
        obs_to_des.iter().copied().map(Some).collect()
    };
    let ctx = BulkDiffContext {
        desired,
        observed,
        desired_rows,
        obs_to_des,
        obs_to_des_opt,
        is_identity_perm,
        is_twin,
        database,
        service,
    };
    let block_results = aligned_blocks
        .par_iter()
        .map(|&(d_block_index, o_block_index)| {
            diff_aligned_block(&ctx, d_block_index, o_block_index)
        })
        .collect::<Result<Vec<_>>>()?;

    let mut all_edits = Vec::new();
    for result in block_results {
        match result {
            BlockResult::Fallback => return Ok(BulkDiffOutcome::Fallback),
            BlockResult::Edits(mut block_edits) => all_edits.append(&mut block_edits),
        }
    }
    if !permuted_pairs.is_empty() {
        let Some(mut permuted_edits) = diff_permuted_pairs(&ctx, &permuted_pairs)? else {
            return Ok(BulkDiffOutcome::Fallback);
        };
        all_edits.append(&mut permuted_edits);
    }
    if all_edits.is_empty() {
        return Ok(BulkDiffOutcome::Unchanged);
    }
    Ok(build_bulk_property_patch(
        desired_rows,
        sibling_ordinals,
        package_root_of,
        all_edits,
        database,
        service,
        min_instances,
    ))
}

struct RowColumnHashes {
    h1: Vec<u64>,
    h2: Vec<u64>,
    hx: Vec<u64>,
    count: Vec<u32>,
    has_unique_id: Vec<bool>,
    ambiguous: Vec<bool>,
}

struct RawHashCtx<'a> {
    string_hashes: &'a [(u64, u64)],
    is_settings_id: &'a [bool],
    remap: Option<&'a [usize]>,
}

fn mix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn hash_bytes_128(tag: u64, bytes: &[u8]) -> (u64, u64) {
    let s1 = ahash::RandomState::with_seeds(
        0x9e37_79b9_7f4a_7c15,
        0xbf58_476d_1ce4_e5b9,
        0x94d0_49bb_1331_11eb,
        0x517c_c1b7_2722_0a95,
    );
    let s2 = ahash::RandomState::with_seeds(
        0xd6e8_feb8_6659_fd93,
        0xa076_1d64_78bd_642f,
        0xe703_7ed1_a0b4_28db,
        0x8ebc_6af0_9c88_c6e3,
    );
    (s1.hash_one((tag, bytes)), s2.hash_one((tag, bytes)))
}

fn combine_128(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    (
        mix64(a.0 ^ mix64(b.0.wrapping_add(0x9e37_79b9_7f4a_7c15))),
        mix64(a.1 ^ mix64(b.1.wrapping_add(0xd6e8_feb8_6659_fd93))),
    )
}

fn block_string_hashes(store: &BlockStore<'_>, open: &OpenBlock) -> (Vec<(u64, u64)>, Vec<bool>) {
    let mut hashes = Vec::with_capacity(open.strings.len());
    let mut is_settings_id = Vec::with_capacity(open.strings.len());
    for entry in &open.strings {
        match entry {
            BlockString::Own(text) => {
                hashes.push(hash_bytes_128(0, text.as_bytes()));
                is_settings_id.push(text == "settingsId");
            }
            BlockString::Pool(idx) => {
                hashes.push(hash_bytes_128(1, &store.hashes[*idx]));
                is_settings_id.push(false);
            }
        }
    }
    (hashes, is_settings_id)
}

fn hash_ref_fallback(
    r: &mut BytecodeReader<'_>,
    ambiguous: &mut bool,
) -> Result<Option<(u64, u64)>> {
    let flags = r.read_var_u64().context("Missing ref flags")?;
    if flags & !63 != 0 {
        return Ok(None);
    }
    if flags == 0 {
        return Ok(Some((mix64(10), mix64(0x10_u64))));
    }
    if flags & 1 != 0 {
        let _ = r.read_len("ref instance id")?;
    }
    if flags & 2 != 0 {
        let _ = r.read_len("ref debug id")?;
    }
    if flags & 4 != 0 {
        let len = r.read_collection_len("ref path segment count")?;
        for _ in 0..len {
            let _ = r.read_len("ref path segment")?;
        }
    }
    if flags & 8 != 0 {
        let _ = r.read_len("ref path")?;
    }
    if flags & 16 != 0 {
        let _ = r.read_len("ref settings id")?;
    }
    if flags & 32 != 0 {
        let len = r.read_collection_len("ref path ordinal count")?;
        for _ in 0..len {
            let _ = r.read_var_u64().context("Missing ref path ordinal")?;
        }
    }
    *ambiguous = true;
    Ok(Some((0, 0)))
}

fn hash_raw_value(
    ctx: &RawHashCtx<'_>,
    kind: u8,
    r: &mut BytecodeReader<'_>,
    depth: usize,
    ambiguous: &mut bool,
) -> Result<Option<(u64, u64)>> {
    if depth > 64 {
        return Ok(None);
    }
    let k = u64::from(kind);
    let value = match kind {
        0..=2 => (mix64(k), mix64(k.wrapping_add(1))),
        3 | 4 => {
            let v = r.read_var_u64()?;
            (mix64(k ^ v), mix64(v.rotate_left(13) ^ k))
        }
        5 => {
            let v = u64::from_le_bytes(r.read_bytes(8)?.try_into().unwrap());
            (mix64(k ^ v), mix64(v.rotate_left(19) ^ k))
        }
        20 => {
            let v = u64::from(u32::from_le_bytes(r.read_bytes(4)?.try_into().unwrap()));
            (mix64(k ^ v), mix64(v.rotate_left(11) ^ k))
        }
        6 | 19 => {
            let id = r.read_len("string id")?;
            let Some(&sh) = ctx.string_hashes.get(id) else {
                return Ok(None);
            };
            combine_128((k, k.rotate_left(7)), sh)
        }
        NESTED_ENUM_ITEM_KIND => {
            let ta = r.read_len("enum type string id")?;
            let na = r.read_len("enum name string id")?;
            let (Some(&th), Some(&nh)) = (ctx.string_hashes.get(ta), ctx.string_hashes.get(na))
            else {
                return Ok(None);
            };
            combine_128(combine_128((k, 0), th), nh)
        }
        7 => {
            let len = r.read_collection_len("array length")?;
            let mut acc = (mix64(k ^ len as u64), mix64(len as u64));
            for _ in 0..len {
                let item_kind = r.read_u8().context("Missing array item kind")?;
                let Some(item_hash) = hash_raw_value(ctx, item_kind, r, depth + 1, ambiguous)?
                else {
                    return Ok(None);
                };
                acc = combine_128(acc, item_hash);
            }
            acc
        }
        8 => {
            let len = r.read_collection_len("object length")?;
            let mut h1 = mix64(k ^ len as u64);
            let mut h2 = mix64(len as u64 ^ 0x517c_c1b7_2722_0a95);
            for _ in 0..len {
                let key_id = r.read_len("object key string id")?;
                let Some(&kh) = ctx.string_hashes.get(key_id) else {
                    return Ok(None);
                };
                if ctx.is_settings_id.get(key_id).copied().unwrap_or(false) {
                    *ambiguous = true;
                }
                let val_kind = r.read_u8().context("Missing object value kind")?;
                let Some(vh) = hash_raw_value(ctx, val_kind, r, depth + 1, ambiguous)? else {
                    return Ok(None);
                };
                let entry = combine_128(kh, vh);
                h1 = h1.wrapping_add(entry.0);
                h2 = h2.wrapping_add(entry.1);
            }
            (mix64(h1), mix64(h2))
        }
        9 => {
            let len = r.read_len("numeric array length")?;
            hash_bytes_128(k, r.read_bytes(len.saturating_mul(8))?)
        }
        10 => {
            let Some(vh) = hash_ref_fallback(r, ambiguous)? else {
                return Ok(None);
            };
            vh
        }
        21 => {
            let len = r.read_len("f32 numeric array length")?;
            hash_bytes_128(k, r.read_bytes(len.saturating_mul(4))?)
        }
        11 | 13 => hash_bytes_128(k, r.read_bytes(8)?),
        12 | 15 => hash_bytes_128(k, r.read_bytes(12)?),
        14 | 18 => hash_bytes_128(k, r.read_bytes(16)?),
        16 => hash_bytes_128(k, r.read_bytes(48)?),
        17 => {
            let ref_idx = r.read_len("resolved ref index")?;
            let mapped = match ctx.remap {
                Some(map) => match map.get(ref_idx).copied() {
                    Some(v) => v,
                    None => {
                        *ambiguous = true;
                        ref_idx
                    }
                },
                None => ref_idx,
            };
            (
                mix64(k ^ mapped as u64),
                mix64((mapped as u64).rotate_left(23) ^ k),
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn read_cframe_group_rotations(
    kind: u8,
    gr: &mut BytecodeReader<'_>,
) -> Result<Option<Vec<[u8; 36]>>> {
    if kind != 16 {
        return Ok(None);
    }
    let mode = gr.read_u8().context("Missing CFrame group codec")?;
    match mode {
        0 => Ok(None),
        1 => {
            let rotation_count =
                checked_cframe_rotation_count(gr.read_len("CFrame rotation table count")?)?;
            let mut rotations = Vec::with_capacity(gr.capacity(rotation_count, 36));
            for _ in 0..rotation_count {
                let bytes = gr.read_bytes(36)?;
                rotations.push(bytes.try_into().unwrap());
            }
            Ok(Some(rotations))
        }
        _ => bail!("Unknown CFrame group codec {mode}"),
    }
}

fn hash_block_columns(
    store: &BlockStore<'_>,
    block_index: usize,
    remap: Option<&[usize]>,
) -> Result<RowColumnHashes> {
    let block = &store.blocks[block_index];
    let count = block.count;
    let mut out = RowColumnHashes {
        h1: vec![0; count],
        h2: vec![0; count],
        hx: vec![0; count],
        count: vec![0; count],
        has_unique_id: vec![false; count],
        ambiguous: vec![false; count],
    };
    let open = store.open_block(block_index)?;
    let prop_names = block_property_names(store, &open)?;
    let (string_hashes, is_settings_id) = block_string_hashes(store, &open);
    let ctx = RawHashCtx {
        string_hashes: &string_hashes,
        is_settings_id: &is_settings_id,
        remap,
    };
    let mut reader = BytecodeReader::new(&open.content[open.rest..]);
    let specs = read_settings_property_groups(&mut reader, &prop_names)?;
    reader.finish()?;

    for (prop_name, kind, group_count, body) in specs {
        let is_unique_id = prop_name == UNIQUE_ID;
        let prop_hash = if is_unique_id {
            (0, 0)
        } else {
            hash_bytes_128(0x7072_6f70, prop_name.as_bytes())
        };
        let mut gr = BytecodeReader::new(body);
        let cframe_rotations = read_cframe_group_rotations(kind, &mut gr)?;
        let mut local = 0_usize;
        let mut prev_ref = 0_usize;
        let mut group_failed = false;
        for pos in 0..group_count {
            local = next_group_instance(local, gr.read_len("property instance index")?, pos)?;
            anyhow::ensure!(
                local < count,
                "A value block gives a value to a row it does not hold"
            );
            let mut row_ambiguous = false;
            let val_hash = if kind == 17 {
                let ref_idx = decode_resolved_ref_target(&mut gr, store.rows, &mut prev_ref)?;
                let mapped = match remap {
                    Some(map) => match map.get(ref_idx).copied() {
                        Some(v) => v,
                        None => {
                            row_ambiguous = true;
                            ref_idx
                        }
                    },
                    None => ref_idx,
                };
                Some((
                    mix64(17 ^ mapped as u64),
                    mix64((mapped as u64).rotate_left(23) ^ 17),
                ))
            } else if let Some(rotations) = cframe_rotations.as_deref() {
                let pos_bytes = gr.read_bytes(12)?;
                let rot_index = gr.read_len("CFrame rotation index")?;
                let rot_bytes = rotations
                    .get(rot_index)
                    .with_context(|| format!("Invalid CFrame rotation index {rot_index}"))?;
                let mut full = [0u8; 48];
                full[..12].copy_from_slice(pos_bytes);
                full[12..].copy_from_slice(rot_bytes);
                Some(hash_bytes_128(16, &full))
            } else {
                hash_raw_value(&ctx, kind, &mut gr, 0, &mut row_ambiguous)?
            };
            let Some(vh) = val_hash else {
                group_failed = true;
                break;
            };
            if is_unique_id {
                out.has_unique_id[local] = true;
                continue;
            }
            if row_ambiguous {
                out.ambiguous[local] = true;
            }
            let entry = combine_128(prop_hash, vh);
            out.h1[local] = out.h1[local].wrapping_add(entry.0);
            out.h2[local] = out.h2[local].wrapping_add(entry.1);
            out.hx[local] ^= entry.0.rotate_left(17) ^ entry.1;
            out.count[local] = out.count[local].saturating_add(1);
        }
        if group_failed {
            out.ambiguous.fill(true);
        } else {
            gr.finish()?;
        }
    }
    Ok(out)
}

fn hash_store_rows(store: &BlockStore<'_>, remap: Option<&[usize]>) -> Result<RowColumnHashes> {
    let block_hashes = (0..store.blocks.len())
        .into_par_iter()
        .map(|block_idx| hash_block_columns(store, block_idx, remap))
        .collect::<Result<Vec<_>>>()?;
    let mut out = RowColumnHashes {
        h1: Vec::with_capacity(store.rows),
        h2: Vec::with_capacity(store.rows),
        hx: Vec::with_capacity(store.rows),
        count: Vec::with_capacity(store.rows),
        has_unique_id: Vec::with_capacity(store.rows),
        ambiguous: Vec::with_capacity(store.rows),
    };
    for mut part in block_hashes {
        out.h1.append(&mut part.h1);
        out.h2.append(&mut part.h2);
        out.hx.append(&mut part.hx);
        out.count.append(&mut part.count);
        out.has_unique_id.append(&mut part.has_unique_id);
        out.ambiguous.append(&mut part.ambiguous);
    }
    Ok(out)
}

pub(crate) fn matching_rows_by_column_hash_stores(
    stores: (&BlockStore<'_>, &BlockStore<'_>),
    rows: (&[SettingsBytecodeInstance], &[SettingsBytecodeInstance]),
    mapping: (&[usize], &[usize], &[bool]),
    protected: Option<usize>,
    pending_rows: &[usize],
) -> Result<Option<Vec<bool>>> {
    let (desired, observed) = stores;
    let (desired_rows, observed_rows) = rows;
    let (places, origins, by_order) = mapping;
    let row_count = desired.rows;
    if row_count == 0 || row_count != observed.rows || row_count != desired_rows.len() {
        return Ok(None);
    }
    let is_identity_perm = places.iter().enumerate().all(|(i, &p)| p == i);
    let remap = (!is_identity_perm).then_some(origins);
    let (des, obs) = rayon::join(
        || hash_store_rows(desired, None),
        || hash_store_rows(observed, remap),
    );
    let (des, obs) = (des?, obs?);
    let mut pending_mask = vec![false; row_count];
    for &row in pending_rows {
        if row < row_count {
            pending_mask[row] = true;
        }
    }
    let mut matched = vec![false; row_count];
    let mut any = false;
    for d in 0..row_count {
        let o = places[d];
        let d_row = &desired_rows[d];
        let o_row = &observed_rows[o];
        let ok = d_row.parent_index.is_some()
            && o_row.parent_index.is_some()
            && protected != Some(o)
            && !pending_mask[d]
            && !des.ambiguous[d]
            && !obs.ambiguous[o]
            && d_row.name == o_row.name
            && d_row.class_name == o_row.class_name
            && d_row.parent_index == o_row.parent_index.map(|p| origins[p])
            && !(by_order[d] && obs.has_unique_id[o] && !des.has_unique_id[d])
            && des.count[d] == obs.count[o]
            && des.h1[d] == obs.h1[o]
            && des.h2[d] == obs.h2[o]
            && des.hx[d] == obs.hx[o];
        if ok {
            matched[d] = true;
            any = true;
        }
    }
    Ok(any.then_some(matched))
}

fn bulk_roots_match(
    desired_rows: &[SettingsBytecodeInstance],
    observed_rows: &[SettingsBytecodeInstance],
    expected_rows: usize,
    service: &str,
) -> bool {
    desired_rows.len() == expected_rows
        && observed_rows.len() == expected_rows
        && desired_rows[0].parent_index.is_none()
        && desired_rows[0].class_name == service
        && observed_rows[0].parent_index.is_none()
        && observed_rows[0].class_name == service
}

struct BulkTopology {
    sibling_ordinals: Vec<u32>,
    obs_to_des: Vec<usize>,
    is_identity_perm: bool,
    is_twin: Vec<bool>,
    package_root_of: Vec<Option<usize>>,
}

fn build_bulk_topology(
    desired_rows: &[SettingsBytecodeInstance],
    observed_rows: &[SettingsBytecodeInstance],
    service: &str,
) -> Option<BulkTopology> {
    let row_count = desired_rows.len();
    let mut sibling_ordinals = vec![1_u32; row_count];
    let mut name_counts: AHashMap<(Option<usize>, &str), u32> = AHashMap::with_capacity(row_count);
    let mut twin_first: AHashMap<(Option<usize>, &str, &str), usize> =
        AHashMap::with_capacity(row_count);
    let mut is_twin = vec![false; row_count];
    let mut is_package_root = vec![false; row_count];
    let mut is_identity_perm = true;

    for (index, (d, o)) in desired_rows.iter().zip(observed_rows).enumerate() {
        if index > 0 && (d.parent_index.is_none() || o.parent_index.is_none()) {
            return None;
        }
        if is_identity_perm
            && (d.name != o.name
                || d.class_name != o.class_name
                || d.parent_index != o.parent_index)
        {
            is_identity_perm = false;
        }
        if let Some(parent) = d.parent_index {
            let entry = name_counts
                .entry((Some(parent), d.name.as_str()))
                .or_insert(0);
            *entry += 1;
            sibling_ordinals[index] = *entry;
            if d.class_name == "PackageLink" {
                is_package_root[parent] = true;
            }
        }
        if let Some(first) = twin_first.insert(
            (d.parent_index, d.name.as_str(), d.class_name.as_str()),
            index,
        ) {
            is_twin[first] = true;
            is_twin[index] = true;
        }
    }

    let obs_to_des = if is_identity_perm {
        (0..row_count).collect()
    } else {
        match match_permuted_tree(desired_rows, &sibling_ordinals, observed_rows) {
            Some(mapping) => mapping,
            None => {
                crate::app::output::log_global(
                    4,
                    format_args!(
                        "[renium] bulk diff fallback ({service}): permuted tree mismatch across {row_count} rows"
                    ),
                );
                return None;
            }
        }
    };

    let mut package_root_of = vec![None; row_count];
    let mut visited = vec![false; row_count];
    let mut chain = Vec::with_capacity(64);
    for index in 0..row_count {
        if visited[index] {
            continue;
        }
        chain.clear();
        let mut cursor = Some(index);
        let mut parent_pkg = None;
        let mut parent_twin = false;
        while let Some(node) = cursor {
            if node >= row_count || chain.len() > row_count {
                return None;
            }
            if visited[node] {
                parent_pkg = package_root_of[node];
                parent_twin = is_twin[node];
                break;
            }
            chain.push(node);
            cursor = desired_rows[node].parent_index;
        }
        for &node in chain.iter().rev() {
            if is_package_root[node] {
                parent_pkg = Some(node);
            }
            if is_twin[node] {
                parent_twin = true;
            }
            package_root_of[node] = parent_pkg;
            is_twin[node] = parent_twin;
            visited[node] = true;
        }
    }

    Some(BulkTopology {
        sibling_ordinals,
        obs_to_des,
        is_identity_perm,
        is_twin,
        package_root_of,
    })
}

fn partition_bulk_blocks(
    desired: &BlockStore<'_>,
    observed: &BlockStore<'_>,
    obs_to_des: &[usize],
    is_identity_perm: bool,
    service: &str,
) -> BlockPartition {
    let same_block_layout = desired.blocks.len() == observed.blocks.len()
        && desired
            .blocks
            .iter()
            .zip(&observed.blocks)
            .all(|(d, o)| d.first == o.first && d.count == o.count);

    let des_blocks_by_span: AHashMap<(usize, usize), usize> = if same_block_layout {
        AHashMap::new()
    } else {
        desired
            .blocks
            .iter()
            .enumerate()
            .map(|(idx, b)| ((b.first, b.count), idx))
            .collect()
    };

    let mut aligned_blocks = Vec::new();
    let mut permuted_pairs = Vec::new();
    if is_identity_perm && same_block_layout {
        aligned_blocks.extend((0..observed.blocks.len()).map(|i| (i, i)));
    } else {
        for (o_block_index, block) in observed.blocks.iter().enumerate() {
            let d_block_index = if same_block_layout {
                Some(o_block_index)
            } else {
                des_blocks_by_span.get(&(block.first, block.count)).copied()
            };
            let slice = &obs_to_des[block.first..block.first + block.count];
            let aligned = d_block_index.is_some()
                && (is_identity_perm
                    || slice
                        .iter()
                        .enumerate()
                        .all(|(offset, &d)| d == block.first + offset));
            if let (Some(d_idx), true) = (d_block_index, aligned) {
                aligned_blocks.push((d_idx, o_block_index));
            } else {
                for (offset, &d) in slice.iter().enumerate() {
                    permuted_pairs.push((d, block.first + offset));
                }
            }
        }
    }
    crate::app::output::log_global(
        5,
        format_args!(
            "[renium] bulk diff layout ({service}): identity={is_identity_perm} same_blocks={same_block_layout} aligned_blocks={} permuted_rows={}",
            aligned_blocks.len(),
            permuted_pairs.len()
        ),
    );
    (aligned_blocks, permuted_pairs)
}

fn diff_aligned_block(
    ctx: &BulkDiffContext<'_>,
    d_block_index: usize,
    o_block_index: usize,
) -> Result<BlockResult> {
    let d_block = &ctx.desired.blocks[d_block_index];
    let o_block = &ctx.observed.blocks[o_block_index];
    if ctx.is_identity_perm
        && d_block.frame.digest == o_block.frame.digest
        && ctx.desired.bytes[d_block.frame.at..d_block.frame.at + d_block.frame.len]
            == ctx.observed.bytes[o_block.frame.at..o_block.frame.at + o_block.frame.len]
    {
        return Ok(BlockResult::Edits(Vec::new()));
    }
    let open_d = ctx.desired.open_block(d_block_index)?;
    let open_o = ctx.observed.open_block(o_block_index)?;
    let d_prop_names = block_property_names(ctx.desired, &open_d)?;
    let o_prop_names = block_property_names(ctx.observed, &open_o)?;
    let mut d_reader = BytecodeReader::new(&open_d.content[open_d.rest..]);
    let d_specs = read_settings_property_groups(&mut d_reader, &d_prop_names)?;
    d_reader.finish()?;
    let mut o_reader = BytecodeReader::new(&open_o.content[open_o.rest..]);
    let o_specs = read_settings_property_groups(&mut o_reader, &o_prop_names)?;
    o_reader.finish()?;

    let mut d_by_name: AHashMap<&str, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    for (name, kind, count, body) in d_specs {
        d_by_name.entry(name).or_default().push((kind, count, body));
    }
    let mut o_by_name: AHashMap<&str, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    for (name, kind, count, body) in o_specs {
        o_by_name.entry(name).or_default().push((kind, count, body));
    }

    let mut all_names = AHashSet::with_capacity(d_by_name.len() + o_by_name.len());
    all_names.extend(d_by_name.keys().copied());
    all_names.extend(o_by_name.keys().copied());

    let block_first = d_block.first;
    let block_count = d_block.count;
    let mut edits = Vec::new();
    let mut eligible_cache: AHashMap<(&str, &str), bool> = AHashMap::new();
    let mut d_strings_cache: Option<Vec<String>> = None;
    let mut o_strings_cache: Option<Vec<String>> = None;

    for prop_name in all_names {
        let d_groups = d_by_name.get(prop_name).map(Vec::as_slice).unwrap_or(&[]);
        let o_groups = o_by_name.get(prop_name).map(Vec::as_slice).unwrap_or(&[]);

        if groups_equal_unpooled(ctx, &open_d, &open_o, d_groups, o_groups)? {
            continue;
        }
        if prop_name == UNIQUE_ID {
            let d_vals = decode_block_property(
                ctx.desired,
                &open_d,
                &mut d_strings_cache,
                prop_name,
                d_groups,
                block_count,
            )?;
            let o_vals = decode_block_property(
                ctx.observed,
                &open_o,
                &mut o_strings_cache,
                prop_name,
                o_groups,
                block_count,
            )?;
            if !unique_ids_safe_for_positional(&d_vals, &o_vals, block_first, ctx.is_twin) {
                return Ok(BlockResult::Fallback);
            }
            continue;
        }
        if prop_name == "CurrentCamera" && ctx.service == "Workspace" && block_first == 0 {
            let d_vals = decode_block_property(
                ctx.desired,
                &open_d,
                &mut d_strings_cache,
                prop_name,
                d_groups,
                block_count,
            )?;
            let mut o_vals = decode_block_property(
                ctx.observed,
                &open_o,
                &mut o_strings_cache,
                prop_name,
                o_groups,
                block_count,
            )?;
            if !ctx.is_identity_perm
                && let Some(val) = o_vals.get_mut(&0)
            {
                remap_ref_indices_in_value(val, &ctx.obs_to_des_opt);
            }
            if d_vals.get(&0) != o_vals.get(&0)
                && d_vals.contains_key(&0)
                && o_vals.contains_key(&0)
            {
                return Ok(BlockResult::Fallback);
            }
            continue;
        }
        if prop_name == "Attributes" {
            let d_attrs = decode_block_attributes(
                ctx.desired,
                &open_d,
                &mut d_strings_cache,
                d_groups,
                block_count,
            )?;
            let o_attrs = decode_block_attributes(
                ctx.observed,
                &open_o,
                &mut o_strings_cache,
                o_groups,
                block_count,
            )?;
            if !diff_aligned_attributes_column(ctx, &d_attrs, &o_attrs, block_first, &mut edits) {
                return Ok(BlockResult::Fallback);
            }
            continue;
        }
        if prop_name == "Tags" {
            let d_vals = decode_block_property(
                ctx.desired,
                &open_d,
                &mut d_strings_cache,
                prop_name,
                d_groups,
                block_count,
            )?;
            let o_vals = decode_block_property(
                ctx.observed,
                &open_o,
                &mut o_strings_cache,
                prop_name,
                o_groups,
                block_count,
            )?;
            if !diff_aligned_tags_column(ctx, &d_vals, &o_vals, block_first, &mut edits) {
                return Ok(BlockResult::Fallback);
            }
            continue;
        }
        if matches!(
            prop_name,
            "Source" | "LinkedSource" | "Name" | "Parent" | "ClassName" | "Archivable"
        ) {
            return Ok(BlockResult::Fallback);
        }

        if !reconciliation_property_is_derived(prop_name)
            && let (Some(d_raw), Some(o_raw)) = (
                decode_block_raw_extracted_opt(d_groups, &open_d, block_count)?,
                decode_block_raw_extracted_opt(o_groups, &open_o, block_count)?,
            )
        {
            if !diff_aligned_raw_column(
                ctx,
                prop_name,
                &d_raw,
                &o_raw,
                block_first,
                &mut eligible_cache,
                &mut edits,
            ) {
                return Ok(BlockResult::Fallback);
            }
            continue;
        }

        let d_vals = decode_block_property(
            ctx.desired,
            &open_d,
            &mut d_strings_cache,
            prop_name,
            d_groups,
            block_count,
        )?;
        let mut o_vals = decode_block_property(
            ctx.observed,
            &open_o,
            &mut o_strings_cache,
            prop_name,
            o_groups,
            block_count,
        )?;
        if !ctx.is_identity_perm {
            for val in o_vals.values_mut() {
                remap_ref_indices_in_value(val, &ctx.obs_to_des_opt);
            }
        }
        if !diff_aligned_decoded_column(
            ctx,
            prop_name,
            &d_vals,
            &o_vals,
            block_first,
            &mut eligible_cache,
            &mut edits,
        ) {
            return Ok(BlockResult::Fallback);
        }
    }

    Ok(BlockResult::Edits(edits))
}

fn groups_equal_unpooled(
    ctx: &BulkDiffContext<'_>,
    open_d: &OpenBlock,
    open_o: &OpenBlock,
    d_groups: &[PropertyGroupSlice<'_>],
    o_groups: &[PropertyGroupSlice<'_>],
) -> Result<bool> {
    if d_groups.is_empty() || d_groups.len() != o_groups.len() {
        return Ok(false);
    }
    let remap = (!ctx.is_identity_perm).then_some(ctx.obs_to_des);
    for (&(dk, dc, db), &(ok, oc, ob)) in d_groups.iter().zip(o_groups.iter()) {
        if dk != ok
            || dc != oc
            || !group_bodies_equal_unpooled(
                dk,
                dc,
                db,
                ob,
                ctx.desired,
                open_d,
                ctx.observed,
                open_o,
                remap,
            )?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn is_engine_managed_attribute(name: &str) -> bool {
    name.starts_with("RBX_")
}

fn scalar_floats_match(a: f64, b: f64, is_f32: bool) -> bool {
    if is_f32 {
        crate::settings::equivalence::f32_values_match(a, b)
    } else {
        a == b
    }
}

fn extracted_values_match(
    a: &ExtractedValue,
    b: &ExtractedValue,
    variant_type: Option<RbxVariantType>,
    is_byte_color: bool,
) -> Option<bool> {
    let is_f32 = variant_type == Some(RbxVariantType::Float32);
    match (a, b) {
        (ExtractedValue::Bool(x), ExtractedValue::Bool(y)) => Some(x == y),
        (ExtractedValue::Int(x), ExtractedValue::Int(y)) => Some(x == y),
        (ExtractedValue::F64(x), ExtractedValue::F64(y)) => {
            Some(scalar_floats_match(*x, *y, is_f32))
        }
        (ExtractedValue::Int(x), ExtractedValue::F64(y))
        | (ExtractedValue::F64(y), ExtractedValue::Int(x)) => {
            if is_f32 {
                Some(crate::settings::equivalence::f32_values_match(
                    *x as f64, *y,
                ))
            } else {
                let exact = y.is_finite()
                    && y.fract() == 0.0
                    && *y >= (i64::MIN as f64)
                    && *y < 9_223_372_036_854_775_808.0_f64
                    && (*y as i64) == *x;
                Some(exact)
            }
        }
        (ExtractedValue::String(x), ExtractedValue::String(y)) => (x == y).then_some(true),
        (ExtractedValue::EnumItem(x), ExtractedValue::EnumItem(y)) => (x == y).then_some(true),
        (ExtractedValue::Vec3(x), ExtractedValue::Vec3(y)) => {
            Some(x.iter().zip(y).all(|(u, v)| {
                crate::settings::equivalence::f32_values_match(f64::from(*u), f64::from(*v))
            }))
        }
        (ExtractedValue::Color3(x), ExtractedValue::Color3(y)) => {
            if is_byte_color {
                let to_byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
                Some(x.iter().zip(y).all(|(u, v)| to_byte(*u) == to_byte(*v)))
            } else {
                Some(x.iter().zip(y).all(|(u, v)| {
                    crate::settings::equivalence::f32_values_match(f64::from(*u), f64::from(*v))
                }))
            }
        }
        (ExtractedValue::CFrame(x), ExtractedValue::CFrame(y)) => {
            let xa = x.map(f64::from);
            let yb = y.map(f64::from);
            Some(crate::settings::equivalence::cframes_match_reconciliation(
                &xa, &yb,
            ))
        }
        _ => None,
    }
}

fn cached_default_extracted(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    prop_name: &str,
) -> Option<Option<ExtractedValue>> {
    if crate::settings::equivalence::reconciliation_property_is_unknown_when_absent(
        Some(database),
        class_name,
        prop_name,
    ) {
        return Some(None);
    }
    let default_val = reflection_default_json(database, class_name, prop_name)?;
    if !reconciliation_property_values_equal(class_name, prop_name, None, Some(&default_val)) {
        return None;
    }
    Some(Some(ExtractedValue::from_json(default_val)))
}

fn diff_aligned_raw_column<'a>(
    ctx: &'a BulkDiffContext<'a>,
    prop_name: &'a str,
    d_raw: &[(usize, ExtractedValue)],
    o_raw: &[(usize, ExtractedValue)],
    block_first: usize,
    eligible_cache: &mut AHashMap<(&'a str, &'a str), bool>,
    edits: &mut Vec<BulkEdit>,
) -> bool {
    let mut default_cache: AHashMap<&str, Option<Option<ExtractedValue>>> = AHashMap::new();
    let mut vtype_cache: AHashMap<&str, (Option<RbxVariantType>, bool)> = AHashMap::new();
    let mut di = 0_usize;
    let mut oi = 0_usize;

    while di < d_raw.len() || oi < o_raw.len() {
        let (local_row, d_val, o_val) =
            if di < d_raw.len() && (oi >= o_raw.len() || d_raw[di].0 < o_raw[oi].0) {
                let (r, ref v) = d_raw[di];
                di += 1;
                (r, Some(v), None)
            } else if oi < o_raw.len() && (di >= d_raw.len() || o_raw[oi].0 < d_raw[di].0) {
                let (r, ref v) = o_raw[oi];
                oi += 1;
                (r, None, Some(v))
            } else {
                let (r, ref dv) = d_raw[di];
                let (_, ref ov) = o_raw[oi];
                di += 1;
                oi += 1;
                (r, Some(dv), Some(ov))
            };
        let global_row = block_first + local_row;
        let class_name = ctx.desired_rows[global_row].class_name.as_str();
        let (variant_type, is_byte_color) = *vtype_cache.entry(class_name).or_insert_with(|| {
            let vt = match crate::rbx::encode::rbx_model_property_descriptor(
                ctx.database,
                class_name,
                prop_name,
            )
            .map(|d| &d.data_type)
            {
                Some(RbxDataType::Value(vt)) => Some(*vt),
                _ => None,
            };
            let byte_color = crate::rbx::encode::rbx_model_property_descriptor(
                ctx.database,
                class_name,
                prop_name,
            )
            .is_some_and(|d| {
                matches!(d.data_type, RbxDataType::Value(RbxVariantType::Color3uint8))
            });
            (vt, byte_color)
        });
        if let (Some(dv), Some(ov)) = (d_val, o_val) {
            match extracted_values_match(dv, ov, variant_type, is_byte_color) {
                Some(true) => continue,
                Some(false) => {}
                None => {
                    let dj = dv.clone().into_json();
                    let oj = ov.clone().into_json();
                    if reconciliation_property_values_equal(
                        class_name,
                        prop_name,
                        Some(&dj),
                        Some(&oj),
                    ) {
                        continue;
                    }
                }
            }
        }
        let target_val = match d_val {
            Some(dv) => {
                if o_val.is_none() {
                    let def_opt = default_cache.entry(class_name).or_insert_with(|| {
                        cached_default_extracted(ctx.database, class_name, prop_name)
                    });
                    if let Some(Some(def)) = def_opt
                        && extracted_values_match(dv, def, variant_type, is_byte_color)
                            == Some(true)
                    {
                        continue;
                    }
                }
                dv.clone()
            }
            None => {
                let def_opt = default_cache.entry(class_name).or_insert_with(|| {
                    cached_default_extracted(ctx.database, class_name, prop_name)
                });
                match def_opt {
                    None => return false,
                    Some(None) => continue,
                    Some(Some(def)) => {
                        if let Some(ov) = o_val
                            && extracted_values_match(def, ov, variant_type, is_byte_color)
                                == Some(true)
                        {
                            continue;
                        }
                        def.clone()
                    }
                }
            }
        };
        if class_name == "PackageLink" {
            continue;
        }
        if class_name == "Camera" {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): raw protected class={class_name} prop={prop_name}",
                    ctx.service
                ),
            );
            return false;
        }
        let class_prop_eligible = *eligible_cache
            .entry((class_name, prop_name))
            .or_insert_with(|| {
                is_bulk_plain_property_class_eligible(
                    ctx.database,
                    ctx.service,
                    class_name,
                    prop_name,
                )
            });
        if !class_prop_eligible {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): raw ineligible class={class_name} prop={prop_name}",
                    ctx.service
                ),
            );
            return false;
        }
        edits.push(BulkEdit::Property(
            global_row,
            prop_name.to_string(),
            target_val,
        ));
    }
    true
}

fn is_bulk_attribute_value_eligible(value: &Value) -> bool {
    !holds_marker(value) && is_bulk_plain_value_eligible(value)
}

fn diff_aligned_attributes_column(
    ctx: &BulkDiffContext<'_>,
    d_attrs: &AHashMap<usize, serde_json::Map<String, Value>>,
    o_attrs: &AHashMap<usize, serde_json::Map<String, Value>>,
    block_first: usize,
    edits: &mut Vec<BulkEdit>,
) -> bool {
    let mut touched_local: Vec<usize> = Vec::with_capacity(d_attrs.len().max(o_attrs.len()));
    touched_local.extend(d_attrs.keys().copied());
    for &k in o_attrs.keys() {
        if !d_attrs.contains_key(&k) {
            touched_local.push(k);
        }
    }
    touched_local.sort_unstable();
    for local_row in touched_local {
        let global_row = block_first + local_row;
        let row = &ctx.desired_rows[global_row];
        let d_map = d_attrs.get(&local_row);
        let o_map = o_attrs.get(&local_row);
        if d_map == o_map {
            continue;
        }
        if row.class_name == "PackageLink" {
            continue;
        }
        if row.class_name == "Camera" || row.name == "__Renium_SessionLock" {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): attr protected class={} name={}",
                    ctx.service, row.class_name, row.name
                ),
            );
            return false;
        }
        if !append_attribute_map_diffs(global_row, d_map, o_map, edits) {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): attr value ineligible class={}",
                    ctx.service, row.class_name
                ),
            );
            return false;
        }
    }
    true
}

fn append_attribute_map_diffs(
    global_row: usize,
    d_map: Option<&serde_json::Map<String, Value>>,
    o_map: Option<&serde_json::Map<String, Value>>,
    edits: &mut Vec<BulkEdit>,
) -> bool {
    if let Some(dm) = d_map {
        for (key, dv) in dm {
            if is_engine_managed_attribute(key) {
                continue;
            }
            let ov = o_map.and_then(|om| om.get(key));
            if ov == Some(dv) {
                continue;
            }
            if !is_bulk_attribute_value_eligible(dv) {
                return false;
            }
            edits.push(BulkEdit::Attribute(
                global_row,
                key.clone(),
                Some(dv.clone()),
            ));
        }
    }
    if let Some(om) = o_map {
        for (key, ov) in om {
            if is_engine_managed_attribute(key) {
                continue;
            }
            if d_map.is_none_or(|dm| !dm.contains_key(key)) {
                if holds_marker(ov) {
                    return false;
                }
                edits.push(BulkEdit::Attribute(global_row, key.clone(), None));
            }
        }
    }
    true
}

fn extract_tag_slice(value: Option<&Value>) -> Option<Vec<&str>> {
    let Some(val) = value else {
        return Some(Vec::new());
    };
    let arr = val.as_array()?;
    let mut tags = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item.as_str()?;
        if holds_marker(item) || s.is_empty() {
            return None;
        }
        tags.push(s);
    }
    tags.sort_unstable();
    tags.dedup();
    Some(tags)
}

fn append_tag_diffs(
    global_row: usize,
    d_val: Option<&Value>,
    o_val: Option<&Value>,
    edits: &mut Vec<BulkEdit>,
) -> bool {
    let (Some(d_tags), Some(o_tags)) = (extract_tag_slice(d_val), extract_tag_slice(o_val)) else {
        return false;
    };
    if d_tags == o_tags {
        return true;
    }
    let (mut di, mut oi) = (0_usize, 0_usize);
    while di < d_tags.len() || oi < o_tags.len() {
        if di < d_tags.len() && (oi >= o_tags.len() || d_tags[di] < o_tags[oi]) {
            edits.push(BulkEdit::Tag(global_row, d_tags[di].to_string(), true));
            di += 1;
        } else if oi < o_tags.len() && (di >= d_tags.len() || o_tags[oi] < d_tags[di]) {
            edits.push(BulkEdit::Tag(global_row, o_tags[oi].to_string(), false));
            oi += 1;
        } else {
            di += 1;
            oi += 1;
        }
    }
    true
}

fn diff_aligned_tags_column(
    ctx: &BulkDiffContext<'_>,
    d_vals: &AHashMap<usize, Value>,
    o_vals: &AHashMap<usize, Value>,
    block_first: usize,
    edits: &mut Vec<BulkEdit>,
) -> bool {
    let mut touched_local: Vec<usize> = Vec::with_capacity(d_vals.len().max(o_vals.len()));
    touched_local.extend(d_vals.keys().copied());
    for &k in o_vals.keys() {
        if !d_vals.contains_key(&k) {
            touched_local.push(k);
        }
    }
    touched_local.sort_unstable();
    for local_row in touched_local {
        let global_row = block_first + local_row;
        let class_name = ctx.desired_rows[global_row].class_name.as_str();
        let d_val = d_vals.get(&local_row);
        let o_val = o_vals.get(&local_row);
        if d_val == o_val {
            continue;
        }
        if class_name == "PackageLink" {
            continue;
        }
        if class_name == "Camera" {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): tag protected class={class_name}",
                    ctx.service
                ),
            );
            return false;
        }
        if !append_tag_diffs(global_row, d_val, o_val, edits) {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): tag value ineligible class={class_name}",
                    ctx.service
                ),
            );
            return false;
        }
    }
    true
}

fn diff_aligned_decoded_column<'a>(
    ctx: &'a BulkDiffContext<'a>,
    prop_name: &'a str,
    d_vals: &AHashMap<usize, Value>,
    o_vals: &AHashMap<usize, Value>,
    block_first: usize,
    eligible_cache: &mut AHashMap<(&'a str, &'a str), bool>,
    edits: &mut Vec<BulkEdit>,
) -> bool {
    let mut touched_local: Vec<usize> = Vec::with_capacity(d_vals.len().max(o_vals.len()));
    touched_local.extend(d_vals.keys().copied());
    for &k in o_vals.keys() {
        if !d_vals.contains_key(&k) {
            touched_local.push(k);
        }
    }
    touched_local.sort_unstable();

    for local_row in touched_local {
        let global_row = block_first + local_row;
        let class_name = ctx.desired_rows[global_row].class_name.as_str();
        let d_val = d_vals.get(&local_row);
        let o_val = o_vals.get(&local_row);
        if d_val.is_some_and(holds_marker) || o_val.is_some_and(holds_marker) {
            return false;
        }
        if reconciliation_property_values_equal(class_name, prop_name, d_val, o_val)
            || reconciliation_property_is_derived(prop_name)
        {
            continue;
        }
        if class_name == "PackageLink" {
            continue;
        }
        if class_name == "Camera" {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): row={} class={class_name} prop={prop_name}",
                    ctx.service, global_row
                ),
            );
            return false;
        }
        let target_value = match resolve_target_property_value(
            ctx.database,
            class_name,
            prop_name,
            d_val,
            o_val,
        ) {
            TargetProperty::Skip => continue,
            TargetProperty::Fallback => {
                crate::app::output::log_global(
                    4,
                    format_args!(
                        "[renium] bulk diff fallback ({}): unresolved default class={class_name} prop={prop_name}",
                        ctx.service
                    ),
                );
                return false;
            }
            TargetProperty::Write(val) => val,
        };
        let class_prop_eligible = *eligible_cache
            .entry((class_name, prop_name))
            .or_insert_with(|| {
                is_bulk_plain_property_class_eligible(
                    ctx.database,
                    ctx.service,
                    class_name,
                    prop_name,
                )
            });
        if !class_prop_eligible || !is_bulk_plain_value_eligible(&target_value) {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): ineligible class={class_name} prop={prop_name} class_ok={class_prop_eligible}",
                    ctx.service
                ),
            );
            return false;
        }
        edits.push(BulkEdit::Property(
            global_row,
            prop_name.to_string(),
            ExtractedValue::from_json(target_value),
        ));
    }
    true
}

enum TargetProperty {
    Skip,
    Fallback,
    Write(Value),
}

fn resolve_target_property_value(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    prop_name: &str,
    d_val: Option<&Value>,
    o_val: Option<&Value>,
) -> TargetProperty {
    if let Some(val) = d_val {
        return TargetProperty::Write(val.clone());
    }
    if crate::settings::equivalence::reconciliation_property_is_unknown_when_absent(
        Some(database),
        class_name,
        prop_name,
    ) {
        return TargetProperty::Skip;
    }
    let Some(default_val) = reflection_default_json(database, class_name, prop_name) else {
        return TargetProperty::Fallback;
    };
    if !reconciliation_property_values_equal(class_name, prop_name, None, Some(&default_val)) {
        return TargetProperty::Fallback;
    }
    if reconciliation_property_values_equal(class_name, prop_name, Some(&default_val), o_val) {
        return TargetProperty::Skip;
    }
    TargetProperty::Write(default_val)
}

fn build_bulk_package_target(
    desired_rows: &[SettingsBytecodeInstance],
    sibling_ordinals: &[u32],
    service: &str,
    pkg_root: usize,
) -> Option<BulkPackageTarget> {
    if pkg_root == 0 || pkg_root >= desired_rows.len() {
        return None;
    }
    let mut chain = Vec::new();
    let mut cursor = Some(pkg_root);
    while let Some(idx) = cursor {
        if idx == 0 {
            break;
        }
        chain.push(idx);
        cursor = desired_rows[idx].parent_index;
    }
    if chain.is_empty() {
        return None;
    }
    let mut path_segments = Vec::with_capacity(chain.len() + 1);
    let mut path_ordinals = Vec::with_capacity(chain.len() + 1);
    path_segments.push(service.to_string());
    path_ordinals.push(1);
    for &idx in chain.iter().rev() {
        path_segments.push(desired_rows[idx].name.clone());
        path_ordinals.push(sibling_ordinals[idx] as usize);
    }
    Some(BulkPackageTarget {
        path_segments,
        path_ordinals,
    })
}

fn build_bulk_property_patch(
    desired_rows: &[SettingsBytecodeInstance],
    sibling_ordinals: &[u32],
    package_root_of: &[Option<usize>],
    all_edits: Vec<BulkEdit>,
    database: &ReflectionDatabase<'_>,
    service: &str,
    min_instances: usize,
) -> BulkDiffOutcome {
    match build_bulk_property_patch_with_rows(
        desired_rows,
        sibling_ordinals,
        package_root_of,
        all_edits,
        database,
        service,
        min_instances,
    ) {
        Some((patch, _)) => BulkDiffOutcome::Patch(patch),
        None => BulkDiffOutcome::Fallback,
    }
}

fn build_bulk_property_patch_with_rows<'a>(
    desired_rows: &'a [SettingsBytecodeInstance],
    sibling_ordinals: &[u32],
    package_root_of: &[Option<usize>],
    all_edits: Vec<BulkEdit>,
    database: &'a ReflectionDatabase<'_>,
    service: &str,
    min_instances: usize,
) -> Option<(EditorBulkPropertyPatch, Vec<usize>)> {
    let row_count = desired_rows.len();
    let mut needed = vec![false; row_count];
    let mut touched_seen = vec![false; row_count];
    let mut touched_package_roots = BTreeSet::new();
    let mut instance_count = 0_usize;
    for edit in &all_edits {
        let row = edit.row();
        if !touched_seen[row] {
            touched_seen[row] = true;
            instance_count += 1;
            if let Some(pkg_root) = package_root_of[row] {
                if pkg_root == 0 {
                    return None;
                }
                touched_package_roots.insert(pkg_root);
            }
            let mut cursor = Some(row);
            while let Some(idx) = cursor {
                if idx == 0 || needed[idx] {
                    break;
                }
                needed[idx] = true;
                cursor = desired_rows[idx].parent_index;
            }
        }
    }
    if instance_count < min_instances {
        return None;
    }
    let mut package_targets = Vec::with_capacity(touched_package_roots.len());
    for pkg_root in touched_package_roots {
        package_targets.push(build_bulk_package_target(
            desired_rows,
            sibling_ordinals,
            service,
            pkg_root,
        )?);
    }

    let mut child_counts = vec![0_u32; row_count];
    let mut needed_count = 0_usize;
    for idx in 1..row_count {
        if needed[idx] {
            needed_count += 1;
            let parent = desired_rows[idx].parent_index.unwrap_or(0);
            child_counts[parent] += 1;
        }
    }
    let mut child_offsets = vec![0_usize; row_count + 1];
    for idx in 0..row_count {
        child_offsets[idx + 1] = child_offsets[idx] + child_counts[idx] as usize;
    }
    let mut flat_children = vec![0_usize; needed_count];
    let mut write_pos = child_offsets[..row_count].to_vec();
    for idx in 1..row_count {
        if needed[idx] {
            let parent = desired_rows[idx].parent_index.unwrap_or(0);
            let slot = write_pos[parent];
            flat_children[slot] = idx;
            write_pos[parent] = slot + 1;
        }
    }

    let mut strings: Vec<String> = Vec::new();
    let mut string_ids: AHashMap<&'a str, u32> = AHashMap::new();
    let mut intern = |text: &'a str| -> u32 {
        if let Some(&id) = string_ids.get(text) {
            id
        } else {
            let id = strings.len() as u32;
            strings.push(text.to_string());
            string_ids.insert(text, id);
            id
        }
    };

    let mut row_to_node = vec![0_u32; row_count];
    let mut node_to_row = Vec::with_capacity(needed_count);
    let mut nodes: Vec<BulkTreeNode> = Vec::with_capacity(needed_count);
    let mut queue = std::collections::VecDeque::with_capacity(needed_count);
    for &child in &flat_children[child_offsets[0]..child_offsets[1]] {
        queue.push_back((child, 0_u32));
    }
    while let Some((row_idx, parent_node)) = queue.pop_front() {
        let node_id = (nodes.len() + 1) as u32;
        row_to_node[row_idx] = node_id;
        node_to_row.push(row_idx);
        let row = &desired_rows[row_idx];
        let name_idx = intern(row.name.as_str());
        let class_idx = intern(row.class_name.as_str());
        nodes.push(BulkTreeNode {
            parent: parent_node,
            ordinal: sibling_ordinals[row_idx],
            name_idx,
            class_idx,
        });
        for &kid in &flat_children[child_offsets[row_idx]..child_offsets[row_idx + 1]] {
            queue.push_back((kid, node_id));
        }
    }
    if nodes.len() != needed_count {
        return None;
    }

    let field_count = all_edits.len();
    let mut by_property: BTreeMap<String, Vec<(u32, &'a str, ExtractedValue)>> = BTreeMap::new();
    let mut by_attribute: BTreeMap<String, Vec<(u32, Value)>> = BTreeMap::new();
    #[cfg(any(windows, target_os = "macos", test))]
    let mut attr_replaces: Vec<(u32, Value)> = Vec::new();
    let mut by_tag: BTreeMap<String, Vec<(u32, u8)>> = BTreeMap::new();
    for edit in all_edits {
        let global_row = edit.row();
        let node_id = row_to_node[global_row];
        if global_row != 0 && node_id == 0 {
            return None;
        }
        match edit {
            BulkEdit::Property(_, prop_name, value) => {
                by_property.entry(prop_name).or_default().push((
                    node_id,
                    desired_rows[global_row].class_name.as_str(),
                    value,
                ));
            }
            BulkEdit::Attribute(_, attr_name, value) => {
                by_attribute.entry(attr_name).or_default().push((
                    node_id,
                    value.unwrap_or_else(|| json!({"_type": "AttributeDelete"})),
                ));
            }
            #[cfg(any(windows, target_os = "macos", test))]
            BulkEdit::AttributesReplace(_, map) => {
                attr_replaces.push((node_id, Value::Object(map)));
            }
            BulkEdit::Tag(_, tag_name, has_tag) => {
                by_tag
                    .entry(tag_name)
                    .or_default()
                    .push((node_id, u8::from(has_tag)));
            }
        }
    }

    let mut columns = Vec::with_capacity(by_property.len() + by_attribute.len() + by_tag.len() + 1);
    for (property_name, mut entries) in by_property {
        entries.sort_unstable_by_key(|(node_id, _, _)| *node_id);
        encode_extracted_columns(database, property_name, entries, &mut columns);
    }
    for (attr_name, mut entries) in by_attribute {
        entries.sort_unstable_by_key(|(node_id, _)| *node_id);
        let (rows, vals): (Vec<u32>, Vec<Value>) = entries.into_iter().unzip();
        columns.push(build_indexed_column(
            BulkColumnKind::Attribute,
            attr_name,
            rows,
            vals,
        ));
    }
    #[cfg(any(windows, target_os = "macos", test))]
    if !attr_replaces.is_empty() {
        attr_replaces.sort_unstable_by_key(|(node_id, _)| *node_id);
        let (rows, vals): (Vec<u32>, Vec<Value>) = attr_replaces.into_iter().unzip();
        columns.push(build_indexed_column(
            BulkColumnKind::AttributesReplace,
            "@attributes".to_string(),
            rows,
            vals,
        ));
    }
    for (tag_name, mut entries) in by_tag {
        entries.sort_unstable_by_key(|(node_id, _)| *node_id);
        let (rows, vals): (Vec<u32>, Vec<u8>) = entries.into_iter().unzip();
        let first = vals[0];
        let values = if vals.iter().all(|&v| v == first) {
            BulkPropertyValues::BoolConst(first != 0)
        } else {
            BulkPropertyValues::Bool(vals)
        };
        columns.push(BulkPropertyColumn {
            kind: BulkColumnKind::Tag,
            property_name: tag_name,
            rows,
            values,
        });
    }
    columns.sort_by_key(|col| match (col.kind, col.property_name.as_str()) {
        (BulkColumnKind::AssertModelScale | BulkColumnKind::AssertModelPivot, _) => {
            (0_u8, col.property_name.clone())
        }
        (BulkColumnKind::Property, "Anchored") => (1_u8, col.property_name.clone()),
        (BulkColumnKind::Property, "Size") => (2_u8, col.property_name.clone()),
        (BulkColumnKind::Property, "CFrame") => (3_u8, col.property_name.clone()),
        (BulkColumnKind::Property, _) => (4_u8, col.property_name.clone()),
        (BulkColumnKind::Attribute | BulkColumnKind::AttributesReplace, _) => {
            (5_u8, col.property_name.clone())
        }
        (BulkColumnKind::Tag, _) => (6_u8, col.property_name.clone()),
    });

    Some((
        EditorBulkPropertyPatch {
            service: service.to_string(),
            strings,
            nodes,
            columns,
            field_count,
            instance_count,
            expected_terrain_digest: None,
            return_old_values: false,
            package_targets,
        },
        node_to_row,
    ))
}

fn block_property_names<'a>(store: &'a BlockStore<'_>, open: &'a OpenBlock) -> Result<Vec<String>> {
    let pooled = open
        .properties
        .iter()
        .filter_map(|id| match open.strings[*id] {
            BlockString::Pool(entry) => Some(entry),
            BlockString::Own(_) => None,
        });
    let read = store.pool_read(pooled)?;
    open.properties
        .iter()
        .map(|id| match &open.strings[*id] {
            BlockString::Own(text) => Ok(text.clone()),
            BlockString::Pool(entry) => Ok(store.entry(&read, *entry)?.to_string()),
        })
        .collect()
}

fn block_strings_equal(
    store_a: &BlockStore<'_>,
    open_a: &OpenBlock,
    id_a: usize,
    store_b: &BlockStore<'_>,
    open_b: &OpenBlock,
    id_b: usize,
) -> bool {
    match (open_a.strings.get(id_a), open_b.strings.get(id_b)) {
        (Some(BlockString::Own(a)), Some(BlockString::Own(b))) => a == b,
        (Some(BlockString::Pool(a)), Some(BlockString::Pool(b))) => {
            store_a.hashes[*a] == store_b.hashes[*b]
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn group_bodies_equal_unpooled(
    kind: u8,
    count: usize,
    body_a: &[u8],
    body_b: &[u8],
    store_a: &BlockStore<'_>,
    open_a: &OpenBlock,
    store_b: &BlockStore<'_>,
    open_b: &OpenBlock,
    obs_to_des: Option<&[usize]>,
) -> Result<bool> {
    match kind {
        0..=5 | 9 | 11..=16 | 18 | 20 | 21 => {
            return Ok(body_a == body_b);
        }
        17 if obs_to_des.is_none() => return Ok(body_a == body_b),
        10 => return Ok(false),
        _ => {}
    }
    let mut ra = BytecodeReader::new(body_a);
    let mut rb = BytecodeReader::new(body_b);
    let mut prev_ref_a = 0_usize;
    let mut prev_ref_b = 0_usize;
    for pos in 0..count {
        let da = ra.read_len("property instance index")?;
        let db = rb.read_len("property instance index")?;
        if da != db {
            return Ok(false);
        }
        if kind == 17 {
            let ref_a = if pos == 0 {
                ra.read_len("resolved ref index")?
            } else {
                prev_ref_a
                    .checked_add_signed(unzigzag_i64(ra.read_var_u64()?) as isize)
                    .context("Ref delta overflow")?
            };
            let ref_b = if pos == 0 {
                rb.read_len("resolved ref index")?
            } else {
                prev_ref_b
                    .checked_add_signed(unzigzag_i64(rb.read_var_u64()?) as isize)
                    .context("Ref delta overflow")?
            };
            prev_ref_a = ref_a;
            prev_ref_b = ref_b;
            let mapped_b = match obs_to_des {
                Some(map) => match map.get(ref_b).copied() {
                    Some(mapped) => mapped,
                    None => return Ok(false),
                },
                None => ref_b,
            };
            if ref_a != mapped_b {
                return Ok(false);
            }
            continue;
        }
        if !raw_values_equal_unpooled(
            kind, &mut ra, &mut rb, store_a, open_a, store_b, open_b, obs_to_des, 0,
        )? {
            return Ok(false);
        }
    }
    ra.finish()?;
    rb.finish()?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn raw_values_equal_unpooled(
    kind: u8,
    ra: &mut BytecodeReader<'_>,
    rb: &mut BytecodeReader<'_>,
    store_a: &BlockStore<'_>,
    open_a: &OpenBlock,
    store_b: &BlockStore<'_>,
    open_b: &OpenBlock,
    obs_to_des: Option<&[usize]>,
    depth: usize,
) -> Result<bool> {
    if depth > 64 {
        return Ok(false);
    }
    match kind {
        0..=2 => Ok(true),
        3 | 4 => Ok(ra.read_var_u64()? == rb.read_var_u64()?),
        5 => Ok(ra.read_bytes(8)? == rb.read_bytes(8)?),
        20 => Ok(ra.read_bytes(4)? == rb.read_bytes(4)?),
        6 | 19 => {
            let sa = ra.read_len("string id")?;
            let sb = rb.read_len("string id")?;
            Ok(block_strings_equal(
                store_a, open_a, sa, store_b, open_b, sb,
            ))
        }
        NESTED_ENUM_ITEM_KIND => {
            let ta = ra.read_len("enum type string id")?;
            let tb = rb.read_len("enum type string id")?;
            let na = ra.read_len("enum name string id")?;
            let nb = rb.read_len("enum name string id")?;
            Ok(
                block_strings_equal(store_a, open_a, ta, store_b, open_b, tb)
                    && block_strings_equal(store_a, open_a, na, store_b, open_b, nb),
            )
        }
        7 => {
            let la = ra.read_collection_len("array length")?;
            let lb = rb.read_collection_len("array length")?;
            if la != lb {
                return Ok(false);
            }
            for _ in 0..la {
                let ka = ra.read_u8().context("Missing array item kind")?;
                let kb = rb.read_u8().context("Missing array item kind")?;
                if ka != kb
                    || !raw_values_equal_unpooled(
                        ka,
                        ra,
                        rb,
                        store_a,
                        open_a,
                        store_b,
                        open_b,
                        obs_to_des,
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
            let lb = rb.read_collection_len("object length")?;
            if la != lb {
                return Ok(false);
            }
            for _ in 0..la {
                let ka = ra.read_len("object key string id")?;
                let kb = rb.read_len("object key string id")?;
                if !block_strings_equal(store_a, open_a, ka, store_b, open_b, kb) {
                    return Ok(false);
                }
                if obs_to_des.is_some() {
                    let is_ref_key = match open_a.strings.get(ka) {
                        Some(BlockString::Own(k)) => matches!(
                            k.as_str(),
                            "instanceIndex" | "settingsId" | "instanceId" | "debugId"
                        ),
                        _ => true,
                    };
                    if is_ref_key {
                        return Ok(false);
                    }
                }
                let va = ra.read_u8().context("Missing object value kind")?;
                let vb = rb.read_u8().context("Missing object value kind")?;
                if va != vb
                    || !raw_values_equal_unpooled(
                        va,
                        ra,
                        rb,
                        store_a,
                        open_a,
                        store_b,
                        open_b,
                        obs_to_des,
                        depth + 1,
                    )?
                {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        9 => {
            let la = ra.read_len("numeric array length")?;
            let lb = rb.read_len("numeric array length")?;
            if la != lb {
                return Ok(false);
            }
            Ok(ra.read_bytes(la.saturating_mul(8))? == rb.read_bytes(lb.saturating_mul(8))?)
        }
        21 => {
            let la = ra.read_len("f32 numeric array length")?;
            let lb = rb.read_len("f32 numeric array length")?;
            if la != lb {
                return Ok(false);
            }
            Ok(ra.read_bytes(la.saturating_mul(4))? == rb.read_bytes(lb.saturating_mul(4))?)
        }
        11 | 13 => Ok(ra.read_bytes(8)? == rb.read_bytes(8)?),
        12 | 15 => Ok(ra.read_bytes(12)? == rb.read_bytes(12)?),
        14 | 18 => Ok(ra.read_bytes(16)? == rb.read_bytes(16)?),
        16 => Ok(ra.read_bytes(48)? == rb.read_bytes(48)?),
        17 => {
            let ref_a = ra.read_len("resolved ref index")?;
            let ref_b = rb.read_len("resolved ref index")?;
            let mapped_b = match obs_to_des {
                Some(map) => match map.get(ref_b).copied() {
                    Some(mapped) => mapped,
                    None => return Ok(false),
                },
                None => ref_b,
            };
            Ok(ref_a == mapped_b)
        }
        _ => Ok(false),
    }
}

/// Pairs every observed row to its exact desired counterpart by matching top-down
/// on `(mapped_parent, name, class_name, sibling_ordinal)`. Returns `None` if the
/// two trees do not have identical hierarchy and sibling ordinals.
fn match_permuted_tree(
    desired_rows: &[SettingsBytecodeInstance],
    desired_ordinals: &[u32],
    observed_rows: &[SettingsBytecodeInstance],
) -> Option<Vec<usize>> {
    let n = desired_rows.len();
    if observed_rows.len() != n || n == 0 {
        return None;
    }
    if desired_rows[0].parent_index.is_some()
        || observed_rows[0].parent_index.is_some()
        || desired_rows[0].name != observed_rows[0].name
        || desired_rows[0].class_name != observed_rows[0].class_name
    {
        return None;
    }
    let mut des_lookup: AHashMap<(usize, &str, &str, u32), usize> = AHashMap::with_capacity(n);
    for (idx, row) in desired_rows.iter().enumerate().skip(1) {
        let parent = row.parent_index?;
        if des_lookup
            .insert(
                (
                    parent,
                    row.name.as_str(),
                    row.class_name.as_str(),
                    desired_ordinals[idx],
                ),
                idx,
            )
            .is_some()
        {
            return None;
        }
    }

    let mut obs_children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut obs_ordinals = vec![1_u32; n];
    let mut obs_name_counts: AHashMap<(usize, &str), u32> = AHashMap::with_capacity(n);
    for (idx, row) in observed_rows.iter().enumerate().skip(1) {
        let parent = row.parent_index?;
        if parent >= n {
            return None;
        }
        let ord = obs_name_counts
            .entry((parent, row.name.as_str()))
            .or_insert(0);
        *ord += 1;
        obs_ordinals[idx] = *ord;
        obs_children[parent].push(idx);
    }

    let mut obs_to_des = vec![usize::MAX; n];
    obs_to_des[0] = 0;
    let mut matched = 1_usize;
    let mut queue = std::collections::VecDeque::with_capacity(n);
    queue.push_back(0_usize);
    while let Some(o_parent) = queue.pop_front() {
        let d_parent = obs_to_des[o_parent];
        for &o_child in &obs_children[o_parent] {
            let row = &observed_rows[o_child];
            let &d_child = des_lookup.get(&(
                d_parent,
                row.name.as_str(),
                row.class_name.as_str(),
                obs_ordinals[o_child],
            ))?;
            if obs_to_des[o_child] != usize::MAX {
                return None;
            }
            obs_to_des[o_child] = d_child;
            matched += 1;
            queue.push_back(o_child);
        }
    }
    (matched == n).then_some(obs_to_des)
}

fn diff_permuted_pairs(
    ctx: &BulkDiffContext<'_>,
    pairs: &[(usize, usize)],
) -> Result<Option<Vec<BulkEdit>>> {
    let desired = ctx.desired;
    let observed = ctx.observed;
    let desired_rows = ctx.desired_rows;
    let obs_to_des = ctx.obs_to_des;
    let is_twin = ctx.is_twin;
    let database = ctx.database;
    let service = ctx.service;
    let row_count = desired_rows.len();
    let mut des_needed: Vec<usize> = pairs.iter().map(|&(d, _)| d).collect();
    let mut obs_needed: Vec<usize> = pairs.iter().map(|&(_, o)| o).collect();
    des_needed.sort_unstable();
    des_needed.dedup();
    obs_needed.sort_unstable();
    obs_needed.dedup();

    let read_marked = |store: &BlockStore<'_>,
                       rows: &[usize],
                       remap: Option<&[Option<usize>]>|
     -> Result<Vec<Option<DecodedRowValues>>> {
        let mut by_block: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for &row in rows {
            by_block.entry(store.block_of(row)).or_default().push(row);
        }
        let blocks: Vec<(usize, Vec<usize>)> = by_block.into_iter().collect();
        let parts = blocks
            .par_iter()
            .map(
                |(block_idx, block_rows)| -> Result<Vec<(usize, DecodedRowValues)>> {
                    let first = store.blocks[*block_idx].first;
                    let count = store.blocks[*block_idx].count;
                    let open = store.open_block(*block_idx)?;
                    let mut values = (0..count)
                        .map(|_| DecodedRowValues::default())
                        .collect::<Vec<_>>();
                    let keep_all = block_rows.len() == count;
                    let keep = |local: usize| {
                        keep_all || block_rows.binary_search(&(first + local)).is_ok()
                    };
                    store.values(&open, &mut values, keep, |_, _| true, Texts::Marked)?;
                    let mut out = Vec::with_capacity(block_rows.len());
                    for &row in block_rows {
                        let mut vals = std::mem::take(&mut values[row - first]);
                        if let Some(remap) = remap {
                            for val in vals.properties.values_mut() {
                                remap_ref_indices_in_value(val, remap);
                            }
                            for val in vals.attributes.values_mut() {
                                remap_ref_indices_in_value(val, remap);
                            }
                        }
                        out.push((row, vals));
                    }
                    Ok(out)
                },
            )
            .collect::<Result<Vec<_>>>()?;
        let mut table: Vec<Option<DecodedRowValues>> = (0..row_count).map(|_| None).collect();
        for part in parts {
            for (row, vals) in part {
                table[row] = Some(vals);
            }
        }
        Ok(table)
    };

    let remap: Vec<Option<usize>> = obs_to_des.iter().copied().map(Some).collect();
    let (des_table, obs_table) = rayon::join(
        || read_marked(desired, &des_needed, None),
        || read_marked(observed, &obs_needed, Some(&remap)),
    );
    let des_table = des_table?;
    let obs_table = obs_table?;

    enum ChunkResult {
        Fallback,
        Edits(Vec<BulkEdit>),
    }

    let chunk_results = pairs
        .par_chunks(2048)
        .map(|chunk| -> Result<ChunkResult> {
            let mut edits = Vec::new();
            let mut eligible_cache: AHashMap<(&str, &str), bool> = AHashMap::new();
            for &(d_row, o_row) in chunk {
                let Some(d_vals) = des_table[d_row].as_ref() else {
                    return Ok(ChunkResult::Fallback);
                };
                let Some(o_vals) = obs_table[o_row].as_ref() else {
                    return Ok(ChunkResult::Fallback);
                };
                let row_inst = &desired_rows[d_row];
                let class_name = row_inst.class_name.as_str();
                if class_name == "PackageLink" {
                    continue;
                }
                if d_vals.attributes != o_vals.attributes
                    && (class_name == "Camera"
                        || row_inst.name == "__Renium_SessionLock"
                        || !append_attribute_map_diffs(
                            d_row,
                            (!d_vals.attributes.is_empty()).then_some(&d_vals.attributes),
                            (!o_vals.attributes.is_empty()).then_some(&o_vals.attributes),
                            &mut edits,
                        ))
                {
                    return Ok(ChunkResult::Fallback);
                }
                if is_twin[d_row]
                    && d_vals.properties.get(UNIQUE_ID) != o_vals.properties.get(UNIQUE_ID)
                {
                    return Ok(ChunkResult::Fallback);
                }
                let mut d_iter = d_vals.properties.iter().peekable();
                let mut o_iter = o_vals.properties.iter().peekable();
                loop {
                    let (prop_name, d_val, o_val) = match (d_iter.peek(), o_iter.peek()) {
                        (None, None) => break,
                        (Some(&(dk, _)), None) => {
                            let (_, dv) = d_iter.next().unwrap();
                            (dk.as_str(), Some(dv), None)
                        }
                        (None, Some(&(ok, _))) => {
                            let (_, ov) = o_iter.next().unwrap();
                            (ok.as_str(), None, Some(ov))
                        }
                        (Some(&(dk, _)), Some(&(ok, _))) => match dk.cmp(ok) {
                            std::cmp::Ordering::Less => {
                                let (_, dv) = d_iter.next().unwrap();
                                (dk.as_str(), Some(dv), None)
                            }
                            std::cmp::Ordering::Greater => {
                                let (_, ov) = o_iter.next().unwrap();
                                (ok.as_str(), None, Some(ov))
                            }
                            std::cmp::Ordering::Equal => {
                                let (_, dv) = d_iter.next().unwrap();
                                let (_, ov) = o_iter.next().unwrap();
                                (dk.as_str(), Some(dv), Some(ov))
                            }
                        },
                    };
                    if prop_name == UNIQUE_ID {
                        continue;
                    }
                    if prop_name == "CurrentCamera" && service == "Workspace" && d_row == 0 {
                        if d_val != o_val && d_val.is_some() && o_val.is_some() {
                            return Ok(ChunkResult::Fallback);
                        }
                        continue;
                    }
                    if d_val == o_val {
                        continue;
                    }
                    if prop_name == "Tags" {
                        if class_name == "Camera"
                            || class_name == "PackageLink"
                            || !append_tag_diffs(d_row, d_val, o_val, &mut edits)
                        {
                            return Ok(ChunkResult::Fallback);
                        }
                        continue;
                    }
                    if d_val.is_some_and(holds_marker) || o_val.is_some_and(holds_marker) {
                        return Ok(ChunkResult::Fallback);
                    }
                    if reconciliation_property_values_equal(class_name, prop_name, d_val, o_val) {
                        continue;
                    }
                    if reconciliation_property_is_derived(prop_name) {
                        continue;
                    }
                    if class_name == "Camera" || class_name == "PackageLink" {
                        return Ok(ChunkResult::Fallback);
                    }
                    let target_value = match d_val {
                        Some(val) => val.clone(),
                        None => {
                            if crate::settings::equivalence::reconciliation_property_is_unknown_when_absent(
                                Some(database),
                                class_name,
                                prop_name,
                            ) {
                                continue;
                            }
                            let Some(default_val) =
                                reflection_default_json(database, class_name, prop_name)
                            else {
                                return Ok(ChunkResult::Fallback);
                            };
                            if !reconciliation_property_values_equal(
                                class_name,
                                prop_name,
                                None,
                                Some(&default_val),
                            ) {
                                return Ok(ChunkResult::Fallback);
                            }
                            if reconciliation_property_values_equal(
                                class_name,
                                prop_name,
                                Some(&default_val),
                                o_val,
                            ) {
                                continue;
                            }
                            default_val
                        }
                    };
                    let class_prop_eligible = *eligible_cache
                        .entry((class_name, prop_name))
                        .or_insert_with(|| {
                            is_bulk_plain_property_class_eligible(
                                database,
                                service,
                                class_name,
                                prop_name,
                            )
                        });
                    if !class_prop_eligible || !is_bulk_plain_value_eligible(&target_value) {
                        crate::app::output::log_global(
                            4,
                            format_args!(
                                "[renium] bulk diff fallback ({service}): permuted ineligible {class_name}.{prop_name}"
                            ),
                        );
                        return Ok(ChunkResult::Fallback);
                    }
                    edits.push(BulkEdit::Property(
                        d_row,
                        prop_name.to_string(),
                        ExtractedValue::from_json(target_value),
                    ));
                }
            }
            Ok(ChunkResult::Edits(edits))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut edits = Vec::new();
    for part in chunk_results {
        match part {
            ChunkResult::Fallback => return Ok(None),
            ChunkResult::Edits(mut chunk_edits) => edits.append(&mut chunk_edits),
        }
    }
    Ok(Some(edits))
}

fn decode_block_attributes(
    store: &BlockStore<'_>,
    open: &OpenBlock,
    strings_cache: &mut Option<Vec<String>>,
    groups: &[(u8, usize, &[u8])],
    block_count: usize,
) -> Result<AHashMap<usize, serde_json::Map<String, Value>>> {
    if groups.is_empty() {
        return Ok(AHashMap::new());
    }
    let strings: &[String] = strings_cache.get_or_insert_with(|| {
        open.strings
            .iter()
            .map(|s| match s {
                BlockString::Own(text) => text.clone(),
                BlockString::Pool(entry) => marker(&store.hashes[*entry]),
            })
            .collect()
    });
    let budget = store.budget.not_holding();
    let scope = ValueScope::from_owned(strings, store.rows, &budget);
    let mut out = AHashMap::new();
    for &(kind, count, body) in groups {
        let entries = decode_property_group_body(body, "Attributes", kind, count, scope)?;
        for entry in entries {
            if let DecodedGroupEntry::Attributes(local_row, attrs) = entry {
                anyhow::ensure!(
                    local_row < block_count,
                    "A value block gives attributes to a row it does not hold"
                );
                out.insert(local_row, attrs);
            }
        }
    }
    Ok(out)
}

fn decode_block_property(
    store: &BlockStore<'_>,
    open: &OpenBlock,
    strings_cache: &mut Option<Vec<String>>,
    prop_name: &str,
    groups: &[(u8, usize, &[u8])],
    block_count: usize,
) -> Result<AHashMap<usize, Value>> {
    if groups.is_empty() {
        return Ok(AHashMap::new());
    }
    let needs_strings = groups
        .iter()
        .any(|&(kind, _, _)| !matches!(kind, 0 | 1 | 2 | 3 | 9 | 11..=16 | 17 | 18 | 20 | 21));
    let strings: &[String] = if needs_strings {
        strings_cache.get_or_insert_with(|| {
            open.strings
                .iter()
                .map(|s| match s {
                    BlockString::Own(text) => text.clone(),
                    BlockString::Pool(entry) => marker(&store.hashes[*entry]),
                })
                .collect()
        })
    } else {
        &[]
    };
    let budget = store.budget.not_holding();
    let scope = ValueScope::from_owned(strings, store.rows, &budget);
    let mut out = AHashMap::new();
    for &(kind, count, body) in groups {
        let entries = decode_property_group_body(body, prop_name, kind, count, scope)?;
        for entry in entries {
            if let DecodedGroupEntry::Property(local_row, value) = entry {
                anyhow::ensure!(
                    local_row < block_count,
                    "A value block gives a value to a row it does not hold"
                );
                out.insert(local_row, value);
            }
        }
    }
    Ok(out)
}

fn unique_ids_safe_for_positional(
    desired: &AHashMap<usize, Value>,
    observed: &AHashMap<usize, Value>,
    block_first: usize,
    is_twin: &[bool],
) -> bool {
    for (&local_row, d_val) in desired {
        let o_val = observed.get(&local_row);
        if o_val != Some(d_val) && is_twin[block_first + local_row] {
            return false;
        }
    }
    for (&local_row, _) in observed {
        if !desired.contains_key(&local_row) && is_twin[block_first + local_row] {
            return false;
        }
    }
    true
}

fn is_bulk_plain_property_class_eligible(
    database: &ReflectionDatabase<'_>,
    service: &str,
    class_name: &str,
    property_name: &str,
) -> bool {
    if matches!(
        property_name,
        "Name"
            | "Parent"
            | "ClassName"
            | "Archivable"
            | "Attributes"
            | "Tags"
            | "Source"
            | "LinkedSource"
            | "UniqueId"
            | "CurrentCamera"
            | "ScriptGuid"
            | "SourceAssetId"
            | "WorldPivot"
            | "WorldPivotData"
            | "Origin"
            | "PivotOffset"
            | "TextScaled"
            | "TextWrapped"
            | "BrickColor"
            | "Color3uint8"
            | "Disabled"
            | "ScreenInsets"
            | "IgnoreGuiInset"
            | "Use2022Materials"
            | "LightingStyle"
            | "PrioritizeLightingQuality"
            | "SmoothGrid"
            | "PhysicsGrid"
            | "CollisionGroupData"
            | "MaterialColors"
            | "MeshId"
            | "MeshContent"
            | "MeshSize"
            | "CollisionFidelity"
            | "RenderFidelity"
            | "FluidFidelity"
            | "Font"
            | "FontFace"
    ) {
        return false;
    }
    if matches!(property_name, "Position" | "Orientation" | "Rotation")
        && (crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BasePart")
            || crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "Attachment"))
    {
        return false;
    }
    if property_name == "Enabled"
        && crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BaseScript")
    {
        return false;
    }
    if property_name == "CFrame"
        && !crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BasePart")
        && !crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "Attachment")
        && !crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "Bone")
    {
        return false;
    }
    if crate::editor::native_roots::is_property(class_name, property_name)
        || crate::editor::native_geometry::is_mesh_geometry_property(class_name, property_name)
        || crate::editor::review::is_engine_managed_editor_property(
            class_name,
            property_name,
            database,
        )
    {
        return false;
    }
    if crate::editor::review::is_externally_managed_editor_property(
        service,
        class_name,
        &[service.to_string()],
        property_name,
    ) {
        return false;
    }

    let Some(descriptor) =
        crate::rbx::encode::rbx_property_descriptor(database, class_name, property_name)
    else {
        return false;
    };
    if descriptor.name != property_name {
        return false;
    }
    if !matches!(descriptor.scriptability, RbxScriptability::ReadWrite) {
        return false;
    }
    if !matches!(
        descriptor.kind,
        RbxPropertyKind::Canonical {
            serialization: RbxPropertySerialization::Serializes
                | RbxPropertySerialization::SerializesAs(_)
                | RbxPropertySerialization::Migrate(_)
        }
    ) {
        return false;
    }

    match &descriptor.data_type {
        RbxDataType::Enum(_) => true,
        RbxDataType::Value(variant_type) => matches!(
            variant_type,
            RbxVariantType::Bool
                | RbxVariantType::Int32
                | RbxVariantType::Int64
                | RbxVariantType::Float32
                | RbxVariantType::Float64
                | RbxVariantType::String
                | RbxVariantType::Vector2
                | RbxVariantType::Vector3
                | RbxVariantType::Color3
                | RbxVariantType::CFrame
                | RbxVariantType::UDim
                | RbxVariantType::UDim2
                | RbxVariantType::Rect
                | RbxVariantType::NumberRange
                | RbxVariantType::NumberSequence
                | RbxVariantType::ColorSequence
                | RbxVariantType::BrickColor
                | RbxVariantType::ContentId
                | RbxVariantType::Vector2int16
                | RbxVariantType::Vector3int16
                | RbxVariantType::Axes
                | RbxVariantType::Faces
                | RbxVariantType::Region3
                | RbxVariantType::Region3int16
                | RbxVariantType::Ray
        ),
        _ => false,
    }
}

fn is_bulk_plain_value_eligible(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return true;
    };
    match obj.get("_type").and_then(Value::as_str) {
        Some(
            "Ref" | "Number" | "BinaryString" | "SharedString" | "Content" | "Font" | "UniqueId",
        ) => false,
        Some("ContentId") => obj.get("value").is_some_and(Value::is_string),
        _ => true,
    }
}

fn reflection_default_json(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    property_name: &str,
) -> Option<Value> {
    let val = crate::settings::equivalence::reflection_default_settings_value(
        database,
        class_name,
        property_name,
    )?;
    (!holds_marker(&val) && is_bulk_plain_value_eligible(&val)).then_some(val)
}

fn enrich_enum_item_type(data_type: Option<&RbxDataType<'_>>, value: &mut Value) {
    if let (Some(RbxDataType::Enum(enum_name)), Value::Object(obj)) = (data_type, value)
        && obj.get("_type").and_then(Value::as_str) == Some("EnumItem")
        && obj
            .get("enumType")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        obj.insert(
            "enumType".to_string(),
            Value::String(format!("Enum.{enum_name}")),
        );
    }
}

fn build_indexed_column(
    kind: BulkColumnKind,
    property_name: String,
    rows: Vec<u32>,
    values: Vec<Value>,
) -> BulkPropertyColumn {
    let mut palette: Vec<Value> = Vec::new();
    let mut by_key: AHashMap<String, u32> = AHashMap::new();
    let mut indices = Vec::with_capacity(values.len());
    for value in values {
        let key = if let Some(obj) = value.as_object()
            && obj.get("_type").and_then(Value::as_str) == Some("EnumItem")
            && let Some(name) = obj.get("name").and_then(Value::as_str)
        {
            let enum_type = obj.get("enumType").and_then(Value::as_str).unwrap_or("");
            format!("E:{enum_type}:{name}")
        } else {
            serde_json::to_string(&value).unwrap_or_default()
        };
        let idx = if let Some(&idx) = by_key.get(&key) {
            idx
        } else {
            let idx = palette.len() as u32;
            palette.push(value);
            by_key.insert(key, idx);
            idx
        };
        indices.push(idx);
    }
    BulkPropertyColumn {
        kind,
        property_name,
        rows,
        values: BulkPropertyValues::Indexed { palette, indices },
    }
}

#[cfg(any(windows, target_os = "macos", test))]
pub(crate) struct BulkExtractedPatch {
    pub(crate) patch: EditorBulkPropertyPatch,
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    pub(crate) node_to_row: Vec<usize>,
}

#[cfg(any(windows, target_os = "macos", test))]
pub(crate) enum BulkExtractOutcome {
    Patch(BulkExtractedPatch),
    Fallback,
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExtractClassProp {
    Absent,
    Eligible,
    Ineligible,
}

#[cfg(any(windows, target_os = "macos", test))]
fn add_prop_bit(props: &mut Vec<String>, name: &str) -> Option<u32> {
    if let Some(pos) = props.iter().position(|existing| existing == name) {
        return Some(1_u32 << pos);
    }
    if props.len() >= 32 {
        return None;
    }
    let bit = 1_u32 << props.len();
    props.push(name.to_string());
    Some(bit)
}

#[cfg(any(windows, target_os = "macos", test))]
struct NormalizedZeroCaptureProps {
    prop_names: Vec<String>,
    class_masks: AHashMap<String, u32>,
}

#[cfg(any(windows, target_os = "macos", test))]
fn is_edit_mode_rigid_joint_class(database: &ReflectionDatabase<'_>, class_name: &str) -> bool {
    crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "JointInstance")
        || crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "WeldConstraint")
        || crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "RigidConstraint")
}

#[cfg(any(windows, target_os = "macos", test))]
fn normalize_zero_capture_class_props(
    rows: &[SettingsBytecodeInstance],
    class_props: &[(String, String)],
    database: &ReflectionDatabase<'_>,
    service: &str,
) -> Option<NormalizedZeroCaptureProps> {
    if class_props.is_empty() {
        return None;
    }
    let mut prop_names: Vec<String> = Vec::new();
    let mut class_masks: AHashMap<String, u32> = AHashMap::new();
    let mut expand_basepart_cframe = false;
    let mut has_rigid_joints: Option<bool> = None;

    for (class_name, raw_prop) in class_props {
        if raw_prop == "@attributes"
            || (raw_prop.starts_with("@tag:") && raw_prop.len() > "@tag:".len())
        {
            let bit = add_prop_bit(&mut prop_names, raw_prop)?;
            *class_masks.entry(class_name.clone()).or_insert(0) |= bit;
            continue;
        }
        if is_edit_mode_rigid_joint_class(database, class_name) {
            return None;
        }
        let is_basepart =
            crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BasePart");
        let is_attachment =
            crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "Attachment");
        let canonical = match raw_prop.as_str() {
            "BrickColor" | "Color3uint8" if is_basepart => "Color",
            "Position" | "Orientation" | "Rotation" | "CFrame" if is_basepart => {
                expand_basepart_cframe = true;
                "CFrame"
            }
            "Size" if is_basepart => "Size",
            "Position" | "Orientation" | "Rotation" | "CFrame" | "Axis" | "SecondaryAxis"
                if is_attachment =>
            {
                let service_has_rigid = *has_rigid_joints.get_or_insert_with(|| {
                    let mut seen: AHashSet<&str> = AHashSet::new();
                    rows.iter().any(|r| {
                        seen.insert(r.class_name.as_str())
                            && is_edit_mode_rigid_joint_class(database, r.class_name.as_str())
                    })
                });
                if service_has_rigid {
                    expand_basepart_cframe = true;
                }
                if matches!(raw_prop.as_str(), "Axis" | "SecondaryAxis") {
                    raw_prop.as_str()
                } else {
                    "CFrame"
                }
            }
            other => other,
        };
        if !is_bulk_plain_property_class_eligible(database, service, class_name, canonical) {
            return None;
        }
        let bit = add_prop_bit(&mut prop_names, canonical)?;
        *class_masks.entry(class_name.clone()).or_insert(0) |= bit;
    }

    if expand_basepart_cframe {
        let cf_bit = add_prop_bit(&mut prop_names, "CFrame")?;
        let mut seen_classes: AHashSet<&str> = AHashSet::new();
        for row in rows {
            let class_name = row.class_name.as_str();
            if class_name != "Terrain"
                && seen_classes.insert(class_name)
                && crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BasePart")
            {
                *class_masks.entry(class_name.to_string()).or_insert(0) |= cf_bit;
            }
        }
    }

    Some(NormalizedZeroCaptureProps {
        prop_names,
        class_masks,
    })
}

#[cfg(any(windows, target_os = "macos", test))]
fn classify_extract_targets<'a>(
    rows: &'a [SettingsBytecodeInstance],
    prop_names: &'a [String],
    class_masks: &AHashMap<String, u32>,
    database: &ReflectionDatabase<'_>,
    service: &str,
) -> Option<(Vec<u32>, usize)> {
    let mut class_cache: AHashMap<(&'a str, &'a str), ExtractClassProp> = AHashMap::new();
    let mut row_masks = vec![0_u32; rows.len()];
    let mut target_count = 0_usize;
    for (row_idx, row) in rows.iter().enumerate() {
        let class_name = row.class_name.as_str();
        let Some(&allowed_mask) = class_masks.get(class_name).filter(|&&m| m != 0) else {
            continue;
        };
        let mut mask = 0_u32;
        for (prop_bit, prop_name) in prop_names.iter().enumerate() {
            let bit = 1_u32 << prop_bit;
            if allowed_mask & bit == 0 {
                continue;
            }
            if prop_name == "@attributes" || prop_name.starts_with("@tag:") {
                if class_name == "Camera"
                    || class_name == "PackageLink"
                    || (prop_name == "@attributes" && row.name == "__Renium_SessionLock")
                {
                    return None;
                }
                mask |= bit;
                continue;
            }
            let state = *class_cache
                .entry((class_name, prop_name.as_str()))
                .or_insert_with(|| {
                    let has_prop = crate::rbx::encode::rbx_property_descriptor(
                        database, class_name, prop_name,
                    )
                    .is_some_and(|d| {
                        d.name == prop_name.as_str()
                            && matches!(d.scriptability, RbxScriptability::ReadWrite)
                    });
                    if !has_prop {
                        ExtractClassProp::Absent
                    } else if is_bulk_plain_property_class_eligible(
                        database, service, class_name, prop_name,
                    ) {
                        ExtractClassProp::Eligible
                    } else {
                        ExtractClassProp::Ineligible
                    }
                });
            match state {
                ExtractClassProp::Absent => return None,
                ExtractClassProp::Ineligible => {
                    crate::app::output::log_global(
                        5,
                        format_args!(
                            "[renium] zero-capture fallback ({service}): ineligible {class_name}.{prop_name}"
                        ),
                    );
                    return None;
                }
                ExtractClassProp::Eligible => {
                    if class_name == "Camera" || class_name == "PackageLink" {
                        crate::app::output::log_global(
                            5,
                            format_args!(
                                "[renium] zero-capture fallback ({service}): row={row_idx} class={class_name}"
                            ),
                        );
                        return None;
                    }
                    mask |= bit;
                }
            }
        }
        if mask != 0 {
            row_masks[row_idx] = mask;
            target_count += 1;
        }
    }
    Some((row_masks, target_count))
}

impl ExtractedValue {
    fn from_json(val: Value) -> Self {
        match &val {
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Self::Int(i)
                } else {
                    match n.as_f64() {
                        Some(f) if f.is_finite() => Self::F64(f),
                        _ => Self::Json(val),
                    }
                }
            }
            Value::String(s) => Self::String(s.clone()),
            Value::Object(obj) => match obj.get("_type").and_then(Value::as_str) {
                Some("Vector3") => {
                    if let (Some(x), Some(y), Some(z)) = (
                        obj.get("x").and_then(Value::as_f64),
                        obj.get("y").and_then(Value::as_f64),
                        obj.get("z").and_then(Value::as_f64),
                    ) {
                        return Self::Vec3([x as f32, y as f32, z as f32]);
                    }
                    Self::Json(val)
                }
                Some("Color3") => {
                    if let (Some(r), Some(g), Some(b)) = (
                        obj.get("r").and_then(Value::as_f64),
                        obj.get("g").and_then(Value::as_f64),
                        obj.get("b").and_then(Value::as_f64),
                    ) {
                        return Self::Color3([r as f32, g as f32, b as f32]);
                    }
                    Self::Json(val)
                }
                Some("CFrame") => {
                    if let Some(arr) = obj.get("components").and_then(Value::as_array)
                        && arr.len() == 12
                        && let Some(vals) = arr
                            .iter()
                            .map(|v| v.as_f64().filter(|f| f.is_finite()).map(|f| f as f32))
                            .collect::<Option<Vec<_>>>()
                    {
                        return Self::CFrame(vals.try_into().unwrap());
                    }
                    Self::Json(val)
                }
                Some("EnumItem") => {
                    if let Some(name) = obj.get("name").and_then(Value::as_str) {
                        return Self::EnumItem(name.to_string());
                    }
                    Self::Json(val)
                }
                _ => Self::Json(val),
            },
            _ => Self::Json(val),
        }
    }

    fn into_json(self) -> Value {
        match self {
            Self::Bool(b) => Value::Bool(b),
            Self::Int(i) => json!(i),
            Self::F64(f) => json_number_f64(f),
            Self::String(s) => Value::String(s),
            Self::Vec3([x, y, z]) => {
                json!({"_type": "Vector3", "x": f64::from(x), "y": f64::from(y), "z": f64::from(z)})
            }
            Self::Color3([r, g, b]) => {
                json!({"_type": "Color3", "r": f64::from(r), "g": f64::from(g), "b": f64::from(b)})
            }
            Self::CFrame(c) => {
                let comps: Vec<Value> = c.iter().map(|&v| json_number_f64(f64::from(v))).collect();
                json!({"_type": "CFrame", "components": comps})
            }
            Self::EnumItem(name) => json!({"_type": "EnumItem", "name": name}),
            Self::Json(v) => v,
        }
    }
}

#[cfg(any(windows, target_os = "macos", test))]
enum ExtractBlockResult {
    Edits(Vec<BulkEdit>),
    Fallback,
}

fn decode_block_raw_extracted_opt(
    groups: &[PropertyGroupSlice<'_>],
    open: &OpenBlock,
    block_count: usize,
) -> Result<Option<Vec<(usize, ExtractedValue)>>> {
    if groups.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let all_fast = groups.iter().all(|&(k, _, body)| match k {
        1..=6 | 12 | 15 | 19 | 20 => true,
        16 => body.first() == Some(&0),
        _ => false,
    });
    if !all_fast {
        return Ok(None);
    }
    let total: usize = groups.iter().map(|&(_, c, _)| c).sum();
    let mut out = Vec::with_capacity(total);
    for &(kind, count, body) in groups {
        let mut reader = BytecodeReader::new(body);
        if kind == 16 {
            let mode = reader.read_u8().context("Missing CFrame group mode")?;
            if mode != 0 {
                return Ok(None);
            }
        }
        let mut row = 0_usize;
        for pos in 0..count {
            row = next_group_instance(row, reader.read_len("property instance index")?, pos)?;
            anyhow::ensure!(row < block_count, "Block row out of range");
            let val = match kind {
                1 => ExtractedValue::Bool(false),
                2 => ExtractedValue::Bool(true),
                3 => ExtractedValue::Int(unzigzag_i64(reader.read_var_u64()?)),
                4 => match i64::try_from(reader.read_var_u64()?) {
                    Ok(i) => ExtractedValue::Int(i),
                    Err(_) => return Ok(None),
                },
                5 => {
                    let f = f64::from_le_bytes(reader.read_bytes(8)?.try_into().unwrap());
                    if !f.is_finite() {
                        return Ok(None);
                    }
                    ExtractedValue::F64(f)
                }
                6 => {
                    let sid = reader.read_len("string id")?;
                    match open.strings.get(sid) {
                        Some(BlockString::Own(text)) => ExtractedValue::String(text.clone()),
                        _ => return Ok(None),
                    }
                }
                20 => {
                    let f = f64::from(f32::from_le_bytes(
                        reader.read_bytes(4)?.try_into().unwrap(),
                    ));
                    if !f.is_finite() {
                        return Ok(None);
                    }
                    ExtractedValue::F64(f)
                }
                12 => {
                    let b = reader.read_bytes(12)?;
                    let v = [
                        f32::from_le_bytes(b[0..4].try_into().unwrap()),
                        f32::from_le_bytes(b[4..8].try_into().unwrap()),
                        f32::from_le_bytes(b[8..12].try_into().unwrap()),
                    ];
                    if !v.iter().all(|c| c.is_finite()) {
                        return Ok(None);
                    }
                    ExtractedValue::Vec3(v)
                }
                15 => {
                    let b = reader.read_bytes(12)?;
                    let v = [
                        f32::from_le_bytes(b[0..4].try_into().unwrap()),
                        f32::from_le_bytes(b[4..8].try_into().unwrap()),
                        f32::from_le_bytes(b[8..12].try_into().unwrap()),
                    ];
                    if !v.iter().all(|c| c.is_finite()) {
                        return Ok(None);
                    }
                    ExtractedValue::Color3(v)
                }
                16 => {
                    let b = reader.read_bytes(48)?;
                    let mut c = [0.0_f32; 12];
                    for (i, slot) in c.iter_mut().enumerate() {
                        *slot = f32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
                        if !slot.is_finite() {
                            return Ok(None);
                        }
                    }
                    ExtractedValue::CFrame(c)
                }
                19 => {
                    let sid = reader.read_len("enum string id")?;
                    match open.strings.get(sid) {
                        Some(BlockString::Own(text)) => ExtractedValue::EnumItem(text.clone()),
                        _ => return Ok(None),
                    }
                }
                _ => return Ok(None),
            };
            out.push((row, val));
        }
        reader.finish()?;
    }
    if groups.len() > 1 {
        out.sort_unstable_by_key(|(r, _)| *r);
    }
    Ok(Some(out))
}

#[cfg(any(windows, target_os = "macos", test))]
fn extract_block_properties(
    store: &BlockStore<'_>,
    block_index: usize,
    rows: &[SettingsBytecodeInstance],
    row_masks: &[u32],
    prop_names: &[String],
    database: &ReflectionDatabase<'_>,
) -> Result<ExtractBlockResult> {
    let block = &store.blocks[block_index];
    let block_first = block.first;
    let block_count = block.count;
    if !row_masks[block_first..block_first + block_count]
        .iter()
        .any(|&m| m != 0)
    {
        return Ok(ExtractBlockResult::Edits(Vec::new()));
    }
    let open = store.open_block(block_index)?;
    let block_props = block_property_names(store, &open)?;
    let mut reader = BytecodeReader::new(&open.content[open.rest..]);
    let specs = read_settings_property_groups(&mut reader, &block_props)?;
    reader.finish()?;

    let want_attrs = prop_names.iter().any(|p| p == "@attributes");
    let want_tags = prop_names.iter().any(|p| p.starts_with("@tag:"));
    let mut by_name: AHashMap<&str, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    for (name, kind, count, body) in specs {
        let keep = prop_names.iter().any(|p| p == name)
            || (want_attrs && name == "Attributes")
            || (want_tags && name == "Tags");
        if keep {
            by_name.entry(name).or_default().push((kind, count, body));
        }
    }

    let mut strings_cache: Option<Vec<String>> = None;
    let mut tags_cache: Option<AHashMap<usize, Value>> = None;
    let mut edits = Vec::new();
    for (prop_bit, prop_name) in prop_names.iter().enumerate() {
        let bit = 1_u32 << prop_bit;
        if prop_name == "@attributes" {
            let groups = by_name.get("Attributes").map(Vec::as_slice).unwrap_or(&[]);
            let decoded =
                decode_block_attributes(store, &open, &mut strings_cache, groups, block_count)?;
            for local_row in 0..block_count {
                let global_row = block_first + local_row;
                if row_masks[global_row] & bit == 0 {
                    continue;
                }
                let map = match decoded.get(&local_row) {
                    Some(attrs) => {
                        let mut filtered = serde_json::Map::with_capacity(attrs.len());
                        for (k, v) in attrs {
                            if is_engine_managed_attribute(k) {
                                continue;
                            }
                            if !is_bulk_attribute_value_eligible(v) {
                                return Ok(ExtractBlockResult::Fallback);
                            }
                            filtered.insert(k.clone(), v.clone());
                        }
                        filtered
                    }
                    None => serde_json::Map::new(),
                };
                edits.push(BulkEdit::AttributesReplace(global_row, map));
            }
            continue;
        }
        if let Some(tag_name) = prop_name.strip_prefix("@tag:") {
            if tags_cache.is_none() {
                let groups = by_name.get("Tags").map(Vec::as_slice).unwrap_or(&[]);
                tags_cache = Some(decode_block_property(
                    store,
                    &open,
                    &mut strings_cache,
                    "Tags",
                    groups,
                    block_count,
                )?);
            }
            let decoded_tags = tags_cache.as_ref().expect("initialized above");
            for local_row in 0..block_count {
                let global_row = block_first + local_row;
                if row_masks[global_row] & bit == 0 {
                    continue;
                }
                let has_tag = match decoded_tags.get(&local_row) {
                    Some(val) => {
                        let Some(tags) = extract_tag_slice(Some(val)) else {
                            return Ok(ExtractBlockResult::Fallback);
                        };
                        tags.contains(&tag_name)
                    }
                    None => false,
                };
                edits.push(BulkEdit::Tag(global_row, tag_name.to_string(), has_tag));
            }
            continue;
        }
        let groups = by_name
            .get(prop_name.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut default_cache: AHashMap<&str, Option<ExtractedValue>> = AHashMap::new();
        if let Some(raw_vals) = decode_block_raw_extracted_opt(groups, &open, block_count)? {
            let mut ri = 0_usize;
            for local_row in 0..block_count {
                while ri < raw_vals.len() && raw_vals[ri].0 < local_row {
                    ri += 1;
                }
                let global_row = block_first + local_row;
                if row_masks[global_row] & bit == 0 {
                    continue;
                }
                let value = if ri < raw_vals.len() && raw_vals[ri].0 == local_row {
                    raw_vals[ri].1.clone()
                } else {
                    let class_name = rows[global_row].class_name.as_str();
                    let Some(def) = default_cache
                        .entry(class_name)
                        .or_insert_with(|| {
                            if crate::settings::equivalence::reconciliation_property_is_unknown_when_absent(
                                Some(database),
                                class_name,
                                prop_name,
                            ) {
                                return None;
                            }
                            let default_val =
                                reflection_default_json(database, class_name, prop_name)?;
                            reconciliation_property_values_equal(
                                class_name,
                                prop_name,
                                None,
                                Some(&default_val),
                            )
                            .then(|| ExtractedValue::from_json(default_val))
                        })
                        .clone()
                    else {
                        return Ok(ExtractBlockResult::Fallback);
                    };
                    def
                };
                edits.push(BulkEdit::Property(global_row, prop_name.clone(), value));
            }
            continue;
        }
        let decoded = decode_block_property(
            store,
            &open,
            &mut strings_cache,
            prop_name,
            groups,
            block_count,
        )?;
        for local_row in 0..block_count {
            let global_row = block_first + local_row;
            if row_masks[global_row] & bit == 0 {
                continue;
            }
            let value = if let Some(val) = decoded.get(&local_row) {
                if holds_marker(val) || !is_bulk_plain_value_eligible(val) {
                    return Ok(ExtractBlockResult::Fallback);
                }
                ExtractedValue::from_json(val.clone())
            } else {
                let class_name = rows[global_row].class_name.as_str();
                let Some(def) = default_cache
                    .entry(class_name)
                    .or_insert_with(|| {
                        if crate::settings::equivalence::reconciliation_property_is_unknown_when_absent(
                            Some(database),
                            class_name,
                            prop_name,
                        ) {
                            return None;
                        }
                        let default_val = reflection_default_json(database, class_name, prop_name)?;
                        reconciliation_property_values_equal(
                            class_name,
                            prop_name,
                            None,
                            Some(&default_val),
                        )
                        .then(|| ExtractedValue::from_json(default_val))
                    })
                    .clone()
                else {
                    return Ok(ExtractBlockResult::Fallback);
                };
                def
            };
            edits.push(BulkEdit::Property(global_row, prop_name.clone(), value));
        }
    }
    Ok(ExtractBlockResult::Edits(edits))
}

#[cfg(any(windows, target_os = "macos", test))]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn extract_v12_bulk_properties_for_classes(
    desired_bytes: &[u8],
    service: &str,
    class_props: &[(String, String)],
    min_instances: usize,
) -> Result<BulkExtractOutcome> {
    if class_props.is_empty() || !is_block_store(desired_bytes) {
        return Ok(BulkExtractOutcome::Fallback);
    }
    let Ok(store) = BlockStore::open(desired_bytes) else {
        return Ok(BulkExtractOutcome::Fallback);
    };
    if store.rows == 0 {
        return Ok(BulkExtractOutcome::Fallback);
    }
    let Some(topo) = cached_store_topology(&store)? else {
        return Ok(BulkExtractOutcome::Fallback);
    };
    let rows = &topo.rows;
    if rows.len() != store.rows || rows[0].parent_index.is_some() || rows[0].class_name != service {
        return Ok(BulkExtractOutcome::Fallback);
    }
    let database = rbx_reflection_database::get()?;
    let Some(normalized) = normalize_zero_capture_class_props(rows, class_props, database, service)
    else {
        return Ok(BulkExtractOutcome::Fallback);
    };
    extract_v12_bulk_with_rows(&store, &topo, service, &normalized, database, min_instances)
}

fn encode_extracted_columns<'a>(
    database: &'a ReflectionDatabase<'_>,
    property_name: String,
    entries: Vec<(u32, &'a str, ExtractedValue)>,
    out: &mut Vec<BulkPropertyColumn>,
) {
    let mut f32_rows = Vec::new();
    let mut f32_vals = Vec::new();
    let mut f64_rows = Vec::new();
    let mut f64_vals = Vec::new();
    let mut i32_rows = Vec::new();
    let mut i32_vals = Vec::new();
    let mut bool_rows = Vec::new();
    let mut bool_vals = Vec::new();
    let mut vec3_rows = Vec::new();
    let mut vec3_vals = Vec::new();
    let mut color3_rows = Vec::new();
    let mut color3_vals = Vec::new();
    let mut cframe_rows = Vec::new();
    let mut cframe_vals = Vec::new();
    let mut enum_rows = Vec::new();
    let mut enum_vals: Vec<(Option<&str>, String)> = Vec::new();
    let mut other_rows = Vec::new();
    let mut other_vals = Vec::new();
    let mut dtype_cache: AHashMap<&'a str, Option<&'a RbxDataType<'_>>> = AHashMap::new();

    for (node_id, class_name, value) in entries {
        let data_type = *dtype_cache.entry(class_name).or_insert_with(|| {
            crate::rbx::encode::rbx_property_descriptor(database, class_name, &property_name)
                .map(|d| &d.data_type)
        });
        let variant_type = match data_type {
            Some(RbxDataType::Value(v)) => Some(*v),
            _ => None,
        };
        match (value, variant_type) {
            (ExtractedValue::Bool(b), Some(RbxVariantType::Bool)) => {
                bool_rows.push(node_id);
                bool_vals.push(u8::from(b));
            }
            (ExtractedValue::Int(i), Some(RbxVariantType::Int32)) => {
                if let Ok(v) = i32::try_from(i) {
                    i32_rows.push(node_id);
                    i32_vals.push(v);
                } else {
                    other_rows.push(node_id);
                    other_vals.push(json!(i));
                }
            }
            (ExtractedValue::Int(i), Some(RbxVariantType::Float32)) => {
                f32_rows.push(node_id);
                f32_vals.push(i as f32);
            }
            (ExtractedValue::Int(i), Some(RbxVariantType::Float64)) => {
                f64_rows.push(node_id);
                f64_vals.push(i as f64);
            }
            (ExtractedValue::F64(f), Some(RbxVariantType::Float32)) => {
                f32_rows.push(node_id);
                f32_vals.push(f as f32);
            }
            (ExtractedValue::F64(f), Some(RbxVariantType::Float64)) => {
                f64_rows.push(node_id);
                f64_vals.push(f);
            }
            (ExtractedValue::Vec3(v), Some(RbxVariantType::Vector3)) => {
                vec3_rows.push(node_id);
                vec3_vals.push(v);
            }
            (ExtractedValue::Color3(c), Some(RbxVariantType::Color3)) => {
                color3_rows.push(node_id);
                color3_vals.push(c);
            }
            (ExtractedValue::CFrame(cf), Some(RbxVariantType::CFrame)) => {
                cframe_rows.push(node_id);
                cframe_vals.push(cf);
            }
            (ExtractedValue::EnumItem(name), _) => {
                let enum_name = match data_type {
                    Some(RbxDataType::Enum(e)) => Some(*e),
                    _ => None,
                };
                enum_rows.push(node_id);
                enum_vals.push((enum_name, name));
            }
            (other, _) => {
                let mut json_val = other.into_json();
                enrich_enum_item_type(data_type, &mut json_val);
                other_rows.push(node_id);
                other_vals.push(json_val);
            }
        }
    }

    if !f32_rows.is_empty() {
        let first = f32_vals[0];
        let values = if f32_vals.iter().all(|v| v.to_bits() == first.to_bits()) {
            BulkPropertyValues::F32Const(first)
        } else {
            BulkPropertyValues::F32(f32_vals)
        };
        out.push(BulkPropertyColumn {
            kind: BulkColumnKind::Property,
            property_name: property_name.clone(),
            rows: f32_rows,
            values,
        });
    }
    if !f64_rows.is_empty() {
        let first = f64_vals[0];
        let values = if f64_vals.iter().all(|v| v.to_bits() == first.to_bits()) {
            BulkPropertyValues::F64Const(first)
        } else {
            BulkPropertyValues::F64(f64_vals)
        };
        out.push(BulkPropertyColumn {
            kind: BulkColumnKind::Property,
            property_name: property_name.clone(),
            rows: f64_rows,
            values,
        });
    }
    if !i32_rows.is_empty() {
        out.push(BulkPropertyColumn {
            kind: BulkColumnKind::Property,
            property_name: property_name.clone(),
            rows: i32_rows,
            values: BulkPropertyValues::I32(i32_vals),
        });
    }
    if !bool_rows.is_empty() {
        let first = bool_vals[0];
        let values = if bool_vals.iter().all(|&v| v == first) {
            BulkPropertyValues::BoolConst(first != 0)
        } else {
            BulkPropertyValues::Bool(bool_vals)
        };
        out.push(BulkPropertyColumn {
            kind: BulkColumnKind::Property,
            property_name: property_name.clone(),
            rows: bool_rows,
            values,
        });
    }
    if !vec3_rows.is_empty() {
        out.push(BulkPropertyColumn {
            kind: BulkColumnKind::Property,
            property_name: property_name.clone(),
            rows: vec3_rows,
            values: BulkPropertyValues::Vector3(vec3_vals),
        });
    }
    if !color3_rows.is_empty() {
        out.push(BulkPropertyColumn {
            kind: BulkColumnKind::Property,
            property_name: property_name.clone(),
            rows: color3_rows,
            values: BulkPropertyValues::Color3(color3_vals),
        });
    }
    if !cframe_rows.is_empty() {
        out.push(BulkPropertyColumn {
            kind: BulkColumnKind::Property,
            property_name: property_name.clone(),
            rows: cframe_rows,
            values: BulkPropertyValues::CFrame(cframe_vals),
        });
    }
    if !enum_rows.is_empty() {
        let mut palette: Vec<Value> = Vec::new();
        let mut by_key: AHashMap<(Option<&str>, String), u32> = AHashMap::new();
        let mut indices = Vec::with_capacity(enum_vals.len());
        for (enum_name, item_name) in enum_vals {
            let key = (enum_name, item_name);
            let idx = if let Some(&idx) = by_key.get(&key) {
                idx
            } else {
                let idx = palette.len() as u32;
                let entry = match key.0 {
                    Some(en) => json!({
                        "_type": "EnumItem",
                        "enumType": format!("Enum.{en}"),
                        "name": key.1,
                    }),
                    None => json!({ "_type": "EnumItem", "name": key.1 }),
                };
                palette.push(entry);
                by_key.insert(key, idx);
                idx
            };
            indices.push(idx);
        }
        out.push(BulkPropertyColumn {
            kind: BulkColumnKind::Property,
            property_name: property_name.clone(),
            rows: enum_rows,
            values: BulkPropertyValues::Indexed { palette, indices },
        });
    }
    if !other_rows.is_empty() {
        out.push(build_indexed_column(
            BulkColumnKind::Property,
            property_name,
            other_rows,
            other_vals,
        ));
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn extract_model_assertion_columns(
    store: &BlockStore<'_>,
    rows: &[SettingsBytecodeInstance],
    node_to_row: &[usize],
    database: &ReflectionDatabase<'_>,
) -> Result<Option<Vec<BulkPropertyColumn>>> {
    let mut by_block: BTreeMap<usize, Vec<(usize, u32)>> = BTreeMap::new();
    let mut is_model_cache: AHashMap<&str, bool> = AHashMap::new();
    for (idx, &row_idx) in node_to_row.iter().enumerate() {
        let class_name = rows[row_idx].class_name.as_str();
        let is_model = *is_model_cache.entry(class_name).or_insert_with(|| {
            crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "Model")
        });
        if is_model {
            let block_idx = store.block_of(row_idx);
            let local_row = row_idx - store.blocks[block_idx].first;
            by_block
                .entry(block_idx)
                .or_default()
                .push((local_row, (idx + 1) as u32));
        }
    }
    if by_block.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let blocks: Vec<(usize, Vec<(usize, u32)>)> = by_block.into_iter().collect();
    type BlockModelRows = (Vec<(u32, f32)>, Vec<(u32, [f32; 12])>, Vec<u32>);
    let block_results = blocks
        .par_iter()
        .map(|(block_idx, targets)| -> Result<Option<BlockModelRows>> {
            let block_count = store.blocks[*block_idx].count;
            let open = store.open_block(*block_idx)?;
            let block_props = block_property_names(store, &open)?;
            let mut reader = BytecodeReader::new(&open.content[open.rest..]);
            let specs = read_settings_property_groups(&mut reader, &block_props)?;
            reader.finish()?;
            let mut scale_groups: Vec<PropertyGroupSlice<'_>> = Vec::new();
            let mut pivot_groups: Vec<PropertyGroupSlice<'_>> = Vec::new();
            for (name, kind, count, body) in specs {
                if name == "Scale" {
                    scale_groups.push((kind, count, body));
                } else if name == "WorldPivot" || name == "WorldPivotData" {
                    pivot_groups.push((kind, count, body));
                }
            }
            let mut strings_cache: Option<Vec<String>> = None;
            let scale_map = decode_block_property(
                store,
                &open,
                &mut strings_cache,
                "Scale",
                &scale_groups,
                block_count,
            )?;
            let pivot_map = decode_block_property(
                store,
                &open,
                &mut strings_cache,
                "WorldPivot",
                &pivot_groups,
                block_count,
            )?;
            let mut scales = Vec::with_capacity(targets.len());
            let mut pivots = Vec::new();
            let mut unset_pivots = Vec::new();
            for &(local_row, node_id) in targets {
                let scale = match scale_map.get(&local_row) {
                    Some(val) => match val.as_f64() {
                        Some(f) if f.is_finite() => f as f32,
                        _ => return Ok(None),
                    },
                    None => 1.0_f32,
                };
                scales.push((node_id, scale));
                match pivot_map.get(&local_row) {
                    Some(Value::Null) | None => unset_pivots.push(node_id),
                    Some(pval) => match ExtractedValue::from_json(pval.clone()) {
                        ExtractedValue::CFrame(cf) => pivots.push((node_id, cf)),
                        _ => return Ok(None),
                    },
                }
            }
            Ok(Some((scales, pivots, unset_pivots)))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut all_scales: Vec<(u32, f32)> = Vec::new();
    let mut all_pivots: Vec<(u32, [f32; 12])> = Vec::new();
    let mut all_unset: Vec<u32> = Vec::new();
    for part in block_results {
        let Some((mut s, mut p, mut u)) = part else {
            return Ok(None);
        };
        all_scales.append(&mut s);
        all_pivots.append(&mut p);
        all_unset.append(&mut u);
    }
    let mut cols = Vec::new();
    if !all_scales.is_empty() {
        all_scales.sort_unstable_by_key(|&(node_id, _)| node_id);
        let (rows_out, vals_out): (Vec<u32>, Vec<f32>) = all_scales.into_iter().unzip();
        let first = vals_out[0];
        let values = if vals_out.iter().all(|v| v.to_bits() == first.to_bits()) {
            BulkPropertyValues::F32Const(first)
        } else {
            BulkPropertyValues::F32(vals_out)
        };
        cols.push(BulkPropertyColumn {
            kind: BulkColumnKind::AssertModelScale,
            property_name: "Scale".to_string(),
            rows: rows_out,
            values,
        });
    }
    if !all_pivots.is_empty() {
        all_pivots.sort_unstable_by_key(|&(node_id, _)| node_id);
        let (rows_out, vals_out): (Vec<u32>, Vec<[f32; 12]>) = all_pivots.into_iter().unzip();
        cols.push(BulkPropertyColumn {
            kind: BulkColumnKind::AssertModelPivot,
            property_name: "WorldPivot".to_string(),
            rows: rows_out,
            values: BulkPropertyValues::CFrame(vals_out),
        });
    }
    if !all_unset.is_empty() {
        all_unset.sort_unstable();
        cols.push(BulkPropertyColumn {
            kind: BulkColumnKind::AssertModelPivot,
            property_name: "WorldPivotUnset".to_string(),
            rows: all_unset,
            values: BulkPropertyValues::BoolConst(true),
        });
    }
    Ok(Some(cols))
}

#[cfg(any(windows, target_os = "macos", test))]
fn extract_v12_bulk_with_rows(
    store: &BlockStore<'_>,
    topo: &CachedStoreTopology,
    service: &str,
    normalized: &NormalizedZeroCaptureProps,
    database: &ReflectionDatabase<'_>,
    min_instances: usize,
) -> Result<BulkExtractOutcome> {
    let rows = &topo.rows;
    let prop_names = &normalized.prop_names;
    let class_masks = &normalized.class_masks;
    let Some((row_masks, target_count)) =
        classify_extract_targets(rows, prop_names, class_masks, database, service)
    else {
        return Ok(BulkExtractOutcome::Fallback);
    };
    if target_count < min_instances {
        return Ok(BulkExtractOutcome::Fallback);
    }
    let block_results = (0..store.blocks.len())
        .into_par_iter()
        .map(|block_index| {
            extract_block_properties(store, block_index, rows, &row_masks, prop_names, database)
        })
        .collect::<Result<Vec<_>>>()?;
    let total_edits: usize = block_results
        .iter()
        .map(|r| match r {
            ExtractBlockResult::Edits(v) => v.len(),
            ExtractBlockResult::Fallback => 0,
        })
        .sum();
    let mut all_edits = Vec::with_capacity(total_edits);
    for result in block_results {
        match result {
            ExtractBlockResult::Fallback => return Ok(BulkExtractOutcome::Fallback),
            ExtractBlockResult::Edits(mut part) => all_edits.append(&mut part),
        }
    }
    match build_bulk_property_patch_with_rows(
        rows,
        &topo.sibling_ordinals,
        &topo.package_root_of,
        all_edits,
        database,
        service,
        min_instances,
    ) {
        Some((mut patch, node_to_row)) => {
            let Some(mut assert_cols) =
                extract_model_assertion_columns(store, rows, &node_to_row, database)?
            else {
                return Ok(BulkExtractOutcome::Fallback);
            };
            if !assert_cols.is_empty() {
                assert_cols.append(&mut patch.columns);
                patch.columns = assert_cols;
            }
            Ok(BulkExtractOutcome::Patch(BulkExtractedPatch {
                patch,
                node_to_row,
            }))
        }
        None => Ok(BulkExtractOutcome::Fallback),
    }
}

#[cfg(any(windows, target_os = "macos", test))]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn push_scalar_or_fallback<'a>(
    store: &BlockStore<'_>,
    prop_name: &'a str,
    global_row: usize,
    scalar: ScalarPatchValue,
    scalar_by_block: &mut [BTreeMap<&'a str, Vec<(usize, ScalarPatchValue)>>],
) -> Result<()> {
    anyhow::ensure!(global_row < store.rows, "Bulk patch row out of range");
    let block_idx = store.block_of(global_row);
    let local_row = global_row - store.blocks[block_idx].first;
    scalar_by_block[block_idx]
        .entry(prop_name)
        .or_default()
        .push((local_row, scalar));
    Ok(())
}

#[cfg(any(windows, target_os = "macos", test))]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn decode_binary_old_scalars<'a>(
    store: &BlockStore<'_>,
    prop_name: &'a str,
    encoding: &str,
    count: usize,
    val_bytes: &[u8],
    resolve_row: &dyn Fn(usize) -> Result<usize>,
    scalar_by_block: &mut [BTreeMap<&'a str, Vec<(usize, ScalarPatchValue)>>],
) -> Result<()> {
    match encoding {
        "f32" => {
            anyhow::ensure!(val_bytes.len() == count * 4, "Bulk f32 size mismatch");
            for i in 0..count {
                let f = f32::from_le_bytes(val_bytes[i * 4..i * 4 + 4].try_into().unwrap());
                anyhow::ensure!(f.is_finite(), "Bulk old f32 value is non-finite");
                push_scalar_or_fallback(
                    store,
                    prop_name,
                    resolve_row(i)?,
                    ScalarPatchValue::Float32(f),
                    scalar_by_block,
                )?;
            }
        }
        "f64" => {
            anyhow::ensure!(val_bytes.len() == count * 8, "Bulk f64 size mismatch");
            for i in 0..count {
                let f = f64::from_le_bytes(val_bytes[i * 8..i * 8 + 8].try_into().unwrap());
                anyhow::ensure!(f.is_finite(), "Bulk old f64 value is non-finite");
                let scalar = match exact_f32(f) {
                    Some(v) => ScalarPatchValue::Float32(v),
                    None => ScalarPatchValue::Float64(f),
                };
                push_scalar_or_fallback(
                    store,
                    prop_name,
                    resolve_row(i)?,
                    scalar,
                    scalar_by_block,
                )?;
            }
        }
        "i32" => {
            anyhow::ensure!(val_bytes.len() == count * 4, "Bulk i32 size mismatch");
            for i in 0..count {
                let v = i32::from_le_bytes(val_bytes[i * 4..i * 4 + 4].try_into().unwrap());
                push_scalar_or_fallback(
                    store,
                    prop_name,
                    resolve_row(i)?,
                    ScalarPatchValue::Int(i64::from(v)),
                    scalar_by_block,
                )?;
            }
        }
        "bool" => {
            anyhow::ensure!(val_bytes.len() == count, "Bulk bool size mismatch");
            for (i, &b) in val_bytes.iter().enumerate() {
                push_scalar_or_fallback(
                    store,
                    prop_name,
                    resolve_row(i)?,
                    ScalarPatchValue::Bool(b != 0),
                    scalar_by_block,
                )?;
            }
        }
        "vector3" => {
            anyhow::ensure!(val_bytes.len() == count * 12, "Bulk vector3 size mismatch");
            for i in 0..count {
                let off = i * 12;
                let v = [
                    f32::from_le_bytes(val_bytes[off..off + 4].try_into().unwrap()),
                    f32::from_le_bytes(val_bytes[off + 4..off + 8].try_into().unwrap()),
                    f32::from_le_bytes(val_bytes[off + 8..off + 12].try_into().unwrap()),
                ];
                anyhow::ensure!(
                    v.iter().all(|c| c.is_finite()),
                    "Bulk old vector3 is non-finite"
                );
                push_scalar_or_fallback(
                    store,
                    prop_name,
                    resolve_row(i)?,
                    ScalarPatchValue::Vector3(v),
                    scalar_by_block,
                )?;
            }
        }
        "color3" => {
            anyhow::ensure!(val_bytes.len() == count * 12, "Bulk color3 size mismatch");
            for i in 0..count {
                let off = i * 12;
                let c = [
                    f32::from_le_bytes(val_bytes[off..off + 4].try_into().unwrap()),
                    f32::from_le_bytes(val_bytes[off + 4..off + 8].try_into().unwrap()),
                    f32::from_le_bytes(val_bytes[off + 8..off + 12].try_into().unwrap()),
                ];
                anyhow::ensure!(
                    c.iter().all(|v| v.is_finite()),
                    "Bulk old color3 is non-finite"
                );
                push_scalar_or_fallback(
                    store,
                    prop_name,
                    resolve_row(i)?,
                    ScalarPatchValue::Color3(c),
                    scalar_by_block,
                )?;
            }
        }
        "cframe" => {
            anyhow::ensure!(val_bytes.len() == count * 48, "Bulk cframe size mismatch");
            for i in 0..count {
                let off = i * 48;
                let mut comps = [0.0_f32; 12];
                for (c, slot) in comps.iter_mut().enumerate() {
                    let bo = off + c * 4;
                    *slot = f32::from_le_bytes(val_bytes[bo..bo + 4].try_into().unwrap());
                    anyhow::ensure!(slot.is_finite(), "Bulk old cframe is non-finite");
                }
                push_scalar_or_fallback(
                    store,
                    prop_name,
                    resolve_row(i)?,
                    ScalarPatchValue::CFrame(comps),
                    scalar_by_block,
                )?;
            }
        }
        _ => bail!("Unsupported bulk old value encoding {encoding}"),
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos", test))]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn patch_v12_bulk_old_value_columns(
    base_bytes: &[u8],
    columns: &[Value],
    node_to_row: &[usize],
) -> Result<Vec<u8>> {
    if columns.is_empty() {
        return Ok(base_bytes.to_vec());
    }
    let store = BlockStore::open(base_bytes)?;
    let mut scalar_by_block: Vec<BTreeMap<&str, Vec<(usize, ScalarPatchValue)>>> =
        (0..store.blocks.len()).map(|_| BTreeMap::new()).collect();
    let mut json_by_block: Vec<BTreeMap<&str, Vec<(usize, Value)>>> =
        (0..store.blocks.len()).map(|_| BTreeMap::new()).collect();
    let mut tag_by_block: Vec<Vec<(usize, &str, bool)>> =
        (0..store.blocks.len()).map(|_| Vec::new()).collect();
    let mut attr_replace_by_block: Vec<Vec<(usize, serde_json::Map<String, Value>)>> =
        (0..store.blocks.len()).map(|_| Vec::new()).collect();
    let mut attr_key_updates: Vec<(usize, String, Option<Value>)> = Vec::new();
    let mut fallback_updates: Vec<(usize, String, Value)> = Vec::new();
    let mut any_row = false;

    for column in columns {
        let col_kind = column
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("property");
        let prop_name = column["propertyName"]
            .as_str()
            .context("Bulk old value column missing propertyName")?;
        let encoding = column["encoding"]
            .as_str()
            .context("Bulk old value column missing encoding")?;
        let count = column["count"]
            .as_u64()
            .context("Bulk old value column missing count")? as usize;
        if count == 0 {
            continue;
        }
        any_row = true;
        let rows_b64 = column["rows"]
            .as_str()
            .context("Bulk old value column missing rows")?;
        let row_bytes = base64::decode(rows_b64).context("Invalid bulk old value rows base64")?;
        anyhow::ensure!(
            row_bytes.len() == count * 4,
            "Bulk old value rows size mismatch"
        );
        let resolve_row = |idx: usize| -> Result<usize> {
            let off = idx * 4;
            let node_id = u32::from_le_bytes(row_bytes[off..off + 4].try_into().unwrap()) as usize;
            if node_id == 0 {
                return Ok(0);
            }
            node_to_row
                .get(node_id - 1)
                .copied()
                .with_context(|| format!("Bulk old value references unknown node {node_id}"))
        };
        if col_kind == "tag" {
            let vals_b64 = column["values"]
                .as_str()
                .context("Bulk old tag column missing values")?;
            let val_bytes =
                base64::decode(vals_b64).context("Invalid bulk old tag values base64")?;
            anyhow::ensure!(val_bytes.len() == count, "Bulk old tag size mismatch");
            for (i, &b) in val_bytes.iter().enumerate() {
                let row = resolve_row(i)?;
                anyhow::ensure!(row < store.rows, "Bulk patch row out of range");
                let block_idx = store.block_of(row);
                let local_row = row - store.blocks[block_idx].first;
                tag_by_block[block_idx].push((local_row, prop_name, b != 0));
            }
            continue;
        }
        if col_kind == "attributes_replace" || col_kind == "attribute" {
            let palette = column["palette"]
                .as_array()
                .context("Bulk old attribute column missing palette")?;
            let indices_b64 = column["indices"]
                .as_str()
                .context("Bulk old attribute column missing indices")?;
            let idx_bytes =
                base64::decode(indices_b64).context("Invalid bulk old attribute indices base64")?;
            anyhow::ensure!(
                idx_bytes.len() == count * 4,
                "Bulk old attribute indices size mismatch"
            );
            for i in 0..count {
                let row = resolve_row(i)?;
                anyhow::ensure!(row < store.rows, "Bulk patch row out of range");
                let p_idx =
                    u32::from_le_bytes(idx_bytes[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
                anyhow::ensure!(
                    p_idx < palette.len(),
                    "Bulk old attribute palette index out of range"
                );
                if col_kind == "attributes_replace" {
                    let map = palette[p_idx].as_object().cloned().unwrap_or_default();
                    let block_idx = store.block_of(row);
                    let local_row = row - store.blocks[block_idx].first;
                    attr_replace_by_block[block_idx].push((local_row, map));
                } else {
                    let is_delete = palette[p_idx].is_null()
                        || palette[p_idx]
                            .as_object()
                            .and_then(|o| o.get("_type"))
                            .and_then(Value::as_str)
                            == Some("AttributeDelete");
                    let val = (!is_delete).then(|| palette[p_idx].clone());
                    attr_key_updates.push((row, prop_name.to_string(), val));
                }
            }
            continue;
        }
        if encoding == "indexed" {
            let palette = column["palette"]
                .as_array()
                .context("Bulk old value indexed column missing palette")?;
            let indices_b64 = column["indices"]
                .as_str()
                .context("Bulk old value indexed column missing indices")?;
            let idx_bytes =
                base64::decode(indices_b64).context("Invalid bulk old value indices base64")?;
            anyhow::ensure!(
                idx_bytes.len() == count * 4,
                "Bulk old value indices size mismatch"
            );
            let scalar_palette: Vec<Option<ScalarPatchValue>> =
                palette.iter().map(ScalarPatchValue::from_json).collect();
            let plain_json_palette: Vec<bool> =
                palette.iter().map(json_value_fits_block_inline).collect();
            for i in 0..count {
                let row = resolve_row(i)?;
                anyhow::ensure!(row < store.rows, "Bulk patch row out of range");
                let p_idx =
                    u32::from_le_bytes(idx_bytes[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
                anyhow::ensure!(
                    p_idx < palette.len(),
                    "Bulk old value palette index out of range"
                );
                if let Some(scalar) = &scalar_palette[p_idx] {
                    push_scalar_or_fallback(
                        &store,
                        prop_name,
                        row,
                        scalar.clone(),
                        &mut scalar_by_block,
                    )?;
                } else if plain_json_palette[p_idx] {
                    let block_idx = store.block_of(row);
                    let local_row = row - store.blocks[block_idx].first;
                    json_by_block[block_idx]
                        .entry(prop_name)
                        .or_default()
                        .push((local_row, palette[p_idx].clone()));
                } else {
                    fallback_updates.push((row, prop_name.to_string(), palette[p_idx].clone()));
                }
            }
        } else if encoding == "json" {
            let values = column["values"]
                .as_array()
                .context("Bulk old value json column missing values array")?;
            anyhow::ensure!(values.len() == count, "Bulk old value json length mismatch");
            for (idx, val) in values.iter().enumerate() {
                let row = resolve_row(idx)?;
                anyhow::ensure!(row < store.rows, "Bulk patch row out of range");
                if let Some(scalar) = ScalarPatchValue::from_json(val) {
                    push_scalar_or_fallback(&store, prop_name, row, scalar, &mut scalar_by_block)?;
                } else if json_value_fits_block_inline(val) {
                    let block_idx = store.block_of(row);
                    let local_row = row - store.blocks[block_idx].first;
                    json_by_block[block_idx]
                        .entry(prop_name)
                        .or_default()
                        .push((local_row, val.clone()));
                } else {
                    fallback_updates.push((row, prop_name.to_string(), val.clone()));
                }
            }
        } else {
            let vals_b64 = column["values"]
                .as_str()
                .context("Bulk old value column missing values")?;
            let val_bytes =
                base64::decode(vals_b64).context("Invalid bulk old value data base64")?;
            decode_binary_old_scalars(
                &store,
                prop_name,
                encoding,
                count,
                &val_bytes,
                &resolve_row,
                &mut scalar_by_block,
            )?;
        }
    }

    if !any_row {
        return Ok(base_bytes.to_vec());
    }
    apply_scalar_block_patches(
        &store,
        base_bytes,
        &scalar_by_block,
        &json_by_block,
        &tag_by_block,
        &attr_replace_by_block,
        &attr_key_updates,
        &fallback_updates,
    )
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Clone)]
enum ScalarPatchValue {
    Bool(bool),
    Int(i64),
    Float32(f32),
    Float64(f64),
    String(String),
    RawStringId(usize),
    Vector3([f32; 3]),
    Color3([f32; 3]),
    CFrame([f32; 12]),
    EnumItem(Option<String>, String),
}

#[cfg(any(windows, target_os = "macos", test))]
impl ScalarPatchValue {
    fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(b) => Some(Self::Bool(*b)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Some(Self::Int(i))
                } else {
                    let f = n.as_f64()?;
                    if !f.is_finite() {
                        return None;
                    }
                    match exact_f32(f) {
                        Some(f32_val) => Some(Self::Float32(f32_val)),
                        None => Some(Self::Float64(f)),
                    }
                }
            }
            Value::String(s) if s.len() < POOL_MIN_BYTES && !holds_marker(value) => {
                Some(Self::String(s.clone()))
            }
            Value::Object(obj) => {
                let kind = obj.get("_type")?.as_str()?;
                if kind == "Vector3" && obj.len() == 4 {
                    let x = obj.get("x")?.as_f64()?;
                    let y = obj.get("y")?.as_f64()?;
                    let z = obj.get("z")?.as_f64()?;
                    return (x.is_finite() && y.is_finite() && z.is_finite())
                        .then_some(Self::Vector3([x as f32, y as f32, z as f32]));
                }
                if kind == "Color3" && obj.len() == 4 {
                    let r = obj.get("r")?.as_f64()?;
                    let g = obj.get("g")?.as_f64()?;
                    let b = obj.get("b")?.as_f64()?;
                    return (r.is_finite() && g.is_finite() && b.is_finite())
                        .then_some(Self::Color3([r as f32, g as f32, b as f32]));
                }
                if kind == "CFrame"
                    && let Some(arr) = obj.get("components").and_then(Value::as_array)
                    && arr.len() == 12
                    && let Some(vals) = arr
                        .iter()
                        .map(|v| v.as_f64().filter(|f| f.is_finite()).map(|f| f as f32))
                        .collect::<Option<Vec<_>>>()
                {
                    return Some(Self::CFrame(vals.try_into().unwrap()));
                }
                if kind == "EnumItem"
                    && let Some(name) = obj.get("name").and_then(Value::as_str)
                    && !name.is_empty()
                    && name.len() < POOL_MIN_BYTES
                    && !holds_marker(value)
                {
                    let enum_type = obj
                        .get("enumType")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    return Some(Self::EnumItem(enum_type, name.to_string()));
                }
                None
            }
            _ => None,
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Bool(b) => Value::Bool(*b),
            Self::Int(v) => json!(*v),
            Self::Float32(v) => json_number_f64(f64::from(*v)),
            Self::Float64(v) => json_number_f64(*v),
            Self::String(s) => Value::String(s.clone()),
            Self::RawStringId(_) => {
                unreachable!("RawStringId is only used inside patch_scalar_block")
            }
            Self::Vector3([x, y, z]) => {
                json!({"_type": "Vector3", "x": f64::from(*x), "y": f64::from(*y), "z": f64::from(*z)})
            }
            Self::Color3([r, g, b]) => {
                json!({"_type": "Color3", "r": f64::from(*r), "g": f64::from(*g), "b": f64::from(*b)})
            }
            Self::CFrame(c) => {
                let comps: Vec<Value> = c.iter().map(|&v| json_number_f64(f64::from(v))).collect();
                json!({"_type": "CFrame", "components": comps})
            }
            Self::EnumItem(Some(enum_type), name) => {
                json!({"_type": "EnumItem", "enumType": enum_type, "name": name})
            }
            Self::EnumItem(None, name) => json!({"_type": "EnumItem", "name": name}),
        }
    }

    fn kind(&self) -> u8 {
        match self {
            Self::Bool(false) => 1,
            Self::Bool(true) => 2,
            Self::Int(_) => 3,
            Self::Float64(_) => 5,
            Self::String(_) | Self::RawStringId(_) => 6,
            Self::Vector3(_) => 12,
            Self::Color3(_) => 15,
            Self::CFrame(_) => 16,
            Self::EnumItem(_, _) => 19,
            Self::Float32(_) => 20,
        }
    }

    fn write_payload(
        &self,
        out: &mut Vec<u8>,
        intern: &mut impl FnMut(&str) -> usize,
    ) -> Result<()> {
        match self {
            Self::Bool(_) => {}
            Self::Int(v) => write_var_u64(out, zigzag_i64(*v))?,
            Self::Float32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::Float64(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::String(text) => {
                let id = intern(text);
                write_var_u64(out, id as u64)?;
            }
            Self::RawStringId(id) => {
                write_var_u64(out, *id as u64)?;
            }
            Self::Vector3([x, y, z]) | Self::Color3([x, y, z]) => {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out.extend_from_slice(&z.to_le_bytes());
            }
            Self::CFrame(components) => {
                for &c in components {
                    out.extend_from_slice(&c.to_le_bytes());
                }
            }
            Self::EnumItem(_, name) => {
                let id = intern(name);
                write_var_u64(out, id as u64)?;
            }
        }
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn is_scalar_group_kind(kind: u8) -> bool {
    matches!(kind, 1 | 2 | 3 | 5 | 6 | 12 | 15 | 16 | 19 | 20)
}

#[cfg(any(windows, target_os = "macos", test))]
fn read_raw_block_groups(bytes: &[u8]) -> Result<Vec<RawBlockGroup<'_>>> {
    let mut reader = BytecodeReader::new(bytes);
    let group_count = reader.read_collection_len("property group count")?;
    let mut out = Vec::with_capacity(reader.capacity(group_count, 4));
    for _ in 0..group_count {
        let property_id = reader.read_len("property id")?;
        let kind = reader.read_u8().context("Missing property kind")?;
        let value_count = reader.read_collection_len("property value count")?;
        let body_len = reader.read_len("property group byte length")?;
        let body = reader.read_bytes(body_len)?;
        out.push((property_id, kind, value_count, body));
    }
    reader.finish()?;
    Ok(out)
}

#[cfg(any(windows, target_os = "macos", test))]
enum PatchedGroup {
    Fallback,
    Empty,
    Group(usize, Vec<u8>),
}

#[cfg(any(windows, target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn build_patched_tags_group(
    store: &BlockStore<'_>,
    open: &OpenBlock,
    strings_cache: &mut Option<Vec<String>>,
    tags_groups: &[PropertyGroupSlice<'_>],
    block_count: usize,
    tag_updates: &[(usize, &str, bool)],
    extra_strings: &mut Vec<String>,
    base_string_count: usize,
) -> Result<PatchedGroup> {
    let existing =
        decode_block_property(store, open, strings_cache, "Tags", tags_groups, block_count)?;
    let mut merged: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (row, val) in existing {
        let Some(tags) = extract_tag_slice(Some(&val)) else {
            return Ok(PatchedGroup::Fallback);
        };
        if tags.iter().any(|t| t.len() >= POOL_MIN_BYTES) {
            return Ok(PatchedGroup::Fallback);
        }
        merged.insert(row, tags.into_iter().map(str::to_string).collect());
    }
    for &(local_row, tag_name, has_tag) in tag_updates {
        if tag_name.len() >= POOL_MIN_BYTES {
            return Ok(PatchedGroup::Fallback);
        }
        let entry = merged.entry(local_row).or_default();
        if has_tag {
            if !entry.iter().any(|t| t == tag_name) {
                entry.push(tag_name.to_string());
                entry.sort();
            }
        } else {
            entry.retain(|t| t != tag_name);
        }
    }
    let non_empty: Vec<(usize, Vec<String>)> = merged
        .into_iter()
        .filter(|(_, tags)| !tags.is_empty())
        .collect();
    if non_empty.is_empty() {
        return Ok(PatchedGroup::Empty);
    }
    let mut body = Vec::with_capacity(non_empty.len() * 8);
    let mut prev_row = 0_usize;
    for (row, tags) in &non_empty {
        write_property_group_instance_index(&mut body, &mut prev_row, *row)?;
        write_var_u64(&mut body, tags.len() as u64)?;
        for tag in tags {
            body.push(6_u8);
            let id = intern_block_own_string(open, extra_strings, base_string_count, tag);
            write_var_u64(&mut body, id as u64)?;
        }
    }
    Ok(PatchedGroup::Group(non_empty.len(), body))
}

#[cfg(any(windows, target_os = "macos", test))]
fn json_value_fits_block_inline(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) => true,
        Value::Number(n) => {
            n.as_i64().is_some() || n.as_u64().is_some() || n.as_f64().is_some_and(f64::is_finite)
        }
        Value::String(s) => s.len() < POOL_MIN_BYTES && !holds_marker(value),
        Value::Array(items) => items.iter().all(json_value_fits_block_inline),
        Value::Object(map) => {
            if map.get("_type").and_then(Value::as_str) == Some("Ref") {
                return false;
            }
            map.iter()
                .all(|(k, v)| k.len() < POOL_MIN_BYTES && json_value_fits_block_inline(v))
        }
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn plain_json_value_kind(value: &Value) -> Option<u8> {
    match value {
        Value::Null => Some(0),
        Value::Bool(false) => Some(1),
        Value::Bool(true) => Some(2),
        Value::Number(n) => {
            if n.as_i64().is_some() {
                Some(3)
            } else if n.as_u64().is_some() {
                Some(4)
            } else if n.as_f64().is_some_and(f64::is_finite) {
                Some(5)
            } else {
                None
            }
        }
        Value::String(_) => Some(6),
        Value::Array(_) => Some(7),
        Value::Object(_) => Some(8),
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn write_plain_json_value_payload(
    value: &Value,
    kind: u8,
    body: &mut Vec<u8>,
    intern_str: &mut impl FnMut(&str) -> usize,
) -> Result<()> {
    match (kind, value) {
        (0..=2, _) => Ok(()),
        (3, Value::Number(n)) => {
            let v = n.as_i64().context("Expected i64 JSON value")?;
            write_var_u64(body, zigzag_i64(v))
        }
        (4, Value::Number(n)) => {
            let v = n.as_u64().context("Expected u64 JSON value")?;
            write_var_u64(body, v)
        }
        (5, Value::Number(n)) => {
            let v = n.as_f64().context("Expected f64 JSON value")?;
            body.extend_from_slice(&v.to_le_bytes());
            Ok(())
        }
        (6, Value::String(s)) => {
            let id = intern_str(s);
            write_var_u64(body, id as u64)
        }
        (7, Value::Array(items)) => {
            write_var_u64(body, items.len() as u64)?;
            for item in items {
                let item_kind =
                    plain_json_value_kind(item).context("Unsupported inline JSON array element")?;
                body.push(item_kind);
                write_plain_json_value_payload(item, item_kind, body, intern_str)?;
            }
            Ok(())
        }
        (8, Value::Object(map)) => {
            write_var_u64(body, map.len() as u64)?;
            for (k, v) in map {
                let kid = intern_str(k);
                write_var_u64(body, kid as u64)?;
                let v_kind =
                    plain_json_value_kind(v).context("Unsupported inline JSON object field")?;
                body.push(v_kind);
                write_plain_json_value_payload(v, v_kind, body, intern_str)?;
            }
            Ok(())
        }
        _ => anyhow::bail!("Mismatched inline JSON value kind {kind}"),
    }
}

#[cfg(any(windows, target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn build_patched_attrs_group(
    store: &BlockStore<'_>,
    open: &OpenBlock,
    strings_cache: &mut Option<Vec<String>>,
    attrs_groups: &[PropertyGroupSlice<'_>],
    block_count: usize,
    attr_replaces: &[(usize, serde_json::Map<String, Value>)],
    extra_strings: &mut Vec<String>,
    base_string_count: usize,
) -> Result<PatchedGroup> {
    let existing = decode_block_attributes(store, open, strings_cache, attrs_groups, block_count)?;
    let mut merged: BTreeMap<usize, serde_json::Map<String, Value>> = BTreeMap::new();
    for (row, map) in existing {
        if !map.is_empty() {
            if !map
                .iter()
                .all(|(k, v)| k.len() < POOL_MIN_BYTES && json_value_fits_block_inline(v))
            {
                return Ok(PatchedGroup::Fallback);
            }
            merged.insert(row, map);
        }
    }
    for (local_row, map) in attr_replaces {
        let mut filtered = serde_json::Map::with_capacity(map.len());
        for (k, v) in map {
            if is_engine_managed_attribute(k) {
                continue;
            }
            if k.len() >= POOL_MIN_BYTES || !json_value_fits_block_inline(v) {
                return Ok(PatchedGroup::Fallback);
            }
            filtered.insert(k.clone(), v.clone());
        }
        if filtered.is_empty() {
            merged.remove(local_row);
        } else {
            merged.insert(*local_row, filtered);
        }
    }
    if merged.is_empty() {
        return Ok(PatchedGroup::Empty);
    }
    let mut body = Vec::with_capacity(merged.len() * 16);
    let mut prev_row = 0_usize;
    let mut intern_str =
        |text: &str| intern_block_own_string(open, extra_strings, base_string_count, text);
    for (row, map) in &merged {
        write_property_group_instance_index(&mut body, &mut prev_row, *row)?;
        write_var_u64(&mut body, map.len() as u64)?;
        for (k, val) in map {
            let kid = intern_str(k);
            write_var_u64(&mut body, kid as u64)?;
            if let Some(scalar) = ScalarPatchValue::from_json(val)
                && !matches!(scalar, ScalarPatchValue::EnumItem(_, _))
            {
                body.push(scalar.kind());
                scalar.write_payload(&mut body, &mut intern_str)?;
            } else {
                let kind = plain_json_value_kind(val).context("Unsupported attribute JSON kind")?;
                body.push(kind);
                write_plain_json_value_payload(val, kind, &mut body, &mut intern_str)?;
            }
        }
    }
    Ok(PatchedGroup::Group(merged.len(), body))
}

#[cfg(any(windows, target_os = "macos", test))]
fn intern_block_own_string(
    open: &OpenBlock,
    extra_strings: &mut Vec<String>,
    base_string_count: usize,
    text: &str,
) -> usize {
    if let Some(pos) = open.strings.iter().position(|s| match s {
        BlockString::Own(existing) => existing == text,
        BlockString::Pool(_) => false,
    }) {
        return pos;
    }
    if let Some(pos) = extra_strings.iter().position(|existing| existing == text) {
        return base_string_count + pos;
    }
    let id = base_string_count + extra_strings.len();
    extra_strings.push(text.to_string());
    id
}

#[cfg(any(windows, target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn append_patched_json_groups<'a>(
    store: &BlockStore<'_>,
    open: &OpenBlock,
    strings_cache: &mut Option<Vec<String>>,
    block_count: usize,
    json_target_prop_ids: &[(&str, usize)],
    json_by_prop: &AHashMap<usize, Vec<PropertyGroupSlice<'_>>>,
    block_updates: &BTreeMap<&str, Vec<(usize, ScalarPatchValue)>>,
    json_updates: &BTreeMap<&str, Vec<(usize, Value)>>,
    extra_strings: &mut Vec<String>,
    base_string_count: usize,
    kept_groups: &mut Vec<(usize, u8, usize, Cow<'a, [u8]>)>,
) -> Result<bool> {
    for &(prop_name, prop_id) in json_target_prop_ids {
        let mut merged: BTreeMap<usize, Value> = BTreeMap::new();
        if let Some(existing_groups) = json_by_prop.get(&prop_id) {
            let existing = decode_block_property(
                store,
                open,
                strings_cache,
                prop_name,
                existing_groups,
                block_count,
            )?;
            for (row, val) in existing {
                if !json_value_fits_block_inline(&val) {
                    return Ok(false);
                }
                merged.insert(row, val);
            }
        }
        if let Some(scalar_updates) = block_updates.get(prop_name) {
            for (local_row, scalar) in scalar_updates {
                merged.insert(*local_row, scalar.to_json());
            }
        }
        for (local_row, val) in &json_updates[prop_name] {
            merged.insert(*local_row, val.clone());
        }
        let mut by_kind: BTreeMap<u8, Vec<(usize, Value)>> = BTreeMap::new();
        for (row, val) in merged {
            let Some(kind) = plain_json_value_kind(&val) else {
                return Ok(false);
            };
            by_kind.entry(kind).or_default().push((row, val));
        }
        let mut intern_str =
            |text: &str| intern_block_own_string(open, extra_strings, base_string_count, text);
        for (kind, entries) in by_kind {
            let mut body = Vec::with_capacity(entries.len() * 16);
            let mut prev_row = 0_usize;
            for (row, val) in &entries {
                write_property_group_instance_index(&mut body, &mut prev_row, *row)?;
                write_plain_json_value_payload(val, kind, &mut body, &mut intern_str)?;
            }
            kept_groups.push((prop_id, kind, entries.len(), Cow::Owned(body)));
        }
    }
    Ok(true)
}

#[cfg(any(windows, target_os = "macos", test))]
fn write_patched_block_section(
    store: &BlockStore<'_>,
    open: &OpenBlock,
    extra_strings: &[String],
    prop_ids: &[usize],
    kept_groups: Vec<(usize, u8, usize, Cow<'_, [u8]>)>,
) -> Result<Section<'static>> {
    let mut content = Vec::with_capacity(open.content.len() + 64);
    if extra_strings.is_empty() && prop_ids.len() == open.properties.len() {
        content.extend_from_slice(&open.content[..open.rest]);
    } else {
        let total_strings = open.strings.len() + extra_strings.len();
        write_var_u64(&mut content, total_strings as u64)?;
        for s in &open.strings {
            match s {
                BlockString::Own(text) => {
                    write_var_u64(&mut content, (text.len() as u64) << 1)?;
                    content.extend_from_slice(text.as_bytes());
                }
                BlockString::Pool(entry) => {
                    write_var_u64(&mut content, 1)?;
                    content.extend_from_slice(&store.hashes[*entry]);
                }
            }
        }
        for text in extra_strings {
            write_var_u64(&mut content, (text.len() as u64) << 1)?;
            content.extend_from_slice(text.as_bytes());
        }
        write_var_u64(&mut content, prop_ids.len() as u64)?;
        for id in prop_ids {
            write_var_u64(&mut content, *id as u64)?;
        }
    }
    write_var_u64(&mut content, kept_groups.len() as u64)?;
    for (prop_id, kind, count, body) in kept_groups {
        write_var_u64(&mut content, prop_id as u64)?;
        content.push(kind);
        write_var_u64(&mut content, count as u64)?;
        write_var_u64(&mut content, body.len() as u64)?;
        content.extend_from_slice(&body);
    }
    compressed(&content)
}

#[cfg(any(windows, target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn patch_scalar_block(
    store: &BlockStore<'_>,
    block_index: usize,
    block_updates: &BTreeMap<&str, Vec<(usize, ScalarPatchValue)>>,
    json_updates: &BTreeMap<&str, Vec<(usize, Value)>>,
    tag_updates: &[(usize, &str, bool)],
    attr_replaces: &[(usize, serde_json::Map<String, Value>)],
) -> Result<Option<Section<'static>>> {
    let open = store.open_block(block_index)?;
    let block_count = store.blocks[block_index].count;
    let base_string_count = open.strings.len();
    let mut extra_strings: Vec<String> = Vec::new();
    let mut prop_ids = open.properties.clone();
    let block_prop_names = block_property_names(store, &open)?;
    let mut prop_name_to_id: AHashMap<String, usize> = block_prop_names
        .iter()
        .enumerate()
        .map(|(idx, name)| (name.clone(), idx))
        .collect();

    let raw_groups = read_raw_block_groups(&open.content[open.rest..])?;
    let mut ensure_prop_id = |prop_name: &str| -> usize {
        if let Some(&id) = prop_name_to_id.get(prop_name) {
            return id;
        }
        let str_id =
            intern_block_own_string(&open, &mut extra_strings, base_string_count, prop_name);
        let id = prop_ids.len();
        prop_ids.push(str_id);
        prop_name_to_id.insert(prop_name.to_string(), id);
        id
    };

    let mut updated_prop_ids: AHashSet<usize> = AHashSet::new();
    let mut json_prop_ids: AHashSet<usize> = AHashSet::new();
    let mut target_prop_ids: Vec<(&str, usize)> = Vec::with_capacity(block_updates.len());
    let mut json_target_prop_ids: Vec<(&str, usize)> = Vec::with_capacity(json_updates.len());
    for &prop_name in json_updates.keys() {
        let prop_id = ensure_prop_id(prop_name);
        updated_prop_ids.insert(prop_id);
        json_prop_ids.insert(prop_id);
        json_target_prop_ids.push((prop_name, prop_id));
    }
    for &prop_name in block_updates.keys() {
        if json_updates.contains_key(prop_name) {
            continue;
        }
        let prop_id = ensure_prop_id(prop_name);
        updated_prop_ids.insert(prop_id);
        target_prop_ids.push((prop_name, prop_id));
    }
    let tags_prop_id = (!tag_updates.is_empty()).then(|| {
        let id = ensure_prop_id("Tags");
        updated_prop_ids.insert(id);
        id
    });
    let attrs_prop_id = (!attr_replaces.is_empty()).then(|| {
        let id = ensure_prop_id("Attributes");
        updated_prop_ids.insert(id);
        id
    });

    let mut kept_groups: Vec<(usize, u8, usize, Cow<'_, [u8]>)> = Vec::new();
    let mut scalar_by_prop: AHashMap<usize, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    let mut json_by_prop: AHashMap<usize, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    let mut other_by_prop: AHashMap<usize, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    let mut tags_groups: Vec<PropertyGroupSlice<'_>> = Vec::new();
    let mut attrs_groups: Vec<PropertyGroupSlice<'_>> = Vec::new();
    for (prop_id, kind, value_count, body) in raw_groups {
        if tags_prop_id == Some(prop_id) {
            tags_groups.push((kind, value_count, body));
        } else if attrs_prop_id == Some(prop_id) {
            attrs_groups.push((kind, value_count, body));
        } else if !updated_prop_ids.contains(&prop_id) {
            kept_groups.push((prop_id, kind, value_count, Cow::Borrowed(body)));
        } else if json_prop_ids.contains(&prop_id) {
            json_by_prop
                .entry(prop_id)
                .or_default()
                .push((kind, value_count, body));
        } else if is_scalar_group_kind(kind) {
            scalar_by_prop
                .entry(prop_id)
                .or_default()
                .push((kind, value_count, body));
        } else {
            other_by_prop
                .entry(prop_id)
                .or_default()
                .push((kind, value_count, body));
            kept_groups.push((prop_id, kind, value_count, Cow::Borrowed(body)));
        }
    }

    let mut strings_cache: Option<Vec<String>> = None;
    for (prop_name, prop_id) in target_prop_ids {
        let updates = &block_updates[prop_name];
        if let Some(other_groups) = other_by_prop.get(&prop_id) {
            let existing_other = decode_block_property(
                store,
                &open,
                &mut strings_cache,
                prop_name,
                other_groups,
                block_count,
            )?;
            if updates.iter().any(|(r, _)| existing_other.contains_key(r)) {
                return Ok(None);
            }
        }
        let mut merged: BTreeMap<usize, ScalarPatchValue> = BTreeMap::new();
        if let Some(existing_groups) = scalar_by_prop.get(&prop_id) {
            let mut non_string_groups: Vec<PropertyGroupSlice<'_>> = Vec::new();
            for &(kind, count, body) in existing_groups {
                if kind == 6 {
                    let mut reader = BytecodeReader::new(body);
                    let mut row = 0_usize;
                    for pos in 0..count {
                        row = next_group_instance(
                            row,
                            reader.read_len("property instance index")?,
                            pos,
                        )?;
                        anyhow::ensure!(
                            row < block_count,
                            "A value block gives a value to a row it does not hold"
                        );
                        let str_id = reader.read_len("string index")?;
                        anyhow::ensure!(
                            str_id < base_string_count,
                            "String index out of block bounds"
                        );
                        merged.insert(row, ScalarPatchValue::RawStringId(str_id));
                    }
                    reader.finish()?;
                } else {
                    non_string_groups.push((kind, count, body));
                }
            }
            if !non_string_groups.is_empty() {
                let existing = decode_block_property(
                    store,
                    &open,
                    &mut strings_cache,
                    prop_name,
                    &non_string_groups,
                    block_count,
                )?;
                for (row, val) in existing {
                    let Some(scalar) = ScalarPatchValue::from_json(&val) else {
                        return Ok(None);
                    };
                    merged.insert(row, scalar);
                }
            }
        }
        for (local_row, scalar) in updates {
            merged.insert(*local_row, scalar.clone());
        }
        let mut by_kind: BTreeMap<u8, Vec<(usize, ScalarPatchValue)>> = BTreeMap::new();
        for (row, scalar) in merged {
            by_kind
                .entry(scalar.kind())
                .or_default()
                .push((row, scalar));
        }
        let mut intern_str = |text: &str| {
            intern_block_own_string(&open, &mut extra_strings, base_string_count, text)
        };
        for (kind, entries) in by_kind {
            let mut body = Vec::with_capacity(entries.len() * 6 + 1);
            if kind == 16 {
                body.push(0_u8);
            }
            let mut prev_row = 0_usize;
            for (row, scalar) in &entries {
                write_property_group_instance_index(&mut body, &mut prev_row, *row)?;
                scalar.write_payload(&mut body, &mut intern_str)?;
            }
            kept_groups.push((prop_id, kind, entries.len(), Cow::Owned(body)));
        }
    }

    if !append_patched_json_groups(
        store,
        &open,
        &mut strings_cache,
        block_count,
        &json_target_prop_ids,
        &json_by_prop,
        block_updates,
        json_updates,
        &mut extra_strings,
        base_string_count,
        &mut kept_groups,
    )? {
        return Ok(None);
    }

    if let Some(prop_id) = tags_prop_id {
        match build_patched_tags_group(
            store,
            &open,
            &mut strings_cache,
            &tags_groups,
            block_count,
            tag_updates,
            &mut extra_strings,
            base_string_count,
        )? {
            PatchedGroup::Fallback => return Ok(None),
            PatchedGroup::Empty => {}
            PatchedGroup::Group(count, body) => {
                kept_groups.push((prop_id, 7_u8, count, Cow::Owned(body)));
            }
        }
    }

    if let Some(prop_id) = attrs_prop_id {
        match build_patched_attrs_group(
            store,
            &open,
            &mut strings_cache,
            &attrs_groups,
            block_count,
            attr_replaces,
            &mut extra_strings,
            base_string_count,
        )? {
            PatchedGroup::Fallback => return Ok(None),
            PatchedGroup::Empty => {}
            PatchedGroup::Group(count, body) => {
                kept_groups.push((prop_id, 8_u8, count, Cow::Owned(body)));
            }
        }
    }

    Ok(Some(write_patched_block_section(
        store,
        &open,
        &extra_strings,
        &prop_ids,
        kept_groups,
    )?))
}

#[cfg(any(windows, target_os = "macos", test))]
#[allow(clippy::too_many_arguments)]
fn apply_scalar_block_patches(
    store: &BlockStore<'_>,
    base_bytes: &[u8],
    scalar_by_block: &[BTreeMap<&str, Vec<(usize, ScalarPatchValue)>>],
    json_by_block: &[BTreeMap<&str, Vec<(usize, Value)>>],
    tag_by_block: &[Vec<(usize, &str, bool)>],
    attr_replace_by_block: &[Vec<(usize, serde_json::Map<String, Value>)>],
    attr_key_updates: &[(usize, String, Option<Value>)],
    fallback_updates: &[(usize, String, Value)],
) -> Result<Vec<u8>> {
    let block_sections = (0..store.blocks.len())
        .into_par_iter()
        .map(|block_idx| -> Result<Option<Section<'_>>> {
            let block_updates = &scalar_by_block[block_idx];
            let json_updates = &json_by_block[block_idx];
            let tags = &tag_by_block[block_idx];
            let attrs = &attr_replace_by_block[block_idx];
            if block_updates.is_empty()
                && json_updates.is_empty()
                && tags.is_empty()
                && attrs.is_empty()
            {
                return Ok(Some(store.raw(store.blocks[block_idx].frame)?));
            }
            patch_scalar_block(store, block_idx, block_updates, json_updates, tags, attrs)
        })
        .collect::<Result<Vec<_>>>()?;

    let all_fast = fallback_updates.is_empty()
        && attr_key_updates.is_empty()
        && block_sections.iter().all(Option::is_some);
    if all_fast {
        let structure = store.raw(store.structure)?;
        let entries = (0..store.hashes.len())
            .map(|idx| (store.hashes[idx], store.counts[idx], store.alone[idx]))
            .collect::<Vec<_>>();
        let pool = store
            .pool
            .iter()
            .map(|pf| store.raw(pf.frame))
            .collect::<Result<Vec<_>>>()?;
        let blocks = store
            .blocks
            .iter()
            .zip(block_sections)
            .map(|(block, section)| (block.count, section.expect("checked above")))
            .collect::<Vec<_>>();
        return assemble(structure, &entries, &pool, &blocks);
    }

    let mut document = store.decode()?;
    for (block_idx, block_updates) in scalar_by_block.iter().enumerate() {
        let block_first = store.blocks[block_idx].first;
        for (prop_name, rows) in block_updates {
            for (local_row, scalar) in rows {
                document.instances[block_first + *local_row]
                    .properties
                    .insert((*prop_name).to_string(), scalar.to_json());
            }
        }
    }
    for (block_idx, json_updates) in json_by_block.iter().enumerate() {
        let block_first = store.blocks[block_idx].first;
        for (prop_name, rows) in json_updates {
            for (local_row, val) in rows {
                document.instances[block_first + *local_row]
                    .properties
                    .insert((*prop_name).to_string(), val.clone());
            }
        }
    }
    for (block_idx, tags) in tag_by_block.iter().enumerate() {
        let block_first = store.blocks[block_idx].first;
        for &(local_row, tag_name, has_tag) in tags {
            let props = &mut document.instances[block_first + local_row].properties;
            let mut current: Vec<String> = props
                .get("Tags")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if has_tag {
                if !current.iter().any(|t| t == tag_name) {
                    current.push(tag_name.to_string());
                    current.sort();
                }
            } else {
                current.retain(|t| t != tag_name);
            }
            if current.is_empty() {
                props.remove("Tags");
            } else {
                props.insert(
                    "Tags".to_string(),
                    Value::Array(current.into_iter().map(Value::String).collect()),
                );
            }
        }
    }
    for (block_idx, attrs) in attr_replace_by_block.iter().enumerate() {
        let block_first = store.blocks[block_idx].first;
        for (local_row, map) in attrs {
            document.instances[block_first + *local_row].attributes = map.clone();
        }
    }
    for (global_row, attr_name, val) in attr_key_updates {
        match val {
            Some(v) => {
                document.instances[*global_row]
                    .attributes
                    .insert(attr_name.clone(), v.clone());
            }
            None => {
                document.instances[*global_row].attributes.remove(attr_name);
            }
        }
    }
    for (global_row, prop_name, value) in fallback_updates {
        document.instances[*global_row]
            .properties
            .insert(prop_name.clone(), value.clone());
    }
    encode_settings_bytecode_over(&document, false, Some(base_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::bytecode::{SettingsBytecode, SettingsBytecodeInstance};
    use crate::snapshot::types::{
        NativeSettingsProperty, NativeSettingsValue, PropertyName, ServiceState, SnapshotInstance,
    };
    use rbx_dom_weak::Ustr;
    use serde_json::Map;

    fn patch_bulk_old_values_via_columns(
        base_bytes: &[u8],
        updates: &BTreeMap<String, Vec<(usize, Value)>>,
    ) -> Result<Vec<u8>> {
        let mut max_row = 0usize;
        let mut columns = Vec::with_capacity(updates.len());
        for (prop_name, rows) in updates {
            let mut row_bytes = Vec::with_capacity(rows.len() * 4);
            let mut values = Vec::with_capacity(rows.len());
            for (row, value) in rows {
                max_row = max_row.max(*row);
                row_bytes.extend_from_slice(&(*row as u32).to_le_bytes());
                values.push(value.clone());
            }
            columns.push(json!({
                "propertyName": prop_name,
                "encoding": "json",
                "count": rows.len(),
                "rows": base64::encode(row_bytes),
                "values": values,
            }));
        }
        let node_to_row: Vec<usize> = (1..=max_row).collect();
        patch_v12_bulk_old_value_columns(base_bytes, &columns, &node_to_row)
    }

    fn part_instance(
        id: &str,
        name: &str,
        class_name: &str,
        parent: Option<usize>,
        props: Map<String, Value>,
    ) -> SettingsBytecodeInstance {
        SettingsBytecodeInstance {
            settings_id: id.to_string(),
            name: name.to_string(),
            class_name: class_name.to_string(),
            parent_index: parent,
            properties: props,
            attributes: Map::new(),
        }
    }

    fn csg_blob(seed: usize, len: usize) -> Value {
        let mut raw = vec![0u8; len];
        let mut state = (seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        if state == 0 {
            state = 1;
        }
        for chunk in raw.chunks_exact_mut(8) {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let val = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            chunk.copy_from_slice(&val.to_le_bytes());
        }
        json!({
            "_type": "BinaryString",
            "base64": base64::encode(&raw),
        })
    }

    fn corrupt_pool_frames(bytes: &[u8]) -> Vec<u8> {
        let store = BlockStore::open(bytes).unwrap();
        assert!(!store.pool.is_empty(), "expected non-empty pool frames");
        let structure = store.raw(store.structure).unwrap();
        let entries = (0..store.hashes.len())
            .map(|idx| (store.hashes[idx], store.counts[idx], store.alone[idx]))
            .collect::<Vec<_>>();
        let pool = store
            .pool
            .iter()
            .map(|pf| {
                let bad = vec![0xA5_u8; pf.frame.len.max(8)];
                Section {
                    decoded: pf.frame.decoded,
                    digest: digest(&[&bad]),
                    bytes: std::borrow::Cow::Owned(bad),
                }
            })
            .collect::<Vec<_>>();
        let blocks = store
            .blocks
            .iter()
            .map(|block| Ok((block.count, store.raw(block.frame)?)))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let corrupted = assemble(structure, &entries, &pool, &blocks).unwrap();
        assert!(
            decode_settings_bytecode(&corrupted).is_err(),
            "corrupted pool frames should fail full decode"
        );
        corrupted
    }

    #[test]
    fn columnar_diff_handles_plain_properties_unions_defaults_and_safety_fallbacks() {
        let mut base_instances = vec![part_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            Map::new(),
        )];
        let mut desired_instances = base_instances.clone();
        base_instances.push(part_instance("fld", "Build", "Folder", Some(0), Map::new()));
        desired_instances.push(part_instance("fld", "Build", "Folder", Some(0), Map::new()));

        for i in 0..120 {
            let is_union = i % 5 == 0;
            let class_name = if is_union { "UnionOperation" } else { "Part" };
            let mut obs_props = Map::new();
            let mut des_props = Map::new();
            if is_union {
                let mesh = csg_blob(i, 1024);
                let phys = csg_blob(i + 1000, 1024);
                obs_props.insert("MeshData".into(), mesh.clone());
                obs_props.insert("PhysicsData".into(), phys.clone());
                des_props.insert("MeshData".into(), mesh);
                des_props.insert("PhysicsData".into(), phys);
            }
            if i < 80 {
                obs_props.insert("Transparency".into(), json!(0.25));
                if i < 40 {
                    des_props.insert("Transparency".into(), json!(0.5));
                }
            }
            base_instances.push(part_instance(
                &format!("p{i}"),
                "Block",
                class_name,
                Some(1),
                obs_props,
            ));
            desired_instances.push(part_instance(
                &format!("p{i}"),
                "Block",
                class_name,
                Some(1),
                des_props,
            ));
        }

        let obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: base_instances.clone(),
        })
        .unwrap();
        let des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: desired_instances.clone(),
        })
        .unwrap();

        assert!(matches!(
            diff_v12_bulk_plain_properties(&obs_bytes, &obs_bytes, "Workspace", 64).unwrap(),
            BulkDiffOutcome::Unchanged
        ));

        let corrupted_des = corrupt_pool_frames(&des_bytes);
        let corrupted_obs = corrupt_pool_frames(&obs_bytes);
        let outcome =
            diff_v12_bulk_plain_properties(&corrupted_des, &corrupted_obs, "Workspace", 64)
                .unwrap();
        let BulkDiffOutcome::Patch(patch) = outcome else {
            panic!("expected bulk patch");
        };
        assert_eq!(patch.service, "Workspace");
        assert_eq!(patch.instance_count, 80);
        assert_eq!(patch.field_count, 80);
        assert_eq!(patch.nodes.len(), 81);
        assert_eq!(patch.columns.len(), 1);
        assert_eq!(patch.columns[0].property_name, "Transparency");

        let mut pkg_obs = base_instances.clone();
        let mut pkg_des = desired_instances.clone();
        pkg_obs.push(part_instance(
            "pl",
            "PackageLink",
            "PackageLink",
            Some(1),
            Map::new(),
        ));
        pkg_des.push(part_instance(
            "pl",
            "PackageLink",
            "PackageLink",
            Some(1),
            Map::new(),
        ));
        let pkg_obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: pkg_obs.clone(),
        })
        .unwrap();
        let pkg_des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: pkg_des.clone(),
        })
        .unwrap();
        let BulkDiffOutcome::Patch(pkg_patch) =
            diff_v12_bulk_plain_properties(&pkg_des_bytes, &pkg_obs_bytes, "Workspace", 64)
                .unwrap()
        else {
            panic!("expected editable package descendants to produce bulk patch");
        };
        assert_eq!(pkg_patch.package_targets.len(), 1);
        assert_eq!(pkg_patch.package_targets[0].path_segments[0], "Workspace");

        let mut pkg_mod_obs = pkg_obs.clone();
        pkg_mod_obs
            .last_mut()
            .unwrap()
            .properties
            .insert("ModifiedState".into(), json!(true));
        let mut pkg_mod_des = pkg_obs.clone();
        pkg_mod_des
            .last_mut()
            .unwrap()
            .properties
            .insert("ModifiedState".into(), json!(false));
        let pkg_mod_obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: pkg_mod_obs,
        })
        .unwrap();
        let pkg_mod_des_only_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: pkg_mod_des,
        })
        .unwrap();
        assert!(matches!(
            diff_v12_bulk_plain_properties(
                &pkg_mod_des_only_bytes,
                &pkg_mod_obs_bytes,
                "Workspace",
                64,
            )
            .unwrap(),
            BulkDiffOutcome::Unchanged
        ));
        let mut pkg_mod_and_part_des = pkg_des.clone();
        pkg_mod_and_part_des
            .last_mut()
            .unwrap()
            .properties
            .insert("ModifiedState".into(), json!(false));
        let pkg_mod_and_part_des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: pkg_mod_and_part_des,
        })
        .unwrap();
        let BulkDiffOutcome::Patch(pkg_mod_patch) = diff_v12_bulk_plain_properties(
            &pkg_mod_and_part_des_bytes,
            &pkg_mod_obs_bytes,
            "Workspace",
            64,
        )
        .unwrap() else {
            panic!("expected PackageLink.ModifiedState diff to be skipped during bulk patch");
        };
        assert_eq!(pkg_mod_patch.package_targets.len(), 1);
        assert_eq!(pkg_mod_patch.instance_count, pkg_patch.instance_count);
        assert_eq!(pkg_mod_patch.columns.len(), 1);
        assert_eq!(pkg_mod_patch.columns[0].property_name, "Transparency");

        let mut attr_des = desired_instances.clone();
        attr_des[2]
            .attributes
            .insert("Custom".into(), json!("value"));
        let attr_des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: attr_des,
        })
        .unwrap();
        let BulkDiffOutcome::Patch(attr_patch) =
            diff_v12_bulk_plain_properties(&attr_des_bytes, &obs_bytes, "Workspace", 64).unwrap()
        else {
            panic!("expected bulk patch for attribute edit");
        };
        assert!(
            attr_patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Attribute && c.property_name == "Custom")
        );

        let mut uid_obs = base_instances.clone();
        let mut uid_des = desired_instances.clone();
        uid_obs[2].properties.insert(
            UNIQUE_ID.into(),
            json!({"_type": "UniqueId", "value": "u1"}),
        );
        uid_obs[3].properties.insert(
            UNIQUE_ID.into(),
            json!({"_type": "UniqueId", "value": "u2"}),
        );
        uid_des[2].properties.insert(
            UNIQUE_ID.into(),
            json!({"_type": "UniqueId", "value": "u2"}),
        );
        uid_des[3].properties.insert(
            UNIQUE_ID.into(),
            json!({"_type": "UniqueId", "value": "u1"}),
        );
        let uid_obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: uid_obs,
        })
        .unwrap();
        let uid_des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: uid_des,
        })
        .unwrap();
        assert!(matches!(
            diff_v12_bulk_plain_properties(&uid_des_bytes, &uid_obs_bytes, "Workspace", 64)
                .unwrap(),
            BulkDiffOutcome::Fallback
        ));
    }

    fn synthesize_mass_push_states(
        total: usize,
        changed: usize,
        unions: usize,
        folders: usize,
    ) -> (ServiceState, ServiceState) {
        use std::sync::Arc;

        let csg_meshes: Vec<Arc<Value>> = (0..unions)
            .into_par_iter()
            .map(|i| Arc::new(csg_blob(i + 1, 4096)))
            .collect();
        let csg_phys: Vec<Arc<Value>> = (0..unions)
            .into_par_iter()
            .map(|i| Arc::new(csg_blob(i + 100_001, 2048)))
            .collect();

        let mut instances = Vec::with_capacity(total);
        let mut obs_native = Vec::with_capacity(total);
        let mut des_native = Vec::with_capacity(total);

        instances.push(SnapshotInstance {
            name: "Workspace".into(),
            class_name: Ustr::from("Workspace"),
            instance_index: Some(1),
            parent_index: None,
            ..Default::default()
        });
        obs_native.push(Vec::new());
        des_native.push(Vec::new());

        for f in 0..folders {
            let idx = instances.len();
            instances.push(SnapshotInstance {
                name: format!("Zone_{f:03}"),
                class_name: Ustr::from("Folder"),
                instance_index: Some(idx + 1),
                parent_index: Some(1),
                ..Default::default()
            });
            obs_native.push(Vec::new());
            des_native.push(Vec::new());
        }

        let part_count = total - 1 - folders;
        for i in 0..part_count {
            let idx = instances.len();
            let parent_idx = 2 + (i % folders);
            let is_union = i < unions;
            let class_name = if is_union { "UnionOperation" } else { "Part" };
            let prefix = if is_union { "Union" } else { "Part" };
            instances.push(SnapshotInstance {
                name: format!("{prefix}_{}", i % 64),
                class_name: Ustr::from(class_name),
                instance_index: Some(idx + 1),
                parent_index: Some(parent_idx),
                ..Default::default()
            });
            let mut obs_props = Vec::with_capacity(if is_union { 6 } else { 4 });
            obs_props.push(NativeSettingsProperty {
                name: PropertyName::held("Anchored"),
                value: NativeSettingsValue::Bool(true),
            });
            obs_props.push(NativeSettingsProperty {
                name: PropertyName::held("Size"),
                value: NativeSettingsValue::Vector3([4.0, 1.0, 2.0]),
            });
            obs_props.push(NativeSettingsProperty {
                name: PropertyName::held("Color"),
                value: NativeSettingsValue::Color3([0.5, 0.5, 0.5]),
            });
            if is_union {
                let slot = i % csg_meshes.len();
                obs_props.push(NativeSettingsProperty {
                    name: PropertyName::held("MeshData"),
                    value: NativeSettingsValue::Shared(Arc::clone(&csg_meshes[slot])),
                });
                obs_props.push(NativeSettingsProperty {
                    name: PropertyName::held("PhysicsData"),
                    value: NativeSettingsValue::Shared(Arc::clone(&csg_phys[slot])),
                });
            }
            let mut des_props = obs_props.clone();
            if i < changed {
                obs_props.push(NativeSettingsProperty {
                    name: PropertyName::held("Transparency"),
                    value: NativeSettingsValue::Float32(0.0),
                });
                des_props.push(NativeSettingsProperty {
                    name: PropertyName::held("Transparency"),
                    value: NativeSettingsValue::Float32(0.5),
                });
            }
            obs_native.push(obs_props);
            des_native.push(des_props);
        }

        let obs_state = ServiceState {
            instances: instances.clone(),
            native_properties_by_instance: Some(obs_native),
            children_by_index: Vec::new(),
            source_in_subtree: Vec::new(),
            service_root_index: 0,
            class_defaults_by_class: HashMap::new(),
            properties_default_elided: true,
            dense_index_topology: true,
        };
        let des_state = ServiceState {
            instances,
            native_properties_by_instance: Some(des_native),
            children_by_index: Vec::new(),
            source_in_subtree: Vec::new(),
            service_root_index: 0,
            class_defaults_by_class: HashMap::new(),
            properties_default_elided: true,
            dense_index_topology: true,
        };
        (obs_state, des_state)
    }

    fn measure_bulk_wire_bytes(patch: &EditorBulkPropertyPatch) -> usize {
        let mut node_bytes = Vec::with_capacity(patch.nodes.len() * 16);
        for node in &patch.nodes {
            node_bytes.extend_from_slice(&node.parent.to_le_bytes());
            node_bytes.extend_from_slice(&node.ordinal.to_le_bytes());
            node_bytes.extend_from_slice(&node.name_idx.to_le_bytes());
            node_bytes.extend_from_slice(&node.class_idx.to_le_bytes());
        }
        let nodes_b64 = base64::encode(&node_bytes);
        let strings_b64 = base64::encode(patch.strings.join("\0").as_bytes());
        let mut row_bytes = Vec::with_capacity(patch.columns[0].rows.len() * 4);
        for &r in &patch.columns[0].rows {
            row_bytes.extend_from_slice(&r.to_le_bytes());
        }
        let rows_b64 = base64::encode(&row_bytes);
        nodes_b64.len() + strings_b64.len() + rows_b64.len()
    }

    #[test]
    #[ignore]
    fn bench_mass_push_140k_transparency() {
        use std::time::Instant;

        const TOTAL: usize = 140_329;
        const CHANGED: usize = 77_414;
        const UNIONS: usize = 14_808;
        const FOLDERS: usize = 200;

        let t0 = Instant::now();
        let (obs_state, des_state) = synthesize_mass_push_states(TOTAL, CHANGED, UNIONS, FOLDERS);
        eprintln!(
            "[bench] synthesized in {:.1} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );

        let t_des = Instant::now();
        let des_bytes = encode_service_settings_binary(&des_state).unwrap();
        eprintln!(
            "[bench] encode desired (cold, previous=None): {:.1} ms ({:.2} MB)",
            t_des.elapsed().as_secs_f64() * 1000.0,
            des_bytes.len() as f64 / (1024.0 * 1024.0)
        );

        let t_cold = Instant::now();
        let obs_bytes_cold = encode_service_settings_binary(&obs_state).unwrap();
        eprintln!(
            "[bench] BEFORE capture store encode (previous=None): {:.1} ms ({:.2} MB)",
            t_cold.elapsed().as_secs_f64() * 1000.0,
            obs_bytes_cold.len() as f64 / (1024.0 * 1024.0)
        );

        let t_warm = Instant::now();
        let obs_bytes_warm =
            encode_service_settings_binary_over(&obs_state, Some(&des_bytes)).unwrap();
        eprintln!(
            "[bench] AFTER  capture store encode (previous=Some): {:.1} ms ({:.2} MB)",
            t_warm.elapsed().as_secs_f64() * 1000.0,
            obs_bytes_warm.len() as f64 / (1024.0 * 1024.0)
        );
        assert_eq!(obs_bytes_cold, obs_bytes_warm);

        let t_old_diff = Instant::now();
        let (old_d, old_o) = crate::settings::bytecode::property_edit::whole_row_digests_of_both(
            &des_bytes,
            &obs_bytes_warm,
            None,
        );
        let _old_digests = (old_d.unwrap(), old_o.unwrap());
        eprintln!(
            "[bench] BEFORE whole_row_digests_of_both: {:.1} ms",
            t_old_diff.elapsed().as_secs_f64() * 1000.0
        );

        let t_new_diff = Instant::now();
        let outcome =
            diff_v12_bulk_plain_properties(&des_bytes, &obs_bytes_warm, "Workspace", 64).unwrap();
        let diff_ms = t_new_diff.elapsed().as_secs_f64() * 1000.0;
        let BulkDiffOutcome::Patch(patch) = outcome else {
            panic!("expected bulk patch on 140k benchmark");
        };
        assert_eq!(patch.instance_count, CHANGED);
        assert_eq!(patch.field_count, CHANGED);
        eprintln!(
            "[bench] AFTER  columnar bulk diff: {:.1} ms (instances={}, fields={}, tree_nodes={}, strings={})",
            diff_ms,
            patch.instance_count,
            patch.field_count,
            patch.nodes.len(),
            patch.strings.len()
        );

        let t_wire = Instant::now();
        let wire_bytes = measure_bulk_wire_bytes(&patch);
        let wire_ms = t_wire.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "[bench] AFTER  wire payload encode: {:.1} ms ({:.2} MB base64)",
            wire_ms,
            wire_bytes as f64 / (1024.0 * 1024.0)
        );
    }

    #[test]
    fn columnar_diff_handles_permuted_row_order_and_terrain_transparency() {
        let mut des_instances = vec![part_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            Map::new(),
        )];
        let mut obs_instances = vec![part_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            Map::new(),
        )];

        des_instances.push(part_instance(
            "terrain",
            "Terrain",
            "Terrain",
            Some(0),
            Map::from_iter([("Transparency".into(), json!(0.0))]),
        ));
        obs_instances.push(part_instance(
            "terrain",
            "Terrain",
            "Terrain",
            Some(0),
            Map::from_iter([("Transparency".into(), json!(0.5))]),
        ));

        let add_folder = |list: &mut Vec<SettingsBytecodeInstance>,
                          folder_name: &str,
                          seed_base: usize,
                          transparency: f64| {
            let folder_idx = list.len();
            list.push(part_instance(
                folder_name,
                folder_name,
                "Folder",
                Some(0),
                Map::new(),
            ));
            for i in 0..40 {
                let mut props = Map::new();
                props.insert("Transparency".into(), json!(transparency));
                props.insert("MeshData".into(), csg_blob(seed_base + i, 1024));
                list.push(part_instance(
                    &format!("{folder_name}_{i}"),
                    &format!("Union_{i}"),
                    "UnionOperation",
                    Some(folder_idx),
                    props,
                ));
            }
        };

        add_folder(&mut des_instances, "FolderA", 100, 0.0);
        add_folder(&mut des_instances, "FolderB", 200, 0.0);
        add_folder(&mut obs_instances, "FolderB", 200, 0.5);
        add_folder(&mut obs_instances, "FolderA", 100, 0.5);

        let des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: des_instances,
        })
        .unwrap();
        let obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: obs_instances,
        })
        .unwrap();

        let outcome =
            diff_v12_bulk_plain_properties(&des_bytes, &obs_bytes, "Workspace", 10).unwrap();
        let BulkDiffOutcome::Patch(patch) = outcome else {
            panic!("expected Patch for permuted row order + Terrain Transparency");
        };
        assert_eq!(patch.instance_count, 81); // 80 UnionOperations + 1 Terrain
        assert_eq!(patch.field_count, 81);
        assert_eq!(patch.columns.len(), 1);
        assert_eq!(patch.columns[0].property_name, "Transparency");
    }

    #[test]
    fn extract_and_patch_v12_bulk_old_values_round_trip() {
        let mut desired = vec![part_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            Map::new(),
        )];
        desired.push(part_instance(
            "terrain",
            "Terrain",
            "Terrain",
            Some(0),
            Map::new(),
        ));
        desired.push(part_instance(
            "fld",
            "Folder",
            "Folder",
            Some(0),
            Map::new(),
        ));
        for i in 0..80 {
            let mut props = Map::new();
            if i % 4 == 0 {
                props.insert("MeshData".into(), csg_blob(i + 50, 4096));
            }
            if i < 40 {
                props.insert("Transparency".into(), json!(0.25));
            }
            desired.push(part_instance(
                &format!("p{i}"),
                "Part",
                if i % 4 == 0 { "UnionOperation" } else { "Part" },
                Some(2),
                props,
            ));
        }
        desired.push(part_instance(
            "pe",
            "Sparks",
            "ParticleEmitter",
            Some(2),
            Map::new(),
        ));

        let desired_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: desired.clone(),
        })
        .unwrap();

        let corrupted_desired = corrupt_pool_frames(&desired_bytes);
        let outcome = extract_v12_bulk_properties_for_classes(
            &corrupted_desired,
            "Workspace",
            &[
                ("Part".into(), "Transparency".into()),
                ("UnionOperation".into(), "Transparency".into()),
                ("Terrain".into(), "Transparency".into()),
                ("Workspace".into(), "Gravity".into()),
            ],
            10,
        )
        .unwrap();
        let BulkExtractOutcome::Patch(extracted) = outcome else {
            panic!("expected zero-capture bulk extract patch");
        };
        assert_eq!(extracted.patch.instance_count, 82);
        assert_eq!(extracted.patch.field_count, 82);

        let mut updates: BTreeMap<String, Vec<(usize, Value)>> = BTreeMap::new();
        let mut expected = desired.clone();
        let mut prop_updates = Vec::new();
        let mut cframe_updates = Vec::new();
        let mut material_updates = Vec::new();
        for (row, inst) in expected.iter_mut().enumerate().skip(1) {
            if inst.class_name == "Part"
                || inst.class_name == "UnionOperation"
                || inst.class_name == "Terrain"
            {
                let old = json_number_f64(0.75);
                inst.properties.insert("Transparency".into(), old.clone());
                prop_updates.push((row, old));
            }
            if inst.class_name == "Part" {
                let cf = json!({
                    "_type": "CFrame",
                    "components": [1.0, 2.0, 3.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
                });
                inst.properties.insert("CFrame".into(), cf.clone());
                cframe_updates.push((row, cf));
                let mat_wire = json!({
                    "_type": "EnumItem",
                    "enumType": "Material",
                    "name": "Neon",
                    "value": 288
                });
                let mat_canon = json!({
                    "_type": "EnumItem",
                    "name": "Neon"
                });
                inst.properties.insert("Material".into(), mat_canon);
                material_updates.push((row, mat_wire));
            }
        }
        updates.insert("Transparency".into(), prop_updates);
        updates.insert("CFrame".into(), cframe_updates);
        updates.insert("Material".into(), material_updates);

        let _ = patch_bulk_old_values_via_columns(&corrupted_desired, &updates).unwrap();
        let patched_bytes = patch_bulk_old_values_via_columns(&desired_bytes, &updates).unwrap();
        let decoded = decode_settings_bytecode(&patched_bytes).unwrap();
        assert_eq!(decoded.instances.len(), expected.len());
        for (got, want) in decoded.instances.iter().zip(&expected) {
            assert_eq!(got.properties, want.properties);
        }
    }

    #[test]
    fn float64_small_delta_is_not_skipped() {
        let mut des = vec![part_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            Map::new(),
        )];
        let mut obs = des.clone();
        for i in 0..80 {
            des.push(part_instance(
                &format!("s{i}"),
                &format!("Sound_{i}"),
                "Sound",
                Some(0),
                Map::from_iter([("TimePosition".into(), json!(1.0 + 1e-8))]),
            ));
            obs.push(part_instance(
                &format!("s{i}"),
                &format!("Sound_{i}"),
                "Sound",
                Some(0),
                Map::from_iter([("TimePosition".into(), json!(1.0))]),
            ));
        }
        let des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: des,
        })
        .unwrap();
        let obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: obs,
        })
        .unwrap();
        let outcome =
            diff_v12_bulk_plain_properties(&des_bytes, &obs_bytes, "Workspace", 10).unwrap();
        let BulkDiffOutcome::Patch(patch) = outcome else {
            panic!("expected Float64 1e-8 delta to produce a bulk patch");
        };
        assert_eq!(patch.instance_count, 80);
        assert_eq!(patch.columns[0].property_name, "TimePosition");
    }

    #[test]
    fn bulk_attributes_tags_and_general_classes_round_trip() {
        let mut base = vec![part_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            Map::new(),
        )];
        let mut desired = base.clone();

        for i in 0..40 {
            let mut obs_props = Map::new();
            let mut des_props = Map::new();
            obs_props.insert("StudsPerTileU".into(), json!(2.0));
            des_props.insert("StudsPerTileU".into(), json!(4.0));
            obs_props.insert("Tags".into(), json!(["KeepTag", "OldTag"]));
            des_props.insert("Tags".into(), json!(["KeepTag", "NewTag"]));
            let mut obs_inst = part_instance(
                &format!("t{i}"),
                &format!("Tex{i}"),
                "Texture",
                Some(0),
                obs_props,
            );
            obs_inst.attributes.insert("Mark".into(), json!(1));
            obs_inst.attributes.insert("DropMe".into(), json!("gone"));
            let mut des_inst = part_instance(
                &format!("t{i}"),
                &format!("Tex{i}"),
                "Texture",
                Some(0),
                des_props,
            );
            des_inst.attributes.insert("Mark".into(), json!(2));
            base.push(obs_inst);
            desired.push(des_inst);
        }
        for i in 0..30 {
            let mut obs_props = Map::new();
            let mut des_props = Map::new();
            obs_props.insert("Text".into(), json!("before"));
            des_props.insert("Text".into(), json!("after"));
            obs_props.insert(
                "Position".into(),
                json!({"_type": "UDim2", "xScale": 0.0, "xOffset": 10, "yScale": 0.0, "yOffset": 20}),
            );
            des_props.insert(
                "Position".into(),
                json!({"_type": "UDim2", "xScale": 0.5, "xOffset": 0, "yScale": 0.5, "yOffset": 0}),
            );
            base.push(part_instance(
                &format!("l{i}"),
                &format!("Lbl{i}"),
                "TextLabel",
                Some(0),
                obs_props,
            ));
            desired.push(part_instance(
                &format!("l{i}"),
                &format!("Lbl{i}"),
                "TextLabel",
                Some(0),
                des_props,
            ));
        }
        for i in 0..10 {
            let mut obs_props = Map::new();
            let mut des_props = Map::new();
            obs_props.insert(
                "CFrame".into(),
                json!({"_type": "CFrame", "components": [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]}),
            );
            des_props.insert(
                "CFrame".into(),
                json!({"_type": "CFrame", "components": [1.0, 2.0, 3.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]}),
            );
            base.push(part_instance(
                &format!("a{i}"),
                &format!("Att{i}"),
                "Attachment",
                Some(0),
                obs_props,
            ));
            desired.push(part_instance(
                &format!("a{i}"),
                &format!("Att{i}"),
                "Attachment",
                Some(0),
                des_props,
            ));
        }

        let obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: base,
        })
        .unwrap();
        let des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: desired.clone(),
        })
        .unwrap();

        let BulkDiffOutcome::Patch(patch) =
            diff_v12_bulk_plain_properties(&des_bytes, &obs_bytes, "Workspace", 10).unwrap()
        else {
            panic!(
                "expected bulk patch for textures, text labels, attachments, attributes, and tags"
            );
        };
        assert_eq!(patch.instance_count, 80);
        assert!(
            patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Property && c.property_name == "StudsPerTileU")
        );
        assert!(
            patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Property && c.property_name == "Text")
        );
        assert!(
            patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Property && c.property_name == "Position")
        );
        assert!(
            patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Property && c.property_name == "CFrame")
        );
        assert!(
            patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Attribute && c.property_name == "Mark")
        );
        assert!(
            patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Attribute && c.property_name == "DropMe")
        );
        assert!(
            patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Tag && c.property_name == "NewTag")
        );
        assert!(
            patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Tag && c.property_name == "OldTag")
        );

        let BulkExtractOutcome::Patch(extracted) = extract_v12_bulk_properties_for_classes(
            &des_bytes,
            "Workspace",
            &[
                ("Texture".into(), "@attributes".into()),
                ("Texture".into(), "@tag:NewTag".into()),
                ("TextLabel".into(), "Text".into()),
                ("TextLabel".into(), "Position".into()),
                ("Attachment".into(), "CFrame".into()),
            ],
            10,
        )
        .unwrap() else {
            panic!("expected zero-capture extract for attributes, tags, TextLabel, Attachment");
        };
        assert_eq!(extracted.patch.instance_count, 80);
        assert!(
            extracted
                .patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::AttributesReplace)
        );
        assert!(
            extracted
                .patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Tag && c.property_name == "NewTag")
        );
        assert!(
            extracted
                .patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::Property && c.property_name == "Text")
        );
    }

    #[test]
    fn bulk_raw_equality_respects_f32_byte_color_and_rotation_rules() {
        let mut obs = vec![part_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            Map::new(),
        )];
        let mut des = obs.clone();
        let angle = 5e-5_f64;
        let (c, s) = (angle.cos(), angle.sin());
        for i in 0..20 {
            let mut op = Map::new();
            let mut dp = Map::new();
            op.insert(
                "Size".into(),
                json!({"_type": "Vector3", "x": 4.0, "y": 1.0, "z": 2.0}),
            );
            dp.insert(
                "Size".into(),
                json!({"_type": "Vector3", "x": 4.00005, "y": 1.0, "z": 2.0}),
            );
            op.insert(
                "CFrame".into(),
                json!({"_type": "CFrame", "components": [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]}),
            );
            dp.insert(
                "CFrame".into(),
                json!({"_type": "CFrame", "components": [0.0, 0.0, 0.0, c, -s, 0.0, s, c, 0.0, 0.0, 0.0, 1.0]}),
            );
            op.insert(
                "Color".into(),
                json!({"_type": "Color3", "r": 77.0 / 255.0, "g": 0.0, "b": 0.0}),
            );
            dp.insert(
                "Color".into(),
                json!({"_type": "Color3", "r": 0.3, "g": 0.0, "b": 0.0}),
            );
            obs.push(part_instance(
                &format!("p{i}"),
                &format!("P{i}"),
                "Part",
                Some(0),
                op,
            ));
            des.push(part_instance(
                &format!("p{i}"),
                &format!("P{i}"),
                "Part",
                Some(0),
                dp,
            ));

            let mut ol = Map::new();
            let mut dl = Map::new();
            ol.insert(
                "Color".into(),
                json!({"_type": "Color3", "r": 0.5, "g": 0.5, "b": 0.5}),
            );
            dl.insert(
                "Color".into(),
                json!({"_type": "Color3", "r": 0.50005, "g": 0.5, "b": 0.5}),
            );
            obs.push(part_instance(
                &format!("l{i}"),
                &format!("L{i}"),
                "PointLight",
                Some(0),
                ol,
            ));
            des.push(part_instance(
                &format!("l{i}"),
                &format!("L{i}"),
                "PointLight",
                Some(0),
                dl,
            ));
        }
        let obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: obs,
        })
        .unwrap();
        let des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: des,
        })
        .unwrap();
        let BulkDiffOutcome::Patch(patch) =
            diff_v12_bulk_plain_properties(&des_bytes, &obs_bytes, "Workspace", 10).unwrap()
        else {
            panic!("expected sub-1e-4 edits to produce bulk patch");
        };
        assert_eq!(patch.instance_count, 40);
        let size_col = patch
            .columns
            .iter()
            .find(|c| c.property_name == "Size")
            .expect("Size column");
        assert_eq!(size_col.rows.len(), 20);
        let cf_col = patch
            .columns
            .iter()
            .find(|c| c.property_name == "CFrame")
            .expect("CFrame column");
        assert_eq!(cf_col.rows.len(), 20);
        let color_col = patch
            .columns
            .iter()
            .find(|c| c.property_name == "Color")
            .expect("PointLight float Color column");
        assert_eq!(color_col.rows.len(), 20);
    }

    #[test]
    fn zero_capture_emits_model_assertions_and_rejects_joints() {
        let mut instances = vec![
            part_instance("ws", "Workspace", "Workspace", None, Map::new()),
            part_instance("m1", "M1", "Model", Some(0), {
                let mut m = Map::new();
                m.insert("Scale".into(), json!(1.5));
                m.insert(
                    "WorldPivot".into(),
                    json!({"_type": "CFrame", "components": [1.0, 2.0, 3.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]}),
                );
                m
            }),
        ];
        for i in 0..10 {
            let mut p = Map::new();
            p.insert(
                "Size".into(),
                json!({"_type": "Vector3", "x": 4.0, "y": 1.0, "z": 2.0}),
            );
            instances.push(part_instance(
                &format!("p{i}"),
                &format!("P{i}"),
                "Part",
                Some(1),
                p,
            ));
        }
        let bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        })
        .unwrap();
        let BulkExtractOutcome::Patch(extracted) = extract_v12_bulk_properties_for_classes(
            &bytes,
            "Workspace",
            &[("Part".into(), "Size".into())],
            1,
        )
        .unwrap() else {
            panic!("expected extract patch with model assertions");
        };
        assert_eq!(extracted.patch.field_count, 10);
        assert!(matches!(
            extracted.patch.columns[0].kind,
            BulkColumnKind::AssertModelScale | BulkColumnKind::AssertModelPivot
        ));
        assert!(
            extracted
                .patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::AssertModelScale && c.property_name == "Scale")
        );
        assert!(
            extracted
                .patch
                .columns
                .iter()
                .any(|c| c.kind == BulkColumnKind::AssertModelPivot
                    && c.property_name == "WorldPivot")
        );

        let outcome = extract_v12_bulk_properties_for_classes(
            &bytes,
            "Workspace",
            &[("WeldConstraint".into(), "Enabled".into())],
            1,
        )
        .unwrap();
        assert!(matches!(outcome, BulkExtractOutcome::Fallback));
    }

    #[test]
    fn bulk_diff_and_extract_skip_rbx_engine_attributes() {
        let mut obs = vec![part_instance(
            "ws",
            "Workspace",
            "Workspace",
            None,
            Map::new(),
        )];
        let mut des = obs.clone();
        for i in 0..15 {
            let mut o = part_instance(
                &format!("p{i}"),
                &format!("P{i}"),
                "Part",
                Some(0),
                Map::new(),
            );
            o.attributes.insert("RBX_SourceAssetId".into(), json!(111));
            o.attributes.insert("UserAttr".into(), json!("same"));
            let mut d = part_instance(
                &format!("p{i}"),
                &format!("P{i}"),
                "Part",
                Some(0),
                Map::new(),
            );
            d.attributes.insert("RBX_SourceAssetId".into(), json!(222));
            d.attributes.insert("UserAttr".into(), json!("same"));
            obs.push(o);
            des.push(d);
        }
        let obs_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: obs,
        })
        .unwrap();
        let des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: des,
        })
        .unwrap();
        let outcome =
            diff_v12_bulk_plain_properties(&des_bytes, &obs_bytes, "Workspace", 1).unwrap();
        assert!(
            matches!(outcome, BulkDiffOutcome::Unchanged),
            "RBX_-only attribute diff must be Unchanged"
        );
        let BulkExtractOutcome::Patch(extracted) = extract_v12_bulk_properties_for_classes(
            &des_bytes,
            "Workspace",
            &[("Part".into(), "@attributes".into())],
            1,
        )
        .unwrap() else {
            panic!("expected extract patch");
        };
        let col = extracted
            .patch
            .columns
            .iter()
            .find(|c| c.kind == BulkColumnKind::AttributesReplace)
            .expect("attributes_replace column");
        if let BulkPropertyValues::Indexed { palette, .. } = &col.values {
            for entry in palette {
                let obj = entry.as_object().expect("attr map");
                assert!(!obj.contains_key("RBX_SourceAssetId"));
                assert_eq!(obj.get("UserAttr"), Some(&json!("same")));
            }
        } else {
            panic!("expected indexed attributes_replace");
        }
    }

    #[test]
    fn history_patch_preserves_enum_item_inside_attributes_replace() {
        let mut instances = vec![
            part_instance("ws", "Workspace", "Workspace", None, Map::new()),
            part_instance("p1", "P1", "Part", Some(0), Map::new()),
        ];
        instances[1].attributes.insert("Kind".into(), json!("new"));
        let base_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances,
        })
        .unwrap();
        let rows_b64 = base64::encode(1_u32.to_le_bytes());
        let indices_b64 = base64::encode(0_u32.to_le_bytes());
        let old_cols = vec![json!({
            "kind": "attributes_replace",
            "propertyName": "@attributes",
            "encoding": "indexed",
            "count": 1,
            "rows": rows_b64,
            "palette": [{
                "Mat": {
                    "_type": "EnumItem",
                    "enumType": "Enum.Material",
                    "name": "Plastic"
                },
                "RBX_Ignore": 1
            }],
            "indices": indices_b64,
        })];
        let patched = patch_v12_bulk_old_value_columns(&base_bytes, &old_cols, &[1]).unwrap();
        let decoded = decode_settings_bytecode(&patched).unwrap();
        let mat = decoded.instances[1]
            .attributes
            .get("Mat")
            .expect("EnumItem attribute preserved");
        assert_eq!(mat.get("enumType"), Some(&json!("Enum.Material")));
        assert_eq!(mat.get("name"), Some(&json!("Plastic")));
        assert!(!decoded.instances[1].attributes.contains_key("RBX_Ignore"));
    }

    #[test]
    fn match_permuted_tree_rejects_mismatched_root_and_unpooled_rejects_ref_objects() {
        let des = vec![
            part_instance("ws", "Workspace", "Workspace", None, Map::new()),
            part_instance("p1", "P1", "Part", Some(0), Map::new()),
        ];
        let obs = vec![
            part_instance("ws", "RenamedWorkspace", "Workspace", None, Map::new()),
            part_instance("p1", "P1", "Part", Some(0), Map::new()),
        ];
        assert!(match_permuted_tree(&des, &[1, 1], &obs).is_none());

        let mut des_ref = vec![
            part_instance("ws", "Workspace", "Workspace", None, Map::new()),
            part_instance("a", "A", "Part", Some(0), Map::new()),
            part_instance("b", "B", "Part", Some(0), Map::new()),
        ];
        des_ref[1]
            .properties
            .insert("CustomObj".into(), json!({"instanceIndex": 2}));
        let mut obs_ref = vec![
            part_instance("ws", "Workspace", "Workspace", None, Map::new()),
            part_instance("b", "B", "Part", Some(0), Map::new()),
            part_instance("a", "A", "Part", Some(0), Map::new()),
        ];
        obs_ref[2]
            .properties
            .insert("CustomObj".into(), json!({"instanceIndex": 2}));
        let des_b = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: des_ref,
        })
        .unwrap();
        let obs_b = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: obs_ref,
        })
        .unwrap();
        let store_d = BlockStore::open(&des_b).unwrap();
        let store_o = BlockStore::open(&obs_b).unwrap();
        let open_d = store_d.open_block(0).unwrap();
        let open_o = store_o.open_block(0).unwrap();
        let mut body_out = Vec::new();
        write_var_u64(&mut body_out, 0).unwrap();
        write_var_u64(&mut body_out, 99).unwrap();
        let mut body_a = Vec::new();
        write_var_u64(&mut body_a, 0).unwrap();
        write_var_u64(&mut body_a, 99).unwrap();
        let eq = group_bodies_equal_unpooled(
            17,
            1,
            &body_a,
            &body_out,
            &store_d,
            &open_d,
            &store_o,
            &open_o,
            Some(&[0, 2, 1]),
        )
        .unwrap();
        assert!(!eq, "out-of-bounds ref_b under permutation must not match");
        let idx_d = open_d
            .strings
            .iter()
            .position(|s| matches!(s, BlockString::Own(t) if t == "instanceIndex"))
            .expect("instanceIndex string in des");
        let idx_o = open_o
            .strings
            .iter()
            .position(|s| matches!(s, BlockString::Own(t) if t == "instanceIndex"))
            .expect("instanceIndex string in obs");
        let mut obj_a = Vec::new();
        write_var_u64(&mut obj_a, 0).unwrap();
        write_var_u64(&mut obj_a, 1).unwrap();
        write_var_u64(&mut obj_a, idx_d as u64).unwrap();
        obj_a.push(3_u8);
        write_var_u64(&mut obj_a, zigzag_i64(2)).unwrap();
        let mut obj_b = Vec::new();
        write_var_u64(&mut obj_b, 0).unwrap();
        write_var_u64(&mut obj_b, 1).unwrap();
        write_var_u64(&mut obj_b, idx_o as u64).unwrap();
        obj_b.push(3_u8);
        write_var_u64(&mut obj_b, zigzag_i64(2)).unwrap();
        let obj_eq = group_bodies_equal_unpooled(
            8,
            1,
            &obj_a,
            &obj_b,
            &store_d,
            &open_d,
            &store_o,
            &open_o,
            Some(&[0, 2, 1]),
        )
        .unwrap();
        assert!(
            !obj_eq,
            "kind-8 object with instanceIndex must not compare equal under permutation"
        );
    }

    #[test]
    fn matching_rows_by_column_hash_handles_cframes_refs_and_permutations() {
        let mut desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        desired.instances.push(SettingsBytecodeInstance::new(
            "ws".to_string(),
            "Workspace".to_string(),
            "Workspace".to_string(),
            None,
        ));
        let mut part_a = SettingsBytecodeInstance::new(
            "a".to_string(),
            "A".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part_a.properties.insert(
            "CFrame".to_string(),
            json!({"_type": "CFrame", "components": [1.0, 2.0, 3.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]}),
        );
        let mut part_b = SettingsBytecodeInstance::new(
            "b".to_string(),
            "B".to_string(),
            "Part".to_string(),
            Some(0),
        );
        part_b
            .properties
            .insert("Transparency".to_string(), json!(0.25));
        let mut weld = SettingsBytecodeInstance::new(
            "w".to_string(),
            "W".to_string(),
            "WeldConstraint".to_string(),
            Some(1),
        );
        weld.properties.insert(
            "Part0".to_string(),
            json!({"_type": "Ref", "settingsId": "a"}),
        );
        weld.properties.insert(
            "Part1".to_string(),
            json!({"_type": "Ref", "settingsId": "b"}),
        );
        desired.instances.push(part_a.clone());
        desired.instances.push(part_b.clone());
        desired.instances.push(weld.clone());

        let mut observed = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        observed.instances.push(desired.instances[0].clone());
        let mut obs_b = part_b.clone();
        obs_b
            .properties
            .insert("Transparency".to_string(), json!(0.75));
        observed.instances.push(obs_b);
        observed.instances.push(part_a.clone());
        let mut obs_w = weld.clone();
        obs_w.parent_index = Some(2);
        observed.instances.push(obs_w);

        let d_bytes = crate::settings::bytecode::encode_settings_bytecode(&desired).unwrap();
        let o_bytes = crate::settings::bytecode::encode_settings_bytecode(&observed).unwrap();
        let places = [0, 2, 1, 3];
        let origins = [0, 2, 1, 3];
        let by_order = [false; 4];
        let d_store = BlockStore::open(&d_bytes).unwrap();
        let o_store = BlockStore::open(&o_bytes).unwrap();
        let matched = matching_rows_by_column_hash_stores(
            (&d_store, &o_store),
            (&desired.instances, &observed.instances),
            (&places, &origins, &by_order),
            None,
            &[],
        )
        .unwrap()
        .expect("column hash matches");
        assert!(!matched[0], "root is always materialized");
        assert!(matched[1], "permuted Part A with CFrame matches");
        assert!(!matched[2], "edited Part B does not match");
        assert!(
            matched[3],
            "permuted WeldConstraint with remapped refs matches"
        );
    }

    #[test]
    fn bulk_diff_allows_editable_package_descendants_and_records_package_targets() {
        let mut desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "pkg".into(),
                    "PkgModel".into(),
                    "Model".into(),
                    Some(0),
                ),
                SettingsBytecodeInstance::new(
                    "link".into(),
                    "PackageLink".into(),
                    "PackageLink".into(),
                    Some(1),
                ),
                SettingsBytecodeInstance::new(
                    "part".into(),
                    "Inside".into(),
                    "Part".into(),
                    Some(1),
                ),
            ],
        };
        desired.instances[3]
            .properties
            .insert("Transparency".into(), json!(0.5));
        let mut observed = desired.clone();
        observed.instances[3]
            .properties
            .insert("Transparency".into(), json!(0.0));

        let d_bytes = encode_settings_bytecode(&desired).unwrap();
        let o_bytes = encode_settings_bytecode(&observed).unwrap();
        match diff_v12_bulk_plain_properties(&d_bytes, &o_bytes, "Workspace", 1).unwrap() {
            BulkDiffOutcome::Patch(patch) => {
                assert_eq!(patch.instance_count, 1);
                assert_eq!(
                    patch.package_targets,
                    vec![BulkPackageTarget {
                        path_segments: vec!["Workspace".into(), "PkgModel".into()],
                        path_ordinals: vec![1, 1],
                    }]
                );
            }
            _ => panic!("expected editable package descendant to produce bulk patch"),
        }

        let mut link_only_desired = desired.clone();
        link_only_desired.instances[2]
            .properties
            .insert("AutoUpdate".into(), json!(true));
        let mut link_only_observed = desired;
        link_only_observed.instances[2]
            .properties
            .insert("AutoUpdate".into(), json!(false));
        let bd = encode_settings_bytecode(&link_only_desired).unwrap();
        let bo = encode_settings_bytecode(&link_only_observed).unwrap();
        assert!(matches!(
            diff_v12_bulk_plain_properties(&bd, &bo, "Workspace", 1).unwrap(),
            BulkDiffOutcome::Unchanged
        ));
    }

    #[test]
    fn bulk_diff_supports_content_id_strings_and_rejects_ref_content_id() {
        assert!(is_bulk_plain_value_eligible(
            &json!({"_type": "ContentId", "value": "rbxassetid://12345"})
        ));
        assert!(!is_bulk_plain_value_eligible(
            &json!({"_type": "ContentId", "value": {"_type": "Ref", "settingsId": "1"}})
        ));
        let db = rbx_reflection_database::get().unwrap();
        assert!(is_bulk_plain_property_class_eligible(
            db,
            "Workspace",
            "Sound",
            "SoundId"
        ));
    }

    #[test]
    fn scalar_string_block_patch_does_not_leak_pool_markers_for_long_untouched_strings() {
        let long_text = "L".repeat(320);
        let mut doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "rs".into(),
                    "ReplicatedStorage".into(),
                    "ReplicatedStorage".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "s1".into(),
                    "Long".into(),
                    "StringValue".into(),
                    Some(0),
                ),
                SettingsBytecodeInstance::new(
                    "s2".into(),
                    "Short".into(),
                    "StringValue".into(),
                    Some(0),
                ),
            ],
        };
        doc.instances[1]
            .properties
            .insert("Value".into(), json!(long_text));
        doc.instances[2]
            .properties
            .insert("Value".into(), json!("new"));
        let base_bytes = encode_settings_bytecode(&doc).unwrap();
        let mut updates = BTreeMap::new();
        updates.insert("Value".to_string(), vec![(2, json!("old"))]);
        let patched = patch_bulk_old_values_via_columns(&base_bytes, &updates).unwrap();
        let decoded = crate::settings::bytecode::decode_settings_bytecode(&patched).unwrap();
        assert_eq!(decoded.instances[1].properties["Value"], json!(long_text));
        assert_eq!(decoded.instances[2].properties["Value"], json!("old"));
        let store = BlockStore::open(&patched).unwrap();
        for b in 0..store.blocks.len() {
            let open = store.open_block(b).unwrap();
            for s in &open.strings {
                if let BlockString::Own(text) = s {
                    assert!(
                        !holds_marker(&Value::String(text.clone())),
                        "own string leaked pool marker: {text}"
                    );
                }
            }
        }
    }

    #[test]
    fn aligned_block_ref_comparison_remaps_indices_in_permuted_store() {
        let mut desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new("a".into(), "A".into(), "Folder".into(), Some(0)),
                SettingsBytecodeInstance::new("b".into(), "B".into(), "Folder".into(), Some(0)),
            ],
        };
        let mut observed = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new("b".into(), "B".into(), "Folder".into(), Some(0)),
                SettingsBytecodeInstance::new("a".into(), "A".into(), "Folder".into(), Some(0)),
            ],
        };
        for i in 0..5_000 {
            let inst = SettingsBytecodeInstance::new(
                format!("pad-{i}"),
                format!("Pad{i}"),
                "Folder".into(),
                Some(0),
            );
            desired.instances.push(inst.clone());
            observed.instances.push(inst);
        }
        let mut d_ov =
            SettingsBytecodeInstance::new("ov".into(), "Obj".into(), "ObjectValue".into(), Some(0));
        d_ov.properties
            .insert("Value".into(), json!({"_type": "Ref", "instanceIndex": 2}));
        let mut o_ov =
            SettingsBytecodeInstance::new("ov".into(), "Obj".into(), "ObjectValue".into(), Some(0));
        o_ov.properties
            .insert("Value".into(), json!({"_type": "Ref", "instanceIndex": 2}));
        desired.instances.push(d_ov);
        observed.instances.push(o_ov);

        let d_bytes = encode_settings_bytecode(&desired).unwrap();
        let o_bytes = encode_settings_bytecode(&observed).unwrap();
        assert!(
            BlockStore::open(&d_bytes).unwrap().blocks.len() >= 2,
            "test must span at least 2 blocks so block 1 is aligned while block 0 is permuted"
        );
        let outcome = diff_v12_bulk_plain_properties(&d_bytes, &o_bytes, "Workspace", 1).unwrap();
        assert!(
            !matches!(outcome, BulkDiffOutcome::Unchanged),
            "permuted Ref pointing to a different target must not compare Unchanged"
        );
    }

    #[test]
    fn zero_capture_extract_falls_back_for_unknown_when_absent_reset_on_spawn() {
        let mut desired = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "sg".into(),
                    "WorldGui".into(),
                    "SurfaceGui".into(),
                    Some(0),
                ),
            ],
        };
        desired.instances[1].properties.clear();
        let mut observed = desired.clone();
        observed.instances[1]
            .properties
            .insert("ResetOnSpawn".into(), json!(false));

        let d_bytes = encode_settings_bytecode(&desired).unwrap();
        let o_bytes = encode_settings_bytecode(&observed).unwrap();
        let diff_outcome =
            diff_v12_bulk_plain_properties(&d_bytes, &o_bytes, "Workspace", 1).unwrap();
        assert!(
            matches!(diff_outcome, BulkDiffOutcome::Unchanged),
            "Settings 2: absent ResetOnSpawn in files vs false in Studio must not write true"
        );

        let extract_outcome = extract_v12_bulk_properties_for_classes(
            &d_bytes,
            "Workspace",
            &[("SurfaceGui".into(), "ResetOnSpawn".into())],
            1,
        )
        .unwrap();
        assert!(
            matches!(extract_outcome, BulkExtractOutcome::Fallback),
            "S3/Settings 2: zero-capture extract must fall back when ResetOnSpawn is absent"
        );
    }

    #[test]
    fn aligned_block_current_camera_remaps_index_in_permuted_store() {
        let mut d2 = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "ws".into(),
                "Workspace".into(),
                "Workspace".into(),
                None,
            )],
        };
        let mut o2 = d2.clone();
        for i in 0..5_000 {
            let inst = SettingsBytecodeInstance::new(
                format!("pad-{i}"),
                format!("Pad{i}"),
                "Folder".into(),
                Some(0),
            );
            d2.instances.push(inst.clone());
            o2.instances.push(inst);
        }
        let cam_row_d = d2.instances.len();
        d2.instances.push(SettingsBytecodeInstance::new(
            "cam".into(),
            "Camera".into(),
            "Camera".into(),
            Some(0),
        ));
        d2.instances.push(SettingsBytecodeInstance::new(
            "part".into(),
            "TargetPart".into(),
            "Part".into(),
            Some(0),
        ));
        o2.instances.push(SettingsBytecodeInstance::new(
            "part".into(),
            "TargetPart".into(),
            "Part".into(),
            Some(0),
        ));
        let cam_row_o = o2.instances.len();
        o2.instances.push(SettingsBytecodeInstance::new(
            "cam".into(),
            "Camera".into(),
            "Camera".into(),
            Some(0),
        ));
        d2.instances[0].properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "instanceIndex": cam_row_d + 1}),
        );
        o2.instances[0].properties.insert(
            "CurrentCamera".into(),
            json!({"_type": "Ref", "instanceIndex": cam_row_o + 1}),
        );
        let d_bytes = encode_settings_bytecode(&d2).unwrap();
        let o_bytes = encode_settings_bytecode(&o2).unwrap();
        assert!(BlockStore::open(&d_bytes).unwrap().blocks.len() >= 2);
        let outcome = diff_v12_bulk_plain_properties(&d_bytes, &o_bytes, "Workspace", 1).unwrap();
        assert!(
            matches!(outcome, BulkDiffOutcome::Unchanged),
            "S2 residual: remapped CurrentCamera in aligned block 0 must compare Unchanged, not Fallback"
        );
    }

    #[test]
    fn column_hash_marks_settings_id_objects_ambiguous_under_identity_order() {
        let mut d = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "ov".into(),
                    "Obj".into(),
                    "ObjectValue".into(),
                    Some(0),
                ),
                SettingsBytecodeInstance::new("p1".into(), "Plain".into(), "Part".into(), Some(0)),
            ],
        };
        d.instances[1].properties.insert(
            "Value".into(),
            json!({"_type": "Ref", "settingsId": "shared-id"}),
        );
        d.instances[2]
            .properties
            .insert("Transparency".into(), json!(0.25));
        let o = d.clone();
        let d_bytes = encode_settings_bytecode(&d).unwrap();
        let o_bytes = encode_settings_bytecode(&o).unwrap();
        let ds = BlockStore::open(&d_bytes).unwrap();
        let os = BlockStore::open(&o_bytes).unwrap();
        let places = [0_usize, 1, 2];
        let origins = [0_usize, 1, 2];
        let by_order = [false, false, false];
        let matched = matching_rows_by_column_hash_stores(
            (&ds, &os),
            (&d.instances, &o.instances),
            (&places, &origins, &by_order),
            None,
            &[],
        )
        .unwrap()
        .expect("column hash must run");
        assert!(matched[2], "ordinary Part row must match by column hash");
        assert!(
            !matched[1],
            "Reconcile 5: settingsId object must be marked ambiguous even under identity row order"
        );
    }

    #[test]
    fn patch_scalar_block_preserves_untouched_pooled_strings_without_full_decode() {
        let long_text = "x".repeat(400);
        let mut doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "sg".into(),
                    "StarterGui".into(),
                    "StarterGui".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "l1".into(),
                    "LongLabel".into(),
                    "TextLabel".into(),
                    Some(0),
                ),
                SettingsBytecodeInstance::new(
                    "l2".into(),
                    "ShortLabel".into(),
                    "TextLabel".into(),
                    Some(0),
                ),
            ],
        };
        doc.instances[1]
            .properties
            .insert("Text".into(), json!(long_text));
        doc.instances[2]
            .properties
            .insert("Text".into(), json!("before"));
        let bytes = encode_settings_bytecode(&doc).unwrap();
        let store = BlockStore::open(&bytes).unwrap();
        assert!(!store.pool.is_empty(), "400-byte Text must be pooled");

        let mut block_updates: BTreeMap<&str, Vec<(usize, ScalarPatchValue)>> = BTreeMap::new();
        block_updates.insert("Text", vec![(2, ScalarPatchValue::String("after".into()))]);
        let patched_section =
            patch_scalar_block(&store, 0, &block_updates, &BTreeMap::new(), &[], &[]).unwrap();
        assert!(
            patched_section.is_some(),
            "Settings 5: patch_scalar_block must return Some when an untouched pooled string shares the block"
        );

        let patched_bytes = apply_scalar_block_patches(
            &store,
            &bytes,
            &[block_updates],
            &[BTreeMap::new()],
            &[Vec::new()],
            &[Vec::new()],
            &[],
            &[],
        )
        .unwrap();
        let decoded = decode_settings_bytecode(&patched_bytes).unwrap();
        assert_eq!(
            decoded.instances[1].properties.get("Text"),
            Some(&json!(long_text)),
            "untouched pooled string must decode back verbatim with no marker leak"
        );
        assert_eq!(
            decoded.instances[2].properties.get("Text"),
            Some(&json!("after")),
            "patched short string must decode to updated value"
        );
    }

    #[test]
    fn extracted_int_vs_f64_comparison_is_exact_above_2_to_53() {
        let big_int = ExtractedValue::Int(9_007_199_254_740_993);
        let rounded_f64 = ExtractedValue::F64(9_007_199_254_740_992.0);
        assert_eq!(
            extracted_values_match(&big_int, &rounded_f64, Some(RbxVariantType::Float64), false),
            Some(false)
        );
        let exact_int = ExtractedValue::Int(9_007_199_254_740_992);
        assert_eq!(
            extracted_values_match(
                &exact_int,
                &rounded_f64,
                Some(RbxVariantType::Float64),
                false
            ),
            Some(true)
        );
    }

    #[test]
    fn zero_capture_emits_model_assertions_for_model_and_subclasses() {
        let mut doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "ws".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new("m".into(), "Rig".into(), "Model".into(), Some(0)),
                SettingsBytecodeInstance::new("p1".into(), "P1".into(), "Part".into(), Some(1)),
                SettingsBytecodeInstance::new("a".into(), "Act".into(), "Actor".into(), Some(0)),
                SettingsBytecodeInstance::new("p2".into(), "P2".into(), "Part".into(), Some(3)),
                SettingsBytecodeInstance::new("t".into(), "Sword".into(), "Tool".into(), Some(0)),
                SettingsBytecodeInstance::new("p3".into(), "P3".into(), "Part".into(), Some(5)),
            ],
        };
        for model_idx in [1_usize, 3, 5] {
            doc.instances[model_idx]
                .properties
                .insert("Scale".into(), json!(1.0));
            doc.instances[model_idx]
                .properties
                .insert("WorldPivotData".into(), Value::Null);
        }
        for part_idx in [2_usize, 4, 6] {
            doc.instances[part_idx]
                .properties
                .insert("Transparency".into(), json!(0.25));
        }
        let bytes = encode_settings_bytecode(&doc).unwrap();
        let outcome = extract_v12_bulk_properties_for_classes(
            &bytes,
            "Workspace",
            &[("Part".into(), "Transparency".into())],
            1,
        )
        .unwrap();
        let BulkExtractOutcome::Patch(extracted) = outcome else {
            panic!("expected bulk extract patch");
        };
        let scale_col = extracted
            .patch
            .columns
            .iter()
            .find(|c| c.kind == BulkColumnKind::AssertModelScale)
            .expect("expected AssertModelScale column");
        assert_eq!(
            scale_col.rows.len(),
            3,
            "Earlier 6: AssertModelScale must cover Model, Actor, and Tool (got {:?})",
            scale_col.rows
        );
        let pivot_col = extracted
            .patch
            .columns
            .iter()
            .find(|c| {
                c.kind == BulkColumnKind::AssertModelPivot && c.property_name == "WorldPivotUnset"
            })
            .expect("expected WorldPivotUnset column");
        assert_eq!(
            pivot_col.rows.len(),
            3,
            "Earlier 6: AssertModelPivot must cover Model, Actor, and Tool (got {:?})",
            pivot_col.rows
        );
    }

    #[test]
    fn bulk_raw_cframe_position_noise_and_rederived_axis_match_reconciliation() {
        let base = ExtractedValue::CFrame([
            10.0, 20.0, 30.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
        ]);
        let pos_in_noise = ExtractedValue::CFrame([
            10.00015, 20.0, 30.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
        ]);
        let base_json = base.clone().into_json();
        let pos_noise_json = pos_in_noise.clone().into_json();
        let expected_noise = crate::settings::equivalence::reconciliation_property_values_equal(
            "Part",
            "CFrame",
            Some(&base_json),
            Some(&pos_noise_json),
        );
        assert!(
            expected_noise,
            "Earlier 4: 1.5e-4 position delta must be accepted by reconciliation_property_values_equal"
        );
        assert_eq!(
            extracted_values_match(&base, &pos_in_noise, Some(RbxVariantType::CFrame), false),
            Some(true),
            "Earlier 4: position-noise CFrame window must accept 1.5e-4 delta"
        );
        let pos_shifted = ExtractedValue::CFrame([
            10.01, 20.0, 30.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
        ]);
        assert_eq!(
            extracted_values_match(&base, &pos_shifted, Some(RbxVariantType::CFrame), false),
            Some(false)
        );
        let rederived_z = ExtractedValue::CFrame([
            10.0, 20.0, 30.0, 1.0, 0.0, 0.005, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
        ]);
        let rederived_json = rederived_z.clone().into_json();
        let expected_rederived = crate::settings::equivalence::reconciliation_property_values_equal(
            "Part",
            "CFrame",
            Some(&base_json),
            Some(&rederived_json),
        );
        assert!(
            expected_rederived,
            "Earlier 4: 3rd-axis drift with orthonormal cols 1&2 must be accepted by rederived-axis branch"
        );
        assert_eq!(
            extracted_values_match(&base, &rederived_z, Some(RbxVariantType::CFrame), false),
            Some(true),
            "Earlier 4: extracted_values_match must accept rederived-axis CFrame"
        );
        let mut d_doc = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                part_instance("ws", "Workspace", "Workspace", None, Map::new()),
                part_instance(
                    "p1",
                    "P1",
                    "Part",
                    Some(0),
                    Map::from_iter([
                        ("CFrame".into(), base_json.clone()),
                        ("Transparency".into(), json!(0.5)),
                    ]),
                ),
                part_instance(
                    "p2",
                    "P2",
                    "Part",
                    Some(0),
                    Map::from_iter([("CFrame".into(), base_json.clone())]),
                ),
                part_instance(
                    "nv",
                    "NV",
                    "NumberValue",
                    Some(0),
                    Map::from_iter([("Value".into(), json!(1.0))]),
                ),
            ],
        };
        let mut o_doc = d_doc.clone();
        o_doc.instances[1]
            .properties
            .insert("CFrame".into(), pos_noise_json);
        o_doc.instances[1]
            .properties
            .insert("Transparency".into(), json!(0.500_000_01));
        o_doc.instances[2]
            .properties
            .insert("CFrame".into(), rederived_json);
        let d_bytes = encode_settings_bytecode(&d_doc).unwrap();
        let o_bytes = encode_settings_bytecode(&o_doc).unwrap();
        assert!(
            matches!(
                diff_v12_bulk_plain_properties(&d_bytes, &o_bytes, "Workspace", 1).unwrap(),
                BulkDiffOutcome::Unchanged
            ),
            "Earlier 4: CFrame noise/rederived-axis and Float32 1e-8 delta must be Unchanged in bulk diff"
        );
        d_doc.instances[3]
            .properties
            .insert("Value".into(), json!(1.000_000_01));
        let d_bytes_f64 = encode_settings_bytecode(&d_doc).unwrap();
        assert!(
            matches!(
                diff_v12_bulk_plain_properties(&d_bytes_f64, &o_bytes, "Workspace", 1).unwrap(),
                BulkDiffOutcome::Patch(ref p) if p.instance_count == 1 && p.columns[0].property_name == "Value"
            ),
            "Earlier 4: Float64 NumberValue.Value 1e-8 delta must produce a patch in bulk diff"
        );
    }
}
