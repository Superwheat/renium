use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rbx_dom_weak::types::Ref as RbxRef;
use serde_json::{Map, Value, json};

use crate::app::output::print_json_output;
use crate::bytecode::query::{bytecode_parent_index, parse_property_assignments};
use crate::bytecode::{
    acquire_settings_file_lock, apply_file_mutations, collect_source_path_updates,
    ensure_service_store_exists, file_mutation_paths, lock_existing_service_store,
    preserve_source_path_extension, resolve_bytecode_cli_settings_file, resolve_bytecode_selector,
};
use crate::cli::{
    BytecodeAddInstanceArgs, BytecodeCloneInstanceArgs, BytecodeDesyncPackageLinkArgs,
    BytecodeRemoveInstanceArgs,
};
use crate::editor::document::is_protected_starter_player_container;
use crate::editor::paths::{
    build_editor_instance_path_parts, build_editor_instance_paths,
    build_editor_source_paths_by_index, script_file_names,
};
use crate::editor::sync::is_lua_source_class;
use crate::settings::bytecode::{
    SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode, SettingsBytecodeInstance,
    encode_settings_bytecode, settings_reference_index, strict_reference_path,
};
use crate::settings::instance::{self as instance_api, AddInstanceSpec};
use crate::settings::tree::settings_children_by_parent;
use crate::system::files::{ensure_existing_ancestor_inside, exact_path_key, path_key};

pub(crate) fn bytecode_add_instance(args: BytecodeAddInstanceArgs) -> Result<()> {
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        None,
    )?;
    ensure_service_store_exists(&settings_file, &service_hint)?;
    let _lock = acquire_settings_file_lock(&settings_file)?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let before_document = document.clone();
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let service_dir = settings_file.parent().unwrap_or_else(|| Path::new("."));
    let source_paths_before = build_editor_source_paths_by_index(&document, &service, service_dir);
    let parent_index = bytecode_parent_index(
        &document,
        args.parent.no_parent,
        args.parent.parent_index,
        args.parent.parent_settings_id.as_deref(),
        args.parent.parent_name.as_deref(),
        args.parent.parent_class_name.as_deref(),
    )?;
    let class_name = args.class_name.clone();
    let mut properties = parse_property_assignments(&args.properties)?;
    let source = if is_lua_source_class(&class_name) {
        match properties.get("Source") {
            Some(Value::String(source)) => source.clone(),
            Some(_) => bail!("Source must be a string"),
            None => String::new(),
        }
    } else {
        String::new()
    };
    if is_lua_source_class(&class_name) {
        properties.insert(
            "Source".to_string(),
            Value::String("__SOURCE_EXTERNAL__".to_string()),
        );
    }
    let added = instance_api::add_instance(
        &mut document,
        AddInstanceSpec {
            settings_id: args.settings_id,
            name: args.name,
            class_name: args.class_name,
            parent_index,
            properties,
            attributes: parse_property_assignments(&args.attributes)?,
        },
    )?;
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let source_paths = collect_source_path_updates(
        &before_document,
        &source_paths_before,
        &document,
        &service,
        service_dir,
        &mut writes,
        &mut removals,
    )?;
    writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    if is_lua_source_class(&document.instances[added.index].class_name)
        && let Some(Some(source_path)) = source_paths.get(added.index)
    {
        writes.insert(source_path.clone(), source.into_bytes());
    }
    let changed_paths = file_mutation_paths(&writes, &removals);
    let source_writes = changed_paths
        .iter()
        .filter(|path| *path != &settings_file)
        .cloned()
        .collect::<Vec<_>>();
    apply_file_mutations(&writes, &removals)?;
    let (path_segments_by_index, path_ordinals_by_index) =
        build_editor_instance_path_parts(&document, &service);
    print_json_output(
        &json!({
            "ok": true,
            "settingsFile": settings_file,
            "index": added.index,
            "settingsId": added.settings_id,
            "pathSegments": path_segments_by_index.get(added.index).and_then(std::clone::Clone::clone),
            "pathOrdinals": path_ordinals_by_index.get(added.index).and_then(std::clone::Clone::clone),
            "changedPaths": changed_paths,
            "sourceWrites": source_writes,
        }),
        args.pretty,
    )
}

