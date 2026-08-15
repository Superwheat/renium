use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rbx_dom_weak::types::{Ref as RbxRef, Variant as RbxVariant};
use rbx_dom_weak::{InstanceBuilder as RbxInstanceBuilder, WeakDom as RbxWeakDom};
use rbx_reflection::PropertyDescriptor as RbxPropertyDescriptor;
use serde_json::{Map, Number, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::app::output::print_json_output;
use crate::app::timing::log_timing;
use crate::bytecode::edit::{
    bytecode_service_name, collect_settings_subtree_preorder, insert_unique_rbx_path,
    instance_path_key, instance_path_parts_key, next_editor_settings_id_fast,
    path_ordinals_from_value, path_segments_from_value, unique_editor_child_name,
};
use crate::bytecode::explorer::explorer_daemon_services;
use crate::bytecode::query::bytecode_parent_index;
use crate::bytecode::{
    acquire_settings_file_lock, apply_file_mutations, bytecode_input_looks_like_settings_file,
    collect_source_path_updates, ensure_service_store_exists, file_mutation_paths,
    resolve_bytecode_cli_settings_file, resolve_bytecode_selector,
};
use crate::cli::{
    BytecodeExportModelArgs, BytecodeExportPlaceArgs, BytecodeImportModelArgs, BytecodeRepackArgs,
};
use crate::editor::document::read_editor_service_documents;
use crate::editor::paths::{
    build_editor_instance_paths, build_editor_instance_paths_with_children,
    build_editor_source_paths_by_index, build_editor_source_paths_by_index_with_children,
    editor_run_context_value, infer_source_script, merge_editor_source_files_into_document,
};
use crate::editor::types::{EditorInstancePath, EditorSettingsWrite};
use crate::project::config;
use crate::project::layout::apply_configured_project_layout;
use crate::rbx::decode::rbx_instance_to_settings_records;
use crate::rbx::encode::{
    BytecodeRbxBuildOptions, BytecodeRbxEncoder, collect_rbx_subtree_preorder,
    rbx_model_top_level_refs, settings_root_indices,
};
use crate::roblox::services::DEFAULT_SYNC_SERVICES;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, encode_settings_bytecode,
    settings_reference_index,
};
use crate::settings::tree::{editor_service_root_index, settings_children_by_parent};
use crate::system::files::{
    absolutize_under, create_output_writer, exact_path_key, is_service_settings_file_name,
    resolve_existing_project_root, service_settings_path, validate_filesystem_instance_name,
};

enum RbxModelFormat {
    Binary,
    Xml,
}

impl RbxModelFormat {
    fn parse(raw: &str) -> Result<Self> {
        match raw
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "rbxm" | "binary" | "bin" => Ok(Self::Binary),
            "rbxmx" | "xml" => Ok(Self::Xml),
            other => bail!("Invalid model format {other:?}. Use rbxm or rbxmx."),
        }
    }

    fn from_path(path: &Path) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .with_context(|| format!("Cannot infer model format from {}", path.display()))?;
        Self::parse(extension)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Binary => "rbxm",
            Self::Xml => "rbxmx",
        }
    }
}

pub(crate) fn rbx_dom_instance_by_path_unique(
    dom: &RbxWeakDom,
    path_segments: &[String],
    path_ordinals: &[usize],
) -> Result<RbxRef> {
    if path_segments.is_empty() {
        bail!("Path selector cannot be empty");
    }
    if path_ordinals.contains(&0) {
        bail!("Path ordinals are 1-based");
    }
    let mut current = vec![dom.root_ref()];
    for (depth, segment) in path_segments.iter().enumerate() {
        let ordinal = path_ordinals.get(depth).copied().unwrap_or(1);
        let mut next = Vec::new();
        for parent_ref in &current {
            let Some(parent) = dom.get_by_ref(*parent_ref) else {
                continue;
            };
            let child = parent
                .children()
                .iter()
                .filter_map(|child_ref| {
                    dom.get_by_ref(*child_ref)
                        .filter(|child| child.name == *segment)
                        .map(|_| *child_ref)
                })
                .nth(ordinal.saturating_sub(1));
            if let Some(child) = child {
                next.push(child);
            }
        }
        if next.is_empty() {
            bail!(
                "No matching place path: {}",
                path_segments[..=depth].join(".")
            );
        }
        current = next;
    }
    current.sort_by_key(ToString::to_string);
    current.dedup();
    let [referent] = current.as_slice() else {
        let candidates = current
            .iter()
            .take(8)
            .map(|referent| rbx_dom_instance_path_segments(dom, *referent).join("."))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "Ambiguous place path: {} matched {} instances [{}]. Use --ords.",
            path_segments.join("."),
            current.len(),
            candidates
        );
    };
    Ok(*referent)
}

pub(crate) fn rbx_dom_instance_path_segments(dom: &RbxWeakDom, referent: RbxRef) -> Vec<String> {
    rbx_dom_instance_path_parts(dom, referent).0
}

pub(crate) fn rbx_dom_instance_path_parts(
    dom: &RbxWeakDom,
    referent: RbxRef,
) -> (Vec<String>, Vec<usize>) {
    let mut segments = Vec::new();
    let mut ordinals = Vec::new();
    let mut current = referent;
    while let Some(instance) = dom.get_by_ref(current) {
        let parent = instance.parent();
        if current != dom.root_ref() || instance.class.as_str() != "DataModel" {
            segments.push(instance.name.clone());
            let ordinal = dom.get_by_ref(parent).map_or(1, |parent| {
                parent
                    .children()
                    .iter()
                    .take_while(|sibling_ref| **sibling_ref != current)
                    .filter(|sibling_ref| {
                        dom.get_by_ref(**sibling_ref)
                            .is_some_and(|sibling| sibling.name == instance.name)
                    })
                    .count()
                    + 1
            });
            ordinals.push(ordinal);
        }
        if parent.is_none() {
            break;
        }
        current = parent;
    }
    segments.reverse();
    ordinals.reverse();
    (segments, ordinals)
}

#[derive(Clone, Copy)]
pub(crate) enum RbxPlaceFormat {
    Binary,
    Xml,
}

