use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::command_line::PushEditorChangesArgs;
use super::property_schema::{
    EnumValueNameMap, MATERIAL_SERVICE_CLASS, PropertySchemaMap, USE_2022_MATERIALS_PROPERTY,
};
use super::settings_bytecode::SettingsBytecode;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorSourceChange {
    pub(super) service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) settings_id: Option<String>,
    pub(super) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) path_ordinals: Vec<usize>,
    pub(super) class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub(super) deleted: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorPropertyChange {
    pub(super) service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) settings_id: Option<String>,
    pub(super) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) path_ordinals: Vec<usize>,
    pub(super) class_name: String,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(super) properties: Map<String, Value>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(super) attributes: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) deleted_attributes: Vec<String>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorInstanceDescriptor {
    pub(super) settings_id: String,
    pub(super) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) path_ordinals: Vec<usize>,
    pub(super) class_name: String,
    #[serde(skip_serializing_if = "is_false")]
    pub(super) ambiguous_siblings: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub(super) anchor_only: bool,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(super) match_properties: Map<String, Value>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(super) match_attributes: Map<String, Value>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorPreserveDescriptor {
    pub(super) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) path_ordinals: Vec<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorInstanceChange {
    pub(super) mode: String,
    pub(super) service: String,
    pub(super) allow_deletes: bool,
    pub(super) instances: Vec<EditorInstanceDescriptor>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) preserve_instances: Vec<EditorPreserveDescriptor>,
}
#[derive(Clone)]
pub(super) struct EditorSourceTarget {
    pub(super) service: String,
    pub(super) settings_id: Option<String>,
    pub(super) path_segments: Vec<String>,
    pub(super) path_ordinals: Vec<usize>,
    pub(super) class_name: String,
}
#[derive(Clone)]
pub(super) struct EditorInstancePath {
    pub(super) path_segments: Vec<String>,
    pub(super) path_ordinals: Vec<usize>,
}

impl EditorInstancePath {
    pub(super) fn is_descendant_of(&self, service: &str) -> bool {
        self.path_segments.len() > 1
            && self
                .path_segments
                .first()
                .is_some_and(|segment| segment == service)
    }
}

#[derive(Default)]
pub(super) struct EditorPropertyFilter {
    pub(super) settings_ids: HashSet<String>,
    pub(super) property_names: HashSet<String>,
}

impl EditorPropertyFilter {
    pub(super) fn from_args(args: &PushEditorChangesArgs) -> Result<Self> {
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

    pub(super) fn is_active(&self) -> bool {
        !self.settings_ids.is_empty() || !self.property_names.is_empty()
    }

    pub(super) fn includes_instance(&self, settings_id: &str) -> bool {
        self.settings_ids.is_empty() || self.settings_ids.contains(settings_id)
    }

    pub(super) fn includes_property(&self, property_name: &str) -> bool {
        self.property_names.is_empty()
            || self
                .property_names
                .contains(&property_name.to_ascii_lowercase())
    }
}

pub(super) fn expand_editor_target_settings_ids(
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

pub(super) struct EditorSourceEnsureResult {
    pub(super) target: EditorSourceTarget,
    pub(super) upsert_instances: Vec<EditorInstanceDescriptor>,
    pub(super) replace_instances: Vec<EditorInstanceDescriptor>,
    pub(super) changed: bool,
}

pub(super) struct EditorSourcePathSpec {
    pub(super) service: String,
    pub(super) class_name: String,
    pub(super) run_context: Option<String>,
    pub(super) instance_name: String,
    pub(super) instance_stem: String,
    pub(super) parent_components: Vec<String>,
    pub(super) path_segments: Vec<String>,
}

#[derive(Default)]
pub(super) struct EditorChangeSet {
    pub(super) instance_changes: Vec<EditorInstanceChange>,
    pub(super) source_changes: Vec<EditorSourceChange>,
    pub(super) property_changes: Vec<EditorPropertyChange>,
    pub(super) history_entries: Vec<EditorHistoryEntry>,
    pub(super) settings_writes: Vec<EditorSettingsWrite>,
    pub(super) files_to_studio_filters_active: bool,
}

impl EditorChangeSet {
    pub(super) fn services(&self) -> impl Iterator<Item = &str> {
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

pub(super) fn take_pre_routed_protected_writes(changes: &mut EditorChangeSet) -> Vec<Value> {
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
pub(super) struct EditorBinaryExportGroup {
    pub(super) service: String,
    pub(super) target_path: Vec<String>,
    pub(super) count: usize,
    pub(super) instance_count: usize,
    pub(super) class_names: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_json_object_or_empty_array")]
    pub(super) root_properties: Map<String, Value>,
    pub(super) change_generation: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorBinaryImportGroup {
    pub(super) service: String,
    pub(super) target_path: Vec<String>,
    pub(super) count: usize,
    pub(super) payload_root_name: String,
    pub(super) root_paths: Vec<EditorBinaryRootPath>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) retained_roots: Vec<EditorBinaryRetainedRoot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) package_roots: Vec<EditorBinaryPackageRoot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) strip_package_payloads: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) change_generation: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorBinaryRootPath {
    pub(super) path_segments: Vec<String>,
    pub(super) path_ordinals: Vec<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorBinaryRetainedRoot {
    pub(super) path_segments: Vec<String>,
    pub(super) path_ordinals: Vec<usize>,
    pub(super) class_name: String,
    pub(super) payload_index: usize,
    pub(super) instance_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorBinaryPackageRoot {
    pub(super) path_segments: Vec<String>,
    pub(super) path_ordinals: Vec<usize>,
    pub(super) class_name: String,
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
pub(super) struct EditorBinarySerializationBatch {
    pub(super) id: String,
    pub(super) services: Vec<String>,
}

pub(super) struct EditorBinaryExport {
    pub(super) bytes: Vec<u8>,
    pub(super) groups: Vec<EditorBinaryExportGroup>,
    pub(super) serialization_batches: Vec<EditorBinarySerializationBatch>,
    pub(super) export_id: Option<String>,
    pub(super) property_schema_by_class: PropertySchemaMap,
    pub(super) enum_value_names_by_type: EnumValueNameMap,
}

pub(super) struct EditorBinaryImport {
    pub(super) bytes: Vec<u8>,
    pub(super) groups: Vec<EditorBinaryImportGroup>,
    pub(super) instance_count: usize,
    pub(super) post_apply_properties_by_class: HashMap<String, HashSet<String>>,
    pub(super) post_apply_properties_by_path: HashMap<String, HashSet<String>>,
    pub(super) external_references_post_applied: bool,
}

impl EditorBinaryImport {
    pub(super) fn retains_path(
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
                    && ordinals
                        .iter()
                        .zip(&root.path_ordinals)
                        .all(|(left, right)| left == right)
            })
    }
}

pub(super) struct EditorSettingsWrite {
    pub(super) path: PathBuf,
    pub(super) document: SettingsBytecode,
}

pub(super) struct EditorHistoryEntry {
    pub(super) service: String,
    pub(super) source_path: Option<PathBuf>,
    pub(super) settings_id: Option<String>,
    pub(super) path_segments: Vec<String>,
    pub(super) path_ordinals: Vec<usize>,
    pub(super) class_name: String,
    pub(super) source_key: Option<String>,
    pub(super) settings_before: Option<SettingsBytecode>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EditorRevertManifest {
    pub(super) version: u8,
    pub(super) created_unix_ms: u128,
    pub(super) service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) settings_id: Option<String>,
    pub(super) path_segments: Vec<String>,
    pub(super) class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) settings_backup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_backup: Option<String>,
}
