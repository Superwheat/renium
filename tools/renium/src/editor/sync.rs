use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Number, Value, json};
use walkdir::WalkDir;

use crate::app::output::{global_json_output, global_pretty_output, global_yes, print_json_output};
use crate::app::timing::{current_millis, elapsed_ms, log_timing, verbose_timing_logs};
use crate::automation::op;
use crate::bytecode::{SettingsFileLock, acquire_settings_file_lock};
use crate::cli::{
    ApplyEditorDeleteArgs, ApplyEditorPropertyArgs, BridgeConnectionArgs, EditorMutationArgs,
    PushEditorChangesArgs,
};
use crate::daemon::try_daemon_control_request;
use crate::editor::diff::{
    append_editor_instance_reconcile, append_editor_property_changes,
    append_editor_target_inline_source_changes, append_editor_target_instance_upserts,
    editor_instance_descriptor_for_known_path,
};
use crate::editor::document::{
    document_instance_index_by_settings_id, ensure_editor_service_document,
    ensure_editor_source_target_in_bytecode, read_editor_service_settings,
};
use crate::editor::history::save_editor_history_entries;
use crate::editor::paths::{
    build_editor_instance_paths, build_editor_source_path_map, editor_run_context_value,
    infer_editor_source_path_spec, service_from_changed_path,
};
use crate::editor::review::{
    apply_protected_writes_offline, is_externally_managed_editor_property,
    is_externally_managed_protected_write, is_user_facing_protected_write,
    local_place_path_for_bridge, protected_root_write_rows_with_live_values,
    protected_write_matches_previous, protected_write_rows_with_previous_values,
    request_editor_push_review, request_protected_write_review, studio_pid_for_bridge,
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
use crate::rbx::encode::rbx_model_property_descriptor;
use crate::roblox::schema::{PropertySchemaMap, load_rbx_dom_property_schema};
use crate::settings::bytecode::SettingsBytecode;
use crate::settings::instance::remove_instances_at_indices;
use crate::snapshot::export::parse_bridge_ports;
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

struct EditorTransaction<'a> {
    bridge: &'a BridgeServer,
    id: String,
    active: bool,
    package_mutation: bool,
}

impl<'a> EditorTransaction<'a> {
    fn parameters(
        changes: &EditorChangeSet,
        binary_import: Option<&EditorBinaryImport>,
        id: &str,
        services: Vec<String>,
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
            add_mutation_root(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            );
        }
        for change in &changes.property_changes {
            add_mutation_root(
                &change.service,
                &change.path_segments,
                &change.path_ordinals,
            );
        }
        for change in &changes.instance_changes {
            for instance in &change.instances {
                add_mutation_root(
                    &change.service,
                    &instance.path_segments,
                    &instance.path_ordinals,
                );
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
                        change
                            .properties
                            .insert("WorldPivot".to_string(), value.clone());
                        change.attributes.clear();
                        change.deleted_attributes.clear();
                        change
                    })
            })
            .collect::<Vec<_>>();
        post_commit_property_changes
            .sort_by_key(|change| std::cmp::Reverse(change.path_segments.len()));
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
            "postCommitPropertyChanges": post_commit_property_changes,
            "nativeImport": native_import,
            "nativeImportServices": native_import_services,
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
        let parameters = Self::parameters(changes, binary_import, &id, services);
        let result = match bridge.call("beginEditorTransaction", parameters) {
            Ok(result) => result,
            Err(error) if error.is::<BridgeRequestTooLarge>() => Self::upload(
                bridge,
                &id,
                Self::parameters(changes, binary_import, &id, upload_services),
            )?,
            Err(error) => return Err(error),
        };
        Ok(Some(Self {
            bridge,
            id,
            active: true,
            package_mutation: result
                .get("packageMutation")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }))
    }

    fn commit(&mut self) -> Result<()> {
        let result = self.bridge.call(
            "commitEditorTransaction",
            json!({
                "transactionId": &self.id,
                "profile": verbose_timing_logs(),
            }),
        )?;
        if verbose_timing_logs()
            && let Some(profile) = result.get("profile")
        {
            eprintln!("[renium] native editor commit profile: {profile}");
        }
        self.active = false;
        if self.package_mutation {
            let dialog_result = studio_pid_for_bridge(self.bridge).and_then(|pid| {
                crate::studio::input::accept_package_changes_dialog_when_visible(pid)
            });
            if let Err(error) = dialog_result {
                eprintln!("[renium] package changes dialog remains open: {error:#}");
            }
        }
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        let result = self.bridge.call(
            "rollbackEditorTransaction",
            json!({ "transactionId": &self.id }),
        );
        if result.is_ok() {
            self.active = false;
        }
        result.map(|_| ())
    }

    fn disarm(&mut self) {
        self.active = false;
    }
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

