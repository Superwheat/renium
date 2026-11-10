use super::*;

const STRUCTURED_FAILURE: &str = "command reported a structured failure";

#[test]
fn check_luau_parses_files_folders_and_reports_failures_in_both_modes() {
    let fx = Fixture::new("check");
    let valid = fx.path("valid.luau");
    fs::write(&valid, "local n: number = 1\nreturn n\n").unwrap();
    let invalid = fx.path("invalid.lua");
    fs::write(&invalid, "local n =").unwrap();
    assert_eq!(
        fx.text(&["ck", &valid]).ok(),
        json!({"checked": 1, "ok": true})
    );
    assert_eq!(
        fx.json(&["ck", &valid]).ok(),
        json!({"files": [{"file": "<root>/valid.luau", "ok": true}], "ok": true})
    );
    let failing = fx.text(&["ck", &valid, &invalid]);
    let syntax_error = "Invalid Luau syntax: syntax error: 1: Expected identifier when parsing expression, got <eof>";
    assert_eq!(
        failing.value,
        json!({"checked": 2, "files": [{"error": syntax_error, "file": "<root>/invalid.lua", "ok": false}], "ok": false})
    );
    assert_eq!(failing.error.as_deref(), Some(STRUCTURED_FAILURE));
    let failing_json = fx.json(&["ck", &valid, &invalid]);
    assert_eq!(
        failing_json.value,
        json!({"files": [{"file": "<root>/valid.luau", "ok": true}, {"error": syntax_error, "file": "<root>/invalid.lua", "ok": false}], "ok": false})
    );
    assert_eq!(failing_json.error.as_deref(), Some(STRUCTURED_FAILURE));
    let crlf = fx.path("crlf.luau");
    fs::write(&crlf, "local a = 1\r\nreturn a\r\n").unwrap();
    assert_eq!(
        fx.text(&["ck", &crlf]).ok(),
        json!({"checked": 1, "ok": true})
    );
    let bom = fx.path("bom.luau");
    fs::write(&bom, "\u{feff}return 1\n").unwrap();
    assert_eq!(
        fx.text(&["ck", &bom]).value["files"][0]["error"],
        json!(
            "The source starts with a UTF-8 byte order mark; strip it (the rest was not checked)"
        )
    );
    let binary = fx.path("binary.luau");
    fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
    assert_eq!(
        fx.text(&["ck", &binary]).value["files"][0]["error"],
        json!("Cannot read <root>/binary.luau as UTF-8 text: stream did not contain valid UTF-8")
    );
    let missing = fx.text(&["ck", &fx.path("nope.luau")]);
    assert!(
        missing.value["files"][0]["error"]
            .as_str()
            .unwrap()
            .starts_with("Cannot read <root>/nope.luau as UTF-8 text")
    );
    assert_eq!(missing.error.as_deref(), Some(STRUCTURED_FAILURE));
    assert_eq!(
        fx.text(&["ck", &fx.path("src")]).ok(),
        json!({"checked": 3, "ok": true})
    );
    assert_eq!(
        fx.json(&["ck", &fx.path("src")]).ok()["files"],
        json!([
            {"file": "<root>/src/ReplicatedStorage/Shared/Config.luau", "ok": true},
            {"file": "<root>/src/Workspace/Lobby/Helper.client.luau", "ok": true},
            {"file": "<root>/src/Workspace/Spawner.server.luau", "ok": true},
        ])
    );
    fs::create_dir_all(fx.path("empty")).unwrap();
    assert_eq!(
        fx.text(&["ck", &fx.path("empty")]).err(),
        "No .luau or .lua files under <root>/empty"
    );
    let notes = fx.path("notes.txt");
    fs::write(&notes, "not code").unwrap();
    assert_eq!(
        fx.text(&["ck", &notes]).value["files"][0]["ok"],
        json!(false)
    );
    assert_eq!(
        fx.text(&["ck", "--project", &fx.path("renium.project.jsonc")])
            .err(),
        "<root> is not in a git repository; pass the files or folders to check"
    );
    assert_eq!(
        fx.text(&["ck", "-", "-"]).err(),
        "Use - only once to read source from stdin"
    );
}

fn mapped_project(fx: &Fixture) -> String {
    for dir in [
        "mapped/src/server",
        "mapped/shared",
        "mapped/src/generated",
        "mapped/src/Workspace",
    ] {
        fs::create_dir_all(fx.path(dir)).unwrap();
    }
    fs::write(
        fx.path("mapped/src/server/Main.server.luau"),
        "print('main')\n",
    )
    .unwrap();
    fs::write(fx.path("mapped/shared/Util.luau"), "return 1\n").unwrap();
    fs::write(fx.path("mapped/src/generated/Gen.luau"), "return 2\n").unwrap();
    fs::write(
        fx.path("mapped/src/Workspace/Draft.server.txt"),
        "print('draft')\n",
    )
    .unwrap();
    fs::write(
        fx.path("mapped/renium.project.jsonc"),
        r#"{
  "schemaVersion": 1,
  "tree": {
    "ServerScriptService": { "$path": "src/server" }
  },
  "mounts": [
    {"source":"shared","target":"ReplicatedStorage.Shared","ownership":"read-only","optional":true}
  ],
  "syncRules": [
    { "pattern": "**/*.server.txt", "use": "serverScript", "suffix": ".server.txt" }
  ],
  "globIgnorePaths": ["src/generated/**"],
  "filters": [
    { "action": "ignore", "direction": "files-to-studio", "class": "ModuleScript" }
  ]
}
"#,
    )
    .unwrap();
    fx.path("mapped/renium.project.jsonc")
}

#[test]
fn a_fixed_wait_sleeps_in_the_command_and_needs_no_studio() {
    let fx = Fixture::new("fixed-wait");
    let started = std::time::Instant::now();
    let waited = fx.json(&["wait", "150ms"]).ok();
    assert!(started.elapsed() >= std::time::Duration::from_millis(150));
    assert_eq!(waited["ok"], json!(true));
    assert_eq!(waited["action"], json!("wait"));
    assert!(waited["waitedSeconds"].as_f64().unwrap() >= 0.15);
    let text = fx.text(&["wait", "0.05"]).ok();
    assert_eq!(text["ok"], json!(true));
    assert!(text.get("action").is_none());
}