pub(crate) fn bytecode_clone_instance(args: BytecodeCloneInstanceArgs) -> Result<()> {
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    let _lock = lock_existing_service_store(&settings_file)?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let before_document = document.clone();
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let source_index = resolve_bytecode_selector(
        &document,
        &service,
        &args.selector,
        "Source instance was not found",
    )?
    .index;
    if document.instances[source_index].parent_index.is_none() {
        bail!("Service roots cannot be copied");
    }

    let parent_selector_specified = args.parent_index.is_some()
        || args
            .parent_settings_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || args
            .parent_name
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        || args
            .parent_class_name
            .as_deref()
            .is_some_and(|value| !value.is_empty());
    let target_parent_index = if parent_selector_specified {
        bytecode_parent_index(
            &document,
            false,
            args.parent_index,
            args.parent_settings_id.as_deref(),
            args.parent_name.as_deref(),
            args.parent_class_name.as_deref(),
        )?
        .ok_or_else(|| anyhow::anyhow!("Parent instance was not found"))?
    } else {
        document.instances[source_index]
            .parent_index
            .ok_or_else(|| anyhow::anyhow!("Source instance has no parent"))?
    };

    let children_before = settings_children_by_parent(&document);
    let mut source_subtree = Vec::new();
    collect_settings_subtree_preorder(&children_before, source_index, &mut source_subtree);
    let source_set = source_subtree.iter().copied().collect::<HashSet<_>>();
    if source_set.contains(&target_parent_index) {
        bail!("Cannot copy an instance into itself or one of its descendants");
    }

    let service_dir = settings_file
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let source_paths_before = build_editor_source_paths_by_index(&document, &service, &service_dir);
    let (path_segments_before, path_ordinals_before) =
        build_editor_instance_path_parts(&document, &service);
    let root_name = unique_editor_child_name(
        &document,
        children_before
            .get(target_parent_index)
            .map_or(&[][..], Vec::as_slice),
        &document.instances[source_index].name,
    );

    let mut existing_settings_ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let mut next_settings_id_seed = document.instances.len();
    let mut old_to_new_index = HashMap::<usize, usize>::with_capacity(source_subtree.len());
    let mut cloned_settings_ids = Vec::with_capacity(source_subtree.len());
    let mut root_settings_id = String::new();

    for old_index in source_subtree.iter().copied() {
        let old_instance = document.instances[old_index].clone();
        let parent_index = if old_index == source_index {
            Some(target_parent_index)
        } else {
            Some(
                old_instance
                    .parent_index
                    .and_then(|parent_index| old_to_new_index.get(&parent_index).copied())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Clone source subtree is missing a parent mapping")
                    })?,
            )
        };
        let settings_id =
            next_editor_settings_id_fast(&mut existing_settings_ids, &mut next_settings_id_seed);
        let new_index = document.instances.len();
        let name = if old_index == source_index {
            root_name.clone()
        } else {
            old_instance.name
        };
        let mut properties = old_instance.properties;
        properties.remove("Source");
        properties.remove("LinkedSource");
        document.instances.push(SettingsBytecodeInstance {
            settings_id: settings_id.clone(),
            name,
            class_name: old_instance.class_name,
            parent_index,
            properties,
            attributes: old_instance.attributes,
        });
        old_to_new_index.insert(old_index, new_index);
        if old_index == source_index {
            root_settings_id.clone_from(&settings_id);
        }
        cloned_settings_ids.push(settings_id);
    }

    let ref_map = build_clone_ref_map(
        &document,
        CloneRefMapInput {
            source_subtree: &source_subtree,
            old_to_new_index: &old_to_new_index,
            path_segments_before: &path_segments_before,
            path_ordinals_before: &path_ordinals_before,
        },
    );
    for new_index in old_to_new_index.values().copied() {
        if let Some(instance) = document.instances.get_mut(new_index) {
            remap_internal_clone_refs_in_record(&mut instance.properties, &ref_map);
            remap_internal_clone_refs_in_record(&mut instance.attributes, &ref_map);
        }
    }

    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    let mut source_paths_after = collect_source_path_updates(
        &before_document,
        &source_paths_before,
        &document,
        &service,
        &service_dir,
        &mut writes,
        &mut removals,
    )?;
    let mut source_copies = Vec::new();
    for old_index in source_subtree.iter().copied() {
        let Some(new_index) = old_to_new_index.get(&old_index).copied() else {
            continue;
        };
        let Some(instance) = document.instances.get(old_index) else {
            continue;
        };
        if script_file_names(&instance.class_name).is_none() {
            continue;
        }
        let Some(Some(from)) = source_paths_before.get(old_index) else {
            continue;
        };
        let Some(Some(to)) = source_paths_after.get_mut(new_index) else {
            continue;
        };
        preserve_source_path_extension(from, to);
        if from == to || !from.exists() {
            continue;
        }
        writes.insert(
            to.clone(),
            fs::read(from).with_context(|| format!("Failed to read {}", from.display()))?,
        );
        source_copies.push(json!({
            "from": from,
            "to": to,
        }));
    }
    writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;

    print_json_output(
        &json!({
            "ok": true,
            "settingsFile": settings_file,
            "service": service,
            "rootSettingsId": root_settings_id,
            "settingsIds": cloned_settings_ids,
            "sourceCopies": source_copies,
            "changedPaths": changed_paths,
        }),
        args.pretty,
    )
}