impl RbxPlaceFormat {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        match raw
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "rbxl" | "binary" | "bin" => Ok(Self::Binary),
            "rbxlx" | "xml" => Ok(Self::Xml),
            other => bail!("Invalid place format {other:?}. Use rbxl or rbxlx."),
        }
    }

    pub(crate) fn from_path(path: &Path) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .with_context(|| format!("Cannot infer place format from {}", path.display()))?;
        Self::parse(extension)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Binary => "rbxl",
            Self::Xml => "rbxlx",
        }
    }

    pub(crate) fn read(self, path: &Path) -> Result<RbxWeakDom> {
        let input =
            File::open(path).with_context(|| format!("Failed to read {}", path.display()))?;
        let reader = BufReader::new(input);
        match self {
            Self::Binary => rbx_binary::from_reader(reader)
                .with_context(|| format!("Failed to read {}", path.display())),
            Self::Xml => rbx_xml::from_reader_default(reader)
                .with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    pub(crate) fn write(self, path: &Path, dom: &RbxWeakDom, roots: &[RbxRef]) -> Result<()> {
        let writer = create_output_writer(path)?;
        match self {
            Self::Binary => rbx_binary::to_writer(writer, dom, roots)
                .with_context(|| format!("Failed to write {}", path.display())),
            Self::Xml => rbx_xml::to_writer_default(writer, dom, roots)
                .with_context(|| format!("Failed to write {}", path.display())),
        }
    }
}

#[derive(Default)]
pub(crate) struct BytecodeModelExportRefs {
    pub(crate) by_index: HashMap<usize, RbxRef>,
    pub(crate) by_settings_id: HashMap<String, RbxRef>,
    pub(crate) global_by_settings_id: Option<Arc<HashMap<String, RbxRef>>>,
    pub(crate) by_path_key: HashMap<String, RbxRef>,
    pub(crate) global_by_path_key: Option<Arc<HashMap<String, RbxRef>>>,
    pub(crate) by_path_segments_key: HashMap<String, Option<RbxRef>>,
    pub(crate) global_by_path_segments_key: Option<Arc<HashMap<String, Option<RbxRef>>>>,
}

pub(crate) struct BytecodeExportPropertyMetadata<'db> {
    pub(crate) property: Option<&'db RbxPropertyDescriptor<'db>>,
    pub(crate) descriptor: Option<&'db RbxPropertyDescriptor<'db>>,
    pub(crate) serialized_name: Option<&'static str>,
    pub(crate) native_setter_property: bool,
    pub(crate) skipped: bool,
}

pub(crate) struct BytecodeExportClassMetadata<'db> {
    pub(crate) triangle_mesh_part: bool,
    pub(crate) model: bool,
    pub(crate) decal: bool,
    pub(crate) properties: HashMap<String, BytecodeExportPropertyMetadata<'db>>,
}

pub(crate) type BytecodeExportMetadata<'db> = HashMap<String, BytecodeExportClassMetadata<'db>>;

#[derive(Default)]
pub(crate) struct BytecodeModelImportRefs {
    pub(crate) new_index_by_ref: HashMap<RbxRef, usize>,
    pub(crate) new_index_by_dense_ref: Option<Vec<usize>>,
    pub(crate) settings_id_by_ref: HashMap<RbxRef, String>,
    pub(crate) path_segments_by_index: Vec<Option<Vec<String>>>,
    pub(crate) path_segments_by_ref: Arc<HashMap<RbxRef, Vec<String>>>,
    pub(crate) path_ordinals_by_ref: Arc<HashMap<RbxRef, Vec<usize>>>,
}

pub(crate) fn rbx_dom_path_import_refs(
    dom: &RbxWeakDom,
    include_indices: bool,
) -> BytecodeModelImportRefs {
    let mut refs_preorder = Vec::new();
    for referent in rbx_model_top_level_refs(dom) {
        collect_rbx_subtree_preorder(dom, referent, &mut refs_preorder);
    }
    let mut path_segments_by_ref = HashMap::with_capacity(refs_preorder.len());
    let mut path_ordinals_by_ref = HashMap::with_capacity(refs_preorder.len());
    for referent in refs_preorder.iter().copied() {
        let (segments, ordinals) = rbx_dom_instance_path_parts(dom, referent);
        path_segments_by_ref.insert(referent, segments);
        path_ordinals_by_ref.insert(referent, ordinals);
    }
    let new_index_by_ref = if include_indices {
        refs_preorder
            .iter()
            .copied()
            .enumerate()
            .map(|(index, referent)| (referent, index))
            .collect()
    } else {
        HashMap::new()
    };
    let path_segments_by_index = if include_indices {
        refs_preorder
            .iter()
            .map(|referent| path_segments_by_ref.get(referent).cloned())
            .collect()
    } else {
        Vec::new()
    };
    BytecodeModelImportRefs {
        new_index_by_ref,
        path_segments_by_index,
        path_segments_by_ref: Arc::new(path_segments_by_ref),
        path_ordinals_by_ref: Arc::new(path_ordinals_by_ref),
        ..Default::default()
    }
}

struct CanonicalSettingsRefTarget {
    service: String,
    index: usize,
    settings_id: String,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
}

fn insert_unique_settings_ref_target(
    map: &mut HashMap<String, Option<usize>>,
    key: String,
    target: usize,
) {
    match map.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Some(target));
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if entry.get().is_some_and(|existing| existing != target) {
                entry.insert(None);
            }
        }
    }
}

