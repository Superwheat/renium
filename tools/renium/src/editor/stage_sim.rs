//! Seeded simulation of the staged copy a project with mapped folders is
//! pushed through.
//!
//! The copy is kept between commands and refreshed from the files a command
//! names. Each seed edits a project the way an editor, a pull and a version
//! control checkout do, then pushes some of what changed, a mapped folder,
//! or everything. Two things must hold. The kept copy holds the same files
//! as one staged from nothing, and a push collected from the kept copy sends
//! what a push collected from a fresh copy sends: a kept copy that misses an
//! edit pushes the state before it.
//!
//! `crate::tests::support::run_seeds` names the variables that run more
//! seeds or replay one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::sync::{collect_project_editor_changes, is_lua_source_class};
use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs, PushEditorChangesArgs};
use crate::project::config::{
    forget_projection, load_project, stage_project, stage_project_cached,
};
use crate::settings::bytecode::{
    SettingsBytecode, SettingsBytecodeInstance, decode_settings_bytecode,
};
use crate::tests::support::{SimRng, run_seeds};

const DEFAULT_SEEDS: u64 = 8;
const STEPS: usize = 10;
const PROJECT: &str = r#"{"schemaVersion":1,"tree":{"ReplicatedStorage":{"Shared":{"$path":"src/shared"}},"ServerScriptService":{"Server":{"$path":"src/server"}}}}"#;
const SERVICES: [&str; 2] = ["ReplicatedStorage", "ServerScriptService"];
const FOLDERS: [&str; 6] = [
    "src/shared",
    "src/shared/Sub",
    "src/shared/Sub/Deep",
    "src/server",
    "src/server/Jobs",
    "src/server/Jobs/Night",
];

struct Project {
    root: PathBuf,
    scripts: Vec<PathBuf>,
    counter: u64,
}

impl Project {
    fn new(seed: u64) -> Result<Self, String> {
        let root = crate::tests::support::temp_dir(&format!("stage-sim-{seed}"));
        fs::create_dir_all(root.join("empty")).map_err(text_of)?;
        fs::create_dir_all(root.join("src/shared")).map_err(text_of)?;
        fs::create_dir_all(root.join("src/server")).map_err(text_of)?;
        fs::write(root.join("renium.project.jsonc"), PROJECT).map_err(text_of)?;
        for service in SERVICES {
            crate::rbx::model::source_only_settings_document(&root.join("empty"), service)
                .and_then(|document| {
                    document.write_file(&root.join(format!("instances/{service}.renium")))
                })
                .map_err(text)?;
        }
        Ok(Self {
            root,
            scripts: Vec::new(),
            counter: 0,
        })
    }

    /// Writes text no earlier write of this run had, of another length than
    /// the file holds now: the kept copy tells a changed file by its size
    /// and time.
    fn write(&mut self, path: &Path) -> Result<(), String> {
        self.counter += 1;
        let mut text = format!("return {}\n", self.counter);
        let held = fs::metadata(path).map(|metadata| metadata.len()).ok();
        while held == Some(text.len() as u64) {
            text.push('\n');
        }
        fs::create_dir_all(path.parent().unwrap()).map_err(text_of)?;
        fs::write(path, text).map_err(text_of)
    }

    /// One edit; returns what it did and the paths it touched.
    fn edit(&mut self, rng: &mut SimRng) -> Result<(String, Vec<PathBuf>), String> {
        let existing = rng.pick(&self.scripts);
        match (rng.below(100), existing) {
            (0..=29, _) | (_, None) => {
                self.counter += 1;
                let folder = FOLDERS[rng.below(FOLDERS.len())];
                let suffix = ["luau", "server.luau", "client.luau"][rng.below(3)];
                let path = self
                    .root
                    .join(folder)
                    .join(format!("M{}.{suffix}", self.counter));
                self.write(&path)?;
                self.scripts.push(path.clone());
                Ok((format!("add {}", self.shown(&path)), vec![path]))
            }
            (30..=54, Some(path)) => {
                self.write(&path)?;
                Ok((format!("edit {}", self.shown(&path)), vec![path]))
            }
            (55..=64, Some(path)) => {
                fs::remove_file(&path).map_err(text_of)?;
                self.scripts.retain(|script| *script != path);
                Ok((format!("delete {}", self.shown(&path)), vec![path]))
            }
            (65..=79, Some(path)) => {
                self.counter += 1;
                let folder = self.root.join(FOLDERS[rng.below(FOLDERS.len())]);
                let to = folder.join(format!("R{}.luau", self.counter));
                fs::create_dir_all(&folder).map_err(text_of)?;
                fs::rename(&path, &to).map_err(text_of)?;
                self.scripts.retain(|script| *script != path);
                self.scripts.push(to.clone());
                Ok((
                    format!("move {} to {}", self.shown(&path), self.shown(&to)),
                    vec![path, to],
                ))
            }
            (80..=91, Some(_)) => {
                self.counter += 1;
                let store = self
                    .root
                    .join(format!("instances/{}.renium", SERVICES[rng.below(2)]));
                let mut document = SettingsBytecode::read_file(&store).map_err(text)?;
                document.instances.push(SettingsBytecodeInstance::new(
                    format!("pulled-{}", self.counter),
                    format!("Pulled{}", self.counter),
                    "Folder".to_string(),
                    Some(0),
                ));
                document.write_file(&store).map_err(text)?;
                Ok((format!("pull into {}", self.shown(&store)), Vec::new()))
            }
            (_, Some(_)) => {
                let path = self.root.join(format!("src/NOTES{}.md", rng.below(2)));
                self.write(&path)?;
                Ok((format!("note {}", self.shown(&path)), Vec::new()))
            }
        }
    }