pub(crate) struct BytecodeCloneRefMap {
    pub(crate) new_index_by_old: HashMap<usize, usize>,
    pub(crate) old_index_by_settings_id: HashMap<String, usize>,
    pub(crate) old_index_by_debug_id: HashMap<String, usize>,
    pub(crate) old_index_by_path_key: HashMap<String, Option<usize>>,
    pub(crate) old_index_by_path_parts_key: HashMap<String, usize>,
}

pub(crate) fn collect_settings_subtree_preorder(
    children_by_parent: &[Vec<usize>],
    index: usize,
    out: &mut Vec<usize>,
) {
    let mut stack = vec![index];
    let mut visited = HashSet::new();
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        out.push(current);
        if let Some(children) = children_by_parent.get(current) {
            stack.extend(children.iter().rev().copied());
        }
    }
}

pub(crate) fn bytecode_service_name(
    document: &SettingsBytecode,
    settings_file: &Path,
    service: &str,
) -> String {
    if !service.is_empty() {
        return service.to_string();
    }
    document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .or_else(|| {
            settings_file
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

pub(crate) fn unique_editor_child_name(
    document: &SettingsBytecode,
    child_indices: &[usize],
    requested: &str,
) -> String {
    let existing = child_indices
        .iter()
        .filter_map(|index| document.instances.get(*index))
        .map(|instance| instance.name.as_str())
        .collect::<HashSet<_>>();
    let base = if requested.trim().is_empty() {
        "Instance"
    } else {
        requested.trim()
    };
    if !existing.contains(base) {
        return base.to_string();
    }
    let mut suffix = 2usize;
    let mut candidate = format!("{base} Copy");
    while existing.contains(candidate.as_str()) {
        candidate = format!("{base} Copy {suffix}");
        suffix += 1;
    }
    candidate
}

pub(crate) fn next_editor_settings_id_fast(
    existing: &mut HashSet<String>,
    next_seed: &mut usize,
) -> String {
    loop {
        let candidate = format!("editor:{:x}", *next_seed);
        *next_seed += 1;
        if existing.insert(candidate.clone()) {
            return candidate;
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CloneRefMapInput<'a> {
    pub(crate) source_subtree: &'a [usize],
    pub(crate) old_to_new_index: &'a HashMap<usize, usize>,
    pub(crate) path_segments_before: &'a [Option<Vec<String>>],
    pub(crate) path_ordinals_before: &'a [Option<Vec<usize>>],
}

pub(crate) fn build_clone_ref_map(
    document: &SettingsBytecode,
    input: CloneRefMapInput<'_>,
) -> BytecodeCloneRefMap {
    let mut old_index_by_settings_id = HashMap::with_capacity(input.source_subtree.len());
    let mut old_index_by_debug_id = HashMap::new();
    let mut old_index_by_path_key = HashMap::with_capacity(input.source_subtree.len());
    let mut old_index_by_path_parts_key = HashMap::with_capacity(input.source_subtree.len());
    for old_index in input.source_subtree.iter().copied() {
        if let Some(instance) = document.instances.get(old_index) {
            old_index_by_settings_id.insert(instance.settings_id.clone(), old_index);
            if let Some(debug_id) = instance.settings_id.strip_prefix("debug:") {
                old_index_by_debug_id.insert(debug_id.to_string(), old_index);
            }
        }
        if let Some(Some(path_segments)) = input.path_segments_before.get(old_index) {
            let path_key = instance_path_key(path_segments);
            old_index_by_path_key
                .entry(path_key)
                .and_modify(|existing| *existing = None)
                .or_insert(Some(old_index));
            if let Some(Some(path_ordinals)) = input.path_ordinals_before.get(old_index) {
                old_index_by_path_parts_key.insert(
                    instance_path_parts_key(path_segments, path_ordinals),
                    old_index,
                );
            }
        }
    }
    BytecodeCloneRefMap {
        new_index_by_old: input.old_to_new_index.clone(),
        old_index_by_settings_id,
        old_index_by_debug_id,
        old_index_by_path_key,
        old_index_by_path_parts_key,
    }
}

pub(crate) fn remap_internal_clone_refs_in_record(
    record: &mut Map<String, Value>,
    refs: &BytecodeCloneRefMap,
) {
    for value in record.values_mut() {
        remap_internal_clone_refs(value, refs);
    }
}

fn remap_internal_clone_refs(value: &mut Value, refs: &BytecodeCloneRefMap) -> bool {
    match value {
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed = remap_internal_clone_refs(item, refs) || changed;
            }
            changed
        }
        Value::Object(object) => {
            if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                return remap_ref_object(object, refs);
            }
            let mut changed = false;
            if let Some(Value::Object(ref_object)) = object.get_mut("Ref") {
                changed = remap_ref_object(ref_object, refs) || changed;
            }
            for nested in object.values_mut() {
                changed = remap_internal_clone_refs(nested, refs) || changed;
            }
            changed
        }
        _ => false,
    }
}

fn remap_ref_object(object: &mut Map<String, Value>, refs: &BytecodeCloneRefMap) -> bool {
    let Some(old_index) = ref_old_index(object, refs) else {
        return false;
    };
    let Some(new_index) = refs.new_index_by_old.get(&old_index).copied() else {
        return false;
    };
    for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
        object.remove(selector);
    }
    object.insert(
        "instanceIndex".to_string(),
        Value::Number(serde_json::Number::from((new_index + 1) as u64)),
    );
    true
}

pub(crate) fn ref_old_index(
    object: &Map<String, Value>,
    refs: &BytecodeCloneRefMap,
) -> Option<usize> {
    object
        .get("settingsId")
        .or_else(|| object.get("instanceId"))
        .and_then(Value::as_str)
        .and_then(|settings_id| refs.old_index_by_settings_id.get(settings_id).copied())
        .or_else(|| {
            object
                .get("instanceIndex")
                .and_then(settings_reference_index)
                .and_then(|index| refs.new_index_by_old.contains_key(&index).then_some(index))
        })
        .or_else(|| {
            object
                .get("debugId")
                .and_then(Value::as_str)
                .and_then(|debug_id| refs.old_index_by_debug_id.get(debug_id).copied())
        })
        .or_else(|| {
            let segments = object
                .get("pathSegments")
                .and_then(path_segments_from_value)?;
            if let Some(ordinals) = object
                .get("pathOrdinals")
                .and_then(path_ordinals_from_value)
            {
                return refs
                    .old_index_by_path_parts_key
                    .get(&instance_path_parts_key(&segments, &ordinals))
                    .copied();
            }
            refs.old_index_by_path_key
                .get(&instance_path_key(&segments))
                .copied()
                .flatten()
        })
}

pub(crate) fn strict_ref_old_index(
    object: &Map<String, Value>,
    refs: &BytecodeCloneRefMap,
) -> Result<Option<usize>> {
    let mut resolved = None;
    let mut accept = |selector: &str, candidate: Option<usize>| -> Result<()> {
        let candidate =
            candidate.with_context(|| format!("Ref {selector} does not identify an instance"))?;
        if let Some(existing) = resolved
            && existing != candidate
        {
            bail!("Ref selectors disagree at {selector}");
        }
        resolved = Some(candidate);
        Ok(())
    };

    if let Some(value) = object.get("instanceIndex") {
        accept(
            "instanceIndex",
            settings_reference_index(value)
                .filter(|index| refs.new_index_by_old.contains_key(index)),
        )?;
    }
    for selector in ["settingsId", "instanceId", "referent", "ref"] {
        if let Some(value) = object.get(selector) {
            let id = value
                .as_str()
                .with_context(|| format!("Ref {selector} must be a string"))?;
            accept(selector, refs.old_index_by_settings_id.get(id).copied())?;
        }
    }
    if let Some(value) = object.get("debugId") {
        let debug_id = value.as_str().context("Ref debugId must be a string")?;
        accept("debugId", refs.old_index_by_debug_id.get(debug_id).copied())?;
    }
    if let Some((segments, ordinals)) = strict_reference_path(object)? {
        let candidate = if let Some(ordinals) = ordinals {
            refs.old_index_by_path_parts_key
                .get(&instance_path_parts_key(&segments, &ordinals))
                .copied()
        } else {
            refs.old_index_by_path_key
                .get(&instance_path_key(&segments))
                .copied()
                .flatten()
        };
        accept("pathSegments", candidate)?;
    }
    if object.contains_key("path") {
        bail!("Ref path is unsupported; use pathSegments and pathOrdinals");
    }
    Ok(resolved)
}

pub(crate) fn validate_settings_model_internal_references(
    document: &SettingsBytecode,
    owner: &str,
) -> Result<()> {
    fn visit(value: &Value, refs: &BytecodeCloneRefMap, owner: &str, location: &str) -> Result<()> {
        match value {
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    visit(item, refs, owner, &format!("{location}[{}]", index + 1))?;
                }
            }
            Value::Object(object) => {
                if object.get("_type").and_then(Value::as_str) == Some("Ref") {
                    strict_ref_old_index(object, refs).with_context(|| {
                        format!(
                            "Model owner {owner} has an external or invalid reference at {location}"
                        )
                    })?;
                    return Ok(());
                }
                if let Some(reference) = object.get("Ref").and_then(Value::as_object) {
                    strict_ref_old_index(reference, refs).with_context(|| {
                        format!(
                            "Model owner {owner} has an external or invalid reference at {location}.Ref"
                        )
                    })?;
                }
                for (key, nested) in object {
                    if key == "Ref" {
                        continue;
                    }
                    visit(nested, refs, owner, &format!("{location}.{key}"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let service = document
        .instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map_or("", |instance| instance.name.as_str());
    let (path_segments, path_ordinals) = build_editor_instance_path_parts(document, service);
    let indexes = (0..document.instances.len()).collect::<Vec<_>>();
    let identity = indexes
        .iter()
        .copied()
        .map(|index| (index, index))
        .collect::<HashMap<_, _>>();
    let refs = build_clone_ref_map(
        document,
        CloneRefMapInput {
            source_subtree: &indexes,
            old_to_new_index: &identity,
            path_segments_before: &path_segments,
            path_ordinals_before: &path_ordinals,
        },
    );
    for instance in &document.instances {
        for (name, value) in &instance.properties {
            visit(
                value,
                &refs,
                owner,
                &format!(
                    "{} ({}) property {name}",
                    instance.name, instance.class_name
                ),
            )?;
        }
        for (name, value) in &instance.attributes {
            visit(
                value,
                &refs,
                owner,
                &format!(
                    "{} ({}) attribute {name}",
                    instance.name, instance.class_name
                ),
            )?;
        }
    }
    Ok(())
}

pub(crate) fn path_segments_from_value(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect::<Vec<_>>()
    })
}

pub(crate) fn instance_path_key(path_segments: &[String]) -> String {
    path_segments.join("\0")
}

pub(crate) fn instance_path_parts_key(path_segments: &[String], path_ordinals: &[usize]) -> String {
    let mut key = instance_path_key(path_segments);
    key.push('\u{1}');
    for ordinal in path_ordinals {
        key.push_str(&ordinal.to_string());
        key.push('\0');
    }
    key
}

pub(crate) fn insert_unique_rbx_path(
    paths: &mut HashMap<String, Option<RbxRef>>,
    key: String,
    referent: RbxRef,
) {
    match paths.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Some(referent));
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if entry.get().is_some_and(|existing| existing != referent) {
                entry.insert(None);
            }
        }
    }
}

