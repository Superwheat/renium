//! Time and peak memory of bulk edits on a place of about 140,000 Workspace
//! instances, run by hand in an optimized build, one run per process:
//! `cargo test --profile fast bulk_bench_fixture -- --ignored`, then each
//! `bulk_bench_*` alone with `--exact --nocapture`. `RENIUM_BENCH_PROFILE=1`
//! prints the profile of the measured step.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde_json::{Value, json};

use crate::bytecode::add_batch::{apply_store_batch, parse_batch_ops};
use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs, PushEditorChangesArgs};
use crate::settings::EXTERNAL_SOURCE_MARKER;
use crate::settings::bytecode::SettingsBytecodeInstance;
use crate::system::files::service_settings_path;
use crate::tests::support::{settings_document, settings_instance};

const MODELS: usize = 1_000;
const PARTS_PER_MODEL: usize = 139;
const SERVER_FOLDERS: usize = 40;
const SERVER_SCRIPTS_PER_FOLDER: usize = 10;
const BULK_SCRIPTS: usize = 300;
const BULK_PARTS: usize = 500;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("renium-bulk-bench")
}

fn part(id: String, name: &str, parent: usize, at: usize) -> SettingsBytecodeInstance {
    let mut part = settings_instance(id, name, "Part", Some(parent));
    let x = (at % 997) as f64;
    for (property, value) in [
        ("Anchored", json!(true)),
        (
            "CFrame",
            json!({"_type": "CFrame", "components": [x, 4.0, -x, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]}),
        ),
        (
            "Size",
            json!({"_type": "Vector3", "x": 4.0, "y": 1.0 + (at % 7) as f64, "z": 2.0}),
        ),
        (
            "Color",
            json!({"_type": "Color3", "r": 0.5, "g": (at % 11) as f64 / 11.0, "b": 0.25}),
        ),
        (
            "Material",
            json!({"_type": "EnumItem", "enumType": "Material", "name": "Plastic"}),
        ),
        ("Transparency", json!(0.0)),
    ] {
        part.properties.insert(property.to_string(), value);
    }
    part
}

fn workspace_instances() -> Vec<SettingsBytecodeInstance> {
    let mut instances = vec![settings_instance("ws", "Workspace", "Workspace", None)];
    for model in 0..MODELS {
        let model_index = instances.len();
        instances.push(settings_instance(
            format!("editor:m{model:x}"),
            "Model",
            "Model",
            Some(0),
        ));
        for at in 0..PARTS_PER_MODEL {
            let name = if at % 3 == 0 { "Part" } else { "Wall" };
            instances.push(part(
                format!("editor:p{model:x}_{at:x}"),
                name,
                model_index,
                model * PARTS_PER_MODEL + at,
            ));
        }
        let mut value = settings_instance(
            format!("editor:v{model:x}"),
            "Target",
            "ObjectValue",
            Some(model_index),
        );
        value.properties.insert(
            "Value".to_string(),
            json!({
                "_type": "Ref",
                "settingsId": format!("editor:p{model:x}_0"),
                "pathSegments": ["Workspace", "Model", "Part"],
                "pathOrdinals": [1, model + 1, 1],
            }),
        );
        instances.push(value);
    }
    instances
}

fn script(id: String, name: &str, class: &str, parent: usize) -> SettingsBytecodeInstance {
    let mut script = settings_instance(id, name, class, Some(parent));
    script
        .properties
        .insert("Source".to_string(), json!(EXTERNAL_SOURCE_MARKER));
    script
}

