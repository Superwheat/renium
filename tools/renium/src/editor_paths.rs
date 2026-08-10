use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use walkdir::WalkDir;

use super::editor_document::ensure_editor_source_target_in_bytecode;
use super::editor_sync::is_lua_source_class;
use super::editor_types::{EditorInstancePath, EditorSourcePathSpec, EditorSourceTarget};
use super::file_io::{path_key, sanitize_name, strip_extended_prefix};
use super::project_config;
use super::settings_bytecode::SettingsBytecode;
use super::settings_tree::{
    assign_editor_instance_paths, editor_child_stems, editor_service_root_index,
    settings_children_by_parent,
};

pub(super) fn script_file_names(class_name: &str) -> Option<(&'static str, &'static str)> {
    script_file_names_for_run_context(class_name, None)
}

pub(super) fn project_script_file_names(
    _project_root: &Path,
    parent_dir: &Path,
    fs_stem: &str,
    has_children: bool,
    class_name: &str,
    properties: &Map<String, Value>,
) -> Option<(String, String)> {
    let run_context = properties.get("RunContext").and_then(run_context_name);
    let naming = project_config::cached_script_naming(parent_dir);
    let suffix = if class_name == "Script"
        && run_context.is_some_and(|value| value.eq_ignore_ascii_case("Client"))
    {
        naming.client_run_context_suffix
    } else if class_name == "Script"
        && run_context.is_some_and(|value| value.eq_ignore_ascii_case("Plugin"))
    {
        naming.plugin_suffix
    } else {
        match class_name {
            "Script" => naming.server_suffix,
            "LocalScript" => naming.client_suffix,
            "ModuleScript" => naming.module_suffix,
            _ => return None,
        }
    };
    if naming.extension == project_config::ScriptExtensionPolicy::Preserve {
        for extension in ["luau", "lua"] {
            let source_file_name = format!("init{suffix}.{extension}");
            let leaf_suffix = format!("{suffix}.{extension}");
            let path = if has_children {
                parent_dir.join(fs_stem).join(&source_file_name)
            } else {
                parent_dir.join(format!("{fs_stem}{leaf_suffix}"))
            };
            if path.is_file() {
                return Some((source_file_name, leaf_suffix));
            }
        }
    }
    let extension = match naming.extension {
        project_config::ScriptExtensionPolicy::Lua => "lua",
        project_config::ScriptExtensionPolicy::Preserve
        | project_config::ScriptExtensionPolicy::Luau => "luau",
    };
    Some((
        format!("init{suffix}.{extension}"),
        format!("{suffix}.{extension}"),
    ))
}

pub(super) fn script_file_names_for_run_context(
    class_name: &str,
    run_context: Option<&str>,
) -> Option<(&'static str, &'static str)> {
    match class_name {
        "Script" if run_context.is_some_and(|value| value.eq_ignore_ascii_case("Client")) => {
            Some(("init.client.luau", ".client.luau"))
        }
        "Script" if run_context.is_some_and(|value| value.eq_ignore_ascii_case("Plugin")) => {
            Some(("init.plugin.luau", ".plugin.luau"))
        }
        "Script" => Some(("init.server.luau", ".server.luau")),
        "LocalScript" => Some(("init.client.luau", ".client.luau")),
        "ModuleScript" => Some(("init.luau", ".luau")),
        _ => None,
    }
}

pub(super) fn run_context_name(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| {
        value.as_object().and_then(|object| {
            object
                .get("name")
                .or_else(|| object.get("Name"))
                .and_then(Value::as_str)
        })
    })
}

pub(super) fn editor_run_context_value(name: &str) -> Value {
    json!({
        "_type": "EnumItem",
        "enumType": "Enum.RunContext",
        "name": name,
    })
}

pub(super) fn build_editor_source_path_map(
    document: &SettingsBytecode,
    service: &str,
    service_dir: &Path,
) -> HashMap<String, EditorSourceTarget> {
    let children_by_parent = settings_children_by_parent(document);
    let Some(root_index) = editor_service_root_index(document, service) else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    let naming_root = service_dir.parent().unwrap_or(service_dir);
    let mut walk = EditorSourceMapWalk {
        document,
        children_by_parent: &children_by_parent,
        service,
        naming_root,
        map: &mut map,
        path_segments: vec![document.instances[root_index].name.clone()],
        path_ordinals: vec![1],
    };
    walk.append_children(&children_by_parent[root_index], service_dir);
    map
}

