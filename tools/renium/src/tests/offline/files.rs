use super::*;

const STORE: &str = "<root>/instances/Workspace.renium";

fn exported(format: &str, output: &str, root_id: &str, instances: usize) -> Value {
    json!({
        "format": format,
        "instances": instances,
        "ok": true,
        "output": output,
        "rootSettingsIds": [root_id],
        "service": "Workspace",
        "settingsFile": STORE,
    })
}

fn assert_lobby_model_view(fx: &Fixture, file: &Path) {
    let view = run_at(
        &fx.root,
        CaptureMode::Json,
        &["v", &file.to_string_lossy(), "--json"],
    )
    .ok();
    assert_eq!(view["instanceCount"], json!(5));
    let lobby = &view["roots"][0];
    assert_eq!(lobby["name"], json!("Lobby"));
    assert_eq!(lobby["className"], json!("Folder"));
    assert_eq!(lobby["childCount"], json!(4));
    let door = &lobby["children"][0];
    assert_eq!(
        door["properties"]["Material"],
        json!({"_type": "EnumItem", "enumType": "Enum.Material", "name": "Wood", "value": 512})
    );
    assert_eq!(door["properties"]["Anchored"], json!(true));
    assert_eq!(
        door["properties"]["Size"],
        json!({"_type": "Vector3", "x": 4.0, "y": 8.0, "z": 1.0})
    );
    assert_eq!(door["properties"]["Tags"], json!(["Interactive"]));
    assert_eq!(door["attributes"]["Owner"], json!("Ada"));
    assert_eq!(
        door["attributes"]["Spawn"],
        json!({"_type": "Vector3", "x": 1.0, "y": 2.0, "z": 3.0})
    );
    assert_eq!(
        lobby["children"][1]["properties"]["Transparency"],
        json!(0.25)
    );
    assert_eq!(
        lobby["children"][2]["properties"]["Transparency"],
        json!(0.75)
    );
    assert_eq!(
        lobby["children"][3],
        json!({"childCount": 0, "className": "LocalScript", "name": "Helper", "settingsId": "editor:4", "source": HELPER_SOURCE})
    );
}

#[test]
fn export_model_writes_both_formats_and_views_read_them_back() {
    let fx = Fixture::new("export-model");
    let store = fx.store_arg("Workspace");
    let out = fx.root.join("lobby.rbxm");
    let out_text = out.to_string_lossy().into_owned();
    assert_eq!(
        fx.both(&["bem", "-f", &store, "Lobby", "-o", &out_text]),
        exported("rbxm", "<root>/lobby.rbxm", "lobby", 5)
    );
    let xml = fx.root.join("lobby.rbxmx");
    assert_eq!(
        fx.both(&[
            "bem",
            "-f",
            &store,
            "-i",
            "lobby",
            "-o",
            &xml.to_string_lossy()
        ]),
        exported("rbxmx", "<root>/lobby.rbxmx", "lobby", 5)
    );
    let txt = fx.root.join("lobby.txt");
    assert_eq!(
        fx.text(&["bem", "-f", &store, "Lobby", "-o", &txt.to_string_lossy()])
            .err(),
        "Invalid model format \"txt\". Use rbxm or rbxmx."
    );
    assert_eq!(
        fx.text(&[
            "bem",
            "-f",
            &store,
            "Lobby",
            "-o",
            &txt.to_string_lossy(),
            "--format",
            "xml"
        ])
        .ok()["format"],
        json!("rbxmx")
    );
    assert_eq!(
        fx.text(&["bem", "-f", &store, "-x", "0", "-o", &out_text])
            .ok(),
        exported("rbxm", "<root>/lobby.rbxm", "ws", 9)
    );
    assert_eq!(
        fx.text(&["bem", "-f", &store, "Lobby", "-o", &out_text])
            .ok()["instances"],
        json!(5)
    );
    for file in [&out, &xml] {
        assert_lobby_model_view(&fx, file);
    }
    let err = |args: &[&str]| {
        fx.text(&[&["bem", "-f", &store], args, &["-o", &out_text]].concat())
            .err()
    };
    assert!(err(&["Nope"]).contains("No matching instance path: Workspace.Nope"));
    assert!(err(&["Border"]).contains("2 instances are named \"Border\""));
    assert_eq!(
        err(&["Target"]),
        "Workspace.Target (ObjectValue) property Value refers to Workspace.Lobby.Door, which is not among the exported instances"
    );
    assert!(err(&["-i", "nope"]).contains("No instance matches the export selector in this store"));
    assert_eq!(
        err(&["Lobby", "-i", "lobby"]),
        "Give the target once: as the positional argument, --path or one selector option"
    );
    let missing = fx.store_arg("StarterGui");
    assert!(
        fx.text(&["bem", "-f", &missing, "-n", "X", "-o", &out_text])
            .err()
            .contains("StarterGui.renium")
    );
}

