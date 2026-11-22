//! Drive the real push coordinator, replacing only the engine capture/mutation
//! boundary. Planning, history, proof eligibility, verification and journals run.
use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::thread;
use tungstenite::{Message, WebSocket, protocol::Role};

use crate::snapshot::export::{PublicationStep, hook_publication_steps};

type Capture = Box<dyn FnOnce(&BoundContext) -> Result<ProjectSnapshot>>;
struct MutationInput<'a> {
    plan: &'a ReconcilePushPlan,
    args: &'a PushEditorChangesArgs,
}
type Mutation = Box<dyn FnOnce(&BoundContext, MutationInput<'_>) -> Result<StagedPushResult>>;
thread_local! {
    static CAPTURES: RefCell<VecDeque<Capture>> = RefCell::new(VecDeque::new());
    static MUTATION: RefCell<Option<Mutation>> = RefCell::new(None);
    static REQUESTED: RefCell<Vec<(Vec<String>, PathBuf)>> = const { RefCell::new(Vec::new()) };
}

/// Answers a capture of `services` into the stage at `stage`.
pub(super) fn capture(
    context: &BoundContext,
    services: &[String],
    stage: &Path,
) -> Option<Result<ProjectSnapshot>> {
    REQUESTED.with(|slot| {
        slot.borrow_mut()
            .push((services.to_vec(), stage.to_path_buf()))
    });
    CAPTURES
        .with(|slot| slot.borrow_mut().pop_front())
        .map(|capture| capture(context))
}

fn last_requested() -> (Vec<String>, PathBuf) {
    REQUESTED.with(|slot| slot.borrow().last().cloned().unwrap_or_default())
}

pub(super) fn mutation(
    context: &BoundContext,
    args: &PushEditorChangesArgs,
    plan: &ReconcilePushPlan,
) -> Option<Result<StagedPushResult>> {
    MUTATION
        .with(|slot| slot.borrow_mut().take())
        .map(|mutation| mutation(context, MutationInput { args, plan }))
}

pub(super) struct Fixture {
    _selection: crate::app::context::Selection,
    pub(super) context: BoundContext,
    pub(super) bridge: Option<BridgeServer>,
    service: String,
    plugin: Option<thread::JoinHandle<()>>,
    tracker: Arc<Mutex<Tracker>>,
    pub(super) desired: ProjectSnapshot,
}

/// What the fixture plugin says of Studio: the generations its edits move,
/// and how it stands against the proofs it holds. A push commits under the
/// proof `{"test": true}`; a held proof is `{"held": true}`.
struct Tracker {
    /// The generations Studio's edits move (`serviceGenerations`).
    edits: Map<String, Value>,
    /// The push committed, so Studio no longer holds the held proof.
    committed: bool,
    /// Studio changed after the commit.
    changed_since_commit: bool,
    /// The services Studio differs from the held proof in once the push
    /// committed, or None where it cannot say.
    scope: Option<Vec<&'static str>>,
    /// How often Studio was asked whether it still holds the commit's proof.
    proofs: usize,
    /// How often Studio was asked where it differs from the held proof.
    scopes: usize,
    /// What changes Studio or the files when Studio is asked of the commit's
    /// proof for the given time.
    late: Late,
    /// A file edit not yet made, which lands as the push releases its proof
    /// where `edit_on_release`.
    edit: Option<(PathBuf, Vec<u8>)>,
    edit_on_release: bool,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            edits: Map::from_iter([
                ("ReplicatedStorage".into(), json!(0)),
                ("Workspace".into(), json!(0)),
            ]),
            committed: false,
            changed_since_commit: false,
            scope: Some(Vec::new()),
            proofs: 0,
            scopes: 0,
            late: None,
            edit: None,
            edit_on_release: false,
        }
    }
}

/// Studio's reply to `params`: a proof holds unless it expired or Studio
/// changed since, and one it keeps holds the same way.
fn tracker_reply(tracker: &Tracker, params: &Value, runtime: &str) -> Value {
    let differs = |proof: &Value| {
        proof["expired"] == true
            || (tracker.committed && proof["held"] == true)
            || (tracker.changed_since_commit && proof["test"] == true)
    };
    let verify = &params["verifyPushProof"];
    let matches = verify.is_object() && !differs(verify);
    let scope = (!matches && params["scopePushProof"] == true).then(|| {
        tracker.scope.as_ref().map_or_else(
            || json!({"unknown": "Terrain unavailable"}),
            |services| json!({"services": services, "touched": {}}),
        )
    });
    let retain = &params["retainPushProof"];
    let retained = (!retain.is_null() && !differs(retain)).then(|| retain.clone());
    json!({
        "pushProofMatches": matches, "pushProofScope": scope,
        "retainedPushProof": retained, "retainedPushProofLocal": true,
        "verifiedPushProof": {"test": true}, "runtimeId": runtime,
        "seq": 0, "tracking": true, "serviceGenerations": tracker.edits
    })
}

fn fixture_runtime(
    bridge: &BridgeServer,
    runtime: &'static str,
    tracker: Arc<Mutex<Tracker>>,
) -> thread::JoinHandle<()> {
    let stream = crate::studio::bridge::routing_tests::connect(
        bridge,
        0,
        crate::studio::bridge::routing_tests::edit_info(runtime, 20),
    );
    thread::spawn(move || {
        let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
        while let Ok(message) = socket.read() {
            // A baseline's connection check probes the socket.
            let Message::Text(text) = message else {
                if let Message::Ping(bytes) = message {
                    let _ = socket.send(Message::Pong(bytes));
                }
                continue;
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            assert!(matches!(
                request["method"].as_str(),
                Some("getStudioChangeState" | "getStudioState")
            ));
            let params = &request["params"];
            let result = {
                let mut state = tracker.lock().unwrap();
                if params["scopePushProof"] == true {
                    state.scopes += 1;
                } else if params["verifyPushProof"]["test"] == true {
                    state.proofs += 1;
                    let count = state.proofs;
                    if let Some((_, change)) = state.late.take_if(|(at, _)| *at == count) {
                        change(&mut state);
                    }
                }
                if params["retainPushProof"]["test"] == true
                    && state.edit_on_release
                    && let Some((path, bytes)) = state.edit.take()
                {
                    fs::write(path, bytes).unwrap();
                }
                tracker_reply(&state, params, runtime)
            };
            let reply = json!({"id": request["id"], "ok": true, "result": result});
            if socket
                .send(Message::Text(reply.to_string().into()))
                .is_err()
            {
                break;
            }
        }
    })
}

impl Fixture {
    pub(super) fn new() -> Result<Self> {
        Self::for_service("ReplicatedStorage")
    }

    fn for_service(service_name: &str) -> Result<Self> {
        let root = crate::tests::support::temp_dir("push-coordinator-settle");
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
        )?;
        let service = Path::new("src").join(service_name);
        fs::create_dir_all(root.join(&service))?;
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "root".into(),
                    service_name.into(),
                    service_name.into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "folder".into(),
                    "Imported".into(),
                    "Folder".into(),
                    Some(0),
                ),
            ],
        };
        fs::write(
            root.join(service_settings_path(&service)),
            encode_settings_bytecode(&document)?,
        )?;
        config::load_project(None, Some(&root))?;
        let desired = capture_snapshot(&root, &["src".into(), "instances".into()])?;
        let context = BoundContext {
            id: 1,
            initialized: true,
            project: root.join("renium.project.jsonc").display().to_string(),
            root: root.display().to_string(),
            source: root.join("src").display().to_string(),
            experience: String::new(),
            resource_lease: None,
            place_id: Some(20),
            game_id: Some(10),
            selector: String::new(),
            runtime_id: Some("push-fixture".into()),
            plugin_build: None,
            fingerprint: String::new(),
        };
        let bridge = crate::studio::bridge::routing_tests::fixture();
        let tracker = Arc::new(Mutex::new(Tracker::default()));
        let plugin = fixture_runtime(&bridge, "push-fixture", Arc::clone(&tracker));
        CAPTURES.with(|slot| {
            slot.borrow_mut()
                .push_back(Box::new(|_| Ok(ProjectSnapshot::default())))
        });
        Ok(Self {
            _selection: bound_context::select_for_thread(&context),
            context,
            service: service_name.into(),
            bridge: Some(bridge),
            plugin: Some(plugin),
            tracker,
            desired,
        })
    }

    fn run(&self) -> Result<Map<String, Value>> {
        self.push(false)
    }

    fn push(&self, slow_import: bool) -> Result<Map<String, Value>> {
        let guard = StudioChangeGuard {
            runtime_id: "push-fixture".into(),
            change_seq: 0,
            tracking_guard_id: None,
            runtime_bootstrap_safe: true,
            service_generations: BTreeMap::new(),
        };
        self.push_with(
            std::slice::from_ref(&self.service),
            Some(&guard),
            (slow_import, true),
        )
    }

    fn push_args(&self) -> PushEditorChangesArgs {
        PushEditorChangesArgs::new(
            crate::cli::ProjectSourceArgs {
                project_root: self.context.root.clone().into(),
                src_root: "src".into(),
            },
            crate::cli::BridgeConnectionArgs::local(0.1),
        )
    }

    /// A push whose caller takes an early reply where `answers_early`.
    fn push_with(
        &self,
        services: &[String],
        guard: Option<&StudioChangeGuard>,
        (slow_import, answers_early): (bool, bool),
    ) -> Result<Map<String, Value>> {
        let _reply = answers_early.then(crate::daemon::fixture_early_response);
        let bridge = self.bridge.as_ref().unwrap();
        let lease = Arc::new(crate::studio::bridge::BridgeRequestLease::new(
            "push-fixture".into(),
        ));
        let _lease = bridge.activate_request_lease(lease)?;
        let _cleanup = import_settle::RequestCleanup;
        let mut args = self.push_args();
        args.slow_import = slow_import;
        let result = push_project(&self.context, bridge, services, args, guard, true);
        import_settle::finish_request(
            result
                .as_ref()
                .err()
                .map(|error| error.to_string())
                .as_deref(),
        )?;
        result
    }
}

