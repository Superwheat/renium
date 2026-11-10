//! References between stores: how commands complete them, keep them on
//! their targets, lock the stores they read, and build them into a place.

use super::edits::run_batch;
use super::*;

const UTIL: [&str; 3] = ["ServerScriptService", "Tools", "Util"];

fn set_reference(fx: &Fixture, service: &str, holder: &str, path: &str) {
    fx.text(&[
        "bs",
        "-f",
        &fx.store_arg(service),
        holder,
        "-p",
        "Value",
        "--str",
        path,
    ])
    .ok();
}

fn stored_reference(fx: &Fixture, service: &str, holder: &str) -> Value {
    fx.both(&["bg", "-f", &fx.store_arg(service), holder, "Value"])
}

fn add_holder(fx: &Fixture, service: &str, name: &str, path: &str) -> Outcome {
    fx.text(&[
        "ba",
        "-f",
        &fx.store_arg(service),
        "-n",
        name,
        "-c",
        "ObjectValue",
        "-p",
        &format!("Value=\"{path}\""),
    ])
}

#[test]
fn a_partial_place_build_refuses_a_reference_into_a_service_it_leaves_out() {
    let fx = outside_reference_fixture("partial-build");
    set_reference(&fx, "Workspace", "Target", &UTIL.join("."));
    // Workspace.Clash has Util's id and is the only instance with it in a
    // build of Workspace alone.
    assert_eq!(
        fx.text(&[
            "bep",
            "-r",
            &fx.path(""),
            "-s",
            "Workspace",
            "-o",
            &fx.path("partial.rbxl"),
        ])
        .err(),
        "Workspace.Target (ObjectValue) property Value refers to ServerScriptService.Tools.Util, which is not among the exported instances"
    );
}

#[test]
fn renaming_a_holder_keeps_its_reference_into_a_service_without_a_store() {
    let fx = outside_reference_fixture("holder-rename");
    // StarterGui has no store. The reference's id is that of
    // ServerScriptService.Tools and of no other instance of the project.
    let unchecked = outside_reference(&["StarterGui", "Menu", "Frame"], &[1, 1, 1], "editor:1");
    let mut workspace = fx.read_store("Workspace");
    workspace.instances[6]
        .properties
        .insert("Value".to_string(), unchecked.clone());
    fx.write_store("Workspace", &workspace);
    fx.text(&[
        "bs",
        "-f",
        &fx.store_arg("Workspace"),
        "Target",
        "-p",
        "Name",
        "--str",
        "Renamed",
    ])
    .ok();
    assert_eq!(stored_reference(&fx, "Workspace", "Renamed"), unchecked);
}

#[test]
fn a_place_build_finds_a_reference_by_its_service_and_id_before_its_path() {
    let fx = outside_reference_fixture("stale-path-build");
    // As a pull of ServerScriptService alone leaves the project: Studio
    // renamed Util to Helper and a new Util took its place, and the
    // Workspace store still carries Util's old path.
    let mut server = fx.read_store("ServerScriptService");
    server.instances[2].name = "Helper".to_string();
    server
        .instances
        .push(settings_instance("editor:9", "Util", "Folder", Some(1)));
    fx.write_store("ServerScriptService", &server);
    let mut workspace = fx.read_store("Workspace");
    workspace.instances[6].properties.insert(
        "Value".to_string(),
        outside_reference(&UTIL, &[1, 1, 1], "editor:2"),
    );
    fx.write_store("Workspace", &workspace);
    assert_eq!(
        built_reference_target(&fx, "Workspace.Target").as_deref(),
        Some("ServerScriptService.Tools.Helper")
    );
}

#[test]
fn a_new_instance_may_refer_to_itself() {
    let fx = Fixture::new("self-reference");
    let added = add_holder(&fx, "Workspace", "Self", "Workspace.Self").ok();
    let id = added["settingsId"].as_str().unwrap();
    assert_eq!(
        stored_reference(&fx, "Workspace", "Self"),
        outside_reference(&["Workspace", "Self"], &[1, 1], id)
    );
    assert_eq!(
        built_reference_target(&fx, "Workspace.Self").as_deref(),
        Some("Workspace.Self")
    );
    // A second Door: the path names the Door that is there, not the one
    // being added.
    fx.text(&[
        "ba",
        "-f",
        &fx.store_arg("Workspace"),
        "-n",
        "Door",
        "-c",
        "ObjectValue",
        "-I",
        "lobby",
        "-p",
        "Value=\"Workspace.Lobby.Door\"",
    ])
    .ok();
    assert_eq!(
        stored_reference(&fx, "Workspace", "Lobby.Door[2]"),
        outside_reference(&["Workspace", "Lobby", "Door"], &[1, 1, 1], "door")
    );
}

