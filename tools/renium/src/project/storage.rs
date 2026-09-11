//! Resolve authoritative stores independently of the script directory.
//!
//! Loaded projects register their exact source root. Temporary projections keep
//! the self-contained legacy layout; they must never resolve into a real project.
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use anyhow::{Context, Result, bail};

use crate::bytecode::acquire_settings_file_lock;
use crate::system::files::{
    SERVICE_SETTINGS_FILE_NAME, absolutize_for_daemon, create_unique_directory, path_key,
};

use super::config::LoadedProject;

#[derive(Clone)]
struct StoreLayout {
    project: PathBuf,
    source: PathBuf,
    mapped: Vec<(PathBuf, PathBuf)>,
}

static LAYOUTS: OnceLock<RwLock<HashMap<String, StoreLayout>>> = OnceLock::new();

pub(crate) fn prepare(loaded: &LoadedProject) -> Result<()> {
    super::config::validate_relative_portable_path(&loaded.project.source_root, "sourceRoot")?;
    let mut layout = StoreLayout {
        project: absolutize_for_daemon(&loaded.root),
        source: absolutize_for_daemon(&loaded.root.join(&loaded.project.source_root)),
        mapped: Vec::new(),
    };
    let instances = layout.project.join("instances");
    if layout.source.starts_with(&instances) || instances.starts_with(&layout.source) {
        bail!("sourceRoot must be separate from the project's instances directory");
    }
    let sources = super::config::project_tree_nodes(&loaded.project.tree)
        .into_iter()
        .filter_map(|(target, node)| node.path.map(|source| (target, source)))
        .chain(
            loaded
                .project
                .mounts
                .iter()
                .map(|mount| (mount.target.segments(), mount.source.clone())),
        );
    for (target, source) in sources {
        super::config::validate_relative_portable_path(&source, "tree source")?;
        let source = absolutize_for_daemon(&loaded.root.join(source));
        if source.is_file() || (!source.is_dir() && source.extension().is_some()) {
            continue;
        }
        if source.starts_with(&instances) || instances.starts_with(&source) {
            bail!("Mapped script sources must be separate from the project's instances directory");
        }
        let mut store = instances.clone();
        for segment in target {
            store.push(crate::system::files::sanitize_name(&segment));
        }
        let mut name = store
            .file_name()
            .context("Mapped store has no name")?
            .to_os_string();
        name.push(".renium");
        store.set_file_name(name);
        if source != layout.source.join(store.file_stem().unwrap_or_default()) {
            layout.mapped.push((source, store));
        }
    }
    migrate_legacy_stores(&layout)?;
    let mut layouts = LAYOUTS
        .get_or_init(Default::default)
        .write()
        .unwrap_or_else(|e| e.into_inner());
    layouts.retain(|_, previous| previous.project != layout.project);
    layouts.insert(path_key(&layout.source), layout);
    Ok(())
}

fn layout_for_source(source: &Path) -> Option<StoreLayout> {
    LAYOUTS
        .get()?
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&path_key(&absolutize_for_daemon(source)))
        .cloned()
}

pub(crate) fn instances_root(source: &Path) -> Option<PathBuf> {
    layout_for_source(source).map(|layout| layout.project.join("instances"))
}

/// Git does not retain empty script directories; their instance store still does.
pub(crate) fn source_directory_exists(source: &Path) -> bool {
    source.is_dir()
        || settings_path(source).is_some_and(|path| {
            path.is_file() && source_directory(&path) == absolutize_for_daemon(source)
        })
}

pub(crate) fn source_for_instances(root: &Path) -> Option<PathBuf> {
    let root = absolutize_for_daemon(root);
    LAYOUTS
        .get()?
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .find(|layout| layout.project.join("instances") == root)
        .map(|layout| layout.source.clone())
}

pub(crate) fn service_for_store(source: &Path, path: &Path) -> Option<String> {
    let root = instances_root(source)?;
    let path = absolutize_for_daemon(path);
    if path.extension().is_none_or(|ext| ext != "renium") {
        return None;
    }
    let relative = path.strip_prefix(root).ok()?;
    if relative.components().count() == 1 {
        path.file_stem()?.to_str().map(str::to_owned)
    } else {
        relative
            .components()
            .next()?
            .as_os_str()
            .to_str()
            .map(str::to_owned)
    }
}

