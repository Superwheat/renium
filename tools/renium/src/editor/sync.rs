use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::app::output::{
    global_log_enabled, global_pretty_output, global_yes, log_global, print_json_output,
};
use crate::app::timing::{current_millis, elapsed_ms, log_timing, verbose_timing_logs};
use crate::automation::op;
use crate::bytecode::{SettingsFileLock, acquire_settings_file_lock};
use crate::cli::{
    ApplyEditorDeleteArgs, ApplyEditorPropertyArgs, BridgeConnectionArgs, EditorMutationArgs,
    PushEditorChangesArgs,
};
use crate::daemon::try_daemon_control_request;
use crate::editor::diff::{
    EditorTargetChangeOptions, append_editor_instance_reconcile, append_editor_target_changes,
    editor_instance_descriptor_for_known_path,
};
use crate::editor::document::{
    document_instance_index_by_settings_id, ensure_editor_source_target_in_bytecode,
    read_editor_service_settings, read_editor_service_settings_cached,
};
use crate::editor::history::save_editor_history_entries;
use crate::editor::paths::{
    build_editor_instance_paths, editor_directory_target, editor_run_context_value,
    editor_source_target_with_children, infer_editor_source_path_spec, infer_source_script,
    service_from_changed_path,
};
use crate::editor::review::{
    apply_protected_writes_offline, is_externally_managed_editor_property,
    is_externally_managed_protected_write, is_user_facing_protected_write,
    local_place_path_for_bridge, normalize_editor_bridge_value,
    protected_root_write_rows_with_live_values, protected_write_matches_previous,
    protected_write_rows_with_previous_values, request_editor_push_review,
    request_protected_write_review, studio_pid_for_bridge,
};
use crate::editor::types::{
    EditorBinaryImport, EditorChangeSet, EditorHistoryEntry, EditorInstanceChange,
    EditorInstanceDescriptor, EditorPreserveDescriptor, EditorPropertyChange, EditorPropertyFilter,
    EditorSettingsWrite, EditorSourceChange, EditorSourceTarget, take_pre_routed_protected_writes,
};
use crate::project::config;
use crate::project::layout::apply_configured_project_layout;
use crate::project::package_links::LinkEnforcement;
use crate::project::package_links::{
    apply_link_enforcement_to_changed_paths, build_link_enforcement,
    build_loaded_project_link_enforcement, package_target_fingerprint_with_external_sources,
};
use crate::rbx::decode::rbx_reflection_class_is_a;
use crate::rbx::encode::rbx_model_property_descriptor;
use crate::roblox::schema::{PropertySchemaMap, load_rbx_dom_property_schema};
use crate::settings::bytecode::{SettingsBytecode, is_reference_object, settings_reference_index};
use crate::settings::equivalence::drop_settings_document;
use crate::settings::instance::remove_instances_at_indices;
use crate::settings::tree::settings_children_by_parent;
use crate::snapshot::export::parse_bridge_ports;
#[cfg(any(windows, target_os = "macos"))]
use crate::studio::bridge::BridgeTarget;
use crate::studio::bridge::{
    BridgeRequestTooLarge, BridgeServer, MAX_BRIDGE_CHUNK_BYTES, MAX_BRIDGE_REQUEST_BYTES,
};
use crate::studio::native::editor::{
    property_change_needs_post_native_apply, send_editor_change_batches,
};
use crate::studio::native::import::{build_editor_binary_import, prepare_native_editor_full_push};
use crate::system::files::{
    absolutize_under, canonical_path, fnv1a_hex, is_service_settings_file_name, path_key,
    service_settings_path, strip_extended_prefix,
};
use crate::system::text::normalized_source_bytes;

struct EditorTransaction<'a> {
    bridge: &'a BridgeServer,
    id: String,
    active: bool,
    package_mutation: bool,
    package_dialog: Option<crate::studio::input::PackageChangesDialogWatcher>,
    auto_desynced_packages: Vec<String>,
    auto_desynced_package_targets: Vec<EditorPackageTarget>,
    package_runtime: Option<(u32, String)>,
    auto_desync_confirmed: bool,
}

#[derive(Clone)]
pub(crate) struct StudioChangeGuard {
    pub(crate) runtime_id: String,
    pub(crate) change_seq: u64,
    pub(crate) tracking_guard_id: Option<String>,
    pub(crate) runtime_bootstrap_safe: bool,
    pub(crate) service_generations: BTreeMap<String, u64>,
}

#[derive(Debug)]
pub(crate) struct StudioChangedBeforePush;

impl std::fmt::Display for StudioChangedBeforePush {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Studio changed while its filesystem update was being prepared")
    }
}

impl std::error::Error for StudioChangedBeforePush {}

#[derive(Default)]
struct EditorCommitStatus {
    package_mutation: bool,
    package_dialog_accepted: bool,
    auto_desynced_packages: Vec<String>,
    auto_desync_confirmed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditorPackageTarget {
    pub(crate) path_segments: Vec<String>,
    pub(crate) path_ordinals: Vec<usize>,
    pub(crate) expected_version: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditorMutationPackages {
    packages: Vec<EditorPackageTarget>,
}

fn package_root_property_is_override(class_name: &str, property_name: &str) -> bool {
    if property_name == "Name" {
        return true;
    }
    let Ok(database) = rbx_reflection_database::get() else {
        return false;
    };
    (rbx_reflection_class_is_a(database, class_name, "Model") && property_name == "WorldPivot")
        || (rbx_reflection_class_is_a(database, class_name, "BasePart")
            && matches!(property_name, "CFrame" | "Position" | "Orientation"))
        || (rbx_reflection_class_is_a(database, class_name, "GuiObject")
            && matches!(property_name, "Position" | "Rotation"))
        || (matches!(class_name, "ScreenGui" | "SurfaceGui" | "BillboardGui")
            && property_name == "Enabled")
}

fn property_change_can_modify_package_root(change: &EditorPropertyChange) -> bool {
    change
        .properties
        .keys()
        .chain(&change.reset_properties)
        .any(|name| !package_root_property_is_override(&change.class_name, name))
}

fn editor_transaction_state(result: &Value) -> Option<&str> {
    result.get("state").and_then(Value::as_str)
}

fn editor_mutation_package_targets(
    changes: &EditorChangeSet,
    binary_import: Option<&EditorBinaryImport>,
) -> Vec<Value> {
    let native_services = binary_import
        .into_iter()
        .flat_map(|import| import.groups.iter().map(|group| group.service.as_str()))
        .collect::<BTreeSet<_>>();
    let mut keys = BTreeSet::new();
    let mut targets = Vec::new();
    let mut add =
        |service: &str, path: &[String], ordinals: &[usize], include_self: bool, kind: &str| {
            let service_prefixed = path.first().is_some_and(|segment| segment == service);
            let path = if service_prefixed {
                path.to_vec()
            } else {
                std::iter::once(service.to_string())
                    .chain(path.iter().cloned())
                    .collect()
            };
            let ordinals = if ordinals.is_empty() || service_prefixed {
                ordinals.to_vec()
            } else {
                std::iter::once(1).chain(ordinals.iter().copied()).collect()
            };
            if path.len() < 2 {
                return;
            }
            let key = (
                service.to_string(),
                path.clone(),
                ordinals.clone(),
                include_self,
                kind.to_string(),
            );
            if keys.insert(key) {
                targets.push(json!({
                    "service": service,
                    "pathSegments": path,
                    "pathOrdinals": ordinals,
                    "includeSelf": include_self,
                    "kind": kind,
                }));
            }
        };
    for change in &changes.source_changes {
        if change.class_name != "PackageLink" {
            add(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
                true,
                "source",
            );
        }
    }
    for change in &changes.property_changes {
        if change.class_name != "PackageLink" {
            add(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
                property_change_can_modify_package_root(change),
                "property",
            );
        }
    }
    for change in &changes.instance_changes {
        if native_services.contains(change.service.as_str()) {
            continue;
        }
        for instance in &change.instances {
            if instance.anchor_only || instance.class_name == "PackageLink" {
                continue;
            }
            add(
                &change.service,
                &instance.path_segments,
                &instance.path_ordinals,
                false,
                "instance",
            );
            if !instance.previous_path_segments.is_empty() {
                add(
                    &change.service,
                    &instance.previous_path_segments,
                    &instance.previous_path_ordinals,
                    false,
                    "instance",
                );
            }
        }
    }
    if let Some(binary_import) = binary_import {
        for group in &binary_import.groups {
            for package_root in &group.mutation_package_roots {
                add(
                    &group.service,
                    &package_root.path_segments,
                    &package_root.path_ordinals,
                    true,
                    "binary",
                );
            }
        }
    }
    targets
}

#[cfg(any(windows, target_os = "macos"))]
fn discover_editor_mutation_packages_with_timeout(
    bridge: &BridgeServer,
    targets: &[Value],
    runtime_id: Option<&str>,
    timeout: Option<Duration>,
) -> Result<Vec<EditorPackageTarget>> {
    let mut packages = BTreeMap::new();
    for chunk in targets.chunks(256) {
        let params = json!({ "targets": chunk });
        let value = if let Some(runtime_id) = runtime_id {
            bridge.call_for_runtime_with_timeout(
                "getEditorMutationPackages",
                params,
                BridgeTarget::Edit,
                runtime_id,
                timeout,
            )?
        } else {
            bridge.call_for_selector_with_timeout(
                "getEditorMutationPackages",
                params,
                BridgeTarget::Edit,
                None,
                timeout,
            )?
        };
        let result: EditorMutationPackages = serde_json::from_value(value)
            .context("Studio returned invalid mutation package targets")?;
        for package in result.packages {
            if package.expected_version <= 0 {
                bail!("Studio returned an invalid package version");
            }
            let key = (package.path_segments.clone(), package.path_ordinals.clone());
            if let Some(previous) = packages.insert(key, package.clone())
                && previous.expected_version != package.expected_version
            {
                bail!("Package changed while Renium was resolving the mutation; retry");
            }
        }
    }
    Ok(packages.into_values().collect())
}

#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn resolve_editor_package_target(
    bridge: &BridgeServer,
    path_segments: &[String],
    path_ordinals: &[usize],
    runtime_id: &str,
    timeout: Duration,
) -> Result<EditorPackageTarget> {
    let service = path_segments
        .first()
        .context("Package target must include a service")?;
    let targets = [json!({
        "service": service,
        "pathSegments": path_segments,
        "pathOrdinals": path_ordinals,
        "includeSelf": true,
    })];
    let mut packages = discover_editor_mutation_packages_with_timeout(
        bridge,
        &targets,
        Some(runtime_id),
        Some(timeout),
    )?;
    packages.retain(|package| {
        package.path_segments == path_segments
            && (path_ordinals.is_empty() || package.path_ordinals == path_ordinals)
    });
    match packages.len() {
        1 => Ok(packages.pop().expect("one package target remains")),
        0 => bail!("Target is not a Roblox package root"),
        count => bail!("Package target resolved to {count} roots; add --ords"),
    }
}

impl<'a> EditorTransaction<'a> {
    fn parameters(
        changes: &EditorChangeSet,
        binary_import: Option<&EditorBinaryImport>,
        id: &str,
        services: Vec<String>,
        guard: Option<&StudioChangeGuard>,
    ) -> Value {
        let native_import = binary_import.is_some();
        let native_import_services = binary_import
            .into_iter()
            .flat_map(|import| import.groups.iter().map(|group| &group.service))
            .collect::<BTreeSet<_>>();
        let source_changes = changes
            .source_changes
            .iter()
            .map(|change| {
                json!({
                    "service": &change.service,
                    "settingsId": &change.settings_id,
                    "pathSegments": &change.path_segments,
                    "pathOrdinals": &change.path_ordinals,
                    "className": &change.class_name,
                    "deleted": change.deleted,
                })
            })
            .collect::<Vec<_>>();
        let mut mutation_root_keys = BTreeSet::new();
        let mut mutation_roots = Vec::new();
        let retained_by_native_import = |service: &str, path: &[String], ordinals: &[usize]| {
            binary_import.is_some_and(|import| import.retains_path(service, path, ordinals))
        };
        let mut add_mutation_root = |service: &str, path: &[String], ordinals: &[usize]| {
            if path.len() < 2 {
                return;
            }
            let ordinal = ordinals.get(1).copied().unwrap_or(1);
            if mutation_root_keys.insert((service.to_string(), path[1].clone(), ordinal)) {
                mutation_roots.push(json!({
                    "service": service,
                    "pathSegments": [&path[0], &path[1]],
                    "pathOrdinals": [ordinals.first().copied().unwrap_or(1), ordinal],
                }));
            }
        };
        for change in &changes.source_changes {
            if retained_by_native_import(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            ) {
                continue;
            }
            add_mutation_root(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            );
        }
        for change in &changes.property_changes {
            if retained_by_native_import(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            ) {
                continue;
            }
            add_mutation_root(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            );
        }
        for change in &changes.instance_changes {
            for instance in &change.instances {
                if instance.anchor_only
                    || retained_by_native_import(
                        &change.service,
                        &instance.path_segments,
                        &instance.path_ordinals,
                    )
                {
                    continue;
                }
                add_mutation_root(
                    &change.service,
                    &instance.path_segments,
                    &instance.path_ordinals,
                );
            }
        }
        if let Some(binary_import) = binary_import {
            for group in &binary_import.groups {
                for package_root in &group.package_roots {
                    let retained = group.retained_roots.iter().any(|root| {
                        root.payload_omitted
                            && root.path_segments == package_root.path_segments
                            && root.path_ordinals == package_root.path_ordinals
                    });
                    if !retained {
                        add_mutation_root(
                            &group.service,
                            &package_root.path_segments,
                            &package_root.path_ordinals,
                        );
                    }
                }
            }
        }
        let has_instance_changes = !changes.instance_changes.is_empty();
        let destructive_services = changes
            .instance_changes
            .iter()
            .filter(|change| change.mode == "reconcileService" && change.allow_deletes)
            .map(|change| &change.service)
            .collect::<BTreeSet<_>>();
        let mut post_commit_property_changes = changes
            .property_changes
            .iter()
            .filter_map(|change| {
                (binary_import.is_some_and(|import| import.imports_service(&change.service))
                    && change.class_name == "Model")
                    .then(|| change.properties.get("WorldPivot"))
                    .flatten()
                    .map(|value| {
                        let mut change = change.clone();
                        change.properties.clear();
                        change.reset_properties.clear();
                        change
                            .properties
                            .insert("WorldPivot".to_string(), value.clone());
                        change.attributes.clear();
                        change.deleted_attributes.clear();
                        change
                    })
            })
            .collect::<Vec<_>>();
        sort_post_commit_model_pivots(&mut post_commit_property_changes);
        let property_changes = changes
            .property_changes
            .iter()
            .filter(|change| {
                !binary_import.is_some_and(|import| import.imports_service(&change.service))
                    || property_change_needs_post_native_apply(change)
            })
            .collect::<Vec<_>>();
        json!({
            "transactionId": id,
            "services": services,
            "hasInstanceChanges": has_instance_changes,
            "destructiveServices": destructive_services,
            "sourceChanges": source_changes,
            "propertyChanges": property_changes,
            "mutationRoots": mutation_roots,
            "mutationPackageTargets": editor_mutation_package_targets(changes, binary_import),
            "postCommitPropertyChanges": post_commit_property_changes,
            "nativeImport": native_import,
            "nativeImportServices": native_import_services,
            "expectedRuntimeId": guard.map(|guard| &guard.runtime_id),
            "expectedStudioGenerations": guard.map(|guard| &guard.service_generations),
        })
    }