pub(super) fn merge_editor_source_files_into_document(
    document: &mut SettingsBytecode,
    service: &str,
    service_dir: &Path,
) -> Result<(bool, HashMap<String, PathBuf>)> {
    let source_paths = build_editor_source_path_map(document, service, service_dir);
    let mut files = WalkDir::new(service_dir)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() => {
                infer_editor_source_path_spec_in_service(service_dir, service, entry.path())
                    .map(|spec| Ok((entry.into_path(), spec)))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to walk {}", service_dir.display()))?;
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut changed = false;
    let mut actual_source_paths = HashMap::new();
    for (path, spec) in files {
        let target = if let Some(target) = source_paths.get(&path_key(&path)) {
            target.clone()
        } else {
            let ensured = ensure_editor_source_target_in_bytecode(document, &spec)?;
            changed |= ensured.changed;
            ensured.target
        };
        let settings_id = target
            .settings_id
            .context("Source file target has no settings ID")?;
        if let Some(previous) = actual_source_paths.insert(settings_id, path.clone())
            && path_key(&previous) != path_key(&path)
        {
            bail!(
                "Source files {} and {} map to the same instance",
                previous.display(),
                path.display()
            );
        }
    }

    for instance in &mut document.instances {
        if !is_lua_source_class(&instance.class_name)
            || actual_source_paths.contains_key(&instance.settings_id)
        {
            continue;
        }
        instance.class_name = "Folder".to_string();
        instance.properties.remove("Source");
        instance.properties.remove("RunContext");
        changed = true;
    }
    Ok((changed, actual_source_paths))
}

pub(super) fn build_editor_source_paths_by_index(
    document: &SettingsBytecode,
    service: &str,
    service_dir: &Path,
) -> Vec<Option<PathBuf>> {
    let children_by_parent = settings_children_by_parent(document);
    build_editor_source_paths_by_index_with_children(
        document,
        service,
        service_dir,
        &children_by_parent,
    )
}

pub(super) fn build_editor_source_paths_by_index_with_children(
    document: &SettingsBytecode,
    service: &str,
    service_dir: &Path,
    children_by_parent: &[Vec<usize>],
) -> Vec<Option<PathBuf>> {
    let mut out = vec![None; document.instances.len()];
    let mut contains_source = vec![false; document.instances.len()];
    for (index, instance) in document.instances.iter().enumerate() {
        if !is_lua_source_class(&instance.class_name) {
            continue;
        }
        let mut current = Some(index);
        let mut remaining = document.instances.len() + 1;
        while let Some(current_index) = current
            && remaining > 0
        {
            let Some(current_instance) = document.instances.get(current_index) else {
                break;
            };
            contains_source[current_index] = true;
            current = current_instance.parent_index;
            remaining -= 1;
        }
    }
    let Some(root_index) = editor_service_root_index(document, service) else {
        return out;
    };
    let naming_root = service_dir.parent().unwrap_or(service_dir);
    let mut walk = EditorSourcePathWalk {
        document,
        children_by_parent,
        naming_root,
        contains_source: &contains_source,
        out: &mut out,
    };
    walk.append_children(&children_by_parent[root_index], service_dir);
    out
}

struct EditorSourcePathWalk<'a> {
    document: &'a SettingsBytecode,
    children_by_parent: &'a [Vec<usize>],
    naming_root: &'a Path,
    contains_source: &'a [bool],
    out: &'a mut [Option<PathBuf>],
}

impl EditorSourcePathWalk<'_> {
    fn append_children(&mut self, child_indices: &[usize], parent_dir: &Path) {
        for (child_index, child_stem, _) in editor_child_stems(self.document, child_indices) {
            if self
                .contains_source
                .get(child_index)
                .copied()
                .unwrap_or(false)
            {
                self.append_node(child_index, parent_dir, &child_stem);
            }
        }
    }

    fn append_node(&mut self, index: usize, parent_dir: &Path, fs_stem: &str) {
        let instance = &self.document.instances[index];
        let child_indices = self
            .children_by_parent
            .get(index)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let has_children = !child_indices.is_empty();

        let source_file = project_script_file_names(
            self.naming_root,
            parent_dir,
            fs_stem,
            has_children,
            &instance.class_name,
            &instance.properties,
        );
        if let Some((source_file_name, leaf_suffix)) = source_file {
            let source_path = if has_children {
                parent_dir.join(fs_stem).join(source_file_name)
            } else {
                parent_dir.join(format!("{fs_stem}{leaf_suffix}"))
            };
            if let Some(slot) = self.out.get_mut(index) {
                *slot = Some(source_path);
            }
            if !has_children {
                return;
            }
        }

        self.append_children(child_indices, &parent_dir.join(fs_stem));
    }
}

struct EditorSourceMapWalk<'a> {
    document: &'a SettingsBytecode,
    children_by_parent: &'a [Vec<usize>],
    service: &'a str,
    naming_root: &'a Path,
    map: &'a mut HashMap<String, EditorSourceTarget>,
    path_segments: Vec<String>,
    path_ordinals: Vec<usize>,
}