/// ServerScriptService with `extra` scripts in a Bulk folder, each with its
/// file under `src`.
fn write_server(root: &Path, bulk: usize, source: &str) {
    let service_dir = root.join("src").join("ServerScriptService");
    let _ = fs::remove_dir_all(&service_dir);
    let mut instances = vec![settings_instance(
        "sss",
        "ServerScriptService",
        "ServerScriptService",
        None,
    )];
    let mut files = Vec::new();
    for folder in 0..SERVER_FOLDERS {
        let folder_index = instances.len();
        instances.push(settings_instance(
            format!("editor:f{folder:x}"),
            format!("Systems{folder}"),
            "Folder",
            Some(0),
        ));
        for at in 0..SERVER_SCRIPTS_PER_FOLDER {
            instances.push(script(
                format!("editor:s{folder:x}_{at:x}"),
                &format!("Module{at}"),
                "ModuleScript",
                folder_index,
            ));
            files.push(
                service_dir
                    .join(format!("Systems{folder}"))
                    .join(format!("Module{at}.luau")),
            );
        }
    }
    if bulk > 0 {
        let folder_index = instances.len();
        instances.push(settings_instance("editor:bulk", "Bulk", "Folder", Some(0)));
        for at in 0..bulk {
            instances.push(script(
                format!("editor:b{at:x}"),
                &format!("Script{at}"),
                "Script",
                folder_index,
            ));
        }
    }
    settings_document(instances)
        .write_file(&service_settings_path(&service_dir))
        .unwrap();
    for file in files {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "return {}\n").unwrap();
    }
    if bulk > 0 {
        write_bulk_files(root, source);
    }
}

fn bulk_files(root: &Path) -> Vec<PathBuf> {
    let folder = root.join("src").join("ServerScriptService").join("Bulk");
    (0..BULK_SCRIPTS)
        .map(|at| folder.join(format!("Script{at}.server.luau")))
        .collect()
}

fn write_bulk_files(root: &Path, source: &str) {
    for file in bulk_files(root) {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, source).unwrap();
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry.unwrap();
        let target = to.join(entry.path().strip_prefix(from).unwrap());
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).unwrap();
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A fresh copy of the fixture project.
fn project(tag: &str) -> PathBuf {
    let fixture = fixture_root();
    assert!(
        fixture.join("ready").is_file(),
        "run bulk_bench_fixture first"
    );
    let root = std::env::temp_dir().join(format!("renium-bulk-bench-{tag}"));
    let _ = fs::remove_dir_all(&root);
    copy_tree(&fixture, &root);
    crate::project::config::load_project(Some(&root.join("renium.project.jsonc")), None).unwrap();
    root
}

fn megabytes() -> (usize, usize) {
    let (mut elapsed, mut user, mut system, mut rss, mut peak_rss) = (0, 0, 0, 0, 0);
    let (mut commit, mut peak_commit, mut faults) = (0, 0, 0);
    unsafe {
        libmimalloc_sys::mi_process_info(
            &mut elapsed,
            &mut user,
            &mut system,
            &mut rss,
            &mut peak_rss,
            &mut commit,
            &mut peak_commit,
            &mut faults,
        );
    }
    (commit >> 20, peak_commit >> 20)
}

fn measure<T>(name: &str, run: impl FnOnce() -> T) -> T {
    if std::env::var_os("RENIUM_BENCH_PROFILE").is_some() {
        let cli = <crate::cli::Cli as clap::Parser>::try_parse_from([
            "rbx",
            "--log-level",
            "debug",
            "status",
        ])
        .unwrap();
        crate::app::output::configure(&cli);
    }
    let (before, _) = megabytes();
    let started = Instant::now();
    let result = run();
    let elapsed = started.elapsed();
    let (_, peak) = megabytes();
    if let Some(profile) = crate::app::profile::report(name, started) {
        eprintln!("{profile}");
    }
    eprintln!(
        "bench {name}: {:.0} ms, peak commit {peak} MB (held {before} MB before)",
        elapsed.as_secs_f64() * 1000.0
    );
    result
}

#[test]
#[ignore = "writes the bulk benchmark fixture"]
fn bulk_bench_fixture() {
    let root = fixture_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("renium.project.jsonc"),
        "{\n  \"schemaVersion\": 1\n}\n",
    )
    .unwrap();
    crate::project::config::load_project(Some(&root.join("renium.project.jsonc")), None).unwrap();
    settings_document(workspace_instances())
        .write_file(&service_settings_path(&root.join("src").join("Workspace")))
        .unwrap();
    write_server(&root, 0, "");
    fs::write(root.join("ready"), "").unwrap();
}