#[test]
fn explain_path_describes_plain_mapped_mounted_ignored_and_rule_owned_paths() {
    let fx = Fixture::new("explain");
    let project = fx.path("renium.project.jsonc");
    let spawner = json!({
        "filterCandidates": [{"filesToStudio": "include", "id": "spawner", "path": "Workspace/Spawner", "studioToFiles": "include"}],
        "ignored": false,
        "ok": true,
        "owned": false,
        "path": "src/Workspace/Spawner.server.luau",
        "project": "<root>/renium.project.jsonc",
        "stagedPaths": ["<root>/instances/Workspace.renium", "Workspace/Spawner.server.luau"],
    });
    assert_eq!(
        fx.both(&[
            "xp",
            "src/Workspace/Spawner.server.luau",
            "--project",
            &project
        ]),
        spawner
    );
    assert_eq!(
        fx.both(&[
            "xp",
            &fx.path("src/Workspace/Spawner.server.luau"),
            "--project",
            &project
        ]),
        spawner
    );
    assert_eq!(
        fx.both(&[
            "xp",
            "src/Workspace/Lobby/Helper.client.luau",
            "--project",
            &project
        ])["filterCandidates"],
        json!([{"filesToStudio": "include", "id": "helper", "path": "Workspace/Lobby/Helper", "studioToFiles": "include"}])
    );
    assert_eq!(
        fx.both(&["xp", "src/Workspace/Nope.luau", "--project", &project]),
        json!({"ignored": false, "ok": true, "owned": false, "path": "src/Workspace/Nope.luau", "project": "<root>/renium.project.jsonc", "stagedPaths": ["<root>/instances/Workspace.renium", "Workspace/Nope.luau"]})
    );
    assert_eq!(
        fx.both(&["xp", &fx.path("../elsewhere.luau"), "--project", &project]),
        json!({"ignored": false, "ok": true, "owned": false, "path": "../elsewhere.luau", "project": "<root>/renium.project.jsonc"})
    );
    let mapped = mapped_project(&fx);
    let tree = fx.both(&["xp", "src/server/Main.server.luau", "--project", &mapped]);
    assert_eq!(
        tree["matches"],
        json!([{"kind": "tree", "source": "src/server", "target": "ServerScriptService"}])
    );
    assert_eq!(tree["owned"], json!(true));
    assert_eq!(tree["ignored"], json!(false));
    assert_eq!(
        tree["stagedPaths"],
        json!([
            "ServerScriptService/Main.server.luau",
            "ServerScriptService/__roblox_sync_settings.renium"
        ])
    );
    assert_eq!(
        tree["filterCandidates"][0]["path"],
        json!("ServerScriptService/Main")
    );
    let mount = fx.both(&["xp", "shared/Util.luau", "--project", &mapped]);
    assert_eq!(
        mount["matches"],
        json!([{"kind": "mount", "ownership": "read-only", "source": "shared", "target": "ReplicatedStorage.Shared"}])
    );
    assert_eq!(mount["owned"], json!(true));
    assert_eq!(
        mount["filterCandidates"][0]["filesToStudio"],
        json!("ignore")
    );
    assert_eq!(
        mount["filters"],
        json!([{"action": "ignore", "candidatePath": "ReplicatedStorage/Shared/Util", "class": "ModuleScript", "direction": "files-to-studio", "index": 0}])
    );
    let ignored = fx.both(&["xp", "src/generated/Gen.luau", "--project", &mapped]);
    assert_eq!(ignored["ignored"], json!(true));
    assert_eq!(ignored["owned"], json!(false));
    let rule = fx.both(&["xp", "src/Workspace/Draft.server.txt", "--project", &mapped]);
    assert_eq!(
        rule["matches"],
        json!([{"excluded": false, "index": 0, "kind": "syncRule", "pattern": "**/*.server.txt", "selected": true, "suffix": ".server.txt", "targetName": "Draft", "use": "serverScript"}])
    );
    assert_eq!(rule["selectedSyncRule"], json!(0));
    assert_eq!(rule["owned"], json!(true));
    assert_eq!(
        rule["filterCandidates"][0]["path"],
        json!("Workspace/Draft")
    );
    fs::create_dir_all(fx.path("broken")).unwrap();
    fs::write(fx.path("broken/renium.project.jsonc"), "{ not json").unwrap();
    assert!(
        fx.text(&[
            "xp",
            "x.luau",
            "--project",
            &fx.path("broken/renium.project.jsonc")
        ])
        .err()
        .starts_with("Invalid JSONC in <root>/broken/renium.project.jsonc")
    );
    assert_eq!(
        fx.text(&["xp", "x.luau", "--project", &fx.path("nope.jsonc")])
            .err(),
        "Project file does not exist: <root>/nope.jsonc"
    );
}

#[test]
fn validate_project_prints_a_line_in_text_mode_and_counts_in_json_mode() {
    let fx = Fixture::new("validate");
    let project = fx.path("renium.project.jsonc");
    let text = fx.text(&["pv", "--project", &project]);
    assert_eq!(text.error, None);
    assert_eq!(text.value, Value::Null);
    assert_eq!(text.stdout, "Validated <root>/renium.project.jsonc\n");
    let json_mode = fx.json(&["pv", "--project", &project]);
    assert_eq!(json_mode.stdout, "");
    assert_eq!(
        json_mode.value,
        json!({"adapters": 0, "entries": 0, "mounts": 0, "ok": true, "project": "<root>/renium.project.jsonc"})
    );
    let mapped = mapped_project(&fx);
    assert_eq!(
        fx.json(&["pv", "--project", &mapped]).ok(),
        json!({"adapters": 0, "entries": 4, "mounts": 1, "ok": true, "project": "<root>/mapped/renium.project.jsonc"})
    );
    assert_eq!(
        fx.json(&["pv", "--project", &fx.path("mapped")]).ok()["project"],
        json!("<root>/mapped/renium.project.jsonc")
    );
    fs::create_dir_all(fx.path("broken")).unwrap();
    fs::write(fx.path("broken/renium.project.jsonc"), "{ not json").unwrap();
    assert!(
        fx.text(&["pv", "--project", &fx.path("broken/renium.project.jsonc")])
            .err()
            .starts_with("Invalid JSONC")
    );
    fs::write(
        fx.path("broken/renium.project.jsonc"),
        r#"{"schemaVersion":1,"mounts":[{"source":"../outside","target":"ReplicatedStorage.X"}]}"#,
    )
    .unwrap();
    let escaped = fx
        .text(&["pv", "--project", &fx.path("broken/renium.project.jsonc")])
        .err();
    assert!(
        escaped.contains("outside") || escaped.contains("escape"),
        "{escaped}"
    );
    assert_eq!(
        fx.text(&["pv", "--project", &fx.path("nope.jsonc")]).err(),
        "Project file does not exist: <root>/nope.jsonc"
    );
}

