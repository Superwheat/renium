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
    let root = if explicit.is_none() {
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
    let Some(loaded) = config::try_load_project(explicit, Some(&root))? else {
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

pub(crate) fn apply_configured_project_layout(
    project_root: &mut PathBuf,
    source_root: &mut PathBuf,
) -> Result<()> {
    let (root, source) = configured_project_layout(project_root, source_root)?;
    *project_root = root;
    *source_root = source;
    Ok(())
}
