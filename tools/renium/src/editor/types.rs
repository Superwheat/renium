use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::cli::PushEditorChangesArgs;
use crate::roblox::schema::{
    EnumValueNameMap, MATERIAL_SERVICE_CLASS, PropertySchemaMap, TEXTURE_PACK_PROPERTY,
    USE_2022_MATERIALS_PROPERTY, has_protected_texture_pack,
};
use crate::settings::bytecode::SettingsBytecode;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Exact values a committed import could not apply in its deferred transaction.
/// Verification may tolerate these fields only on this target and at this value.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingImportFields {
    pub(crate) service: String,
    pub(crate) settings_id: Option<String>,
    pub(crate) class_name: String,
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) properties: Map<String, Value>,
}

impl From<&EditorPropertyChange> for PendingImportFields {
    fn from(row: &EditorPropertyChange) -> Self {
        Self {
            service: row.service.clone(),
            settings_id: row.settings_id.clone(),
            class_name: row.class_name.clone(),
            path_segments: row.path_segments.clone(),
            path_ordinals: row.path_ordinals.clone(),
            properties: row.properties.clone(),
        }
    }
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

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorPropertyChange {
    pub(crate) service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) settings_id: Option<String>,
    pub(crate) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    #[serde(
        skip_serializing_if = "Map::is_empty",
        serialize_with = "serialize_bridge_properties"
    )]
    pub(crate) properties: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) reset_properties: Vec<String>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub(crate) attributes: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) deleted_attributes: Vec<String>,
    /// `attributes` is the whole map; Studio drops attributes absent from it.
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) attributes_complete: bool,
}

// Font.new cannot carry the serializer's cached face, and even a missing cache
// must replace a previous cached value. Keep native values transport-only: the
// change plan and saved settings remain ordinary, readable property JSON.
fn serialize_bridge_properties<S: serde::Serializer>(
    properties: &Map<String, Value>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use rbx_dom_weak::{InstanceBuilder, WeakDom};
    use serde::ser::{Error, SerializeMap};
    let mut map = serializer.serialize_map(Some(properties.len()))?;
    for (name, value) in properties {
        if value.get("_type").and_then(Value::as_str) == Some("Font")
            || name == "FontFace" && value.get("family").is_some()
        {
            let Some(font) = crate::rbx::encode::json_to_rbx_font(value) else {
                // Leave invalid input for the ordinary property validator,
                // rather than converting an invalid enum to a default Font.
                map.serialize_entry(name, value)?;
                continue;
            };
            let mut native = crate::rbx::decode::rbx_font_to_settings_json(&font);
            let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
            let carrier = dom.insert(
                dom.root_ref(),
                InstanceBuilder::new("TextLabel").with_property("FontFace", font),
            );
            let mut bytes = Vec::new();
            rbx_binary::to_writer(&mut bytes, &dom, &[carrier]).map_err(S::Error::custom)?;
            native["_nativeFont"] = Value::String(base64::encode(bytes));
            map.serialize_entry(name, &native)?;
        } else {
            map.serialize_entry(name, value)?;
        }
    }
    map.end()
}

#[cfg(test)]
mod identity_text_tests {
    use super::*;

    #[test]
    fn identity_strings_travel_as_one_joined_text_unless_one_holds_nul() -> Result<()> {
        #[derive(Serialize)]
        struct Wire(
            #[serde(serialize_with = "serialize_identity_text")] Option<EditorBinaryStructure>,
        );
        let structure = |strings: &[&str]| EditorBinaryStructure {
            strings: strings.iter().map(|text| text.to_string()).collect(),
            nodes: "AAAA".into(),
        };
        assert_eq!(
            serde_json::to_value(Wire(Some(structure(&["", "Part", "editor:1"]))))?,
            json!({ "stringText": base64::encode("\0Part\0editor:1"), "stringCount": 3, "nodes": "AAAA" })
        );
        assert_eq!(
            serde_json::to_value(Wire(Some(structure(&["Part", "a\0b"]))))?,
            json!({ "strings": ["Part", "a\0b"], "nodes": "AAAA" })
        );
        Ok(())
    }
}

