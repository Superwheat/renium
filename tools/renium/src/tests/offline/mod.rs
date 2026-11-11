//! Offline saved-data commands run through the real dispatch on a fixed
//! fixture project, with their printed output captured as text mode and as
//! json mode would print it.

use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{Map, Value, json};

use crate::app::output::{CaptureMode, capture_output};
use crate::cli::Cli;
use crate::editor::paths::build_editor_source_paths_by_index;
use crate::settings::EXTERNAL_SOURCE_MARKER;
use crate::settings::bytecode::{SettingsBytecode, SettingsBytecodeInstance};
use crate::tests::support::{settings_document, settings_instance, temp_dir};

mod edits;
mod files;
mod project;
mod references;
mod stores;

/// What one captured command run produced.
pub(super) struct Outcome {
    pub(super) value: Value,
    pub(super) stdout: String,
    pub(super) error: Option<String>,
}

impl Outcome {
    #[track_caller]
    pub(super) fn ok(self) -> Value {
        assert!(self.error.is_none(), "{}", self.error.unwrap());
        self.value
    }

    #[track_caller]
    pub(super) fn err(self) -> String {
        assert!(self.error.is_some(), "{}", self.value);
        self.error.unwrap()
    }
}

/// A project folder holding a fixed set of stores and script files.
pub(super) struct Fixture {
    pub(super) root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(super) const DOOR_CFRAME: [f64; 12] =
    [0.0, 4.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// The double expansion Studio stores for the single-precision 0.2.
pub(super) const RATIO_EXPANDED: f64 = 0.20000000298023224;

/// The Door part's properties as every store command prints them.
pub(super) fn door_props() -> Value {
    json!({
        "Anchored": true,
        "CFrame": {"_type": "CFrame", "components": DOOR_CFRAME},
        "Color": {"_type": "Color3", "b": 0.0, "g": 0.5, "r": 1.0},
        "Material": {"_type": "EnumItem", "name": "Wood"},
        "Size": {"_type": "Vector3", "x": 4.0, "y": 8.0, "z": 1.0},
        "Tags": ["Interactive"],
        "Transparency": 0.5,
    })
}

/// The Door part's attributes; text mode prints the shortest decimal that
/// round-trips the single-precision value, json mode the exact double.
pub(super) fn door_attrs(json_mode: bool) -> Value {
    json!({
        "Owner": "Ada",
        "Ratio": if json_mode { RATIO_EXPANDED } else { 0.2 },
        "Reviewed": true,
        "Spawn": {"_type": "Vector3", "x": 1.0, "y": 2.0, "z": 3.0},
        "Weight": 2.5,
    })
}

fn with_values(
    mut instance: SettingsBytecodeInstance,
    properties: &[(&str, Value)],
    attributes: &[(&str, Value)],
) -> SettingsBytecodeInstance {
    for (name, value) in properties {
        instance
            .properties
            .insert((*name).to_string(), value.clone());
    }
    for (name, value) in attributes {
        instance
            .attributes
            .insert((*name).to_string(), value.clone());
    }
    instance
}

fn script(
    settings_id: &str,
    name: &str,
    class_name: &str,
    parent_index: usize,
) -> SettingsBytecodeInstance {
    with_values(
        settings_instance(settings_id, name, class_name, Some(parent_index)),
        &[("Source", json!(EXTERNAL_SOURCE_MARKER))],
        &[],
    )
}

pub(super) fn door_reference() -> Value {
    json!({
        "_type": "Ref",
        "settingsId": "door",
        "pathSegments": ["Workspace", "Lobby", "Door"],
        "pathOrdinals": [1, 1, 1],
    })
}

pub(super) fn workspace_document() -> SettingsBytecode {
    settings_document(vec![
        with_values(
            settings_instance("ws", "Workspace", "Workspace", None),
            &[("Gravity", json!(120.0))],
            &[],
        ),
        settings_instance("lobby", "Lobby", "Folder", Some(0)),
        with_values(
            settings_instance("door", "Door", "Part", Some(1)),
            &[
                ("Anchored", json!(true)),
                ("Transparency", json!(0.5)),
                (
                    "Size",
                    json!({"_type": "Vector3", "x": 4.0, "y": 8.0, "z": 1.0}),
                ),
                (
                    "Color",
                    json!({"_type": "Color3", "r": 1.0, "g": 0.5, "b": 0.0}),
                ),
                (
                    "CFrame",
                    json!({"_type": "CFrame", "components": DOOR_CFRAME}),
                ),
                (
                    "Material",
                    json!({"_type": "EnumItem", "enumType": "Material", "name": "Wood"}),
                ),
                ("Tags", json!(["Interactive"])),
            ],
            &[
                ("Reviewed", json!(true)),
                ("Owner", json!("Ada")),
                ("Weight", json!(2.5)),
                ("Ratio", json!(RATIO_EXPANDED)),
                (
                    "Spawn",
                    json!({"_type": "Vector3", "x": 1.0, "y": 2.0, "z": 3.0}),
                ),
            ],
        ),
        with_values(
            settings_instance("border1", "Border", "Part", Some(1)),
            &[("Transparency", json!(0.25))],
            &[],
        ),
        with_values(
            settings_instance("border2", "Border", "Part", Some(1)),
            &[("Transparency", json!(0.75))],
            &[],
        ),
        script("spawner", "Spawner", "Script", 0),
        with_values(
            settings_instance("target", "Target", "ObjectValue", Some(0)),
            &[("Value", door_reference())],
            &[],
        ),
        settings_instance("cafe", "Café ☕", "Folder", Some(0)),
        script("helper", "Helper", "LocalScript", 1),
    ])
}

pub(super) fn replicated_document() -> SettingsBytecode {
    settings_document(vec![
        settings_instance("rs", "ReplicatedStorage", "ReplicatedStorage", None),
        settings_instance("shared", "Shared", "Folder", Some(0)),
        script("config", "Config", "ModuleScript", 1),
        with_values(
            settings_instance("link", "Link", "ObjectValue", Some(0)),
            &[("Value", door_reference())],
            &[],
        ),
    ])
}

pub(super) fn server_document() -> SettingsBytecode {
    settings_document(vec![settings_instance(
        "sss",
        "ServerScriptService",
        "ServerScriptService",
        None,
    )])
}

/// A project whose ServerScriptService holds Tools.Util and whose Workspace
/// holds an instance with Util's settings id: stores built offline number
/// their instances alike, so the same id names different instances in them.
pub(super) fn outside_reference_fixture(tag: &str) -> Fixture {
    let fx = Fixture::new(tag);
    fx.write_store(
        "ServerScriptService",
        &settings_document(vec![
            settings_instance("sss", "ServerScriptService", "ServerScriptService", None),
            settings_instance("editor:1", "Tools", "Folder", Some(0)),
            settings_instance("editor:2", "Util", "Folder", Some(1)),
        ]),
    );
    let mut workspace = workspace_document();
    workspace
        .instances
        .push(settings_instance("editor:2", "Clash", "Folder", Some(0)));
    fx.write_store("Workspace", &workspace);
    fx
}

pub(super) fn outside_reference(path: &[&str], ordinals: &[usize], settings_id: &str) -> Value {
    json!({"_type": "Ref", "pathOrdinals": ordinals, "pathSegments": path, "settingsId": settings_id})
}

/// Builds the fixture's place and returns the path the reference property
/// of `holder` points at in it.
pub(super) fn built_reference_target(fx: &Fixture, holder: &str) -> Option<String> {
    let place = fx.root.join("references.rbxl");
    fx.text(&["bep", "-r", &fx.path(""), "-o", &place.to_string_lossy()])
        .ok();
    place_reference_target(&place, holder, "Value")
}

pub(super) const SPAWNER_SOURCE: &str = "print('spawn')\n";
pub(super) const HELPER_SOURCE: &str = "local a = 1\r\nreturn a\r\n";
pub(super) const CONFIG_SOURCE: &str = "return { rounds = 3 }\n";

impl Fixture {
    /// A project with Workspace, ReplicatedStorage and ServerScriptService
    /// stores; StarterGui has no store yet.
    pub(super) fn new(tag: &str) -> Self {
        let root = temp_dir(&format!("offline-{tag}"));
        fs::write(
            root.join("renium.project.jsonc"),
            "{\n  \"schemaVersion\": 1\n}\n",
        )
        .unwrap();
        crate::project::config::load_project(Some(&root.join("renium.project.jsonc")), None)
            .unwrap();
        let fixture = Self { root };
        fixture.write_store("Workspace", &workspace_document());
        fixture.write_store("ReplicatedStorage", &replicated_document());
        fixture.write_store("ServerScriptService", &server_document());
        fixture.write_source("Workspace", 5, SPAWNER_SOURCE);
        fixture.write_source("Workspace", 8, HELPER_SOURCE);
        fixture.write_source("ReplicatedStorage", 2, CONFIG_SOURCE);
        fixture
    }

    pub(super) fn store(&self, service: &str) -> PathBuf {
        self.root
            .join("instances")
            .join(format!("{service}.renium"))
    }

    pub(super) fn service_dir(&self, service: &str) -> PathBuf {
        self.root.join("src").join(service)
    }

    pub(super) fn write_store(&self, service: &str, document: &SettingsBytecode) {
        document.write_file(&self.store(service)).unwrap();
    }

    pub(super) fn read_store(&self, service: &str) -> SettingsBytecode {
        SettingsBytecode::read_file(&self.store(service)).unwrap()
    }

    pub(super) fn source_path(&self, service: &str, index: usize) -> PathBuf {
        let document = self.read_store(service);
        build_editor_source_paths_by_index(&document, service, &self.service_dir(service))[index]
            .clone()
            .unwrap()
    }

    pub(super) fn write_source(&self, service: &str, index: usize, source: &str) -> PathBuf {
        let path = self.source_path(service, index);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        path
    }

    pub(super) fn path(&self, relative: &str) -> String {
        self.root.join(relative).to_string_lossy().into_owned()
    }

    pub(super) fn store_arg(&self, service: &str) -> String {
        self.store(service).to_string_lossy().into_owned()
    }

    pub(super) fn run(&self, mode: CaptureMode, args: &[&str]) -> Outcome {
        run_at(&self.root, mode, args)
    }

    pub(super) fn text(&self, args: &[&str]) -> Outcome {
        self.run(CaptureMode::Text, args)
    }

    pub(super) fn json(&self, args: &[&str]) -> Outcome {
        self.run(CaptureMode::Json, args)
    }

    /// The same run in both output modes; both outcomes must agree.
    #[track_caller]
    pub(super) fn both(&self, args: &[&str]) -> Value {
        let text = self.text(args).ok();
        let json = self.json(args).ok();
        assert_eq!(text, json, "text and json output differ for {args:?}");
        text
    }

    /// Instance names of a store in index order.
    pub(super) fn names(&self, service: &str) -> Vec<String> {
        self.read_store(service)
            .instances
            .iter()
            .map(|instance| instance.name.clone())
            .collect()
    }

    pub(super) fn exists(&self, relative: &str) -> bool {
        self.root.join(relative).exists()
    }
}

/// Runs one CLI invocation (`args` without the program name) through clap
/// and the command dispatcher, with paths under `root` printed as `<root>`.
pub(super) fn run_at(root: &Path, mode: CaptureMode, args: &[&str]) -> Outcome {
    let cli = match Cli::try_parse_from(std::iter::once("rbx").chain(args.iter().copied())) {
        Ok(cli) => cli,
        Err(error) => {
            return Outcome {
                value: Value::Null,
                stdout: String::new(),
                error: Some(error.to_string()),
            };
        }
    };
    captured(
        root,
        capture_output(mode, || {
            crate::cli::dispatch::dispatch(cli.command, cli.project.as_deref())
        }),
    )
}

fn captured(root: &Path, run: (anyhow::Result<()>, Option<Value>, String)) -> Outcome {
    let (result, value, stdout) = run;
    Outcome {
        value: normalize(value.unwrap_or(Value::Null), root),
        stdout: normalize_text(&stdout, root),
        error: result
            .err()
            .map(|error| normalize_text(&format!("{error:#}"), root)),
    }
}

/// Runs `rbx se` (`args` without the program and command names) with `stdin`
/// as its input and `push` in place of the push to Studio.
pub(super) fn run_script_edit(
    root: &Path,
    mode: CaptureMode,
    args: &[&str],
    stdin: &str,
    push: crate::automation::script_edit::Push,
) -> Outcome {
    let cli = Cli::try_parse_from(["rbx", "se"].into_iter().chain(args.iter().copied()))
        .expect("the se command line parses");
    let crate::cli::Commands::ScriptEdit(parsed) = cli.command else {
        unreachable!("se is the script edit command");
    };
    captured(
        root,
        capture_output(mode, || {
            crate::automation::script_edit::run(
                parsed,
                cli.project.as_deref(),
                &mut stdin.as_bytes(),
                push,
            )
        }),
    )
}

fn root_forms(root: &Path) -> Vec<String> {
    let text = root.to_string_lossy().into_owned();
    let forward = text.replace('\\', "/");
    let mut forms = vec![text.clone()];
    if forward != text {
        forms.push(forward);
    }
    forms
}

/// Paths under `root` print as `<root>/...` whatever separator the platform
/// used, and the fixture's own folder name prints as `<dir>`.
pub(super) fn normalize_text(text: &str, root: &Path) -> String {
    let mut text = text.to_string();
    for form in root_forms(root) {
        text = text.replace(&form, "<root>");
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find("<root>") {
        out.push_str(&rest[..start + "<root>".len()]);
        rest = &rest[start + "<root>".len()..];
        let end = rest
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | ':' | ';' | ','))
            .unwrap_or(rest.len());
        out.push_str(&rest[..end].replace('\\', "/"));
        rest = &rest[end..];
    }
    out.push_str(rest);
    if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
        out = out.replace(name, "<dir>");
    }
    out
}