pub(super) fn replace_capture(snapshot: ProjectSnapshot) {
    CAPTURES.with(|slot| {
        let mut captures = slot.borrow_mut();
        captures.clear();
        captures.push_back(Box::new(|_| Ok(snapshot)));
    });
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = import_settle::finish_request(Some("test ended"));
        CAPTURES.with(|slot| slot.borrow_mut().clear());
        MUTATION.with(|slot| slot.borrow_mut().take());
        drop(self.bridge.take());
        self.plugin.take().unwrap().join().unwrap();
        let _ = fs::remove_dir_all(&self.context.root);
    }
}

fn summary(verified: bool) -> Map<String, Value> {
    let mut summary = Map::from_iter([
        ("ok".into(), json!(true)),
        ("sourceVerifyFailed".into(), json!(0)),
    ]);
    if verified {
        summary.insert(
            "nativeVerifiedServices".into(),
            json!(["ReplicatedStorage"]),
        );
        summary.insert("verifiedPushProof".into(), json!({"test":true}));
    }
    summary
}

#[test]
fn committed_deferred_failures_survive_full_push_readback_without_masking_other_fields()
-> Result<()> {
    use crate::editor::types::PendingImportFields;
    for mutation in [
        "",
        "unreported",
        "wrong_target",
        "wrong_value",
        "wrong_class",
        "wrong_ordinal",
    ] {
        let fixture = Fixture::new()?;
        let root = Path::new(&fixture.context.root);
        let store = fixture
            .desired
            .entries
            .keys()
            .find(|path| path.extension().is_some_and(|ext| ext == "renium"))
            .unwrap()
            .clone();
        let mut desired = decode_settings_bytecode(&fs::read(root.join(&store))?)?;
        let mut pending = Vec::new();
        for index in 0..33 {
            let id = format!("part{index}");
            let name = format!("Part{index}");
            let mut part =
                SettingsBytecodeInstance::new(id.clone(), name.clone(), "Part".into(), Some(0));
            part.properties.insert("Transparency".into(), json!(0.75));
            part.properties.insert("Anchored".into(), json!(true));
            desired.instances.push(part);
            pending.push(PendingImportFields {
                service: "ReplicatedStorage".into(),
                settings_id: Some(id),
                class_name: "Part".into(),
                path_segments: vec!["ReplicatedStorage".into(), name],
                path_ordinals: vec![1, 1],
                properties: Map::from_iter([("Transparency".into(), json!(0.75))]),
            });
        }
        let bytes = encode_settings_bytecode(&desired)?;
        fs::write(root.join(&store), &bytes)?;
        let mut observed = desired.clone();
        for part in observed.instances.iter_mut().skip(2) {
            part.properties.insert("Transparency".into(), json!(0.0));
        }
        match mutation {
            "unreported" => {
                observed.instances[2]
                    .properties
                    .insert("Anchored".into(), json!(false));
            }
            "wrong_target" => pending[0].settings_id = Some("part1".into()),
            "wrong_value" => {
                pending[0]
                    .properties
                    .insert("Transparency".into(), json!(0.5));
            }
            "wrong_class" => pending[0].class_name = "Folder".into(),
            "wrong_ordinal" => pending[0].path_ordinals[1] = 2,
            _ => {}
        }
        let mut readback = fixture.desired.clone();
        readback.entries.insert(
            store.clone(),
            SnapshotEntry::File(encode_settings_bytecode(&observed)?),
        );
        CAPTURES.with(|slot| slot.borrow_mut().push_back(Box::new(move |_| Ok(readback))));
        MUTATION.with(|slot| *slot.borrow_mut() = Some(Box::new(move |_, request| {
            assert!(request.args.slow_import);
            let mut committed = summary(false);
            committed.insert("supplementalImportPendingWrites".into(), json!(33));
            committed.insert("supplementalImportPendingFields".into(), json!(pending));
            committed.insert("unsupportedProperties".into(), json!(["deferred write failed; the import committed, retry the push to write this field"]));
            Ok(StagedPushResult { generated:ProjectSnapshot::default(), summary:committed })
        })));
        let result = fixture.push(true);
        if mutation.is_empty() {
            let result = result?;
            assert_eq!(result["supplementalImportPendingWrites"], 33);
            assert_eq!(result["verified"], false);
            assert!(
                result["unsupportedProperties"][0]
                    .as_str()
                    .unwrap()
                    .contains("retry the push")
            );
            let id = result["historyId"].as_str().unwrap();
            assert!(
                root.join(".renium/editor-history/sync")
                    .join(id)
                    .join("sync.rmp")
                    .is_file()
            );
        } else {
            assert!(result.is_err(), "accepted {mutation}");
        }
        assert_eq!(
            fs::read(root.join(&store))?,
            bytes,
            "pending requested values were lost"
        );
        assert!(!import_settle::pending(&fixture.context));
    }
    Ok(())
}

#[test]
fn refused_staged_push_discards_only_its_undo_entry_and_opt_in_survives_staging() -> Result<()> {
    for allow in [false, true] {
        let fixture = Fixture::new()?;
        let directory = Path::new(&fixture.context.root).join(".renium/editor-history/sync");
        let previous = directory.join("previous");
        fs::create_dir_all(&previous)?;
        fs::write(previous.join("keep"), "previous undo")?;
        MUTATION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |context, request| {
                assert_ne!(request.args.project.project_root, Path::new(&context.root));
                assert_eq!(
                    request.args.link_project_root.as_deref(),
                    Some(Path::new(&context.root))
                );
                assert!(!request.args.changed_paths.is_empty());
                assert_eq!(request.args.slow_import, allow);
                if !request.args.slow_import {
                    return Err(crate::editor::sync::SlowImportRequired {
                        message: "simulated reader failure".into(),
                        live_scope: None,
                    }
                    .into());
                }
                let mut accepted = summary(false);
                accepted.insert("supplementalImportWrites".into(), json!(40));
                Ok(StagedPushResult {
                    generated: ProjectSnapshot::default(),
                    summary: accepted,
                })
            }))
        });
        if allow {
            let desired = fixture.desired.clone();
            CAPTURES.with(|slot| slot.borrow_mut().push_back(Box::new(move |_| Ok(desired))));
            assert_eq!(fixture.push(true)?["supplementalImportWrites"], 40);
        } else {
            assert!(
                fixture
                    .run()
                    .unwrap_err()
                    .is::<crate::editor::sync::SlowImportRequired>()
            );
            assert_eq!(fs::read_dir(directory)?.count(), 1);
            assert!(!import_settle::pending(&fixture.context));
        }
        assert_eq!(fs::read_to_string(previous.join("keep"))?, "previous undo");
    }
    Ok(())
}

#[test]
fn staged_push_keeps_undo_only_if_the_mutation_may_have_started() -> Result<()> {
    for result in ["held", "skipped", "preflight", "unknown outcome"] {
        let fixture = Fixture::new()?;
        MUTATION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |_, args| {
                if result == "held" {
                    return Ok(StagedPushResult {
                        generated: ProjectSnapshot::default(),
                        summary: Map::from_iter([("heldForReview".into(), json!(true))]),
                    });
                }
                if result == "skipped" {
                    return Err(SkippedPushReview.into());
                }
                if result == "unknown outcome" {
                    args.args
                        .mutation_started
                        .as_ref()
                        .unwrap()
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                bail!("{result}")
            }));
        });
        let response = fixture.run();
        if result == "held" {
            assert_eq!(response?["heldForReview"], true);
        } else {
            assert!(response.is_err());
        }
        let directory = Path::new(&fixture.context.root).join(".renium/editor-history/sync");
        assert_eq!(
            fs::read_dir(directory)?.count(),
            usize::from(result == "unknown outcome"),
            "{result}"
        );
    }
    Ok(())
}

#[test]
fn both_live_coordinator_paths_remove_undo_on_an_uncommitted_refusal() -> Result<()> {
    for incremental in [false, true] {
        let mut fixture = Fixture::new()?;
        fixture.context.experience.clone_from(&fixture.context.root);
        let root = Path::new(&fixture.context.root);
        let context = &fixture.context;
        let bridge = fixture.bridge.as_ref().unwrap();
        let coordinator = Coordinator::default();
        let mut setup = coordinator.prepare(
            context,
            bridge,
            PairConfiguration {
                mode: PairMode::Reconcile,
                conflict_preference: ConflictPreference::Editor,
                resolution_preference: Some(ConflictPreference::Editor),
                decisions: Default::default(),
                runtime_settings: Map::new(),
            },
        )?;
        let mut record = load_record(context, &setup.key)?.unwrap();
        record.baseline = Some(StoredSnapshot::write(
            root,
            &setup.key,
            &ProjectSnapshot::default(),
        )?);
        write_record(context, &setup.key, &record)?;
        let undo = root.join(".renium/editor-history/sync");
        let previous = undo.join("previous");
        fs::create_dir_all(&previous)?;
        MUTATION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|_, request| {
                assert!(
                    !request
                        .args
                        .mutation_started
                        .as_ref()
                        .unwrap()
                        .load(std::sync::atomic::Ordering::Acquire)
                );
                bail!("fixture preflight refusal")
            }))
        });
        let error = if incremental {
            let path = fixture
                .desired
                .entries
                .keys()
                .find(|p| p.extension().is_some_and(|ext| ext == "renium"))
                .unwrap();
            coordinator
                .push_editor_changes(
                    context,
                    &setup.key,
                    bridge,
                    std::slice::from_ref(path),
                    None,
                )
                .err()
        } else {
            coordinator
                .reconcile_with_gate_held(context, bridge, &mut setup)
                .err()
        }
        .context("expected a staged refusal")?;
        assert!(
            error.to_string().contains("fixture preflight refusal"),
            "{error:#}"
        );
        assert!(MUTATION.with(|slot| slot.borrow().is_none()));
        assert!(previous.is_dir());
        assert_eq!(fs::read_dir(undo)?.count(), 1);
    }
    Ok(())
}