#[cfg(test)]
mod font_transport_tests {
    use super::*;

    #[test]
    fn font_transport_preserves_native_cache_without_changing_saved_values() {
        #[derive(Serialize)]
        struct Wire<'a>(
            #[serde(serialize_with = "serialize_bridge_properties")] &'a Map<String, Value>,
        );
        for font in [
            json!({"family":"rbxasset://fonts/families/BuilderSans.json", "weight":"Medium", "style":"Normal", "cachedFaceId":"rbxasset://fonts/BuilderSans-Medium.otf"}),
            json!({"_type":"Font", "family":"rbxasset://fonts/families/Arial.json", "weight":700, "style":1}),
            json!({"family":"rbxasset://fonts/families/SourceSansPro.json"}),
        ] {
            let properties = Map::from_iter([
                ("FontFace".to_string(), font.clone()),
                ("TextSize".to_string(), json!(12)),
            ]);
            let wire = serde_json::to_value(Wire(&properties)).unwrap();
            assert_eq!(properties["FontFace"], font);
            assert_eq!(wire["TextSize"], 12);
            let bytes = base64::decode(wire["FontFace"]["_nativeFont"].as_str().unwrap()).unwrap();
            assert!(bytes.len() < 1024);
            let dom = rbx_binary::from_reader(std::io::Cursor::new(bytes)).unwrap();
            assert_eq!(dom.root().children().len(), 1);
            let carrier = dom.get_by_ref(dom.root().children()[0]).unwrap();
            assert_eq!(carrier.class.as_str(), "TextLabel");
            assert!(carrier.children().is_empty());
            let rbx_dom_weak::types::Variant::Font(native) =
                &carrier.properties[&"FontFace".into()]
            else {
                panic!("Font missing")
            };
            assert_eq!(
                *native,
                crate::rbx::encode::json_to_rbx_font(&font).unwrap()
            );
            let mut transported = wire["FontFace"].clone();
            transported.as_object_mut().unwrap().remove("_nativeFont");
            assert_eq!(
                transported,
                crate::rbx::decode::rbx_font_to_settings_json(native)
            );
        }
        let invalid = Map::from_iter([(
            "FontFace".to_string(),
            json!({"family":"family", "weight":"invalid"}),
        )]);
        assert_eq!(
            serde_json::to_value(Wire(&invalid)).unwrap(),
            Value::Object(invalid)
        );
    }
}

#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorInstanceDescriptor {
    pub(crate) settings_id: String,
    pub(crate) path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) path_ordinals: Vec<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) previous_path_segments: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) previous_path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous_class_name: Option<String>,
    /// The identity Studio's own capture gave the instance this entry
    /// replaces or removes: a matched identity, or `debug:` plus the debug id
    /// of an instance no push has identified. Studio resolves it before the
    /// previous path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous_settings_id: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) ambiguous_siblings: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) anchor_only: bool,
    /// The reconciler found no Studio counterpart for this identity. Studio
    /// creates the instance instead of adopting a same-named sibling that no
    /// other entry claimed.
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) recreate: bool,
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

#[derive(Clone, Serialize)]
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

#[derive(Default, Clone)]
pub(crate) struct EditorPropertyFilter {
    /// Ids given without a service; they select that instance in every store.
    pub(crate) settings_ids: HashSet<String>,
    /// Ids given as `Service/ID`; stores number instances from their own
    /// size, so the same id names unrelated instances in other services.
    pub(crate) service_settings_ids: std::collections::HashMap<String, HashSet<String>>,
    pub(crate) property_names: HashSet<String>,
}