pub(crate) fn canonicalize_settings_reference_documents(
    documents: &mut BTreeMap<String, SettingsBytecode>,
) -> BTreeSet<String> {
    let mut targets = Vec::new();
    let mut local_ids = HashMap::<String, HashMap<String, usize>>::new();
    let mut global_ids = HashMap::<String, Option<usize>>::new();
    let mut exact_paths = HashMap::<String, Option<usize>>::new();
    let mut segment_paths = HashMap::<String, Option<usize>>::new();
    for (service, document) in documents.iter() {
        let paths = build_editor_instance_paths(document, service);
        for (index, instance) in document.instances.iter().enumerate() {
            let Some(Some(path)) = paths.get(index) else {
                continue;
            };
            let target = targets.len();
            targets.push(CanonicalSettingsRefTarget {
                service: service.clone(),
                index,
                settings_id: instance.settings_id.clone(),
                path_segments: path.path_segments.clone(),
                path_ordinals: path.path_ordinals.clone(),
            });
            local_ids
                .entry(service.clone())
                .or_default()
                .entry(instance.settings_id.clone())
                .or_insert(target);
            insert_unique_settings_ref_target(
                &mut global_ids,
                instance.settings_id.clone(),
                target,
            );
            insert_unique_settings_ref_target(
                &mut exact_paths,
                instance_path_parts_key(&path.path_segments, &path.path_ordinals),
                target,
            );
            insert_unique_settings_ref_target(
                &mut segment_paths,
                instance_path_key(&path.path_segments),
                target,
            );
        }
    }

    fn resolve(
        object: &Map<String, Value>,
        owner_service: &str,
        targets: &[CanonicalSettingsRefTarget],
        local_ids: &HashMap<String, HashMap<String, usize>>,
        global_ids: &HashMap<String, Option<usize>>,
        exact_paths: &HashMap<String, Option<usize>>,
        segment_paths: &HashMap<String, Option<usize>>,
    ) -> Option<usize> {
        let persistent_id = object
            .get("settingsId")
            .or_else(|| object.get("instanceId"))
            .and_then(Value::as_str);
        if let Some(target) = persistent_id
            .and_then(|id| global_ids.get(id))
            .copied()
            .flatten()
        {
            return Some(target);
        }
        if let Some(target) = object
            .get("debugId")
            .and_then(Value::as_str)
            .map(|debug_id| format!("debug:{debug_id}"))
            .and_then(|id| global_ids.get(&id).copied().flatten())
        {
            return Some(target);
        }
        if let Some(segments) = object
            .get("pathSegments")
            .and_then(path_segments_from_value)
        {
            if let Some(ordinals) = object
                .get("pathOrdinals")
                .and_then(path_ordinals_from_value)
                && segments.len() == ordinals.len()
                && let Some(target) = exact_paths
                    .get(&instance_path_parts_key(&segments, &ordinals))
                    .copied()
                    .flatten()
            {
                return Some(target);
            }
            if let Some(target) = segment_paths
                .get(&instance_path_key(&segments))
                .copied()
                .flatten()
            {
                return Some(target);
            }
        }
        if let Some(index) = object
            .get("instanceIndex")
            .and_then(settings_reference_index)
            && let Some(target) = targets
                .iter()
                .position(|target| target.service == owner_service && target.index == index)
        {
            return Some(target);
        }
        persistent_id.and_then(|id| {
            local_ids
                .get(owner_service)
                .and_then(|ids| ids.get(id))
                .copied()
        })
    }

    fn apply_target(
        object: &mut Map<String, Value>,
        owner_service: &str,
        target: &CanonicalSettingsRefTarget,
    ) -> bool {
        let mut changed = false;
        let settings_id = Value::String(target.settings_id.clone());
        if object.get("settingsId") != Some(&settings_id) {
            object.insert("settingsId".to_string(), settings_id.clone());
            changed = true;
        }
        if object.contains_key("instanceId") && object.get("instanceId") != Some(&settings_id) {
            object.insert("instanceId".to_string(), settings_id);
            changed = true;
        }
        let path_segments = Value::Array(
            target
                .path_segments
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        );
        if object.get("pathSegments") != Some(&path_segments) {
            object.insert("pathSegments".to_string(), path_segments);
            changed = true;
        }
        let path_ordinals = Value::Array(
            target
                .path_ordinals
                .iter()
                .map(|ordinal| Value::Number(Number::from(*ordinal as u64)))
                .collect(),
        );
        if object.get("pathOrdinals") != Some(&path_ordinals) {
            object.insert("pathOrdinals".to_string(), path_ordinals);
            changed = true;
        }
        if target.service != owner_service && object.remove("instanceIndex").is_some() {
            changed = true;
        }
        changed
    }

    fn visit(
        value: &mut Value,
        owner_service: &str,
        targets: &[CanonicalSettingsRefTarget],
        local_ids: &HashMap<String, HashMap<String, usize>>,
        global_ids: &HashMap<String, Option<usize>>,
        exact_paths: &HashMap<String, Option<usize>>,
        segment_paths: &HashMap<String, Option<usize>>,
    ) -> bool {
        match value {
            Value::Array(values) => {
                let mut changed = false;
                for value in values {
                    changed = visit(
                        value,
                        owner_service,
                        targets,
                        local_ids,
                        global_ids,
                        exact_paths,
                        segment_paths,
                    ) || changed;
                }
                changed
            }
            Value::Object(object) => {
                let direct_target = (object.get("_type").and_then(Value::as_str) == Some("Ref"))
                    .then(|| {
                        resolve(
                            object,
                            owner_service,
                            targets,
                            local_ids,
                            global_ids,
                            exact_paths,
                            segment_paths,
                        )
                    })
                    .flatten();
                let mut changed = direct_target
                    .and_then(|index| targets.get(index))
                    .is_some_and(|target| apply_target(object, owner_service, target));
                let wrapped_target =
                    object
                        .get("Ref")
                        .and_then(Value::as_object)
                        .and_then(|reference| {
                            resolve(
                                reference,
                                owner_service,
                                targets,
                                local_ids,
                                global_ids,
                                exact_paths,
                                segment_paths,
                            )
                        });
                if let Some(target) = wrapped_target.and_then(|index| targets.get(index))
                    && let Some(reference) = object.get_mut("Ref").and_then(Value::as_object_mut)
                {
                    changed = apply_target(reference, owner_service, target) || changed;
                }
                for nested in object.values_mut() {
                    changed = visit(
                        nested,
                        owner_service,
                        targets,
                        local_ids,
                        global_ids,
                        exact_paths,
                        segment_paths,
                    ) || changed;
                }
                changed
            }
            _ => false,
        }
    }

    let mut changed_services = BTreeSet::new();
    for (service, document) in documents.iter_mut() {
        let mut changed = false;
        for instance in &mut document.instances {
            for value in instance.properties.values_mut() {
                changed = visit(
                    value,
                    service,
                    &targets,
                    &local_ids,
                    &global_ids,
                    &exact_paths,
                    &segment_paths,
                ) || changed;
            }
            for value in instance.attributes.values_mut() {
                changed = visit(
                    value,
                    service,
                    &targets,
                    &local_ids,
                    &global_ids,
                    &exact_paths,
                    &segment_paths,
                ) || changed;
            }
        }
        if changed {
            changed_services.insert(service.clone());
        }
    }
    changed_services
}

pub(crate) fn canonicalize_settings_reference_stores(src_root: &Path) -> Result<usize> {
    let mut files = BTreeMap::new();
    let mut documents = BTreeMap::new();
    for entry in read_editor_service_documents(src_root)? {
        files.insert(entry.service.clone(), entry.settings_file);
        documents.insert(entry.service, entry.document);
    }
    let changed = canonicalize_settings_reference_documents(&mut documents);
    if changed.is_empty() {
        return Ok(0);
    }
    let mut writes = BTreeMap::new();
    for service in &changed {
        let path = &files[service];
        writes.insert(path.clone(), encode_settings_bytecode(&documents[service])?);
    }
    apply_file_mutations(&writes, &[])?;
    Ok(changed.len())
}

pub(crate) fn imported_instance_index(
    refs: &BytecodeModelImportRefs,
    referent: RbxRef,
) -> Option<usize> {
    if let Some(dense) = refs.new_index_by_dense_ref.as_ref() {
        return referent
            .as_u128()
            .and_then(|value| usize::try_from(value).ok())
            .and_then(|value| value.checked_sub(1))
            .and_then(|index| dense.get(index).copied())
            .filter(|index| *index != usize::MAX);
    }
    refs.new_index_by_ref.get(&referent).copied()
}

pub(crate) struct BytecodeModelImportOutcome {
    format: RbxModelFormat,
    pub(crate) root_settings_ids: Vec<String>,
    pub(crate) settings_ids: Vec<String>,
    pub(crate) source_writes: Vec<Value>,
    source_files: BTreeMap<PathBuf, Vec<u8>>,
    pub(crate) source_by_settings_id: BTreeMap<String, Vec<u8>>,
}

