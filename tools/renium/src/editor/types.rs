use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::cli::PushEditorChangesArgs;
use crate::roblox::schema::{
    EnumValueNameMap, MATERIAL_SERVICE_CLASS, PropertySchemaMap, USE_2022_MATERIALS_PROPERTY,
};
use crate::settings::bytecode::SettingsBytecode;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorSourceChange {
    pub(crate) service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) settings_id: Option<String>,
    pub(crate) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorPropertyChange {
    pub(crate) service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) settings_id: Option<String>,
    pub(crate) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(crate) properties: Map<String, Value>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(crate) attributes: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) deleted_attributes: Vec<String>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorInstanceDescriptor {
    pub(crate) settings_id: String,
    pub(crate) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) ambiguous_siblings: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) anchor_only: bool,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(crate) match_properties: Map<String, Value>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(crate) match_attributes: Map<String, Value>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorPreserveDescriptor {
    pub(crate) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) path_ordinals: Vec<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorInstanceChange {
    pub(crate) mode: String,
    pub(crate) service: String,
    pub(crate) allow_deletes: bool,
    pub(crate) instances: Vec<EditorInstanceDescriptor>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) preserve_instances: Vec<EditorPreserveDescriptor>,
}
#[derive(Clone)]
pub(crate) struct EditorSourceTarget {
    pub(crate) service: String,
    pub(crate) settings_id: Option<String>,
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
}
#[derive(Clone)]
pub(crate) struct EditorInstancePath {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
}

impl EditorInstancePath {
    pub(crate) fn is_descendant_of(&self, service: &str) -> bool {
        self.path_segments.len() > 1
            && self
                .path_segments
                .first()
                .is_some_and(|segment| segment == service)
    }
}

#[derive(Default)]
pub(crate) struct EditorPropertyFilter {
    pub(crate) settings_ids: HashSet<String>,
    pub(crate) property_names: HashSet<String>,
}

impl EditorPropertyFilter {
    pub(crate) fn from_args(args: &PushEditorChangesArgs) -> Result<Self> {
        Ok(Self {
            settings_ids: expand_editor_target_settings_ids(args)?,
            property_names: args
                .target_properties
                .iter()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .collect(),
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.settings_ids.is_empty() || !self.property_names.is_empty()
    }

    pub(crate) fn includes_instance(&self, settings_id: &str) -> bool {
        self.settings_ids.is_empty() || self.settings_ids.contains(settings_id)
    }

    pub(crate) fn includes_property(&self, property_name: &str) -> bool {
        self.property_names.is_empty()
            || self
                .property_names
                .contains(&property_name.to_ascii_lowercase())
    }
}

pub(crate) fn expand_editor_target_settings_ids(
    args: &PushEditorChangesArgs,
) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for value in &args.target_settings_ids {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            ids.insert(trimmed.to_string());
        }
    }
    for list_path in &args.target_settings_id_files {
        let raw = fs::read_to_string(list_path).with_context(|| {
            format!(
                "Failed to read target settings id file {}",
                list_path.display()
            )
        })?;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            ids.insert(trimmed.to_string());
        }
    }
    Ok(ids)
}

pub(crate) struct EditorSourceEnsureResult {
    pub(crate) target: EditorSourceTarget,
    pub(crate) upsert_instances: Vec<EditorInstanceDescriptor>,
    pub(crate) replace_instances: Vec<EditorInstanceDescriptor>,
    pub(crate) changed: bool,
}

pub(crate) struct EditorSourcePathSpec {
    pub(crate) service: String,
    pub(crate) class_name: String,
    pub(crate) run_context: Option<String>,
    pub(crate) is_init: bool,
    pub(crate) instance_name: String,
    pub(crate) instance_stem: String,
    pub(crate) parent_components: Vec<String>,
    pub(crate) path_segments: Vec<String>,
}

#[derive(Default)]
pub(crate) struct EditorChangeSet {
    pub(crate) instance_changes: Vec<EditorInstanceChange>,
    pub(crate) source_changes: Vec<EditorSourceChange>,
    pub(crate) property_changes: Vec<EditorPropertyChange>,
    pub(crate) history_entries: Vec<EditorHistoryEntry>,
    pub(crate) settings_writes: Vec<EditorSettingsWrite>,
    pub(crate) files_to_studio_filters_active: bool,
}

impl EditorChangeSet {
    pub(crate) fn services(&self) -> impl Iterator<Item = &str> {
        self.instance_changes
            .iter()
            .map(|change| change.service.as_str())
            .chain(
                self.source_changes
                    .iter()
                    .map(|change| change.service.as_str()),
            )
            .chain(
                self.property_changes
                    .iter()
                    .map(|change| change.service.as_str()),
            )
    }
}

