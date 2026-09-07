use super::*;

#[test]
fn leased_commands_require_an_explicit_daemon_acknowledgement() {
    assert!(verify_lease_ack(false, &json!({})).is_ok());
    for response in [
        json!({}),
        json!({"resourceLeaseProtected":false}),
        json!({"resourceLeaseProtected":1}),
    ] {
        assert!(verify_lease_ack(true, &response).is_err());
    }
    assert!(verify_lease_ack(true, &json!({"resourceLeaseProtected":true})).is_ok());
}

fn example() -> Manifest {
    serde_json::from_value(json!({
        "schemaVersion":1,"name":"example-workflow","version":"0.1.0","description":"Example",
        "executable":{"default":["bin/example"]},"commands":{"inspect":{"description":"Inspect",
        "arguments":[{"name":"count","type":"integer","description":"Count","default":3},{"name":"enabled","type":"boolean","description":"Enable"},{"name":"label","type":"string","description":"Label","required":true}]}}
    })).unwrap()
}

#[test]
fn manifest_is_the_single_source_for_help_types_and_defaults() {
    let manifest = example();
    manifest::validate(&manifest).unwrap();
    let matches = manifest::command(&manifest)
        .try_get_matches_from([
            "example-workflow",
            "inspect",
            "--label",
            "hello",
            "--enabled",
            "--session",
            "thread-one",
        ])
        .unwrap();
    assert_eq!(matches.get_one::<String>("session").unwrap(), "thread-one");
    assert_eq!(
        manifest::arguments(
            &manifest.commands["inspect"],
            matches.subcommand().unwrap().1
        ),
        json!({"count":3,"label":"hello","enabled":true})
    );
    assert!(
        manifest::command(&manifest)
            .try_get_matches_from([
                "example-workflow",
                "inspect",
                "--label",
                "x",
                "--count",
                "oops"
            ])
            .is_err()
    );
    assert!(
        manifest::command(&manifest)
            .try_get_matches_from(["example-workflow", "inspect"])
            .is_err()
    );
}

#[test]
fn invalid_manifests_never_reach_execution() {
    for name in ["../escape", "Status", "status", "studio-status", "help"] {
        let mut manifest = example();
        manifest.name = name.into();
        assert!(manifest::validate(&manifest).is_err(), "{name}");
    }
    let mut manifest = example();
    manifest.commands.get_mut("inspect").unwrap().arguments[0].default = Some(json!("bad"));
    assert!(manifest::validate(&manifest).is_err());
    let mut manifest = example();
    manifest
        .executable
        .insert("default".into(), vec!["../escape".into()]);
    assert!(manifest::validate(&manifest).is_err());
}

#[test]
fn registration_and_resource_names_cannot_escape_the_registry() {
    for name in ["../one", "a/b", "C:\\one", "", "a.b"] {
        assert!(registration_path(Path::new("test"), name).is_err());
    }
}

#[test]
fn leases_are_exclusive_durable_and_reject_stale_claims() {
    let root = std::env::temp_dir().join(format!(
        "renium-plugin-lease-{}-{}",
        std::process::id(),
        crate::app::timing::current_millis()
    ));
    fs::create_dir(&root).unwrap();
    let registry = lease::Registry::new(root.join("leases"));
    let held = registry
        .acquire("studio-place-123", "test", "owner", &root)
        .unwrap();
    assert!(
        registry
            .acquire("studio-place-123", "test", "other", &root)
            .is_err()
    );
    assert!(registry.verify("studio-place-123", None).is_err());
    assert!(
        registry
            .verify("studio-place-456", Some(&held.claim))
            .is_err()
    );
    registry
        .update(&held.claim, json!({"phase":"cleanup"}))
        .unwrap();
    let registry = lease::Registry::new(root.join("leases"));
    assert_eq!(
        registry.read("studio-place-123").unwrap().unwrap().data["phase"],
        "cleanup"
    );
    registry.release(&held.claim).unwrap();
    assert!(
        registry
            .verify("studio-place-123", Some(&held.claim))
            .is_err()
    );
    assert!(
        registry
            .acquire("studio-place-123", "test", "next", &root)
            .is_ok()
    );
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn subprocess_reads_output_and_times_out_without_blocking_on_pipes() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--list"]);
    let output = renium_plugin_sdk::process::output(command, b"", Duration::from_secs(5)).unwrap();
    assert!(output.status.success());
    assert!(!output.stdout.is_empty());
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "plugins::tests::subprocess_sleep_fixture",
        "--ignored",
    ]);
    let start = std::time::Instant::now();
    let error =
        renium_plugin_sdk::process::output(command, b"", Duration::from_millis(100)).unwrap_err();
    assert!(error.to_string().contains("exceeded"));
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
#[ignore = "subprocess timeout fixture; invoked only by its parent test"]
fn subprocess_sleep_fixture() {
    std::thread::sleep(Duration::from_secs(5));
}

#[test]
fn concurrent_claims_have_one_winner_and_corrupt_records_stay_quarantined() {
    let root = crate::tests::support::temp_dir("plugin-claim-race");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let claims = std::thread::scope(|scope| {
        let workers = (0..16)
            .map(|index| {
                let barrier = barrier.clone();
                let root = &root;
                scope.spawn(move || {
                    barrier.wait();
                    lease::Registry::new(root.join("leases")).acquire(
                        "studio-place-1",
                        "example",
                        &format!("task-{index}"),
                        root,
                    )
                })
            })
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap().ok())
            .collect::<Vec<_>>()
    });
    assert_eq!(claims.len(), 1);
    fs::write(root.join("leases/studio-place-1.json"), b"{broken").unwrap();
    let registry = lease::Registry::new(root.join("leases"));
    assert!(
        registry
            .acquire("studio-place-1", "example", "other", &root)
            .is_err()
    );
    assert!(registry.verify("studio-place-1", None).is_err());
    fs::remove_dir_all(root).unwrap();
}