#[test]
fn import_model_adds_the_tree_with_unique_names_sources_and_references() {
    let fx = Fixture::new("import-model");
    let store = fx.store_arg("Workspace");
    let replicated = fx.store_arg("ReplicatedStorage");
    let lobby = fx.root.join("lobby.rbxm");
    let lobby_text = lobby.to_string_lossy().into_owned();
    fx.text(&["bem", "-f", &store, "Lobby", "-o", &lobby_text])
        .ok();
    assert_eq!(
        fx.text(&[
            "bim",
            "-f",
            &replicated,
            "--model",
            &lobby_text,
            "-I",
            "shared"
        ])
        .ok(),
        json!({
            "changedPaths": ["<root>/instances/ReplicatedStorage.renium", "<root>/src/ReplicatedStorage/Shared/Lobby/Helper.client.luau"],
            "format": "rbxm",
            "model": "<root>/lobby.rbxm",
            "ok": true,
            "rootSettingsIds": ["editor:4"],
            "service": "ReplicatedStorage",
            "settingsFile": "<root>/instances/ReplicatedStorage.renium",
            "settingsIds": ["editor:4", "editor:5", "editor:6", "editor:7", "editor:8"],
            "sourceWrites": [{"path": "<root>/src/ReplicatedStorage/Shared/Lobby/Helper.client.luau", "settingsId": "editor:8"}],
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
    assert_eq!(
        fx.read_store("ReplicatedStorage").instances[5].properties["Material"],
        json!({"_type": "EnumItem", "name": "Wood"})
    );
    assert_eq!(
        fx.read_store("ReplicatedStorage").instances[5].attributes["Owner"],
        json!("Ada")
    );
    let again = fx
        .json(&[
            "bim",
            "-f",
            &replicated,
            "--model",
            &lobby_text,
            "-I",
            "Shared",
        ])
        .ok();
    assert_eq!(
        again["sourceWrites"][0]["path"],
        json!("<root>/src/ReplicatedStorage/Shared/Lobby Copy/Helper.client.luau")
    );
    assert_eq!(
        fx.read_store("ReplicatedStorage").instances[9].name,
        "Lobby Copy"
    );
    let kit = fx.root.join("kit.rbxm");
    write_model_file(&kit, &sample_model_dom());
    let kit_text = kit.to_string_lossy().into_owned();
    let at_root = fx
        .text(&["bim", "-f", &store, "--model", &kit_text, "--no-parent"])
        .ok();
    assert_eq!(at_root["rootSettingsIds"], json!(["editor:9"]));
    assert_eq!(
        at_root["sourceWrites"],
        json!([{"path": "<root>/src/Workspace/Kit/Setup.luau", "settingsId": "editor:b"}])
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Kit.Primary", "Value"]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Kit", "Base"], "settingsId": "editor:a"})
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Kit.Base", "Transparency"]),
        json!(0.5)
    );
    assert_eq!(
        fx.text(&["bim", "-f", &store, "--model", &kit_text, "-N", "Café ☕"])
            .ok()["sourceWrites"][0]["path"],
        json!("<root>/src/Workspace/Café ☕/Kit/Setup.luau")
    );
    assert_eq!(
        fx.text(&["bim", "-f", &store, "--model", &kit_text, "-x", "7"])
            .ok()["sourceWrites"][0]["path"],
        json!("<root>/src/Workspace/Café ☕/Kit Copy/Setup.luau")
    );
    assert_eq!(
        fx.text(&["bim", "-f", &store, "--model", &kit_text, "-N", "Target"])
            .ok()["sourceWrites"][0]["path"],
        json!("<root>/src/Workspace/Target/Kit/Setup.luau")
    );
    assert_eq!(
        fx.text(&["bim", "-f", &store, "--model", &kit_text, "-C", "Script"])
            .ok()["sourceWrites"][0]["path"],
        json!("<root>/src/Workspace/Spawner/Kit/Setup.luau")
    );
    let missing = fx.store_arg("StarterGui");
    let seeded = fx.text(&["bim", "-f", &missing, "--model", &kit_text]).ok();
    assert_eq!(seeded["rootSettingsIds"], json!(["editor:1"]));
    assert_eq!(
        fx.names("StarterGui"),
        ["StarterGui", "Kit", "Base", "Setup", "Primary"]
    );
    let err = |args: &[&str]| {
        fx.text(&[&["bim", "-f", &store, "--model"], args].concat())
            .err()
    };
    assert!(err(&[&kit_text, "-I", "nope"]).contains("No matching instance path: Workspace.nope"));
    assert!(err(&[&kit_text, "-I", "Lobby.Border"]).contains("Ambiguous instance path"));
    assert!(err(&[&fx.path("nope.rbxm")]).starts_with("Failed to read"));
    let txt = fx.root.join("kit.txt");
    fs::copy(&kit, &txt).unwrap();
    assert_eq!(
        err(&[&txt.to_string_lossy()]),
        "Invalid model format \"txt\". Use rbxm or rbxmx."
    );
    let linked = fx.root.join("linked.rbxm");
    let mut dom = sample_model_dom();
    let model = dom.root().children()[0];
    dom.insert(model, rbx_dom_weak::InstanceBuilder::new("PackageLink"));
    write_model_file(&linked, &dom);
    let before = fs::read(fx.store("Workspace")).unwrap();
    assert_eq!(
        err(&[&linked.to_string_lossy()]),
        "Models containing PackageLink instances cannot be imported; remove the link from the model first"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
}

fn built(format: &str, output: &str) -> Value {
    json!({
        "base": null,
        "baseSource": null,
        "format": format,
        "instances": 14,
        "ok": true,
        "output": output,
        "serviceCount": 3,
        "services": ["Workspace", "ReplicatedStorage", "ServerScriptService"],
        "warning": null,
    })
}

fn place_query(fx: &Fixture, file: &str, args: &[&str], mode: CaptureMode) -> Outcome {
    run_at(&fx.root, mode, &[&["q", file], args].concat())
}

fn assert_place_query_values(fx: &Fixture, file: &str) {
    let query = |args: &[&str], mode: CaptureMode| place_query(fx, file, args, mode);
    let door = query(
        &[
            "-n",
            "Door",
            "--props",
            "Anchored,Material,Size,Color,CFrame,Transparency,Parent",
        ],
        CaptureMode::Text,
    )
    .ok();
    assert_eq!(
        door,
        json!({
            "matches": [{
                "className": "Part",
                "path": "Workspace.Lobby.Door",
                "props": {
                    "Anchored": true,
                    "CFrame": {"_type": "CFrame", "components": DOOR_CFRAME},
                    "Color": {"_type": "Color3", "b": 0.0, "g": 0.5019608, "r": 1.0},
                    "Material": "Enum.Material.Wood",
                    "Parent": "Workspace.Lobby",
                    "Size": {"_type": "Vector3", "x": 4.0, "y": 8.0, "z": 1.0},
                    "Transparency": 0.5,
                },
            }],
            "ok": true,
        })
    );
    let door_json = query(
        &["-n", "Door", "--props", "Anchored,Color"],
        CaptureMode::Json,
    )
    .ok();
    assert_eq!(
        door_json,
        json!({
            "input": format!("<root>/{}", Path::new(file).file_name().unwrap().to_str().unwrap()),
            "matches": [{"className": "Part", "name": "Door", "path": ["Workspace", "Lobby", "Door"], "props": {"Anchored": true, "Color": {"_type": "Color3", "b": 0.0, "g": 0.501960813999176, "r": 1.0}}}],
            "ok": true,
            "searched": 14,
        })
    );
    assert_eq!(
        query(&["-n", "Border"], CaptureMode::Text).ok()["matches"],
        json!([{"className": "Part", "path": "Workspace.Lobby.Border[2]"}, {"className": "Part", "path": "Workspace.Lobby.Border"}])
    );
    assert_eq!(
        query(&["-n", "Border"], CaptureMode::Json).ok()["matches"],
        json!([
            {"className": "Part", "name": "Border", "ordinals": [1, 1, 2], "path": ["Workspace", "Lobby", "Border"]},
            {"className": "Part", "name": "Border", "path": ["Workspace", "Lobby", "Border"]},
        ])
    );
    let limited = query(&["-n", "Border", "--limit", "1"], CaptureMode::Text).ok();
    assert_eq!(limited["matches"].as_array().unwrap().len(), 1);
    assert_eq!(limited["truncated"], json!(true));
    assert_eq!(
        query(&["-n", "Border", "--limit", "1", "-a"], CaptureMode::Text).ok()["matches"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        query(
            &["-n", "Workspace", "--props", "Gravity,Name,ClassName"],
            CaptureMode::Text
        )
        .ok()["matches"][0]["props"],
        json!({"ClassName": "Workspace", "Gravity": 120.0, "Name": "Workspace"})
    );
    assert_eq!(
        query(&["-n", "Target", "--props", "Value"], CaptureMode::Text).ok()["matches"][0]["props"],
        json!({"Value": "Workspace.Lobby.Door"})
    );
    let derived = query(
        &["-n", "Door", "--props", "Position,Bogus,CanCollide"],
        CaptureMode::Text,
    )
    .ok();
    assert_eq!(derived["matches"][0]["props"], json!({"CanCollide": true}));
    assert_eq!(derived["matches"][0]["notSaved"], json!(["Position"]));
    assert_eq!(derived["matches"][0]["unknown"], json!(["Bogus"]));
    assert_eq!(derived["matches"][0]["defaulted"], json!(["CanCollide"]));
}

fn assert_place_query_filters(fx: &Fixture, file: &str) {
    let query = |args: &[&str], mode: CaptureMode| place_query(fx, file, args, mode);
    assert_eq!(
        query(&["-c", "Script", "--props", "Source"], CaptureMode::Text).ok()["matches"],
        json!([{"className": "Script", "path": "Workspace.Spawner", "props": {"Source": SPAWNER_SOURCE}}])
    );
    assert_eq!(
        query(&["--source", "spawn"], CaptureMode::Text).ok()["matches"],
        json!([{"className": "Script", "path": "Workspace.Spawner"}])
    );
    assert_eq!(
        query(
            &["-c", "LocalScript", "--props", "Source"],
            CaptureMode::Text
        )
        .ok()["matches"][0]["props"]["Source"],
        json!(HELPER_SOURCE)
    );
    assert_eq!(
        query(&["--source", "nope"], CaptureMode::Text).ok(),
        json!({"matches": [], "ok": true})
    );
    assert_eq!(
        query(&["Nope"], CaptureMode::Text).ok(),
        json!({"hint": "A query matches instance names and classes only; use -s TEXT to search script source", "matches": [], "ok": true})
    );
    assert_eq!(
        query(&["folder"], CaptureMode::Text).ok()["matches"],
        json!([{"className": "Folder", "path": "ReplicatedStorage.Shared"}, {"className": "Folder", "path": "Workspace.Café ☕"}, {"className": "Folder", "path": "Workspace.Lobby"}])
    );
    assert_eq!(
        query(&["-n", "Café ☕", "--props", "all"], CaptureMode::Text).ok()["matches"],
        json!([{"className": "Folder", "path": "Workspace.Café ☕", "props": {}}])
    );
    assert_eq!(
        query(&[], CaptureMode::Text).err(),
        "Provide a query, --name, --class, or --source"
    );
    assert!(
        query(&["-n", "Door", "--props", ","], CaptureMode::Text)
            .err()
            .contains("--props needs property names or all")
    );
}

#[test]
fn export_place_builds_the_project_and_queries_read_the_saved_values() {
    let fx = Fixture::new("export-place");
    let root = fx.path("");
    let place = fx.root.join("build.rbxl");
    let place_text = place.to_string_lossy().into_owned();
    assert_eq!(
        fx.both(&["bep", "-r", &root, "-o", &place_text]),
        built("rbxl", "<root>/build.rbxl")
    );
    let xml = fx.root.join("build.rbxlx");
    let xml_text = xml.to_string_lossy().into_owned();
    assert_eq!(
        fx.both(&["bep", "-r", &root, "-o", &xml_text]),
        built("rbxlx", "<root>/build.rbxlx")
    );
    let txt = fx.root.join("build.txt");
    assert_eq!(
        fx.text(&["bep", "-r", &root, "-o", &txt.to_string_lossy()])
            .err(),
        "Invalid place format \"txt\". Use rbxl or rbxlx."
    );
    assert_eq!(
        fx.text(&[
            "bep",
            "-r",
            &root,
            "-o",
            &txt.to_string_lossy(),
            "--format",
            "rbxlx"
        ])
        .ok()["format"],
        json!("rbxlx")
    );
    let partial = fx
        .text(&[
            "bep",
            "-r",
            &root,
            "-o",
            &place_text,
            "-s",
            "Workspace,ReplicatedStorage",
        ])
        .ok();
    assert_eq!(
        partial["services"],
        json!(["Workspace", "ReplicatedStorage"])
    );
    assert_eq!(partial["serviceCount"], json!(2));
    assert_eq!(partial["instances"], json!(13));
    assert_eq!(
        fx.text(&["bep", "-r", &root, "-o", &place_text, "-s", "Bogus"])
            .err(),
        "Unsupported service: Bogus"
    );
    assert!(
        fx.text(&[
            "bep",
            "-r",
            &root,
            "-o",
            &place_text,
            "--base",
            &fx.path("nope.rbxl")
        ])
        .err()
        .starts_with("Failed to read")
    );
    assert!(
        fx.text(&["bep", "-r", &fx.path("nowhere"), "-o", &place_text])
            .err()
            .starts_with("Failed to resolve project root")
    );
    let saved = fx.root.join("saved.rbxl");
    write_place_file(&saved, &sample_place_dom());
    let with_base = fx
        .text(&[
            "bep",
            "-r",
            &root,
            "-o",
            &place_text,
            "--base",
            &saved.to_string_lossy(),
        ])
        .ok();
    assert_eq!(with_base["base"], json!("<root>/saved.rbxl"));
    assert_eq!(with_base["baseSource"], json!("argument"));
    assert_eq!(with_base["instances"], json!(14));
    assert_eq!(
        fx.both(&["bep", "-r", &root, "-o", &place_text]),
        built("rbxl", "<root>/build.rbxl")
    );

    for file in [&place_text, &xml_text] {
        assert_place_query_values(&fx, file);
        assert_place_query_filters(&fx, file);
    }
    assert!(
        run_at(
            &fx.root,
            CaptureMode::Text,
            &["q", &fx.path("nope.rbxl"), "Door"]
        )
        .err()
        .starts_with("Place file does not exist")
    );
    assert_eq!(
        run_at(
            &fx.root,
            CaptureMode::Text,
            &["q", &txt.to_string_lossy(), "Door"]
        )
        .err(),
        "Invalid place format \"txt\". Use rbxl or rbxlx."
    );
    assert!(
        run_at(
            &fx.root,
            CaptureMode::Text,
            &["cmp", &txt.to_string_lossy(), &place_text, "--full"]
        )
        .err()
        .contains("Invalid place format \"txt\"")
    );
    let bogus = fx.root.join("bogus.rbxl");
    fs::write(&bogus, b"not a place").unwrap();
    assert!(
        run_at(
            &fx.root,
            CaptureMode::Text,
            &["q", &bogus.to_string_lossy(), "Door"]
        )
        .err()
        .contains("bogus.rbxl")
    );
}

#[test]
fn compare_place_reports_script_and_full_differences_against_the_project() {
    let fx = Fixture::new("compare");
    let root = fx.path("");
    let project = fx.path("renium.project.jsonc");
    let place = fx.root.join("build.rbxl");
    let place_text = place.to_string_lossy().into_owned();
    fx.text(&["bep", "-r", &root, "-o", &place_text]).ok();
    let scripts_equal = json!({
        "changed": 0,
        "differences": [],
        "extraInPlace": 0,
        "input": "<root>/build.rbxl",
        "matches": true,
        "matchingScripts": 3,
        "missingFromPlace": 0,
        "ok": true,
        "placeScripts": 3,
        "project": "<root>/renium.project.jsonc",
        "projectScripts": 3,
        "scope": "scripts",
        "services": ["ReplicatedStorage", "ServerScriptService", "Workspace"],
        "targetKind": "project",
    });
    assert_eq!(
        fx.both(&["cmp", &place_text, "--project", &project]),
        scripts_equal
    );
    assert_eq!(
        fx.text(&["cmp", &place_text, "--full", "--project", &project])
            .ok(),
        json!({"added": 0, "changed": 0, "differenceCount": 0, "differences": [], "input": "<root>/build.rbxl", "matches": true, "ok": true, "removed": 0, "scope": "full", "target": "<root>/renium.project.jsonc", "targetKind": "project", "unchanged": 14})
    );
    assert_eq!(
        fx.json(&["cmp", &place_text, "--full", "--project", &project]).ok(),
        json!({"added": 0, "afterInstances": 14, "beforeInstances": 14, "changed": 0, "differenceCount": 0, "differences": [], "direction": "input -> target", "input": "<root>/build.rbxl", "matches": false, "ok": true, "removed": 0, "scope": "full", "services": ["ReplicatedStorage", "ServerScriptService", "Workspace"], "target": "<root>/renium.project.jsonc", "targetKind": "project", "unchanged": 14}).as_object().map(|map| {
            let mut map = map.clone();
            map["matches"] = json!(true);
            Value::Object(map)
        }).unwrap()
    );
    let store = fx.store_arg("Workspace");
    fx.text(&["bss", "-f", &store, "Spawner", "--str", "print('changed')"])
        .ok();
    fx.text(&[
        "bs", "-f", &store, "Door", "-p", "Anchored", "--bool", "false",
    ])
    .ok();
    fx.text(&["ba", "-f", &store, "-n", "Added", "-c", "Folder"])
        .ok();
    let mut changed_scripts = scripts_equal;
    changed_scripts["changed"] = json!(1);
    changed_scripts["differences"] =
        json!([{"class": "Script", "kind": "sourceChanged", "path": ["Workspace", "Spawner"]}]);
    changed_scripts["matches"] = json!(false);
    changed_scripts["matchingScripts"] = json!(2);
    assert_eq!(
        fx.both(&["cmp", &place_text, "--project", &project]),
        changed_scripts
    );
    let full_text = fx
        .text(&["cmp", &place_text, "--full", "--project", &project])
        .ok();
    assert_eq!(
        full_text,
        json!({"added": 1, "changed": 2, "differenceCount": 3, "differences": ["changed Part Workspace.Lobby.Door: Anchored", "changed Script Workspace.Spawner: Source", "added Folder Workspace.Added"], "input": "<root>/build.rbxl", "matches": false, "ok": true, "removed": 0, "scope": "full", "target": "<root>/renium.project.jsonc", "targetKind": "project", "unchanged": 12})
    );
    let full_json = fx
        .json(&["cmp", &place_text, "--full", "--project", &project])
        .ok();
    let mut differences = full_json["differences"].clone();
    for difference in differences.as_array_mut().unwrap() {
        for side in ["before", "after"] {
            if let Some(record) = difference.get_mut(side).and_then(Value::as_object_mut) {
                assert!(record.remove("index").is_some_and(|index| index.is_u64()));
            }
        }
    }
    assert_eq!(
        differences,
        json!([
            {"after": {"className": "Part", "path": ["Workspace", "Lobby", "Door"]}, "attributes": [], "before": {"className": "Part", "path": ["Workspace", "Lobby", "Door"]}, "kind": "changed", "properties": [{"name": "Anchored"}]},
            {"after": {"className": "Script", "path": ["Workspace", "Spawner"]}, "attributes": [], "before": {"className": "Script", "path": ["Workspace", "Spawner"]}, "kind": "changed", "properties": [{"name": "Source"}]},
            {"after": {"className": "Folder", "path": ["Workspace", "Added"]}, "kind": "added"},
        ])
    );
    assert_eq!(full_json["afterInstances"], json!(15));
    let values = fx
        .text(&[
            "cmp",
            &place_text,
            "--full",
            "--values",
            "--all",
            "--project",
            &project,
        ])
        .ok();
    assert_eq!(
        values["differences"][0]["properties"],
        json!([{"after": null, "afterPresent": false, "before": true, "beforePresent": true, "name": "Anchored"}])
    );
    assert_eq!(
        values["differences"][1]["properties"],
        json!([{"after": "print('changed')", "afterPresent": true, "before": SPAWNER_SOURCE, "beforePresent": true, "name": "Source"}])
    );
    let scoped = fx
        .text(&[
            "cmp",
            &place_text,
            "--full",
            "--class",
            "Part",
            "--props",
            "Anchored",
            "--project",
            &project,
        ])
        .ok();
    assert_eq!(
        scoped["differences"],
        json!(["changed Part Workspace.Lobby.Door: Anchored"])
    );
    assert_eq!(scoped["unchanged"], json!(2));
    let limited = fx
        .text(&[
            "cmp",
            &place_text,
            "--full",
            "--limit",
            "1",
            "--project",
            &project,
        ])
        .ok();
    assert_eq!(
        limited["differences"],
        json!(["changed Part Workspace.Lobby.Door: Anchored"])
    );
    assert_eq!(limited["truncated"], json!(true));
    assert_eq!(limited["differenceCount"], json!(3));
    assert!(
        fx.text(&["cmp", &place_text, "--values", "--project", &project])
            .err()
            .contains("--full")
    );
    assert!(
        fx.text(&["cmp", &fx.path("nope.rbxl"), "--project", &project])
            .err()
            .starts_with("Place file does not exist")
    );
    assert!(
        fx.text(&["cmp", &place_text, "--project", &fx.path("nope.jsonc")])
            .err()
            .starts_with("Project file does not exist")
    );
}

/// A place file holds SoundId as AudioContent, BrickColor as Color and so
/// on; the store keeps the name the edit used.
#[test]
fn compare_place_matches_a_project_that_sets_properties_a_place_file_renames() {
    let fx = Fixture::new("compare-renamed");
    let root = fx.path("");
    let project = fx.path("renium.project.jsonc");
    let store = fx.store_arg("Workspace");
    for (name, class, property) in [
        ("Snd", "Sound", r#"SoundId="rbxassetid://5""#),
        ("Wedge", "WedgePart", r#"BrickColor="Bright red""#),
        ("Label", "ImageLabel", r#"Image="rbxassetid://6""#),
        ("Sticker", "Decal", r#"Texture="rbxassetid://7""#),
        ("Gui", "ScreenGui", "IgnoreGuiInset=true"),
    ] {
        fx.text(&["ba", "-f", &store, "-n", name, "-c", class, "-p", property])
            .ok();
    }
    for file in ["build.rbxl", "build.rbxlx"] {
        let place = fx.path(file);
        fx.text(&["bep", "-r", &root, "-o", &place]).ok();
        let compared = fx
            .text(&["cmp", &place, "--full", "--all", "--project", &project])
            .ok();
        assert_eq!(compared["differences"], json!([]), "{file}");
        assert_eq!(compared["unchanged"], json!(19), "{file}");
    }
    fx.text(&[
        "bs",
        "-f",
        &store,
        "Snd",
        "-p",
        "SoundId",
        "--str",
        "rbxassetid://9",
    ])
    .ok();
    assert_eq!(
        fx.text(&[
            "cmp",
            &fx.path("build.rbxl"),
            "--full",
            "--project",
            &project
        ])
        .ok()["differences"],
        json!(["changed Sound Workspace.Snd: AudioContent"])
    );
}

#[test]
fn compare_place_compares_two_files_in_either_format() {
    let fx = Fixture::new("compare-files");
    let root = fx.path("");
    let place = fx.root.join("build.rbxl");
    let place_text = place.to_string_lossy().into_owned();
    fx.text(&["bep", "-r", &root, "-o", &place_text]).ok();
    let saved = fx.root.join("saved.rbxl");
    write_place_file(&saved, &sample_place_dom());
    let saved_xml = fx.root.join("saved.rbxlx");
    write_place_file(&saved_xml, &sample_place_dom());
    let scripts = fx.both(&[
        "cmp",
        &place_text,
        "--against",
        &saved_xml.to_string_lossy(),
    ]);
    assert_eq!(
        scripts,
        json!({
            "changed": 0,
            "differences": [{"kind": "extraInPlace", "path": ["Workspace", "Lobby", "Helper"], "placeClass": "LocalScript"}],
            "extraInPlace": 1,
            "input": "<root>/build.rbxl",
            "matches": false,
            "matchingScripts": 2,
            "missingFromPlace": 0,
            "ok": true,
            "placeScripts": 3,
            "project": "<root>/saved.rbxlx",
            "projectScripts": 2,
            "scope": "scripts",
            "services": ["ReplicatedStorage", "ServerScriptService", "Workspace"],
            "targetKind": "place",
        })
    );
    let full = fx
        .text(&[
            "cmp",
            &place_text,
            &saved.to_string_lossy(),
            "--full",
            "--all",
        ])
        .ok();
    assert_eq!(full["targetKind"], json!("place"));
    assert_eq!(full["target"], json!("<root>/saved.rbxl"));
    assert_eq!(full["beforeInstances"], Value::Null);
    let lines = full["differences"].as_array().unwrap();
    for expected in [
        "changed Workspace Workspace: Gravity",
        "changed Part Workspace.Lobby.Door: CFrame, Color, Material, Size, Tags, @Owner, @Ratio, @Reviewed, @Spawn, @Weight",
        "added Part Workspace.Lobby.Pad",
        "removed Part Workspace.Lobby.Border",
        "removed LocalScript Workspace.Lobby.Helper",
        "removed Folder Workspace.Café ☕",
        "removed ObjectValue ReplicatedStorage.Link",
    ] {
        assert!(
            lines.contains(&json!(expected)),
            "{expected} missing from {lines:?}"
        );
    }
    assert_eq!(full["added"], json!(2));
    assert_eq!(full["removed"], json!(5));
    assert_eq!(full["changed"], json!(2));
    let reversed = fx
        .json(&["cmp", &saved_xml.to_string_lossy(), &place_text, "--full"])
        .ok();
    assert_eq!(reversed["added"], json!(5));
    assert_eq!(reversed["removed"], json!(2));
    assert_eq!(
        reversed["differences"][0]["before"]["className"],
        json!("Workspace")
    );
    assert_eq!(
        fx.json(&[
            "cmp",
            &saved.to_string_lossy(),
            &saved_xml.to_string_lossy(),
            "--full"
        ])
        .ok()["matches"],
        json!(true)
    );
    assert!(
        fx.text(&[
            "cmp",
            &place_text,
            &saved.to_string_lossy(),
            "--against",
            &saved.to_string_lossy()
        ])
        .err()
        .contains("cannot be used with")
    );
}

fn workspace_store_text() -> String {
    [
        "# renium store v11: 9 instances",
        "= Workspace [Workspace] id=ws",
        "  Gravity = 120.0",
        "= Workspace/Lobby [Folder] id=lobby",
        "= Workspace/Lobby/Door [Part] id=door",
        "  Anchored = true",
        "  CFrame = {\"_type\":\"CFrame\",\"components\":[0.0,4.0,0.0,1.0,0.0,0.0,0.0,1.0,0.0,0.0,0.0,1.0]}",
        "  Color = {\"_type\":\"Color3\",\"b\":0.0,\"g\":0.5,\"r\":1.0}",
        "  Material = {\"_type\":\"EnumItem\",\"name\":\"Wood\"}",
        "  Size = {\"_type\":\"Vector3\",\"x\":4.0,\"y\":8.0,\"z\":1.0}",
        "  Tags = [\"Interactive\"]",
        "  Transparency = 0.5",
        "  @Owner = \"Ada\"",
        "  @Ratio = 0.20000000298023224",
        "  @Reviewed = true",
        "  @Spawn = {\"_type\":\"Vector3\",\"x\":1.0,\"y\":2.0,\"z\":3.0}",
        "  @Weight = 2.5",
        "= Workspace/Lobby/Border [Part] id=border1",
        "  Transparency = 0.25",
        "= Workspace/Lobby/Border [Part] id=border2",
        "  Transparency = 0.75",
        "= Workspace/Lobby/Helper [LocalScript] id=helper",
        "  Source = <1 lines, 19 bytes, fnv1a=cddd0515d07d278e>",
        "= Workspace/Spawner [Script] id=spawner",
        "  Source = <1 lines, 19 bytes, fnv1a=cddd0515d07d278e>",
        "= Workspace/Target [ObjectValue] id=target",
        "  Value = {\"_type\":\"Ref\",\"pathOrdinals\":[1,1,1],\"pathSegments\":[\"Workspace\",\"Lobby\",\"Door\"],\"settingsId\":\"door\"}",
        "= Workspace/Café ☕ [Folder] id=cafe",
        "",
    ]
    .join("\n")
}

fn workspace_store_tree() -> Value {
    json!({
        "instanceCount": 9,
        "roots": [{
            "childCount": 4,
            "children": [
                {
                    "childCount": 4,
                    "children": [
                        {"attributes": door_attrs(true), "childCount": 0, "className": "Part", "name": "Door", "properties": door_props(), "settingsId": "door"},
                        {"childCount": 0, "className": "Part", "name": "Border", "properties": {"Transparency": 0.25}, "settingsId": "border1"},
                        {"childCount": 0, "className": "Part", "name": "Border", "properties": {"Transparency": 0.75}, "settingsId": "border2"},
                        {"childCount": 0, "className": "LocalScript", "name": "Helper", "settingsId": "helper", "source": EXTERNAL_SOURCE_MARKER},
                    ],
                    "className": "Folder",
                    "name": "Lobby",
                    "settingsId": "lobby",
                },
                {"childCount": 0, "className": "Script", "name": "Spawner", "settingsId": "spawner", "source": EXTERNAL_SOURCE_MARKER},
                {"childCount": 0, "className": "ObjectValue", "name": "Target", "properties": {"Value": {"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"}}, "settingsId": "target"},
                {"childCount": 0, "className": "Folder", "name": "Café ☕", "settingsId": "cafe"},
            ],
            "className": "Workspace",
            "name": "Workspace",
            "properties": {"Gravity": 120.0},
            "settingsId": "ws",
        }],
        "version": 11,
    })
}

#[test]
fn view_and_textconv_render_stores_models_places_and_empty_files() {
    let fx = Fixture::new("view");
    let store = fx.store_arg("Workspace");
    let text = fx.text(&["v", &store]);
    assert_eq!(text.error, None);
    assert_eq!(text.stdout, workspace_store_text());
    assert_eq!(fx.json(&["v", &store]).stdout, workspace_store_text());
    assert_eq!(
        fx.json(&["v", &store, "--json"]).ok(),
        workspace_store_tree()
    );
    let mut text_tree = workspace_store_tree();
    text_tree["roots"][0]["children"][0]["children"][0]["attributes"] = door_attrs(false);
    assert_eq!(fx.text(&["v", &store, "--json"]).ok(), text_tree);
    let textconv = fx.text(&["vct", &store]);
    assert_eq!(textconv.error, None);
    assert_eq!(textconv.stdout, workspace_store_text());
}

#[test]
fn view_and_textconv_render_places_models_empty_and_foreign_files() {
    let fx = Fixture::new("view-files");
    let saved = fx.root.join("saved.rbxl");
    write_place_file(&saved, &sample_place_dom());
    let saved_xml = fx.root.join("saved.rbxlx");
    write_place_file(&saved_xml, &sample_place_dom());
    let place_tree = fx.json(&["v", &saved.to_string_lossy(), "--json"]).ok();
    assert_eq!(place_tree["instanceCount"], json!(11));
    assert_eq!(
        place_tree["roots"][0]["properties"],
        json!({"Gravity": 150.0})
    );
    assert_eq!(
        place_tree["roots"][0]["children"][1]["source"],
        json!("print('spawn')\n")
    );
    assert_eq!(
        place_tree["roots"][0]["children"][2]["properties"]["Value"],
        json!({"_type": "Ref", "settingsId": "place:2"})
    );
    assert_eq!(
        place_tree["roots"][1]["children"][0]["children"][0]["source"],
        json!("return { rounds = 3 }\r\n")
    );
    assert_eq!(place_tree["roots"][2]["name"], json!("ServerScriptService"));
    let place_text = fx.text(&["v", &saved_xml.to_string_lossy()]).stdout;
    assert!(place_text.starts_with(
        "# renium store v11: 11 instances\n= Workspace [Workspace] id=place:0\n  Gravity = 150.0\n"
    ));
    assert!(place_text.contains(
        "= Workspace/Lobby/Pad [Part] id=place:3\n= Workspace/Lobby/Pad [Part] id=place:4\n"
    ));
    assert!(place_text.contains("  Source = <1 lines, 15 bytes, fnv1a=68100125ee5e5fb2>\n"));
    let model = fx.root.join("kit.rbxm");
    write_model_file(&model, &sample_model_dom());
    let model_tree = fx.json(&["v", &model.to_string_lossy(), "--json"]).ok();
    assert_eq!(model_tree["instanceCount"], json!(4));
    assert_eq!(
        model_tree["roots"][0]["children"][2]["properties"]["Value"],
        json!({"_type": "Ref", "pathOrdinals": [1, 1], "pathSegments": ["Kit", "Base"], "settingsId": "editor:1"})
    );
    assert!(
        fx.text(&["v", &model.to_string_lossy()])
            .stdout
            .starts_with("# renium store v11: 4 instances\n= Kit [Model] id=editor:0\n")
    );
    let empty = fx.root.join("empty.renium");
    fs::write(&empty, b"").unwrap();
    assert_eq!(
        fx.text(&["v", &empty.to_string_lossy()]).stdout,
        "# renium store: empty file\n"
    );
    assert_eq!(
        fx.both(&["v", &empty.to_string_lossy(), "--json"]),
        json!({"instanceCount": 0, "roots": [], "version": 0})
    );
    assert_eq!(
        fx.text(&["vct", &empty.to_string_lossy()]).stdout,
        "# renium store: empty file\n"
    );
    assert!(
        fx.text(&["v", &fx.path("nope.renium")])
            .err()
            .starts_with("File not found: <root>/nope.renium")
    );
    let txt = fx.root.join("kit.txt");
    fs::copy(&model, &txt).unwrap();
    assert_eq!(
        fx.text(&["v", &txt.to_string_lossy()]).err(),
        "Unsupported file type for <root>/kit.txt; expected .renium, .rbxm, .rbxmx, .rbxl, or .rbxlx"
    );
    assert!(
        fx.text(&["vct", &fx.path("nope.renium")])
            .err()
            .starts_with("Failed to stat <root>/nope.renium")
    );
    assert!(
        fx.text(&["vct", &model.to_string_lossy()])
            .err()
            .contains("Invalid settings bytecode magic")
    );
}

fn sorted_services() -> Vec<&'static str> {
    let mut services = crate::roblox::services::DEFAULT_SYNC_SERVICES.to_vec();
    services.sort_unstable();
    services
}

#[test]
fn import_place_creates_a_project_and_records_the_build_base() {
    let fx = Fixture::new("import-place");
    let saved = fx.root.join("saved.rbxl");
    write_place_file(&saved, &sample_place_dom());
    let saved_text = saved.to_string_lossy().into_owned();
    let fresh = fx.path("fresh");
    let mut changed = vec![
        "sourcemap.json".to_string(),
        ".renium/adapter-baseline.json".to_string(),
    ];
    changed.extend(
        sorted_services()
            .iter()
            .map(|service| format!("instances/{service}.renium")),
    );
    changed.extend(
        sorted_services()
            .iter()
            .map(|service| format!("src/{service}")),
    );
    assert_eq!(
        fx.text(&["pi", &saved_text, "-r", &fresh]).ok(),
        json!({"changedPaths": changed, "importBase": true, "instances": 8, "ok": true, "services": 18})
    );
    assert!(fx.exists("fresh/renium.project.jsonc"));
    assert!(fx.exists("fresh/.renium/import-base.json"));
    assert_eq!(
        fs::read_to_string(fx.root.join("fresh/src/Workspace/Spawner.server.luau")).unwrap(),
        "print('spawn')\n"
    );
    assert_eq!(
        fs::read_to_string(
            fx.root
                .join("fresh/src/ReplicatedStorage/Shared/Config.luau")
        )
        .unwrap(),
        "return { rounds = 3 }\r\n"
    );
    assert_eq!(
        fx.both(&["tr", "Workspace", "--depth", "3", "-r", &fresh]),
        json!([
            {"c": "Workspace", "cc": 3, "ch": ["2", "6", "7"], "id": "1", "n": "Workspace", "ords": [1]},
            {"c": "Folder", "cc": 3, "ch": ["3", "4", "5"], "id": "2", "n": "Lobby", "ords": [1, 1]},
            {"c": "Part", "cc": 0, "id": "3", "n": "Door", "ords": [1, 1, 1]},
            {"c": "Part", "cc": 0, "id": "4", "n": "Pad", "ords": [1, 1, 1]},
            {"c": "Part", "cc": 0, "id": "5", "n": "Pad", "ords": [1, 1, 2]},
            {"c": "Script", "cc": 0, "id": "6", "n": "Spawner", "ords": [1, 1]},
            {"c": "ObjectValue", "cc": 0, "id": "7", "n": "Target", "ords": [1, 1]},
        ])
    );
    assert_eq!(
        fx.both(&["in", "Workspace", "Target", "-F", "props", "-r", &fresh]),
        json!({"props": {"Value": {"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "3"}}})
    );
    let fresh_store = fx.path("fresh/instances/Workspace.renium");
    assert_eq!(
        fx.both(&["bg", "-f", &fresh_store, "-p", "Gravity"]),
        json!(150.0)
    );
    assert_eq!(
        fx.both(&["bg", "-f", &fresh_store, "Door", "Anchored"]),
        json!(true)
    );
    assert_eq!(
        fx.both(&["bg", "-f", &fresh_store, "Door", "Transparency"]),
        json!(0.5)
    );
}

#[test]
fn import_place_records_the_build_base_and_warns_when_it_changes() {
    let fx = Fixture::new("import-base");
    let saved = fx.root.join("saved.rbxl");
    write_place_file(&saved, &sample_place_dom());
    let saved_text = saved.to_string_lossy().into_owned();
    let fresh = fx.path("fresh");
    assert_eq!(
        fx.text(&["pi", &saved_text, "-r", &fresh]).ok()["ok"],
        json!(true)
    );
    let built = fx.root.join("fresh/built.rbxl");
    let built_text = built.to_string_lossy().into_owned();
    let result = fx.text(&["bep", "-r", &fresh, "-o", &built_text]).ok();
    assert_eq!(result["base"], json!("<root>/saved.rbxl"));
    assert_eq!(result["baseSource"], json!("import"));
    assert_eq!(result["warning"], Value::Null);
    assert_eq!(result["serviceCount"], json!(18));
    let compared = fx
        .text(&[
            "cmp",
            &saved_text,
            "--full",
            "--project",
            &fx.path("fresh/renium.project.jsonc"),
        ])
        .ok();
    assert_eq!(compared["changed"], json!(0));
    assert_eq!(compared["removed"], json!(0));
    assert_eq!(compared["added"], json!(15));
    assert_eq!(compared["unchanged"], json!(11));
    let rebuilt = fx.text(&["cmp", &saved_text, &built_text, "--full"]).ok();
    assert_eq!(rebuilt["changed"], json!(0));
    assert_eq!(rebuilt["removed"], json!(0));
    assert_eq!(rebuilt["added"], json!(15));
    assert_eq!(rebuilt["unchanged"], json!(11));
    let saved_xml = fx.root.join("saved.rbxlx");
    write_place_file(&saved_xml, &sample_place_dom());
    assert_eq!(
        fx.text(&[
            "pi",
            &saved_xml.to_string_lossy(),
            "-r",
            &fresh,
            "-s",
            "Workspace"
        ])
        .ok(),
        json!({"changedPaths": [], "importBase": true, "instances": 6, "ok": true, "services": 1})
    );
    assert_eq!(
        fx.text(&["bep", "-r", &fresh, "-o", &built_text]).ok()["base"],
        json!("<root>/saved.rbxlx")
    );
    fs::write(&saved_xml, b"changed").unwrap();
    let stale = fx.text(&["bep", "-r", &fresh, "-o", &built_text]).ok();
    assert_eq!(stale["base"], Value::Null);
    assert_eq!(stale["baseSource"], Value::Null);
    assert_eq!(
        stale["warning"],
        json!(
            "built without the imported place's unsynced services and root fields: <root>/saved.rbxlx changed since the import; pass --base"
        )
    );
    fs::remove_file(&saved_xml).unwrap();
    assert_eq!(
        fx.text(&["bep", "-r", &fresh, "-o", &built_text]).ok()["warning"],
        json!(
            "built without the imported place's unsynced services and root fields: <root>/saved.rbxlx is gone; pass --base"
        )
    );
    assert_eq!(
        fx.text(&["pi", &saved_text, "-r", &fresh, "-s", "Bogus"])
            .err(),
        "Unsupported service: Bogus"
    );
    assert!(
        fx.text(&["pi", &fx.path("nope.rbxl"), "-r", &fresh])
            .err()
            .starts_with("Failed to read <root>/nope.rbxl")
    );
    let txt = fx.root.join("saved.txt");
    fs::copy(&saved, &txt).unwrap();
    assert!(
        fx.text(&["pi", &txt.to_string_lossy(), "-r", &fresh])
            .err()
            .contains("rbxl or rbxlx")
    );
}

#[test]
fn import_place_into_an_existing_project_keeps_matching_identities() {
    let fx = Fixture::new("import-existing");
    let saved = fx.root.join("saved.rbxl");
    write_place_file(&saved, &sample_place_dom());
    let root = fx.path("");
    let result = fx.text(&["pi", &saved.to_string_lossy(), "-r", &root]).ok();
    assert_eq!(result["instances"], json!(8));
    assert_eq!(result["services"], json!(18));
    let changed = result["changedPaths"].as_array().unwrap();
    assert!(changed.contains(&json!("instances/Workspace.renium")));
    assert!(changed.contains(&json!("src/Workspace/Lobby")));
    assert!(changed.contains(&json!("src/ReplicatedStorage/Shared/Config.luau")));
    assert_eq!(
        fx.both(&["tr", "Workspace", "--depth", "2", "-r", &root]),
        json!([
            {"c": "Workspace", "cc": 3, "ch": ["lobby", "spawner", "target"], "id": "ws", "n": "Workspace", "ords": [1]},
            {"c": "Folder", "cc": 3, "ch": ["door", "4", "5"], "id": "lobby", "n": "Lobby", "ords": [1, 1]},
            {"c": "Part", "cc": 0, "id": "door", "n": "Door", "ords": [1, 1, 1]},
            {"c": "Part", "cc": 0, "id": "4", "n": "Pad", "ords": [1, 1, 1]},
            {"c": "Part", "cc": 0, "id": "5", "n": "Pad", "ords": [1, 1, 2]},
            {"c": "Script", "cc": 0, "id": "spawner", "n": "Spawner", "ords": [1, 1]},
            {"c": "ObjectValue", "cc": 0, "id": "target", "n": "Target", "ords": [1, 1]},
        ])
    );
    assert!(!fx.exists("src/Workspace/Lobby/Helper.client.luau"));
    assert_eq!(
        fs::read_to_string(fx.root.join("src/ReplicatedStorage/Shared/Config.luau")).unwrap(),
        "return { rounds = 3 }\r\n"
    );
    assert_eq!(
        fx.both(&["bg", "-f", &fx.store_arg("Workspace"), "-p", "Gravity"]),
        json!(150.0)
    );
    assert_eq!(
        fx.text(&["pi", &saved.to_string_lossy(), "-r", &root]).ok()["changedPaths"],
        json!([])
    );
}

#[test]
fn import_place_leaves_no_discarded_stage_beside_the_project() {
    let fx = Fixture::new("import-discard");
    let saved = fx.root.join("saved.rbxl");
    write_place_file(&saved, &sample_place_dom());
    let saved_text = saved.to_string_lossy().into_owned();
    // The stage is made beside the project, so the project gets a parent
    // folder no other test writes to.
    let project = fx.path("alone/project");
    for _ in 0..2 {
        fx.text(&["pi", &saved_text, "-r", &project]).ok();
    }
    // What the process does before it exits.
    crate::system::files::wait_for_discards();
    let beside = fs::read_dir(fx.root.join("alone"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(beside, ["project"]);
}

#[test]
fn merge_combines_disjoint_edits_and_reports_conflicts() {
    let fx = Fixture::new("merge");
    let base = fx.root.join("base.renium");
    let ours = fx.root.join("ours.renium");
    let theirs = fx.root.join("theirs.renium");
    let merged = fx.root.join("merged.renium");
    let (base_text, ours_text, theirs_text, merged_text) = (
        base.to_string_lossy().into_owned(),
        ours.to_string_lossy().into_owned(),
        theirs.to_string_lossy().into_owned(),
        merged.to_string_lossy().into_owned(),
    );
    let base_document = workspace_document();
    base_document.write_file(&base).unwrap();
    let mut ours_document = base_document.clone();
    ours_document.instances[2]
        .properties
        .insert("Anchored".into(), json!(false));
    ours_document.write_file(&ours).unwrap();
    let mut theirs_document = base_document.clone();
    theirs_document.instances[2]
        .properties
        .insert("Transparency".into(), json!(0.9));
    theirs_document
        .instances
        .push(settings_instance("extra", "Extra", "Folder", Some(0)));
    theirs_document
        .instances
        .retain(|instance| instance.settings_id != "cafe");
    theirs_document.write_file(&theirs).unwrap();
    assert_eq!(
        fx.both(&[
            "vcm",
            &base_text,
            &ours_text,
            &theirs_text,
            "-o",
            &merged_text
        ]),
        json!({"instances": 9, "ok": true, "output": "<root>/merged.renium", "path": "<root>/ours.renium"})
    );
    let door = &SettingsBytecode::read_file(&merged).unwrap().instances[2];
    assert_eq!(door.properties["Anchored"], json!(false));
    assert_eq!(door.properties["Transparency"], json!(0.9));
    let names = SettingsBytecode::read_file(&merged)
        .unwrap()
        .instances
        .iter()
        .map(|instance| instance.name.clone())
        .collect::<Vec<_>>();
    assert!(
        names.contains(&"Extra".to_string()) && !names.contains(&"Café ☕".to_string()),
        "{names:?}"
    );
    let ours_after = SettingsBytecode::read_file(&ours).unwrap();
    assert_eq!(ours_after.instances[2].properties["Anchored"], json!(false));
    assert_eq!(
        ours_after.instances[2].properties["Transparency"],
        json!(0.5)
    );
    assert_eq!(ours_after.instances.len(), 9);
    assert_eq!(
        fx.text(&[
            "vcm",
            &base_text,
            &ours_text,
            &theirs_text,
            "-o",
            &merged_text,
            "--path",
            "instances/Workspace.renium"
        ])
        .ok()["path"],
        json!("instances/Workspace.renium")
    );
    let conflict = fx.root.join("conflict.renium");
    let mut conflict_document = base_document.clone();
    conflict_document.instances[2]
        .properties
        .insert("Anchored".into(), json!(true));
    conflict_document.instances[2]
        .properties
        .insert("Transparency".into(), json!(0.1));
    conflict_document.instances[2]
        .properties
        .insert("Anchored".into(), json!(true));
    conflict_document.instances[2]
        .attributes
        .insert("Owner".into(), json!("Bob"));
    conflict_document.write_file(&conflict).unwrap();
    let mut ours_conflict = base_document.clone();
    ours_conflict.instances[2]
        .properties
        .insert("Transparency".into(), json!(0.3));
    ours_conflict.instances[2]
        .attributes
        .insert("Owner".into(), json!("Cy"));
    ours_conflict.write_file(&ours).unwrap();
    let conflict_text = conflict.to_string_lossy().into_owned();
    assert_eq!(
        fx.text(&[
            "vcm",
            &base_text,
            &ours_text,
            &conflict_text,
            "-o",
            &merged_text
        ])
        .err(),
        "2 merge conflict(s) in <root>/ours.renium. Re-run with --prefer ours|theirs, or resolve the listed fields manually and retry."
    );
    assert_eq!(
        fx.text(&[
            "vcm",
            &base_text,
            &ours_text,
            &conflict_text,
            "-o",
            &merged_text,
            "--prefer",
            "ours"
        ])
        .ok()["instances"],
        json!(9)
    );
    assert_eq!(
        fx.both(&["bg", "-f", &merged_text, "Door", "Transparency"]),
        json!(0.3)
    );
    assert_eq!(
        fx.both(&["bg", "-f", &merged_text, "Door", "Owner"]),
        json!("Cy")
    );
    fx.text(&[
        "vcm",
        &base_text,
        &ours_text,
        &conflict_text,
        "-o",
        &merged_text,
        "--prefer",
        "theirs",
    ])
    .ok();
    assert_eq!(
        fx.both(&["bg", "-f", &merged_text, "Door", "Transparency"]),
        json!(0.1)
    );
    assert_eq!(
        fx.both(&["bg", "-f", &merged_text, "Door", "Owner"]),
        json!("Bob")
    );
    assert_eq!(
        fx.text(&[
            "vcm",
            &base_text,
            &ours_text,
            &conflict_text,
            "--prefer",
            "mine"
        ])
        .err(),
        "--prefer must be `ours` or `theirs`, got `mine`"
    );
    let empty = fx.root.join("empty.renium");
    fs::write(&empty, b"").unwrap();
    assert!(
        fx.text(&[
            "vcm",
            &empty.to_string_lossy(),
            &ours_text,
            &theirs_text,
            "-o",
            &merged_text
        ])
        .err()
        .starts_with("2 merge conflict(s)")
    );
    assert_eq!(
        fx.text(&[
            "vcm",
            &base_text,
            &base_text,
            &empty.to_string_lossy(),
            "-o",
            &merged_text
        ])
        .ok()["instances"],
        json!(0)
    );
    assert!(
        fx.text(&["vcm", &base_text, &fx.path("nope.renium"), &theirs_text])
            .err()
            .starts_with("Failed to stat ours file <root>/nope.renium")
    );
    ours_document.write_file(&ours).unwrap();
    let in_place = fx.text(&["vcm", &base_text, &ours_text, &theirs_text]);
    assert_eq!(in_place.error, None);
    assert_eq!(in_place.value, Value::Null);
    assert_eq!(in_place.stdout, "");
    assert_eq!(
        fx.both(&["bg", "-f", &ours_text, "Door", "Transparency"]),
        json!(0.9)
    );
    assert_eq!(
        fx.text(&["vcm", &base_text, &ours_text, &theirs_text, "--pretty"])
            .ok(),
        json!({"instances": 9, "ok": true, "output": "<root>/ours.renium", "path": "<root>/ours.renium"})
    );
}