    fn upload(bridge: &BridgeServer, id: &str, mut parameters: Value) -> Result<Value> {
        let object = parameters
            .as_object_mut()
            .context("Editor transaction parameters must be an object")?;
        let services = object
            .remove("services")
            .context("Editor transaction services are missing")?;
        let has_instance_changes = object
            .remove("hasInstanceChanges")
            .unwrap_or(Value::Bool(false));
        let destructive_services = object
            .remove("destructiveServices")
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let native_import = object.remove("nativeImport").unwrap_or(Value::Bool(false));
        let native_import_services = object
            .remove("nativeImportServices")
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let mutation_roots = object
            .remove("mutationRoots")
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let mutation_package_targets = object
            .remove("mutationPackageTargets")
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let expected_runtime_id = object.remove("expectedRuntimeId").unwrap_or(Value::Null);
        let expected_studio_generations = object
            .remove("expectedStudioGenerations")
            .unwrap_or(Value::Null);
        let mut rows = Vec::new();
        for (field, kind) in [
            ("sourceChanges", "source"),
            ("propertyChanges", "property"),
            ("postCommitPropertyChanges", "postCommitProperty"),
        ] {
            let values = object
                .remove(field)
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            rows.extend(
                values
                    .into_iter()
                    .map(|change| json!({ "kind": kind, "change": change })),
            );
        }
        let mut chunks = Vec::<Vec<Value>>::new();
        let mut chunk = Vec::new();
        let mut chunk_bytes = 2usize;
        for row in rows {
            let row_bytes = serde_json::to_vec(&row)?.len() + usize::from(!chunk.is_empty());
            if row_bytes + 65536 > MAX_BRIDGE_REQUEST_BYTES {
                bail!("One editor transaction row exceeds the bridge request limit");
            }
            if !chunk.is_empty() && chunk_bytes.saturating_add(row_bytes) > MAX_BRIDGE_CHUNK_BYTES {
                chunks.push(std::mem::take(&mut chunk));
                chunk_bytes = 2;
            }
            chunk_bytes = chunk_bytes.saturating_add(row_bytes);
            chunk.push(row);
        }
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        bridge.call(
            "beginEditorTransactionUpload",
            json!({
                "transactionId": id,
                "services": services,
                "hasInstanceChanges": has_instance_changes,
                "destructiveServices": destructive_services,
                "nativeImport": native_import,
                "nativeImportServices": native_import_services,
                "mutationRoots": mutation_roots,
                "mutationPackageTargets": mutation_package_targets,
                "expectedRuntimeId": expected_runtime_id,
                "expectedStudioGenerations": expected_studio_generations,
                "totalChunks": chunks.len(),
                "rowCount": chunks.iter().map(Vec::len).sum::<usize>(),
            }),
        )?;
        let result = (|| -> Result<Value> {
            for (index, rows) in chunks.iter().enumerate() {
                bridge.call(
                    "appendEditorTransactionUpload",
                    json!({
                        "transactionId": id,
                        "index": index + 1,
                        "rows": rows,
                    }),
                )?;
            }
            bridge.call(
                "finishEditorTransactionUpload",
                json!({ "transactionId": id }),
            )
        })();
        if result.is_err() {
            let _ = bridge.call(
                "cancelEditorTransactionUpload",
                json!({ "transactionId": id }),
            );
        }
        result
    }

    fn begin(
        bridge: &'a BridgeServer,
        changes: &EditorChangeSet,
        binary_import: Option<&EditorBinaryImport>,
        guard: Option<&StudioChangeGuard>,
    ) -> Result<Option<Self>> {
        let mut services = changes.services().map(str::to_string).collect::<Vec<_>>();
        services.sort();
        services.dedup();
        if services.is_empty() {
            return Ok(None);
        }
        let id = format!(
            "{}-{}",
            current_millis(),
            fnv1a_hex(services.join("\0").as_bytes())
        );
        let upload_services = services.clone();
        let parameters = Self::parameters(changes, binary_import, &id, services, guard);
        let result = match bridge.call("beginEditorTransaction", parameters) {
            Ok(result) => result,
            Err(error) if error.is::<BridgeRequestTooLarge>() => Self::upload(
                bridge,
                &id,
                Self::parameters(changes, binary_import, &id, upload_services, guard),
            )?,
            Err(error) => return Err(error),
        };
        if result.get("studioChanged").and_then(Value::as_bool) == Some(true) {
            return Err(StudioChangedBeforePush.into());
        }
        let package_mutation = result
            .get("packageMutation")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut transaction = Self {
            bridge,
            id,
            active: true,
            package_mutation,
            package_dialog: None,
            auto_desynced_packages: Vec::new(),
            auto_desynced_package_targets: Vec::new(),
            package_runtime: None,
            auto_desync_confirmed: false,
        };

        let packages: EditorMutationPackages = serde_json::from_value(json!({
            "packages": result
                .get("mutationPackages")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        }))
        .context("Studio returned invalid transaction package targets")?;
        if global_log_enabled(5) {
            log_global(
                5,
                format_args!(
                    "[renium] editor package preflight: source={}, property={}, instance={}, packages={:?}",
                    changes.source_changes.len(),
                    changes.property_changes.len(),
                    changes.instance_changes.len(),
                    packages
                        .packages
                        .iter()
                        .map(|package| package.path_segments.join("."))
                        .collect::<Vec<_>>()
                ),
            );
        }
        #[cfg(any(windows, target_os = "macos"))]
        if !packages.packages.is_empty() {
            let started = Instant::now();
            let timeout = Duration::from_secs(20);
            let pid = studio_pid_for_bridge(bridge)?;
            let title = crate::studio::input::studio_window_title(pid)?;
            transaction.package_runtime = Some((pid, title.clone()));
            for root in &packages.packages {
                let result = (|| {
                    let remaining = timeout
                        .checked_sub(started.elapsed())
                        .context("Automatic package desync exceeded 20 seconds")?;
                    crate::studio::native::serializer::run_package_action(
                        pid,
                        &title,
                        &crate::studio::native::serializer::PackageTarget {
                            path_segments: root.path_segments.clone(),
                            path_ordinals: root.path_ordinals.clone(),
                            expected_version: root.expected_version,
                        },
                        crate::studio::native::serializer::PackageAction::Desync,
                        remaining,
                    )
                })();
                match result {
                    Ok(result) if result.changed => {
                        transaction.auto_desynced_packages.push(result.path);
                        transaction.auto_desynced_package_targets.push(root.clone());
                    }
                    Ok(_) => {}
                    Err(error) => {
                        if let Err(rollback_error) = transaction.rollback() {
                            return Err(error.context(format!(
                                "Automatic package desync rollback also failed: {rollback_error:#}"
                            )));
                        }
                        return Err(error);
                    }
                }
            }
            transaction.auto_desync_confirmed = true;
        }
        if package_mutation && !cfg!(any(windows, target_os = "macos")) {
            transaction.auto_desynced_packages = packages
                .packages
                .iter()
                .map(|package| package.path_segments.join("."))
                .collect();
            transaction.package_dialog = Some(
                studio_pid_for_bridge(bridge)
                    .and_then(crate::studio::input::watch_package_changes_dialog)
                    .context("Package changes cannot be applied without a dialog watcher")?,
            );
        }
        Ok(Some(transaction))
    }

    #[cfg(any(windows, target_os = "macos"))]
    fn restore_auto_desynced_packages(&mut self) -> Result<()> {
        if self.auto_desynced_package_targets.is_empty() {
            return Ok(());
        }
        let (pid, title) = self
            .package_runtime
            .as_ref()
            .context("Studio package runtime was not retained for rollback")?;
        let started = Instant::now();
        let timeout = Duration::from_secs(20);
        for root in self.auto_desynced_package_targets.iter().rev() {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .context("Automatic package state rollback exceeded 20 seconds")?;
            crate::studio::native::serializer::run_package_action(
                *pid,
                title,
                &crate::studio::native::serializer::PackageTarget {
                    path_segments: root.path_segments.clone(),
                    path_ordinals: root.path_ordinals.clone(),
                    expected_version: root.expected_version,
                },
                crate::studio::native::serializer::PackageAction::Restore,
                remaining,
            )?;
        }
        self.auto_desynced_package_targets.clear();
        self.auto_desynced_packages.clear();
        Ok(())
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn restore_auto_desynced_packages(&mut self) -> Result<()> {
        Ok(())
    }

    fn finish_rollback(&mut self) -> Result<()> {
        self.package_dialog.take();
        self.restore_auto_desynced_packages()?;
        self.active = false;
        Ok(())
    }

    fn commit(&mut self) -> Result<EditorCommitStatus> {
        let commit_result = self.bridge.call(
            "commitEditorTransaction",
            json!({
                "transactionId": &self.id,
                "profile": verbose_timing_logs(),
            }),
        );
        let result = match commit_result {
            Ok(result) => result,
            Err(commit_error) => {
                let state_result = self.bridge.call(
                    "getEditorTransactionState",
                    json!({ "transactionId": &self.id }),
                );
                match state_result {
                    Ok(result) => match editor_transaction_state(&result) {
                        Some("committed") => result,
                        Some("rolledBack") => {
                            self.finish_rollback().context(
                                "Studio rolled the transaction back, but Renium could not restore its automatic package state changes",
                            )?;
                            return Err(commit_error.context(
                                "Studio rolled the transaction back before its commit response was received",
                            ));
                        }
                        Some("open" | "prepared") => {
                            return Err(commit_error.context(
                                "Studio did not commit the transaction; rollback is still available",
                            ));
                        }
                        Some("rollbackFailed") => {
                            return Err(commit_error.context(
                                "Studio could not roll the transaction back; its recovery state was retained",
                            ));
                        }
                        Some("notFound") | None => {
                            self.active = false;
                            self.package_dialog.take();
                            return Err(commit_error.context(
                                "Studio did not retain this transaction outcome; whether it committed is unknown",
                            ));
                        }
                        Some(state) => {
                            return Err(commit_error.context(format!(
                                "Studio returned an invalid transaction state: {state}"
                            )));
                        }
                    },
                    Err(state_error) => {
                        return Err(commit_error.context(format!(
                            "Studio commit outcome could not be queried: {state_error:#}"
                        )));
                    }
                }
            }
        };
        match editor_transaction_state(&result) {
            Some("rolledBack") => {
                self.finish_rollback().context(
                    "Studio rolled the transaction back, but Renium could not restore its automatic package state changes",
                )?;
                bail!("Studio rolled the transaction back instead of committing it");
            }
            Some("open" | "prepared" | "notFound") => {
                bail!("Studio did not confirm the editor transaction commit");
            }
            Some("rollbackFailed") => {
                bail!("Studio retained the transaction because its rollback failed")
            }
            Some("committed") | None => {}
            Some(state) => bail!("Studio returned an invalid transaction state: {state}"),
        }
        self.active = false;
        self.auto_desynced_package_targets.clear();
        self.package_runtime = None;
        let package_dialog_accepted = self
            .package_dialog
            .take()
            .map(|watcher| watcher.finish())
            .transpose();
        let package_dialog_accepted = package_dialog_accepted
            .context("Studio did not finish accepting package changes")?
            .unwrap_or(false);
        if verbose_timing_logs()
            && let Some(profile) = result.get("profile")
        {
            eprintln!("[renium] native editor commit profile: {profile}");
        }
        Ok(EditorCommitStatus {
            package_mutation: self.package_mutation,
            package_dialog_accepted,
            auto_desynced_packages: std::mem::take(&mut self.auto_desynced_packages),
            auto_desync_confirmed: self.auto_desync_confirmed,
        })
    }

    fn rollback(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        self.package_dialog.take();
        self.restore_auto_desynced_packages()?;
        let rollback_result = self.bridge.call(
            "rollbackEditorTransaction",
            json!({ "transactionId": &self.id }),
        );
        let result = match rollback_result {
            Ok(result) => result,
            Err(rollback_error) => {
                let state_result = self.bridge.call(
                    "getEditorTransactionState",
                    json!({ "transactionId": &self.id }),
                );
                match state_result {
                    Ok(result) => result,
                    Err(state_error) => {
                        return Err(rollback_error.context(format!(
                            "Studio rollback outcome could not be queried: {state_error:#}"
                        )));
                    }
                }
            }
        };
        match editor_transaction_state(&result) {
            Some("rolledBack") | None => {
                self.active = false;
                Ok(())
            }
            Some("committed") => {
                self.active = false;
                bail!("Studio had already committed the transaction; rollback was not performed")
            }
            Some("notFound") => {
                self.active = false;
                bail!("Studio did not retain this transaction outcome; rollback is unconfirmed")
            }
            Some("open" | "prepared") => bail!("Studio did not roll the transaction back"),
            Some("rollbackFailed") => {
                bail!("Studio retained the transaction because its rollback failed")
            }
            Some(state) => bail!("Studio returned an invalid transaction state: {state}"),
        }
    }

    fn disarm(&mut self) {
        self.active = false;
        self.package_dialog.take();
    }
}

fn sort_post_commit_model_pivots(changes: &mut [EditorPropertyChange]) {
    changes.sort_by_key(|change| change.path_segments.len());
}

impl Drop for EditorTransaction<'_> {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.rollback()
        {
            eprintln!("[renium] editor rollback failed: {error:#}");
        }
    }
}

fn skipped_editor_summary(changes: &EditorChangeSet) -> Map<String, Value> {
    let mut summary = Map::new();
    summary.insert("ok".to_string(), Value::Bool(true));
    summary.insert("skippedByReview".to_string(), Value::Bool(true));
    summary.insert(
        "instanceQueued".to_string(),
        Value::Number(Number::from(
            changes
                .instance_changes
                .iter()
                .map(|change| change.instances.len())
                .sum::<usize>() as u64,
        )),
    );
    summary.insert(
        "sourceQueued".to_string(),
        Value::Number(Number::from(changes.source_changes.len() as u64)),
    );
    summary.insert(
        "propertyQueued".to_string(),
        Value::Number(Number::from(changes.property_changes.len() as u64)),
    );
    summary.insert("noops".to_string(), Value::Number(Number::from(0)));
    summary
}

