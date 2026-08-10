use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::bytecode_edit::next_editor_settings_id_fast;
use crate::command_args::{VcInitArgs, VcMergeArgs, VcTextconvArgs, ViewArgs};
use crate::editor_paths::build_editor_source_paths_by_index;
use crate::editor_sync::is_lua_source_class;
use crate::external_tools::run_git_checked;
use crate::file_io::{fnv1a_hex, resolve_link_project_root, strip_extended_prefix};
use crate::output::print_json_output;
use crate::package_links::RENIUM_DIR_GITIGNORE;
use crate::settings_bytecode::{
    SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance, reindex_reference_indices,
};
use crate::settings_tree::settings_children_by_parent;
use crate::snapshot_refs::{
    remap_record_reference_ids, settings_instance_path, stabilize_record_references,
};

const VC_GITATTRIBUTES_TEMPLATE: &str = "\
# Renium project version-control policy (written by `renium vc-init`).
* text=auto
*.lua text eol=lf
*.luau text eol=lf
*.json text eol=lf
*.toml text eol=lf
*.md text eol=lf
*.rbxmx text eol=lf
*.rbxlx text eol=lf
*.renium binary diff=renium merge=renium
*.rbxm binary
*.rbxl binary
*.png binary
*.jpg binary
*.jpeg binary
*.webp binary
*.ico binary
*.mp3 binary
*.ogg binary
";

const VC_GITIGNORE_LINES: &[&str] = &[
    "/*.rbxl",
    "/*.rbxlx",
    "*.rbxl.lock",
    "*.rbxlx.lock",
    "*.renium.lock",
    "/sourcemap.json",
    "/snapshots/",
    "/snapshots-profile/",
    "/reports/",
    "/tmp/",
];

fn vc_run_git(git_path: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let owned = args
        .iter()
        .map(|&value| value.to_owned())
        .collect::<Vec<_>>();
    run_git_checked(git_path, &owned, cwd)
}

pub(super) fn vc_init(args: VcInitArgs) -> Result<()> {
    let project_root = resolve_link_project_root(&args.project_root)?;

    let attributes_path = project_root.join(".gitattributes");
    let wrote_gitattributes = if attributes_path.exists() {
        false
    } else {
        fs::write(&attributes_path, VC_GITATTRIBUTES_TEMPLATE)
            .with_context(|| format!("Failed to write {}", attributes_path.display()))?;
        true
    };

    let ignore_path = project_root.join(".gitignore");
    let existing_ignore = fs::read_to_string(&ignore_path).unwrap_or_default();
    let existing_lines: HashSet<&str> = existing_ignore.lines().map(str::trim).collect();
    let added_ignore_lines: Vec<&str> = VC_GITIGNORE_LINES
        .iter()
        .copied()
        .filter(|line| !existing_lines.contains(line))
        .collect();
    if !added_ignore_lines.is_empty() {
        let mut out = existing_ignore.clone();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("# Renium version control (added by `renium vc-init`)\n");
        for line in &added_ignore_lines {
            out.push_str(line);
            out.push('\n');
        }
        fs::write(&ignore_path, out)
            .with_context(|| format!("Failed to write {}", ignore_path.display()))?;
    }

    let renium_dir = project_root.join(".renium");
    fs::create_dir_all(&renium_dir)
        .with_context(|| format!("Failed to create {}", renium_dir.display()))?;
    let renium_ignore_path = renium_dir.join(".gitignore");
    let current_renium_ignore = fs::read_to_string(&renium_ignore_path).unwrap_or_default();
    let renium_ignore_updated = if current_renium_ignore.is_empty() {
        fs::write(&renium_ignore_path, RENIUM_DIR_GITIGNORE)
            .with_context(|| format!("Failed to write {}", renium_ignore_path.display()))?;
        true
    } else {
        false
    };

    let mut git_initialized = false;
    let mut git_configured = false;
    let mut git_root = Value::Null;
    let mut remote = Value::Null;
    if !args.skip_git {
        let inside = vc_run_git(
            &args.git_path,
            &["rev-parse", "--is-inside-work-tree"],
            &project_root,
        )
        .map(|out| out.trim() == "true")
        .unwrap_or(false);
        if !inside {
            vc_run_git(&args.git_path, &["init"], &project_root)?;
            git_initialized = true;
        }
        if let Ok(top) = vc_run_git(
            &args.git_path,
            &["rev-parse", "--show-toplevel"],
            &project_root,
        ) {
            git_root = json!(top.trim());
        }
        let exe = std::env::current_exe()
            .ok()
            .map(strip_extended_prefix)
            .and_then(|path| path.to_str().map(str::to_string))
            .unwrap_or_else(|| "renium".to_string())
            .replace('\\', "/");
        let exe = if exe.contains(' ') {
            format!("\"{exe}\"")
        } else {
            exe
        };
        vc_run_git(
            &args.git_path,
            &[
                "config",
                "diff.renium.textconv",
                &format!("{exe} vc-textconv"),
            ],
            &project_root,
        )?;
        vc_run_git(
            &args.git_path,
            &["config", "merge.renium.name", "Renium settings store merge"],
            &project_root,
        )?;
        vc_run_git(
            &args.git_path,
            &[
                "config",
                "merge.renium.driver",
                &format!("{exe} vc-merge %O %A %B --path %P"),
            ],
            &project_root,
        )?;
        git_configured = true;
        if let Some(url) = &args.remote {
            let has_origin = vc_run_git(
                &args.git_path,
                &["remote", "get-url", "origin"],
                &project_root,
            )
            .is_ok();
            if has_origin {
                vc_run_git(
                    &args.git_path,
                    &["remote", "set-url", "origin", url],
                    &project_root,
                )?;
            } else {
                vc_run_git(
                    &args.git_path,
                    &["remote", "add", "origin", url],
                    &project_root,
                )?;
            }
            remote = json!(url);
        }
    }

    print_json_output(
        &json!({
            "ok": true,
            "projectRoot": project_root,
            "wroteGitattributes": wrote_gitattributes,
            "gitignoreAddedLines": added_ignore_lines,
            "reniumIgnoreUpdated": renium_ignore_updated,
            "gitInitialized": git_initialized,
            "gitConfigured": git_configured,
            "gitRoot": git_root,
            "remote": remote,
        }),
        args.pretty,
    )
}