#[test]
fn reconcile_keeps_pending_deferred_fields_and_retries_against_the_actual_baseline() -> Result<()> {
    for remote_edit in [false, true] {
        for unreported in [false, true] {
            let mut fixture = Fixture::new()?;
            fixture.context.experience.clone_from(&fixture.context.root);
            let root = Path::new(&fixture.context.root);
            let store = fixture
                .desired
                .entries
                .keys()
                .find(|path| path.extension().is_some_and(|ext| ext == "renium"))
                .unwrap()
                .clone();
            let mut document = decode_settings_bytecode(&fs::read(root.join(&store))?)?;
            let mut part =
                SettingsBytecodeInstance::new("part".into(), "Part".into(), "Part".into(), Some(0));
            part.properties = serde_json::from_value(
                json!({"Transparency":0.0,"Anchored":false,"CanCollide":false}),
            )?;
            document.instances.push(part);
            let snapshot = |doc: &SettingsBytecode| -> Result<ProjectSnapshot> {
                let mut snapshot = fixture.desired.clone();
                // Reconcile captures services, not their containing source root.
                snapshot.entries.remove(Path::new("src"));
                snapshot.entries.remove(Path::new("instances"));
                snapshot.entries.insert(
                    store.clone(),
                    SnapshotEntry::File(encode_settings_bytecode(doc)?),
                );
                Ok(snapshot)
            };
            let baseline = snapshot(&document)?;
            let mut before = document.clone();
            before.instances[2]
                .properties
                .insert("CanCollide".into(), json!(remote_edit));
            let mut desired = document.clone();
            desired.instances[2]
                .properties
                .insert("Transparency".into(), json!(0.75));
            desired.instances[2]
                .properties
                .insert("Anchored".into(), json!(true));
            fs::write(root.join(&store), encode_settings_bytecode(&desired)?)?;
            let mut actual = before.clone();
            actual.instances[2]
                .properties
                .insert("Anchored".into(), json!(!unreported));
            let actual = snapshot(&actual)?;
            replace_capture(snapshot(&before)?);
            let captured = actual.clone();
            CAPTURES.with(|slot| slot.borrow_mut().push_back(Box::new(move |_| Ok(captured))));
            let bridge = fixture.bridge.as_ref().unwrap();
            let coordinator = Coordinator::default();
            let mut setup = coordinator.prepare(
                &fixture.context,
                bridge,
                PairConfiguration {
                    mode: PairMode::Reconcile,
                    conflict_preference: ConflictPreference::Editor,
                    resolution_preference: Some(ConflictPreference::Editor),
                    decisions: Default::default(),
                    runtime_settings: Map::new(),
                },
            )?;
            let mut record = load_record(&fixture.context, &setup.key)?.unwrap();
            // A pairing that has no baseline yet gets one from this reconcile.
            record.baseline = if remote_edit || unreported {
                Some(StoredSnapshot::write(root, &setup.key, &baseline)?)
            } else {
                None
            };
            // What an earlier comparison left open is settled by this one.
            record
                .differences
                .push("push: an earlier difference".into());
            record.resolution_required = true;
            write_record(&fixture.context, &setup.key, &record)?;
            MUTATION.with(|slot| *slot.borrow_mut() = Some(Box::new(|_, _| {
                let mut committed = summary(false);
                committed.insert("supplementalImportPendingWrites".into(), json!(1));
                committed.insert("supplementalImportPendingFields".into(), json!([{
                    "service":"ReplicatedStorage","settingsId":"part","className":"Part",
                    "pathSegments":["ReplicatedStorage","Part"],"pathOrdinals":[1,1],"properties":{"Transparency":0.75}
                }]));
                Ok(StagedPushResult { generated: ProjectSnapshot::default(), summary: committed })
            })));
            let result = coordinator.reconcile_with_gate_held(&fixture.context, bridge, &mut setup);
            if unreported {
                assert!(result.is_err(), "unreported loss was hidden");
                continue;
            }
            result.with_context(|| format!("pending reconcile remote_edit={remote_edit}"))?;
            assert!(setup.error.as_ref().is_some_and(|message| message.contains("pending") && message.contains("Retry")));
            assert!(setup.unsynced_paths.contains(&store));
            let files = decode_settings_bytecode(&fs::read(root.join(&store))?)?;
            assert_eq!(files.instances[2].properties["Transparency"], 0.75);
            assert_eq!(files.instances[2].properties["CanCollide"], remote_edit);
            let record = load_record(&fixture.context, &setup.key)?.unwrap();
            assert!(record.differences.is_empty() && record.conflicts.is_empty());
            assert!(!record.resolution_required && !setup.resolution_required);
            let saved = record.baseline.unwrap().load(root, &setup.key)?;
            assert!(
                snapshot_path_differences(&saved, &actual, &HashSet::from([store.clone()]))?
                    .is_empty()
            );
            let undo = root.join(".renium/editor-history/sync");
            assert!(
                fs::read_dir(undo)?
                    .filter_map(std::result::Result::ok)
                    .any(|entry| entry.path().join("sync.rmp").is_file())
            );
        }
    }
    Ok(())
}

#[test]
fn incomplete_offline_reopen_retains_undo_and_does_not_read_the_retired_runtime() -> Result<()> {
    let fixture = Fixture::new()?;
    MUTATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(|_, _| {
            let mut summary = summary(false);
            summary.insert(
                "protectedOfflineApply".into(),
                json!({
                    "committed":true, "recoveryPending":true, "published":true,
                    "reopenedPath":"local.rbxl", "error":"Studio did not reconnect in 90 s",
                    "nextStep":"Reconnect the published local place and retry the push",
                }),
            );
            Ok(StagedPushResult {
                generated: ProjectSnapshot::default(),
                summary,
            })
        }));
    });
    CAPTURES.with(|slot| {
        slot.borrow_mut()
            .push_back(Box::new(|_| panic!("readback used the retired runtime")))
    });
    let summary = fixture.run()?;
    assert!(summary["historyId"].is_string());
    assert_eq!(summary["verified"], false);
    assert_eq!(summary["protectedOfflineApply"]["recoveryPending"], true);
    assert_eq!(
        fs::read_dir(Path::new(&fixture.context.root).join(".renium/editor-history/sync"))?.count(),
        1
    );
    Ok(())
}

#[test]
fn offline_commit_keeps_undo_receipt_and_reopened_readback_when_saved_fields_are_busy() -> Result<()>
{
    let fixture = Fixture::for_service("Workspace")?;
    let root = Path::new(&fixture.context.root);
    let store = fixture
        .desired
        .entries
        .keys()
        .find(|path| path.extension().is_some_and(|ext| ext == "renium"))
        .context("missing fixture store")?
        .clone();
    let mut document =
        crate::settings::bytecode::decode_settings_bytecode(&fs::read(root.join(&store))?)?;

    document.instances[0]
        .properties
        .insert("StreamingMinRadius".into(), json!(64));
    let stage =
        ExportProjectStage::create_for_comparison(root, Path::new("src"), &["Workspace".into()])?;
    let mut before = capture_snapshot(root, stage.publish_paths())?;
    before.entries.insert(
        store.clone(),
        SnapshotEntry::File(encode_settings_bytecode(&document)?),
    );
    document.instances[0]
        .properties
        .insert("StreamingMinRadius".into(), json!(128));
    document.instances[1].name = "Renamed".into();
    fs::write(root.join(&store), encode_settings_bytecode(&document)?)?;
    let desired = capture_snapshot(root, stage.publish_paths())?;
    replace_capture(before);
    let bridge = fixture.bridge.as_ref().unwrap();
    let reopened = fixture_runtime(bridge, "saved-reopened", Arc::default());
    bridge.pin_runtime(crate::studio::bridge::BridgeTarget::Edit, "push-fixture");
    let busy = bridge
        .try_acquire_runtime_gate("saved-reopened", "test")
        .unwrap();
    let receipt =
        json!({"ok":true,"previousRuntimeId":"push-fixture","reopenedRuntimeId":"saved-reopened"});
    let sent_receipt = receipt.clone();
    MUTATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_, request| {
            assert_eq!(request.args.saved_fields.as_ref().unwrap().fields.len(), 1);
            let mut summary = summary(false);
            summary.insert("protectedOfflineApply".into(), sent_receipt);
            Ok(StagedPushResult {
                generated: ProjectSnapshot::default(),
                summary,
            })
        }));
    });
    CAPTURES.with(|slot| {
        slot.borrow_mut().push_back(Box::new(move |context| {
            assert_eq!(context.runtime_id.as_deref(), Some("saved-reopened"));
            Ok(desired)
        }))
    });
    let result = fixture.run()?;
    assert_eq!(result["protectedOfflineApply"], receipt);
    assert!(
        result["unsupportedProperties"][0]
            .as_str()
            .is_some_and(|s| s.contains("Workspace.StreamingMinRadius") && s.contains("busy"))
    );
    let entry = result["historyId"].as_str().unwrap();
    assert!(
        root.join(".renium/editor-history/sync")
            .join(entry)
            .join("sync.rmp")
            .is_file()
    );
    drop(busy);
    drop(fixture);
    reopened.join().unwrap();
    Ok(())
}

