use super::*;
use crate::studio::bridge::{BridgeInfoPayload, routing_tests};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tungstenite::{Message, WebSocket, protocol::Role};

struct Fixture {
    bridge: Option<BridgeServer>,
    info: BridgeInfoPayload,
    requests: Arc<Mutex<Vec<Value>>>,
    plugin: Option<std::thread::JoinHandle<()>>,
    native: access::TestState,
}

impl Fixture {
    fn new(batch: bool, reply: Value) -> Self {
        let bridge = routing_tests::fixture();
        let mut info = routing_tests::edit_info("root-write-test", 20);
        info.native_root_target_batch = batch;
        info.native_root_target_ids = batch;
        let stream = routing_tests::connect(&bridge, 0, info.clone());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::clone(&requests);
        let plugin = std::thread::spawn(move || {
            let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
            while let Ok(Message::Text(text)) = socket.read() {
                let request: Value = serde_json::from_str(&text).unwrap();
                calls.lock().unwrap().push(request.clone());
                assert_eq!(request["method"], "getEditorTransactionState");
                let params = &request["params"];
                let result = if params.get("describeNativeRootWrites").is_some() {
                    reply.clone()
                } else if params["describeNativeRootWrite"] == true {
                    json!({"nativeRootWrite":params["nativeRootWrite"],"pathSegments":["Workspace","moved"],"pathOrdinals":[1,2]})
                } else {
                    json!({"nativeRootWrite":params["nativeRootWrite"],"historyRecording":"test-recording","terrainBaseline":"test-baseline"})
                };
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"ok":true,"result":result})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
            }
        });
        let native = std::rc::Rc::new(std::cell::RefCell::new(access::TestAccess {
            class: "MeshPart".into(),
            identity: "original-instance".into(),
            value: "old".into(),
            ..Default::default()
        }));
        access::TEST_ACCESS.with_borrow_mut(|slot| *slot = Some(native.clone()));
        Self {
            bridge: Some(bridge),
            info,
            requests,
            plugin: Some(plugin),
            native,
        }
    }
    fn bridge(&self) -> &BridgeServer {
        self.bridge.as_ref().unwrap()
    }
    fn prepared(
        &self,
        targets: Vec<PreparedRootTarget>,
        identities: Option<Vec<String>>,
    ) -> PreparedRootWrites {
        PreparedRootWrites {
            runtime_id: self.info.runtime_id.clone(),
            pid: 0,
            title: "test".into(),
            positions: targets
                .iter()
                .enumerate()
                .map(|(position, target)| (target.index, position))
                .collect(),
            targets,
            identities,
            #[cfg(windows)]
            properties: None,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        access::TEST_ACCESS.with_borrow_mut(|slot| *slot = None);
        drop(self.bridge.take());
        self.plugin.take().unwrap().join().unwrap();
    }
}

fn write() -> RootWrite {
    serde_json::from_value(json!({"index":7,"className":"MeshPart","pathSegments":["Workspace","same"],"pathOrdinals":[1,2],"name":"MeshId","value":"saved"})).unwrap()
}

fn target() -> Value {
    json!({"index":7,"className":"MeshPart","pathSegments":["Workspace","same"],"pathOrdinals":[1,2],"debugId":"queued"})
}

#[test]
fn apply_write_uses_released_plugin_lookup_and_native_preparation_without_batch_capability()
-> Result<()> {
    let fixture = Fixture::new(false, Value::Null);
    let write = write();
    let targets = describe_root_batch(
        fixture.bridge(),
        &fixture.info,
        "transaction",
        std::slice::from_ref(&write),
    )?;
    let mut prepared = fixture.prepared(targets, None);
    apply_write(
        fixture.bridge(),
        "transaction",
        &write,
        &write.text()?,
        &mut prepared,
    )?;
    let native = fixture.native.borrow();
    assert_eq!(native.writes, 1);
    assert_eq!(native.value, "saved");
    assert_eq!(
        native.paths,
        [(vec!["Workspace".into(), "moved".into()], vec![1, 2])]
    );
    let calls = fixture.requests.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["params"]["describeNativeRootWrite"], true);
    assert_eq!(calls[1]["params"]["finishNativeRootWrite"], false);
    assert_eq!(calls[2]["params"]["finishNativeRootWrite"], true);
    Ok(())
}