pub(crate) fn settings_file_hash(path: &Path) -> Result<Option<[u8; 32]>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(Sha256::digest(bytes).into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn add_editor_commit_status(summary: &mut Map<String, Value>, status: EditorCommitStatus) {
    if status.package_mutation {
        summary.insert("packageModified".to_string(), Value::Bool(true));
    }
    if status.package_dialog_accepted {
        summary.insert("packageDialogAccepted".to_string(), Value::Bool(true));
    }
    if !status.auto_desynced_packages.is_empty()
        && (status.auto_desync_confirmed || status.package_dialog_accepted)
    {
        summary.insert(
            "autoDesyncedPackages".to_string(),
            Value::Array(
                status
                    .auto_desynced_packages
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
}

fn listen_editor_push_bridge(args: &BridgeConnectionArgs) -> Result<BridgeServer> {
    let ports = parse_bridge_ports(&args.ports)?;
    let (bridge, metrics) = BridgeServer::listen(&args.host, &ports, args.wait_seconds)?;
    log_global(
        5,
        format_args!(
            "[renium] editor push bridge ready: channels={}/{}, bind_ms={:.1}, handshake_ms={:.1}",
            bridge.channel_count(),
            bridge.expected_channel_count(),
            metrics.bind_ms,
            metrics.wait_for_channels_ms
        ),
    );
    Ok(bridge)
}

pub(crate) fn push_editor_changes(mut args: PushEditorChangesArgs) -> Result<()> {
    args.changed_paths.append(&mut args.paths);
    apply_configured_project_layout(&mut args.project.project_root, &mut args.project.src_root)?;
    let incremental = !args.changed_paths.is_empty()
        || !args.changed_paths_files.is_empty()
        || !args.target_settings_ids.is_empty()
        || !args.target_settings_id_files.is_empty()
        || !args.target_properties.is_empty();
    let parameters = json!({
        "srcDir": args.project.src_root,
        "changedPaths": args.changed_paths,
        "changedPathsFiles": args.changed_paths_files,
        "targetSettingsIds": args.target_settings_ids,
        "targetSettingsIdFiles": args.target_settings_id_files,
        "targetProperties": args.target_properties,
        "upsertInstancesOnly": args.upsert_instances_only,
        "probeEvents": args.probe_events,
        "verifySources": args.verify_sources,
        "overridePackages": args.override_packages,
        "allowProtectedWrites": !args.no_review,
        "linkCacheDir": args.link_cache_dir,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
        "destructive": !incremental,
    });
    let approved = !args.no_review && (args.yes || global_yes());
    if let Some(result) = try_daemon_control_request(
        op::PUSH,
        Some(&args.project.project_root),
        parameters,
        approved,
    )? {
        return print_json_output(&result, global_pretty_output(false));
    }
    if !incremental {
        bail!("A full push requires Renium's semantic sync daemon; Studio was not changed");
    }
    let started = Instant::now();
    if native_editor_full_push_eligible(&args)? {
        let bridge = listen_editor_push_bridge(&args.bridge)?;
        let (changes, binary_import) = prepare_native_editor_full_push(&args, &bridge)?;
        let summary = push_editor_changes_with_collected(
            args,
            &bridge,
            changes,
            CollectedPushOptions {
                started,
                projection: None,
                prepared_binary_import: Some(binary_import),
                guard: None,
                validate_project: None,
            },
        )?;
        return print_editor_push_summary(&summary);
    }
    let (changes, projection) = collect_project_editor_changes(&args)?;
    let bridge = listen_editor_push_bridge(&args.bridge)?;
    let summary = push_editor_changes_with_collected(
        args,
        &bridge,
        changes,
        CollectedPushOptions {
            started,
            projection: projection.as_ref(),
            prepared_binary_import: None,
            guard: None,
            validate_project: None,
        },
    )?;
    print_editor_push_summary(&summary)
}

pub(crate) fn push_editor_changes_with_warm_bridge(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
) -> Result<serde_json::Map<String, Value>> {
    push_editor_changes_with_warm_bridge_guarded(args, bridge, None)
}

pub(crate) fn push_editor_changes_with_warm_bridge_guarded(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
    guard: Option<&StudioChangeGuard>,
) -> Result<serde_json::Map<String, Value>> {
    let started = Instant::now();
    if native_editor_full_push_eligible(&args)? {
        bail!("A full push must use the semantic delta path; Studio was not changed");
    }
    let (changes, projection) = collect_project_editor_changes(&args)?;
    push_editor_changes_with_collected(
        args,
        bridge,
        changes,
        CollectedPushOptions {
            started,
            projection: projection.as_ref(),
            prepared_binary_import: None,
            guard,
            validate_project: None,
        },
    )
}

pub(crate) fn push_reconciled_editor_changes_with_warm_bridge<F, G>(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
    guard: Option<&StudioChangeGuard>,
    prepared_documents: HashMap<String, SettingsBytecode>,
    amend: F,
    validate_project: G,
) -> Result<serde_json::Map<String, Value>>
where
    F: FnOnce(&mut EditorChangeSet) -> Result<()>,
    G: Fn() -> Result<()>,
{
    let started = Instant::now();
    let no_selection = args.changed_paths.is_empty()
        && args.changed_paths_files.is_empty()
        && args.target_settings_ids.is_empty()
        && args.target_settings_id_files.is_empty()
        && args.target_properties.is_empty();
    let (mut changes, projection) = if no_selection {
        (EditorChangeSet::default(), None)
    } else {
        collect_project_editor_changes_with_documents(&args, prepared_documents)?
    };
    amend(&mut changes)?;
    push_editor_changes_with_collected(
        args,
        bridge,
        changes,
        CollectedPushOptions {
            started,
            projection: projection.as_ref(),
            prepared_binary_import: None,
            guard,
            validate_project: Some(&validate_project),
        },
    )
}

pub(crate) fn native_editor_full_push_eligible(args: &PushEditorChangesArgs) -> Result<bool> {
    if !args.changed_paths.is_empty()
        || !args.changed_paths_files.is_empty()
        || !args.target_settings_ids.is_empty()
        || !args.target_settings_id_files.is_empty()
        || !args.target_properties.is_empty()
        || args.upsert_instances_only
        || args.probe_events
        || args.verify_sources
        || (!args.no_review && !args.yes && !global_yes())
    {
        return Ok(false);
    }
    if let Some(project) = config::try_load_project(None, Some(&args.project.project_root))?
        && config::project_requires_temporary_stage(&project)?
    {
        return Ok(false);
    }
    if !args.override_packages && args.project.project_root.join("renium-link.json").exists() {
        return Ok(false);
    }
    Ok(true)
}

fn collect_project_editor_changes(
    args: &PushEditorChangesArgs,
) -> Result<(EditorChangeSet, Option<config::ProjectionStage>)> {
    collect_project_editor_changes_with_documents(args, HashMap::new())
}

fn collect_project_editor_changes_with_documents(
    args: &PushEditorChangesArgs,
    mut prepared_documents: HashMap<String, SettingsBytecode>,
) -> Result<(EditorChangeSet, Option<config::ProjectionStage>)> {
    let phase_started = Instant::now();
    let Some(loaded) = config::try_load_project(None, Some(&args.project.project_root))? else {
        return Ok((collect_editor_changes(args)?, None));
    };
    log_editor_collection_timing("project load", phase_started);
    let phase_started = Instant::now();
    let link_enforcement = build_loaded_project_link_enforcement(&loaded, args.override_packages)?;
    log_editor_collection_timing("link enforcement", phase_started);
    let mut changed_paths = expand_editor_changed_paths(args)?;
    let full_selection = changed_paths.is_empty();
    if full_selection {
        changed_paths = collect_editor_full_paths(&loaded.root.join(&loaded.project.source_root))?;
    }
    let changed_paths =
        apply_link_enforcement_to_changed_paths(&loaded.root, &link_enforcement, changed_paths)?;
    let mut changed_sources = changed_paths
        .iter()
        .map(|path| absolutize_under(&loaded.root, path))
        .collect::<Vec<_>>();
    if full_selection {
        changed_sources.push(loaded.path.clone());
    }
    changed_sources.sort();
    changed_sources.dedup();
    let phase_started = Instant::now();
    let projection = config::stage_project_cached(&loaded, &changed_sources)?;
    log_editor_collection_timing("projection", phase_started);
    if !projection.is_temporary() {
        let (project_root, src_root) = editor_project_roots(args)?;
        let phase_started = Instant::now();
        let changes = collect_editor_changes_with_link_enforcement_and_documents(
            args,
            &project_root,
            &src_root,
            &link_enforcement,
            &mut prepared_documents,
        )?;
        log_editor_collection_timing("changes", phase_started);
        return Ok((changes, Some(projection)));
    }
    let mut projected_paths = if full_selection {
        collect_editor_full_paths(projection.root())?
    } else {
        let mut paths = Vec::new();
        let naming = config::project_script_naming(&loaded.project);
        for changed_path in changed_paths {
            let absolute = absolutize_under(&loaded.root, &changed_path);
            let source_file = absolute.is_file()
                && absolute
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| infer_source_script(name, &naming))
                    .is_some();
            let mut mapped =
                config::project_source_to_staged_paths(&loaded, &absolute, projection.root())?;
            if source_file {
                mapped.retain(|path| {
                    !path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(is_service_settings_file_name)
                });
            }
            paths.extend(mapped);
        }
        paths
    };
    projected_paths.sort();
    projected_paths.dedup();
    let mut projected_args = args.clone();
    projected_args.project.project_root = projection.root().to_path_buf();
    projected_args.project.src_root = PathBuf::from(".");
    projected_args.changed_paths = projected_paths;
    projected_args.changed_paths_files.clear();
    projected_args.link_cache_dir = None;
    let (project_root, src_root) = editor_project_roots(&projected_args)?;
    let changes = collect_editor_changes_with_link_enforcement(
        &projected_args,
        &project_root,
        &src_root,
        &link_enforcement,
    )?;
    Ok((changes, Some(projection)))
}

fn log_editor_collection_timing(label: &str, started: Instant) {
    log_global(
        5,
        format_args!(
            "[renium] editor collection {label}: {:.1}ms",
            elapsed_ms(started)
        ),
    );
}

struct OwnedEditorFilterCandidate {
    id: String,
    path: String,
    name: String,
    class_name: String,
    tags: BTreeSet<String>,
    attributes: BTreeSet<String>,
    properties: BTreeSet<String>,
}

struct EditorFilterCandidateIndex {
    candidates: Vec<OwnedEditorFilterCandidate>,
    by_id: HashMap<(String, String), usize>,
    by_path: HashMap<String, usize>,
}

impl OwnedEditorFilterCandidate {
    fn filter_candidate(&self) -> config::FilterCandidate<'_> {
        config::FilterCandidate {
            id: &self.id,
            path: &self.path,
            name: &self.name,
            class: &self.class_name,
            tags: &self.tags,
            attributes: &self.attributes,
            properties: &self.properties,
        }
    }

    fn allows_instance(&self, rules: &[config::FilterRule]) -> Result<bool> {
        config::filter_allows_instance(
            rules,
            config::FilterDirection::FilesToStudio,
            &self.filter_candidate(),
        )
    }

    fn allows_property(&self, rules: &[config::FilterRule], property: &str) -> Result<bool> {
        config::filter_allows_property(
            rules,
            config::FilterDirection::FilesToStudio,
            &self.filter_candidate(),
            property,
        )
    }

    fn allows_attribute(&self, rules: &[config::FilterRule], attribute: &str) -> Result<bool> {
        config::filter_allows_attribute(
            rules,
            config::FilterDirection::FilesToStudio,
            &self.filter_candidate(),
            attribute,
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditorFilterCandidatePage {
    items: Vec<EditorFilterCandidateRow>,
    next_index: Option<usize>,
    snapshot_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditorFilterCandidateRow {
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
    name: String,
    class_name: String,
    #[serde(default)]
    settings_id: String,
    tags: Vec<String>,
    attributes: Vec<String>,
}

fn editor_filter_path_key(path_segments: &[String], path_ordinals: &[usize]) -> String {
    serde_json::to_string(&(path_segments, path_ordinals))
        .expect("instance paths are JSON-serializable")
}

fn canonical_filter_settings_id(
    projection: Option<&config::ProjectionStage>,
    settings_id: &str,
) -> String {
    projection
        .and_then(|stage| stage.canonical_identity(settings_id))
        .map_or_else(
            || settings_id.to_string(),
            |(_, canonical_id)| canonical_id.to_string(),
        )
}

fn files_to_studio_filter_rules(project_root: &Path) -> Result<Option<Vec<config::FilterRule>>> {
    let Some(loaded) = config::try_load_project(None, Some(project_root))? else {
        return Ok(None);
    };
    if loaded.root != project_root {
        return Ok(None);
    }
    let rules = config::compiled_files_to_studio_filters(&loaded)?;
    Ok((!rules.is_empty()).then_some(rules))
}

fn files_to_studio_ignore_unknown_targets(
    project_root: &Path,
    reconciled_services: &BTreeSet<String>,
) -> Result<HashMap<String, Vec<Vec<String>>>> {
    let Some(loaded) = config::try_load_project(None, Some(project_root))? else {
        return Ok(HashMap::new());
    };
    if loaded.root != project_root {
        return Ok(HashMap::new());
    }
    let mut output = HashMap::<String, Vec<Vec<String>>>::new();
    for target in
        config::compiled_files_to_studio_ignore_unknown_targets(&loaded, reconciled_services)?
    {
        if let Some(service) = target.first() {
            output.entry(service.clone()).or_default().push(target);
        }
    }
    for targets in output.values_mut() {
        targets.sort();
        targets.dedup();
    }
    Ok(output)
}

fn sort_editor_preserves(preserves: &mut Vec<EditorPreserveDescriptor>) {
    preserves.sort_by(|left, right| {
        (&left.path_segments, &left.path_ordinals)
            .cmp(&(&right.path_segments, &right.path_ordinals))
    });
    preserves.dedup_by(|left, right| {
        left.path_segments == right.path_segments && left.path_ordinals == right.path_ordinals
    });
}

fn attach_ignore_unknown_preserves(
    args: &PushEditorChangesArgs,
    bridge: &BridgeServer,
    changes: &mut EditorChangeSet,
    projection: Option<&config::ProjectionStage>,
) -> Result<()> {
    let reconciled_services = changes
        .instance_changes
        .iter()
        .filter(|change| change.mode == "reconcileService" && change.allow_deletes)
        .map(|change| change.service.clone())
        .collect::<BTreeSet<_>>();
    let targets =
        files_to_studio_ignore_unknown_targets(&args.project.project_root, &reconciled_services)?;
    if targets.is_empty() {
        return Ok(());
    }
    let desired = build_editor_filter_candidates(args, changes, projection)?;
    for change in &mut changes.instance_changes {
        if change.mode != "reconcileService" || !change.allow_deletes {
            continue;
        }
        let Some(service_targets) = targets.get(&change.service) else {
            continue;
        };
        let mut start_index = 1usize;
        let mut snapshot_id = None;
        let mut preserves = change.preserve_instances.clone();
        loop {
            let page: EditorFilterCandidatePage = serde_json::from_value(bridge.call(
                "getEditorFilterCandidates",
                json!({
                    "service": &change.service,
                    "startIndex": start_index,
                    "maxCount": 500,
                    "snapshotId": snapshot_id,
                    "includeSettingsIds": true,
                }),
            )?)
            .context("Studio returned invalid ignore-unknown candidates")?;
            if snapshot_id.is_none() {
                snapshot_id.clone_from(&page.snapshot_id);
            }
            for row in page.items {
                if !service_targets
                    .iter()
                    .any(|target| row.path_segments.starts_with(target))
                {
                    continue;
                }
                let known = desired.by_id.contains_key(&(
                    change.service.clone(),
                    canonical_filter_settings_id(projection, &row.settings_id),
                )) || desired.by_path.contains_key(&editor_filter_path_key(
                    &row.path_segments,
                    &row.path_ordinals,
                )) || desired
                    .by_path
                    .contains_key(&editor_filter_path_key(&row.path_segments, &[]));
                if !known {
                    preserves.push(EditorPreserveDescriptor {
                        path_segments: row.path_segments.clone(),
                        path_ordinals: row.path_ordinals.clone(),
                    });
                }
            }
            let Some(next_index) = page.next_index else {
                break;
            };
            if next_index <= start_index {
                bail!("Studio ignore-unknown cursor did not advance");
            }
            start_index = next_index;
        }
        sort_editor_preserves(&mut preserves);
        change.preserve_instances = preserves;
    }
    Ok(())
}

fn editor_change_services(changes: &EditorChangeSet) -> BTreeSet<String> {
    changes
        .services()
        .chain(
            changes
                .history_entries
                .iter()
                .map(|entry| entry.service.as_str()),
        )
        .map(str::to_string)
        .collect()
}

fn build_editor_filter_candidates(
    args: &PushEditorChangesArgs,
    changes: &EditorChangeSet,
    projection: Option<&config::ProjectionStage>,
) -> Result<EditorFilterCandidateIndex> {
    let src_root = projection.filter(|stage| stage.is_temporary()).map_or_else(
        || args.project.project_root.join(&args.project.src_root),
        |stage| stage.root().to_path_buf(),
    );
    let document_overrides = changes
        .settings_writes
        .iter()
        .filter_map(|write| {
            let service = write.path.parent()?.file_name()?.to_str()?.to_string();
            Some((service, &write.document))
        })
        .collect::<HashMap<_, _>>();
    let mut candidates = Vec::new();
    let mut by_id = HashMap::new();
    let mut by_path = HashMap::new();
    let mut ambiguous_paths = HashSet::new();
    let services = editor_change_services(changes);
    for service in services {
        let stored;
        let document = if let Some(document) = document_overrides.get(&service) {
            *document
        } else {
            stored = read_editor_service_settings(&src_root, &service)?;
            let Some(document) = stored.as_ref() else {
                continue;
            };
            document
        };
        let paths = build_editor_instance_paths(document, &service);
        for (index, instance) in document.instances.iter().enumerate() {
            let Some(path) = paths.get(index).and_then(Option::as_ref) else {
                continue;
            };
            let candidate_index = candidates.len();
            let canonical_id = canonical_filter_settings_id(projection, &instance.settings_id);
            let fields =
                config::filter_candidate_fields(&instance.properties, &instance.attributes);
            candidates.push(OwnedEditorFilterCandidate {
                id: canonical_id.clone(),
                path: config::filter_path_segments(&path.path_segments),
                name: instance.name.clone(),
                class_name: instance.class_name.clone(),
                tags: fields.tags,
                attributes: fields.attributes,
                properties: fields.properties,
            });
            by_id.insert((service.clone(), canonical_id), candidate_index);
            by_path.insert(
                editor_filter_path_key(&path.path_segments, &path.path_ordinals),
                candidate_index,
            );
            let path_only_key = editor_filter_path_key(&path.path_segments, &[]);
            if !ambiguous_paths.contains(&path_only_key)
                && by_path
                    .insert(path_only_key.clone(), candidate_index)
                    .is_some()
            {
                by_path.remove(&path_only_key);
                ambiguous_paths.insert(path_only_key);
            }
        }
    }
    Ok(EditorFilterCandidateIndex {
        candidates,
        by_id,
        by_path,
    })
}

fn editor_change_filter_candidate<'a>(
    index: &'a EditorFilterCandidateIndex,
    projection: Option<&config::ProjectionStage>,
    service: &str,
    settings_id: Option<&str>,
    path_segments: &[String],
    path_ordinals: &[usize],
) -> Option<&'a OwnedEditorFilterCandidate> {
    settings_id
        .map(|id| canonical_filter_settings_id(projection, id))
        .and_then(|id| index.by_id.get(&(service.to_string(), id)))
        .or_else(|| {
            index
                .by_path
                .get(&editor_filter_path_key(path_segments, path_ordinals))
        })
        .or_else(|| {
            index
                .by_path
                .get(&editor_filter_path_key(path_segments, &[]))
        })
        .and_then(|candidate| index.candidates.get(*candidate))
}

fn fallback_editor_filter_candidate(
    settings_id: Option<&str>,
    path_segments: &[String],
    class_name: &str,
    properties: &Map<String, Value>,
    attributes: &Map<String, Value>,
) -> OwnedEditorFilterCandidate {
    let fields = config::filter_candidate_fields(properties, attributes);
    OwnedEditorFilterCandidate {
        id: settings_id.unwrap_or("").to_string(),
        path: config::filter_path_segments(path_segments),
        name: path_segments.last().cloned().unwrap_or_default(),
        class_name: class_name.to_string(),
        tags: fields.tags,
        attributes: fields.attributes,
        properties: fields.properties,
    }
}

fn editor_change_filter_candidates<'a>(
    desired: &'a EditorFilterCandidateIndex,
    live: &'a EditorFilterCandidateIndex,
    projection: Option<&config::ProjectionStage>,
    service: &str,
    settings_id: Option<&str>,
    path_segments: &[String],
    path_ordinals: &[usize],
) -> (
    Option<&'a OwnedEditorFilterCandidate>,
    Option<&'a OwnedEditorFilterCandidate>,
) {
    (
        editor_change_filter_candidate(
            desired,
            projection,
            service,
            settings_id,
            path_segments,
            path_ordinals,
        ),
        editor_change_filter_candidate(
            live,
            projection,
            service,
            settings_id,
            path_segments,
            path_ordinals,
        ),
    )
}

fn attach_live_filter_preserves(
    bridge: &BridgeServer,
    rules: &[config::FilterRule],
    changes: &mut EditorChangeSet,
    projection: Option<&config::ProjectionStage>,
) -> Result<EditorFilterCandidateIndex> {
    let property_names = rules
        .iter()
        .filter_map(|rule| rule.property.as_deref())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let services = editor_change_services(changes);
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let mut candidates = Vec::new();
    let mut by_id = HashMap::new();
    let mut by_path = HashMap::new();
    let mut ambiguous_paths = HashSet::new();
    for service in services {
        let mut start_index = 1usize;
        let mut snapshot_id = None;
        let mut preserves = changes
            .instance_changes
            .iter()
            .find(|change| {
                change.service == service
                    && change.mode == "reconcileService"
                    && change.allow_deletes
            })
            .map(|change| change.preserve_instances.clone())
            .unwrap_or_default();
        loop {
            let page: EditorFilterCandidatePage = serde_json::from_value(bridge.call(
                "getEditorFilterCandidates",
                json!({
                    "service": &service,
                    "startIndex": start_index,
                    "maxCount": 500,
                    "snapshotId": snapshot_id,
                    "includeSettingsIds": true,
                }),
            )?)
            .context("Studio returned invalid filter candidates")?;
            if snapshot_id.is_none() {
                snapshot_id.clone_from(&page.snapshot_id);
            }
            for row in page.items {
                let exact_key = editor_filter_path_key(&row.path_segments, &row.path_ordinals);
                let path_only_key = editor_filter_path_key(&row.path_segments, &[]);
                let properties = property_names
                    .iter()
                    .filter(|name| {
                        rbx_model_property_descriptor(database, &row.class_name, name).is_some()
                    })
                    .cloned()
                    .collect();
                let canonical_id = canonical_filter_settings_id(projection, &row.settings_id);
                let candidate = OwnedEditorFilterCandidate {
                    id: canonical_id.clone(),
                    path: config::filter_path_segments(&row.path_segments),
                    name: row.name.clone(),
                    class_name: row.class_name.clone(),
                    tags: row.tags.into_iter().collect(),
                    attributes: row.attributes.into_iter().collect(),
                    properties,
                };
                if !candidate.allows_instance(rules)? {
                    preserves.push(EditorPreserveDescriptor {
                        path_segments: row.path_segments,
                        path_ordinals: row.path_ordinals,
                    });
                }
                let candidate_index = candidates.len();
                if !candidate.id.is_empty() {
                    by_id.insert((service.clone(), canonical_id), candidate_index);
                }
                by_path.insert(exact_key, candidate_index);
                if !ambiguous_paths.contains(&path_only_key)
                    && by_path
                        .insert(path_only_key.clone(), candidate_index)
                        .is_some()
                {
                    by_path.remove(&path_only_key);
                    ambiguous_paths.insert(path_only_key);
                }
                candidates.push(candidate);
            }
            let Some(next_index) = page.next_index else {
                break;
            };
            if next_index <= start_index {
                bail!("Studio filter candidate cursor did not advance");
            }
            start_index = next_index;
        }
        sort_editor_preserves(&mut preserves);
        for change in &mut changes.instance_changes {
            if change.service == service
                && change.mode == "reconcileService"
                && change.allow_deletes
            {
                change.preserve_instances = preserves;
                break;
            }
        }
    }
    Ok(EditorFilterCandidateIndex {
        candidates,
        by_id,
        by_path,
    })
}

fn apply_files_to_studio_filters(
    args: &PushEditorChangesArgs,
    bridge: &BridgeServer,
    changes: &mut EditorChangeSet,
    projection: Option<&config::ProjectionStage>,
) -> Result<()> {
    attach_ignore_unknown_preserves(args, bridge, changes, projection)?;
    let Some(rules) = files_to_studio_filter_rules(&args.project.project_root)? else {
        return Ok(());
    };
    changes.files_to_studio_filters_active = true;
    let live_index = attach_live_filter_preserves(bridge, &rules, changes, projection)?;
    let candidate_index = build_editor_filter_candidates(args, changes, projection)?;
    for change in &mut changes.instance_changes {
        let allowed = change
            .instances
            .iter()
            .map(|instance| {
                let fallback = fallback_editor_filter_candidate(
                    Some(&instance.settings_id),
                    &instance.path_segments,
                    &instance.class_name,
                    &instance.match_properties,
                    &instance.match_attributes,
                );
                let (candidate, current) = editor_change_filter_candidates(
                    &candidate_index,
                    &live_index,
                    projection,
                    &change.service,
                    Some(&instance.settings_id),
                    &instance.path_segments,
                    &instance.path_ordinals,
                );
                let candidate = candidate.unwrap_or(&fallback);
                Ok(candidate.allows_instance(&rules)?
                    && current
                        .map(|candidate| candidate.allows_instance(&rules))
                        .transpose()?
                        .unwrap_or(true))
            })
            .collect::<Result<Vec<_>>>()?;
        let allowed_paths = change
            .instances
            .iter()
            .zip(&allowed)
            .filter(|(_, allowed)| **allowed)
            .map(|(instance, _)| {
                (
                    instance.path_segments.clone(),
                    instance.path_ordinals.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut retained = Vec::with_capacity(change.instances.len());
        for (mut instance, allowed) in change.instances.drain(..).zip(allowed) {
            let needed = allowed_paths.iter().any(|(path_segments, path_ordinals)| {
                if !path_segments.starts_with(&instance.path_segments) {
                    return false;
                }
                instance.path_segments.iter().enumerate().all(|(index, _)| {
                    path_ordinals.get(index).copied().unwrap_or(1)
                        == instance.path_ordinals.get(index).copied().unwrap_or(1)
                })
            });
            if needed {
                instance.anchor_only = !allowed;
                retained.push(instance);
            }
        }
        change.instances = retained;
    }
    let mut source_changes = Vec::with_capacity(changes.source_changes.len());
    for change in changes.source_changes.drain(..) {
        let fallback = fallback_editor_filter_candidate(
            change.settings_id.as_deref(),
            &change.path_segments,
            &change.class_name,
            &Map::from_iter([("Source".to_string(), Value::Null)]),
            &Map::new(),
        );
        let (candidate, current) = editor_change_filter_candidates(
            &candidate_index,
            &live_index,
            projection,
            &change.service,
            change.settings_id.as_deref(),
            &change.path_segments,
            &change.path_ordinals,
        );
        let candidate = candidate.unwrap_or(&fallback);
        if candidate.allows_property(&rules, "Source")?
            && current
                .map(|candidate| candidate.allows_property(&rules, "Source"))
                .transpose()?
                .unwrap_or(true)
        {
            source_changes.push(change);
        }
    }
    changes.source_changes = source_changes;
    let mut property_changes = Vec::with_capacity(changes.property_changes.len());
    for mut change in changes.property_changes.drain(..) {
        let fallback = fallback_editor_filter_candidate(
            change.settings_id.as_deref(),
            &change.path_segments,
            &change.class_name,
            &change.properties,
            &change.attributes,
        );
        let (candidate, current) = editor_change_filter_candidates(
            &candidate_index,
            &live_index,
            projection,
            &change.service,
            change.settings_id.as_deref(),
            &change.path_segments,
            &change.path_ordinals,
        );
        let candidate = candidate.unwrap_or(&fallback);
        if !candidate.allows_instance(&rules)?
            || !current
                .map(|candidate| candidate.allows_instance(&rules))
                .transpose()?
                .unwrap_or(true)
        {
            continue;
        }
        let mut kept_properties = Map::new();
        for (name, value) in change.properties {
            if candidate.allows_property(&rules, &name)?
                && current
                    .map(|candidate| candidate.allows_property(&rules, &name))
                    .transpose()?
                    .unwrap_or(true)
            {
                kept_properties.insert(name, value);
            }
        }
        change.properties = kept_properties;
        let mut kept_reset_properties = Vec::new();
        for name in change.reset_properties {
            if candidate.allows_property(&rules, &name)?
                && current
                    .map(|candidate| candidate.allows_property(&rules, &name))
                    .transpose()?
                    .unwrap_or(true)
            {
                kept_reset_properties.push(name);
            }
        }
        change.reset_properties = kept_reset_properties;
        let mut kept_attributes = Map::new();
        for (name, value) in change.attributes {
            if candidate.allows_attribute(&rules, &name)?
                && current
                    .map(|candidate| candidate.allows_attribute(&rules, &name))
                    .transpose()?
                    .unwrap_or(true)
            {
                kept_attributes.insert(name, value);
            }
        }
        change.attributes = kept_attributes;
        let mut kept_deleted_attributes = Vec::new();
        for name in change.deleted_attributes {
            if candidate.allows_attribute(&rules, &name)?
                && current
                    .map(|candidate| candidate.allows_attribute(&rules, &name))
                    .transpose()?
                    .unwrap_or(true)
            {
                kept_deleted_attributes.push(name);
            }
        }
        change.deleted_attributes = kept_deleted_attributes;
        if !change.properties.is_empty()
            || !change.reset_properties.is_empty()
            || !change.attributes.is_empty()
            || !change.deleted_attributes.is_empty()
        {
            property_changes.push(change);
        }
    }
    changes.property_changes = property_changes;
    let mut history_entries = Vec::with_capacity(changes.history_entries.len());
    for entry in changes.history_entries.drain(..) {
        let fallback = fallback_editor_filter_candidate(
            entry.settings_id.as_deref(),
            &entry.path_segments,
            &entry.class_name,
            &Map::new(),
            &Map::new(),
        );
        let (candidate, current) = editor_change_filter_candidates(
            &candidate_index,
            &live_index,
            projection,
            &entry.service,
            entry.settings_id.as_deref(),
            &entry.path_segments,
            &[],
        );
        let candidate = candidate.unwrap_or(&fallback);
        if candidate.allows_instance(&rules)?
            && current
                .map(|candidate| candidate.allows_instance(&rules))
                .transpose()?
                .unwrap_or(true)
        {
            history_entries.push(entry);
        }
    }
    changes.history_entries = history_entries;
    Ok(())
}

fn verify_pushed_sources(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
    transaction_id: Option<&str>,
    summary: &mut Map<String, Value>,
) -> Result<()> {
    let mut verification = verify_editor_source_changes(bridge, changes)?;
    if !verification.failed_indexes.is_empty() {
        let retry_changes = EditorChangeSet {
            source_changes: verification
                .failed_indexes
                .iter()
                .map(|index| changes.source_changes[*index].clone())
                .collect(),
            ..EditorChangeSet::default()
        };
        send_editor_change_batches(
            bridge,
            &retry_changes,
            false,
            false,
            false,
            None,
            transaction_id,
        )?;
        verification = verify_editor_source_changes(bridge, changes)?;
    }
    summary.insert(
        "sourceVerified".to_string(),
        Value::Number(Number::from(verification.verified as u64)),
    );
    summary.insert(
        "sourceVerifyFailed".to_string(),
        Value::Number(Number::from(verification.failed.len() as u64)),
    );
    if verification.failed.is_empty() {
        return Ok(());
    }
    summary.insert("ok".to_string(), Value::Bool(false));
    summary.insert(
        "sourceVerifyErrors".to_string(),
        Value::Array(
            verification
                .failed
                .iter()
                .map(|error| Value::String(error.clone()))
                .collect(),
        ),
    );
    Err(EditorSourceVerificationError {
        details: verification.failed,
    }
    .into())
}

struct ProtectedWritePlan {
    writes: Vec<Value>,
    apply_offline: bool,
}

fn prepare_protected_writes(
    args: &PushEditorChangesArgs,
    bridge: &BridgeServer,
    summary: &mut Map<String, Value>,
    pre_routed: Vec<Value>,
) -> Result<ProtectedWritePlan> {
    let mut reported = summary
        .get("protectedWrites")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut studio_pending = vec![true; reported.len()];
    let pre_routed_count = pre_routed.len();
    let pre_routed =
        protected_root_write_rows_with_live_values(bridge, pre_routed).unwrap_or_else(|rows| rows);
    studio_pending.extend(
        pre_routed
            .iter()
            .map(|row| !protected_write_matches_previous(row)),
    );
    reported.extend(pre_routed);
    if pre_routed_count > 0 {
        summary.insert(
            "protectedPreRouted".to_string(),
            Value::Number(Number::from(pre_routed_count as u64)),
        );
    }
    let reported_count = reported.len();
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let applicable = reported
        .into_iter()
        .enumerate()
        .filter(|(_, row)| {
            !is_externally_managed_protected_write(row)
                && is_user_facing_protected_write(row, database)
        })
        .map(|(index, row)| (studio_pending[index], row))
        .collect::<Vec<_>>();
    let unavailable_count = reported_count - applicable.len();
    let applicable_rows = applicable
        .iter()
        .map(|(_, row)| row.clone())
        .collect::<Vec<_>>();
    let enriched_rows = if args.no_review {
        applicable_rows
    } else {
        local_place_path_for_bridge(bridge)
            .and_then(|path| {
                protected_write_rows_with_previous_values(&path, &applicable_rows).ok()
            })
            .unwrap_or(applicable_rows)
    };
    let enriched = applicable
        .into_iter()
        .zip(enriched_rows)
        .map(|((studio_pending, _), row)| (studio_pending, row))
        .collect::<Vec<_>>();
    let writes = enriched
        .iter()
        .filter(|(studio_pending, row)| *studio_pending || !protected_write_matches_previous(row))
        .map(|(_, row)| row.clone())
        .collect::<Vec<_>>();
    if args.no_review && !writes.is_empty() {
        bail!(
            "Studio could not apply {} protected write(s); all changes were rolled back and remain pending",
            writes.len()
        );
    }
    summary.remove("protectedWrites");
    if !writes.is_empty() {
        summary.insert(
            "protectedPending".to_string(),
            Value::Number(Number::from(writes.len() as u64)),
        );
    }
    if unavailable_count > 0 {
        summary.insert(
            "unavailableProtectedSkipped".to_string(),
            Value::Number(Number::from(unavailable_count as u64)),
        );
    }
    let already_current = enriched.len() - writes.len();
    if already_current > 0 {
        summary.insert(
            "protectedAlreadyCurrent".to_string(),
            Value::Number(Number::from(already_current as u64)),
        );
    }
    let apply_offline = !args.no_review
        && !writes.is_empty()
        && (args.yes || global_yes() || request_protected_write_review(bridge, &writes)?);
    if crate::app::output::global_log_enabled(5) && !writes.is_empty() {
        eprintln!(
            "[renium] protected writes: {}",
            serde_json::to_string(&writes)?
        );
    }
    Ok(ProtectedWritePlan {
        writes,
        apply_offline,
    })
}

struct CollectedPushOptions<'a> {
    started: Instant,
    projection: Option<&'a config::ProjectionStage>,
    prepared_binary_import: Option<EditorBinaryImport>,
    guard: Option<&'a StudioChangeGuard>,
    validate_project: Option<&'a dyn Fn() -> Result<()>>,
}

fn push_editor_changes_with_collected(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
    mut changes: EditorChangeSet,
    options: CollectedPushOptions<'_>,
) -> Result<serde_json::Map<String, Value>> {
    let CollectedPushOptions {
        started,
        projection,
        prepared_binary_import,
        guard,
        validate_project,
    } = options;
    let phase_started = Instant::now();
    apply_files_to_studio_filters(&args, bridge, &mut changes, projection)?;
    log_timing("native editor push filters", phase_started);
    let pre_routed_protected_writes = take_pre_routed_protected_writes(&mut changes);
    let review_skipped = !args.no_review
        && !args.yes
        && !global_yes()
        && (!changes.instance_changes.is_empty()
            || !changes.source_changes.is_empty()
            || !changes.property_changes.is_empty())
        && !request_editor_push_review(bridge, &changes)?;
    let binary_import = if review_skipped {
        None
    } else if prepared_binary_import.is_some() {
        prepared_binary_import
    } else {
        build_editor_binary_import(&args, &changes, bridge)?
    };
    let unstaged_replacements = changes
        .instance_changes
        .iter()
        .filter(|change| change.mode == "reconcileService" && change.allow_deletes)
        .map(|change| change.service.as_str())
        .collect::<Vec<_>>();
    if binary_import.is_none()
        && !changes.files_to_studio_filters_active
        && !unstaged_replacements.is_empty()
    {
        bail!(
            "A full replacement of {} could not be staged; Studio was not changed",
            unstaged_replacements.join(", ")
        );
    }
    let mut history_transaction = if review_skipped || binary_import.is_some() {
        None
    } else {
        save_editor_history_entries(bridge, &args.project.project_root, &changes)?
    };
    let phase_started = Instant::now();
    if !review_skipped && let Some(validate_project) = validate_project {
        validate_project()?;
    }
    let mut transaction = if review_skipped {
        None
    } else {
        EditorTransaction::begin(bridge, &changes, binary_import.as_ref(), guard)?
    };
    let result = (|| {
        log_timing("native editor transaction begin", phase_started);
        let mut summary = if review_skipped {
            skipped_editor_summary(&changes)
        } else {
            let transaction_id = transaction.as_ref().map(|value| value.id.as_str());
            let phase_started = Instant::now();
            let result = send_editor_change_batches(
                bridge,
                &changes,
                args.probe_events,
                false,
                false,
                binary_import.as_ref(),
                transaction_id,
            );
            log_timing("native editor change batches", phase_started);
            match result {
                Ok(summary) => summary,
                Err(error) => {
                    if let Some(transaction) = transaction.as_mut()
                        && let Err(rollback_error) = transaction.rollback()
                    {
                        return Err(error
                            .context(format!("Studio rollback also failed: {rollback_error:#}")));
                    }
                    return Err(error);
                }
            }
        };
        if !review_skipped {
            let errors = summary.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
            if summary.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
                bail!("Studio rejected or failed one or more editor push changes");
            }
        }
        if args.verify_sources && !review_skipped {
            verify_pushed_sources(
                bridge,
                &changes,
                transaction.as_ref().map(|value| value.id.as_str()),
                &mut summary,
            )?;
        }
        let phase_started = Instant::now();
        let protected =
            prepare_protected_writes(&args, bridge, &mut summary, pre_routed_protected_writes)?;
        log_timing("native editor protected write preparation", phase_started);
        let phase_started = Instant::now();
        if !review_skipped && let Some(validate_project) = validate_project {
            validate_project()?;
        }
        let settings_transaction = if review_skipped {
            None
        } else {
            Some(EditorSettingsTransaction::apply(&changes)?)
        };
        if let Some(history_transaction) = history_transaction.as_mut() {
            history_transaction.publish()?;
        }
        log_timing("native editor settings apply", phase_started);
        let phase_started = Instant::now();
        let mut commit_status = None;
        if protected.apply_offline {
            let result = apply_protected_writes_offline(bridge, &args, &protected.writes)?;
            if let Some(transaction) = transaction.as_mut() {
                transaction.disarm();
            }
            summary.insert("protectedOfflineApply".to_string(), result);
            summary.insert(
                "protectedApplied".to_string(),
                Value::Number(serde_json::Number::from(protected.writes.len() as u64)),
            );
            summary.remove("protectedPending");
        } else if let Some(transaction) = transaction.as_mut() {
            match transaction.commit() {
                Ok(status) => commit_status = Some(status),
                Err(commit_error) => {
                    if !transaction.active {
                        return Err(commit_error);
                    }
                    if let Err(rollback_error) = transaction.rollback() {
                        return Err(commit_error
                            .context(format!("Studio rollback also failed: {rollback_error:#}")));
                    }
                    return Err(commit_error
                        .context("Studio rejected the commit; its changes were rolled back"));
                }
            }
        }
        if let Some(status) = commit_status {
            add_editor_commit_status(&mut summary, status);
        }
        log_timing("native editor transaction commit", phase_started);
        if let Some(settings_transaction) = settings_transaction {
            settings_transaction.commit();
        }
        if let Some(history_transaction) = history_transaction {
            history_transaction.commit();
        }
        log_global(
            5,
            format_args!(
                "[renium] editor push done: elapsed_ms={:.1}, summary={}",
                elapsed_ms(started),
                Value::Object(summary.clone())
            ),
        );
        let errors = summary.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
        if summary.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
            bail!("Studio rejected or failed one or more editor push changes");
        }
        Ok(summary)
    })();
    match result {
        Ok(summary) => Ok(summary),
        Err(error) => {
            if let Some(transaction) = transaction.as_mut()
                && transaction.active
                && let Err(rollback_error) = transaction.rollback()
            {
                return Err(
                    error.context(format!("Studio rollback also failed: {rollback_error:#}"))
                );
            }
            Err(error)
        }
    }
}

fn listen_editor_oneshot_bridge(
    label: &str,
    host: &str,
    ports_raw: &str,
    wait_seconds: f64,
) -> Result<BridgeServer> {
    let ports = parse_bridge_ports(ports_raw)?;
    let (bridge, listen_metrics) = BridgeServer::listen(host, &ports, wait_seconds)?;
    log_global(
        5,
        format_args!(
            "[renium] editor {label} bridge ready: channels={}/{}, bind_ms={:.1}, handshake_ms={:.1}",
            bridge.channel_count(),
            bridge.expected_channel_count(),
            listen_metrics.bind_ms,
            listen_metrics.wait_for_channels_ms
        ),
    );
    Ok(bridge)
}

fn apply_editor_change_with_warm_bridge(
    bridge: &BridgeServer,
    label: &str,
    collect: impl FnOnce() -> Result<EditorChangeSet>,
) -> Result<Map<String, Value>> {
    let started = Instant::now();
    let changes = collect()?;
    if !request_editor_push_review(bridge, &changes)? {
        let summary = skipped_editor_summary(&changes);
        log_global(
            5,
            format_args!(
                "[renium] editor {label} apply done: elapsed_ms={:.1}, summary={}",
                elapsed_ms(started),
                Value::Object(summary.clone())
            ),
        );
        return Ok(summary);
    }
    let mut transaction = EditorTransaction::begin(bridge, &changes, None, None)?;
    let result = (|| {
        let transaction_id = transaction.as_ref().map(|value| value.id.as_str());
        let mut summary = send_editor_change_batches(
            bridge,
            &changes,
            false,
            false,
            false,
            None,
            transaction_id,
        )?;
        let errors = summary.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
        if summary.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
            bail!("Studio rejected or failed editor {label} apply");
        }
        if let Some(transaction) = transaction.as_mut() {
            let status = transaction.commit()?;
            add_editor_commit_status(&mut summary, status);
        }
        log_global(
            5,
            format_args!(
                "[renium] editor {label} apply done: elapsed_ms={:.1}, summary={}",
                elapsed_ms(started),
                Value::Object(summary.clone())
            ),
        );
        Ok(summary)
    })();
    match result {
        Ok(summary) => Ok(summary),
        Err(error) => {
            if let Some(transaction) = transaction.as_mut()
                && transaction.active
                && let Err(rollback_error) = transaction.rollback()
            {
                return Err(
                    error.context(format!("Studio rollback also failed: {rollback_error:#}"))
                );
            }
            Err(error)
        }
    }
}

fn print_editor_push_summary(summary: &serde_json::Map<String, Value>) -> Result<()> {
    print_json_output(&Value::Object(summary.clone()), global_pretty_output(false))
}

fn print_direct_editor_summary(summary: &serde_json::Map<String, Value>) -> Result<()> {
    let changed = [
        "attributeUpdated",
        "instanceCreated",
        "instanceDeleted",
        "instanceReplaced",
        "propertyUpdated",
        "sourceCreated",
        "sourceDeleted",
        "sourceUpdated",
    ]
    .into_iter()
    .any(|key| summary.get(key).and_then(Value::as_f64).unwrap_or(0.0) > 0.0);
    let mut output = Map::from_iter([
        ("ok".to_string(), Value::Bool(true)),
        ("changed".to_string(), Value::Bool(changed)),
    ]);
    if let Some(packages) = summary.get("autoDesyncedPackages") {
        output.insert("autoDesyncedPackages".to_string(), packages.clone());
    }
    print_json_output(&Value::Object(output), false)
}

pub(crate) fn apply_editor_property(mut args: ApplyEditorPropertyArgs) -> Result<()> {
    resolve_editor_property_source_file(&mut args)?;
    apply_configured_project_layout(
        &mut args.target.project.project_root,
        &mut args.target.project.src_root,
    )?;
    let mut parameters = editor_mutation_parameters(&args.target)?;
    parameters.insert("editor".to_string(), Value::Bool(true));
    parameters.insert("scope".to_string(), Value::String(args.scope.clone()));
    parameters.insert("property".to_string(), Value::String(args.property.clone()));
    parameters.insert(
        "value".to_string(),
        serde_json::from_str(
            args.value_json
                .as_deref()
                .context("Provide --value-json or --source-file")?,
        )
        .context("Failed to parse --value-json")?,
    );
    let approved = !args.no_review && (args.yes || global_yes());
    if let Some(result) = try_daemon_control_request(
        op::SET_PROPERTY,
        Some(&args.target.project.project_root),
        Value::Object(parameters),
        approved,
    )? {
        return print_direct_editor_summary(
            result
                .as_object()
                .context("The daemon returned an invalid property result")?,
        );
    }
    let bridge = listen_editor_oneshot_bridge(
        "property",
        &args.target.bridge.host,
        &args.target.bridge.ports,
        args.target.bridge.wait_seconds,
    )?;
    let summary = apply_editor_property_with_warm_bridge(args, &bridge)?;
    print_direct_editor_summary(&summary)
}

pub(crate) fn apply_editor_property_with_warm_bridge(
    mut args: ApplyEditorPropertyArgs,
    bridge: &BridgeServer,
) -> Result<Map<String, Value>> {
    resolve_editor_property_source_file(&mut args)?;
    let started = Instant::now();
    let changes = collect_direct_editor_property_change(&args)?;
    let verify_sources = !changes.source_changes.is_empty();
    push_editor_changes_with_collected(
        PushEditorChangesArgs {
            no_review: args.no_review,
            yes: args.yes || global_yes(),
            override_packages: args.target.override_packages,
            verify_sources,
            ..PushEditorChangesArgs::new(args.target.project, args.target.bridge)
        },
        bridge,
        changes,
        CollectedPushOptions {
            started,
            projection: None,
            prepared_binary_import: None,
            guard: None,
            validate_project: None,
        },
    )
}

fn resolve_editor_property_source_file(args: &mut ApplyEditorPropertyArgs) -> Result<()> {
    let Some(path) = args.source_file.take() else {
        return Ok(());
    };
    if !args.scope.eq_ignore_ascii_case("property") || args.property != "Source" {
        bail!("--source-file requires --property Source");
    }
    let source = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read source file {}", path.display()))?;
    args.value_json = Some(serde_json::to_string(&source)?);
    Ok(())
}

fn collect_direct_editor_property_change(
    args: &ApplyEditorPropertyArgs,
) -> Result<EditorChangeSet> {
    let target = parse_direct_editor_target(&args.target)?;
    let property = args.property.trim().to_string();
    if property.is_empty() {
        bail!("--property is required");
    }
    if target.path_segments.is_empty() {
        bail!("--path-segments-json must contain at least one segment");
    }
    if is_externally_managed_editor_property(
        &target.service,
        &target.class_name,
        &target.path_segments,
        &property,
    ) {
        bail!(
            "{}.{} is managed through Roblox Game Settings",
            target.service,
            property
        )
    }
    let value: Value = serde_json::from_str(
        args.value_json
            .as_deref()
            .context("Provide --value-json or --source-file")?,
    )
    .context("Failed to parse --value-json")?;

    if args.scope.eq_ignore_ascii_case("property") && property == "Source" {
        let source = value
            .as_str()
            .context("Source must be a string")?
            .to_string();
        let mut changes = EditorChangeSet::default();
        changes.source_changes.push(EditorSourceChange {
            service: target.service,
            settings_id: target.settings_id,
            path_segments: target.path_segments,
            path_ordinals: target.path_ordinals,
            class_name: target.class_name,
            source: Some(source),
            deleted: false,
        });
        return Ok(changes);
    }

    let mut properties = Map::new();
    let mut attributes = Map::new();
    let mut deleted_attributes = Vec::new();
    if args.scope.eq_ignore_ascii_case("attribute") {
        if value.is_null() {
            deleted_attributes.push(property);
        } else {
            attributes.insert(property, value);
        }
    } else {
        properties.insert(property, value);
    }

    let mut changes = EditorChangeSet::default();
    changes.property_changes.push(EditorPropertyChange {
        service: target.service,
        settings_id: target.settings_id,
        path_segments: target.path_segments,
        path_ordinals: target.path_ordinals,
        class_name: target.class_name,
        properties,
        reset_properties: Vec::new(),
        attributes,
        deleted_attributes,
    });
    Ok(changes)
}

pub(crate) fn apply_editor_delete(args: ApplyEditorDeleteArgs) -> Result<()> {
    let mut parameters = editor_mutation_parameters(&args.target)?;
    parameters.insert("editor".to_string(), Value::Bool(true));
    if let Some(result) = try_daemon_control_request(
        op::REMOVE,
        Some(&args.target.project.project_root),
        Value::Object(parameters),
        false,
    )? {
        return print_direct_editor_summary(
            result
                .as_object()
                .context("The daemon returned an invalid delete result")?,
        );
    }
    let bridge = listen_editor_oneshot_bridge(
        "delete",
        &args.target.bridge.host,
        &args.target.bridge.ports,
        args.target.bridge.wait_seconds,
    )?;
    let summary = apply_editor_delete_with_warm_bridge(args, &bridge)?;
    print_direct_editor_summary(&summary)
}

fn editor_mutation_parameters(target: &EditorMutationArgs) -> Result<Map<String, Value>> {
    let mut parameters = Map::new();
    parameters.insert("service".to_string(), Value::String(target.service.clone()));
    parameters.insert(
        "settingsId".to_string(),
        target
            .settings_id
            .clone()
            .map_or(Value::Null, Value::String),
    );
    parameters.insert(
        "className".to_string(),
        Value::String(target.class_name.clone()),
    );
    parameters.insert(
        "pathSegments".to_string(),
        serde_json::from_str(&target.path_segments_json).context("Invalid --path-segments-json")?,
    );
    parameters.insert(
        "pathOrdinals".to_string(),
        serde_json::from_str(&target.path_ordinals_json).context("Invalid --path-ordinals-json")?,
    );
    parameters.insert(
        "overridePackages".to_string(),
        Value::Bool(target.override_packages),
    );
    parameters.insert("srcDir".to_string(), json!(target.project.src_root));
    parameters.insert(
        "bridgeWaitSeconds".to_string(),
        json!(target.bridge.wait_seconds),
    );
    parameters.insert("bridgePorts".to_string(), json!(target.bridge.ports));
    Ok(parameters)
}

pub(crate) fn apply_editor_delete_with_warm_bridge(
    args: ApplyEditorDeleteArgs,
    bridge: &BridgeServer,
) -> Result<Map<String, Value>> {
    apply_editor_change_with_warm_bridge(bridge, "delete", || {
        collect_direct_editor_delete_change(args)
    })
}

pub(crate) fn collect_direct_editor_delete_change(
    args: ApplyEditorDeleteArgs,
) -> Result<EditorChangeSet> {
    let target = parse_direct_editor_target(&args.target)?;
    if target.path_segments.len() <= 1 {
        bail!("Refusing to delete a service root");
    }

    let mut changes = EditorChangeSet::default();
    changes.instance_changes.push(EditorInstanceChange {
        mode: "deleteInstances".to_string(),
        service: target.service,
        allow_deletes: false,
        instances: vec![EditorInstanceDescriptor {
            settings_id: target.settings_id.unwrap_or_default(),
            path_segments: target.path_segments,
            path_ordinals: target.path_ordinals,
            class_name: target.class_name,
            ..EditorInstanceDescriptor::default()
        }],
        preserve_instances: Vec::new(),
    });
    Ok(changes)
}

struct DirectEditorTarget {
    service: String,
    settings_id: Option<String>,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
    class_name: String,
}

fn parse_direct_editor_target(target: &EditorMutationArgs) -> Result<DirectEditorTarget> {
    let service = target.service.trim().to_string();
    if service.is_empty() {
        bail!("--service is required");
    }
    let mut path_segments: Vec<String> = serde_json::from_str(&target.path_segments_json)
        .context("Failed to parse --path-segments-json")?;
    let mut path_ordinals: Vec<usize> = serde_json::from_str(&target.path_ordinals_json)
        .context("Failed to parse --path-ordinals-json")?;
    if path_segments.first().map(String::as_str) != Some(service.as_str()) {
        path_segments.insert(0, service.clone());
        if !path_ordinals.is_empty() {
            path_ordinals.insert(0, 1);
        }
    }
    reject_direct_read_only_package_change(
        &target.project.project_root,
        target.override_packages,
        &service,
        &path_segments,
        &path_ordinals,
    )?;
    Ok(DirectEditorTarget {
        service,
        settings_id: target
            .settings_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        path_segments,
        path_ordinals,
        class_name: target.class_name.clone(),
    })
}

fn reject_direct_read_only_package_change(
    project_root: &Path,
    override_packages: bool,
    service: &str,
    path_segments: &[String],
    path_ordinals: &[usize],
) -> Result<()> {
    if override_packages {
        return Ok(());
    }
    let Some(loaded) = config::try_load_project(None, Some(project_root))? else {
        return Ok(());
    };
    build_loaded_project_link_enforcement(&loaded, false)?.reject_read_only_package_path(
        service,
        path_segments,
        path_ordinals,
    )
}

#[derive(Default)]
struct EditorSourceVerification {
    verified: usize,
    failed_indexes: Vec<usize>,
    failed: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct EditorSourceVerificationError {
    pub(crate) details: Vec<String>,
}

impl std::fmt::Display for EditorSourceVerificationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Studio source verification failed for {} script(s): {}",
            self.details.len(),
            self.details.join("; ")
        )
    }
}