impl EditorPropertyFilter {
    pub(crate) fn from_args(args: &PushEditorChangesArgs) -> Result<Self> {
        let targets = expand_editor_target_settings_ids(args)?;
        Ok(Self {
            settings_ids: targets.any,
            service_settings_ids: targets.by_service,
            property_names: args
                .target_properties
                .iter()
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .collect(),
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.has_target_ids() || !self.property_names.is_empty()
    }

    pub(crate) fn has_target_ids(&self) -> bool {
        !self.settings_ids.is_empty() || !self.service_settings_ids.is_empty()
    }

    pub(crate) fn includes_instance_in(&self, service: &str, settings_id: &str) -> bool {
        !self.has_target_ids()
            || self.settings_ids.contains(settings_id)
            || self
                .service_settings_ids
                .get(service)
                .is_some_and(|ids| ids.contains(settings_id))
    }

    /// Every id that selects instances of this service.
    pub(crate) fn ids_for_service(&self, service: &str) -> HashSet<String> {
        let mut ids = self.settings_ids.clone();
        if let Some(scoped) = self.service_settings_ids.get(service) {
            ids.extend(scoped.iter().cloned());
        }
        ids
    }

    pub(crate) fn add_service_ids(&mut self, service: &str, ids: impl IntoIterator<Item = String>) {
        self.service_settings_ids
            .entry(service.to_string())
            .or_default()
            .extend(ids);
    }

    pub(crate) fn includes_property(&self, property_name: &str) -> bool {
        self.property_names.is_empty()
            || self
                .property_names
                .contains(&property_name.to_ascii_lowercase())
    }
}

#[derive(Default)]
pub(crate) struct EditorTargetIds {
    pub(crate) any: HashSet<String>,
    pub(crate) by_service: std::collections::HashMap<String, HashSet<String>>,
}

impl EditorTargetIds {
    /// `Service/ID` binds the id to one store; a bare id applies everywhere.
    fn insert(&mut self, entry: &str) {
        match entry.split_once('/') {
            Some((service, id)) if !service.is_empty() && !id.is_empty() => {
                self.by_service
                    .entry(service.to_string())
                    .or_default()
                    .insert(id.to_string());
            }
            _ => {
                self.any.insert(entry.to_string());
            }
        }
    }
}

pub(crate) fn expand_editor_target_settings_ids(
    args: &PushEditorChangesArgs,
) -> Result<EditorTargetIds> {
    let mut ids = EditorTargetIds::default();
    for value in &args.target_settings_ids {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            ids.insert(trimmed);
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
            ids.insert(trimmed);
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
pub(crate) struct VirtualStageFiles {
    stage_root_keys: Vec<String>,
    files: HashMap<String, Vec<u8>>,
    dirs: HashSet<String>,
}

impl VirtualStageFiles {
    fn normalized_root_key(path: &Path) -> String {
        let stripped = crate::system::files::strip_extended_prefix(path.to_path_buf());
        crate::system::files::path_key(&stripped)
            .trim_end_matches('/')
            .to_string()
    }

    fn normalized_rel_key(rel: &Path) -> String {
        crate::system::files::path_key(rel)
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string()
    }

    pub(crate) fn new(stage_root: &Path) -> Self {
        let mut stage_root_keys = Vec::with_capacity(2);
        let raw_key = Self::normalized_root_key(stage_root);
        if !raw_key.is_empty() {
            stage_root_keys.push(raw_key);
        }
        if let Ok(canonical) = crate::system::files::canonical_path(stage_root) {
            let canon_key = Self::normalized_root_key(&canonical);
            if !canon_key.is_empty() && !stage_root_keys.contains(&canon_key) {
                stage_root_keys.push(canon_key);
            }
        }
        Self {
            stage_root_keys,
            files: HashMap::new(),
            dirs: HashSet::new(),
        }
    }

    pub(crate) fn insert_file(&mut self, rel: &Path, bytes: Vec<u8>) {
        self.files.insert(Self::normalized_rel_key(rel), bytes);
    }

    pub(crate) fn insert_dir(&mut self, rel: &Path) {
        self.dirs.insert(Self::normalized_rel_key(rel));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty()
    }

    pub(crate) fn relative_key(&self, path: &Path) -> Option<String> {
        let stripped = crate::system::files::strip_extended_prefix(path.to_path_buf());
        let key = crate::system::files::path_key(&stripped);
        let trimmed = key.trim_end_matches('/');
        for root_key in &self.stage_root_keys {
            if let Some(rest) = trimmed.strip_prefix(root_key.as_str())
                && let Some(after) = rest.strip_prefix('/')
                && !after.is_empty()
            {
                return Some(after.to_string());
            }
        }
        None
    }

    pub(crate) fn is_dir_key(&self, rel_key: &str) -> bool {
        self.dirs.contains(rel_key)
    }

    pub(crate) fn file_bytes_by_key(&self, rel_key: &str) -> Option<&[u8]> {
        self.files.get(rel_key).map(Vec::as_slice)
    }

    pub(crate) fn file_bytes(&self, path: &Path) -> Option<&[u8]> {
        let rel = self.relative_key(path)?;
        self.files.get(&rel).map(Vec::as_slice)
    }

    pub(crate) fn read_to_string(&self, path: &Path) -> Option<anyhow::Result<String>> {
        let bytes = self.file_bytes(path)?;
        Some(
            String::from_utf8(bytes.to_vec())
                .with_context(|| format!("File {} is not valid UTF-8", path.display())),
        )
    }
}

#[cfg(test)]
mod virtual_stage_files_tests {
    use super::*;

    #[test]
    fn virtual_stage_files_matches_extended_prefix_separators_and_dirs() {
        let dir =
            crate::system::files::create_unique_directory(&std::env::temp_dir(), "renium-vf-test-")
                .unwrap();
        let mut vf = VirtualStageFiles::new(&dir);
        vf.insert_dir(Path::new("src/ReplicatedStorage/Sub"));
        vf.insert_file(
            Path::new("src/ReplicatedStorage/Sub/init.server.luau"),
            b"print(1)".to_vec(),
        );
        let joined = dir
            .join("src")
            .join("ReplicatedStorage")
            .join("Sub")
            .join("init.server.luau");
        assert_eq!(vf.file_bytes(&joined), Some(b"print(1)".as_slice()));
        let dir_path = dir.join("src").join("ReplicatedStorage").join("Sub");
        let dir_key = vf.relative_key(&dir_path).expect("dir key");
        assert!(vf.is_dir_key(&dir_key));
        #[cfg(windows)]
        {
            let ext = PathBuf::from(format!(r"\\?\{}", joined.display()));
            assert_eq!(vf.file_bytes(&ext), Some(b"print(1)".as_slice()));
            let upper = PathBuf::from(joined.to_string_lossy().to_uppercase());
            assert_eq!(vf.file_bytes(&upper), Some(b"print(1)".as_slice()));
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

#[derive(Default)]
pub(crate) struct PreparedEditorDocuments {
    pub(crate) documents: HashMap<String, Arc<SettingsBytecode>>,
    pub(crate) native_services: HashSet<String>,
    /// Changed paths that name an instance by its place in a service and are
    /// pushed as that instance and what it holds, as a folder is; no folder
    /// needs to stand there.
    pub(crate) instance_paths: HashSet<String>,
    /// Of those, the instances a file defines in full: what Studio holds
    /// below one and the file does not make is removed with the push.
    pub(crate) whole_instance_paths: HashSet<String>,
    /// Script files and directories kept in memory instead of copied onto the
    /// temporary push stage on disk.
    pub(crate) virtual_files: Option<Arc<VirtualStageFiles>>,
}

#[derive(Clone, Debug)]
pub(crate) struct BulkTreeNode {
    /// 0 for direct child of service root; 1-based index into `nodes` otherwise.
    pub(crate) parent: u32,
    /// 1-based ordinal among siblings with the same `(parent, name)`.
    pub(crate) ordinal: u32,
    /// 0-based index into `EditorBulkPropertyPatch::strings`.
    pub(crate) name_idx: u32,
    /// 0-based index into `EditorBulkPropertyPatch::strings`.
    pub(crate) class_idx: u32,
}

#[derive(Clone, Debug)]
pub(crate) enum BulkPropertyValues {
    F32Const(f32),
    F64Const(f64),
    BoolConst(bool),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Bool(Vec<u8>),
    I32(Vec<i32>),
    Vector3(Vec<[f32; 3]>),
    Color3(Vec<[f32; 3]>),
    CFrame(Vec<[f32; 12]>),
    Indexed {
        palette: Vec<Value>,
        indices: Vec<u32>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BulkColumnKind {
    Property,
    Attribute,
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    AttributesReplace,
    Tag,
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    AssertModelScale,
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    AssertModelPivot,
}

#[derive(Clone, Debug)]
pub(crate) struct BulkPropertyColumn {
    pub(crate) kind: BulkColumnKind,
    pub(crate) property_name: String,
    /// 1-based indices into `EditorBulkPropertyPatch::nodes` (0 = service root).
    pub(crate) rows: Vec<u32>,
    pub(crate) values: BulkPropertyValues,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BulkPackageTarget {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct EditorBulkPropertyPatch {
    pub(crate) service: String,
    pub(crate) strings: Vec<String>,
    pub(crate) nodes: Vec<BulkTreeNode>,
    pub(crate) columns: Vec<BulkPropertyColumn>,
    pub(crate) field_count: usize,
    pub(crate) instance_count: usize,
    pub(crate) expected_terrain_digest: Option<String>,
    pub(crate) return_old_values: bool,
    pub(crate) package_targets: Vec<BulkPackageTarget>,
}

#[derive(Default)]
pub(crate) struct EditorChangeSet {
    pub(crate) instance_changes: Vec<EditorInstanceChange>,
    pub(crate) source_changes: Vec<EditorSourceChange>,
    pub(crate) property_changes: Vec<EditorPropertyChange>,
    pub(crate) bulk_property_patches: Vec<EditorBulkPropertyPatch>,
    pub(crate) history_entries: Vec<EditorHistoryEntry>,
    pub(crate) settings_writes: Vec<EditorSettingsWrite>,
    pub(crate) geometry_readbacks: Vec<super::native_geometry::GeometryReadback>,
    pub(crate) files_to_studio_filters_active: bool,
    // Full native replacements need only serializer omissions as property
    // edits. Keep the accepted document until the binary plan identifies them.
    pub(crate) native_property_documents: HashMap<String, Arc<SettingsBytecode>>,
    /// Engine-set fields of imported instances, written in a follow-up
    /// transaction of the same push once the import has joined its service.
    pub(crate) deferred_native_rows: Vec<EditorPropertyChange>,
    /// Pushed instances, each with its service, that a file defines in full.
    /// The push creates and updates what the file makes; what Studio holds
    /// below such an instance beyond that is found and removed before it runs.
    pub(crate) whole_instances: Vec<(String, EditorInstancePath)>,
    pub(crate) virtual_files: Option<Arc<VirtualStageFiles>>,
    pub(crate) capture_before_terrain: bool,
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
            .chain(
                self.bulk_property_patches
                    .iter()
                    .map(|patch| patch.service.as_str()),
            )
    }
}

pub(crate) fn take_pre_routed_protected_writes(
    changes: &mut EditorChangeSet,
    binary_import: Option<&EditorBinaryImport>,
) -> Vec<Value> {
    let mut rows = Vec::new();
    for change in &mut changes.property_changes {
        let property_name = if change.service == MATERIAL_SERVICE_CLASS
            && change.class_name == MATERIAL_SERVICE_CLASS
            && change.path_segments.len() == 1
            && change.path_segments[0] == MATERIAL_SERVICE_CLASS
        {
            USE_2022_MATERIALS_PROPERTY
        } else if has_protected_texture_pack(&change.class_name) {
            TEXTURE_PACK_PROPERTY
        } else {
            continue;
        };
        if super::native_roots::is_property(&change.class_name, property_name) {
            // Verified transactional setters must reach the plugin's native
            // queue instead of forcing a whole-place snapshot and reopen.
            continue;
        }
        if property_name == TEXTURE_PACK_PROPERTY
            && binary_import.is_some_and(|import| {
                import.imports_path(
                    &change.service,
                    &change.path_segments,
                    &change.path_ordinals,
                ) && !import
                    .post_apply_properties_by_class
                    .get(&change.class_name)
                    .is_some_and(|names| names.contains(property_name))
                    && !import
                        .post_apply_properties_by_path
                        .get(&crate::bytecode::edit::instance_path_parts_key(
                            &change.path_segments,
                            &change.path_ordinals,
                        ))
                        .is_some_and(|names| names.contains(property_name))
            })
        {
            // The staged binary already carries this saved texture pack. Keep
            // the normal native post-apply filter; an offline rewrite would
            // target the old live hierarchy and discard the staged import.
            continue;
        }
        let Some(value) = change.properties.remove(property_name) else {
            continue;
        };
        rows.push(json!({
            "kind": "property",
            "service": &change.service,
            "settingsId": &change.settings_id,
            "pathSegments": &change.path_segments,
            "pathOrdinals": &change.path_ordinals,
            "className": &change.class_name,
            "name": property_name,
            "value": value,
        }));
    }
    changes.property_changes.retain(|change| {
        !change.properties.is_empty()
            || !change.reset_properties.is_empty()
            || !change.attributes.is_empty()
            || !change.deleted_attributes.is_empty()
    });
    rows
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorBinaryExportGroup {
    pub(crate) service: String,
    pub(crate) target_path: Vec<String>,
    pub(crate) count: usize,
    pub(crate) instance_count: usize,
    #[serde(default)]
    pub(crate) identity_carrier_class: String,
    #[serde(default)]
    pub(crate) identity_carrier_prefix: String,
    #[serde(default)]
    pub(crate) identity_carrier_slots: Vec<String>,
    #[serde(default)]
    pub(crate) identity_carrier_count: usize,
    #[serde(default)]
    pub(crate) script_count: usize,
    pub(crate) class_names: Vec<String>,
    #[serde(default)]
    pub(crate) non_archivable_indices: Vec<usize>,
    #[serde(default, deserialize_with = "deserialize_json_object_or_empty_array")]
    pub(crate) root_properties: Map<String, Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorBinaryImportGroup {
    pub(crate) service: String,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) additive: bool,
    pub(crate) target_path: Vec<String>,
    pub(crate) count: usize,
    pub(crate) payload_root_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expected_structure: Option<EditorBinaryStructure>,
    pub(crate) root_paths: Vec<EditorBinaryRootPath>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) root_settings_ids: Vec<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_identity_text"
    )]
    pub(crate) instance_identities: Option<EditorBinaryStructure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) viewport_camera: Option<EditorBinaryRootPath>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) retained_roots: Vec<EditorBinaryRetainedRoot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) package_roots: Vec<EditorBinaryPackageRoot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) mutation_package_roots: Vec<EditorBinaryPackageRoot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) change_generation: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct EditorBinaryStructure {
    pub(crate) strings: Vec<String>,
    pub(crate) nodes: String,
}

/// Identity strings travel as one base64 text joined by NUL with their count,
/// which Studio splits in one call instead of reading a JSON array element by
/// element. A string that holds NUL keeps the array.
fn serialize_identity_text<S: serde::Serializer>(
    identities: &Option<EditorBinaryStructure>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    let Some(identities) = identities else {
        return serializer.serialize_none();
    };
    if identities.strings.iter().any(|text| text.contains('\0')) {
        return identities.serialize(serializer);
    }
    let mut fields = serializer.serialize_struct("EditorBinaryStructure", 3)?;
    fields.serialize_field("stringText", &base64::encode(identities.strings.join("\0")))?;
    fields.serialize_field("stringCount", &identities.strings.len())?;
    fields.serialize_field("nodes", &identities.nodes)?;
    fields.end()
}

#[derive(Clone, Serialize)]
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
    #[cfg(windows)]
    pub(crate) attribute_guard: Option<crate::studio::native::serializer::AttributeGuard>,
    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) bytes: Vec<u8>,
    pub(crate) native_capture: bool,
    pub(crate) groups: Vec<EditorBinaryExportGroup>,
    pub(crate) serialization_batches: Vec<EditorBinarySerializationBatch>,
    pub(crate) export_id: Option<String>,
    pub(crate) property_schema_by_class: PropertySchemaMap,
    pub(crate) enum_value_names_by_type: EnumValueNameMap,
}

#[derive(Debug)]
pub(crate) enum ServiceReaderFallback {
    Disabled,
    Platform,
    PackageRoots,
    DiscoveryFailed(String),
}

#[derive(Clone)]
pub(crate) struct PushSavedFields {
    pub(crate) project: PathBuf,
    pub(crate) runtime: String,
    pub(crate) fields: Vec<crate::settings::equivalence::SavedFieldDifference>,
}

impl PushSavedFields {
    pub(crate) fn validate(&self, project: &std::path::Path, runtime: &str) -> Result<()> {
        anyhow::ensure!(
            self.project == project && self.runtime == runtime,
            "Saved fields belong to a different project or Studio runtime; retry the push in its original place"
        );
        Ok(())
    }
}

impl ServiceReaderFallback {
    pub(crate) fn reason(&self) -> &str {
        match self {
            Self::Disabled => {
                "the native service reader was disabled by RENIUM_NATIVE_SERVICE_IMPORT"
            }
            Self::Platform => "the native service reader is unavailable on this platform",
            Self::PackageRoots => "the import must preserve live package roots",
            Self::DiscoveryFailed(reason) => reason,
        }
    }
}

/// The existing post-import pivot rule, shared by selection and row retention.
pub(crate) const IMPORT_MODEL_PIVOT: (&str, &str) = ("Model", "WorldPivot");

pub(crate) fn import_property_omissions(
    mut omitted: HashMap<String, HashSet<String>>,
    native_reader: bool,
) -> HashMap<String, HashSet<String>> {
    if !native_reader {
        omitted
            .entry(IMPORT_MODEL_PIVOT.0.into())
            .or_default()
            .insert(IMPORT_MODEL_PIVOT.1.into());
        omitted
            .entry("SurfaceAppearance".into())
            .or_default()
            .insert(crate::roblox::schema::TEXTURE_PACK_PROPERTY.into());
    }
    omitted
}

type NativeImportEncodeOutput = anyhow::Result<(Vec<u8>, EditorNativeReplacement, usize)>;

pub(crate) struct PendingNativeImportEncode {
    handle: std::sync::Mutex<Option<std::thread::JoinHandle<NativeImportEncodeOutput>>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PendingNativeImportEncode {
    pub(crate) fn new(
        handle: std::thread::JoinHandle<NativeImportEncodeOutput>,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            handle: std::sync::Mutex::new(Some(handle)),
            cancelled,
        }
    }

    pub(crate) fn finish(mut self) -> NativeImportEncodeOutput {
        let Some(handle) = self
            .handle
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            anyhow::bail!("Native import payload encoder already finished");
        };
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("Native import payload encoder panicked"))?
    }
}