fn listen_editor_push_bridge(args: &BridgeConnectionArgs) -> Result<BridgeServer> {
    let ports = parse_bridge_ports(&args.ports)?;
    let (bridge, metrics) = BridgeServer::listen(&args.host, &ports, args.wait_seconds)?;
    println!(
        "[renium] editor push bridge ready: channels={}/{}, bind_ms={:.1}, handshake_ms={:.1}",
        bridge.channel_count(),
        bridge.expected_channel_count(),
        metrics.bind_ms,
        metrics.wait_for_channels_ms
    );
    Ok(bridge)
}

pub(crate) fn push_editor_changes(mut args: PushEditorChangesArgs) -> Result<()> {
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
    let started = Instant::now();
    if native_editor_full_push_eligible(&args)? {
        let bridge = listen_editor_push_bridge(&args.bridge)?;
        let (changes, binary_import) = prepare_native_editor_full_push(&args, &bridge)?;
        return push_editor_changes_with_collected(
            args,
            &bridge,
            changes,
            started,
            None,
            Some(binary_import),
        )
        .map(|_| ());
    }
    let (changes, projection) = collect_project_editor_changes(&args)?;
    let bridge = listen_editor_push_bridge(&args.bridge)?;
    push_editor_changes_with_collected(args, &bridge, changes, started, projection.as_ref(), None)
        .map(|_| ())
}

pub(crate) fn push_editor_changes_with_warm_bridge(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
) -> Result<serde_json::Map<String, Value>> {
    let started = Instant::now();
    if native_editor_full_push_eligible(&args)? {
        let (changes, binary_import) = prepare_native_editor_full_push(&args, bridge)?;
        return push_editor_changes_with_collected(
            args,
            bridge,
            changes,
            started,
            None,
            Some(binary_import),
        );
    }
    let (changes, projection) = collect_project_editor_changes(&args)?;
    push_editor_changes_with_collected(args, bridge, changes, started, projection.as_ref(), None)
}

