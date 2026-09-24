use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use ahash::AHashMap;
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::BoundContext;
use super::context as bound_context;
use super::runtime::{acknowledge_pulled_changes, automation_pull_args, automation_push_args};
use crate::app::output::{ensure_plugin_api_ok, global_log_enabled, log_global};
use crate::app::timing::elapsed_ms;
use crate::cli::PushEditorChangesArgs;
use crate::editor::diff::editor_instance_descriptor_for_known_path;
use crate::editor::document::is_protected_engine_container;
use crate::editor::paths::{
    build_editor_instance_paths_for_indices, build_editor_source_paths_by_index,
};
use crate::editor::review::{is_externally_managed_editor_property, local_place_path_for_runtime};
use crate::editor::sync::{
    StudioChangeGuard, expand_editor_changed_paths, is_lua_source_class,
    push_reconciled_editor_changes_with_warm_bridge, settings_file_hash,
};
use crate::editor::types::{
    EditorChangeSet, EditorInstanceChange, EditorInstancePath, EditorPropertyChange,
    PreparedEditorDocuments,
};
use crate::project::version_control::{VcMergeConflict, merge_aligned_settings_documents};
use crate::project::{config, config::project_watch_inputs};
use crate::roblox::services::DEFAULT_SYNC_SERVICES;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, decode_settings_bytecode,
    encode_settings_bytecode,
};
use crate::settings::equivalence::{
    align_equivalent_values, align_reconciliation_protected_workspace_cameras,
    align_settings_ids_to_reference, canonicalize_settings_property_names, drop_settings_document,
    drop_settings_documents, is_reconciliation_protected_workspace_camera, persistent_identity,
    persistent_identity_index, reconciliation_maps_equal, reconciliation_property_is_derived,
    reconciliation_property_value, reconciliation_property_values_equal,
    reconciliation_values_equal, reconciliation_values_map_equal,
    remove_reconciliation_derived_properties, settings_documents_equivalent,
    settings_documents_positionally_equivalent, stabilize_settings_reference_ids,
};
use crate::snapshot::export::{
    ExportProjectStage, PublishEntryState, capture_exported_services,
    export_snapshots_with_warm_bridge,
};
use crate::snapshot::import::service_projection_in_memory;
use crate::snapshot::refs::remap_record_reference_ids;
use crate::studio::bridge::{BridgeServer, BridgeTarget};
use crate::system::files::{
    OnDrop, absolutize_under, atomic_write_file, canonical_path, create_unique_directory, fnv1a,
    is_service_settings_file_name, service_settings_path,
};

pub(crate) mod history;
mod store;

mod coordinator;
#[cfg(test)]
mod escape_tests;
mod guards;
mod identity;
mod merge;
mod pairing;
mod push_flow;
mod push_plan;
mod replacement;
mod staging;
#[cfg(test)]
mod tests;
mod types;
pub(crate) mod verify;

pub(crate) use coordinator::*;
pub(crate) use guards::*;
pub(crate) use identity::*;
pub(crate) use merge::*;
pub(crate) use pairing::*;
pub(crate) use push_flow::*;
pub(crate) use push_plan::*;
pub(crate) use replacement::*;
pub(crate) use staging::*;
pub(crate) use types::*;
pub(crate) use verify::*;

use crate::system::LockRecover;
use store::StoredSnapshot;

const RECORD_VERSION: u8 = 2;
const RECORD_DIR: &str = "reconcile";

fn log_reconcile_timing(label: &str, started: Instant) {
    log_global(
        4,
        format_args!("[renium] reconcile {label}: {:.1}ms", elapsed_ms(started)),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PairMode {
    Reconcile,
    Verify,
}

impl PairMode {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        if matches!(value, Some("none" | "verify")) {
            Self::Verify
        } else {
            Self::Reconcile
        }
    }

    pub(crate) fn writes(self) -> bool {
        self == Self::Reconcile
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Verify => "verify",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ConflictPreference {
    None,
    Studio,
    Editor,
}

impl ConflictPreference {
    pub(crate) fn parse(value: Option<&str>) -> Self {
        match value {
            Some("studio") => Self::Studio,
            Some("editor") => Self::Editor,
            _ => Self::None,
        }
    }
}

pub(crate) struct PairConfiguration {
    pub(crate) mode: PairMode,
    pub(crate) conflict_preference: ConflictPreference,
    pub(crate) resolution_preference: Option<ConflictPreference>,
    pub(crate) runtime_settings: Map<String, Value>,
}

#[derive(Clone, Copy)]
pub(crate) enum BaselineSide {
    Editor,
    Studio,
}