pub(crate) struct RbxPlaceBuild {
    pub(crate) dom: RbxWeakDom,
    pub(crate) service_roots: Vec<(String, RbxRef)>,
    pub(crate) documents_by_service: HashMap<String, SettingsBytecode>,
    pub(crate) paths_by_service: HashMap<String, Vec<Option<EditorInstancePath>>>,
    pub(crate) settings_writes: Vec<EditorSettingsWrite>,
    pub(crate) total_instances: usize,
    pub(crate) omitted_properties_by_class: HashMap<String, HashSet<String>>,
    pub(crate) logical_properties_by_ref: HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
}

pub(crate) fn source_only_settings_document(
    service_dir: &Path,
    service: &str,
) -> Result<SettingsBytecode> {
    source_settings_document(service_dir, service, None, &[], true)
}

pub(crate) fn source_structure_settings_document(
    service_dir: &Path,
    service: &str,
    naming: &config::ProjectScriptNaming,
    excluded_sources: &[PathBuf],
) -> Result<SettingsBytecode> {
    source_settings_document(service_dir, service, Some(naming), excluded_sources, false)
}

fn source_settings_document(
    service_dir: &Path,
    service: &str,
    naming: Option<&config::ProjectScriptNaming>,
    excluded_sources: &[PathBuf],
    include_source: bool,
) -> Result<SettingsBytecode> {
    let root_class = if DEFAULT_SYNC_SERVICES.contains(&service) {
        service
    } else {
        "Folder"
    };
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            source_projection_settings_id(service, service, "service"),
            service.to_string(),
            root_class.to_string(),
            None,
        )],
    };
    let excluded_source_keys = excluded_sources
        .iter()
        .map(|path| {
            let key = exact_path_key(path);
            if cfg!(windows) || cfg!(target_os = "macos") {
                key.to_ascii_lowercase()
            } else {
                key
            }
        })
        .collect::<Vec<_>>();
    let options = SourceSettingsOptions {
        naming,
        excluded_source_keys: &excluded_source_keys,
        include_source,
    };
    append_source_only_children(&mut document, service_dir, service, 0, service, &options)?;
    Ok(document)
}

struct SourceSettingsOptions<'a> {
    naming: Option<&'a config::ProjectScriptNaming>,
    excluded_source_keys: &'a [String],
    include_source: bool,
}

fn append_source_only_children(
    document: &mut SettingsBytecode,
    directory: &Path,
    service: &str,
    parent_index: usize,
    path_key: &str,
    options: &SourceSettingsOptions<'_>,
) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    let resolved_naming = options
        .naming
        .cloned()
        .unwrap_or_else(|| config::cached_script_naming(directory));
    let mut entries = fs::read_dir(directory)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_cached_key(|entry| entry.file_name().to_string_lossy().to_ascii_lowercase());
    for entry in entries {
        let path = entry.path();
        let physical_key = exact_path_key(&path);
        let physical_key = if cfg!(windows) || cfg!(target_os = "macos") {
            physical_key.to_ascii_lowercase()
        } else {
            physical_key
        };
        if options.excluded_source_keys.iter().any(|excluded_key| {
            physical_key == *excluded_key
                || physical_key
                    .strip_prefix(excluded_key)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if is_service_settings_file_name(&file_name) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "Source-only projects cannot contain symlinks: {}",
                path.display()
            );
        }
        if file_type.is_dir() {
            let mut init_sources = Vec::new();
            for child in fs::read_dir(&path)? {
                let child = child?;
                let child_type = child.file_type()?;
                if child_type.is_symlink() {
                    bail!(
                        "Source-only projects cannot contain symlinks: {}",
                        child.path().display()
                    );
                }
                if !child_type.is_file() {
                    continue;
                }
                let name = child.file_name().to_string_lossy().into_owned();
                if let Some((class_name, None, run_context)) =
                    infer_source_script(&name, &resolved_naming)
                {
                    init_sources.push((child.path(), name, class_name, run_context));
                }
            }
            init_sources.sort_by(|left, right| left.1.cmp(&right.1));
            if init_sources.len() > 1 {
                bail!("{} contains more than one init source file", path.display());
            }
            let child_path_key = format!("{path_key}/{file_name}");
            let (class_name, properties) =
                if let Some((source_path, _source_name, class_name, run_context)) =
                    init_sources.first()
                {
                    let mut properties = Map::new();
                    if options.include_source {
                        properties.insert(
                            "Source".to_string(),
                            Value::String(fs::read_to_string(source_path).with_context(|| {
                                format!("{} is not valid UTF-8", source_path.display())
                            })?),
                        );
                    }
                    if let Some(run_context) = run_context {
                        properties.insert(
                            "RunContext".to_string(),
                            editor_run_context_value(run_context),
                        );
                    }
                    ((*class_name).to_string(), properties)
                } else {
                    ("Folder".to_string(), Map::new())
                };
            let index = document.instances.len();
            document.instances.push(SettingsBytecodeInstance {
                settings_id: source_projection_settings_id(service, &child_path_key, &class_name),
                name: file_name.clone(),
                class_name,
                parent_index: Some(parent_index),
                properties,
                attributes: Map::new(),
            });
            append_source_only_children(document, &path, service, index, &child_path_key, options)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Some((class_name, leaf_name, run_context)) =
            infer_source_script(&file_name, &resolved_naming)
        else {
            continue;
        };
        let Some(name) = leaf_name else {
            continue;
        };
        let child_path_key = format!("{path_key}/{file_name}");
        let mut properties = Map::new();
        if options.include_source {
            properties.insert(
                "Source".to_string(),
                Value::String(
                    fs::read_to_string(&path)
                        .with_context(|| format!("{} is not valid UTF-8", path.display()))?,
                ),
            );
        }
        if let Some(run_context) = run_context {
            properties.insert(
                "RunContext".to_string(),
                editor_run_context_value(run_context),
            );
        }
        document.instances.push(SettingsBytecodeInstance {
            settings_id: source_projection_settings_id(service, &child_path_key, class_name),
            name,
            class_name: class_name.to_string(),
            parent_index: Some(parent_index),
            properties,
            attributes: Map::new(),
        });
    }
    Ok(())
}

fn source_projection_settings_id(service: &str, path: &str, class_name: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(service.as_bytes());
    digest.update([0]);
    digest.update(path.as_bytes());
    digest.update([0]);
    digest.update(class_name.as_bytes());
    format!("source:{:x}", digest.finalize())
}