#[test]
fn push_coordinator_precommit_failures_never_create_a_journal() -> Result<()> {
    for reason in [
        "client cancelled",
        "review timed out; nothing was applied",
        "skipped at the Studio review",
        "changed while its Studio update was being prepared",
        "protected writes rolled back",
        "WebSocket closed",
    ] {
        let fixture = Fixture::new()?;
        MUTATION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |context, request| {
                assert!(
                    request
                        .plan
                        .initial_native_services
                        .contains("ReplicatedStorage")
                );
                assert!(
                    !import_settle::pending(context),
                    "uncommitted import was journaled"
                );
                bail!("{reason}")
            }))
        });
        assert!(fixture.run().unwrap_err().to_string().contains(reason));
        assert!(!import_settle::pending(&fixture.context));
        assert!(
            import_settle::before_request(
                &fixture.context,
                fixture.bridge.as_ref().unwrap(),
                crate::automation::op::PUSH,
                &json!({})
            )?
            .is_none()
        );
    }
    Ok(())
}

#[test]
fn synchronous_generated_settings_and_readback_failure_leave_no_journal() -> Result<()> {
    let fixture = Fixture::new()?;
    let generated = fixture.desired.clone();
    MUTATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_, _| {
            Ok(StagedPushResult {
                generated,
                summary: summary(false),
            })
        }))
    });
    CAPTURES.with(|slot| {
        slot.borrow_mut().push_back(Box::new(|context| {
            assert!(
                !import_settle::pending(context),
                "synchronous fallback cannot leave a recovery journal"
            );
            Ok(ProjectSnapshot::default()) // Real missing-instance verification failure.
        }))
    });
    assert!(fixture.run().is_err());
    assert!(!import_settle::pending(&fixture.context));
    assert!(
        import_settle::before_request(
            &fixture.context,
            fixture.bridge.as_ref().unwrap(),
            crate::automation::op::PUSH,
            &json!({})
        )?
        .is_none()
    );
    Ok(())
}

#[test]
fn early_reply_readback_mismatch_is_recorded_and_repush_remains_available() -> Result<()> {
    let fixture = Fixture::new()?;
    MUTATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(|_, _| {
            Ok(StagedPushResult {
                generated: ProjectSnapshot::default(),
                summary: summary(true),
            })
        }))
    });
    CAPTURES.with(|slot| {
        slot.borrow_mut().push_back(Box::new(|context| {
            assert!(
                !crate::daemon::can_respond_before_settle(),
                "the verified reply must already have left"
            );
            assert!(import_settle::pending(context));
            Ok(ProjectSnapshot::default())
        }))
    });
    let error = fixture.run().unwrap_err().to_string();
    assert!(error.contains("retain"), "{error}");
    assert!(import_settle::pending(&fixture.context));
    assert!(
        import_settle::before_request(
            &fixture.context,
            fixture.bridge.as_ref().unwrap(),
            crate::automation::op::PUSH,
            &json!({})
        )?
        .is_none()
    );
    assert!(!import_settle::pending(&fixture.context));
    Ok(())
}

#[test]
fn source_edit_after_the_early_reply_is_never_published_over() -> Result<()> {
    let fixture = Fixture::new()?;
    let desired = fixture.desired.clone();
    MUTATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(|_, _| {
            Ok(StagedPushResult {
                generated: ProjectSnapshot::default(),
                summary: summary(true),
            })
        }))
    });
    CAPTURES.with(|slot| {
        slot.borrow_mut().push_back(Box::new(move |context| {
            assert!(!crate::daemon::can_respond_before_settle());
            fs::write(
                Path::new(&context.source).join("ReplicatedStorage/New.luau"),
                "return 42",
            )?;
            Ok(desired)
        }))
    });
    fixture.run()?;
    assert_eq!(
        fs::read_to_string(Path::new(&fixture.context.source).join("ReplicatedStorage/New.luau"))?,
        "return 42"
    );
    assert!(!import_settle::pending(&fixture.context));
    Ok(())
}

/// What a Live Sync start left: its outcome, the pair's setup and record,
/// the last-sync copy and the files.
#[derive(PartialEq)]
struct Started {
    outcome: String,
    setup: String,
    record: String,
    last_sync: Option<ProjectSnapshot>,
    files: ProjectSnapshot,
}

impl Started {
    fn describe(&self) -> String {
        format!(
            "{} | {} | {} | last sync {:?} | {} files",
            self.outcome,
            self.setup,
            self.record,
            self.last_sync
                .as_ref()
                .map(|snapshot| snapshot.entries.len()),
            self.files.entries.len()
        )
    }
}

/// One Live Sync start of a fresh fixture whose store holds a Block at
/// transparency 0.25, with or without a held baseline of `held`'s kind:
/// `exact` (a pull's, Studio and the files as it wrote them), `moved`
/// (Studio moved since its proof), `pushed` (its files are what a push
/// sent), `other` (kept for another push), `files` (the files changed
/// since), `paths` (it read fewer paths than the comparison), or `links`
/// (a link manifest folds into the comparison). `record` is the pair's
/// last sync: `none`, `same` as the files, `stale`, or `review` (with a
/// difference open). Also whether Studio was captured and when a baseline
/// held after ends.
fn live_start(
    held: &str,
    record_kind: &str,
    mode: PairMode,
    hold: bool,
) -> Result<(Started, bool, Option<Instant>)> {
    let mut fixture = Fixture::new()?;
    fixture.context.experience.clone_from(&fixture.context.root);
    let context = &fixture.context;
    let root = Path::new(&context.root);
    let bridge = fixture.bridge.as_ref().unwrap();
    let store = fixture
        .desired
        .entries
        .keys()
        .find(|path| path.extension().is_some_and(|ext| ext == "renium"))
        .unwrap()
        .clone();
    let paths = project_comparison_stage(context, &sync_services())?
        .publish_paths()
        .to_vec();
    let mut document = decode_settings_bytecode(&fs::read(root.join(&store))?)?;
    document.instances.push(SettingsBytecodeInstance::new(
        "block".into(),
        "Block".into(),
        "Part".into(),
        Some(0),
    ));
    let mut files_at = |transparency: f64| -> Result<ProjectSnapshot> {
        document.instances[2]
            .properties
            .insert("Transparency".into(), json!(transparency));
        fs::write(root.join(&store), encode_settings_bytecode(&document)?)?;
        capture_snapshot(root, &paths)
    };
    let stale = files_at(0.5)?;
    let moved = files_at(0.75)?;
    let files = files_at(0.25)?;
    let workspace = Path::new(&context.source).join("Workspace");
    fs::create_dir_all(&workspace)?;
    fs::write(workspace.join("Extra.luau"), "return 1")?;
    let extra = capture_snapshot(root, &paths)?;
    fs::remove_dir_all(&workspace)?;
    let coordinator = Coordinator::default();
    let mut setup = coordinator.prepare(
        context,
        bridge,
        PairConfiguration {
            mode,
            conflict_preference: ConflictPreference::None,
            resolution_preference: None,
            decisions: BTreeMap::new(),
            runtime_settings: Map::new(),
        },
    )?;
    let mut record = load_record(context, &setup.key)?.unwrap();
    record.baseline = match record_kind {
        "none" => None,
        "stale" => Some(StoredSnapshot::write(root, &setup.key, &stale)?),
        _ => Some(StoredSnapshot::write(root, &setup.key, &files)?),
    };
    if record_kind == "review" {
        record
            .differences
            .push("push: an earlier difference".into());
        record.resolution_required = true;
    }
    write_record(context, &setup.key, &record)?;
    if held == "links" {
        fs::write(
            root.join("renium-link.json"),
            r#"{"version":1,"links":[{"id":"other","readOnly":true,"source":{"type":"local","path":"links/missing.renium"},"targets":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","Other"]}]}],"broken":[]}"#,
        )?;
    }
    let (held_project, held_paths) = match held {
        "files" => (stale.clone(), paths.clone()),
        "paths" => {
            let read = paths
                .iter()
                .filter(|path| !path.to_string_lossy().contains("Workspace"))
                .cloned()
                .collect::<Vec<_>>();
            assert!(read.len() < paths.len(), "the comparison reads Workspace");
            (files.clone(), read)
        }
        _ => (files.clone(), paths.clone()),
    };
    if hold {
        let input = if held == "other" {
            json!({"other": true})
        } else {
            plain_push_input(&automation_push_args(context, &json!({}), false)?)?
        };
        bridge.verified_full_pushes.lock_recover().insert(
            "push-fixture".into(),
            VerifiedFullPush {
                connections: bridge.runtime_connection_signature("push-fixture"),
                input,
                project: held_project,
                paths: held_paths,
                proof: if held == "moved" {
                    json!({"expired": true})
                } else {
                    json!({"id": 1})
                },
                expires: Instant::now() + Duration::from_secs(600),
                sourcemap: (held != "pushed").then(Box::default),
                #[cfg(windows)]
                relay: None,
            },
        );
    }
    replace_capture(match held {
        "exact" | "links" => files,
        "files" => stale,
        "paths" => extra,
        _ => moved,
    });
    let outcome = coordinator.reconcile_with_gate_held(context, bridge, &mut setup);
    let captured = CAPTURES.with(|slot| slot.borrow().is_empty());
    let kept = bridge
        .verified_full_pushes
        .lock_recover()
        .get("push-fixture")
        .map(|entry| entry.expires);
    let anonymous = |text: &str| text.replace(&context.root, "<root>");
    // When a file was written differs between two runs.
    let unstamped = |conflicts: &[Conflict]| {
        conflicts
            .iter()
            .cloned()
            .map(|mut conflict| {
                conflict.files_changed_at = None;
                conflict
            })
            .collect::<Vec<_>>()
    };
    let record = load_record(context, &setup.key)?.unwrap();
    let started = Started {
        outcome: outcome.map_or_else(|error| anonymous(&format!("{error:#}")), |()| "ok".into()),
        setup: format!(
            "{:?} {:?} {} {:?} {:?} {:?} {}",
            setup.mode,
            setup.error.as_deref().map(anonymous),
            setup.resolution_required,
            unstamped(&setup.conflicts),
            setup.differences,
            setup.unsynced_paths,
            setup.moot_decisions.len()
        ),
        record: format!(
            "{:?} {} {:?} {:?} {}",
            record.mode,
            record.resolution_required,
            unstamped(&record.conflicts),
            record.differences,
            record.studio_checkpoint.is_some()
        ),
        last_sync: record
            .baseline
            .map(|baseline| baseline.load(root, &setup.key))
            .transpose()?,
        files: capture_snapshot(root, &paths)?,
    };
    Ok((started, captured, kept))
}