#[test]
fn fmt_keeps_comments_and_the_values_after_them_at_their_depth() {
    let fx = Fixture::new("fmt-comments");
    let project = fx.path("renium.project.jsonc");
    let formatted = "// lead\n{\n  /* block */\n  \"schemaVersion\": 1 // inline\n}\n";
    fs::write(&project, formatted).unwrap();
    fx.text(&["fmt", &project, "--check"]).ok();
    for unformatted in [
        " // lead\n{\n  /* block */\n   \"schemaVersion\": 1 // inline\n}\n",
        "// lead\n{ /* block */ \"schemaVersion\": 1// inline\n}",
    ] {
        fs::write(&project, unformatted).unwrap();
        assert_eq!(
            fx.text(&["fmt", &project, "--check"]).err(),
            "<root>/renium.project.jsonc is not formatted"
        );
        fx.text(&["fmt", &project]).ok();
        assert_eq!(fs::read_to_string(&project).unwrap(), formatted);
    }
}

/// Windows PowerShell 5.1 writes a byte order mark with
/// `Set-Content -Encoding utf8`.
#[test]
fn project_file_with_a_byte_order_mark_loads_and_formats_without_it() {
    let fx = Fixture::new("project-bom");
    let project = fx.path("renium.project.jsonc");
    fs::write(
        &project,
        "\u{feff}{\n  // rules\n  \"schemaVersion\": 1\n}\n",
    )
    .unwrap();
    let validated = fx.text(&["pv", "--project", &project]);
    assert_eq!(validated.error, None);
    assert_eq!(validated.stdout, "Validated <root>/renium.project.jsonc\n");
    assert_eq!(
        fx.both(&["tr", "Workspace", "--depth", "1", "-r", &fx.path("")])[0]["id"],
        json!("ws")
    );
    assert_eq!(
        fx.text(&["fmt", "--check", &project]).err(),
        "<root>/renium.project.jsonc is not formatted"
    );
    assert_eq!(fx.text(&["fmt", &project]).error, None);
    assert_eq!(
        fs::read_to_string(&project).unwrap(),
        "{\n  // rules\n  \"schemaVersion\": 1\n}\n"
    );
    fs::write(&project, "\u{feff}{\"schemaVersion\":1}").unwrap();
    assert_eq!(fx.text(&["fmt", &project]).error, None);
    assert_eq!(
        fs::read_to_string(&project).unwrap(),
        "{\n  \"schemaVersion\": 1\n}\n"
    );
}

/// An empty project folder, for projects a test writes itself.
fn empty_folder(tag: &str) -> Fixture {
    Fixture {
        root: temp_dir(&format!("offline-{tag}")),
    }
}

fn write_files(fx: &Fixture, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = fx.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// Builds the project into a place and returns the one instance named `name`
/// with the properties asked for.
fn built_instance(fx: &Fixture, name: &str, props: &str) -> Value {
    let place = fx.path("built.rbxl");
    fx.text(&[
        "build",
        "--project",
        &fx.path("renium.project.jsonc"),
        "-o",
        &place,
    ])
    .ok();
    let found = fx.json(&["q", &place, "-n", name, "--props", props]).ok();
    let matches = found["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{found}");
    matches[0].clone()
}

/// The place project `rojo init` writes.
const ROJO_PLACE_PROJECT: &str = r#"{
  "name": "stock",
  "tree": {
    "$className": "DataModel",
    "ReplicatedStorage": {
      "Shared": { "$path": "src/shared" }
    },
    "ServerScriptService": {
      "Server": { "$path": "src/server" }
    },
    "StarterPlayer": {
      "StarterPlayerScripts": {
        "Client": { "$path": "src/client" }
      }
    },
    "Workspace": {
      "$properties": { "FilteringEnabled": true },
      "Baseplate": {
        "$className": "Part",
        "$properties": {
          "Anchored": true,
          "Color": [0.38823, 0.37254, 0.38823],
          "Locked": true,
          "Position": [0, -10, 0],
          "Size": [512, 20, 512]
        }
      }
    },
    "Lighting": {
      "$properties": {
        "Ambient": [0, 0, 0],
        "Brightness": 2,
        "GlobalShadows": true,
        "Outlines": false,
        "Technology": "Voxel"
      }
    },
    "SoundService": {
      "$properties": { "RespectFilteringEnabled": true }
    }
  }
}
"#;

