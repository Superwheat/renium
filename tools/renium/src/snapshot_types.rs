use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use rbx_dom_weak::Ustr as RbxUstr;
use rbx_reflection::ReflectionDatabase;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::bridge_server::{BridgeServer, ChunkFetchMetrics};
use super::native_editor::NativeServiceDom;
use super::property_schema::{EnumValueNameMap, PropertySchemaMap};
use super::rbx_decode::NativePropertyFilter;

#[derive(Debug, Default, Deserialize)]
pub(super) struct SnapshotManifest {
    #[serde(default)]
    pub(super) instances: Vec<SnapshotInstance>,
    #[serde(default, rename = "instanceChunks")]
    pub(super) instance_chunks: Vec<InstanceChunkEntry>,
    #[serde(default)]
    pub(super) services: Vec<SnapshotServiceRef>,
    #[serde(default, rename = "classDefaults")]
    pub(super) class_defaults: Value,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SnapshotServiceRef {
    #[serde(default)]
    pub(super) path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum InstanceChunkEntry {
    FileName(String),
    Entry { file: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct SnapshotInstance {
    pub(super) path: String,
    #[serde(default, rename = "pathSegments")]
    pub(super) path_segments: Vec<String>,
    pub(super) name: String,
    #[serde(rename = "className")]
    pub(super) class_name: RbxUstr,
    #[serde(default)]
    pub(super) properties: Map<String, Value>,
    #[serde(default, rename = "sourceKey", skip_serializing_if = "Option::is_none")]
    pub(super) source_key: Option<String>,
    #[serde(default, rename = "parentPath")]
    pub(super) parent_path: Option<String>,
    #[serde(default)]
    pub(super) attributes: Map<String, Value>,
    #[serde(default, rename = "debugId")]
    pub(super) debug_id: Option<String>,
    #[serde(default, rename = "parentDebugId")]
    pub(super) parent_debug_id: Option<String>,
    #[serde(default, rename = "instanceId")]
    pub(super) instance_id: Option<String>,
    #[serde(default, rename = "parentInstanceId")]
    pub(super) parent_instance_id: Option<String>,
    #[serde(
        default,
        rename = "instanceIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) instance_index: Option<usize>,
    #[serde(
        default,
        rename = "parentIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub(super) parent_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct NativeSettingsProperty {
    pub(super) name: String,
    pub(super) value: NativeSettingsValue,
}

#[derive(Debug, Clone)]
pub(super) enum NativeSettingsValue {
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

#[derive(Debug, Clone)]
pub(super) struct ServiceState {
    pub(super) instances: Vec<SnapshotInstance>,
    pub(super) native_properties_by_instance: Option<Vec<Vec<NativeSettingsProperty>>>,
    pub(super) children_by_index: Vec<Vec<usize>>,
    pub(super) source_in_subtree: Vec<bool>,
    pub(super) script_count_in_subtree: Vec<usize>,
    pub(super) subtree_sizes: Vec<usize>,
    pub(super) service_root_index: usize,
    pub(super) class_defaults_by_class: HashMap<String, Map<String, Value>>,
    pub(super) properties_default_elided: bool,
    pub(super) dense_index_topology: bool,
}

pub(super) struct ExportedSnapshotParts {
    pub(super) service_name: String,
    pub(super) service_class: String,
    pub(super) service_path: String,
    pub(super) generated_at: i64,
    pub(super) script_count: usize,
    pub(super) class_defaults: Value,
    pub(super) instances: Vec<SnapshotInstance>,
    pub(super) native_properties_by_instance: Option<Vec<Vec<NativeSettingsProperty>>>,
    pub(super) adaptive_tune: Option<AdaptiveTuneEntry>,
}

pub(super) struct ServiceExecutionSpan {
    pub(super) service: String,
    pub(super) export_start_ms: f64,
    pub(super) export_end_ms: f64,
    pub(super) export_duration_ms: f64,
}

pub(super) struct ServiceExportOutput {
    pub(super) parts: ExportedSnapshotParts,
    pub(super) span: ServiceExecutionSpan,
    pub(super) tune: Option<AdaptiveTuneEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct AdaptiveTuneCache {
    #[serde(default)]
    pub(super) version: u32,
    #[serde(default, rename = "cacheKey")]
    pub(super) cache_key: String,
    #[serde(default)]
    pub(super) services: HashMap<String, AdaptiveTuneEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AdaptiveTuneEntry {
    #[serde(rename = "batchSize")]
    pub(super) batch_size: usize,
    pub(super) workers: usize,
    #[serde(default, rename = "instanceCount")]
    pub(super) instance_count: usize,
    #[serde(default, rename = "frameMs")]
    pub(super) frame_ms: Option<f64>,
    #[serde(default, rename = "maxFrameMs")]
    pub(super) max_frame_ms: Option<f64>,
    #[serde(default, rename = "waveMs")]
    pub(super) wave_ms: Option<f64>,
    #[serde(default, rename = "avgReqMs")]
    pub(super) avg_req_ms: Option<f64>,
    #[serde(default, rename = "maxReqMs")]
    pub(super) max_req_ms: Option<f64>,
    #[serde(default, rename = "avgReqBytes")]
    pub(super) avg_req_bytes: usize,
    #[serde(default, rename = "maxReqBytes")]
    pub(super) max_req_bytes: usize,
    #[serde(default, rename = "payloadBytes")]
    pub(super) payload_bytes: usize,
    #[serde(default, rename = "chunkCount")]
    pub(super) chunk_count: usize,
    #[serde(default, rename = "requestCount")]
    pub(super) request_count: usize,
    #[serde(default, rename = "itemsFetched")]
    pub(super) items_fetched: usize,
    #[serde(default, rename = "stallCountOver50Ms")]
    pub(super) stall_count_over_50_ms: u64,
    #[serde(default, rename = "updatedAtUnix")]
    pub(super) updated_at_unix: i64,
}

pub(super) struct InstanceFetchResult {
    pub(super) instances: Vec<SnapshotInstance>,
    pub(super) tune: Option<AdaptiveTuneEntry>,
}

pub(super) struct InstanceBatchFetch {
    pub(super) total_hint: usize,
    pub(super) metrics: ChunkFetchMetrics,
    pub(super) compact_expand_ms: f64,
    pub(super) request_ms: f64,
    pub(super) items: Vec<SnapshotInstance>,
}

pub(super) struct NativeOverlayFetch {
    pub(super) metrics: ChunkFetchMetrics,
    pub(super) compact_expand_ms: f64,
    pub(super) request_ms: f64,
    pub(super) debug_ids: Vec<Option<String>>,
    pub(super) items: Vec<NativeOverlayItem>,
}

pub(super) struct NativeConditionalOverlayFetch {
    pub(super) candidate_count: usize,
    pub(super) overlay: NativeOverlayFetch,
}

pub(super) type NativeConditionalOverlayRequest = (PropertySchemaMap, Value, usize);
pub(super) type NativeServiceFetch = (
    NativeServiceDom,
    bool,
    Option<NativeConditionalOverlayRequest>,
);
pub(super) type NativeConditionalOverlayReceiver =
    mpsc::Receiver<Result<Option<NativeConditionalOverlayFetch>>>;

pub(super) struct NativeServiceFinishDependencies<'a, 'db> {
    pub(super) bridge: &'a BridgeServer,
    pub(super) export_id: &'a str,
    pub(super) enum_value_names_by_type: &'a EnumValueNameMap,
    pub(super) database: &'a ReflectionDatabase<'db>,
    pub(super) native_filters: &'a HashMap<String, NativePropertyFilter>,
    pub(super) run_started: Instant,
}

pub(super) struct NativeServiceFinishInput {
    pub(super) native: NativeServiceDom,
    pub(super) debug_ids: Vec<Option<String>>,
    pub(super) overlay: NativeOverlayFetch,
    pub(super) reference_prefetch: Option<NativeConditionalOverlayReceiver>,
    pub(super) reference_request: Option<NativeConditionalOverlayRequest>,
    pub(super) export_started_ms: f64,
}

#[derive(Default)]
pub(super) struct NativeOverlayItem {
    pub(super) instance_index: usize,
    pub(super) class_index: usize,
    pub(super) properties: Map<String, Value>,
    pub(super) attributes: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompactBatchPayload {
    pub(super) format: String,
    #[serde(rename = "codecVersion")]
    pub(super) codec_version: String,
    pub(super) total: usize,
    pub(super) strings: Vec<String>,
    #[serde(default)]
    pub(super) shapes: Vec<Value>,
    #[serde(default, rename = "debugIds")]
    pub(super) debug_ids: Vec<Value>,
    pub(super) items: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeOverlayPayload {
    pub(super) format: String,
    #[serde(rename = "codecVersion")]
    pub(super) codec_version: String,
    pub(super) total: usize,
    pub(super) strings: Vec<String>,
    #[serde(default, rename = "debugIdBuffer")]
    pub(super) debug_id_buffer: Value,
    #[serde(default, rename = "debugIdEncoding")]
    pub(super) debug_id_encoding: String,
    #[serde(default, rename = "debugIdBufferBytes")]
    pub(super) debug_id_buffer_bytes: usize,
    pub(super) items: Vec<Value>,
}