impl std::error::Error for EditorSourceVerificationError {}

#[derive(Deserialize)]
struct LiveSourceBatch {
    rows: Vec<LiveSourceRow>,
}

#[derive(Deserialize)]
struct LiveSourceRow {
    index: usize,
    source: Option<String>,
    error: Option<String>,
}

fn fetch_live_editor_sources(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
    indexes: &[usize],
) -> Result<HashMap<usize, std::result::Result<String, String>>> {
    let mut sources = HashMap::with_capacity(indexes.len());
    for batch in indexes.chunks(16) {
        let selectors = batch
            .iter()
            .map(|index| {
                let change = &changes.source_changes[*index];
                json!({
                    "index": index,
                    "pathSegments": &change.path_segments,
                    "pathOrdinals": &change.path_ordinals,
                })
            })
            .collect::<Vec<_>>();
        let response = bridge
            .call("getLiveSourceBatch", json!({ "selectors": selectors }))
            .and_then(|value| {
                serde_json::from_value::<LiveSourceBatch>(value)
                    .context("Studio returned an invalid live source batch")
            })?;
        let batch_indexes = batch.iter().copied().collect::<HashSet<_>>();
        for row in response.rows {
            if !batch_indexes.contains(&row.index) || sources.contains_key(&row.index) {
                continue;
            }
            let value = match (row.source, row.error) {
                (Some(source), _) => Ok(source),
                (None, Some(error)) => Err(error),
                (None, None) => Err("Studio did not return the script Source".to_string()),
            };
            sources.insert(row.index, value);
        }
        for index in batch {
            sources
                .entry(*index)
                .or_insert_with(|| Err("Studio did not return the script Source".to_string()));
        }
    }
    Ok(sources)
}