fn native_editor_full_push_eligible(args: &PushEditorChangesArgs) -> Result<bool> {
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
    let Some(loaded) = config::try_load_project(None, Some(&args.project.project_root))? else {
        return Ok((collect_editor_changes(args)?, None));
    };
    let link_enforcement = build_loaded_project_link_enforcement(&loaded, args.override_packages)?;
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
    let projection = config::stage_project_cached(&loaded, &changed_sources)?;
    if !projection.is_temporary() {
        let (project_root, src_root) = editor_project_roots(args)?;
        return Ok((
            collect_editor_changes_with_link_enforcement(
                args,
                &project_root,
                &src_root,
                &link_enforcement,
            )?,
            Some(projection),
        ));
    }
    let mut projected_paths = if full_selection {
        collect_editor_full_paths(projection.root())?
    } else {
        let mut paths = Vec::new();
        for changed_path in changed_paths {
            let absolute = absolutize_under(&loaded.root, &changed_path);
            paths.extend(config::project_source_to_staged_paths(
                &loaded,
                &absolute,
                projection.root(),
            )?);
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

fn push_editor_changes_with_collected(
    args: PushEditorChangesArgs,
    bridge: &BridgeServer,
    mut changes: EditorChangeSet,
    started: Instant,
    projection: Option<&config::ProjectionStage>,
    prepared_binary_import: Option<EditorBinaryImport>,
) -> Result<serde_json::Map<String, Value>> {
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
    if binary_import.is_none()
        && !changes.files_to_studio_filters_active
        && changes
            .instance_changes
            .iter()
            .any(|change| change.mode == "reconcileService" && change.allow_deletes)
    {
        bail!("A full service replacement could not be staged; Studio was not changed");
    }
    let mut history_transaction = if review_skipped || binary_import.is_some() {
        None
    } else {
        save_editor_history_entries(bridge, &args.project.project_root, &changes)?
    };
    let phase_started = Instant::now();
    let mut transaction = if review_skipped {
        None
    } else {
        EditorTransaction::begin(bridge, &changes, binary_import.as_ref())?
    };
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
                    return Err(
                        error.context(format!("Studio rollback also failed: {rollback_error:#}"))
                    );
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
        let mut verification = verify_editor_source_changes(bridge, &changes)?;
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
                transaction.as_ref().map(|value| value.id.as_str()),
            )?;
            verification = verify_editor_source_changes(bridge, &changes)?;
        }
        summary.insert(
            "sourceVerified".to_string(),
            Value::Number(serde_json::Number::from(verification.verified as u64)),
        );
        summary.insert(
            "sourceVerifyFailed".to_string(),
            Value::Number(serde_json::Number::from(verification.failed.len() as u64)),
        );
        if !verification.failed.is_empty() {
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
            emit_editor_push_summary(&summary)?;
            return Err(EditorSourceVerificationError {
                details: verification.failed,
            }
            .into());
        }
    }
    let phase_started = Instant::now();
    let mut reported_protected_writes = summary
        .get("protectedWrites")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let pre_routed_protected_count = pre_routed_protected_writes.len();
    let pre_routed_protected_writes =
        protected_root_write_rows_with_live_values(bridge, pre_routed_protected_writes)
            .unwrap_or_else(|rows| rows);
    reported_protected_writes.extend(pre_routed_protected_writes);
    if pre_routed_protected_count > 0 {
        summary.insert(
            "protectedPreRouted".to_string(),
            Value::Number(Number::from(pre_routed_protected_count as u64)),
        );
    }
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    let applicable_protected_writes = reported_protected_writes
        .iter()
        .filter(|row| {
            !is_externally_managed_protected_write(row)
                && is_user_facing_protected_write(row, database)
        })
        .cloned()
        .collect::<Vec<_>>();
    let unavailable_protected_skipped =
        reported_protected_writes.len() - applicable_protected_writes.len();
    let enriched_protected_writes = if args.no_review {
        applicable_protected_writes
    } else {
        local_place_path_for_bridge(bridge)
            .and_then(|path| {
                protected_write_rows_with_previous_values(&path, &applicable_protected_writes).ok()
            })
            .unwrap_or(applicable_protected_writes)
    };
    let protected_writes = enriched_protected_writes
        .iter()
        .filter(|row| !protected_write_matches_previous(row))
        .cloned()
        .collect::<Vec<_>>();
    let already_current = enriched_protected_writes.len() - protected_writes.len();
    summary.insert(
        "protectedWrites".to_string(),
        Value::Array(protected_writes.clone()),
    );
    if unavailable_protected_skipped > 0 {
        summary.insert(
            "unavailableProtectedSkipped".to_string(),
            Value::Number(Number::from(unavailable_protected_skipped as u64)),
        );
    }
    if already_current > 0 {
        summary.insert(
            "protectedAlreadyCurrent".to_string(),
            Value::Number(Number::from(already_current as u64)),
        );
    }
    let apply_protected_offline = !args.no_review
        && !protected_writes.is_empty()
        && (args.yes || global_yes() || request_protected_write_review(bridge, &protected_writes)?);
    log_timing("native editor protected write preparation", phase_started);
    let phase_started = Instant::now();
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
    if apply_protected_offline {
        let result = apply_protected_writes_offline(bridge, &args, &protected_writes)?;
        if let Some(transaction) = transaction.as_mut() {
            transaction.disarm();
        }
        summary.insert("protectedOfflineApply".to_string(), result);
        summary.insert(
            "protectedApplied".to_string(),
            Value::Number(serde_json::Number::from(protected_writes.len() as u64)),
        );
    } else if let Some(transaction) = transaction.as_mut()
        && let Err(commit_error) = transaction.commit()
    {
        if let Err(rollback_error) = transaction.rollback() {
            return Err(
                commit_error.context(format!("Studio rollback also failed: {rollback_error:#}"))
            );
        }
        return Err(
            commit_error.context("Studio rejected the commit; its changes were rolled back")
        );
    }
    log_timing("native editor transaction commit", phase_started);
    if let Some(settings_transaction) = settings_transaction {
        settings_transaction.commit();
    }
    if let Some(history_transaction) = history_transaction {
        history_transaction.commit();
    }
    println!(
        "[renium] editor push done: elapsed_ms={:.1}, summary={}",
        elapsed_ms(started),
        Value::Object(summary.clone())
    );
    emit_editor_push_summary(&summary)?;
    let errors = summary.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
    if summary.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
        bail!("Studio rejected or failed one or more editor push changes");
    }
    Ok(summary)
}