#[test]
fn a_reference_to_an_instance_a_later_command_adds_is_refused() {
    let fx = Fixture::new("forward-command");
    let before = fs::read(fx.store("ReplicatedStorage")).unwrap();
    assert_eq!(
        add_holder(&fx, "ReplicatedStorage", "Forward", "Workspace.Future").err(),
        "Reference target was not found: Workspace.Future is not in the Workspace store; create the target first or add both in one `rbx ba --batch` (`rbx f Workspace NAME` finds a path, `rbx pl` refreshes the store from Studio)"
    );
    assert_eq!(fs::read(fx.store("ReplicatedStorage")).unwrap(), before);
    fx.text(&[
        "ba",
        "-f",
        &fx.store_arg("Workspace"),
        "-n",
        "Future",
        "-c",
        "Part",
    ])
    .ok();
    add_holder(&fx, "ReplicatedStorage", "Forward", "Workspace.Future").ok();
    assert_eq!(
        built_reference_target(&fx, "ReplicatedStorage.Forward").as_deref(),
        Some("Workspace.Future")
    );
}

#[test]
fn a_batch_resolves_references_against_what_it_leaves() {
    let fx = Fixture::new("batch-forward");
    let result = run_batch(
        &fx,
        r#"[
            {"parent":"Workspace","name":"Early","class":"ObjectValue","props":{"Value":"Workspace.Late"}},
            {"parent":"ReplicatedStorage","name":"Across","class":"ObjectValue","props":{"Value":"Workspace.Late"}},
            {"parent":"Workspace","name":"Self","class":"ObjectValue","props":{"Value":"Workspace.Self"}},
            {"op":"set","path":"ReplicatedStorage.Link","props":{"Value":{"_type":"Ref","pathSegments":["Workspace","Late"]}}},
            {"parent":"Workspace","name":"Late","class":"Folder"}
        ]"#,
    )
    .unwrap();
    let id = |op: usize| result["ops"][op]["id"].as_str().unwrap().to_string();
    let late = outside_reference(&["Workspace", "Late"], &[1, 1], &id(4));
    assert_eq!(stored_reference(&fx, "Workspace", "Early"), late);
    assert_eq!(stored_reference(&fx, "ReplicatedStorage", "Across"), late);
    assert_eq!(stored_reference(&fx, "ReplicatedStorage", "Link"), late);
    assert_eq!(
        stored_reference(&fx, "Workspace", "Self"),
        outside_reference(&["Workspace", "Self"], &[1, 1], &id(2))
    );
    assert_eq!(
        built_reference_target(&fx, "ReplicatedStorage.Across").as_deref(),
        Some("Workspace.Late")
    );
    let stores =
        || ["Workspace", "ReplicatedStorage"].map(|service| fs::read(fx.store(service)).unwrap());
    let before = stores();
    // The path names nothing when the batch ends: never added, or added and
    // renamed away.
    for (batch, op) in [
        (
            r#"[
                {"parent":"Workspace","name":"Other","class":"Folder"},
                {"op":"set","path":"ReplicatedStorage.Across","props":{"Value":"Workspace.Never"}}
            ]"#,
            "Batch op 1 (set)",
        ),
        (
            r#"[
                {"parent":"Workspace","name":"Lost","class":"ObjectValue","props":{"Value":"Workspace.Never"}},
                {"parent":"Workspace","name":"Never","class":"Folder"},
                {"op":"rename","path":"Workspace.Never","name":"Renamed"}
            ]"#,
            "Batch op 0 (add)",
        ),
    ] {
        assert_eq!(
            run_batch(&fx, batch).unwrap_err(),
            format!(
                "{op}: Reference target was not found: Workspace.Never is not in the Workspace store when the batch ends; add it in this batch or before it (`rbx f Workspace NAME` finds a path, `rbx pl` refreshes the store from Studio)"
            )
        );
        assert_eq!(stores(), before);
    }
}

#[test]
fn a_batch_clears_references_to_an_instance_it_added_and_removed() {
    let fx = Fixture::new("batch-ephemeral");
    run_batch(
        &fx,
        r#"[
            {"parent":"Workspace","name":"Ephemeral","class":"Folder"},
            {"op":"set","path":"ReplicatedStorage.Link","props":{"Value":"Workspace.Ephemeral"}},
            {"op":"set","path":"Workspace.Target","props":{"Value":"Workspace.Ephemeral"}},
            {"op":"remove","path":"Workspace.Ephemeral"}
        ]"#,
    )
    .unwrap();
    let empty = json!({"_type": "Ref"});
    assert_eq!(stored_reference(&fx, "ReplicatedStorage", "Link"), empty);
    assert_eq!(stored_reference(&fx, "Workspace", "Target"), empty);
    assert_eq!(built_reference_target(&fx, "ReplicatedStorage.Link"), None);
}