pub(crate) fn store_service_name(path: &Path) -> Option<String> {
    if path
        .file_name()
        .is_some_and(|name| name == SERVICE_SETTINGS_FILE_NAME)
    {
        path.parent()?.file_name()?.to_str().map(str::to_owned)
    } else {
        path.file_stem()?.to_str().map(str::to_owned)
    }
}

pub(crate) fn resolve_explicit_file(path: &Path) -> Result<PathBuf> {
    super::config::try_load_project(None, path.parent())?;
    Ok(
        if path
            .file_name()
            .is_some_and(|name| name == SERVICE_SETTINGS_FILE_NAME)
        {
            path.parent()
                .and_then(settings_path)
                .unwrap_or_else(|| path.to_path_buf())
        } else {
            path.to_path_buf()
        },
    )
}

pub(crate) fn relocated_legacy_path(root: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.file_name()? != SERVICE_SETTINGS_FILE_NAME {
        return None;
    }
    let destination = settings_path(root.join(relative).parent()?)?;
    destination
        .strip_prefix(absolutize_for_daemon(root))
        .ok()
        .map(Path::to_path_buf)
}

pub(crate) fn legacy_store_paths(root: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let root = absolutize_for_daemon(root);
    let layouts = LAYOUTS
        .get()
        .map(|layouts| {
            layouts
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .filter(|layout| layout.project.starts_with(&root))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut paths = BTreeMap::new();
    for layout in layouts {
        for source in service_directories(&layout.source)?
            .into_iter()
            .chain(layout.mapped.into_iter().map(|(source, _)| source))
        {
            let Some(store) = settings_path(&source) else {
                continue;
            };
            if source_directory(&store) != absolutize_for_daemon(&source) {
                continue;
            }
            paths.insert(
                source
                    .join(SERVICE_SETTINGS_FILE_NAME)
                    .strip_prefix(&root)?
                    .to_path_buf(),
                store.strip_prefix(&root)?.to_path_buf(),
            );
        }
    }
    Ok(paths.into_iter().collect())
}

pub(crate) fn settings_path(service_dir: &Path) -> Option<PathBuf> {
    let absolute = absolutize_for_daemon(service_dir);
    if let Some(layouts) = LAYOUTS.get() {
        let layouts = layouts.read().unwrap_or_else(|e| e.into_inner());
        if let Some((_, store)) = layouts
            .values()
            .flat_map(|layout| &layout.mapped)
            .find(|(source, _)| path_key(source) == path_key(&absolute))
        {
            return Some(store.clone());
        }
    }
    let service = service_dir.file_name()?;
    Some(
        instances_root(service_dir.parent()?)?
            .join(service)
            .with_extension("renium"),
    )
}

pub(crate) fn source_directory(settings: &Path) -> PathBuf {
    let settings = absolutize_for_daemon(settings);
    if let Some(layouts) = LAYOUTS.get() {
        let layouts = layouts.read().unwrap_or_else(|e| e.into_inner());
        if let Some((source, _)) = layouts
            .values()
            .flat_map(|layout| &layout.mapped)
            .find(|(_, store)| path_key(store) == path_key(&settings))
        {
            return source.clone();
        }
        if let Some(layout) = layouts
            .values()
            .find(|layout| settings.parent() == Some(layout.project.join("instances").as_path()))
        {
            return layout.source.join(settings.file_stem().unwrap_or_default());
        }
    }
    settings.parent().unwrap_or(Path::new(".")).to_path_buf()
}

pub(crate) fn forget(root: &Path) {
    if let Some(layouts) = LAYOUTS.get() {
        let root = absolutize_for_daemon(root);
        layouts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, layout| !layout.project.starts_with(&root));
    }
}

pub(crate) fn copy_stores_to_stage(source: &Path, destination: &Path) -> Result<()> {
    if let Some(instances) = instances_root(source) {
        for service in service_directories(source)? {
            let Some(settings) = settings_path(&service).filter(|path| path.is_file()) else {
                continue;
            };
            let expected = instances
                .join(service.file_name().unwrap_or_default())
                .with_extension("renium");
            if settings != expected {
                continue;
            }
            if source_directory(&settings) != absolutize_for_daemon(&service) {
                continue;
            }
            let target = destination.join(service.file_name().unwrap_or_default());
            fs::create_dir_all(&target)?;
            fs::copy(settings, target.join(SERVICE_SETTINGS_FILE_NAME))?;
        }
    } else if let Some(settings) = settings_path(source).filter(|path| path.is_file()) {
        fs::create_dir_all(destination)?;
        fs::copy(settings, destination.join(SERVICE_SETTINGS_FILE_NAME))?;
    }
    Ok(())
}

/// Include services with instance data but no script directory (also after clone).
pub(crate) fn service_directories(source: &Path) -> Result<Vec<PathBuf>> {
    let mut services = BTreeMap::new();
    if source.is_dir() {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                services.insert(entry.file_name(), entry.path());
            }
        }
    }
    if let Some(instances) = instances_root(source).filter(|path| path.is_dir()) {
        for entry in fs::read_dir(instances)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_file() && path.extension().is_some_and(|ext| ext == "renium") {
                let name = path
                    .file_stem()
                    .context("Store has no service name")?
                    .to_os_string();
                services.insert(name.clone(), source.join(name));
            }
        }
    }
    Ok(services.into_values().collect())
}

