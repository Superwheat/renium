//! Seeded runs of the commands that add, copy, import, rename, move and
//! remove instances, each on two copies of one generated project: one whose
//! stores are of version 11, which those commands read and write whole, and
//! one whose stores are in small blocks, which they read by their instances
//! and write block by block. The first is the oracle. After every command
//! the two must have printed the same, hold the same files beside their
//! stores, and hold stores that decode to the same documents; and each store
//! in blocks must be the file a whole encode of its document gives.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::editor::paths::{build_editor_instance_paths, build_editor_instance_paths_for_indices};
use crate::editor::types::EditorInstancePath;
use crate::settings::bytecode::blocks::sim::{
    FORMATS, block_rows_of, foreign_file, in_blocks, same_document,
};
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
/// How often each kind of command, and the import of a model, went through.
static DONE: [AtomicUsize; KINDS.len() + 1] = [const { AtomicUsize::new(0) }; KINDS.len() + 1];

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

pub(super) fn project(tag: &str) -> Fixture {
    let root = temp_dir(tag);
    let config = root.join("renium.project.jsonc");
    fs::write(&config, "{\n  \"schemaVersion\": 1\n}\n").unwrap();
    crate::project::config::load_project(Some(&config), None).unwrap();
    Fixture { root }
}

/// The files of a project beside its stores, each with its content.
fn files_of(fixture: &Fixture) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut folders = vec![fixture.root.join("src")];
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
    fill(&mut rng, &mut stores);
    through_namesakes(&mut rng, &mut stores);
    // Now and then another writer wrote the stores, which are then written
    // whole on their first edit.
    let foreign = rng.below(5) == 0;
    let (oracle, sparse) = projects(&stores, block_rows, foreign)?;
    log.push(format!("blocks of {block_rows} rows, foreign: {foreign}"));
    for step in 0..STEPS {
        let kind = rng.below(KINDS.len() + 1);
        let commands = match kind == KINDS.len() {
            true => model_round_trip(&mut rng, &oracle, step),
            false => command(&mut rng, &oracle, kind, "<root>").map(|command| vec![command]),
        };
        let before = SERVICES.map(|service| fs::read(sparse.store(service)).ok());
        let mut went = commands.is_some();
        for args in commands.into_iter().flatten() {
            log.push(format!("step {step}: {}", args.join(" ")));
            let whole = printed(run_on(&oracle, &args));
            let blocks = printed(run_on(&sparse, &args));
            if whole != blocks {
                return Err(format!(
                    "step {step}: the whole stores gave {whole:?}, the stores in blocks {blocks:?}"
                ));
            }
            log.extend(whole.0.iter().map(|error| format!("  refused: {error}")));
            went &= whole.0.is_none();
        }
        DONE[kind].fetch_add(usize::from(went), Ordering::Relaxed);
        same_projects(&oracle, &sparse, before).map_err(|error| format!("step {step}: {error}"))?;
    }
    Ok(())
}

/// Two projects of `stores`: one whose stores are of version 11, the oracle,
/// and one whose stores are in blocks of `block_rows` rows, `foreign` where
/// another writer wrote them.
pub(super) fn projects(
    stores: &BTreeMap<&str, SettingsBytecode>,
    block_rows: usize,
    foreign: bool,
) -> Result<(Fixture, Fixture), String> {
    let (oracle, sparse) = (project("edits-whole"), project("edits-blocks"));
    for (service, document) in stores {
        let whole = (FORMATS[0].encode)(document).map_err(|error| format!("{error:#}"))?;
        let mut blocks = in_blocks(document, block_rows).map_err(|error| format!("{error:#}"))?;
        if foreign {
            blocks = foreign_file(&blocks);
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
/// store in blocks the whole encode of its document. `before` holds the
/// stores in blocks as they were. The stores of the oracle are of version
/// 11 again after it.
pub(super) fn same_projects(
    oracle: &Fixture,
    sparse: &Fixture,
    before: [Option<Vec<u8>>; SERVICES.len()],
) -> Result<(), String> {
    if files_of(oracle) != files_of(sparse) {
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
        if !same_document(&expected, &read) {
            let difference = difference(&expected, &read);
            return Err(format!("the {service} stores differ at {difference}"));
        }
        // A file the command left alone may be another writer's.
        let encoded = in_blocks(&expected, block_rows_of(&before));
        if written != before && encoded.map_err(|error| format!("{error:#}"))? != written {
            return Err(format!(
                "the {service} store is not the whole encode of its document"
            ));
        }
        // The oracle's next command reads its store whole again.
        let whole_again = (FORMATS[0].encode)(&expected);
        fs::write(&whole, whole_again.map_err(|error| format!("{error:#}"))?).unwrap();
    }
    Ok(())
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
        let (oracle, sparse) = projects(&stores, 2, false).unwrap();
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
        same_projects(&oracle, &sparse, before).unwrap();
        let holder = oracle.read_store("ReplicatedStorage").instances[1].clone();
        match id {
            None => assert!(holder.properties["Value"].get("instanceIndex").is_some()),
            Some(_) => assert!(whole.0.is_some_and(|error| error.contains("mix local"))),
        }
    }
}

#[test]
fn structure_edits_of_stores_in_blocks_match_whole_documents() {
    run_seeds(SEEDS, run_seed);
    if std::env::var_os("RENIUM_SIM_SEED").is_some() {
        return;
    }
    let names = KINDS.into_iter().chain(["import"]);
    for (name, done) in names.zip(&DONE) {
        assert!(done.load(Ordering::Relaxed) > 0, "no {name} went through");
    }
}