#[test]
fn a_batch_checks_the_unresolved_references_its_copies_carry() {
    let fx = Fixture::new("batch-copy-pending");
    let stores =
        || ["Workspace", "ReplicatedStorage"].map(|service| fs::read(fx.store(service)).unwrap());
    let before = stores();
    // The copy keeps the path its original wrote, and outlives the original.
    for (batch, op) in [
        (
            r#"[
                {"parent":"Workspace","name":"Deferred","class":"ObjectValue","props":{"Value":"Workspace.Never"}},
                {"op":"clone","path":"Workspace.Deferred","name":"DeferredCopy"},
                {"op":"remove","path":"Workspace.Deferred"}
            ]"#,
            "Batch op 1 (clone)",
        ),
        (
            r#"[
                {"parent":"Workspace","name":"Kit","class":"Folder"},
                {"parent":"Workspace.Kit","name":"Deferred","class":"ObjectValue","props":{"Value":"Workspace.Never"}},
                {"op":"clone","path":"Workspace.Kit","name":"KitCopy","ref":"copy"},
                {"op":"remove","path":"Workspace.Kit"}
            ]"#,
            "Batch op 2 (clone #copy)",
        ),
    ] {
        assert_eq!(
            run_batch(&fx, batch).unwrap_err(),
            format!(
                "{op}: Reference target was not found: Workspace.Never is not in the Workspace store when the batch ends; add it in this batch or before it (`rbx f Workspace NAME` finds a path, `rbx pl` refreshes the store from Studio)"
            )
        );
        assert_eq!(stores(), before);
    }
    let result = run_batch(
        &fx,
        r#"[
            {"parent":"Workspace","name":"Deferred","class":"ObjectValue","props":{"Value":"Workspace.Late"}},
            {"op":"clone","path":"Workspace.Deferred","name":"DeferredCopy"},
            {"op":"clone","path":"Workspace.Deferred","name":"Retargeted","props":{"Value":"Workspace.Lobby"}},
            {"op":"remove","path":"Workspace.Deferred"},
            {"parent":"Workspace","name":"Late","class":"Folder"}
        ]"#,
    )
    .unwrap();
    let late = result["ops"][4]["id"].as_str().unwrap();
    assert_eq!(
        stored_reference(&fx, "Workspace", "DeferredCopy"),
        outside_reference(&["Workspace", "Late"], &[1, 1], late)
    );
    assert_eq!(
        stored_reference(&fx, "Workspace", "Retargeted")["pathSegments"],
        json!(["Workspace", "Lobby"])
    );
    assert_eq!(
        built_reference_target(&fx, "Workspace.DeferredCopy").as_deref(),
        Some("Workspace.Late")
    );
}

/// `ba ReplicatedStorage -n RefTarget -c Folder` and an ObjectValue in
/// ServerScriptService that refers to it; returns the folder's store id.
fn add_target_and_probe(fx: &Fixture) -> String {
    let target = fx
        .text(&[
            "ba",
            "-f",
            &fx.store_arg("ReplicatedStorage"),
            "-n",
            "RefTarget",
            "-c",
            "Folder",
        ])
        .ok();
    let id = target["settingsId"].as_str().unwrap().to_string();
    add_holder(
        fx,
        "ServerScriptService",
        "RefProbe",
        "ReplicatedStorage.RefTarget",
    )
    .ok();
    assert_eq!(
        stored_reference(fx, "ServerScriptService", "RefProbe"),
        outside_reference(&["ReplicatedStorage", "RefTarget"], &[1, 1], &id)
    );
    id
}

