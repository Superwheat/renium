use super::*;

#[test]
#[ignore = "Builds only an owned local package fixture from the offline comparison copy"]
fn build_property_package_fixture() -> Result<()> {
    use rbx_dom_weak::InstanceBuilder;
    let mut source =
        rbx_binary::from_reader(fs::File::open("../../audit/place-comparison/current.rbxl")?)?;
    let mut pending = source.root().children().to_vec();
    let root = loop {
        let id = pending
            .pop()
            .context("No linked package in offline fixture")?;
        let node = source.get_by_ref(id).unwrap();
        if node.class.as_str() == "PackageLink" {
            break node.parent();
        }
        pending.extend_from_slice(node.children());
    };
    let child = source
        .get_by_ref(root)
        .unwrap()
        .children()
        .iter()
        .map(|id| source.get_by_ref(*id).unwrap())
        .find(|node| node.class.as_str() != "PackageLink")
        .context("Package has no editable child")?
        .name
        .clone();
    source.get_by_ref_mut(root).unwrap().name = "ReniumAccessPackage".into();
    let mut target = rbx_binary::from_reader(fs::File::open(
        "../../audit/performance/ReniumPropertyTest.rbxl",
    )?)?;
    let service = target.insert(target.root_ref(), InstanceBuilder::new("ReplicatedStorage"));
    source.transfer(root, &mut target, service);
    rbx_binary::to_writer(
        fs::File::create("../../audit/performance/ReniumPropertyPackageTest.rbxl")?,
        &target,
        target.root().children(),
    )?;
    println!(
        "{}",
        serde_json::json!({"target":["ReplicatedStorage","ReniumAccessPackage",child]})
    );
    Ok(())
}

#[test]
#[ignore = "Builds an owned local geometry fixture from the existing offline comparison copy"]
fn build_property_geometry_fixture() -> Result<()> {
    use rbx_dom_weak::{InstanceBuilder, types::Variant};
    let mut source =
        rbx_binary::from_reader(fs::File::open("../../audit/place-comparison/current.rbxl")?)?;
    let mut pending = source.root().children().to_vec();
    let mesh = loop {
        let id = pending.pop().context("No MeshPart in fixture")?;
        let node = source.get_by_ref(id).unwrap();
        if node.class.as_str() == "MeshPart" {
            break id;
        }
        pending.extend_from_slice(node.children());
    };
    for child in source.get_by_ref(mesh).unwrap().children().to_vec() {
        source.destroy(child);
    }
    let mut target = rbx_binary::from_reader(fs::File::open(
        "../../audit/network-simulation/ReniumNetworkTest.rbxl",
    )?)?;
    let workspace = target
        .root()
        .children()
        .iter()
        .find(|id| target.get_by_ref(**id).unwrap().class.as_str() == "Workspace")
        .copied()
        .unwrap_or_else(|| target.insert(target.root_ref(), InstanceBuilder::new("Workspace")));
    let node = source.get_by_ref_mut(mesh).unwrap();
    node.name = "ReniumAccessFixture".into();
    node.properties
        .insert("Anchored".into(), Variant::Bool(true));
    println!(
        "Geometry fixture: {:?}",
        node.properties.get(&"MeshId".into())
    );
    source.transfer(mesh, &mut target, workspace);
    target.insert(
        workspace,
        InstanceBuilder::new("StringValue")
            .with_name("ReniumAccessText")
            .with_property("Value", "fidelity-".repeat(1024)),
    );
    rbx_binary::to_writer(
        fs::File::create("../../audit/performance/ReniumPropertyTest.rbxl")?,
        &target,
        target.root().children(),
    )?;
    Ok(())
}

#[test]
#[ignore = "Requires RENIUM_INSPECT_FIXTURE_PID pointing at the explicitly prepared disposable place"]
fn protected_property_live_fixture() -> Result<()> {
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let title = "ReniumPropertyTest.rbxl";
    let mut samples = Vec::new();
    for index in 0..10 {
        let started = Instant::now();
        let mut text = prepare_property(
            pid,
            title,
            &["Workspace".into(), "ReniumAccessText".into()],
            &[],
            "Value",
            Duration::from_secs(3),
        )?;
        assert_eq!(text.class_name, "StringValue");
        assert_eq!(text.read()?, "fidelity-".repeat(1024));
        samples.push(started.elapsed().as_secs_f64() * 1000.);
        if index == 0 {
            let changed = text.write("native value 🧪");
            let restored = text.write(&"fidelity-".repeat(1024));
            assert_eq!(changed?, "native value 🧪");
            assert_eq!(restored?, "fidelity-".repeat(1024));
        }
    }
    println!("end-to-end native prepare/read ms: {samples:?}");
    let path = ["Workspace".into(), "ReniumAccessFixture".into()];
    let mut property = prepare_property(
        pid,
        title,
        &path,
        &[],
        "CollisionFidelity",
        Duration::from_secs(3),
    )?;
    assert_eq!(property.class_name, "MeshPart");
    let initial = property.read()?;
    println!("geometry initial CollisionFidelity={initial}");
    let started = Instant::now();
    assert_eq!(property.write("Hull")?, "Hull");
    println!(
        "verified CollisionFidelity write ms: {:.3}",
        started.elapsed().as_secs_f64() * 1000.
    );
    assert_eq!(property.write(&initial)?, initial);
    let mut streaming = prepare_property(
        pid,
        title,
        &["Workspace".into()],
        &[],
        "StreamingEnabled",
        Duration::from_secs(3),
    )?;
    assert_eq!(streaming.read()?, "false");
    let changed = streaming.write("true");
    let restored = streaming.write("false");
    assert_eq!(changed?, "true");
    assert_eq!(restored?, "false");
    Ok(())
}

#[test]
#[ignore = "Exercises deletion/replacement only inside the owned ReniumPropertyTest fixture"]
fn protected_property_rejects_replaced_live_targets() -> Result<()> {
    let pid = std::env::var("RENIUM_INSPECT_FIXTURE_PID")?.parse()?;
    let title = "ReniumPropertyTest.rbxl";
    let executable = std::env::var_os("RENIUM_LIVE_TEST_RBX").context(
        "Set RENIUM_LIVE_TEST_RBX to the installed CLI, avoiding old workspace binaries",
    )?;
    let run = |code: &str| -> Result<()> {
        let output = std::process::Command::new(&executable)
            .current_dir("../../audit/network-simulation")
            .args(["--place", title, "l", code, "--timeout", "3"])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        anyhow::ensure!(
            output.status.success() && result["ok"] == true,
            "Fixture edit failed: {result}; {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    let replace = "local old=workspace:FindFirstChild(\"ReniumAccessReplacement\"); if old then old:Destroy() end; local p=Instance.new(\"StringValue\"); p.Name=\"ReniumAccessReplacement\"; p.Value=\"unchanged\"; p.Parent=workspace";
    let path = ["Workspace".into(), "ReniumAccessReplacement".into()];
    for _ in 0..20 {
        run(replace)?;
        let mut before = prepare_property(pid, title, &path, &[], "Value", Duration::from_secs(3))?;
        run(replace)?;
        assert!(
            before.write("wrong-instance").is_err(),
            "A stale target must never be edited"
        );
        let mut after = prepare_property(pid, title, &path, &[], "Value", Duration::from_secs(3))?;
        assert_ne!(before.instance_id, after.instance_id);
        assert_eq!(after.read()?, "unchanged");
    }
    run("workspace.ReniumAccessReplacement:Destroy()")?;
    Ok(())
}
