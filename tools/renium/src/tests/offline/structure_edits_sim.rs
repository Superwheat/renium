//! Seeded runs of the commands that add, copy, import, rename, move and
//! remove instances, `ba -J` batches of them among them, each on two copies
//! of one generated project: one whose stores are of version 11, which those
//! commands read and write whole, and one whose stores are in small blocks,
//! which they read by their instances and write block by block. The first is
//! the oracle. After every command the two must have printed the same, hold
//! the same files beside their stores, and hold stores that decode to the
//! same documents; and each store in blocks must be the file a whole encode
//! of its document gives. What a command names as the files it changed and
//! copied is held to the files themselves: each changed path holds other
//! bytes than it did, or none, and each copy the bytes its source held.
//!
//! Some runs start from stores a pull wrote (`Writer::Pull`), which hold
//! values as Studio gives them where a whole encode settles them. Before each
//! command the oracle then holds the stores in blocks as a pull writes them,
//! and after it each store in blocks holds what the whole path wrote up to
//! the values the two encoders write apart (`pulled_alike`), and each of its
//! blocks is the whole encode's or the one it held before, left standing.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::editor::paths::{build_editor_instance_paths, build_editor_instance_paths_for_indices};
use crate::editor::types::EditorInstancePath;
use crate::settings::bytecode::blocks::sim::{
    FORMATS, block_apart, block_rows_of, foreign_file, in_blocks, pulled_alike, pulled_in_blocks,
    pulled_v11, same_document, with_source_markers_of, without_source_markers,
};
use crate::settings::bytecode::decode_settings_bytecode;
use crate::settings::instance::remap_ref_indices_in_value;
use crate::tests::support::{SimRng, run_seeds};

const SEEDS: u64 = 24;
const STEPS: usize = 14;
pub(super) const SERVICES: [&str; 3] = ["Workspace", "ReplicatedStorage", "ServerStorage"];
/// Few names, so that siblings share them, and one no generated instance
/// has, which the references that name nothing use.
pub(super) const NAMES: [&str; 6] = ["Model", "Part", "Door", "Value", "Config", "Ghost"];
pub(super) const CLASSES: [&str; 5] = ["Folder", "Model", "Part", "ObjectValue", "ModuleScript"];
/// The first instances of each store, which the commands pick more often
/// than any other and under which the references to nothing name a child.
pub(super) const NEAR: usize = 4;
pub(super) const KINDS: [&str; 8] = [
    "add",
    "copy",
    "copy named",
    "rename",
    "move",
    "remove",
    "remove alone",
    "move to service",
];
/// What a run draws beside `KINDS`: the import of a model, and `ba -J`
/// batches without and with a restructuring op.
const MORE: [&str; 3] = ["import", "batch of adds", "batch of adds and a restructure"];
/// How often each kind of command went through.
static DONE: [AtomicUsize; KINDS.len() + MORE.len()] =
    [const { AtomicUsize::new(0) }; KINDS.len() + MORE.len()];
/// How often a store in blocks held values that the whole path settled.
static APART: AtomicUsize = AtomicUsize::new(0);
/// What the script files of a run go through, each counted in `COVERED`.
const COVERAGE: [&str; 3] = [
    "named copy that moved the script files it copied",
    "command that changed a .lua file",
    "move of a file to a path that differs only in letter case",
];
static COVERED: [AtomicUsize; COVERAGE.len()] = [const { AtomicUsize::new(0) }; COVERAGE.len()];

/// What wrote the stores a run starts from: a whole encode, another writer,
/// whose stores an edit writes whole first, or a pull.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Writer {
    Whole,
    Foreign,
    Pull,
}

impl Writer {
    pub(super) fn drawn(rng: &mut SimRng) -> Self {
        match rng.below(5) {
            0 => Self::Foreign,
            1 | 2 => Self::Pull,
            _ => Self::Whole,
        }
    }
}

fn reference(fields: Value) -> Value {
    let mut reference = json!({"_type": "Ref"});
    for (name, value) in fields.as_object().into_iter().flatten() {
        reference[name.as_str()] = value.clone();
    }
    reference
}

/// A store of `rows` instances under the root of `service`, without values.
pub(super) fn tree(rng: &mut SimRng, service: &str, rows: usize) -> SettingsBytecode {
    let mut instances = vec![settings_instance(
        format!("{service}-root"),
        service,
        service,
        None,
    )];
    if service == "Workspace" {
        instances.push(settings_instance("camera", "Camera", "Camera", Some(0)));
    }
    for row in instances.len()..rows {
        let parent = rng.below(row.min(4 + row / 3));
        // An id other stores hold too, now and then.
        let id = match rng.below(3) {
            0 => format!("n{row}"),
            _ => format!("{service}-{row}"),
        };
        let class = match rng.below(12) {
            0 => "Script",
            other => CLASSES[other % CLASSES.len()],
        };
        let name = match class {
            "Script" | "ModuleScript" => format!("Code{row}"),
            _ => NAMES[rng.below(NAMES.len() - 1)].to_string(),
        };
        instances.push(settings_instance(id, name, class, Some(parent)));
    }
    settings_document(instances)
}

