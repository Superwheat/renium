use std::collections::{BTreeMap, HashMap};

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
    BulkPropertyColumn, BulkPropertyValues, BulkTreeNode, EditorBulkPropertyPatch,
};
use crate::settings::equivalence::{
    reconciliation_property_is_derived, reconciliation_property_values_equal,
};

type PropertyGroupSlice<'a> = (u8, usize, &'a [u8]);
type BlockPartition = (Vec<(usize, usize)>, Vec<(usize, usize)>);
#[cfg(any(windows, target_os = "macos", test))]
type RawBlockGroup<'a> = (usize, u8, usize, &'a [u8]);

enum BlockResult {
    Fallback,
    Edits(Vec<(usize, String, Value)>),
}

struct BulkDiffContext<'a> {
    desired: &'a BlockStore<'a>,
    observed: &'a BlockStore<'a>,
    desired_rows: &'a [SettingsBytecodeInstance],
    obs_to_des: &'a [usize],
    is_identity_perm: bool,
    is_twin: &'a [bool],
    in_package: &'a [bool],
    database: &'a ReflectionDatabase<'a>,
    service: &'a str,
}

pub(crate) enum BulkDiffOutcome {
    Unchanged,
    Patch(EditorBulkPropertyPatch),
    Fallback,
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
    let (aligned_blocks, permuted_pairs) = partition_bulk_blocks(
        &desired,
        &observed,
        &topology.obs_to_des,
        topology.is_identity_perm,
        service,
    );

