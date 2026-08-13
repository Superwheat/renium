use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use globset::Glob;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::cli::args::GenerateSourcemapArgs;
use crate::editor::paths::infer_source_script;
use crate::project::config;
use crate::roblox::services::explorer_service_order;
use crate::system::files::{
    absolutize_for_daemon, path_key, read_json_file, resolve_existing_project_root,
    strip_extended_prefix, write_json_file, write_utf8_file,
};
use crate::system::watch::FileWatcher;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourcemapNode {
    pub(crate) name: String,
    pub(crate) class_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) file_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) children: Vec<SourcemapNode>,
}

pub(crate) fn path_to_sourcemap_relative(project_root: &Path, path: &Path) -> String {
    let root = strip_extended_prefix(project_root.to_path_buf());
    let path = strip_extended_prefix(path.to_path_buf());
    if let Ok(stripped) = path.strip_prefix(&root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    if cfg!(windows) {
        let root_key = path_key(&root);
        let full_key = path_key(&path);
        if let Some(rest) = full_key.strip_prefix(&root_key) {
            let tail = rest.trim_start_matches('/');
            let text = path.to_string_lossy().replace('\\', "/");
            if text.len() >= tail.len() && text.is_char_boundary(text.len() - tail.len()) {
                return text[text.len() - tail.len()..].to_string();
            }
        }
    }
    path.to_string_lossy().replace('\\', "/")
}

fn sourcemap_root_name(project_root: &Path) -> String {
    project_root
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("game")
        .to_string()
}

fn make_sourcemap_root(project_root: &Path) -> SourcemapNode {
    SourcemapNode {
        name: sourcemap_root_name(project_root),
        class_name: "DataModel".to_string(),
        file_paths: Vec::new(),
        children: Vec::new(),
    }
}

fn sort_sourcemap_root_children(root: &mut SourcemapNode) {
    root.children.sort_by(|a, b| {
        let a_index = explorer_service_order(a.name.as_str()).unwrap_or(usize::MAX);
        let b_index = explorer_service_order(b.name.as_str()).unwrap_or(usize::MAX);
        a_index.cmp(&b_index).then_with(|| a.name.cmp(&b.name))
    });
}

fn write_sourcemap_root(project_root: &Path, mut root: SourcemapNode) -> Result<()> {
    sort_sourcemap_root_children(&mut root);
    let output_file = project_root.join("sourcemap.json");
    write_json_file(&output_file, &root, true).context("Failed to serialize sourcemap")?;
    println!("[renium] wrote {}", output_file.display());
    Ok(())
}

pub(crate) fn load_existing_sourcemap_root(project_root: &Path) -> Result<Option<SourcemapNode>> {
    let sourcemap_path = project_root.join("sourcemap.json");
    if !sourcemap_path.is_file() {
        return Ok(None);
    }

    let root: SourcemapNode = read_json_file(&sourcemap_path)
        .with_context(|| format!("Invalid JSON in {}", sourcemap_path.display()))?;
    Ok(Some(root))
}

pub(crate) fn write_project_sourcemap_from_service_nodes(
    project_root: &Path,
    service_nodes: &HashMap<String, SourcemapNode>,
) -> Result<()> {
    let mut root = make_sourcemap_root(project_root);
    root.children = service_nodes.values().cloned().collect();
    write_sourcemap_root(project_root, root)
}

pub(crate) fn finalize_project_sourcemap_temp(
    project_root: &Path,
    service_nodes: &HashMap<String, SourcemapNode>,
) -> Result<()> {
    let mut root = make_sourcemap_root(project_root);
    root.children = service_nodes.values().cloned().collect();
    sort_sourcemap_root_children(&mut root);
    let output_file = project_root.join("sourcemap.json");
    write_json_file(&output_file, &root, true).context("Failed to serialize sourcemap")?;
    println!("[renium] wrote {}", output_file.display());
    Ok(())
}

pub(crate) fn write_project_sourcemap_with_updates(
    project_root: &Path,
    updated_nodes: HashMap<String, SourcemapNode>,
) -> Result<()> {
    let mut root = load_existing_sourcemap_root(project_root)?
        .unwrap_or_else(|| make_sourcemap_root(project_root));
    let mut children_by_name: HashMap<String, SourcemapNode> = root
        .children
        .into_iter()
        .map(|child| (child.name.clone(), child))
        .collect();

    for (service_name, node) in updated_nodes {
        children_by_name.insert(service_name, node);
    }

    root.children = children_by_name.into_values().collect();
    write_sourcemap_root(project_root, root)
}

struct SourcemapBuildNode {
    name: String,
    class_name: String,
    file_paths: Vec<String>,
    children: BTreeMap<String, SourcemapBuildNode>,
}

impl SourcemapBuildNode {
    fn new(name: impl Into<String>, class_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            class_name: class_name.into(),
            file_paths: Vec::new(),
            children: BTreeMap::new(),
        }
    }

    fn child_mut(&mut self, name: &str, class_name: &str) -> &mut SourcemapBuildNode {
        let entry = self
            .children
            .entry(name.to_string())
            .or_insert_with(|| SourcemapBuildNode::new(name.to_string(), class_name.to_string()));
        if class_name != "Folder" {
            entry.class_name = class_name.to_string();
        }
        entry
    }

    fn push_file_path(&mut self, file_path: String) {
        if !self
            .file_paths
            .iter()
            .any(|existing| existing == &file_path)
        {
            self.file_paths.push(file_path);
        }
    }

    fn into_sourcemap(self) -> SourcemapNode {
        SourcemapNode {
            name: self.name,
            class_name: self.class_name,
            file_paths: self.file_paths,
            children: self
                .children
                .into_values()
                .map(SourcemapBuildNode::into_sourcemap)
                .collect(),
        }
    }
}

