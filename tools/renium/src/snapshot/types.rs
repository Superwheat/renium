use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use rbx_dom_weak::Ustr as RbxUstr;
use rbx_reflection::ReflectionDatabase;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::rbx::decode::NativePropertyFilter;
use crate::roblox::schema::{EnumValueNameMap, PropertySchemaMap};
use crate::studio::bridge::{BridgeServer, ChunkFetchMetrics};
use crate::studio::native::editor::NativeServiceDom;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotManifest {
    pub(crate) instances: Vec<SnapshotInstance>,
    pub(crate) class_defaults: Value,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotInstance {
    pub(crate) path: String,
    pub(crate) path_segments: Vec<String>,
    pub(crate) name: String,
    pub(crate) class_name: RbxUstr,
    pub(crate) properties: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_key: Option<String>,
    pub(crate) parent_path: Option<String>,
    pub(crate) attributes: Map<String, Value>,
    pub(crate) debug_id: Option<String>,
    pub(crate) parent_debug_id: Option<String>,
    pub(crate) instance_id: Option<String>,
    pub(crate) parent_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instance_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_index: Option<usize>,
}

#[derive(Clone)]
pub(crate) struct NativeSettingsProperty {
    pub(crate) name: String,
    pub(crate) value: NativeSettingsValue,
}

#[derive(Clone)]
pub(crate) enum NativeSettingsValue {
    Bool(bool),
    Int(i64),
    Float32(f32),
    Float64(f64),
    String(String),
    Ref(usize),
    Vector2([f32; 2]),
    Vector3([f32; 3]),
    UDim([f32; 2]),
    UDim2([f32; 4]),
    Color3([f32; 3]),
    CFrame([f32; 12]),
    Rect([f32; 4]),
    Enum(String),
}
#[derive(Clone)]
pub(crate) struct ServiceState {
    pub(crate) instances: Vec<SnapshotInstance>,
    pub(crate) native_properties_by_instance: Option<Vec<Vec<NativeSettingsProperty>>>,
    pub(crate) children_by_index: Vec<Vec<usize>>,
    pub(crate) source_in_subtree: Vec<bool>,
    pub(crate) script_count_in_subtree: Vec<usize>,
    pub(crate) subtree_sizes: Vec<usize>,
    pub(crate) service_root_index: usize,
    pub(crate) class_defaults_by_class: HashMap<String, Map<String, Value>>,
    pub(crate) properties_default_elided: bool,
    pub(crate) dense_index_topology: bool,
}

pub(crate) struct ExportedSnapshotParts {
    pub(crate) class_defaults: Value,
    pub(crate) instances: Vec<SnapshotInstance>,
    pub(crate) native_properties_by_instance: Option<Vec<Vec<NativeSettingsProperty>>>,
}

pub(crate) struct ServiceExecutionSpan {
    pub(crate) service: String,
    pub(crate) export_start_ms: f64,
    pub(crate) export_end_ms: f64,
}

pub(crate) struct ServiceExportOutput {
    pub(crate) parts: ExportedSnapshotParts,
    pub(crate) span: ServiceExecutionSpan,
    pub(crate) tune: Option<AdaptiveTuneEntry>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdaptiveTuneCache {
    pub(crate) version: u32,
    pub(crate) cache_key: String,
    pub(crate) services: HashMap<String, AdaptiveTuneEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdaptiveTuneEntry {
    pub(crate) batch_size: usize,
    pub(crate) workers: usize,
    pub(crate) instance_count: usize,
    pub(crate) frame_ms: Option<f64>,
    pub(crate) max_frame_ms: Option<f64>,
    pub(crate) wave_ms: Option<f64>,
    pub(crate) payload_bytes: usize,
    pub(crate) request_count: usize,
    pub(crate) items_fetched: usize,
    pub(crate) stall_count_over_50_ms: u64,
    pub(crate) updated_at_unix: i64,
}

pub(crate) struct InstanceFetchResult {
    pub(crate) instances: Vec<SnapshotInstance>,
    pub(crate) tune: Option<AdaptiveTuneEntry>,
}

pub(crate) struct InstanceBatchFetch {
    pub(crate) total_hint: usize,
    pub(crate) metrics: ChunkFetchMetrics,
    pub(crate) compact_expand_ms: f64,
    pub(crate) request_ms: f64,
    pub(crate) items: Vec<SnapshotInstance>,
}

pub(crate) struct NativeOverlayFetch {
    pub(crate) metrics: ChunkFetchMetrics,
    pub(crate) compact_expand_ms: f64,
    pub(crate) request_ms: f64,
    pub(crate) debug_ids: Vec<Option<String>>,
    pub(crate) items: Vec<NativeOverlayItem>,
}

pub(crate) struct NativeConditionalOverlayFetch {
    pub(crate) candidate_count: usize,
    pub(crate) overlay: NativeOverlayFetch,
}

pub(crate) type NativeConditionalOverlayRequest = (PropertySchemaMap, Value, usize);
pub(crate) type NativeServiceFetch = (
    NativeServiceDom,
    bool,
    Option<NativeConditionalOverlayRequest>,
);
pub(crate) type NativeConditionalOverlayReceiver =
    mpsc::Receiver<Result<Option<NativeConditionalOverlayFetch>>>;

pub(crate) struct NativeServiceFinishDependencies<'a, 'db> {
    pub(crate) bridge: &'a BridgeServer,
    pub(crate) export_id: &'a str,
    pub(crate) enum_value_names_by_type: &'a EnumValueNameMap,
    pub(crate) database: &'a ReflectionDatabase<'db>,
    pub(crate) native_filters: &'a HashMap<String, NativePropertyFilter>,
    pub(crate) run_started: Instant,
}

pub(crate) struct NativeServiceFinishInput {
    pub(crate) native: NativeServiceDom,
    pub(crate) debug_ids: Vec<Option<String>>,
    pub(crate) overlay: NativeOverlayFetch,
    pub(crate) reference_prefetch: Option<NativeConditionalOverlayReceiver>,
    pub(crate) reference_request: Option<NativeConditionalOverlayRequest>,
    pub(crate) export_started_ms: f64,
}

pub(crate) struct NativeOverlayItem {
    pub(crate) instance_index: usize,
    pub(crate) class_index: usize,
    pub(crate) properties: Map<String, Value>,
    pub(crate) attributes: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CompactBatchPayload {
    pub(crate) format: String,
    pub(crate) codec_version: String,
    pub(crate) total: usize,
    pub(crate) strings: Vec<String>,
    #[serde(default)]
    pub(crate) shapes: Vec<Value>,
    #[serde(default)]
    pub(crate) debug_ids: Vec<Value>,
    pub(crate) items: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeOverlayPayload {
    pub(crate) format: String,
    pub(crate) codec_version: String,
    pub(crate) total: usize,
    pub(crate) strings: Vec<String>,
    #[serde(default)]
    pub(crate) debug_id_buffer: Value,
    #[serde(default)]
    pub(crate) debug_id_encoding: String,
    #[serde(default)]
    pub(crate) debug_id_buffer_bytes: usize,
    pub(crate) items: Vec<Value>,
}