impl EditorSourceMapWalk<'_> {
    fn append_children(&mut self, child_indices: &[usize], parent_dir: &Path) {
        for (child_index, child_stem, child_ordinal) in
            editor_child_stems(self.document, child_indices)
        {
            self.append_node(child_index, parent_dir, &child_stem, child_ordinal);
        }
    }

    fn append_node(
        &mut self,
        index: usize,
        parent_dir: &Path,
        fs_stem: &str,
        child_ordinal: usize,
    ) {
        let instance = &self.document.instances[index];
        self.path_segments.push(instance.name.clone());
        self.path_ordinals.push(child_ordinal);
        let child_indices = self
            .children_by_parent
            .get(index)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let has_children = !child_indices.is_empty();

        let source_file = project_script_file_names(
            self.naming_root,
            parent_dir,
            fs_stem,
            has_children,
            &instance.class_name,
            &instance.properties,
        );
        let is_source = source_file.is_some();
        if let Some((source_file_name, leaf_suffix)) = source_file {
            let source_path = if has_children {
                parent_dir.join(fs_stem).join(source_file_name)
            } else {
                parent_dir.join(format!("{fs_stem}{leaf_suffix}"))
            };
            self.map.insert(
                path_key(&source_path),
                EditorSourceTarget {
                    service: self.service.to_string(),
                    settings_id: Some(instance.settings_id.clone()),
                    path_segments: self.path_segments.clone(),
                    path_ordinals: self.path_ordinals.clone(),
                    class_name: instance.class_name.clone(),
                },
            );
        }
        if !is_source || has_children {
            self.append_children(child_indices, &parent_dir.join(fs_stem));
        }
        self.path_segments.pop();
        self.path_ordinals.pop();
    }
}

pub(super) fn build_editor_instance_path_segments(
    document: &SettingsBytecode,
    service: &str,
) -> Vec<Option<Vec<String>>> {
    build_editor_instance_paths(document, service)
        .into_iter()
        .map(|path| path.map(|path| path.path_segments))
        .collect()
}

pub(super) type EditorPathSegments = Vec<Option<Vec<String>>>;
pub(super) type EditorPathOrdinals = Vec<Option<Vec<usize>>>;

pub(super) fn build_editor_instance_path_parts(
    document: &SettingsBytecode,
    service: &str,
) -> (EditorPathSegments, EditorPathOrdinals) {
    let paths = build_editor_instance_paths(document, service);
    let mut segments = Vec::with_capacity(paths.len());
    let mut ordinals = Vec::with_capacity(paths.len());
    for path in paths {
        if let Some(path) = path {
            segments.push(Some(path.path_segments));
            ordinals.push(Some(path.path_ordinals));
        } else {
            segments.push(None);
            ordinals.push(None);
        }
    }
    (segments, ordinals)
}

pub(super) fn build_editor_instance_paths(
    document: &SettingsBytecode,
    service: &str,
) -> Vec<Option<EditorInstancePath>> {
    let children_by_parent = settings_children_by_parent(document);
    build_editor_instance_paths_with_children(document, service, &children_by_parent)
}

pub(super) fn build_editor_instance_paths_with_children(
    document: &SettingsBytecode,
    service: &str,
    children_by_parent: &[Vec<usize>],
) -> Vec<Option<EditorInstancePath>> {
    let mut out = vec![None; document.instances.len()];
    let Some(root_index) = editor_service_root_index(document, service) else {
        return out;
    };
    let mut root_segments = vec![document.instances[root_index].name.clone()];
    let mut root_ordinals = vec![1];
    assign_editor_instance_paths(
        document,
        children_by_parent,
        root_index,
        &mut root_segments,
        &mut root_ordinals,
        &mut out,
    );
    out
}

fn document_instance_path_matches(
    document: &SettingsBytecode,
    path_segments: &[String],
    path_ordinals: &[usize],
) -> Vec<(usize, EditorInstancePath)> {
    let Some(service) = path_segments.first() else {
        return Vec::new();
    };
    build_editor_instance_paths(document, service)
        .into_iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let path = path?;
            if path.path_segments != path_segments {
                return None;
            }
            if !path_ordinals.is_empty() && !path_ordinals_match(&path.path_ordinals, path_ordinals)
            {
                return None;
            }
            Some((index, path))
        })
        .collect()
}

pub(super) fn path_ordinals_match(candidate: &[usize], path_ordinals: &[usize]) -> bool {
    candidate == path_ordinals || candidate.ends_with(path_ordinals)
}