fn normalize(value: Value, root: &Path) -> Value {
    match value {
        Value::String(text) => {
            let mut text = normalize_text(&text, root);
            if text.starts_with("<root>") {
                text = text.replace('\\', "/");
            }
            Value::String(text)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| normalize(item, root))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| (key, normalize(item, root)))
                .collect::<Map<_, _>>(),
        ),
        other => other,
    }
}

/// A model with a part, a script and an internal reference, as Studio saves
/// one.
pub(super) fn sample_model_dom() -> rbx_dom_weak::WeakDom {
    use rbx_dom_weak::{InstanceBuilder, WeakDom};
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let model = dom.insert(
        dom.root_ref(),
        InstanceBuilder::new("Model").with_name("Kit"),
    );
    let part = dom.insert(
        model,
        InstanceBuilder::new("Part")
            .with_name("Base")
            .with_property("Anchored", true)
            .with_property("Transparency", 0.5f32),
    );
    dom.insert(
        model,
        InstanceBuilder::new("ModuleScript")
            .with_name("Setup")
            .with_property("Source", "return { kit = true }\n"),
    );
    dom.insert(
        model,
        InstanceBuilder::new("ObjectValue")
            .with_name("Primary")
            .with_property("Value", rbx_dom_weak::types::Variant::Ref(part)),
    );
    dom
}