type Paths = Vec<Option<EditorInstancePath>>;

/// A reference to an instance of one of `stores`: by position where it is
/// of `own`, else by an id, a path or both, and now and then to a child no
/// instance has yet.
fn generated_reference(
    rng: &mut SimRng,
    own: &str,
    stores: &BTreeMap<&str, (SettingsBytecode, Paths)>,
) -> Value {
    let service = SERVICES[rng.below(SERVICES.len())];
    let (document, paths) = &stores[service];
    let ghost = rng.below(5) == 0;
    let row = rng.below(if ghost {
        NEAR
    } else {
        document.instances.len()
    });
    let id = &document.instances[row].settings_id;
    let Some(path) = paths[row].clone() else {
        return reference(json!({"settingsId": "gone"}));
    };
    let (mut segments, mut ordinals) = (path.path_segments, path.path_ordinals);
    if ghost {
        segments.push("Ghost".to_string());
        ordinals.push(1);
    }
    match rng.below(6) {
        _ if ghost => reference(json!({"pathSegments": segments, "pathOrdinals": ordinals})),
        0 | 1 if service == own => reference(json!({"instanceIndex": row + 1})),
        0 => reference(json!({"settingsId": id})),
        1 | 2 => reference(json!({"pathSegments": segments, "pathOrdinals": ordinals})),
        3 => reference(json!({"pathSegments": segments, "pathOrdinals": vec![1; ordinals.len()]})),
        _ => {
            reference(json!({"settingsId": id, "pathSegments": segments, "pathOrdinals": ordinals}))
        }
    }
}

/// Gives the instances of every store values: plain ones, long texts that
/// several blocks name, and references of every kind.
pub(super) fn fill(rng: &mut SimRng, stores: &mut BTreeMap<&str, SettingsBytecode>) {
    let mut structure = BTreeMap::new();
    for (service, document) in stores.iter() {
        let paths = build_editor_instance_paths(document, service);
        structure.insert(*service, (document.clone(), paths));
    }
    for (service, document) in stores.iter_mut() {
        for row in 0..document.instances.len() {
            for _ in 0..rng.below(4) {
                let reference = generated_reference(rng, service, &structure);
                let instance = &mut document.instances[row];
                let (name, value) = match rng.below(8) {
                    0 => ("Anchored", json!(rng.below(2) == 0)),
                    1 => ("Label", json!(format!("label {}", rng.below(5)))),
                    2 => ("Blob", json!(format!("{}", rng.below(4)).repeat(300))),
                    3 => ("Nested", json!([reference, {"Inner": rng.below(9)}])),
                    4 => ("Adornee", reference),
                    5 => ("PrimaryPart", reference),
                    6 if instance.class_name == "Script" => {
                        ("RunContext", json!({"_type": "EnumItem", "name": "Client"}))
                    }
                    _ => ("Value", reference),
                };
                instance.properties.insert(name.to_string(), value);
            }
        }
    }
    let workspace = stores.get_mut("Workspace").unwrap();
    let camera = reference(json!({"instanceIndex": 2}));
    let root = &mut workspace.instances[0].properties;
    root.insert("CurrentCamera".to_string(), camera);
}

/// Now and then puts among the first instances of a store, once the stores
/// hold their values, an instance of a name no other store holds, and a
/// namesake of it behind every instance with a child named Ghost, and gives
/// an instance ahead of the first a reference to that child by its path
/// through the first, which names nothing until a command takes the first
/// away. No other reference names either of them, so a removal of the first
/// reads no other store, nor the instances of its own store that name others
/// by a path (`read_for_removal`). Now and then the reference also names an
/// id no instance has, with which the whole write refuses that command.
fn through_namesakes(rng: &mut SimRng, stores: &mut BTreeMap<&str, SettingsBytecode>) {
    for (service, document) in stores.iter_mut() {
        if rng.below(3) == 0 {
            continue;
        }
        let row = NEAR - 1;
        let to = (0..document.instances.len())
            .map(|at| Some(at + usize::from(at >= row)))
            .collect::<Vec<_>>();
        for instance in &mut document.instances {
            instance.parent_index = instance.parent_index.and_then(|parent| to[parent]);
            for value in instance.properties.values_mut() {
                remap_ref_indices_in_value(value, &to);
            }
        }
        let holders = (0..row)
            .filter(|holder| document.instances[*holder].class_name != "Camera")
            .collect::<Vec<_>>();
        let (name, parent) = (format!("{service}Twin"), holders[rng.below(holders.len())]);
        let twin = settings_instance(format!("{service}-twin"), &name, "Folder", Some(parent));
        document.instances.insert(row, twin);
        let behind = document.instances.len();
        let instances = &mut document.instances;
        instances.push(settings_instance(
            format!("{service}-behind"),
            name,
            "Folder",
            Some(parent),
        ));
        instances.push(settings_instance(
            format!("{service}-ghost"),
            "Ghost",
            "Folder",
            Some(behind),
        ));
        let path = build_editor_instance_paths_for_indices(document, service, &[behind + 1]);
        let path = path[&(behind + 1)].clone();
        let mut ordinals = path.path_ordinals;
        ordinals[path.path_segments.len() - 2] -= 1;
        let mut fields = json!({"pathSegments": path.path_segments, "pathOrdinals": ordinals});
        if rng.below(2) == 0 {
            fields["settingsId"] = json!("gone");
        }
        let holder = holders[rng.below(holders.len())];
        let properties = &mut document.instances[holder].properties;
        properties.insert("Through".to_string(), reference(fields));
    }
}