fn listen_editor_oneshot_bridge(
    label: &str,
    host: &str,
    ports_raw: &str,
    wait_seconds: f64,
) -> Result<BridgeServer> {
    let ports = parse_bridge_ports(ports_raw)?;
    let (bridge, listen_metrics) = BridgeServer::listen(host, &ports, wait_seconds)?;
    println!(
        "[renium] editor {label} bridge ready: channels={}/{}, bind_ms={:.1}, handshake_ms={:.1}",
        bridge.channel_count(),
        bridge.expected_channel_count(),
        listen_metrics.bind_ms,
        listen_metrics.wait_for_channels_ms
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
        println!(
            "[renium] editor {label} apply done: elapsed_ms={:.1}, summary={}",
            elapsed_ms(started),
            Value::Object(summary.clone())
        );
        emit_editor_push_summary(&summary)?;
        return Ok(summary);
    }
    let mut transaction = EditorTransaction::begin(bridge, &changes, None)?;
    let transaction_id = transaction.as_ref().map(|value| value.id.as_str());
    let summary =
        send_editor_change_batches(bridge, &changes, false, false, false, None, transaction_id)?;
    let errors = summary.get("errors").and_then(Value::as_f64).unwrap_or(0.0);
    if summary.get("ok").and_then(Value::as_bool) == Some(false) || errors > 0.0 {
        bail!("Studio rejected or failed editor {label} apply");
    }
    if let Some(transaction) = transaction.as_mut() {
        transaction.commit()?;
    }
    println!(
        "[renium] editor {label} apply done: elapsed_ms={:.1}, summary={}",
        elapsed_ms(started),
        Value::Object(summary.clone())
    );
    emit_editor_push_summary(&summary)?;
    Ok(summary)
}

fn emit_editor_push_summary(summary: &serde_json::Map<String, Value>) -> Result<()> {
    let value = Value::Object(summary.clone());
    if global_json_output() {
        print_json_output(&value, global_pretty_output(false))
    } else {
        println!("__ROBLOX_SYNC_EDITOR_PUSH_RESULT__ {value}");
        Ok(())
    }
}

