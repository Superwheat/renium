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
        add(&["-n", "X", "-c", "Part", "-p", "Anchored=\"yes\""]).err(),
        "Anchored needs true or false, got \"yes\"; for example `-p Anchored=true`"
    );
    assert_eq!(
        add(&["-n", "X", "-c", "Part", "-a", "has space=1"]).err(),
        "Attribute name \"has space\" holds ' '; Roblox takes letters, digits, `_`, `.`, `-` and `/`"
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
    let sign = add(&[
        "-n",
        "Sign",
        "-c",
        "StringValue",
        "-p",
        "Value=5",
        "-a",
        "Stage.Boss-1/hp=1",
    ])
    .ok();
    let instance =
        fx.read_store("Workspace").instances[sign["index"].as_u64().unwrap() as usize].clone();
    assert_eq!(instance.properties["Value"], json!("5"));
    assert_eq!(instance.attributes["Stage.Boss-1/hp"], json!(1));
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

pub(super) fn run_batch(fx: &Fixture, input: &str) -> Result<Value, String> {
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
            "Spawner",
            "Target",
            "Café ☕",
            "Props",
            "Border",
            "Helper",
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
        ("Part", Some(6))
    );
    assert_eq!(gate.properties["Anchored"], json!(false));
    assert_eq!(gate.properties["Tags"], json!(["Door"]));
    assert_eq!(gate.attributes["Kind"], json!("gate"));
    assert_eq!(gate.attributes["Owner"], json!("Grace"));
    assert_eq!(workspace.instances[7].parent_index, Some(6));
    assert_eq!(
        workspace.instances[7].properties["Transparency"],
        json!(0.75)
    );
    assert_eq!(workspace.instances[8].parent_index, Some(6));
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
fn batch_ops_resolve_and_carry_references_into_other_services() {
    let fx = outside_reference_fixture("batch-outside-refs");
    let mut workspace = fx.read_store("Workspace");
    workspace.instances.push(with_values(
        settings_instance("old", "Old", "ObjectValue", Some(0)),
        &[(
            "Value",
            json!({"_type": "Ref", "pathSegments": ["ServerScriptService", "Tools", "Util"]}),
        )],
        &[],
    ));
    fx.write_store("Workspace", &workspace);
    let store = fx.store_arg("Workspace");
    let value = |holder: &str| fx.both(&["bg", "-f", &store, holder, "Value"]);
    let result = run_batch(
        &fx,
        r##"[
            {"parent":"ServerScriptService.Tools","name":"Fresh","class":"Folder","ref":"fresh"},
            {"parent":"Workspace","name":"ToFresh","class":"ObjectValue","props":{"Value":"ServerScriptService.Tools.Fresh"}},
            {"op":"set","path":"Workspace.Target","props":{"Value":"ServerScriptService.Tools.Util"}},
            {"op":"rename","path":"ServerScriptService.Tools.Util","name":"Helper"},
            {"op":"rename","path":"#fresh","name":"Newer"},
            {"op":"clone","path":"Workspace.Target","name":"Twin"},
            {"op":"move","path":"ServerScriptService.Tools.Helper","parent":"ServerScriptService"},
            {"op":"rename","path":"ServerScriptService.Tools","name":"Kit"}
        ]"##,
    )
    .unwrap();
    assert_eq!(result["stores"], 2);
    let helper = outside_reference(&["ServerScriptService", "Helper"], &[1, 1], "editor:2");
    for holder in ["Target", "Twin", "Old"] {
        assert_eq!(value(holder), helper, "{holder}");
    }
    assert_eq!(
        value("ToFresh"),
        outside_reference(
            &["ServerScriptService", "Kit", "Newer"],
            &[1, 1, 1],
            "editor:3"
        )
    );
    let before = fs::read(fx.store("Workspace")).unwrap();
    let missing = run_batch(
        &fx,
        r#"[
            {"parent":"Workspace","name":"Unwritten","class":"Folder"},
            {"op":"remove","path":"ServerScriptService.Kit.Newer"},
            {"op":"set","path":"Workspace.Target","props":{"Value":"ServerScriptService.Kit.Newer"}}
        ]"#,
    )
    .unwrap_err();
    assert!(
        missing.ends_with(
            "Batch op 2 (set): Reference target was not found: ServerScriptService.Kit.Newer is not in the ServerScriptService store when the batch ends; add it in this batch or before it (`rbx f ServerScriptService NAME` finds a path, `rbx pl` refreshes the store from Studio)"
        ),
        "{missing}"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    run_batch(
        &fx,
        r#"[
            {"op":"remove","path":"ServerScriptService.Kit"},
            {"op":"remove","path":"ServerScriptService.Helper"},
            {"parent":"ServerScriptService","name":"Helper","class":"Folder"}
        ]"#,
    )
    .unwrap();
    for holder in ["Target", "Twin", "Old", "ToFresh"] {
        assert_eq!(value(holder), json!({"_type": "Ref"}), "{holder}");
    }
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
    let paths = studio
        .paths
        .iter()
        .map(|path| (path.path_segments.join("/"), path.path_ordinals.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            ("Workspace/Lobby".to_string(), vec![1, 1]),
            ("Workspace/Lobby/Door".to_string(), vec![1, 1, 1]),
            ("Workspace/Café ☕".to_string(), vec![1, 1]),
            ("Workspace/Target".to_string(), vec![1, 1]),
        ]
    );
    assert_eq!(studio.renames, [(0, "Hall".to_string())]);
    assert_eq!(studio.moves, [(1, 2)]);
    assert_eq!(studio.destroys, [3]);
    let luau = studio.luau();
    assert!(
        luau.contains(r#"in {{ {"Workspace", "Lobby"}, {1, 1} }, { {"Workspace", "Lobby", "Door"}, {1, 1, 1} }, { {"Workspace", "Café ☕"}, {1, 1} }, { {"Workspace", "Target"}, {1, 1} }} do"#)
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
fn a_preferred_merge_keeps_a_parent_that_breaking_a_cycle_needs() {
    let fx = Fixture::new("merge-cycle-orphans");
    run_batch(
        &fx,
        r##"[{"parent":"Workspace","name":"P","class":"Folder","ref":"p"},
            {"parent":"#p","name":"A","class":"Folder","ref":"a"},
            {"parent":"#a","name":"Leaf","class":"Folder"},
            {"parent":"Workspace","name":"B","class":"Folder"}]"##,
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
    // Ours moves A under B and removes P; theirs moves B under A.
    run_batch(
        &fx,
        r##"[{"op":"move","path":"Workspace.P.A","parent":"Workspace.B"}]"##,
    )
    .unwrap();
    fx.text(&["br", "-f", &workspace, "P"]).ok();
    let ours = snapshot("ours.renium");
    fs::copy(&base, &store).unwrap();
    run_batch(
        &fx,
        r##"[{"op":"move","path":"Workspace.B","parent":"Workspace.P.A"}]"##,
    )
    .unwrap();
    let theirs = snapshot("theirs.renium");
    let merged = fx.path("merged.renium");
    let placed = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(name, parent)| (name.to_string(), parent.to_string()))
            .collect::<Vec<_>>()
    };
    // Theirs wins: the cycle breaks toward their tree, which still has P.
    fx.text(&[
        "vcm", &base, &ours, &theirs, "-o", &merged, "--prefer", "theirs",
    ])
    .ok();
    assert_eq!(
        merged_placements(&merged, &["P", "A", "Leaf", "B"]),
        placed(&[("P", "Workspace"), ("A", "P"), ("Leaf", "A"), ("B", "A")])
    );
    // Ours wins: A sits under B and P stays removed.
    fx.text(&[
        "vcm", &base, &ours, &theirs, "-o", &merged, "--prefer", "ours",
    ])
    .ok();
    assert_eq!(
        merged_placements(&merged, &["P", "A", "Leaf", "B"]),
        placed(&[("A", "B"), ("Leaf", "A"), ("B", "Workspace")])
    );
}