fn batch_ops(parent: &str) -> String {
    let mut ops =
        vec![json!({"parent": "Workspace", "name": "MemMany", "class": "Folder", "ref": "many"})];
    for at in 0..BULK_PARTS {
        ops.push(json!({
            "parent": parent,
            "name": format!("P{at}"),
            "class": "Part",
            "props": {"Anchored": true, "Transparency": 0.5},
        }));
    }
    json!({ "ops": ops }).to_string()
}

fn add_parts(root: &Path, parent: &str) -> Value {
    let ops = parse_batch_ops(&batch_ops(parent)).unwrap();
    let resolve = |service: &str| -> Result<(PathBuf, String)> {
        Ok((
            service_settings_path(&root.join("src").join(service)),
            service.to_string(),
        ))
    };
    apply_store_batch(ops, &resolve, false, false)
        .unwrap()
        .result
}

#[test]
#[ignore = "benchmark"]
fn bulk_bench_br_folder() {
    let root = project("br");
    add_parts(&root, "#many");
    let settings_file = service_settings_path(&root.join("src").join("Workspace"));
    measure("br", || {
        crate::bytecode::edit::bytecode_remove_instance(crate::cli::BytecodeRemoveInstanceArgs {
            where_query: None,
            push: false,
            more_targets: Vec::new(),
            input: crate::cli::BytecodeFileArgs::settings_file(settings_file.clone()),
            selector: crate::cli::BytecodeInstanceSelectorArgs {
                target: Some("Workspace.MemMany".to_string()),
                ..Default::default()
            },
            no_recursive: false,
            override_packages: false,
            pretty: false,
        })
        .unwrap();
    });
    let _ = fs::remove_dir_all(&root);
}

fn collect(root: &Path, changed: Vec<PathBuf>) -> crate::editor::types::EditorChangeSet {
    crate::editor::sync::collect_editor_changes(&PushEditorChangesArgs {
        changed_paths: changed,
        ..PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: root.to_path_buf(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.0),
        )
    })
    .unwrap()
}

/// Writes the history entries of `changes` as a push does once Studio gave
/// the sources it holds: none, or with `studio_sources` one for each entry.
fn write_history(
    root: &Path,
    name: &str,
    changes: &crate::editor::types::EditorChangeSet,
    studio_sources: bool,
) {
    let sources = (0..changes.history_entries.len())
        .filter(|_| studio_sources)
        .map(|at| (at, "print('before')\n".to_string()))
        .collect();
    let written = measure(name, || {
        crate::editor::history::write_editor_history_entries(
            root,
            &changes.history_entries,
            &sources,
            &std::collections::HashMap::new(),
        )
        .unwrap()
    });
    assert!(written.is_some());
}

/// What writing the files of 300 history entries costs, each entry in a new
/// directory, through the write that replaces a file in place, through a
/// plain write, and through plain writes of the entries side by side.
#[test]
#[ignore = "benchmark"]
fn bulk_bench_history_files() {
    use rayon::prelude::*;
    let backup = vec![7u8; 100_000];
    let manifest = vec![b'{'; 600];
    let write_entry = |root: &Path, entry: usize, plain: bool| {
        let dir = root.join(format!("{entry}-entry"));
        fs::create_dir_all(&dir).unwrap();
        for (file, bytes) in [("settings.renium", &backup), ("manifest.json", &manifest)] {
            if plain {
                fs::write(dir.join(file), bytes).unwrap();
            } else {
                crate::system::files::write_bytes_if_changed(&dir.join(file), bytes).unwrap();
            }
        }
    };
    for (name, plain, parallel) in [
        ("replacing writes", false, false),
        ("plain writes", true, false),
        ("parallel plain writes", true, true),
    ] {
        let root = std::env::temp_dir().join(format!("renium-bulk-bench-files-{plain}-{parallel}"));
        let _ = fs::remove_dir_all(&root);
        measure(name, || match parallel {
            true => (0..BULK_SCRIPTS)
                .into_par_iter()
                .for_each(|entry| write_entry(&root, entry, plain)),
            false => (0..BULK_SCRIPTS).for_each(|entry| write_entry(&root, entry, plain)),
        });
        let _ = fs::remove_dir_all(&root);
    }
}

