use super::*;

const STORE: &str = "<root>/instances/Workspace.renium";

fn added(index: usize, id: &str, path: &[&str], ordinals: &[usize]) -> Value {
    json!({
        "adoptedSource": null,
        "changedPaths": [STORE],
        "index": index,
        "ok": true,
        "pathOrdinals": ordinals,
        "pathSegments": path,
        "settingsFile": STORE,
        "settingsId": id,
    })
}

#[test]
fn add_instance_prints_the_new_instance_in_text_and_json_mode() {
    let fx = Fixture::new("add-golden");
    let store = fx.store_arg("Workspace");
    assert_eq!(
        fx.text(&["ba", "-f", &store, "-n", "NewModel", "-c", "Model"])
            .ok(),
        added(9, "editor:9", &["Workspace", "NewModel"], &[1, 1])
    );
    assert_eq!(
        fx.json(&[
            "ba",
            "-f",
            &store,
            "-I",
            "Lobby",
            "-n",
            "NewPart",
            "-c",
            "Part",
            "-p",
            "Anchored=true",
            "-a",
            "Tag=\"x\""
        ])
        .ok(),
        added(
            10,
            "editor:a",
            &["Workspace", "Lobby", "NewPart"],
            &[1, 1, 1]
        )
    );
    let part = &fx.read_store("Workspace").instances[10];
    assert_eq!(part.properties["Anchored"], json!(true));
    assert_eq!(part.attributes["Tag"], json!("x"));
    assert_eq!(part.parent_index, Some(1));
    assert_eq!(fx.names("Workspace")[9..], ["NewModel", "NewPart"]);
}

#[test]
fn add_instance_takes_every_parent_selector() {
    let fx = Fixture::new("add-parents");
    let store = fx.store_arg("Workspace");
    let add =
        |args: &[&str]| fx.text(&[&["ba", "-f", &store, "-c", "Folder", "-n"], args].concat());
    assert_eq!(
        add(&["ById", "-I", "lobby"]).ok()["pathSegments"],
        json!(["Workspace", "Lobby", "ById"])
    );
    assert_eq!(
        add(&["ByPath", "-I", "Lobby"]).ok()["pathSegments"],
        json!(["Workspace", "Lobby", "ByPath"])
    );
    assert_eq!(
        add(&["ByIndex", "-x", "1"]).ok()["pathSegments"],
        json!(["Workspace", "Lobby", "ByIndex"])
    );
    assert_eq!(
        add(&["ByName", "-N", "Lobby"]).ok()["pathSegments"],
        json!(["Workspace", "Lobby", "ByName"])
    );
    assert_eq!(
        add(&["ByClass", "-C", "ObjectValue"]).ok()["pathSegments"],
        json!(["Workspace", "Target", "ByClass"])
    );
    assert_eq!(
        add(&["AtRoot", "--no-parent"]).ok()["pathSegments"],
        json!(["Workspace", "AtRoot"])
    );
    assert_eq!(
        add(&["ByOrdinal", "-I", "Lobby.Border[2]"]).ok()["pathSegments"],
        json!(["Workspace", "Lobby", "Border", "ByOrdinal"])
    );
    assert_eq!(
        add(&["Door", "-I", "Lobby"]).ok()["pathOrdinals"],
        json!([1, 1, 2])
    );
    assert!(
        add(&["X", "-C", "Folder"])
            .err()
            .contains("Ambiguous selector className \"Folder\"")
    );
    assert!(
        add(&["X", "-I", "nope"])
            .err()
            .contains("No matching instance path: Workspace.nope")
    );
    assert!(
        add(&["X", "-I", "Lobby.Border"])
            .err()
            .contains("Ambiguous instance path: Workspace.Lobby.Border")
    );
    assert_eq!(
        add(&["X", "--no-parent", "-I", "lobby"]).err(),
        "--no-parent cannot be combined with a parent selector"
    );
    assert_eq!(
        add(&["X", "-x", "1", "-I", "lobby"]).err(),
        "Provide only one parent selector"
    );
}

#[test]
fn add_instance_creates_scripts_adopts_files_and_refuses_conflicts() {
    let fx = Fixture::new("add-scripts");
    let store = fx.store_arg("Workspace");
    let fresh = fx
        .text(&[
            "ba",
            "-f",
            &store,
            "-n",
            "Fresh",
            "-c",
            "ModuleScript",
            "-p",
            "Source=\"return 7\"",
        ])
        .ok();
    assert_eq!(
        fresh["changedPaths"],
        json!([STORE, "<root>/src/Workspace/Fresh.luau"])
    );
    assert_eq!(
        fresh["sourceWrites"],
        json!(["<root>/src/Workspace/Fresh.luau"])
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Fresh.luau")).unwrap(),
        "return 7"
    );
    assert_eq!(
        fx.read_store("Workspace").instances[9].properties["Source"],
        json!(EXTERNAL_SOURCE_MARKER)
    );
    let empty = fx
        .text(&["ba", "-f", &store, "-n", "Empty", "-c", "Script"])
        .ok();
    assert_eq!(
        empty["sourceWrites"],
        json!(["<root>/src/Workspace/Empty.server.luau"])
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Empty.server.luau")).unwrap(),
        ""
    );
    let local = fx
        .text(&[
            "ba",
            "-f",
            &store,
            "-I",
            "Lobby",
            "-n",
            "Client",
            "-c",
            "LocalScript",
        ])
        .ok();
    assert_eq!(
        local["sourceWrites"],
        json!(["<root>/src/Workspace/Lobby/Client.client.luau"])
    );
    let adopted_file = fx.service_dir("Workspace").join("Adopted.luau");
    fs::write(&adopted_file, "return 'adopted'\n").unwrap();
    let adopted = fx
        .text(&["ba", "-f", &store, "-n", "Adopted", "-c", "ModuleScript"])
        .ok();
    assert_eq!(
        adopted["adoptedSource"],
        json!("<root>/src/Workspace/Adopted.luau")
    );
    assert_eq!(adopted["changedPaths"], json!([STORE]));
    assert_eq!(
        fs::read_to_string(&adopted_file).unwrap(),
        "return 'adopted'\n"
    );
    let conflict_file = fx.service_dir("Workspace").join("Conflict.luau");
    fs::write(&conflict_file, "return 'old'\n").unwrap();
    let before = fs::read(fx.store("Workspace")).unwrap();
    let refused = fx
        .text(&[
            "ba",
            "-f",
            &store,
            "-n",
            "Conflict",
            "-c",
            "ModuleScript",
            "-p",
            "Source=\"return 'new'\"",
        ])
        .err();
    assert!(
        refused.contains(
            "Conflict.luau already exists with different contents; omit the Source property"
        ),
        "{refused}"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    assert_eq!(
        fs::read_to_string(&conflict_file).unwrap(),
        "return 'old'\n"
    );
    let same = fx
        .text(&[
            "ba",
            "-f",
            &store,
            "-n",
            "Conflict",
            "-c",
            "ModuleScript",
            "-p",
            "Source=\"return 'old'\\n\"",
        ])
        .ok();
    assert_eq!(
        same["adoptedSource"],
        json!("<root>/src/Workspace/Conflict.luau")
    );
    let script_with_context = fx
        .text(&[
            "ba",
            "-f",
            &store,
            "-n",
            "Runner",
            "-c",
            "Script",
            "-p",
            "RunContext=\"Client\"",
        ])
        .ok();
    assert_eq!(
        script_with_context["sourceWrites"],
        json!(["<root>/src/Workspace/Runner.run-client.luau"])
    );
    assert_eq!(
        fx.read_store("Workspace").instances
            [script_with_context["index"].as_u64().unwrap() as usize]
            .properties["RunContext"],
        json!({"_type": "EnumItem", "name": "Client"})
    );
}

#[test]
fn add_instance_validates_names_classes_properties_ids_and_references() {
    let fx = Fixture::new("add-values");
    let store = fx.store_arg("Workspace");
    let add = |args: &[&str]| fx.text(&[&["ba", "-f", &store], args].concat());
    assert_eq!(
        add(&["-n", "Link", "-c", "PackageLink"]).err(),
        "PackageLink instances can only be created by Roblox package workflows"
    );
    assert_eq!(
        add(&["-n", "X", "-c", "Part", "-p", "Nope=1"]).err(),
        "Property Nope does not exist on Part; use -a for an attribute"
    );
    assert_eq!(
        add(&["-n", "X", "-c", "Part", "-p", "Bogus"]).err(),
        "Expected NAME=JSON assignment: Bogus"
    );
    assert!(
        add(&["-n", "X", "-c", "Part", "-p", "Size=notjson"])
            .err()
            .starts_with("Invalid assignment JSON value in Size=notjson")
    );
    assert_eq!(
        add(&["-n", "X", "-c", "Part", "-p", "=1"]).err(),
        "Property assignment name cannot be empty: =1"
    );
    assert!(
        add(&["-n", "X", "-c", "Part", "-p", "Size=\"big\""])
            .err()
            .contains("Size needs a Vector3 value")
    );
    assert_eq!(
        add(&["-n", "", "-c", "Folder"]).err(),
        "Instance name cannot be empty"
    );
    assert_eq!(
        add(&["-n", "X", "-c", ""]).err(),
        "ClassName cannot be empty"
    );
    let sized = add(&[
        "-n",
        "Sized",
        "-c",
        "Part",
        "-p",
        "Size=[1,2,3]",
        "-p",
        "transparency=0.5",
        "-a",
        "Note=\"n\"",
    ])
    .ok();
    let instance =
        fx.read_store("Workspace").instances[sized["index"].as_u64().unwrap() as usize].clone();
    assert_eq!(
        instance.properties["Size"],
        json!({"_type": "Vector3", "x": 1.0, "y": 2.0, "z": 3.0})
    );
    assert_eq!(instance.properties["Transparency"], json!(0.5));
    assert_eq!(instance.attributes["Note"], json!("n"));
    assert_eq!(
        add(&["-n", "Custom", "-c", "Folder", "-i", "custom:1"]).ok()["settingsId"],
        json!("custom:1")
    );
    assert_eq!(
        add(&["-n", "Custom2", "-c", "Folder", "-i", "custom:1"]).err(),
        "settingsId already exists: custom:1"
    );
    assert_eq!(
        add(&["-n", "Weird", "-c", "MadeUpClass", "-p", "Any=1"]).ok()["pathSegments"],
        json!(["Workspace", "Weird"])
    );
    let holder = add(&[
        "-n",
        "Holder",
        "-c",
        "ObjectValue",
        "-p",
        "Value={\"_type\":\"Ref\",\"settingsId\":\"door\"}",
    ])
    .ok();
    assert_eq!(
        fx.both(&[
            "bg",
            "-f",
            &store,
            "-i",
            holder["settingsId"].as_str().unwrap(),
            "Value"
        ]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"})
    );
    let remote = add(&[
        "-n",
        "Remote",
        "-c",
        "ObjectValue",
        "-p",
        "Value={\"_type\":\"Ref\",\"settingsId\":\"config\"}",
    ])
    .ok();
    assert_eq!(
        fx.both(&[
            "bg",
            "-f",
            &store,
            "-i",
            remote["settingsId"].as_str().unwrap(),
            "Value"
        ]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["ReplicatedStorage", "Shared", "Config"], "settingsId": "config"})
    );
    assert_eq!(
        add(&[
            "-n",
            "Broken",
            "-c",
            "ObjectValue",
            "-p",
            "Value={\"_type\":\"Ref\",\"settingsId\":\"nope\"}"
        ])
        .err(),
        "Reference target was not found: nope"
    );
    assert!(
        fx.text(&["ba", "-f", &store])
            .err()
            .contains("required arguments were not provided")
    );
    assert!(
        fx.text(&[
            "ba", "-f", &store, "-n", "X", "-c", "Folder", "--batch", "[]"
        ])
        .err()
        .contains("cannot be used with")
    );
}

#[test]
fn add_instance_seeds_an_unsynced_store_from_its_file_name() {
    let fx = Fixture::new("add-seed");
    let missing = fx.store_arg("StarterGui");
    assert_eq!(
        fx.text(&["ba", "-f", &missing, "-n", "Menu", "-c", "ScreenGui"])
            .ok(),
        json!({
            "adoptedSource": null,
            "changedPaths": ["<root>/instances/StarterGui.renium"],
            "index": 1,
            "ok": true,
            "pathOrdinals": [1, 1],
            "pathSegments": ["StarterGui", "Menu"],
            "settingsFile": "<root>/instances/StarterGui.renium",
            "settingsId": "editor:1",
        })
    );
    assert_eq!(fx.names("StarterGui"), ["StarterGui", "Menu"]);
    assert_eq!(
        fx.read_store("StarterGui").instances[0].class_name,
        "StarterGui"
    );
}

fn added_receipt(id: &str, path: &str) -> Value {
    json!({"id": id, "op": "add", "path": path})
}

/// `ba --batch` resolves services from the working directory, which a test
/// cannot change; the batch runs against the fixture's stores directly.
fn batch_outcome(
    fx: &Fixture,
    input: &str,
    push: bool,
) -> Result<crate::bytecode::add_batch::BatchOutcome, String> {
    use crate::bytecode::add_batch::{apply_store_batch, parse_batch_ops, read_batch_input};
    let run = || {
        let ops = parse_batch_ops(&read_batch_input(Path::new(input))?)?;
        apply_store_batch(
            ops,
            &|service| Ok((fx.store(service), service.to_string())),
            push,
            false,
        )
    };
    run().map_err(|error| normalize_text(&format!("{error:#}"), &fx.root))
}

fn run_batch(fx: &Fixture, input: &str) -> Result<Value, String> {
    batch_outcome(fx, input, false).map(|outcome| super::normalize(outcome.result, &fx.root))
}

#[test]
fn add_batch_creates_across_stores_with_references_and_sources() {
    let fx = Fixture::new("add-batch");
    let batch = fx.root.join("batch.json");
    fs::write(
        &batch,
        r##"{"ops":[
            {"parent":"ServerScriptService","name":"Battle","class":"Folder","ref":"battle"},
            {"parent":"#battle","name":"Abilities","class":"Folder","ref":"abilities"},
            {"parent":"#abilities","name":"CarMods","class":"ModuleScript","source":"return {}"},
            {"parent":"ReplicatedStorage.Shared","name":"Mode","class":"StringValue","props":{"Value":"ranked"},"attrs":{"Version":3}},
            {"parent":"#battle","name":"Main","class":"Script","source":"print('main')\n"},
            {"parent":"Workspace","name":"Store","class":"Folder","id":"custom:store"},
            {"parent":"lobby","service":"Workspace","name":"Lamp","class":"PointLight","props":{"Brightness":2}}
        ]}"##,
    )
    .unwrap();
    let result = run_batch(&fx, &batch.to_string_lossy()).unwrap();
    assert_eq!(
        result["ops"],
        json!([
            {"id": "editor:1", "op": "add", "path": "ServerScriptService.Battle", "ref": "battle"},
            {"id": "editor:2", "op": "add", "path": "ServerScriptService.Battle.Abilities", "ref": "abilities"},
            {"id": "editor:3", "op": "add", "path": "ServerScriptService.Battle.Abilities.CarMods", "sourcePath": "<root>/src/ServerScriptService/Battle/Abilities/CarMods.luau"},
            added_receipt("editor:4", "ReplicatedStorage.Shared.Mode"),
            {"id": "editor:4", "op": "add", "path": "ServerScriptService.Battle.Main", "sourcePath": "<root>/src/ServerScriptService/Battle/Main.server.luau"},
            added_receipt("custom:store", "Workspace.Store"),
            added_receipt("editor:a", "Workspace.Lobby.Lamp"),
        ])
    );
    assert_eq!(result["stores"], 3);
    assert_eq!(result["ok"], true);
    assert_eq!(
        result["changedPaths"],
        json!([
            "<root>/instances/ServerScriptService.renium",
            "<root>/src/ServerScriptService/Battle/Abilities/CarMods.luau",
            "<root>/src/ServerScriptService/Battle/Main.server.luau",
            "<root>/instances/ReplicatedStorage.renium",
            "<root>/instances/Workspace.renium",
        ])
    );
    assert_eq!(
        fs::read_to_string(
            fx.root
                .join("src/ServerScriptService/Battle/Abilities/CarMods.luau")
        )
        .unwrap(),
        "return {}"
    );
    assert_eq!(
        fs::read_to_string(
            fx.root
                .join("src/ServerScriptService/Battle/Main.server.luau")
        )
        .unwrap(),
        "print('main')\n"
    );
    let replicated = fx.read_store("ReplicatedStorage");
    assert_eq!(replicated.instances[4].properties["Value"], json!("ranked"));
    assert_eq!(replicated.instances[4].attributes["Version"], json!(3));
    assert_eq!(
        fx.read_store("Workspace").instances[10].properties["Brightness"],
        json!(2)
    );
    assert_eq!(
        fx.read_store("Workspace").instances[10].parent_index,
        Some(1)
    );
    let parsed = crate::cli::BytecodeAddInstanceArgs::try_parse_from(["ba", "--batch"]).unwrap();
    assert_eq!(parsed.batch.as_deref(), Some(Path::new("-")));
    assert!(
        crate::cli::BytecodeAddInstanceArgs::try_parse_from([
            "ba", "-n", "X", "-c", "Folder", "--batch", "[]"
        ])
        .is_err()
    );
    assert!(
        crate::cli::BytecodeAddInstanceArgs::try_parse_from([
            "ba", "-f", "x.renium", "--batch", "[]"
        ])
        .is_err()
    );
}

