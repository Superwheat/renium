use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::app::output::{
    global_log_enabled, global_pretty_output, global_yes, log_global, print_json_output,
};
use crate::app::timing::{current_millis, elapsed_ms, log_timing};
use crate::automation::op;
use crate::bytecode::{SettingsFileLock, acquire_settings_file_locks};
use crate::cli::{
    ApplyEditorDeleteArgs, ApplyEditorPropertyArgs, EditorMutationArgs, PushEditorChangesArgs,
};
use crate::daemon::daemon_control_request;
use crate::editor::diff::{
    EditorPropertyScope, EditorTargetChangeOptions, append_editor_instance_reconcile,
    append_editor_target_changes, editor_instance_descriptor_for_known_path,
};
use crate::editor::document::{
    EditorDocumentIndex, document_instance_index_by_settings_id,
    ensure_editor_source_target_with_index, read_editor_service_settings,
    read_editor_service_settings_cached,
};
use crate::editor::history::save_editor_history_entries;
use crate::editor::paths::{
    EditorChildStemCache, build_editor_instance_paths, build_editor_instance_paths_for_indices,
    editor_directory_target, editor_run_context_value, editor_source_target_with_cache,
    infer_editor_source_path_spec, infer_source_script, service_from_changed_path,
};
use crate::editor::review::{
    EditorReviewOutcome, OutsideReferencePaths, ProtectedPlaceCache,
    apply_protected_writes_offline, editor_review_payload, is_externally_managed_editor_property,
    is_externally_managed_protected_write, is_user_facing_protected_write,
    local_place_path_for_bridge, normalize_editor_bridge_value,
    protected_root_write_rows_with_live_values, protected_write_matches_previous,
    request_editor_push_review, request_editor_push_review_payload, request_protected_write_review,
    studio_pid_for_bridge,
};
use crate::editor::types::{
    EditorBinaryImport, EditorChangeSet, EditorHistoryEntry, EditorInstanceChange,
    EditorInstanceDescriptor, EditorInstancePath, EditorPreserveDescriptor, EditorPropertyChange,
    EditorPropertyFilter, EditorSettingsWrite, EditorSourceChange, EditorSourceTarget,
    PreparedEditorDocuments, ServiceReaderFallback, take_pre_routed_protected_writes,
};
use crate::project::config;
use crate::project::layout::{apply_configured_project_layout, configured_project_layout};
use crate::project::package_links::LinkEnforcement;
use crate::project::package_links::{
    apply_link_enforcement_to_changed_paths, build_link_enforcement,
    build_loaded_project_link_enforcement,
};
use crate::rbx::decode::rbx_reflection_class_is_a;
use crate::rbx::encode::rbx_model_property_descriptor;
use crate::roblox::schema::{PropertySchemaMap, load_rbx_dom_property_schema};
use crate::settings::bytecode::{SettingsBytecode, is_reference_object, settings_reference_index};
use crate::settings::equivalence::drop_settings_document;
use crate::settings::instance::remove_instances_at_indices;
use crate::settings::tree::settings_children_by_parent;
use crate::studio::bridge::BridgeTarget;
use crate::studio::bridge::{
    BridgeRequestTooLarge, BridgeServer, MAX_BRIDGE_CHUNK_BYTES, MAX_BRIDGE_REQUEST_BYTES,
};
use crate::studio::native::editor::{
    property_change_needs_post_native_apply, send_editor_change_batches,
};
use crate::studio::native::import::{
    BinaryImportSelection, build_editor_binary_import, build_editor_binary_import_with_documents,
    select_editor_binary_import,
};
use crate::system::files::{
    absolutize_under, canonical_path, fnv1a_hex, is_service_settings_file_name, path_key,
    service_settings_path, strip_extended_prefix, validate_filesystem_instance_name,
};
use crate::system::text::normalized_source_bytes;

#[path = "sync_bulk.rs"]
mod bulk;

struct EditorTransaction<'a> {
    mutation_started: Option<&'a std::sync::atomic::AtomicBool>,
    bridge: &'a BridgeServer,
    id: String,
    plugin_session_id: Option<String>,
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
    verified_push_proof: Option<Value>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) creator: Option<PackageCreator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owned: Option<bool>,
}

/// Who published a package, as the Studio plugin read it from the catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PackageCreator {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) id: i64,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) asset: String,
}

