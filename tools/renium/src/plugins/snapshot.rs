use crate::project::config;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

pub(super) fn create(destination: &Path) -> Result<Value> {
    let destination = std::path::absolute(destination)?;
    if destination.try_exists()? {
        bail!(
            "Snapshot destination must not exist: {}",
            destination.display()
        );
    }
    let project = crate::app::context::project_override();
    let selected = if project.is_none() {
        crate::project::experience::resolve_experience_place(
            &std::env::current_dir()?,
            crate::studio::target::place_filter().as_deref(),
        )?
        .map(|place| place.root)
    } else {
        None
    };
    let loaded = config::load_project(project.as_deref(), selected.as_deref())?;
    let stage = config::stage_project(&loaded)?;
    let canonical_source = if stage.root().try_exists()? {
        crate::system::files::canonical_path(stage.root())?
    } else {
        crate::system::files::absolutize_for_daemon(stage.root())
    };
    let canonical_destination = crate::system::files::canonical_path(
        destination.parent().context("Missing destination parent")?,
    )?
    .join(
        destination
            .file_name()
            .context("Missing destination name")?,
    );
    if canonical_destination.starts_with(canonical_source) {
        bail!("Snapshot destination cannot be inside the projected source tree");
    }
    fs::create_dir(&destination)?;
    let mut cleanup = crate::system::files::OnDrop::new(|| {
        let _ = fs::remove_dir_all(&destination);
    });
    let output = destination.join("src");
    fs::create_dir(&output)?;
    let instances = destination.join("instances");
    fs::create_dir(&instances)?;
    for service in crate::project::storage::service_directories(stage.root())? {
        let store = crate::system::files::service_settings_path(&service);
        if !store.is_file() {
            continue;
        }
        let mut name = service
            .file_name()
            .context("Service directory has no name")?
            .to_os_string();
        name.push(".renium");
        fs::copy(store, instances.join(name))?;
    }
    // Only projected game data is copied, not Git history, credentials or runtime state.
    for entry in stage
        .root()
        .is_dir()
        .then(|| {
            walkdir::WalkDir::new(stage.root())
                .min_depth(1)
                .follow_links(false)
        })
        .into_iter()
        .flat_map(|walk| walk.into_iter())
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type().is_symlink() {
            bail!("Projection contains an unresolved link: {}", path.display());
        }
        let target = output.join(path.strip_prefix(stage.root())?);
        if entry.file_type().is_dir() {
            fs::create_dir(&target)?;
        } else if entry.file_type().is_file() {
            if !crate::system::files::path_extension_is(path, &["renium"]) {
                fs::copy(path, target)?;
            }
        } else {
            bail!(
                "Projection contains an unsupported file: {}",
                path.display()
            );
        }
    }
    let snapshot = config::ReniumProject {
        name: Some("Projection snapshot".into()),
        script_extension: loaded.project.script_extension,
        export_naming: loaded.project.export_naming.clone(),
        ..Default::default()
    };
    let project = destination.join(config::PROJECT_FILE_NAME);
    crate::system::files::atomic_write_file(&project, &serde_json::to_vec_pretty(&snapshot)?)
        .context("Could not write snapshot project")?;
    cleanup.disarm();
    Ok(json!({"project":project,"sourceProject":loaded.path,"source":output}))
}