#[test]
fn import_rojo_and_init_convert_the_stock_rojo_place_project() {
    let fx = empty_folder("rojo-stock");
    write_files(
        &fx,
        &[
            ("default.project.json", ROJO_PLACE_PROJECT),
            ("src/server/init.server.luau", "print('server')\n"),
            ("src/client/init.client.luau", "print('client')\n"),
            ("src/shared/Hello.luau", "return {}\n"),
        ],
    );
    let preview = fx.text(&["ir", &fx.path(""), "--preview"]);
    assert_eq!(preview.error, None);
    let converted: Value = serde_json::from_str(&preview.stdout).unwrap();
    assert_eq!(
        converted["tree"]["Lighting"]["$properties"],
        json!({
            "Ambient": {"_type": "Color3", "r": 0.0, "g": 0.0, "b": 0.0},
            "Brightness": 2.0,
            "GlobalShadows": true,
            "Outlines": false,
            "Technology": {"_type": "EnumItem", "enumType": "Enum.Technology", "name": "Voxel", "value": 1},
        })
    );
    assert_eq!(converted["tree"]["Lighting"].get("$className"), None);
    fx.text(&["init", &fx.path("")]).ok();
    assert_eq!(
        built_instance(&fx, "Baseplate", "Anchored,CFrame,Size")["props"],
        json!({
            "Anchored": true,
            "CFrame": {"_type": "CFrame", "components": [0.0, -10.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]},
            "Size": {"_type": "Vector3", "x": 512.0, "y": 20.0, "z": 512.0},
        })
    );
    assert_eq!(
        built_instance(&fx, "Lighting", "Ambient,Technology")["props"],
        json!({
            "Ambient": {"_type": "Color3", "r": 0.0, "g": 0.0, "b": 0.0},
            "Technology": "Enum.Technology.Voxel",
        })
    );
    let server = built_instance(&fx, "Server", "Source");
    assert_eq!(server["className"], json!("Script"));
    assert_eq!(server["props"], json!({"Source": "print('server')\n"}));
}

#[test]
fn build_keeps_an_init_script_folder_that_also_holds_an_adapter_file() {
    let fx = empty_folder("build-init-adapter");
    write_files(
        &fx,
        &[
            (
                "default.project.json",
                r#"{"name": "p", "tree": {"$className": "DataModel", "ReplicatedStorage": {
                    "Config": {"$className": "Folder"},
                    "Shared": {"$path": "src/shared"}
                }}}"#,
            ),
            ("src/shared/init.luau", "return {}\n"),
            ("src/shared/Note.txt", "hello"),
        ],
    );
    fx.text(&["ir", &fx.path(""), "--apply"]).ok();
    let shared = built_instance(&fx, "Shared", "Source");
    assert_eq!(shared["className"], json!("ModuleScript"));
    assert_eq!(shared["props"], json!({"Source": "return {}\n"}));
    let note = built_instance(&fx, "Note", "Value");
    assert_eq!(note["className"], json!("StringValue"));
    assert_eq!(note["path"], json!(["ReplicatedStorage", "Shared", "Note"]));
    assert_eq!(note["props"], json!({"Value": "hello"}));
}

/// A Rojo project whose ReplicatedStorage node is `children`.
fn rojo_replicated_storage(children: &str) -> String {
    format!(
        r#"{{"name": "p", "tree": {{"$className": "DataModel", "ReplicatedStorage": {children}}}}}"#
    )
}

/// Builds the project, converting `default.project.json` first when the
/// folder holds one, and returns ReplicatedStorage.Shared as `rbx v` shows
/// it in the built place.
fn built_shared(fx: &Fixture) -> Value {
    if fx.exists("default.project.json") {
        fx.text(&["ir", &fx.path(""), "--apply"]).ok();
    }
    let place = fx.path("built.rbxl");
    fx.text(&[
        "build",
        "--project",
        &fx.path("renium.project.jsonc"),
        "-o",
        &place,
    ])
    .ok();
    let view = fx.json(&["v", &place, "--json"]).ok();
    let named = |nodes: &Value, name: &str| {
        nodes
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} in {nodes}"))
    };
    named(
        &named(&view["roots"], "ReplicatedStorage")["children"],
        "Shared",
    )
}

const INIT_SOURCE: &str = "return {}\n";
const EARLIER_NODE: &str = r#""Config": {"$className": "Folder"}, "#;

/// Each shape runs alone and after an earlier tree node, which writes the
/// service's document before the directory is staged.
#[test]
fn build_keeps_the_init_script_of_a_directory_however_the_project_reaches_it() {
    let shapes = [
        (
            "tree node with fields",
            r#""Shared": {"$path": "src/shared", "$attributes": {"X": 1}}"#,
            "src/shared/init.luau",
        ),
        (
            "tree node with fields and an adapter file",
            r#""Shared": {"$path": "src/shared", "$attributes": {"X": 1}}"#,
            "src/shared/init.luau",
        ),
        (
            "model file in the directory",
            r#""Shared": {"$path": "src/shared"}"#,
            "src/shared/init.luau",
        ),
        (
            "nested project",
            r#""Shared": {"$path": "lib/default.project.json"}"#,
            "lib/src/init.luau",
        ),
    ];
    for (shape, node, init) in shapes {
        for earlier in ["", EARLIER_NODE] {
            let fx = empty_folder("build-init-shapes");
            let project = rojo_replicated_storage(&format!("{{{earlier}{node}}}"));
            write_files(
                &fx,
                &[
                    ("default.project.json", project.as_str()),
                    (init, INIT_SOURCE),
                ],
            );
            match shape {
                "tree node with fields and an adapter file" => {
                    write_files(&fx, &[("src/shared/Note.txt", "hello")]);
                }
                "model file in the directory" => {
                    write_model_file(&fx.root.join("src/shared/Kit.rbxm"), &sample_model_dom());
                }
                "nested project" => write_files(
                    &fx,
                    &[(
                        "lib/default.project.json",
                        r#"{"name": "lib", "tree": {"$path": "src"}}"#,
                    )],
                ),
                _ => {}
            }
            let shared = built_shared(&fx);
            let case = format!("{shape}, earlier node: {}", !earlier.is_empty());
            assert_eq!(shared["className"], json!("ModuleScript"), "{case}");
            assert_eq!(shared["source"], json!(INIT_SOURCE), "{case}");
            match shape {
                "tree node with fields" => {
                    assert_eq!(shared["attributes"], json!({"X": 1.0}), "{case}");
                }
                "tree node with fields and an adapter file" => {
                    assert_eq!(shared["attributes"], json!({"X": 1.0}), "{case}");
                    assert_eq!(shared["children"][0]["name"], json!("Note"), "{case}");
                }
                "model file in the directory" => {
                    assert_eq!(shared["children"][0]["name"], json!("Kit"), "{case}");
                    assert_eq!(shared["children"][0]["className"], json!("Model"), "{case}");
                }
                _ => {}
            }
        }
    }
}