fn verify_editor_source_changes(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
) -> Result<EditorSourceVerification> {
    let mut pending = changes
        .source_changes
        .iter()
        .enumerate()
        .filter_map(|(index, change)| (!change.deleted && change.source.is_some()).then_some(index))
        .collect::<Vec<_>>();
    let verified = pending.len();
    let mut failures = HashMap::<usize, String>::with_capacity(verified);
    let retry_delays = [
        Duration::ZERO,
        Duration::from_millis(20),
        Duration::from_millis(80),
    ];

    for (attempt, delay) in retry_delays.into_iter().enumerate() {
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let sources = match fetch_live_editor_sources(bridge, changes, &pending) {
            Ok(sources) => sources,
            Err(error) if attempt + 1 < retry_delays.len() => {
                crate::log_global(
                    3,
                    format_args!("Studio source verification read will retry: {error:#}"),
                );
                continue;
            }
            Err(error) => return Err(error),
        };
        pending.retain(|index| {
            let change = &changes.source_changes[*index];
            let expected = change.source.as_deref().unwrap_or_default();
            let source_key = editor_source_key(change);
            let failure = match sources.get(index) {
                Some(Ok(actual)) if editor_sources_match(expected, actual) => None,
                Some(Ok(actual)) => Some(format!(
                    "{} source mismatch: editor_len={} studio_len={} editor_hash={} studio_hash={} key={}",
                    change.path_segments.join("."),
                    expected.len(),
                    actual.len(),
                    fnv1a_hex(expected.as_bytes()),
                    fnv1a_hex(actual.as_bytes()),
                    source_key,
                )),
                Some(Err(error)) => Some(format!(
                    "{} source could not be read: {} key={}",
                    change.path_segments.join("."),
                    error,
                    source_key,
                )),
                None => Some(format!(
                    "{} source was omitted by Studio: key={}",
                    change.path_segments.join("."),
                    source_key,
                )),
            };
            if let Some(failure) = failure {
                failures.insert(*index, failure);
                true
            } else {
                failures.remove(index);
                false
            }
        });
        if pending.is_empty() {
            break;
        }
    }

    let failed_indexes = pending;
    let failed = failed_indexes
        .iter()
        .filter_map(|index| failures.remove(index))
        .collect();
    Ok(EditorSourceVerification {
        verified,
        failed_indexes,
        failed,
    })
}