#[test]
fn malformed_batches_fall_back_before_apply_write() -> Result<()> {
    for rows in [
        Value::Null,
        json!([]),
        json!([target(), target()]),
        json!([{"index":8}]),
    ] {
        let fixture = Fixture::new(true, json!({"nativeRootTargets":rows}));
        let write = write();
        let targets = describe_root_batch(
            fixture.bridge(),
            &fixture.info,
            "transaction",
            std::slice::from_ref(&write),
        )?;
        assert!(targets.is_empty());
        let mut prepared = fixture.prepared(targets, None);
        apply_write(
            fixture.bridge(),
            "transaction",
            &write,
            &write.text()?,
            &mut prepared,
        )?;
        assert_eq!(fixture.native.borrow().writes, 1);
        let calls = fixture.requests.lock().unwrap();
        // On non-arm64 macOS the scope check already chooses the old route.
        assert_eq!(
            calls[calls.len() - 3]["params"]["describeNativeRootWrite"],
            true
        );
    }
    Ok(())
}

#[test]
fn identity_capture_failure_keeps_the_per_write_route_available() -> Result<()> {
    for failure in [
        "none",
        "duplicate identity",
        "nil identity",
        "deadline",
        "GetDebugId unavailable",
        "million-instance limit",
        "missing target",
    ] {
        let fixture = Fixture::new(true, Value::Null);
        let write = write();
        let mut targets = prepared_targets(
            json!({"nativeRootTargets":[target()]}),
            std::slice::from_ref(&write),
        )?;
        let identities = prepare_identity_batch(&mut targets, |services| {
            assert_eq!(services, ["Workspace"]);
            match failure {
                "none" => Ok(None),
                "missing target" => Ok(Some(HashMap::new())),
                _ => anyhow::bail!("{failure}"),
            }
        });
        assert!(identities.is_none() && targets.is_empty());
        let mut prepared = fixture.prepared(targets, identities);
        apply_write(
            fixture.bridge(),
            "transaction",
            &write,
            &write.text()?,
            &mut prepared,
        )?;
        assert_eq!(fixture.native.borrow().writes, 1, "{failure}");
    }
    Ok(())
}

#[test]
fn apply_write_checks_batch_identity_before_property_or_terrain_fences() -> Result<()> {
    for terrain in [false, true] {
        for identity in ["original-instance", "shifted-sibling"] {
            let fixture = Fixture::new(true, Value::Null);
            let mut write = write();
            fixture.native.borrow_mut().identity = identity.into();
            if terrain {
                write.class_name = "Terrain".into();
                write.name = "SmoothGrid".into();
                write.value = json!({"SmoothGrid":{"_type":"BinaryString","base64":"AA=="}});
            }
            let mut row = target();
            row["className"] = json!(write.class_name);
            let targets = prepared_targets(
                json!({"nativeRootTargets":[row]}),
                std::slice::from_ref(&write),
            )?;
            let mut prepared = fixture.prepared(targets, Some(vec!["original-instance".into()]));
            let result = apply_write(
                fixture.bridge(),
                "transaction",
                &write,
                &write.text()?,
                &mut prepared,
            );
            if identity == "original-instance" {
                result?;
                assert_eq!(fixture.native.borrow().writes, 1);
                assert_eq!(fixture.requests.lock().unwrap().len(), 2);
            } else {
                assert!(result.unwrap_err().to_string().contains("identity changed"));
                assert_eq!(fixture.native.borrow().writes, 0);
                assert!(fixture.requests.lock().unwrap().is_empty());
            }
        }
    }
    Ok(())
}

#[test]
fn service_scope_policy_compiles_and_runs_on_every_test_platform() -> Result<()> {
    let mut write = write();
    assert!(identity_batch_scope_supported(
        std::slice::from_ref(&write),
        true
    )?);
    assert!(!identity_batch_scope_supported(
        std::slice::from_ref(&write),
        false
    )?);
    write.path_segments[0] = "ordinary".into();
    assert!(!identity_batch_scope_supported(&[write], true)?);
    Ok(())
}