fn insert_script_path_into_service_tree(
    service_root: &mut SourcemapBuildNode,
    service_path: &Path,
    file_path: &Path,
    project_root: &Path,
) -> Result<()> {
    let relative_path = file_path.strip_prefix(service_path).with_context(|| {
        format!(
            "Failed to resolve {} relative to {}",
            file_path.display(),
            service_path.display()
        )
    })?;
    let mut components: Vec<String> = relative_path
        .iter()
        .map(|component| component.to_string_lossy().into_owned())
        .collect();
    if components.is_empty() {
        return Ok(());
    }

    let file_name = components.pop().unwrap_or_default();
    let sourcemap_path = path_to_sourcemap_relative(project_root, file_path);
    let naming = config::cached_script_naming(file_path.parent().unwrap_or(project_root));

    if let Some((class_name, None, _)) = infer_source_script(&file_name, &naming) {
        if components.is_empty() {
            return Ok(());
        }

        let node_name = components.pop().unwrap_or_default();
        let mut current = service_root;
        for component in &components {
            current = current.child_mut(component, "Folder");
        }
        let target = current.child_mut(&node_name, class_name);
        target.push_file_path(sourcemap_path);
        return Ok(());
    }

    let Some((class_name, Some(instance_name), _)) = infer_source_script(&file_name, &naming)
    else {
        return Ok(());
    };

    let mut current = service_root;
    for component in &components {
        current = current.child_mut(component, "Folder");
    }
    let target = current.child_mut(&instance_name, class_name);
    target.push_file_path(sourcemap_path);
    Ok(())
}