fn editor_sources_match(expected: &str, actual: &str) -> bool {
    expected == actual
        || normalized_source_bytes(expected.as_bytes())
            .eq(normalized_source_bytes(actual.as_bytes()))
        || strip_one_source_line_ending(expected).is_some_and(|expected| {
            normalized_source_bytes(expected.as_bytes())
                .eq(normalized_source_bytes(actual.as_bytes()))
        })
}

fn strip_one_source_line_ending(source: &str) -> Option<&str> {
    source
        .strip_suffix("\r\n")
        .or_else(|| source.strip_suffix('\r'))
        .or_else(|| source.strip_suffix('\n'))
}

fn editor_source_key(change: &EditorSourceChange) -> String {
    editor_source_key_for_path(&change.path_segments, &change.path_ordinals)
}

fn editor_source_key_from_target(target: &EditorSourceTarget) -> String {
    editor_source_key_for_path(&target.path_segments, &target.path_ordinals)
}

fn editor_source_key_for_path(path_segments: &[String], path_ordinals: &[usize]) -> String {
    if path_ordinals.len() == path_segments.len() {
        return format!(
            "pathord:{}:{}",
            path_ordinals
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(","),
            path_segments.join(".")
        );
    }
    format!("path:{}", path_segments.join("."))
}

pub(crate) fn is_lua_source_class(class_name: &str) -> bool {
    matches!(class_name, "Script" | "LocalScript" | "ModuleScript")
}

pub(crate) fn expand_editor_changed_paths(args: &PushEditorChangesArgs) -> Result<Vec<PathBuf>> {
    let mut paths = args.changed_paths.clone();
    for list_path in &args.changed_paths_files {
        let raw = fs::read_to_string(list_path).with_context(|| {
            format!("Failed to read changed paths file {}", list_path.display())
        })?;
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            paths.push(PathBuf::from(trimmed));
        }
    }
    let mut expanded = Vec::new();
    for path in paths {
        let absolute = absolutize_under(&args.project.project_root, &path);
        if !absolute.is_dir() {
            expanded.push(path);
            continue;
        }
        expanded.push(path);
        for entry in WalkDir::new(&absolute) {
            let entry = entry.with_context(|| format!("Failed to walk {}", absolute.display()))?;
            if entry.file_type().is_file() {
                expanded.push(entry.into_path());
            }
        }
    }
    expanded.sort();
    expanded.dedup();
    Ok(expanded)
}

fn collect_editor_full_paths(src_root: &Path) -> Result<Vec<PathBuf>> {
    if !src_root.is_dir() {
        bail!(
            "Cannot collect editor changes from missing source directory {}",
            src_root.display()
        );
    }
    let mut paths = WalkDir::new(src_root)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() => Some(Ok(entry.into_path())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to walk {}", src_root.display()))?;
    paths.sort();
    Ok(paths)
}

pub(crate) struct EditorSettingsTransaction {
    _locks: Vec<SettingsFileLock>,
    published: Vec<(PathBuf, Option<PathBuf>)>,
    temporary: Vec<PathBuf>,
    active: bool,
}

impl EditorSettingsTransaction {
    pub(crate) fn apply(changes: &EditorChangeSet) -> Result<Self> {
        let mut transaction = Self {
            _locks: Vec::with_capacity(changes.settings_writes.len()),
            published: Vec::new(),
            temporary: Vec::new(),
            active: true,
        };
        let result = (|| -> Result<()> {
            let mut lock_paths = changes
                .settings_writes
                .iter()
                .map(|write| (path_key(&write.path), &write.path))
                .collect::<Vec<_>>();
            lock_paths.sort_by(|left, right| left.0.cmp(&right.0));
            for (_, path) in lock_paths {
                transaction._locks.push(acquire_settings_file_lock(path)?);
            }
            for write in &changes.settings_writes {
                if settings_file_hash(&write.path)? != write.expected_hash {
                    bail!(
                        "{} changed while the Studio update was being prepared; retry the sync",
                        write.path.display()
                    );
                }
            }
            for (index, write) in changes.settings_writes.iter().enumerate() {
                let file_name = write
                    .path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("settings.renium");
                let temporary = write.path.with_file_name(format!(
                    ".{file_name}.renium-write-{}-{index}",
                    std::process::id()
                ));
                let _ = fs::remove_file(&temporary);
                write.document.write_file(&temporary)?;
                transaction.temporary.push(temporary);
            }
            for (index, write) in changes.settings_writes.iter().enumerate() {
                let temporary = transaction.temporary[index].clone();
                let backup = if write.path.exists() {
                    let file_name = write
                        .path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or("settings.renium");
                    let backup = write.path.with_file_name(format!(
                        ".{file_name}.renium-previous-{}-{index}",
                        std::process::id()
                    ));
                    let _ = fs::remove_file(&backup);
                    fs::rename(&write.path, &backup)
                        .with_context(|| format!("Failed to preserve {}", write.path.display()))?;
                    Some(backup)
                } else {
                    None
                };
                if let Err(error) = fs::rename(&temporary, &write.path) {
                    if let Some(backup) = backup.as_ref() {
                        let _ = fs::rename(backup, &write.path);
                    }
                    return Err(error)
                        .with_context(|| format!("Failed to publish {}", write.path.display()));
                }
                transaction.published.push((write.path.clone(), backup));
            }
            Ok(())
        })();
        if let Err(error) = result {
            drop(transaction);
            return Err(error);
        }
        Ok(transaction)
    }

    pub(crate) fn commit(mut self) {
        self.active = false;
        for (_, backup) in &self.published {
            if let Some(backup) = backup {
                let _ = fs::remove_file(backup);
            }
        }
        for temporary in &self.temporary {
            let _ = fs::remove_file(temporary);
        }
    }
}

impl Drop for EditorSettingsTransaction {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        for (destination, backup) in self.published.iter().rev() {
            let _ = fs::remove_file(destination);
            if let Some(backup) = backup {
                let _ = fs::rename(backup, destination);
            }
        }
        for temporary in &self.temporary {
            let _ = fs::remove_file(temporary);
        }
    }
}