#[test]
fn build_keeps_the_init_script_of_a_mounted_directory() {
    let mount = r#"{"source": "src/shared", "target": "ReplicatedStorage.Shared"}"#;
    let earlier = r#"{"source": "other", "target": "ReplicatedStorage.Other"}, "#;
    for earlier in ["", earlier] {
        let fx = empty_folder("build-init-mount");
        let project = format!(r#"{{"schemaVersion": 1, "mounts": [{earlier}{mount}]}}"#);
        write_files(
            &fx,
            &[
                ("renium.project.jsonc", project.as_str()),
                ("src/shared/init.luau", INIT_SOURCE),
                ("other/A.luau", "return 1\n"),
            ],
        );
        let shared = built_shared(&fx);
        let case = format!("earlier mount: {}", !earlier.is_empty());
        assert_eq!(shared["className"], json!("ModuleScript"), "{case}");
        assert_eq!(shared["source"], json!(INIT_SOURCE), "{case}");
    }
}

#[test]
fn build_keeps_a_folder_class_the_tree_node_names_over_an_init_script() {
    for earlier in ["", EARLIER_NODE] {
        let fx = empty_folder("build-init-explicit");
        let project = rojo_replicated_storage(&format!(
            r#"{{{earlier}"Shared": {{"$path": "src/shared", "$className": "Folder"}}}}"#
        ));
        write_files(
            &fx,
            &[
                ("default.project.json", project.as_str()),
                ("src/shared/init.luau", INIT_SOURCE),
            ],
        );
        let shared = built_shared(&fx);
        assert_eq!(shared["className"], json!("Folder"), "{earlier}");
        assert_eq!(shared.get("source"), None, "{earlier}");
    }
}

#[test]
fn build_places_a_part_by_the_position_and_orientation_of_its_tree_node_or_sidecar() {
    let fx = empty_folder("build-position");
    write_files(
        &fx,
        &[
            (
                "renium.project.jsonc",
                r#"{"schemaVersion": 1, "tree": {"Workspace": {
                    "Pad": {"$className": "Part", "$properties": {
                        "Anchored": true, "Position": [0, 5, 0], "Orientation": [0, 90, 0]
                    }},
                    "Sign": {"$className": "Part", "$properties": {
                        "CFrame": {"position": [1, 2, 3], "orientation": [0, 90, 0]},
                        "Position": [4, 5, 6]
                    }}
                }}}"#,
            ),
            (
                "src/Workspace/Block.meta.json",
                r#"{"className": "Part", "properties": {"Position": [7, 8, 9]}}"#,
            ),
        ],
    );
    let placed =
        |name: &str| built_instance(&fx, name, "CFrame")["props"]["CFrame"]["components"].clone();
    // A quarter turn about Y after the position.
    assert_eq!(
        placed("Pad"),
        json!([0.0, 5.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 0.0])
    );
    assert_eq!(
        placed("Sign"),
        json!([4.0, 5.0, 6.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 0.0])
    );
    assert_eq!(
        placed("Block"),
        json!([7.0, 8.0, 9.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0])
    );
}

fn fixture_sourcemap(root_name: &str, absolute: bool) -> Value {
    let file = |relative: &str| {
        if absolute {
            format!("<root>/{relative}")
        } else {
            relative.to_string()
        }
    };
    json!({
        "name": root_name,
        "className": "DataModel",
        "children": [
            {
                "name": "ReplicatedStorage",
                "className": "ReplicatedStorage",
                "children": [
                    {"name": "Shared", "className": "Folder", "children": [
                        {"name": "Config", "className": "ModuleScript", "filePaths": [file("src/ReplicatedStorage/Shared/Config.luau")]},
                    ]},
                    {"name": "Link", "className": "ObjectValue"},
                ],
            },
            {"name": "ServerScriptService", "className": "ServerScriptService"},
            {
                "name": "Workspace",
                "className": "Workspace",
                "children": [
                    {"name": "Lobby", "className": "Folder", "children": [
                        {"name": "Door", "className": "Part"},
                        {"name": "Border", "className": "Part"},
                        {"name": "Border", "className": "Part"},
                        {"name": "Helper", "className": "LocalScript", "filePaths": [file("src/Workspace/Lobby/Helper.client.luau")]},
                    ]},
                    {"name": "Spawner", "className": "Script", "filePaths": [file("src/Workspace/Spawner.server.luau")]},
                    {"name": "Target", "className": "ObjectValue"},
                    {"name": "Café ☕", "className": "Folder"},
                ],
            },
        ],
    })
}