pub(super) fn settings_doc_to_text(document: &SettingsBytecode) -> String {
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (index, instance) in document.instances.iter().enumerate() {
        children
            .entry(instance.parent_index)
            .or_default()
            .push(index);
    }
    let mut out = format!(
        "# renium store v{}: {} instances\n",
        document.version,
        document.instances.len()
    );
    let mut stack: Vec<(usize, String)> = Vec::new();
    for root in children
        .get(&None)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .rev()
    {
        stack.push((root, String::new()));
    }
    while let Some((index, parent_path)) = stack.pop() {
        let instance = &document.instances[index];
        let path = if parent_path.is_empty() {
            instance.name.clone()
        } else {
            format!("{parent_path}/{}", instance.name)
        };
        out.push_str(&format!(
            "= {path} [{}] id={}\n",
            instance.class_name, instance.settings_id
        ));
        let mut properties: Vec<(&String, &Value)> = instance.properties.iter().collect();
        properties.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in properties {
            if key == "Source"
                && let Some(text) = value.as_str()
            {
                out.push_str(&format!(
                    "  Source = <{} lines, {} bytes, fnv1a={}>\n",
                    text.lines().count(),
                    text.len(),
                    fnv1a_hex(text.as_bytes())
                ));
                continue;
            }
            out.push_str(&format!("  {key} = {value}\n"));
        }
        let mut attributes: Vec<(&String, &Value)> = instance.attributes.iter().collect();
        attributes.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in attributes {
            out.push_str(&format!("  @{key} = {value}\n"));
        }
        if let Some(kids) = children.get(&Some(index)) {
            for kid in kids.iter().rev() {
                stack.push((*kid, path.clone()));
            }
        }
    }
    out
}

pub(super) fn vc_textconv(args: VcTextconvArgs) -> Result<()> {
    let metadata = fs::metadata(&args.file)
        .with_context(|| format!("Failed to stat {}", args.file.display()))?;
    if metadata.len() == 0 {
        println!("# renium store: empty file");
        return Ok(());
    }
    let document = SettingsBytecode::read_file(&args.file)?;
    print!("{}", settings_doc_to_text(&document));
    Ok(())
}