fn build_service_sourcemap_node_from_paths(
    service_name: &str,
    service_path: &Path,
    project_root: &Path,
) -> Result<Option<SourcemapNode>> {
    let mut root = SourcemapBuildNode::new(service_name.to_string(), service_name.to_string());

    for entry in WalkDir::new(service_path) {
        let entry = entry.with_context(|| format!("Failed to walk {}", service_path.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        insert_script_path_into_service_tree(&mut root, service_path, entry.path(), project_root)?;
    }

    if root.file_paths.is_empty() && root.children.is_empty() {
        return Ok(None);
    }

    Ok(Some(root.into_sourcemap()))
}

pub(crate) fn generate_project_sourcemap(project_root: &Path) -> Result<()> {
    let loaded = config::try_load_project(None, Some(project_root))?
        .filter(|loaded| loaded.root == project_root);
    let root = build_project_sourcemap_with_loaded(project_root, loaded.as_ref())?;
    write_sourcemap_root(project_root, root)
}

pub(crate) fn generate_project_sourcemap_for_projection(
    loaded: &config::LoadedProject,
    projection: &config::ProjectionStage,
) -> Result<()> {
    let root = build_project_sourcemap_from_source(
        &loaded.root,
        projection.root(),
        projection.is_temporary().then_some((loaded, projection)),
    )?;
    write_sourcemap_root(&loaded.root, root)
}

pub(crate) fn build_project_sourcemap_with_loaded(
    project_root: &Path,
    loaded: Option<&config::LoadedProject>,
) -> Result<SourcemapNode> {
    let projection = loaded.map(config::stage_project).transpose()?;
    let src_root = projection.as_ref().map_or_else(
        || project_root.join("src"),
        |projection| projection.root().to_path_buf(),
    );
    let projected = loaded
        .zip(projection.as_ref())
        .filter(|(_, projection)| projection.is_temporary());
    build_project_sourcemap_from_source(project_root, &src_root, projected)
}

fn build_project_sourcemap_from_source(
    project_root: &Path,
    src_root: &Path,
    projected: Option<(&config::LoadedProject, &config::ProjectionStage)>,
) -> Result<SourcemapNode> {
    let sourcemap_base = if projected.is_some() {
        src_root
    } else {
        project_root
    };
    if !src_root.is_dir() {
        bail!(
            "Cannot generate sourcemap: missing source directory {}",
            src_root.display()
        );
    }

    let root_name = project_root
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("game")
        .to_string();

    let mut root = SourcemapNode {
        name: root_name,
        class_name: "DataModel".to_string(),
        file_paths: Vec::new(),
        children: Vec::new(),
    };

    let mut service_entries: Vec<_> = fs::read_dir(src_root)
        .with_context(|| format!("Failed to read {}", src_root.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to iterate {}", src_root.display()))?;
    service_entries.sort_by(|a, b| {
        a.file_name()
            .to_string_lossy()
            .cmp(&b.file_name().to_string_lossy())
    });

    let built_children = service_entries
        .par_iter()
        .enumerate()
        .map(|(index, entry)| -> Result<Option<(usize, SourcemapNode)>> {
            let file_type = entry
                .file_type()
                .with_context(|| format!("Failed to stat {}", entry.path().display()))?;
            if !file_type.is_dir() {
                return Ok(None);
            }

            let service_name = entry.file_name().to_string_lossy().into_owned();
            let node = build_service_sourcemap_node_from_paths(
                &service_name,
                &entry.path(),
                sourcemap_base,
            )?;
            Ok(node.map(|node| (index, node)))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut built_children: Vec<_> = built_children.into_iter().flatten().collect();
    built_children.sort_by_key(|(index, _)| *index);
    for (_, node) in built_children {
        root.children.push(node);
    }
    if let Some((loaded, projection)) = projected {
        inject_projected_transform_sourcemap_nodes(&mut root, loaded, projection);
        rewrite_projected_sourcemap_paths(&mut root, loaded, projection, &mut Vec::new())?;
    }

    Ok(root)
}

fn sourcemap_child_mut<'a>(
    children: &'a mut Vec<SourcemapNode>,
    name: &str,
    class_name: &str,
) -> &'a mut SourcemapNode {
    let index = children
        .iter()
        .position(|child| child.name == name)
        .unwrap_or_else(|| {
            children.push(SourcemapNode {
                name: name.to_string(),
                class_name: class_name.to_string(),
                file_paths: Vec::new(),
                children: Vec::new(),
            });
            children.len() - 1
        });
    let child = &mut children[index];
    if class_name != "Folder" {
        child.class_name = class_name.to_string();
    }
    child
}

fn inject_projected_transform_sourcemap_nodes(
    root: &mut SourcemapNode,
    loaded: &config::LoadedProject,
    projection: &config::ProjectionStage,
) {
    for (target, source, class_name) in projection.transformed_scripts() {
        let Some((service, descendants)) = target.split_first() else {
            continue;
        };
        let mut current = sourcemap_child_mut(&mut root.children, service, service);
        for (index, segment) in descendants.iter().enumerate() {
            let class = if index + 1 == descendants.len() {
                class_name
            } else {
                "Folder"
            };
            current = sourcemap_child_mut(&mut current.children, segment, class);
        }
        current.file_paths = vec![path_to_sourcemap_relative(&loaded.root, source)];
    }
}

fn rewrite_projected_sourcemap_paths(
    node: &mut SourcemapNode,
    loaded: &config::LoadedProject,
    projection: &config::ProjectionStage,
    target: &mut Vec<String>,
) -> Result<()> {
    if let Some(source) = projection.transformed_source_for_target(target) {
        if !node.file_paths.is_empty() {
            node.file_paths = vec![path_to_sourcemap_relative(&loaded.root, source)];
        }
    } else {
        for file_path in &mut node.file_paths {
            if let Some(source) =
                config::staged_path_to_project_source(loaded, Path::new(file_path))?
            {
                *file_path = path_to_sourcemap_relative(&loaded.root, &source);
            }
        }
    }
    for child in &mut node.children {
        target.push(child.name.clone());
        rewrite_projected_sourcemap_paths(child, loaded, projection, target)?;
        target.pop();
    }
    Ok(())
}

pub(crate) fn generate_sourcemap_command(
    args: GenerateSourcemapArgs,
    global_project: Option<&Path>,
) -> Result<()> {
    let explicit_project = args
        .project
        .as_deref()
        .or(global_project)
        .map(Path::to_path_buf);
    let project_root = if let Some(project) = explicit_project.as_deref() {
        config::load_project(Some(project), None)?.root
    } else {
        resolve_existing_project_root(&args.project_root)?
    };
    let matchers = args
        .filters
        .iter()
        .map(|pattern| {
            Glob::new(pattern)
                .with_context(|| format!("Invalid sourcemap filter '{pattern}'"))
                .map(|glob| glob.compile_matcher())
        })
        .collect::<Result<Vec<_>>>()?;
    loop {
        let loaded = if let Some(project) = explicit_project.as_deref() {
            Some(config::load_project(Some(project), None)?)
        } else {
            config::try_load_project(None, Some(&project_root))?
                .filter(|loaded| loaded.root == project_root)
        };
        let mut root = build_project_sourcemap_with_loaded(&project_root, loaded.as_ref())?;
        if !matchers.is_empty() {
            filter_sourcemap_nodes(&mut root, &matchers);
        }
        if args.absolute_paths {
            make_sourcemap_paths_absolute(&mut root, &project_root);
        }
        let text = serde_json::to_string_pretty(&root)? + "\n";
        let output = if args.stdout {
            None
        } else {
            Some(
                args.output
                    .clone()
                    .unwrap_or_else(|| project_root.join("sourcemap.json")),
            )
        };
        if args.stdout {
            print!("{text}");
        } else if let Some(output) = output.as_ref() {
            write_utf8_file(output, &text)?;
            println!("[renium] wrote {}", output.display());
        }
        if !args.watch {
            return Ok(());
        }
        wait_for_sourcemap_change(
            &project_root,
            loaded.as_ref(),
            output.as_deref(),
            args.interval_ms.clamp(25, 1_000),
        )?;
    }
}

fn filter_sourcemap_nodes(node: &mut SourcemapNode, matchers: &[globset::GlobMatcher]) -> bool {
    node.children
        .retain_mut(|child| filter_sourcemap_nodes(child, matchers));
    !node.children.is_empty()
        || node
            .file_paths
            .iter()
            .any(|path| matchers.iter().any(|matcher| matcher.is_match(path)))
        || matchers.iter().any(|matcher| matcher.is_match(&node.name))
}

fn make_sourcemap_paths_absolute(node: &mut SourcemapNode, project_root: &Path) {
    for path in &mut node.file_paths {
        let current = Path::new(path);
        if !current.is_absolute() {
            *path = project_root
                .join(current)
                .to_string_lossy()
                .replace('\\', "/");
        }
    }
    for child in &mut node.children {
        make_sourcemap_paths_absolute(child, project_root);
    }
}

fn sourcemap_watch_inputs(
    project_root: &Path,
    loaded: Option<&config::LoadedProject>,
) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
    let mut directories = BTreeSet::new();
    let mut files = BTreeSet::new();
    if let Some(loaded) = loaded {
        for path in config::project_source_roots(loaded)? {
            directories.insert(absolutize_for_daemon(&path));
        }
        for path in config::project_config_paths(loaded)? {
            files.insert(absolutize_for_daemon(&path));
        }
    } else {
        directories.insert(absolutize_for_daemon(&project_root.join("src")));
    }
    Ok((directories, files))
}

fn wait_for_sourcemap_change(
    project_root: &Path,
    loaded: Option<&config::LoadedProject>,
    output: Option<&Path>,
    debounce_ms: u64,
) -> Result<()> {
    let (directories, files) = sourcemap_watch_inputs(project_root, loaded)?;
    let mut watcher = FileWatcher::new(4_096)?;
    watcher.set_inputs(&files, &directories)?;
    let output = output.map(absolutize_for_daemon);
    loop {
        if watcher.take_overflowed() {
            return Ok(());
        }
        let event = watcher
            .receiver()
            .recv()
            .context("Sourcemap watcher stopped")??;
        let relevant = event.paths.iter().any(|path| {
            let path = absolutize_for_daemon(path);
            output.as_ref() != Some(&path)
                && !path
                    .components()
                    .any(|part| part.as_os_str() == OsStr::new(".renium"))
                && (files.contains(&path)
                    || directories
                        .iter()
                        .any(|directory| path == *directory || path.starts_with(directory)))
        });
        if !relevant {
            continue;
        }
        loop {
            if watcher.take_overflowed() {
                return Ok(());
            }
            match watcher
                .receiver()
                .recv_timeout(Duration::from_millis(debounce_ms))
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => return Err(error).context("Sourcemap watcher failed"),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("Sourcemap watcher stopped")
                }
            }
        }
        return Ok(());
    }
}