#[test]
fn add_batch_reads_inline_json_adopts_files_and_seeds_new_stores() {
    let fx = Fixture::new("add-batch-inline");
    let adopted = fx.service_dir("Workspace").join("Adopted.luau");
    fs::write(&adopted, "return 'adopted'\n").unwrap();
    let result = run_batch(
        &fx,
        r#"[{"parent":"Workspace","name":"Adopted","class":"ModuleScript"},{"parent":"StarterGui","name":"Menu","class":"ScreenGui"}]"#,
    )
    .unwrap();
    assert_eq!(
        result,
        json!({
            "changedPaths": ["<root>/instances/Workspace.renium", "<root>/instances/StarterGui.renium"],
            "ok": true,
            "ops": [
                {"adoptedSource": true, "id": "editor:9", "op": "add", "path": "Workspace.Adopted", "sourcePath": "<root>/src/Workspace/Adopted.luau"},
                added_receipt("editor:1", "StarterGui.Menu"),
            ],
            "stores": 2,
        })
    );
    assert_eq!(fs::read_to_string(&adopted).unwrap(), "return 'adopted'\n");
    assert_eq!(fx.names("StarterGui"), ["StarterGui", "Menu"]);
    let duplicate = run_batch(
        &fx,
        r#"[{"parent":"Workspace.Lobby","name":"Door","class":"Part"}]"#,
    )
    .unwrap();
    assert_eq!(
        duplicate["ops"][0]["path"],
        json!("Workspace.Lobby.Door[2]")
    );
    let bom = fx.root.join("bom.json");
    fs::write(
        &bom,
        "\u{feff}{\"ops\":[{\"parent\":\"Workspace\",\"name\":\"FromFile\",\"class\":\"Folder\"}]}",
    )
    .unwrap();
    assert_eq!(
        run_batch(&fx, &bom.to_string_lossy()).unwrap()["ops"][0]["path"],
        json!("Workspace.FromFile")
    );
}