fn build_view_node(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    source_paths: &[Option<PathBuf>],
    index: usize,
    visited: &mut HashSet<usize>,
) -> Value {
    let instance = &document.instances[index];
    let mut node = Map::new();
    node.insert("name".into(), json!(instance.name));
    node.insert("className".into(), json!(instance.class_name));
    node.insert("settingsId".into(), json!(instance.settings_id));

    let mut properties: Vec<(&String, &Value)> = instance
        .properties
        .iter()
        .filter(|(key, _)| key.as_str() != "Source")
        .collect();
    properties.sort_by(|a, b| a.0.cmp(b.0));
    if !properties.is_empty() {
        node.insert(
            "properties".into(),
            Value::Object(
                properties
                    .into_iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
    }
    if !instance.attributes.is_empty() {
        let mut attributes: Vec<(&String, &Value)> = instance.attributes.iter().collect();
        attributes.sort_by(|a, b| a.0.cmp(b.0));
        node.insert(
            "attributes".into(),
            Value::Object(
                attributes
                    .into_iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
    }
    if is_lua_source_class(&instance.class_name) {
        if let Some(source) = instance.properties.get("Source").and_then(Value::as_str) {
            node.insert("source".into(), json!(source));
        } else if let Some(Some(path)) = source_paths.get(index)
            && let Ok(source) = fs::read_to_string(path)
        {
            node.insert("source".into(), json!(source));
        }
    }

    let mut children = Vec::new();
    if let Some(kids) = children_by_parent.get(index) {
        for &kid in kids {
            if visited.insert(kid) {
                children.push(build_view_node(
                    document,
                    children_by_parent,
                    source_paths,
                    kid,
                    visited,
                ));
            }
        }
    }
    node.insert("childCount".into(), json!(children.len()));
    if !children.is_empty() {
        node.insert("children".into(), Value::Array(children));
    }
    Value::Object(node)
}

pub(super) fn settings_doc_to_json_tree(document: &SettingsBytecode, file: &Path) -> Value {
    let children_by_parent = settings_children_by_parent(document);
    let source_paths = match file.parent() {
        Some(dir) => {
            let service = document
                .instances
                .iter()
                .find(|instance| instance.parent_index.is_none())
                .map(|instance| instance.name.clone())
                .unwrap_or_default();
            build_editor_source_paths_by_index(document, &service, dir)
        }
        None => vec![None; document.instances.len()],
    };
    let mut visited: HashSet<usize> = HashSet::new();
    let mut roots = Vec::new();
    for (index, instance) in document.instances.iter().enumerate() {
        if instance.parent_index.is_none() && visited.insert(index) {
            roots.push(build_view_node(
                document,
                &children_by_parent,
                &source_paths,
                index,
                &mut visited,
            ));
        }
    }
    json!({
        "version": document.version,
        "instanceCount": document.instances.len(),
        "roots": roots,
    })
}

pub(super) fn view_command(args: ViewArgs) -> Result<()> {
    let metadata = fs::metadata(&args.file)
        .with_context(|| format!("File not found: {}", args.file.display()))?;
    if metadata.len() == 0 {
        if args.json {
            return print_json_output(
                &json!({"version": 0, "instanceCount": 0, "roots": []}),
                args.pretty,
            );
        }
        println!("# renium store: empty file");
        return Ok(());
    }
    let document = SettingsBytecode::read_file(&args.file)?;
    if args.json {
        return print_json_output(
            &settings_doc_to_json_tree(&document, &args.file),
            args.pretty,
        );
    }
    print!("{}", settings_doc_to_text(&document));
    Ok(())
}

#[derive(Debug)]
pub(super) struct VcMergeConflict {
    path: String,
    pub(super) detail: String,
}

#[derive(Clone)]
struct VcMergedInstance {
    settings_id: String,
    name: String,
    class_name: String,
    parent_id: Option<String>,
    properties: Map<String, Value>,
    attributes: Map<String, Value>,
}

fn settings_ids_by_index(document: &SettingsBytecode) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for (index, instance) in document.instances.iter().enumerate() {
        out.entry(instance.settings_id.clone()).or_insert(index);
    }
    out
}

fn settings_parent_id(document: &SettingsBytecode, index: usize) -> Option<String> {
    document.instances[index]
        .parent_index
        .and_then(|parent| document.instances.get(parent))
        .map(|parent| parent.settings_id.clone())
}

fn vc_instance_equal(
    a_doc: &SettingsBytecode,
    a: usize,
    b_doc: &SettingsBytecode,
    b: usize,
) -> bool {
    let ia = &a_doc.instances[a];
    let ib = &b_doc.instances[b];
    ia.name == ib.name
        && ia.class_name == ib.class_name
        && settings_parent_id(a_doc, a) == settings_parent_id(b_doc, b)
        && ia.properties == ib.properties
        && ia.attributes == ib.attributes
}

fn vc_render_short(value: Option<&Value>) -> String {
    match value {
        None => "<absent>".to_string(),
        Some(value) => {
            let text = value.to_string();
            if text.len() > 80 {
                format!(
                    "{}... ({} bytes)",
                    text.chars().take(60).collect::<String>(),
                    text.len()
                )
            } else {
                text
            }
        }
    }
}

fn vc_merge_scalar<T: Clone + PartialEq>(
    base: Option<&T>,
    ours: &T,
    theirs: &T,
    prefer: Option<bool>,
) -> Result<T, ()> {
    if ours == theirs {
        return Ok(ours.clone());
    }
    if Some(ours) == base {
        return Ok(theirs.clone());
    }
    if Some(theirs) == base {
        return Ok(ours.clone());
    }
    match prefer {
        Some(true) => Ok(ours.clone()),
        Some(false) => Ok(theirs.clone()),
        None => Err(()),
    }
}

struct VcMergeContext<'a> {
    prefer: Option<bool>,
    path: &'a str,
    conflicts: &'a mut Vec<VcMergeConflict>,
}

impl VcMergeContext<'_> {
    fn merge_maps(
        &mut self,
        base: &Map<String, Value>,
        ours: &Map<String, Value>,
        theirs: &Map<String, Value>,
        label: &str,
    ) -> Map<String, Value> {
        let mut keys: Vec<&String> = base
            .keys()
            .chain(ours.keys())
            .chain(theirs.keys())
            .collect();
        keys.sort();
        keys.dedup();
        let mut out = Map::new();
        for key in keys {
            let b = base.get(key.as_str());
            let o = ours.get(key.as_str());
            let t = theirs.get(key.as_str());
            let merged = if o == t {
                o.cloned()
            } else if o == b {
                t.cloned()
            } else if t == b {
                o.cloned()
            } else {
                match self.prefer {
                    Some(true) => o.cloned(),
                    Some(false) => t.cloned(),
                    None => {
                        self.conflicts.push(VcMergeConflict {
                            path: self.path.to_string(),
                            detail: format!(
                                "{label} {key}: ours={} theirs={} (base={})",
                                vc_render_short(o),
                                vc_render_short(t),
                                vc_render_short(b)
                            ),
                        });
                        o.cloned()
                    }
                }
            };
            if let Some(value) = merged {
                out.insert(key.clone(), value);
            }
        }
        out
    }
}

struct PreparedSettingsMerge {
    base: SettingsBytecode,
    ours: SettingsBytecode,
    theirs: SettingsBytecode,
    theirs_id_remap: HashMap<String, String>,
}

fn prepare_settings_merge(
    base: &SettingsBytecode,
    ours: &SettingsBytecode,
    theirs: &SettingsBytecode,
) -> PreparedSettingsMerge {
    let mut base = base.clone();
    let mut ours = ours.clone();
    let mut theirs = theirs.clone();
    stabilize_settings_document_references(&mut base);
    stabilize_settings_document_references(&mut ours);
    stabilize_settings_document_references(&mut theirs);
    let base_ids = settings_ids_by_index(&base);
    let ours_ids = settings_ids_by_index(&ours);
    let mut all_ids = [&base, &ours, &theirs]
        .into_iter()
        .flat_map(|document| {
            document
                .instances
                .iter()
                .map(|instance| instance.settings_id.clone())
        })
        .collect::<HashSet<_>>();
    let mut id_seed = all_ids.len();
    let mut theirs_id_remap = HashMap::new();
    for (theirs_index, instance) in theirs.instances.iter().enumerate() {
        let id = &instance.settings_id;
        if let Some(ours_index) = ours_ids.get(id).copied()
            && !base_ids.contains_key(id)
            && !vc_instance_equal(&ours, ours_index, &theirs, theirs_index)
        {
            let fresh = next_editor_settings_id_fast(&mut all_ids, &mut id_seed);
            theirs_id_remap.insert(id.clone(), fresh);
        }
    }
    for instance in &mut theirs.instances {
        remap_record_reference_ids(&mut instance.properties, &theirs_id_remap);
        remap_record_reference_ids(&mut instance.attributes, &theirs_id_remap);
    }
    PreparedSettingsMerge {
        base,
        ours,
        theirs,
        theirs_id_remap,
    }
}

fn merged_instance_from(document: &SettingsBytecode, index: usize) -> VcMergedInstance {
    VcMergedInstance {
        settings_id: document.instances[index].settings_id.clone(),
        name: document.instances[index].name.clone(),
        class_name: document.instances[index].class_name.clone(),
        parent_id: settings_parent_id(document, index),
        properties: document.instances[index].properties.clone(),
        attributes: document.instances[index].attributes.clone(),
    }
}

fn finish_settings_merge(
    mut merged: Vec<VcMergedInstance>,
    prefer: Option<bool>,
    version: u8,
    mut conflicts: Vec<VcMergeConflict>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    let index_by_id = merged
        .iter()
        .enumerate()
        .map(|(index, record)| (record.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (index, record) in merged.iter().enumerate() {
        match record
            .parent_id
            .as_deref()
            .and_then(|parent| index_by_id.get(parent))
        {
            Some(parent_index) => children.entry(Some(*parent_index)).or_default().push(index),
            None if record.parent_id.is_none() => children.entry(None).or_default().push(index),
            None if prefer.is_none() => conflicts.push(VcMergeConflict {
                path: record.name.clone(),
                detail: format!(
                    "added under a parent (id={}) deleted on the other side",
                    record.parent_id.as_deref().unwrap_or_default()
                ),
            }),
            None => {}
        }
    }
    let mut instances = Vec::new();
    let mut output_indices = HashMap::new();
    let mut stack = children
        .get(&None)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    while let Some(index) = stack.pop() {
        let parent_index = merged[index]
            .parent_id
            .as_deref()
            .and_then(|parent| index_by_id.get(parent))
            .and_then(|parent| output_indices.get(parent))
            .copied();
        output_indices.insert(index, instances.len());
        let record = &mut merged[index];
        instances.push(SettingsBytecodeInstance {
            settings_id: std::mem::take(&mut record.settings_id),
            name: std::mem::take(&mut record.name),
            class_name: std::mem::take(&mut record.class_name),
            parent_index,
            properties: std::mem::take(&mut record.properties),
            attributes: std::mem::take(&mut record.attributes),
        });
        if let Some(child_indices) = children.get(&Some(index)) {
            stack.extend(child_indices.iter().rev().copied());
        }
    }
    let indices_by_id = instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    for instance in &mut instances {
        reindex_reference_indices(&mut instance.properties, &indices_by_id);
        reindex_reference_indices(&mut instance.attributes, &indices_by_id);
    }
    (SettingsBytecode { version, instances }, conflicts)
}

pub(super) fn merge_settings_documents(
    base: &SettingsBytecode,
    ours: &SettingsBytecode,
    theirs: &SettingsBytecode,
    prefer: Option<bool>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    let PreparedSettingsMerge {
        base,
        ours,
        theirs,
        theirs_id_remap,
    } = prepare_settings_merge(base, ours, theirs);
    let version = ours.version.max(theirs.version);
    let base = &base;
    let ours = &ours;
    let theirs = &theirs;
    let mut conflicts: Vec<VcMergeConflict> = Vec::new();
    let base_ids = settings_ids_by_index(base);
    let ours_ids = settings_ids_by_index(ours);
    let theirs_ids = settings_ids_by_index(theirs);

    let mut merged: Vec<VcMergedInstance> = Vec::new();
    let mut merged_ids: HashSet<String> = HashSet::new();

    for (ours_index, instance) in ours.instances.iter().enumerate() {
        let id = &instance.settings_id;
        if merged_ids.contains(id) {
            continue;
        }
        let inst_path = settings_instance_path(ours, ours_index);
        match (base_ids.get(id).copied(), theirs_ids.get(id).copied()) {
            (Some(base_index), Some(theirs_index)) => {
                let base_inst = &base.instances[base_index];
                let theirs_inst = &theirs.instances[theirs_index];
                let name = match vc_merge_scalar(
                    Some(&base_inst.name),
                    &instance.name,
                    &theirs_inst.name,
                    prefer,
                ) {
                    Ok(value) => value,
                    Err(()) => {
                        conflicts.push(VcMergeConflict {
                            path: inst_path.clone(),
                            detail: format!(
                                "Name: ours={} theirs={}",
                                instance.name, theirs_inst.name
                            ),
                        });
                        instance.name.clone()
                    }
                };
                let class_name = match vc_merge_scalar(
                    Some(&base_inst.class_name),
                    &instance.class_name,
                    &theirs_inst.class_name,
                    prefer,
                ) {
                    Ok(value) => value,
                    Err(()) => {
                        conflicts.push(VcMergeConflict {
                            path: inst_path.clone(),
                            detail: format!(
                                "ClassName: ours={} theirs={}",
                                instance.class_name, theirs_inst.class_name
                            ),
                        });
                        instance.class_name.clone()
                    }
                };
                let ours_parent = settings_parent_id(ours, ours_index);
                let base_parent = settings_parent_id(base, base_index);
                let theirs_parent = settings_parent_id(theirs, theirs_index);
                let parent_id =
                    match vc_merge_scalar(Some(&base_parent), &ours_parent, &theirs_parent, prefer)
                    {
                        Ok(value) => value,
                        Err(()) => {
                            conflicts.push(VcMergeConflict {
                                path: inst_path.clone(),
                                detail: "Parent: moved to different parents on both sides"
                                    .to_string(),
                            });
                            ours_parent.clone()
                        }
                    };
                let mut merge_context = VcMergeContext {
                    prefer,
                    path: &inst_path,
                    conflicts: &mut conflicts,
                };
                let properties = merge_context.merge_maps(
                    &base_inst.properties,
                    &instance.properties,
                    &theirs_inst.properties,
                    "property",
                );
                let attributes = merge_context.merge_maps(
                    &base_inst.attributes,
                    &instance.attributes,
                    &theirs_inst.attributes,
                    "attribute",
                );
                merged.push(VcMergedInstance {
                    settings_id: id.clone(),
                    name,
                    class_name,
                    parent_id,
                    properties,
                    attributes,
                });
                merged_ids.insert(id.clone());
            }
            (Some(base_index), None) => {
                if vc_instance_equal(base, base_index, ours, ours_index) {
                    continue;
                }
                match prefer {
                    Some(false) => {}
                    Some(true) => {
                        merged.push(merged_instance_from(ours, ours_index));
                        merged_ids.insert(id.clone());
                    }
                    None => {
                        conflicts.push(VcMergeConflict {
                            path: inst_path.clone(),
                            detail: "modified here but deleted on the other side".to_string(),
                        });
                        merged.push(merged_instance_from(ours, ours_index));
                        merged_ids.insert(id.clone());
                    }
                }
            }
            (None, _) => {
                merged.push(merged_instance_from(ours, ours_index));
                merged_ids.insert(id.clone());
            }
        }
    }

    for (theirs_index, instance) in theirs.instances.iter().enumerate() {
        let id = &instance.settings_id;
        let inst_path = settings_instance_path(theirs, theirs_index);
        let remapped_parent = settings_parent_id(theirs, theirs_index)
            .map(|pid| theirs_id_remap.get(&pid).cloned().unwrap_or(pid));
        if ours_ids.contains_key(id) {
            if let Some(fresh) = theirs_id_remap.get(id).cloned() {
                let mut properties = instance.properties.clone();
                let mut attributes = instance.attributes.clone();
                remap_record_reference_ids(&mut properties, &theirs_id_remap);
                remap_record_reference_ids(&mut attributes, &theirs_id_remap);
                merged.push(VcMergedInstance {
                    settings_id: fresh.clone(),
                    name: instance.name.clone(),
                    class_name: instance.class_name.clone(),
                    parent_id: remapped_parent,
                    properties,
                    attributes,
                });
                merged_ids.insert(fresh);
            }
            continue;
        }
        if let Some(base_index) = base_ids.get(id).copied() {
            if vc_instance_equal(base, base_index, theirs, theirs_index) {
                continue;
            }
            match prefer {
                Some(true) => continue,
                Some(false) => {
                    let mut properties = instance.properties.clone();
                    let mut attributes = instance.attributes.clone();
                    remap_record_reference_ids(&mut properties, &theirs_id_remap);
                    remap_record_reference_ids(&mut attributes, &theirs_id_remap);
                    merged.push(VcMergedInstance {
                        settings_id: id.clone(),
                        name: instance.name.clone(),
                        class_name: instance.class_name.clone(),
                        parent_id: remapped_parent,
                        properties,
                        attributes,
                    });
                    merged_ids.insert(id.clone());
                }
                None => {
                    conflicts.push(VcMergeConflict {
                        path: inst_path,
                        detail: "deleted here but modified on the other side".to_string(),
                    });
                }
            }
            continue;
        }
        let mut properties = instance.properties.clone();
        let mut attributes = instance.attributes.clone();
        remap_record_reference_ids(&mut properties, &theirs_id_remap);
        remap_record_reference_ids(&mut attributes, &theirs_id_remap);
        merged.push(VcMergedInstance {
            settings_id: theirs_id_remap
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.clone()),
            name: instance.name.clone(),
            class_name: instance.class_name.clone(),
            parent_id: remapped_parent,
            properties,
            attributes,
        });
        merged_ids.insert(
            theirs_id_remap
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.clone()),
        );
    }

    finish_settings_merge(merged, prefer, version, conflicts)
}

fn stabilize_settings_document_references(document: &mut SettingsBytecode) {
    let ids = document
        .instances
        .iter()
        .map(|instance| instance.settings_id.clone())
        .collect::<Vec<_>>();
    for instance in &mut document.instances {
        stabilize_record_references(&mut instance.properties, &ids);
        stabilize_record_references(&mut instance.attributes, &ids);
    }
}

fn read_settings_doc_or_empty(path: &Path, label: &str) -> Result<SettingsBytecode> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to stat {label} file {}", path.display()))?;
    if metadata.len() == 0 {
        return Ok(SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: Vec::new(),
        });
    }
    SettingsBytecode::read_file(path)
        .with_context(|| format!("Failed to read {label} file {}", path.display()))
}

