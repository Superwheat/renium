//! Drive the real push coordinator, replacing only the engine capture/mutation
//! boundary. Planning, history, proof eligibility, verification and journals run.
use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::thread;
use tungstenite::{Message, WebSocket, protocol::Role};

type Capture = Box<dyn FnOnce(&BoundContext) -> Result<ProjectSnapshot>>;
type Mutation = Box<dyn FnOnce(&BoundContext, &StagedPushRequest<'_>) -> Result<StagedPushResult>>;
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
    request: &StagedPushRequest<'_>,
) -> Option<Result<StagedPushResult>> {
    MUTATION
        .with(|slot| slot.borrow_mut().take())
        .map(|mutation| mutation(context, request))
}

pub(super) struct Fixture {
    pub(super) context: BoundContext,
    pub(super) bridge: Option<BridgeServer>,
    plugin: Option<thread::JoinHandle<()>>,
    pub(super) desired: ProjectSnapshot,
}

impl Fixture {
    pub(super) fn new() -> Result<Self> {
        let root = crate::tests::support::temp_dir("push-coordinator-settle");
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
        )?;
        let service = PathBuf::from("src/ReplicatedStorage");
        fs::create_dir_all(root.join(&service))?;
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "root".into(),
                    "ReplicatedStorage".into(),
                    "ReplicatedStorage".into(),
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
        let stream = crate::studio::bridge::routing_tests::connect(
            &bridge,
            0,
            crate::studio::bridge::routing_tests::edit_info("push-fixture", 20),
        );
        let plugin = thread::spawn(move || {
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
                    "verifiedPushProof":{"test":true}, "runtimeId":"push-fixture",
                    "seq":0, "tracking":true, "serviceGenerations":{"ReplicatedStorage":0}
                }});
                if socket
                    .send(Message::Text(reply.to_string().into()))
                    .is_err()
                {
                    break;
                }
            }
        });
        CAPTURES.with(|slot| {
            slot.borrow_mut()
                .push_back(Box::new(|_| Ok(ProjectSnapshot::default())))
        });
        Ok(Self {
            context,
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
            &["ReplicatedStorage".into()],
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
fn refused_staged_push_discards_only_its_undo_entry_and_opt_in_survives_staging() -> Result<()> {
    for allow in [false, true] {
        let fixture = Fixture::new()?;
        let directory = Path::new(&fixture.context.root).join(".renium/editor-history/sync");
        let previous = directory.join("previous");
        fs::create_dir_all(&previous)?;
        fs::write(previous.join("keep"), "previous undo")?;
        MUTATION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |_, request| {
                assert_eq!(request.args.slow_import, allow);
                if !request.args.slow_import {
                    return Err(crate::editor::sync::SlowImportRequired(
                        "simulated reader failure".into(),
                    )
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