/// A Live Sync start under a held baseline leaves the pair as a start that
/// captures Studio leaves it. Only a pull's baseline that Studio and the
/// files still hold spares the capture, and is held again for as long as a
/// new one; every other stays held as it was.
#[test]
fn a_live_start_from_a_held_pull_leaves_what_a_capture_leaves() -> Result<()> {
    for held in [
        "exact", "moved", "pushed", "other", "files", "paths", "links",
    ] {
        let (modes, records): (&[PairMode], &[&str]) = if held == "exact" {
            (
                &[PairMode::Reconcile, PairMode::Verify],
                &["none", "same", "stale", "review"],
            )
        } else {
            (&[PairMode::Verify], &["none", "same"])
        };
        for &mode in modes {
            for &record in records {
                let what = format!("{held} baseline, {record} record, {mode:?}");
                let (started, captured, kept) = live_start(held, record, mode, true)?;
                let (oracle, _, _) = live_start(held, record, mode, false)?;
                assert!(
                    started == oracle,
                    "{what}: {}\nagainst a capture: {}",
                    started.describe(),
                    oracle.describe()
                );
                assert_eq!(captured, held != "exact", "{what}: Studio captured");
                let kept = kept.with_context(|| format!("{what}: no baseline held after"))?;
                assert_eq!(
                    kept > Instant::now() + Duration::from_secs(900),
                    held == "exact",
                    "{what}: baseline renewed"
                );
            }
        }
    }
    Ok(())
}

const SETTLE_SERVICES: [&str; 2] = ["ReplicatedStorage", "ServerStorage"];
const SETTLE_SCRIPT: &str = "src/ServerStorage/Foo.luau";

/// What a push that imports ReplicatedStorage finds of ServerStorage, which
/// it writes nothing to. A pull of both wrote the files, a script among
/// them, and Renium holds them as its baseline.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Elsewhere {
    /// Studio's copy is the files'.
    Untouched,
    /// Studio edits it during the import, which moves its generation.
    Edited,
    /// Studio edits it during the import where no event tracks it, as with
    /// Terrain voxels, and says it differs from the held proof there.
    EditedUnseen,
    /// As `EditedUnseen`, where Studio cannot say where it differs.
    Unscoped,
    /// Studio edits it once the push committed, while a push that answers
    /// only after its settlement reads Studio back.
    EditedLate,
    /// The import moves the target of its reference into ReplicatedStorage.
    Referring,
    /// Its store is the older encoder's spelling of Studio's copy, under a
    /// baseline a push kept: that says what Studio holds, not how a pull
    /// spells it.
    Respelled,
    /// Its store is the older encoder's spelling of Studio's copy, which the
    /// pull found alike and left as it was.
    Unwitnessed,
    /// Its store is edited in the files during the import.
    EditedFile,
    /// No baseline is held, and Studio edits it during the import where no
    /// event tracks it.
    Fresh,
    /// A tool rewrote the sourcemap after the pull.
    Remapped,
    /// A tool rewrites the sourcemap once the push committed, as the push
    /// asks Studio of its commit before it answers, which a push that
    /// answers only after its settlement never does.
    RemappedLate,
    /// Its store is edited in the files once the settlement wrote Studio's
    /// copy.
    EditedAfterSwaps,
    /// Studio edits it once the settlement wrote Studio's copy.
    ChangedAfterSwaps,
    /// ReplicatedStorage's store is saved after the settlement found the
    /// files unchanged and before it moves that store aside.
    SavedDuringSwap,
    /// A push of its script, which the editor saved with a final line break,
    /// kept the baseline, and Studio holds the script without that break.
    PushedScript,
}

/// How the push writes ReplicatedStorage, where Studio generates an
/// identity for the folder it adds.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Import {
    /// It imports the whole service.
    Whole,
    /// The pull left an instance there, so it imports the folder's root
    /// beside it, with a field receipt.
    AddedRoot,
    /// As `AddedRoot`, where the importer replaces the service whole all the
    /// same, for a saved field no setter writes, with its loader receipt.
    Replaced,
    /// As `Whole`, where the push also adds roots to ServerStorage, with a
    /// field receipt.
    Mixed,
}

impl Import {
    /// Whether the pull left an instance in ReplicatedStorage.
    fn beside_kept(self) -> bool {
        matches!(self, Self::AddedRoot | Self::Replaced)
    }
}

impl Elsewhere {
    const ALL: [Self; 16] = [
        Self::Untouched,
        Self::Edited,
        Self::EditedUnseen,
        Self::Unscoped,
        Self::EditedLate,
        Self::Referring,
        Self::Respelled,
        Self::Unwitnessed,
        Self::EditedFile,
        Self::Fresh,
        Self::Remapped,
        Self::RemappedLate,
        Self::EditedAfterSwaps,
        Self::ChangedAfterSwaps,
        Self::SavedDuringSwap,
        Self::PushedScript,
    ];

    /// Whether the pull left the older encoder's store.
    fn respelled(self) -> bool {
        matches!(self, Self::Respelled | Self::Unwitnessed)
    }

    /// Where Studio says it differs from the held proof once the push
    /// committed.
    fn scope(self) -> Option<Vec<&'static str>> {
        match self {
            Self::Edited | Self::EditedUnseen => Some(SETTLE_SERVICES.to_vec()),
            Self::Unscoped => None,
            _ => Some(SETTLE_SERVICES[..1].to_vec()),
        }
    }
}

/// ServerStorage as the pull wrote it, as Studio holds it before the push
/// and as Studio holds it after the import.
struct Documents {
    pulled: SettingsBytecode,
    before: SettingsBytecode,
    after: SettingsBytecode,
}

fn elsewhere_documents(seed: u64, elsewhere: Elsewhere) -> Documents {
    let mut rng = crate::tests::support::SimRng(seed);
    let mut pulled = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "root".into(),
            "ServerStorage".into(),
            "ServerStorage".into(),
            None,
        )],
    };
    for index in 0..1 + rng.below(4) {
        let mut value = SettingsBytecodeInstance::new(
            format!("item{index}"),
            format!("Item{index}"),
            "StringValue".into(),
            Some(0),
        );
        value.properties.insert(
            "Value".into(),
            json!(format!("s{seed}v{}", rng.below(1000))),
        );
        pulled.instances.push(value);
    }
    let reference = |ordinal: u64| {
        json!({
            "_type": "Ref",
            "settingsId": "folder",
            "pathSegments": ["ReplicatedStorage", "Imported"],
            "pathOrdinals": [1, ordinal],
        })
    };
    if elsewhere == Elsewhere::Referring {
        let mut link = SettingsBytecodeInstance::new(
            "link".into(),
            "Link".into(),
            "ObjectValue".into(),
            Some(0),
        );
        link.properties.insert("Value".into(), reference(1));
        pulled.instances.push(link);
    }
    let before = pulled.clone();
    let mut after = before.clone();
    match elsewhere {
        Elsewhere::Edited
        | Elsewhere::EditedUnseen
        | Elsewhere::Unscoped
        | Elsewhere::EditedLate
        | Elsewhere::Fresh => {
            let index = 1 + rng.below(after.instances.len() - 1);
            after.instances[index]
                .properties
                .insert("Value".into(), json!(format!("edited{seed}")));
        }
        Elsewhere::Referring => {
            let link = after.instances.last_mut().unwrap();
            link.properties.insert("Value".into(), reference(2));
        }
        _ => {}
    }
    Documents {
        pulled,
        before,
        after,
    }
}

/// The sourcemap entry a capture writes for a service holding `document`.
fn sourcemap_node(document: &SettingsBytecode) -> Value {
    let (root, children) = document
        .instances
        .split_first()
        .expect("a store holds its service");
    json!({
        "name": root.name,
        "className": root.class_name,
        "children": children
            .iter()
            .map(|child| json!({"name": child.name, "className": child.class_name}))
            .collect::<Vec<_>>(),
    })
}

/// The sourcemap `bytes` with `nodes` over the entries of their services.
fn with_sourcemap_nodes(bytes: Option<&[u8]>, nodes: &[Value]) -> Result<Vec<u8>> {
    let mut map = bytes
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .unwrap_or_else(|| json!({"name": "Game", "className": "DataModel", "children": []}));
    let children = map["children"]
        .as_array_mut()
        .context("the sourcemap has no services")?;
    for node in nodes {
        match children
            .iter_mut()
            .find(|child| child["name"] == node["name"])
        {
            Some(child) => child.clone_from(node),
            None => children.push(node.clone()),
        }
    }
    Ok(serde_json::to_vec(&map)?)
}

fn write_sourcemap_nodes(path: &Path, nodes: &[Value]) -> Result<()> {
    let map = with_sourcemap_nodes(fs::read(path).ok().as_deref(), nodes)?;
    fs::write(path, map)?;
    Ok(())
}