#[test]
fn sourcemap_writes_prints_filters_and_reads_the_cache() {
    let fx = Fixture::new("sourcemap");
    let root = fx.path("");
    let output = fx.root.join("map.json");
    let written = fx.text(&[
        "sm",
        "--project-root",
        &root,
        "-o",
        &output.to_string_lossy(),
    ]);
    assert_eq!(written.error, None);
    assert_eq!(written.value, Value::Null);
    assert_eq!(written.stdout, "");
    let file_text = normalize_text(&fs::read_to_string(&output).unwrap(), &fx.root);
    assert_eq!(
        serde_json::from_str::<Value>(&file_text).unwrap(),
        fixture_sourcemap("<dir>", false)
    );
    assert!(file_text.ends_with("}\n"));
    let printed = fx.text(&["sm", "--project-root", &root, "--stdout"]);
    assert_eq!(printed.value, Value::Null);
    assert_eq!(printed.stdout, file_text);
    assert_eq!(
        serde_json::from_str::<Value>(&printed.stdout).unwrap(),
        fixture_sourcemap("<dir>", false)
    );
    assert_eq!(
        fx.json(&["sm", "--project-root", &root, "--stdout"]).stdout,
        printed.stdout
    );
    assert_eq!(
        fx.text(&["sm", "--project-root", &root, "--stdout", "--cached"])
            .err(),
        "No cached sourcemap exists at <root>/sourcemap.json"
    );
    assert!(
        fx.text(&["sm", "--project-root", &root, "--cached"])
            .err()
            .contains("--stdout")
    );
    let filtered = fx
        .text(&[
            "sm",
            "--project-root",
            &root,
            "--stdout",
            "--filter",
            "**/Lobby/**",
        ])
        .stdout;
    assert_eq!(
        serde_json::from_str::<Value>(&filtered).unwrap(),
        json!({"name": "<dir>", "className": "DataModel", "children": [{"name": "Workspace", "className": "Workspace", "children": [{"name": "Lobby", "className": "Folder", "children": [{"name": "Helper", "className": "LocalScript", "filePaths": ["src/Workspace/Lobby/Helper.client.luau"]}]}]}]})
    );
    let absolute = fx
        .text(&[
            "sm",
            "--project-root",
            &root,
            "--stdout",
            "--absolute-paths",
        ])
        .stdout;
    assert_eq!(
        serde_json::from_str::<Value>(&absolute).unwrap(),
        fixture_sourcemap("<dir>", true)
    );
    assert!(
        fx.text(&["sm", "--project-root", &root, "--stdout", "--filter", "["])
            .err()
            .starts_with("Invalid sourcemap filter '['")
    );
    let default = fx.text(&["sm", "--project-root", &root]);
    assert_eq!(default.error, None);
    assert_eq!(
        serde_json::from_str::<Value>(&normalize_text(
            &fs::read_to_string(fx.root.join("sourcemap.json")).unwrap(),
            &fx.root
        ))
        .unwrap(),
        fixture_sourcemap("<dir>", false)
    );
    assert_eq!(
        fx.text(&["sm", "--project-root", &root, "--stdout", "--cached"])
            .stdout,
        printed.stdout
    );
    assert_eq!(
        fx.text(&[
            "sm",
            "--project-root",
            &root,
            "--stdout",
            "--cached",
            "--watch"
        ])
        .err(),
        "--cached cannot be combined with --watch"
    );
    assert!(
        fx.text(&["sm", "--project-root", &fx.path("nowhere"), "--stdout"])
            .err()
            .starts_with("Failed to resolve project root")
    );
    let mapped = mapped_project(&fx);
    let mapped_map =
        serde_json::from_str::<Value>(&fx.text(&["sm", "--project", &mapped, "--stdout"]).stdout)
            .unwrap();
    assert_eq!(
        mapped_map["children"],
        json!([
            {"name": "ReplicatedStorage", "className": "ReplicatedStorage", "children": [{"name": "Shared", "className": "Folder", "children": [{"name": "Util", "className": "ModuleScript", "filePaths": ["shared/Util.luau"]}]}]},
            {"name": "ServerScriptService", "className": "ServerScriptService", "children": [{"name": "Main", "className": "Script", "filePaths": ["src/server/Main.server.luau"]}]},
            {"name": "Workspace", "className": "Workspace", "children": [{"name": "Draft", "className": "Script", "filePaths": ["src/Workspace/Draft.server.txt"]}]},
        ])
    );
}

fn import_result(
    destination: &str,
    action: &str,
    path: &str,
    source: &str,
    dry_run: bool,
) -> Value {
    json!({
        "destination": destination,
        "dryRun": dry_run,
        "files": [{"action": action, "path": path}],
        "ok": true,
        "source": source,
    })
}

