use super::*;

fn door_lookup() -> Value {
    json!({"c": "Part", "id": "door", "n": "Door", "path": "Workspace.Lobby.Door"})
}

fn border_lookups() -> Value {
    json!([
        {"c": "Part", "id": "border1", "n": "Border", "path": "Workspace.Lobby.Border"},
        {"c": "Part", "id": "border2", "n": "Border", "path": "Workspace.Lobby.Border[2]"},
    ])
}

fn workspace_tree() -> Value {
    json!([
        {"c": "Workspace", "cc": 4, "ch": ["lobby", "spawner", "target", "cafe"], "id": "ws", "n": "Workspace", "ords": [1]},
        {"c": "Folder", "cc": 4, "id": "lobby", "n": "Lobby", "ords": [1, 1]},
        {"c": "Script", "cc": 0, "id": "spawner", "n": "Spawner", "ords": [1, 1]},
        {"c": "ObjectValue", "cc": 0, "id": "target", "n": "Target", "ords": [1, 1]},
        {"c": "Folder", "cc": 0, "id": "cafe", "n": "Café ☕", "ords": [1, 1]},
    ])
}

fn lobby_tree() -> Value {
    json!([
        {"c": "Folder", "cc": 4, "ch": ["door", "border1", "border2", "helper"], "id": "lobby", "n": "Lobby", "ords": [1, 1]},
        {"c": "Part", "cc": 0, "id": "door", "n": "Door", "ords": [1, 1, 1]},
        {"c": "Part", "cc": 0, "id": "border1", "n": "Border", "ords": [1, 1, 1]},
        {"c": "Part", "cc": 0, "id": "border2", "n": "Border", "ords": [1, 1, 2]},
        {"c": "LocalScript", "cc": 0, "id": "helper", "n": "Helper", "ords": [1, 1, 1]},
    ])
}

fn border_ambiguity() -> Value {
    json!({
        "c": "Part",
        "ct": 2,
        "e": "ambiguous",
        "m": [{"id": "border1", "ord": 1}, {"id": "border2", "ord": 2}],
        "path": "Workspace.Lobby.Border",
    })
}

const STRUCTURED_FAILURE: &str = "command reported a structured failure";
const MISSING_STARTER_GUI: &str = "No synced Renium store for service 'StarterGui'";

#[test]
fn find_prints_lookup_rows_in_text_and_json_mode() {
    let fx = Fixture::new("find-golden");
    let root = fx.path("");
    assert_eq!(
        fx.both(&["f", "Workspace", "Door", "-r", &root]),
        json!([door_lookup()])
    );
    let full = json!({
        "attributes": door_attrs(false),
        "className": "Part",
        "name": "Door",
        "path": "Workspace.Lobby.Door",
        "properties": door_props(),
        "settingsId": "door",
    });
    assert_eq!(
        fx.text(&["f", "Workspace", "Door", "-o", "full", "-r", &root])
            .ok(),
        json!({"matches": [full], "service": "Workspace", "settingsFile": "<root>/instances/Workspace.renium"})
    );
    let mut json_full = full;
    json_full["attributes"] = door_attrs(true);
    json_full.as_object_mut().unwrap().remove("path");
    json_full["pathOrdinals"] = json!([1, 1, 1]);
    json_full["pathSegments"] = json!(["Workspace", "Lobby", "Door"]);
    assert_eq!(
        fx.json(&["f", "Workspace", "Door", "-o", "full", "-r", &root])
            .ok(),
        json!({"matches": [json_full], "service": "Workspace", "settingsFile": "<root>/instances/Workspace.renium"})
    );
    let summary = json!({"className": "Part", "name": "Door", "path": "Workspace.Lobby.Door", "settingsId": "door"});
    for mode in ["summary", "detail"] {
        assert_eq!(
            fx.text(&["f", "Workspace", "Door", "-o", mode, "-r", &root])
                .ok()["matches"],
            json!([summary])
        );
    }
}

#[test]
fn find_filters_by_name_class_tag_property_attribute_and_fields() {
    let fx = Fixture::new("find-filters");
    let root = fx.path("");
    let run = |args: &[&str]| fx.both(&[&["f", "Workspace"], args, &["-r", &root]].concat());
    assert_eq!(run(&["-n", "Border"]), border_lookups());
    assert_eq!(
        run(&["-c", "Script"]),
        json!([{"c": "Script", "id": "spawner", "n": "Spawner", "path": "Workspace.Spawner"}])
    );
    assert_eq!(run(&["-t", "Interactive"]), json!([door_lookup()]));
    assert_eq!(run(&["-p", "Anchored=true"]), json!([door_lookup()]));
    assert_eq!(run(&["-p", "Anchored"]), json!([door_lookup()]));
    assert_eq!(run(&["-a", "Reviewed=true"]), json!([door_lookup()]));
    assert_eq!(run(&["-a", "Owner=\"Bob\""]), json!([]));
    assert_eq!(run(&["Nope"]), json!([]));
    assert_eq!(
        run(&["Door", "-F", "id,n"]),
        json!([{"id": "door", "n": "Door"}])
    );
    assert_eq!(
        run(&["Border", "--limit", "1"]),
        json!([border_lookups()[0]])
    );
    assert_eq!(run(&["Border", "--limit", "1", "--all"]), border_lookups());
    assert_eq!(
        run(&["Door", "-F", "lookup,prop:Transparency,attr:Owner"]),
        json!([{"attrs": {"Owner": "Ada"}, "c": "Part", "id": "door", "n": "Door", "path": "Workspace.Lobby.Door", "props": {"Transparency": 0.5}}])
    );
}

#[test]
fn find_scopes_the_search_by_dotted_service_path_parent_and_ordinals() {
    let fx = Fixture::new("find-scopes");
    let root = fx.path("");
    let lobby_subtree = json!([
        {"c": "Folder", "id": "lobby", "n": "Lobby", "path": "Workspace.Lobby"},
        door_lookup(),
        border_lookups()[0],
        border_lookups()[1],
        {"c": "LocalScript", "id": "helper", "n": "Helper", "path": "Workspace.Lobby.Helper"},
    ]);
    assert_eq!(
        fx.both(&["f", "Workspace.Lobby", "-r", &root]),
        lobby_subtree
    );
    assert_eq!(
        fx.both(&[
            "f",
            "Workspace",
            "--path",
            "Lobby",
            "-c",
            "Part",
            "-r",
            &root
        ]),
        json!([door_lookup(), border_lookups()[0], border_lookups()[1]])
    );
    assert_eq!(
        fx.both(&["f", "Workspace", "-I", "lobby", "Border", "-r", &root]),
        border_lookups()
    );
    assert_eq!(
        fx.both(&[
            "f",
            "Workspace",
            "-I",
            "Lobby",
            "-c",
            "LocalScript",
            "-r",
            &root
        ]),
        json!([{"c": "LocalScript", "id": "helper", "n": "Helper", "path": "Workspace.Lobby.Helper"}])
    );
    assert_eq!(
        fx.both(&[
            "f",
            "Workspace",
            "--path",
            "Lobby.Border",
            "--ords",
            "[1,1,2]",
            "-r",
            &root
        ]),
        json!([border_lookups()[1]])
    );
    assert_eq!(
        fx.both(&["f", "Workspace", "--path", "Lobby.Border[2]", "-r", &root]),
        json!([border_lookups()[1]])
    );
    assert_eq!(
        fx.both(&["f", "Workspace.Café ☕", "-r", &root]),
        json!([{"c": "Folder", "id": "cafe", "n": "Café ☕", "path": "Workspace.Café ☕"}])
    );
    let ambiguous = fx.text(&["f", "Workspace", "--path", "Lobby.Border", "-r", &root]);
    assert_eq!(ambiguous.value, border_ambiguity());
    assert_eq!(ambiguous.error.as_deref(), Some(STRUCTURED_FAILURE));
    assert!(
        fx.text(&[
            "f",
            "Workspace",
            "--path",
            "Lobby",
            "-I",
            "lobby",
            "Door",
            "-r",
            &root
        ])
        .err()
        .contains("Scope the search with --path or -I, not both")
    );
    assert!(
        fx.text(&["f", "Workspace", "--path", "Nope", "-r", &root])
            .err()
            .contains("No matching instance path: Workspace.Nope")
    );
}

#[test]
fn find_query_syntax_covers_classes_properties_paths_alternatives_and_unicode() {
    let fx = Fixture::new("find-query");
    let root = fx.path("");
    let run = |query: &str| fx.both(&["f", "Workspace", query, "-r", &root]);
    assert_eq!(
        run("is:Part"),
        json!([door_lookup(), border_lookups()[0], border_lookups()[1]])
    );
    assert_eq!(run("Transparency>0.6"), json!([border_lookups()[1]]));
    assert_eq!(run("Transparency=0.25"), json!([border_lookups()[0]]));
    assert_eq!(run("tag:Interactive"), json!([door_lookup()]));
    assert_eq!(
        run("Lobby.*"),
        json!([
            door_lookup(),
            border_lookups()[0],
            border_lookups()[1],
            {"c": "LocalScript", "id": "helper", "n": "Helper", "path": "Workspace.Lobby.Helper"},
        ])
    );
    assert_eq!(
        run("Door or Spawner"),
        json!([door_lookup(), {"c": "Script", "id": "spawner", "n": "Spawner", "path": "Workspace.Spawner"}])
    );
    assert_eq!(
        run("Café"),
        json!([{"c": "Folder", "id": "cafe", "n": "Café ☕", "path": "Workspace.Café ☕"}])
    );
    assert_eq!(
        fx.both(&["f", "-s", "Workspace", "Door", "-r", &root]),
        json!([door_lookup()])
    );
    assert_eq!(
        fx.both(&["f", "-s", "Workspace", "Door", "Spawner", "-r", &root]),
        json!([])
    );
}