pub(super) fn document_instance_index_by_path_unique(
    document: &SettingsBytecode,
    path_segments: &[String],
    path_ordinals: &[usize],
) -> Result<usize> {
    if path_segments.is_empty() {
        bail!("Path selector cannot be empty");
    }
    let matches = document_instance_path_matches(document, path_segments, path_ordinals);
    match matches.len() {
        0 => bail!("No matching instance path: {}", path_segments.join(".")),
        1 => Ok(matches[0].0),
        _ => {
            let candidates = matches
                .iter()
                .take(8)
                .filter_map(|(index, path)| {
                    let instance = document.instances.get(*index)?;
                    Some(format!("{}@{:?}", instance.settings_id, path.path_ordinals))
                })
                .collect::<Vec<_>>()
                .join(",");
            bail!(
                "Ambiguous instance path: {} matched {} instances [{}]. Use --ords or --id.",
                path_segments.join("."),
                matches.len(),
                candidates
            );
        }
    }
}

pub(super) fn infer_editor_source_path_spec(
    src_root: &Path,
    service: &str,
    source_path: &Path,
) -> Option<EditorSourcePathSpec> {
    infer_editor_source_path_spec_in_service(&src_root.join(service), service, source_path)
}

pub(super) fn infer_editor_source_path_spec_in_service(
    service_dir: &Path,
    service: &str,
    source_path: &Path,
) -> Option<EditorSourcePathSpec> {
    let file_name = source_path.file_name()?.to_string_lossy();
    let naming = project_config::cached_script_naming(source_path.parent().unwrap_or(service_dir));
    let (class_name, leaf_name, run_context) = infer_source_script(&file_name, &naming)?;
    let run_context = run_context.map(str::to_string);
    let relative = source_path.strip_prefix(service_dir).ok()?;
    let mut components = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if components.is_empty() {
        return None;
    }
    components.pop();

    let (parent_components, instance_name) = if let Some(leaf_name) = leaf_name {
        (components, leaf_name)
    } else {
        let instance_name = components.pop()?;
        (components, instance_name)
    };
    let instance_stem = sanitize_name(&instance_name);
    let mut path_segments = Vec::with_capacity(parent_components.len() + 2);
    path_segments.push(service.to_string());
    path_segments.extend(parent_components.iter().cloned());
    path_segments.push(instance_name.clone());

    Some(EditorSourcePathSpec {
        service: service.to_string(),
        class_name: class_name.to_string(),
        run_context,
        instance_name,
        instance_stem,
        parent_components,
        path_segments,
    })
}

pub(crate) fn infer_source_class_and_leaf_name(
    file_name: &str,
) -> Option<(&'static str, Option<String>)> {
    infer_source_script(file_name, &project_config::ProjectScriptNaming::default())
        .map(|(class_name, leaf, _)| (class_name, leaf))
}

pub(crate) fn infer_source_script(
    file_name: &str,
    naming: &project_config::ProjectScriptNaming,
) -> Option<(&'static str, Option<String>, Option<&'static str>)> {
    let extensions: &[&str] = match naming.extension {
        project_config::ScriptExtensionPolicy::Lua => &["lua"],
        project_config::ScriptExtensionPolicy::Luau => &["luau"],
        project_config::ScriptExtensionPolicy::Preserve => &["luau", "lua"],
    };
    let lower = file_name.to_ascii_lowercase();
    let patterns = [
        (
            naming.client_run_context_suffix.as_str(),
            "Script",
            Some("Client"),
        ),
        (naming.plugin_suffix.as_str(), "Script", Some("Plugin")),
        (naming.server_suffix.as_str(), "Script", Some("Legacy")),
        (naming.client_suffix.as_str(), "LocalScript", None),
        (naming.module_suffix.as_str(), "ModuleScript", None),
    ];
    let mut candidates = Vec::new();
    for extension in extensions {
        for &(configured_suffix, class_name, run_context) in &patterns {
            let suffix = format!("{configured_suffix}.{extension}").to_ascii_lowercase();
            candidates.push((suffix, class_name, run_context));
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0.len()));
    for (suffix, class_name, run_context) in candidates {
        let init = format!("init{suffix}");
        if lower == init {
            return Some((class_name, None, run_context));
        }
        if lower.ends_with(&suffix) && file_name.len() > suffix.len() {
            return Some((
                class_name,
                Some(file_name[..file_name.len() - suffix.len()].to_string()),
                run_context,
            ));
        }
    }
    None
}

pub(super) fn service_from_changed_path(src_root: &Path, changed_path: &Path) -> Option<String> {
    let src_norm = strip_extended_prefix(src_root.to_path_buf());
    let changed_norm = strip_extended_prefix(changed_path.to_path_buf());
    if let Ok(relative) = changed_norm.strip_prefix(&src_norm) {
        let mut components = relative.components();
        return match components.next()? {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        };
    }
    let src_key = path_key(&src_norm);
    let changed_key = path_key(&changed_norm);
    let rest = changed_key.strip_prefix(&src_key)?;
    let rest = rest.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let changed_text = changed_norm.to_string_lossy();
    let original_rest = changed_text.get(changed_text.len() - rest.len()..)?;
    original_rest
        .split(['/', '\\'])
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
