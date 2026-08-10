use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::file_io::canonical_path;
use crate::{project_config, runtime_context};

pub(super) fn configured_project_layout(
    project_root: &Path,
    source_root: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let project_override = runtime_context::project_override();
    let explicit = project_override.as_deref();
    if explicit.is_none() && source_root != Path::new("src") {
        return Ok((project_root.to_path_buf(), source_root.to_path_buf()));
    }
    let Some(loaded) = project_config::try_load_project(explicit, Some(project_root))? else {
        return Ok((project_root.to_path_buf(), source_root.to_path_buf()));
    };
    let root = canonical_path(&loaded.root)
        .with_context(|| format!("Failed to resolve project root {}", loaded.root.display()))?;
    let source_root = if source_root == Path::new("src") {
        project_config::validate_relative_portable_path(&loaded.project.source_root, "sourceRoot")?;
        loaded.project.source_root
    } else {
        source_root.to_path_buf()
    };
    Ok((root, source_root))
}

pub(super) fn apply_configured_project_layout(
    project_root: &mut PathBuf,
    source_root: &mut PathBuf,
) -> Result<()> {
    let (root, source) = configured_project_layout(project_root, source_root)?;
    *project_root = root;
    *source_root = source;
    Ok(())
}