fn changed_bulk_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![root.join("src").join("ServerScriptService").join("Bulk")];
    paths.extend(bulk_files(root));
    paths
}

/// New scripts, each with the history entry of its instance and that of its
/// Source.
#[test]
#[ignore = "benchmark"]
fn bulk_bench_scripts_created() {
    let root = project("created");
    write_bulk_files(&root, "print('new')\n");
    let changes = measure("created", || collect(&root, changed_bulk_paths(&root)));
    assert_eq!(changes.history_entries.len(), 2 * BULK_SCRIPTS);
    write_history(&root, "created history", &changes, false);
    let _ = fs::remove_dir_all(&root);
}

#[test]
#[ignore = "benchmark"]
fn bulk_bench_scripts_edited() {
    let root = project("edited");
    write_server(&root, BULK_SCRIPTS, "print('edited')\n");
    let changes = measure("edited", || collect(&root, changed_bulk_paths(&root)));
    assert_eq!(changes.source_changes.len(), BULK_SCRIPTS);
    write_history(&root, "edited history", &changes, true);
    let _ = fs::remove_dir_all(&root);
}

#[test]
#[ignore = "benchmark"]
fn bulk_bench_scripts_deleted() {
    let root = project("deleted");
    write_server(&root, BULK_SCRIPTS, "print('gone')\n");
    let changed = changed_bulk_paths(&root);
    fs::remove_dir_all(root.join("src").join("ServerScriptService").join("Bulk")).unwrap();
    let changes = measure("deleted", || collect(&root, changed));
    assert_eq!(changes.history_entries.len(), BULK_SCRIPTS);
    write_history(&root, "deleted history", &changes, false);
    let _ = fs::remove_dir_all(&root);
}

/// Adds a Bulk folder of Scripts to the Workspace, and ObjectValues that
/// refer to them: one to the last, or with `every` one to each. With
/// `children`, each script holds a Folder, so its file is the `init` file of
/// its own folder. Returns the Bulk folder's directory, which holds none of
/// their files.
fn write_bulk_workspace(root: &Path, every: bool, children: bool) -> PathBuf {
    let mut instances = workspace_instances();
    let bulk = instances.len();
    instances.push(settings_instance("editor:bulk", "Bulk", "Folder", Some(0)));
    for at in 0..BULK_SCRIPTS {
        let name = format!("Script{at:04}");
        let script_index = instances.len();
        instances.push(script(format!("editor:b{at:x}"), &name, "Script", bulk));
        if children {
            let id = format!("editor:c{at:x}");
            instances.push(settings_instance(id, "Child", "Folder", Some(script_index)));
        }
    }
    let referred = if every { 0 } else { BULK_SCRIPTS - 1 };
    for at in referred..BULK_SCRIPTS {
        let id = format!("editor:link{at:x}");
        let mut link = settings_instance(id, "Link", "ObjectValue", Some(0));
        let value = json!({"_type": "Ref", "settingsId": format!("editor:b{at:x}")});
        link.properties.insert("Value".to_string(), value);
        instances.push(link);
    }
    let workspace = root.join("src").join("Workspace");
    settings_document(instances)
        .write_file(&service_settings_path(&workspace))
        .unwrap();
    workspace.join("Bulk")
}

/// The repairs of the ObjectValues that refer to the Bulk scripts.
fn link_repairs(changes: &crate::editor::types::EditorChangeSet) -> usize {
    (changes.property_changes.iter())
        .filter(|change| {
            (change.settings_id.as_deref()).is_some_and(|id| id.starts_with("editor:link"))
        })
        .count()
}