#[test]
fn import_path_copies_scripts_and_folders_and_registers_store_instances() {
    let fx = Fixture::new("import-path");
    let root = fx.path("");
    fs::create_dir_all(fx.path("imports/tree/deep")).unwrap();
    let shared = fx.path("imports/Shared.luau");
    fs::write(&shared, "return 'shared'\n").unwrap();
    fs::write(fx.path("imports/tree/a.luau"), "return 'a'\n").unwrap();
    fs::write(fx.path("imports/tree/deep/b.server.luau"), "print('b')\n").unwrap();
    let boot = fx.path("imports/Boot.server.luau");
    fs::write(&boot, "print('boot')\n").unwrap();
    let init = fx.path("imports/init.luau");
    fs::write(&init, "return {}\n").unwrap();
    let dry = fx.both(&[
        "ip",
        &shared,
        "--path-json",
        "[\"ServerScriptService\",\"Shared\"]",
        "-r",
        &root,
        "--dry-run",
    ]);
    assert_eq!(
        dry,
        import_result(
            "<root>/src/ServerScriptService/Shared.luau",
            "create",
            "src/ServerScriptService/Shared.luau",
            "<root>/imports/Shared.luau",
            true
        )
    );
    assert!(!fx.exists("src/ServerScriptService/Shared.luau"));
    let mut created = import_result(
        "<root>/src/ServerScriptService/Shared.luau",
        "create",
        "src/ServerScriptService/Shared.luau",
        "<root>/imports/Shared.luau",
        false,
    );
    created["storeInstance"] = json!({
        "adoptedSource": "<root>/src/ServerScriptService/Shared.luau",
        "changedPaths": ["<root>/instances/ServerScriptService.renium"],
        "index": 1,
        "ok": true,
        "pathOrdinals": [1, 1],
        "pathSegments": ["ServerScriptService", "Shared"],
        "settingsFile": "<root>/instances/ServerScriptService.renium",
        "settingsId": "editor:1",
    });
    assert_eq!(
        fx.text(&[
            "ip",
            &shared,
            "--path-json",
            "[\"ServerScriptService\",\"Shared\"]",
            "-r",
            &root
        ])
        .ok(),
        created
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/ServerScriptService/Shared.luau")).unwrap(),
        "return 'shared'\n"
    );
    assert_eq!(
        fx.json(&[
            "ip",
            &shared,
            "--path-json",
            "[\"ServerScriptService\",\"Shared\"]",
            "-r",
            &root
        ])
        .ok(),
        import_result(
            "<root>/src/ServerScriptService/Shared.luau",
            "unchanged",
            "src/ServerScriptService/Shared.luau",
            "<root>/imports/Shared.luau",
            false
        )
    );
    fs::write(&shared, "return 'shared v2'\n").unwrap();
    assert_eq!(
        fx.text(&[
            "ip",
            &shared,
            "--path-json",
            "[ServerScriptService, Shared]",
            "-r",
            &root
        ])
        .ok()["files"][0]["action"],
        json!("overwrite")
    );
    assert_eq!(
        fs::read_to_string(fx.root.join("src/ServerScriptService/Shared.luau")).unwrap(),
        "return 'shared v2'\n"
    );
    let script = fx
        .text(&[
            "ip",
            &boot,
            "--path-json",
            "[\"ServerScriptService\",\"Boot\"]",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        script["destination"],
        json!("<root>/src/ServerScriptService/Boot.server.luau")
    );
    assert_eq!(script["storeInstance"]["settingsId"], json!("editor:2"));
    let pack = fx
        .text(&[
            "ip",
            &init,
            "--path-json",
            "[\"ServerScriptService\",\"Pack\"]",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        pack["destination"],
        json!("<root>/src/ServerScriptService/Pack.luau")
    );
    assert_eq!(
        fx.names("ServerScriptService"),
        ["ServerScriptService", "Shared", "Boot", "Pack"]
    );
    assert_eq!(
        fx.read_store("ServerScriptService").instances[2].class_name,
        "Script"
    );
}

#[test]
fn import_path_reports_store_conflicts_bad_inputs_and_mapped_destinations() {
    let fx = Fixture::new("import-path-errors");
    let root = fx.path("");
    fs::create_dir_all(fx.path("imports/tree/deep")).unwrap();
    let shared = fx.path("imports/Shared.luau");
    fs::write(&shared, "return 'shared'\n").unwrap();
    fs::write(fx.path("imports/tree/a.luau"), "return 'a'\n").unwrap();
    fs::write(fx.path("imports/tree/deep/b.server.luau"), "print('b')\n").unwrap();
    fx.text(&[
        "ip",
        &shared,
        "--path-json",
        "[\"ServerScriptService\",\"Shared\"]",
        "-r",
        &root,
    ])
    .ok();
    let folder_clash = fx
        .text(&[
            "ip",
            &shared,
            "--path-json",
            "[\"Workspace\",\"Lobby\"]",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        folder_clash["storeInstanceError"],
        json!(
            "Workspace.Lobby already exists in the store as a Folder; remove it with `rbx br` or choose another name"
        )
    );
    assert_eq!(folder_clash["files"][0]["action"], json!("create"));
    assert_eq!(
        fx.text(&[
            "ip",
            &shared,
            "--path-json",
            "[\"Workspace\",\"Lobby\",\"Border\"]",
            "-r",
            &root
        ])
        .ok()["storeInstanceError"],
        json!("Workspace.Lobby.Border names several store instances")
    );
    let unsynced = fx
        .text(&[
            "ip",
            &shared,
            "--path-json",
            "[\"StarterGui\",\"Menu\"]",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(
        unsynced["destination"],
        json!("<root>/src/StarterGui/Menu.luau")
    );
    assert!(unsynced.get("storeInstance").is_none());
    let err = |args: &[&str]| fx.text(&[&["ip"], args, &["-r", &root]].concat()).err();
    assert_eq!(
        err(&[&shared, "--path-json", "[\"ServerScriptService\"]"]),
        "--path-json must include a service and target name"
    );
    assert_eq!(
        err(&[&shared, "--path-json", "[\"ServerScriptService\",\"..\"]"]),
        "--path-json contains an invalid path segment: \"..\""
    );
    assert!(
        err(&[&shared, "--path-json", "nope"])
            .starts_with("--path-json must be a JSON string array or bracketed comma list")
    );
    assert_eq!(
        err(&[&shared]),
        "ip imports a script file with --path-json or a directory with --destination; import places with pi and models with mip"
    );
    assert!(
        err(&[&fx.path("imports/nope.luau"), "--destination", "src/X"])
            .starts_with("Failed to resolve <root>/imports/nope.luau")
    );
    assert_eq!(
        err(&[&fx.path("imports/tree"), "--destination", "../outside"]),
        "destination cannot leave the project: ../outside"
    );
    assert!(
        fx.text(&[
            "ip",
            &shared,
            "--path-json",
            "[\"ServerScriptService\",\"Shared\"]",
            "--destination",
            "src/X",
            "-r",
            &root
        ])
        .err()
        .contains("cannot be used with")
    );
    let tree = fx.path("imports/tree");
    assert_eq!(
        fx.both(&[
            "ip",
            &tree,
            "--destination",
            "src/ReplicatedStorage/Tree",
            "-r",
            &root,
            "--dry-run"
        ]),
        json!({"destination": "<root>/src/ReplicatedStorage/Tree", "dryRun": true, "files": [{"action": "create", "path": "src/ReplicatedStorage/Tree/a.luau"}, {"action": "create", "path": "src/ReplicatedStorage/Tree/deep/b.server.luau"}], "ok": true, "source": "<root>/imports/tree"})
    );
    assert!(!fx.exists("src/ReplicatedStorage/Tree"));
    let applied = fx
        .text(&[
            "ip",
            &tree,
            "--destination",
            "src/ReplicatedStorage/Tree",
            "-r",
            &root,
        ])
        .ok();
    assert_eq!(applied["files"][1]["action"], json!("create"));
    assert_eq!(
        fs::read_to_string(
            fx.root
                .join("src/ReplicatedStorage/Tree/deep/b.server.luau")
        )
        .unwrap(),
        "print('b')\n"
    );
    assert_eq!(
        fx.text(&[
            "ip",
            &tree,
            "--destination",
            "src/ReplicatedStorage/Tree",
            "-r",
            &root
        ])
        .ok()["files"][0]["action"],
        json!("unchanged")
    );
    assert_eq!(
        fx.text(&[
            "ip",
            &shared,
            "--destination",
            "src/ServerScriptService",
            "-r",
            &root
        ])
        .ok()["files"],
        json!([{"action": "unchanged", "path": "src/ServerScriptService/Shared.luau"}])
    );
    let mapped = mapped_project(&fx);
    assert_eq!(
        fx.text(&[
            "ip",
            &shared,
            "--path-json",
            "[\"ServerScriptService\",\"Mapped\"]",
            "--project",
            &mapped
        ])
        .ok(),
        import_result(
            "<root>/mapped/src/server/Mapped.luau",
            "create",
            "src/server/Mapped.luau",
            "<root>/imports/Shared.luau",
            false
        )
    );
    let nested = fx
        .text(&[
            "ip",
            &shared,
            "--path-json",
            "[\"ServerScriptService\",\"Nested\"]",
            "-r",
            &fx.path("nowhere"),
        ])
        .ok();
    assert_eq!(
        nested["files"],
        json!([{"action": "create", "path": "src/ServerScriptService/Nested.luau"}])
    );
    assert_eq!(
        nested["storeInstance"]["pathSegments"],
        json!(["ServerScriptService", "Nested"])
    );
    let outside = temp_dir("offline-import-no-project");
    let error = run_at(
        &outside,
        CaptureMode::Text,
        &[
            "ip",
            &shared,
            "--path-json",
            "[\"ServerScriptService\",\"X\"]",
            "-r",
            &outside.to_string_lossy(),
        ],
    )
    .err();
    assert!(
        error.contains("No renium.project.jsonc was found"),
        "{error}"
    );
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn the_project_map_follows_the_stores_and_decodes_only_a_changed_store() {
    let fx = Fixture::new("project-map");
    let map_path = fx.root.join("RENIUM").join("map.md");
    let cache_path = fx.root.join(".renium").join("map-cache.json");
    let refresh = || crate::project::workflows::refresh_project_map(&fx.root).unwrap();
    let map = || fs::read_to_string(&map_path).unwrap();
    let add = |service: &str, name: &str| {
        fx.text(&[
            "ba",
            "-f",
            &fx.store_arg(service),
            "-n",
            name,
            "-c",
            "Folder",
        ])
        .ok();
    };

    refresh();
    let first = map();
    for service in ["Workspace", "ReplicatedStorage", "ServerScriptService"] {
        assert!(first.contains(&format!("- **{service}** (")), "{first}");
    }
    assert!(first.contains("StarterGui"), "{first}");
    assert!(!first.contains("MapProbe"), "{first}");

    add("Workspace", "MapProbe");
    refresh();
    assert!(map().contains("MapProbe"), "{}", map());

    let key = "instances/ReplicatedStorage.renium";
    let stamp_store = |seconds_from_now: i64| {
        let now = std::time::SystemTime::now();
        let offset = std::time::Duration::from_secs(seconds_from_now.unsigned_abs());
        fs::File::options()
            .write(true)
            .open(fx.store("ReplicatedStorage"))
            .unwrap()
            .set_modified(if seconds_from_now < 0 {
                now - offset
            } else {
                now + offset
            })
            .unwrap();
    };
    let replace_cached_line = || {
        let mut cache: Value = serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        cache["stores"][key]["line"] = json!("- **ReplicatedStorage** kept line");
        fs::write(&cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();
        cache["stores"][key]["settled"].clone()
    };

    stamp_store(60);
    refresh();
    assert_eq!(replace_cached_line(), json!(false));
    refresh();
    assert!(!map().contains("kept line"), "{}", map());

    stamp_store(-60);
    refresh();
    assert_eq!(replace_cached_line(), json!(true));
    refresh();
    assert!(map().contains("kept line"), "{}", map());

    add("ReplicatedStorage", "MapChanged");
    refresh();
    let changed = map();
    assert!(!changed.contains("kept line"), "{changed}");
    assert!(changed.contains("MapChanged"), "{changed}");

    fs::remove_file(&map_path).unwrap();
    refresh();
    assert_eq!(map(), changed);

    fs::write(&cache_path, b"not json").unwrap();
    fs::write(&map_path, "edited by hand").unwrap();
    refresh();
    assert_eq!(map(), changed);
}

#[test]
fn the_project_map_of_an_experience_has_a_section_per_place_and_lists_mapped_stores() {
    let root = temp_dir("offline-project-map-experience");
    fs::write(
        root.join("renium.experience.json"),
        r#"{"gameId": 1, "places": {"arena": {"placeId": 2, "root": "arena"}, "lobby": {"placeId": 3, "root": "lobby"}}}"#,
    )
    .unwrap();
    let write = |relative: &str, document: &SettingsBytecode| {
        let store = root.join(relative);
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        document.write_file(&store).unwrap();
    };
    write("arena/instances/Workspace.renium", &workspace_document());
    write(
        "arena/instances/ReplicatedStorage/Shared.renium",
        &replicated_document(),
    );
    write(
        "lobby/instances/ServerScriptService.renium",
        &server_document(),
    );

    crate::project::workflows::refresh_project_map(&root.join("lobby")).unwrap();
    let map = fs::read_to_string(root.join("RENIUM").join("map.md")).unwrap();
    let position = |needle: &str| {
        map.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {map}"))
    };
    let arena = position("## arena");
    let lobby = position("## lobby");
    assert!(arena < position("- **ReplicatedStorage.Shared** ("));
    assert!(position("- **ReplicatedStorage.Shared** (") < position("- **Workspace** ("));
    assert!(position("- **Workspace** (") < lobby);
    assert!(lobby < position("- **ServerScriptService** ("));
    assert!(!root.join("lobby").join("RENIUM").exists());

    let cache: Value =
        serde_json::from_slice(&fs::read(root.join(".renium").join("map-cache.json")).unwrap())
            .unwrap();
    assert_eq!(
        cache["stores"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        [
            "arena/instances/ReplicatedStorage/Shared.renium",
            "arena/instances/Workspace.renium",
            "lobby/instances/ServerScriptService.renium",
        ]
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn every_project_a_command_loads_is_known_to_the_map_refresh() {
    let fx = Fixture::new("project-map-roots");
    let other = temp_dir("offline-project-map-other");
    fs::write(
        other.join("renium.project.jsonc"),
        "{\n  \"schemaVersion\": 1,\n  \"sourceRoot\": \"scripts\"\n}\n",
    )
    .unwrap();
    fs::create_dir_all(other.join("instances")).unwrap();
    workspace_document()
        .write_file(&other.join("instances").join("Workspace.renium"))
        .unwrap();
    run_at(
        &fx.root,
        CaptureMode::Text,
        &["tr", "Workspace", "-r", &other.to_string_lossy()],
    )
    .ok();
    let roots = crate::project::storage::project_roots();
    for root in [&fx.root, &other] {
        let root = crate::system::files::absolutize_for_daemon(root);
        assert!(roots.contains(&root), "{root:?} not in {roots:?}");
    }
    let _ = fs::remove_dir_all(other);
}