/// Refuses to touch packages this account does not own: Renium never desyncs
/// or rewrites someone else's package on the user's behalf.
pub(crate) fn ensure_packages_owned(packages: &[EditorPackageTarget]) -> Result<()> {
    let foreign: Vec<String> = packages
        .iter()
        .filter(|package| package.owned == Some(false))
        .map(|package| {
            let path = package.path_segments.join(".");
            match &package.creator {
                Some(creator) if !creator.asset.is_empty() => format!(
                    "{path} (package '{}' by {} {})",
                    creator.asset,
                    creator.kind.to_ascii_lowercase(),
                    creator.name
                ),
                Some(creator) => format!(
                    "{path} (by {} {})",
                    creator.kind.to_ascii_lowercase(),
                    creator.name
                ),
                None => path,
            }
        })
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    bail!(
        "The change touches a Roblox package this account does not own: {}. Renium does not push into or desync packages owned by others; revert those files to the package's content, or unlink the package in Studio (right-click the root, Unlink from Package) and push again",
        foreign.join(", ")
    );
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditorMutationPackages {
    packages: Vec<EditorPackageTarget>,
}

pub(crate) fn package_root_property_is_override(class_name: &str, property_name: &str) -> bool {
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

const ABANDONED_TRANSACTION: &str = "Studio released the failed transaction; unresolved Studio edits and rollback data are retained only in this plugin session and can be lost when the plugin reloads or Studio closes. Recovery has at most eight scheduled opportunities while the original Studio ownership remains valid. Local data release is scheduled 30 minutes after terminal transfer and waits for the exclusive gate. Package contents are not confirmed restored";
const NATIVE_OUTCOME_UNKNOWN: &str = "Native work has an unknown terminal outcome; rollback remains blocked because a native write may still be running. Restart Studio to clear this operation";

fn editor_transaction_begin_result(
    direct: Result<Value>,
    upload: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    let result = match direct {
        Err(error) if error.is::<BridgeRequestTooLarge>() => upload(),
        result => result,
    };
    result.map_err(|error| {
        let message = format!("{error:#}");
        if message.contains("Another editor transaction is already active")
            || message.contains("An earlier editor transaction has a failed rollback")
        {
            error.context("Studio still has an active editor transaction; wait for it to finish (`rbx lst` shows Live Sync's, `rbx lof` releases a stuck one). Failed rollback cleanup retries at two-minute idle intervals")
        } else {
            error
        }
    })
}

fn editor_transaction_state(result: &Value) -> Option<&str> {
    result.get("state").and_then(Value::as_str)
}

#[cfg(test)]
mod transaction_recovery_tests {
    use super::*;
    use crate::studio::bridge::routing_tests::{connect, edit_info, fixture};
    use std::thread::{self, JoinHandle};
    use tungstenite::{Message, WebSocket, protocol::Role};

    fn scripted_bridge(responses: Vec<(&'static str, Value)>) -> (BridgeServer, JoinHandle<()>) {
        let bridge = fixture();
        let stream = connect(&bridge, 0, edit_info("transaction-recovery", 19));
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let peer = thread::spawn(move || {
            let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
            for (method, mut response) in responses {
                let Message::Text(text) = socket.read().unwrap() else {
                    panic!("expected request")
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(request["method"], method);
                response["id"] = request["id"].clone();
                socket
                    .send(Message::Text(response.to_string().into()))
                    .unwrap();
            }
        });
        (bridge, peer)
    }

    fn transaction(bridge: &BridgeServer) -> EditorTransaction<'_> {
        EditorTransaction {
            mutation_started: None,
            bridge,
            id: "retained".into(),
            plugin_session_id: Some("original-plugin-session".into()),
            active: true,
            package_mutation: true,
            package_dialog: None,
            auto_desynced_packages: vec!["Workspace.Package".into()],
            auto_desynced_package_targets: vec![EditorPackageTarget {
                path_segments: vec!["Workspace".into(), "Package".into()],
                path_ordinals: vec![1, 1],
                expected_version: 1,
                creator: None,
                owned: Some(true),
            }],
            // Any attempt to Restore fails before native execution. Terminal
            // abandonment must never reach that attempt or clear Changed data.
            package_runtime: None,
            auto_desync_confirmed: true,
        }
    }

    fn state_response(state: &str) -> Value {
        json!({"ok": true, "result": {"state": state}})
    }

    #[test]
    fn abandoned_generation_change_reports_stopped_replay() {
        let bridge = fixture();
        let mut tx = transaction(&bridge);
        let reason = "Studio ownership changed from generation 7 to 9; old rollback and cleanup will not be replayed";
        let error = tx
            .finish_abandoned(&json!({"recovery": {
                "state": "stopped", "retryStopped": true, "retirementReason": reason,
                "unreplayedEdits": 1, "unresolvedNativeRoots": 0, "detachedRoots": 0,
                "pendingCleanup": 0
            }}))
            .to_string();
        assert!(error.contains(reason) && error.contains("recovery: \"stopped\""));
        assert!(!tx.active && tx.auto_desynced_package_targets.len() == 1);
    }

    #[test]
    fn plugin_reload_is_distinct_from_an_expired_outcome() {
        for current in ["original-plugin-session", "reloaded-plugin-session"] {
            let bridge = fixture();
            let mut tx = transaction(&bridge);
            let error = tx.rollback_response(
                json!({"state":"notFound","pluginSessionId":current,"recoveryDataMayBeLost":true}),
                None,
            ).unwrap_err().to_string();
            assert_eq!(
                error.contains("plugin session changed"),
                current != "original-plugin-session"
            );
            assert!(error.contains("recovery data") && error.contains("lost"));
            assert!(!tx.active && tx.auto_desynced_package_targets.len() == 1);
        }
        let error =
            editor_transaction_begin_result(Err(anyhow::anyhow!(NATIVE_OUTCOME_UNKNOWN)), || {
                panic!("unexpected upload")
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("Restart Studio") && !error.contains("two-minute"));
    }

    #[test]
    fn rollback_releases_terminal_ownership_before_package_restoration_failure() {
        for path in ["rollback", "commit", "commit-query", "prepare"] {
            let method = if path == "rollback" {
                "rollbackEditorTransaction"
            } else {
                "commitEditorTransaction"
            };
            let mut responses = Vec::new();
            if path == "commit-query" {
                responses.push((
                    method,
                    json!({"ok": false, "error": "injected RPC failure"}),
                ));
                responses.push(("getEditorTransactionState", state_response("rolledBack")));
            } else {
                responses.push((method, state_response("rolledBack")));
            }
            let (bridge, peer) = scripted_bridge(responses);
            let mut tx = transaction(&bridge);
            let result = match path {
                "rollback" => tx.rollback(),
                "prepare" => tx.prepare_snapshot(),
                _ => tx.commit().map(|_| ()),
            };
            #[cfg(any(windows, target_os = "macos"))]
            assert!(
                format!("{:#}", result.unwrap_err()).contains("package runtime was not retained")
            );
            #[cfg(not(any(windows, target_os = "macos")))]
            let _ = result;
            assert!(!tx.active && tx.package_dialog.is_none());
            drop(tx);
            peer.join().unwrap();
        }
    }

    #[test]
    fn failed_offline_snapshot_before_commit_rolls_back_the_open_transaction() {
        let (bridge, peer) = scripted_bridge(vec![(
            "rollbackEditorTransaction",
            state_response("rolledBack"),
        )]);
        let mut tx = transaction(&bridge);
        tx.auto_desynced_package_targets.clear();
        tx.auto_desynced_packages.clear();
        tx.package_mutation = false;
        let root = crate::tests::support::temp_dir("offline-prepare-rollback");
        let mut args = PushEditorChangesArgs::new(
            crate::cli::ProjectSourceArgs {
                project_root: root.clone(),
                src_root: "src".into(),
            },
            crate::cli::BridgeConnectionArgs::local(0.1),
        );
        let pending = PendingProtectedWrites {
            writes: vec![json!({"kind":"property","name":"SavedField","value":true})],
            supplemental: 1,
        };
        args.yes = true;
        let protected =
            prepare_protected_writes(&args, &bridge, pending, false, &HashSet::new(), false)
                .unwrap();
        assert!(protected.apply_offline);
        let snapshot = crate::editor::review::patch_place_protected_writes(
            &root.join("missing.rbxl"),
            &protected.writes,
        );
        let error = EditorTransaction::finish(Some(&mut tx), snapshot).unwrap_err();
        assert!(!tx.active, "{error:#}");
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unconfirmed_rollback_never_restores_package_state() {
        for state in [
            "notFound",
            "committed",
            "rollbackFailed",
            "open",
            "prepared",
            "invalid",
        ] {
            let bridge = fixture();
            let mut tx = transaction(&bridge);
            assert!(tx.rollback_response(json!({"state":state}), None).is_err());
            assert_eq!(tx.auto_desynced_package_targets.len(), 1);
            assert_eq!(tx.active, !matches!(state, "committed" | "notFound"));
            tx.active = false;
        }
        let bridge = fixture();
        let mut tx = transaction(&bridge);
        assert!(tx.rollback_response(json!({"ok":true}), None).is_err());
        assert!(!tx.active && tx.auto_desynced_package_targets.len() == 1);
    }

    #[test]
    fn begin_hint_covers_direct_and_uploaded_refusals_without_diagnosing_failure() {
        let refusal = "Another editor transaction is already active";
        let error = editor_transaction_begin_result(Err(anyhow::anyhow!(refusal)), || {
            panic!("unexpected upload")
        })
        .unwrap_err();
        let hint = format!("{error:#}");
        assert!(hint.contains("wait for it to finish") && !hint.contains("whose rollback failed"));
        let (bridge, peer) = scripted_bridge(vec![(
            "finishEditorTransactionUpload",
            json!({"ok":false,"error":refusal}),
        )]);
        let oversized = bridge.call(
            "beginEditorTransaction",
            json!({"payload":"x".repeat(17 * 1024 * 1024)}),
        );
        assert!(
            oversized
                .as_ref()
                .unwrap_err()
                .is::<BridgeRequestTooLarge>()
        );
        let error = editor_transaction_begin_result(oversized, || {
            bridge.call("finishEditorTransactionUpload", json!({}))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("two-minute idle intervals"));
        peer.join().unwrap();
        let error = editor_transaction_begin_result(
            Err(anyhow::anyhow!("unrelated error")),
            || unreachable!(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "unrelated error");
    }
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
        if change.class_name != "PackageLink" && !native_services.contains(change.service.as_str())
        {
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
        if change.class_name != "PackageLink" && !native_services.contains(change.service.as_str())
        {
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
        // Native preflight already compares the imported contents and reports
        // the actual changed package roots. Rechecking every imported descendant
        // would duplicate that work (and inflate full-import metadata).
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

pub(crate) fn discover_editor_mutation_packages_with_timeout(
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
        ensure_packages_owned(&result.packages)?;
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
    fn parameters<'c>(
        changes: &'c EditorChangeSet,
        binary_import: Option<&'c EditorBinaryImport>,
        id: &str,
        services: Vec<String>,
        guard: Option<&StudioChangeGuard>,
    ) -> Value {
        let _profile = crate::app::profile::span("native transaction parameters");
        let native_import = binary_import.is_some();
        let native_import_services = binary_import
            .into_iter()
            .flat_map(|import| {
                import
                    .groups
                    .iter()
                    .filter(|group| !group.additive)
                    .map(|group| &group.service)
            })
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
        let mut mutation_root_keys = HashSet::new();
        let mut mutation_root_names = Vec::<(&str, Vec<&str>, Vec<usize>)>::new();
        let retained_by_native_import = |service: &str, path: &[String], ordinals: &[usize]| {
            binary_import.is_some_and(|import| import.retains_path(service, path, ordinals))
        };
        let mut add_mutation_root = |service: &'c str, path: &'c [String], ordinals: &[usize]| {
            if path.len() < 2 {
                return;
            }
            let ordinal = ordinals.get(1).copied().unwrap_or(1);
            if mutation_root_keys.insert((service, path[1].as_str(), ordinal)) {
                let position = match mutation_root_names
                    .iter()
                    .position(|(name, ..)| *name == service)
                {
                    Some(position) => position,
                    None => {
                        mutation_root_names.push((service, Vec::new(), Vec::new()));
                        mutation_root_names.len() - 1
                    }
                };
                let (_, names, ordinals) = &mut mutation_root_names[position];
                names.push(path[1].as_str());
                ordinals.push(ordinal);
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
                for root in &group.root_paths {
                    if !retained_by_native_import(
                        &group.service,
                        &root.path_segments,
                        &root.path_ordinals,
                    ) {
                        add_mutation_root(&group.service, &root.path_segments, &root.path_ordinals);
                    }
                }
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
        let has_instance_changes = changes_instances(&changes.instance_changes);
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
                (binary_import.is_some_and(|import| {
                    import.imports_path(
                        &change.service,
                        &change.path_segments,
                        &change.path_ordinals,
                    )
                }) && change.class_name == "Model")
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
                !binary_import.is_some_and(|import| {
                    import.imports_path(
                        &change.service,
                        &change.path_segments,
                        &change.path_ordinals,
                    )
                }) || property_change_needs_post_native_apply(change)
            })
            .collect::<Vec<_>>();
        // A descriptor names roots of one service by name and ordinal under it,
        // at most 1,024 of them so an upload row stays small.
        let mutation_roots = mutation_root_names
            .iter()
            .flat_map(|(service, names, ordinals)| {
                names
                    .chunks(1024)
                    .zip(ordinals.chunks(1024))
                    .map(move |(names, ordinals)| {
                        json!({ "service": service, "names": names, "ordinals": ordinals })
                    })
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
            "hasBulkPropertyPatches": !changes.bulk_property_patches.is_empty(),
            "nativeImport": native_import,
            "nativeImportServices": native_import_services,
            "expectedRuntimeId": guard.map(|guard| &guard.runtime_id),
            "expectedStudioGenerations": guard.map(|guard| &guard.service_generations),
        })
    }

    fn chunk_parameters(mut parameters: Value) -> Result<(Value, Vec<Vec<Value>>)> {
        let object = parameters
            .as_object_mut()
            .context("Editor transaction parameters must be an object")?;
        let mut rows = Vec::new();
        for (field, kind) in [
            ("sourceChanges", "source"),
            ("propertyChanges", "property"),
            ("postCommitPropertyChanges", "postCommitProperty"),
            ("mutationRoots", "mutationRoot"),
            ("mutationPackageTargets", "mutationPackageTarget"),
        ] {
            let Some(Value::Array(values)) = object.remove(field) else {
                continue;
            };
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
        object.insert("totalChunks".to_string(), json!(chunks.len()));
        object.insert(
            "rowCount".to_string(),
            json!(chunks.iter().map(Vec::len).sum::<usize>()),
        );
        Ok((parameters, chunks))
    }

    fn upload(bridge: &BridgeServer, id: &str, parameters: Value) -> Result<Value> {
        let (header, chunks) = Self::chunk_parameters(parameters)?;
        bridge.call("beginEditorTransactionUpload", header)?;
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
        mutation_started: Option<&'a std::sync::atomic::AtomicBool>,
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
        let native_properties_available = {
            let _profile = crate::app::profile::span("native transaction property capability");
            super::native_roots::available(bridge)?
        };
        let parameters = |services| {
            let mut parameters = Self::parameters(changes, binary_import, &id, services, guard);
            parameters["nativePropertiesAvailable"] = json!(native_properties_available);
            parameters
        };
        if let Some(started) = mutation_started {
            started.store(true, std::sync::atomic::Ordering::Release);
        }
        let result = editor_transaction_begin_result(
            bridge.call("beginEditorTransaction", parameters(services)),
            || Self::upload(bridge, &id, parameters(upload_services)),
        )?;
        if result.get("studioChanged").and_then(Value::as_bool) == Some(true) {
            if let Some(started) = mutation_started {
                started.store(false, std::sync::atomic::Ordering::Release);
            }
            return Err(StudioChangedBeforePush.into());
        }
        let package_mutation = result
            .get("packageMutation")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut transaction = Self {
            mutation_started,
            bridge,
            id,
            plugin_session_id: result
                .get("pluginSessionId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            active: true,
            package_mutation,
            package_dialog: None,
            auto_desynced_packages: Vec::new(),
            auto_desynced_package_targets: Vec::new(),
            package_runtime: None,
            auto_desync_confirmed: false,
        };

        // A newly imported package has no live root for native desync preflight.
        // Its first edits can still open Studio's package notice on Windows/macOS.
        if package_mutation
            || binary_import.is_some_and(|import| {
                import
                    .groups
                    .iter()
                    .any(|group| !group.package_roots.is_empty())
            })
        {
            let watcher_started = Instant::now();
            // With the popup disabled inside Studio no dialog can appear, so the
            // accessibility watcher and its slow final pass are unnecessary.
            #[cfg(target_os = "macos")]
            let suppressed = match studio_pid_for_bridge(bridge)
                .and_then(crate::studio::native::serializer::suppress_package_notices)
            {
                Ok(()) => true,
                Err(error) => {
                    log_global(
                        4,
                        format_args!("[renium] package notice suppression unavailable: {error:#}"),
                    );
                    false
                }
            };
            #[cfg(not(target_os = "macos"))]
            let suppressed = false;
            if !suppressed {
                transaction.package_dialog = Some(
                    studio_pid_for_bridge(bridge)
                        .and_then(crate::studio::input::watch_package_changes_dialog)
                        .context("Package changes cannot be applied without a dialog watcher")?,
                );
            }
            log_timing(
                "native editor package dialog watcher start",
                watcher_started,
            );
        }

        #[cfg(any(windows, target_os = "macos"))]
        if binary_import.is_some()
            || package_mutation
            || !changes.instance_changes.is_empty()
            || changes.property_changes.iter().any(|change| {
                change
                    .properties
                    .keys()
                    .chain(change.reset_properties.iter())
                    .any(|name| {
                        super::native_roots::is_property(&change.class_name, name)
                            || change.class_name == "Terrain" && name == "SmoothGrid"
                    })
            })
        {
            #[cfg(test)]
            let mocked = crate::editor::native_roots::RECORDED_WRITES.with_borrow(Option::is_some);
            #[cfg(not(test))]
            let mocked = false;
            if !mocked {
                let token = result
                    .get("historyRecording")
                    .and_then(Value::as_str)
                    .context("Studio plugin needs updating for safe Terrain history")?;
                let info = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
                let pid = bridge.studio_pid_for_runtime(BridgeTarget::Edit, &info.runtime_id)?;
                let title = crate::studio::native::serializer::target_name(pid, &info.place_name)?;
                let terrain_write = changes.property_changes.iter().any(|change| {
                    change.class_name == "Terrain"
                        && change
                            .properties
                            .keys()
                            .chain(change.reset_properties.iter())
                            .any(|name| matches!(name.as_str(), "SmoothGrid" | "PhysicsGrid"))
                });
                crate::studio::native::serializer::register_history_if_available(
                    pid,
                    &title,
                    token,
                    terrain_write && native_properties_available,
                )?;
                for change in changes
                    .property_changes
                    .iter()
                    .filter(|_| native_properties_available)
                {
                    if change.class_name == "Terrain"
                        && change
                            .properties
                            .keys()
                            .chain(change.reset_properties.iter())
                            .any(|name| matches!(name.as_str(), "SmoothGrid" | "PhysicsGrid"))
                    {
                        let mut terrain = crate::studio::native::serializer::prepare_terrain(
                            pid,
                            &title,
                            &change.path_segments,
                            &change.path_ordinals,
                            Duration::from_secs(3),
                        )?;
                        terrain.write(token, None, None)?;
                        bridge.call_for_runtime_with_timeout("getEditorTransactionState", json!({"transactionId":transaction.id,"nativeTerrainBaseline":terrain.fingerprint()}), BridgeTarget::Edit, &info.runtime_id, Some(Duration::from_secs(2)))?;
                        break;
                    }
                }
            }
        }

        let packages: EditorMutationPackages = serde_json::from_value(json!({
            "packages": result
                .get("mutationPackages")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        }))
        .context("Studio returned invalid transaction package targets")?;
        if let Err(error) = ensure_packages_owned(&packages.packages) {
            if let Err(rollback_error) = transaction.rollback() {
                return Err(error.context(format!(
                    "Rollback after refusing a foreign package also failed: {rollback_error:#}"
                )));
            }
            return Err(error);
        }
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
            let title = crate::studio::native::serializer::target_name(
                pid,
                &bridge
                    .cached_bridge_info_for_target(BridgeTarget::Edit)?
                    .place_name,
            )?;
            log_timing("native editor package title lookup", started);
            transaction.package_runtime = Some((pid, title.clone()));
            for root in &packages.packages {
                let action_started = Instant::now();
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
                log_timing("native editor package desync action", action_started);
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

    fn release_terminal_resources(&mut self) {
        self.active = false;
        self.package_dialog.take();
    }

    fn finish_abandoned(&mut self, result: &Value) -> anyhow::Error {
        self.release_terminal_resources();
        // Restore marks a package Unmodified. Abandonment proves no such thing.
        let Some(recovery) = result.get("recovery") else {
            return anyhow::anyhow!("{ABANDONED_TRANSACTION}; unresolved counts unavailable");
        };
        let retirement = recovery["retirementReason"]
            .as_str()
            .map(|reason| format!("; {reason}"))
            .unwrap_or_default();
        anyhow::anyhow!(
            "{ABANDONED_TRANSACTION}; unresolved edits: {}, native roots: {}, detached roots: {}, pending cleanup: {}; recovery: {}{retirement}",
            recovery["unreplayedEdits"],
            recovery["unresolvedNativeRoots"],
            recovery["detachedRoots"],
            recovery["pendingCleanup"],
            recovery["state"],
        )
    }

    fn finish_native_unknown(&mut self, result: &Value) -> anyhow::Error {
        // End daemon retries, leaving Studio's operation fence and package
        // state intact until a verified receipt or a Studio restart.
        self.release_terminal_resources();
        anyhow::anyhow!(
            "{NATIVE_OUTCOME_UNKNOWN} (transaction {}, import {})",
            self.id,
            result["nativeOperation"]["importId"]
        )
    }

    fn finish_missing_outcome(&mut self, result: &Value) -> anyhow::Error {
        self.release_terminal_resources();
        if let (Some(previous), Some(current)) = (
            self.plugin_session_id.as_deref(),
            result.get("pluginSessionId").and_then(Value::as_str),
        ) && previous != current
        {
            return anyhow::anyhow!(
                "Studio's plugin session changed ({previous} -> {current}); transaction {} and its session-only recovery data are unavailable and may have been lost. Package contents are not confirmed restored",
                self.id
            );
        }
        anyhow::anyhow!(
            "Studio did not retain this transaction outcome; commit and rollback are unconfirmed. Session-only recovery data may have expired or been lost on plugin reload or Studio close"
        )
    }

    fn finish_rollback(&mut self) -> Result<()> {
        self.active = false;
        let watcher = self
            .package_dialog
            .take()
            .map(|watcher| watcher.finish())
            .transpose();
        // Both resources are terminal even if either cleanup step fails.
        let restored = self.restore_auto_desynced_packages();
        match (restored, watcher) {
            (Err(error), Err(watcher_error)) => Err(error.context(format!(
                "Package dialog cleanup also failed: {watcher_error:#}"
            ))),
            (Err(error), _) => Err(error),
            (Ok(()), watcher) => watcher
                .context("Studio did not finish accepting rollback package changes")
                .map(|_| {
                    if let Some(started) = self.mutation_started {
                        started.store(false, std::sync::atomic::Ordering::Release);
                    }
                }),
        }
    }

    fn commit(&mut self) -> Result<EditorCommitStatus> {
        let commit_result = self.bridge.call(
            "commitEditorTransaction",
            json!({ "transactionId": &self.id }),
        );
        let result = match commit_result {
            Ok(result) => result,
            Err(commit_error) => {
                let state_result = self.bridge.call(
                    "getEditorTransactionState",
                    json!({ "transactionId": &self.id }),
                );
                return self.commit_after_error(commit_error, state_result);
            }
        };
        self.commit_response(result)
    }

    fn commit_after_error(
        &mut self,
        commit_error: anyhow::Error,
        state_result: Result<Value>,
    ) -> Result<EditorCommitStatus> {
        let result = match state_result {
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
                Some("abandoned") => {
                    return Err(commit_error.context(self.finish_abandoned(&result)));
                }
                Some("nativeOutcomeUnknown") => {
                    return Err(commit_error.context(self.finish_native_unknown(&result)));
                }
                Some("notFound") | None => {
                    return Err(commit_error.context(self.finish_missing_outcome(&result)));
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
        };
        self.commit_response(result)
    }

    fn commit_response(&mut self, result: Value) -> Result<EditorCommitStatus> {
        match editor_transaction_state(&result) {
            Some("abandoned") => return Err(self.finish_abandoned(&result)),
            Some("nativeOutcomeUnknown") => return Err(self.finish_native_unknown(&result)),
            Some("notFound") => {
                return Err(self.finish_missing_outcome(&result));
            }
            Some("rolledBack") => {
                self.finish_rollback().context(
                    "Studio rolled the transaction back, but Renium could not restore its automatic package state changes",
                )?;
                bail!("Studio rolled the transaction back instead of committing it");
            }
            Some("open" | "prepared") => {
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
        let watcher_started = Instant::now();
        let package_dialog_accepted = self
            .package_dialog
            .take()
            .map(|watcher| watcher.finish())
            .transpose();
        log_timing(
            "native editor package dialog watcher finish",
            watcher_started,
        );
        let package_dialog_accepted = package_dialog_accepted
            .context("Studio did not finish accepting package changes")?
            .unwrap_or(false);
        if let Some(reason) = result.get("pushProofUnavailable").and_then(Value::as_str) {
            log_global(
                5,
                format_args!("[renium] commit push proof unavailable: {reason}"),
            );
        }
        Ok(EditorCommitStatus {
            verified_push_proof: result
                .get("verifiedPushProof")
                .filter(|value| !value.is_null())
                .cloned(),
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
        // Ask Studio first: a terminal transaction must release local resources
        // even when a missing package target makes native restoration fail.
        let rollback_result = self.bridge.call(
            "rollbackEditorTransaction",
            json!({ "transactionId": &self.id }),
        );
        let (result, rollback_error) = match rollback_result {
            Ok(result) => (result, None),
            Err(rollback_error) => {
                let state_result = self.bridge.call(
                    "getEditorTransactionState",
                    json!({ "transactionId": &self.id }),
                );
                match state_result {
                    Ok(result) => (result, Some(rollback_error)),
                    Err(state_error) => {
                        return Err(rollback_error.context(format!(
                            "Studio rollback outcome could not be queried: {state_error:#}"
                        )));
                    }
                }
            }
        };
        self.rollback_response(result, rollback_error)
    }

    fn rollback_response(
        &mut self,
        result: Value,
        rollback_error: Option<anyhow::Error>,
    ) -> Result<()> {
        match editor_transaction_state(&result) {
            Some("rolledBack") => self.finish_rollback(),
            Some("abandoned") => Err(self.finish_abandoned(&result)),
            Some("nativeOutcomeUnknown") => Err(self.finish_native_unknown(&result)),
            Some("committed") => {
                self.release_terminal_resources();
                bail!("Studio had already committed the transaction; rollback was not performed")
            }
            Some("notFound") | None => Err(self.finish_missing_outcome(&result)),
            Some("open" | "prepared" | "rollbackFailed") => {
                if let Some(error) = rollback_error {
                    return Err(error
                        .context("Studio retained the transaction because its rollback failed"));
                }
                bail!("Studio did not roll the transaction back: {result}")
            }
            Some(state) => bail!("Studio returned an invalid transaction state: {state}"),
        }
    }

    fn finish<T>(transaction: Option<&mut Self>, result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            if let Some(transaction) = transaction
                && transaction.active
                && let Err(rollback_error) = transaction.rollback()
            {
                return error.context(format!("Studio rollback also failed: {rollback_error:#}"));
            }
            error
        })
    }

    fn prepare_snapshot(&mut self) -> Result<()> {
        let result = self.bridge.call(
            "commitEditorTransaction",
            json!({
                "transactionId": &self.id,
                "prepareOnly": true,
            }),
        )?;
        self.prepare_snapshot_response(result)
    }

    fn prepare_snapshot_response(&mut self, result: Value) -> Result<()> {
        match editor_transaction_state(&result) {
            Some("abandoned") => return Err(self.finish_abandoned(&result)),
            Some("nativeOutcomeUnknown") => return Err(self.finish_native_unknown(&result)),
            Some("rolledBack") => {
                self.finish_rollback()?;
                bail!("Studio rolled back instead of preparing the transaction");
            }
            Some("notFound") => return Err(self.finish_missing_outcome(&result)),
            Some("committed") => self.release_terminal_resources(),
            _ => {}
        }
        anyhow::ensure!(
            result["state"] == "prepared" && result["committed"] == false,
            "Studio did not prepare the complete transaction for its place snapshot"
        );
        Ok(())
    }
}

fn sort_post_commit_model_pivots(changes: &mut [EditorPropertyChange]) {
    changes.sort_by_key(|change| change.path_segments.len());
}

/// The rows that bind parts to one another go after every other row, in
/// their order. A joint bound while one of its parts still stands where it
/// was created keeps that offset, and Studio then moves the parts to it.
fn bind_joints_last(changes: &mut [EditorPropertyChange]) {
    changes.sort_by_key(|change| {
        ["Part0", "Part1", "Attachment0", "Attachment1"]
            .iter()
            .any(|bound| change.properties.contains_key(*bound))
    });
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
        json!(
            changes
                .instance_changes
                .iter()
                .map(|change| change.instances.len())
                .sum::<usize>() as u64
        ),
    );
    summary.insert(
        "sourceQueued".to_string(),
        json!(changes.source_changes.len() as u64),
    );
    summary.insert(
        "propertyQueued".to_string(),
        json!(changes.property_changes.len() as u64),
    );
    summary.insert("noops".to_string(), json!(0));
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
    if let Some(proof) = status.verified_push_proof {
        summary.insert("verifiedPushProof".into(), proof);
    }
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

pub(crate) fn push_editor_changes(mut args: PushEditorChangesArgs) -> Result<()> {
    name_script_paths(&mut args, &std::env::current_dir()?)?;
    if !args.verify && !args.no_check {
        crate::cli::syntax::ensure_script_files_parse(&args.paths)?;
    }
    let result = push_editor_changes_result(args)?;
    print_json_output(&result, global_pretty_output(false))
}

/// Script files named on the command line are the whole intent of the push,
/// so their changes apply without the large-push hold; a store file can carry
/// any number of instance changes and keeps it.
fn named_script_files(paths: &[PathBuf]) -> bool {
    !paths.is_empty()
        && paths.iter().all(|path| {
            matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("lua" | "luau")
            )
        })
}

/// The hold is skipped only when those script files are everything the push
/// selects: a service store, a path list or instance ids named beside them
/// can carry any number of changes.
fn only_named_scripts(args: &PushEditorChangesArgs) -> bool {
    named_script_files(&args.paths)
        && args.services.is_empty()
        && args.changed_paths.is_empty()
        && args.changed_paths_files.is_empty()
        && args.target_settings_ids.is_empty()
        && args.target_settings_id_files.is_empty()
}

/// A positional path that is no file or folder becomes the file of the one
/// script that goes by that name in the project (`HonkHandler` for
/// `src/ServerScriptService/HonkHandler.server.luau`); a name no script goes
/// by stays as it is.
fn name_script_paths(args: &mut PushEditorChangesArgs, invocation_root: &Path) -> Result<()> {
    let missing = |path: &Path| !absolutize_under(invocation_root, path).exists();
    if !args.paths.iter().any(|path| missing(path)) {
        return Ok(());
    }
    let (root, source) =
        configured_project_layout(&args.project.project_root, &args.project.src_root)?;
    let root = absolutize_under(invocation_root, &root);
    let source = absolutize_under(&root, &source);
    for path in args.paths.iter_mut().filter(|path| missing(path)) {
        let named = path
            .to_str()
            .map(|name| crate::automation::tools::script_by_name(&root, &source, name))
            .transpose()
            .map_err(anyhow::Error::msg)?
            .flatten();
        if let Some(file) = named {
            *path = file;
        } else if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                ["lua", "luau", "renium"]
                    .iter()
                    .any(|kept| extension.eq_ignore_ascii_case(kept))
            })
        {
            bail!(
                "`{}` is not a file or a folder, and no script goes by that name; nothing was pushed. A deleted script is pushed by its file name (`rbx ps src/Folder/Old.luau`); `rbx ps` without paths pushes every difference",
                path.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn push_editor_changes_result(args: PushEditorChangesArgs) -> Result<Value> {
    push_editor_changes_via(args, daemon_control_request)
}

fn push_editor_changes_via(
    mut args: PushEditorChangesArgs,
    mut send: impl FnMut(u16, Option<&Path>, Value, bool) -> Result<Value>,
) -> Result<Value> {
    let invocation_root = std::env::current_dir()?;
    name_script_paths(&mut args, &invocation_root)?;
    let named_scripts = only_named_scripts(&args);
    args.changed_paths.append(&mut args.paths);
    resolve_cli_push_paths(&mut args, &invocation_root)?;
    apply_configured_project_layout(&mut args.project.project_root, &mut args.project.src_root)?;
    add_service_store_paths(&mut args)?;
    let mut parameters = push_request_parameters(&args, named_scripts);
    let disable = parameters["disableServiceReader"] == true;
    let options = match (args.slow_import, disable) {
        (true, true) => "--slow-import and RENIUM_NATIVE_SERVICE_IMPORT=0",
        (true, false) => "--slow-import",
        (false, true) => "RENIUM_NATIVE_SERVICE_IMPORT=0",
        (false, false) => {
            return send(
                op::PUSH,
                Some(&args.project.project_root),
                parameters,
                !args.no_review,
            );
        }
    };
    let transport = json!({"bridgePorts": parameters["bridgePorts"], "bridgeWaitSeconds": parameters["bridgeWaitSeconds"]});
    let advertised = match send(op::CAP, None, transport, false) {
        Ok(reply) => Some(reply["slowImport"] == true),
        Err(error)
            if error.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                )
            }) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    parameters["confirmImportOptions"] = json!(true);
    let reply = send(
        op::PUSH,
        Some(&args.project.project_root),
        parameters,
        !args.no_review,
    );
    let warning = match advertised {
        Some(true) => None,
        Some(false) => Some(format!(
            "The selected daemon did not advertise support for {options}. Restart that daemon with this CLI build to confirm support for the requested options."
        )),
        None => Some(format!(
            "The capability check timed out. An older daemon may ignore {options}. Restart that daemon with this CLI build to confirm support."
        )),
    };
    let mut result = reply.map_err(|error| match &warning {
        Some(message) => error.context(message.clone()),
        None => error,
    })?;
    if let Some(fields) = result.as_object_mut() {
        let confirmed = fields.remove("confirmedImportOptions") == Some(Value::Bool(true));
        if !confirmed && let Some(warning) = warning {
            fields.insert("importOptionWarning".into(), json!(warning));
        }
    }
    Ok(result)
}

pub(crate) fn push_request_parameters(args: &PushEditorChangesArgs, named_scripts: bool) -> Value {
    push_request_with_reader_setting(
        args,
        named_scripts,
        std::env::var_os("RENIUM_NATIVE_SERVICE_IMPORT").as_deref(),
    )
}

pub(crate) fn push_request_with_reader_setting(
    args: &PushEditorChangesArgs,
    named_scripts: bool,
    reader_setting: Option<&std::ffi::OsStr>,
) -> Value {
    let incremental = !args.changed_paths.is_empty()
        || !args.changed_paths_files.is_empty()
        || !args.target_settings_ids.is_empty()
        || !args.target_settings_id_files.is_empty()
        || !args.target_properties.is_empty();
    json!({
        "srcDir": args.project.src_root,
        "changedPaths": args.changed_paths,
        "changedPathsFiles": args.changed_paths_files,
        "targetSettingsIds": args.target_settings_ids,
        "exactTargetIds": args.exact_target_ids,
        "targetSettingsIdFiles": args.target_settings_id_files,
        "targetProperties": args.target_properties,
        "upsertInstancesOnly": args.upsert_instances_only,
        "verifySources": args.verify_sources,
        "verify": args.verify,
        "slowImport": args.slow_import,
        "disableServiceReader": args.disable_service_reader || reader_setting.is_some_and(|value| value == "0"),
        "overridePackages": args.override_packages,
        "allowProtectedWrites": !args.no_review,
        "yes": args.yes || global_yes() || named_scripts,
        "reviewReport": true,
        "linkCacheDir": args.link_cache_dir,
        "bridgeWaitSeconds": args.bridge.wait_seconds,
        "bridgePorts": args.bridge.ports,
        "destructive": !incremental,
        "deferToLiveSync": args.defer_to_live_sync,
    })
}

/// `-s SERVICE` names a service's store file the way `rbx pl -s` does.
fn add_service_store_paths(args: &mut PushEditorChangesArgs) -> Result<()> {
    if args.services.is_empty() {
        return Ok(());
    }
    let stores = crate::project::structural::service_store_paths(&args.project.src_root)?;
    for service in std::mem::take(&mut args.services) {
        let Some(path) = stores.get(&service) else {
            bail!(
                "No store file for {service} under {}; `rbx pl -s {service}` pulls it first",
                args.project.src_root.display()
            );
        };
        args.changed_paths.push(path.clone());
    }
    Ok(())
}

fn resolve_cli_push_paths(args: &mut PushEditorChangesArgs, invocation_root: &Path) -> Result<()> {
    // --place can rebind the project to places/<alias>. User-supplied paths still
    // belong to the invoking directory, including paths read from a selection list.
    for list_path in &args.changed_paths_files {
        let list_path = absolutize_under(invocation_root, list_path);
        let contents = fs::read_to_string(&list_path).with_context(|| {
            format!("Failed to read changed paths file {}", list_path.display())
        })?;
        args.changed_paths.extend(
            contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(PathBuf::from),
        );
    }
    if !args.changed_paths_files.is_empty()
        && args.changed_paths.is_empty()
        && args.target_settings_ids.is_empty()
        && args.target_settings_id_files.is_empty()
        && args.target_properties.is_empty()
    {
        bail!("The changed paths files contain no paths; no push was performed");
    }
    args.changed_paths_files.clear();
    for path in args
        .changed_paths
        .iter_mut()
        .chain(&mut args.target_settings_id_files)
    {
        *path = absolutize_under(invocation_root, path);
    }
    if let Some(path) = &mut args.link_cache_dir {
        *path = absolutize_under(invocation_root, path);
    }
    Ok(())
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
    if args.verify {
        let (changes, _) = collect_verified_editor_changes(&args)?;
        validate_source_verification_selection(&args, &changes)?;
        return verify_editor_sources_only(bridge, &changes);
    }
    if native_editor_full_push_eligible(&args)? {
        bail!("A full push must use the managed replacement path; Studio was not changed");
    }
    let (changes, projection) = collect_pushed_editor_changes(&args)?;
    validate_source_verification_selection(&args, &changes)?;
    let mut args = args;
    // A push of a few scripts reads them back so the result's `verified`
    // answers the question an agent would otherwise ask Studio again.
    if !args.verify_sources
        && !changes.source_changes.is_empty()
        && changes.source_changes.len() <= VERIFY_SOURCES_BY_DEFAULT_LIMIT
    {
        args.verify_sources = true;
    }
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
            finalize_settings: None,
        },
    )
}

/// Pushes with at most this many changed scripts verify their sources without
/// `--verify-sources`.
const VERIFY_SOURCES_BY_DEFAULT_LIMIT: usize = 25;

/// An unreviewed full push removes whatever Studio holds and the files do not,
/// including newer Team Create work; say so before it happens.
fn announce_unreviewed_deletions(args: &PushEditorChangesArgs, changes: &EditorChangeSet) {
    if !args.no_review || args.upsert_instances_only {
        return;
    }
    let deletions = changes
        .instance_changes
        .iter()
        .filter(|change| change.mode == "deleteInstances")
        .map(|change| change.instances.len())
        .sum::<usize>();
    let reconciled = changes
        .instance_changes
        .iter()
        .filter(|change| change.allow_deletes && change.mode != "deleteInstances")
        .map(|change| change.service.as_str())
        .collect::<BTreeSet<_>>();
    if deletions > 0 {
        log_global(
            3,
            format_args!(
                "[renium] push deletes {deletions} instance(s) Studio has and the files do not, without review; pass -u to keep them, or pull first"
            ),
        );
    }
    if !reconciled.is_empty() {
        log_global(
            3,
            format_args!(
                "[renium] push reconciles {} without review and removes whatever Studio holds there that the files do not; pass -u to keep Studio-only instances, or pull first",
                reconciled.into_iter().collect::<Vec<_>>().join(", ")
            ),
        );
    }
}

/// `ps --verify`: compares the selected script sources with what Studio holds
/// and reports the differences; nothing is pushed.
fn verify_editor_sources_only(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
) -> Result<serde_json::Map<String, Value>> {
    let verification = verify_editor_source_changes(bridge, changes, None)?;
    let mut summary = serde_json::Map::new();
    summary.insert(
        "ok".to_string(),
        Value::Bool(verification.failed.is_empty()),
    );
    summary.insert("verifyOnly".to_string(), Value::Bool(true));
    summary.insert(
        "sourceVerification".to_string(),
        json!("plugin ScriptDocument/source batch"),
    );
    summary.insert("pushed".to_string(), Value::Bool(false));
    summary.insert(
        "sourceVerified".to_string(),
        json!(verification.verified as u64),
    );
    summary.insert(
        "sourceVerifyFailed".to_string(),
        json!(verification.failed.len() as u64),
    );
    if !verification.failed.is_empty() {
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
    }
    Ok(summary)
}

fn validate_source_verification_selection(
    args: &PushEditorChangesArgs,
    changes: &EditorChangeSet,
) -> Result<()> {
    if !(args.verify_sources || args.verify) || !changes.source_changes.is_empty() {
        return Ok(());
    }
    let selected_paths = expand_editor_changed_paths(args)?;
    let selected_scripts = selected_paths.iter().any(|path| {
        matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("lua" | "luau")
        )
    });
    let empty_selection = !selected_paths.is_empty()
        && changes.instance_changes.is_empty()
        && changes.property_changes.is_empty();
    if selected_scripts || empty_selection {
        bail!(
            "None of the requested scripts matched the selected project; no sources were pushed or verified. Check the paths and --place target"
        );
    }
    Ok(())
}

pub(crate) fn push_reconciled_editor_changes_with_warm_bridge<F, G, H>(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
    guard: Option<&StudioChangeGuard>,
    mut prepared_documents: PreparedEditorDocuments,
    amend: F,
    mut finalize_settings: H,
    validate_project: G,
) -> Result<serde_json::Map<String, Value>>
where
    F: FnOnce(&mut EditorChangeSet) -> Result<()>,
    G: Fn() -> Result<()>,
    H: FnMut(&mut EditorChangeSet) -> Result<()>,
{
    let started = Instant::now();
    // Filters and interactive review require the complete edit plan. Native
    // replacement selection is internal, never inferred from a partial edit.
    if (!args.no_review && !args.yes && !global_yes())
        || files_to_studio_filter_rules(&args.project.project_root)?.is_some()
    {
        read_imported_documents_whole(&args, &mut prepared_documents)?;
    }
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
    let amended = Instant::now();
    amend(&mut changes)?;
    crate::app::profile::record("editor change set amended", amended);
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
            finalize_settings: Some(&mut finalize_settings),
        },
    )
}

/// Whether a change set creates, moves or removes an instance. An upsert
/// entry that only anchors a lookup does none of that, so values pushed to
/// instances in place need no structural rollback snapshot of the branches
/// that hold them. The values they held are then read by path before the
/// anchors are resolved, which finds the same instances only where each
/// anchor stands in Studio at the place the files give it.
fn changes_instances(changes: &[super::types::EditorInstanceChange]) -> bool {
    let in_place = |instance: &super::types::EditorInstanceDescriptor| {
        let ordinal = |ordinals: &[usize], at: usize| ordinals.get(at).copied().unwrap_or(1);
        // An identity of Studio's capture names its instance whatever the
        // order of its namesakes is by now.
        instance.anchor_only
            && instance.previous_settings_id.is_none()
            && if instance.previous_path_segments.is_empty() {
                !instance.ambiguous_siblings
            } else {
                instance.previous_path_segments == instance.path_segments
                    && (0..instance.path_segments.len()).all(|at| {
                        ordinal(&instance.previous_path_ordinals, at)
                            == ordinal(&instance.path_ordinals, at)
                    })
            }
    };
    changes.iter().any(|change| {
        change.mode != "upsertInstances"
            || change.allow_deletes
            || !change.instances.iter().all(in_place)
    })
}

pub(crate) fn native_editor_full_push_eligible(args: &PushEditorChangesArgs) -> Result<bool> {
    if !args.changed_paths.is_empty()
        || !args.changed_paths_files.is_empty()
        || !args.target_settings_ids.is_empty()
        || !args.target_settings_id_files.is_empty()
        || !args.target_properties.is_empty()
        || args.upsert_instances_only
        || (!args.no_review && !args.yes && !global_yes())
    {
        return Ok(false);
    }
    project_pushes_stores_as_held(args)
}

/// Whether a push takes every instance of a store as the files hold it: the
/// project keeps no instances Studio alone knows, needs no temporary stage,
/// enforces no linked package and filters nothing on the way to Studio.
pub(crate) fn project_pushes_stores_as_held(args: &PushEditorChangesArgs) -> Result<bool> {
    if let Some(project) = config::try_load_project(None, Some(&args.project.project_root))?
        && (project
            .project
            .root
            .ignore_unknown_instances
            .unwrap_or(false)
            || config::project_requires_temporary_stage(&project)?)
    {
        return Ok(false);
    }
    if !args.override_packages && args.project.project_root.join("renium-link.json").exists() {
        return Ok(false);
    }
    if files_to_studio_filter_rules(&args.project.project_root)?.is_some() {
        return Ok(false);
    }
    Ok(true)
}

pub(super) fn collect_project_editor_changes(
    args: &PushEditorChangesArgs,
) -> Result<(EditorChangeSet, Option<config::ProjectionStage>)> {
    collect_project_editor_changes_with_documents(args, PreparedEditorDocuments::default())
}

/// A store with fewer bytes is read whole for a push of chosen instances:
/// its document is small and stays cached.
const TARGETED_STORE_MIN_BYTES: usize = if cfg!(test) { 1 } else { 256 * 1024 };

/// The most instances a push of chosen instances reads of one store before
/// it reads the store whole.
const MAX_TARGETED_INSTANCES: usize = 16_384;

/// The stores a push of chosen instances names, each read for those
/// instances and for what the collection reads beside them: what stands
/// under them where the push takes it along, their ancestors, the namesakes
/// that tell each of these from its siblings, and the roots. Every other
/// instance of such a document stands unread (`held::stand_unchanged`), so
/// the cost of the push follows what it pushes and not the store it is in.
/// Empty where the collection may read past those instances; it then reads
/// each store whole.
fn documents_of_targets(args: &PushEditorChangesArgs) -> Result<PreparedEditorDocuments> {
    let whole = PreparedEditorDocuments::default;
    if args.target_settings_ids.is_empty()
        || !args.target_settings_id_files.is_empty()
        || args.targets_extend_paths
        || args.changed_paths.is_empty()
        || !args.changed_paths_files.is_empty()
        || (!args.override_packages && args.project.project_root.join("renium-link.json").exists())
        || files_to_studio_filter_rules(&args.project.project_root)?.is_some()
    {
        return Ok(whole());
    }
    let (project_root, src_root) = editor_project_roots(args)?;
    let mut prepared = whole();
    for path in &args.changed_paths {
        let path = canonical_editor_changed_path(&project_root, path);
        // A script or a folder beside the store is read with its instance.
        let names_a_store = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(crate::system::files::is_service_settings_file_name);
        let Some(service) = service_from_changed_path(&src_root, &path).filter(|_| names_a_store)
        else {
            return Ok(whole());
        };
        let Ok(bytes) = fs::read(service_settings_path(&src_root.join(&service))) else {
            return Ok(whole());
        };
        if bytes.len() < TARGETED_STORE_MIN_BYTES {
            continue;
        }
        let ids = (args.target_settings_ids.iter())
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let chosen = |instance: &crate::settings::bytecode::SettingsBytecodeInstance| {
            ids.contains(instance.settings_id.as_str())
        };
        let Ok(Some(document)) = document_of_targets(&bytes, chosen, args.exact_target_ids) else {
            return Ok(whole());
        };
        prepared.documents.insert(service, Arc::new(document));
    }
    Ok(prepared)
}

/// The stores `ps --verify` reads, each for its scripts and what tells them
/// from their siblings: the sources alone are compared. A store that cannot
/// be read so is read whole, and so is every store where the collection may
/// read past its scripts.
fn documents_of_scripts(args: &PushEditorChangesArgs) -> Result<PreparedEditorDocuments> {
    let mut prepared = PreparedEditorDocuments::default();
    if (!args.override_packages && args.project.project_root.join("renium-link.json").exists())
        || files_to_studio_filter_rules(&args.project.project_root)?.is_some()
    {
        return Ok(prepared);
    }
    let (project_root, src_root) = editor_project_roots(args)?;
    let mut paths = expand_editor_changed_paths(args)?;
    if paths.is_empty() {
        paths = collect_editor_full_paths(&src_root)?;
    }
    for path in paths {
        let path = canonical_editor_changed_path(&project_root, &path);
        let names_a_store = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(crate::system::files::is_service_settings_file_name);
        let Some(service) = service_from_changed_path(&src_root, &path).filter(|_| names_a_store)
        else {
            continue;
        };
        let Ok(bytes) = fs::read(service_settings_path(&src_root.join(&service))) else {
            continue;
        };
        if bytes.len() < TARGETED_STORE_MIN_BYTES {
            continue;
        }
        let scripts = |instance: &crate::settings::bytecode::SettingsBytecodeInstance| {
            is_lua_source_class(&instance.class_name)
        };
        if let Ok(Some(document)) = document_of_targets(&bytes, scripts, true) {
            prepared.documents.insert(service, Arc::new(document));
        }
    }
    Ok(prepared)
}

/// The changes whose sources `ps --verify` compares with Studio's.
fn collect_verified_editor_changes(
    args: &PushEditorChangesArgs,
) -> Result<(EditorChangeSet, Option<config::ProjectionStage>)> {
    let prepared = documents_of_scripts(args)?;
    if !prepared.documents.is_empty()
        && let Ok(collected) = collect_project_editor_changes_with_documents(args, prepared)
    {
        return Ok(collected);
    }
    collect_project_editor_changes(args)
}

/// One store read for the instances `chosen` (see `documents_of_targets`).
/// `None` where the store holds none of them, or where so many instances are
/// read that the whole store is as well.
fn document_of_targets(
    bytes: &[u8],
    chosen: impl Fn(&crate::settings::bytecode::SettingsBytecodeInstance) -> bool,
    exact: bool,
) -> Result<Option<SettingsBytecode>> {
    use crate::settings::bytecode::property_edit::decode_rows;

    let mut document = SettingsBytecode::decode_structure(bytes)?;
    let count = document.instances.len();
    let mut read = document.instances.iter().map(chosen).collect::<Vec<_>>();
    if !read.contains(&true) {
        return Ok(None);
    }
    let parent = |index: usize| {
        document.instances[index]
            .parent_index
            .filter(|at| *at < count)
    };
    if !exact {
        // What stands under a chosen instance goes with it. A store can hold
        // a parent after its child, so the pass repeats until nothing joins.
        loop {
            let mut joined = false;
            for index in 0..count {
                if !read[index] && parent(index).is_some_and(|at| read[at]) {
                    read[index] = true;
                    joined = true;
                }
            }
            if !joined {
                break;
            }
        }
    }
    // A request names an instance by its path, so each one on that path.
    for index in (0..count).filter(|&index| read[index]).collect::<Vec<_>>() {
        let mut at = parent(index);
        let mut steps = 0;
        while let Some(ancestor) = at.filter(|ancestor| !read[*ancestor]) {
            read[ancestor] = true;
            at = parent(ancestor);
            steps += 1;
            if steps > count {
                break;
            }
        }
    }
    for (instance, read) in document.instances.iter().zip(&mut read) {
        *read |= instance.parent_index.is_none();
    }
    // An instance on a path is told from its namesakes by what each holds.
    let namesakes = (0..count)
        .filter(|&index| read[index])
        .map(|index| {
            let instance = &document.instances[index];
            (instance.parent_index, instance.name.as_str())
        })
        .collect::<HashSet<_>>();
    let rows = (0..count)
        .filter(|&index| {
            let instance = &document.instances[index];
            namesakes.contains(&(instance.parent_index, instance.name.as_str()))
        })
        .collect::<Vec<_>>();
    drop(namesakes);
    if rows.len() > MAX_TARGETED_INSTANCES || rows.len() == count {
        return Ok(None);
    }
    let (decoded, _) = decode_rows(bytes, &rows)?;
    for instance in &mut document.instances {
        crate::settings::held::stand_unchanged(&mut instance.properties);
    }
    for (index, row) in rows.into_iter().zip(decoded) {
        document.instances[index] = row;
    }
    Ok(Some(document))
}

/// Whether a collection read an unread instance of a document for its
/// values: what it sends would then carry that instance's mark.
fn changes_name_an_unread_instance(changes: &EditorChangeSet) -> bool {
    [
        serde_json::to_value(&changes.property_changes),
        serde_json::to_value(&changes.instance_changes),
        serde_json::to_value(&changes.deferred_native_rows),
    ]
    .iter()
    .any(|value| {
        value
            .as_ref()
            .map_or(true, crate::settings::held::names_unchanged)
    })
}

/// The changes of a push, with the stores a push of chosen instances names
/// read for those instances alone where that reads all the collection uses.
fn collect_pushed_editor_changes(
    args: &PushEditorChangesArgs,
) -> Result<(EditorChangeSet, Option<config::ProjectionStage>)> {
    let prepared = documents_of_targets(args)?;
    if prepared.documents.is_empty() {
        return collect_project_editor_changes(args);
    }
    let collected = collect_project_editor_changes_with_documents(args, prepared);
    match collected {
        Ok((changes, projection)) if !changes_name_an_unread_instance(&changes) => {
            Ok((changes, projection))
        }
        // Whatever the stores read whole say, an error included.
        _ => collect_project_editor_changes(args),
    }
}

fn collect_project_editor_changes_with_documents(
    args: &PushEditorChangesArgs,
    mut prepared_documents: PreparedEditorDocuments,
) -> Result<(EditorChangeSet, Option<config::ProjectionStage>)> {
    let phase_started = Instant::now();
    let Some(loaded) = config::try_load_project(None, Some(&args.project.project_root))? else {
        return Ok((collect_editor_changes(args)?, None));
    };
    log_editor_collection_timing("project load", phase_started);
    let phase_started = Instant::now();
    let authored = args
        .link_project_root
        .as_deref()
        .map(|root| config::try_load_project(None, Some(root)))
        .transpose()?
        .flatten();
    let link_enforcement = build_loaded_project_link_enforcement(
        authored.as_ref().unwrap_or(&loaded),
        args.override_packages,
    )?;
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
    let mut projected_instance_paths = Vec::new();
    let mut projected_whole_instance_paths = Vec::new();
    let mut projected_paths = if full_selection {
        collect_editor_full_paths(projection.root())?
    } else {
        let mut paths = Vec::new();
        let naming = config::project_script_naming(&loaded.project);
        // A store named in the push carries its whole service, and with it
        // every folder and file of that service named beside it.
        let mut whole_services = HashSet::new();
        for changed_path in &changed_paths {
            if changed_path.extension().is_some_and(|ext| ext == "renium") {
                let absolute = absolutize_under(&loaded.root, changed_path);
                whole_services.extend(
                    config::project_source_to_staged_relatives(&loaded, &absolute)?
                        .iter()
                        .filter_map(|relative| staged_service(relative)),
                );
            }
        }
        let folders =
            named_push_folders(&loaded, &changed_paths, projection.root(), &whole_services)?;
        paths.extend(folders.staged.iter().cloned());
        let mut instance_paths = Vec::new();
        let mut whole_instance_paths = Vec::new();
        for changed_path in changed_paths {
            let absolute = absolutize_under(&loaded.root, &changed_path);
            let source_file = !absolute.is_dir()
                && absolute
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| infer_source_script(name, &naming))
                    .is_some();
            let mut mapped =
                config::project_source_to_staged_paths(&loaded, &absolute, projection.root())?;
            let produced = if source_file || folders.cover(&absolute) {
                Some(NamedFileInstances::default())
            } else if args.link_project_root.is_some() {
                // A planned push names exactly what it compared.
                None
            } else {
                named_file_instances(&loaded, &projection, &absolute, &whole_services)?
            };
            if let Some(produced) = produced {
                mapped.retain(|path| {
                    !path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(is_service_settings_file_name)
                });
                if produced.whole {
                    whole_instance_paths.extend(produced.staged.iter().cloned());
                }
                instance_paths.extend(produced.staged);
            }
            paths.extend(mapped);
        }
        paths.extend(instance_paths.iter().cloned());
        projected_instance_paths = instance_paths;
        projected_whole_instance_paths = whole_instance_paths;
        paths
    };
    projected_paths.sort();
    projected_paths.dedup();
    if projected_paths.is_empty() {
        return Ok((EditorChangeSet::default(), Some(projection)));
    }
    let mut projected_args = args.clone();
    projected_args.project.project_root = projection.root().to_path_buf();
    projected_args.project.src_root = PathBuf::from(".");
    projected_args.changed_paths = projected_paths;
    projected_args.changed_paths_files.clear();
    projected_args.link_cache_dir = None;
    let (project_root, src_root) = editor_project_roots(&projected_args)?;
    let mut changes = collect_editor_changes_with_link_enforcement_and_documents(
        &projected_args,
        &project_root,
        &src_root,
        &link_enforcement,
        &mut PreparedEditorDocuments {
            instance_paths: projected_instance_paths
                .iter()
                .map(|path| path_key(&canonical_editor_changed_path(&project_root, path)))
                .collect(),
            whole_instance_paths: projected_whole_instance_paths
                .iter()
                .map(|path| path_key(&canonical_editor_changed_path(&project_root, path)))
                .collect(),
            ..PreparedEditorDocuments::default()
        },
    )?;
    // Projection also creates identities for new source files. Publish their
    // metadata through the ordinary guarded settings transaction, so later
    // edits/deletions resolve the same instances without requiring a pull.
    // Keep writes to the temporary projection out of the authored project.
    for write in changes.settings_writes.drain(..) {
        write.document.write_file(&write.path)?;
    }
    for (path, bytes) in config::projected_settings_writes(&loaded, projection.root())? {
        changes.settings_writes.push(EditorSettingsWrite {
            expected_hash: settings_file_hash(&path)?,
            path,
            document: Arc::new(crate::settings::bytecode::decode_settings_bytecode(&bytes)?),
            encoded: None,
        });
    }
    Ok((changes, Some(projection)))
}

/// The folders a push names, and the mapped sources inside them, sorted by
/// how each is pushed: as the subtree it maps to (`staged` holds those staged
/// folders), or as a whole service because it maps to one.
#[derive(Default)]
struct NamedPushFolders {
    subtrees: Vec<PathBuf>,
    services: Vec<PathBuf>,
    staged: Vec<PathBuf>,
}

impl NamedPushFolders {
    /// Whether the nearest such folder around a path is pushed as a subtree,
    /// which then carries the path's instance data.
    fn cover(&self, path: &Path) -> bool {
        let nearest = |folders: &[PathBuf]| {
            folders
                .iter()
                .filter(|folder| path.starts_with(folder))
                .map(|folder| folder.components().count())
                .max()
        };
        nearest(&self.subtrees) > nearest(&self.services)
    }
}

fn named_push_folders(
    loaded: &config::LoadedProject,
    changed_paths: &[PathBuf],
    stage_root: &Path,
    whole_services: &HashSet<String>,
) -> Result<NamedPushFolders> {
    let mut folders = NamedPushFolders::default();
    let mut mapped_sources = None;
    for changed_path in changed_paths {
        let named = absolutize_under(&loaded.root, changed_path);
        if !named.is_dir() {
            continue;
        }
        if mapped_sources.is_none() {
            mapped_sources = Some(config::project_source_roots(loaded)?);
        }
        // A mapped source inside the named folder may land in another service.
        let inner = mapped_sources
            .iter()
            .flatten()
            .filter(|source| source.is_dir() && **source != named && source.starts_with(&named))
            .cloned()
            .collect::<Vec<_>>();
        for folder in std::iter::once(named).chain(inner) {
            let staged =
                staged_folders_below_service(loaded, &folder, stage_root)?.filter(|staged| {
                    !staged.iter().any(|staged| {
                        staged
                            .strip_prefix(stage_root)
                            .ok()
                            .and_then(staged_service)
                            .is_some_and(|service| whole_services.contains(&service))
                    })
                });
            match staged {
                Some(staged) => {
                    folders.staged.extend(staged);
                    folders.subtrees.push(folder);
                }
                None => folders.services.push(folder),
            }
        }
    }
    Ok(folders)
}

/// The service a path of the staged copy lies in.
fn staged_service(relative: &Path) -> Option<String> {
    match relative.components().next()? {
        std::path::Component::Normal(service) => service.to_str().map(str::to_owned),
        _ => None,
    }
}

/// What a named file that is no script pushes: staged paths that name
/// instances, and whether the file defines those instances in full.
#[derive(Default)]
struct NamedFileInstances {
    staged: Vec<PathBuf>,
    whole: bool,
}

/// A metadata file pushes the instance it describes. The source of a sync
/// rule pushes the instance it makes, in full: what Studio holds below it and
/// the file no longer makes is removed. A file that makes no instance pushes
/// nothing. `None` leaves the file to its whole service: a store or a nested
/// project, a file of a service whose store is pushed with it, and a file
/// that describes a service itself.
fn named_file_instances(
    loaded: &config::LoadedProject,
    projection: &config::ProjectionStage,
    source: &Path,
    whole_services: &HashSet<String>,
) -> Result<Option<NamedFileInstances>> {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !source.is_file()
        || name.ends_with(".renium")
        || name.ends_with(".project.json")
        || name.ends_with(".project.jsonc")
    {
        return Ok(None);
    }
    let relatives = config::project_source_to_staged_relatives(loaded, source)?;
    if relatives.is_empty()
        || relatives.iter().any(|relative| {
            relative.components().count() < 2
                || staged_service(relative).is_none_or(|service| whole_services.contains(&service))
        })
    {
        return Ok(None);
    }
    let mut instances = projection.transformed_targets_for_source(source);
    let whole = !instances.is_empty();
    for relative in &relatives {
        instances.extend(config::metadata_sidecar_instance(loaded, relative)?);
    }
    let mut staged = Vec::new();
    for instance in instances {
        let Some((service, below)) = instance
            .split_first()
            .filter(|(_, below)| !below.is_empty())
        else {
            return Ok(None);
        };
        if whole_services.contains(service) {
            return Ok(None);
        }
        staged.push(
            below
                .iter()
                .fold(projection.root().join(service), |path, name| {
                    path.join(crate::system::files::sanitize_name(name))
                }),
        );
    }
    Ok(Some(NamedFileInstances { staged, whole }))
}

/// The staged folders a named source folder becomes, when each lies below a
/// service. Such a folder is pushed as the instance it maps to and what that
/// holds; a folder that maps to a service is the whole service.
fn staged_folders_below_service(
    loaded: &config::LoadedProject,
    source: &Path,
    stage_root: &Path,
) -> Result<Option<Vec<PathBuf>>> {
    if !source.is_dir() {
        return Ok(None);
    }
    let relatives = config::project_source_to_staged_relatives(loaded, source)?;
    let folders = relatives
        .iter()
        .filter(|relative| relative.components().count() > 1)
        .map(|relative| stage_root.join(relative))
        .filter(|staged| staged.is_dir())
        .collect::<Vec<_>>();
    Ok((!folders.is_empty() && folders.len() == relatives.len()).then_some(folders))
}

fn log_editor_collection_timing(label: &str, started: Instant) {
    crate::app::profile::record(&format!("editor collection {label}"), started);
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
    /// Studio's own setting that keeps instances the files do not know.
    #[serde(default)]
    keep_unknowns: bool,
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

pub(crate) fn files_to_studio_filter_rules(
    project_root: &Path,
) -> Result<Option<Vec<config::FilterRule>>> {
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

/// Whether one instance path lies below another, sibling ordinals included;
/// an ordinal that is not written is the first sibling.
fn instance_path_is_below(
    segments: &[String],
    ordinals: &[usize],
    root_segments: &[String],
    root_ordinals: &[usize],
) -> bool {
    segments.len() > root_segments.len()
        && segments.starts_with(root_segments)
        && (0..root_segments.len()).all(|index| {
            ordinals.get(index).copied().unwrap_or(1)
                == root_ordinals.get(index).copied().unwrap_or(1)
        })
}

/// Every instance Studio holds in a service, and whether Studio is set to
/// keep the ones the files do not know.
fn studio_service_instances(
    bridge: &BridgeServer,
    service: &str,
) -> Result<(Vec<EditorFilterCandidateRow>, bool)> {
    let mut rows = Vec::new();
    let mut keep_unknowns = false;
    let mut start_index = 1usize;
    let mut snapshot_id = None;
    loop {
        let page: EditorFilterCandidatePage = serde_json::from_value(bridge.call(
            "getEditorFilterCandidates",
            json!({
                "service": service,
                "startIndex": start_index,
                "maxCount": 500,
                "snapshotId": snapshot_id,
                "includeSettingsIds": true,
            }),
        )?)
        .context("Studio returned invalid instance candidates")?;
        if snapshot_id.is_none() {
            snapshot_id.clone_from(&page.snapshot_id);
        }
        keep_unknowns |= page.keep_unknowns;
        rows.extend(page.items);
        let Some(next_index) = page.next_index else {
            return Ok((rows, keep_unknowns));
        };
        if next_index <= start_index {
            bail!("Studio instance candidate cursor did not advance");
        }
        start_index = next_index;
    }
}

/// Finds what Studio holds below each instance a file defines in full and
/// the files do not make, and removes it ahead of the push. The push itself
/// only creates and updates, so the rest of the service is left alone.
///
/// A removal has to be certain. An instance stays when the files hold any
/// instance by its path of names, since same-named siblings are told apart
/// by position alone; when its whole instance cannot be told apart in Studio
/// by name and class; and wherever the project or Studio keeps unknown
/// instances.
fn attach_whole_instance_removals(
    args: &PushEditorChangesArgs,
    bridge: &BridgeServer,
    changes: &mut EditorChangeSet,
    projection: Option<&config::ProjectionStage>,
) -> Result<()> {
    let whole_instances = std::mem::take(&mut changes.whole_instances);
    if whole_instances.is_empty() || args.upsert_instances_only {
        return Ok(());
    }
    let desired = build_editor_filter_candidates(args, changes, projection)?;
    let mut desired_names = HashSet::new();
    for key in desired.by_path.keys() {
        if let Ok((segments, _)) = serde_json::from_str::<(Vec<String>, Vec<usize>)>(key) {
            desired_names.insert(segments);
        }
    }
    let mut roots_by_service: BTreeMap<String, Vec<EditorInstancePath>> = BTreeMap::new();
    for (service, root) in whole_instances {
        roots_by_service.entry(service).or_default().push(root);
    }
    let kept_targets = files_to_studio_ignore_unknown_targets(
        &args.project.project_root,
        &roots_by_service.keys().cloned().collect(),
    )?;
    let mut removals = Vec::new();
    for (service, roots) in roots_by_service {
        let (rows, keep_unknowns) = studio_service_instances(bridge, &service)?;
        if keep_unknowns {
            continue;
        }
        let kept = kept_targets.get(&service).map_or(&[][..], Vec::as_slice);
        // A whole instance is looked for where the files place it. It counts
        // only when Studio holds one instance by that path of names and that
        // one has the class the file gives it.
        let roots = roots
            .into_iter()
            .filter(|root| {
                let mut named = rows
                    .iter()
                    .filter(|row| row.path_segments == root.path_segments);
                let class = desired
                    .by_path
                    .get(&editor_filter_path_key(
                        &root.path_segments,
                        &root.path_ordinals,
                    ))
                    .map(|index| desired.candidates[*index].class_name.as_str());
                matches!(
                    (named.next(), named.next()),
                    (Some(only), None) if class == Some(only.class_name.as_str())
                )
            })
            .collect::<Vec<_>>();
        let unknown = rows
            .iter()
            .filter(|row| {
                row.class_name != "PackageLink"
                    && roots.iter().any(|root| {
                        instance_path_is_below(
                            &row.path_segments,
                            &row.path_ordinals,
                            &root.path_segments,
                            &root.path_ordinals,
                        )
                    })
                    && !kept
                        .iter()
                        .any(|target| row.path_segments.starts_with(target))
                    && !desired_names.contains(&row.path_segments)
                    && !desired.by_id.contains_key(&(
                        service.clone(),
                        canonical_filter_settings_id(projection, &row.settings_id),
                    ))
            })
            .collect::<Vec<_>>();
        // An instance below one that is removed goes with it.
        let instances = unknown
            .iter()
            .filter(|row| {
                !unknown.iter().any(|above| {
                    instance_path_is_below(
                        &row.path_segments,
                        &row.path_ordinals,
                        &above.path_segments,
                        &above.path_ordinals,
                    )
                })
            })
            .map(|row| EditorInstanceDescriptor {
                // Studio's own identity for the instance outranks its path;
                // one Studio gave no identity is named by its path.
                settings_id: if row.settings_id.is_empty() {
                    format!(
                        "studio:{}",
                        editor_filter_path_key(&row.path_segments, &row.path_ordinals)
                    )
                } else {
                    row.settings_id.clone()
                },
                path_segments: row.path_segments.clone(),
                path_ordinals: row.path_ordinals.clone(),
                previous_settings_id: (!row.settings_id.is_empty())
                    .then(|| row.settings_id.clone()),
                ..EditorInstanceDescriptor::default()
            })
            .collect::<Vec<_>>();
        if !instances.is_empty() {
            removals.push(EditorInstanceChange {
                mode: "deleteInstances".to_string(),
                service,
                allow_deletes: false,
                instances,
                preserve_instances: Vec::new(),
            });
        }
    }
    changes.instance_changes.splice(0..0, removals);
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
            // Temporary projections already contain the updated full-service
            // document; published writes may be only a mapped subtree.
            if projection.is_some_and(config::ProjectionStage::is_temporary) {
                return None;
            }
            let service = service_from_changed_path(&src_root, &write.path)?;
            Some((service, &*write.document))
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
    attach_whole_instance_removals(args, bridge, changes, projection)?;
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
    let mut verification = verify_editor_source_changes(bridge, changes, transaction_id)?;
    if !verification.failed_indexes.is_empty() {
        let retry_changes = EditorChangeSet {
            source_changes: verification
                .failed_indexes
                .iter()
                .map(|index| changes.source_changes[*index].clone())
                .collect(),
            ..EditorChangeSet::default()
        };
        send_editor_change_batches(bridge, &retry_changes, None, transaction_id)?;
        verification = verify_editor_source_changes(bridge, changes, transaction_id)?;
    }
    summary.insert(
        "sourceVerified".to_string(),
        json!(verification.verified as u64),
    );
    summary.insert(
        "sourceVerifyFailed".to_string(),
        json!(verification.failed.len() as u64),
    );
    summary.insert(
        "sourceInexact".to_string(),
        json!(verification.inexact as u64),
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
    supplemental: usize,
}

// Saved fields with no plugin API (Lighting.Technology, Players.BanningEnabled,
// Path2D.Transparency, ...) are ordinary settings a user can change in Studio,
// so the push writes them natively without an approval; whatever cannot be
// written is named in the result.
pub(crate) fn finish_committed_saved_fields(
    saved: Option<&crate::editor::types::PushSavedFields>,
    project: &Path,
    bridge: &BridgeServer,
    summary: &mut Map<String, Value>,
) {
    let Some(saved) = saved else { return };
    let receipt = summary.get("protectedOfflineApply").cloned();
    if let Err(error) =
        apply_saved_field_differences(saved, project, bridge, summary, receipt.as_ref())
    {
        record_unsupported_properties(
            summary,
            saved
                .fields
                .iter()
                .map(|field| format!("{} ({error:#})", field.label())),
        );
    }
    warn_unsupported_properties(summary);
}

fn apply_saved_field_differences(
    saved: &crate::editor::types::PushSavedFields,
    project: &Path,
    bridge: &BridgeServer,
    summary: &mut Map<String, Value>,
    offline_receipt: Option<&Value>,
) -> Result<()> {
    apply_saved_field_writes(
        saved,
        project,
        bridge,
        summary,
        offline_receipt,
        |runtime, field| {
            crate::studio::automation::property_access::write_saved_field(
                bridge,
                runtime,
                &field.path_segments,
                &[],
                &field.property,
                &field.value,
            )
        },
    )
}

fn apply_saved_field_writes(
    saved: &crate::editor::types::PushSavedFields,
    project: &Path,
    bridge: &BridgeServer,
    summary: &mut Map<String, Value>,
    offline_receipt: Option<&Value>,
    mut write: impl FnMut(&str, &crate::settings::equivalence::SavedFieldDifference) -> Result<bool>,
) -> Result<()> {
    let (runtime, _replacement_gate) = match offline_receipt {
        Some(receipt) => {
            saved.validate(
                project,
                receipt["previousRuntimeId"].as_str().unwrap_or_default(),
            )?;
            let runtime =
                super::review::snapshot_replacement_runtime(Some(&saved.runtime), receipt)?;
            let gate = bridge.acquire_runtime_gate_within(runtime, "push", bridge.active_request_lease().as_deref(), Some(Duration::from_secs(1)))?.context(
                "The reopened place is busy. Its saved fields are still pending; retry the push.",
            )?;
            anyhow::ensure!(
                bridge.edit_runtime(Some(runtime)).is_some(),
                "The reopened place disconnected. Its saved fields are still pending; reopen the place and retry the push."
            );
            (runtime, Some(gate))
        }
        None => {
            let current = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
            saved.validate(project, &current.runtime_id)?;
            (saved.runtime.as_str(), None)
        }
    };
    let mut applied = 0u64;
    let mut unsupported = Vec::new();
    for difference in &saved.fields {
        if difference.ambiguous {
            unsupported.push(format!(
                "{} (duplicate sibling names along {})",
                difference.label(),
                difference.path_segments.join(".")
            ));
            continue;
        }
        match write(runtime, difference) {
            Ok(true) => applied += 1,
            Ok(false) => {}
            Err(error) => unsupported.push(format!("{} ({error:#})", difference.label())),
        }
    }
    if applied > 0 {
        summary.insert("savedFieldsApplied".to_string(), json!(applied));
    }
    record_unsupported_properties(summary, unsupported);
    Ok(())
}

fn record_unsupported_properties(
    summary: &mut Map<String, Value>,
    names: impl IntoIterator<Item = String>,
) {
    let mut merged = summary
        .get("unsupportedProperties")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    merged.extend(names);
    if !merged.is_empty() {
        summary.insert(
            "unsupportedProperties".to_string(),
            json!(merged.into_iter().collect::<Vec<_>>()),
        );
    }
}

fn warn_unsupported_properties(summary: &Map<String, Value>) {
    let names = summary
        .get("unsupportedProperties")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if names.is_empty() {
        return;
    }
    log_global(
        2,
        format_args!(
            "[renium] warning: {} saved propert{} could not be written to the open place; the files keep the value: {}",
            names.len(),
            if names.len() == 1 { "y" } else { "ies" },
            names.join(", ")
        ),
    );
}

struct PendingProtectedWrites {
    writes: Vec<Value>,
    supplemental: usize,
}

#[cfg(test)]
fn pending_protected_writes(
    bridge: &BridgeServer,
    summary: &mut Map<String, Value>,
    pre_routed: Vec<Value>,
    imported_suffix: usize,
    local_place: &dyn Fn(&BridgeServer) -> Option<PathBuf>,
) -> PendingProtectedWrites {
    pending_protected_writes_cached(
        bridge,
        summary,
        pre_routed,
        imported_suffix,
        local_place,
        &mut ProtectedPlaceCache::default(),
    )
}

fn pending_protected_writes_cached(
    bridge: &BridgeServer,
    summary: &mut Map<String, Value>,
    pre_routed: Vec<Value>,
    imported_suffix: usize,
    local_place: &dyn Fn(&BridgeServer) -> Option<PathBuf>,
    cache: &mut ProtectedPlaceCache,
) -> PendingProtectedWrites {
    let mut reported = summary
        .get("protectedWrites")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut studio_pending = vec![true; reported.len()];
    let mut live_checked = vec![false; reported.len()];
    let pre_routed_count = pre_routed.len();
    let pre_routed =
        protected_root_write_rows_with_live_values(bridge, pre_routed).unwrap_or_else(|rows| rows);
    studio_pending.extend(
        pre_routed
            .iter()
            .map(|row| !protected_write_matches_previous(row)),
    );
    live_checked.extend(vec![true; pre_routed.len()]);
    reported.extend(pre_routed);
    if pre_routed_count > 0 {
        summary.insert(
            "protectedPreRouted".to_string(),
            json!(pre_routed_count as u64),
        );
    }
    let reported_count = reported.len();
    let applicable = reported
        .into_iter()
        .enumerate()
        .filter(|(_, row)| {
            !is_externally_managed_protected_write(row) && is_user_facing_protected_write(row)
        })
        .map(|(index, row)| {
            (
                (
                    studio_pending[index],
                    live_checked[index],
                    index >= reported_count - imported_suffix,
                ),
                row,
            )
        })
        .collect::<Vec<_>>();
    let unavailable_count = reported_count - applicable.len();
    let applicable_rows = applicable
        .iter()
        .map(|(_, row)| row.clone())
        .collect::<Vec<_>>();
    let enriched_rows = if applicable_rows.is_empty() {
        applicable_rows
    } else {
        local_place(bridge)
            .and_then(|path| cache.enrich(&path, &applicable_rows).ok())
            .unwrap_or(applicable_rows)
    };
    let enriched = applicable
        .into_iter()
        .zip(enriched_rows)
        .map(|((flags, _), row)| (flags, row))
        .collect::<Vec<_>>();
    let pending = enriched
        .iter()
        .filter(|((studio_pending, live_checked, _), row)| {
            let differs = !protected_write_matches_previous(row);
            if *live_checked {
                *studio_pending || differs
            } else {
                differs
            }
        })
        .collect::<Vec<_>>();
    let supplemental = pending
        .iter()
        .filter(|((_, _, imported), _)| *imported)
        .count();
    let writes: Vec<_> = pending.into_iter().map(|(_, row)| row.clone()).collect();
    summary.remove("protectedWrites");
    if !writes.is_empty() {
        summary.insert("protectedPending".to_string(), json!(writes.len() as u64));
    }
    if unavailable_count > 0 {
        summary.insert(
            "unavailableProtectedSkipped".to_string(),
            json!(unavailable_count as u64),
        );
    }
    let already_current = enriched.len() - writes.len();
    if already_current > 0 {
        summary.insert(
            "protectedAlreadyCurrent".to_string(),
            json!(already_current as u64),
        );
    }
    PendingProtectedWrites {
        writes,
        supplemental,
    }
}

fn protected_write_intent(row: &Value) -> Value {
    let mut intent = row.clone();
    if let Some(fields) = intent.as_object_mut() {
        for key in ["oldValue", "oldValueKnown", "oldValueMissing"] {
            fields.remove(key);
        }
    }
    intent
}

fn prepare_protected_writes(
    args: &PushEditorChangesArgs,
    bridge: &BridgeServer,
    pending: PendingProtectedWrites,
    offline_route: bool,
    approved: &HashSet<Value>,
    offline_declined: bool,
) -> Result<ProtectedWritePlan> {
    let PendingProtectedWrites {
        writes,
        supplemental,
    } = pending;
    if args.no_review && !writes.is_empty() {
        let named = writes
            .iter()
            .take(5)
            .map(|row| {
                let path = row
                    .get("pathSegments")
                    .and_then(Value::as_array)
                    .map(|segments| {
                        segments
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(".")
                    })
                    .unwrap_or_default();
                let name = row.get("name").and_then(Value::as_str).unwrap_or("?");
                match (row.get("oldValue"), row.get("value")) {
                    (Some(previous), Some(requested)) => {
                        format!("{path}.{name} ({previous} -> {requested})")
                    }
                    _ => format!("{path}.{name}"),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "Studio could not apply {} protected write(s) ({named}); all changes were rolled back and remain pending",
            writes.len()
        );
    }
    let needs_approval = !args.yes && !global_yes();
    let apply_offline = if offline_route {
        if needs_approval {
            let additional = writes
                .iter()
                .filter(|row| !approved.contains(&protected_write_intent(row)))
                .cloned()
                .collect::<Vec<_>>();
            if !request_protected_write_review(bridge, &additional)? {
                return Err(crate::automation::reconcile::SkippedPushReview.into());
            }
        }
        true
    } else {
        !writes.is_empty()
            && !args.no_review
            && !offline_declined
            && (!needs_approval || request_protected_write_review(bridge, &writes)?)
    };
    if crate::app::output::global_log_enabled(5) && !writes.is_empty() {
        eprintln!(
            "[renium] protected writes: {}",
            serde_json::to_string(&writes)?
        );
    }
    Ok(ProtectedWritePlan {
        writes,
        apply_offline,
        supplemental,
    })
}

type SettingsFinalizer<'a> = dyn FnMut(&mut EditorChangeSet) -> Result<()> + 'a;

struct CollectedPushOptions<'a> {
    started: Instant,
    projection: Option<&'a config::ProjectionStage>,
    prepared_binary_import: Option<EditorBinaryImport>,
    guard: Option<&'a StudioChangeGuard>,
    validate_project: Option<&'a dyn Fn() -> Result<()>>,
    finalize_settings: Option<&'a mut SettingsFinalizer<'a>>,
}

/// The project whose configuration decides the review: a staged push collects
/// in a private copy that carries no place configuration.
fn review_project_root(args: &PushEditorChangesArgs) -> &Path {
    args.link_project_root
        .as_deref()
        .unwrap_or(&args.project.project_root)
}

fn push_editor_changes_with_collected(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
    changes: EditorChangeSet,
    options: CollectedPushOptions<'_>,
) -> Result<Map<String, Value>> {
    push_with_root_availability(
        args,
        bridge,
        changes,
        options,
        super::native_roots::available,
        PLACE_FILE,
    )
}

type ApplyPlaceFile<'a> = dyn Fn(
        &BridgeServer,
        &PushEditorChangesArgs,
        &[Value],
        &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value>
    + 'a;
type PlacePath<'a> = dyn Fn(&BridgeServer) -> Option<PathBuf> + 'a;
type ReadPending<'a> = dyn Fn(
        &BridgeServer,
        &mut Map<String, Value>,
        Vec<Value>,
        usize,
        &mut ProtectedPlaceCache,
    ) -> PendingProtectedWrites
    + 'a;

struct PlaceFileAccess<'a> {
    path: &'a PlacePath<'a>,
    pending: &'a ReadPending<'a>,
    apply: &'a ApplyPlaceFile<'a>,
}

const PLACE_FILE: PlaceFileAccess<'static> = PlaceFileAccess {
    path: &local_place_path_for_bridge,
    pending: &|bridge, summary, rows, suffix, cache| {
        pending_protected_writes_cached(
            bridge,
            summary,
            rows,
            suffix,
            &local_place_path_for_bridge,
            cache,
        )
    },
    apply: &|bridge, args, rows, commit| apply_protected_writes_offline(bridge, args, rows, commit),
};

fn push_with_root_availability(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
    changes: EditorChangeSet,
    options: CollectedPushOptions<'_>,
    root_writes_available: impl FnOnce(&BridgeServer) -> Result<bool>,
    place: PlaceFileAccess<'_>,
) -> Result<serde_json::Map<String, Value>> {
    push_with_reader_platform(
        args,
        bridge,
        changes,
        options,
        root_writes_available,
        place,
        cfg!(windows),
    )
}

fn push_with_reader_platform(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
    mut changes: EditorChangeSet,
    options: CollectedPushOptions<'_>,
    root_writes_available: impl FnOnce(&BridgeServer) -> Result<bool>,
    place: PlaceFileAccess<'_>,
    windows_reader: bool,
) -> Result<serde_json::Map<String, Value>> {
    let CollectedPushOptions {
        started,
        projection,
        prepared_binary_import,
        guard,
        validate_project,
        finalize_settings,
    } = options;
    super::native_roots::normalize_resets(&mut changes)?;
    let phase_started = Instant::now();
    apply_files_to_studio_filters(&args, bridge, &mut changes, projection)?;
    announce_unreviewed_deletions(&args, &changes);
    log_timing("native editor push filters", phase_started);
    let phase_started = Instant::now();
    let (unchanged_sources, inexact_unchanged) = if prepared_binary_import.is_none() {
        drop_unchanged_source_changes(bridge, &mut changes)?
    } else {
        (0, 0)
    };
    log_timing("native editor unchanged source filter", phase_started);
    let phase_started = Instant::now();
    let ownership_targets = editor_mutation_package_targets(&changes, None);
    if !ownership_targets.is_empty() {
        discover_editor_mutation_packages_with_timeout(
            bridge,
            &ownership_targets,
            None,
            Some(Duration::from_secs(20)),
        )?;
    }
    log_timing("native editor package ownership", phase_started);
    let phase_started = Instant::now();
    let mut review_payload = (!args.no_review
        && !args.yes
        && !global_yes()
        && (!changes.instance_changes.is_empty()
            || !changes.source_changes.is_empty()
            || !changes.property_changes.is_empty()))
    .then(|| {
        (
            editor_review_payload(&changes),
            skipped_editor_summary(&changes),
        )
    });
    let review_payload_present = review_payload.is_some();
    log_timing("native editor review payload", phase_started);
    let reviews_shown = std::cell::Cell::new(0);
    let mut review = |report| -> Result<Option<Map<String, Value>>> {
        let Some((payload, skipped)) = review_payload.take() else {
            return Ok(None);
        };
        let (outcome, shown) = request_editor_push_review_payload(
            bridge,
            payload,
            Some(review_project_root(&args)),
            report,
        )?;
        reviews_shown.set(reviews_shown.get() + usize::from(shown));
        Ok(match outcome {
            EditorReviewOutcome::Proceed => None,
            EditorReviewOutcome::Skipped => Some(skipped),
            EditorReviewOutcome::Held(held) => Some(held),
        })
    };
    // A review can return immediately when its outcome cannot depend on the reader.
    if (args.slow_import
        || (prepared_binary_import.is_none()
            && (!windows_reader || crate::studio::native::import::service_reader_disabled(&args)))
        || (review_payload_present && individual_writes_fit_before_build(&changes)))
        && let Some(summary) = review(args.review_report)?
    {
        return Ok(summary);
    }
    let mut selection = if prepared_binary_import.is_none() {
        select_editor_binary_import(&args, &changes, bridge)?
    } else {
        None
    };
    let selected_after_review = reviews_shown.get();
    let fallback = prepared_binary_import
        .as_ref()
        .and_then(|import| import.fallback_reason.as_ref())
        .or_else(|| {
            selection
                .as_ref()
                .and_then(|plan| plan.fallback_reason.as_ref())
        });
    if !matches!(fallback, Some(ServiceReaderFallback::DiscoveryFailed(_)))
        && let Some(summary) = review(args.review_report)?
    {
        return Ok(summary);
    }
    if prepared_binary_import.is_none() && reviews_shown.get() != selected_after_review {
        crate::studio::native::import::refresh_import_after_review(
            &mut selection,
            &changes,
            bridge,
        )?;
    }
    let mut binary_import = if prepared_binary_import.is_some() {
        prepared_binary_import
    } else if let Some(projection) = projection.filter(|stage| stage.is_temporary()) {
        let mut projected_args = args.clone();
        projected_args.project.project_root = projection.root().to_path_buf();
        projected_args.project.src_root = PathBuf::from(".");
        build_native_import(&projected_args, &changes, bridge, selection)?
    } else {
        build_native_import(&args, &changes, bridge, selection)?
    };
    let stores_root = match projection.filter(|stage| stage.is_temporary()) {
        Some(projection) => projection.root().to_path_buf(),
        None => absolutize_under(&args.project.project_root, &args.project.src_root),
    };
    let supplemental = supplemental_documents(&changes, binary_import.as_mut(), &stores_root)?;
    changes.native_property_documents.extend(supplemental);
    materialize_native_property_changes(
        &mut changes,
        binary_import.as_ref(),
        &args.project.project_root,
        &stores_root,
    )?;
    defer_imported_native_root_rows(&mut changes, binary_import.as_ref());
    bind_joints_last(&mut changes.property_changes);
    if global_log_enabled(5) {
        log_change_set_composition(&changes, binary_import.as_ref());
    }
    let phase_started = Instant::now();
    let mut pre_routed_protected_writes =
        take_pre_routed_protected_writes(&mut changes, binary_import.as_ref());
    let mut rerouted_fields = 0;
    let routed = !changes.deferred_native_rows.is_empty() && !root_writes_available(bridge)?;
    log_timing("native editor protected write routing", phase_started);
    if routed {
        // Plugin-only imports cannot run the native follow-up transaction. Keep
        // its saved fields in the original transaction's protected snapshot.
        let rerouted = protected_write_rows(&std::mem::take(&mut changes.deferred_native_rows));
        rerouted_fields = rerouted.len();
        pre_routed_protected_writes.extend(rerouted);
    }
    let fallback = binary_import
        .as_ref()
        .and_then(|import| import.fallback_reason.as_ref());
    let local_place = (place.path)(bridge).filter(|path| path.is_file());
    let place_values = std::cell::RefCell::new(ProtectedPlaceCache::default());
    let pending_before = std::cell::LazyCell::new(|| {
        if pre_routed_protected_writes.is_empty() || local_place.is_none() {
            return Vec::new();
        }
        (place.pending)(
            bridge,
            &mut Map::new(),
            pre_routed_protected_writes.clone(),
            rerouted_fields,
            &mut place_values.borrow_mut(),
        )
        .writes
    });
    let requires_offline = check_import_route(
        &changes.deferred_native_rows,
        fallback,
        args.slow_import,
        !args.no_review && local_place.is_some(),
        || !pending_before.is_empty(),
    )?;
    if let Some(summary) = review(args.review_report && !requires_offline)? {
        return Ok(summary);
    }
    let needs_approval = !args.no_review && !args.yes && !global_yes();
    let mut offline_route = requires_offline || (needs_approval && !pending_before.is_empty());
    let mut offline_declined = false;
    let mut approved = HashSet::new();
    if offline_route && needs_approval {
        let mut rows = Vec::clone(&pending_before);
        rows.extend(protected_write_rows(&changes.deferred_native_rows));
        if !request_protected_write_review(bridge, &rows)? {
            if requires_offline {
                return Ok(skipped_editor_summary(&changes));
            }
            offline_route = false;
            offline_declined = true;
        } else {
            approved.extend(rows.iter().map(protected_write_intent));
        }
    }
    drop(pending_before);
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
    let mut history_transaction = if binary_import.is_some() {
        None
    } else {
        save_editor_history_entries(bridge, &args.project.project_root, &changes)?
    };
    let phase_started = Instant::now();
    if let Some(saved) = &args.saved_fields {
        let current = bridge.cached_bridge_info_for_target(BridgeTarget::Edit)?;
        saved.validate(review_project_root(&args), &current.runtime_id)?;
    }
    if let Some(validate_project) = validate_project {
        validate_project()?;
    }
    let mut transaction = EditorTransaction::begin(
        bridge,
        &changes,
        binary_import.as_ref(),
        guard,
        args.mutation_started.as_deref(),
    )?;
    let result = (|| {
        log_timing("native editor transaction begin", phase_started);
        let can_inline_commit = can_inline_bulk_commit(
            transaction.is_some(),
            &changes,
            binary_import.is_some(),
            !pre_routed_protected_writes.is_empty(),
            rerouted_fields,
            offline_route,
        );
        let (mut summary, mut inline_commit_result, mut inline_release_result) = {
            let transaction_id = transaction.as_ref().map(|value| value.id.as_str());
            let phase_started = Instant::now();
            let result = crate::studio::native::editor::send_editor_change_batches_with_options(
                bridge,
                &changes,
                binary_import.as_ref(),
                transaction_id,
                can_inline_commit,
                can_inline_commit
                    .then_some(args.inline_release_tracking.as_ref())
                    .flatten(),
            );
            log_timing("native editor change batches", phase_started);
            let batch = result?;
            (batch.summary, batch.commit_result, batch.release_result)
        };
        if handle_bulk_fallback_capture(&summary, transaction.as_mut()) {
            return Ok(summary);
        }
        ensure_editor_summary_succeeded(&summary)?;
        if args.verify_sources || summary.contains_key("nativeVerifiedServices") {
            verify_pushed_sources(
                bridge,
                &changes,
                transaction.as_ref().map(|value| value.id.as_str()),
                &mut summary,
            )?;
        }
        record_unchanged_source_summary(
            args.verify_sources,
            unchanged_sources,
            inexact_unchanged,
            &mut summary,
        );
        let phase_started = Instant::now();
        let pending = (place.pending)(
            bridge,
            &mut summary,
            pre_routed_protected_writes,
            rerouted_fields,
            &mut place_values.borrow_mut(),
        );
        // Approval and transaction preparation are the only consumers. Release
        // the DOM before snapshot publication parses the place again.
        drop(place_values);
        let offline_route = offline_route
            && (!pending.writes.is_empty() || !changes.deferred_native_rows.is_empty());
        let mut protected = prepare_protected_writes(
            &args,
            bridge,
            pending,
            offline_route,
            &approved,
            offline_declined,
        )?;
        if protected.apply_offline && !changes.deferred_native_rows.is_empty() {
            // The offline apply reopens Studio after the commit, so the
            // follow-up transaction has no runtime to run in; the saved place
            // takes the imported instances' engine-set fields instead.
            let imported = protected_write_rows(&changes.deferred_native_rows);
            protected.supplemental += imported.len();
            protected.writes.extend(imported);
        }
        log_timing("native editor protected write preparation", phase_started);
        let phase_started = Instant::now();
        if inline_commit_result.is_none()
            && let Some(validate_project) = validate_project
        {
            validate_project()?;
        }
        {
            super::native_geometry::capture_generated(
                bridge,
                &mut changes,
                &args.project.src_root,
                transaction
                    .as_ref()
                    .map(|transaction| transaction.id.as_str()),
            )?;
            if let Some(finalize_settings) = finalize_settings {
                finalize_settings(&mut changes)?;
            }
        }
        let mut settings_transaction = Some(EditorSettingsTransaction::apply(&changes)?);
        if let Some(history_transaction) = history_transaction.as_mut() {
            history_transaction.publish()?;
        }
        log_timing("native editor settings apply", phase_started);
        let phase_started = Instant::now();
        let mut commit_status = None;
        let mut offline_receipt = None;
        if protected.apply_offline {
            let transaction = transaction
                .as_mut()
                .context("Protected snapshot requires an active transaction")?;
            transaction.prepare_snapshot()?;
            let result = (place.apply)(bridge, &args, &protected.writes, &mut || {
                commit_status = Some(transaction.commit()?);
                if let Some(settings) = settings_transaction.take() {
                    settings.commit();
                }
                if let Some(history) = history_transaction.take() {
                    history.commit();
                }
                Ok(())
            });
            let result = match result {
                Ok(receipt) => receipt,
                Err(error) if commit_status.is_some() => json!({
                    "committed": true, "recoveryPending": true,
                    "error": format!("{error:#}"),
                    "nextStep": "Studio committed the push. Recover the prepared local snapshot and reconnect Studio before retrying the pending fields.",
                }),
                Err(error) => return Err(error),
            };
            let recovered = result.get("recoveryPending").and_then(Value::as_bool) != Some(true);
            offline_receipt = Some(result);
            if recovered {
                summary.insert(
                    "protectedApplied".to_string(),
                    json!(protected.writes.len() as u64),
                );
                summary.remove("protectedPending");
            } else {
                summary.insert("protectedPending".into(), json!(protected.writes.len()));
                summary.insert("verified".into(), json!(false));
            }
        } else if let Some(transaction) = transaction.as_mut() {
            let commit_outcome = if let Some(inline_result) = inline_commit_result.take() {
                transaction.commit_response(inline_result)
            } else {
                transaction.commit()
            };
            match commit_outcome {
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
            if let Some(release_result) = inline_release_result.take() {
                summary.insert("_inlineReleaseResult".to_string(), release_result);
            }
        }
        if protected.apply_offline {
            summary.remove("verifiedPushProof");
        }
        if protected.apply_offline || !protected.writes.is_empty() {
            // Protected values still require full readback. An in-process
            // commit proof fences that readback; it does not assert equality.
            summary.remove("nativeVerifiedServices");
            summary.remove("fieldVerifiedServices");
        }
        log_timing("native editor transaction commit", phase_started);
        if let Some(settings) = settings_transaction.take() {
            settings.commit();
        }
        if let Some(history_transaction) = history_transaction {
            history_transaction.commit();
        }
        // The files above record what Studio committed; a failure here leaves
        // both in step and reports the fields that still need a push.
        let mut supplemental = 0;
        if protected.apply_offline {
            if protected.supplemental != 0 {
                summary.insert(
                    "supplementalImportOfflineWrites".into(),
                    json!(protected.supplemental),
                );
            }
        } else if !changes.deferred_native_rows.is_empty() {
            let phase_started = Instant::now();
            supplemental = apply_deferred_native_rows(
                bridge,
                guard,
                &changes.deferred_native_rows,
                &mut summary,
            );
            log_timing("native editor imported engine fields", phase_started);
        }
        if binary_import.is_some() {
            summary.insert("supplementalImportWrites".into(), json!(supplemental));
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
        if let Some(receipt) = offline_receipt {
            summary.insert("protectedOfflineApply".into(), receipt);
        } else {
            summary.remove("protectedOfflineApply");
        }
        if args.saved_fields.is_none() {
            warn_unsupported_properties(&summary);
        }
        Ok(summary)
    })();
    EditorTransaction::finish(transaction.as_mut(), result)
}

fn record_unchanged_source_summary(
    verify_sources: bool,
    unchanged_sources: usize,
    inexact_unchanged: usize,
    summary: &mut Map<String, Value>,
) {
    if unchanged_sources == 0 {
        return;
    }
    summary.insert("sourceUnchanged".to_string(), json!(unchanged_sources));
    if inexact_unchanged > 0 {
        let inexact = summary
            .get("sourceInexact")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        summary.insert(
            "sourceInexact".to_string(),
            json!(inexact + inexact_unchanged as u64),
        );
    }
    if verify_sources {
        let verified = summary
            .get("sourceVerified")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        summary.insert(
            "sourceVerified".to_string(),
            json!(verified + unchanged_sources as u64),
        );
    }
}

fn handle_bulk_fallback_capture(
    summary: &Map<String, Value>,
    transaction: Option<&mut EditorTransaction<'_>>,
) -> bool {
    if summary.get("fallbackFullCapture") != Some(&Value::Bool(true)) {
        return false;
    }
    if let Some(tx) = transaction
        && tx.active
    {
        let _ = tx.rollback();
    }
    true
}

fn ensure_editor_summary_succeeded(summary: &Map<String, Value>) -> Result<()> {
    let errors = summary.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
    if summary.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
        bail!("Studio rejected or failed one or more editor push changes");
    }
    Ok(())
}

fn can_inline_bulk_commit(
    has_transaction: bool,
    changes: &EditorChangeSet,
    has_binary_import: bool,
    has_pre_routed: bool,
    rerouted_fields: usize,
    offline_route: bool,
) -> bool {
    has_transaction
        && !changes.bulk_property_patches.is_empty()
        && changes.instance_changes.is_empty()
        && changes.source_changes.is_empty()
        && changes.property_changes.is_empty()
        && changes.deferred_native_rows.is_empty()
        && changes.settings_writes.is_empty()
        && !has_binary_import
        && !has_pre_routed
        && rerouted_fields == 0
        && !offline_route
}

/// The native setter finds its target by path from the service root, and an
/// import's incoming roots join the service only at commit. Engine-set fields
/// of imported instances (a texture pack, a mesh id the reader dropped) are
/// held back here and written by `apply_deferred_native_rows` afterwards.
fn defer_imported_native_root_rows(
    changes: &mut EditorChangeSet,
    import: Option<&EditorBinaryImport>,
) {
    let Some(import) = import else {
        return;
    };
    let _span = crate::app::profile::span("native editor imported root rows deferred");
    let mut deferred = Vec::new();
    changes.property_changes.retain_mut(|change| {
        if property_change_needs_post_native_apply(change)
            || !import.imports_path(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            )
            || import.retains_path(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            )
            || import.carries_container_settings(&change.service, &change.path_segments)
        {
            return true;
        }
        let names = change
            .properties
            .keys()
            .filter(|name| super::native_roots::is_property(&change.class_name, name))
            .cloned()
            .collect::<Vec<_>>();
        if names.is_empty() {
            return true;
        }
        // A follow-up write inside an imported package would trip the package
        // ownership check after the import has landed; the payload's values
        // stand for those instances, as they did before the follow-up existed.
        if import.imports_package_path(&change.service, &change.path_segments) {
            change.properties.retain(|name, _| !names.contains(name));
            return !change.properties.is_empty()
                || !change.reset_properties.is_empty()
                || !change.attributes.is_empty()
                || !change.deleted_attributes.is_empty();
        }
        let mut held = change.clone();
        held.properties.retain(|name, _| names.contains(name));
        held.reset_properties.clear();
        held.attributes.clear();
        held.deleted_attributes.clear();
        held.attributes_complete = false;
        deferred.push(held);
        change.properties.retain(|name, _| !names.contains(name));
        !change.properties.is_empty()
            || !change.reset_properties.is_empty()
            || !change.attributes.is_empty()
            || !change.deleted_attributes.is_empty()
    });
    changes.deferred_native_rows = deferred;
}

// Each of these fields needs a separate Studio target lookup, transaction
// fence and write. Check the plan before opening the import transaction.
const MAX_INDIVIDUAL_IMPORT_WRITES: usize = 32;

fn individual_writes_fit_before_build(changes: &EditorChangeSet) -> bool {
    let mut budget = MAX_INDIVIDUAL_IMPORT_WRITES;
    for row in &changes.property_changes {
        if property_change_needs_post_native_apply(row) || row.path_segments.len() < 2 {
            continue;
        }
        let count = row
            .properties
            .keys()
            .filter(|name| super::native_roots::is_property(&row.class_name, name))
            .count();
        let Some(left) = budget.checked_sub(count) else {
            return false;
        };
        budget = left;
    }
    // A stored spelling may be an alias. Count every stored property until
    // materialization has resolved it to its logical name.
    for store in changes.native_property_documents.values() {
        // A store that left instances in its file is far past any budget.
        if crate::settings::equivalence::holds_unread_instances(store) {
            return false;
        }
        for instance in &store.instances {
            if instance.parent_index.is_some() {
                let Some(left) = budget.checked_sub(instance.properties.len()) else {
                    return false;
                };
                budget = left;
            }
        }
    }
    true
}

fn individual_import_write_count(rows: &[EditorPropertyChange]) -> usize {
    rows.iter().map(|row| row.properties.len()).sum()
}

#[derive(Debug)]
pub(crate) struct SlowImportRequired {
    pub(crate) message: String,
    pub(crate) live_scope: Option<String>,
}
impl SlowImportRequired {
    pub(crate) fn live_message(&self) -> String {
        let next = self.live_scope.as_ref().map_or_else(
            || "Inspect the pending instances with rbx lst and select the intended settings IDs for a scoped push.".into(),
            |scope| format!("Inspect pending edits with rbx lst. To push the first affected subtree individually, use {scope}."),
        );
        format!("[slow_import_required] {} {next}", self.message)
    }
}

impl std::fmt::Display for SlowImportRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} Retry the pending project changes with rbx ps --slow-import; pass store paths to limit that push.",
            self.message
        )
    }
}
impl std::error::Error for SlowImportRequired {}

fn check_import_route(
    rows: &[EditorPropertyChange],
    fallback: Option<&ServiceReaderFallback>,
    allow_slow: bool,
    offline_allowed: bool,
    pending_offline: impl FnOnce() -> bool,
) -> Result<bool> {
    let Some(ServiceReaderFallback::DiscoveryFailed(reason)) = fallback else {
        return Ok(false);
    };
    let count = individual_import_write_count(rows);
    if allow_slow || count <= MAX_INDIVIDUAL_IMPORT_WRITES {
        return Ok(false);
    }
    let pending = offline_allowed && pending_offline();
    if pending {
        return Ok(true);
    }
    let mut counts = BTreeMap::new();
    for row in rows {
        for name in row.properties.keys() {
            *counts
                .entry((row.class_name.as_str(), name.as_str()))
                .or_insert(0_usize) += 1;
        }
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by_key(|&(key, n)| (std::cmp::Reverse(n), key));
    let fields = counts
        .iter()
        .take(5)
        .map(|((class, name), n)| format!("{class}.{name} ({n})"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut route = format!(
        "Individual writes would take about {:.1} s at 31 ms per field.",
        count as f64 * 0.031
    );
    if offline_allowed {
        route.push_str(" If Studio reports additional protected writes, an approved local snapshot can replace the individual-write route.");
    }
    let target = rows
        .iter()
        .find_map(|row| {
            let service = (!row.service.is_empty()).then_some(&row.service)?;
            Some(match &row.settings_id {
                Some(id) => format!(
                    " First affected store {service}, settings ID {}.",
                    json!(id)
                ),
                None => format!(
                    " First affected store {service}, path {}.",
                    json!(row.path_segments)
                ),
            })
        })
        .unwrap_or_default();
    let message = format!(
        "Import stopped before changing Studio: {reason}. The fallback plans {count} individual supplemental field writes (limit {MAX_INDIVIDUAL_IMPORT_WRITES}), including {fields}.{target} {route}"
    );
    let safe_argument = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
    };
    let live_scope = rows.iter().find_map(|row| {
        let id = row.settings_id.as_deref()?;
        if row.service.is_empty() || id.is_empty() {
            return None;
        }
        Some(if safe_argument(&row.service) && safe_argument(id) {
            format!("rbx ps -s {} -i {id} --slow-import", row.service)
        } else {
            format!(
                "rbx with arguments {}",
                json!([
                    "ps",
                    "-s",
                    row.service,
                    format!("--target-settings-id={id}"),
                    "--slow-import"
                ])
            )
        })
    });
    Err(SlowImportRequired {
        message,
        live_scope,
    }
    .into())
}

fn protected_write_rows(rows: &[EditorPropertyChange]) -> Vec<Value> {
    rows.iter()
        .flat_map(|change| {
            change.properties.iter().map(move |(name, value)| {
                json!({
                    "kind": "property",
                    "service": &change.service,
                    "settingsId": &change.settings_id,
                    "pathSegments": &change.path_segments,
                    "pathOrdinals": &change.path_ordinals,
                    "className": &change.class_name,
                    "name": name,
                    "value": value,
                })
            })
        })
        .collect()
}

fn apply_deferred_native_rows(
    bridge: &BridgeServer,
    guard: Option<&StudioChangeGuard>,
    rows: &[EditorPropertyChange],
    summary: &mut Map<String, Value>,
) -> usize {
    let fields = rows.iter().map(|row| row.properties.len()).sum::<usize>();
    let changes = EditorChangeSet {
        property_changes: rows.to_vec(),
        ..EditorChangeSet::default()
    };
    let mut transaction = None;
    let result = (|| {
        transaction = EditorTransaction::begin(bridge, &changes, None, guard, None)?;
        let transaction_id = transaction.as_ref().map(|value| value.id.as_str());
        let applied = send_editor_change_batches(bridge, &changes, None, transaction_id)?;
        let follow_up = applied.summary;
        let errors = follow_up
            .get("errors")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if follow_up.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
            bail!("Studio rejected or failed an engine-set field of an imported instance");
        }
        if let Some(transaction) = transaction.as_mut() {
            // The second commit retires the first one's push proof.
            let status = transaction.commit()?;
            summary.remove("verifiedPushProof");
            add_editor_commit_status(summary, status);
        }
        // The import transaction listed these services as field-verified
        // without the rows held back from it; only the follow-up's own
        // readback can vouch for them now.
        let verified = follow_up
            .get("fieldVerifiedServices")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for key in ["fieldVerifiedServices", "nativeVerifiedServices"] {
            if let Some(Value::Array(services)) = summary.get_mut(key) {
                services.retain(|service| {
                    !rows
                        .iter()
                        .any(|row| Value::String(row.service.clone()) == *service)
                        || verified.contains(service)
                });
            }
        }
        let updated = follow_up
            .get("propertyUpdated")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let previous = summary
            .get("propertyUpdated")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        summary.insert("propertyUpdated".into(), json!(previous + updated));
        Ok(applied.individual_dispatches)
    })();
    match EditorTransaction::finish(transaction.as_mut(), result) {
        Ok(count) => count,
        Err(error) => {
            summary.remove("verifiedPushProof");
            for key in ["fieldVerifiedServices", "nativeVerifiedServices"] {
                if let Some(Value::Array(services)) = summary.get_mut(key) {
                    services.retain(|service| {
                        !rows
                            .iter()
                            .any(|row| service.as_str() == Some(&row.service))
                    });
                }
            }
            record_unsupported_properties(summary, rows.iter().flat_map(|row| row.properties.keys().map(|name| {
                format!("{}.{} ({error:#}; the import committed, retry the push to write this field)", row.path_segments.join("."), name)
            })));
            summary.insert("supplementalImportPendingWrites".into(), json!(fields));
            summary.insert(
                "supplementalImportPendingFields".into(),
                json!(
                    rows.iter()
                        .map(crate::editor::types::PendingImportFields::from)
                        .collect::<Vec<_>>()
                ),
            );
            summary.insert("verified".into(), json!(false));
            0
        }
    }
}

fn apply_editor_change_with_warm_bridge(
    bridge: &BridgeServer,
    label: &str,
    review: Option<(&Path, bool)>,
    collect: impl FnOnce() -> Result<EditorChangeSet>,
) -> Result<Map<String, Value>> {
    let started = Instant::now();
    let mut changes = collect()?;
    super::native_roots::normalize_resets(&mut changes)?;
    if let Some((project_root, report)) = review
        && let outcome = request_editor_push_review(bridge, &changes, Some(project_root), report)?
        && !outcome.proceeds()
    {
        let summary = match outcome {
            EditorReviewOutcome::Held(summary) => summary,
            _ => skipped_editor_summary(&changes),
        };
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
    let mut transaction = EditorTransaction::begin(bridge, &changes, None, None, None)?;
    let result = (|| {
        let transaction_id = transaction.as_ref().map(|value| value.id.as_str());
        let mut summary =
            send_editor_change_batches(bridge, &changes, None, transaction_id)?.summary;
        let errors = summary.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
        if summary.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
            bail!("Studio rejected or failed editor {label} apply");
        }
        if summary
            .get("protectedWrites")
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty())
        {
            bail!(
                "Studio cannot write these saved fields through its plugin; changes were rolled back. Edit the store and use rbx ps STORE -u --yes for the protected snapshot fallback"
            );
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
    EditorTransaction::finish(transaction.as_mut(), result)
}

/// Whether a `prop` or `del` summary records a change Studio applied.
pub(crate) fn direct_editor_summary_changed(summary: &Map<String, Value>) -> bool {
    if summary.get("skippedByReview").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    [
        "attributeUpdated",
        "instanceCreated",
        "instanceDeleted",
        "instanceReplaced",
        "propertyUpdated",
        "sourceCreated",
        "sourceDeleted",
        "sourceUpdated",
        "protectedApplied",
        "savedFieldsApplied",
    ]
    .into_iter()
    .any(|key| summary.get(key).and_then(Value::as_f64).unwrap_or(0.0) > 0.0)
}

/// The fields a direct edit answers with beside `ok` and `changed`: the same
/// outcome fields a push prints, plus where the edit now lives.
const DIRECT_EDITOR_SUMMARY_FIELDS: &[&str] = &[
    "skippedByReview",
    "heldForReview",
    "effectiveChanges",
    "reviewThreshold",
    "review",
    "moreReviewRows",
    "next",
    "persistence",
    "filesPulled",
    "filesError",
    "savedFieldsApplied",
    "protectedApplied",
    "unsupportedProperties",
    "packageModified",
    "autoDesyncedPackages",
];

pub(crate) fn direct_editor_summary(summary: &Map<String, Value>) -> Map<String, Value> {
    let mut output = Map::from_iter([
        ("ok".to_string(), Value::Bool(true)),
        (
            "changed".to_string(),
            Value::Bool(direct_editor_summary_changed(summary)),
        ),
    ]);
    for key in DIRECT_EDITOR_SUMMARY_FIELDS {
        if let Some(value) = summary.get(*key).filter(|value| !value.is_null()) {
            output.insert((*key).to_string(), value.clone());
        }
    }
    output
}

fn print_direct_editor_summary(summary: &serde_json::Map<String, Value>) -> Result<()> {
    print_json_output(&Value::Object(direct_editor_summary(summary)), false)
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
    parameters.insert("yes".to_string(), Value::Bool(args.yes || global_yes()));
    parameters.insert("reviewReport".to_string(), Value::Bool(true));
    // Every edit the user has not opted out of with --no-review goes through
    // the daemon's review so protected writes stay allowed; `yes` alone
    // decides whether an edit over the threshold applies or comes back as a
    // change list.
    let result = daemon_control_request(
        op::SET_PROPERTY,
        Some(&args.target.project.project_root),
        Value::Object(parameters),
        !args.no_review,
    )?;
    print_direct_editor_summary(
        result
            .as_object()
            .context("The daemon returned an invalid property result")?,
    )
}

pub(crate) fn apply_editor_property_with_warm_bridge(
    mut args: ApplyEditorPropertyArgs,
    bridge: &BridgeServer,
) -> Result<Map<String, Value>> {
    resolve_editor_property_source_file(&mut args)?;
    let started = Instant::now();
    let changes = collect_direct_editor_property_change(&args)?;
    if let Some(summary) = apply_direct_saved_field(&args, bridge, &changes)? {
        return Ok(summary);
    }
    let verify_sources = !changes.source_changes.is_empty();
    push_editor_changes_with_collected(
        PushEditorChangesArgs {
            no_review: args.no_review,
            yes: args.yes || global_yes(),
            review_report: args.review_report,
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
            finalize_settings: None,
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

/// A saved field with no plugin API (`Lighting.Technology`,
/// `Workspace.StreamingMinRadius`) is written through the native property
/// writer, as a push writes it; the plugin would only report it as a
/// protected write it cannot apply.
fn apply_direct_saved_field(
    args: &ApplyEditorPropertyArgs,
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
) -> Result<Option<Map<String, Value>>> {
    let [change] = changes.property_changes.as_slice() else {
        return Ok(None);
    };
    let Some((name, value)) = change.properties.iter().next() else {
        return Ok(None);
    };
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    if change.class_name.is_empty()
        || !super::review::plugin_cannot_write_property(database, &change.class_name, name)
    {
        return Ok(None);
    }
    let label = format!("{}.{name}", change.path_segments.join("."));
    if args.no_review {
        bail!(
            "{label} has no plugin API and is written natively; drop --no-review to write it, or set it in Studio's Properties window"
        );
    }
    let runtime = bridge
        .cached_bridge_info_for_target(BridgeTarget::Edit)?
        .runtime_id;
    if !args.yes && !global_yes() {
        match request_editor_push_review(
            bridge,
            changes,
            Some(&args.target.project.project_root),
            args.review_report,
        )? {
            EditorReviewOutcome::Proceed => {}
            EditorReviewOutcome::Held(summary) => return Ok(Some(summary)),
            EditorReviewOutcome::Skipped => return Ok(Some(skipped_editor_summary(changes))),
        }
    }
    let written = crate::studio::automation::property_access::write_saved_field(
        bridge,
        &runtime,
        &change.path_segments,
        &change.path_ordinals,
        name,
        value,
    )
    .with_context(|| {
        format!(
            "{label} has no plugin API and the native property writer could not set it; set it in Studio's Properties window, or edit instances/{}.renium and push it with rbx ps instances/{}.renium -u --yes",
            change.service, change.service
        )
    })?;
    let mut summary = Map::new();
    summary.insert("ok".to_string(), Value::Bool(true));
    summary.insert("propertyUpdated".to_string(), json!(u64::from(written)));
    if written {
        summary.insert("savedFieldsApplied".to_string(), json!(1));
    }
    Ok(Some(summary))
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
        attributes_complete: false,
    });
    Ok(changes)
}

pub(crate) fn apply_editor_delete(args: ApplyEditorDeleteArgs) -> Result<()> {
    let mut parameters = editor_mutation_parameters(&args.target)?;
    parameters.insert("editor".to_string(), Value::Bool(true));
    parameters.insert("yes".to_string(), Value::Bool(args.yes || global_yes()));
    parameters.insert("reviewReport".to_string(), Value::Bool(true));
    let result = daemon_control_request(
        op::REMOVE,
        Some(&args.target.project.project_root),
        Value::Object(parameters),
        !args.no_review,
    )?;
    print_direct_editor_summary(
        result
            .as_object()
            .context("The daemon returned an invalid delete result")?,
    )
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
    let project_root = args.target.project.project_root.clone();
    let review = (!args.no_review && !args.yes && !global_yes())
        .then_some((project_root.as_path(), args.review_report));
    apply_editor_change_with_warm_bridge(bridge, "delete", review, || {
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
    /// Verified sources Studio holds in other bytes than the file's.
    inexact: usize,
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
    #[serde(default, rename = "className")]
    class_name: Option<String>,
}

struct LiveSource {
    source: String,
    class_name: Option<String>,
}

// Batches are bounded by the Source bytes the files expect. Studio's replies
// are limited to 16 MiB of JSON, so a batch that still outgrows that is split.
const LIVE_SOURCE_BATCH_ITEMS: usize = 512;
const LIVE_SOURCE_BATCH_BYTES: usize = 2 * 1024 * 1024;

fn fetch_live_editor_sources(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
    indexes: &[usize],
    transaction_id: Option<&str>,
) -> Result<HashMap<usize, std::result::Result<LiveSource, String>>> {
    let mut batches = Vec::<&[usize]>::new();
    let mut batch_start = 0;
    let mut batch_bytes = 0;
    for (position, index) in indexes.iter().enumerate() {
        let bytes = changes.source_changes[*index]
            .source
            .as_ref()
            .map_or(0, String::len);
        if position > batch_start
            && (position - batch_start == LIVE_SOURCE_BATCH_ITEMS
                || batch_bytes + bytes > LIVE_SOURCE_BATCH_BYTES)
        {
            batches.push(&indexes[batch_start..position]);
            batch_start = position;
            batch_bytes = 0;
        }
        batch_bytes += bytes;
    }
    if batch_start < indexes.len() {
        batches.push(&indexes[batch_start..]);
    }
    let request_lease = bridge.active_request_lease();
    let workers = bridge.channel_count().clamp(1, batches.len().max(1));
    let fetched = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .context("Failed to start live source readers")?
        .install(|| {
            batches
                .par_iter()
                .map(|batch| {
                    let _lease = request_lease
                        .as_ref()
                        .map(|lease| bridge.inherit_request_lease(Arc::clone(lease)))
                        .transpose()?;
                    fetch_live_source_batch(bridge, changes, batch, transaction_id)
                })
                .collect::<Result<Vec<_>>>()
        })?;
    let mut sources = HashMap::with_capacity(indexes.len());
    for batch in fetched {
        sources.extend(batch);
    }
    Ok(sources)
}

fn fetch_live_source_batch(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
    batch: &[usize],
    transaction_id: Option<&str>,
) -> Result<HashMap<usize, std::result::Result<LiveSource, String>>> {
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
    let response = match bridge.call(
        "getLiveSourceBatch",
        json!({ "selectors": selectors, "transactionId": transaction_id }),
    ) {
        Err(error)
            if batch.len() > 1 && format!("{error:#}").contains("exceeds safe size limit") =>
        {
            let (left, right) = batch.split_at(batch.len() / 2);
            let mut sources = fetch_live_source_batch(bridge, changes, left, transaction_id)?;
            sources.extend(fetch_live_source_batch(
                bridge,
                changes,
                right,
                transaction_id,
            )?);
            return Ok(sources);
        }
        response => response.and_then(|value| {
            serde_json::from_value::<LiveSourceBatch>(value)
                .context("Studio returned an invalid live source batch")
        })?,
    };
    let batch_indexes = batch.iter().copied().collect::<HashSet<_>>();
    let mut sources = HashMap::with_capacity(batch.len());
    for row in response.rows {
        if !batch_indexes.contains(&row.index) || sources.contains_key(&row.index) {
            continue;
        }
        let value = match (row.source, row.error) {
            (Some(source), _) => Ok(LiveSource {
                source,
                class_name: row.class_name,
            }),
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
    Ok(sources)
}

// A script whose Studio Source and class already equal the file is not a
// change. Dropping it before the transaction keeps rollback capture, package
// preflight and history recording scoped to real edits. A final line break
// the file has and Studio lacks is an edit: a sync compares the two texts as
// they are, so leaving it out would make the next one see Studio differ.
// Returns how many it dropped, and how many of those Studio holds with other
// line breaks than the file.
fn drop_unchanged_source_changes(
    bridge: &BridgeServer,
    changes: &mut EditorChangeSet,
) -> Result<(usize, usize)> {
    let indexes = changes
        .source_changes
        .iter()
        .enumerate()
        .filter_map(|(index, change)| (!change.deleted && change.source.is_some()).then_some(index))
        .collect::<Vec<_>>();
    if indexes.is_empty() {
        return Ok((0, 0));
    }
    let sources = fetch_live_editor_sources(bridge, changes, &indexes, None)?;
    let unchanged = indexes
        .into_iter()
        .filter(|index| {
            let change = &changes.source_changes[*index];
            matches!(
                sources.get(index),
                Some(Ok(live))
                    if live.class_name.as_deref() == Some(change.class_name.as_str())
                        && studio_holds_source(
                            change.source.as_deref().unwrap_or_default(),
                            &live.source
                        )
            )
        })
        .collect::<HashSet<_>>();
    if unchanged.is_empty() {
        return Ok((0, 0));
    }
    let inexact = unchanged
        .iter()
        .filter(|index| {
            matches!(
                sources.get(*index),
                Some(Ok(live))
                    if Some(live.source.as_str()) != changes.source_changes[**index].source.as_deref()
            )
        })
        .count();
    if global_log_enabled(5) {
        for index in &unchanged {
            let change = &changes.source_changes[*index];
            log_global(
                5,
                format_args!(
                    "[renium] source unchanged in Studio, not sent: {}.{}",
                    change.service,
                    change.path_segments.join(".")
                ),
            );
        }
    }
    // History keeps a script's previous Source only when the push changes it,
    // so these entries would fetch the same live sources again for nothing.
    let unchanged_keys = unchanged
        .iter()
        .map(|index| {
            let change = &changes.source_changes[*index];
            (change.service.clone(), editor_source_key(change))
        })
        .collect::<HashSet<_>>();
    changes.history_entries.retain(|entry| {
        entry.settings_before.is_some()
            || !entry
                .source_key
                .as_ref()
                .is_some_and(|key| unchanged_keys.contains(&(entry.service.clone(), key.clone())))
    });
    let mut index = 0;
    changes.source_changes.retain(|_| {
        let keep = !unchanged.contains(&index);
        index += 1;
        keep
    });
    Ok((unchanged.len(), inexact))
}

fn verify_editor_source_changes(
    bridge: &BridgeServer,
    changes: &EditorChangeSet,
    transaction_id: Option<&str>,
) -> Result<EditorSourceVerification> {
    let mut pending = changes
        .source_changes
        .iter()
        .enumerate()
        .filter_map(|(index, change)| (!change.deleted && change.source.is_some()).then_some(index))
        .collect::<Vec<_>>();
    let verified = pending.len();
    let mut inexact = 0;
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
        let sources = match fetch_live_editor_sources(bridge, changes, &pending, transaction_id) {
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
            let failure = match sources.get(index) {
                Some(Ok(live)) if editor_sources_match(expected, &live.source) => {
                    inexact += usize::from(live.source != expected);
                    None
                }
                Some(Ok(live)) => Some(format!(
                    "{} source mismatch ({} vs {} bytes in Studio)",
                    change.path_segments.join("."),
                    expected.len(),
                    live.source.len(),
                )),
                Some(Err(error)) => Some(format!(
                    "{} source could not be read: {error}",
                    change.path_segments.join("."),
                )),
                None => Some(format!(
                    "{} source was omitted by Studio",
                    change.path_segments.join("."),
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
        verified: verified - failed_indexes.len(),
        inexact,
        failed_indexes,
        failed,
    })
}

/// The text is the same but for how its line breaks are written, which is
/// also what a sync takes for the same script.
fn studio_holds_source(expected: &str, actual: &str) -> bool {
    expected == actual
        || normalized_source_bytes(expected.as_bytes())
            .eq(normalized_source_bytes(actual.as_bytes()))
}

fn editor_sources_match(expected: &str, actual: &str) -> bool {
    studio_holds_source(expected, actual)
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

fn is_init_source_file_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("init.") && (lower.ends_with(".lua") || lower.ends_with(".luau"))
}

/// A directory's `init.*` file names the instance its sibling files live
/// under, so it is collected before them. Collected after a sibling, the
/// sibling has already created the directory's instance as a Folder and the
/// init file then replaces an instance Studio does not hold yet.
fn order_init_files_before_their_siblings(paths: &mut [PathBuf]) {
    paths.sort_by_cached_key(|path| {
        let mut key = path
            .components()
            .map(|component| (1u8, component.as_os_str().to_string_lossy().into_owned()))
            .collect::<Vec<_>>();
        if let Some(last) = key.last_mut()
            && is_init_source_file_name(&last.1)
        {
            last.0 = 0;
        }
        key
    });
}

fn collect_editor_full_paths(src_root: &Path) -> Result<Vec<PathBuf>> {
    if !src_root.is_dir()
        && !crate::project::storage::instances_root(src_root).is_some_and(|path| path.is_dir())
    {
        bail!(
            "Cannot collect editor changes from missing source directory {}",
            src_root.display()
        );
    }
    let roots = std::iter::once(src_root.to_path_buf())
        .chain(crate::project::storage::instances_root(src_root));
    let mut paths = roots
        .filter(|root| root.is_dir())
        .flat_map(|root| WalkDir::new(root).into_iter())
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
            transaction._locks = acquire_settings_file_locks(
                changes
                    .settings_writes
                    .iter()
                    .map(|write| write.path.as_path()),
            )?;
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
                match &write.encoded {
                    Some(bytes) => fs::write(&temporary, bytes)
                        .with_context(|| format!("Failed to write {}", temporary.display()))?,
                    None => write.document.write_file(&temporary)?,
                }
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

/// A path under a read-only package is checked by comparing the whole target
/// with its package, so files a link apply wrote pass and other edits fail.
fn read_only_changed_path(
    link_enforcement: &LinkEnforcement,
    service: &str,
    protected_path: Option<(&[String], &[usize])>,
    skip: bool,
    changed_services: &mut EditorChangedServices,
) -> bool {
    if protected_path
        .and_then(|(path, ordinals)| {
            link_enforcement.read_only_package_for_path(service, path, ordinals)
        })
        .is_none()
    {
        return false;
    }
    changed_services.read_only.insert(service.to_string());
    skip
}

#[derive(Default)]
struct EditorChangedServices {
    settings: HashSet<String>,
    reconcile: HashSet<String>,
    target_upsert: HashSet<String>,
    dirty: HashSet<String>,
    read_only: HashSet<String>,
    /// The subtrees a targeted push touches, per service; a full push
    /// records none and every read-only package of its services is checked.
    scopes: PushScopes,
}

/// A pushed subtree as a path relative to its service, with sibling ordinals.
type PushScope = (Vec<String>, Vec<usize>);
type PushScopes = HashMap<String, Vec<PushScope>>;

impl EditorChangedServices {
    /// Records a pushed subtree; a full push records nothing.
    fn record_scope(
        &mut self,
        full_reconcile: bool,
        service: &str,
        path: Option<(&[String], &[usize])>,
    ) {
        let Some((segments, ordinals)) = path.filter(|_| !full_reconcile) else {
            return;
        };
        self.scopes
            .entry(service.to_string())
            .or_default()
            .push(push_scope(service, segments, ordinals));
    }
}

/// A pushed subtree relative to its service; paths arrive with or without
/// the service as their first segment.
fn push_scope(service: &str, segments: &[String], ordinals: &[usize]) -> PushScope {
    let prefixed = segments.first().map(String::as_str) == Some(service);
    let relative = if prefixed { &segments[1..] } else { segments };
    let offset = usize::from(prefixed && ordinals.len() == segments.len());
    let relative_ordinals = &ordinals[offset.min(ordinals.len())..];
    (relative.to_vec(), relative_ordinals.to_vec())
}

fn source_path_within_filter(scopes: &[PushScope], path: &PushScope) -> bool {
    scopes.iter().any(|(segments, ordinals)| {
        crate::project::package_links::selector_starts_with(&path.0, &path.1, segments, ordinals)
    })
}

fn append_target_scopes(
    scopes: &mut Vec<PushScope>,
    service: &str,
    document: &SettingsBytecode,
    filter: &EditorPropertyFilter,
) {
    let settings_ids = filter.ids_for_service(service);
    let (segments, ordinals) =
        crate::editor::paths::build_editor_instance_path_parts(document, service);
    for (index, instance) in document.instances.iter().enumerate() {
        if settings_ids.contains(&instance.settings_id)
            && let (Some(Some(segments)), Some(Some(ordinals))) =
                (segments.get(index), ordinals.get(index))
        {
            scopes.push(push_scope(service, segments, ordinals));
        }
    }
}

/// The subtrees a push filtered by settings ids touches. Every loaded
/// service gets an entry, so a service holding none of the targets carries
/// an empty scope and its read-only packages are left alone.
fn target_filter_scopes(
    documents: &HashMap<String, Option<Arc<SettingsBytecode>>>,
    filter: &EditorPropertyFilter,
    recorded: &PushScopes,
) -> PushScopes {
    let mut scopes: PushScopes = HashMap::new();
    for (service, document) in documents {
        let entry = scopes.entry(service.clone()).or_default();
        if let Some(paths) = recorded.get(service) {
            entry.extend(paths.iter().cloned());
        }
        let Some(document) = document else {
            continue;
        };
        append_target_scopes(entry, service, document, filter);
    }
    scopes
}

/// A read-only package needs its fingerprint check only when the push can
/// reach it: an unscoped push, or a scope inside the package or containing it.
fn push_scope_touches(
    scopes: Option<&Vec<PushScope>>,
    target_segments: &[String],
    target_ordinals: &[usize],
) -> bool {
    let Some(scopes) = scopes else {
        return true;
    };
    scopes.iter().any(|(segments, ordinals)| {
        crate::project::package_links::selector_starts_with(
            segments,
            ordinals,
            target_segments,
            target_ordinals,
        ) || crate::project::package_links::selector_starts_with(
            target_segments,
            target_ordinals,
            segments,
            ordinals,
        )
    })
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

/// An instance whose values refer to a target: its settings id and the
/// names of the properties and attributes that do.
struct EditorReferenceHit {
    settings_id: String,
    properties: Vec<String>,
    attributes: Vec<String>,
}

/// The instances of `before` whose values refer to the instance
/// `target_settings_id` names there, in store order.
fn editor_reference_hits(
    before: &SettingsBytecode,
    service: &str,
    target_settings_id: &str,
) -> Vec<EditorReferenceHit> {
    let Some(target_index) = document_instance_index_by_settings_id(before, target_settings_id)
    else {
        return Vec::new();
    };
    let Some(target_path) =
        build_editor_instance_paths_for_indices(before, service, &[target_index])
            .remove(&target_index)
    else {
        return Vec::new();
    };
    crate::settings::tree::count_whole_store_pass();
    (before.instances.iter())
        .filter_map(|instance| {
            editor_reference_hit(instance, target_index, target_settings_id, &target_path)
        })
        .collect()
}

/// `instance` where its values refer to the target at `target_index`.
fn editor_reference_hit(
    instance: &crate::settings::bytecode::SettingsBytecodeInstance,
    target_index: usize,
    target_settings_id: &str,
    target_path: &EditorInstancePath,
) -> Option<EditorReferenceHit> {
    let referring = |values: &Map<String, Value>| {
        (values.iter())
            .filter(|(_, value)| {
                settings_value_references_target(
                    value,
                    target_index,
                    target_settings_id,
                    &target_path.path_segments,
                    &target_path.path_ordinals,
                )
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    };
    let properties = referring(&instance.properties);
    let attributes = referring(&instance.attributes);
    (!properties.is_empty() || !attributes.is_empty()).then(|| EditorReferenceHit {
        settings_id: instance.settings_id.clone(),
        properties,
        attributes,
    })
}

/// The property changes that bring Studio's values of `hits` to those of
/// `after`, the store once the target changed.
fn append_editor_reference_hit_repairs(
    changes: &mut EditorChangeSet,
    hits: &[EditorReferenceHit],
    after: &SettingsBytecode,
    service: &str,
) {
    if hits.is_empty() {
        return;
    }
    let after_paths = build_editor_instance_paths(after, service);
    let after_indices = after
        .instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    append_editor_reference_repairs_from(changes, hits, after, service, &after_paths, |id| {
        after_indices.get(id).copied()
    });
}

/// `append_editor_reference_hit_repairs` with the paths of the instances of
/// `after` and the last position of each of its settings ids.
fn append_editor_reference_repairs_from(
    changes: &mut EditorChangeSet,
    hits: &[EditorReferenceHit],
    after: &SettingsBytecode,
    service: &str,
    after_paths: &[Option<EditorInstancePath>],
    after_index: impl Fn(&str) -> Option<usize>,
) {
    let after_settings_ids = crate::editor::review::EditorReferenceIds::new(
        after
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str()),
    )
    .in_service(service);

    for hit in hits {
        let Some(after_index) = after_index(&hit.settings_id) else {
            continue;
        };
        let after_instance = &after.instances[after_index];
        let mut properties = Map::new();
        let mut reset_properties = Vec::new();
        for name in &hit.properties {
            if let Some(value) = after_instance.properties.get(name) {
                properties.insert(
                    name.clone(),
                    normalize_editor_bridge_value(value, None, after_paths, &after_settings_ids),
                );
            } else {
                reset_properties.push(name.clone());
            }
        }

        let mut attributes = Map::new();
        let mut deleted_attributes = Vec::new();
        for name in &hit.attributes {
            if let Some(value) = after_instance.attributes.get(name) {
                attributes.insert(
                    name.clone(),
                    normalize_editor_bridge_value(value, None, after_paths, &after_settings_ids),
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
            attributes_complete: false,
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
    scopes: &PushScopes,
    documents: &HashMap<String, Option<Arc<SettingsBytecode>>>,
    src_root: &Path,
) -> Result<()> {
    let targets = link_enforcement
        .read_only_packages
        .iter()
        .filter(|target| {
            changed_services.contains(&target.service)
                && push_scope_touches(
                    scopes.get(&target.service),
                    &target.target_segments,
                    &target.target_ordinals,
                )
        })
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return Ok(());
    }
    let mut canonical = HashMap::new();
    for target in &targets {
        if let Some(document) = documents.get(&target.service).and_then(Option::as_ref) {
            canonical
                .entry(PathBuf::from(&target.service))
                .or_insert_with(|| (**document).clone());
        }
    }
    crate::project::package_links::canonicalize_loaded_settings_documents(&mut canonical)?;
    for target in targets {
        let current = match canonical.get(Path::new(&target.service)) {
            Some(document) => crate::project::package_links::read_only_target_current(
                document,
                &target.service,
                &src_root.join(&target.service),
                &target.target_segments,
                &target.target_ordinals,
                target.package.as_deref(),
                &target.expected_fingerprint,
            )?,
            None => false,
        };
        crate::app::output::log_global(
            4,
            format_args!(
                "[renium] read-only link {} at {}.{}: files {} the package",
                target.link_id,
                target.service,
                target.target_segments.join("."),
                if current { "match" } else { "differ from" }
            ),
        );
        if !current {
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
    let validated = services
        .settings
        .union(&services.read_only)
        .cloned()
        .collect::<HashSet<_>>();
    let mut scopes = if !property_filter.has_target_ids() {
        services.scopes.clone()
    } else {
        target_filter_scopes(documents, property_filter, &services.scopes)
    };
    for service in &services.reconcile {
        scopes.insert(service.clone(), vec![(Vec::new(), Vec::new())]);
    }
    validate_read_only_service_changes(link_enforcement, &validated, &scopes, documents, src_root)?;
    log_editor_collection_timing("package validation", phase_started);
    for service in sorted_services(services.dirty) {
        if let Some(document) = documents.get(&service).and_then(Option::as_ref) {
            let path = service_settings_path(&src_root.join(&service));
            changes.settings_writes.push(EditorSettingsWrite {
                expected_hash: settings_file_hash(&path)?,
                path,
                document: Arc::clone(document),
                encoded: None,
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
    let outside_references = outside_reference_paths(src_root, Some(documents));
    for service in changed_services {
        if let Some(document) = documents.get(&service).and_then(Option::as_ref) {
            let deferred = changes.native_property_documents.contains_key(&service);
            if deferred {
                changes
                    .native_property_documents
                    .insert(service.clone(), Arc::clone(document));
            }
            append_editor_target_changes(
                &mut changes,
                document,
                &service,
                property_filter,
                EditorTargetChangeOptions {
                    upsert_instances: target_services.contains(&service),
                    properties: settings_services.binary_search(&service).is_ok(),
                    property_scope: if deferred {
                        EditorPropertyScope::RetainedContainers
                    } else {
                        EditorPropertyScope::All
                    },
                    property_schema_by_class: &property_schema_by_class,
                    database,
                    outside_references: Some(&outside_references),
                },
            );
        }
    }
    log_editor_collection_timing("target changes", phase_started);
    Ok(changes)
}

/// The stores of the project as a push reads them for the references it
/// sends into another service: the document the push already holds, or else
/// the instances of the store on disk.
fn outside_reference_paths<'a>(
    src_root: &'a Path,
    documents: Option<&'a HashMap<String, Option<Arc<SettingsBytecode>>>>,
) -> OutsideReferencePaths<'a> {
    OutsideReferencePaths::new(move |service| {
        if let Some(document) = documents
            .and_then(|documents| documents.get(service))
            .and_then(Option::as_ref)
        {
            return Some(Arc::clone(document));
        }
        if validate_filesystem_instance_name(service, "service").is_err() {
            return None;
        }
        let path = service_settings_path(&src_root.join(service));
        path.is_file()
            .then(|| SettingsBytecode::read_structure_file(&path).ok())
            .flatten()
            .map(Arc::new)
    })
}

/// The binary import of a change set. A document that left instances in its
/// store file is not handed to the builder, which then reads that service
/// from the file, a few instances at a time.
fn build_native_import(
    args: &PushEditorChangesArgs,
    changes: &EditorChangeSet,
    bridge: &BridgeServer,
    selection: Option<BinaryImportSelection>,
) -> Result<Option<EditorBinaryImport>> {
    let unread = |document: &Arc<SettingsBytecode>| {
        crate::settings::equivalence::holds_unread_instances(document)
    };
    if !changes.native_property_documents.values().any(unread) {
        return build_editor_binary_import(args, changes, bridge, selection);
    }
    let Some(selection) = selection else {
        return Ok(None);
    };
    let documents = changes
        .native_property_documents
        .iter()
        .filter(|(_, document)| !unread(document))
        .map(|(service, document)| {
            (
                service.clone(),
                crate::rbx::model::RbxPlaceDocument::Shared(Arc::clone(document)),
            )
        })
        .chain(changes.settings_writes.iter().filter_map(|write| {
            let service = crate::editor::paths::service_from_changed_path(
                &args.project.project_root.join(&args.project.src_root),
                &write.path,
            )?;
            Some((
                service,
                crate::rbx::model::RbxPlaceDocument::Borrowed(&write.document),
            ))
        }))
        .collect::<HashMap<_, _>>();
    build_editor_binary_import_with_documents(args, changes, bridge, selection, documents)
}

/// Reads the instances a document left in its store file before the
/// document is edited: an edited store is written whole.
fn read_whole_for_edit(
    document: Option<&mut Arc<SettingsBytecode>>,
    src_root: &Path,
    service: &str,
) -> Result<()> {
    if let Some(document) = document
        && crate::settings::equivalence::holds_unread_instances(document)
    {
        crate::settings::equivalence::read_unread_instances(
            Arc::make_mut(document),
            &store_bytes(src_root, service)?,
        )?;
    }
    Ok(())
}

fn store_bytes(stores_root: &Path, service: &str) -> Result<Vec<u8>> {
    let path = service_settings_path(&stores_root.join(service));
    std::fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))
}

/// A push that hands its services to no binary import reads the documents
/// an import would have carried whole: those left the instances the import
/// takes from the store file in that file. A document of any other service
/// that holds unread instances was compared by the instances that differ,
/// and the others are the ones Studio holds.
fn read_imported_documents_whole(
    args: &PushEditorChangesArgs,
    prepared: &mut PreparedEditorDocuments,
) -> Result<()> {
    let _span = crate::app::profile::span("imported documents read whole");
    let (_, src_root) = editor_project_roots(args)?;
    let imported = std::mem::take(&mut prepared.native_services);
    for (service, document) in &mut prepared.documents {
        if imported.contains(service)
            && crate::settings::equivalence::holds_unread_instances(document)
        {
            crate::settings::equivalence::read_unread_instances(
                Arc::make_mut(document),
                &store_bytes(&src_root, service)?,
            )?;
        }
    }
    Ok(())
}

/// For each document that left instances in its store file, the document
/// the fields a push writes beside the import are made of: with the
/// instances of a class or at a path the import names, and the models, whose
/// pivot is written after it. A service the import does not take whole is
/// read whole.
fn supplemental_documents(
    changes: &EditorChangeSet,
    import: Option<&mut EditorBinaryImport>,
    stores_root: &Path,
) -> Result<HashMap<String, Arc<SettingsBytecode>>> {
    use crate::settings::equivalence::{holds_unread_instances, read_unread_instances};
    let mut rows = import
        .map(|import| {
            let mut rows = std::mem::take(&mut import.kept_rows);
            rows.retain(|service, _| import.imports_service(service));
            rows
        })
        .unwrap_or_default();
    changes
        .native_property_documents
        .iter()
        .filter(|(_, document)| holds_unread_instances(document))
        .map(|(service, document)| (service, document, rows.remove(service)))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(service, document, rows)| {
            let started = Instant::now();
            let read = if let Some(rows) = rows {
                rows.materialize(document)?
            } else {
                let mut whole = document.as_ref().clone();
                read_unread_instances(&mut whole, &store_bytes(stores_root, service)?)?;
                whole
            };
            log_editor_collection_timing("native supplemental rows", started);
            Ok((service.clone(), Arc::new(read)))
        })
        .collect()
}

fn materialize_native_property_changes(
    changes: &mut EditorChangeSet,
    import: Option<&EditorBinaryImport>,
    project_root: &Path,
    src_root: &Path,
) -> Result<()> {
    if changes.native_property_documents.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let schema = load_rbx_dom_property_schema(project_root)?.unwrap_or_default();
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let filter = EditorPropertyFilter::default();
    let outside_references = outside_reference_paths(src_root, None);
    for (service, document) in std::mem::take(&mut changes.native_property_documents) {
        append_editor_target_changes(
            changes,
            &document,
            &service,
            &filter,
            EditorTargetChangeOptions {
                upsert_instances: false,
                properties: true,
                property_scope: EditorPropertyScope::Imported(import),
                property_schema_by_class: &schema,
                database,
                outside_references: Some(&outside_references),
            },
        );
    }
    log_editor_collection_timing("native supplemental properties", started);
    Ok(())
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
        changed_services.settings.insert(service.to_string());
    } else {
        changed_services.settings.insert(service.to_string());
        if !property_filter.is_active() {
            changed_services.reconcile.insert(service.to_string());
        } else if property_filter.has_target_ids() {
            changed_services.target_upsert.insert(service.to_string());
        }
    }
    true
}

/// What a push of a plain folder sends, made from documents a reconcile
/// prepared instead of the stores on disk.
#[cfg(test)]
pub(crate) fn collect_prepared_editor_changes(
    args: &PushEditorChangesArgs,
    documents: HashMap<String, Arc<SettingsBytecode>>,
    native_services: HashSet<String>,
) -> Result<EditorChangeSet> {
    let (project_root, src_root) = editor_project_roots(args)?;
    collect_editor_changes_with_link_enforcement_and_documents(
        args,
        &project_root,
        &src_root,
        &LinkEnforcement::default(),
        &mut PreparedEditorDocuments {
            documents,
            native_services,
            ..PreparedEditorDocuments::default()
        },
    )
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
        &mut PreparedEditorDocuments::default(),
    )
}

fn load_editor_service_document(
    service: &str,
    src_root: &Path,
    documents: &mut HashMap<String, Option<Arc<SettingsBytecode>>>,
    prepared_documents: &mut HashMap<String, Arc<SettingsBytecode>>,
) -> Result<()> {
    if documents.contains_key(service) {
        return Ok(());
    }
    let phase_started = Instant::now();
    let document = match prepared_documents.remove(service) {
        Some(document) => Some(document),
        None => read_editor_service_settings_cached(src_root, service)?,
    };
    documents.insert(service.to_string(), document);
    log_editor_collection_timing("settings decode", phase_started);
    Ok(())
}

fn unique_editor_canonical_path(
    src_root: &Path,
    absolute_path: PathBuf,
    seen_paths: &mut HashSet<String>,
) -> Option<(PathBuf, String)> {
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
    prepared_documents: &mut PreparedEditorDocuments,
) -> Result<EditorChangeSet> {
    let mut property_filter = EditorPropertyFilter::from_args(args)?;
    let explicit_targets = property_filter.clone();
    let mut source_scopes = PushScopes::new();
    let mut changes = EditorChangeSet {
        native_property_documents: prepared_documents
            .native_services
            .iter()
            .filter_map(|service| {
                prepared_documents
                    .documents
                    .get(service)
                    .map(|document| (service.clone(), Arc::clone(document)))
            })
            .collect(),
        ..EditorChangeSet::default()
    };
    let mut documents: HashMap<String, Option<Arc<SettingsBytecode>>> = HashMap::new();
    let mut source_children: HashMap<String, EditorDocumentIndex> = HashMap::new();
    let mut source_stems: HashMap<String, EditorChildStemCache> = HashMap::new();
    let mut changed_services = EditorChangedServices::default();
    let mut seen_paths = HashSet::new();
    let mut retired_ids: HashMap<String, HashSet<String>> = HashMap::new();
    let mut snapshots: HashMap<String, bulk::HistorySnapshot> = HashMap::new();
    let mut read_whole = HashSet::new();
    let mut store_references = HashMap::new();
    let mut store_referrers = HashMap::new();

    let mut changed_paths = expand_editor_changed_paths(args)?;
    let full_reconcile = changed_paths.is_empty();
    if full_reconcile {
        changed_paths = collect_editor_full_paths(src_root)?;
    }
    let mut enforced_changed_paths =
        apply_link_enforcement_to_changed_paths(project_root, link_enforcement, changed_paths)?;
    order_init_files_before_their_siblings(&mut enforced_changed_paths);
    // Each path needs a canonical form, its metadata and, for scripts, its
    // contents. Those file system calls dominate a large push, so they run in
    // parallel before the ordered pass below.
    let mut inspected_paths = enforced_changed_paths
        .par_iter()
        .map(|changed_path| {
            let absolute_path = canonical_editor_changed_path(project_root, changed_path);
            let metadata = fs::metadata(&absolute_path).ok();
            let source = metadata
                .as_ref()
                .is_some_and(std::fs::Metadata::is_file)
                .then(|| {
                    absolute_path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| matches!(extension, "lua" | "luau"))
                        .then(|| fs::read_to_string(&absolute_path))
                })
                .flatten();
            (absolute_path, metadata, source)
        })
        .collect::<Vec<_>>();
    let mut next = 0;
    while next < inspected_paths.len() {
        let at = next;
        next += 1;
        let (absolute_path, metadata, mut prefetched_source) =
            std::mem::take(&mut inspected_paths[at]);
        let Some((absolute_path, service)) =
            unique_editor_canonical_path(src_root, absolute_path, &mut seen_paths)
        else {
            continue;
        };

        load_editor_service_document(
            &service,
            src_root,
            &mut documents,
            &mut prepared_documents.documents,
        )?;
        if explicit_targets.has_target_ids() {
            source_scopes.entry(service.clone()).or_insert_with(|| {
                let mut scopes = Vec::new();
                if let Some(Some(document)) = documents.get(&service) {
                    append_target_scopes(&mut scopes, &service, document, &explicit_targets);
                }
                scopes
            });
        }

        if collect_settings_file_change(
            args,
            &absolute_path,
            &service,
            &property_filter,
            &mut changed_services,
        ) {
            continue;
        }

        source_children.entry(service.clone()).or_insert_with(|| {
            (documents.get(&service).and_then(Option::as_ref))
                .map_or_else(EditorDocumentIndex::default, |document| {
                    EditorDocumentIndex::new(document)
                })
        });

        if (metadata.as_ref().is_some_and(std::fs::Metadata::is_dir)
            || prepared_documents
                .instance_paths
                .contains(&path_key(&absolute_path)))
            && let Some(document) = documents.get(&service).and_then(Option::as_ref)
            && let Some(target) = editor_directory_target(
                document,
                &source_children[&service].children,
                &service,
                &src_root.join(&service),
                &absolute_path,
            )
        {
            if read_only_changed_path(
                link_enforcement,
                &service,
                Some((&target.path_segments, &target.path_ordinals)),
                full_reconcile,
                &mut changed_services,
            ) {
                continue;
            }
            changed_services.record_scope(
                full_reconcile,
                &service,
                Some((&target.path_segments, &target.path_ordinals)),
            );
            if prepared_documents
                .whole_instance_paths
                .contains(&path_key(&absolute_path))
            {
                changes.whole_instances.push((
                    service.clone(),
                    EditorInstancePath {
                        path_segments: target.path_segments.clone(),
                        path_ordinals: target.path_ordinals.clone(),
                    },
                ));
            }
            property_filter.add_service_ids(&service, target.settings_ids);
            changed_services.settings.insert(service.clone());
            changed_services.target_upsert.insert(service);
            continue;
        }

        let mut mapped_target = documents
            .get(&service)
            .and_then(Option::as_ref)
            .zip(source_children.get(&service))
            .and_then(|(document, index)| {
                editor_source_target_with_cache(
                    document,
                    &service,
                    &src_root.join(&service),
                    &absolute_path,
                    &index.children,
                    source_stems.entry(service.clone()).or_default(),
                )
            });

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
        if explicit_targets.has_target_ids()
            && !args.targets_extend_paths
            && !protected_path.is_some_and(|(segments, ordinals)| {
                source_path_within_filter(
                    source_scopes.get(&service).map_or(&[], Vec::as_slice),
                    &push_scope(&service, segments, ordinals),
                )
            })
        {
            continue;
        }
        if read_only_changed_path(
            link_enforcement,
            &service,
            protected_path,
            full_reconcile && !(exists_as_file && mapped_target.is_some()),
            &mut changed_services,
        ) {
            continue;
        }
        changed_services.record_scope(full_reconcile, &service, protected_path);
        if mapped_target.is_none()
            && exists_as_file
            && let Some(spec) = inferred_spec.as_ref()
        {
            bulk::read_whole_once(&mut read_whole, &mut documents, src_root, &service)?;
            let index = source_children
                .get_mut(&service)
                .expect("the service's index is built above");
            let snapshot = snapshots.entry(service.clone()).or_default();
            let settings_before = documents
                .get(&service)
                .and_then(Option::as_ref)
                .map(|document| snapshot.before_creating(document, index, spec))
                .transpose()?;
            let slot = documents
                .get_mut(&service)
                .expect("service document should be loaded");
            let document = Arc::make_mut(slot.get_or_insert_with(|| {
                Arc::new(SettingsBytecode {
                    version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
                    instances: Vec::new(),
                })
            }));
            let reserved = retired_ids.entry(service.clone()).or_default();
            let added_from = document.instances.len();
            let ensured = ensure_editor_source_target_with_index(document, spec, reserved, index)?;
            let target = ensured.target;
            if ensured.changed {
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
                        attributes_complete: false,
                    });
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
                    if let (Some(_), Some(target_settings_id)) =
                        (settings_before.as_ref(), target.settings_id.as_deref())
                    {
                        bulk::repair_references(
                            &mut store_referrers,
                            &mut changes,
                            document,
                            &service,
                            &index.children,
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
                changes.history_entries.push(EditorHistoryEntry {
                    service: service.clone(),
                    source_path: Some(absolute_path.clone()),
                    settings_id: target.settings_id.clone(),
                    path_segments: target.path_segments.clone(),
                    path_ordinals: target.path_ordinals.clone(),
                    class_name: target.class_name.clone(),
                    source_key: Some(editor_source_key_from_target(&target)),
                    settings_before,
                });
                snapshot.created(document, added_from, target.settings_id.as_deref());
                bulk::forget_added(
                    (&mut source_children, &mut source_stems),
                    &service,
                    document,
                    added_from,
                );
            }
            mapped_target = Some(target);
        }

        if !exists_as_file
            && let Some(target) = mapped_target.as_ref()
            && is_lua_source_class(&target.class_name)
        {
            if !explicit_targets.has_target_ids()
                && inferred_spec.as_ref().is_some_and(|spec| !spec.is_init)
                && let Some(collected) = (bulk::DeletedScriptRun {
                    src_root,
                    service: &service,
                    full_reconcile,
                    link_enforcement,
                    instance_paths: &prepared_documents.instance_paths,
                    documents: &mut documents,
                    read_whole: &mut read_whole,
                    references: &mut store_references,
                    referrers: &mut store_referrers,
                    indexes: &mut source_children,
                    stems: &mut source_stems,
                    seen_paths: &mut seen_paths,
                    changed_services: &mut changed_services,
                    retired_ids: &mut retired_ids,
                    snapshots: &mut snapshots,
                    changes: &mut changes,
                })
                .collect(&absolute_path, &inspected_paths[at + 1..])?
            {
                next = at + 1 + collected;
                continue;
            }
            if let Some(settings_id) = target.settings_id.as_deref()
                && let Some(document) = documents.get_mut(&service).and_then(Option::as_mut)
                && let Some(index) =
                    bulk::instance_index(&store_referrers, document, &service, settings_id)
                && link_enforcement
                    .read_only_package_for_path(
                        &service,
                        &target.path_segments,
                        &target.path_ordinals,
                    )
                    .is_none()
            {
                bulk::read_whole_once(&mut read_whole, &mut documents, src_root, &service)?;
                let document = Arc::make_mut(
                    (documents.get_mut(&service).and_then(Option::as_mut))
                        .expect("the service's document is loaded above"),
                );
                let original_class = document.instances[index].class_name.clone();
                let init = inferred_spec.as_ref().is_some_and(|spec| spec.is_init);
                if is_lua_source_class(&original_class) {
                    let snapshot = snapshots.entry(service.clone()).or_default();
                    changed_services.dirty.insert(service.clone());
                    let settings_before = if init {
                        let children = &(source_children.get(&service))
                            .expect("the service's index is built above")
                            .children;
                        let settings_before =
                            snapshot.before_changing_subtree(document, children, Some(index))?;
                        document.instances[index].class_name = "Folder".to_string();
                        let descriptor = bulk::descriptor_for_known_path(
                            document,
                            children,
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
                        bulk::repair_references(
                            &mut store_referrers,
                            &mut changes,
                            document,
                            &service,
                            children,
                            settings_id,
                        );
                        snapshot.touch([settings_id.to_string()]);
                        settings_before
                    } else {
                        let descriptor = editor_instance_descriptor_for_known_path(
                            document,
                            index,
                            target.path_segments.clone(),
                            target.path_ordinals.clone(),
                        )
                        .context("Failed to describe the deleted source instance")?;
                        let mut removed = Vec::new();
                        crate::bytecode::edit::collect_settings_subtree_preorder(
                            &settings_children_by_parent(document),
                            index,
                            &mut removed,
                        );
                        let removed_ids = (removed.iter())
                            .map(|removed| document.instances[*removed].settings_id.clone())
                            .collect::<Vec<_>>();
                        let settings_before = snapshot
                            .before_changing(document, removed_ids.iter().map(String::as_str))?;
                        let hits = editor_reference_hits(document, &service, settings_id);
                        let parent = (document.instances[index].parent_index)
                            .map(|parent| document.instances[parent].settings_id.clone());
                        let referring = bulk::referring_by_index(document, &removed);
                        (retired_ids.entry(service.clone()).or_default())
                            .extend(removed_ids.iter().cloned());
                        remove_instances_at_indices(document, &[index], true)?;
                        store_referrers.remove(&service);
                        append_editor_reference_hit_repairs(
                            &mut changes,
                            &hits,
                            document,
                            &service,
                        );
                        changes.instance_changes.push(EditorInstanceChange {
                            mode: "deleteInstances".to_string(),
                            service: service.clone(),
                            allow_deletes: false,
                            instances: vec![descriptor],
                            preserve_instances: Vec::new(),
                        });
                        snapshot.touch(removed_ids.into_iter().chain(parent).chain(referring));
                        settings_before
                    };
                    changes.history_entries.push(EditorHistoryEntry {
                        service: service.clone(),
                        source_path: Some(absolute_path.clone()),
                        settings_id: Some(settings_id.to_string()),
                        path_segments: target.path_segments.clone(),
                        path_ordinals: target.path_ordinals.clone(),
                        class_name: original_class,
                        source_key: Some(editor_source_key_from_target(target)),
                        settings_before: Some(settings_before),
                    });
                    bulk::forget_deleted((&mut source_children, &mut source_stems), &service, init);
                }
            }
            continue;
        }

        let Some(target) = mapped_target else {
            continue;
        };

        let source = if exists_as_file {
            Some(
                prefetched_source
                    .take()
                    .unwrap_or_else(|| fs::read_to_string(&absolute_path))
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

    widen_target_ids_to_subtrees(&mut property_filter, &documents, args.exact_target_ids);
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

/// A targeted id names one instance. A script keeps that meaning (its child
/// scripts arrive through the source scopes), but a model or folder carries
/// its whole subtree, so a newly added model is pushed complete rather than
/// as an empty root.
fn widen_target_ids_to_subtrees(
    filter: &mut EditorPropertyFilter,
    documents: &HashMap<String, Option<Arc<SettingsBytecode>>>,
    exact: bool,
) {
    // A push that names properties edits the targeted instances alone.
    if exact || !filter.has_target_ids() || !filter.property_names.is_empty() {
        return;
    }
    for (service, document) in documents {
        let Some(document) = document else {
            continue;
        };
        let ids = filter.ids_for_service(service);
        let children = settings_children_by_parent(document);
        let mut widened = Vec::new();
        for (index, instance) in document.instances.iter().enumerate() {
            if instance.parent_index.is_none()
                || is_lua_source_class(&instance.class_name)
                || !ids.contains(&instance.settings_id)
            {
                continue;
            }
            let mut subtree = Vec::new();
            crate::bytecode::edit::collect_settings_subtree_preorder(
                &children,
                index,
                &mut subtree,
            );
            widened.extend(
                subtree
                    .into_iter()
                    .skip(1)
                    .map(|child| document.instances[child].settings_id.clone()),
            );
        }
        if !widened.is_empty() {
            filter.add_service_ids(service, widened);
        }
    }
}

fn log_change_set_composition(
    changes: &EditorChangeSet,
    binary_import: Option<&EditorBinaryImport>,
) {
    let _span = crate::app::profile::span("native editor change set composition");
    let instances = changes
        .instance_changes
        .iter()
        .map(|change| {
            format!(
                "{}:{}(deletes={}, instances={}, preserve={})",
                change.service,
                change.mode,
                change.allow_deletes,
                change.instances.len(),
                change.preserve_instances.len()
            )
        })
        .collect::<Vec<_>>();
    let mut by_class = HashMap::<&str, (usize, usize)>::new();
    for change in &changes.property_changes {
        let entry = by_class.entry(change.class_name.as_str()).or_default();
        entry.0 += 1;
        entry.1 += change.properties.len() + change.reset_properties.len();
    }
    let mut by_class = by_class.into_iter().collect::<Vec<_>>();
    by_class.sort_by_key(|(_, (rows, _))| std::cmp::Reverse(*rows));
    let properties = by_class
        .iter()
        .take(12)
        .map(|(class, (rows, fields))| format!("{class}:{rows}/{fields}"))
        .collect::<Vec<_>>();
    let import = binary_import.map(|import| {
        format!(
            "bytes={}, instances={}, replacement={}, post_apply_paths={}, groups=[{}]",
            import.bytes.len(),
            import.instance_count,
            import.native_replacement.is_some(),
            import.post_apply_properties_by_path.len(),
            import
                .groups
                .iter()
                .map(|group| format!(
                    "{}:{}{}",
                    group.service,
                    group.count,
                    if group.additive { " additive" } else { "" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    log_global(
        5,
        format_args!(
            "[renium] editor change set: instances=[{}] sources={} properties=[{}] import={}",
            instances.join(", "),
            changes.source_changes.len(),
            properties.join(", "),
            import.unwrap_or_else(|| "none".into())
        ),
    );
}

#[cfg(test)]
mod sync_tests {

    #[test]
    fn mutation_roots_name_each_root_of_a_service_once_by_name_and_ordinal() {
        use super::{EditorChangeSet, EditorTransaction};
        use crate::editor::types::EditorPropertyChange;
        use serde_json::json;
        let change = |service: &str, path: &[&str], ordinals: &[usize]| EditorPropertyChange {
            service: service.into(),
            path_segments: path.iter().map(|segment| segment.to_string()).collect(),
            path_ordinals: ordinals.to_vec(),
            class_name: "Part".into(),
            ..EditorPropertyChange::default()
        };
        let mut changes = EditorChangeSet {
            property_changes: vec![
                change("Workspace", &["Workspace", "Model", "Part"], &[1, 2, 1]),
                change("Lighting", &["Lighting", "Sky"], &[]),
                change("Workspace", &["Workspace", "Model"], &[1, 2]),
                change("Workspace", &["Workspace", "Model"], &[1, 1]),
                change("Workspace", &["Workspace"], &[1]),
            ],
            ..EditorChangeSet::default()
        };
        for index in 0..1500 {
            let name = format!("Wide{index}");
            changes
                .property_changes
                .push(change("Workspace", &["Workspace", &name], &[1, 1]));
        }
        let parameters = EditorTransaction::parameters(
            &changes,
            None,
            "id",
            vec!["Lighting".into(), "Workspace".into()],
            None,
        );
        let names = |range: std::ops::Range<usize>| {
            range
                .map(|index| match index {
                    0 | 1 => "Model".to_string(),
                    index => format!("Wide{}", index - 2),
                })
                .collect::<Vec<_>>()
        };
        let ordinals = |range: std::ops::Range<usize>| {
            range
                .map(|index| if index == 0 { 2 } else { 1 })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            parameters["mutationRoots"],
            json!([
                { "service": "Workspace", "names": names(0..1024), "ordinals": ordinals(0..1024) },
                { "service": "Workspace", "names": names(1024..1502), "ordinals": ordinals(1024..1502) },
                { "service": "Lighting", "names": ["Sky"], "ordinals": [1] },
            ])
        );
    }

    #[test]
    fn a_final_line_break_only_the_file_has_is_an_edit_to_send() {
        use super::{editor_sources_match, studio_holds_source};
        // (file, Studio, already held, accepted after a write)
        let cases = [
            ("return 1", "return 1", true, true),
            ("a\r\nb", "a\nb", true, true),
            ("return 1\n", "return 1", false, true),
            ("return 1\r\n", "return 1", false, true),
            ("return 1", "return 1\n", false, false),
            ("return 1\n\n", "return 1", false, false),
            ("return 1", "return 2", false, false),
        ];
        for (file, studio, held, accepted) in cases {
            assert_eq!(
                studio_holds_source(file, studio),
                held,
                "{file:?} {studio:?}"
            );
            assert_eq!(
                editor_sources_match(file, studio),
                accepted,
                "{file:?} {studio:?}"
            );
        }
    }

    /// A push counts the scripts Studio holds in other bytes than the file:
    /// those it verifies and those it finds unchanged.
    #[test]
    fn a_push_counts_the_scripts_studio_holds_in_other_bytes() -> anyhow::Result<()> {
        use super::{EditorChangeSet, drop_unchanged_source_changes, verify_editor_source_changes};
        use crate::editor::types::EditorSourceChange;
        use crate::studio::bridge::routing_tests;
        use serde_json::{Value, json};
        use tungstenite::{Message, WebSocket, protocol::Role};
        // (file, Studio)
        let scripts = [
            ("return 1", "return 1"),
            ("return 2\n", "return 2"),
            ("a\r\nb", "a\nb"),
            ("return 3", "return 4"),
        ];
        let bridge = routing_tests::fixture();
        let stream = routing_tests::connect(&bridge, 0, routing_tests::edit_info("sources", 20));
        let studio = scripts.map(|(_, studio)| studio);
        let plugin = std::thread::spawn(move || {
            let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
            while let Ok(Message::Text(text)) = socket.read() {
                let request: Value = serde_json::from_str(&text).unwrap();
                let rows = request["params"]["selectors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|selector| {
                        let index = selector["index"].as_u64().unwrap();
                        json!({"index": index, "source": studio[index as usize], "className": "ModuleScript"})
                    })
                    .collect::<Vec<_>>();
                let reply = json!({"id": request["id"], "ok": true, "result": {"rows": rows}});
                if socket
                    .send(Message::Text(reply.to_string().into()))
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut changes = EditorChangeSet {
            source_changes: scripts
                .iter()
                .enumerate()
                .map(|(index, (file, _))| EditorSourceChange {
                    service: "ServerStorage".into(),
                    settings_id: None,
                    path_segments: vec!["ServerStorage".into(), format!("Script{index}")],
                    path_ordinals: vec![1, 1],
                    class_name: "ModuleScript".into(),
                    source: Some((*file).into()),
                    deleted: false,
                })
                .collect(),
            ..EditorChangeSet::default()
        };
        let verification = verify_editor_source_changes(&bridge, &changes, None)?;
        assert_eq!((verification.verified, verification.inexact), (3, 2));
        assert_eq!(
            drop_unchanged_source_changes(&bridge, &mut changes)?,
            (2, 1)
        );
        drop(bridge);
        let _ = plugin.join();
        Ok(())
    }

    #[test]
    fn supplemental_write_limit_counts_fields_and_names_the_reason_before_commit() {
        let row = EditorPropertyChange {
            class_name: "Example".into(),
            properties: Map::from_iter([
                ("SavedA".into(), json!(true)),
                ("SavedB".into(), json!(7)),
            ]),
            ..Default::default()
        };
        let mut rows = vec![row; MAX_INDIVIDUAL_IMPORT_WRITES / 2];
        let failed = ServiceReaderFallback::DiscoveryFailed("ambiguous reader".into());
        assert!(!check_import_route(&rows, Some(&failed), false, true, || panic!()).unwrap());
        rows.push(EditorPropertyChange {
            class_name: "Other".into(),
            properties: Map::from_iter([("SavedC".into(), json!(0))]),
            ..Default::default()
        });
        let error = check_import_route(&rows, Some(&failed), false, false, || panic!())
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous reader"));
        assert!(error.contains("1.0 s at 31 ms per field"));
        assert!(error.contains("--slow-import"));
        assert!(!error.contains("rbx ps --yes"));
        assert!(error.contains("33 individual supplemental field writes (limit 32)"));
        assert!(error.contains("Example.SavedA (16)"));
        assert!(error.contains("Other.SavedC (1)"));
        assert!(check_import_route(&rows, Some(&failed), false, true, || true).unwrap());
        let unknown = check_import_route(&rows, Some(&failed), false, true, || false).unwrap_err();
        assert!(unknown.is::<SlowImportRequired>());
        let unknown = unknown.to_string();
        assert!(unknown.contains("If Studio reports additional protected writes"));
        assert!(unknown.contains("31 ms"));
        assert!(unknown.contains("rbx ps --slow-import"));
        assert!(
            crate::cli::command()
                .try_get_matches_from(["rbx", "ps", "--slow-import"])
                .is_ok()
        );
        let individual =
            check_import_route(&rows, Some(&failed), false, false, || panic!()).unwrap_err();
        assert!(individual.to_string().contains("1.0 s at 31 ms"));
        assert!(!check_import_route(&rows, Some(&failed), true, true, || panic!()).unwrap());
        for fallback in [
            None,
            Some(ServiceReaderFallback::Platform),
            Some(ServiceReaderFallback::Disabled),
            Some(ServiceReaderFallback::PackageRoots),
        ] {
            assert!(
                !check_import_route(&rows, fallback.as_ref(), false, true, || panic!()).unwrap()
            );
        }
    }

    #[test]
    fn supplemental_documents_use_retained_originals_without_the_store() -> Result<()> {
        use crate::rbx::import_rows::{ImportRowCandidates, identity};
        use crate::settings::bytecode::SettingsBytecodeInstance;
        let mut document = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "root".into(),
                    "ReplicatedStorage".into(),
                    "ReplicatedStorage".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "model".into(),
                    "Model".into(),
                    "Model".into(),
                    Some(0),
                ),
                SettingsBytecodeInstance::new("part".into(), "Part".into(), "Part".into(), Some(1)),
            ],
        };
        document.instances[1].properties.insert(
            "PrimaryPart".into(),
            json!({"_type":"Ref", "instanceIndex":2}),
        );
        document.instances[1].properties.insert(
            "WorldPivotData".into(),
            json!({
                "_type":"CFrame", "components":[1,2,3,1,0,0,0,1,0,0,0,1]
            }),
        );
        document.instances[1].properties.insert("FuturePayload".into(), json!({
            "_type":"SharedString", "base64":base64::encode(b"whole original retained payload".repeat(64))
        }));
        document.instances[1]
            .attributes
            .insert("WholeRow".into(), json!(true));
        let bytes = crate::settings::bytecode::encode_settings_bytecode(&document)?;
        let document = crate::settings::bytecode::decode_settings_bytecode_holding(&bytes)?;
        let candidates = || {
            ImportRowCandidates {
                services: HashMap::from([(
                    "ReplicatedStorage".into(),
                    (
                        identity(&document),
                        vec![(
                            rbx_dom_weak::types::Ref::new(),
                            1,
                            document.instances[1].clone(),
                        )],
                    ),
                )]),
            }
            .finish(&HashMap::new(), &HashMap::new(), &HashMap::new())
        };
        let mut sparse = document.clone();
        for row in sparse.instances.iter_mut().skip(1) {
            row.properties.clear();
            row.attributes.clear();
            crate::settings::held::stand_unchanged(&mut row.properties);
        }
        let expected = crate::settings::equivalence::read_instances(&sparse, &bytes, &[1])?;
        let mut changes = EditorChangeSet::default();
        changes
            .native_property_documents
            .insert("ReplicatedStorage".into(), Arc::new(sparse.clone()));
        let mut import = replacement_fixture();
        import.kept_rows = candidates()?;
        // An empty private directory has no store to reread. A regression to
        // file materialization must fail here, before comparing any values.
        let root = crate::tests::support::temp_dir("import-retained-supplemental");
        let actual = supplemental_documents(&changes, Some(&mut import), &root)?;
        assert_eq!(actual["ReplicatedStorage"].as_ref(), &expected);
        assert!(import.kept_rows.is_empty());
        for changed in [0, 1] {
            let mut invalid = sparse.clone();
            if changed == 0 {
                invalid.instances[2].settings_id.push_str(":changed");
            } else {
                invalid.instances[1].parent_index = Some(2);
            }
            let kept = candidates()?.remove("ReplicatedStorage").unwrap();
            assert!(kept.materialize(&invalid).is_err());
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn supplemental_without_a_whole_import_uses_the_held_whole_reader() -> Result<()> {
        use crate::settings::bytecode::{SETTINGS_BINARY_VERSION, SettingsBytecodeInstance};
        let root = crate::tests::support::temp_dir("supplemental-whole-reader");
        let service = "ReplicatedStorage";
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![SettingsBytecodeInstance::new(
                "root".into(),
                service.into(),
                service.into(),
                None,
            )],
        };
        for index in 0..64 {
            let mut row = SettingsBytecodeInstance::new(
                index.to_string(),
                "Payload".into(),
                "MeshPart".into(),
                Some(0),
            );
            row.properties.insert("TexturePack".into(), json!({"_type":"SharedString", "base64":base64::encode(b"repeated whole-reader payload".repeat(256))}));
            document.instances.push(row);
        }
        let path = service_settings_path(&root.join(service));
        fs::create_dir_all(path.parent().unwrap())?;
        document.write_file(&path)?;
        let bytes = fs::read(&path)?;
        let mut expected = crate::settings::bytecode::decode_settings_bytecode_holding(&bytes)?;
        crate::settings::equivalence::stabilize_settings_reference_ids(&mut expected);
        crate::settings::equivalence::canonicalize_settings_property_names(&mut expected)?;
        let mut sparse = expected.clone();
        for row in sparse.instances.iter_mut().skip(1) {
            row.properties.clear();
            crate::settings::held::stand_unchanged(&mut row.properties);
        }
        let changes = EditorChangeSet {
            native_property_documents: HashMap::from([(service.into(), Arc::new(sparse))]),
            ..Default::default()
        };
        // An additive document can have reduced/re-indexed rows. Its identities
        // must never be tested against this partly read whole store.
        let mut additive = replacement_fixture();
        additive.groups[0].additive = true;
        additive.kept_rows = crate::rbx::import_rows::ImportRowCandidates {
            services: HashMap::from([(service.into(), ([99; 32], Vec::new()))]),
        }
        .finish(&HashMap::new(), &HashMap::new(), &HashMap::new())?;
        let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build()?;
        for import in [None, Some(&mut additive)] {
            pool.install(|| -> Result<()> {
                use crate::settings::bytecode::property_edit::ROW_DECODE_CALLS;
                ROW_DECODE_CALLS.set(0);
                let actual = supplemental_documents(&changes, import, &root)?;
                assert_eq!(actual[service].as_ref(), &expected);
                // This is a memory-path regression: decode_rows expands shared
                // payloads before holding them. Equal values alone cannot catch it.
                assert_eq!(
                    ROW_DECODE_CALLS.get(),
                    0,
                    "whole fallback must not expand shared payloads row by row"
                );
                Ok(())
            })?;
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    pub(super) fn replacement_fixture() -> EditorBinaryImport {
        use crate::editor::types::EditorBinaryImportGroup;
        EditorBinaryImport {
            kept_rows: HashMap::new(),
            bytes: Vec::new(),
            native_replacement: None,
            fallback_reason: None,
            groups: vec![EditorBinaryImportGroup {
                additive: false,
                service: "ReplicatedStorage".to_string(),
                target_path: vec!["ReplicatedStorage".to_string()],
                count: 1,
                payload_root_name: "payload".to_string(),
                expected_structure: None,
                root_paths: Vec::new(),
                root_settings_ids: Vec::new(),
                instance_identities: None,
                viewport_camera: None,
                retained_roots: Vec::new(),
                package_roots: Vec::new(),
                mutation_package_roots: Vec::new(),
                change_generation: None,
            }],
            instance_count: 0,
            post_apply_properties_by_class: HashMap::new(),
            post_apply_properties_by_path: HashMap::new(),
            external_references_post_applied: false,
            viewport_references_post_applied: false,
        }
    }

    #[test]
    fn imported_engine_fields_wait_for_the_import_to_land() {
        let import = replacement_fixture();
        let row = |segments: &[&str], class_name: &str, properties: &[(&str, Value)]| {
            EditorPropertyChange {
                service: "ReplicatedStorage".into(),
                settings_id: None,
                path_segments: segments.iter().map(|s| s.to_string()).collect(),
                path_ordinals: vec![1; segments.len()],
                class_name: class_name.into(),
                properties: properties
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
                reset_properties: Vec::new(),
                attributes: Map::new(),
                deleted_attributes: Vec::new(),
                attributes_complete: false,
            }
        };
        let mut changes = EditorChangeSet {
            property_changes: vec![
                row(
                    &["ReplicatedStorage"],
                    "ReplicatedStorage",
                    &[("SourceAssetId", json!(7))],
                ),
                row(
                    &["ReplicatedStorage", "Car", "Body", "SurfaceAppearance"],
                    "SurfaceAppearance",
                    &[
                        ("TexturePack", json!("rbxassetid://1")),
                        ("Color", json!({"_type": "Color3", "r": 1, "g": 1, "b": 1})),
                    ],
                ),
                row(
                    &["ReplicatedStorage", "Car", "Body", "Mesh"],
                    "MeshPart",
                    &[("MeshId", json!("rbxassetid://2"))],
                ),
            ],
            ..EditorChangeSet::default()
        };
        defer_imported_native_root_rows(&mut changes, Some(&import));
        assert_eq!(changes.deferred_native_rows.len(), 2);
        assert_eq!(
            individual_import_write_count(&changes.deferred_native_rows),
            2
        );
        assert_eq!(changes.property_changes.len(), 2);
        assert!(
            changes.property_changes[0]
                .properties
                .contains_key("SourceAssetId")
        );
        assert_eq!(
            changes.property_changes[1]
                .properties
                .keys()
                .collect::<Vec<_>>(),
            vec!["Color"]
        );
        assert!(
            changes.deferred_native_rows[0]
                .properties
                .contains_key("TexturePack")
        );
        assert!(
            changes.deferred_native_rows[1]
                .properties
                .contains_key("MeshId")
        );
        let mut untouched = EditorChangeSet {
            property_changes: vec![row(
                &["ReplicatedStorage", "Car"],
                "Model",
                &[("Scale", json!(2.0))],
            )],
            ..EditorChangeSet::default()
        };
        defer_imported_native_root_rows(&mut untouched, None);
        assert!(untouched.deferred_native_rows.is_empty());
        assert_eq!(untouched.property_changes.len(), 1);
    }

    use super::*;

    #[test]
    fn service_upserts_create_parents_before_source_batch_anchors() {
        let descriptor =
            |name: &str, anchor_only: bool| crate::editor::types::EditorInstanceDescriptor {
                settings_id: name.to_string(),
                path_segments: if name == "Public" {
                    vec!["ReplicatedStorage".into(), name.into()]
                } else {
                    vec!["ReplicatedStorage".into(), "Public".into(), name.into()]
                },
                class_name: "Folder".into(),
                anchor_only,
                ..Default::default()
            };
        let mut changes = EditorChangeSet::default();
        changes
            .instance_changes
            .push(crate::editor::types::EditorInstanceChange {
                mode: "upsertInstances".into(),
                service: "ReplicatedStorage".into(),
                allow_deletes: false,
                instances: vec![descriptor("Public", true), descriptor("Child", false)],
                preserve_instances: Vec::new(),
            });
        crate::editor::diff::push_editor_instance_change(
            &mut changes,
            "upsertInstances",
            "ReplicatedStorage",
            false,
            vec![descriptor("Public", false), descriptor("Untouched", true)],
        );
        assert_eq!(changes.instance_changes.len(), 1);
        let instances = &changes.instance_changes[0].instances;
        assert_eq!(instances.len(), 3);
        assert_eq!(instances[0].settings_id, "Public");
        assert!(!instances[0].anchor_only);
        assert_eq!(instances[1].settings_id, "Child");
        assert!(!instances[1].anchor_only);
        assert_eq!(instances[2].settings_id, "Untouched");
        assert!(instances[2].anchor_only);
    }

    #[test]
    fn only_anchors_at_their_studio_place_spare_the_structural_snapshot() {
        let anchor = || crate::editor::types::EditorInstanceDescriptor {
            path_segments: vec!["Workspace".into(), "Rail".into()],
            path_ordinals: vec![1, 2],
            previous_path_segments: vec!["Workspace".into(), "Rail".into()],
            previous_path_ordinals: vec![1, 2],
            anchor_only: true,
            ..Default::default()
        };
        let upsert = |instance: crate::editor::types::EditorInstanceDescriptor| {
            vec![crate::editor::types::EditorInstanceChange {
                mode: "upsertInstances".into(),
                service: "Workspace".into(),
                allow_deletes: false,
                instances: vec![anchor(), instance],
                preserve_instances: Vec::new(),
            }]
        };
        assert!(!changes_instances(&upsert(anchor())));
        // Studio lists the namesakes in another order: the value read by
        // path before the push would be another instance's.
        let mut reordered = anchor();
        reordered.previous_path_ordinals = vec![1, 1];
        assert!(changes_instances(&upsert(reordered)));
        let mut created = anchor();
        created.anchor_only = false;
        assert!(changes_instances(&upsert(created)));
        let mut unplaced = anchor();
        unplaced.previous_path_segments.clear();
        unplaced.previous_path_ordinals.clear();
        assert!(!changes_instances(&upsert(unplaced.clone())));
        unplaced.ambiguous_siblings = true;
        assert!(changes_instances(&upsert(unplaced)));
        let mut identified = anchor();
        identified.previous_settings_id = Some("debug:0_1".into());
        assert!(changes_instances(&upsert(identified)));
        let mut removal = upsert(anchor());
        removal[0].mode = "deleteInstances".into();
        assert!(changes_instances(&removal));
    }

    use crate::cli::BridgeConnectionArgs;

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
            attributes_complete: false,
        }
    }

    #[test]
    fn rows_that_bind_parts_follow_the_rows_that_place_them() {
        let row = |name: &str, properties: &[&str]| {
            let mut row = model_pivot(&["Workspace", "Rig", name]);
            for property in properties {
                row.properties.insert((*property).to_string(), json!(1));
            }
            row
        };
        let mut rows = vec![
            row("Torso", &["CFrame"]),
            row("Neck", &["C0", "Part0", "Part1"]),
            row("Weld", &["Part1"]),
            row("Head", &["CFrame", "Anchored"]),
            row("Rigid", &["Attachment0"]),
            row("Arm", &[]),
        ];
        bind_joints_last(&mut rows);
        let order = rows
            .iter()
            .map(|row| row.path_segments[2].as_str())
            .collect::<Vec<_>>();
        assert_eq!(order, ["Torso", "Head", "Arm", "Neck", "Weld", "Rigid"]);
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

    /// A push of chosen instances reads its store for those instances and
    /// what tells them from their siblings. Whatever the store holds and
    /// whichever instances are chosen, it sends what the push that reads the
    /// whole store sends.
    #[test]
    fn a_push_of_chosen_instances_reads_them_alone_and_sends_what_the_whole_store_sends() {
        use crate::cli::ProjectSourceArgs;
        use crate::settings::bytecode::SettingsBytecodeInstance;
        use crate::tests::support::{SimRng, run_seeds};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (read_in_part, sent_something) = (AtomicUsize::new(0), AtomicUsize::new(0));
        run_seeds(200, |seed, _| {
            let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let service = ["Workspace", "ReplicatedStorage"][rng.below(2)];
            let mut instances = vec![SettingsBytecodeInstance::new(
                "root".into(),
                service.into(),
                service.into(),
                None,
            )];
            let count = 12 + rng.below(40);
            for index in 1..count {
                let class = ["Folder", "Part", "ObjectValue", "Camera"][rng.below(4)];
                let mut instance = SettingsBytecodeInstance::new(
                    format!("id{index}"),
                    ["Crate", "Door", "Lamp", "Spawn"][rng.below(4)].into(),
                    class.into(),
                    Some(rng.below(index)),
                );
                match class {
                    "Part" => {
                        instance
                            .properties
                            .insert("Transparency".into(), json!(rng.below(5) as f64 / 4.0));
                        instance
                            .properties
                            .insert("Anchored".into(), json!(rng.below(2) == 0));
                    }
                    "ObjectValue" => {
                        instance.properties.insert(
                            "Value".into(),
                            json!({"_type": "Ref", "settingsId": format!("id{}", 1 + rng.below(count - 1))}),
                        );
                    }
                    _ => {}
                }
                if rng.below(3) == 0 {
                    instance
                        .attributes
                        .insert("Tier".into(), json!(rng.below(4)));
                }
                instances.push(instance);
            }
            if service == "Workspace"
                && let Some(camera) = instances.iter().position(|row| row.class_name == "Camera")
            {
                let id = instances[camera].settings_id.clone();
                instances[0].properties.insert(
                    "CurrentCamera".into(),
                    json!({"_type": "Ref", "settingsId": id}),
                );
            }
            let document = SettingsBytecode {
                version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
                instances,
            };
            let root = crate::tests::support::temp_dir(&format!("chosen-instances-{seed}"));
            let store = root.join(format!("instances/{service}.renium"));
            fs::create_dir_all(root.join("src")).map_err(|error| error.to_string())?;
            fs::write(root.join("renium.project.jsonc"), r#"{"schemaVersion":1}"#)
                .map_err(|error| error.to_string())?;
            document
                .write_file(&store)
                .map_err(|error| format!("{error:#}"))?;
            let mut args = PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root.clone(),
                    src_root: PathBuf::from("src"),
                },
                BridgeConnectionArgs::local(0.1),
            );
            args.changed_paths.push(store);
            for _ in 0..1 + rng.below(3) {
                args.target_settings_ids
                    .push(format!("id{}", 1 + rng.below(count - 1)));
            }
            args.exact_target_ids = rng.below(2) == 0;
            let sent = |changes: &EditorChangeSet| {
                json!({
                    "properties": &changes.property_changes,
                    "instances": &changes.instance_changes,
                    "sources": &changes.source_changes,
                    "deferred": &changes.deferred_native_rows,
                    "whole": changes.whole_instances.len(),
                })
            };
            let prepared = documents_of_targets(&args).map_err(|error| format!("{error:#}"))?;
            let partly = !prepared.documents.is_empty();
            let in_part = collect_project_editor_changes_with_documents(&args, prepared)
                .map(|(changes, _)| sent(&changes))
                .map_err(|error| format!("{error:#}"));
            let whole = collect_project_editor_changes(&args)
                .map(|(changes, _)| sent(&changes))
                .map_err(|error| format!("{error:#}"));
            let _ = fs::remove_dir_all(&root);
            if in_part != whole {
                return Err(format!(
                    "read for its chosen instances the push sends\n{in_part:?}\nand read whole\n{whole:?}"
                ));
            }
            if partly {
                read_in_part.fetch_add(1, Ordering::Relaxed);
                if whole.is_ok_and(|sent| {
                    sent["properties"]
                        .as_array()
                        .is_some_and(|rows| !rows.is_empty())
                        || sent["instances"]
                            .as_array()
                            .is_some_and(|rows| !rows.is_empty())
                }) {
                    sent_something.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        });
        if std::env::var_os("RENIUM_SIM_SEED").is_none() {
            let (partly, sending) = (
                read_in_part.load(Ordering::Relaxed),
                sent_something.load(Ordering::Relaxed),
            );
            assert!(
                partly >= 100 && sending >= 80,
                "{partly} pushes read a store in part, {sending} of them sent something"
            );
        }
    }

    /// A verification of sources reads each store for its scripts and what
    /// tells them from their siblings. Whatever the store holds, it compares
    /// the sources the collection that reads the whole store gives.
    #[test]
    fn a_verification_reads_the_scripts_of_a_store_alone_and_compares_what_the_whole_store_gives() {
        use crate::cli::ProjectSourceArgs;
        use crate::settings::bytecode::SettingsBytecodeInstance;
        use crate::tests::support::{SimRng, run_seeds};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (read_in_part, sources) = (AtomicUsize::new(0), AtomicUsize::new(0));
        run_seeds(200, |seed, _| {
            let mut rng = SimRng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let service = ["Workspace", "ReplicatedStorage", "ServerScriptService"][rng.below(3)];
            let root = crate::tests::support::temp_dir(&format!("verified-scripts-{seed}"));
            let scripts = root.join("src").join(service);
            fs::write(root.join("renium.project.jsonc"), r#"{"schemaVersion":1}"#)
                .map_err(|error| error.to_string())?;
            // Scripts as files, some in folders, as a pull writes them.
            let names = ["Crate", "Door", "Main", "Util", "Lamp", "Spawn"];
            let mut folders = vec![scripts.clone()];
            for _ in 0..rng.below(4) {
                let folder = folders[rng.below(folders.len())].join(names[rng.below(6)]);
                if !folders.contains(&folder) {
                    folders.push(folder);
                }
            }
            for folder in &folders {
                fs::create_dir_all(folder).map_err(|error| error.to_string())?;
            }
            for _ in 0..3 + rng.below(8) {
                let file = folders[rng.below(folders.len())].join(format!(
                    "{}{}",
                    names[rng.below(6)],
                    [".luau", ".server.luau", ".client.luau"][rng.below(3)]
                ));
                fs::write(file, format!("return {}\n", rng.below(9)))
                    .map_err(|error| error.to_string())?;
            }
            let mut document = crate::rbx::model::source_only_settings_document(&scripts, service)
                .map_err(|error| format!("{error:#}"))?;
            // What a place holds beside its scripts, namesakes of them too.
            for index in 0..8 + rng.below(40) {
                let mut instance = SettingsBytecodeInstance::new(
                    format!("id{index}"),
                    names[rng.below(6)].into(),
                    ["Folder", "Part", "Model"][rng.below(3)].into(),
                    Some(rng.below(document.instances.len())),
                );
                instance
                    .properties
                    .insert("Transparency".into(), json!(rng.below(5) as f64 / 4.0));
                if rng.below(4) == 0 {
                    instance
                        .attributes
                        .insert("Tier".into(), json!(rng.below(4)));
                }
                document.instances.push(instance);
            }
            document
                .write_file(&root.join(format!("instances/{service}.renium")))
                .map_err(|error| format!("{error:#}"))?;
            let mut args = PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root.clone(),
                    src_root: PathBuf::from("src"),
                },
                BridgeConnectionArgs::local(0.1),
            );
            args.verify = true;
            let compared = |changes: &EditorChangeSet| json!(&changes.source_changes);
            let prepared = documents_of_scripts(&args).map_err(|error| format!("{error:#}"))?;
            let partly = !prepared.documents.is_empty();
            let in_part = collect_project_editor_changes_with_documents(&args, prepared)
                .map(|(changes, _)| compared(&changes))
                .map_err(|error| format!("{error:#}"));
            let whole = collect_project_editor_changes(&args)
                .map(|(changes, _)| compared(&changes))
                .map_err(|error| format!("{error:#}"));
            let _ = fs::remove_dir_all(&root);
            if in_part != whole {
                return Err(format!(
                    "read for its scripts the verification compares\n{in_part:?}\nand read whole\n{whole:?}"
                ));
            }
            if partly && let Ok(compared) = whole {
                read_in_part.fetch_add(1, Ordering::Relaxed);
                sources.fetch_add(compared.as_array().map_or(0, Vec::len), Ordering::Relaxed);
            }
            Ok(())
        });
        if std::env::var_os("RENIUM_SIM_SEED").is_none() {
            let (partly, sources) = (
                read_in_part.load(Ordering::Relaxed),
                sources.load(Ordering::Relaxed),
            );
            assert!(
                partly >= 100 && sources >= 1_000,
                "{partly} verifications read a store in part and compared {sources} sources"
            );
        }
    }

    #[test]
    fn a_changed_script_beside_targeted_instances_is_pushed_by_the_planner() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("script-beside-targets");
        fs::create_dir_all(root.join("src/shared")).unwrap();
        let kept = root.join("src/shared/Keep.luau");
        fs::write(&kept, "return 42\n").unwrap();
        fs::write(root.join("src/shared/Other.luau"), "return 1\n").unwrap();
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"tree":{"ReplicatedStorage":{"$path":"src/shared"}}}"#,
        )
        .unwrap();
        let document = crate::rbx::model::source_only_settings_document(
            &root.join("src/shared"),
            "ReplicatedStorage",
        )
        .unwrap();
        let other = document
            .instances
            .iter()
            .find(|instance| instance.name == "Other")
            .unwrap()
            .settings_id
            .clone();
        document
            .write_file(&root.join("instances/ReplicatedStorage.renium"))
            .unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.push(kept.clone());
        args.target_settings_ids.push(other.clone());
        args.exact_target_ids = true;
        let (narrowed, _) = collect_project_editor_changes(&args).unwrap();
        assert!(
            narrowed.source_changes.is_empty(),
            "a user's -i narrows the named file to the targeted instances"
        );
        args.targets_extend_paths = true;
        let (planned, _) = collect_project_editor_changes(&args).unwrap();
        assert_eq!(
            planned
                .source_changes
                .iter()
                .map(|change| change.path_segments.join("."))
                .collect::<Vec<_>>(),
            ["ReplicatedStorage.Keep"],
            "the planner's listed script file is pushed beside its targeted instances"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_named_script_push_keeps_what_a_pull_wrote_to_the_store() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("named-push-after-pull");
        fs::create_dir_all(root.join("src/shared")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        let script = root.join("src/shared/Balance.luau");
        fs::write(&script, "return 1\n").unwrap();
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"tree":{"ReplicatedStorage":{"Shared":{"$path":"src/shared"}}}}"#,
        )
        .unwrap();
        let store = root.join("instances/ReplicatedStorage.renium");
        crate::rbx::model::source_only_settings_document(&root.join("empty"), "ReplicatedStorage")
            .unwrap()
            .write_file(&store)
            .unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.push(script);
        let write_back = |args: &PushEditorChangesArgs| {
            let (changes, _) = collect_project_editor_changes(args).unwrap();
            for write in changes.settings_writes {
                write.document.write_file(&write.path).unwrap();
            }
        };
        write_back(&args);

        // A file beside the service folders changes too; it belongs to no service.
        fs::write(
            root.join("src/README.md"),
            "notes
",
        )
        .unwrap();
        let mut pulled = SettingsBytecode::read_file(&store).unwrap();
        pulled
            .instances
            .push(crate::settings::bytecode::SettingsBytecodeInstance::new(
                "pulled-folder".to_string(),
                "Finds".to_string(),
                "Folder".to_string(),
                Some(0),
            ));
        pulled.write_file(&store).unwrap();
        write_back(&args);
        assert!(
            SettingsBytecode::read_file(&store)
                .unwrap()
                .instances
                .iter()
                .any(|instance| instance.name == "Finds"),
            "the folder a pull added to the store survives a push of one script"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_named_mapped_folder_is_pushed_as_its_own_subtree() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("named-mapped-folder");
        fs::create_dir_all(root.join("src/shared")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("src/shared/Balance.luau"), "return 1\n").unwrap();
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"tree":{"ReplicatedStorage":{"Shared":{"$path":"src/shared"}}}}"#,
        )
        .unwrap();
        let mut store = crate::rbx::model::source_only_settings_document(
            &root.join("empty"),
            "ReplicatedStorage",
        )
        .unwrap();
        store
            .instances
            .push(crate::settings::bytecode::SettingsBytecodeInstance::new(
                "outside-folder".to_string(),
                "Finds".to_string(),
                "Folder".to_string(),
                Some(0),
            ));
        store
            .write_file(&root.join("instances/ReplicatedStorage.renium"))
            .unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.push(root.join("src/shared"));
        let (changes, _) = collect_project_editor_changes(&args).unwrap();
        assert_eq!(
            changes
                .source_changes
                .iter()
                .map(|change| change.path_segments.join("."))
                .collect::<Vec<_>>(),
            ["ReplicatedStorage.Shared.Balance"]
        );
        assert!(
            changes.instance_changes.iter().all(|change| {
                !change.allow_deletes
                    && change
                        .instances
                        .iter()
                        .all(|instance| instance.settings_id != "outside-folder")
            }),
            "a named folder leaves the rest of its service alone"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_named_file_that_is_no_script_pushes_only_the_instance_it_makes() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("named-mapped-file");
        fs::create_dir_all(root.join("src/shared")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("src/shared/Balance.luau"), "return 1\n").unwrap();
        fs::write(root.join("src/shared/notes.txt"), "to do\n").unwrap();
        let sidecar = root.join("src/shared/Pad.meta.json");
        fs::write(
            &sidecar,
            r#"{"className": "Part", "properties": {"Anchored": true}}"#,
        )
        .unwrap();
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"tree":{"ReplicatedStorage":{"Shared":{"$path":"src/shared"}}}}"#,
        )
        .unwrap();
        let mut store = crate::rbx::model::source_only_settings_document(
            &root.join("empty"),
            "ReplicatedStorage",
        )
        .unwrap();
        store
            .instances
            .push(crate::settings::bytecode::SettingsBytecodeInstance::new(
                "outside-folder".to_string(),
                "Finds".to_string(),
                "Folder".to_string(),
                Some(0),
            ));
        store
            .write_file(&root.join("instances/ReplicatedStorage.renium"))
            .unwrap();
        let pushed = |path: PathBuf| {
            let mut args = PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root.clone(),
                    src_root: PathBuf::from("src"),
                },
                BridgeConnectionArgs::local(0.1),
            );
            args.changed_paths.push(path);
            collect_project_editor_changes(&args).unwrap().0
        };

        let described = pushed(sidecar);
        assert!(
            described
                .instance_changes
                .iter()
                .all(|change| change.mode == "upsertInstances" && !change.allow_deletes),
            "a metadata file does not reconcile its service"
        );
        let paths = described
            .instance_changes
            .iter()
            .flat_map(|change| &change.instances)
            .map(|instance| instance.path_segments.join("."))
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            ["ReplicatedStorage.Shared", "ReplicatedStorage.Shared.Pad"],
            "the instance the file describes, below its parent"
        );

        let carried = pushed(root.join("src/shared/notes.txt"));
        assert!(
            carried.instance_changes.is_empty() && carried.source_changes.is_empty(),
            "a file that makes no instance pushes nothing"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_named_sync_rule_source_is_pushed_as_the_whole_instance_it_makes() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("named-model-file");
        fs::create_dir_all(root.join("src/shared")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("src/shared/Balance.luau"), "return 1\n").unwrap();
        let model = root.join("src/shared/Map.model.json");
        fs::write(
            &model,
            r#"{"className": "Folder", "children": [{"name": "Pad", "className": "Part"}]}"#,
        )
        .unwrap();
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,
                "syncRules":[{"pattern":"**/*.model.json","use":"model-json","suffix":".model.json"}],
                "tree":{"ReplicatedStorage":{"Shared":{"$path":"src/shared"}}}}"#,
        )
        .unwrap();
        let mut store = crate::rbx::model::source_only_settings_document(
            &root.join("empty"),
            "ReplicatedStorage",
        )
        .unwrap();
        store
            .instances
            .push(crate::settings::bytecode::SettingsBytecodeInstance::new(
                "outside-folder".to_string(),
                "Finds".to_string(),
                "Folder".to_string(),
                Some(0),
            ));
        store
            .write_file(&root.join("instances/ReplicatedStorage.renium"))
            .unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.push(model);
        let (changes, _) = collect_project_editor_changes(&args).unwrap();
        assert!(
            changes
                .instance_changes
                .iter()
                .all(|change| change.mode == "upsertInstances" && !change.allow_deletes),
            "the file does not reconcile its service"
        );
        let paths = changes
            .instance_changes
            .iter()
            .flat_map(|change| &change.instances)
            .map(|instance| instance.path_segments.join("."))
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "ReplicatedStorage.Shared",
                "ReplicatedStorage.Shared.Map",
                "ReplicatedStorage.Shared.Map.Pad"
            ]
        );
        let whole = changes
            .whole_instances
            .iter()
            .map(|(service, path)| format!("{service}: {}", path.path_segments.join(".")))
            .collect::<Vec<_>>();
        assert_eq!(
            whole,
            ["ReplicatedStorage: ReplicatedStorage.Shared.Map"],
            "what Studio holds below Map beyond the file is looked up and removed"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_case_only_renamed_script_file_maps_to_its_instance() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("case-only-rename");
        fs::create_dir_all(root.join("src/server")).unwrap();
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"tree":{"ServerScriptService":{"$path":"src/server"}}}"#,
        )
        .unwrap();
        let original = root.join("src/server/Main.server.luau");
        fs::write(&original, "print(\"main\")\n").unwrap();
        crate::rbx::model::source_only_settings_document(
            &root.join("src/server"),
            "ServerScriptService",
        )
        .unwrap()
        .write_file(&root.join("instances/ServerScriptService.renium"))
        .unwrap();
        let renamed = root.join("src/server/main.server.luau");
        fs::rename(&original, &renamed).unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.push(renamed);
        let (changes, _) = collect_project_editor_changes(&args).unwrap();
        assert!(
            changes.instance_changes.is_empty(),
            "the renamed file created an instance: {:?}",
            changes
                .instance_changes
                .iter()
                .flat_map(|change| change.instances.iter().map(|i| i.path_segments.join(".")))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            changes
                .source_changes
                .iter()
                .map(|change| change.path_segments.join("."))
                .collect::<Vec<_>>(),
            ["ServerScriptService.Main"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_new_init_module_and_its_child_are_created_in_one_push() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("init-module-with-child");
        let service_dir = root.join("src/ReplicatedStorage");
        fs::create_dir_all(service_dir.join("Shared")).unwrap();
        fs::write(root.join("renium.project.jsonc"), br#"{"schemaVersion":1}"#).unwrap();
        fs::write(service_dir.join("Shared/Util.luau"), "return 1\n").unwrap();
        crate::rbx::model::source_only_settings_document(&service_dir, "ReplicatedStorage")
            .unwrap()
            .write_file(&root.join("instances/ReplicatedStorage.renium"))
            .unwrap();
        fs::create_dir_all(service_dir.join("Shared/Lib")).unwrap();
        fs::write(
            service_dir.join("Shared/Lib/init.luau"),
            "return { lib = true }\n",
        )
        .unwrap();
        fs::write(
            service_dir.join("Shared/Lib/Child.luau"),
            "return { child = true }\n",
        )
        .unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.push(service_dir.clone());
        let (changes, _) = collect_project_editor_changes(&args).unwrap();
        let replaced = changes
            .instance_changes
            .iter()
            .flat_map(|change| change.instances.iter())
            .filter(|instance| {
                instance.previous_class_name.is_some()
                    || !instance.previous_path_segments.is_empty()
            })
            .map(|instance| instance.path_segments.join("."))
            .collect::<Vec<_>>();
        assert!(
            changes
                .instance_changes
                .iter()
                .all(|change| change.mode != "replaceInstances")
                && replaced.is_empty(),
            "the init file replaced an instance this push creates: {replaced:?}"
        );
        let created = changes
            .instance_changes
            .iter()
            .flat_map(|change| change.instances.iter())
            .map(|instance| {
                format!(
                    "{} {}",
                    instance.path_segments.join("."),
                    instance.class_name
                )
            })
            .collect::<Vec<_>>();
        assert!(
            created.contains(&"ReplicatedStorage.Shared.Lib ModuleScript".to_string())
                && created.contains(&"ReplicatedStorage.Shared.Lib.Child ModuleScript".to_string())
                && !created.contains(&"ReplicatedStorage.Shared.Lib Folder".to_string()),
            "{created:?}"
        );
        let sources = changes
            .source_changes
            .iter()
            .map(|change| change.path_segments.join("."))
            .collect::<Vec<_>>();
        assert!(
            sources.contains(&"ReplicatedStorage.Shared.Lib".to_string())
                && sources.contains(&"ReplicatedStorage.Shared.Lib.Child".to_string()),
            "{sources:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_renamed_directory_deletes_the_old_scripts_and_creates_the_new_ones() {
        use crate::cli::ProjectSourceArgs;
        use crate::tests::support::{settings_document, settings_instance};

        let root = crate::tests::support::temp_dir("directory-rename-collection");
        let service_dir = root.join("src/ReplicatedStorage");
        fs::create_dir_all(service_dir.join("Shared")).unwrap();
        fs::write(root.join("renium.project.jsonc"), br#"{"schemaVersion":1}"#).unwrap();
        fs::write(
            service_dir.join("Shared/Util.luau"),
            "return { version = 1 }\n",
        )
        .unwrap();
        let mut util = settings_instance("editor:2", "Util", "ModuleScript", Some(1));
        util.properties.insert(
            "Source".into(),
            json!(crate::settings::EXTERNAL_SOURCE_MARKER),
        );
        settings_document(vec![
            settings_instance("editor:0", "ReplicatedStorage", "ReplicatedStorage", None),
            settings_instance("editor:1", "Shared", "Folder", Some(0)),
            util,
        ])
        .write_file(&root.join("instances/ReplicatedStorage.renium"))
        .unwrap();
        fs::rename(service_dir.join("Shared"), service_dir.join("Shared2")).unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: root.join("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.extend([
            PathBuf::from("src/ReplicatedStorage/Shared"),
            PathBuf::from("src/ReplicatedStorage/Shared/Util.luau"),
            PathBuf::from("src/ReplicatedStorage/Shared2/Util.luau"),
        ]);
        args.exact_target_ids = true;
        args.targets_extend_paths = true;
        args.verify_sources = true;
        args.yes = true;
        let (changes, _) = collect_project_editor_changes(&args).unwrap();
        let described = changes
            .instance_changes
            .iter()
            .flat_map(|change| {
                change.instances.iter().map(move |instance| {
                    format!(
                        "{} {} {} {}",
                        change.mode,
                        instance.path_segments.join("."),
                        instance.class_name,
                        instance.settings_id
                    )
                })
            })
            .collect::<Vec<_>>();
        let deleted = described
            .iter()
            .filter(|entry| entry.starts_with("deleteInstances "))
            .collect::<Vec<_>>();
        let created = described
            .iter()
            .filter(|entry| entry.starts_with("upsertInstances "))
            .collect::<Vec<_>>();
        assert!(
            deleted.len() == 1
                && deleted[0].contains(" ReplicatedStorage.Shared.Util ModuleScript "),
            "{described:?}"
        );
        assert!(
            created
                .iter()
                .any(|entry| entry.contains(" ReplicatedStorage.Shared2 Folder "))
                && created
                    .iter()
                    .any(|entry| entry.contains(" ReplicatedStorage.Shared2.Util ModuleScript ")),
            "{described:?}"
        );
        let ids = |entries: &[&String]| {
            entries
                .iter()
                .map(|entry| entry.rsplit(' ').next().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let deleted_ids = ids(&deleted);
        let created_ids = ids(&created);
        assert!(
            !created_ids.iter().any(|id| deleted_ids.contains(id)),
            "a created instance reuses a deleted identity: deleted {deleted_ids:?} created {created_ids:?}"
        );
        assert_eq!(
            changes
                .source_changes
                .iter()
                .map(|change| change.path_segments.join("."))
                .collect::<Vec<_>>(),
            ["ReplicatedStorage.Shared2.Util"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_mapped_script_metadata_is_published_before_later_deletion() {
        use crate::cli::ProjectSourceArgs;

        let root = crate::tests::support::temp_dir("mapped-source-identity-lifecycle");
        fs::create_dir_all(root.join("src/shared")).unwrap();
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"tree":{"ReplicatedStorage":{"$path":"src/shared"}}}"#,
        )
        .unwrap();
        let source = root.join("src/shared/Temporary.luau");
        fs::write(&source, "return 99\n").unwrap();
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths.push(source.clone());
        let store = root.join("instances/ReplicatedStorage.renium");
        let (created, _stage) = collect_project_editor_changes(&args).unwrap();
        assert!(
            !store.exists(),
            "collection must not publish before Studio succeeds"
        );
        let write = created
            .settings_writes
            .iter()
            .find(|write| write.path == store)
            .unwrap();
        let id = write
            .document
            .instances
            .iter()
            .find(|v| v.name == "Temporary")
            .unwrap()
            .settings_id
            .clone();
        let transaction = EditorSettingsTransaction::apply(&created).unwrap();
        transaction.commit();
        fs::remove_file(&source).unwrap();
        let (deleted, _) = collect_project_editor_changes(&args).unwrap();
        let removal = deleted
            .instance_changes
            .iter()
            .find(|v| v.mode == "deleteInstances")
            .unwrap();
        assert_eq!(removal.instances.len(), 1);
        assert_eq!(removal.instances[0].settings_id, id);
        assert_eq!(
            removal.instances[0].path_segments,
            ["ReplicatedStorage", "Temporary"]
        );
        let write = deleted
            .settings_writes
            .iter()
            .find(|write| write.path == store)
            .unwrap();
        assert!(
            !write
                .document
                .instances
                .iter()
                .any(|v| v.name == "Temporary")
        );
        assert!(
            SettingsBytecode::read_file(&store)
                .unwrap()
                .instances
                .iter()
                .any(|v| v.name == "Temporary")
        );
        fs::remove_dir_all(root).unwrap();
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
}

#[cfg(test)]
mod package_ownership_tests {
    use super::{EditorPackageTarget, PackageCreator, ensure_packages_owned};

    fn target(owned: Option<bool>, creator: Option<PackageCreator>) -> EditorPackageTarget {
        EditorPackageTarget {
            path_segments: vec![
                "ReplicatedStorage".into(),
                "TopbarPlus".into(),
                "Icon".into(),
            ],
            path_ordinals: vec![1, 1, 1],
            expected_version: 15,
            creator,
            owned,
        }
    }

    #[test]
    fn foreign_packages_are_refused_with_their_creator() {
        let group = PackageCreator {
            kind: "Group".into(),
            id: 4676369,
            name: "HD Admin".into(),
            asset: "Icon".into(),
        };
        let error = ensure_packages_owned(&[target(Some(false), Some(group.clone()))])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("ReplicatedStorage.TopbarPlus.Icon (package 'Icon' by group HD Admin)"),
            "{error}"
        );
        assert!(error.contains("Unlink from Package"), "{error}");
        assert!(ensure_packages_owned(&[target(Some(true), Some(group))]).is_ok());
        assert!(ensure_packages_owned(&[target(None, None)]).is_ok());
        assert!(ensure_packages_owned(&[]).is_ok());
    }
}

#[cfg(test)]
mod saved_field_review_tests {
    use super::*;
    use crate::cli::{
        ApplyEditorPropertyArgs, BridgeConnectionArgs, EditorMutationArgs, ProjectSourceArgs,
    };
    use crate::studio::bridge::routing_tests::{connect, edit_info, fixture};
    use std::sync::Mutex;
    use std::thread::{self, JoinHandle};
    use tungstenite::{Message, WebSocket, protocol::Role};

    fn reviewing_bridge(
        decision: &'static str,
    ) -> (BridgeServer, Arc<Mutex<Vec<Value>>>, JoinHandle<()>) {
        reviewing_bridge_with_offline(decision, decision)
    }

    fn reviewing_bridge_with_offline(
        decision: &'static str,
        offline: &'static str,
    ) -> (BridgeServer, Arc<Mutex<Vec<Value>>>, JoinHandle<()>) {
        let bridge = fixture();
        let stream = connect(&bridge, 0, edit_info("saved-field-review", 21));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let plugin = thread::spawn(move || {
            let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
            while let Ok(message) = socket.read() {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "getEditorMutationPackages" => json!({ "packages": [] }),
                    "requestEditorPushReview" => {
                        json!({ "required": true, "reviewId": "studio-1" })
                    }
                    "applyEditorChanges"
                        if decision == "write-fails"
                            && request["params"]["propertyChanges"]
                                .as_array()
                                .is_some_and(|rows| !rows.is_empty()) =>
                    {
                        json!({"ok":false,"errors":1,"error":"simulated imported field setter failure"})
                    }
                    "applyEditorChanges" if decision == "count" => {
                        let params = &request["params"];
                        let mut writes = Vec::new();
                        for row in params["propertyChanges"].as_array().into_iter().flatten() {
                            for (name, value) in row["properties"].as_object().into_iter().flatten()
                            {
                                writes.push(json!({"index": writes.len()+1,"className":row["className"],
                                    "pathSegments":row["pathSegments"],"pathOrdinals":row["pathOrdinals"],
                                    "name":name,"value":value}));
                            }
                        }
                        if params["verifyOnly"] == true {
                            // The fixture can only confirm fields already checked by the writer.
                            json!({"ok":true, "verifyOnly":true, "errors":0, "verified":0,
                                "verifyMismatches":writes.iter().map(|w| w["name"].clone()).collect::<Vec<_>>()})
                        } else {
                            json!({"ok":true,"propertyUpdated":writes.len(),"nativeRootWrites":writes})
                        }
                    }
                    "requestProtectedWriteReview" => json!({"required":true,"reviewId":"offline"}),
                    "applyEditorChanges" if request["params"]["verifyOnly"] == true => {
                        let count: usize = request["params"]["propertyChanges"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|row| row["properties"].as_object().map_or(0, Map::len))
                            .sum();
                        json!({"ok":true,"verifyOnly":true,"errors":0,"verified":count,"verifyMismatches":[]})
                    }
                    "getEditorPushReviewDecision" => {
                        json!({ "decided": true, "decision": if request["params"]["reviewId"] == "offline" {offline} else {decision} })
                    }
                    "rollbackEditorTransaction" => json!({"state":"rolledBack"}),
                    "commitEditorTransaction" if request["params"]["prepareOnly"] == true => {
                        json!({"state":"prepared","committed":false})
                    }
                    "commitEditorTransaction" => json!({"state":"committed","committed":true}),
                    _ => json!({}),
                };
                seen.lock().unwrap().push(request.clone());
                let answer = json!({ "id": request["id"], "ok": true, "result": result });
                if socket
                    .send(Message::Text(answer.to_string().into()))
                    .is_err()
                {
                    break;
                }
            }
        });
        (bridge, calls, plugin)
    }

    fn project_with_threshold_zero(tag: &str) -> PathBuf {
        let root = crate::tests::support::temp_dir(tag);
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".renium")).unwrap();
        fs::write(
            root.join(".renium").join("config.json"),
            r#"{"schemaVersion":1,"liveSync":{"changesThreshold":0}}"#,
        )
        .unwrap();
        root
    }

    fn saved_field_args(root: &Path, yes: bool) -> ApplyEditorPropertyArgs {
        ApplyEditorPropertyArgs {
            review_report: false,
            target: EditorMutationArgs {
                project: ProjectSourceArgs {
                    project_root: root.to_path_buf(),
                    src_root: PathBuf::from("src"),
                },
                bridge: BridgeConnectionArgs::local(0.1),
                service: "Workspace".into(),
                settings_id: None,
                class_name: "Workspace".into(),
                path_segments_json: r#"["Workspace"]"#.into(),
                path_ordinals_json: "[]".into(),
                override_packages: false,
            },
            scope: "property".into(),
            property: "StreamingMinRadius".into(),
            value_json: Some("64".into()),
            source_file: None,
            no_review: false,
            yes,
        }
    }

    #[test]
    fn protected_snapshot_counts_only_pending_import_fields() {
        let bridge = fixture();
        let row = json!({"kind":"property", "className":"SurfaceAppearance", "name":"TexturePack", "value":"a"});
        let mut current = row.clone();
        current["oldValueKnown"] = json!(true);
        current["oldValue"] = json!("a");
        let mut filtered = row.clone();
        filtered["name"] = json!("RBX_Ignored");
        let mut summary = Map::from_iter([("protectedWrites".into(), json!([row]))]);
        let pending = pending_protected_writes(
            &bridge,
            &mut summary,
            vec![row, current, filtered],
            3,
            PLACE_FILE.path,
        );
        assert_eq!(pending.supplemental, 1);
        assert_eq!(pending.writes.len(), 2);
    }

    #[test]
    fn unexpected_slow_import_is_refused_before_review_and_transaction() {
        let root = project_with_threshold_zero("slow-import-review");
        let (bridge, calls, plugin) = reviewing_bridge("skip");
        let mut import = super::sync_tests::replacement_fixture();
        import.fallback_reason = Some(ServiceReaderFallback::DiscoveryFailed(
            "reader mismatch".into(),
        ));
        let row = EditorPropertyChange {
            service: "ReplicatedStorage".into(),
            class_name: "SurfaceAppearance".into(),
            path_segments: vec!["ReplicatedStorage".into(), "Surface".into()],
            path_ordinals: vec![1, 1],
            properties: Map::from_iter([("TexturePack".into(), json!("rbxassetid://123"))]),
            ..Default::default()
        };
        let args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: "src".into(),
            },
            BridgeConnectionArgs::local(0.1),
        );
        let mut properties = vec![row; MAX_INDIVIDUAL_IMPORT_WRITES + 1];
        properties.push(EditorPropertyChange {
            service: "Workspace".into(),
            class_name: "MaterialVariant".into(),
            path_segments: vec!["Workspace".into(), "Variant".into()],
            path_ordinals: vec![1, 1],
            properties: Map::from_iter([("TexturePack".into(), json!("rbxassetid://456"))]),
            ..Default::default()
        });
        let error = push_with_root_availability(
            args,
            &bridge,
            EditorChangeSet {
                property_changes: properties,
                ..Default::default()
            },
            CollectedPushOptions {
                started: Instant::now(),
                projection: None,
                prepared_binary_import: Some(import),
                guard: None,
                validate_project: None,
                finalize_settings: None,
            },
            |_| Ok(true),
            PLACE_FILE,
        )
        .unwrap_err();
        assert!(error.to_string().contains("reader mismatch"), "{error:#}");
        assert!(
            !error.to_string().contains("--yes"),
            "cloud place: {error:#}"
        );
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call["method"].clone())
                .collect::<Vec<_>>(),
            [json!("getEditorMutationPackages")]
        );
        drop(bridge);
        plugin.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn push_reviewed_offline_import(
        decision: &'static str,
        snapshot_fails: bool,
        count: usize,
        after_commit_fails: bool,
    ) {
        let root = project_with_threshold_zero("review-offline-before-import");
        let path = root.join("local.rbxlx");
        fs::write(&path, r#"<roblox version="4"></roblox>"#).unwrap();
        crate::editor::native_roots::RECORDED_WRITES.replace(Some(Vec::new()));
        let _native = crate::system::files::OnDrop::new(|| {
            crate::editor::native_roots::RECORDED_WRITES.replace(None);
        });
        let (bridge, calls, plugin) = reviewing_bridge_with_offline("apply", decision);
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: "src".into(),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.review_report = count > MAX_INDIVIDUAL_IMPORT_WRITES;
        args.mutation_started = Some(Arc::clone(&started));
        let mut import = super::sync_tests::replacement_fixture();
        import.fallback_reason = Some(ServiceReaderFallback::DiscoveryFailed(
            "reader unavailable".into(),
        ));
        let service = "ReplicatedStorage";
        let mut properties = (0..count)
            .map(|i| EditorPropertyChange {
                service: service.into(),
                settings_id: Some(format!("part{i}")),
                path_segments: vec![service.into(), format!("Surface{i}")],
                class_name: "SurfaceAppearance".into(),
                properties: Map::from_iter([("TexturePack".into(), json!(format!("value{i}")))]),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        properties.push(EditorPropertyChange {
            service: "Workspace".into(),
            path_segments: vec!["Workspace".into(), "OutsideImport".into()],
            class_name: "MaterialVariant".into(),
            properties: Map::from_iter([("TexturePack".into(), json!("requested"))]),
            ..Default::default()
        });
        let settings_path = root.join("publication.renium");
        fs::write(&settings_path, b"previous settings").unwrap();
        let document = SettingsBytecode {
            version: crate::settings::bytecode::SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        };
        let expected_settings =
            crate::settings::bytecode::encode_settings_bytecode(&document).unwrap();
        let changes = EditorChangeSet {
            settings_writes: vec![EditorSettingsWrite {
                expected_hash: settings_file_hash(&settings_path).unwrap(),
                path: settings_path.clone(),
                document: Arc::new(document),
                encoded: None,
            }],
            property_changes: properties,
            instance_changes: vec![EditorInstanceChange {
                service: service.into(),
                mode: "reconcileService".into(),
                allow_deletes: true,
                instances: Vec::new(),
                preserve_instances: Vec::new(),
            }],
            ..Default::default()
        };
        let result = push_with_root_availability(
            args,
            &bridge,
            changes,
            CollectedPushOptions {
                started: Instant::now(),
                projection: None,
                prepared_binary_import: Some(import),
                guard: None,
                validate_project: None,
                finalize_settings: None,
            },
            |_| Ok(true),
            PlaceFileAccess {
                path: &|_| Some(path.clone()),
                pending: &|bridge, summary, rows, suffix, cache| {
                    if started.load(std::sync::atomic::Ordering::Acquire) && !snapshot_fails {
                        // The first protected value now agrees, but the supplemental rows still need the approved snapshot.
                        assert_eq!(rows.len(), 1);
                        PendingProtectedWrites {
                            writes: Vec::new(),
                            supplemental: 0,
                        }
                    } else {
                        pending_protected_writes_cached(
                            bridge,
                            summary,
                            rows,
                            suffix,
                            &|_| Some(path.clone()),
                            cache,
                        )
                    }
                },
                apply: &|_, _, rows, commit| {
                    assert!(count > 0, "zero writes must not restart Studio");
                    assert_eq!(rows.len(), count + usize::from(snapshot_fails));
                    if snapshot_fails {
                        bail!("snapshot writer unavailable")
                    }
                    commit()?;
                    if after_commit_fails {
                        bail!("Studio did not reconnect in 90 s");
                    }
                    Ok(json!({"successor":"reopened"}))
                },
            },
        );
        let calls = calls.lock().unwrap().clone();
        let methods: Vec<_> = calls
            .iter()
            .map(|r| r["method"].as_str().unwrap())
            .collect();
        let review = methods
            .iter()
            .position(|m| *m == "requestEditorPushReview")
            .unwrap();
        let panel = methods
            .iter()
            .position(|m| *m == "requestProtectedWriteReview")
            .unwrap();
        assert!(review < panel);
        assert_eq!(calls[review]["params"]["report"], false);
        assert_eq!(
            calls[panel]["params"]["rows"].as_array().unwrap().len(),
            count + 1
        );
        assert_eq!(
            methods
                .iter()
                .filter(|m| **m == "requestProtectedWriteReview")
                .count(),
            1
        );
        if decision == "skip" {
            assert_eq!(result.unwrap()["skippedByReview"], true);
            assert!(!methods.contains(&"beginEditorTransaction"));
            assert!(!started.load(std::sync::atomic::Ordering::Acquire));
        } else {
            assert!(
                methods
                    .iter()
                    .position(|m| *m == "beginEditorTransaction")
                    .unwrap()
                    > panel
            );
            if snapshot_fails {
                let error = result.unwrap_err();
                assert!(
                    format!("{error:#}").contains("snapshot writer unavailable"),
                    "{error:#}"
                );
                assert!(methods.contains(&"rollbackEditorTransaction"));
                assert!(!started.load(std::sync::atomic::Ordering::Acquire));
            } else {
                let summary = result.unwrap();
                assert_eq!(fs::read(&settings_path).unwrap(), expected_settings);
                assert_offline_result(&summary, &calls, count, after_commit_fails);
            }
        }
        drop(bridge);
        plugin.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn assert_offline_result(
        summary: &Map<String, Value>,
        calls: &[Value],
        count: usize,
        after_commit_fails: bool,
    ) {
        if count == 0 {
            assert!(!summary.contains_key("protectedOfflineApply"));
            assert!(!calls.iter().any(|r| r["params"]["prepareOnly"] == true));
        } else if after_commit_fails {
            assert_eq!(summary["protectedOfflineApply"]["recoveryPending"], true);
            assert!(
                summary["protectedOfflineApply"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("90 s")
            );
            assert!(summary["protectedOfflineApply"]["nextStep"].is_string());
            assert_eq!(summary["verified"], false);
            assert!(
                !calls
                    .iter()
                    .any(|r| r["method"] == "rollbackEditorTransaction")
            );
        } else {
            assert_eq!(summary["supplementalImportWrites"], 0);
            assert_eq!(summary["supplementalImportOfflineWrites"], count);
            assert_eq!(summary["protectedOfflineApply"]["successor"], "reopened");
        }
    }

    // On macOS a push resolves its native targets in a running Studio.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn offline_approval_does_not_restart_studio_when_nothing_remains_to_write() {
        push_reviewed_offline_import("apply", false, 0, false);
    }

    #[test]
    fn declining_additional_offline_rows_returns_review_skipped() {
        let (bridge, _, plugin) = reviewing_bridge("skip");
        let args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: PathBuf::from("."),
                src_root: "src".into(),
            },
            BridgeConnectionArgs::local(0.1),
        );
        let result = prepare_protected_writes(
            &args,
            &bridge,
            PendingProtectedWrites {
                writes: vec![json!({"name":"newly-reported","value":true})],
                supplemental: 0,
            },
            true,
            &HashSet::new(),
            false,
        );
        assert!(
            result
                .err()
                .unwrap()
                .is::<crate::automation::reconcile::SkippedPushReview>()
        );
        drop(bridge);
        plugin.join().unwrap();
    }

    // On macOS a push resolves its native targets in a running Studio.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn offline_reconnect_failure_keeps_committed_settings_and_receipt() {
        push_reviewed_offline_import("apply", false, 33, true);
    }

    #[test]
    fn declining_reviewed_offline_import_leaves_studio_untouched() {
        push_reviewed_offline_import("skip", false, 33, false);
    }

    // On macOS a push resolves its native targets in a running Studio.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn declining_optional_offline_writes_commits_the_rest_and_keeps_pending() {
        let root = project_with_threshold_zero("decline-optional-offline");
        let path = root.join("local.rbxlx");
        fs::write(&path, r#"<roblox version="4"></roblox>"#).unwrap();
        let (bridge, calls, plugin) = reviewing_bridge_with_offline("apply", "skip");
        let args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: "src".into(),
            },
            BridgeConnectionArgs::local(0.1),
        );
        let properties = [
            ("Part", "Transparency", json!(0.5)),
            ("MaterialVariant", "TexturePack", json!("requested")),
        ]
        .into_iter()
        .map(|(class, name, value)| EditorPropertyChange {
            service: "Workspace".into(),
            path_segments: vec!["Workspace".into(), class.into()],
            class_name: class.into(),
            properties: Map::from_iter([(name.into(), value)]),
            ..Default::default()
        })
        .collect();
        let parses = std::cell::Cell::new(0);
        let result = push_with_root_availability(
            args,
            &bridge,
            EditorChangeSet {
                property_changes: properties,
                ..Default::default()
            },
            CollectedPushOptions {
                started: Instant::now(),
                projection: None,
                prepared_binary_import: None,
                guard: None,
                validate_project: None,
                finalize_settings: None,
            },
            |_| Ok(true),
            PlaceFileAccess {
                path: &|_| Some(path.clone()),
                pending: &|bridge, summary, rows, suffix, cache| {
                    let pending = pending_protected_writes_cached(
                        bridge,
                        summary,
                        rows,
                        suffix,
                        &|_| Some(path.clone()),
                        cache,
                    );
                    parses.set(cache.reads);
                    pending
                },
                apply: &|_, _, _, _| panic!("declined offline write was applied"),
            },
        )
        .unwrap();
        assert_eq!(result["protectedPending"], 1);
        assert_eq!(
            parses.get(),
            1,
            "preflight and transaction must share the parsed place"
        );
        assert!(!result.contains_key("skippedByReview"));
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|r| r["method"] == "requestProtectedWriteReview")
                .count(),
            1
        );
        assert!(calls.iter().any(|r| {
            r["method"] == "applyEditorChanges"
                && r["params"]["propertyChanges"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|r| r["properties"]["Transparency"] == 0.5))
        }));
        assert!(
            calls
                .iter()
                .any(|r| r["method"] == "commitEditorTransaction")
        );
        assert!(
            !calls
                .iter()
                .any(|r| r["method"] == "rollbackEditorTransaction")
        );
        drop(calls);
        drop(bridge);
        plugin.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    // On macOS a push resolves its native targets in a running Studio.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn offline_approval_carries_import_rows_and_snapshot_failure_rolls_back_the_push() {
        push_reviewed_offline_import("apply", false, 33, false);
        push_reviewed_offline_import("apply", true, 33, false);
    }

    // On macOS a push resolves its native targets in a running Studio.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn opted_in_push_counts_successful_individual_writes_and_keeps_both_batch_proofs() {
        push_individual_fields(false);
    }

    // On macOS a push resolves its native targets in a running Studio.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn deferred_field_failure_preserves_the_committed_push_and_lists_pending_fields() {
        push_individual_fields(true);
    }

    fn push_individual_fields(fail: bool) {
        use crate::editor::native_roots::RECORDED_WRITES;
        let root = project_with_threshold_zero("count-individual-writes");
        let _cleanup = crate::system::files::OnDrop::new(|| {
            RECORDED_WRITES.replace(None);
            let _ = fs::remove_dir_all(&root);
        });
        RECORDED_WRITES.replace(Some(Vec::new()));
        let (bridge, calls, plugin) = reviewing_bridge(if fail { "write-fails" } else { "count" });
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: "src".into(),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.slow_import = true;
        args.yes = true;
        let mut import = super::sync_tests::replacement_fixture();
        import.fallback_reason = Some(ServiceReaderFallback::DiscoveryFailed("no reader".into()));
        let service = "ReplicatedStorage";
        let properties = (0..33)
            .map(|i| EditorPropertyChange {
                service: service.into(),
                settings_id: Some(format!("part{i}")),
                class_name: "SurfaceAppearance".into(),
                path_ordinals: vec![1, 1],
                path_segments: vec![service.into(), format!("Surface{i}")],
                properties: Map::from_iter([(
                    "TexturePack".into(),
                    json!(format!("rbxassetid://{}", i + 1)),
                )]),
                ..Default::default()
            })
            .collect();
        let changes = EditorChangeSet {
            property_changes: properties,
            instance_changes: vec![EditorInstanceChange {
                service: service.into(),
                mode: "reconcileService".into(),
                allow_deletes: true,
                instances: vec![],
                preserve_instances: vec![],
            }],
            ..Default::default()
        };
        let summary = push_with_root_availability(
            args,
            &bridge,
            changes,
            CollectedPushOptions {
                started: Instant::now(),
                projection: None,
                prepared_binary_import: Some(import),
                guard: None,
                validate_project: None,
                finalize_settings: None,
            },
            |_| Ok(true),
            PLACE_FILE,
        )
        .unwrap();
        if fail {
            assert_eq!(summary["supplementalImportPendingWrites"], 33);
            assert_eq!(
                summary["supplementalImportPendingFields"]
                    .as_array()
                    .unwrap()
                    .len(),
                33
            );
            assert_eq!(
                summary["supplementalImportPendingFields"][0]["properties"]["TexturePack"],
                "rbxassetid://1"
            );
            assert_eq!(
                summary["unsupportedProperties"].as_array().unwrap().len(),
                33
            );
            assert!(!summary.contains_key("verifiedPushProof"));
            let methods = calls
                .lock()
                .unwrap()
                .iter()
                .map(|r| r["method"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            let commit = methods
                .iter()
                .position(|m| m == "commitEditorTransaction")
                .unwrap();
            let rollback = methods
                .iter()
                .position(|m| m == "rollbackEditorTransaction")
                .unwrap();
            assert!(commit < rollback);
            assert_eq!(
                methods
                    .iter()
                    .filter(|m| *m == "commitEditorTransaction")
                    .count(),
                1
            );
            drop(bridge);
            plugin.join().unwrap();
            return;
        }
        assert_eq!(summary["supplementalImportWrites"], 33);
        assert!(summary.get("supplementalImportOfflineWrites").is_none());
        let writes = RECORDED_WRITES.replace(Some(vec![])).unwrap();
        assert_eq!(writes.len(), 33);
        for (i, w) in writes.iter().enumerate() {
            assert_eq!(
                *w,
                json!([
                    [service, format!("Surface{i}")],
                    "TexturePack",
                    format!("rbxassetid://{}", i + 1)
                ])
            );
        }
        let row = |class: &str, name: &str| EditorPropertyChange {
            service: class.into(),
            class_name: class.into(),
            path_segments: vec![class.into()],
            path_ordinals: vec![1],
            properties: Map::from_iter([(name.into(), json!(true))]),
            ..Default::default()
        };
        let changes = EditorChangeSet {
            property_changes: vec![
                row("MaterialService", "Use2022Materials"),
                row("Lighting", "PrioritizeLightingQuality"),
            ],
            ..Default::default()
        };
        let batch = send_editor_change_batches(&bridge, &changes, None, Some("proof")).unwrap();
        assert_eq!(batch.individual_dispatches, 2);
        assert_eq!(
            batch.summary["fieldVerifiedServices"],
            json!(["Lighting", "MaterialService"])
        );
        assert_eq!(
            RECORDED_WRITES.take().unwrap(),
            [
                json!([["MaterialService"], "Use2022Materials", true]),
                json!([["Lighting"], "PrioritizeLightingQuality", true])
            ]
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|r| r["params"]["verifyOnly"] == true)
        );
        drop(bridge);
        plugin.join().unwrap();
    }

    #[test]
    fn reviews_that_cannot_reach_the_cap_hold_before_reader_selection_or_encoding() {
        use crate::studio::native::import::SELECTION_CALLS;
        for (disabled, count) in [(true, 33), (false, MAX_INDIVIDUAL_IMPORT_WRITES)] {
            let selections = SELECTION_CALLS.get();
            let root = project_with_threshold_zero("review-before-import-build");
            let (bridge, calls, plugin) = reviewing_bridge("skip");
            let mut args = PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root.clone(),
                    src_root: "src".into(),
                },
                BridgeConnectionArgs::local(0.1),
            );
            args.disable_service_reader = disabled;
            args.review_report = true;
            let service = "ReplicatedStorage";
            let changes = EditorChangeSet {
                instance_changes: vec![EditorInstanceChange {
                    mode: "reconcileService".into(),
                    service: service.into(),
                    allow_deletes: true,
                    instances: vec![],
                    preserve_instances: vec![],
                }],
                property_changes: vec![
                    EditorPropertyChange {
                        service: service.into(),
                        class_name: "SurfaceAppearance".into(),
                        path_segments: vec![service.into(), "Folder".into()],
                        properties: Map::from_iter([("TexturePack".into(), json!("uri"))]),
                        ..Default::default()
                    };
                    count
                ],
                ..Default::default()
            };
            // There is no store to encode. The review must still return the hold.
            let result = push_with_reader_platform(
                args,
                &bridge,
                changes,
                CollectedPushOptions {
                    started: Instant::now(),
                    projection: None,
                    prepared_binary_import: None,
                    guard: None,
                    validate_project: None,
                    finalize_settings: None,
                },
                |_| panic!("unexpected individual write preparation"),
                PLACE_FILE,
                true,
            )
            .unwrap();
            assert_eq!(result["heldForReview"], true);
            assert_eq!(SELECTION_CALLS.get(), selections);
            let methods = calls
                .lock()
                .unwrap()
                .iter()
                .map(|r| r["method"].clone())
                .collect::<Vec<_>>();
            assert_eq!(
                methods,
                [
                    json!("getEditorMutationPackages"),
                    json!("requestEditorPushReview")
                ]
            );
            drop(bridge);
            plugin.join().unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn only_a_shown_review_refreshes_a_reused_additive_reply() {
        use crate::studio::native::import::{READER_AVAILABLE, SELECTION_CALLS};
        READER_AVAILABLE.set(true);
        let _reader = crate::system::files::OnDrop::new(|| READER_AVAILABLE.set(false));
        for (whole, shown) in [(false, false), (false, true), (true, true)] {
            let selections = SELECTION_CALLS.get();
            let root = project_with_threshold_zero("additive-review-generation");
            crate::project::config::try_load_project(None, Some(&root)).unwrap();
            let store = service_settings_path(&root.join("src/Workspace"));
            fs::create_dir_all(store.parent().unwrap()).unwrap();
            // Stop the unchanged-reply case at decoding, before any mutation.
            fs::write(&store, b"invalid store fixture").unwrap();
            let bridge = fixture();
            let stream = connect(&bridge, 0, edit_info("additive-review", 21));
            let plugin = thread::spawn(move || {
                let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
                let mut reviewed = false;
                let mut generations = 0;
                while let Ok(Message::Text(text)) = socket.read() {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let result = match request["method"].as_str().unwrap() {
                        "getEditorMutationPackages" => json!({"packages":[]}),
                        "requestEditorPushReview" => {
                            json!({"required":shown,"reviewId":"changing"})
                        }
                        "getEditorPushReviewDecision" => {
                            reviewed = true;
                            json!({"decided":true,"decision":"apply"})
                        }
                        "getEditorServiceChangeGenerations" => {
                            generations += 1;
                            assert_eq!(reviewed, whole || generations == 2);
                            if reviewed {
                                // Stop at the new read, before any build or mutation.
                                socket.send(Message::Text(json!({"id":request["id"],"ok":false,"error":"fresh generation unavailable"}).to_string().into())).unwrap();
                                continue;
                            }
                            json!({"generations":{"Workspace":7},"hasPackageLinks":{"Workspace":false},"absentRoots":{"Workspace":["New"]}})
                        }
                        other => {
                            panic!("unexpected request before a fresh existence reply: {other}")
                        }
                    };
                    socket
                        .send(Message::Text(
                            json!({"id":request["id"],"ok":true,"result":result})
                                .to_string()
                                .into(),
                        ))
                        .unwrap();
                }
                assert_eq!(generations, if shown && !whole { 2 } else { 1 });
            });
            let args = PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root.clone(),
                    src_root: "src".into(),
                },
                BridgeConnectionArgs::local(0.1),
            );
            let changes = EditorChangeSet {
                instance_changes: vec![EditorInstanceChange {
                    service: "Workspace".into(),
                    mode: if whole {
                        "reconcileService"
                    } else {
                        "upsertInstances"
                    }
                    .into(),
                    allow_deletes: whole,
                    instances: vec![EditorInstanceDescriptor {
                        settings_id: "new".into(),
                        class_name: "Model".into(),
                        path_segments: vec!["Workspace".into(), "New".into()],
                        path_ordinals: vec![1, 1],
                        ..Default::default()
                    }],
                    preserve_instances: vec![],
                }],
                property_changes: vec![
                    EditorPropertyChange {
                        service: "Workspace".into(),
                        class_name: "SurfaceAppearance".into(),
                        path_segments: vec!["Workspace".into(), "New".into()],
                        properties: Map::from_iter([("TexturePack".into(), json!("uri"))]),
                        ..Default::default()
                    };
                    MAX_INDIVIDUAL_IMPORT_WRITES + 1
                ],
                ..Default::default()
            };
            let result = push_with_reader_platform(
                args,
                &bridge,
                changes,
                CollectedPushOptions {
                    started: Instant::now(),
                    projection: None,
                    prepared_binary_import: None,
                    guard: None,
                    validate_project: None,
                    finalize_settings: None,
                },
                |_| panic!("unexpected preparation"),
                PLACE_FILE,
                true,
            );
            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains(if shown && !whole {
                    "fresh generation unavailable"
                } else {
                    "Failed to decode"
                }),
                "{error}"
            );
            assert_eq!(
                SELECTION_CALLS.get(),
                selections + 1,
                "reader selection must not repeat"
            );
            drop(bridge);
            plugin.join().unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn a_push_checks_saved_field_ownership_before_it_starts_a_transaction() {
        let root = project_with_threshold_zero("saved-field-origin");
        let (bridge, calls, plugin) = reviewing_bridge("apply");
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.clone(),
                src_root: "src".into(),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.yes = true;
        args.saved_fields = Some(Arc::new(crate::editor::types::PushSavedFields {
            project: root.clone(),
            runtime: "different-runtime".into(),
            fields: vec![],
        }));
        let error = push_editor_changes_with_collected(
            args,
            &bridge,
            EditorChangeSet::default(),
            CollectedPushOptions {
                started: Instant::now(),
                projection: None,
                prepared_binary_import: Some(super::sync_tests::replacement_fixture()),
                guard: None,
                validate_project: None,
                finalize_settings: None,
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("different project or Studio runtime"),
            "{error:#}"
        );
        assert!(calls.lock().unwrap().is_empty());
        drop(bridge);
        plugin.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_fields_from_a_skipped_review_cannot_reach_another_place() {
        let a = project_with_threshold_zero("saved-fields-place-a");
        let b = project_with_threshold_zero("saved-fields-place-b");
        let args_for = |root: &Path| {
            PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root.into(),
                    src_root: "src".into(),
                },
                BridgeConnectionArgs::local(0.1),
            )
        };
        let field = crate::settings::equivalence::SavedFieldDifference {
            class_name: "Lighting".into(),
            property: "Technology".into(),
            path_segments: vec!["Lighting".into()],
            ambiguous: false,
            value: json!("Compatibility"),
        };
        let batch = crate::editor::types::PushSavedFields {
            project: a.clone(),
            runtime: "saved-field-review".into(),
            fields: vec![field],
        };
        let mut args_a = args_for(&a);
        args_a.saved_fields = Some(Arc::new(batch.clone()));
        let (bridge, calls, plugin) = reviewing_bridge("skip");
        let summary = push_editor_changes_with_collected(
            args_a,
            &bridge,
            EditorChangeSet {
                property_changes: vec![EditorPropertyChange {
                    class_name: "Folder".into(),
                    path_segments: vec!["Workspace".into(), "Folder".into()],
                    properties: Map::from_iter([("Archivable".into(), json!(true))]),
                    ..Default::default()
                }],
                ..Default::default()
            },
            CollectedPushOptions {
                started: Instant::now(),
                projection: None,
                prepared_binary_import: None,
                guard: None,
                validate_project: None,
                finalize_settings: None,
            },
        )
        .unwrap();
        assert_eq!(summary["skippedByReview"], true);
        let calls_before = calls.lock().unwrap().len();
        assert!(calls_before > 0);
        let args_b = args_for(&b);
        let mut summary = Map::new();
        assert!(args_b.saved_fields.is_none());
        assert!(batch.validate(&a, "later-runtime").is_err());
        assert!(
            apply_saved_field_differences(&batch, &b, &bridge, &mut summary, None)
                .unwrap_err()
                .to_string()
                .contains("different project or Studio runtime")
        );
        assert_eq!(calls.lock().unwrap().len(), calls_before);
        drop(bridge);
        plugin.join().unwrap();
        for project in [a, b] {
            fs::remove_dir_all(project).unwrap();
        }
    }

    #[test]
    fn compound_commands_accept_the_opt_in_only_with_their_push() {
        use crate::cli::{Cli, Commands};
        use clap::{FromArgMatches, Parser};
        for (command, args, push) in [
            ("imp", vec!["script.luau"], "--push"),
            ("rev", vec!["--sync", "latest"], "--apply-studio"),
            ("bs", vec!["Workspace", "-p", "Name=target"], "--push"),
        ] {
            let argv = ["rbx", command]
                .into_iter()
                .chain(args.iter().copied())
                .collect::<Vec<_>>();
            assert!(Cli::try_parse_from(argv.iter().copied().chain(["--slow-import"])).is_err());
            let argv = argv
                .into_iter()
                .chain([push, "--slow-import"])
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            let matches = crate::cli::command().try_get_matches_from(&argv).unwrap();
            let parsed = Cli::from_arg_matches(&matches).unwrap();
            assert!(match parsed.command {
                Commands::ImportPath(args) => args.slow_import,
                Commands::EditorRevert(args) => args.slow_import,
                Commands::BytecodeSetProperty(args) => args.slow_import,
                _ => unreachable!(),
            });
        }
        let argv = ["rbx", "rv", "apply"];
        let matches = crate::cli::command().try_get_matches_from(argv).unwrap();
        assert!(matches!(
            Cli::from_arg_matches(&matches).unwrap().command,
            Commands::EditorReviewDecision(_)
        ));
        assert!(
            crate::cli::command()
                .try_get_matches_from([
                    "rbx",
                    "rv",
                    "--sync",
                    "latest",
                    "--apply-studio",
                    "--slow-import"
                ])
                .is_err()
        );
    }

    #[test]
    fn push_negotiates_import_options_and_sends_once_even_on_capability_timeout() {
        let root = project_with_threshold_zero("push-capabilities");
        for (slow, disable, support, ack, error_text) in [
            (
                true,
                false,
                Some(false),
                false,
                Some("did not advertise support"),
            ),
            (
                false,
                true,
                Some(false),
                false,
                Some("did not advertise support for RENIUM_NATIVE_SERVICE_IMPORT=0"),
            ),
            (true, true, None, false, Some("capability check timed out")),
            (true, false, None, true, None),
            (false, true, Some(true), false, None),
        ] {
            let mut args = PushEditorChangesArgs::new(
                ProjectSourceArgs {
                    project_root: root.clone(),
                    src_root: "src".into(),
                },
                BridgeConnectionArgs::local(0.1),
            );
            args.slow_import = slow;
            args.disable_service_reader = disable;
            let mut calls = Vec::new();
            let result = push_editor_changes_via(args, |op, project, params, _| {
                calls.push(op);
                if op == op::CAP {
                    assert!(project.is_none());
                    return support
                        .map(|s| json!({"slowImport":s}))
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut).into());
                }
                assert_eq!(project, Some(root.as_path()));
                assert_eq!(params["slowImport"] == true, slow);
                assert_eq!(params["disableServiceReader"] == true, disable);
                assert_eq!(params["confirmImportOptions"], true);
                Ok(json!({"unchanged":true, "confirmedImportOptions":ack}))
            })
            .unwrap();
            assert_eq!(calls, [op::CAP, op::PUSH]);
            assert_eq!(result["unchanged"], true);
            assert!(result.get("confirmedImportOptions").is_none());
            match error_text {
                Some(text) => assert!(
                    result["importOptionWarning"]
                        .as_str()
                        .unwrap()
                        .contains(text)
                ),
                None => assert!(result.get("importOptionWarning").is_none()),
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_saved_fields_require_the_originating_place_and_its_connected_successor() {
        let root = project_with_threshold_zero("saved-fields-reopen");
        let (bridge, calls, plugin) = reviewing_bridge("skip");
        let saved = crate::editor::types::PushSavedFields {
            project: root.clone(),
            runtime: "previous".into(),
            fields: vec![crate::settings::equivalence::SavedFieldDifference {
                path_segments: vec!["A".into()],
                property: "P".into(),
                class_name: "Folder".into(),
                ambiguous: true,
                value: json!(0),
            }],
        };
        let receipt = json!({"ok":true,"previousRuntimeId":"previous","reopenedRuntimeId":"saved-field-review"});
        let mut summary = Map::new();
        let busy = bridge
            .try_acquire_runtime_gate("saved-field-review", "test")
            .unwrap();
        assert!(
            apply_saved_field_differences(&saved, &root, &bridge, &mut summary, Some(&receipt))
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        drop(busy);
        apply_saved_field_differences(&saved, &root, &bridge, &mut summary, Some(&receipt))
            .unwrap();
        assert!(summary.contains_key("unsupportedProperties"));
        let mut writable = saved.clone();
        writable.fields[0].ambiguous = false;
        let mut seen = Vec::new();
        apply_saved_field_writes(
            &writable,
            &root,
            &bridge,
            &mut summary,
            Some(&receipt),
            |runtime, field| {
                seen.push((runtime.to_owned(), field.label()));
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(
            seen,
            [("saved-field-review".to_owned(), writable.fields[0].label())]
        );
        assert_eq!(summary["savedFieldsApplied"], 1);
        for (key, wrong) in [
            ("previousRuntimeId", "another-project-runtime"),
            ("reopenedRuntimeId", "disconnected"),
        ] {
            let mut bad = receipt.clone();
            bad[key] = json!(wrong);
            assert!(
                apply_saved_field_differences(&saved, &root, &bridge, &mut Map::new(), Some(&bad))
                    .is_err()
            );
        }
        assert!(calls.lock().unwrap().is_empty());
        drop(bridge);
        plugin.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_saved_field_edit_is_reviewed_before_the_native_write() {
        let root = project_with_threshold_zero("saved-field-review");
        for (case, yes, decision) in [
            ("default", false, "apply"),
            ("yes", true, "apply"),
            ("skip", false, "skip"),
        ] {
            let (bridge, calls, plugin) = reviewing_bridge(decision);
            let result =
                apply_editor_property_with_warm_bridge(saved_field_args(&root, yes), &bridge);
            let calls = calls.lock().unwrap().clone();
            let methods = calls
                .iter()
                .map(|call| call["method"].as_str().unwrap())
                .collect::<Vec<_>>();
            if case == "skip" {
                let summary = result.unwrap();
                assert_eq!(summary["skippedByReview"], json!(true), "{case}");
                assert_eq!(direct_editor_summary(&summary)["changed"], json!(false));
            } else {
                let error = format!("{:#}", result.unwrap_err());
                assert!(
                    error.contains("native property writer could not set it"),
                    "{case}: {error}"
                );
            }
            if case == "yes" {
                assert!(!methods.contains(&"requestEditorPushReview"), "{methods:?}");
            } else {
                assert_eq!(
                    methods.first(),
                    Some(&"requestEditorPushReview"),
                    "{case}: {methods:?}"
                );
                assert_eq!(calls[0]["params"]["changeCount"], json!(1), "{case}");
                assert_eq!(calls[0]["params"]["changesThreshold"], json!(0), "{case}");
            }
            drop(bridge);
            plugin.join().unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod named_script_tests {
    use super::*;
    use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs};

    #[test]
    fn a_push_takes_a_script_by_the_name_it_alone_goes_by() {
        let root = crate::tests::support::temp_dir("push-names");
        for path in [
            "src/Server/Honk.server.luau",
            "src/A/Util.luau",
            "src/B/Util.luau",
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "return 1").unwrap();
        }
        let named = |paths: &[&str]| {
            let project = ProjectSourceArgs {
                project_root: root.clone(),
                src_root: PathBuf::from("src"),
            };
            let mut args =
                PushEditorChangesArgs::new(project, BridgeConnectionArgs::default_connection());
            args.paths = paths.iter().map(PathBuf::from).collect();
            name_script_paths(&mut args, &root).map(|()| (only_named_scripts(&args), args.paths))
        };
        let honk = root.join("src").join("Server").join("Honk.server.luau");
        let util = PathBuf::from("src/A/Util.luau");
        assert_eq!(
            named(&["Honk", "src/A/Util.luau"]).unwrap(),
            (true, vec![honk, util])
        );
        let nothing = named(&["Nope"]).unwrap_err().to_string();
        assert!(
            nothing
                .starts_with("`Nope` is not a file or a folder, and no script goes by that name"),
            "{nothing}"
        );
        assert_eq!(
            named(&["src/A/Gone.luau"]).unwrap(),
            (true, vec![PathBuf::from("src/A/Gone.luau")]),
            "a deleted script is still named by its file"
        );
        let shared = named(&["Util"]).unwrap_err().to_string();
        assert!(
            shared.starts_with("Util names 2 scripts: src/A/Util.luau, src/B/Util.luau"),
            "{shared}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_script_pushed_by_name_is_syntax_checked_before_studio_is_asked() {
        let root = crate::tests::support::temp_dir("push-name-syntax");
        let script = root.join("src").join("Server").join("Honk.server.luau");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(
            &script,
            "print('honk'
",
        )
        .unwrap();
        let project = ProjectSourceArgs {
            project_root: root.clone(),
            src_root: PathBuf::from("src"),
        };
        let mut args =
            PushEditorChangesArgs::new(project, BridgeConnectionArgs::default_connection());
        args.paths = vec![PathBuf::from("Honk")];
        let error = push_editor_changes(args).unwrap_err().to_string();
        assert!(
            error.starts_with(&script.display().to_string())
                && error.contains("nothing was pushed"),
            "{error}"
        );
        let marked = root.join("src").join("Server").join("Marked.server.luau");
        fs::write(&marked, "\u{feff}return 1\n").unwrap();
        assert!(crate::cli::syntax::ensure_script_files_parse(&[marked]).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}