pub(crate) fn path_ordinals_from_value(value: &Value) -> Option<Vec<usize>> {
    value
        .as_array()?
        .iter()
        .map(|item| {
            item.as_u64()
                .filter(|ordinal| *ordinal > 0)
                .and_then(|ordinal| usize::try_from(ordinal).ok())
        })
        .collect()
}

pub(crate) fn prune_empty_source_dirs(service_dir: &Path, start: &Path) -> Result<()> {
    let mut current = start.to_path_buf();
    while current != service_dir && current.starts_with(service_dir) {
        let is_empty = match fs::read_dir(&current) {
            Ok(mut entries) => entries.next().is_none(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to inspect {}", current.display()));
            }
        };
        if !is_empty {
            break;
        }
        match fs::remove_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to remove empty {}", current.display()));
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    Ok(())
}

pub(crate) fn prune_removed_source_dirs(root: &Path, removals: &[PathBuf]) {
    let mut directories = removals
        .iter()
        .filter_map(|path| path.parent().filter(|path| path.starts_with(root)))
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories.dedup();
    for directory in directories {
        let _ = prune_empty_source_dirs(root, directory);
    }
}

pub(crate) fn plan_editor_source_file_removals(
    service_dir: &Path,
    source_paths_by_index: &[Option<PathBuf>],
    removed_indexes: &[usize],
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for index in removed_indexes {
        let Some(Some(source_path)) = source_paths_by_index.get(*index) else {
            continue;
        };
        ensure_existing_ancestor_inside(service_dir, source_path, "removed source file")?;
        if fs::symlink_metadata(source_path).is_ok() && !source_path.is_dir() {
            paths.push(source_path.clone());
        }
    }
    paths.sort_by_key(|path| exact_path_key(path));
    paths.dedup_by(|left, right| exact_path_key(left) == exact_path_key(right));
    Ok(paths)
}

pub(crate) fn bytecode_remove_instance(args: BytecodeRemoveInstanceArgs) -> Result<()> {
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        None,
    )?;
    let _lock = lock_existing_service_store(&settings_file)?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let before_document = document.clone();
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let source_paths_by_index = settings_file.parent().map_or_else(
        || vec![None; document.instances.len()],
        |service_dir| build_editor_source_paths_by_index(&document, &service, service_dir),
    );
    let index =
        resolve_bytecode_selector(&document, &service, &args.selector, "No matching instance")?
            .index;
    if is_protected_starter_player_container(&document, index) {
        bail!("{} cannot be removed", document.instances[index].name);
    }
    let removed =
        instance_api::remove_instances_at_indices(&mut document, &[index], !args.no_recursive)?;
    let removed_paths = removed
        .iter()
        .filter_map(|index| source_paths_by_index.get(*index).and_then(Option::as_ref))
        .filter(|path| path.is_file())
        .cloned()
        .collect::<Vec<_>>();
    let service_dir = settings_file.parent().unwrap_or_else(|| Path::new("."));
    let mut writes = BTreeMap::new();
    let mut removals = Vec::new();
    collect_source_path_updates(
        &before_document,
        &source_paths_by_index,
        &document,
        &service,
        service_dir,
        &mut writes,
        &mut removals,
    )?;
    removals.extend(removed_paths);
    removals.retain(|path| !writes.keys().any(|write| path_key(write) == path_key(path)));
    removals.sort_by_key(|path| path_key(path));
    removals.dedup_by(|left, right| path_key(left) == path_key(right));
    writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;
    let removed_source_paths = removals
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if let Some(service_dir) = settings_file.parent() {
        prune_removed_source_dirs(service_dir, &removals);
    }
    print_json_output(
        &json!({
            "ok": true,
            "settingsFile": settings_file,
            "removedIndexes": removed,
            "removedSourcePaths": removed_source_paths,
            "changedPaths": changed_paths,
        }),
        args.pretty,
    )
}