fn migrate_legacy_stores(layout: &StoreLayout) -> Result<()> {
    let mut moves = Vec::new();
    for entry in if layout.source.is_dir() {
        Some(fs::read_dir(&layout.source)?)
    } else {
        None
    }
    .into_iter()
    .flatten()
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let old = entry.path().join(SERVICE_SETTINGS_FILE_NAME);
        if !old.is_file() {
            continue;
        }
        if fs::symlink_metadata(&old)?.file_type().is_symlink() {
            bail!("Cannot migrate a symlinked store: {}", old.display());
        }
        let new = layout
            .mapped
            .iter()
            .find(|(source, _)| *source == entry.path())
            .map(|(_, store)| store.clone())
            .unwrap_or_else(|| {
                layout
                    .project
                    .join("instances")
                    .join(entry.file_name())
                    .with_extension("renium")
            });
        moves.push((old, new));
    }
    for (source, store) in &layout.mapped {
        let old = source.join(SERVICE_SETTINGS_FILE_NAME);
        if old.is_file() && !moves.iter().any(|(path, _)| path == &old) {
            moves.push((old, store.clone()));
        }
    }
    if moves.is_empty() {
        return Ok(());
    }
    // Lock both layouts so two CLI/daemon loads can resume the same migration.
    let mut locks = moves
        .iter()
        .flat_map(|(old, new)| [old, new])
        .collect::<Vec<_>>();
    locks.sort();
    locks.dedup();
    let _locks = locks
        .into_iter()
        .map(|path| acquire_settings_file_lock(path))
        .collect::<Result<Vec<_>>>()?;
    let mut copies = Vec::new();
    for (old, new) in moves {
        if fs::symlink_metadata(&old).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("Cannot migrate a symlinked store: {}", old.display());
        }
        let bytes = match fs::read(&old) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if new.exists() && fs::read(&new)? != bytes {
            bail!(
                "Store migration conflict: {} and {} differ. Preserve both and reconcile them before retrying.",
                old.display(),
                new.display()
            );
        }
        copies.push((old, new, bytes));
    }
    // Publish complete bytes without replacing an independently created file.
    // A crash after publication leaves equal copies, which the next load resumes.
    for (old, new, bytes) in copies {
        if !new.exists() {
            let temporary =
                create_unique_directory(&layout.project.join(".renium/store-migration"), "")?;
            let cleanup = crate::system::files::OnDrop::new(|| {
                let _ = fs::remove_dir_all(&temporary);
            });
            let staged = temporary.join("store");
            let mut file = fs::File::create(&staged)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::hard_link(&staged, &new)
                .with_context(|| format!("Could not publish migrated store {}", new.display()))?;
            drop(cleanup);
        }
        if fs::read(&old)? != bytes || fs::read(&new)? != bytes {
            bail!(
                "Store changed during migration; both copies were retained: {}",
                old.display()
            );
        }
        fs::remove_file(&old)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::config;
    use crate::settings::bytecode::{
        SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
    };
    use crate::system::files::OnDrop;
    use clap::Parser;
    use serde_json::json;

    fn fixture() -> Result<PathBuf> {
        let root = create_unique_directory(
            &absolutize_for_daemon(Path::new("target/storage-tests")),
            "project-",
        )?;
        fs::write(
            root.join("renium.project.jsonc"),
            br#"{"schemaVersion":1,"sourceRoot":"code/scripts"}"#,
        )?;
        Ok(root)
    }

    #[test]
    fn instance_only_services_keep_references_during_single_and_batch_rename() -> Result<()> {
        let root = fixture()?;
        let _cleanup = OnDrop::new(|| {
            forget(&root);
            let _ = fs::remove_dir_all(&root);
        });
        config::load_project(Some(&root.join("renium.project.jsonc")), None)?;
        let source = root.join("instances/ReplicatedStorage.renium");
        let target = root.join("instances/ServerStorage.renium");
        let document = |service: &str, child: &str, class: &str| SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    format!("{service}:root"),
                    service.into(),
                    service.into(),
                    None,
                ),
                SettingsBytecodeInstance::new(child.into(), child.into(), class.into(), Some(0)),
            ],
        };
        document("ReplicatedStorage", "Target", "Folder").write_file(&source)?;
        let mut holder = document("ServerStorage", "Holder", "ObjectValue");
        holder.instances[1].properties.insert("Value".into(), json!({
            "_type":"Ref", "settingsId":"Target", "pathSegments":["ReplicatedStorage","Target"], "pathOrdinals":[1,1]
        }));
        holder.write_file(&target)?;
        assert!(!root.join("code/scripts").exists());
        crate::bytecode::bytecode_set_property(
            crate::cli::BytecodeSetPropertyArgs::try_parse_from([
                "bs",
                source.to_str().unwrap(),
                "-i",
                "Target",
                "-p",
                "Name",
                "--str",
                "Renamed",
            ])?,
        )?;
        assert_eq!(
            SettingsBytecode::read_file(&target)?.instances[1].properties["Value"]["pathSegments"],
            json!(["ReplicatedStorage", "Renamed"])
        );
        let batch = root.join("batch.json");
        fs::write(
            &batch,
            serde_json::to_vec(&json!([{
                "service":"ReplicatedStorage", "settingsId":"Target", "pathSegments":["ReplicatedStorage","Renamed"], "scope":"metadata", "property":"Name", "value":"Again"
            }]))?,
        )?;
        crate::bytecode::bytecode_apply_property_batch(
            crate::cli::BytecodeApplyPropertyBatchArgs {
                project_root: root.clone(),
                input: batch,
                direction: "studio-to-files".into(),
                override_packages: false,
            },
        )?;
        assert_eq!(
            SettingsBytecode::read_file(&target)?.instances[1].properties["Value"]["pathSegments"],
            json!(["ReplicatedStorage", "Again"])
        );
        assert!(!root.join("code/scripts").exists());
        Ok(())
    }

    #[test]
    fn mounted_store_migrates_and_roundtrips_without_scripts() -> Result<()> {
        let root = fixture()?;
        let _cleanup = OnDrop::new(|| {
            forget(&root);
            let _ = fs::remove_dir_all(&root);
        });
        fs::write(
            root.join("renium.project.jsonc"),
            serde_json::to_vec(&json!({
                "schemaVersion":1,
                "mounts":[{"source":"src/shared", "target":"ReplicatedStorage.Shared"}]
            }))?,
        )?;
        let old = root.join("src/shared").join(SERVICE_SETTINGS_FILE_NAME);
        let document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "shared".into(),
                    "Shared".into(),
                    "Folder".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "child".into(),
                    "Child".into(),
                    "Folder".into(),
                    Some(0),
                ),
            ],
        };
        document.write_file(&old)?;
        let bytes = fs::read(&old)?;
        let loaded = config::load_project(Some(&root.join("renium.project.jsonc")), None)?;
        let new = root.join("instances/ReplicatedStorage/Shared.renium");
        assert_eq!(fs::read(&new)?, bytes);
        assert!(!old.exists());
        fs::remove_dir(root.join("src/shared"))?;
        let stage = config::stage_project(&loaded)?;
        config::syncback_project_projection(&loaded, stage.root(), false)?;
        let next = config::stage_project(&loaded)?;
        let projected = SettingsBytecode::read_file(&crate::system::files::service_settings_path(
            &next.root().join("ReplicatedStorage"),
        ))?;
        assert_eq!(
            projected
                .instances
                .iter()
                .filter(|instance| instance.name == "Child")
                .count(),
            1
        );
        assert!(!old.exists());
        Ok(())
    }

    #[test]
    fn migration_is_exact_resumable_and_keeps_places_independent() -> Result<()> {
        let root = fixture()?;
        let _cleanup = OnDrop::new(|| {
            forget(&root);
            let _ = fs::remove_dir_all(&root);
        });
        let source = root.join("code/scripts");
        let old = source.join("Workspace").join(SERVICE_SETTINGS_FILE_NAME);
        fs::create_dir_all(old.parent().unwrap())?;
        // Migration must preserve opaque current/future bytes, not re-encode IDs,
        // references, Terrain grids or PackageLink properties.
        fs::write(&old, b"\0\xffopaque store data")?;
        let script = source.join("Workspace/Main.server.luau");
        fs::write(&script, b"print('preserved')\n")?;
        let loaded = config::load_project(Some(&root.join("renium.project.jsonc")), None)?;
        let new = root.join("instances/Workspace.renium");
        assert_eq!(fs::read(&new)?, b"\0\xffopaque store data");
        assert!(!old.exists());
        assert_eq!(source_directory(&new), source.join("Workspace"));
        assert_eq!(fs::read(&script)?, b"print('preserved')\n");
        // A process interrupted after publishing can leave an equal old copy.
        fs::write(&old, fs::read(&new)?)?;
        prepare(&loaded)?;
        assert!(!old.exists());
        let other = root.join("places/other");
        fs::create_dir_all(&other)?;
        fs::write(
            other.join("renium.project.jsonc"),
            br#"{"schemaVersion":1}"#,
        )?;
        let other_project = config::load_project(Some(&other.join("renium.project.jsonc")), None)?;
        assert_eq!(
            settings_path(&other_project.root.join("src/Workspace")),
            Some(other.join("instances/Workspace.renium"))
        );
        assert_eq!(settings_path(&source.join("Workspace")), Some(new));
        Ok(())
    }

    #[test]
    fn migration_conflict_preserves_every_original() -> Result<()> {
        let root = fixture()?;
        let _cleanup = OnDrop::new(|| {
            forget(&root);
            let _ = fs::remove_dir_all(&root);
        });
        for name in ["Workspace", "ServerStorage"] {
            let old = root
                .join("code/scripts")
                .join(name)
                .join(SERVICE_SETTINGS_FILE_NAME);
            fs::create_dir_all(old.parent().unwrap())?;
            fs::write(old, name.as_bytes())?;
        }
        fs::create_dir_all(root.join("instances"))?;
        fs::write(
            root.join("instances/Workspace.renium"),
            b"newer independent edit",
        )?;
        let error = config::load_project(Some(&root.join("renium.project.jsonc")), None)
            .err()
            .context("Expected collision refusal")?;
        assert!(error.to_string().contains("migration conflict"));
        assert_eq!(
            fs::read(root.join("instances/Workspace.renium"))?,
            b"newer independent edit"
        );
        for name in ["Workspace", "ServerStorage"] {
            assert_eq!(
                fs::read(
                    root.join("code/scripts")
                        .join(name)
                        .join(SERVICE_SETTINGS_FILE_NAME)
                )?,
                name.as_bytes()
            );
        }
        assert!(!root.join("instances/ServerStorage.renium").exists());
        Ok(())
    }

    #[test]
    fn optional_client_server_trees_keep_all_stores_outside_scripts() -> Result<()> {
        let root = fixture()?;
        let _cleanup = OnDrop::new(|| {
            forget(&root);
            let _ = fs::remove_dir_all(&root);
        });
        fs::write(
            root.join("renium.project.jsonc"),
            serde_json::to_vec(&json!({
                "schemaVersion": 1,
                "tree": {
                    "ServerScriptService": { "$path": "src/server" },
                    "StarterPlayer": { "StarterPlayerScripts": { "$className": "StarterPlayerScripts", "$path": "src/client" } }
                }
            }))?,
        )?;
        for (directory, name) in [
            ("server", "Main.server.luau"),
            ("client", "Main.client.luau"),
        ] {
            fs::create_dir_all(root.join("src").join(directory))?;
            fs::write(
                root.join("src").join(directory).join(name),
                b"print('mapped')\n",
            )?;
        }
        let loaded = config::load_project(Some(&root.join("renium.project.jsonc")), None)?;
        let stage = config::stage_project(&loaded)?;
        assert!(
            stage
                .root()
                .join("ServerScriptService/Main.server.luau")
                .is_file()
        );
        assert!(
            stage
                .root()
                .join("StarterPlayer/StarterPlayerScripts/Main.client.luau")
                .is_file()
        );
        config::syncback_project_projection(&loaded, stage.root(), false)?;
        assert!(root.join("instances/ServerScriptService.renium").is_file());
        assert!(
            root.join("instances/StarterPlayer/StarterPlayerScripts.renium")
                .is_file()
        );
        assert!(
            walkdir::WalkDir::new(root.join("src"))
                .into_iter()
                .all(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_none_or(|ext| ext != "renium"))
        );
        let next = config::stage_project(&loaded)?;
        for service in ["StarterPlayer", "ServerScriptService"] {
            let settings = crate::system::files::service_settings_path(&next.root().join(service));
            let document = SettingsBytecode::read_file(&settings)?;
            assert_eq!(
                document
                    .instances
                    .iter()
                    .filter(|instance| instance.name == "Main")
                    .count(),
                1
            );
        }
        // Clone/export publication must include nested mapped stores too.
        let export = crate::snapshot::export::ExportProjectStage::create(
            &root,
            &loaded.project.source_root,
            &["StarterPlayer".into(), "ServerScriptService".into()],
        )?;
        assert!(
            export
                .project_root
                .join("instances/StarterPlayer/StarterPlayerScripts.renium")
                .is_file()
        );
        Ok(())
    }

    #[test]
    fn stores_without_scripts_survive_projection_and_snapshot_publication() -> Result<()> {
        let root = fixture()?;
        let _cleanup = OnDrop::new(|| {
            forget(&root);
            let _ = fs::remove_dir_all(&root);
        });
        let loaded = config::load_project(Some(&root.join("renium.project.jsonc")), None)?;
        let source = root.join("code/scripts");
        let store = root.join("instances/Workspace.renium");
        let mut document = SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: vec![
                SettingsBytecodeInstance::new(
                    "stable:root".into(),
                    "Workspace".into(),
                    "Workspace".into(),
                    None,
                ),
                SettingsBytecodeInstance::new(
                    "stable:part".into(),
                    "Part".into(),
                    "Part".into(),
                    Some(0),
                ),
            ],
        };
        document.instances[1]
            .attributes
            .insert("kept".into(), json!(42));
        document.write_file(&store)?;
        assert!(!source.exists());
        let services = super::super::structural::service_store_paths(&source)?;
        assert_eq!(services.get("Workspace"), Some(&store));
        assert_eq!(
            crate::editor::document::read_editor_service_documents(&source)?.len(),
            1
        );
        assert_eq!(
            crate::editor::paths::service_from_changed_path(&source, &store).as_deref(),
            Some("Workspace")
        );
        let stage = crate::snapshot::export::ExportProjectStage::create(
            &root,
            &loaded.project.source_root,
            &["Workspace".into()],
        )?;
        let staged_store = stage.project_root.join("instances/Workspace.renium");
        assert_eq!(fs::read(&staged_store)?, fs::read(&store)?);
        document.instances[1]
            .attributes
            .insert("kept".into(), json!(43));
        document.write_file(&staged_store)?;
        stage.publish(&root, false)?;
        assert_eq!(
            SettingsBytecode::read_file(&store)?.instances[1].attributes["kept"],
            json!(43)
        );
        assert!(
            !source
                .join("Workspace")
                .join(SERVICE_SETTINGS_FILE_NAME)
                .exists()
        );
        Ok(())
    }
}