/// A baseline's files and the sourcemap it holds them with.
type Kept = (ProjectSnapshot, Option<Box<SourcemapState>>);

struct Settled {
    files: ProjectSnapshot,
    sourcemap: Vec<u8>,
    read_back: Vec<String>,
    baseline: Option<Kept>,
    /// What a whole capture of Studio writes after the push, and the
    /// sourcemap it writes.
    whole: (ProjectSnapshot, Vec<u8>),
    /// The files as the push found them.
    pushed: ProjectSnapshot,
    /// ServerStorage's store as an edit in the files during the import
    /// leaves it.
    file_edit: (PathBuf, SnapshotEntry),
    /// ReplicatedStorage's store as a save during the settlement's swaps
    /// leaves it.
    saved: (PathBuf, SnapshotEntry),
    /// How often the push asked Studio whether it holds the commit's proof,
    /// and where it differs from the held one.
    asks: (usize, usize),
    /// How often it compared the files with Studio's to find the services
    /// its settlement reads back.
    compared: usize,
    /// The push's error, and the error its unsettled journal holds.
    failed: Option<String>,
    unsettled: Option<String>,
}

/// Files as a pull of both services leaves them: what it wrote, where, and
/// the sourcemap it wrote.
struct Pulled {
    paths: Vec<PathBuf>,
    held: ProjectSnapshot,
    sourcemap: Vec<u8>,
}

/// Pulls ReplicatedStorage as `replicated` and ServerStorage as `documents`
/// say.
fn pull_settle_services(
    root: &Path,
    (imported, other): (&Path, &Path),
    (replicated, documents): (&SettingsBytecode, &Documents),
    elsewhere: Elsewhere,
) -> Result<Pulled> {
    let services = SETTLE_SERVICES.map(String::from);
    let pulled = if elsewhere.respelled() {
        crate::settings::bytecode::encode_settings_bytecode_v11(&documents.pulled)?
    } else {
        encode_settings_bytecode(&documents.pulled)?
    };
    fs::write(root.join(imported), encode_settings_bytecode(replicated)?)?;
    fs::write(root.join(other), pulled)?;
    fs::create_dir_all(root.join("src/ServerStorage"))?;
    fs::write(root.join(SETTLE_SCRIPT), "return 1")?;
    let sourcemap = root.join("sourcemap.json");
    write_sourcemap_nodes(
        &sourcemap,
        &[
            sourcemap_node(replicated),
            sourcemap_node(&documents.pulled),
        ],
    )?;
    if elsewhere != Elsewhere::Unwitnessed {
        crate::snapshot::import::remember_pulled_encoding(
            &fs::read(root.join(other))?,
            "ServerStorage",
        )?;
    }
    let paths = ExportProjectStage::create_for_comparison(root, Path::new("src"), &services)?
        .publish_paths()
        .to_vec();
    Ok(Pulled {
        held: capture_snapshot(root, &paths)?,
        sourcemap: fs::read(sourcemap)?,
        paths,
    })
}

