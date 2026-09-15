use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::app::context;
use crate::daemon::try_daemon_project_root;
use crate::project::config;
use crate::project::experience::resolve_experience_place;
use crate::system::files::canonical_path;

pub(crate) fn configured_project_layout(
    project_root: &Path,
    source_root: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let project_override = context::project_override();
    let explicit = project_override.as_deref();
    if explicit.is_none() && source_root != Path::new("src") {
        return Ok((project_root.to_path_buf(), source_root.to_path_buf()));
    }
    let root = if explicit.is_none() && !project_root.is_absolute() {
        let selector = context::place_selector();
        match resolve_experience_place(project_root, selector.as_deref()) {
            Ok(place) => place.map_or_else(|| project_root.to_path_buf(), |place| place.root),
            Err(error) if selector.is_none() => {
                try_daemon_project_root(project_root)?.ok_or(error)?
            }
            Err(error) => return Err(error),
        }
    } else {
        project_root.to_path_buf()
    };
    let loaded = config::try_load_project(explicit, Some(&root))?;
    let loaded = if explicit.is_none() && project_root.is_absolute() {
        let requested = canonical_path(project_root).with_context(|| {
            format!("Failed to resolve project root {}", project_root.display())
        })?;
        loaded.filter(|loaded| canonical_path(&loaded.root).is_ok_and(|loaded| loaded == requested))
    } else {
        loaded
    };
    let Some(loaded) = loaded else {
        return Ok((root, source_root.to_path_buf()));
    };
    let root = canonical_path(&loaded.root)
        .with_context(|| format!("Failed to resolve project root {}", loaded.root.display()))?;
    let source_root = if source_root == Path::new("src") {
        config::validate_relative_portable_path(&loaded.project.source_root, "sourceRoot")?;
        loaded.project.source_root
    } else {
        source_root.to_path_buf()
    };
    Ok((root, source_root))
}

/// An explicit `-r DIR` names that directory as the project. When it holds no
/// project yet, create the default one there instead of discovering a parent
/// project and writing into it.
pub(crate) fn ensure_explicit_project_root(project_root: &Path) -> Result<()> {
    if project_root == Path::new(".") {
        return Ok(());
    }
    if !project_root.exists() {
        std::fs::create_dir_all(project_root)
            .with_context(|| format!("Failed to create {}", project_root.display()))?;
    }
    if !project_root.is_dir() {
        return Ok(());
    }
    if ["renium.project.jsonc", "src", "instances", "places"]
        .iter()
        .any(|name| project_root.join(name).exists())
    {
        return Ok(());
    }
    let path = project_root.join("renium.project.jsonc");
    std::fs::write(&path, "{\n  \"schemaVersion\": 1\n}\n")
        .with_context(|| format!("Failed to create {}", path.display()))?;
    crate::app::output::log_global(3, format_args!("[renium] created {}", path.display()));
    Ok(())
}

pub(crate) fn apply_configured_project_layout(
    project_root: &mut PathBuf,
    source_root: &mut PathBuf,
) -> Result<()> {
    let (root, source) = configured_project_layout(project_root, source_root)?;
    *project_root = root;
    *source_root = source;
    Ok(())
}

#[cfg(test)]
mod explicit_root_tests {
    use super::ensure_explicit_project_root;

    #[test]
    fn explicit_missing_root_is_created_with_a_project_file() {
        let base = std::env::temp_dir().join(format!(
            "renium-explicit-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("artifacts").join("replication-lab");
        ensure_explicit_project_root(&root).unwrap();
        assert!(root.join("renium.project.jsonc").is_file());
        ensure_explicit_project_root(&root).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
    }
}