/// Gives each service root the id a pull writes it under, `1`
/// (`settings_binary_id`), before the references that name roots by their
/// ids are drawn.
pub(super) fn pulled_roots(stores: &mut BTreeMap<&str, SettingsBytecode>) {
    for document in stores.values_mut() {
        document.instances[0].settings_id = "1".to_string();
    }
}

/// Gives some instances values a pull writes as Studio gives them and a
/// whole encode settles: TextScaled on beside TextWrapped off or absent, and
/// an attribute in a wrapped form.
pub(super) fn pulled_values(rng: &mut SimRng, stores: &mut BTreeMap<&str, SettingsBytecode>) {
    let instances = stores.values_mut().flat_map(|store| &mut store.instances);
    for instance in instances.filter(|instance| instance.class_name != "Camera") {
        if rng.below(6) == 0 {
            let properties = &mut instance.properties;
            properties.insert("TextScaled".to_string(), json!(true));
            if rng.below(2) == 0 {
                properties.insert("TextWrapped".to_string(), json!(false));
            }
        }
        if rng.below(6) == 0 {
            let hue = json!({"BrickColor": 1 + rng.below(40)});
            instance.attributes.insert("Hue".to_string(), hue);
        }
    }
}

/// Gives now and then an instance the name of the first child of its parent
/// in another letter case, so that the two take file stems that differ only
/// in case.
fn case_twins(rng: &mut SimRng, stores: &mut BTreeMap<&str, SettingsBytecode>) {
    for document in stores.values_mut() {
        let mut first = BTreeMap::new();
        let instances = document.instances.iter_mut();
        for instance in instances.filter(|instance| instance.class_name != "Camera") {
            let Some(parent) = instance.parent_index else {
                continue;
            };
            match first.get(&parent) {
                None => {
                    first.insert(parent, instance.name.clone());
                }
                Some(name) if rng.below(4) == 0 => instance.name = other_case(name),
                Some(_) => {}
            }
        }
    }
}

fn other_case(name: &str) -> String {
    match name.to_lowercase() {
        lower if lower == name => name.to_uppercase(),
        lower => lower,
    }
}

/// Writes for about half the scripts of `stores` a file of its own text in
/// both `projects`, a third of them `.lua` files.
fn script_files(
    rng: &mut SimRng,
    stores: &BTreeMap<&str, SettingsBytecode>,
    projects: [&Fixture; 2],
) {
    for (service, document) in stores {
        let service_dir = projects[0].service_dir(service);
        let paths = build_editor_source_paths_by_index(document, service, &service_dir);
        for (row, path) in paths.into_iter().enumerate() {
            let Some(path) = path.filter(|_| rng.below(2) == 0) else {
                continue;
            };
            let path = match rng.below(3) {
                0 => path.with_extension("lua"),
                _ => path,
            };
            let relative = path.strip_prefix(&projects[0].root).unwrap();
            for project in projects {
                let path = project.root.join(relative);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, format!("return \"{service} {row}\"\n")).unwrap();
            }
        }
    }
}

pub(super) fn project(tag: &str) -> Fixture {
    let root = temp_dir(tag);
    let config = root.join("renium.project.jsonc");
    fs::write(&config, "{\n  \"schemaVersion\": 1\n}\n").unwrap();
    crate::project::config::load_project(Some(&config), None).unwrap();
    Fixture { root }
}