pub(super) fn write_model_file(path: &Path, dom: &rbx_dom_weak::WeakDom) {
    let file = fs::File::create(path).unwrap();
    if path
        .extension()
        .is_some_and(|extension| extension == "rbxmx")
    {
        rbx_xml::to_writer_default(file, dom, dom.root().children()).unwrap();
    } else {
        rbx_binary::to_writer(file, dom, dom.root().children()).unwrap();
    }
}

/// A small place with Workspace, ReplicatedStorage and ServerScriptService
/// content, in the format the file name selects.
pub(super) fn sample_place_dom() -> rbx_dom_weak::WeakDom {
    use rbx_dom_weak::{InstanceBuilder, WeakDom};
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let workspace = dom.insert(
        dom.root_ref(),
        InstanceBuilder::new("Workspace").with_property("Gravity", 150.0f32),
    );
    let lobby = dom.insert(workspace, InstanceBuilder::new("Folder").with_name("Lobby"));
    let door = dom.insert(
        lobby,
        InstanceBuilder::new("Part")
            .with_name("Door")
            .with_property("Anchored", true)
            .with_property("Transparency", 0.5f32),
    );
    for _ in 0..2 {
        dom.insert(lobby, InstanceBuilder::new("Part").with_name("Pad"));
    }
    dom.insert(
        workspace,
        InstanceBuilder::new("Script")
            .with_name("Spawner")
            .with_property("Source", "print('spawn')\n"),
    );
    dom.insert(
        workspace,
        InstanceBuilder::new("ObjectValue")
            .with_name("Target")
            .with_property("Value", rbx_dom_weak::types::Variant::Ref(door)),
    );
    let replicated = dom.insert(dom.root_ref(), InstanceBuilder::new("ReplicatedStorage"));
    let shared = dom.insert(
        replicated,
        InstanceBuilder::new("Folder").with_name("Shared"),
    );
    dom.insert(
        shared,
        InstanceBuilder::new("ModuleScript")
            .with_name("Config")
            .with_property("Source", "return { rounds = 3 }\r\n"),
    );
    dom.insert(dom.root_ref(), InstanceBuilder::new("ServerScriptService"));
    dom
}