pub(crate) fn bytecode_desync_package_link(args: BytecodeDesyncPackageLinkArgs) -> Result<()> {
    let explicit_service = (!args.service.trim().is_empty()).then_some(args.service.as_str());
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        explicit_service,
    )?;
    let _lock = lock_existing_service_store(&settings_file)?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let target_index =
        resolve_bytecode_selector(&document, &service, &args.selector, "No matching instance")?
            .index;
    let path_segments_by_index = build_editor_instance_paths(&document, &service)
        .into_iter()
        .map(|path| path.map(|path| path.path_segments))
        .collect::<Vec<_>>();
    let children_by_parent = settings_children_by_parent(&document);
    let package_link_indices =
        package_link_indices_for_desync(&document, &children_by_parent, target_index)?;
    let removed_package_links = package_link_indices
        .iter()
        .map(|index| {
            let instance = &document.instances[*index];
            json!({
                "index": *index,
                "settingsId": instance.settings_id.clone(),
                "name": instance.name.clone(),
                "className": instance.class_name.clone(),
                "pathSegments": path_segments_by_index.get(*index).and_then(std::clone::Clone::clone),
            })
        })
        .collect::<Vec<_>>();
    let target_path = path_segments_by_index
        .get(target_index)
        .and_then(std::clone::Clone::clone);
    let removed =
        instance_api::remove_instances_at_indices(&mut document, &package_link_indices, true)?;
    document.write_file(&settings_file)?;
    print_json_output(
        &json!({
            "ok": true,
            "settingsFile": settings_file,
            "service": service,
            "targetIndex": target_index,
            "targetPathSegments": target_path,
            "removedIndexes": removed,
            "removedPackageLinks": removed_package_links,
        }),
        args.pretty,
    )
}