#[test]
fn a_merge_settles_a_reference_to_an_instance_the_other_side_removed() {
    let fx = Fixture::new("merge-references");
    run_batch(
        &fx,
        r##"[{"parent":"Workspace","name":"Props","class":"Folder","ref":"props"},
            {"parent":"#props","name":"Sign","class":"StringValue","props":{"Value":"hello"}},
            {"parent":"#props","name":"Lamp","class":"Folder"}]"##,
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
    fx.text(&["br", "-f", &workspace, "Props.Sign"]).ok();
    let removed_sign = snapshot("removed-sign.renium");
    fs::copy(&base, &store).unwrap();
    run_batch(
        &fx,
        r##"[{"parent":"Workspace.Props","name":"Ptr","class":"ObjectValue","props":{"Value":"Workspace.Props.Sign"}}]"##,
    )
    .unwrap();
    let added_pointer = snapshot("added-pointer.renium");
    let merged = fx.path("merged.renium");
    let pointer_target = || {
        let document = SettingsBytecode::read_file(Path::new(&merged)).unwrap();
        let pointer = document
            .instances
            .iter()
            .find(|instance| instance.name == "Ptr")
            .expect("the added pointer is kept");
        let reference = &pointer.properties["Value"];
        let by_id = reference
            .get("settingsId")
            .and_then(Value::as_str)
            .and_then(|id| {
                document
                    .instances
                    .iter()
                    .find(|instance| instance.settings_id == id)
            });
        let by_index = reference
            .get("instanceIndex")
            .and_then(Value::as_u64)
            .and_then(|index| document.instances.get(index as usize - 1));
        let named =
            reference.get("settingsId").is_some() || reference.get("instanceIndex").is_some();
        let held = by_id
            .or(by_index)
            .is_some_and(|instance| instance.name == "Sign");
        (named, held)
    };

    let conflict = fx
        .text(&["vcm", &base, &removed_sign, &added_pointer, "-o", &merged])
        .err();
    assert!(conflict.contains("merge conflict"), "{conflict}");
    // The side that removed Sign wins: the pointer stays and points at nothing.
    fx.text(&[
        "vcm",
        &base,
        &removed_sign,
        &added_pointer,
        "-o",
        &merged,
        "--prefer",
        "ours",
    ])
    .ok();
    assert_eq!(pointer_target(), (false, false));
    // The side that points at Sign wins: Sign comes back.
    fx.text(&[
        "vcm",
        &base,
        &removed_sign,
        &added_pointer,
        "-o",
        &merged,
        "--prefer",
        "theirs",
    ])
    .ok();
    assert_eq!(pointer_target(), (true, true));
    // The merged store builds into a place either way.
    fs::copy(&merged, &store).unwrap();
    fx.text(&["bep", "-r", &fx.path(""), "-o", &fx.path("merged.rbxl")])
        .ok();
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
    run_batch(
        &fx,
        r#"[
            {"op":"set","path":"Workspace.Lobby.Door","props":{"Transparency":"0.25"},"attrs":{"Stage.Boss-1/hp":1}},
            {"parent":"Workspace","name":"Sign","class":"StringValue","props":{"Value":5}}
        ]"#,
    )
    .unwrap();
    let typed = fx.read_store("Workspace");
    assert_eq!(typed.instances[2].properties["Transparency"], json!(0.25));
    assert_eq!(
        typed.instances.last().unwrap().properties["Value"],
        json!("5")
    );
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
            r#"[{"op":"set","path":"Workspace.Lobby.Door","props":{"Anchored":"yes"}}]"#,
            "Batch op 0 (set): Anchored needs true or false, got \"yes\"; for example `-p Anchored=true`",
        ),
        (
            r#"[{"op":"set","path":"Workspace.Lobby.Door","attrs":{"has space":1}}]"#,
            "Batch op 0 (set): Attribute name \"has space\" holds ' '; Roblox takes letters, digits, `_`, `.`, `-` and `/`",
        ),
        (
            r#"[{"parent":"Workspace","name":"A","class":"Folder","attrs":{"":1}}]"#,
            "Batch op 0 (add): An attribute name cannot be empty",
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
fn clone_command_copies_subtrees_with_unique_names_scripts_and_references() {
    let fx = Fixture::new("clone");
    let store = fx.store_arg("Workspace");
    let root = fx.path("");
    let copy = |source: &str, parent: &str| {
        fx.text(&["cp", "Workspace", "-i", source, "-I", parent, "-r", &root])
            .ok()
    };
    assert_eq!(
        copy("Lobby.Door", "Lobby"),
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
        fx.json(&["cp", "Workspace", "-i", "door", "-I", "lobby", "-r", &root])
            .ok(),
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
        copy("door", "cafe"),
        cloned(
            "editor:b",
            "Door",
            "Part",
            &["Workspace", "Café ☕", "Door"],
            &[1, 1, 1],
            &["editor:b"]
        )
    );
    assert_eq!(copy("door", "Café ☕")["name"], json!("Door Copy"));
    assert_eq!(
        copy("door", "Workspace")["pathSegments"],
        json!(["Workspace", "Door"])
    );
    assert_eq!(
        copy("door", "Lobby.Border[2]")["pathOrdinals"],
        json!([1, 1, 2, 1])
    );
    assert_eq!(
        fx.read_store("Workspace").instances[9].properties,
        fx.read_store("Workspace").instances[2].properties
    );
    assert_eq!(
        copy("Spawner", "Workspace"),
        json!({
            "changedPaths": [STORE, "<root>/src/Workspace/Spawner Copy.server.luau"],
            "className": "Script",
            "name": "Spawner Copy",
            "ok": true,
            "pathOrdinals": [1, 1],
            "pathSegments": ["Workspace", "Spawner Copy"],
            "rootSettingsId": "editor:f",
            "service": "Workspace",
            "settingsFile": STORE,
            "settingsIds": ["editor:f"],
            "sourceCopies": [{"from": "<root>/src/Workspace/Spawner.server.luau", "to": "<root>/src/Workspace/Spawner Copy.server.luau"}],
        })
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Spawner Copy.server.luau")).unwrap(),
        SPAWNER_SOURCE
    );
    let lobby = copy("Lobby", "Workspace");
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
    let target = copy("Target", "Workspace");
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
    let cafe_copy = copy("Café ☕", "Workspace");
    let copied_holder = fx.both(&["bg", "-f", &store, "Café ☕ Copy.Holder", "Value"]);
    assert_eq!(
        copied_holder["pathSegments"],
        json!(["Workspace", "Café ☕ Copy", "Door"])
    );
    assert_eq!(cafe_copy["name"], json!("Café ☕ Copy"));
}

#[test]
fn clone_command_refuses_roots_cycles_duplicates_and_missing_targets() {
    let fx = Fixture::new("clone-errors");
    let root = fx.path("");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let err = |args: &[&str]| {
        fx.text(&[&["cp", "Workspace"], args, &["-r", &root]].concat())
            .err()
    };
    assert_eq!(
        err(&["-i", "ws", "-I", "cafe"]),
        "Service roots cannot be copied"
    );
    assert_eq!(
        err(&["-i", "lobby", "-I", "door"]),
        "Cannot copy an instance into itself or one of its descendants"
    );
    assert_eq!(
        err(&["-i", "lobby", "-I", "lobby"]),
        "Cannot copy an instance into itself or one of its descendants"
    );
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
    let collision = fx.service_dir("Workspace").join("Spawner Copy.server.luau");
    fs::write(&collision, "return 2").unwrap();
    assert!(err(&["-i", "Spawner", "-I", "ws"]).contains("already exists with different contents"));
    assert_eq!(fs::read_to_string(&collision).unwrap(), "return 2");
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    assert!(
        fx.text(&["cp", "StarterGui", "-i", "x", "-I", "y", "-r", &root])
            .err()
            .contains("Projected service 'StarterGui' has no Renium store")
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
            "changedPaths": ["<root>/instances/ReplicatedStorage.renium", STORE],
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
        json!({"_type": "Ref"})
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
            "Spawner",
            "Target",
            "Café ☕",
            "Border",
            "Border",
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
        fx.text(&["br", "-f", &store, "-x", "4"]).ok()["removedSettingsIds"],
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
        fx.text(&["cp", "Workspace", "-i", "Pkg", "-I", "ws", "-r", &root])
            .err(),
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
        json!({"changedPaths": ["<root>/instances/ReplicatedStorage.renium", STORE], "ok": true, "removed": ["Workspace.Lobby.Gate"], "removedSettingsIds": ["door"], "settingsFile": STORE, "storeRemoved": false})
    );
    let kept = fx
        .json(&["rm", "Workspace", "-i", "lobby", "-R", "-r", &root])
        .ok();
    assert_eq!(kept["removedSettingsIds"], json!(["lobby"]));
    assert_eq!(
        fx.names("Workspace"),
        [
            "Workspace",
            "Boot",
            "Target",
            "a/b",
            "Inside",
            "Border",
            "Edge",
            "Helper"
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
fn project_commands_keep_references_from_other_services_current() {
    let fx = outside_reference_fixture("command-refs");
    let root = fx.path("");
    let workspace = fx.store_arg("Workspace");
    let replicated = fx.store_arg("ReplicatedStorage");
    for (store, holder) in [(&workspace, "Target"), (&replicated, "Link")] {
        fx.text(&[
            "bs",
            "-f",
            store,
            holder,
            "-p",
            "Value",
            "--str",
            "ServerScriptService.Tools.Util",
        ])
        .ok();
    }
    let run = |args: &[&str]| {
        fx.text(&[args, &["-r", &root]].concat()).ok();
    };
    // The holder goes into the Workspace folder that shares the target's
    // id; a copy of that folder must not take the reference for one to the
    // copied folder.
    run(&["mv", "Workspace", "-i", "target", "-I", "editor:2"]);
    run(&["cp", "Workspace", "-i", "editor:2", "-I", "lobby"]);
    let held = || {
        [
            fx.both(&["bg", "-f", &workspace, "Clash.Target", "Value"]),
            fx.both(&["bg", "-f", &workspace, "Lobby.Clash.Target", "Value"]),
            fx.both(&["bg", "-f", &replicated, "Link", "Value"]),
        ]
    };
    let all = |reference: Value| [reference.clone(), reference.clone(), reference];
    assert_eq!(
        held(),
        all(outside_reference(
            &["ServerScriptService", "Tools", "Util"],
            &[1, 1, 1],
            "editor:2"
        ))
    );
    run(&["rn", "ServerScriptService", "-i", "Tools.Util", "Helper"]);
    assert_eq!(
        held(),
        all(outside_reference(
            &["ServerScriptService", "Tools", "Helper"],
            &[1, 1, 1],
            "editor:2"
        ))
    );
    run(&["mv", "ServerScriptService", "-i", "Tools.Helper"]);
    assert_eq!(
        held(),
        all(outside_reference(
            &["ServerScriptService", "Helper"],
            &[1, 1],
            "editor:2"
        ))
    );
    // Workspace has an instance with the id editor:2, so the moved instance
    // takes a new one there.
    run(&[
        "mv",
        "ServerScriptService",
        "-i",
        "Helper",
        "--to-service",
        "Workspace",
        "-I",
        "lobby",
    ]);
    let moved = fx.read_store("Workspace");
    let helper = moved
        .instances
        .iter()
        .find(|instance| instance.name == "Helper" && instance.class_name == "Folder")
        .unwrap();
    assert_ne!(helper.settings_id, "editor:2");
    assert_eq!(
        held(),
        all(outside_reference(
            &["Workspace", "Lobby", "Helper"],
            &[1, 1, 2],
            &helper.settings_id
        ))
    );
    run(&["rm", "Workspace", "-i", &helper.settings_id]);
    assert_eq!(held(), all(json!({"_type": "Ref"})));
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
    let root = fx.path("");
    fx.text(&["ba", "-f", &store, "-n", "Other", "-c", "Folder"])
        .ok();
    let before = fs::read(fx.store("ReplicatedStorage")).unwrap();
    let refusal = "Cannot edit read-only link \"shared\" at ReplicatedStorage.Shared. Use --override-packages to replace it intentionally.";
    let edits: [&[&str]; 5] = [
        &["bss", "-f", &store, "Shared.Util", "--str", "return 2"],
        &[
            "ba", "-f", &store, "-n", "Extra", "-c", "Folder", "-I", "Shared",
        ],
        &[
            "cp",
            "ReplicatedStorage",
            "-i",
            "Shared.Util",
            "-I",
            "Shared",
            "-r",
            &root,
        ],
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
        ["ReplicatedStorage", "Shared", "Extra", "Util Copy", "Other"]
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

/// A project whose read-only link "shared" sits at
/// ReplicatedStorage.Outer.Shared, after an unlinked Spare folder.
fn nested_link_fixture(tag: &str) -> Fixture {
    let fx = Fixture::new(tag);
    crate::tests::support::write_packed_link_project(&fx.root);
    let mut util = settings_instance("editor:2", "Util", "ModuleScript", Some(3));
    util.properties
        .insert("Source".to_string(), json!("return { version = 1 }"));
    fx.write_store(
        "ReplicatedStorage",
        &settings_document(vec![
            settings_instance("editor:0", "ReplicatedStorage", "ReplicatedStorage", None),
            settings_instance("editor:3", "Outer", "Folder", Some(0)),
            settings_instance("editor:4", "Spare", "Folder", Some(1)),
            settings_instance("editor:1", "Shared", "Folder", Some(1)),
            util,
        ]),
    );
    fs::write(
        fx.root.join("renium-link.json"),
        br#"{"version":1,"links":[{"id":"shared","readOnly":true,"source":{"type":"local","path":"links/shared.renium"},"targets":[{"service":"ReplicatedStorage","path":["ReplicatedStorage","Outer","Shared"]}]}],"broken":[]}"#,
    )
    .unwrap();
    fx
}

const NESTED_LINK_REFUSAL: &str = "Cannot edit read-only link \"shared\" at ReplicatedStorage.Outer.Shared. Use --override-packages to replace it intentionally.";

#[test]
fn edits_refuse_removing_renaming_and_moving_what_holds_a_read_only_link() {
    let fx = nested_link_fixture("link-holder");
    let store = fx.store_arg("ReplicatedStorage");
    let root = fx.path("");
    let other = fx
        .text(&["ba", "-f", &store, "-n", "Other", "-c", "Folder"])
        .ok()["settingsId"]
        .as_str()
        .unwrap()
        .to_string();
    let before = fs::read(fx.store("ReplicatedStorage")).unwrap();
    let rename = [
        "rn",
        "ReplicatedStorage",
        "Renamed",
        "-i",
        "editor:3",
        "-r",
        &root,
    ];
    let edits: [&[&str]; 7] = [
        &["br", "-f", &store, "Outer"],
        &["br", "-f", &store, "Outer", "-R"],
        &[
            "bs", "-f", &store, "Outer", "-p", "Name", "--str", "Renamed",
        ],
        &["bs", "-f", &store, "Outer", "-p", "Parent", "--str", &other],
        &["rm", "ReplicatedStorage", "-i", "editor:3", "-r", &root],
        &rename,
        &[
            "mv",
            "ReplicatedStorage",
            "-i",
            "editor:3",
            "-I",
            &other,
            "-r",
            &root,
        ],
    ];
    for edit in edits {
        assert_eq!(fx.text(edit).err(), NESTED_LINK_REFUSAL, "{edit:?}");
    }
    for (label, op) in [
        (
            "remove",
            r#"{"op":"remove","path":"ReplicatedStorage.Outer"}"#,
        ),
        (
            "rename",
            r#"{"op":"rename","path":"ReplicatedStorage.Outer","name":"Renamed"}"#,
        ),
        (
            "move",
            r#"{"op":"move","path":"ReplicatedStorage.Outer","parent":"ReplicatedStorage.Other"}"#,
        ),
    ] {
        assert_eq!(
            run_batch(&fx, &format!("[{op}]")).unwrap_err(),
            format!("Batch op 0 ({label}): {NESTED_LINK_REFUSAL}")
        );
    }
    assert_eq!(fs::read(fx.store("ReplicatedStorage")).unwrap(), before);
    let mark = [
        "bs",
        "-f",
        &store,
        "Outer",
        "-p",
        "Mark",
        "-S",
        "attribute",
        "--str",
        "x",
    ];
    fx.text(&mark).ok();
    fx.text(&[&rename[..], &["--override-packages"]].concat())
        .ok();
    assert_eq!(fx.names("ReplicatedStorage")[1], "Renamed");
    fx.text(&["br", "-f", &store, "Renamed", "--override-packages"])
        .ok();
    assert_eq!(
        fx.names("ReplicatedStorage"),
        ["ReplicatedStorage", "Other"]
    );
}

#[test]
fn rename_where_refuses_a_match_that_changes_a_read_only_link() {
    let fx = nested_link_fixture("link-rename-where");
    let root = fx.path("");
    let before = fs::read(fx.store("ReplicatedStorage")).unwrap();
    let rename = |from: &str, to: &str, extra: &[&str]| {
        let args = [
            "rn",
            "ReplicatedStorage",
            "-i",
            "editor:3",
            "--where",
            "is:Folder",
            "--replace",
            from,
            to,
            "-r",
            &root,
        ];
        fx.text(&[&args[..], extra].concat())
    };
    assert_eq!(rename("Shared", "Changed", &[]).err(), NESTED_LINK_REFUSAL);
    assert_eq!(rename("S", "Z", &[]).err(), NESTED_LINK_REFUSAL);
    assert_eq!(fs::read(fx.store("ReplicatedStorage")).unwrap(), before);
    rename("Spare", "Free", &[]).ok();
    assert_eq!(fx.names("ReplicatedStorage")[2], "Free");
    rename("Shared", "Changed", &["--override-packages"]).ok();
    assert_eq!(fx.names("ReplicatedStorage")[3], "Changed");
}

#[test]
fn property_batches_refuse_renaming_and_moving_what_holds_a_read_only_link() {
    let fx = nested_link_fixture("link-property-batch");
    let store = fx.store_arg("ReplicatedStorage");
    let root = fx.path("");
    let other = fx
        .text(&["ba", "-f", &store, "-n", "Other", "-c", "Folder"])
        .ok()["settingsId"]
        .as_str()
        .unwrap()
        .to_string();
    let before = fs::read(fx.store("ReplicatedStorage")).unwrap();
    let batch = fx.path("panel-edit.json");
    let panel_edit = |scope: &str, property: &str, value: &str, extra: &[&str]| {
        fs::write(
            &batch,
            json!([{
                "service": "ReplicatedStorage",
                "settingsId": "editor:3",
                "className": "Folder",
                "pathSegments": ["ReplicatedStorage", "Outer"],
                "pathOrdinals": [1, 1],
                "scope": scope,
                "property": property,
                "value": value,
            }])
            .to_string(),
        )
        .unwrap();
        let args = [
            "bytecode-apply-property-batch",
            "--project-root",
            &root,
            "--input",
            &batch,
            "--direction",
            "files-to-studio",
        ];
        fx.text(&[&args[..], extra].concat())
    };
    for (scope, property, value) in [
        ("metadata", "Name", "Renamed"),
        ("auto", "Name", "Renamed"),
        ("metadata", "Parent", other.as_str()),
    ] {
        assert_eq!(
            panel_edit(scope, property, value, &[]).err(),
            NESTED_LINK_REFUSAL,
            "{scope} {property}"
        );
    }
    assert_eq!(fs::read(fx.store("ReplicatedStorage")).unwrap(), before);
    assert_eq!(
        panel_edit("attribute", "Mark", "x", &[]).ok()["applied"],
        json!(1)
    );
    panel_edit("metadata", "Name", "Renamed", &["--override-packages"]).ok();
    assert_eq!(fx.names("ReplicatedStorage")[1], "Renamed");
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

/// A place the pushed-edit chunks can run against: `Workspace.Borders` holds
/// three folders named `Border` whose `Which` attribute tells them apart,
/// `Workspace.Spare` is empty, and the ServerStorage service was renamed to
/// `Vault` while a plain folder carries its old name.
const MOCK_PLACE: &str = r#"
local kids, services = {}, {}
function new(name, parent, which)
    local state = { Name = name, ClassName = 'Folder' }
    local methods = {
        GetChildren = function(self) return table.clone(kids[self]) end,
        FindFirstChild = function(self, wanted)
            for _, child in kids[self] do if child.Name == wanted then return child end end
            return nil
        end,
        FindService = function(_, wanted) return services[wanted] end,
        Destroy = function(self)
            for _, child in table.clone(kids[self]) do child:Destroy() end
            self.Parent = nil
        end,
        GetAttribute = function(_, key) return if key == 'Which' then which else nil end,
        GetTags = function() return {} end,
        GetFullName = function()
            local above = state.Parent
            return if above and above ~= game then above:GetFullName() .. '.' .. state.Name else state.Name
        end,
    }
    local inst = setmetatable({}, {
        __index = function(_, key) return methods[key] or state[key] end,
        __newindex = function(self, key, value)
            if key ~= 'Parent' then state[key] = value return end
            if state.Parent then table.remove(kids[state.Parent], table.find(kids[state.Parent], self)) end
            state.Parent = value
            if value then table.insert(kids[value], self) end
        end,
    })
    kids[inst] = {}
    inst.Parent = parent
    return inst
end
game = new('Place')
local workspace = new('Workspace', game)
local borders = new('Borders', workspace)
for _, which in { 'first', 'second', 'third' } do new('Border', borders, which) end
new('Spare', workspace)
services.ServerStorage = new('Vault', game)
new('Item', services.ServerStorage, 'stored')
new('ServerStorage', game)
function children(path)
    local inst = game
    for name in string.gmatch(path, '[^.]+') do inst = inst:FindFirstChild(name) end
    local out = {}
    for _, child in kids[inst] do table.insert(out, child.Name .. ':' .. (child:GetAttribute('Which') or '-')) end
    return table.concat(out, ',')
end
"#;

fn mock_place() -> mlua::Lua {
    let lua = mlua::Lua::new();
    lua.load(MOCK_PLACE).exec().unwrap();
    lua
}

fn run_in(place: &mlua::Lua, code: &str) -> Value {
    use mlua::LuaSerdeExt;
    let value: mlua::Value = place.load(code).eval().unwrap();
    place.from_value(value).unwrap()
}

fn children_in(place: &mlua::Lua, path: &str) -> String {
    place
        .load(format!("return children({path:?})"))
        .eval()
        .unwrap()
}

fn store_path(segments: &[&str], ordinals: &[usize]) -> crate::editor::types::EditorInstancePath {
    crate::editor::types::EditorInstancePath {
        path_segments: segments.iter().map(|segment| segment.to_string()).collect(),
        path_ordinals: ordinals.to_vec(),
    }
}

fn border(ordinal: usize) -> crate::editor::types::EditorInstancePath {
    store_path(&["Workspace", "Borders", "Border"], &[1, 1, ordinal])
}

#[test]
fn pushed_edits_reach_the_same_named_sibling_the_store_names() {
    use crate::bytecode::edit::{destroy_luau, read_back_luau, read_back_verifies};

    let place = mock_place();
    let removed = run_in(&place, &destroy_luau(&[border(2)], None));
    assert_eq!(removed["destroyed"].as_f64(), Some(1.0), "{removed}");
    assert_eq!(
        children_in(&place, "Workspace.Borders"),
        "Border:first,Border:third"
    );

    let place = mock_place();
    run_in(&place, &destroy_luau(&[border(1), border(2)], None));
    assert_eq!(children_in(&place, "Workspace.Borders"), "Border:third");

    let place = mock_place();
    let renames = crate::bytecode::add_batch::StudioPrep {
        paths: vec![border(1), border(2)],
        renames: vec![(0, "Edge".to_string()), (1, "Mid".to_string())],
        ..Default::default()
    };
    let renamed = run_in(&place, &renames.luau());
    assert_eq!(renamed["renamed"].as_f64(), Some(2.0), "{renamed}");
    assert_eq!(
        children_in(&place, "Workspace.Borders"),
        "Edge:first,Mid:second,Border:third"
    );
    let absent = run_in(&place, &destroy_luau(&[border(2)], None));
    assert_eq!(absent["destroyed"].as_f64(), Some(0.0), "{absent}");
    assert_eq!(absent["missing"], json!(["Workspace.Borders.Border[2]"]));
    assert_eq!(
        children_in(&place, "Workspace.Borders"),
        "Edge:first,Mid:second,Border:third"
    );

    let place = mock_place();
    let rows = run_in(
        &place,
        &read_back_luau(&[border(3), border(4)], &[], &["Which".to_string()]),
    );
    assert_eq!(rows[0]["@Which"], json!("third"), "{rows}");
    assert_eq!(rows[0]["path"], json!("Workspace.Borders.Border"));
    assert_eq!(
        rows[1],
        json!({"missing": true, "path": "Workspace.Borders.Border[4]"})
    );

    let stored = store_path(&["ServerStorage", "Item"], &[1, 1]);
    let which = ["Which".to_string()];
    let rows = run_in(
        &place,
        &read_back_luau(std::slice::from_ref(&stored), &[], &which),
    );
    assert_eq!(rows[0]["path"], json!("ServerStorage.Item"), "{rows}");
    let mut item = settings_instance("item", "Item", "Folder", Some(0));
    item.attributes.insert("Which".into(), json!("stored"));
    assert_eq!(
        read_back_verifies(&rows[0], &stored.path_segments, &item, &[], &which),
        Some(true)
    );
    let removed = run_in(&place, &destroy_luau(&[stored], None));
    assert_eq!(removed["destroyed"].as_f64(), Some(1.0), "{removed}");
    assert_eq!(children_in(&place, "Vault"), "");

    let place = mock_place();
    let borders = store_path(&["Workspace", "Borders"], &[1, 1]);
    let removed = run_in(&place, &destroy_luau(&[borders], Some(&[])));
    assert_eq!(removed["destroyed"].as_f64(), Some(1.0), "{removed}");
    assert_eq!(
        children_in(&place, "Workspace"),
        "Spare:-,Border:first,Border:second,Border:third"
    );
}

/// Removes `targets` without their descendants from a store whose Workspace
/// holds `tree`, runs the destroy chunk in a place that holds the same tree,
/// and returns the same-named children the store and the place end up with
/// under Workspace.
fn removal_keeps(
    tree: &[(&str, &str, usize)],
    setup: &str,
    targets: &[usize],
) -> (Vec<String>, String) {
    use crate::bytecode::edit::{destroy_luau, kept_instance_moves};
    use crate::editor::paths::build_editor_instance_paths;
    use crate::settings::instance::{order_reparented_instances_last, remove_instances_at_indices};

    let mut instances = vec![settings_instance("ws", "Workspace", "Workspace", None)];
    for (id, name, parent) in tree {
        instances.push(settings_instance(*id, *name, "Folder", Some(*parent)));
    }
    let before = settings_document(instances);
    let mut after = before.clone();
    remove_instances_at_indices(&mut after, targets, false).unwrap();
    order_reparented_instances_last(&mut after, &before);
    let paths = build_editor_instance_paths(&before, "Workspace");
    let removed = targets
        .iter()
        .map(|&index| paths[index].clone().unwrap())
        .collect::<Vec<_>>();
    let kept = kept_instance_moves(&before, &after, "Workspace");
    let place = mock_place();
    place.load(setup).exec().unwrap();
    let outcome = run_in(&place, &destroy_luau(&removed, Some(&kept)));
    assert_eq!(
        outcome["destroyed"].as_f64(),
        Some(targets.len() as f64),
        "{outcome}"
    );
    let stored = after
        .instances
        .iter()
        .filter(|instance| instance.parent_index == Some(0))
        .map(|instance| instance.settings_id.clone())
        .collect();
    (stored, children_in(&place, "Workspace"))
}

#[test]
fn a_pushed_removal_keeps_descendants_in_the_store_order() {
    let (stored, live) = removal_keeps(
        &[
            ("a", "A", 0),
            ("in-a", "Border", 1),
            ("b", "B", 0),
            ("in-b", "Border", 3),
        ],
        "local ws = game:FindFirstChild('Workspace') local a = new('A', ws) new('Border', a, 'in-a') local b = new('B', ws) new('Border', b, 'in-b')",
        &[3, 1],
    );
    assert_eq!(stored, ["in-a", "in-b"]);
    assert_eq!(live, "Borders:-,Spare:-,Border:in-a,Border:in-b");

    let (stored, live) = removal_keeps(
        &[
            ("p", "P", 0),
            ("c", "C", 1),
            ("deep", "Border", 2),
            ("shallow", "Border", 1),
        ],
        "local p = new('P', game:FindFirstChild('Workspace')) local c = new('C', p) new('Border', c, 'deep') new('Border', p, 'shallow') new('Extra', c, 'unknown')",
        &[1, 2],
    );
    assert_eq!(stored, ["deep", "shallow"]);
    assert_eq!(
        live,
        "Borders:-,Spare:-,Border:deep,Border:shallow,Extra:unknown"
    );
}

fn child_ids(fx: &Fixture, parent: &str) -> Vec<String> {
    let document = fx.read_store("Workspace");
    let parent = document
        .instances
        .iter()
        .position(|instance| instance.settings_id == parent);
    assert!(parent.is_some());
    document
        .instances
        .iter()
        .enumerate()
        .filter(|(_, instance)| instance.parent_index == parent)
        .map(|(index, instance)| {
            assert!(instance.parent_index < Some(index));
            instance.settings_id.clone()
        })
        .collect()
}

#[test]
fn a_store_move_puts_the_instance_last_under_its_new_parent() {
    let fx = Fixture::new("move-order");
    let root = fx.path("");
    let mv = |id: &str, parent: &str| {
        fx.text(&["mv", "Workspace", "-i", id, "-I", parent, "-r", &root])
            .ok()
    };
    mv("border1", "cafe");
    assert_eq!(child_ids(&fx, "lobby"), ["door", "border2", "helper"]);
    assert_eq!(mv("border1", "lobby")["pathOrdinals"], json!([1, 1, 2]));
    assert_eq!(
        child_ids(&fx, "lobby"),
        ["door", "border2", "helper", "border1"]
    );
    mv("lobby", "cafe");
    assert_eq!(child_ids(&fx, "ws"), ["spawner", "target", "cafe"]);
    assert_eq!(child_ids(&fx, "cafe"), ["lobby"]);
    assert_eq!(
        child_ids(&fx, "lobby"),
        ["door", "border2", "helper", "border1"]
    );

    let fx = Fixture::new("move-order-batch");
    run_batch(
        &fx,
        r##"[
            {"op":"move","path":"Workspace.Lobby.Helper","parent":"Workspace.Café ☕"},
            {"op":"move","path":"Workspace.Lobby.Border[1]","parent":"Workspace.Café ☕"},
            {"op":"move","path":"Workspace.Lobby.Door","parent":"Workspace"},
            {"op":"move","path":"Workspace.Door","parent":"Workspace.Lobby"},
            {"parent":"Workspace.Café ☕","name":"Menu","class":"Folder","ref":"menu"},
            {"op":"move","path":"Workspace.Spawner","parent":"#menu"}
        ]"##,
    )
    .unwrap();
    assert_eq!(child_ids(&fx, "lobby"), ["door", "border2"]);
    assert_eq!(child_ids(&fx, "cafe"), ["border1", "helper", "editor:9"]);
    assert_eq!(child_ids(&fx, "editor:9"), ["spawner"]);
    assert_eq!(child_ids(&fx, "ws"), ["lobby", "target", "cafe"]);

    let fx = Fixture::new("move-order-batch-selector");
    run_batch(
        &fx,
        r##"[{"op":"move","path":"Workspace.Lobby.Border[2]","parent":"Workspace.Café ☕"}]"##,
    )
    .unwrap();
    run_batch(
        &fx,
        r##"[
            {"op":"move","path":"Workspace.Lobby.Border","parent":"Workspace.Café ☕"},
            {"op":"set","path":"Workspace.Café ☕.Border[1]","attrs":{"Resident":true}},
            {"op":"set","path":"Workspace.Café ☕.Border[2]","props":{"Transparency":0.5}}
        ]"##,
    )
    .unwrap();
    assert_eq!(child_ids(&fx, "cafe"), ["border2", "border1"]);
    let document = fx.read_store("Workspace");
    let stored = |id: &str| {
        document
            .instances
            .iter()
            .find(|instance| instance.settings_id == id)
            .unwrap()
    };
    assert_eq!(
        stored("border2").attributes.get("Resident"),
        Some(&json!(true))
    );
    assert_eq!(stored("border1").attributes.get("Resident"), None);
    assert_eq!(stored("border1").properties["Transparency"], json!(0.5));
    assert_eq!(stored("border2").properties["Transparency"], json!(0.75));
}

#[test]
fn pushed_moves_take_the_same_named_sibling_the_store_names() {
    use crate::bytecode::add_batch::StudioPrep;
    use crate::project::commands::reparent_luau;

    let spare = store_path(&["Workspace", "Spare"], &[1, 1]);
    let place = mock_place();
    assert_eq!(
        run_in(&place, &reparent_luau(&border(2), &spare)),
        json!("moved")
    );
    assert_eq!(
        children_in(&place, "Workspace.Borders"),
        "Border:first,Border:third"
    );
    assert_eq!(children_in(&place, "Workspace.Spare"), "Border:second");
    let borders = store_path(&["Workspace", "Borders"], &[1, 1]);
    assert_eq!(
        run_in(&place, &reparent_luau(&border(1), &borders)),
        json!("unchanged")
    );
    assert_eq!(
        run_in(&place, &reparent_luau(&border(3), &spare)),
        json!("missing")
    );

    let place = mock_place();
    let prep = StudioPrep {
        paths: vec![border(1), border(2), border(3), spare],
        renames: vec![(2, "Last".to_string())],
        moves: vec![(0, 3)],
        destroys: vec![1],
    };
    let outcome = run_in(&place, &prep.luau());
    assert_eq!(outcome["renamed"].as_f64(), Some(1.0), "{outcome}");
    assert_eq!(outcome["moved"].as_f64(), Some(1.0), "{outcome}");
    assert_eq!(outcome["destroyed"].as_f64(), Some(1.0), "{outcome}");
    assert_eq!(children_in(&place, "Workspace.Borders"), "Last:third");
    assert_eq!(children_in(&place, "Workspace.Spare"), "Border:first");

    let mut instances = vec![
        settings_instance("ws", "Workspace", "Workspace", None),
        settings_instance("borders", "Borders", "Folder", Some(0)),
    ];
    for id in ["first", "second", "third"] {
        instances.push(settings_instance(id, "Border", "Folder", Some(1)));
    }
    instances.push(settings_instance("spare", "Spare", "Folder", Some(0)));
    let before = settings_document(instances);
    let edited = |edit: &dyn Fn(&mut SettingsBytecode)| {
        let mut after = before.clone();
        edit(&mut after);
        crate::settings::instance::order_reparented_instances_last(&mut after, &before);
        let mut prep = StudioPrep::default();
        prep.add_store(&before, &after, "Workspace").unwrap();
        let place = mock_place();
        (run_in(&place, &prep.luau()), place, after)
    };

    let (outcome, place, after) = edited(&|after| {
        after.instances[4].parent_index = Some(5);
        after.instances[2].parent_index = Some(5);
        after.instances[3].name = "Mid".to_string();
    });
    assert_eq!(outcome["moved"].as_f64(), Some(2.0), "{outcome}");
    assert_eq!(outcome["renamed"].as_f64(), Some(1.0), "{outcome}");
    assert_eq!(children_in(&place, "Workspace.Borders"), "Mid:second");
    assert_eq!(
        children_in(&place, "Workspace.Spare"),
        "Border:first,Border:third"
    );
    let under_spare = after
        .instances
        .iter()
        .filter(|instance| {
            instance
                .parent_index
                .is_some_and(|parent| after.instances[parent].settings_id == "spare")
        })
        .map(|instance| instance.settings_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(under_spare, ["first", "third"]);

    let (outcome, place, _) = edited(&|after| after.instances[1].parent_index = Some(5));
    assert_eq!(outcome["moved"].as_f64(), Some(1.0), "{outcome}");
    assert_eq!(children_in(&place, "Workspace"), "Spare:-");
    assert_eq!(
        children_in(&place, "Workspace.Spare.Borders"),
        "Border:first,Border:second,Border:third"
    );
}

#[test]
fn pushed_edits_report_a_store_the_project_does_not_have() {
    let fx = Fixture::new("cold-store-push");
    fs::remove_file(fx.store("Workspace")).unwrap();
    let store = fx.store_arg("Workspace");
    let edits: [&[&str]; 3] = [
        &["Lobby", "-p", "Parent", "--str", "ws", "--push"],
        &["Lobby", "-p", "Name", "--str", "Hall", "--push"],
        &["--where", "Border", "-p", "Parent", "--str", "ws", "--push"],
    ];
    for edit in edits {
        let error = fx.text(&[&["bs", "-f", &store], edit].concat()).err();
        assert!(
            error.contains("No synced Renium store for service 'Workspace'"),
            "{edit:?}: {error}"
        );
    }
}

#[test]
fn a_store_edit_prepares_studio_for_its_own_renames_and_moves_only() {
    use crate::bytecode::set_property_outcome;
    use crate::cli::BytecodeSetPropertyArgs;

    let fx = Fixture::new("edit-studio-prep");
    let mut instances = vec![
        settings_instance("ws", "Workspace", "Workspace", None),
        settings_instance("borders", "Borders", "Folder", Some(0)),
    ];
    for id in ["first", "second", "third"] {
        instances.push(settings_instance(id, "Border", "Folder", Some(1)));
    }
    instances.push(settings_instance("spare", "Spare", "Folder", Some(0)));
    fx.write_store("Workspace", &settings_document(instances));
    let store = fx.store_arg("Workspace");
    let prep = |edit: &[&str]| {
        let args = [&["bs", "-f", &store], edit].concat();
        set_property_outcome(BytecodeSetPropertyArgs::try_parse_from(args).unwrap())
            .unwrap()
            .1
    };
    let paths = |prep: &crate::bytecode::add_batch::StudioPrep| {
        prep.paths
            .iter()
            .map(|path| (path.path_segments.join("."), path.path_ordinals.clone()))
            .collect::<Vec<_>>()
    };
    let place = mock_place();

    let moved = prep(&["-i", "second", "-p", "Parent", "--str", "spare"]);
    assert_eq!(
        paths(&moved),
        [
            ("Workspace.Borders.Border".to_string(), vec![1, 1, 2]),
            ("Workspace.Spare".to_string(), vec![1, 1]),
        ]
    );
    assert_eq!(moved.moves, [(0, 1)]);
    assert!(moved.renames.is_empty() && moved.destroys.is_empty());
    let outcome = run_in(&place, &moved.luau());
    assert_eq!(outcome["moved"].as_f64(), Some(1.0), "{outcome}");

    let renamed = prep(&["-i", "third", "-p", "Name", "--str", "Last"]);
    assert_eq!(
        paths(&renamed),
        [("Workspace.Borders.Border".to_string(), vec![1, 1, 2])]
    );
    assert_eq!(renamed.renames, [(0, "Last".to_string())]);
    assert!(renamed.moves.is_empty() && renamed.destroys.is_empty());
    let outcome = run_in(&place, &renamed.luau());
    assert_eq!(outcome["renamed"].as_f64(), Some(1.0), "{outcome}");
    assert_eq!(
        children_in(&place, "Workspace.Borders"),
        "Border:first,Last:third"
    );
    assert_eq!(children_in(&place, "Workspace.Spare"), "Border:second");

    assert!(prep(&["-i", "first", "-p", "Archivable", "-j", "false"]).is_empty());
}