fn editor_project_roots(args: &PushEditorChangesArgs) -> Result<(PathBuf, PathBuf)> {
    let project_root = if args.project.project_root.exists() {
        strip_extended_prefix(canonical_path(&args.project.project_root).with_context(|| {
            format!(
                "Failed to resolve project root: {}",
                args.project.project_root.display()
            )
        })?)
    } else {
        args.project.project_root.clone()
    };
    let src_root = absolutize_under(&project_root, &args.project.src_root);
    Ok((project_root, src_root))
}

pub(crate) fn collect_editor_changes(args: &PushEditorChangesArgs) -> Result<EditorChangeSet> {
    let (project_root, src_root) = editor_project_roots(args)?;
    let link_enforcement = if args.override_packages {
        LinkEnforcement::default()
    } else {
        build_link_enforcement(&project_root, &src_root, args.link_cache_dir.as_deref())?
    };
    collect_editor_changes_with_link_enforcement(args, &project_root, &src_root, &link_enforcement)
}

fn canonical_editor_changed_path(project_root: &Path, changed_path: &Path) -> PathBuf {
    let absolute_path = absolutize_under(project_root, changed_path);
    match canonical_path(&absolute_path) {
        Ok(canonical) => strip_extended_prefix(canonical),
        Err(_) => absolute_path
            .parent()
            .and_then(|parent| canonical_path(parent).ok())
            .map(strip_extended_prefix)
            .and_then(|parent| absolute_path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| strip_extended_prefix(absolute_path)),
    }
}

fn reject_read_only_changed_path(
    link_enforcement: &LinkEnforcement,
    service: &str,
    protected_path: Option<(&[String], &[usize])>,
    absolute_path: &Path,
    full_reconcile: bool,
) -> Result<bool> {
    let Some(target) = protected_path.and_then(|(path, ordinals)| {
        link_enforcement.read_only_package_for_path(service, path, ordinals)
    }) else {
        return Ok(false);
    };
    if full_reconcile {
        return Ok(true);
    }
    bail!(
        "Cannot edit {} because it belongs to read-only link \"{}\" at {}.{}. Use --override-packages to replace it intentionally.",
        absolute_path.display(),
        target.link_id,
        target.service,
        target.target_segments.join(".")
    )
}

#[derive(Default)]
struct EditorChangedServices {
    settings: HashSet<String>,
    reconcile: HashSet<String>,
    target_upsert: HashSet<String>,
    dirty: HashSet<String>,
}

fn settings_value_references_target(
    value: &Value,
    target_index: usize,
    target_settings_id: &str,
    target_path_segments: &[String],
    target_path_ordinals: &[usize],
) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| {
            settings_value_references_target(
                value,
                target_index,
                target_settings_id,
                target_path_segments,
                target_path_ordinals,
            )
        }),
        Value::Object(object) => {
            let directly_references_target = is_reference_object(object)
                && (object
                    .get("instanceIndex")
                    .and_then(settings_reference_index)
                    == Some(target_index)
                    || ["settingsId", "instanceId", "referent", "ref"]
                        .iter()
                        .any(|key| {
                            object.get(*key).and_then(Value::as_str) == Some(target_settings_id)
                        })
                    || (object
                        .get("pathSegments")
                        .and_then(Value::as_array)
                        .is_some_and(|segments| {
                            segments.len() == target_path_segments.len()
                                && segments
                                    .iter()
                                    .zip(target_path_segments)
                                    .all(|(segment, expected)| segment.as_str() == Some(expected))
                        })
                        && object
                            .get("pathOrdinals")
                            .and_then(Value::as_array)
                            .is_none_or(|ordinals| {
                                ordinals.len() == target_path_ordinals.len()
                                    && ordinals.iter().zip(target_path_ordinals).all(
                                        |(ordinal, expected)| {
                                            ordinal.as_u64() == Some(*expected as u64)
                                        },
                                    )
                            })));
            directly_references_target
                || object.values().any(|value| {
                    settings_value_references_target(
                        value,
                        target_index,
                        target_settings_id,
                        target_path_segments,
                        target_path_ordinals,
                    )
                })
        }
        _ => false,
    }
}

fn append_editor_reference_repairs(
    changes: &mut EditorChangeSet,
    before: &SettingsBytecode,
    after: &SettingsBytecode,
    service: &str,
    target_settings_id: &str,
) {
    let Some(target_index) = document_instance_index_by_settings_id(before, target_settings_id)
    else {
        return;
    };
    let before_paths = build_editor_instance_paths(before, service);
    let Some(target_path) = before_paths.get(target_index).and_then(Clone::clone) else {
        return;
    };
    let after_paths = build_editor_instance_paths(after, service);
    let after_settings_ids = after
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<Vec<_>>();
    let after_indices = after
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();

    for before_instance in &before.instances {
        let Some(after_index) = after_indices
            .get(before_instance.settings_id.as_str())
            .copied()
        else {
            continue;
        };
        let after_instance = &after.instances[after_index];
        let mut properties = Map::new();
        let mut reset_properties = Vec::new();
        for (name, value) in &before_instance.properties {
            if !settings_value_references_target(
                value,
                target_index,
                target_settings_id,
                &target_path.path_segments,
                &target_path.path_ordinals,
            ) {
                continue;
            }
            if let Some(value) = after_instance.properties.get(name) {
                properties.insert(
                    name.clone(),
                    normalize_editor_bridge_value(value, None, &after_paths, &after_settings_ids),
                );
            } else {
                reset_properties.push(name.clone());
            }
        }

        let mut attributes = Map::new();
        let mut deleted_attributes = Vec::new();
        for (name, value) in &before_instance.attributes {
            if !settings_value_references_target(
                value,
                target_index,
                target_settings_id,
                &target_path.path_segments,
                &target_path.path_ordinals,
            ) {
                continue;
            }
            if let Some(value) = after_instance.attributes.get(name) {
                attributes.insert(
                    name.clone(),
                    normalize_editor_bridge_value(value, None, &after_paths, &after_settings_ids),
                );
            } else {
                deleted_attributes.push(name.clone());
            }
        }

        if properties.is_empty()
            && reset_properties.is_empty()
            && attributes.is_empty()
            && deleted_attributes.is_empty()
        {
            continue;
        }
        let Some(path) = after_paths.get(after_index).and_then(Clone::clone) else {
            continue;
        };
        if !path.is_descendant_of(service) {
            continue;
        }
        changes.property_changes.push(EditorPropertyChange {
            service: service.to_string(),
            settings_id: Some(after_instance.settings_id.clone()),
            path_segments: path.path_segments,
            path_ordinals: path.path_ordinals,
            class_name: after_instance.class_name.clone(),
            properties,
            reset_properties,
            attributes,
            deleted_attributes,
        });
    }
}

fn sorted_services(services: HashSet<String>) -> Vec<String> {
    let mut services = services.into_iter().collect::<Vec<_>>();
    services.sort();
    services
}

fn validate_read_only_service_changes(
    link_enforcement: &LinkEnforcement,
    changed_services: &HashSet<String>,
    documents: &HashMap<String, Option<Arc<SettingsBytecode>>>,
    src_root: &Path,
) -> Result<()> {
    for target in &link_enforcement.read_only_packages {
        if !changed_services.contains(&target.service) {
            continue;
        }
        let current = documents
            .get(&target.service)
            .and_then(Option::as_ref)
            .map(|document| {
                package_target_fingerprint_with_external_sources(
                    document,
                    &target.service,
                    &src_root.join(&target.service),
                    &target.target_segments,
                    &target.target_ordinals,
                )
            })
            .transpose()?
            .flatten();
        if current.as_deref() != Some(target.expected_fingerprint.as_str()) {
            bail!(
                "Cannot edit read-only link \"{}\" at {}.{}. Apply the link again or use --override-packages to replace it intentionally.",
                target.link_id,
                target.service,
                target.target_segments.join(".")
            );
        }
    }
    Ok(())
}

fn finish_editor_change_collection(
    mut changes: EditorChangeSet,
    documents: &HashMap<String, Option<Arc<SettingsBytecode>>>,
    services: EditorChangedServices,
    property_filter: &EditorPropertyFilter,
    project_root: &Path,
    src_root: &Path,
    link_enforcement: &LinkEnforcement,
) -> Result<EditorChangeSet> {
    let phase_started = Instant::now();
    validate_read_only_service_changes(link_enforcement, &services.settings, documents, src_root)?;
    log_editor_collection_timing("package validation", phase_started);
    for service in sorted_services(services.dirty) {
        if let Some(document) = documents.get(&service).and_then(Option::as_ref) {
            let path = service_settings_path(&src_root.join(&service));
            changes.settings_writes.push(EditorSettingsWrite {
                expected_hash: settings_file_hash(&path)?,
                path,
                document: document.as_ref().clone(),
            });
        }
    }
    for service in sorted_services(services.reconcile) {
        let document = documents
            .get(&service)
            .and_then(Option::as_ref)
            .with_context(|| {
                format!("Cannot reconcile {service}: its settings document is missing")
            })?;
        append_editor_instance_reconcile(&mut changes, document, &service);
    }
    let target_services = services.target_upsert;
    let settings_services = sorted_services(services.settings);
    let phase_started = Instant::now();
    let property_schema_by_class = if settings_services.is_empty() {
        PropertySchemaMap::new()
    } else {
        load_rbx_dom_property_schema(project_root)?.unwrap_or_default()
    };
    let mut changed_services = target_services.iter().cloned().collect::<BTreeSet<_>>();
    changed_services.extend(settings_services.iter().cloned());
    if changed_services.is_empty() {
        log_editor_collection_timing("property schema", phase_started);
        return Ok(changes);
    }
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    log_editor_collection_timing("property schema", phase_started);
    let phase_started = Instant::now();
    for service in changed_services {
        if let Some(document) = documents.get(&service).and_then(Option::as_ref) {
            append_editor_target_changes(
                &mut changes,
                document,
                &service,
                property_filter,
                EditorTargetChangeOptions {
                    upsert_instances: target_services.contains(&service),
                    properties: settings_services.binary_search(&service).is_ok(),
                    property_schema_by_class: &property_schema_by_class,
                    database,
                },
            );
        }
    }
    log_editor_collection_timing("target changes", phase_started);
    Ok(changes)
}

fn collect_settings_file_change(
    args: &PushEditorChangesArgs,
    path: &Path,
    service: &str,
    property_filter: &EditorPropertyFilter,
    changed_services: &mut EditorChangedServices,
) -> bool {
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_service_settings_file_name)
    {
        return false;
    }
    if args.upsert_instances_only {
        changed_services.target_upsert.insert(service.to_string());
    } else {
        changed_services.settings.insert(service.to_string());
        if !property_filter.is_active() {
            changed_services.reconcile.insert(service.to_string());
        } else if !property_filter.settings_ids.is_empty() {
            changed_services.target_upsert.insert(service.to_string());
        }
    }
    true
}

fn collect_editor_changes_with_link_enforcement(
    args: &PushEditorChangesArgs,
    project_root: &Path,
    src_root: &Path,
    link_enforcement: &LinkEnforcement,
) -> Result<EditorChangeSet> {
    collect_editor_changes_with_link_enforcement_and_documents(
        args,
        project_root,
        src_root,
        link_enforcement,
        &mut HashMap::new(),
    )
}

fn load_editor_service_document(
    service: &str,
    src_root: &Path,
    documents: &mut HashMap<String, Option<Arc<SettingsBytecode>>>,
    prepared_documents: &mut HashMap<String, SettingsBytecode>,
) -> Result<()> {
    if documents.contains_key(service) {
        return Ok(());
    }
    let phase_started = Instant::now();
    let document = match prepared_documents.remove(service) {
        Some(document) => Some(Arc::new(document)),
        None => read_editor_service_settings_cached(src_root, service)?,
    };
    documents.insert(service.to_string(), document);
    log_editor_collection_timing("settings decode", phase_started);
    Ok(())
}

fn unique_editor_changed_path(
    project_root: &Path,
    src_root: &Path,
    changed_path: &Path,
    seen_paths: &mut HashSet<String>,
) -> Option<(PathBuf, String)> {
    let absolute_path = canonical_editor_changed_path(project_root, changed_path);
    if !seen_paths.insert(path_key(&absolute_path)) {
        return None;
    }
    let service = service_from_changed_path(src_root, &absolute_path)?;
    Some((absolute_path, service))
}