pub(super) fn vc_merge(args: VcMergeArgs) -> Result<()> {
    let prefer = match args.prefer.as_deref() {
        None => None,
        Some("ours") => Some(true),
        Some("theirs") => Some(false),
        Some(other) => bail!("--prefer must be `ours` or `theirs`, got `{other}`"),
    };
    let base = read_settings_doc_or_empty(&args.base, "base")?;
    let ours = read_settings_doc_or_empty(&args.ours, "ours")?;
    let theirs = read_settings_doc_or_empty(&args.theirs, "theirs")?;
    let label = args
        .path
        .clone()
        .unwrap_or_else(|| args.ours.display().to_string());

    let (merged, conflicts) = merge_settings_documents(&base, &ours, &theirs, prefer);

    if !conflicts.is_empty() {
        for conflict in &conflicts {
            eprintln!(
                "[renium vc-merge] conflict in {label}: {} -- {}",
                conflict.path, conflict.detail
            );
        }
        bail!(
            "{} merge conflict(s) in {label}. Re-run with --prefer ours|theirs, or resolve the listed properties manually and retry.",
            conflicts.len()
        );
    }

    let out_path = args.output.clone().unwrap_or_else(|| args.ours.clone());
    merged.write_file(&out_path)?;
    if args.output.is_none() && !args.pretty {
        return Ok(());
    }
    print_json_output(
        &json!({
            "ok": true,
            "path": label,
            "instances": merged.instances.len(),
            "output": out_path,
        }),
        args.pretty,
    )
}