/// A whole push that imports into ReplicatedStorage as `import` says after
/// a pull of both services, into a Studio holding ServerStorage as
/// `documents` say. In `full`, Studio cannot say where it differs from the
/// held proof, so every service settles. A `synchronous` push has no caller
/// to answer early, so it settles before it answers.
fn settle_import(
    (import, elsewhere): (Import, Elsewhere),
    documents: &Documents,
    full: bool,
    synchronous: bool,
) -> Result<Settled> {
    let fixture = Fixture::new()?;
    let bridge = fixture
        .bridge
        .as_ref()
        .context("the fixture has no bridge")?;
    let root = PathBuf::from(&fixture.context.root);
    let services = SETTLE_SERVICES.map(String::from);
    let imported = fixture
        .desired
        .entries
        .keys()
        .find(|path| path.extension().is_some_and(|ext| ext == "renium"))
        .context("missing fixture store")?
        .clone();
    let other = PathBuf::from(
        imported
            .to_string_lossy()
            .replace("ReplicatedStorage", "ServerStorage"),
    );
    fs::create_dir_all(root.join(other.parent().context("store has no parent")?))?;
    let mut imported_store = fs::read(root.join(&imported))?;
    let mut replicated = SettingsBytecode {
        version: SETTINGS_BINARY_VERSION,
        instances: vec![SettingsBytecodeInstance::new(
            "root".into(),
            "ReplicatedStorage".into(),
            "ReplicatedStorage".into(),
            None,
        )],
    };
    if import.beside_kept() {
        let kept =
            SettingsBytecodeInstance::new("kept".into(), "Kept".into(), "Folder".into(), Some(0));
        replicated.instances.push(kept.clone());
        let mut desired = crate::settings::bytecode::decode_settings_bytecode(&imported_store)?;
        desired.instances.insert(1, kept);
        imported_store = encode_settings_bytecode(&desired)?;
    }
    REQUESTED.with(|slot| slot.borrow_mut().clear());
    verify::SETTLED_COMPARISONS.with(|count| count.set(0));
    let Pulled {
        paths,
        held,
        sourcemap,
    } = pull_settle_services(
        &root,
        (&imported, &other),
        (&replicated, documents),
        elsewhere,
    )?;
    let held_sourcemap =
        crate::snapshot::export::collect_publish_hashes(&root, &[PathBuf::from("sourcemap.json")])?;
    let remapped = with_sourcemap_nodes(
        Some(&sourcemap),
        &[json!({
            "name": "ServerStorage", "className": "ServerStorage",
            "filePaths": ["src/ServerStorage"], "children": [],
        })],
    )?;
    if elsewhere == Elsewhere::Remapped {
        fs::write(root.join("sourcemap.json"), &remapped)?;
    }
    fs::write(root.join(&imported), &imported_store)?;
    let pushed = capture_snapshot(&root, &paths)?;
    let mut readback = pushed.clone();
    let mut generated = crate::settings::bytecode::decode_settings_bytecode(&imported_store)?;
    generated
        .instances
        .iter_mut()
        .find(|instance| instance.name == "Imported")
        .context("the push adds no folder")?
        .properties
        .insert(
            "UniqueId".into(),
            json!({"_type": "UniqueId", "value": format!("{:032x}", 7)}),
        );
    readback.entries.insert(
        imported.clone(),
        SnapshotEntry::File(encode_settings_bytecode(&generated)?),
    );
    readback.entries.insert(
        other.clone(),
        SnapshotEntry::File(encode_settings_bytecode(&documents.after)?),
    );
    if elsewhere == Elsewhere::PushedScript {
        readback.entries.insert(
            SETTLE_SCRIPT.into(),
            SnapshotEntry::File(b"return 2".to_vec()),
        );
    }
    let nodes = [
        (
            SETTLE_SERVICES[0],
            sourcemap_node(&crate::settings::bytecode::decode_settings_bytecode(
                &imported_store,
            )?),
        ),
        (SETTLE_SERVICES[1], sourcemap_node(&documents.after)),
    ];
    let whole_sourcemap = with_sourcemap_nodes(
        Some(&sourcemap),
        &nodes
            .iter()
            .map(|(_, node)| node.clone())
            .collect::<Vec<_>>(),
    )?;
    let whole = readback.clone();
    CAPTURES.with(|slot| slot.borrow_mut().clear());
    if elsewhere == Elsewhere::Fresh {
        // Studio's capture before the push reads nothing back.
        CAPTURES.with(|slot| {
            slot.borrow_mut().push_back(Box::new(|_| {
                REQUESTED.with(|slot| slot.borrow_mut().clear());
                Ok(held)
            }))
        });
    } else {
        let mut baseline = VerifiedFullPush {
            connections: bridge.runtime_connection_signature("push-fixture"),
            input: full_push_cache_input(&fixture.push_args(), &services)?,
            project: held,
            paths: paths.clone(),
            proof: json!({"held": true}),
            expires: Instant::now() + Duration::from_secs(60),
            sourcemap: (elsewhere != Elsewhere::Respelled).then(|| Box::new(held_sourcemap)),
            #[cfg(windows)]
            relay: None,
        };
        if elsewhere == Elsewhere::PushedScript {
            fs::write(root.join(SETTLE_SCRIPT), "return 2\n")?;
            let mut pushed = baseline.project.clone();
            pushed.entries.insert(
                SETTLE_SCRIPT.into(),
                SnapshotEntry::File(b"return 2\n".to_vec()),
            );
            let verified = Map::from_iter([("sourceInexact".to_string(), json!(1))]);
            baseline.sourcemap = sourcemap_after_push(&baseline, &pushed, &verified);
            baseline.project = pushed;
        }
        keep_baseline(bridge, "push-fixture", baseline, "a pull");
    }
    let mut edited = documents.before.clone();
    edited.instances[1]
        .properties
        .insert("Value".into(), json!("file edit"));
    let file_edit = (other.clone(), encode_settings_bytecode(&edited)?);
    let edit = (root.join(&other), file_edit.1.clone());
    let saved = (imported.clone(), encode_settings_bytecode(&replicated)?);
    let tracker = Arc::clone(&fixture.tracker);
    CAPTURES.with(|slot| {
        slot.borrow_mut().push_back(Box::new(move |context| {
            anyhow::ensure!(
                import_settle::pending(context) != synchronous,
                "the settlement ran on the other path"
            );
            // A daemon that stops during it reads the same of the journal.
            if !synchronous {
                let journal =
                    fs::read_dir(Path::new(&context.root).join(".renium/import-settlement"))?
                        .next()
                        .context("no settlement journal")??
                        .path();
                let record: Value = rmp_serde::from_slice(&fs::read(journal.join("pending.rmp"))?)?;
                anyhow::ensure!(
                    record["optional"] == (import == Import::AddedRoot),
                    "the journal is optional: {}",
                    record["optional"]
                );
                // Nothing loads an optional record's copies of the files.
                anyhow::ensure!(
                    record["optional"] == false || !journal.join("packs").exists(),
                    "the optional journal copied the files"
                );
            }
            {
                let mut tracker = tracker.lock().unwrap();
                if let Some((path, bytes)) = tracker.edit.take() {
                    fs::write(path, bytes)?;
                }
                if elsewhere == Elsewhere::EditedLate {
                    tracker.edits.insert("ServerStorage".into(), json!(1));
                    tracker.changed_since_commit = true;
                }
            }
            let (requested, stage) = last_requested();
            let read = |service: &str| requested.iter().any(|name| name == service);
            readback
                .entries
                .retain(|path, _| of_services(path, &requested));
            write_sourcemap_nodes(
                &stage.join("sourcemap.json"),
                &nodes
                    .iter()
                    .filter(|(service, _)| read(service))
                    .map(|(_, node)| node.clone())
                    .collect::<Vec<_>>(),
            )?;
            Ok(readback)
        }))
    });
    {
        let mut tracker = fixture.tracker.lock().unwrap();
        tracker.edits = crate::roblox::services::DEFAULT_SYNC_SERVICES
            .iter()
            .map(|service| ((*service).to_string(), json!(0)))
            .collect();
        tracker.scope = elsewhere.scope().filter(|_| !full);
        let sourcemap = root.join("sourcemap.json");
        tracker.late = (elsewhere == Elsewhere::RemappedLate)
            .then(|| late(1, move |_| fs::write(sourcemap, remapped).unwrap()))
            .flatten();
        tracker.edit = (elsewhere == Elsewhere::EditedFile).then(|| edit.clone());
        // A push of added roots that reads nothing back still meets the edit.
        tracker.edit_on_release = import == Import::AddedRoot;
    }
    let mut unsaved =
        (elsewhere == Elsewhere::SavedDuringSwap).then(|| (root.join(&saved.0), saved.1.clone()));
    let tracker = Arc::clone(&fixture.tracker);
    let mut after_swaps: Option<Box<dyn FnOnce()>> = match elsewhere {
        Elsewhere::EditedAfterSwaps => Some(Box::new(move || fs::write(edit.0, edit.1).unwrap())),
        Elsewhere::ChangedAfterSwaps => Some(Box::new(move || {
            let mut tracker = tracker.lock().unwrap();
            tracker.edits.insert("ServerStorage".into(), json!(1));
            tracker.changed_since_commit = true;
        })),
        _ => None,
    };
    let store = imported.clone();
    let _hook = hook_publication_steps(move |step| match step {
        PublicationStep::MoveAside(path) if store.starts_with(path) => {
            if let Some((path, bytes)) = unsaved.take() {
                fs::write(path, bytes).unwrap();
            }
        }
        PublicationStep::Swapped => {
            if let Some(change) = after_swaps.take() {
                change();
            }
        }
        _ => {}
    });
    let tracker = Arc::clone(&fixture.tracker);
    MUTATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_, request| {
            anyhow::ensure!(
                request.plan.initial_native_services.is_empty() == import.beside_kept(),
                "the push imports {:?} whole",
                request.plan.initial_native_services
            );
            let mut tracker = tracker.lock().unwrap();
            tracker.committed = true;
            if elsewhere == Elsewhere::Edited {
                tracker.edits.insert("ServerStorage".into(), json!(1));
            }
            let mut committed = summary(true);
            match import {
                Import::Whole => {
                    committed.insert("nativeVerifiedServices".into(), json!(SETTLE_SERVICES));
                }
                Import::AddedRoot => {
                    committed.remove("nativeVerifiedServices");
                    committed.insert("fieldVerifiedServices".into(), json!(SETTLE_SERVICES));
                }
                Import::Replaced => {}
                Import::Mixed => {
                    committed.insert("fieldVerifiedServices".into(), json!(["ServerStorage"]));
                }
            }
            if import != Import::Whole {
                committed.insert("binaryInstances".into(), json!(1));
            }
            Ok(StagedPushResult {
                generated: ProjectSnapshot::default(),
                summary: committed,
            })
        }))
    });
    // Only a settlement of added roots fails a push that answered, where
    // Studio changes during it.
    let fails = import == Import::AddedRoot
        && matches!(
            elsewhere,
            Elsewhere::EditedLate | Elsewhere::ChangedAfterSwaps
        );
    let failed = match fixture.push_with(&services, None, (false, !synchronous)) {
        Ok(_) => None,
        Err(error) if fails => Some(format!("{error:#}")),
        Err(error) => return Err(error),
    };
    let unsettled = if import_settle::pending(&fixture.context) {
        let journal = fs::read_dir(root.join(".renium/import-settlement"))?
            .next()
            .context("no settlement journal")??
            .path();
        let record: Value = rmp_serde::from_slice(&fs::read(journal.join("pending.rmp"))?)?;
        Some(record["error"].as_str().unwrap_or_default().to_string())
    } else {
        None
    };
    anyhow::ensure!(
        unsettled.is_none() || fails,
        "the settlement kept its journal"
    );
    let baseline = bridge
        .verified_full_pushes
        .lock_recover()
        .get("push-fixture")
        .map(|kept| (kept.project.clone(), kept.sourcemap.clone()));
    let (asks, edit_left) = {
        let tracker = fixture.tracker.lock().unwrap();
        ((tracker.proofs, tracker.scopes), tracker.edit.is_some())
    };
    let settled = Settled {
        files: capture_snapshot(&root, &paths)?,
        sourcemap: fs::read(root.join("sourcemap.json"))?,
        read_back: last_requested().0,
        baseline,
        whole: (whole, whole_sourcemap),
        pushed,
        file_edit: (file_edit.0, SnapshotEntry::File(file_edit.1)),
        saved: (saved.0, SnapshotEntry::File(saved.1)),
        asks,
        compared: verify::SETTLED_COMPARISONS.get(),
        failed,
        unsettled,
    };
    if import == Import::AddedRoot && elsewhere == Elsewhere::EditedFile {
        anyhow::ensure!(!edit_left, "the file edit was not made");
        // The next push sends the file edit on the baseline this one kept.
        // It reads Studio only back, where it imports the edited service
        // whole, and Studio then holds what the files hold.
        let read = paths.clone();
        CAPTURES.with(|slot| {
            let mut captures = slot.borrow_mut();
            captures.clear();
            captures.push_back(Box::new(move |context| {
                anyhow::ensure!(
                    MUTATION.with(|slot| slot.borrow().is_none()),
                    "the push after the edit captured Studio"
                );
                let mut held = capture_snapshot(Path::new(&context.root), &read)?;
                let (requested, _) = last_requested();
                held.entries.retain(|path, _| of_services(path, &requested));
                Ok(held)
            }));
        });
        let sent = std::rc::Rc::new(RefCell::new(Vec::new()));
        let plan_paths = std::rc::Rc::clone(&sent);
        MUTATION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |_, request| {
                plan_paths
                    .borrow_mut()
                    .clone_from(&request.plan.changed_paths);
                let mut committed = summary(false);
                committed.insert("fieldVerifiedServices".into(), json!(["ServerStorage"]));
                committed.insert("verifiedPushProof".into(), json!({"test": true}));
                Ok(StagedPushResult {
                    generated: ProjectSnapshot::default(),
                    summary: committed,
                })
            }))
        });
        fixture.push_with(&services, None, (false, true))?;
        anyhow::ensure!(
            MUTATION.with(|slot| slot.borrow().is_none()),
            "the push after the edit sent nothing"
        );
        let sent = sent.borrow();
        anyhow::ensure!(
            sent.iter().any(|path| path.ends_with(&other)),
            "the push after the edit sent {sent:?}, not {}",
            other.display()
        );
    }
    Ok(settled)
}

/// Whether `path` is a file of one of `services`: its store or under its
/// folder.
fn of_services(path: &Path, services: &[String]) -> bool {
    path.components().any(|part| {
        services.iter().any(|service| {
            part.as_os_str() == service.as_str()
                || part.as_os_str() == format!("{service}.renium").as_str()
        })
    })
}

type Late = Option<(usize, Box<dyn FnOnce(&mut Tracker) + Send>)>;

/// `change` made when Studio is asked of the commit's proof for the `at`th
/// time.
fn late(at: usize, change: impl FnOnce(&mut Tracker) + Send + 'static) -> Late {
    Some((at, Box::new(change)))
}

fn store_bytes<'a>(files: &'a ProjectSnapshot, service: &str) -> Option<&'a [u8]> {
    files.entries.iter().find_map(|(path, entry)| match entry {
        SnapshotEntry::File(bytes)
            if path.extension().is_some_and(|ext| ext == "renium")
                && path.to_string_lossy().contains(service) =>
        {
            Some(bytes.as_slice())
        }
        _ => None,
    })
}

/// A settlement that skips services leaves what the full one leaves, and a
/// baseline either keeps is what a whole capture of Studio writes, its
/// sourcemap included.
fn narrowed_like_full(narrow: &Settled, full: &Settled) -> Result<(), String> {
    let differing = narrow
        .files
        .entries
        .keys()
        .chain(full.files.entries.keys())
        .filter(|path| narrow.files.entries.get(*path) != full.files.entries.get(*path))
        .collect::<BTreeSet<_>>();
    if !differing.is_empty() || narrow.sourcemap != full.sourcemap {
        return Err(format!(
            "the settled files differ at {differing:?}, sourcemap equal: {}",
            narrow.sourcemap == full.sourcemap
        ));
    }
    if narrow.baseline != full.baseline {
        return Err(format!(
            "kept a baseline: {} and, in full, {}",
            narrow.baseline.is_some(),
            full.baseline.is_some()
        ));
    }
    kept_as_whole(full)
}