/// The dotted path of the instance a reference property of `holder` points
/// at in a binary place file; None for an empty reference.
pub(super) fn place_reference_target(place: &Path, holder: &str, property: &str) -> Option<String> {
    let dom = rbx_binary::from_reader(fs::File::open(place).unwrap()).unwrap();
    let mut current = dom.root();
    for name in holder.split('.') {
        current = current
            .children()
            .iter()
            .filter_map(|referent| dom.get_by_ref(*referent))
            .find(|instance| instance.name == name)
            .unwrap_or_else(|| panic!("{holder} is not in the place"));
    }
    let Some(rbx_dom_weak::types::Variant::Ref(target)) = current.properties.get(&property.into())
    else {
        panic!("{holder}.{property} is not a reference in the place");
    };
    let mut names = Vec::new();
    let mut cursor = dom.get_by_ref(*target)?;
    while cursor.referent() != dom.root_ref() {
        names.push(cursor.name.clone());
        cursor = dom.get_by_ref(cursor.parent())?;
    }
    names.reverse();
    Some(names.join("."))
}

pub(super) fn write_place_file(path: &Path, dom: &rbx_dom_weak::WeakDom) {
    crate::rbx::model::RbxPlaceFormat::from_path(path)
        .unwrap()
        .write(path, dom, dom.root().children())
        .unwrap();
}