fn package_link_indices_for_desync(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    target_index: usize,
) -> Result<Vec<usize>> {
    let target = document
        .instances
        .get(target_index)
        .ok_or_else(|| anyhow::anyhow!("Invalid target index {target_index}"))?;
    if target.class_name == "PackageLink" {
        return Ok(vec![target_index]);
    }
    let links = children_by_parent
        .get(target_index)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .copied()
        .filter(|index| {
            document
                .instances
                .get(*index)
                .is_some_and(|instance| instance.class_name == "PackageLink")
        })
        .collect::<Vec<_>>();
    if links.is_empty() {
        bail!(
            "{} has no direct PackageLink child",
            path_segments_for_error(document, target_index)
        );
    }
    Ok(links)
}

pub(crate) fn has_direct_package_link_child(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    target_index: usize,
) -> bool {
    children_by_parent
        .get(target_index)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .any(|index| {
            document
                .instances
                .get(*index)
                .is_some_and(|instance| instance.class_name == "PackageLink")
        })
}

fn path_segments_for_error(document: &SettingsBytecode, index: usize) -> String {
    let Some(instance) = document.instances.get(index) else {
        return format!("#{index}");
    };
    let mut names = vec![instance.name.clone()];
    let mut current = instance.parent_index;
    while let Some(parent_index) = current {
        let Some(parent) = document.instances.get(parent_index) else {
            break;
        };
        names.push(parent.name.clone());
        current = parent.parent_index;
    }
    names.reverse();
    names.join(".")
}