impl Drop for PendingNativeImportEncode {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self
            .handle
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = handle.join();
        }
    }
}

pub(crate) struct EditorBinaryImport {
    /// Host-only decoded rows. The explicit wire payload never includes them.
    pub(crate) kept_rows: HashMap<String, crate::rbx::import_rows::KeptImportRows>,
    pub(crate) bytes: Vec<u8>,
    pub(crate) groups: Vec<EditorBinaryImportGroup>,
    pub(crate) native_replacement: Option<EditorNativeReplacement>,
    pub(crate) pending_encode: Option<PendingNativeImportEncode>,
    /// Why this plan uses Studio's plugin deserializer instead of the service reader.
    pub(crate) fallback_reason: Option<ServiceReaderFallback>,
    pub(crate) instance_count: usize,
    pub(crate) post_apply_properties_by_class: HashMap<String, HashSet<String>>,
    pub(crate) post_apply_properties_by_path: HashMap<String, HashSet<String>>,
    pub(crate) external_references_post_applied: bool,
    pub(crate) viewport_references_post_applied: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorNativeReplacement {
    #[serde(flatten)]
    pub(crate) plan: EditorNativePlan,
    #[serde(skip)]
    pub(crate) batches: Vec<EditorNativeBatch>,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct EditorNativeBatch {
    pub(crate) bytes: std::ops::Range<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorNativePlan {
    pub(crate) bindings: Vec<EditorNativeBinding>,
    pub(crate) classes: Vec<EditorNativeClass>,
    pub(crate) aliases: Vec<EditorNativeAlias>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorNativeAlias {
    pub(crate) class_index: u32,
    pub(crate) ordinal: u32,
    pub(crate) source_ordinal: u32,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorNativeClass {
    pub(crate) name: String,
    pub(crate) count: u32,
    pub(crate) tags_absent: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorNativeBinding {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    pub(crate) binary_referent: i32,
    pub(crate) ordinal: u32,
    pub(crate) class_count: u32,
    pub(crate) reference_only: bool,
}

impl EditorBinaryImport {
    pub(crate) fn is_native_replacement(&self) -> bool {
        self.native_replacement.is_some() || self.pending_encode.is_some()
    }

    pub(crate) fn finish_encode(&mut self) -> anyhow::Result<()> {
        let Some(pending) = self.pending_encode.take() else {
            return Ok(());
        };
        let (bytes, replacement, instance_count) = pending.finish()?;
        self.bytes = bytes;
        self.native_replacement = Some(replacement);
        self.instance_count = instance_count;
        Ok(())
    }

    pub(crate) fn carries_container_settings(&self, service: &str, segments: &[String]) -> bool {
        self.is_native_replacement()
            && self.groups.iter().any(|group| {
                !group.additive && group.service == service && group.target_path == segments
            })
    }

    /// Whether the path lies in a package root the import creates or retains.
    pub(crate) fn imports_package_path(&self, service: &str, segments: &[String]) -> bool {
        self.groups.iter().any(|group| {
            group.service == service
                && group
                    .package_roots
                    .iter()
                    .chain(&group.mutation_package_roots)
                    .any(|root| segments.starts_with(&root.path_segments))
        })
    }

    pub(crate) fn imports_service(&self, service: &str) -> bool {
        self.groups
            .iter()
            .any(|group| group.service == service && !group.additive)
    }

    pub(crate) fn imports_path(
        &self,
        service: &str,
        segments: &[String],
        ordinals: &[usize],
    ) -> bool {
        self.groups.iter().any(|group| {
            group.service == service
                && (!group.additive
                    || group.root_paths.iter().any(|root| {
                        segments.starts_with(&root.path_segments)
                            && ordinals.starts_with(&root.path_ordinals)
                    }))
        })
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
                root.payload_omitted
                    && segments.starts_with(&root.path_segments)
                    && ordinals.starts_with(&root.path_ordinals)
            })
    }
}

pub(crate) struct EditorSettingsWrite {
    pub(crate) path: PathBuf,
    pub(crate) expected_hash: Option<[u8; 32]>,
    /// Shared with the collection that wrote it until one of them changes it.
    pub(crate) document: Arc<SettingsBytecode>,
    /// The store as it is written, where the write changed values of a few
    /// instances of the file: `document` then holds those instances whole
    /// and every other one bare.
    pub(crate) encoded: Option<Vec<u8>>,
}

impl EditorSettingsWrite {
    /// The bytes of the store this write publishes.
    pub(crate) fn store_bytes(&self) -> anyhow::Result<std::borrow::Cow<'_, [u8]>> {
        Ok(match &self.encoded {
            Some(bytes) => std::borrow::Cow::Borrowed(bytes),
            None => std::borrow::Cow::Owned(crate::settings::bytecode::encode_settings_bytecode(
                &self.document,
            )?),
        })
    }
}

pub(crate) struct EditorHistoryEntry {
    pub(crate) service: String,
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) settings_id: Option<String>,
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) class_name: String,
    pub(crate) source_key: Option<String>,
    /// The bytes of the store as undoing the change reads it from before the
    /// change: entries share one copy where undoing each reads the same from
    /// it.
    pub(crate) settings_before: Option<Arc<Vec<u8>>>,
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