pub(crate) fn take_pre_routed_protected_writes(changes: &mut EditorChangeSet) -> Vec<Value> {
    let mut rows = Vec::new();
    for change in &mut changes.property_changes {
        if change.service != MATERIAL_SERVICE_CLASS
            || change.class_name != MATERIAL_SERVICE_CLASS
            || change.path_segments.len() != 1
            || change.path_segments[0] != MATERIAL_SERVICE_CLASS
        {
            continue;
        }
        let Some(value) = change.properties.remove(USE_2022_MATERIALS_PROPERTY) else {
            continue;
        };
        rows.push(json!({
            "kind": "property",
            "service": &change.service,
            "settingsId": &change.settings_id,
            "pathSegments": &change.path_segments,
            "pathOrdinals": &change.path_ordinals,
            "className": &change.class_name,
            "name": USE_2022_MATERIALS_PROPERTY,
            "value": value,
        }));
    }
    changes.property_changes.retain(|change| {
        !change.properties.is_empty()
            || !change.attributes.is_empty()
            || !change.deleted_attributes.is_empty()
    });
    rows
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorBinaryExportGroup {
    pub(crate) service: String,
    pub(crate) target_path: Vec<String>,
    pub(crate) count: usize,
    pub(crate) instance_count: usize,
    #[serde(default)]
    pub(crate) script_count: usize,
    pub(crate) class_names: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_json_object_or_empty_array")]
    pub(crate) root_properties: Map<String, Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorBinaryImportGroup {
    pub(crate) service: String,
    pub(crate) target_path: Vec<String>,
    pub(crate) count: usize,
    pub(crate) payload_root_name: String,
    pub(crate) root_paths: Vec<EditorBinaryRootPath>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) retained_roots: Vec<EditorBinaryRetainedRoot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) package_roots: Vec<EditorBinaryPackageRoot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) strip_package_payloads: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) change_generation: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorBinaryRootPath {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorBinaryRetainedRoot {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    pub(crate) payload_index: usize,
    pub(crate) instance_count: usize,
    pub(crate) payload_omitted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorBinaryPackageRoot {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
}

fn deserialize_json_object_or_empty_array<'de, D>(
    deserializer: D,
) -> std::result::Result<Map<String, Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::Object(value) => Ok(value),
        Value::Array(value) if value.is_empty() => Ok(Map::new()),
        _ => Err(serde::de::Error::custom("expected a map")),
    }
}

#[derive(Deserialize)]
pub(crate) struct EditorBinarySerializationBatch {
    pub(crate) id: String,
    pub(crate) services: Vec<String>,
}

pub(crate) struct EditorBinaryExport {
    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) bytes: Vec<u8>,
    pub(crate) groups: Vec<EditorBinaryExportGroup>,
    pub(crate) serialization_batches: Vec<EditorBinarySerializationBatch>,
    pub(crate) export_id: Option<String>,
    pub(crate) property_schema_by_class: PropertySchemaMap,
    pub(crate) enum_value_names_by_type: EnumValueNameMap,
}

pub(crate) struct EditorBinaryImport {
    pub(crate) bytes: Vec<u8>,
    pub(crate) groups: Vec<EditorBinaryImportGroup>,
    pub(crate) instance_count: usize,
    pub(crate) post_apply_properties_by_class: HashMap<String, HashSet<String>>,
    pub(crate) post_apply_properties_by_path: HashMap<String, HashSet<String>>,
    pub(crate) external_references_post_applied: bool,
}

impl EditorBinaryImport {
    pub(crate) fn imports_service(&self, service: &str) -> bool {
        self.groups.iter().any(|group| group.service == service)
    }

    pub(crate) fn retains_path(
        &self,
        service: &str,
        segments: &[String],
        ordinals: &[usize],
    ) -> bool {
        self.groups
            .iter()
            .filter(|group| group.service == service)
            .flat_map(|group| &group.retained_roots)
            .any(|root| {
                segments.starts_with(&root.path_segments)
                    && ordinals.starts_with(&root.path_ordinals)
            })
    }
}

pub(crate) struct EditorSettingsWrite {
    pub(crate) path: PathBuf,
    pub(crate) document: SettingsBytecode,
}

pub(crate) struct EditorHistoryEntry {
    pub(crate) service: String,
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) settings_id: Option<String>,
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    pub(crate) source_key: Option<String>,
    pub(crate) settings_before: Option<SettingsBytecode>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorRevertManifest {
    pub(crate) version: u8,
    pub(crate) created_unix_ms: u128,
    pub(crate) service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) settings_id: Option<String>,
    pub(crate) path_segments: Vec<String>,
    pub(crate) class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) settings_backup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_backup: Option<String>,
}