pub(crate) fn bytecode_export_model(args: BytecodeExportModelArgs) -> Result<()> {
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    let document = SettingsBytecode::read_file(&settings_file)?;
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let root_index = resolve_bytecode_selector(
        &document,
        &service,
        &args.selector,
        "Export instance was not found",
    )?
    .index;
    let format = match args.format.as_deref() {
        Some(raw) => RbxModelFormat::parse(raw)?,
        None => RbxModelFormat::from_path(&args.output)?,
    };
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let service_dir = settings_file
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let source_paths = build_editor_source_paths_by_index(&document, &service, &service_dir);
    let instance_paths_by_index = build_editor_instance_paths(&document, &service);

    let children_by_parent = settings_children_by_parent(&document);
    let mut subtree = Vec::new();
    collect_settings_subtree_preorder(&children_by_parent, root_index, &mut subtree);

    let mut export_refs = BytecodeModelExportRefs {
        by_index: HashMap::with_capacity(subtree.len()),
        by_settings_id: HashMap::with_capacity(subtree.len()),
        by_path_key: HashMap::with_capacity(subtree.len()),
        by_path_segments_key: HashMap::with_capacity(subtree.len()),
        ..Default::default()
    };
    for index in subtree.iter().copied() {
        let referent = RbxRef::new();
        export_refs.by_index.insert(index, referent);
        if let Some(instance) = document.instances.get(index) {
            export_refs
                .by_settings_id
                .insert(instance.settings_id.clone(), referent);
        }
        if let Some(Some(path)) = instance_paths_by_index.get(index) {
            insert_unique_rbx_path(
                &mut export_refs.by_path_segments_key,
                instance_path_key(&path.path_segments),
                referent,
            );
            export_refs.by_path_key.insert(
                instance_path_parts_key(&path.path_segments, &path.path_ordinals),
                referent,
            );
        }
    }

    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let mut metadata = BytecodeExportMetadata::new();
    let mut encoder = BytecodeRbxEncoder::new(&document, database, &mut metadata, &export_refs);
    for index in subtree.iter().copied() {
        let parent_ref = if index == root_index {
            dom.root_ref()
        } else {
            let parent_index = document.instances[index]
                .parent_index
                .ok_or_else(|| anyhow::anyhow!("Export subtree contains a detached child"))?;
            *export_refs
                .by_index
                .get(&parent_index)
                .ok_or_else(|| anyhow::anyhow!("Export subtree is missing parent referent"))?
        };
        let builder = encoder.build(
            index,
            BytecodeRbxBuildOptions {
                source_path: source_paths.get(index).and_then(Option::as_deref),
                ..Default::default()
            },
        )?;
        dom.insert(parent_ref, builder);
    }

    let writer = create_output_writer(&args.output)?;
    let root_ref = *export_refs
        .by_index
        .get(&root_index)
        .ok_or_else(|| anyhow::anyhow!("Export root referent missing"))?;
    match format {
        RbxModelFormat::Binary => rbx_binary::to_writer(writer, &dom, &[root_ref])
            .with_context(|| format!("Failed to write {}", args.output.display()))?,
        RbxModelFormat::Xml => rbx_xml::to_writer_default(writer, &dom, &[root_ref])
            .with_context(|| format!("Failed to write {}", args.output.display()))?,
    }

    print_json_output(
        &json!({
            "ok": true,
            "settingsFile": settings_file,
            "service": service,
            "output": args.output,
            "format": format.label(),
            "rootSettingsIds": [document.instances[root_index].settings_id.clone()],
            "instances": subtree.len(),
        }),
        args.pretty,
    )
}

pub(crate) fn encode_settings_model(document: &SettingsBytecode, binary: bool) -> Result<Vec<u8>> {
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let root_indices = settings_root_indices(document);
    if root_indices.is_empty() {
        bail!("Model owner has no root instance");
    }
    let service = document.instances[root_indices[0]].name.as_str();
    let paths = build_editor_instance_paths(document, service);
    let mut refs = BytecodeModelExportRefs {
        by_index: HashMap::with_capacity(document.instances.len()),
        by_settings_id: HashMap::with_capacity(document.instances.len()),
        by_path_key: HashMap::with_capacity(document.instances.len()),
        by_path_segments_key: HashMap::with_capacity(document.instances.len()),
        ..Default::default()
    };
    for (index, instance) in document.instances.iter().enumerate() {
        let digest = Sha256::digest(instance.settings_id.as_bytes());
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        let value = u128::from_be_bytes(bytes).max(1);
        let referent = RbxRef::some(value);
        refs.by_index.insert(index, referent);
        refs.by_settings_id
            .insert(instance.settings_id.clone(), referent);
        if let Some(Some(path)) = paths.get(index) {
            insert_unique_rbx_path(
                &mut refs.by_path_segments_key,
                instance_path_key(&path.path_segments),
                referent,
            );
            refs.by_path_key.insert(
                instance_path_parts_key(&path.path_segments, &path.path_ordinals),
                referent,
            );
        }
    }
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let mut metadata = BytecodeExportMetadata::new();
    let mut encoder = BytecodeRbxEncoder::new(document, database, &mut metadata, &refs);
    for index in 0..document.instances.len() {
        let parent = document.instances[index]
            .parent_index
            .and_then(|parent| refs.by_index.get(&parent).copied())
            .unwrap_or_else(|| dom.root_ref());
        let builder = encoder.build(index, BytecodeRbxBuildOptions::default())?;
        dom.insert(parent, builder);
    }
    let roots = root_indices
        .iter()
        .map(|index| refs.by_index[index])
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    if binary {
        rbx_binary::to_writer(&mut output, &dom, &roots)
            .context("Failed to encode binary model")?;
    } else {
        rbx_xml::to_writer_default(&mut output, &dom, &roots)
            .context("Failed to encode XML model")?;
    }
    Ok(output)
}

