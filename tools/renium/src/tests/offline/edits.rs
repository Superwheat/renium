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

fn batch_receipt(item: usize, id: &str, index: usize, service: &str, path: &str) -> Value {
    json!({"id": id, "index": index, "item": item, "path": path, "service": service})
}

/// `ba --batch` resolves services from the working directory, which a test
/// cannot change; the batch runs against the fixture's stores directly.
fn run_batch(fx: &Fixture, input: &str) -> Result<Value, String> {
    use crate::bytecode::add_batch::{
        add_store_instances_batch, parse_batch_add_items, read_batch_input,
    };
    let run = || -> anyhow::Result<Value> {
        let items = parse_batch_add_items(&read_batch_input(Path::new(input))?)?;
        add_store_instances_batch(items, &|service| {
            Ok((fx.store(service), service.to_string()))
        })
    };
    match run() {
        Ok(value) => Ok(super::normalize(value, &fx.root)),
        Err(error) => Err(normalize_text(&format!("{error:#}"), &fx.root)),
    }
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
        result["created"],
        json!([
            {"id": "editor:1", "index": 1, "item": 0, "path": "ServerScriptService.Battle", "ref": "battle", "service": "ServerScriptService"},
            {"id": "editor:2", "index": 2, "item": 1, "path": "ServerScriptService.Battle.Abilities", "ref": "abilities", "service": "ServerScriptService"},
            {"id": "editor:3", "index": 3, "item": 2, "path": "ServerScriptService.Battle.Abilities.CarMods", "service": "ServerScriptService", "sourcePath": "<root>/src/ServerScriptService/Battle/Abilities/CarMods.luau"},
            batch_receipt(3, "editor:4", 4, "ReplicatedStorage", "ReplicatedStorage.Shared.Mode"),
            {"id": "editor:4", "index": 4, "item": 4, "path": "ServerScriptService.Battle.Main", "service": "ServerScriptService", "sourcePath": "<root>/src/ServerScriptService/Battle/Main.server.luau"},
            batch_receipt(5, "custom:store", 9, "Workspace", "Workspace.Store"),
            batch_receipt(6, "editor:a", 10, "Workspace", "Workspace.Lobby.Lamp"),
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
            "created": [
                {"adoptedSource": true, "id": "editor:9", "index": 9, "item": 0, "path": "Workspace.Adopted", "service": "Workspace", "sourcePath": "<root>/src/Workspace/Adopted.luau"},
                {"id": "editor:1", "index": 1, "item": 1, "path": "StarterGui.Menu", "service": "StarterGui"},
            ],
            "ok": true,
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
        duplicate["created"][0]["path"],
        json!("Workspace.Lobby.Door[2]")
    );
    let bom = fx.root.join("bom.json");
    fs::write(
        &bom,
        "\u{feff}{\"ops\":[{\"parent\":\"Workspace\",\"name\":\"FromFile\",\"class\":\"Folder\"}]}",
    )
    .unwrap();
    assert_eq!(
        run_batch(&fx, &bom.to_string_lossy()).unwrap()["created"][0]["path"],
        json!("Workspace.FromFile")
    );
}

#[test]
fn add_batch_rejects_bad_items_without_writing_any_store() {
    let fx = Fixture::new("add-batch-errors");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let err = |ops: &str| run_batch(&fx, ops).unwrap_err();
    assert!(err(r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"a"},{"parent":"#b","name":"B","class":"Folder"}]"##).ends_with("Batch item 1: parent #b does not name an earlier item"));
    assert!(err(r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"a"},{"parent":"Workspace","name":"B","class":"Folder","ref":"a"}]"##).ends_with("Batch item 1 (#a): ref #a is already used by an earlier item"));
    assert!(
        err(r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"#a"}]"##)
            .ends_with("ref must be a non-empty name without a leading #")
    );
    assert!(err(r##"[{"parent":"Workspace","name":"A","class":"Folder","ref":"a"},{"parent":"#a","service":"ReplicatedStorage","name":"B","class":"Folder"}]"##).ends_with("Batch item 1: parent #a lives in Workspace, not ReplicatedStorage"));
    assert!(
        err(r#"[{"parent":"editor:1","name":"A","class":"Folder"}]"#).ends_with(
            "parent editor:1 is a settings id; add \"service\" to say which store it is in"
        )
    );
    assert!(
        err(r#"[{"parent":"Workspace.Nope","name":"A","class":"Folder"}]"#)
            .ends_with("Batch item 0: No matching instance path: Workspace.Nope")
    );
    assert!(
        err(r#"[{"parent":"Workspace.Lobby.Border","name":"A","class":"Folder"}]"#)
            .contains("Ambiguous instance path: Workspace.Lobby.Border")
    );
    assert!(
        err(r#"[{"parent":"Workspace","name":"A","class":"Folder","source":"x"}]"#)
            .ends_with("source is only accepted for Script, LocalScript and ModuleScript")
    );
    assert!(err(r#"[{"parent":"Workspace","name":"A","class":"ModuleScript","source":"x","props":{"Source":"y"}}]"#).ends_with("give source or props.Source, not both"));
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
    assert_eq!(err(r#"{"ops":[]}"#), "Batch input has no items");
    assert_eq!(err("[]"), "Batch input has no items");
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
        refused.contains("Batch item 1")
            && refused.contains("already exists with different contents"),
        "{refused}"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    assert!(!fx.exists("src/Workspace/Orphan.luau"));
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
        json!({"changedPaths": [STORE], "ok": true, "removedSettingsIds": ["door"], "settingsFile": STORE, "storeRemoved": false})
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