#[test]
fn find_reports_missing_query_store_project_and_output_mode() {
    let fx = Fixture::new("find-errors");
    let root = fx.path("");
    assert!(
        fx.text(&["f", "Workspace", "-r", &root])
            .err()
            .contains("Provide a query or filter: find <SERVICE> <QUERY>")
    );
    assert!(
        fx.text(&["f", "StarterGui", "Door", "-r", &root])
            .err()
            .contains(MISSING_STARTER_GUI)
    );
    assert!(
        fx.text(&["f", "Workspace", "Door", "-o", "bogus", "-r", &root])
            .err()
            .contains("Invalid output mode: bogus")
    );
    let outside = temp_dir("offline-no-project");
    let error = run_at(
        &outside,
        CaptureMode::Text,
        &["f", "Workspace", "Door", "-r", &outside.to_string_lossy()],
    )
    .err();
    assert!(
        error.contains("No Renium project in <root> or its parents"),
        "{error}"
    );
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn tree_prints_nodes_in_text_and_json_mode() {
    let fx = Fixture::new("tree-golden");
    let root = fx.path("");
    assert_eq!(fx.both(&["tr", "Workspace", "-r", &root]), workspace_tree());
    let full = fx.both(&["tr", "Workspace", "-o", "full", "-r", &root]);
    assert_eq!(
        full,
        json!({
            "nodes": [
                {"attributes": {}, "childCount": 4, "children": ["lobby", "spawner", "target", "cafe"], "className": "Workspace", "name": "Workspace", "pathOrdinals": [1], "properties": {"Gravity": 120.0}, "settingsId": "ws"},
                {"attributes": {}, "childCount": 4, "className": "Folder", "name": "Lobby", "pathOrdinals": [1, 1], "properties": {}, "settingsId": "lobby"},
                {"attributes": {}, "childCount": 0, "className": "Script", "name": "Spawner", "pathOrdinals": [1, 1], "properties": {"Source": "__SOURCE_EXTERNAL__"}, "settingsId": "spawner"},
                {"attributes": {}, "childCount": 0, "className": "ObjectValue", "name": "Target", "pathOrdinals": [1, 1], "properties": {"Value": {"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"}}, "settingsId": "target"},
                {"attributes": {}, "childCount": 0, "className": "Folder", "name": "Café ☕", "pathOrdinals": [1, 1], "properties": {}, "settingsId": "cafe"},
            ],
            "rootIds": ["ws"],
            "service": "Workspace",
            "settingsFile": "<root>/instances/Workspace.renium",
        })
    );
    let text = fx
        .text(&[
            "tr",
            "Workspace",
            "Lobby.Door",
            "-o",
            "summary",
            "-F",
            "lookup",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        text["nodes"],
        json!([{"className": "Part", "name": "Door", "path": "Workspace.Lobby.Door", "settingsId": "door"}])
    );
    let json_mode = fx
        .json(&[
            "tr",
            "Workspace",
            "Lobby.Door",
            "-o",
            "summary",
            "-F",
            "lookup",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        json_mode["nodes"],
        json!([{"className": "Part", "name": "Door", "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"}])
    );
}

#[test]
fn tree_takes_every_target_form_depth_and_limit() {
    let fx = Fixture::new("tree-targets");
    let root = fx.path("");
    let run = |args: &[&str]| fx.both(&[&["tr", "Workspace"], args, &["-r", &root]].concat());
    assert_eq!(
        fx.both(&["tr", "Workspace.Lobby", "--depth", "2", "-r", &root]),
        lobby_tree()
    );
    assert_eq!(run(&["Lobby", "--ords", "[1,1]"]), lobby_tree());
    assert_eq!(run(&["--path", "[\"Workspace\",\"Lobby\"]"]), lobby_tree());
    let door = json!([{"c": "Part", "cc": 0, "id": "door", "n": "Door", "ords": [1, 1, 1]}]);
    assert_eq!(run(&["Lobby.Door"]), door);
    assert_eq!(run(&["Door"]), door);
    assert_eq!(run(&["-i", "door"]), door);
    assert_eq!(run(&["-x", "2"]), door);
    assert_eq!(run(&["-n", "Door"]), door);
    assert_eq!(
        run(&["-c", "ObjectValue"]),
        json!([{"c": "ObjectValue", "cc": 0, "id": "target", "n": "Target", "ords": [1, 1]}])
    );
    assert_eq!(
        run(&["Lobby.Border[2]"]),
        json!([{"c": "Part", "cc": 0, "id": "border2", "n": "Border", "ords": [1, 1, 2]}])
    );
    assert_eq!(
        run(&["--depth", "0"]),
        json!([{"c": "Workspace", "cc": 4, "id": "ws", "n": "Workspace", "ords": [1]}])
    );
    let mut limited = workspace_tree().as_array().unwrap()[..3].to_vec();
    limited[0]["ch"] = json!(["lobby", "spawner"]);
    assert_eq!(run(&["--limit", "3"]), json!(limited));
    assert_eq!(
        run(&["Café ☕"]),
        json!([{"c": "Folder", "cc": 0, "id": "cafe", "n": "Café ☕", "ords": [1, 1]}])
    );
    assert_eq!(
        fx.both(&["tr", "-s", "Workspace", "Lobby.Door", "-r", &root]),
        door
    );
}

#[test]
fn tree_reports_ambiguous_targets_as_a_structured_failure() {
    let fx = Fixture::new("tree-ambiguous");
    let root = fx.path("");
    let shared = fx.text(&["tr", "Workspace", "Border", "-r", &root]);
    assert_eq!(shared.value, border_ambiguity());
    assert_eq!(shared.error.as_deref(), Some(STRUCTURED_FAILURE));
    assert_eq!(
        fx.json(&["tr", "Workspace", "Border", "-r", &root]).value,
        border_ambiguity()
    );
    let by_class = fx.text(&["tr", "Workspace", "-c", "Folder", "-r", &root]);
    assert_eq!(
        by_class.value,
        json!({
            "c": "Folder",
            "ct": 2,
            "e": "ambiguous",
            "m": [
                {"id": "lobby", "n": "Lobby", "path": "Workspace.Lobby"},
                {"id": "cafe", "n": "Café ☕", "path": "Workspace.Café ☕"},
            ],
        })
    );
    let by_name = fx.text(&["tr", "Workspace", "-n", "Border", "-r", &root]);
    assert_eq!(by_name.value, border_ambiguity());
    let full = fx.text(&["tr", "Workspace", "Border", "-o", "full", "-r", &root]);
    assert_eq!(full.value["error"], "ambiguous");
    assert_eq!(full.value["count"], 2);
    assert_eq!(full.value["matches"][1]["settingsId"], "border2");
}

#[test]
fn tree_reports_missing_targets_and_bad_arguments() {
    let fx = Fixture::new("tree-errors");
    let root = fx.path("");
    let err = |args: &[&str]| {
        fx.text(&[&["tr", "Workspace"], args, &["-r", &root]].concat())
            .err()
    };
    assert!(
        err(&["Nope"])
            .contains("No matching instance: Nope; `rbx f SERVICE NAME` searches by name")
    );
    assert!(err(&["Lobby.Nope"]).contains("No matching instance path: Workspace.Lobby.Nope"));
    assert!(err(&["-x", "99"]).contains("No matching instance index 99"));
    assert_eq!(err(&["-i", "nope"]), "No matching instance");
    assert_eq!(err(&["-n", "Nope"]), "No matching instance");
    assert!(
        err(&["Lobby", "--ords", "[1,2]"]).contains("No matching instance path: Workspace.Lobby")
    );
    assert!(
        err(&["Door", "-x", "1"]).contains("Path target cannot be combined with another selector")
    );
    assert!(err(&["--ords", "[1]"]).contains("--ords requires --path or a positional target"));
    assert!(
        err(&["Lobby.Border[2]", "--ords", "[1,1,2]"])
            .contains("inline (Name[2]) or with --ords, not both")
    );
    assert!(
        err(&["--path", "Lobby", "Door"])
            .contains("Provide either --path or a positional target, not both")
    );
    assert!(err(&["--ords", "[x]", "--path", "Lobby"]).contains("Invalid ordinals JSON"));
    assert!(err(&["-o", "bogus"]).contains("Invalid output mode: bogus"));
    assert!(
        fx.text(&["tr", "-r", &root])
            .err()
            .contains("Provide a service: tree <SERVICE>")
    );
    assert!(
        fx.text(&["tr", "StarterGui", "-r", &root])
            .err()
            .contains(MISSING_STARTER_GUI)
    );
}

#[test]
fn inspect_prints_one_instance_in_text_and_json_mode() {
    let fx = Fixture::new("inspect-golden");
    let root = fx.path("");
    assert_eq!(
        fx.both(&["in", "Workspace", "-r", &root]),
        json!({"c": "Workspace", "cc": 4, "id": "ws", "n": "Workspace", "path": "Workspace"})
    );
    assert_eq!(
        fx.both(&["in", "Workspace", "Lobby.Door", "-r", &root]),
        json!({"c": "Part", "cc": 0, "id": "door", "n": "Door", "path": "Workspace.Lobby.Door"})
    );
    let full = json!({
        "attributes": door_attrs(false),
        "childCount": 0,
        "className": "Part",
        "name": "Door",
        "path": "Workspace.Lobby.Door",
        "properties": door_props(),
        "settingsId": "door",
    });
    assert_eq!(
        fx.text(&["in", "Workspace", "Door", "-o", "full", "-r", &root])
            .ok(),
        full
    );
    let mut json_full = full;
    json_full["attributes"] = door_attrs(true);
    json_full.as_object_mut().unwrap().remove("path");
    json_full["pathOrdinals"] = json!([1, 1, 1]);
    json_full["pathSegments"] = json!(["Workspace", "Lobby", "Door"]);
    assert_eq!(
        fx.json(&["in", "Workspace", "Door", "-o", "full", "-r", &root])
            .ok(),
        json_full
    );
}

#[test]
fn inspect_reads_properties_attributes_sources_and_references() {
    let fx = Fixture::new("inspect-fields");
    let root = fx.path("");
    assert_eq!(
        fx.both(&["in", "Workspace", "Door", "-F", "props", "-r", &root]),
        json!({"props": door_props()})
    );
    assert_eq!(
        fx.text(&["in", "Workspace", "Door", "-F", "attrs,props", "-r", &root])
            .ok(),
        json!({"attrs": door_attrs(false), "props": door_props()})
    );
    assert_eq!(
        fx.json(&["in", "Workspace", "Door", "-F", "attrs,props", "-r", &root])
            .ok(),
        json!({"attrs": door_attrs(true), "props": door_props()})
    );
    assert_eq!(
        fx.both(&[
            "in",
            "Workspace",
            "Door",
            "-F",
            "prop:Anchored,attr:Owner",
            "-r",
            &root
        ]),
        json!({"attrs": {"Owner": "Ada"}, "props": {"Anchored": true}})
    );
    assert_eq!(
        fx.both(&[
            "in",
            "Workspace",
            "Spawner",
            "-F",
            "prop:Source",
            "-r",
            &root
        ]),
        json!({"props": {"Source": SPAWNER_SOURCE}})
    );
    assert_eq!(
        fx.both(&[
            "in",
            "Workspace",
            "Lobby.Helper",
            "-F",
            "prop:Source",
            "-r",
            &root
        ]),
        json!({"props": {"Source": HELPER_SOURCE}})
    );
    let door_ref = json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"});
    assert_eq!(
        fx.both(&["in", "Workspace", "Target", "-F", "props", "-r", &root]),
        json!({"props": {"Value": door_ref}})
    );
    assert_eq!(
        fx.both(&[
            "in",
            "ReplicatedStorage",
            "Link",
            "-F",
            "props",
            "-r",
            &root
        ]),
        json!({"props": {"Value": door_ref}})
    );
    assert_eq!(
        fx.both(&["in", "Workspace", "Café ☕", "-r", &root]),
        json!({"c": "Folder", "cc": 0, "id": "cafe", "n": "Café ☕", "path": "Workspace.Café ☕"})
    );
    assert_eq!(
        fx.both(&[
            "in",
            "Workspace",
            "--path",
            "[\"Workspace\",\"Lobby\",\"Border\"]",
            "--ords",
            "[1,1,2]",
            "-r",
            &root
        ]),
        json!({"c": "Part", "cc": 0, "id": "border2", "n": "Border", "path": "Workspace.Lobby.Border[2]"})
    );
    assert_eq!(
        fx.both(&["in", "-s", "Workspace", "Door", "-r", &root]),
        json!({"c": "Part", "cc": 0, "id": "door", "n": "Door", "path": "Workspace.Lobby.Door"})
    );
    assert_eq!(
        fx.both(&["in", "Workspace", "-x", "0", "-F", "brief,hpl", "-r", &root]),
        json!({"c": "Workspace", "cc": 4, "hpl": false, "id": "ws", "n": "Workspace", "path": "Workspace"})
    );
}

#[test]
fn inspect_reports_ambiguous_and_missing_targets() {
    let fx = Fixture::new("inspect-errors");
    let root = fx.path("");
    let ambiguous = fx.text(&["in", "Workspace", "Border", "-r", &root]);
    assert_eq!(ambiguous.value, border_ambiguity());
    assert_eq!(ambiguous.error.as_deref(), Some(STRUCTURED_FAILURE));
    assert!(
        fx.text(&[
            "in",
            "Workspace",
            "--path",
            "Lobby",
            "--ords",
            "[1,2]",
            "-r",
            &root
        ])
        .err()
        .contains("No matching instance path: Workspace.Lobby")
    );
    assert!(
        fx.text(&["in", "Workspace", "Nope", "-r", &root])
            .err()
            .contains("No matching instance: Nope")
    );
    assert!(
        fx.text(&["in", "StarterGui", "-r", &root])
            .err()
            .contains(MISSING_STARTER_GUI)
    );
}

#[test]
fn get_property_reads_every_stored_value_kind() {
    let fx = Fixture::new("get-kinds");
    let store = fx.store_arg("Workspace");
    let get = |target: &str, property: &str| fx.both(&["bg", "-f", &store, target, property]);
    assert_eq!(get("Lobby.Door", "Anchored"), json!(true));
    assert_eq!(get("Door", "Transparency"), json!(0.5));
    assert_eq!(
        get("Door", "Size"),
        json!({"_type": "Vector3", "x": 4.0, "y": 8.0, "z": 1.0})
    );
    assert_eq!(
        get("Door", "Color"),
        json!({"_type": "Color3", "b": 0.0, "g": 0.5, "r": 1.0})
    );
    assert_eq!(
        get("Door", "CFrame"),
        json!({"_type": "CFrame", "components": DOOR_CFRAME})
    );
    assert_eq!(
        get("Door", "Material"),
        json!({"_type": "EnumItem", "name": "Wood"})
    );
    assert_eq!(get("Door", "Tags"), json!(["Interactive"]));
    assert_eq!(get("Door", "Reviewed"), json!(true));
    assert_eq!(get("Door", "Owner"), json!("Ada"));
    assert_eq!(
        get("Door", "Spawn"),
        json!({"_type": "Vector3", "x": 1.0, "y": 2.0, "z": 3.0})
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Door", "Reviewed", "-S", "attribute"]),
        json!(true)
    );
    assert_eq!(
        fx.text(&["bg", "-f", &store, "Door", "Ratio"]).ok(),
        json!(0.2)
    );
    assert_eq!(
        fx.json(&["bg", "-f", &store, "Door", "Ratio"]).ok(),
        json!(RATIO_EXPANDED)
    );
    let property_only = fx
        .text(&["bg", "-f", &store, "Door", "Reviewed", "-S", "property"])
        .err();
    assert!(
        property_only.starts_with("Property not found: Reviewed"),
        "{property_only}"
    );
    assert_eq!(get("Door", "CanCollide"), json!(true));
    assert_eq!(get("Door", "anchored"), json!(true));
    assert_eq!(get("Lobby.Border[2]", "Transparency"), json!(0.75));
    assert_eq!(get("Café ☕", "Name"), json!("Café ☕"));
}

#[test]
fn get_property_reads_metadata_sources_references_and_the_service_root() {
    let fx = Fixture::new("get-meta");
    let store = fx.store_arg("Workspace");
    let replicated = fx.store_arg("ReplicatedStorage");
    let get = |target: &str, property: &str| fx.both(&["bg", "-f", &store, target, property]);
    assert_eq!(get("Door", "Name"), json!("Door"));
    assert_eq!(get("Door", "Parent"), json!("lobby"));
    assert_eq!(get("Door", "ClassName"), json!("Part"));
    assert_eq!(
        fx.both(&["bg", "-f", &store, "-p", "Gravity"]),
        json!(120.0)
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "-p", "Name"]),
        json!("Workspace")
    );
    assert_eq!(fx.both(&["bg", "-f", &store, "-p", "Parent"]), Value::Null);
    assert_eq!(get("Spawner", "Source"), json!(SPAWNER_SOURCE));
    assert_eq!(get("Lobby.Helper", "Source"), json!(HELPER_SOURCE));
    let door_ref = json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Lobby", "Door"], "settingsId": "door"});
    assert_eq!(get("Target", "Value"), door_ref);
    assert_eq!(
        fx.both(&["bg", "-f", &replicated, "Link", "Value"]),
        door_ref
    );
    assert_eq!(
        fx.both(&["bg", "-f", &replicated, "Shared.Config", "Source"]),
        json!(CONFIG_SOURCE)
    );
    fs::remove_file(fx.source_path("Workspace", 5)).unwrap();
    assert_eq!(get("Spawner", "Source"), json!(EXTERNAL_SOURCE_MARKER));
    assert!(
        fx.text(&["bg", "-f", &store, "Door", "Value", "-S", "metadata"])
            .err()
            .starts_with("Property not found: Value")
    );
}

#[test]
fn get_property_takes_every_selector_form() {
    let fx = Fixture::new("get-selectors");
    let store = fx.store_arg("Workspace");
    let run = |args: &[&str]| fx.both(&[&["bg", "-f", &store], args].concat());
    assert_eq!(run(&["-i", "door", "Anchored"]), json!(true));
    assert_eq!(run(&["-x", "2", "Anchored"]), json!(true));
    assert_eq!(run(&["-n", "Door", "-p", "Anchored"]), json!(true));
    assert_eq!(
        run(&["-n", "Helper", "-c", "LocalScript", "-p", "Name"]),
        json!("Helper")
    );
    assert_eq!(run(&["-c", "Script", "Source"]), json!(SPAWNER_SOURCE));
    assert_eq!(run(&["--path", "Lobby.Door", "Anchored"]), json!(true));
    assert_eq!(
        run(&[
            "--path",
            "[\"Workspace\",\"Lobby\",\"Border\"]",
            "--ords",
            "[1,1,2]",
            "Transparency"
        ]),
        json!(0.75)
    );
    assert_eq!(run(&["Lobby.Door", "-p", "Anchored"]), json!(true));
    let path_form = fx.path("instances/Workspace.renium");
    assert_eq!(
        fx.both(&["bg", &path_form, "Lobby.Door", "Anchored"]),
        json!(true)
    );
    assert_eq!(
        fx.both(&["bg", &path_form, "-i", "door", "-p", "Anchored"]),
        json!(true)
    );
}

#[test]
fn get_property_reports_missing_names_targets_stores_and_bad_arguments() {
    let fx = Fixture::new("get-errors");
    let store = fx.store_arg("Workspace");
    let err = |args: &[&str]| fx.text(&[&["bg", "-f", &store], args].concat()).err();
    let missing = err(&["Door", "Nope"]);
    assert!(missing.starts_with("Property not found: Nope"), "{missing}");
    assert!(
        missing.ends_with("(rbx in Workspace Door -F props lists all)"),
        "{missing}"
    );
    let attribute = err(&["Door", "Owne", "-S", "attribute"]);
    assert!(
        attribute.contains("stored attributes include Owner"),
        "{attribute}"
    );
    let close = err(&["Door", "Transparenc"]);
    assert!(
        close
            .starts_with("Property not found: Transparenc; stored properties include Transparency"),
        "{close}"
    );
    assert!(err(&["Door", "Anchored", "-S", "bogus"]).contains("Invalid property scope: bogus"));
    assert!(err(&["Border", "Anchored"]).contains("2 instances are named \"Border\""));
    assert!(err(&["-c", "Part", "Anchored"]).contains("Ambiguous selector className \"Part\""));
    assert!(err(&["Nope", "Anchored"]).contains("No matching instance path: Workspace.Nope"));
    assert_eq!(err(&["-i", "nope", "Anchored"]), "No matching instance");
    assert!(err(&["Door"]).contains("Name the property: rbx bg SERVICE TARGET PROPERTY"));
    assert!(err(&["-x", "1", "-n", "Door", "Name"]).contains("Provide only one selector"));
    // With a flag selector every positional word is a property name.
    assert!(err(&["Door", "-i", "door", "Name"]).starts_with("Property not found: Door"));
    // Several property names read in one call; a missing one still names itself.
    assert!(err(&["Door", "Size", "Extra"]).starts_with("Property not found: Extra"));
    let missing_store = fx.store_arg("StarterGui");
    let missing_store = fx.text(&["bg", "-f", &missing_store, "-p", "Name"]).err();
    assert!(
        missing_store.contains(MISSING_STARTER_GUI),
        "{missing_store}"
    );
    assert!(missing_store.contains("rbx ba StarterGui --name <Name> --class-name <Class>"));
    let both = fx
        .text(&["bg", "other.renium", "-f", &store, "-p", "Name"])
        .err();
    assert!(
        both.contains("Provide either SERVICE_OR_FILE or --file, not both"),
        "{both}"
    );
    let as_target = fx
        .text(&["bg", "ReplicatedStorage", "-f", &store, "-p", "Name"])
        .err();
    assert!(
        as_target.contains("No matching instance path: Workspace.ReplicatedStorage"),
        "{as_target}"
    );
}

#[test]
fn batch_arguments_parse_the_store_and_project_root_together() {
    let parsed = crate::cli::Cli::try_parse_from([
        "rbx",
        "bb",
        "--project-root",
        "r",
        "-s",
        "Workspace",
        "-f",
        "x.renium",
        "-j",
        "[]",
    ])
    .unwrap();
    let crate::cli::Commands::BytecodeExplorerBatch(args) = parsed.command else {
        panic!("bb parses to the batch command");
    };
    assert_eq!(args.project_root.as_deref(), Some(Path::new("r")));
    assert_eq!(args.service, "Workspace");
    assert_eq!(
        args.input.settings_file.as_deref(),
        Some(Path::new("x.renium"))
    );
    assert_eq!(args.ops_json.as_deref(), Some("[]"));
}

fn set_result(property: &str) -> Value {
    json!({
        "changedPaths": ["<root>/instances/Workspace.renium"],
        "ok": true,
        "property": property,
        "settingsFile": "<root>/instances/Workspace.renium",
    })
}

#[test]
fn set_property_writes_every_value_form() {
    let fx = Fixture::new("set-values");
    let store = fx.store_arg("Workspace");
    let set = |args: &[&str]| {
        fx.text(&[&["bs", "-f", &store, "Lobby.Door", "-p"], args].concat())
            .ok()
    };
    let get = |property: &str| fx.both(&["bg", "-f", &store, "Door", property]);
    assert_eq!(
        set(&["Transparency", "--num", "0.25"]),
        set_result("Transparency")
    );
    assert_eq!(get("Transparency"), json!(0.25));
    assert_eq!(
        set(&["Anchored", "--bool", "false"]),
        set_result("Anchored")
    );
    assert_eq!(get("Anchored"), json!(false));
    assert_eq!(set(&["Transparency", "--null"]), set_result("Transparency"));
    assert_eq!(get("Transparency"), json!(0.0));
    assert!(
        !fx.read_store("Workspace").instances[2]
            .properties
            .contains_key("Transparency")
    );
    assert_eq!(
        set(&["Size", "-j", r#"{"_type":"Vector3","x":1,"y":2,"z":3}"#]),
        set_result("Size")
    );
    assert_eq!(
        get("Size"),
        json!({"_type": "Vector3", "x": 1.0, "y": 2.0, "z": 3.0})
    );
    let unchanged = fx
        .text(&["bs", "-f", &store, "Door", "-p", "Size", "-j", "[1,2,3]"])
        .ok();
    assert_eq!(unchanged["changedPaths"], json!([]));
    assert_eq!(set(&["Size", "-j", "[2,2,2]"]), set_result("Size"));
    assert_eq!(
        get("Size"),
        json!({"_type": "Vector3", "x": 2.0, "y": 2.0, "z": 2.0})
    );
    assert_eq!(
        set(&["Color", "-j", r#"{"_type":"Color3","r":0,"g":0,"b":1}"#]),
        set_result("Color")
    );
    assert_eq!(
        get("Color"),
        json!({"_type": "Color3", "b": 1.0, "g": 0.0, "r": 0.0})
    );
    assert_eq!(
        set(&["Material", "--str", "Plastic"]),
        set_result("Material")
    );
    assert_eq!(
        get("Material"),
        json!({"_type": "EnumItem", "name": "Plastic"})
    );
    assert_eq!(
        set(&[
            "Material",
            "-j",
            r#"{"_type":"EnumItem","enumType":"Material","name":"Grass"}"#
        ]),
        set_result("Material")
    );
    assert_eq!(
        get("Material"),
        json!({"_type": "EnumItem", "name": "Grass"})
    );
    assert_eq!(set(&["Tags", "-j", r#"["A","B"]"#]), set_result("Tags"));
    assert_eq!(get("Tags"), json!(["A", "B"]));
    assert_eq!(
        set(&["Name", "--str", "VIP Door"]).get("name"),
        Some(&json!("VIP Door"))
    );
    assert_eq!(
        fx.json(&[
            "bs",
            "-f",
            &store,
            "VIP Door",
            "-p",
            "Transparency",
            "--num",
            "0.75"
        ])
        .ok(),
        set_result("Transparency")
    );
    let typed = fx
        .text(&["bs", "-f", &store, "VIP Door", "-p", "Size", "--str", "big"])
        .err();
    assert!(
        typed.contains("Size needs a Vector3 value, got \"big\""),
        "{typed}"
    );
}

#[test]
fn set_property_resolves_scopes_and_property_names() {
    let fx = Fixture::new("set-scopes");
    let store = fx.store_arg("Workspace");
    let set = |args: &[&str]| fx.text(&[&["bs", "-f", &store, "Door", "-p"], args].concat());
    assert_eq!(
        set(&["Reviewed", "--bool", "false"]).ok(),
        set_result("Reviewed")
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Door", "Reviewed", "-S", "attribute"]),
        json!(false)
    );
    assert!(
        fx.read_store("Workspace").instances[2]
            .properties
            .get("Reviewed")
            .is_none()
    );
    assert_eq!(
        set(&["Score", "-S", "attribute", "--num", "7"]).ok(),
        set_result("Score")
    );
    assert_eq!(fx.both(&["bg", "-f", &store, "Door", "Score"]), json!(7.0));
    assert_eq!(
        set(&["Score", "-S", "attr", "--null"]).ok(),
        set_result("Score")
    );
    assert!(
        fx.text(&["bg", "-f", &store, "Door", "Score"])
            .err()
            .starts_with("Property not found: Score")
    );
    let unknown = set(&["Nope", "--num", "1"]).err();
    assert!(
        unknown.contains("Property \"Nope\" does not exist on Part; use --scope property"),
        "{unknown}"
    );
    assert_eq!(
        set(&["Nope", "-S", "property", "--num", "1"]).ok(),
        set_result("Nope")
    );
    assert_eq!(fx.both(&["bg", "-f", &store, "Door", "Nope"]), json!(1.0));
    let mut cased = set_result("Anchored");
    cased["requestedProperty"] = json!("anchored");
    assert_eq!(set(&["anchored", "--bool", "false"]).ok(), cased);
    assert_eq!(
        fx.read_store("Workspace").instances[2].properties["Anchored"],
        json!(false)
    );
    assert_eq!(
        set(&["ClassName", "--str", "Model"]).err(),
        "ClassName is read-only"
    );
    assert_eq!(
        set(&["Source", "--str", "x"]).err(),
        "Property \"Source\" does not exist on Part; use --scope property only for an unlisted Roblox property"
    );
}

#[test]
fn set_property_reports_value_selector_and_store_errors() {
    let fx = Fixture::new("set-errors");
    let store = fx.store_arg("Workspace");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let err = |args: &[&str]| fx.text(&[&["bs", "-f", &store], args].concat()).err();
    let one_value =
        "Provide exactly one value: --value-json/--json, --str, --num, --bool, or --null";
    assert_eq!(err(&["Door", "-p", "Transparency"]), one_value);
    assert_eq!(
        err(&["Door", "-p", "Transparency", "--num", "1", "--bool", "true"]),
        one_value
    );
    assert_eq!(
        err(&["Door", "-p", "Transparency", "--num", "inf"]),
        "--num must be finite"
    );
    assert!(
        err(&["Door", "-p", "Transparency", "-j", "{bad"])
            .starts_with("Invalid --value-json: {bad")
    );
    assert_eq!(
        err(&["Door", "-p", "Anchored", "--bool", "true", "-S", "bogus"]),
        "Invalid property scope: bogus"
    );
    let once = "Give the target once: as the positional argument, --path or one selector option";
    assert_eq!(
        err(&["Door", "Extra", "-p", "Anchored", "--bool", "true"]),
        once
    );
    assert_eq!(
        err(&["Door", "-i", "door", "-p", "Anchored", "--bool", "true"]),
        once
    );
    assert!(
        err(&["Border", "-p", "Transparency", "--num", "1"])
            .contains("2 instances are named \"Border\"")
    );
    assert!(
        err(&["Nope", "-p", "Transparency", "--num", "1"])
            .contains("No matching instance path: Workspace.Nope")
    );
    assert_eq!(
        err(&["Lobby", "-p", "Name", "--num", "3"]),
        "Name must be a string"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    let missing = fx.store_arg("StarterGui");
    assert!(
        fx.text(&["bs", "-f", &missing, "-p", "Name", "--str", "X"])
            .err()
            .contains(MISSING_STARTER_GUI)
    );
}

#[test]
fn set_property_refuses_an_enum_number_that_is_no_item() {
    let fx = Fixture::new("set-enum-number");
    let store = fx.store_arg("Workspace");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let set = |value: &str| fx.text(&["bs", "-f", &store, "Door", "-p", "Material", "-j", value]);
    assert_eq!(
        set("99999").err(),
        "Material needs an Enum.Material item, got 99999"
    );
    assert_eq!(
        set(r#"{"_type":"EnumItem","value":99999}"#).err(),
        r#"Material needs an Enum.Material item, got {"_type":"EnumItem","value":99999}"#
    );
    let added = fx
        .text(&[
            "ba",
            "-f",
            &store,
            "-n",
            "Slab",
            "-c",
            "Part",
            "-p",
            "Material=99999",
        ])
        .err();
    assert!(
        added.contains("Material needs an Enum.Material item, got 99999"),
        "{added}"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    set("256").ok();
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Door", "Material"]),
        json!({"_type": "EnumItem", "name": "Plastic"})
    );
}

#[test]
fn set_property_stores_a_scalar_in_the_property_type_or_refuses_it() {
    let fx = Fixture::new("set-scalars");
    let store = fx.store_arg("Workspace");
    for (name, class) in [("Sign", "StringValue"), ("Count", "IntValue")] {
        fx.text(&["ba", "-f", &store, "-n", name, "-c", class]).ok();
    }
    let set = |target: &str, args: &[&str]| {
        fx.text(&[&["bs", "-f", &store, target, "-p"], args].concat())
    };
    let stored = |name: &str, property: &str| {
        let document = fx.read_store("Workspace");
        let instance = document.instances.iter().find(|i| i.name == name).unwrap();
        instance.properties[property].clone()
    };
    set("Door", &["Transparency", "--str", "0.25"]).ok();
    assert_eq!(stored("Door", "Transparency"), json!(0.25));
    set("Sign", &["Value=5"]).ok();
    assert_eq!(stored("Sign", "Value"), json!("5"));
    set("Sign", &["Value", "--num", "0.5"]).ok();
    assert_eq!(stored("Sign", "Value"), json!("0.5"));
    set("Count", &["Value", "--num", "3"]).ok();
    assert_eq!(stored("Count", "Value"), json!(3));

    let before = fs::read(fx.store("Workspace")).unwrap();
    assert_eq!(
        set("Door", &["Anchored", "--str", "yes"]).err(),
        "Anchored needs true or false, got \"yes\"; for example `-p Anchored=true`"
    );
    assert_eq!(
        set("Door", &["Transparency", "--str", "half"]).err(),
        "Transparency needs a number, got \"half\"; for example `-p Transparency=0.5`"
    );
    assert_eq!(
        set("Count", &["Value", "--num", "2.5"]).err(),
        "Value needs a whole number, got 2.5; for example `-p Value=5`"
    );
    assert_eq!(
        set("Sign", &["Value", "--bool", "true"]).err(),
        "Value needs a string, got true; for example `-p 'Value=\"text\"'`"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);

    let place = fx.path("typed.rbxl");
    fx.text(&["bep", "-r", &fx.path(""), "-o", &place]).ok();
    let project = fx.path("renium.project.jsonc");
    assert_eq!(
        fx.text(&["cmp", &place, "--full", "--all", "--project", &project])
            .ok()["differences"],
        json!([])
    );
}

#[test]
fn edits_refuse_an_attribute_name_roblox_refuses() {
    let fx = Fixture::new("set-attribute-names");
    let store = fx.store_arg("Workspace");
    let mut document = fx.read_store("Workspace");
    for name in ["has space", "RBX_Seen"] {
        document.instances[2]
            .attributes
            .insert(name.to_string(), json!(1));
    }
    fx.write_store("Workspace", &document);
    let before = fs::read(fx.store("Workspace")).unwrap();
    let set = |scope: &str, name: &str, value: &[&str]| {
        fx.text(
            &[
                &["bs", "-f", &store, "Door", "-S", scope, "-p", name],
                value,
            ]
            .concat(),
        )
    };
    let spaced = "Attribute name \"has space\" holds ' '; Roblox takes letters, digits, `_`, `.`, `-` and `/`";
    let too_long = "a".repeat(101);
    for (name, refusal) in [
        ("has space", spaced),
        ("", "An attribute name cannot be empty"),
        (
            "Café",
            "Attribute name \"Café\" holds 'é'; Roblox takes letters, digits, `_`, `.`, `-` and `/`",
        ),
        (
            "RBX_Seen",
            "Attribute name \"RBX_Seen\" starts with RBX, which Roblox reserves for its own attributes",
        ),
        (
            too_long.as_str(),
            "An attribute name holds at most 100 characters, got 101",
        ),
    ] {
        assert_eq!(set("attribute", name, &["--num", "2"]).err(), refusal);
    }
    assert_eq!(set("auto", "has space", &["--num", "2"]).err(), spaced);
    let batch = fx.root.join("panel-edit.json");
    let panel_edit = |name: &str| {
        fs::write(
            &batch,
            json!([{
                "service": "Workspace",
                "settingsId": "door",
                "className": "Part",
                "pathSegments": ["Workspace", "Lobby", "Door"],
                "pathOrdinals": [1, 1, 1],
                "scope": "attribute",
                "property": name,
                "value": 2,
            }])
            .to_string(),
        )
        .unwrap();
        fx.text(&[
            "bytecode-apply-property-batch",
            "--project-root",
            &fx.path(""),
            "--input",
            &batch.to_string_lossy(),
            "--direction",
            "files-to-studio",
        ])
    };
    assert_eq!(panel_edit("has space").err(), spaced);
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);

    let longest = "a".repeat(100);
    set("attribute", "Stage.Boss-1/hp_max", &["--num", "2"]).ok();
    set("attribute", &longest, &["--num", "2"]).ok();
    assert_eq!(panel_edit("Kind").ok()["applied"], json!(1));
    set("attribute", "has space", &["--null"]).ok();
    let attributes = fx.read_store("Workspace").instances[2].attributes.clone();
    for name in ["Stage.Boss-1/hp_max", longest.as_str(), "Kind", "RBX_Seen"] {
        assert!(attributes.contains_key(name), "{name}");
    }
    assert!(!attributes.contains_key("has space"));
}

#[test]
fn property_batches_store_values_in_the_form_of_the_property() {
    let fx = Fixture::new("property-batch-values");
    let root = fx.path("");
    let batch = fx.path("panel-edit.json");
    let panel_edit = |entries: Value| {
        fs::write(&batch, entries.to_string()).unwrap();
        fx.text(&[
            "bytecode-apply-property-batch",
            "--project-root",
            &root,
            "--input",
            &batch,
            "--direction",
            "files-to-studio",
        ])
    };
    let door = |property: &str, value: Value| {
        json!({
            "service": "Workspace",
            "settingsId": "door",
            "className": "Part",
            "pathSegments": ["Workspace", "Lobby", "Door"],
            "pathOrdinals": [1, 1, 1],
            "scope": "property",
            "property": property,
            "value": value,
        })
    };
    let before = fs::read(fx.store("Workspace")).unwrap();
    assert_eq!(
        panel_edit(json!([
            door("Anchored", json!(false)),
            door("Transparency", json!("half")),
        ]))
        .err(),
        "Transparency needs a number, got \"half\"; for example `-p Transparency=0.5`"
    );
    assert_eq!(
        panel_edit(json!([door("Material", json!("NoSuchMaterial"))])).err(),
        "Material needs an Enum.Material item, got \"NoSuchMaterial\""
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);

    assert_eq!(
        panel_edit(json!([
            door("Transparency", json!("0.25")),
            door("Material", json!("Neon")),
            door("Size", json!({"x": 1, "y": 2, "z": 3})),
            door("anchored", json!(false)),
        ]))
        .ok()["applied"],
        json!(4)
    );
    let stored = fx.read_store("Workspace").instances[2].properties.clone();
    assert_eq!(stored["Transparency"], json!(0.25));
    assert_eq!(
        stored["Material"],
        json!({"_type": "EnumItem", "name": "Neon"})
    );
    assert_eq!(
        stored["Size"],
        json!({"_type": "Vector3", "x": 1.0, "y": 2.0, "z": 3.0})
    );
    assert_eq!(stored["Anchored"], json!(false));
    assert!(!stored.contains_key("anchored"));
    fx.text(&["bep", "-r", &root, "-o", &fx.path("panel.rbxl")])
        .ok();
}

#[test]
fn set_property_refuses_an_item_of_another_enum() {
    let fx = Fixture::new("set-enum-other");
    let store = fx.store_arg("Workspace");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let set = |value: &str| fx.text(&["bs", "-f", &store, "Door", "-p", "Material", "-j", value]);
    for value in [
        r#"{"_type":"EnumItem","enumType":"Enum.Axis","value":0}"#,
        r#"{"_type":"EnumItem","enumType":"Axis","name":"X"}"#,
    ] {
        assert_eq!(
            set(value).err(),
            format!("Material needs an Enum.Material item, got {value}")
        );
    }
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    set(r#"{"_type":"EnumItem","enumType":"Enum.Material","value":256}"#).ok();
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Door", "Material"]),
        json!({"_type": "EnumItem", "name": "Plastic"})
    );
}

#[test]
fn set_property_edits_the_service_root_and_references() {
    let fx = Fixture::new("set-refs");
    let store = fx.store_arg("Workspace");
    assert_eq!(
        fx.text(&["bs", "-f", &store, "-p", "Gravity", "--num", "100"])
            .ok(),
        set_result("Gravity")
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "-p", "Gravity"]),
        json!(100.0)
    );
    let set_value =
        |value: &str| fx.text(&["bs", "-f", &store, "Target", "-p", "Value", "-j", value]);
    assert_eq!(
        set_value(r#"{"_type":"Ref","settingsId":"lobby"}"#).ok(),
        set_result("Value")
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Target", "Value"]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1], "pathSegments": ["Workspace", "Lobby"], "settingsId": "lobby"})
    );
    assert_eq!(
        set_value(r#"{"_type":"Ref","settingsId":"config"}"#).ok(),
        set_result("Value")
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Target", "Value"]),
        json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["ReplicatedStorage", "Shared", "Config"], "settingsId": "config"})
    );
    assert_eq!(
        set_value(r#"{"_type":"Ref","settingsId":"nope"}"#).err(),
        "Reference target was not found: nope"
    );
    fx.write_store(
        "StarterGui",
        &settings_document(vec![
            settings_instance("sg", "StarterGui", "StarterGui", None),
            settings_instance("config", "Config", "Folder", Some(0)),
        ]),
    );
    let ambiguous = set_value(r#"{"_type":"Ref","settingsId":"config"}"#).err();
    assert!(
        ambiguous.contains(
            "Reference target is ambiguous: config exists in ReplicatedStorage, StarterGui"
        ),
        "{ambiguous}"
    );
    assert_eq!(
        fx.text(&["bs", "-f", &store, "Target", "-p", "Value", "--null"])
            .ok(),
        set_result("Value")
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Target", "Value"]),
        Value::Null
    );
}

#[test]
fn set_property_renames_scripts_moves_source_files_and_writes_source() {
    let fx = Fixture::new("set-name-source");
    let store = fx.store_arg("Workspace");
    let renamed = fx
        .text(&[
            "bs", "-f", &store, "Spawner", "-p", "Name", "--str", "Launcher",
        ])
        .ok();
    assert_eq!(
        renamed,
        json!({
            "changedPaths": ["<root>/instances/Workspace.renium", "<root>/src/Workspace/Launcher.server.luau", "<root>/src/Workspace/Spawner.server.luau"],
            "className": "Script",
            "name": "Launcher",
            "ok": true,
            "pathOrdinals": [1, 1],
            "pathSegments": ["Workspace", "Launcher"],
            "property": "Name",
            "settingsFile": "<root>/instances/Workspace.renium",
            "settingsId": "spawner",
        })
    );
    assert!(!fx.exists("src/Workspace/Spawner.server.luau"));
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Launcher.server.luau")).unwrap(),
        SPAWNER_SOURCE
    );
    let source = fx
        .text(&[
            "bs",
            "-f",
            &store,
            "Launcher",
            "-p",
            "Source",
            "--str",
            "print('launch')",
        ])
        .ok();
    assert_eq!(
        source,
        json!({
            "changedPaths": ["<root>/src/Workspace/Launcher.server.luau"],
            "ok": true,
            "property": "Source",
            "settingsFile": "<root>/instances/Workspace.renium",
            "sourcePath": "<root>/src/Workspace/Launcher.server.luau",
        })
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Launcher", "Source"]),
        json!("print('launch')")
    );
    let same = fx
        .json(&[
            "bs",
            "-f",
            &store,
            "Launcher",
            "-p",
            "Source",
            "--str",
            "print('launch')",
        ])
        .ok();
    assert_eq!(same["changedPaths"], json!([]));
    assert_eq!(
        fx.text(&["bs", "-f", &store, "-p", "Name", "--str", "World"])
            .err(),
        "The Workspace service root cannot be renamed"
    );
}

#[test]
fn set_property_reparents_and_rewrites_cross_service_references() {
    let fx = Fixture::new("set-parent");
    let store = fx.store_arg("Workspace");
    let replicated = fx.store_arg("ReplicatedStorage");
    let moved = fx
        .text(&["bs", "-f", &store, "Door", "-p", "Parent", "--str", "cafe"])
        .ok();
    assert_eq!(
        moved,
        json!({
            "changedPaths": ["<root>/instances/ReplicatedStorage.renium", "<root>/instances/Workspace.renium"],
            "className": "Part",
            "name": "Door",
            "ok": true,
            "pathOrdinals": [1, 1, 1],
            "pathSegments": ["Workspace", "Café ☕", "Door"],
            "property": "Parent",
            "settingsFile": "<root>/instances/Workspace.renium",
            "settingsId": "door",
        })
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Door", "Parent"]),
        json!("cafe")
    );
    let moved_ref = json!({"_type": "Ref", "pathOrdinals": [1, 1, 1], "pathSegments": ["Workspace", "Café ☕", "Door"], "settingsId": "door"});
    assert_eq!(
        fx.both(&["bg", "-f", &replicated, "Link", "Value"]),
        moved_ref
    );
    assert_eq!(fx.both(&["bg", "-f", &store, "Target", "Value"]), moved_ref);
    let err = |args: &[&str]| fx.text(&[&["bs", "-f", &store], args].concat()).err();
    assert_eq!(
        err(&["Lobby", "-p", "Parent", "--str", "helper"]),
        "Cannot set Parent to the instance itself or one of its descendants"
    );
    assert_eq!(
        err(&["Lobby", "-p", "Parent", "--str", "nope"]),
        "Parent settings id was not found"
    );
    assert_eq!(
        err(&["-p", "Parent", "--str", "lobby"]),
        "The service root cannot be moved under another instance"
    );
    assert_eq!(
        err(&["Lobby", "-p", "Parent", "--null"]),
        "Only the service root can have no parent"
    );
    assert_eq!(
        err(&["Lobby", "-p", "Parent", "-j", "99"]),
        "Parent index is out of range"
    );
    assert_eq!(
        err(&["Lobby", "-p", "Parent", "--num", "7"]),
        "Parent index is out of range"
    );
    assert_eq!(
        err(&["Lobby", "-p", "Parent", "--bool", "true"]),
        "Parent must be null, an index, or a settings id"
    );
    let by_index = fx
        .text(&["bs", "-f", &store, "Lobby", "-p", "Parent", "-j", "7"])
        .ok();
    assert_eq!(
        by_index["pathSegments"],
        json!(["Workspace", "Café ☕", "Lobby"])
    );
}

#[test]
fn set_property_resolves_a_path_reference_against_the_project_stores() {
    let fx = outside_reference_fixture("set-path-ref");
    let store = fx.store_arg("Workspace");
    let before = fs::read(fx.store("Workspace")).unwrap();
    let set = |path: &str| fx.text(&["bs", "-f", &store, "Target", "-p", "Value", "--str", path]);
    let value = || fx.both(&["bg", "-f", &store, "Target", "Value"]);
    assert_eq!(
        set("ServerScriptService.Tools.Nope").err(),
        "Reference target was not found: ServerScriptService.Tools.Nope is not in the ServerScriptService store; create the target first or add both in one `rbx ba --batch` (`rbx f ServerScriptService NAME` finds a path, `rbx pl` refreshes the store from Studio)"
    );
    assert_eq!(
        set("Workspace.Lobby.Border").err(),
        "Reference target is ambiguous: 2 instances are at Workspace.Lobby.Border; give the ordinal, as in Workspace.Lobby.Border[2]"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    set("game.ServerScriptService.Tools.Util").ok();
    assert_eq!(
        value(),
        outside_reference(
            &["ServerScriptService", "Tools", "Util"],
            &[1, 1, 1],
            "editor:2"
        )
    );
    assert_eq!(
        built_reference_target(&fx, "Workspace.Target").as_deref(),
        Some("ServerScriptService.Tools.Util")
    );
    set("Workspace.Lobby.Border[2]").ok();
    assert_eq!(
        value(),
        outside_reference(&["Workspace", "Lobby", "Border"], &[1, 1, 2], "border2")
    );
    // StarterGui has no store, so a path into it cannot be checked.
    set("StarterGui.Menu.Frame").ok();
    assert_eq!(
        value(),
        json!({"_type": "Ref", "pathSegments": ["StarterGui", "Menu", "Frame"]})
    );
    let add = |name: &str, path: &str| {
        fx.text(&[
            "ba",
            "-f",
            &store,
            "-n",
            name,
            "-c",
            "ObjectValue",
            "-p",
            &format!("Value=\"{path}\""),
        ])
    };
    assert_eq!(
        add("Broken", "ReplicatedStorage.Shared.Nope").err(),
        "Reference target was not found: ReplicatedStorage.Shared.Nope is not in the ReplicatedStorage store; create the target first or add both in one `rbx ba --batch` (`rbx f ReplicatedStorage NAME` finds a path, `rbx pl` refreshes the store from Studio)"
    );
    add("Pointer", "ReplicatedStorage.Shared.Config").ok();
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Pointer", "Value"]),
        outside_reference(
            &["ReplicatedStorage", "Shared", "Config"],
            &[1, 1, 1],
            "config"
        )
    );
}

#[test]
fn references_into_another_service_follow_renames_and_moves_and_clear_on_removal() {
    let fx = outside_reference_fixture("outside-refs");
    let workspace = fx.store_arg("Workspace");
    let server = fx.store_arg("ServerScriptService");
    let both_stores = json!([
        "<root>/instances/ServerScriptService.renium",
        "<root>/instances/Workspace.renium"
    ]);
    let value = || fx.both(&["bg", "-f", &workspace, "Target", "Value"]);
    let edit = |target: &str, property: &str, text: &str| {
        fx.text(&["bs", "-f", &server, target, "-p", property, "--str", text])
            .ok()
    };
    fx.text(&[
        "bs",
        "-f",
        &workspace,
        "Target",
        "-p",
        "Value",
        "--str",
        "ServerScriptService.Tools.Util",
    ])
    .ok();
    assert_eq!(edit("Util", "Name", "Helper")["changedPaths"], both_stores);
    assert_eq!(
        value(),
        outside_reference(
            &["ServerScriptService", "Tools", "Helper"],
            &[1, 1, 1],
            "editor:2"
        )
    );
    edit("Tools", "Name", "Kit");
    assert_eq!(
        value(),
        outside_reference(
            &["ServerScriptService", "Kit", "Helper"],
            &[1, 1, 1],
            "editor:2"
        )
    );
    edit("Helper", "Parent", "sss");
    assert_eq!(
        value(),
        outside_reference(&["ServerScriptService", "Helper"], &[1, 1], "editor:2")
    );
    assert_eq!(
        built_reference_target(&fx, "Workspace.Target").as_deref(),
        Some("ServerScriptService.Helper")
    );
    let compared = fx
        .text(&[
            "cmp",
            &fx.path("references.rbxl"),
            "--full",
            "--project",
            &fx.path("renium.project.jsonc"),
        ])
        .ok();
    assert_eq!(compared["matches"], json!(true), "{compared}");
    // Renaming the Workspace instance that shares the target's id leaves the
    // reference alone, and so it does one that names nothing in its service.
    let dangling = outside_reference(
        &["ReplicatedStorage", "Shared", "Gone"],
        &[1, 1, 1],
        "editor:2",
    );
    let mut with_dangling = fx.read_store("Workspace");
    with_dangling.instances.push(with_values(
        settings_instance("stale", "Stale", "ObjectValue", Some(0)),
        &[("Value", dangling.clone())],
        &[],
    ));
    fx.write_store("Workspace", &with_dangling);
    fx.text(&[
        "bs", "-f", &workspace, "Clash", "-p", "Name", "--str", "Other",
    ])
    .ok();
    assert_eq!(
        value(),
        outside_reference(&["ServerScriptService", "Helper"], &[1, 1], "editor:2")
    );
    assert_eq!(
        fx.both(&["bg", "-f", &workspace, "Stale", "Value"]),
        dangling
    );
    fx.text(&["br", "-f", &workspace, "Stale"]).ok();
    edit("Helper", "Parent", "editor:1");
    let removed = fx.text(&["br", "-f", &server, "Kit"]).ok();
    assert_eq!(removed["changedPaths"], both_stores);
    assert_eq!(value(), json!({"_type": "Ref"}));
    assert_eq!(built_reference_target(&fx, "Workspace.Target"), None);
}

#[test]
fn a_path_only_reference_on_disk_builds_and_follows_its_target() {
    let fx = Fixture::new("stored-path-ref");
    let path_only =
        |name: &str| json!({"_type": "Ref", "pathSegments": ["ReplicatedStorage", "Shared", name]});
    let mut workspace = workspace_document();
    workspace.instances[6]
        .properties
        .insert("Value".to_string(), path_only("Config"));
    fx.write_store("Workspace", &workspace);
    let stored =
        |index: usize| fx.read_store("Workspace").instances[index].properties["Value"].clone();
    assert_eq!(stored(6), path_only("Config"));
    assert_eq!(
        built_reference_target(&fx, "Workspace.Target").as_deref(),
        Some("ReplicatedStorage.Shared.Config")
    );
    workspace.instances.push(with_values(
        settings_instance("stale", "Stale", "ObjectValue", Some(0)),
        &[("Value", path_only("Gone"))],
        &[],
    ));
    fx.write_store("Workspace", &workspace);
    assert_eq!(
        fx.text(&["bep", "-r", &fx.path(""), "-o", &fx.path("stale.rbxl")])
            .err(),
        "Workspace.Stale (ObjectValue) property Value refers to ReplicatedStorage.Shared.Gone, which is not among the exported instances"
    );
    let replicated = fx.store_arg("ReplicatedStorage");
    fx.text(&[
        "bs",
        "-f",
        &replicated,
        "Config",
        "-p",
        "Name",
        "--str",
        "Settings",
    ])
    .ok();
    assert_eq!(
        stored(6),
        outside_reference(
            &["ReplicatedStorage", "Shared", "Settings"],
            &[1, 1, 1],
            "config"
        )
    );
    // A path that names nothing stays as it was written.
    assert_eq!(stored(9), path_only("Gone"));
    fx.text(&["br", "-f", &fx.store_arg("Workspace"), "Stale"])
        .ok();
    assert_eq!(
        built_reference_target(&fx, "Workspace.Target").as_deref(),
        Some("ReplicatedStorage.Shared.Settings")
    );
    workspace.instances.pop();
    workspace.instances[6]
        .properties
        .insert("Value".to_string(), path_only("Settings"));
    fx.write_store("Workspace", &workspace);
    fx.text(&[
        "mv",
        "ReplicatedStorage",
        "-i",
        "config",
        "--to-service",
        "ServerScriptService",
        "-r",
        &fx.path(""),
    ])
    .ok();
    assert_eq!(
        stored(6),
        outside_reference(&["ServerScriptService", "Settings"], &[1, 1], "config")
    );
    workspace.instances[6].properties.insert(
        "Value".to_string(),
        json!({"_type": "Ref", "pathSegments": ["ServerScriptService", "Settings"]}),
    );
    fx.write_store("Workspace", &workspace);
    fx.text(&["br", "-f", &fx.store_arg("ServerScriptService"), "Settings"])
        .ok();
    assert_eq!(stored(6), json!({"_type": "Ref"}));
}

#[test]
fn property_batch_renames_carry_references_from_other_services() {
    let fx = outside_reference_fixture("property-batch-refs");
    let workspace = fx.store_arg("Workspace");
    let mut holders = fx.read_store("Workspace");
    holders.instances[6].properties.insert(
        "Value".to_string(),
        json!({"_type": "Ref", "pathSegments": ["ServerScriptService", "Tools", "Util"]}),
    );
    fx.write_store("Workspace", &holders);
    let batch = fx.root.join("properties.json");
    fs::write(
        &batch,
        json!([{
            "service": "ServerScriptService",
            "settingsId": "editor:1",
            "pathSegments": ["ServerScriptService", "Tools"],
            "scope": "metadata",
            "property": "Name",
            "value": "Kit",
        }])
        .to_string(),
    )
    .unwrap();
    fx.text(&[
        "bytecode-apply-property-batch",
        "--project-root",
        &fx.path(""),
        "--input",
        &batch.to_string_lossy(),
    ])
    .ok();
    assert_eq!(
        fx.both(&["bg", "-f", &workspace, "Target", "Value"]),
        outside_reference(
            &["ServerScriptService", "Kit", "Util"],
            &[1, 1, 1],
            "editor:2"
        )
    );
}

#[test]
fn set_property_applies_several_assignments_to_the_one_selected_instance() {
    let fx = Fixture::new("set-several");
    let store = fx.store_arg("Workspace");
    let set = |args: &[&str]| fx.text(&[&["bs", "-f", &store], args].concat());
    let first = [
        "Lobby.Border[1]",
        "-p",
        "Name=\"First\"",
        "-p",
        "Transparency=0.5",
    ];
    assert_eq!(
        set(&first).ok(),
        json!({
            "changedPaths": ["<root>/instances/Workspace.renium"],
            "ok": true,
            "pathOrdinals": [1, 1, 1],
            "pathSegments": ["Workspace", "Lobby", "First"],
            "set": ["Name", "Transparency"],
            "settingsFile": "<root>/instances/Workspace.renium",
            "settingsId": "border1",
        })
    );
    let instances = fx.read_store("Workspace").instances;
    assert_eq!(instances[3].name, "First");
    assert_eq!(instances[3].properties["Transparency"], json!(0.5));
    assert_eq!(instances[4].name, "Border");
    assert_eq!(instances[4].properties["Transparency"], json!(0.75));
    let launcher = [
        "Spawner",
        "-p",
        "Name=\"Launcher\"",
        "-p",
        "Source=\"print('launch')\"",
    ];
    assert_eq!(
        set(&launcher).ok()["changedPaths"],
        json!([
            "<root>/instances/Workspace.renium",
            "<root>/src/Workspace/Launcher.server.luau",
            "<root>/src/Workspace/Spawner.server.luau"
        ])
    );
    assert!(!fx.exists("src/Workspace/Spawner.server.luau"));
    assert_eq!(
        fs::read_to_string(fx.root.join("src/Workspace/Launcher.server.luau")).unwrap(),
        "print('launch')"
    );
    let before = fs::read(fx.store("Workspace")).unwrap();
    let unknown = set(&["Door", "-p", "Anchored=false", "-p", "Bogus=1"]).err();
    assert!(
        unknown.contains("Property \"Bogus\" does not exist on Part"),
        "{unknown}"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
}

#[test]
fn set_property_where_writes_every_match_or_none() {
    let fx = Fixture::new("set-where");
    let store = fx.store_arg("Workspace");
    let set = |args: &[&str]| fx.text(&[&["bs", "-f", &store, "--where"], args].concat());
    let before = fs::read(fx.store("Workspace")).unwrap();
    let mixed = set(&["Border or Target", "-p", "Anchored=false"]).err();
    assert!(
        mixed.contains("Property \"Anchored\" does not exist on ObjectValue"),
        "{mixed}"
    );
    assert_eq!(fs::read(fx.store("Workspace")).unwrap(), before);
    assert_eq!(
        set(&["Border", "-p", "Anchored=true", "-p", "Transparency=0.1"]).ok(),
        json!({
            "changedPaths": ["<root>/instances/Workspace.renium"],
            "instances": ["Workspace.Lobby.Border", "Workspace.Lobby.Border"],
            "matched": 2,
            "ok": true,
            "scope": "Workspace",
            "set": ["Anchored", "Transparency"],
            "settingsFile": "<root>/instances/Workspace.renium",
            "where": "Border",
        })
    );
    let instances = fx.read_store("Workspace").instances;
    for border in [&instances[3], &instances[4]] {
        assert_eq!(border.properties["Anchored"], json!(true));
        assert_eq!(border.properties["Transparency"], json!(0.1));
    }
    assert_eq!(
        set(&["Border", "-p", "Transparency", "--num", "0.2"]).ok()["set"],
        json!(["Transparency"])
    );
    assert_eq!(
        fx.read_store("Workspace").instances[4].properties["Transparency"],
        json!(0.2)
    );
}

fn source_result(service: &str, settings_id: &str, source_path: &str, changed: bool) -> Value {
    let store = format!("<root>/instances/{service}.renium");
    json!({
        "changedPaths": if changed { json!([source_path]) } else { json!([]) },
        "ok": true,
        "service": service,
        "settingsFile": store,
        "settingsId": settings_id,
        "sourcePath": source_path,
    })
}

#[test]
fn set_source_writes_script_files_from_every_input_form() {
    let fx = Fixture::new("set-source");
    let store = fx.store_arg("Workspace");
    let spawner = "<root>/src/Workspace/Spawner.server.luau";
    assert_eq!(
        fx.text(&["bss", "-f", &store, "-i", "spawner", "--str", "return 1"])
            .ok(),
        source_result("Workspace", "spawner", spawner, true)
    );
    assert_eq!(
        fs::read_to_string(fx.source_path("Workspace", 5)).unwrap(),
        "return 1"
    );
    assert_eq!(
        fx.json(&["bss", "-f", &store, "-i", "spawner", "--str", "return 1"])
            .ok(),
        source_result("Workspace", "spawner", spawner, false)
    );
    let file = fx.root.join("new-source.luau");
    fs::write(&file, "return 'from file'\r\n").unwrap();
    assert_eq!(
        fx.text(&[
            "bss",
            "-f",
            &store,
            "Spawner",
            "--source-file",
            &file.to_string_lossy()
        ])
        .ok(),
        source_result("Workspace", "spawner", spawner, true)
    );
    assert_eq!(
        fx.both(&["bg", "-f", &store, "Spawner", "Source"]),
        json!("return 'from file'\r\n")
    );
    assert_eq!(
        fx.text(&["bss", "-f", &store, "Spawner", "-j", "\"return 2\""])
            .ok(),
        source_result("Workspace", "spawner", spawner, true)
    );
    assert_eq!(
        fx.text(&[
            "bss",
            "-f",
            &store,
            "Lobby.Helper",
            "--str",
            "return 'crlf'\r\n"
        ])
        .ok(),
        source_result(
            "Workspace",
            "helper",
            "<root>/src/Workspace/Lobby/Helper.client.luau",
            true
        )
    );
    assert_eq!(
        fs::read_to_string(fx.source_path("Workspace", 8)).unwrap(),
        "return 'crlf'\r\n"
    );
    let replicated = fx.store_arg("ReplicatedStorage");
    assert_eq!(
        fx.text(&[
            "bss",
            "-f",
            &replicated,
            "Shared.Config",
            "--str",
            "return {}"
        ])
        .ok(),
        source_result(
            "ReplicatedStorage",
            "config",
            "<root>/src/ReplicatedStorage/Shared/Config.luau",
            true
        )
    );
    assert_eq!(
        fx.text(&[
            "bss",
            "-f",
            &store,
            "--path",
            "[\"Workspace\",\"Spawner\"]",
            "--str",
            "return 3"
        ])
        .ok(),
        source_result("Workspace", "spawner", spawner, true)
    );
}

#[test]
fn set_source_reports_bad_inputs_and_targets() {
    let fx = Fixture::new("set-source-errors");
    let store = fx.store_arg("Workspace");
    let err = |args: &[&str]| fx.text(&[&["bss", "-f", &store], args].concat()).err();
    let file = fx.root.join("new-source.luau");
    fs::write(&file, "x").unwrap();
    assert_eq!(
        err(&[
            "Spawner",
            "--source-file",
            &file.to_string_lossy(),
            "--str",
            "x"
        ]),
        "Provide either --source-file or --str/--value-json, not both"
    );
    let one = "Provide exactly one source value: --value-json/--json or --str";
    assert_eq!(err(&["Spawner"]), one);
    assert_eq!(err(&["Spawner", "-j", "\"a\"", "--str", "b"]), one);
    assert!(err(&["Spawner", "-j", "{bad"]).starts_with("Invalid --value-json"));
    assert!(
        err(&["Spawner", "--source-file", &fx.path("nope.luau")])
            .starts_with("Failed to read source file")
    );
    assert_eq!(
        err(&["Door", "--str", "x"]),
        "Part is not a Lua source container"
    );
    assert!(err(&["Nope", "--str", "x"]).contains("No matching instance path: Workspace.Nope"));
    assert!(err(&["Border", "--str", "x"]).contains("2 instances are named \"Border\""));
    assert!(err(&["--str", "x"]).contains("Select the instance: a name or dotted path"));
    let missing = fx.store_arg("StarterGui");
    assert!(
        fx.text(&["bss", "-f", &missing, "-n", "X", "--str", "x"])
            .err()
            .contains(MISSING_STARTER_GUI)
    );
    assert_eq!(
        fs::read_to_string(fx.source_path("Workspace", 5)).unwrap(),
        SPAWNER_SOURCE
    );
}

fn project_node(id: &str, name: &str, class: &str, child_count: usize) -> Value {
    json!({
        "c": class,
        "canonicalSettingsId": id,
        "cc": child_count,
        "hpl": false,
        "id": id,
        "n": name,
        "settingsFile": "<root>/instances/Workspace.renium",
    })
}

#[test]
fn batch_prints_counts_and_searches_in_text_and_json_mode() {
    let fx = Fixture::new("batch-golden");
    let store = fx.store_arg("Workspace");
    let counts = json!({"d": 8, "n": 9, "r": "ws", "rc": 4, "t": "counts"});
    assert_eq!(
        fx.both(&["bb", "-f", &store, "-j", r#"{"ops":[{"type":"counts"}]}"#]),
        json!({"f": "<root>/instances/Workspace.renium", "rs": [counts], "s": "Workspace"})
    );
    assert_eq!(
        fx.both(&["bb", "-f", &store, "-j", r#"{"ops":[{"type":"search","q":"Door","limit":5,"fields":"lookup,prop:Anchored"},{"type":"counts"}]}"#]),
        json!({
            "f": "<root>/instances/Workspace.renium",
            "rs": [
                {
                    "m": ["door"],
                    "ns": [
                        {"c": "Workspace", "id": "ws", "n": "Workspace", "path": "Workspace"},
                        {"c": "Folder", "id": "lobby", "n": "Lobby", "path": "Workspace.Lobby"},
                        {"c": "Part", "id": "door", "n": "Door", "path": "Workspace.Lobby.Door", "props": {"Anchored": true}},
                    ],
                    "r": ["ws"],
                    "t": "search",
                    "tr": false,
                    "v": ["ws", "lobby", "door"],
                },
                counts,
            ],
            "s": "Workspace",
        })
    );
    assert_eq!(
        fx.both(&[
            "bb",
            "-f",
            &store,
            "-o",
            "full",
            "-j",
            r#"[{"type":"counts","rid":"c1"}]"#
        ]),
        json!({
            "results": [{"descendants": 8, "instances": 9, "requestId": "c1", "rootChildren": 4, "rootId": "ws", "type": "counts"}],
            "service": "Workspace",
            "settingsFile": "<root>/instances/Workspace.renium",
        })
    );
    let ops = r#"[{"type":"search","id":"cafe","fields":"lookup","output":"full"}]"#;
    let text = fx.text(&["bb", "-f", &store, "-j", ops]).ok();
    assert_eq!(
        text["rs"][0]["nodes"],
        json!([{"attributes": {}, "className": "Folder", "name": "Café ☕", "path": "Workspace.Café ☕", "properties": {}, "settingsId": "cafe"}])
    );
    let json_mode = fx.json(&["bb", "-f", &store, "-j", ops]).ok();
    assert_eq!(
        json_mode["rs"][0]["nodes"],
        json!([{"attributes": {}, "className": "Folder", "name": "Café ☕", "pathSegments": ["Workspace", "Café ☕"], "properties": {}, "settingsId": "cafe"}])
    );
    assert_eq!(json_mode["rs"][0]["visibleIds"], json!(["cafe"]));
}

#[test]
fn batch_runs_every_op_type_with_every_scope_selector() {
    let fx = Fixture::new("batch-ops");
    let store = fx.store_arg("Workspace");
    let run = |ops: &str| fx.both(&["bb", "-f", &store, "-j", ops])["rs"].clone();
    assert_eq!(
        run(
            r#"[{"type":"counts","id":"lobby"},{"type":"counts","path":"Lobby"},{"type":"counts","index":1},{"type":"counts","name":"Lobby"},{"type":"counts","className":"Folder","name":"Lobby"}]"#
        ),
        json!([
            {"d": 4, "n": 9, "r": "lobby", "rc": 4, "t": "counts"},
            {"d": 4, "n": 9, "r": "lobby", "rc": 4, "t": "counts"},
            {"d": 4, "n": 9, "r": "lobby", "rc": 4, "t": "counts"},
            {"d": 4, "n": 9, "r": "lobby", "rc": 4, "t": "counts"},
            {"d": 4, "n": 9, "r": "lobby", "rc": 4, "t": "counts"},
        ])
    );
    assert_eq!(
        run(r#"[{"type":"counts","id":"nope"}]"#),
        json!([{"found": false, "t": "counts"}])
    );
    let children = run(r#"[{"type":"children","id":"lobby"}]"#);
    assert_eq!(
        children,
        json!([{
            "ch": [
                project_node("door", "Door", "Part", 0),
                project_node("border1", "Border", "Part", 0),
                project_node("border2", "Border", "Part", 0),
                project_node("helper", "Helper", "LocalScript", 0),
            ],
            "p": {"c": "Folder", "canonicalSettingsId": "lobby", "cc": 4, "ch": ["door", "border1", "border2", "helper"], "hpl": false, "id": "lobby", "n": "Lobby", "settingsFile": "<root>/instances/Workspace.renium"},
            "t": "children",
        }])
    );
    assert_eq!(
        run(r#"[{"type":"children","path":["Workspace","Lobby"]}]"#),
        children
    );
    assert_eq!(
        run(r#"[{"type":"children","id":"nope"}]"#),
        json!([{"found": false, "t": "children"}])
    );
    assert_eq!(
        run(r#"[{"type":"search","id":"lobby","limit":2}]"#),
        json!([{
            "m": ["lobby", "door"],
            "ns": [
                {"c": "Folder", "canonicalSettingsId": "lobby", "cc": 4, "ch": ["door"], "hpl": false, "id": "lobby", "n": "Lobby", "settingsFile": "<root>/instances/Workspace.renium"},
                project_node("door", "Door", "Part", 0),
            ],
            "r": ["lobby"],
            "t": "search",
            "tr": true,
            "v": ["lobby", "door"],
        }])
    );
    assert_eq!(
        run(r#"[{"type":"search","path":["Workspace","Lobby"],"q":"Border","fields":"id"}]"#),
        json!([{"m": ["border1", "border2"], "ns": [{"id": "lobby"}, {"id": "border1"}, {"id": "border2"}], "r": ["lobby"], "t": "search", "tr": false, "v": ["lobby", "border1", "border2"]}])
    );
    assert_eq!(
        run(r#"[{"type":"search","id":"nope","q":"Door"}]"#),
        json!([{"found": false, "t": "search"}])
    );
    let instance_ops = r#"[{"type":"instance","id":"door","fields":"props,attrs"}]"#;
    assert_eq!(
        fx.text(&["bb", "-f", &store, "-j", instance_ops]).ok()["rs"],
        json!([{"attrs": door_attrs(false), "props": door_props(), "t": "instance"}])
    );
    assert_eq!(
        fx.json(&["bb", "-f", &store, "-j", instance_ops]).ok()["rs"],
        json!([{"attrs": door_attrs(true), "props": door_props(), "t": "instance"}])
    );
    assert_eq!(
        run(r#"[{"type":"instance","path":"Lobby.Border","ords":[1,1,2],"fields":"lookup"}]"#),
        json!([{"c": "Part", "id": "border2", "n": "Border", "path": "Workspace.Lobby.Border[2]", "t": "instance"}])
    );
    assert_eq!(
        run(r#"[{"type":"instance","id":"nope"}]"#),
        json!([{"found": false, "t": "instance"}])
    );
    assert_eq!(
        run(
            r#"[{"type":"find","name":"Border"},{"type":"find","className":"Part","props":["Transparency=0.75"]},{"type":"find","tag":"Interactive","limit":1},{"type":"find","attrs":["Owner=\"Ada\""],"fields":"id"},{"type":"find","parentId":"lobby","className":"LocalScript","fields":"id"},{"type":"find","name":"Nope"}]"#
        ),
        json!([
            {"m": [{"c": "Part", "cc": 0, "hpl": false, "id": "border1", "n": "Border"}, {"c": "Part", "cc": 0, "hpl": false, "id": "border2", "n": "Border"}], "t": "find", "tr": false},
            {"m": [{"c": "Part", "cc": 0, "hpl": false, "id": "border2", "n": "Border"}], "t": "find", "tr": false},
            {"m": [{"c": "Part", "cc": 0, "hpl": false, "id": "door", "n": "Door"}], "t": "find", "tr": false},
            {"m": [{"id": "door"}], "t": "find", "tr": false},
            {"m": [{"id": "helper"}], "t": "find", "tr": false},
            {"m": [], "t": "find", "tr": false},
        ])
    );
    assert_eq!(
        run(r#"[{"type":"find","className":"Part","limit":2}]"#)[0]["tr"],
        json!(true)
    );
    assert_eq!(
        run(
            r#"[{"type":"sources"},{"type":"sources","id":"lobby"},{"type":"sources","id":"nope"}]"#
        ),
        json!([
            {"found": true, "sourcePaths": ["<root>/src/Workspace/Lobby/Helper.client.luau", "<root>/src/Workspace/Spawner.server.luau"], "t": "sources"},
            {"found": true, "sourcePaths": ["<root>/src/Workspace/Lobby/Helper.client.luau"], "t": "sources"},
            {"found": false, "sourcePaths": [], "t": "sources"},
        ])
    );
    assert_eq!(
        run(r#"[{"type":"service","fields":["id","n"]}]"#),
        json!([{
            "ns": [
                {"id": "ws", "n": "Workspace"}, {"id": "lobby", "n": "Lobby"}, {"id": "door", "n": "Door"},
                {"id": "border1", "n": "Border"}, {"id": "border2", "n": "Border"}, {"id": "spawner", "n": "Spawner"},
                {"id": "target", "n": "Target"}, {"id": "cafe", "n": "Café ☕"}, {"id": "helper", "n": "Helper"},
            ],
            "r": ["ws"],
            "t": "service",
        }])
    );
    assert_eq!(
        run(r#"[{"type":"search","q":"Spawner","fields":"lookup,prop:Source,src"}]"#)[0]["ns"][1],
        json!({"c": "Script", "id": "spawner", "n": "Spawner", "path": "Workspace.Spawner", "props": {"Source": SPAWNER_SOURCE}, "src": "<root>/src/Workspace/Spawner.server.luau"})
    );
    assert_eq!(
        run(r#"[{"op":"bq","q":"is:LocalScript","fields":"id"},{"kind":"bc","id":"cafe"}]"#),
        json!([
            {"m": ["helper"], "ns": [{"id": "ws"}, {"id": "lobby"}, {"id": "helper"}], "r": ["ws"], "t": "search", "tr": false, "v": ["ws", "lobby", "helper"]},
            {"d": 0, "n": 9, "r": "cafe", "rc": 0, "t": "counts"},
        ])
    );
}

#[test]
fn batch_reads_ops_from_every_input_and_reports_bad_requests() {
    let fx = Fixture::new("batch-inputs");
    let store = fx.store_arg("Workspace");
    let counts = json!({"f": "<root>/instances/Workspace.renium", "rs": [{"d": 8, "n": 9, "r": "ws", "rc": 4, "t": "counts"}], "s": "Workspace"});
    let ops_file = fx.root.join("ops.json");
    fs::write(&ops_file, "\u{feff}{\"ops\":[{\"type\":\"counts\"}]}").unwrap();
    assert_eq!(
        fx.both(&["bb", "-f", &store, "-J", &ops_file.to_string_lossy()]),
        counts
    );
    assert_eq!(
        fx.both(&["bb", "-f", &store, "-J", r#"[{"type":"counts"}]"#]),
        counts
    );
    assert_eq!(
        fx.both(&["bb", "-f", &store, "--ops", r#"[{"type":"counts"}]"#]),
        counts
    );
    let path_form = fx.path("instances/Workspace.renium");
    assert_eq!(
        fx.both(&["bb", &path_form, "-j", r#"[{"type":"counts"}]"#]),
        counts
    );
    let err = |args: &[&str]| fx.text(&[&["bb", "-f", &store], args].concat()).err();
    assert_eq!(
        err(&["-j", r#"[{"type":"bogus"}]"#]),
        "Unsupported batch op type: bogus. Use counts, sources, children, service, search, instance, or find."
    );
    assert_eq!(err(&["-j", "[]"]), "Batch request is empty");
    assert!(
        err(&["-j", r#"{"ops":[{"type":"counts","extra":1}]}"#]).contains("unknown field `extra`")
    );
    assert!(
        err(&["-j", "{bad"])
            .starts_with("Invalid batch ops JSON; expected {\"ops\":[...]} or [...]")
    );
    assert!(
        err(&["-j", r#"[{"type":"instance"}]"#])
            .contains("Select the instance: id, path, index, name or className")
    );
    assert!(
        err(&["-j", r#"[{"type":"instance","name":"Border"}]"#])
            .contains("Ambiguous selector name \"Border\"")
    );
    assert_eq!(
        err(&["-j", r#"[{"type":"search","id":"lobby","path":"Lobby"}]"#]),
        "pathSegments cannot be combined with another selector"
    );
    assert!(
        err(&["-j", r#"[{"type":"counts","path":7}]"#])
            .contains("path must be a string or an array of string segments")
    );
    assert!(
        err(&["-j", r#"[{"type":"counts","output":"bogus"}]"#])
            .contains("Invalid output mode: bogus")
    );
    assert_eq!(err(&[]), "Provide -j/--ops, --ops-json, or -J/--ops-file");
    assert!(err(&["-J", &fx.path("nope.json")]).starts_with("Failed to read"));
    assert!(
        fx.text(&["bb", "-f", &store, "-j", "[]", "-J", "[]"])
            .err()
            .contains("cannot be used with")
    );
    let missing = fx.store_arg("StarterGui");
    assert!(
        fx.text(&["bb", "-f", &missing, "-j", r#"[{"type":"counts"}]"#])
            .err()
            .contains(MISSING_STARTER_GUI)
    );
}

#[test]
fn batch_resolves_project_services_and_synthesizes_unsynced_ones() {
    let fx = Fixture::new("batch-project");
    let root = fx.path("");
    assert_eq!(
        fx.both(&[
            "bb",
            "--project-root",
            &root,
            "-s",
            "Workspace",
            "-j",
            r#"[{"type":"counts"},{"type":"instance","id":"door","fields":"f,canonical"}]"#
        ]),
        json!({
            "f": "<root>/instances/Workspace.renium",
            "rs": [
                {"d": 8, "n": 9, "r": "ws", "rc": 4, "t": "counts"},
                {"canonicalSettingsId": "door", "settingsFile": "<root>/instances/Workspace.renium", "t": "instance"},
            ],
            "s": "Workspace",
        })
    );
    assert_eq!(
        fx.both(&[
            "bb",
            "--project-root",
            &root,
            "-s",
            "StarterGui",
            "-j",
            r#"[{"type":"counts"}]"#
        ]),
        json!({"f": "<root>/instances/StarterGui.renium", "rs": [{"d": 0, "n": 1, "r": "service:StarterGui", "rc": 0, "t": "counts"}], "s": "StarterGui"})
    );
    assert_eq!(
        fx.text(&[
            "bb",
            "--project-root",
            &root,
            "-j",
            r#"[{"type":"counts"}]"#
        ])
        .err(),
        "--project-root requires --service"
    );
    let store = fx.store_arg("Workspace");
    assert_eq!(
        fx.text(&[
            "bb",
            "--project-root",
            &root,
            "-s",
            "Workspace",
            "-f",
            &store,
            "-j",
            r#"[{"type":"counts"}]"#
        ])
        .err(),
        "--project-root cannot be combined with a settings file"
    );
    let nested = fx
        .text(&[
            "bb",
            "--project-root",
            &fx.path("nowhere"),
            "-s",
            "Workspace",
            "-j",
            r#"[{"type":"counts"}]"#,
        ])
        .ok();
    assert_eq!(nested["f"], json!("<root>/instances/Workspace.renium"));
    assert_eq!(nested["rs"][0]["n"], json!(9));
    let outside = temp_dir("offline-batch-no-project");
    let synthesized = run_at(
        &outside,
        CaptureMode::Text,
        &[
            "bb",
            "--project-root",
            &outside.to_string_lossy(),
            "-s",
            "Workspace",
            "-j",
            r#"[{"type":"counts"}]"#,
        ],
    )
    .ok();
    assert_eq!(synthesized["rs"][0]["r"], json!("service:Workspace"));
    assert_eq!(synthesized["rs"][0]["n"], json!(1));
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn set_property_refuses_the_workspace_viewport_camera() {
    let fx = Fixture::new("viewport-camera");
    let mut document = workspace_document();
    document
        .instances
        .push(settings_instance("viewport", "Camera", "Camera", Some(0)));
    document
        .instances
        .push(settings_instance("rig", "Camera", "Camera", Some(1)));
    fx.write_store("Workspace", &document);
    let store = fx.store_arg("Workspace");
    let refused = "Camera is the Workspace viewport camera, which Studio owns; Renium does not sync it, so its properties cannot be set in the store";
    assert_eq!(
        fx.text(&[
            "bs",
            "-f",
            &store,
            "-i",
            "viewport",
            "-p",
            "FieldOfView",
            "--num",
            "80"
        ])
        .err(),
        refused
    );
    fx.text(&[
        "bs",
        "-f",
        &store,
        "-i",
        "rig",
        "-p",
        "FieldOfView",
        "--num",
        "80",
    ])
    .ok();
    assert_eq!(
        fx.json(&["bg", "-f", &store, "-i", "rig", "FieldOfView"])
            .ok(),
        json!(80.0)
    );
    document.instances[0].properties.insert(
        "CurrentCamera".into(),
        json!({"_type": "Ref", "settingsId": "viewport"}),
    );
    fx.write_store("Workspace", &document);
    assert_eq!(
        fx.text(&[
            "bs",
            "-f",
            &store,
            "-i",
            "viewport",
            "-p",
            "FieldOfView",
            "--num",
            "80"
        ])
        .err(),
        refused
    );
}
