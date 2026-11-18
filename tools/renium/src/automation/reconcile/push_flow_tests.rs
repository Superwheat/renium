//! Drive the real push coordinator, replacing only the engine capture/mutation
//! boundary. Planning, history, proof eligibility, verification and journals run.
use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::thread;
use tungstenite::{Message, WebSocket, protocol::Role};

type Capture = Box<dyn FnOnce(&BoundContext) -> Result<ProjectSnapshot>>;
struct MutationInput<'a> {
    plan: &'a ReconcilePushPlan,
    args: &'a PushEditorChangesArgs,
}
type Mutation = Box<dyn FnOnce(&BoundContext, MutationInput<'_>) -> Result<StagedPushResult>>;
thread_local! {
    static CAPTURES: RefCell<VecDeque<Capture>> = RefCell::new(VecDeque::new());
    static MUTATION: RefCell<Option<Mutation>> = RefCell::new(None);
}

pub(super) fn capture(context: &BoundContext) -> Option<Result<ProjectSnapshot>> {
    CAPTURES
        .with(|slot| slot.borrow_mut().pop_front())
        .map(|capture| capture(context))
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
    pub(super) desired: ProjectSnapshot,
}

fn fixture_runtime(bridge: &BridgeServer, runtime: &'static str) -> thread::JoinHandle<()> {
    let stream = crate::studio::bridge::routing_tests::connect(
        bridge,
        0,
        crate::studio::bridge::routing_tests::edit_info(runtime, 20),
    );
    thread::spawn(move || {
        let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
        while let Ok(Message::Text(text)) = socket.read() {
            let request: Value = serde_json::from_str(&text).unwrap();
            assert!(matches!(
                request["method"].as_str(),
                Some("getStudioChangeState" | "getStudioState")
            ));
            let reply = json!({"id":request["id"], "ok":true, "result":{
                "pushProofMatches":request["params"]["verifyPushProof"]["expired"] != true,
                "retainedPushProof":{"test":true}, "retainedPushProofLocal":true,
                "verifiedPushProof":{"test":true}, "runtimeId":runtime,
                "seq":0, "tracking":true, "serviceGenerations":{"ReplicatedStorage":0,"Workspace":0}
            }});
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
        let plugin = fixture_runtime(&bridge, "push-fixture");
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
            desired,
        })
    }

    fn run(&self) -> Result<Map<String, Value>> {
        self.push(false)
    }

    fn push(&self, slow_import: bool) -> Result<Map<String, Value>> {
        let _reply = crate::daemon::fixture_early_response();
        let bridge = self.bridge.as_ref().unwrap();
        let lease = Arc::new(crate::studio::bridge::BridgeRequestLease::new(
            "push-fixture".into(),
        ));
        let _lease = bridge.activate_request_lease(lease)?;
        let _cleanup = import_settle::RequestCleanup;
        let guard = StudioChangeGuard {
            runtime_id: "push-fixture".into(),
            change_seq: 0,
            tracking_guard_id: None,
            runtime_bootstrap_safe: true,
            service_generations: BTreeMap::new(),
        };
        let mut args = PushEditorChangesArgs::new(
            crate::cli::ProjectSourceArgs {
                project_root: self.context.root.clone().into(),
                src_root: "src".into(),
            },
            crate::cli::BridgeConnectionArgs::local(0.1),
        );
        args.slow_import = slow_import;
        let result = push_project(
            &self.context,
            bridge,
            std::slice::from_ref(&self.service),
            args,
            Some(&guard),
            true,
        );
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
            record.baseline = Some(StoredSnapshot::write(root, &setup.key, &baseline)?);
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
    let reopened = fixture_runtime(bridge, "saved-reopened");
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