#[test]
fn add_batch_rejects_bad_items_without_writing_any_store() {
    let fx = Fixture::new("add-batch-errors");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let err = |ops: &str| run_batch(&fx, ops).unwrap_err();
    assert!(err(r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"a"},{"parent":"#b","name":"B","class":"Folder"}]"##).ends_with("Batch op 1 (add): parent #b does not name an earlier op"));
    assert!(err(r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"a"},{"parent":"Workspace","name":"B","class":"Folder","ref":"a"}]"##).ends_with("Batch op 1 (add #a): ref #a is already used by an earlier op"));
    assert!(
        err(r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"#a"}]"##)
            .ends_with("ref must be a non-empty name without a leading #")
    );
    assert!(err(r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"a"},{"parent":"#a","service":"ReplicatedStorage","name":"B","class":"Folder"}]"##).ends_with("Batch op 1 (add): parent #a lives in Workspace, not ReplicatedStorage"));
    assert!(
        err(r#"[{"parent":"editor:1","name":"A","class":"Folder"}]"#).ends_with(
            "parent editor:1 is a settings id; add \"service\" to say which store it is in"
        )
    );
    assert!(
        err(r#"[{"parent":"Workspace.Nope","name":"A","class":"Folder"}]"#)
            .ends_with("Batch op 0 (add): No matching instance path: Workspace.Nope")
    );
    assert!(
        err(r#"[{"parent":"Workspace.Lobby.Border","name":"A","class":"Folder"}]"#)
            .contains("Ambiguous instance path: Workspace.Lobby.Border")
    );
    assert!(
        err(r#"[{"parent":"Workspace","name":"A","class":"Folder","source":"x"}]"#)
            .ends_with("source is only accepted for Script, LocalScript and ModuleScript")
    );
    assert!(err(r#"[{"parent":"Workspace","name":"A","class":"ModuleScript","source":"x","props":{"Source":"y"}}]"#).ends_with("give one of source, sourceFile or props.Source"));
    assert!(
        err(r#"[{"parent":"Workspace","name":"A","class":"Folder","bogus":1}]"#)
            .contains("unknown field `bogus`")
    );
    assert!(
        err(r#"[{"parent":"Workspace","name":"A","class":"Part","props":{"Nope":1}}]"#)
            .ends_with("Property Nope does not exist on Part; use -a for an attribute")
    );
    assert!(
        err(r#"[{"parent":"Workspace","name":"A","class":"PackageLink"}]"#)
            .ends_with("PackageLink instances can only be created by Roblox package workflows")
    );
    assert_eq!(err(r#"{"ops":[]}"#), "Batch input has no ops");
    assert_eq!(err("[]"), "Batch input has no ops");
    assert!(err("{bad").starts_with("Invalid batch input"));
    assert!(err("nope").starts_with("Failed to read nope"));
    assert!(err(r#"[{"parent":"","name":"A","class":"Folder"}]"#).contains("names no service"));
    let conflict = fx.service_dir("Workspace").join("Conflict.luau");
    fs::write(&conflict, "return 'old'\n").unwrap();
    let refused = run_batch(
        &fx,
        r#"[{"parent":"Workspace","name":"Fresh","class":"Folder"},{"parent":"Workspace","name":"Conflict","class":"ModuleScript","source":"zzz"}]"#,
    )
    .unwrap_err();
    assert!(
        refused.contains("Batch op 1 (add)")
            && refused.contains("already exists with different contents"),
        "{refused}"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    assert!(!fx.exists("src/Workspace/Orphan.luau"));
}

#[test]
fn batch_sets_clones_renames_moves_and_removes_in_order() {
    let fx = Fixture::new("batch-ops-edit");
    let result = run_batch(
        &fx,
        r##"{"ops":[
            {"parent":"Workspace","name":"Props","class":"Folder","ref":"props"},
            {"op":"set","path":"Workspace.Lobby.Door","props":{"Position":[0,10,0],"Transparency":0.25},"attrs":{"Owner":"Grace","Reviewed":null},"tags":["Door"]},
            {"op":"set","path":"Workspace","props":{"Gravity":50}},
            {"op":"clone","path":"Workspace.Lobby.Door","name":"Gate","parent":"#props","ref":"gate","props":{"Anchored":false}},
            {"op":"rename","path":"Workspace.Lobby.Door","name":"FrontDoor"},
            {"op":"move","path":"Workspace.Lobby.Border[2]","parent":"#props"},
            {"op":"remove","path":"Workspace.Lobby.Border"},
            {"op":"set","path":"#gate","attrs":{"Kind":"gate"}},
            {"op":"move","path":"Workspace.Lobby.Helper","parent":"#props"}
        ]}"##,
    )
    .unwrap();
    assert_eq!(
        result,
        json!({
            "changedPaths": [
                STORE,
                "<root>/src/Workspace/Lobby/Helper.client.luau",
                "<root>/src/Workspace/Props/Helper.client.luau",
                "<root>/instances/ReplicatedStorage.renium",
            ],
            "ok": true,
            "ops": [
                {"id": "editor:9", "op": "add", "path": "Workspace.Props", "ref": "props"},
                {"attrs": ["Owner", "Reviewed"], "op": "set", "path": "Workspace.Lobby.FrontDoor", "props": ["CFrame", "Tags", "Transparency"]},
                {"op": "set", "path": "Workspace", "props": ["Gravity"]},
                {"from": "Workspace.Lobby.Door", "id": "editor:a", "op": "clone", "path": "Workspace.Props.Gate", "props": ["Anchored"], "ref": "gate"},
                {"from": "Workspace.Lobby.Door", "op": "rename", "path": "Workspace.Lobby.FrontDoor"},
                {"from": "Workspace.Lobby.Border[2]", "op": "move", "path": "Workspace.Props.Border"},
                {"op": "remove", "path": "Workspace.Lobby.Border", "removed": 1},
                {"attrs": ["Kind"], "op": "set", "path": "Workspace.Props.Gate"},
                {"from": "Workspace.Lobby.Helper", "op": "move", "path": "Workspace.Props.Helper"},
            ],
            "stores": 2,
        })
    );
    assert_eq!(
        fx.names("Workspace"),
        [
            "Workspace",
            "Lobby",
            "FrontDoor",
            "Border",
            "Spawner",
            "Target",
            "Café ☕",
            "Helper",
            "Props",
            "Gate"
        ]
    );
    let workspace = fx.read_store("Workspace");
    let door = &workspace.instances[2];
    let position = door.properties["CFrame"]["components"].as_array().unwrap()[..3]
        .iter()
        .map(|component| component.as_f64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(position, [0.0, 10.0, 0.0]);
    assert_eq!(door.properties["Transparency"], json!(0.25));
    assert_eq!(door.properties["Tags"], json!(["Door"]));
    assert_eq!(door.attributes["Owner"], json!("Grace"));
    assert!(!door.attributes.contains_key("Reviewed"));
    assert_eq!(workspace.instances[0].properties["Gravity"], json!(50));
    let gate = &workspace.instances[9];
    assert_eq!(
        (gate.class_name.as_str(), gate.parent_index),
        ("Part", Some(8))
    );
    assert_eq!(gate.properties["Anchored"], json!(false));
    assert_eq!(gate.properties["Tags"], json!(["Door"]));
    assert_eq!(gate.attributes["Kind"], json!("gate"));
    assert_eq!(gate.attributes["Owner"], json!("Grace"));
    assert_eq!(workspace.instances[3].parent_index, Some(8));
    assert_eq!(
        workspace.instances[3].properties["Transparency"],
        json!(0.75)
    );
    assert_eq!(workspace.instances[7].parent_index, Some(8));
    let moved_door = json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "FrontDoor"], "settingsId": "door"});
    assert_eq!(
        fx.both(&["bg", "-f", &fx.store_arg("Workspace"), "Target", "Value"]),
        moved_door
    );
    assert_eq!(
        fx.both(&[
            "bg",
            "-f",
            &fx.store_arg("ReplicatedStorage"),
            "Link",
            "Value"
        ]),
        moved_door
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Props/Helper.client.luau")).unwrap(),
        HELPER_SOURCE
    );
    assert!(!fx.exists("src/Workspace/Lobby"));
}

#[test]
fn batch_ops_write_script_sources_from_text_files_and_copies() {
    let fx = Fixture::new("batch-ops-sources");
    let patch = fx.root.join("patch.luau");
    fs::write(&patch, "print('from file')\n").unwrap();
    let main = fx.root.join("main.luau");
    fs::write(&main, "return 'main'\n").unwrap();
    let batch = json!({"ops": [
        {"op": "set", "path": "Workspace.Spawner", "source": "print('patched')\n"},
        {"op": "clone", "path": "Workspace.Spawner", "name": "Spawner2", "ref": "copy"},
        {"op": "set", "path": "Workspace.Spawner", "sourceFile": patch},
        {"parent": "ServerScriptService", "name": "Main", "class": "Script", "sourceFile": main},
        {"op": "remove", "path": "ReplicatedStorage.Shared.Config"},
        {"op": "rename", "path": "Workspace.Lobby.Helper", "name": "Client"},
    ]});
    let spawner = "<root>/src/Workspace/Spawner.server.luau";
    assert_eq!(
        run_batch(&fx, &batch.to_string()).unwrap(),
        json!({
            "changedPaths": [
                STORE,
                "<root>/src/Workspace/Lobby/Client.client.luau",
                "<root>/src/Workspace/Lobby/Helper.client.luau",
                spawner,
                "<root>/src/Workspace/Spawner2.server.luau",
                "<root>/instances/ServerScriptService.renium",
                "<root>/src/ServerScriptService/Main.server.luau",
                "<root>/instances/ReplicatedStorage.renium",
                "<root>/src/ReplicatedStorage/Shared/Config.luau",
            ],
            "ok": true,
            "ops": [
                {"op": "set", "path": "Workspace.Spawner", "sourcePath": spawner},
                {"from": "Workspace.Spawner", "id": "editor:9", "op": "clone", "path": "Workspace.Spawner2", "ref": "copy", "sourcePath": "<root>/src/Workspace/Spawner2.server.luau"},
                {"op": "set", "path": "Workspace.Spawner", "sourcePath": spawner},
                {"id": "editor:1", "op": "add", "path": "ServerScriptService.Main", "sourcePath": "<root>/src/ServerScriptService/Main.server.luau"},
                {"op": "remove", "path": "ReplicatedStorage.Shared.Config", "removed": 1},
                {"from": "Workspace.Lobby.Helper", "op": "rename", "path": "Workspace.Lobby.Client"},
            ],
            "stores": 3,
        })
    );
    let read = |relative: &str| fs::read_to_string(fx.root.join(relative)).unwrap();
    assert_eq!(
        read("src/Workspace/Spawner.server.luau"),
        "print('from file')\n"
    );
    assert_eq!(
        read("src/Workspace/Spawner2.server.luau"),
        "print('patched')\n"
    );
    assert_eq!(
        read("src/ServerScriptService/Main.server.luau"),
        "return 'main'\n"
    );
    assert_eq!(
        read("src/Workspace/Lobby/Client.client.luau"),
        HELPER_SOURCE
    );
    assert!(!fx.exists("src/Workspace/Lobby/Helper.client.luau"));
    assert!(!fx.exists("src/ReplicatedStorage/Shared"));
    assert_eq!(
        fx.names("ReplicatedStorage"),
        ["ReplicatedStorage", "Shared", "Link"]
    );
    let broken = fx.root.join("broken.luau");
    fs::write(&broken, "local x =").unwrap();
    let before = fs::read(fx.store("Workspace")).unwrap();
    let refused = run_batch(
        &fx,
        &json!({"ops": [{"op": "set", "path": "Workspace.Spawner", "sourceFile": broken}]})
            .to_string(),
    )
    .unwrap_err();
    assert!(
        refused.starts_with("Batch op 0 (set): sourceFile <root>/broken.luau: Invalid Luau syntax"),
        "{refused}"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    assert_eq!(
        read("src/Workspace/Spawner.server.luau"),
        "print('from file')\n"
    );
}

#[test]
fn pushed_batch_finds_renamed_moved_and_removed_instances_by_their_old_paths() {
    let fx = Fixture::new("batch-ops-push");
    let outcome = batch_outcome(
        &fx,
        r##"{"ops":[
            {"op":"rename","path":"Workspace.Lobby","name":"Hall"},
            {"op":"move","path":"Workspace.Hall.Door","parent":"Workspace.Café ☕"},
            {"op":"remove","path":"Workspace.Target"},
            {"parent":"Workspace.Hall","name":"Fresh","class":"Folder","ref":"fresh"},
            {"op":"remove","path":"#fresh"},
            {"op":"set","path":"Workspace.Spawner","attrs":{"Mode":"fast"}}
        ]}"##,
        true,
    )
    .unwrap();
    let studio = &outcome.studio;
    assert_eq!(
        studio.paths,
        [
            vec!["Workspace", "Lobby"],
            vec!["Workspace", "Lobby", "Door"],
            vec!["Workspace", "Café ☕"],
            vec!["Workspace", "Target"],
        ]
    );
    assert_eq!(studio.renames, [(0, "Hall".to_string())]);
    assert_eq!(studio.moves, [(1, 2)]);
    assert_eq!(studio.destroys, [3]);
    let luau = studio.luau();
    assert!(
        luau.contains(r#"in {{"Workspace", "Lobby"}, {"Workspace", "Lobby", "Door"}, {"Workspace", "Café ☕"}, {"Workspace", "Target"}} do"#)
            && luau.contains(r#"{ 1, "Hall" }"#)
            && luau.contains("{ 2, 3 }")
            && luau.contains("in {4} do"),
        "{luau}"
    );
    crate::studio::automation::validate_luau_syntax(&luau).unwrap();
    let pushes = outcome
        .pushes
        .iter()
        .map(|push| (push.service.as_str(), push.settings_ids.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        pushes,
        [(
            "Workspace",
            vec![
                "lobby".to_string(),
                "door".to_string(),
                "spawner".to_string()
            ]
        )]
    );
    assert_eq!(
        fx.both(&[
            "bg",
            "-f",
            &fx.store_arg("ReplicatedStorage"),
            "Link",
            "Value"
        ]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Café ☕", "Door"], "settingsId": "door"})
    );
    let into_new = r##"[{"parent":"Workspace","name":"Props","class":"Folder","ref":"props"},{"op":"move","path":"Workspace.Spawner","parent":"#props"}]"##;
    let before = fs::read(fx.store("Workspace")).unwrap();
    let Err(refused) = batch_outcome(&fx, into_new, true) else {
        panic!("a pushed move into a new folder was accepted");
    };
    assert_eq!(
        refused,
        "Batch op 1 (move): with --push, Workspace.Spawner cannot move into Workspace.Props, which this batch creates (Studio would get a copy); create it in an earlier batch"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    assert!(
        batch_outcome(&fx, into_new, false)
            .unwrap()
            .studio
            .is_empty()
    );
}

/// Names with their parent's name, for the instances a merge test cares about.
fn merged_placements(path: &str, names: &[&str]) -> Vec<(String, String)> {
    let document = SettingsBytecode::read_file(Path::new(path)).unwrap();
    names
        .iter()
        .filter_map(|name| {
            let instance = document
                .instances
                .iter()
                .find(|instance| instance.name == *name)?;
            let parent = instance
                .parent_index
                .map(|index| document.instances[index].name.clone())
                .unwrap_or_default();
            Some((name.to_string(), parent))
        })
        .collect()
}

#[test]
fn a_preferred_merge_keeps_instances_whose_parent_the_other_side_removed() {
    let fx = Fixture::new("merge-orphans");
    run_batch(
        &fx,
        r##"[{"parent":"Workspace","name":"Props","class":"Folder","ref":"props"},
            {"parent":"#props","name":"A","class":"Folder","ref":"a"},
            {"parent":"#a","name":"Leaf","class":"Folder"},
            {"parent":"#props","name":"B","class":"Folder"},
            {"parent":"#props","name":"Sign","class":"StringValue","props":{"Value":"hello"}}]"##,
    )
    .unwrap();
    let store = fx.store("Workspace");
    let workspace = fx.store_arg("Workspace");
    let snapshot = |name: &str| {
        let path = fx.root.join(name);
        fs::copy(&store, &path).unwrap();
        path.to_string_lossy().into_owned()
    };
    let base = snapshot("base.renium");
    let merged = fx.path("merged.renium");
    let merge = |ours: &str, theirs: &str, prefer: &str, names: &[&str]| {
        fx.text(&[
            "vcm", &base, ours, theirs, "-o", &merged, "--prefer", prefer,
        ])
        .ok();
        merged_placements(&merged, names)
    };
    let placed = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(name, parent)| (name.to_string(), parent.to_string()))
            .collect::<Vec<_>>()
    };

    // One side removes B, the other moves A (with Leaf) into B.
    fx.text(&["br", "-f", &workspace, "Props.B"]).ok();
    let removed_b = snapshot("removed-b.renium");
    fs::copy(&base, &store).unwrap();
    run_batch(
        &fx,
        r##"[{"op":"move","path":"Workspace.Props.A","parent":"Workspace.Props.B"}]"##,
    )
    .unwrap();
    let moved_a = snapshot("moved-a.renium");
    assert!(
        fx.text(&["vcm", &base, &removed_b, &moved_a, "-o", &merged])
            .err()
            .contains("merge conflict"),
        "without a preference the move into a removed parent is a conflict"
    );
    // The side that removed B wins: A stays where that side has it.
    assert_eq!(
        merge(&removed_b, &moved_a, "ours", &["A", "Leaf", "B"]),
        placed(&[("A", "Props"), ("Leaf", "A")])
    );
    // The side that moved A wins: B comes back to hold it.
    assert_eq!(
        merge(&removed_b, &moved_a, "theirs", &["A", "Leaf", "B"]),
        placed(&[("A", "B"), ("Leaf", "A"), ("B", "Props")])
    );
    assert_eq!(
        merge(&moved_a, &removed_b, "ours", &["A", "Leaf", "B"]),
        placed(&[("A", "B"), ("Leaf", "A"), ("B", "Props")])
    );

    // One side removes Props, the other edits Sign inside it.
    fs::copy(&base, &store).unwrap();
    fx.text(&["br", "-f", &workspace, "Props"]).ok();
    let removed_props = snapshot("removed-props.renium");
    fs::copy(&base, &store).unwrap();
    fx.text(&[
        "bs",
        "-f",
        &workspace,
        "Props.Sign",
        "-p",
        "Value=\"edited\"",
    ])
    .ok();
    let edited_sign = snapshot("edited-sign.renium");
    // The edit wins: Props comes back for it; what nobody touched stays removed.
    assert_eq!(
        merge(
            &removed_props,
            &edited_sign,
            "theirs",
            &["Props", "Sign", "A", "B"]
        ),
        placed(&[("Props", "Workspace"), ("Sign", "Props")])
    );
    assert_eq!(
        SettingsBytecode::read_file(Path::new(&merged))
            .unwrap()
            .instances
            .iter()
            .find(|instance| instance.name == "Sign")
            .unwrap()
            .properties["Value"],
        json!("edited")
    );
    // The removal wins: nothing of Props is left.
    assert_eq!(
        merge(
            &removed_props,
            &edited_sign,
            "ours",
            &["Props", "Sign", "A", "B"]
        ),
        placed(&[])
    );
}

#[test]
fn a_scaled_label_stores_the_wrapping_studio_keeps() {
    let fx = Fixture::new("scaled-label");
    run_batch(
        &fx,
        r##"[{"parent":"Workspace","name":"Scaled","class":"TextLabel","props":{"TextScaled":true,"TextWrapped":false}},
            {"parent":"Workspace","name":"Bare","class":"TextLabel","props":{"TextScaled":true}},
            {"parent":"Workspace","name":"Plain","class":"TextLabel","props":{"TextScaled":false,"TextWrapped":false}}]"##,
    )
    .unwrap();
    let store = fx.read_store("Workspace");
    let label = |name: &str| {
        store
            .instances
            .iter()
            .find(|instance| instance.name == name)
            .unwrap()
    };
    assert_eq!(label("Scaled").properties["TextWrapped"], json!(true));
    assert_eq!(label("Bare").properties["TextWrapped"], json!(true));
    assert_eq!(label("Plain").properties["TextWrapped"], json!(false));
    let workspace = fx.store_arg("Workspace");
    fx.text(&["bs", "-f", &workspace, "Plain", "-p", "TextScaled=true"])
        .ok();
    assert_eq!(
        fx.read_store("Workspace")
            .instances
            .iter()
            .find(|instance| instance.name == "Plain")
            .unwrap()
            .properties["TextWrapped"],
        json!(true)
    );
    // Both values of one call reach the store together: settled between the
    // two assignments, the wrapping would be back on before scaling went off.
    fx.text(&[
        "bs",
        "-f",
        &workspace,
        "Scaled",
        "-p",
        "TextWrapped=false",
        "-p",
        "TextScaled=false",
    ])
    .ok();
    let store = fx.read_store("Workspace");
    let scaled = store
        .instances
        .iter()
        .find(|instance| instance.name == "Scaled")
        .unwrap();
    assert_eq!(scaled.properties["TextScaled"], json!(false));
    assert_eq!(scaled.properties["TextWrapped"], json!(false));
}

#[test]
fn batch_ops_share_the_value_handling_and_refusals_of_single_edits() {
    let fx = Fixture::new("batch-ops-values");
    let result = run_batch(
        &fx,
        r##"{"ops":[
            {"parent":"Workspace","name":"Team","class":"BrickColorValue","ref":"team"},
            {"op":"set","path":"#team","props":{"Value":"Bright red"}},
            {"op":"set","path":"Workspace.Target","props":{"Value":"Workspace.Café ☕"}},
            {"parent":"Lighting","name":"Haze","class":"Atmosphere"},
            {"op":"set","path":"Lighting","props":{"ClockTime":18.25}},
            {"parent":"Workspace","name":"Tmp","class":"Folder","ref":"tmp"},
            {"op":"remove","path":"#tmp"},
            {"parent":"Workspace","name":"Next","class":"Folder"}
        ]}"##,
    )
    .unwrap();
    let ops = result["ops"].as_array().unwrap();
    assert_eq!(ops[1]["props"], json!(["Value"]));
    assert_eq!(ops[4]["props"], json!(["TimeOfDay"]));
    assert_eq!(ops[5]["id"], json!("editor:a"));
    assert_eq!(ops[6]["path"], json!("Workspace.Tmp"));
    assert_eq!(ops[7]["id"], json!("editor:b"));
    let store = fx.store_arg("Workspace");
    let batch_color = fx.both(&["bg", "-f", &store, "Team", "Value"]);
    fx.text(&[
        "bs",
        "-f",
        &store,
        "Team",
        "-p",
        "Value",
        "-j",
        r#"{"_type":"BrickColor","number":21}"#,
    ])
    .ok();
    assert_eq!(fx.both(&["bg", "-f", &store, "Team", "Value"]), batch_color);
    let batch_reference = fx.both(&["bg", "-f", &store, "Target", "Value"]);
    assert_eq!(
        batch_reference["pathSegments"],
        json!(["Workspace", "Café ☕"])
    );
    fx.text(&[
        "bs",
        "-f",
        &store,
        "Target",
        "-p",
        "Value",
        "-j",
        "\"Workspace.Café ☕\"",
    ])
    .ok();
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Target", "Value"]),
        batch_reference
    );
    let lighting = &fx.read_store("Lighting").instances[0];
    assert_eq!(lighting.properties["TimeOfDay"], json!("18:15:00"));
    assert!(!lighting.properties.contains_key("ClockTime"));
    let before = fs::read(fx.store("Workspace")).unwrap();
    let err = |ops: &str| run_batch(&fx, ops).unwrap_err();
    assert!(err(r##"[{"parent":"Workspace","name":"Label","class":"TextLabel","ref":"label"},{"op":"set","path":"#label","props":{"Font":"GothamBold"}}]"##).starts_with("Batch op 1 (set): TextLabel.Font is hidden or engine-managed"));
    for (ops, refusal) in [
        (
            r#"[{"op":"set","path":"Workspace.Lobby.Door","props":{"Name":"Gate"}}]"#,
            "Batch op 0 (set): Name changes through a rename op",
        ),
        (
            r#"[{"op":"set","path":"Workspace.Lobby.Door","props":{"Parent":"Workspace"}}]"#,
            "Batch op 0 (set): Parent changes through a move op",
        ),
        (
            r#"[{"op":"set","path":"Workspace.Lobby.Door"}]"#,
            "Batch op 0 (set): set needs props, attrs, tags, source or sourceFile",
        ),
        (
            r#"[{"op":"set","path":"Workspace.Lobby.Door","source":"x"}]"#,
            "Batch op 0 (set): Part is not a Lua source container",
        ),
        (
            r#"[{"op":"set","path":"Workspace.Lobby.Door","tags":["A"],"props":{"Tags":["B"]}}]"#,
            "Batch op 0 (set): give tags or props.Tags, not both",
        ),
        (
            r#"[{"op":"move","path":"Workspace.Lobby.Door","parent":"ReplicatedStorage.Shared"}]"#,
            "Batch op 0 (move): parent ReplicatedStorage.Shared is in ReplicatedStorage, not Workspace; moves between services go through `rbx mv`",
        ),
        (
            r#"[{"op":"move","path":"Workspace.Lobby","parent":"Workspace.Lobby.Door"}]"#,
            "Batch op 0 (move): Cannot set Parent to the instance itself or one of its descendants",
        ),
        (
            r#"[{"op":"rename","path":"Workspace","name":"World"}]"#,
            "Batch op 0 (rename): The Workspace service root cannot be renamed",
        ),
        (
            r#"[{"op":"rename","path":"Workspace.Lobby","name":""}]"#,
            "Batch op 0 (rename): Instance name cannot be empty",
        ),
        (
            r#"[{"op":"remove","path":"Workspace"}]"#,
            "Batch op 0 (remove): Refusing to remove root instance",
        ),
        (
            r#"[{"op":"clone","path":"Workspace","name":"Copy"}]"#,
            "Batch op 0 (clone): Service roots cannot be copied",
        ),
        (
            r#"[{"op":"clone","path":"Workspace.Lobby","name":"Copy","parent":"ReplicatedStorage"}]"#,
            "Batch op 0 (clone): parent ReplicatedStorage is in ReplicatedStorage, not Workspace; a copy stays in its store",
        ),
        (
            r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"a"},{"op":"remove","path":"#a"},{"op":"rename","path":"#a","name":"B"}]"##,
            "Batch op 2 (rename): path #a was removed by an earlier op",
        ),
        (
            r#"[{"op":"remove","path":"Workspace.Target"},{"parent":"Workspace","name":"T2","class":"Folder","id":"target"}]"#,
            "Batch op 1 (add): settingsId target belonged to an instance this batch removed",
        ),
        (
            r#"[{"op":"delete","path":"Workspace.Lobby"}]"#,
            "Batch op 0: unknown op \"delete\"; use add, set, clone, rename, move or remove",
        ),
        (r#"[{"op":3}]"#, "Batch op 0: op must be a string, got 3"),
        ("[1]", "Batch op 0: an op must be a JSON object"),
    ] {
        assert_eq!(err(ops), refusal, "{ops}");
    }
    assert!(
        err(r#"[{"op":"set","path":"Workspace.Lobby.Door","props":{"Size":"big"}}]"#)
            .contains("Size needs a Vector3 value")
    );
    assert!(
        err(r#"[{"op":"set","path":"Players","props":{"MaxPlayers":4}}]"#)
            .contains("No synced Renium store for service 'Players'")
    );
    assert!(
        err(r#"[{"op":"rename","path":"Workspace.Lobby","name":"Hall","class":"Folder"}]"#)
            .starts_with("Batch op 0: unknown field `class`")
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
}

fn cloned(
    id: &str,
    name: &str,
    class: &str,
    path: &[&str],
    ordinals: &[usize],
    ids: &[&str],
) -> Value {
    json!({
        "changedPaths": [STORE],
        "className": class,
        "name": name,
        "ok": true,
        "pathOrdinals": ordinals,
        "pathSegments": path,
        "rootSettingsId": id,
        "service": "Workspace",
        "settingsFile": STORE,
        "settingsIds": ids,
        "sourceCopies": [],
    })
}

#[test]
fn clone_instance_copies_subtrees_with_unique_names_scripts_and_references() {
    let fx = Fixture::new("clone");
    let store = fx.store_arg("Workspace");
    assert_eq!(
        fx.text(&["bcl", "-f", &store, "Lobby.Door"]).ok(),
        cloned(
            "editor:9",
            "Door Copy",
            "Part",
            &["Workspace", "Lobby", "Door Copy"],
            &[1, 1, 1],
            &["editor:9"]
        )
    );
    assert_eq!(
        fx.json(&["bcl", "-f", &store, "-i", "door"]).ok(),
        cloned(
            "editor:a",
            "Door Copy 2",
            "Part",
            &["Workspace", "Lobby", "Door Copy 2"],
            &[1, 1, 1],
            &["editor:a"]
        )
    );
    assert_eq!(
        fx.text(&["bcl", "-f", &store, "-i", "door", "-I", "cafe"])
            .ok(),
        cloned(
            "editor:b",
            "Door",
            "Part",
            &["Workspace", "Café ☕", "Door"],
            &[1, 1, 1],
            &["editor:b"]
        )
    );
    assert_eq!(
        fx.text(&["bcl", "-f", &store, "-i", "door", "-N", "Café ☕"])
            .ok()["name"],
        json!("Door Copy")
    );
    assert_eq!(
        fx.text(&["bcl", "-f", &store, "-i", "door", "-X", "7"])
            .ok()["name"],
        json!("Door Copy 2")
    );
    assert_eq!(
        fx.text(&["bcl", "-f", &store, "-i", "door", "-C", "Workspace"])
            .ok()["pathSegments"],
        json!(["Workspace", "Door"])
    );
    assert_eq!(
        fx.text(&["bcl", "-f", &store, "-i", "door", "-I", "Lobby.Border[2]"])
            .ok()["pathOrdinals"],
        json!([1, 1, 2, 1])
    );
    assert_eq!(
        fx.read_store("Workspace").instances[9].properties,
        fx.read_store("Workspace").instances[2].properties
    );
    let script = fx.text(&["bcl", "-f", &store, "Spawner"]).ok();
    assert_eq!(
        script,
        json!({
            "changedPaths": [STORE, "<root>/src/Workspace/Spawner Copy.server.luau"],
            "className": "Script",
            "name": "Spawner Copy",
            "ok": true,
            "pathOrdinals": [1, 1],
            "pathSegments": ["Workspace", "Spawner Copy"],
            "rootSettingsId": "editor:10",
            "service": "Workspace",
            "settingsFile": STORE,
            "settingsIds": ["editor:10"],
            "sourceCopies": [{"from": "<root>/src/Workspace/Spawner.server.luau", "to": "<root>/src/Workspace/Spawner Copy.server.luau"}],
        })
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Spawner Copy.server.luau")).unwrap(),
        SPAWNER_SOURCE
    );
    let lobby = fx.text(&["bcl", "-f", &store, "Lobby"]).ok();
    assert_eq!(lobby["name"], json!("Lobby Copy"));
    assert_eq!(lobby["settingsIds"].as_array().unwrap().len(), 8);
    assert_eq!(
        lobby["sourceCopies"],
        json!([{"from": "<root>/src/Workspace/Lobby/Helper.client.luau", "to": "<root>/src/Workspace/Lobby Copy/Helper.client.luau"}])
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Lobby Copy/Helper.client.luau")).unwrap(),
        HELPER_SOURCE
    );
    let target = fx.text(&["bcl", "-f", &store, "Target"]).ok();
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Target Copy", "Value"]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"})
    );
    assert_eq!(target["settingsIds"].as_array().unwrap().len(), 1);
    let with_holder = fx
        .text(&[
            "ba",
            "-f",
            &store,
            "-I",
            "Café ☕",
            "-n",
            "Holder",
            "-c",
            "ObjectValue",
            "-p",
            "Value={\"_type\":\"Ref\",\"settingsId\":\"editor:b\"}",
        ])
        .ok();
    assert_eq!(with_holder["ok"], true);
    let cafe_copy = fx.text(&["bcl", "-f", &store, "Café ☕"]).ok();
    let copied_holder = fx.both(&["bg", "-f", &store, "Café ☕ Copy.Holder", "Value"]);
    assert_eq!(
        copied_holder["pathSegments"],
        json!(["Workspace", "Café ☕ Copy", "Door"])
    );
    assert_eq!(cafe_copy["name"], json!("Café ☕ Copy"));
}

#[test]
fn clone_instance_refuses_roots_cycles_duplicates_and_missing_targets() {
    let fx = Fixture::new("clone-errors");
    let store = fx.store_arg("Workspace");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let err = |args: &[&str]| fx.text(&[&["bcl", "-f", &store], args].concat()).err();
    assert_eq!(err(&["-i", "ws"]), "Service roots cannot be copied");
    assert_eq!(
        err(&["Lobby", "-I", "door"]),
        "Cannot copy an instance into itself or one of its descendants"
    );
    assert_eq!(
        err(&["Lobby", "-I", "lobby"]),
        "Cannot copy an instance into itself or one of its descendants"
    );
    assert!(err(&["Nope"]).contains("No matching instance path: Workspace.Nope"));
    assert!(err(&["Border"]).contains("2 instances are named \"Border\""));
    assert!(
        err(&["-i", "door", "-I", "nope"]).contains("No matching instance path: Workspace.nope")
    );
    assert_eq!(err(&["-i", "nope"]), "Source instance was not found");
    assert_eq!(
        err(&["Door", "-i", "door"]),
        "Give the target once: as the positional argument, --path or one selector option"
    );
    let collision = fx.service_dir("Workspace").join("Spawner Copy.server.luau");
    fs::write(&collision, "return 2").unwrap();
    assert!(err(&["Spawner"]).contains("already exists with different contents"));
    assert_eq!(fs::read_to_string(&collision).unwrap(), "return 2");
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    let missing = fx.store_arg("StarterGui");
    assert!(
        fx.text(&["bcl", "-f", &missing, "-n", "X"])
            .err()
            .contains("No synced Renium store for service 'StarterGui'")
    );
}

#[test]
fn remove_instance_deletes_subtrees_sources_and_keeps_descendants_on_request() {
    let fx = Fixture::new("remove");
    let store = fx.store_arg("Workspace");
    let replicated = fx.store_arg("ReplicatedStorage");
    assert_eq!(
        fx.text(&["br", "-f", &store, "Lobby.Door"]).ok(),
        json!({
            "changedPaths": [STORE],
            "ok": true,
            "removed": ["Workspace.Lobby.Door"],
            "removedSettingsIds": ["door"],
            "settingsFile": STORE,
            "storeRemoved": false,
        })
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Target", "Value"]),
        json!({"_type": "Ref"})
    );
    assert_eq!(
        fx.both(&["bg", "-f", &replicated, "Link", "Value"]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"})
    );
    assert_eq!(
        fx.json(&["br", "-f", &store, "Lobby", "-R"]).ok(),
        json!({
            "changedPaths": [STORE, "<root>/src/Workspace/Helper.client.luau", "<root>/src/Workspace/Lobby/Helper.client.luau"],
            "ok": true,
            "removed": ["Workspace.Lobby"],
            "removedSettingsIds": ["lobby"],
            "removedSourcePaths": ["<root>/src/Workspace/Lobby/Helper.client.luau"],
            "settingsFile": STORE,
            "storeRemoved": false,
        })
    );
    assert_eq!(
        fx.names("Workspace"),
        [
            "Workspace",
            "Border",
            "Border",
            "Spawner",
            "Target",
            "Café ☕",
            "Helper"
        ]
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Helper.client.luau")).unwrap(),
        HELPER_SOURCE
    );
    assert!(!fx.exists("src/Workspace/Lobby"));
    assert_eq!(
        fx.text(&["br", "-f", &replicated, "Shared"]).ok(),
        json!({
            "changedPaths": ["<root>/instances/ReplicatedStorage.renium", "<root>/src/ReplicatedStorage/Shared/Config.luau"],
            "ok": true,
            "removed": ["ReplicatedStorage.Shared"],
            "removedSettingsIds": ["shared", "config"],
            "removedSourcePaths": ["<root>/src/ReplicatedStorage/Shared/Config.luau"],
            "settingsFile": "<root>/instances/ReplicatedStorage.renium",
            "storeRemoved": false,
        })
    );
    assert!(!fx.exists("src/ReplicatedStorage/Shared"));
    assert_eq!(fx.names("ReplicatedStorage"), ["ReplicatedStorage", "Link"]);
    assert_eq!(
        fx.text(&["br", "-f", &store, "-i", "border2"]).ok()["removedSettingsIds"],
        json!(["border2"])
    );
    assert_eq!(
        fx.text(&["br", "-f", &store, "-x", "1"]).ok()["removedSettingsIds"],
        json!(["border1"])
    );
    assert_eq!(
        fx.text(&["br", "-f", &store, "-n", "Spawner"]).ok()["removedSourcePaths"],
        json!(["<root>/src/Workspace/Spawner.server.luau"])
    );
}

#[test]
fn remove_instance_refuses_roots_ambiguous_and_missing_targets() {
    let fx = Fixture::new("remove-errors");
    let store = fx.store_arg("Workspace");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let err = |args: &[&str]| fx.text(&[&["br", "-f", &store], args].concat()).err();
    assert_eq!(err(&["-i", "ws"]), "Refusing to remove root instance");
    assert!(err(&["Nope"]).contains("No matching instance path: Workspace.Nope"));
    assert!(err(&["Border"]).contains("2 instances are named \"Border\""));
    assert_eq!(err(&["-i", "nope"]), "No matching instance");
    assert!(err(&[]).contains("Select the instance: a name or dotted path"));
    assert_eq!(
        err(&["Door", "-i", "door"]),
        "Give the target once: as the positional argument, --path or one selector option"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    let missing = fx.store_arg("StarterGui");
    assert!(
        fx.text(&["br", "-f", &missing, "-n", "X"])
            .err()
            .contains("No synced Renium store for service 'StarterGui'")
    );
}

fn protected_fixture(tag: &str) -> Fixture {
    let fx = Fixture::new(tag);
    let mut document = workspace_document();
    document
        .instances
        .push(settings_instance("terrain", "Terrain", "Terrain", Some(0)));
    document
        .instances
        .push(settings_instance("pkg", "Pkg", "Model", Some(0)));
    document.instances.push(settings_instance(
        "pkglink",
        "PackageLink",
        "PackageLink",
        Some(10),
    ));
    fx.write_store("Workspace", &document);
    fx
}

#[test]
fn protected_containers_and_package_links_refuse_structural_edits() {
    let fx = protected_fixture("protected");
    let store = fx.store_arg("Workspace");
    let root = fx.path("");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let err = |args: &[&str]| fx.text(&[&["bs", "-f", &store], args].concat()).err();
    assert_eq!(
        fx.text(&["br", "-f", &store, "Terrain"]).err(),
        "Terrain cannot be removed"
    );
    assert_eq!(
        err(&["Terrain", "-p", "Parent", "--str", "lobby"]),
        "Terrain metadata is read-only"
    );
    assert_eq!(
        err(&["Terrain", "-p", "Name", "--str", "Land"]),
        "Terrain metadata is read-only"
    );
    assert_eq!(
        err(&["Terrain", "-p", "ClassName", "--str", "Folder"]),
        "ClassName is read-only"
    );
    assert_eq!(
        fx.text(&[
            "mv",
            "Workspace",
            "-i",
            "terrain",
            "--to-service",
            "ReplicatedStorage",
            "-r",
            &root
        ])
        .err(),
        "Terrain cannot be moved"
    );
    assert_eq!(
        fx.text(&["rm", "Workspace", "-i", "terrain", "-r", &root])
            .err(),
        "Terrain cannot be removed"
    );
    assert_eq!(
        fx.text(&["br", "-f", &store, "Pkg"]).err(),
        "Package-bearing instances cannot be removed; use upl on the package root first"
    );
    assert_eq!(
        fx.text(&["bcl", "-f", &store, "Pkg"]).err(),
        "Package-bearing instances cannot be copied; use upl on the package root first"
    );
    assert_eq!(
        fx.text(&["br", "-f", &store, "-i", "pkglink"]).err(),
        "Package-bearing instances cannot be removed; use upl on the package root first"
    );
    assert_eq!(
        err(&["-i", "pkglink", "-p", "Name", "--str", "X"]),
        "PackageLink instances cannot be edited; use upl on the package root to remove its link"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    assert_eq!(
        fx.both(&["in", "Workspace", "Pkg", "-F", "brief,hpl", "-r", &root]),
        json!({"c": "Model", "cc": 1, "hpl": true, "id": "pkg", "n": "Pkg", "path": "Workspace.Pkg"})
    );
    assert_eq!(
        fx.text(&["bs", "-f", &store, "Pkg", "-p", "Name", "--str", "Renamed"])
            .ok()["name"],
        json!("Renamed")
    );
    assert_eq!(
        fx.text(&["mv", "Workspace", "-i", "pkg", "-I", "lobby", "-r", &root])
            .ok()["pathSegments"],
        json!(["Workspace", "Lobby", "Renamed"])
    );
    assert_eq!(
        fx.text(&[
            "bs",
            "-f",
            &store,
            "Terrain",
            "-p",
            "Decoration",
            "--bool",
            "true"
        ])
        .ok()["property"],
        json!("Decoration")
    );
}

fn project_added(
    service: &str,
    index: usize,
    id: &str,
    path: &[&str],
    ordinals: &[usize],
) -> Value {
    let store = format!("<root>/instances/{service}.renium");
    json!({
        "adoptedSource": null,
        "changedPaths": [store],
        "index": index,
        "ok": true,
        "pathOrdinals": ordinals,
        "pathSegments": path,
        "settingsFile": store,
        "settingsId": id,
    })
}

#[test]
fn create_resolves_project_stores_parents_and_unsynced_services() {
    let fx = Fixture::new("create");
    let root = fx.path("");
    assert_eq!(
        fx.text(&[
            "cr",
            "Workspace",
            "-c",
            "Folder",
            "-n",
            "Fresh",
            "-r",
            &root
        ])
        .ok(),
        project_added("Workspace", 9, "editor:9", &["Workspace", "Fresh"], &[1, 1])
    );
    assert_eq!(
        fx.json(&[
            "cr",
            "Workspace",
            "-c",
            "Part",
            "-n",
            "P",
            "-I",
            "Lobby",
            "-r",
            &root
        ])
        .ok(),
        project_added(
            "Workspace",
            10,
            "editor:a",
            &["Workspace", "Lobby", "P"],
            &[1, 1, 1]
        )
    );
    assert_eq!(
        fx.text(&[
            "cr",
            "Workspace",
            "-c",
            "Part",
            "-n",
            "P2",
            "-I",
            "lobby",
            "-r",
            &root
        ])
        .ok()["pathSegments"],
        json!(["Workspace", "Lobby", "P2"])
    );
    assert_eq!(
        fx.text(&[
            "cr",
            "Workspace",
            "-c",
            "Part",
            "-n",
            "P3",
            "-I",
            "Lobby.Border[2]",
            "-r",
            &root
        ])
        .ok()["pathOrdinals"],
        json!([1, 1, 2, 1])
    );
    let script = fx
        .text(&[
            "cr",
            "Workspace",
            "-c",
            "ModuleScript",
            "-n",
            "Mod",
            "-p",
            "Source=\"return 1\"",
            "-a",
            "A=1",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        script["sourceWrites"],
        json!(["<root>/src/Workspace/Mod.luau"])
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Mod.luau")).unwrap(),
        "return 1"
    );
    assert_eq!(
        fx.text(&[
            "cr",
            "StarterGui",
            "-c",
            "ScreenGui",
            "-n",
            "Menu",
            "-r",
            &root
        ])
        .ok(),
        project_added(
            "StarterGui",
            1,
            "editor:1",
            &["StarterGui", "Menu"],
            &[1, 1]
        )
    );
    assert_eq!(
        fx.text(&[
            "cr",
            "StarterGui",
            "-c",
            "Frame",
            "-n",
            "F",
            "-I",
            "Menu",
            "-r",
            &root
        ])
        .ok()["pathSegments"],
        json!(["StarterGui", "Menu", "F"])
    );
    assert_eq!(
        fx.text(&[
            "cr",
            "Workspace",
            "-c",
            "Folder",
            "-n",
            "Legacy",
            "-r",
            &root,
            "-d",
            "src"
        ])
        .ok()["pathSegments"],
        json!(["Workspace", "Legacy"])
    );
    assert_eq!(
        fx.text(&[
            "cr",
            "Workspace",
            "-c",
            "Folder",
            "-n",
            "Nested",
            "-r",
            &fx.path("nowhere")
        ])
        .ok()["settingsFile"],
        json!(STORE)
    );
    let err = |args: &[&str]| {
        fx.text(&[&["cr", "Workspace", "-c", "Part"], args, &["-r", &root]].concat())
            .err()
    };
    assert!(err(&["-n", "X", "-I", "Lobby.Border"]).contains(
        "Projected service 'Workspace' has no instance 'Lobby.Border': Ambiguous instance path"
    ));
    assert!(
        err(&["-n", "X", "-I", "nope"])
            .contains("Projected service 'Workspace' has no instance 'nope'")
    );
    assert_eq!(
        err(&["-n", "X", "-p", "Nope=1"]),
        "Property Nope does not exist on Part; use -a for an attribute"
    );
    let outside = temp_dir("offline-create-no-project");
    let legacy = run_at(
        &outside,
        CaptureMode::Text,
        &[
            "cr",
            "Workspace",
            "-c",
            "Folder",
            "-n",
            "X",
            "-r",
            &outside.to_string_lossy(),
        ],
    )
    .ok();
    assert_eq!(
        legacy["settingsFile"],
        json!("<root>/src/Workspace/__roblox_sync_settings.renium")
    );
    assert_eq!(legacy["pathSegments"], json!(["Workspace", "X"]));
    assert!(
        outside
            .join("src/Workspace/__roblox_sync_settings.renium")
            .is_file()
    );
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn clone_command_copies_within_the_project_by_id_or_path() {
    let fx = Fixture::new("clone-command");
    let root = fx.path("");
    assert_eq!(
        fx.text(&["cp", "Workspace", "-i", "door", "-I", "cafe", "-r", &root])
            .ok(),
        cloned(
            "editor:9",
            "Door",
            "Part",
            &["Workspace", "Café ☕", "Door"],
            &[1, 1, 1],
            &["editor:9"]
        )
    );
    assert_eq!(
        fx.json(&[
            "cp",
            "Workspace",
            "-i",
            "Lobby.Door",
            "-I",
            "Café ☕",
            "-r",
            &root
        ])
        .ok()["name"],
        json!("Door Copy")
    );
    assert_eq!(
        fx.text(&[
            "cp",
            "Workspace",
            "-i",
            "door",
            "-I",
            "Lobby.Border[2]",
            "-r",
            &root
        ])
        .ok()["pathOrdinals"],
        json!([1, 1, 2, 1])
    );
    let err = |args: &[&str]| {
        fx.text(&[&["cp", "Workspace"], args, &["-r", &root]].concat())
            .err()
    };
    assert!(err(&["-i", "Border", "-I", "cafe"]).contains(
        "Projected service 'Workspace' has no instance 'Border': 2 instances are named \"Border\""
    ));
    assert!(
        err(&["-i", "nope", "-I", "cafe"])
            .contains("Projected service 'Workspace' has no instance 'nope'")
    );
    assert!(
        err(&["-i", "door", "-I", "nope"])
            .contains("Projected service 'Workspace' has no instance 'nope'")
    );
    assert_eq!(
        err(&["-i", "ws", "-I", "cafe"]),
        "Service roots cannot be copied"
    );
    assert_eq!(
        err(&["-i", "lobby", "-I", "door"]),
        "Cannot copy an instance into itself or one of its descendants"
    );
    assert!(
        fx.text(&["cp", "StarterGui", "-i", "x", "-I", "y", "-r", &root])
            .err()
            .contains("Projected service 'StarterGui' has no Renium store")
    );
}

#[test]
fn move_command_reparents_within_and_across_services() {
    let fx = Fixture::new("move-command");
    let root = fx.path("");
    let replicated = fx.store_arg("ReplicatedStorage");
    let store = fx.store_arg("Workspace");
    assert_eq!(
        fx.text(&["mv", "Workspace", "-i", "door", "-I", "cafe", "-r", &root])
            .ok(),
        json!({
            "changedPaths": ["<root>/instances/ReplicatedStorage.renium", STORE],
            "className": "Part",
            "name": "Door",
            "ok": true,
            "pathOrdinals": [1, 1, 1],
            "pathSegments": ["Workspace", "Café ☕", "Door"],
            "property": "Parent",
            "settingsFile": STORE,
            "settingsId": "door",
        })
    );
    assert_eq!(
        fx.text(&["mv", "Workspace", "-i", "Café ☕.Door", "-r", &root])
            .ok()["pathSegments"],
        json!(["Workspace", "Door"])
    );
    assert_eq!(
        fx.json(&["mv", "Workspace", "-i", "door", "-I", "Lobby", "-r", &root])
            .ok()["pathSegments"],
        json!(["Workspace", "Lobby", "Door"])
    );
    let crossed = fx
        .text(&[
            "mv",
            "Workspace",
            "-i",
            "lobby",
            "--to-service",
            "ReplicatedStorage",
            "-I",
            "shared",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        crossed,
        json!({
            "changedPaths": [
                "<root>/instances/ReplicatedStorage.renium",
                STORE,
                "<root>/src/ReplicatedStorage/Shared/Lobby/Helper.client.luau",
                "<root>/src/Workspace/Lobby/Helper.client.luau",
            ],
            "className": "Folder",
            "movedInstances": 5,
            "name": "Lobby",
            "ok": true,
            "pathOrdinals": [1, 1, 1],
            "pathSegments": ["ReplicatedStorage", "Shared", "Lobby"],
            "settingsId": "lobby",
            "sourceService": "Workspace",
            "sourceSettingsFile": STORE,
            "sourceStoreRemoved": false,
            "targetService": "ReplicatedStorage",
            "targetSettingsFile": "<root>/instances/ReplicatedStorage.renium",
        })
    );
    assert_eq!(
        fs::read_to_string(
            fx.root
                .join("src/ReplicatedStorage/Shared/Lobby/Helper.client.luau")
        )
        .unwrap(),
        HELPER_SOURCE
    );
    assert!(!fx.exists("src/Workspace/Lobby"));
    let moved_ref = json!({"_type": "Ref", "pathOrdinals": [1, 1, 1, 1], "pathSegments": ["ReplicatedStorage", "Shared", "Lobby", "Door"], "settingsId": "door"});
    assert_eq!(fx.both(&["bg", "-f", &store, "Target", "Value"]), moved_ref);
    assert_eq!(
        fx.both(&["bg", "-f", &replicated, "Link", "Value"]),
        moved_ref
    );
    assert_eq!(
        fx.names("Workspace"),
        ["Workspace", "Spawner", "Target", "Café ☕"]
    );
    let back = fx
        .text(&[
            "mv",
            "ReplicatedStorage",
            "-i",
            "lobby",
            "--to-service",
            "Workspace",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(back["pathSegments"], json!(["Workspace", "Lobby"]));
    assert_eq!(
        fx.both(&["bg", "-f", &replicated, "Link", "Value"]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"})
    );
    assert_eq!(
        fx.text(&[
            "mv",
            "Workspace",
            "-i",
            "spawner",
            "--to-service",
            "StarterGui",
            "-r",
            &root
        ])
        .err(),
        "Target service 'StarterGui' has no Renium store"
    );
    assert!(fx.exists("src/Workspace/Spawner.server.luau"));
    fx.text(&[
        "cr",
        "StarterGui",
        "-c",
        "ScreenGui",
        "-n",
        "Menu",
        "-r",
        &root,
    ])
    .ok();
    let into_new = fx
        .text(&[
            "mv",
            "Workspace",
            "-i",
            "spawner",
            "--to-service",
            "StarterGui",
            "-I",
            "Menu",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        into_new["targetSettingsFile"],
        json!("<root>/instances/StarterGui.renium")
    );
    assert_eq!(
        into_new["pathSegments"],
        json!(["StarterGui", "Menu", "Spawner"])
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/StarterGui/Menu/Spawner.server.luau")).unwrap(),
        SPAWNER_SOURCE
    );
    assert!(!fx.exists("src/Workspace/Spawner.server.luau"));
    assert_eq!(fx.names("StarterGui"), ["StarterGui", "Menu", "Spawner"]);
    let err = |args: &[&str]| {
        fx.text(&[&["mv", "Workspace"], args, &["-r", &root]].concat())
            .err()
    };
    assert_eq!(
        err(&["-i", "door", "-I", "door"]),
        "Cannot set Parent to the instance itself or one of its descendants"
    );
    assert_eq!(
        err(&["-i", "ws", "-I", "cafe"]),
        "The service root cannot be moved under another instance"
    );
    assert_eq!(
        err(&["-i", "ws", "--to-service", "ReplicatedStorage"]),
        "Service roots cannot be moved"
    );
    assert!(
        err(&["-i", "nope", "-I", "cafe"])
            .contains("Projected service 'Workspace' has no instance 'nope'")
    );
    assert!(
        err(&[
            "-i",
            "lobby",
            "--to-service",
            "ReplicatedStorage",
            "-I",
            "nope"
        ])
        .contains("Projected service 'ReplicatedStorage' has no instance 'nope'")
    );
    assert!(
        err(&["-i", "lobby", "--to-service", "Workspace", "-I", "lobby"])
            .contains("itself or one of its descendants")
    );
}

#[test]
fn rename_and_remove_commands_edit_the_project_store() {
    let fx = Fixture::new("rename-remove");
    let root = fx.path("");
    assert_eq!(
        fx.text(&["rn", "Workspace", "-i", "door", "Gate", "-r", &root])
            .ok(),
        json!({
            "changedPaths": ["<root>/instances/ReplicatedStorage.renium", STORE],
            "className": "Part",
            "name": "Gate",
            "ok": true,
            "pathOrdinals": [1, 1, 1],
            "pathSegments": ["Workspace", "Lobby", "Gate"],
            "property": "Name",
            "settingsFile": STORE,
            "settingsId": "door",
        })
    );
    let script = fx
        .json(&["rn", "Workspace", "-i", "Spawner", "Boot", "-r", &root])
        .ok();
    assert_eq!(
        script["changedPaths"],
        json!([
            STORE,
            "<root>/src/Workspace/Boot.server.luau",
            "<root>/src/Workspace/Spawner.server.luau"
        ])
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Boot.server.luau")).unwrap(),
        SPAWNER_SOURCE
    );
    let slashed = fx
        .text(&["rn", "Workspace", "-i", "cafe", "a/b", "-r", &root])
        .ok();
    assert_eq!(slashed["pathSegments"], json!(["Workspace", "a/b"]));
    assert_eq!(
        fx.text(&[
            "cr",
            "Workspace",
            "-c",
            "ModuleScript",
            "-n",
            "Inside",
            "-I",
            "cafe",
            "-r",
            &root
        ])
        .ok()["sourceWrites"],
        json!(["<root>/src/Workspace/a_b/Inside.luau"])
    );
    let err = |args: &[&str]| {
        fx.text(&[&["rn", "Workspace"], args, &["-r", &root]].concat())
            .err()
    };
    assert_eq!(
        err(&["-i", "ws", "World"]),
        "The Workspace service root cannot be renamed"
    );
    assert!(
        err(&["-i", "nope", "X"]).contains("Projected service 'Workspace' has no instance 'nope'")
    );
    assert!(
        err(&["-i", "Lobby.Border", "Edge"])
            .contains("Ambiguous instance path: Workspace.Lobby.Border")
    );
    assert_eq!(
        fx.text(&[
            "rn",
            "Workspace",
            "-i",
            "Lobby.Border[2]",
            "Edge",
            "-r",
            &root
        ])
        .ok()["settingsId"],
        json!("border2")
    );
    assert_eq!(
        fx.text(&["rm", "Workspace", "-i", "Gate", "-r", &root])
            .ok(),
        json!({"changedPaths": [STORE], "ok": true, "removed": ["Workspace.Lobby.Gate"], "removedSettingsIds": ["door"], "settingsFile": STORE, "storeRemoved": false})
    );
    let kept = fx
        .json(&["rm", "Workspace", "-i", "lobby", "-R", "-r", &root])
        .ok();
    assert_eq!(kept["removedSettingsIds"], json!(["lobby"]));
    assert_eq!(
        fx.names("Workspace"),
        [
            "Workspace",
            "Border",
            "Edge",
            "Boot",
            "Target",
            "a/b",
            "Helper",
            "Inside"
        ]
    );
    assert!(
        fx.text(&["rm", "Workspace", "-i", "nope", "-r", &root])
            .err()
            .contains("Projected service 'Workspace' has no instance 'nope'")
    );
    assert_eq!(
        fx.text(&["rm", "Workspace", "-i", "ws", "-r", &root]).err(),
        "Refusing to remove root instance"
    );
    assert!(
        fx.text(&["rm", "StarterGui", "-i", "x", "-r", &root])
            .err()
            .contains("Projected service 'StarterGui' has no Renium store")
    );
}

#[test]
fn set_property_refuses_an_edit_inside_a_read_only_link() {
    let fx = Fixture::new("bs-read-only-link");
    crate::tests::support::write_packed_link_project(&fx.root);
    let store = fx.store_arg("ReplicatedStorage");
    let root = fx.path("");
    let before = fs::read(fx.store("ReplicatedStorage")).unwrap();
    let mark = |target: &str, extra: &[&str]| {
        let edit = [
            "bs",
            store.as_str(),
            target,
            "-p",
            "Mark",
            "--scope",
            "attribute",
            "--str",
            "x",
        ];
        fx.text(&[&edit[..], extra].concat())
    };
    let refusal = "Cannot edit read-only link \"shared\" at ReplicatedStorage.Shared. Use --override-packages to replace it intentionally.";
    assert_eq!(mark("Shared", &[]).err(), refusal);
    assert_eq!(mark("Shared.Util", &[]).err(), refusal);
    assert_eq!(fs::read(fx.store("ReplicatedStorage")).unwrap(), before);
    mark("ReplicatedStorage", &[]).ok();
    mark("Shared", &["--override-packages"]).ok();
    assert_eq!(
        fx.read_store("ReplicatedStorage").instances[1].attributes["Mark"],
        json!("x")
    );
    let rename = [
        "rn",
        "ReplicatedStorage",
        "Other",
        "-i",
        "editor:1",
        "-r",
        &root,
    ];
    assert_eq!(fx.text(&rename).err(), refusal);
    fx.text(&[&rename[..], &["--override-packages"]].concat())
        .ok();
    assert_eq!(fx.names("ReplicatedStorage")[1], "Other");
}

#[test]
fn store_edits_refuse_changes_inside_a_read_only_link() {
    let fx = Fixture::new("store-read-only-link");
    crate::tests::support::write_packed_link_project(&fx.root);
    let store = fx.store_arg("ReplicatedStorage");
    fx.text(&["ba", "-f", &store, "-n", "Other", "-c", "Folder"])
        .ok();
    let before = fs::read(fx.store("ReplicatedStorage")).unwrap();
    let refusal = "Cannot edit read-only link \"shared\" at ReplicatedStorage.Shared. Use --override-packages to replace it intentionally.";
    let edits: [&[&str]; 5] = [
        &["bss", "-f", &store, "Shared.Util", "--str", "return 2"],
        &[
            "ba", "-f", &store, "-n", "Extra", "-c", "Folder", "-I", "Shared",
        ],
        &["bcl", "-f", &store, "Shared.Util"],
        &["br", "-f", &store, "Shared.Util"],
        &[
            "bs", "-f", &store, "Other", "-p", "Parent", "--str", "editor:1",
        ],
    ];
    for edit in edits {
        assert_eq!(fx.text(edit).err(), refusal, "{edit:?}");
    }
    for (label, op) in [
        (
            "add",
            r#"{"parent":"ReplicatedStorage.Shared","name":"Extra","class":"Folder"}"#,
        ),
        (
            "clone",
            r#"{"op":"clone","path":"ReplicatedStorage.Shared.Util","name":"Copy"}"#,
        ),
        (
            "remove",
            r#"{"op":"remove","path":"ReplicatedStorage.Shared.Util"}"#,
        ),
        (
            "move",
            r#"{"op":"move","path":"ReplicatedStorage.Other","parent":"ReplicatedStorage.Shared"}"#,
        ),
    ] {
        assert_eq!(
            run_batch(&fx, &format!("[{op}]")).unwrap_err(),
            format!("Batch op 0 ({label}): {refusal}")
        );
    }
    assert_eq!(fs::read(fx.store("ReplicatedStorage")).unwrap(), before);
    assert!(!fx.exists("src/ReplicatedStorage/Shared/Util.luau"));
    for edit in edits {
        fx.text(&[edit, &["--override-packages"]].concat()).ok();
    }
    assert_eq!(
        fx.names("ReplicatedStorage"),
        ["ReplicatedStorage", "Shared", "Other", "Extra", "Util Copy"]
    );
    let ops = crate::bytecode::add_batch::parse_batch_ops(
        r#"[{"op":"remove","path":"ReplicatedStorage.Shared.Other"}]"#,
    )
    .unwrap();
    crate::bytecode::add_batch::apply_store_batch(
        ops,
        &|service| Ok((fx.store(service), service.to_string())),
        false,
        true,
    )
    .unwrap();
    assert_eq!(
        fx.names("ReplicatedStorage"),
        ["ReplicatedStorage", "Shared", "Extra", "Util Copy"]
    );
}

#[test]
fn model_import_and_export_commands_use_project_targets() {
    let fx = Fixture::new("model-commands");
    let root = fx.path("");
    let kit = fx.root.join("kit.rbxm");
    write_model_file(&kit, &sample_model_dom());
    let kit_xml = fx.root.join("kit.rbxmx");
    write_model_file(&kit_xml, &sample_model_dom());
    let imported = fx
        .text(&[
            "mip",
            "Workspace",
            "-I",
            "cafe",
            "--model",
            &kit.to_string_lossy(),
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        imported,
        json!({
            "changedPaths": [STORE, "<root>/src/Workspace/Café ☕/Kit/Setup.luau"],
            "format": "rbxm",
            "model": "<root>/kit.rbxm",
            "ok": true,
            "rootSettingsIds": ["editor:9"],
            "service": "Workspace",
            "settingsFile": STORE,
            "settingsIds": ["editor:9", "editor:a", "editor:b", "editor:c"],
            "sourceWrites": [{"path": "<root>/src/Workspace/Café ☕/Kit/Setup.luau", "settingsId": "editor:b"}],
        })
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Café ☕/Kit/Setup.luau")).unwrap(),
        "return { kit = true }\n"
    );
    assert_eq!(
        fx.both(&[
            "in",
            "Workspace",
            "Café ☕.Kit.Primary",
            "-F",
            "props",
            "-r",
            &root
        ]),
        json!({"props": {"Value": {"_type": "Ref", "pathOrdinals": [1, 1, 1, 1], "pathSegments": ["Workspace", "Café ☕", "Kit", "Base"], "settingsId": "editor:a"}}})
    );
    let xml = fx
        .json(&[
            "mip",
            "Workspace",
            "-I",
            "Café ☕",
            "--model",
            &kit_xml.to_string_lossy(),
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(xml["format"], json!("rbxmx"));
    assert_eq!(
        xml["sourceWrites"][0]["path"],
        json!("<root>/src/Workspace/Café ☕/Kit Copy/Setup.luau")
    );
    let out = fx.root.join("out.rbxm");
    assert_eq!(
        fx.text(&[
            "mep",
            "Workspace",
            "-i",
            "cafe",
            "-o",
            &out.to_string_lossy(),
            "-r",
            &root
        ])
        .ok(),
        json!({"format": "rbxm", "instances": 9, "ok": true, "output": "<root>/out.rbxm", "rootSettingsIds": ["cafe"], "service": "Workspace", "settingsFile": STORE})
    );
    let out_xml = fx.root.join("out.rbxmx");
    assert_eq!(
        fx.json(&[
            "mep",
            "Workspace",
            "-i",
            "Café ☕.Kit",
            "-o",
            &out_xml.to_string_lossy(),
            "-r",
            &root
        ])
        .ok(),
        json!({"format": "rbxmx", "instances": 4, "ok": true, "output": "<root>/out.rbxmx", "rootSettingsIds": ["editor:9"], "service": "Workspace", "settingsFile": STORE})
    );
    let view = run_at(
        &fx.root,
        CaptureMode::Json,
        &["v", &out_xml.to_string_lossy(), "--json"],
    )
    .ok();
    assert_eq!(view["roots"][0]["name"], json!("Kit"));
    assert_eq!(
        view["roots"][0]["children"][1]["source"],
        json!("return { kit = true }\n")
    );
    let out_txt = fx.root.join("out.txt");
    assert_eq!(
        fx.text(&[
            "mep",
            "Workspace",
            "-i",
            "cafe",
            "-o",
            &out_txt.to_string_lossy(),
            "--format",
            "rbxmx",
            "-r",
            &root
        ])
        .ok()["format"],
        json!("rbxmx")
    );
    let err = |args: &[&str]| fx.text(&[args, &["-r", &root]].concat()).err();
    assert_eq!(
        err(&[
            "mep",
            "Workspace",
            "-i",
            "cafe",
            "-o",
            &out_txt.to_string_lossy()
        ]),
        "Invalid model format \"txt\". Use rbxm or rbxmx."
    );
    assert!(
        err(&[
            "mep",
            "Workspace",
            "-i",
            "nope",
            "-o",
            &out.to_string_lossy()
        ])
        .contains("Projected service 'Workspace' has no instance 'nope'")
    );
    assert!(
        err(&[
            "mep",
            "Workspace",
            "-i",
            "Border",
            "-o",
            &out.to_string_lossy()
        ])
        .contains("2 instances are named \"Border\"")
    );
    assert!(
        err(&[
            "mep",
            "StarterPack",
            "-i",
            "x",
            "-o",
            &out.to_string_lossy()
        ])
        .contains("Projected service 'StarterPack' has no Renium store")
    );
    assert!(
        err(&[
            "mip",
            "Workspace",
            "-I",
            "nope",
            "--model",
            &kit.to_string_lossy()
        ])
        .contains("Projected service 'Workspace' has no instance 'nope'")
    );
    assert_eq!(
        err(&[
            "mip",
            "Workspace",
            "-I",
            "cafe",
            "--model",
            &out_txt.to_string_lossy()
        ]),
        "Invalid model format \"txt\". Use rbxm or rbxmx."
    );
    assert!(
        err(&[
            "mip",
            "Workspace",
            "-I",
            "cafe",
            "--model",
            &fx.path("nope.rbxm")
        ])
        .starts_with("Failed to read")
    );
    let menu = fx
        .text(&[
            "cr",
            "StarterGui",
            "-c",
            "ScreenGui",
            "-n",
            "Menu",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(menu["ok"], true);
    assert_eq!(
        fx.text(&[
            "mip",
            "StarterGui",
            "-I",
            "Menu",
            "--model",
            &kit.to_string_lossy(),
            "-r",
            &root
        ])
        .ok()["rootSettingsIds"],
        json!(["editor:2"])
    );
}