/// The files of a project under `folder`, each by its path from the root of
/// the project as the file system spells it, with its content.
fn files_of(fixture: &Fixture, folder: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut folders = vec![folder.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else {
                let name = path.strip_prefix(&fixture.root).unwrap().to_string_lossy();
                files.insert(name.replace('\\', "/"), fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// One command of kind `kind` on instances drawn from the oracle's stores.
pub(super) fn command(
    rng: &mut SimRng,
    oracle: &Fixture,
    kind: usize,
    root: &str,
) -> Option<Vec<String>> {
    let stored = |service: &&str| oracle.store(service).is_file();
    let services = SERVICES.into_iter().filter(stored).collect::<Vec<_>>();
    let service = rng.pick(&services)?;
    let document = oracle.read_store(service);
    let mut id = || {
        let near = rng.below(2) == 0;
        let rows = document.instances.len();
        let row = rng.below(if near { rows.min(NEAR) } else { rows });
        document.instances[row].settings_id.clone()
    };
    let (first, second) = (id(), id());
    let name = NAMES[NAMES.len() - 1 - rng.below(2) * rng.below(NAMES.len())];
    let other = SERVICES[rng.below(SERVICES.len())];
    let class = CLASSES[rng.below(CLASSES.len())];
    let args: Vec<&str> = match kind {
        0 => vec!["cr", service, "-c", class, "-n", name, "-I", &first],
        1 => vec!["cp", service, "-i", &first, "-I", &second],
        2 => vec!["cp", service, "-i", &first, "-I", &second, "-n", name],
        3 => vec!["rn", service, "-i", &first, name],
        4 => vec!["mv", service, "-i", &first, "-I", &second],
        5 => vec!["rm", service, "-i", &first],
        6 => vec!["rm", service, "-i", &first, "-R"],
        _ => vec!["mv", service, "-i", &first, "--to-service", other],
    };
    let args = args.into_iter().chain(["-r", root]);
    Some(args.map(str::to_string).collect())
}

/// A named copy or a rename `command` drew, given half the time the name of
/// the instance it copies or renames in another letter case; and whether the
/// name it gives is not that instance's.
fn in_other_case(
    rng: &mut SimRng,
    oracle: &Fixture,
    kind: usize,
    mut args: Vec<String>,
) -> (Vec<String>, bool) {
    // Where `command` puts the name.
    let at = match kind {
        2 => 7,
        3 => 4,
        _ => return (args, false),
    };
    let document = oracle.read_store(&args[1]);
    let named = (document.instances.iter()).find(|instance| instance.settings_id == args[3]);
    let name = named.map(|instance| instance.name.clone()).unwrap();
    if rng.below(2) == 0 {
        args[at] = other_case(&name);
    }
    let renamed = args[at] != name;
    (args, renamed)
}

/// Holds what a command answered about the files it changed to the files of
/// its project `before` and `after` it: each path it names as changed holds
/// other bytes, or none, and each script file it names as copied holds the
/// bytes its source held. Gives the paths it names as changed, from the root
/// of the project, and how many script files it copied.
///
/// A path is read as spelled and as a file system that ignores the case of
/// directories finds it, and it changed if it did either way: a file
/// written under a directory name that differs only in case from one left
/// by an earlier edit lands in that directory as it is spelled.
fn answered_files(
    outcome: &Outcome,
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> Result<(Vec<String>, usize), String> {
    let found = |path: &str| match path.rsplit_once('/') {
        Some((directory, name)) => format!("{}/{name}", directory.to_ascii_lowercase()),
        None => path.to_string(),
    };
    let found_before = (before.iter())
        .map(|(path, bytes)| (found(path), bytes))
        .collect::<BTreeMap<_, _>>();
    let found_after = (after.iter())
        .map(|(path, bytes)| (found(path), bytes))
        .collect::<BTreeMap<_, _>>();
    let listed = |field: &str| {
        let list = outcome.value.get(field).and_then(Value::as_array);
        list.into_iter().flatten()
    };
    let relative = |path: &Value| {
        let path = path.as_str().unwrap_or_default();
        path.strip_prefix("<root>/").unwrap_or(path).to_string()
    };
    let changed = listed("changedPaths").map(relative).collect::<Vec<_>>();
    let same = |path: &str| {
        before.get(path) == after.get(path)
            && found_before.get(&found(path)) == found_after.get(&found(path))
    };
    if let Some(path) = changed.iter().find(|path| same(path)) {
        return Err(format!("{path} is named as changed but holds what it held"));
    }
    let copies = listed("sourceCopies").collect::<Vec<_>>();
    for copy in &copies {
        let (from, to) = (relative(&copy["from"]), relative(&copy["to"]));
        let source = (before.get(&from)).or_else(|| found_before.get(&found(&from)).copied());
        let copied = (after.get(&to)).or_else(|| found_after.get(&found(&to)).copied());
        if source.is_none() || copied != source {
            return Err(format!("the copy {to} does not hold what {from} held"));
        }
    }
    Ok((changed, copies.len()))
}

/// One `ba -J` batch on instances drawn from the oracle's stores: adds, each
/// under an instance or an earlier add, and with `restructure` a rename, a
/// move or a removal of an instance or an earlier add, at any place among
/// them.
fn batch(rng: &mut SimRng, oracle: &Fixture, restructure: bool) -> Option<Vec<String>> {
    let stored = |service: &&str| oracle.store(service).is_file();
    let services = SERVICES.into_iter().filter(stored).collect::<Vec<_>>();
    let documents = (services.into_iter())
        .map(|service| (service, oracle.read_store(service)))
        .collect::<Vec<_>>();
    if documents.is_empty() {
        return None;
    }
    let near = |rng: &mut SimRng, at: usize| {
        let (service, document) = &documents[at];
        let rows = match rng.below(2) {
            0 => document.instances.len().min(NEAR),
            _ => document.instances.len(),
        };
        (
            *service,
            document.instances[rng.below(rows)].settings_id.clone(),
        )
    };
    // An instance of a store, or one of the adds `added` made, by its store.
    let target = |rng: &mut SimRng, added: &[usize]| match rng.below(3) {
        0 if !added.is_empty() => {
            let add = rng.below(added.len());
            (documents[added[add]].0, format!("#a{add}"))
        }
        _ => {
            let at = rng.below(documents.len());
            near(rng, at)
        }
    };
    let mut ops = Vec::new();
    let mut added = Vec::new();
    for add in 0..1 + rng.below(3) {
        let (service, parent) = target(rng, &added);
        let name = NAMES[NAMES.len() - 1 - rng.below(2) * rng.below(NAMES.len())];
        let class = CLASSES[rng.below(CLASSES.len())];
        ops.push(json!({
            "service": service,
            "parent": parent,
            "name": name,
            "class": class,
            "ref": format!("a{add}"),
        }));
        added.push(documents.iter().position(|(held, _)| *held == service)?);
    }
    if restructure {
        let at = rng.below(ops.len() + 1);
        let (service, path) = target(rng, &added[..at]);
        let store = documents.iter().position(|(held, _)| *held == service)?;
        let mut op = match rng.below(3) {
            0 => json!({"op": "rename", "name": NAMES[rng.below(NAMES.len())]}),
            1 => json!({"op": "move", "parent": near(rng, store).1}),
            _ => json!({"op": "remove"}),
        };
        op["service"] = json!(service);
        op["path"] = json!(path);
        ops.insert(at, op);
    }
    Some(vec![
        "ba".to_string(),
        "-J".to_string(),
        json!(ops).to_string(),
    ])
}

/// Exports the subtree of an instance as a model and imports it elsewhere.
fn model_round_trip(rng: &mut SimRng, oracle: &Fixture, step: usize) -> Option<Vec<Vec<String>>> {
    let service = "ReplicatedStorage";
    let document = (oracle.store(service).is_file()).then(|| oracle.read_store(service))?;
    let rows = document
        .instances
        .len()
        .checked_sub(1)
        .filter(|rows| *rows > 0)?;
    let row = 1 + rng.below(rows);
    let (source, parent) = (&document.instances[row], &document.instances[row / 2]);
    let model = format!("<root>/model-{step}.rbxm");
    let export = [
        "bem",
        "-f",
        "<store>",
        "-i",
        &source.settings_id,
        "-o",
        &model,
    ];
    let import = [
        "mip",
        service,
        "-I",
        &parent.settings_id,
        "-m",
        &model,
        "-r",
        "<root>",
    ];
    let owned = |args: &[&str]| args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    Some(vec![owned(&export), owned(&import)])
}

/// What a command printed, without the stores among the files it changed:
/// a whole write of a version 11 store changes its file whatever the edit.
/// An import lists the sources it wrote in no fixed order.
pub(super) fn printed(mut outcome: Outcome) -> (Option<String>, Value) {
    if let Some(Value::Array(changed)) = outcome.value.get_mut("changedPaths") {
        changed.retain(|path| !path.as_str().is_some_and(|path| path.ends_with(".renium")));
    }
    if let Some(Value::Array(sources)) = outcome.value.get_mut("sourceWrites") {
        sources.sort_by_key(Value::to_string);
    }
    (outcome.error, outcome.value)
}

/// The first instance two documents hold otherwise.
fn difference(expected: &SettingsBytecode, read: &SettingsBytecode) -> String {
    let shown = |instance: Option<&SettingsBytecodeInstance>| {
        instance.map(|instance| {
            json!([
                instance.settings_id,
                instance.name,
                instance.parent_index,
                instance.properties,
                instance.attributes
            ])
        })
    };
    let rows = expected.instances.len().max(read.instances.len());
    (0..rows)
        .map(|row| {
            (
                row,
                shown(expected.instances.get(row)),
                shown(read.instances.get(row)),
            )
        })
        .find(|(_, expected, read)| expected != read)
        .map_or_else(String::new, |(row, expected, read)| {
            format!("instance {row}: whole {expected:?}, in blocks {read:?}")
        })
}

pub(super) fn run_on(fixture: &Fixture, args: &[String]) -> Outcome {
    let root = fixture.root.to_string_lossy().into_owned();
    let store = fixture.store_arg("ReplicatedStorage");
    let args = (args.iter())
        .map(|arg| arg.replace("<root>", &root).replace("<store>", &store))
        .collect::<Vec<_>>();
    // `ba -J` finds the stores its ops name by their services in the project.
    let _project = (args.first().is_some_and(|command| command == "ba")).then(|| {
        crate::app::context::select_automation_for_thread(None, fixture.root.clone(), None)
    });
    fixture.json(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

fn run_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = SimRng(seed ^ 0x57_0e5);
    let block_rows = [2, 3, 5, 8, 13, 4096][rng.below(6)];
    let mut stores = BTreeMap::new();
    for service in SERVICES {
        let rows = 12 + rng.below(50);
        stores.insert(service, tree(&mut rng, service, rows));
    }
    case_twins(&mut rng, &mut stores);
    let writer = Writer::drawn(&mut rng);
    if writer == Writer::Pull {
        pulled_roots(&mut stores);
    }
    fill(&mut rng, &mut stores);
    through_namesakes(&mut rng, &mut stores);
    if writer == Writer::Pull {
        pulled_values(&mut rng, &mut stores);
    }
    let (oracle, sparse) = projects(&stores, block_rows, writer)?;
    script_files(&mut rng, &stores, [&oracle, &sparse]);
    log.push(format!(
        "blocks of {block_rows} rows, written by {writer:?}"
    ));
    for step in 0..STEPS {
        let kind = rng.below(KINDS.len() + MORE.len());
        let mut renamed = false;
        let commands = match kind.checked_sub(KINDS.len()) {
            Some(0) => model_round_trip(&mut rng, &oracle, step),
            Some(more) => batch(&mut rng, &oracle, more == 2).map(|batch| vec![batch]),
            None => command(&mut rng, &oracle, kind, "<root>").map(|command| {
                let (command, renames) = in_other_case(&mut rng, &oracle, kind, command);
                renamed = renames;
                vec![command]
            }),
        };
        let before = SERVICES.map(|service| fs::read(sparse.store(service)).ok());
        let mut went = commands.is_some();
        for args in commands.into_iter().flatten() {
            log.push(format!("step {step}: {}", args.join(" ")));
            let whole = printed(run_on(&oracle, &args));
            let files = files_of(&sparse, &sparse.root);
            let outcome = run_on(&sparse, &args);
            let answered = answered_files(&outcome, &files, &files_of(&sparse, &sparse.root));
            let (changed, copies) = answered.map_err(|error| format!("step {step}: {error}"))?;
            let blocks = printed(outcome);
            if whole != blocks {
                return Err(format!(
                    "step {step}: the whole stores gave {whole:?}, the stores in blocks {blocks:?}"
                ));
            }
            log.extend(whole.0.iter().map(|error| format!("  refused: {error}")));
            went &= whole.0.is_none();
            let case_move = |path: &String| {
                (changed.iter()).any(|other| other != path && other.eq_ignore_ascii_case(path))
            };
            let covered = [
                renamed && kind == 2 && copies > 0,
                changed.iter().any(|path| path.ends_with(".lua")),
                changed.iter().any(case_move),
            ];
            for (count, seen) in COVERED.iter().zip(covered) {
                count.fetch_add(usize::from(seen && whole.0.is_none()), Ordering::Relaxed);
            }
        }
        DONE[kind].fetch_add(usize::from(went), Ordering::Relaxed);
        let restructured = kind == KINDS.len() + 2;
        let held = same_projects(&oracle, &sparse, before, (writer, restructured));
        let apart = held.map_err(|error| format!("step {step}: {error}"))?;
        APART.fetch_add(apart, Ordering::Relaxed);
    }
    Ok(())
}

/// Two projects of `stores`: one whose stores are of version 11, the oracle,
/// and one whose stores are in blocks of `block_rows` rows, both as `writer`
/// wrote them; a pull's two decode to one document.
pub(super) fn projects(
    stores: &BTreeMap<&str, SettingsBytecode>,
    block_rows: usize,
    writer: Writer,
) -> Result<(Fixture, Fixture), String> {
    let (oracle, sparse) = (project("edits-whole"), project("edits-blocks"));
    let failed = |error: anyhow::Error| format!("{error:#}");
    for (service, document) in stores {
        let (whole, blocks) = match writer {
            Writer::Pull => (
                pulled_v11(document).map_err(failed)?,
                pulled_in_blocks(document, block_rows).map_err(failed)?,
            ),
            _ => (
                (FORMATS[0].encode)(document).map_err(failed)?,
                in_blocks(document, block_rows).map_err(failed)?,
            ),
        };
        let blocks = match writer {
            Writer::Foreign => foreign_file(&blocks),
            _ => blocks,
        };
        if writer == Writer::Pull {
            let mut held = decode_settings_bytecode(&whole).map_err(failed)?;
            held.version = FORMATS[1].version;
            let pulled = decode_settings_bytecode(&blocks).map_err(failed)?;
            if !same_document(&held, &pulled) {
                let difference = difference(&held, &pulled);
                return Err(format!(
                    "the pulled {service} stores differ at {difference}"
                ));
            }
        }
        fs::create_dir_all(oracle.store(service).parent().unwrap()).unwrap();
        fs::create_dir_all(sparse.store(service).parent().unwrap()).unwrap();
        fs::write(oracle.store(service), whole).unwrap();
        fs::write(sparse.store(service), blocks).unwrap();
    }
    Ok((oracle, sparse))
}

/// Holds the project in blocks to the oracle after a command ran on both:
/// the same files beside the stores, stores of the same documents, and each
/// store in blocks the whole encode of its document; for stores a pull wrote,
/// the same documents up to the values the two encoders write apart, and
/// each block the whole encode's or the one the store held before
/// (`pulled_alike`). `before` holds the stores in blocks as they were;
/// `cut_whole` where the command writes the stores it changes as a whole
/// encode cuts them (`encode_settings_bytecode`), not in the blocks of their
/// files: a `ba -J` batch that restructures reads and writes its stores
/// whole. The stores of the oracle are of version 11 again after it, and
/// hold what the stores in blocks hold, as a pull writes it, where a pull
/// wrote those. Gives how many stores in blocks hold values apart from the
/// whole path's.
pub(super) fn same_projects(
    oracle: &Fixture,
    sparse: &Fixture,
    before: [Option<Vec<u8>>; SERVICES.len()],
    (writer, cut_whole): (Writer, bool),
) -> Result<usize, String> {
    let failed = |error: anyhow::Error| format!("{error:#}");
    let mut apart = 0;
    if files_of(oracle, &oracle.root.join("src")) != files_of(sparse, &sparse.root.join("src")) {
        return Err("the files beside the stores differ".into());
    }
    for (service, before) in SERVICES.into_iter().zip(before) {
        let (whole, blocks) = (oracle.store(service), sparse.store(service));
        if whole.is_file() != blocks.is_file() {
            return Err(format!("only one {service} store is left"));
        }
        let (Some(before), true) = (before, whole.is_file()) else {
            continue;
        };
        // A store the oracle's command left alone is of version 11 still.
        let mut expected = oracle.read_store(service);
        expected.version = FORMATS[1].version;
        let rows = (0..expected.instances.len()).collect::<Vec<_>>();
        let mut selected = build_editor_instance_paths_for_indices(&expected, service, &rows);
        let parts = |path: Option<EditorInstancePath>| {
            path.map(|path| (path.path_segments, path.path_ordinals))
        };
        let mut every = build_editor_instance_paths(&expected, service)
            .into_iter()
            .enumerate();
        if every.any(|(row, path)| parts(path) != parts(selected.remove(&row))) {
            return Err(format!("the paths of the {service} instances differ"));
        }
        let written = fs::read(&blocks).unwrap();
        let read = sparse.read_store(service);
        match writer {
            Writer::Pull => {
                pulled_alike(&read, &expected, service)
                    .map_err(|error| format!("the {service} stores differ: {error}"))?;
                let held = (
                    without_source_markers(&expected),
                    without_source_markers(&read),
                );
                apart += usize::from(!same_document(&held.0, &held.1));
                // The whole path writes the markers the oracle's last pull lost.
                expected = with_source_markers_of(&expected, &read);
            }
            _ if !same_document(&expected, &read) => {
                let difference = difference(&expected, &read);
                return Err(format!("the {service} stores differ at {difference}"));
            }
            _ => {}
        }
        // A file the command left alone may be another writer's.
        if written != before {
            let encoded = match cut_whole {
                true => (FORMATS[1].encode)(&expected),
                false => in_blocks(&expected, block_rows_of(&before)),
            };
            let encoded = encoded.map_err(failed)?;
            let differing = match writer {
                Writer::Pull => block_apart(&written, &encoded, &before),
                _ => (encoded != written).then_some(0),
            };
            if let Some(block) = differing {
                return Err(format!(
                    "the {service} store is not the whole encode of its document at block {block}"
                ));
            }
        }
        // The oracle's next command reads its store whole again.
        let whole_again = match writer {
            Writer::Pull => pulled_v11(&read).map_err(failed)?,
            _ => (FORMATS[0].encode)(&expected).map_err(failed)?,
        };
        if writer == Writer::Pull {
            let mut held = decode_settings_bytecode(&whole_again).map_err(failed)?;
            held.version = read.version;
            let read = without_source_markers(&read);
            if !same_document(&read, &held) {
                let difference = difference(&read, &held);
                return Err(format!(
                    "a pull of the {service} store differs at {difference}"
                ));
            }
        }
        fs::write(&whole, whole_again).unwrap();
    }
    Ok(apart)
}

/// A reference by a path through the first of two namesakes, to a child only
/// the second holds, in a block ahead of both: it names nothing until an
/// edit takes the first away, removing, renaming or moving it, and then the
/// whole write gives the child by position; with an id beside the path that
/// no instance has, the whole write refuses the edit. Each edit of the
/// stores in blocks prints and writes what it does of the whole stores.
#[test]
fn a_path_through_a_namesake_an_edit_takes_away_matches_whole_documents() {
    let store = |id: Option<&str>| {
        let mut fields = json!({
            "pathSegments": ["ReplicatedStorage", "A", "P"],
            "pathOrdinals": [1, 1, 1],
        });
        if let Some(id) = id {
            fields["settingsId"] = json!(id);
        }
        let mut holder = settings_instance("holder", "Holder", "ObjectValue", Some(0));
        holder.properties.insert("Value".into(), reference(fields));
        settings_document(vec![
            settings_instance("root", "ReplicatedStorage", "ReplicatedStorage", None),
            holder,
            settings_instance("filler", "Filler", "Folder", Some(0)),
            settings_instance("other", "Other", "Folder", Some(0)),
            settings_instance("first", "A", "Folder", Some(0)),
            settings_instance("inside", "Q", "Folder", Some(4)),
            settings_instance("second", "A", "Folder", Some(0)),
            settings_instance("target", "P", "Folder", Some(6)),
        ])
    };
    let workspace = settings_document(vec![settings_instance(
        "workspace",
        "Workspace",
        "Workspace",
        None,
    )]);
    let edits: [&[&str]; 5] = [
        &["rm", "ReplicatedStorage", "-i", "first"],
        &["rm", "ReplicatedStorage", "-i", "first", "-R"],
        &["rn", "ReplicatedStorage", "-i", "first", "B"],
        &["mv", "ReplicatedStorage", "-i", "first", "-I", "filler"],
        &[
            "mv",
            "ReplicatedStorage",
            "-i",
            "first",
            "--to-service",
            "Workspace",
        ],
    ];
    for (id, edit) in [None, Some("gone")]
        .into_iter()
        .flat_map(|id| edits.map(|edit| (id, edit)))
    {
        let stores = BTreeMap::from([
            ("ReplicatedStorage", store(id)),
            ("Workspace", workspace.clone()),
        ]);
        let (oracle, sparse) = projects(&stores, 2, Writer::Whole).unwrap();
        let args = (edit.iter().chain(&["-r", "<root>"]))
            .map(|arg| arg.to_string())
            .collect::<Vec<_>>();
        let before = SERVICES.map(|service| fs::read(sparse.store(service)).ok());
        let whole = printed(run_on(&oracle, &args));
        assert_eq!(
            printed(run_on(&sparse, &args)),
            whole,
            "{edit:?} beside {id:?}"
        );
        same_projects(&oracle, &sparse, before, (Writer::Whole, false)).unwrap();
        let holder = oracle.read_store("ReplicatedStorage").instances[1].clone();
        match id {
            None => assert!(holder.properties["Value"].get("instanceIndex").is_some()),
            Some(_) => assert!(whole.0.is_some_and(|error| error.contains("mix local"))),
        }
    }
}

/// A rename that changes only the letter case of a folder moves the script
/// files in it to the new spelling, the folder with them, and names each
/// file it moved as changed.
#[test]
fn a_rename_in_another_case_moves_the_files_of_a_folder() {
    let document = settings_document(vec![
        settings_instance("root", "Workspace", "Workspace", None),
        settings_instance("folder", "Model", "Folder", Some(0)),
        settings_instance("a", "A", "ModuleScript", Some(1)),
        settings_instance("b", "B", "ModuleScript", Some(1)),
    ]);
    let (fixture, _) =
        projects(&BTreeMap::from([("Workspace", document)]), 2, Writer::Whole).unwrap();
    for row in [2, 3] {
        fixture.write_source("Workspace", row, &format!("return {row}\n"));
    }
    let before = files_of(&fixture, &fixture.root);
    let args = ["rn", "Workspace", "-i", "folder", "model", "-r", "<root>"].map(str::to_string);
    let outcome = run_on(&fixture, &args);
    assert_eq!(outcome.error, None);
    let after = files_of(&fixture, &fixture.root);
    let (changed, _) = answered_files(&outcome, &before, &after).unwrap();
    for name in ["A", "B"] {
        let (from, to) = (
            format!("src/Workspace/Model/{name}.luau"),
            format!("src/Workspace/model/{name}.luau"),
        );
        assert_eq!(after.get(&to), before.get(&from), "{after:?}");
        assert!(
            changed.contains(&from) && changed.contains(&to),
            "{changed:?}"
        );
    }
}

#[test]
fn structure_edits_of_stores_in_blocks_match_whole_documents() {
    run_seeds(SEEDS, run_seed);
    if std::env::var_os("RENIUM_SIM_SEED").is_some() {
        return;
    }
    for (name, done) in KINDS.into_iter().chain(MORE).zip(&DONE) {
        assert!(done.load(Ordering::Relaxed) > 0, "no {name} went through");
    }
    for (name, covered) in COVERAGE.into_iter().zip(&COVERED) {
        assert!(
            covered.load(Ordering::Relaxed) > 0,
            "no {name} went through"
        );
    }
    let apart = APART.load(Ordering::Relaxed);
    assert!(
        apart > 0,
        "no store in blocks held values the whole path settled"
    );
}