/// A baseline `settled` keeps is what a whole capture of Studio writes, its
/// sourcemap included.
fn kept_as_whole(settled: &Settled) -> Result<(), String> {
    let (whole, sourcemap) = &settled.whole;
    let captured = SourcemapState::from([(
        PathBuf::from("sourcemap.json"),
        crate::snapshot::export::file_publish_state(sourcemap),
    )]);
    match &settled.baseline {
        Some((files, _)) if files != whole => Err(format!(
            "a kept baseline's files are not what a whole capture writes at {:?}",
            files
                .entries
                .keys()
                .chain(whole.entries.keys())
                .filter(|path| files.entries.get(*path) != whole.entries.get(*path))
                .collect::<BTreeSet<_>>()
        )),
        Some((_, kept)) if kept.as_deref() != Some(&captured) => {
            Err("a kept baseline does not hold the sourcemap a whole capture writes".into())
        }
        _ => Ok(()),
    }
}

/// A push that adds roots settles only in the background, from a held pull
/// whose sourcemap the files still hold, where that reads back fewer
/// services than a pull, and asks Studio nothing of its commit for one it
/// cannot settle so. Its files are then what a whole capture writes but for
/// newer edits; otherwise they are as the push found them but for newer
/// edits, and the files the push wrote stay its baseline, with no pull's
/// sourcemap, while Studio holds its commit. A Studio change during it fails
/// it, which leaves it unsettled only once it wrote Studio's copy.
fn added_roots_left(
    (import, elsewhere, synchronous): (Import, Elsewhere, bool),
    narrow: &Settled,
    full: &Settled,
) -> Result<(), String> {
    use Elsewhere::*;
    for (settled, full) in [(narrow, false), (full, true)] {
        let reads = import == Import::AddedRoot
            && !synchronous
            && !full
            && matches!(
                elsewhere,
                Untouched
                    | EditedLate
                    | EditedFile
                    | EditedAfterSwaps
                    | ChangedAfterSwaps
                    | SavedDuringSwap
            );
        let publishes =
            reads && matches!(elsewhere, Untouched | EditedAfterSwaps | ChangedAfterSwaps);
        let stopped = reads && matches!(elsewhere, EditedAfterSwaps | ChangedAfterSwaps);
        let failed = reads && matches!(elsewhere, EditedLate | ChangedAfterSwaps);
        // Only a settlement that wrote Studio's copy leaves one unsettled.
        let unsettled = reads && elsewhere == ChangedAfterSwaps;
        let studio_changed = |error: &Option<String>| {
            error
                .as_deref()
                .is_some_and(|error| error.contains("Studio changed during import settlement"))
        };
        if (settled.failed.is_some(), settled.unsettled.is_some()) != (failed, unsettled)
            || failed && !studio_changed(&settled.failed)
            || unsettled && !studio_changed(&settled.unsettled)
        {
            return Err(format!(
                "the push failed with {:?}, its journal holds {:?}, full={full}",
                settled.failed, settled.unsettled
            ));
        }
        let expected: &[&str] = if reads { &SETTLE_SERVICES[..1] } else { &[] };
        if settled.read_back != expected {
            return Err(format!("read back {:?}, full={full}", settled.read_back));
        }
        let mut wanted = if publishes {
            settled.whole.0.clone()
        } else {
            settled.pushed.clone()
        };
        let edits = [
            (
                import == Import::AddedRoot && elsewhere == EditedFile,
                &settled.file_edit,
            ),
            (reads && elsewhere == EditedAfterSwaps, &settled.file_edit),
            (reads && elsewhere == SavedDuringSwap, &settled.saved),
        ];
        for (_, (path, entry)) in edits.into_iter().filter(|(made, _)| *made) {
            wanted.entries.insert(path.clone(), entry.clone());
        }
        let wanted = &wanted;
        if &settled.files != wanted {
            return Err(format!(
                "the files are not {} at {:?}, full={full}",
                if publishes {
                    "what a whole capture writes"
                } else {
                    "as the push found them"
                },
                settled
                    .files
                    .entries
                    .keys()
                    .chain(wanted.entries.keys())
                    .filter(|path| settled.files.entries.get(*path) != wanted.entries.get(*path))
                    .collect::<BTreeSet<_>>()
            ));
        }
        let studio_moved = elsewhere == Edited || (reads && elsewhere == EditedLate);
        let kept = match &settled.baseline {
            None => studio_moved || stopped,
            Some((_, Some(_))) => publishes && !stopped,
            Some((files, None)) => !publishes && !studio_moved && files == &settled.pushed,
        };
        if !kept {
            return Err(format!(
                "kept a baseline: {}, a pull's sourcemap: {}, full={full}",
                settled.baseline.is_some(),
                settled
                    .baseline
                    .as_ref()
                    .is_some_and(|(_, sourcemap)| sourcemap.is_some())
            ));
        }
        // Where nothing could narrow the settlement Studio is asked nothing,
        // and where only its answer rules that out, only where it differs.
        // A settlement that reads back asks of the commit's proof before
        // the push answers, then before and after its swaps, up to where it
        // stops.
        let unasked = synchronous
            || import == Import::Replaced
            || matches!(elsewhere, Remapped | Fresh | Referring);
        let unnarrowed = full || elsewhere == Edited;
        let asks = match (unasked, unnarrowed) {
            (true, _) => (0, 0),
            (false, true) => (0, 1),
            (false, false) => match elsewhere {
                RemappedLate => (1, 1),
                EditedLate | EditedFile | SavedDuringSwap => (2, 1),
                _ => (3, 1),
            },
        };
        if asks != settled.asks {
            return Err(format!(
                "asked Studio of the commit's proof and where it differs {:?} times, not {asks:?}, full={full}",
                settled.asks
            ));
        }
        if publishes && !stopped {
            kept_as_whole(settled)?;
        }
    }
    Ok(())
}

// The oracle is the settlement that reads every service back. Reading back
// only what the push wrote, what Studio says differs from the held proof and
// what refers into those must leave the same files and keep the same
// baseline, which must be what a whole capture of Studio writes.
#[test]
fn seeded_import_settlement_reads_back_only_what_can_differ() {
    use Elsewhere::*;
    let scenarios = Elsewhere::ALL
        .map(|elsewhere| (Import::Whole, elsewhere))
        .into_iter()
        .chain(
            [
                Untouched,
                Edited,
                EditedLate,
                EditedFile,
                Referring,
                Remapped,
                RemappedLate,
                Fresh,
                EditedAfterSwaps,
                ChangedAfterSwaps,
                SavedDuringSwap,
            ]
            .map(|elsewhere| (Import::AddedRoot, elsewhere)),
        )
        .chain([(Import::Replaced, Untouched), (Import::Mixed, Untouched)])
        .collect::<Vec<_>>();
    let count = scenarios.len() as u64;
    crate::tests::support::run_seeds(2 * count, |seed, log| {
        let (import, elsewhere) = scenarios[(seed % count) as usize];
        // A whole import that answers before its settlement fails on a
        // Studio that changes during it, and leaves it to the next command.
        let synchronous = (import == Import::Whole
            && matches!(elsewhere, EditedLate | ChangedAfterSwaps))
            || seed / count % 2 == 1;
        let documents = elsewhere_documents(seed, elsewhere);
        log.push(format!(
            "{import:?} with {elsewhere:?} and {} ServerStorage instances, synchronous={synchronous}",
            documents.before.instances.len()
        ));
        let run = |full| {
            settle_import((import, elsewhere), &documents, full, synchronous)
                .map_err(|error| format!("{error:#}"))
        };
        let (narrow, full) = (run(false)?, run(true)?);
        // The files are compared only where Studio says which services it
        // differs in and the push and those leave a service out.
        let compares = narrow.asks.1 == 1 && !matches!(elsewhere, Edited | EditedUnseen | Unscoped);
        if (narrow.compared, full.compared) != (usize::from(compares), 0) {
            return Err(format!(
                "compared the files {} times and, in full, {}",
                narrow.compared, full.compared
            ));
        }
        if import.beside_kept() {
            return added_roots_left((import, elsewhere, synchronous), &narrow, &full);
        }
        let studio =
            encode_settings_bytecode(&documents.after).map_err(|error| error.to_string())?;
        if matches!(elsewhere, EditedFile | EditedAfterSwaps)
            == (store_bytes(&full.files, "ServerStorage") == Some(studio.as_slice()))
        {
            return Err("the full settlement did not write Studio's copy".into());
        }
        let script: &[u8] = if elsewhere == PushedScript {
            b"return 2"
        } else {
            b"return 1"
        };
        if full.files.entries.get(Path::new(SETTLE_SCRIPT))
            != Some(&SnapshotEntry::File(script.into()))
        {
            return Err("the full settlement did not write Studio's script".into());
        }
        let newer = matches!(
            elsewhere,
            Edited
                | EditedLate
                | EditedFile
                | EditedAfterSwaps
                | ChangedAfterSwaps
                | SavedDuringSwap
        );
        if full.baseline.is_some() == newer {
            return Err(format!("kept a baseline: {}", full.baseline.is_some()));
        }
        narrowed_like_full(&narrow, &full)?;
        let narrows = matches!(
            elsewhere,
            Untouched | EditedFile | EditedAfterSwaps | SavedDuringSwap
        );
        let expected = if !synchronous && narrows {
            &SETTLE_SERVICES[..1]
        } else {
            &SETTLE_SERVICES[..]
        };
        if narrow.read_back != expected || full.read_back != SETTLE_SERVICES {
            return Err(format!(
                "read back {:?} and, in full, {:?}",
                narrow.read_back, full.read_back
            ));
        }
        Ok(())
    });
}