    fn shown(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// What a push of these paths would send to Studio, without the places
    /// on disk it was read from.
    fn push(&self, named: &[PathBuf]) -> Result<Value, String> {
        let mut args = PushEditorChangesArgs::new(
            ProjectSourceArgs {
                project_root: self.root.clone(),
                src_root: PathBuf::from("src"),
            },
            BridgeConnectionArgs::local(0.1),
        );
        args.changed_paths = named.to_vec();
        let (changes, _) = collect_project_editor_changes(&args).map_err(text)?;
        let mut sources = serde_json::to_value(&changes.source_changes).unwrap();
        let mut instances = serde_json::to_value(&changes.instance_changes).unwrap();
        let mut properties = serde_json::to_value(&changes.property_changes).unwrap();
        for list in [&mut sources, &mut instances, &mut properties] {
            let items = list.as_array_mut().unwrap();
            items.sort_by_key(Value::to_string);
        }
        Ok(json!({ "sources": sources, "instances": instances, "properties": properties }))
    }
}

fn text(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn text_of(error: std::io::Error) -> String {
    error.to_string()
}

fn staged_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut files = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(relative, fs::read(entry.path()).map_err(text_of)?);
        }
    }
    Ok(files)
}

/// A staged store without the text of its scripts: a push reads a script's
/// text from its staged file, and the copy inside the store is refreshed only
/// when its service is staged again.
fn staged_store(bytes: &[u8]) -> Vec<SettingsBytecodeInstance> {
    let mut instances = decode_settings_bytecode(bytes)
        .map(|document| document.instances)
        .unwrap_or_default();
    for instance in &mut instances {
        if is_lua_source_class(&instance.class_name) {
            instance.properties.remove("Source");
        }
    }
    instances
}

fn first_difference(
    kept: &BTreeMap<String, Vec<u8>>,
    fresh: &BTreeMap<String, Vec<u8>>,
) -> Option<String> {
    for (path, bytes) in fresh {
        match kept.get(path) {
            None => return Some(format!("the kept copy lacks {path}")),
            Some(held) if held != bytes => {
                if !path.ends_with(".renium") {
                    return Some(format!("the kept copy holds another {path}"));
                }
                let (held, fresh) = (staged_store(held), staged_store(bytes));
                if held != fresh {
                    let differing = held
                        .iter()
                        .zip(&fresh)
                        .find(|(held, fresh)| held != fresh)
                        .map(|(held, fresh)| format!("{held:?} against {fresh:?}"))
                        .unwrap_or_else(|| {
                            format!("{} against {} instances", held.len(), fresh.len())
                        });
                    return Some(format!("the kept copy holds another {path}: {differing}"));
                }
            }
            Some(_) => {}
        }
    }
    kept.keys()
        .find(|path| !fresh.contains_key(*path))
        .map(|path| format!("the kept copy still holds {path}"))
}

fn run_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = SimRng(seed);
    let mut project = Project::new(seed)?;
    let project_file = project.root.join("renium.project.jsonc");
    let outcome = (|| {
        for step in 0..STEPS {
            let mut touched = Vec::new();
            for _ in 0..1 + rng.below(3) {
                let (what, paths) = project.edit(&mut rng)?;
                log.push(format!("step {step}: {what}"));
                touched.extend(paths);
            }
            let named = match rng.below(8) {
                0 => vec![project.root.join(FOLDERS[rng.below(FOLDERS.len())])],
                1 => Vec::new(),
                _ => touched.into_iter().filter(|_| rng.below(2) == 0).collect(),
            };
            let named = named
                .into_iter()
                .filter(|path| path.exists() || path.extension().is_some())
                .collect::<Vec<_>>();
            log.push(format!(
                "step {step}: push {:?}",
                named
                    .iter()
                    .map(|path| project.shown(path))
                    .collect::<Vec<_>>()
            ));
            let from_kept = project.push(&named);
            let loaded = load_project(Some(&project_file), None).map_err(text)?;
            let kept = stage_project_cached(&loaded, &named).map_err(text)?;
            let fresh = stage_project(&loaded).map_err(text)?;
            let difference =
                first_difference(&staged_files(kept.root())?, &staged_files(fresh.root())?);
            if let Some(difference) = difference {
                return Err(format!("step {step}: {difference}"));
            }
            if rng.below(2) == 0 {
                forget_projection(&project_file);
                let from_fresh = project.push(&named);
                if from_kept != from_fresh {
                    return Err(format!(
                        "step {step}: a push from the kept copy sends {} and one from a fresh copy sends {}",
                        from_kept.map_or_else(|error| error, |sent| sent.to_string()),
                        from_fresh.map_or_else(|error| error, |sent| sent.to_string()),
                    ));
                }
            }
        }
        Ok(())
    })();
    forget_projection(&project_file);
    let _ = fs::remove_dir_all(&project.root);
    outcome
}

#[test]
fn a_kept_staged_copy_pushes_what_a_fresh_one_does_after_seeded_edits() {
    run_seeds(DEFAULT_SEEDS, run_seed);
}