fn collect_editor_changes_with_link_enforcement_and_documents(
    args: &PushEditorChangesArgs,
    project_root: &Path,
    src_root: &Path,
    link_enforcement: &LinkEnforcement,
    prepared_documents: &mut HashMap<String, SettingsBytecode>,
) -> Result<EditorChangeSet> {
    let mut property_filter = EditorPropertyFilter::from_args(args)?;
    let mut changes = EditorChangeSet::default();
    let mut documents: HashMap<String, Option<Arc<SettingsBytecode>>> = HashMap::new();
    let mut source_children: HashMap<String, Vec<Vec<usize>>> = HashMap::new();
    let mut changed_services = EditorChangedServices::default();
    let mut seen_paths = HashSet::new();

    let mut changed_paths = expand_editor_changed_paths(args)?;
    let full_reconcile = changed_paths.is_empty();
    if full_reconcile {
        changed_paths = collect_editor_full_paths(src_root)?;
    }
    let enforced_changed_paths =
        apply_link_enforcement_to_changed_paths(project_root, link_enforcement, changed_paths)?;
    for changed_path in enforced_changed_paths {
        let Some((absolute_path, service)) =
            unique_editor_changed_path(project_root, src_root, &changed_path, &mut seen_paths)
        else {
            continue;
        };

        load_editor_service_document(&service, src_root, &mut documents, prepared_documents)?;

        if collect_settings_file_change(
            args,
            &absolute_path,
            &service,
            &property_filter,
            &mut changed_services,
        ) {
            continue;
        }

        if absolute_path.is_dir()
            && let Some(document) = documents.get(&service).and_then(Option::as_ref)
            && let Some(target) = editor_directory_target(
                document,
                &service,
                &src_root.join(&service),
                &absolute_path,
            )
        {
            if reject_read_only_changed_path(
                link_enforcement,
                &service,
                Some((&target.path_segments, &target.path_ordinals)),
                &absolute_path,
                full_reconcile,
            )? {
                continue;
            }
            property_filter.settings_ids.extend(target.settings_ids);
            changed_services.settings.insert(service.clone());
            changed_services.target_upsert.insert(service);
            continue;
        }

        if !source_children.contains_key(&service) {
            let children = documents
                .get(&service)
                .and_then(Option::as_ref)
                .map(|document| settings_children_by_parent(document.as_ref()))
                .unwrap_or_default();
            source_children.insert(service.clone(), children);
        }

        let mut mapped_target = documents
            .get(&service)
            .and_then(Option::as_ref)
            .zip(source_children.get(&service))
            .and_then(|(document, children)| {
                editor_source_target_with_children(
                    document,
                    &service,
                    &src_root.join(&service),
                    &absolute_path,
                    children,
                )
            });

        let metadata = fs::metadata(&absolute_path).ok();
        let exists_as_file = metadata.as_ref().is_some_and(std::fs::Metadata::is_file);
        let inferred_spec = infer_editor_source_path_spec(src_root, &service, &absolute_path);
        let protected_path = mapped_target
            .as_ref()
            .map(|target| {
                (
                    target.path_segments.as_slice(),
                    target.path_ordinals.as_slice(),
                )
            })
            .or_else(|| {
                inferred_spec
                    .as_ref()
                    .map(|spec| (spec.path_segments.as_slice(), &[][..]))
            });
        if reject_read_only_changed_path(
            link_enforcement,
            &service,
            protected_path,
            &absolute_path,
            full_reconcile,
        )? {
            continue;
        }
        if mapped_target.is_none()
            && exists_as_file
            && let Some(spec) = inferred_spec.as_ref()
        {
            let settings_before = documents
                .get(&service)
                .and_then(Option::as_ref)
                .map(|document| document.as_ref().clone());
            let slot = documents
                .get_mut(&service)
                .expect("service document should be loaded");
            let document = Arc::make_mut(slot.get_or_insert_with(|| {
                Arc::new(SettingsBytecode {
                    version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
                    instances: Vec::new(),
                })
            }));
            let ensured = ensure_editor_source_target_in_bytecode(document, spec)?;
            mapped_target = Some(ensured.target);
            if ensured.changed {
                if let Some(target) = mapped_target.as_ref() {
                    changes.history_entries.push(EditorHistoryEntry {
                        service: service.clone(),
                        source_path: Some(absolute_path.clone()),
                        settings_id: target.settings_id.clone(),
                        path_segments: target.path_segments.clone(),
                        path_ordinals: target.path_ordinals.clone(),
                        class_name: target.class_name.clone(),
                        source_key: Some(editor_source_key_from_target(target)),
                        settings_before: settings_before.clone(),
                    });
                    if let Some(run_context) = spec.run_context.as_ref() {
                        let mut properties = Map::new();
                        properties.insert(
                            "RunContext".to_string(),
                            editor_run_context_value(run_context),
                        );
                        changes.property_changes.push(EditorPropertyChange {
                            service: service.clone(),
                            settings_id: target.settings_id.clone(),
                            path_segments: target.path_segments.clone(),
                            path_ordinals: target.path_ordinals.clone(),
                            class_name: target.class_name.clone(),
                            properties,
                            reset_properties: Vec::new(),
                            attributes: Map::new(),
                            deleted_attributes: Vec::new(),
                        });
                    }
                }
                changed_services.dirty.insert(service.clone());
                if !ensured.upsert_instances.is_empty() {
                    changes.instance_changes.push(EditorInstanceChange {
                        mode: "upsertInstances".to_string(),
                        service: service.clone(),
                        allow_deletes: false,
                        instances: ensured.upsert_instances,
                        preserve_instances: Vec::new(),
                    });
                }
                if !ensured.replace_instances.is_empty() {
                    if let (Some(before), Some(target_settings_id)) = (
                        settings_before.as_ref(),
                        mapped_target
                            .as_ref()
                            .and_then(|target| target.settings_id.as_deref()),
                    ) {
                        append_editor_reference_repairs(
                            &mut changes,
                            before,
                            document,
                            &service,
                            target_settings_id,
                        );
                    }
                    changes.instance_changes.push(EditorInstanceChange {
                        mode: "replaceInstances".to_string(),
                        service: service.clone(),
                        allow_deletes: false,
                        instances: ensured.replace_instances,
                        preserve_instances: Vec::new(),
                    });
                }
                source_children.remove(&service);
            }
        }

        if !exists_as_file
            && let Some(target) = mapped_target.as_ref()
            && is_lua_source_class(&target.class_name)
        {
            if let Some(settings_id) = target.settings_id.as_deref()
                && let Some(document) = documents.get_mut(&service).and_then(Option::as_mut)
                && let Some(index) =
                    document_instance_index_by_settings_id(document.as_ref(), settings_id)
            {
                let document = Arc::make_mut(document);
                let original_class = document.instances[index].class_name.clone();
                if is_lua_source_class(&original_class) {
                    let settings_before = document.clone();
                    changes.history_entries.push(EditorHistoryEntry {
                        service: service.clone(),
                        source_path: Some(absolute_path.clone()),
                        settings_id: Some(settings_id.to_string()),
                        path_segments: target.path_segments.clone(),
                        path_ordinals: target.path_ordinals.clone(),
                        class_name: original_class,
                        source_key: Some(editor_source_key_from_target(target)),
                        settings_before: Some(settings_before.clone()),
                    });
                    changed_services.dirty.insert(service.clone());
                    if inferred_spec.as_ref().is_some_and(|spec| spec.is_init) {
                        document.instances[index].class_name = "Folder".to_string();
                        let descriptor = editor_instance_descriptor_for_known_path(
                            document,
                            index,
                            target.path_segments.clone(),
                            target.path_ordinals.clone(),
                        )
                        .context("Failed to describe the replaced source instance")?;
                        changes.instance_changes.push(EditorInstanceChange {
                            mode: "replaceInstances".to_string(),
                            service: service.clone(),
                            allow_deletes: false,
                            instances: vec![descriptor],
                            preserve_instances: Vec::new(),
                        });
                        append_editor_reference_repairs(
                            &mut changes,
                            &settings_before,
                            document,
                            &service,
                            settings_id,
                        );
                    } else {
                        let descriptor = editor_instance_descriptor_for_known_path(
                            document,
                            index,
                            target.path_segments.clone(),
                            target.path_ordinals.clone(),
                        )
                        .context("Failed to describe the deleted source instance")?;
                        remove_instances_at_indices(document, &[index], true)?;
                        append_editor_reference_repairs(
                            &mut changes,
                            &settings_before,
                            document,
                            &service,
                            settings_id,
                        );
                        changes.instance_changes.push(EditorInstanceChange {
                            mode: "deleteInstances".to_string(),
                            service: service.clone(),
                            allow_deletes: false,
                            instances: vec![descriptor],
                            preserve_instances: Vec::new(),
                        });
                    }
                    source_children.remove(&service);
                }
            }
            continue;
        }

        let target = if let Some(target) = mapped_target {
            target
        } else if exists_as_file {
            let Some(spec) = inferred_spec else {
                continue;
            };
            EditorSourceTarget {
                service: spec.service,
                settings_id: None,
                path_segments: spec.path_segments,
                path_ordinals: Vec::new(),
                class_name: spec.class_name,
            }
        } else {
            continue;
        };

        let source = if exists_as_file {
            Some(
                fs::read_to_string(&absolute_path)
                    .with_context(|| format!("Failed to read {}", absolute_path.display()))?,
            )
        } else {
            Some(String::new())
        };
        if exists_as_file && target.settings_id.is_some() && is_lua_source_class(&target.class_name)
        {
            changes.history_entries.push(EditorHistoryEntry {
                service: target.service.clone(),
                source_path: Some(absolute_path.clone()),
                settings_id: target.settings_id.clone(),
                path_segments: target.path_segments.clone(),
                path_ordinals: target.path_ordinals.clone(),
                class_name: target.class_name.clone(),
                source_key: Some(editor_source_key_from_target(&target)),
                settings_before: None,
            });
        }
        changes.source_changes.push(EditorSourceChange {
            service: target.service,
            settings_id: target.settings_id,
            path_segments: target.path_segments,
            path_ordinals: target.path_ordinals,
            class_name: target.class_name,
            source,
            deleted: false,
        });
    }

    let changes = finish_editor_change_collection(
        changes,
        &documents,
        changed_services,
        &property_filter,
        project_root,
        src_root,
        link_enforcement,
    )?;
    for document in documents.into_values().flatten() {
        if let Ok(document) = Arc::try_unwrap(document) {
            drop_settings_document(document);
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    fn model_pivot(path: &[&str]) -> EditorPropertyChange {
        EditorPropertyChange {
            service: "Workspace".to_string(),
            settings_id: None,
            path_segments: path.iter().map(|segment| (*segment).to_string()).collect(),
            path_ordinals: vec![1; path.len()],
            class_name: "Model".to_string(),
            properties: Map::new(),
            reset_properties: Vec::new(),
            attributes: Map::new(),
            deleted_attributes: Vec::new(),
        }
    }

    #[test]
    fn nested_model_pivots_apply_parent_before_child() {
        let mut changes = vec![
            model_pivot(&["Workspace", "Parent", "Child"]),
            model_pivot(&["Workspace", "Parent"]),
        ];
        sort_post_commit_model_pivots(&mut changes);
        assert_eq!(changes[0].path_segments, ["Workspace", "Parent"]);
        assert_eq!(changes[1].path_segments, ["Workspace", "Parent", "Child"]);
    }

    #[test]
    fn source_verification_accepts_studio_newline_normalization() {
        assert!(editor_sources_match(
            "first\r\nsecond\rthird\r\n",
            "first\nsecond\nthird\n"
        ));
        assert!(editor_sources_match("return true\r\n", "return true"));
        assert!(!editor_sources_match("return true", "return false"));
    }

    #[test]
    fn projected_script_change_does_not_expand_to_the_whole_service_store() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("projected-script-push");
        let first = root.join("src/ReplicatedStorage/Package/First/Thing.lua");
        let second = root.join("src/ReplicatedStorage/Package/Second/Thing.lua");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, "return true\r\n").unwrap();
        fs::write(&second, "return false\r\n").unwrap();
        fs::write(
            root.join("default.project.json"),
            r#"{
                "name": "projected-script-push",
                "tree": {
                    "$className": "DataModel",
                    "ReplicatedStorage": {
                        "$className": "ReplicatedStorage",
                        "$ignoreUnknownInstances": true,
                        "$path": "src/ReplicatedStorage"
                    }
                }
            }"#,
        )
        .unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.extend([first, second]);
        let (changes, _) = collect_project_editor_changes(&args).unwrap();
        assert_eq!(changes.source_changes.len(), 2);
        assert!(changes.instance_changes.is_empty());
        assert_eq!(
            changes.source_changes[0].path_segments,
            ["ReplicatedStorage", "Package", "First", "Thing"]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_preflight_uses_exact_mutation_paths_once() {
        let source = EditorSourceChange {
            service: "ReplicatedStorage".to_string(),
            settings_id: Some("script".to_string()),
            path_segments: ["ReplicatedStorage", "Package", "Script"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            path_ordinals: vec![1, 1, 1],
            class_name: "ModuleScript".to_string(),
            source: Some("return true".to_string()),
            deleted: false,
        };
        let changes = EditorChangeSet {
            source_changes: vec![source.clone(), source],
            ..Default::default()
        };
        let targets = editor_mutation_package_targets(&changes, None);
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0]["pathSegments"],
            json!(["ReplicatedStorage", "Package", "Script"])
        );
        assert_eq!(targets[0]["includeSelf"], true);

        let mut without_ordinals = model_pivot(&["Package", "Value"]);
        without_ordinals.service = "ReplicatedStorage".to_string();
        without_ordinals.path_ordinals.clear();
        without_ordinals.properties.clear();
        without_ordinals
            .properties
            .insert("Value".to_string(), json!("changed"));
        let targets = editor_mutation_package_targets(
            &EditorChangeSet {
                property_changes: vec![without_ordinals],
                ..Default::default()
            },
            None,
        );
        assert_eq!(targets[0]["pathOrdinals"], json!([]));

        let mut relative = model_pivot(&["Package", "Value"]);
        relative.service = "ReplicatedStorage".to_string();
        relative.class_name = "StringValue".to_string();
        relative.properties.clear();
        relative
            .properties
            .insert("Value".to_string(), json!("changed"));
        let targets = editor_mutation_package_targets(
            &EditorChangeSet {
                property_changes: vec![relative],
                ..Default::default()
            },
            None,
        );
        assert_eq!(
            targets[0]["pathSegments"],
            json!(["ReplicatedStorage", "Package", "Value"])
        );
        assert_eq!(targets[0]["pathOrdinals"], json!([1, 1, 1]));
    }

    #[test]
    fn package_root_overrides_do_not_trigger_desync_but_content_properties_do() {
        let mut root_override = model_pivot(&["Workspace", "Package"]);
        root_override
            .properties
            .insert("WorldPivot".to_string(), json!({}));
        root_override
            .attributes
            .insert("Configuration".to_string(), json!(true));
        let override_targets = editor_mutation_package_targets(
            &EditorChangeSet {
                property_changes: vec![root_override],
                ..Default::default()
            },
            None,
        );
        assert_eq!(override_targets[0]["includeSelf"], false);

        let mut content_change = model_pivot(&["Workspace", "Package"]);
        content_change
            .properties
            .insert("Archivable".to_string(), json!(false));
        let content_targets = editor_mutation_package_targets(
            &EditorChangeSet {
                property_changes: vec![content_change],
                ..Default::default()
            },
            None,
        );
        assert_eq!(content_targets[0]["includeSelf"], true);
    }

    #[test]
    fn native_import_preflights_only_changed_actual_package_roots() {
        use crate::editor::types::{EditorBinaryImportGroup, EditorBinaryPackageRoot};

        let binary_import = EditorBinaryImport {
            bytes: Vec::new(),
            groups: vec![EditorBinaryImportGroup {
                service: "ReplicatedStorage".to_string(),
                target_path: vec!["ReplicatedStorage".to_string()],
                count: 1,
                payload_root_name: "payload".to_string(),
                root_paths: Vec::new(),
                retained_roots: Vec::new(),
                package_roots: vec![EditorBinaryPackageRoot {
                    path_segments: ["ReplicatedStorage", "Outer"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    path_ordinals: vec![1, 1],
                    class_name: "Folder".to_string(),
                }],
                mutation_package_roots: vec![EditorBinaryPackageRoot {
                    path_segments: ["ReplicatedStorage", "Outer", "Package"]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    path_ordinals: vec![1, 1, 1],
                    class_name: "Model".to_string(),
                }],
                change_generation: Some(1),
            }],
            instance_count: 0,
            post_apply_properties_by_class: HashMap::new(),
            post_apply_properties_by_path: HashMap::new(),
            external_references_post_applied: false,
        };
        let targets =
            editor_mutation_package_targets(&EditorChangeSet::default(), Some(&binary_import));
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0]["pathSegments"],
            json!(["ReplicatedStorage", "Outer", "Package"])
        );
        assert_eq!(targets[0]["includeSelf"], true);
    }
}