/// A pull of ReplicatedStorage alone after Studio changed the folder a push
/// created there; `edit` receives the capture and the folder's index in it.
/// A capture names an instance by its Studio debug id. The pushed folder
/// arrives under its store id when `identified`: Studio's plugin reports that
/// id for an instance a push created or matched, for as long as its session
/// with the pushing daemon lasts.
fn pull_replicated_storage(
    fx: &Fixture,
    pushed_id: &str,
    identified: bool,
    edit: impl FnOnce(&mut SettingsBytecode, usize),
) {
    use crate::settings::bytecode::encode_settings_bytecode;
    use crate::settings::equivalence::{SettingsAlignment, align_settings_bytes_to_reference};

    let mut captured = fx.read_store("ReplicatedStorage");
    let pushed = captured
        .instances
        .iter()
        .position(|instance| instance.settings_id == pushed_id)
        .unwrap();
    for (index, instance) in captured.instances.iter_mut().enumerate() {
        if index == 0 {
            instance.settings_id = "1".to_string();
        } else if index != pushed || !identified {
            instance.settings_id = format!("debug:0_{}", index + 20);
        }
    }
    edit(&mut captured, pushed);
    let SettingsAlignment::Changed(aligned) = align_settings_bytes_to_reference(
        &fs::read(fx.store("ReplicatedStorage")).unwrap(),
        &encode_settings_bytecode(&captured).unwrap(),
    )
    .unwrap() else {
        panic!("Studio's edit must change the store");
    };
    fs::write(fx.store("ReplicatedStorage"), aligned).unwrap();
}

/// `Parent.Name=id` for the pushed folder and for whatever took its name, in
/// store order.
fn target_rows(fx: &Fixture) -> Vec<String> {
    let store = fx.read_store("ReplicatedStorage");
    store
        .instances
        .iter()
        .filter(|instance| instance.name.starts_with("RefTarget"))
        .map(|instance| {
            let parent = &store.instances[instance.parent_index.unwrap()].name;
            format!("{parent}.{}={}", instance.name, instance.settings_id)
        })
        .collect()
}

fn newcomer() -> SettingsBytecodeInstance {
    settings_instance("debug:0_99", "RefTarget", "Folder", Some(0))
}

#[test]
fn a_pull_keeps_a_reference_on_the_pushed_instance_studio_renamed() {
    let fx = Fixture::new("pull-renamed-target");
    let id = add_target_and_probe(&fx);
    // Studio renamed RefTarget and a new folder took its name.
    pull_replicated_storage(&fx, &id, true, |captured, pushed| {
        captured.instances[pushed].name = "RefTargetMoved".to_string();
        captured.instances.push(newcomer());
    });
    assert_eq!(
        target_rows(&fx),
        [
            format!("ReplicatedStorage.RefTargetMoved={id}"),
            "ReplicatedStorage.RefTarget=debug:0_99".to_string(),
        ]
    );
    // The holder's store was not pulled and still carries the old path.
    assert_eq!(
        stored_reference(&fx, "ServerScriptService", "RefProbe"),
        outside_reference(&["ReplicatedStorage", "RefTarget"], &[1, 1], &id)
    );
    assert_eq!(
        built_reference_target(&fx, "ServerScriptService.RefProbe").as_deref(),
        Some("ReplicatedStorage.RefTargetMoved")
    );
}

#[test]
fn a_pull_keeps_a_reference_on_the_pushed_instance_studio_moved() {
    let fx = Fixture::new("pull-moved-target");
    let id = add_target_and_probe(&fx);
    // Studio moved RefTarget into Shared and a new folder took its place.
    pull_replicated_storage(&fx, &id, true, |captured, pushed| {
        captured.instances[pushed].parent_index = Some(1);
        captured.instances.push(newcomer());
    });
    assert_eq!(
        target_rows(&fx),
        [
            format!("Shared.RefTarget={id}"),
            "ReplicatedStorage.RefTarget=debug:0_99".to_string(),
        ]
    );
    assert_eq!(
        built_reference_target(&fx, "ServerScriptService.RefProbe").as_deref(),
        Some("ReplicatedStorage.Shared.RefTarget")
    );
}

#[test]
fn a_pull_without_a_pushed_identity_pairs_the_store_by_path() {
    let fx = Fixture::new("pull-unidentified-target");
    let id = add_target_and_probe(&fx);
    // The same Studio edit, captured by a Studio that knows no pushed
    // identity (one restarted since the push): the capture carries debug
    // ids alone, so the store id stays on the path it was stored at.
    pull_replicated_storage(&fx, &id, false, |captured, pushed| {
        captured.instances[pushed].name = "RefTargetMoved".to_string();
        captured.instances.push(newcomer());
    });
    assert_eq!(
        target_rows(&fx),
        [
            "ReplicatedStorage.RefTargetMoved=debug:0_24".to_string(),
            format!("ReplicatedStorage.RefTarget={id}"),
        ]
    );
    assert_eq!(
        built_reference_target(&fx, "ServerScriptService.RefProbe").as_deref(),
        Some("ReplicatedStorage.RefTarget")
    );
}