pub(crate) fn build_rbx_place(
    src_root: &Path,
    services: Vec<String>,
    document_overrides: Option<&HashMap<String, &SettingsBytecode>>,
    allow_unrepresentable_properties: bool,
    capture_logical_properties: bool,
    merge_source_files: bool,
) -> Result<RbxPlaceBuild> {
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let phase_started = Instant::now();
    let export_inputs = services
        .into_par_iter()
        .map(|service| {
            let service_dir = src_root.join(&service);
            let settings_file = service_settings_path(&service_dir);
            if !settings_file.exists() && !service_dir.is_dir() {
                return Ok(None);
            }
            let mut document = if let Some(document) = document_overrides
                .and_then(|documents| documents.get(&service))
                .map(|document| (*document).clone())
            {
                document
            } else if !settings_file.exists() {
                source_only_settings_document(&service_dir, &service)?
            } else {
                SettingsBytecode::read_file(&settings_file)
                    .with_context(|| format!("Failed to read {}", settings_file.display()))?
            };
            let (source_structure_changed, actual_source_paths) =
                if merge_source_files && settings_file.exists() {
                    merge_editor_source_files_into_document(&mut document, &service, &service_dir)?
                } else {
                    (false, HashMap::new())
                };
            let service_name = bytecode_service_name(&document, &settings_file, &service);
            let root_index = editor_service_root_index(&document, &service_name)
                .or_else(|| settings_root_indices(&document).into_iter().next())
                .ok_or_else(|| anyhow::anyhow!("No service root found for {service_name}"))?;
            let children_by_parent = settings_children_by_parent(&document);
            let mut subtree = Vec::new();
            collect_settings_subtree_preorder(&children_by_parent, root_index, &mut subtree);
            let mut source_paths = build_editor_source_paths_by_index_with_children(
                &document,
                &service_name,
                &service_dir,
                &children_by_parent,
            );
            for (index, instance) in document.instances.iter().enumerate() {
                if let Some(path) = actual_source_paths.get(&instance.settings_id) {
                    source_paths[index] = Some(path.clone());
                }
            }
            let instance_paths = build_editor_instance_paths_with_children(
                &document,
                &service_name,
                &children_by_parent,
            );
            Ok(Some((
                service_name,
                settings_file,
                service_dir,
                document,
                source_paths,
                instance_paths,
                root_index,
                subtree,
                source_structure_changed,
            )))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    if export_inputs.is_empty() {
        bail!(
            "No service settings files were found under {}",
            src_root.display()
        );
    }
    log_timing("native editor place input read", phase_started);

    let phase_started = Instant::now();
    let mut unique_settings_id_counts = HashMap::<String, usize>::new();
    let mut global_path_refs = HashMap::<String, RbxRef>::new();
    let mut global_path_segment_refs = HashMap::<String, Option<RbxRef>>::new();
    let mut per_service_index_refs =
        Vec::<HashMap<usize, RbxRef>>::with_capacity(export_inputs.len());
    let mut per_service_settings_refs =
        Vec::<HashMap<String, RbxRef>>::with_capacity(export_inputs.len());
    let mut total_instances = 0usize;

    for (_, _, _, document, _, _, _, subtree, _) in &export_inputs {
        let mut by_index = HashMap::with_capacity(subtree.len());
        let mut by_settings_id = HashMap::with_capacity(subtree.len());
        total_instances += subtree.len();
        for index in subtree.iter().copied() {
            let referent = RbxRef::new();
            by_index.insert(index, referent);
            if let Some(instance) = document.instances.get(index) {
                by_settings_id.insert(instance.settings_id.clone(), referent);
                *unique_settings_id_counts
                    .entry(instance.settings_id.clone())
                    .or_insert(0) += 1;
            }
        }
        per_service_index_refs.push(by_index);
        per_service_settings_refs.push(by_settings_id);
    }

    let mut global_unique_settings_refs = HashMap::<String, RbxRef>::new();
    for (service_index, (_, _, _, document, _, instance_paths_by_index, _, subtree, _)) in
        export_inputs.iter().enumerate()
    {
        let by_index = &per_service_index_refs[service_index];
        for index in subtree.iter().copied() {
            let Some(referent) = by_index.get(&index).copied() else {
                continue;
            };
            if let Some(instance) = document.instances.get(index)
                && unique_settings_id_counts
                    .get(&instance.settings_id)
                    .copied()
                    .unwrap_or(0)
                    == 1
            {
                global_unique_settings_refs.insert(instance.settings_id.clone(), referent);
            }
            if let Some(Some(path)) = instance_paths_by_index.get(index) {
                insert_unique_rbx_path(
                    &mut global_path_segment_refs,
                    instance_path_key(&path.path_segments),
                    referent,
                );
                global_path_refs.insert(
                    instance_path_parts_key(&path.path_segments, &path.path_ordinals),
                    referent,
                );
            }
        }
    }

    let global_unique_settings_refs = Arc::new(global_unique_settings_refs);
    let global_path_refs = Arc::new(global_path_refs);
    let global_path_segment_refs = Arc::new(global_path_segment_refs);
    log_timing("native editor place reference indexes", phase_started);
    struct ServiceBuild {
        root_ref: RbxRef,
        instances: Vec<(Option<RbxRef>, RbxInstanceBuilder)>,
        omitted_properties_by_class: HashMap<String, HashSet<String>>,
        logical_properties_by_ref: HashMap<RbxRef, HashMap<rbx_dom_weak::Ustr, RbxVariant>>,
    }
    let phase_started = Instant::now();
    let built_services = export_inputs
        .par_iter()
        .enumerate()
        .map(
            |(service_index, (_, _, _, document, source_paths, _, root_index, subtree, _))| {
                let by_index = &per_service_index_refs[service_index];
                let refs = BytecodeModelExportRefs {
                    by_index: by_index.clone(),
                    by_settings_id: per_service_settings_refs[service_index].clone(),
                    global_by_settings_id: Some(Arc::clone(&global_unique_settings_refs)),
                    global_by_path_key: Some(Arc::clone(&global_path_refs)),
                    global_by_path_segments_key: Some(Arc::clone(&global_path_segment_refs)),
                    ..Default::default()
                };
                let mut instances = Vec::with_capacity(subtree.len());
                let mut omitted_properties_by_class = HashMap::new();
                let mut logical_properties_by_ref = HashMap::new();
                let mut metadata = BytecodeExportMetadata::new();
                let mut encoder = BytecodeRbxEncoder::new(document, database, &mut metadata, &refs);
                for index in subtree.iter().copied() {
                    let parent_ref = if index == *root_index {
                        None
                    } else {
                        let parent_index =
                            document.instances[index].parent_index.ok_or_else(|| {
                                anyhow::anyhow!("Export subtree contains a detached child")
                            })?;
                        Some(*by_index.get(&parent_index).ok_or_else(|| {
                            anyhow::anyhow!("Export subtree is missing parent referent")
                        })?)
                    };
                    let mut logical_properties = HashMap::new();
                    let builder = encoder.build(
                        index,
                        BytecodeRbxBuildOptions {
                            source_path: source_paths.get(index).and_then(Option::as_deref),
                            omitted_properties_by_class: allow_unrepresentable_properties
                                .then_some(&mut omitted_properties_by_class),
                            logical_omitted_properties: capture_logical_properties
                                .then_some(&mut logical_properties),
                        },
                    )?;
                    if !logical_properties.is_empty() {
                        logical_properties_by_ref.insert(
                            *by_index
                                .get(&index)
                                .context("Export instance referent is missing")?,
                            logical_properties,
                        );
                    }
                    instances.push((parent_ref, builder));
                }
                Ok(ServiceBuild {
                    root_ref: *by_index
                        .get(root_index)
                        .ok_or_else(|| anyhow::anyhow!("Export root referent missing"))?,
                    instances,
                    omitted_properties_by_class,
                    logical_properties_by_ref,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;
    log_timing("native editor place service conversion", phase_started);
    let phase_started = Instant::now();
    let mut dom = RbxWeakDom::new(RbxInstanceBuilder::new("DataModel"));
    let mut omitted_properties_by_class = HashMap::new();
    let mut logical_properties_by_ref = HashMap::new();
    let mut top_level_refs = Vec::with_capacity(export_inputs.len());
    for service in built_services {
        for (class_name, names) in service.omitted_properties_by_class {
            omitted_properties_by_class
                .entry(class_name)
                .or_insert_with(HashSet::new)
                .extend(names);
        }
        logical_properties_by_ref.extend(service.logical_properties_by_ref);
        for (parent_ref, builder) in service.instances {
            let parent_ref = parent_ref.unwrap_or_else(|| dom.root_ref());
            dom.insert(parent_ref, builder);
        }
        top_level_refs.push(service.root_ref);
    }

    let service_roots = export_inputs
        .iter()
        .zip(top_level_refs)
        .map(|((service, _, _, _, _, _, _, _, _), referent)| (service.clone(), referent))
        .collect::<Vec<_>>();
    let mut settings_writes = export_inputs
        .iter()
        .filter(|(_, _, _, _, _, _, _, _, changed)| *changed)
        .map(
            |(service, _, _, document, _, _, _, _, _)| -> Result<EditorSettingsWrite> {
                Ok(EditorSettingsWrite {
                    path: service_settings_path(&src_root.join(service)),
                    document: document.clone(),
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;
    settings_writes.sort_by(|left, right| left.path.cmp(&right.path));
    let mut documents_by_service = HashMap::with_capacity(export_inputs.len());
    let mut paths_by_service = HashMap::with_capacity(export_inputs.len());
    for (service, _, _, document, _, instance_paths, _, _, _) in export_inputs {
        paths_by_service.insert(service.clone(), instance_paths);
        documents_by_service.insert(service, document);
    }
    log_timing("native editor place DOM assembly", phase_started);
    Ok(RbxPlaceBuild {
        dom,
        service_roots,
        documents_by_service,
        paths_by_service,
        settings_writes,
        total_instances,
        omitted_properties_by_class,
        logical_properties_by_ref,
    })
}

pub(crate) fn bytecode_export_place(mut args: BytecodeExportPlaceArgs) -> Result<()> {
    apply_configured_project_layout(&mut args.project.project_root, &mut args.project.src_root)?;
    let project_root = resolve_existing_project_root(&args.project.project_root)?;
    let src_root = absolutize_under(&project_root, &args.project.src_root);
    let format = match args.format.as_deref() {
        Some(raw) => RbxPlaceFormat::parse(raw)?,
        None => RbxPlaceFormat::from_path(&args.output)?,
    };
    let services = explorer_daemon_services(&src_root, &args.services)?;
    let build = build_rbx_place(&src_root, services, None, false, false, false)?;
    let top_level_refs = build
        .service_roots
        .iter()
        .map(|(_, referent)| *referent)
        .collect::<Vec<_>>();
    format.write(&args.output, &build.dom, &top_level_refs)?;
    let exported_services = build
        .service_roots
        .iter()
        .map(|(service, _)| service.clone())
        .collect::<Vec<_>>();
    print_json_output(
        &json!({
            "ok": true,
            "output": args.output,
            "format": format.label(),
            "services": exported_services,
            "serviceCount": top_level_refs.len(),
            "instances": build.total_instances,
        }),
        args.pretty,
    )
}

pub(crate) fn bytecode_import_model(args: BytecodeImportModelArgs) -> Result<()> {
    let (settings_file, service_hint) = resolve_bytecode_cli_settings_file(
        args.input.settings_file.as_deref(),
        args.input.service_or_file.as_deref(),
        Some(args.service.as_str()),
    )?;
    ensure_service_store_exists(&settings_file, &service_hint)?;
    let _lock = acquire_settings_file_lock(&settings_file)?;
    let mut document = SettingsBytecode::read_file(&settings_file)?;
    let before_document = document.clone();
    let service = bytecode_service_name(&document, &settings_file, &service_hint);
    let service_dir = settings_file
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let source_paths_before = build_editor_source_paths_by_index(&document, &service, &service_dir);
    let target_parent_index = bytecode_parent_index(
        &document,
        args.parent.no_parent,
        args.parent.parent_index,
        args.parent.parent_settings_id.as_deref(),
        args.parent.parent_name.as_deref(),
        args.parent.parent_class_name.as_deref(),
    )?;
    let outcome = import_rbx_model_into_document(
        &mut document,
        &settings_file,
        &service,
        &args.model,
        target_parent_index,
    )?;

    let mut writes = outcome.source_files.clone();
    let mut removals = Vec::new();
    collect_source_path_updates(
        &before_document,
        &source_paths_before,
        &document,
        &service,
        &service_dir,
        &mut writes,
        &mut removals,
    )?;
    writes.insert(settings_file.clone(), encode_settings_bytecode(&document)?);
    let changed_paths = file_mutation_paths(&writes, &removals);
    apply_file_mutations(&writes, &removals)?;
    print_json_output(
        &json!({
            "ok": true,
            "settingsFile": settings_file,
            "service": service,
            "model": args.model,
            "format": outcome.format.label(),
            "rootSettingsIds": outcome.root_settings_ids,
            "settingsIds": outcome.settings_ids,
            "sourceWrites": outcome.source_writes,
            "changedPaths": changed_paths,
        }),
        args.pretty,
    )
}

pub(crate) fn import_rbx_model_into_document(
    document: &mut SettingsBytecode,
    settings_file: &Path,
    service: &str,
    model: &Path,
    target_parent_index: Option<usize>,
) -> Result<BytecodeModelImportOutcome> {
    let format = RbxModelFormat::from_path(model)?;
    let input = File::open(model).with_context(|| format!("Failed to read {}", model.display()))?;
    let reader = BufReader::new(input);
    let dom = match format {
        RbxModelFormat::Binary => rbx_binary::from_reader(reader)
            .with_context(|| format!("Failed to read {}", model.display()))?,
        RbxModelFormat::Xml => rbx_xml::from_reader_default(reader)
            .with_context(|| format!("Failed to read {}", model.display()))?,
    };
    let root_refs = rbx_model_top_level_refs(&dom);
    if root_refs.is_empty() {
        bail!("Model file contains no importable root instances");
    }
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;

    let mut refs_preorder = Vec::new();
    for root_ref in &root_refs {
        collect_rbx_subtree_preorder(&dom, *root_ref, &mut refs_preorder);
    }

    let children_before = settings_children_by_parent(document);
    let mut target_child_indices = match target_parent_index {
        Some(parent_index) => children_before
            .get(parent_index)
            .cloned()
            .unwrap_or_default(),
        None => settings_root_indices(document),
    };
    let root_ref_set = root_refs.iter().copied().collect::<HashSet<_>>();
    let mut existing_settings_ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<HashSet<_>>();
    let mut next_settings_id_seed = document.instances.len();
    let mut new_index_by_ref = HashMap::with_capacity(refs_preorder.len());
    let mut settings_id_by_ref = HashMap::with_capacity(refs_preorder.len());
    let mut imported_settings_ids = Vec::with_capacity(refs_preorder.len());
    let mut root_settings_ids = Vec::with_capacity(root_refs.len());

    for referent in refs_preorder.iter().copied() {
        let rbx_instance = dom
            .get_by_ref(referent)
            .ok_or_else(|| anyhow::anyhow!("Model contains a missing referent"))?;
        let is_root = root_ref_set.contains(&referent);
        let parent_index =
            if is_root {
                target_parent_index
            } else {
                let parent_ref = rbx_instance.parent();
                Some(*new_index_by_ref.get(&parent_ref).ok_or_else(|| {
                    anyhow::anyhow!("Model subtree is missing an imported parent")
                })?)
            };
        let name = if is_root {
            unique_editor_child_name(document, &target_child_indices, &rbx_instance.name)
        } else {
            rbx_instance.name.clone()
        };
        let settings_id =
            next_editor_settings_id_fast(&mut existing_settings_ids, &mut next_settings_id_seed);
        let new_index = document.instances.len();
        document.instances.push(SettingsBytecodeInstance::new(
            settings_id.clone(),
            name,
            rbx_instance.class.to_string(),
            parent_index,
        ));
        if is_root {
            target_child_indices.push(new_index);
            root_settings_ids.push(settings_id.clone());
        }
        imported_settings_ids.push(settings_id.clone());
        new_index_by_ref.insert(referent, new_index);
        settings_id_by_ref.insert(referent, settings_id);
    }

    let path_parts_by_index = build_editor_instance_paths(document, service);
    let path_segments_by_index = path_parts_by_index
        .iter()
        .map(|path| path.as_ref().map(|path| path.path_segments.clone()))
        .collect::<Vec<_>>();
    let import_refs = BytecodeModelImportRefs {
        path_segments_by_ref: Arc::new(
            new_index_by_ref
                .iter()
                .filter_map(|(referent, index)| {
                    path_segments_by_index
                        .get(*index)
                        .and_then(std::clone::Clone::clone)
                        .map(|path| (*referent, path))
                })
                .collect(),
        ),
        path_ordinals_by_ref: Arc::new(
            new_index_by_ref
                .iter()
                .filter_map(|(referent, index)| {
                    path_parts_by_index
                        .get(*index)
                        .and_then(Option::as_ref)
                        .map(|path| (*referent, path.path_ordinals.clone()))
                })
                .collect(),
        ),
        new_index_by_ref,
        settings_id_by_ref,
        path_segments_by_index,
        ..Default::default()
    };
    let mut source_by_index = HashMap::<usize, String>::new();
    for referent in refs_preorder.iter().copied() {
        let new_index = *import_refs
            .new_index_by_ref
            .get(&referent)
            .ok_or_else(|| anyhow::anyhow!("Imported referent map missing"))?;
        let rbx_instance = dom
            .get_by_ref(referent)
            .ok_or_else(|| anyhow::anyhow!("Model contains a missing referent"))?;
        let (properties, attributes, source) =
            rbx_instance_to_settings_records(rbx_instance, database, &import_refs, false, None);
        if let Some(instance) = document.instances.get_mut(new_index) {
            instance.properties = properties;
            instance.attributes = attributes;
        }
        if let Some(source) = source {
            source_by_index.insert(new_index, source);
        }
    }

    let service_dir = settings_file
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let source_paths = build_editor_source_paths_by_index(document, service, &service_dir);
    let mut source_writes = Vec::new();
    let mut source_files = BTreeMap::new();
    let mut source_by_settings_id = BTreeMap::new();
    for (index, source) in &source_by_index {
        let path = source_paths
            .get(*index)
            .and_then(|path| path.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Could not resolve source path for imported script {}",
                    document.instances[*index].name
                )
            })?;
        source_files.insert(path.clone(), source.as_bytes().to_vec());
        source_by_settings_id.insert(
            document.instances[*index].settings_id.clone(),
            source.as_bytes().to_vec(),
        );
        source_writes.push(json!({
            "settingsId": document.instances[*index].settings_id.clone(),
            "path": path,
        }));
    }

    Ok(BytecodeModelImportOutcome {
        format,
        root_settings_ids,
        settings_ids: imported_settings_ids,
        source_writes,
        source_files,
        source_by_settings_id,
    })
}

pub(crate) fn read_settings_model_document(path: &Path) -> Result<SettingsBytecode> {
    let mut document = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: Vec::new(),
    };
    import_rbx_model_into_document(&mut document, path, "", path, None)?;
    Ok(document)
}

pub(crate) fn bytecode_repack(mut args: BytecodeRepackArgs) -> Result<()> {
    apply_configured_project_layout(&mut args.project.project_root, &mut args.project.src_root)?;
    let settings_files = bytecode_repack_settings_files(&args)?;
    let mut rewritten = Vec::with_capacity(settings_files.len());
    let mut total_before = 0_u64;
    let mut total_after = 0_u64;

    for settings_file in settings_files {
        let before = fs::metadata(&settings_file)
            .with_context(|| format!("Failed to stat {}", settings_file.display()))?
            .len();
        let document = SettingsBytecode::read_file(&settings_file)?;
        document.write_file(&settings_file)?;
        let after = fs::metadata(&settings_file)
            .with_context(|| format!("Failed to stat {}", settings_file.display()))?
            .len();
        total_before += before;
        total_after += after;
        rewritten.push(json!({
            "path": settings_file,
            "bytesBefore": before,
            "bytesAfter": after,
            "bytesSaved": before.saturating_sub(after),
        }));
    }

    print_json_output(
        &json!({
            "ok": true,
            "files": rewritten,
            "fileCount": rewritten.len(),
            "bytesBefore": total_before,
            "bytesAfter": total_after,
            "bytesSaved": total_before.saturating_sub(total_after),
        }),
        args.pretty,
    )
}

fn bytecode_repack_settings_files(args: &BytecodeRepackArgs) -> Result<Vec<PathBuf>> {
    let project_root = resolve_existing_project_root(&args.project.project_root)?;

    let mut settings_files = Vec::new();
    if args.paths.is_empty() {
        let src_root = absolutize_under(&project_root, &args.project.src_root);
        for entry in WalkDir::new(&src_root) {
            let entry = entry?;
            if entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(is_service_settings_file_name)
            {
                settings_files.push(entry.into_path());
            }
        }
    } else {
        for raw_path in &args.paths {
            let raw = raw_path.to_string_lossy();
            let path = absolutize_under(&project_root, raw_path);
            let settings_file = if path.is_dir() {
                service_settings_path(&path)
            } else if !path.exists() && !bytecode_input_looks_like_settings_file(&raw) {
                let src_root = absolutize_under(&project_root, &args.project.src_root);
                let service = raw.trim();
                validate_filesystem_instance_name(service, "service")?;
                service_settings_path(&src_root.join(service))
            } else {
                path
            };
            if !settings_file.exists() {
                bail!("Settings file does not exist: {}", settings_file.display());
            }
            settings_files.push(settings_file);
        }
    }

    settings_files.sort();
    settings_files.dedup();
    Ok(settings_files)
}