/// The Workspace's Bulk scripts deleted, where the last cannot join the run
/// of the others as something refers to it, or with `every`, where
/// something refers to each, so each goes alone.
fn scripts_deleted(name: &str, every: bool) {
    let root = project(&name.replace(' ', "-"));
    let folder = write_bulk_workspace(&root, every, false);
    let mut changed = vec![folder.clone()];
    changed.extend((0..BULK_SCRIPTS).map(|at| folder.join(format!("Script{at:04}.server.luau"))));
    let changes = measure(name, || collect(&root, changed));
    assert_eq!(changes.history_entries.len(), BULK_SCRIPTS);
    assert_eq!(link_repairs(&changes), if every { BULK_SCRIPTS } else { 1 });
    write_history(&root, &format!("{name} history"), &changes, false);
    let _ = fs::remove_dir_all(&root);
}

#[test]
#[ignore = "benchmark"]
fn bulk_bench_scripts_deleted_blocked() {
    scripts_deleted("deleted blocked", false);
}

#[test]
#[ignore = "benchmark"]
fn bulk_bench_scripts_deleted_referenced() {
    scripts_deleted("deleted referenced", true);
}

/// The Workspace's Bulk scripts renamed from `.server.luau` to
/// `.client.luau`, which makes each a LocalScript with the history entries
/// of its instance and its Source. With `every`, something refers to each.
fn scripts_renamed(name: &str, every: bool) {
    let root = project(&name.replace(' ', "-"));
    let folder = write_bulk_workspace(&root, every, false);
    fs::create_dir_all(&folder).unwrap();
    let mut changed = Vec::new();
    for at in 0..BULK_SCRIPTS {
        let file = folder.join(format!("Script{at:04}.client.luau"));
        fs::write(&file, "print('client')\n").unwrap();
        changed.extend([file, folder.join(format!("Script{at:04}.server.luau"))]);
    }
    let changes = measure(name, || collect(&root, changed));
    assert_eq!(changes.history_entries.len(), 2 * BULK_SCRIPTS);
    assert_eq!(link_repairs(&changes), if every { BULK_SCRIPTS } else { 1 });
    write_history(&root, &format!("{name} history"), &changes, false);
    let _ = fs::remove_dir_all(&root);
}

#[test]
#[ignore = "benchmark"]
fn bulk_bench_scripts_renamed() {
    scripts_renamed("renamed", false);
}

#[test]
#[ignore = "benchmark"]
fn bulk_bench_scripts_renamed_referenced() {
    scripts_renamed("renamed referenced", true);
}

/// The `init` files of the Workspace's Bulk scripts deleted, which turns
/// each script into a Folder that keeps its child.
#[test]
#[ignore = "benchmark"]
fn bulk_bench_init_scripts_deleted() {
    let root = project("init-deleted");
    let folder = write_bulk_workspace(&root, false, true);
    let changed = (0..BULK_SCRIPTS)
        .map(|at| {
            folder
                .join(format!("Script{at:04}"))
                .join("init.server.luau")
        })
        .collect();
    let changes = measure("init deleted", || collect(&root, changed));
    assert_eq!(changes.history_entries.len(), BULK_SCRIPTS);
    write_history(&root, "init deleted history", &changes, false);
    let _ = fs::remove_dir_all(&root);
}

/// One encode of the Workspace store, what a history entry's copy of the
/// store costs.
#[test]
#[ignore = "benchmark"]
fn bulk_bench_store_encoded() {
    let root = project("encoded");
    let workspace = root.join("src").join("Workspace");
    let document =
        crate::settings::bytecode::SettingsBytecode::read_file(&service_settings_path(&workspace))
            .unwrap();
    let _ = fs::remove_dir_all(&root);
    let encoded = measure("store encoded", || {
        crate::settings::bytecode::encode_settings_bytecode(&document).unwrap()
    });
    assert!(!encoded.is_empty());
}