    let database = rbx_reflection_database::get()?;
    let ctx = BulkDiffContext {
        desired: &desired,
        observed: &observed,
        desired_rows: &desired_rows,
        obs_to_des: &topology.obs_to_des,
        is_identity_perm: topology.is_identity_perm,
        is_twin: &topology.is_twin,
        in_package: &topology.in_package,
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
        &desired_rows,
        &topology.sibling_ordinals,
        all_edits,
        database,
        service,
        min_instances,
    ))
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
    in_package: Vec<bool>,
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

    let mut in_package = vec![false; row_count];
    let mut visited = vec![false; row_count];
    let mut chain = Vec::with_capacity(64);
    for index in 0..row_count {
        if visited[index] {
            continue;
        }
        chain.clear();
        let mut cursor = Some(index);
        let mut parent_pkg = false;
        let mut parent_twin = false;
        while let Some(node) = cursor {
            if node >= row_count || chain.len() > row_count {
                return None;
            }
            if visited[node] {
                parent_pkg = in_package[node];
                parent_twin = is_twin[node];
                break;
            }
            chain.push(node);
            cursor = desired_rows[node].parent_index;
        }
        for &node in chain.iter().rev() {
            if desired_rows[node].class_name == "PackageLink" || is_package_root[node] {
                parent_pkg = true;
            }
            if is_twin[node] {
                parent_twin = true;
            }
            in_package[node] = parent_pkg;
            is_twin[node] = parent_twin;
            visited[node] = true;
        }
    }

    Some(BulkTopology {
        sibling_ordinals,
        obs_to_des,
        is_identity_perm,
        is_twin,
        in_package,
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
            let o_vals = decode_block_property(
                ctx.observed,
                &open_o,
                &mut o_strings_cache,
                prop_name,
                o_groups,
                block_count,
            )?;
            if d_vals.get(&0) != o_vals.get(&0)
                && d_vals.contains_key(&0)
                && o_vals.contains_key(&0)
            {
                return Ok(BlockResult::Fallback);
            }
            continue;
        }
        if matches!(
            prop_name,
            "Attributes"
                | "Tags"
                | "Source"
                | "LinkedSource"
                | "Name"
                | "Parent"
                | "ClassName"
                | "Archivable"
        ) {
            return Ok(BlockResult::Fallback);
        }

        let all_scalar_floats = d_groups.iter().all(|&(k, _, _)| k == 5 || k == 20)
            && o_groups.iter().all(|&(k, _, _)| k == 5 || k == 20)
            && !reconciliation_property_is_derived(prop_name);
        if all_scalar_floats {
            if !diff_aligned_float_column(
                ctx,
                prop_name,
                d_groups,
                o_groups,
                (block_first, block_count),
                &mut eligible_cache,
                &mut edits,
            )? {
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
        let o_vals = decode_block_property(
            ctx.observed,
            &open_o,
            &mut o_strings_cache,
            prop_name,
            o_groups,
            block_count,
        )?;
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

fn scalar_floats_match(a: f64, b: f64, is_f32: bool) -> bool {
    a == b || (is_f32 && (a - b).abs() <= 1e-6)
}

fn diff_aligned_float_column<'a>(
    ctx: &'a BulkDiffContext<'a>,
    prop_name: &'a str,
    d_groups: &[PropertyGroupSlice<'_>],
    o_groups: &[PropertyGroupSlice<'_>],
    (block_first, block_count): (usize, usize),
    eligible_cache: &mut AHashMap<(&'a str, &'a str), bool>,
    edits: &mut Vec<(usize, String, Value)>,
) -> Result<bool> {
    let d_floats = decode_block_float_column(d_groups, block_count)?;
    let o_floats = decode_block_float_column(o_groups, block_count)?;
    let mut default_cache: AHashMap<&str, Option<Option<f64>>> = AHashMap::new();
    let mut f32_cache: AHashMap<&str, bool> = AHashMap::new();
    let mut di = 0_usize;
    let mut oi = 0_usize;

    while di < d_floats.len() || oi < o_floats.len() {
        let (local_row, d_num, o_num) = if di < d_floats.len()
            && (oi >= o_floats.len() || d_floats[di].0 < o_floats[oi].0)
        {
            let (r, v) = d_floats[di];
            di += 1;
            (r, Some(v), None)
        } else if oi < o_floats.len() && (di >= d_floats.len() || o_floats[oi].0 < d_floats[di].0) {
            let (r, v) = o_floats[oi];
            oi += 1;
            (r, None, Some(v))
        } else {
            let (r, dv) = d_floats[di];
            let (_, ov) = o_floats[oi];
            di += 1;
            oi += 1;
            (r, Some(dv), Some(ov))
        };
        let global_row = block_first + local_row;
        let class_name = ctx.desired_rows[global_row].class_name.as_str();
        let is_f32 = *f32_cache.entry(class_name).or_insert_with(|| {
            matches!(
                crate::rbx::encode::rbx_property_descriptor(ctx.database, class_name, prop_name)
                    .map(|d| &d.data_type),
                Some(RbxDataType::Value(RbxVariantType::Float32))
            )
        });
        if let (Some(dv), Some(ov)) = (d_num, o_num)
            && scalar_floats_match(dv, ov, is_f32)
        {
            continue;
        }
        let target_num = match d_num {
            Some(dv) => {
                if !dv.is_finite() {
                    return Ok(false);
                }
                if o_num.is_none() {
                    let default_opt = *default_cache.entry(class_name).or_insert_with(|| {
                        cached_default_float(ctx.database, class_name, prop_name)
                    });
                    if let Some(Some(def)) = default_opt
                        && scalar_floats_match(dv, def, is_f32)
                    {
                        continue;
                    }
                }
                dv
            }
            None => {
                let default_opt = *default_cache
                    .entry(class_name)
                    .or_insert_with(|| cached_default_float(ctx.database, class_name, prop_name));
                match default_opt {
                    None => return Ok(false),
                    Some(None) => continue,
                    Some(Some(def)) => {
                        if let Some(ov) = o_num
                            && scalar_floats_match(def, ov, is_f32)
                        {
                            continue;
                        }
                        def
                    }
                }
            }
        };
        if global_row == 0
            || ctx.in_package[global_row]
            || class_name == "Camera"
            || class_name == "PackageLink"
        {
            return Ok(false);
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
            return Ok(false);
        }
        edits.push((global_row, prop_name.to_string(), json!(target_num)));
    }
    Ok(true)
}

fn diff_aligned_decoded_column<'a>(
    ctx: &'a BulkDiffContext<'a>,
    prop_name: &'a str,
    d_vals: &AHashMap<usize, Value>,
    o_vals: &AHashMap<usize, Value>,
    block_first: usize,
    eligible_cache: &mut AHashMap<(&'a str, &'a str), bool>,
    edits: &mut Vec<(usize, String, Value)>,
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
        if global_row == 0
            || ctx.in_package[global_row]
            || class_name == "Camera"
            || class_name == "PackageLink"
        {
            crate::app::output::log_global(
                4,
                format_args!(
                    "[renium] bulk diff fallback ({}): row={} class={class_name} prop={prop_name} in_package={}",
                    ctx.service, global_row, ctx.in_package[global_row]
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
        edits.push((global_row, prop_name.to_string(), target_value));
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
    if crate::settings::equivalence::capture_may_omit(class_name, prop_name) {
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

fn build_bulk_property_patch(
    desired_rows: &[SettingsBytecodeInstance],
    sibling_ordinals: &[u32],
    all_edits: Vec<(usize, String, Value)>,
    database: &ReflectionDatabase<'_>,
    service: &str,
    min_instances: usize,
) -> BulkDiffOutcome {
    match build_bulk_property_patch_with_rows(
        desired_rows,
        sibling_ordinals,
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
    all_edits: Vec<(usize, String, Value)>,
    database: &ReflectionDatabase<'_>,
    service: &str,
    min_instances: usize,
) -> Option<(EditorBulkPropertyPatch, Vec<usize>)> {
    let row_count = desired_rows.len();
    let mut touched_rows: Vec<usize> = all_edits.iter().map(|(row, _, _)| *row).collect();
    touched_rows.sort_unstable();
    touched_rows.dedup();
    let instance_count = touched_rows.len();
    if instance_count < min_instances {
        return None;
    }

    let mut needed = vec![false; row_count];
    for &row in &touched_rows {
        let mut cursor = Some(row);
        while let Some(idx) = cursor {
            if idx == 0 || needed[idx] {
                break;
            }
            needed[idx] = true;
            cursor = desired_rows[idx].parent_index;
        }
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
    let mut by_property: BTreeMap<String, Vec<(u32, &str, Value)>> = BTreeMap::new();
    for (global_row, prop_name, value) in all_edits {
        let node_id = row_to_node[global_row];
        if node_id == 0 {
            return None;
        }
        by_property.entry(prop_name).or_default().push((
            node_id,
            desired_rows[global_row].class_name.as_str(),
            value,
        ));
    }

    let mut columns = Vec::with_capacity(by_property.len());
    for (property_name, mut entries) in by_property {
        entries.sort_unstable_by_key(|(node_id, _, _)| *node_id);
        encode_property_columns(database, property_name, entries, &mut columns);
    }
    columns.sort_by_key(|col| match col.property_name.as_str() {
        "Anchored" => (0_u8, col.property_name.clone()),
        "Size" => (1_u8, col.property_name.clone()),
        "CFrame" => (2_u8, col.property_name.clone()),
        _ => (3_u8, col.property_name.clone()),
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
            let mapped_b = obs_to_des
                .and_then(|map| map.get(ref_b).copied())
                .unwrap_or(ref_b);
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
            let mapped_b = obs_to_des
                .and_then(|map| map.get(ref_b).copied())
                .unwrap_or(ref_b);
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
) -> Result<Option<Vec<(usize, String, Value)>>> {
    let desired = ctx.desired;
    let observed = ctx.observed;
    let desired_rows = ctx.desired_rows;
    let obs_to_des = ctx.obs_to_des;
    let is_twin = ctx.is_twin;
    let in_package = ctx.in_package;
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
        Edits(Vec<(usize, String, Value)>),
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
                if d_vals.attributes != o_vals.attributes {
                    crate::app::output::log_global(
                        4,
                        format_args!("[renium] bulk diff fallback ({service}): permuted row attributes differ"),
                    );
                    return Ok(ChunkResult::Fallback);
                }
                if is_twin[d_row]
                    && d_vals.properties.get(UNIQUE_ID) != o_vals.properties.get(UNIQUE_ID)
                {
                    return Ok(ChunkResult::Fallback);
                }
                let class_name = desired_rows[d_row].class_name.as_str();
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
                    if d_val.is_some_and(holds_marker) || o_val.is_some_and(holds_marker) {
                        return Ok(ChunkResult::Fallback);
                    }
                    if reconciliation_property_values_equal(class_name, prop_name, d_val, o_val) {
                        continue;
                    }
                    if reconciliation_property_is_derived(prop_name) {
                        continue;
                    }
                    if d_row == 0
                        || in_package[d_row]
                        || class_name == "Camera"
                        || class_name == "PackageLink"
                    {
                        return Ok(ChunkResult::Fallback);
                    }
                    let target_value = match d_val {
                        Some(val) => val.clone(),
                        None => {
                            if crate::settings::equivalence::capture_may_omit(class_name, prop_name)
                            {
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
                    edits.push((d_row, prop_name.to_string(), target_value));
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

fn decode_block_float_column(
    groups: &[(u8, usize, &[u8])],
    block_count: usize,
) -> Result<Vec<(usize, f64)>> {
    let total: usize = groups.iter().map(|&(_, c, _)| c).sum();
    let mut out = Vec::with_capacity(total);
    for &(kind, count, body) in groups {
        let mut reader = BytecodeReader::new(body);
        let mut row = 0_usize;
        for pos in 0..count {
            row = next_group_instance(row, reader.read_len("property instance index")?, pos)?;
            anyhow::ensure!(
                row < block_count,
                "A value block gives a value to a row it does not hold"
            );
            let value = match kind {
                5 => {
                    let b = reader.read_bytes(8)?;
                    f64::from_le_bytes(b.try_into().unwrap())
                }
                20 => {
                    let b = reader.read_bytes(4)?;
                    f64::from(f32::from_le_bytes(b.try_into().unwrap()))
                }
                _ => bail!("Unexpected non-float group kind {kind}"),
            };
            out.push((row, value));
        }
        reader.finish()?;
    }
    if groups.len() > 1 {
        out.sort_unstable_by_key(|&(r, _)| r);
    }
    Ok(out)
}

fn cached_default_float(
    database: &ReflectionDatabase<'_>,
    class_name: &str,
    prop_name: &str,
) -> Option<Option<f64>> {
    if crate::settings::equivalence::capture_may_omit(class_name, prop_name) {
        return Some(None);
    }
    let default_val = reflection_default_json(database, class_name, prop_name)?;
    if !reconciliation_property_values_equal(class_name, prop_name, None, Some(&default_val)) {
        return None;
    }
    let num = default_val.as_f64()?;
    num.is_finite().then_some(Some(num))
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
            | "Position"
            | "Orientation"
            | "Rotation"
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
    if property_name == "Enabled"
        && crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BaseScript")
    {
        return false;
    }
    if property_name == "CFrame"
        && !crate::rbx::decode::rbx_reflection_class_is_a(database, class_name, "BasePart")
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
        ),
        _ => false,
    }
}

fn is_bulk_plain_value_eligible(value: &Value) -> bool {
    !value
        .as_object()
        .and_then(|obj| obj.get("_type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "Ref"
                    | "Number"
                    | "BinaryString"
                    | "SharedString"
                    | "Content"
                    | "ContentId"
                    | "Font"
                    | "UniqueId"
            )
        })
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
    property_name: String,
    rows: Vec<u32>,
    values: Vec<Value>,
) -> BulkPropertyColumn {
    let mut palette: Vec<Value> = Vec::new();
    let mut by_key: HashMap<String, u32> = HashMap::new();
    let mut indices = Vec::with_capacity(values.len());
    for value in values {
        let key = serde_json::to_string(&value).unwrap_or_default();
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
        property_name,
        rows,
        values: BulkPropertyValues::Indexed { palette, indices },
    }
}

fn encode_property_columns(
    database: &ReflectionDatabase<'_>,
    property_name: String,
    entries: Vec<(u32, &str, Value)>,
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
    let mut other_rows = Vec::new();
    let mut other_vals = Vec::new();

    for (node_id, class_name, mut value) in entries {
        let data_type =
            crate::rbx::encode::rbx_property_descriptor(database, class_name, &property_name)
                .map(|d| &d.data_type);
        let variant_type = match data_type {
            Some(RbxDataType::Value(v)) => Some(*v),
            _ => None,
        };
        enrich_enum_item_type(data_type, &mut value);
        match (&value, variant_type) {
            (Value::Bool(b), Some(RbxVariantType::Bool)) => {
                bool_rows.push(node_id);
                bool_vals.push(u8::from(*b));
            }
            (Value::Number(n), Some(RbxVariantType::Float32)) => {
                if let Some(f) = n.as_f64() {
                    f32_rows.push(node_id);
                    f32_vals.push(f as f32);
                } else {
                    other_rows.push(node_id);
                    other_vals.push(value);
                }
            }
            (Value::Number(n), Some(RbxVariantType::Int32)) => {
                if let Some(i) = n.as_i64().and_then(|v| i32::try_from(v).ok()) {
                    i32_rows.push(node_id);
                    i32_vals.push(i);
                } else {
                    other_rows.push(node_id);
                    other_vals.push(value);
                }
            }
            (Value::Number(n), Some(RbxVariantType::Float64)) => {
                if let Some(f) = n.as_f64() {
                    f64_rows.push(node_id);
                    f64_vals.push(f);
                } else {
                    other_rows.push(node_id);
                    other_vals.push(value);
                }
            }
            (Value::Object(obj), Some(RbxVariantType::Vector3))
                if obj.get("_type").and_then(Value::as_str) == Some("Vector3") =>
            {
                if let (Some(x), Some(y), Some(z)) = (
                    obj.get("x").and_then(Value::as_f64),
                    obj.get("y").and_then(Value::as_f64),
                    obj.get("z").and_then(Value::as_f64),
                ) {
                    vec3_rows.push(node_id);
                    vec3_vals.push([x as f32, y as f32, z as f32]);
                } else {
                    other_rows.push(node_id);
                    other_vals.push(value);
                }
            }
            (Value::Object(obj), Some(RbxVariantType::Color3))
                if obj.get("_type").and_then(Value::as_str) == Some("Color3") =>
            {
                if let (Some(r), Some(g), Some(b)) = (
                    obj.get("r").and_then(Value::as_f64),
                    obj.get("g").and_then(Value::as_f64),
                    obj.get("b").and_then(Value::as_f64),
                ) {
                    color3_rows.push(node_id);
                    color3_vals.push([r as f32, g as f32, b as f32]);
                } else {
                    other_rows.push(node_id);
                    other_vals.push(value);
                }
            }
            (Value::Object(obj), Some(RbxVariantType::CFrame))
                if obj.get("_type").and_then(Value::as_str) == Some("CFrame") =>
            {
                if let Some(arr) = obj.get("components").and_then(Value::as_array)
                    && arr.len() == 12
                    && let Some(vals) = arr
                        .iter()
                        .map(|v| v.as_f64().filter(|f| f.is_finite()).map(|f| f as f32))
                        .collect::<Option<Vec<_>>>()
                {
                    cframe_rows.push(node_id);
                    cframe_vals.push(vals.try_into().unwrap());
                } else {
                    other_rows.push(node_id);
                    other_vals.push(value);
                }
            }
            _ => {
                other_rows.push(node_id);
                other_vals.push(value);
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
            property_name: property_name.clone(),
            rows: f64_rows,
            values,
        });
    }
    if !i32_rows.is_empty() {
        out.push(BulkPropertyColumn {
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
            property_name: property_name.clone(),
            rows: bool_rows,
            values,
        });
    }
    if !vec3_rows.is_empty() {
        out.push(BulkPropertyColumn {
            property_name: property_name.clone(),
            rows: vec3_rows,
            values: BulkPropertyValues::Vector3(vec3_vals),
        });
    }
    if !color3_rows.is_empty() {
        out.push(BulkPropertyColumn {
            property_name: property_name.clone(),
            rows: color3_rows,
            values: BulkPropertyValues::Color3(color3_vals),
        });
    }
    if !cframe_rows.is_empty() {
        out.push(BulkPropertyColumn {
            property_name: property_name.clone(),
            rows: cframe_rows,
            values: BulkPropertyValues::CFrame(cframe_vals),
        });
    }
    if !other_rows.is_empty() {
        out.push(build_indexed_column(property_name, other_rows, other_vals));
    }
}

#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn is_zero_capture_safe_property(property_name: &str) -> bool {
    matches!(
        property_name,
        "Transparency"
            | "Reflectance"
            | "CastShadow"
            | "Locked"
            | "Visible"
            | "ZIndex"
            | "LayoutOrder"
            | "BackgroundTransparency"
            | "TextTransparency"
            | "TextStrokeTransparency"
            | "ImageTransparency"
            | "ScrollBarImageTransparency"
            | "GroupTransparency"
            | "FillTransparency"
            | "OutlineTransparency"
            | "LightEmission"
            | "LightInfluence"
            | "Range"
            | "Angle"
            | "Shadows"
            | "Volume"
            | "PlaybackSpeed"
            | "Looped"
            | "RollOffMaxDistance"
            | "RollOffMinDistance"
    )
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
fn single_store_topology(rows: &[SettingsBytecodeInstance]) -> Option<(Vec<u32>, Vec<bool>)> {
    let row_count = rows.len();
    let mut sibling_ordinals = vec![1_u32; row_count];
    let mut name_counts: AHashMap<(Option<usize>, &str), u32> = AHashMap::with_capacity(row_count);
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
    }
    let mut in_package = vec![false; row_count];
    let mut visited = vec![false; row_count];
    let mut chain = Vec::with_capacity(64);
    for index in 0..row_count {
        if visited[index] {
            continue;
        }
        chain.clear();
        let mut cursor = Some(index);
        let mut parent_pkg = false;
        while let Some(node) = cursor {
            if node >= row_count || chain.len() > row_count {
                return None;
            }
            if visited[node] {
                parent_pkg = in_package[node];
                break;
            }
            chain.push(node);
            cursor = rows[node].parent_index;
        }
        for &node in chain.iter().rev() {
            if rows[node].class_name == "PackageLink" || is_package_root[node] {
                parent_pkg = true;
            }
            in_package[node] = parent_pkg;
            visited[node] = true;
        }
    }
    Some((sibling_ordinals, in_package))
}

#[cfg(any(windows, target_os = "macos", test))]
fn classify_extract_targets<'a>(
    rows: &'a [SettingsBytecodeInstance],
    in_package: &[bool],
    prop_names: &'a [String],
    database: &ReflectionDatabase<'_>,
    service: &str,
) -> Option<(Vec<u8>, usize)> {
    let mut class_cache: AHashMap<(&'a str, &'a str), ExtractClassProp> = AHashMap::new();
    let mut row_masks = vec![0_u8; rows.len()];
    let mut target_count = 0_usize;
    for (row_idx, row) in rows.iter().enumerate() {
        let class_name = row.class_name.as_str();
        let mut mask = 0_u8;
        for (prop_bit, prop_name) in prop_names.iter().enumerate() {
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
                ExtractClassProp::Absent => {}
                ExtractClassProp::Ineligible => return None,
                ExtractClassProp::Eligible => {
                    if row_idx == 0
                        || in_package[row_idx]
                        || class_name == "Camera"
                        || class_name == "PackageLink"
                    {
                        return None;
                    }
                    mask |= 1_u8 << prop_bit;
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

#[cfg(any(windows, target_os = "macos", test))]
fn extract_block_properties(
    store: &BlockStore<'_>,
    block_index: usize,
    rows: &[SettingsBytecodeInstance],
    row_masks: &[u8],
    prop_names: &[String],
    database: &ReflectionDatabase<'_>,
) -> Result<BlockResult> {
    let block = &store.blocks[block_index];
    let block_first = block.first;
    let block_count = block.count;
    if !row_masks[block_first..block_first + block_count]
        .iter()
        .any(|&m| m != 0)
    {
        return Ok(BlockResult::Edits(Vec::new()));
    }
    let open = store.open_block(block_index)?;
    let block_props = block_property_names(store, &open)?;
    let mut reader = BytecodeReader::new(&open.content[open.rest..]);
    let specs = read_settings_property_groups(&mut reader, &block_props)?;
    reader.finish()?;

    let mut by_name: AHashMap<&str, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    for (name, kind, count, body) in specs {
        if prop_names.iter().any(|p| p == name) {
            by_name.entry(name).or_default().push((kind, count, body));
        }
    }

    let mut strings_cache: Option<Vec<String>> = None;
    let mut edits = Vec::new();
    for (prop_bit, prop_name) in prop_names.iter().enumerate() {
        let bit = 1_u8 << prop_bit;
        let groups = by_name
            .get(prop_name.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let all_floats = !groups.is_empty() && groups.iter().all(|&(k, _, _)| k == 5 || k == 20);
        if all_floats {
            let floats = decode_block_float_column(groups, block_count)?;
            let mut default_cache: AHashMap<&str, Option<Value>> = AHashMap::new();
            let mut fi = 0_usize;
            for local_row in 0..block_count {
                while fi < floats.len() && floats[fi].0 < local_row {
                    fi += 1;
                }
                let global_row = block_first + local_row;
                if row_masks[global_row] & bit == 0 {
                    continue;
                }
                let value = if fi < floats.len() && floats[fi].0 == local_row {
                    let num = floats[fi].1;
                    if !num.is_finite() {
                        return Ok(BlockResult::Fallback);
                    }
                    json_number_f64(num)
                } else {
                    let class_name = rows[global_row].class_name.as_str();
                    let Some(def) = default_cache
                        .entry(class_name)
                        .or_insert_with(|| reflection_default_json(database, class_name, prop_name))
                        .clone()
                    else {
                        return Ok(BlockResult::Fallback);
                    };
                    def
                };
                edits.push((global_row, prop_name.clone(), value));
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
        let mut default_cache: AHashMap<&str, Option<Value>> = AHashMap::new();
        for local_row in 0..block_count {
            let global_row = block_first + local_row;
            if row_masks[global_row] & bit == 0 {
                continue;
            }
            let value = if let Some(val) = decoded.get(&local_row) {
                if holds_marker(val) || !is_bulk_plain_value_eligible(val) {
                    return Ok(BlockResult::Fallback);
                }
                val.clone()
            } else {
                let class_name = rows[global_row].class_name.as_str();
                let Some(def) = default_cache
                    .entry(class_name)
                    .or_insert_with(|| reflection_default_json(database, class_name, prop_name))
                    .clone()
                else {
                    return Ok(BlockResult::Fallback);
                };
                if !is_bulk_plain_value_eligible(&def) {
                    return Ok(BlockResult::Fallback);
                }
                def
            };
            edits.push((global_row, prop_name.clone(), value));
        }
    }
    Ok(BlockResult::Edits(edits))
}

#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn extract_v12_bulk_properties(
    desired_bytes: &[u8],
    service: &str,
    prop_names: &[String],
    min_instances: usize,
) -> Result<BulkExtractOutcome> {
    if prop_names.is_empty()
        || prop_names.len() > 8
        || !prop_names
            .iter()
            .all(|name| is_zero_capture_safe_property(name))
        || !is_block_store(desired_bytes)
    {
        return Ok(BulkExtractOutcome::Fallback);
    }
    let Ok(store) = BlockStore::open(desired_bytes) else {
        return Ok(BulkExtractOutcome::Fallback);
    };
    if store.rows == 0 {
        return Ok(BulkExtractOutcome::Fallback);
    }
    let rows = store.structure()?.instances;
    if rows.len() != store.rows || rows[0].parent_index.is_some() || rows[0].class_name != service {
        return Ok(BulkExtractOutcome::Fallback);
    }
    let Some((sibling_ordinals, in_package)) = single_store_topology(&rows) else {
        return Ok(BulkExtractOutcome::Fallback);
    };
    let database = rbx_reflection_database::get()?;
    let Some((row_masks, target_count)) =
        classify_extract_targets(&rows, &in_package, prop_names, database, service)
    else {
        return Ok(BulkExtractOutcome::Fallback);
    };
    if target_count < min_instances {
        return Ok(BulkExtractOutcome::Fallback);
    }
    let block_results = (0..store.blocks.len())
        .into_par_iter()
        .map(|block_index| {
            extract_block_properties(&store, block_index, &rows, &row_masks, prop_names, database)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut all_edits = Vec::new();
    for result in block_results {
        match result {
            BlockResult::Fallback => return Ok(BulkExtractOutcome::Fallback),
            BlockResult::Edits(mut part) => all_edits.append(&mut part),
        }
    }
    match build_bulk_property_patch_with_rows(
        &rows,
        &sibling_ordinals,
        all_edits,
        database,
        service,
        min_instances,
    ) {
        Some((patch, node_to_row)) => Ok(BulkExtractOutcome::Patch(BulkExtractedPatch {
            patch,
            node_to_row,
        })),
        None => Ok(BulkExtractOutcome::Fallback),
    }
}

#[cfg(any(windows, target_os = "macos", test))]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn decode_bulk_old_value_columns(
    columns: &[Value],
    node_to_row: &[usize],
) -> Result<BTreeMap<String, Vec<(usize, Value)>>> {
    let mut out: BTreeMap<String, Vec<(usize, Value)>> = BTreeMap::new();
    for column in columns {
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
        let rows_b64 = column["rows"]
            .as_str()
            .context("Bulk old value column missing rows")?;
        let row_bytes = base64::decode(rows_b64).context("Invalid bulk old value rows base64")?;
        anyhow::ensure!(
            row_bytes.len() == count * 4,
            "Bulk old value rows size mismatch"
        );
        let entry = out.entry(prop_name.to_string()).or_default();
        entry.reserve(count);
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
        if encoding == "json" {
            let values = column["values"]
                .as_array()
                .context("Bulk old value json column missing values array")?;
            anyhow::ensure!(values.len() == count, "Bulk old value json length mismatch");
            for (idx, val) in values.iter().enumerate() {
                entry.push((resolve_row(idx)?, val.clone()));
            }
            continue;
        }
        let vals_b64 = column["values"]
            .as_str()
            .context("Bulk old value column missing values")?;
        let val_bytes = base64::decode(vals_b64).context("Invalid bulk old value data base64")?;
        decode_binary_old_values(encoding, count, &val_bytes, &resolve_row, entry)?;
    }
    for rows in out.values_mut() {
        rows.sort_unstable_by_key(|(row, _)| *row);
    }
    Ok(out)
}

#[cfg(any(windows, target_os = "macos", test))]
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn decode_binary_old_values(
    encoding: &str,
    count: usize,
    val_bytes: &[u8],
    resolve_row: &dyn Fn(usize) -> Result<usize>,
    entry: &mut Vec<(usize, Value)>,
) -> Result<()> {
    match encoding {
        "f32" => {
            anyhow::ensure!(val_bytes.len() == count * 4, "Bulk f32 size mismatch");
            for i in 0..count {
                let f = f32::from_le_bytes(val_bytes[i * 4..i * 4 + 4].try_into().unwrap());
                entry.push((resolve_row(i)?, json_number_f64(f64::from(f))));
            }
        }
        "f64" => {
            anyhow::ensure!(val_bytes.len() == count * 8, "Bulk f64 size mismatch");
            for i in 0..count {
                let f = f64::from_le_bytes(val_bytes[i * 8..i * 8 + 8].try_into().unwrap());
                entry.push((resolve_row(i)?, json_number_f64(f)));
            }
        }
        "i32" => {
            anyhow::ensure!(val_bytes.len() == count * 4, "Bulk i32 size mismatch");
            for i in 0..count {
                let v = i32::from_le_bytes(val_bytes[i * 4..i * 4 + 4].try_into().unwrap());
                entry.push((resolve_row(i)?, json!(v)));
            }
        }
        "bool" => {
            anyhow::ensure!(val_bytes.len() == count, "Bulk bool size mismatch");
            for (i, &b) in val_bytes.iter().enumerate() {
                entry.push((resolve_row(i)?, Value::Bool(b != 0)));
            }
        }
        "vector3" => {
            anyhow::ensure!(val_bytes.len() == count * 12, "Bulk vector3 size mismatch");
            for i in 0..count {
                let off = i * 12;
                let x = f32::from_le_bytes(val_bytes[off..off + 4].try_into().unwrap());
                let y = f32::from_le_bytes(val_bytes[off + 4..off + 8].try_into().unwrap());
                let z = f32::from_le_bytes(val_bytes[off + 8..off + 12].try_into().unwrap());
                entry.push((
                    resolve_row(i)?,
                    json!({"_type": "Vector3", "x": f64::from(x), "y": f64::from(y), "z": f64::from(z)}),
                ));
            }
        }
        "color3" => {
            anyhow::ensure!(val_bytes.len() == count * 12, "Bulk color3 size mismatch");
            for i in 0..count {
                let off = i * 12;
                let r = f32::from_le_bytes(val_bytes[off..off + 4].try_into().unwrap());
                let g = f32::from_le_bytes(val_bytes[off + 4..off + 8].try_into().unwrap());
                let b = f32::from_le_bytes(val_bytes[off + 8..off + 12].try_into().unwrap());
                entry.push((
                    resolve_row(i)?,
                    json!({"_type": "Color3", "r": f64::from(r), "g": f64::from(g), "b": f64::from(b)}),
                ));
            }
        }
        _ => bail!("Unsupported bulk old value encoding {encoding}"),
    }
    Ok(())
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Clone, Copy)]
enum ScalarPatchValue {
    Bool(bool),
    Int(i64),
    Float32(f32),
    Float64(f64),
    Vector3([f32; 3]),
    Color3([f32; 3]),
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
                None
            }
            _ => None,
        }
    }

    fn kind(self) -> u8 {
        match self {
            Self::Bool(false) => 1,
            Self::Bool(true) => 2,
            Self::Int(_) => 3,
            Self::Float64(_) => 5,
            Self::Vector3(_) => 12,
            Self::Color3(_) => 15,
            Self::Float32(_) => 20,
        }
    }

    fn write_payload(self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Bool(_) => {}
            Self::Int(v) => write_var_u64(out, zigzag_i64(v))?,
            Self::Float32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::Float64(v) => out.extend_from_slice(&v.to_le_bytes()),
            Self::Vector3([x, y, z]) | Self::Color3([x, y, z]) => {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out.extend_from_slice(&z.to_le_bytes());
            }
        }
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos", test))]
fn is_scalar_group_kind(kind: u8) -> bool {
    matches!(kind, 1 | 2 | 3 | 5 | 12 | 15 | 20)
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
fn patch_scalar_block(
    store: &BlockStore<'_>,
    block_index: usize,
    block_updates: &BTreeMap<&str, Vec<(usize, ScalarPatchValue)>>,
) -> Result<Option<Section<'static>>> {
    let open = store.open_block(block_index)?;
    let block_count = store.blocks[block_index].count;
    let mut strings = open.strings.iter().collect::<Vec<_>>();
    let mut extra_strings: Vec<String> = Vec::new();
    let mut prop_ids = open.properties.clone();
    let block_prop_names = block_property_names(store, &open)?;
    let mut prop_name_to_id: AHashMap<String, usize> = block_prop_names
        .iter()
        .enumerate()
        .map(|(idx, name)| (name.clone(), idx))
        .collect();

    let raw_groups = read_raw_block_groups(&open.content[open.rest..])?;
    let mut updated_prop_ids: AHashSet<usize> = AHashSet::new();
    let mut target_prop_ids: Vec<(&str, usize)> = Vec::with_capacity(block_updates.len());
    for &prop_name in block_updates.keys() {
        let prop_id = if let Some(&id) = prop_name_to_id.get(prop_name) {
            id
        } else {
            let str_id = open.strings.iter().position(|s| match s {
                BlockString::Own(text) => text == prop_name,
                BlockString::Pool(_) => false,
            });
            let str_id = match str_id {
                Some(id) => id,
                None => {
                    let id = strings.len() + extra_strings.len();
                    extra_strings.push(prop_name.to_string());
                    id
                }
            };
            let id = prop_ids.len();
            prop_ids.push(str_id);
            prop_name_to_id.insert(prop_name.to_string(), id);
            id
        };
        updated_prop_ids.insert(prop_id);
        target_prop_ids.push((prop_name, prop_id));
    }

    let mut kept_groups: Vec<(usize, u8, usize, Cow<'_, [u8]>)> = Vec::new();
    let mut scalar_by_prop: AHashMap<usize, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    let mut other_by_prop: AHashMap<usize, Vec<PropertyGroupSlice<'_>>> = AHashMap::new();
    for (prop_id, kind, value_count, body) in raw_groups {
        if !updated_prop_ids.contains(&prop_id) {
            kept_groups.push((prop_id, kind, value_count, Cow::Borrowed(body)));
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
            let existing = decode_block_property(
                store,
                &open,
                &mut strings_cache,
                prop_name,
                existing_groups,
                block_count,
            )?;
            for (row, val) in existing {
                let Some(scalar) = ScalarPatchValue::from_json(&val) else {
                    return Ok(None);
                };
                merged.insert(row, scalar);
            }
        }
        for &(local_row, scalar) in updates {
            merged.insert(local_row, scalar);
        }
        let mut by_kind: BTreeMap<u8, Vec<(usize, ScalarPatchValue)>> = BTreeMap::new();
        for (row, scalar) in merged {
            by_kind
                .entry(scalar.kind())
                .or_default()
                .push((row, scalar));
        }
        for (kind, entries) in by_kind {
            let mut body = Vec::with_capacity(entries.len() * 6);
            let mut prev_row = 0_usize;
            for (row, scalar) in &entries {
                write_property_group_instance_index(&mut body, &mut prev_row, *row)?;
                scalar.write_payload(&mut body)?;
            }
            kept_groups.push((prop_id, kind, entries.len(), Cow::Owned(body)));
        }
    }

    let mut content = Vec::with_capacity(open.content.len() + 64);
    if extra_strings.is_empty() && prop_ids.len() == open.properties.len() {
        content.extend_from_slice(&open.content[..open.rest]);
    } else {
        let total_strings = strings.len() + extra_strings.len();
        write_var_u64(&mut content, total_strings as u64)?;
        for s in strings.drain(..) {
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
        for text in &extra_strings {
            write_var_u64(&mut content, (text.len() as u64) << 1)?;
            content.extend_from_slice(text.as_bytes());
        }
        write_var_u64(&mut content, prop_ids.len() as u64)?;
        for id in &prop_ids {
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
    Ok(Some(compressed(&content)?))
}

#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn patch_v12_bulk_old_values(
    base_bytes: &[u8],
    updates: &BTreeMap<String, Vec<(usize, Value)>>,
) -> Result<Vec<u8>> {
    if updates.is_empty() {
        return Ok(base_bytes.to_vec());
    }
    let store = BlockStore::open(base_bytes)?;
    let mut scalar_by_block: Vec<BTreeMap<&str, Vec<(usize, ScalarPatchValue)>>> =
        (0..store.blocks.len()).map(|_| BTreeMap::new()).collect();
    let mut fallback_updates: Vec<(usize, &str, &Value)> = Vec::new();
    for (prop_name, rows) in updates {
        for (global_row, value) in rows {
            anyhow::ensure!(*global_row < store.rows, "Bulk patch row out of range");
            if let Some(scalar) = ScalarPatchValue::from_json(value) {
                let block_idx = store.block_of(*global_row);
                let local_row = *global_row - store.blocks[block_idx].first;
                scalar_by_block[block_idx]
                    .entry(prop_name.as_str())
                    .or_default()
                    .push((local_row, scalar));
            } else {
                fallback_updates.push((*global_row, prop_name.as_str(), value));
            }
        }
    }

    let block_sections = scalar_by_block
        .par_iter()
        .enumerate()
        .map(
            |(block_idx, block_updates)| -> Result<Option<Section<'_>>> {
                if block_updates.is_empty() {
                    return Ok(Some(store.raw(store.blocks[block_idx].frame)?));
                }
                patch_scalar_block(&store, block_idx, block_updates)
            },
        )
        .collect::<Result<Vec<_>>>()?;

    let all_fast = fallback_updates.is_empty() && block_sections.iter().all(Option::is_some);
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
    for (prop_name, rows) in updates {
        for (global_row, value) in rows {
            document.instances[*global_row]
                .properties
                .insert(prop_name.clone(), value.clone());
        }
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
            instances: pkg_obs,
        })
        .unwrap();
        let pkg_des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: pkg_des,
        })
        .unwrap();
        assert!(matches!(
            diff_v12_bulk_plain_properties(&pkg_des_bytes, &pkg_obs_bytes, "Workspace", 64)
                .unwrap(),
            BulkDiffOutcome::Fallback
        ));

        let mut attr_des = desired_instances.clone();
        attr_des[2]
            .attributes
            .insert("Custom".into(), json!("value"));
        let attr_des_bytes = encode_settings_bytecode(&SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: attr_des,
        })
        .unwrap();
        assert!(matches!(
            diff_v12_bulk_plain_properties(&attr_des_bytes, &obs_bytes, "Workspace", 64).unwrap(),
            BulkDiffOutcome::Fallback
        ));

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
        let outcome = extract_v12_bulk_properties(
            &corrupted_desired,
            "Workspace",
            &["Transparency".to_string()],
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
        for (row, inst) in expected.iter_mut().enumerate().skip(1) {
            if inst.class_name == "Part"
                || inst.class_name == "UnionOperation"
                || inst.class_name == "Terrain"
            {
                let old = json_number_f64(0.75);
                inst.properties.insert("Transparency".into(), old.clone());
                prop_updates.push((row, old));
            }
        }
        updates.insert("Transparency".into(), prop_updates);

        let _ = patch_v12_bulk_old_values(&corrupted_desired, &updates).unwrap();
        let patched_bytes = patch_v12_bulk_old_values(&desired_bytes, &updates).unwrap();
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
}