pub(crate) fn apply_editor_property(mut args: ApplyEditorPropertyArgs) -> Result<()> {
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
        serde_json::from_str(&args.value_json).context("Failed to parse --value-json")?,
    );
    let approved = !args.no_review && (args.yes || global_yes());
    if let Some(result) = try_daemon_control_request(
        op::SET_PROPERTY,
        Some(&args.target.project.project_root),
        Value::Object(parameters),
        approved,
    )? {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let bridge = listen_editor_oneshot_bridge(
        "property",
        &args.target.bridge.host,
        &args.target.bridge.ports,
        args.target.bridge.wait_seconds,
    )?;
    apply_editor_property_with_warm_bridge(args, &bridge).map(|_| ())
}

pub(crate) fn apply_editor_property_with_warm_bridge(
    args: ApplyEditorPropertyArgs,
    bridge: &BridgeServer,
) -> Result<Map<String, Value>> {
    let started = Instant::now();
    let changes = collect_direct_editor_property_change(&args)?;
    push_editor_changes_with_collected(
        PushEditorChangesArgs {
            no_review: args.no_review,
            yes: args.yes || global_yes(),
            override_packages: args.target.override_packages,
            ..PushEditorChangesArgs::new(args.target.project, args.target.bridge)
        },
        bridge,
        changes,
        started,
        None,
        None,
    )
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
    let value: Value =
        serde_json::from_str(&args.value_json).context("Failed to parse --value-json")?;

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
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let bridge = listen_editor_oneshot_bridge(
        "delete",
        &args.target.bridge.host,
        &args.target.bridge.ports,
        args.target.bridge.wait_seconds,
    )?;
    apply_editor_delete_with_warm_bridge(args, &bridge).map(|_| ())
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
    let path_segments: Vec<String> = serde_json::from_str(&target.path_segments_json)
        .context("Failed to parse --path-segments-json")?;
    let path_ordinals: Vec<usize> = serde_json::from_str(&target.path_ordinals_json)
        .context("Failed to parse --path-ordinals-json")?;
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
                Some(Ok(actual)) if actual == expected => None,
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

fn expand_editor_changed_paths(args: &PushEditorChangesArgs) -> Result<Vec<PathBuf>> {
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
    Ok(paths)
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

fn sorted_services(services: HashSet<String>) -> Vec<String> {
    let mut services = services.into_iter().collect::<Vec<_>>();
    services.sort();
    services
}

fn validate_read_only_service_changes(
    link_enforcement: &LinkEnforcement,
    changed_services: &HashSet<String>,
    documents: &HashMap<String, Option<SettingsBytecode>>,
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
    documents: &HashMap<String, Option<SettingsBytecode>>,
    services: EditorChangedServices,
    property_filter: &EditorPropertyFilter,
    project_root: &Path,
    src_root: &Path,
    link_enforcement: &LinkEnforcement,
) -> Result<EditorChangeSet> {
    validate_read_only_service_changes(link_enforcement, &services.settings, documents, src_root)?;
    for service in sorted_services(services.dirty) {
        if let Some(document) = documents.get(&service).and_then(Option::as_ref) {
            changes.settings_writes.push(EditorSettingsWrite {
                path: service_settings_path(&src_root.join(&service)),
                document: document.clone(),
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
    for service in sorted_services(services.target_upsert) {
        if let Some(document) = documents.get(&service).and_then(Option::as_ref) {
            append_editor_target_instance_upserts(
                &mut changes,
                document,
                &service,
                property_filter,
            );
            append_editor_target_inline_source_changes(
                &mut changes,
                document,
                &service,
                property_filter,
            );
        }
    }
    let settings_services = sorted_services(services.settings);
    let property_schema_by_class = if settings_services.is_empty() {
        PropertySchemaMap::new()
    } else {
        load_rbx_dom_property_schema(project_root)?.unwrap_or_default()
    };
    let database = rbx_reflection_database::get().context("Failed to load Roblox reflection DB")?;
    for service in settings_services {
        if let Some(document) = documents.get(&service).and_then(Option::as_ref) {
            append_editor_property_changes(
                &mut changes,
                document,
                &service,
                &property_schema_by_class,
                property_filter,
                database,
            );
        }
    }
    Ok(changes)
}

fn collect_editor_changes_with_link_enforcement(
    args: &PushEditorChangesArgs,
    project_root: &Path,
    src_root: &Path,
    link_enforcement: &LinkEnforcement,
) -> Result<EditorChangeSet> {
    let property_filter = EditorPropertyFilter::from_args(args)?;
    let mut changes = EditorChangeSet::default();
    let mut documents: HashMap<String, Option<SettingsBytecode>> = HashMap::new();
    let mut source_maps: HashMap<String, HashMap<String, EditorSourceTarget>> = HashMap::new();
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
        let absolute_path = canonical_editor_changed_path(project_root, &changed_path);
        let path_id = path_key(&absolute_path);
        if !seen_paths.insert(path_id.clone()) {
            continue;
        }
        let Some(service) = service_from_changed_path(src_root, &absolute_path) else {
            continue;
        };

        if !documents.contains_key(&service) {
            documents.insert(
                service.clone(),
                read_editor_service_settings(src_root, &service)?,
            );
        }

        if absolute_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_service_settings_file_name)
        {
            if args.upsert_instances_only {
                changed_services.target_upsert.insert(service.clone());
            } else {
                changed_services.settings.insert(service.clone());
                if !property_filter.is_active() {
                    changed_services.reconcile.insert(service.clone());
                } else if !property_filter.settings_ids.is_empty() {
                    changed_services.target_upsert.insert(service.clone());
                }
            }
            continue;
        }

        if !source_maps.contains_key(&service) {
            let map = documents
                .get(&service)
                .and_then(Option::as_ref)
                .map(|document| {
                    build_editor_source_path_map(document, &service, &src_root.join(&service))
                })
                .unwrap_or_default();
            source_maps.insert(service.clone(), map);
        }

        let mut mapped_target = source_maps
            .get(&service)
            .and_then(|map| map.get(&path_id))
            .cloned();

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
            let settings_before = documents.get(&service).and_then(Option::as_ref).cloned();
            let slot = documents
                .get_mut(&service)
                .expect("service document should be loaded");
            let document = ensure_editor_service_document(slot);
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
                        settings_before,
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
                    changes.instance_changes.push(EditorInstanceChange {
                        mode: "replaceInstances".to_string(),
                        service: service.clone(),
                        allow_deletes: false,
                        instances: ensured.replace_instances,
                        preserve_instances: Vec::new(),
                    });
                    changed_services.settings.insert(service.clone());
                }
                source_maps.remove(&service);
            }
        }

        if !exists_as_file
            && let Some(target) = mapped_target.as_ref()
            && is_lua_source_class(&target.class_name)
        {
            if let Some(settings_id) = target.settings_id.as_deref()
                && let Some(document) = documents.get_mut(&service).and_then(Option::as_mut)
                && let Some(index) = document_instance_index_by_settings_id(document, settings_id)
            {
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
                        settings_before: Some(settings_before),
                    });
                    changed_services.dirty.insert(service.clone());
                    changed_services.settings.insert(service.clone());
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
                    } else {
                        let descriptor = editor_instance_descriptor_for_known_path(
                            document,
                            index,
                            target.path_segments.clone(),
                            target.path_ordinals.clone(),
                        )
                        .context("Failed to describe the deleted source instance")?;
                        remove_instances_at_indices(document, &[index], true)?;
                        changes.instance_changes.push(EditorInstanceChange {
                            mode: "deleteInstances".to_string(),
                            service: service.clone(),
                            allow_deletes: false,
                            instances: vec![descriptor],
                            preserve_instances: Vec::new(),
                        });
                    }
                    source_maps.remove(&service);
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

    finish_editor_change_collection(
        changes,
        &documents,
        changed_services,
        &property_filter,
        project_root,
        src_root,
        link_enforcement,
    )
}
