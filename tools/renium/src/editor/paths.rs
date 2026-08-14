use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use walkdir::WalkDir;

use crate::editor::document::ensure_editor_source_target_in_bytecode;
use crate::editor::sync::is_lua_source_class;
use crate::editor::types::{EditorInstancePath, EditorSourcePathSpec, EditorSourceTarget};
use crate::project::config;
use crate::settings::bytecode::SettingsBytecode;
use crate::settings::tree::{
    assign_editor_instance_paths, editor_child_stems, editor_service_root_index,
    settings_children_by_parent,
};
use crate::system::files::{path_key, sanitize_name, strip_extended_prefix};

pub(crate) fn script_file_names(class_name: &str) -> Option<(&'static str, &'static str)> {
    script_file_names_for_run_context(class_name, None)
}

pub(crate) fn project_script_file_names(
    parent_dir: &Path,
    fs_stem: &str,
    has_children: bool,
    class_name: &str,
    properties: &Map<String, Value>,
) -> Option<(String, String)> {
    let run_context = properties.get("RunContext").and_then(run_context_name);
    let naming = config::cached_script_naming(parent_dir);
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
    if naming.extension == config::ScriptExtensionPolicy::Preserve {
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
        config::ScriptExtensionPolicy::Lua => "lua",
        config::ScriptExtensionPolicy::Preserve | config::ScriptExtensionPolicy::Luau => "luau",
    };
    Some((
        format!("init{suffix}.{extension}"),
        format!("{suffix}.{extension}"),
    ))
}

pub(crate) fn project_script_path(
    parent_dir: &Path,
    fs_stem: &str,
    has_children: bool,
    class_name: &str,
    properties: &Map<String, Value>,
) -> Option<PathBuf> {
    let (source_file_name, leaf_suffix) =
        project_script_file_names(parent_dir, fs_stem, has_children, class_name, properties)?;
    Some(if has_children {
        parent_dir.join(fs_stem).join(source_file_name)
    } else {
        parent_dir.join(format!("{fs_stem}{leaf_suffix}"))
    })
}

pub(crate) fn script_file_names_for_run_context(
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

pub(crate) fn run_context_name(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| {
        value.as_object().and_then(|object| {
            object
                .get("name")
                .or_else(|| object.get("Name"))
                .and_then(Value::as_str)
        })
    })
}

pub(crate) fn editor_run_context_value(name: &str) -> Value {
    json!({
        "_type": "EnumItem",
        "enumType": "Enum.RunContext",
        "name": name,
    })
}

pub(crate) fn build_editor_source_path_map(
    document: &SettingsBytecode,
    service: &str,
    service_dir: &Path,
) -> HashMap<String, EditorSourceTarget> {
    let children_by_parent = settings_children_by_parent(document);
    let Some(root_index) = editor_service_root_index(document, service) else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    let mut walk = EditorSourceMapWalk {
        document,
        children_by_parent: &children_by_parent,
        service,
        map: &mut map,
        path_segments: vec![document.instances[root_index].name.clone()],
        path_ordinals: vec![1],
    };
    walk.append_children(&children_by_parent[root_index], service_dir);
    map
}

pub(crate) fn merge_editor_source_files_into_document(
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

pub(crate) fn build_editor_source_paths_by_index(
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

pub(crate) fn build_editor_source_paths_by_index_with_children(
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
    let mut walk = EditorSourcePathWalk {
        document,
        children_by_parent,
        contains_source: &contains_source,
        out: &mut out,
    };
    walk.append_children(&children_by_parent[root_index], service_dir);
    out
}

struct EditorSourcePathWalk<'a> {
    document: &'a SettingsBytecode,
    children_by_parent: &'a [Vec<usize>],
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
            .map_or(&[][..], Vec::as_slice);
        let has_children = !child_indices.is_empty();
        let source_path = project_script_path(
            parent_dir,
            fs_stem,
            has_children,
            &instance.class_name,
            &instance.properties,
        );
        if let Some(source_path) = source_path {
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
            .map_or(&[][..], Vec::as_slice);
        let has_children = !child_indices.is_empty();
        let source_path = project_script_path(
            parent_dir,
            fs_stem,
            has_children,
            &instance.class_name,
            &instance.properties,
        );
        let is_source = source_path.is_some();
        if let Some(source_path) = source_path {
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

pub(crate) type EditorPathSegments = Vec<Option<Vec<String>>>;
pub(crate) type EditorPathOrdinals = Vec<Option<Vec<usize>>>;

pub(crate) fn build_editor_instance_path_parts(
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

pub(crate) fn build_editor_instance_paths(
    document: &SettingsBytecode,
    service: &str,
) -> Vec<Option<EditorInstancePath>> {
    let children_by_parent = settings_children_by_parent(document);
    build_editor_instance_paths_with_children(document, service, &children_by_parent)
}

pub(crate) fn build_editor_instance_paths_with_children(
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

pub(crate) fn path_ordinals_match(candidate: &[usize], path_ordinals: &[usize]) -> bool {
    candidate == path_ordinals || candidate.ends_with(path_ordinals)
}

pub(crate) fn document_instance_index_by_path_unique(
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

pub(crate) fn infer_editor_source_path_spec(
    src_root: &Path,
    service: &str,
    source_path: &Path,
) -> Option<EditorSourcePathSpec> {
    infer_editor_source_path_spec_in_service(&src_root.join(service), service, source_path)
}

pub(crate) fn infer_editor_source_path_spec_in_service(
    service_dir: &Path,
    service: &str,
    source_path: &Path,
) -> Option<EditorSourcePathSpec> {
    let file_name = source_path.file_name()?.to_string_lossy();
    let naming = config::cached_script_naming(source_path.parent().unwrap_or(service_dir));
    let (class_name, leaf_name, run_context) = infer_source_script(&file_name, &naming)?;
    let run_context = run_context.map(str::to_string);
    let relative = source_path.strip_prefix(service_dir).ok()?;
    let mut components = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
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

pub(crate) fn infer_source_script(
    file_name: &str,
    naming: &config::ProjectScriptNaming,
) -> Option<(&'static str, Option<String>, Option<&'static str>)> {
    let extensions: &[&str] = match naming.extension {
        config::ScriptExtensionPolicy::Lua => &["lua"],
        config::ScriptExtensionPolicy::Luau => &["luau"],
        config::ScriptExtensionPolicy::Preserve => &["luau", "lua"],
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
    let mut best: Option<(usize, &'static str, Option<&'static str>, bool)> = None;
    for extension in extensions {
        for &(configured_suffix, class_name, run_context) in &patterns {
            let suffix = format!("{configured_suffix}.{extension}").to_ascii_lowercase();
            let is_init = lower
                .strip_prefix("init")
                .is_some_and(|rest| rest == suffix);
            if (is_init || lower.ends_with(&suffix) && file_name.len() > suffix.len())
                && best.is_none_or(|(length, ..)| suffix.len() > length)
            {
                best = Some((suffix.len(), class_name, run_context, is_init));
            }
        }
    }
    best.map(|(suffix_len, class_name, run_context, is_init)| {
        let stem = (!is_init).then(|| file_name[..file_name.len() - suffix_len].to_string());
        (class_name, stem, run_context)
    })
}

pub(crate) fn service_from_changed_path(src_root: &Path, changed_path: &Path) -> Option<String> {
    let src_norm = strip_extended_prefix(src_root.to_path_buf());
    let changed_norm = strip_extended_prefix(changed_path.to_path_buf());
    if let Ok(relative) = changed_norm.strip_prefix(&src_norm) {
        let mut components = relative.components();
        return match components.next()? {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
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
