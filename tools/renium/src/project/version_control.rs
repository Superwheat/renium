use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde_json::{Map, Value, json};

use crate::app::output::{JsonParts, print_json_output, visible};
use crate::bytecode::edit::next_editor_settings_id_fast;
use crate::bytecode::stabilize_reference_output;
use crate::cli::args::{VcInitArgs, VcMergeArgs, VcTextconvArgs, ViewArgs};
use crate::editor::paths::{build_editor_instance_path_parts, build_editor_source_paths_by_index};
use crate::editor::sync::is_lua_source_class;
use crate::project::package_links::RENIUM_DIR_GITIGNORE;
use crate::rbx::model::read_settings_model_document;
use crate::settings::EXTERNAL_SOURCE_MARKER;
use crate::settings::bytecode::blocks::RowReader;
use crate::settings::bytecode::{
    SETTINGS_BINARY_VERSION, SETTINGS_REFERENCE_SELECTOR_KEYS, SettingsBytecode,
    SettingsBytecodeInstance, decode_settings_bytecode, for_each_reference, for_each_reference_mut,
    reference_leads_out_of, reindex_store_reference_indices,
};
use crate::settings::tree::settings_children_by_parent;
use crate::snapshot::refs::{
    remap_store_reference_ids, settings_instance_path, stabilize_record_references,
};
use crate::system::files::{
    fnv1a_hex, resolve_link_project_root, strip_extended_prefix, write_bytes_if_changed,
};
use crate::system::tools::run_git_checked;

const VC_GITATTRIBUTES_TEMPLATE: &str = "\
# Renium project version-control policy (written by `renium vc-init`).
* text=auto
.gitattributes text eol=lf
.gitignore text eol=lf
.renium/.gitignore text eol=lf
*.lua text eol=lf
*.luau text eol=lf
*.json text eol=lf
*.jsonc text eol=lf
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
*.mp4 binary
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

fn read_optional_text(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn append_missing_lines<'a>(
    path: &Path,
    heading: Option<&str>,
    required: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<&'a str>> {
    let existing = read_optional_text(path)?;
    let existing_lines = existing.lines().map(str::trim).collect::<HashSet<_>>();
    let missing = required
        .into_iter()
        .filter(|line| !existing_lines.contains(line.trim()))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(missing);
    }

    let mut output = existing;
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    if !output.is_empty() {
        output.push('\n');
    }
    if let Some(heading) = heading {
        output.push_str(heading);
        output.push('\n');
    }
    for line in &missing {
        output.push_str(line);
        output.push('\n');
    }
    fs::write(path, output).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(missing)
}

pub(crate) fn ensure_renium_local_state_ignored(project_root: &Path) -> Result<()> {
    let renium_dir = project_root.join(".renium");
    fs::create_dir_all(&renium_dir)
        .with_context(|| format!("Failed to create {}", renium_dir.display()))?;
    append_missing_lines(
        &renium_dir.join(".gitignore"),
        None,
        RENIUM_DIR_GITIGNORE.lines(),
    )?;
    Ok(())
}

pub(crate) fn vc_init(args: VcInitArgs) -> Result<()> {
    let project_root = resolve_link_project_root(&args.project_root)?;

    let attributes_path = project_root.join(".gitattributes");
    let wrote_gitattributes =
        !append_missing_lines(&attributes_path, None, VC_GITATTRIBUTES_TEMPLATE.lines())?
            .is_empty();

    let ignore_path = project_root.join(".gitignore");
    let added_ignore_lines = append_missing_lines(
        &ignore_path,
        Some("# Renium version control (added by `renium vc-init`)"),
        VC_GITIGNORE_LINES.iter().copied(),
    )?;

    let renium_dir = project_root.join(".renium");
    fs::create_dir_all(&renium_dir)
        .with_context(|| format!("Failed to create {}", renium_dir.display()))?;
    let renium_ignore_path = renium_dir.join(".gitignore");
    let renium_ignore_updated =
        !append_missing_lines(&renium_ignore_path, None, RENIUM_DIR_GITIGNORE.lines())?.is_empty();

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
        .is_ok_and(|out| out.trim() == "true");
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
                &format!("{exe} vc-merge \"%O\" \"%A\" \"%B\" --path \"%P\""),
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

/// What `settings_doc_to_text` gives of a document before the lines of its
/// instances, and the instances in the order of their lines, each with its
/// path. None of it reads a value, so a store read by its rows has it whole.
struct TextLayout {
    head: String,
    path_segments_by_index: Vec<Option<Vec<String>>>,
    path_ordinals_by_index: Vec<Option<Vec<usize>>>,
    canonical_settings_ids_by_index: Vec<Option<String>>,
    rows: Vec<(usize, String)>,
}

impl TextLayout {
    fn of(document: &SettingsBytecode) -> Self {
        let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        for (index, instance) in document.instances.iter().enumerate() {
            children
                .entry(instance.parent_index)
                .or_default()
                .push(index);
        }
        let head = format!(
            "# renium store v{}: {} instances\n",
            document.version,
            document.instances.len()
        );
        let service = document
            .instances
            .iter()
            .find(|instance| instance.parent_index.is_none())
            .map(|instance| instance.name.as_str())
            .unwrap_or_default();
        let (path_segments_by_index, path_ordinals_by_index) =
            build_editor_instance_path_parts(document, service);
        let canonical_settings_ids_by_index = document
            .instances
            .iter()
            .map(|instance| Some(instance.settings_id.clone()))
            .collect::<Vec<_>>();
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
        let mut rows = Vec::with_capacity(document.instances.len());
        while let Some((index, parent_path)) = stack.pop() {
            let instance = &document.instances[index];
            let path = if parent_path.is_empty() {
                visible(&instance.name).into_owned()
            } else {
                format!("{parent_path}/{}", visible(&instance.name))
            };
            if let Some(kids) = children.get(&Some(index)) {
                for kid in kids.iter().rev() {
                    stack.push((*kid, path.clone()));
                }
            }
            rows.push((index, path));
        }
        Self {
            head,
            path_segments_by_index,
            path_ordinals_by_index,
            canonical_settings_ids_by_index,
            rows,
        }
    }

    /// The lines of `rows`, some of the layout's, which hold their values in
    /// `document`.
    fn render(&self, document: &SettingsBytecode, rows: &[(usize, String)]) -> String {
        let stabilize = |record: &mut Map<String, Value>| {
            stabilize_reference_output(
                document,
                &self.path_segments_by_index,
                &self.path_ordinals_by_index,
                &self.canonical_settings_ids_by_index,
                record,
            )
        };
        let mut out = String::new();
        for (index, path) in rows {
            let instance = &document.instances[*index];
            let _ = writeln!(
                out,
                "= {path} [{}] id={}",
                visible(&instance.class_name),
                visible(&instance.settings_id)
            );
            let property_record = text_record(&instance.properties, stabilize);
            let mut properties: Vec<(&String, &Value)> = property_record.iter().collect();
            properties.sort_by(|a, b| a.0.cmp(b.0));
            for (key, value) in properties {
                if key == "Source"
                    && let Some(text) = value.as_str()
                {
                    let _ = writeln!(
                        out,
                        "  Source = <{} lines, {} bytes, fnv1a={}>",
                        text.lines().count(),
                        text.len(),
                        fnv1a_hex(text.as_bytes())
                    );
                    continue;
                }
                let _ = writeln!(out, "  {} = {}", visible(key), visible(&value.to_string()));
            }
            let attribute_record = text_record(&instance.attributes, stabilize);
            let mut attributes: Vec<(&String, &Value)> = attribute_record.iter().collect();
            attributes.sort_by(|a, b| a.0.cmp(b.0));
            for (key, value) in attributes {
                let _ = writeln!(out, "  @{} = {}", visible(key), visible(&value.to_string()));
            }
        }
        out
    }

    /// `render` of `rows` in pieces that follow one another.
    fn pieces(&self, document: &SettingsBytecode, rows: &[(usize, String)]) -> Vec<String> {
        // Indexed chunks preserve the traversal order, regardless of worker order.
        let workers = rayon::current_num_threads();
        if rows.len() >= 2_048 && workers > 1 {
            (rows.par_chunks(rows.len().div_ceil(workers * 2).clamp(64, 512)))
                .map(|rows| self.render(document, rows))
                .collect()
        } else {
            vec![self.render(document, rows)]
        }
    }
}

pub(crate) fn settings_doc_to_text(document: &SettingsBytecode) -> String {
    let layout = TextLayout::of(document);
    let pieces = layout.pieces(document, &layout.rows);
    let mut out = layout.head;
    out.reserve(pieces.iter().map(String::len).sum());
    for piece in pieces {
        out.push_str(&piece);
    }
    out
}

/// The rows a view or a comparison reads at a time: those of one block of a
/// store.
pub(super) const VIEW_ROWS_AT_ONCE: usize = 4_096;

/// What gives the rows of a document their values a turn at a time, as
/// `RowReader::walk_turns` does: the reader of a store, or the DOM of a place
/// file.
pub(super) trait RowTurns {
    fn turns(
        &self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        at_once: usize,
        visit: impl FnMut(&SettingsBytecode, &[usize]) -> Result<()>,
    ) -> Result<()>;
}

impl RowTurns for RowReader {
    fn turns(
        &self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        at_once: usize,
        visit: impl FnMut(&SettingsBytecode, &[usize]) -> Result<()>,
    ) -> Result<()> {
        self.walk_turns(document, rows, at_once, visit)
    }
}

impl RowTurns for super::place_diff::DomRows<'_> {
    fn turns(
        &self,
        document: &mut SettingsBytecode,
        rows: &[usize],
        at_once: usize,
        visit: impl FnMut(&SettingsBytecode, &[usize]) -> Result<()>,
    ) -> Result<()> {
        self.walk_turns(document, rows, at_once, visit)
    }
}

/// Gives `out`, piece after piece, `settings_doc_to_text` of the document
/// `values` reads, whose instances `document` holds without their values:
/// `at_once` rows at a time in the order of their lines, so that neither the
/// document nor its text is ever whole in memory.
pub(super) fn text_by_rows(
    document: &mut SettingsBytecode,
    values: &impl RowTurns,
    at_once: usize,
    mut out: impl FnMut(String),
) -> Result<()> {
    let mut layout = TextLayout::of(document);
    out(std::mem::take(&mut layout.head));
    let order = (layout.rows.iter().map(|(index, _)| *index)).collect::<Vec<_>>();
    let mut given = 0;
    values.turns(document, &order, at_once, |document, turn| {
        let rows = &layout.rows[given..given + turn.len()];
        given += turn.len();
        (layout.pieces(document, rows).into_iter()).for_each(&mut out);
        Ok(())
    })
}

/// Gives `out` `settings_doc_to_text` of a store: of the document whole, or
/// of the one its reader gives the values of some rows at a time.
fn store_text(
    (mut document, reader): (SettingsBytecode, Option<RowReader>),
    mut out: impl FnMut(String),
) -> Result<()> {
    let Some(reader) = reader else {
        out(settings_doc_to_text(&document));
        return Ok(());
    };
    text_by_rows(&mut document, &reader, VIEW_ROWS_AT_ONCE, out)
}

/// Prints `settings_doc_to_text` of a store file.
fn print_store_text(path: &Path) -> Result<()> {
    // A process that serves automation prints each `print!` as a line.
    let read = if crate::app::context::automation_stdio() {
        (SettingsBytecode::read_file(path)?, None)
    } else {
        SettingsBytecode::read_whole_by_rows(path)?
    };
    store_text(read, |piece| print!("{piece}"))
}

/// Gives `out` `settings_doc_to_text` of the bytes of a store file, as
/// `print_store_text` prints it. A store its rows do not give is decoded
/// whole, which says why.
pub(crate) fn store_text_of(bytes: &[u8], out: impl FnMut(String)) -> Result<()> {
    let by_rows = if crate::app::context::automation_stdio() {
        None
    } else {
        SettingsBytecode::decode_whole_by_rows(bytes).ok()
    };
    let read = match by_rows {
        Some(read) => read,
        None => (decode_settings_bytecode(bytes)?, None),
    };
    store_text(read, out)
}

/// Gives `out` `settings_doc_to_text` of the document of a place file, which
/// is read from the file's DOM `at_once` rows at a time. The DOM holds no
/// value of a binary file that is its reflection default: a row states those
/// again while its turn is converted.
pub(super) fn place_text(path: &Path, at_once: usize, out: impl FnMut(String)) -> Result<()> {
    let place = super::place_file::sparse_place_dom(path)?;
    let mut rows = super::place_diff::dom_rows(&place.dom, "place", None, false)?;
    rows.left_out = place.elided_defaults;
    text_by_rows(&mut rows.structure(), &rows, at_once, out)
}

fn text_record<'a>(
    record: &'a Map<String, Value>,
    stabilize: impl FnOnce(&mut Map<String, Value>),
) -> Cow<'a, Map<String, Value>> {
    // Stabilization visits only reference objects. Ordinary fields can be
    // rendered directly, including strings large enough to dominate copying.
    let has_reference = record.values().any(|value| {
        let mut found = false;
        for_each_reference(value, &mut |_| found = true);
        found
    });
    if has_reference {
        let mut record = record.clone();
        stabilize(&mut record);
        Cow::Owned(record)
    } else {
        Cow::Borrowed(record)
    }
}

pub(crate) fn vc_textconv(args: VcTextconvArgs) -> Result<()> {
    let metadata = fs::metadata(&args.file)
        .with_context(|| format!("Failed to stat {}", args.file.display()))?;
    if metadata.len() == 0 {
        println!("# renium store: empty file");
        return Ok(());
    }
    print_store_text(&args.file)
}

/// What `settings_doc_to_json_tree` reads of a document beside the values of
/// its instances, which a store read by its rows has whole once each Script
/// holds its RunContext.
struct ViewTree {
    children_by_parent: Vec<Vec<usize>>,
    source_paths: Vec<Option<PathBuf>>,
    path_segments_by_index: Vec<Option<Vec<String>>>,
    path_ordinals_by_index: Vec<Option<Vec<usize>>>,
    canonical_settings_ids_by_index: Vec<Option<String>>,
}

impl ViewTree {
    fn of(document: &SettingsBytecode, file: &Path) -> Self {
        let children_by_parent = settings_children_by_parent(document);
        let service = document
            .instances
            .iter()
            .find(|instance| instance.parent_index.is_none())
            .map(|instance| instance.name.clone())
            .unwrap_or_default();
        let (path_segments_by_index, path_ordinals_by_index) =
            build_editor_instance_path_parts(document, &service);
        let canonical_settings_ids_by_index = document
            .instances
            .iter()
            .map(|instance| Some(instance.settings_id.clone()))
            .collect::<Vec<_>>();
        let source_paths = build_editor_source_paths_by_index(
            document,
            &service,
            &super::storage::source_directory(file),
        );
        Self {
            children_by_parent,
            source_paths,
            path_segments_by_index,
            path_ordinals_by_index,
            canonical_settings_ids_by_index,
        }
    }

    fn roots(document: &SettingsBytecode) -> impl Iterator<Item = usize> + '_ {
        (0..document.instances.len())
            .filter(|index| document.instances[*index].parent_index.is_none())
    }

    /// The node of an instance, which holds its values in `document`, without
    /// its children.
    fn node(&self, document: &SettingsBytecode, index: usize) -> Map<String, Value> {
        let instance = &document.instances[index];
        let mut node = Map::new();
        node.insert("name".into(), json!(instance.name));
        node.insert("className".into(), json!(instance.class_name));
        node.insert("settingsId".into(), json!(instance.settings_id));
        let stabilized = |mut record: Map<String, Value>| {
            stabilize_reference_output(
                document,
                &self.path_segments_by_index,
                &self.path_ordinals_by_index,
                &self.canonical_settings_ids_by_index,
                &mut record,
            );
            Value::Object(record)
        };
        let properties = (instance.properties.iter())
            .filter(|(key, _)| key.as_str() != "Source")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Map<String, Value>>();
        if !properties.is_empty() {
            node.insert("properties".into(), stabilized(properties));
        }
        if !instance.attributes.is_empty() {
            node.insert("attributes".into(), stabilized(instance.attributes.clone()));
        }
        if is_lua_source_class(&instance.class_name) {
            if let Some(source) = instance.properties.get("Source").and_then(Value::as_str) {
                node.insert("source".into(), json!(source));
            } else if let Some(Some(path)) = self.source_paths.get(index)
                && let Ok(source) = fs::read_to_string(path)
            {
                node.insert("source".into(), json!(source));
            }
        }
        node
    }

    fn build(&self, document: &SettingsBytecode, index: usize) -> Value {
        let mut node = self.node(document, index);
        let children = (self.children_by_parent[index].iter())
            .map(|kid| self.build(document, *kid))
            .collect::<Vec<_>>();
        node.insert("childCount".into(), json!(children.len()));
        if !children.is_empty() {
            node.insert("children".into(), Value::Array(children));
        }
        Value::Object(node)
    }
}

pub(crate) fn settings_doc_to_json_tree(document: &SettingsBytecode, file: &Path) -> Value {
    let tree = ViewTree::of(document, file);
    let roots = ViewTree::roots(document)
        .map(|root| tree.build(document, root))
        .collect::<Vec<_>>();
    json!({
        "version": document.version,
        "instanceCount": document.instances.len(),
        "roots": roots,
    })
}

/// The text of a JSON tree whose nodes are given one at a time, each before
/// its children: a node is the text before its children and the text after
/// them, at the depth its first line is indented to where the tree is pretty.
struct JsonTreeText {
    pretty: bool,
    /// The nodes whose children are not all given: the text after them, how
    /// many are to come, and the node's depth.
    open: Vec<(String, usize, usize)>,
    text: String,
}

impl JsonTreeText {
    fn line(&mut self, depth: usize) {
        if self.pretty {
            self.text.push('\n');
            self.text.extend(std::iter::repeat_n("  ", depth));
        }
    }

    fn put(&mut self, (before, after): (String, String), children: usize, depth: usize) {
        self.text.push_str(&before);
        if children > 0 {
            self.text.push('[');
            self.line(depth + 2);
            self.open.push((after, children, depth));
            return;
        }
        while let Some((_, left, depth)) = self.open.last_mut() {
            *left -= 1;
            let (left, depth) = (*left, *depth);
            if left > 0 {
                self.text.push(',');
                self.line(depth + 2);
                break;
            }
            self.line(depth + 1);
            self.text.push(']');
            let (after, ..) = self.open.pop().unwrap_or_default();
            self.text.push_str(&after);
        }
    }
}

/// Gives `out`, piece after piece, the line `parts` prints for
/// `settings_doc_to_json_tree` of the store `reader` reads, whose instances
/// `document` holds without their values but each Script's RunContext:
/// `at_once` rows at a time in the order of the tree, so that neither the
/// document nor the tree is ever whole in memory.
pub(super) fn store_json_by_rows(
    document: &mut SettingsBytecode,
    reader: &RowReader,
    file: &Path,
    parts: &JsonParts,
    at_once: usize,
    mut out: impl FnMut(String),
) -> Result<()> {
    let tree = ViewTree::of(document, file);
    let roots = ViewTree::roots(document).collect::<Vec<_>>();
    let mut order = Vec::with_capacity(document.instances.len());
    let mut pending = (roots.iter().rev().map(|root| (*root, 2))).collect::<Vec<_>>();
    while let Some((index, depth)) = pending.pop() {
        order.push((index, depth));
        let kids = tree.children_by_parent[index].iter().rev();
        pending.extend(kids.map(|kid| (*kid, depth + 2)));
    }
    // No value of a store holds this text, drawn anew by every process, so
    // it stands in a node's text only where its children go.
    let children = format!(
        "\u{1}children:{:016x}",
        ahash::RandomState::new().hash_one(0_u8)
    );
    let children_text = serde_json::to_string(&children)?;
    let halves = |node: Map<String, Value>, depth: usize| -> Result<(String, String)> {
        let mut text = parts.text(Value::Object(node))?;
        if parts.pretty && depth > 0 {
            text = text.replace('\n', &format!("\n{}", "  ".repeat(depth)));
        }
        Ok(match text.split_once(&children_text) {
            Some((before, after)) => (before.to_string(), after.to_string()),
            None => (text, String::new()),
        })
    };
    let mut top = Map::new();
    top.insert("version".into(), json!(document.version));
    top.insert("instanceCount".into(), json!(document.instances.len()));
    top.insert(
        "roots".into(),
        if roots.is_empty() {
            json!([])
        } else {
            json!(children)
        },
    );
    let mut written = JsonTreeText {
        pretty: parts.pretty,
        open: Vec::new(),
        text: String::new(),
    };
    written.put(halves(top, 0)?, roots.len(), 0);
    let rows = order.iter().map(|(index, _)| *index).collect::<Vec<_>>();
    let mut given = 0;
    reader.walk_turns(document, &rows, at_once, |document, turn| {
        let nodes = &order[given..given + turn.len()];
        given += turn.len();
        let texts = (nodes.par_iter())
            .map(|(index, depth)| {
                let kids = tree.children_by_parent[*index].len();
                let mut node = tree.node(document, *index);
                node.insert("childCount".into(), json!(kids));
                if kids > 0 {
                    node.insert("children".into(), json!(children));
                }
                halves(node, *depth)
            })
            .collect::<Result<Vec<_>>>()?;
        for ((index, depth), halves) in nodes.iter().zip(texts) {
            written.put(halves, tree.children_by_parent[*index].len(), *depth);
        }
        out(std::mem::take(&mut written.text));
        Ok(())
    })?;
    out(written.text + "\n");
    Ok(())
}

/// Prints `settings_doc_to_json_tree` of a store file as `print_json_output`
/// prints it.
fn print_store_json(path: &Path, pretty: bool) -> Result<()> {
    let parts = crate::app::output::json_parts(pretty);
    let (mut document, reader) = if parts.is_some() {
        crate::bytecode::read_whole_store(path)?
    } else {
        (SettingsBytecode::read_file(path)?, None)
    };
    let (Some(parts), Some(reader)) = (parts, reader) else {
        return print_json_output(&settings_doc_to_json_tree(&document, path), pretty);
    };
    store_json_by_rows(
        &mut document,
        &reader,
        path,
        &parts,
        VIEW_ROWS_AT_ONCE,
        |piece| print!("{piece}"),
    )
}

pub(crate) fn view_command(args: ViewArgs) -> Result<()> {
    let metadata = fs::metadata(&args.file)
        .with_context(|| format!("File not found: {}", args.file.display()))?;
    let extension = args
        .file
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    if metadata.len() == 0 && extension.as_deref() == Some("renium") {
        if args.json {
            return print_json_output(
                &json!({"version": 0, "instanceCount": 0, "roots": []}),
                args.pretty,
            );
        }
        println!("# renium store: empty file");
        return Ok(());
    }
    if extension.as_deref() == Some("renium") {
        return if args.json {
            print_store_json(&args.file, args.pretty)
        } else {
            print_store_text(&args.file)
        };
    }
    // A process that serves automation prints each `print!` as a line.
    if matches!(extension.as_deref(), Some("rbxl" | "rbxlx"))
        && !args.json
        && !crate::app::context::automation_stdio()
    {
        return place_text(&args.file, VIEW_ROWS_AT_ONCE, |piece| print!("{piece}"));
    }
    let document = match extension.as_deref() {
        Some("rbxm" | "rbxmx") => read_settings_model_document(&args.file)?,
        Some("rbxl" | "rbxlx") => super::place_diff::document(
            &super::place_file::place_dom(&args.file)?,
            "place",
            None,
            false,
        )?,
        _ => bail!(
            "Unsupported file type for {}; expected .renium, .rbxm, .rbxmx, .rbxl, or .rbxlx",
            args.file.display()
        ),
    };
    if args.json {
        return print_json_output(
            &settings_doc_to_json_tree(&document, &args.file),
            args.pretty,
        );
    }
    print!("{}", settings_doc_to_text(&document));
    Ok(())
}

/// What a conflict of a store merge is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VcConflictKind {
    Name,
    ClassName,
    Parent,
    Property,
    Attribute,
    /// One side changed an instance the other side removed.
    Removal,
    /// An instance one side put under a parent the other side removed.
    Orphan,
    /// Moves on both sides that put instances under each other.
    Cycle,
    /// A reference to an instance the other side removed.
    Reference,
}

/// What the base and each side hold where a conflict is about values;
/// `None` where that place lacks it.
#[derive(Clone)]
pub(crate) struct VcConflictValues {
    pub(crate) base: Option<Value>,
    pub(crate) ours: Option<Value>,
    pub(crate) theirs: Option<Value>,
}

/// One conflict of a store merge. `path` and `detail` are the text
/// `vc-merge` prints; the other fields say what it is about.
#[derive(Clone)]
pub(crate) struct VcMergeConflict {
    pub(crate) path: String,
    pub(crate) detail: String,
    pub(crate) kind: VcConflictKind,
    /// The instance's id, as the merge uses it.
    pub(crate) settings_id: String,
    /// The instance's path on a side that has it.
    pub(crate) instance_path: String,
    /// The property, attribute or referring field.
    pub(crate) field: Option<String>,
    pub(crate) values: Option<VcConflictValues>,
    /// The side (`true` ours) an earlier decision this conflict hangs on
    /// took.
    pub(crate) follows: Option<bool>,
    /// Whether choosing a side settles it.
    pub(crate) decidable: bool,
    /// Whether the instance is itself one side's removal, a conflict this
    /// one is part of: keeping the instance places it, removing it leaves
    /// nothing to place.
    pub(crate) part_of_removal: bool,
    /// For a conflict that hangs on another instance (a removed parent or
    /// target): what deciding ours, then theirs, does.
    pub(crate) effects: Option<[String; 2]>,
}

/// Settles the conflicts of a store merge as the merge meets them.
pub(crate) trait VcResolve {
    /// The side a conflict takes (`true` ours), or `None` to leave it open.
    fn resolve(&mut self, conflict: &VcMergeConflict) -> Option<bool>;

    /// Hears that the decision of `by` brought back an instance the merge
    /// had removed, which overrules a decision that removed it.
    fn restored(&mut self, _settings_id: &str, _by: &VcMergeConflict) {}
}

/// One preference for every value conflict and one for every structural
/// conflict.
pub(crate) struct VcPreference {
    pub(crate) value: Option<bool>,
    pub(crate) structural: Option<bool>,
}

impl VcResolve for VcPreference {
    fn resolve(&mut self, conflict: &VcMergeConflict) -> Option<bool> {
        if !conflict.decidable {
            return None;
        }
        match conflict.kind {
            VcConflictKind::Property | VcConflictKind::Attribute => self.value,
            _ => self.structural,
        }
    }
}

/// The two sides a merge takes records from (ours first), each with the id
/// remap its records need under the ids the merge uses.
struct MergeSides<'a> {
    documents: [&'a SettingsBytecode; 2],
    remaps: [&'a HashMap<String, String>; 2],
    ids: [HashMap<String, usize>; 2],
    service: &'a str,
}

impl<'a> MergeSides<'a> {
    fn new(
        documents: [&'a SettingsBytecode; 2],
        remaps: [&'a HashMap<String, String>; 2],
        service: &'a str,
    ) -> Self {
        Self {
            ids: documents.map(settings_ids_by_index),
            documents,
            remaps,
            service,
        }
    }

    fn document(&self, ours: bool) -> &'a SettingsBytecode {
        self.documents[usize::from(!ours)]
    }

    fn index(&self, ours: bool, id: &str) -> Option<usize> {
        self.ids[usize::from(!ours)].get(id).copied()
    }

    /// A side's record of an instance, with the ids the merge uses.
    fn record(&self, ours: bool, index: usize) -> VcMergedInstance {
        let side = usize::from(!ours);
        preferred_instance(self.documents[side], index, self.remaps[side], self.service)
    }

    /// A side's parent of an instance, with the id the merge uses.
    fn parent_of(&self, ours: bool, id: &str) -> Option<String> {
        let side = usize::from(!ours);
        let parent = settings_parent_id(self.documents[side], self.index(ours, id)?)?;
        Some(self.remaps[side].get(&parent).cloned().unwrap_or(parent))
    }

    /// Where a side holds an instance.
    fn path_on(&self, ours: bool, id: &str) -> Option<String> {
        self.index(ours, id)
            .map(|index| settings_instance_path(self.document(ours), index))
    }

    /// Where the first side that has an instance holds it.
    fn instance_path(&self, id: &str, name: &str) -> String {
        self.path_on(true, id)
            .or_else(|| self.path_on(false, id))
            .unwrap_or_else(|| name.to_string())
    }

    /// Where an instance stands under a parent, on the first side that has
    /// the parent.
    fn child_path(&self, parent: &str, name: &str) -> String {
        self.path_on(true, parent)
            .or_else(|| self.path_on(false, parent))
            .map_or_else(|| name.to_string(), |parent| format!("{parent}/{name}"))
    }

    /// Where each side holds an instance.
    fn paths(&self, id: &str) -> VcConflictValues {
        VcConflictValues {
            base: None,
            ours: self.path_on(true, id).map(Value::String),
            theirs: self.path_on(false, id).map(Value::String),
        }
    }
}

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

// Two branches that pulled the same Studio-created instance carry the same id,
// name, class and parent; differing property edits merge like any other
// three-way change instead of producing a second copy.
fn vc_instance_same_identity(
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
}

fn vc_render_short(value: Option<&Value>) -> String {
    match value {
        None => "<absent>".to_string(),
        Some(value) => {
            let text = crate::settings::held::resolve(value).to_string();
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

struct VcMergeContext<'a> {
    resolver: &'a mut dyn VcResolve,
    document: &'a SettingsBytecode,
    index: usize,
    conflicts: &'a mut Vec<VcMergeConflict>,
}

impl VcMergeContext<'_> {
    fn property_is_engine_state(&self, name: &str, value: &Value) -> bool {
        crate::settings::equivalence::reconciliation_property_is_engine_state(
            rbx_reflection_database::get().ok(),
            &self.document.instances[self.index].class_name,
            name,
            value,
        )
    }

    fn conflict(
        &self,
        kind: VcConflictKind,
        field: Option<&str>,
        values: VcConflictValues,
        detail: String,
    ) -> VcMergeConflict {
        let path = settings_instance_path(self.document, self.index);
        VcMergeConflict {
            instance_path: path.clone(),
            path,
            detail,
            kind,
            settings_id: self.document.instances[self.index].settings_id.clone(),
            field: field.map(str::to_string),
            values: Some(values),
            follows: None,
            decidable: true,
            part_of_removal: false,
            effects: None,
        }
    }

    /// The side a conflict takes, or `None` after recording it.
    fn settle(&mut self, conflict: VcMergeConflict) -> Option<bool> {
        let side = self.resolver.resolve(&conflict);
        if side.is_none() {
            self.conflicts.push(conflict);
        }
        side
    }

    fn merge_scalar<T: Clone + PartialEq>(
        &mut self,
        base: Option<&T>,
        ours: &T,
        theirs: &T,
        conflict: impl FnOnce(&Self) -> VcMergeConflict,
    ) -> T {
        if ours == theirs {
            return ours.clone();
        }
        if Some(ours) == base {
            return theirs.clone();
        }
        if Some(theirs) == base {
            return ours.clone();
        }
        let conflict = conflict(self);
        match self.settle(conflict) {
            Some(false) => theirs.clone(),
            _ => ours.clone(),
        }
    }

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
            } else if label == "property"
                && self.property_is_engine_state(key, o.or(t).unwrap_or(&Value::Null))
            {
                // The engine recomputes these on load, so a difference is not
                // an authored change on either side.
                o.cloned()
            } else {
                let kind = if label == "property" {
                    VcConflictKind::Property
                } else {
                    VcConflictKind::Attribute
                };
                let conflict = self.conflict(
                    kind,
                    Some(key),
                    VcConflictValues {
                        base: b.cloned(),
                        ours: o.cloned(),
                        theirs: t.cloned(),
                    },
                    format!(
                        "{label} {key}: ours={} theirs={} (base={})",
                        vc_render_short(o),
                        vc_render_short(t),
                        vc_render_short(b)
                    ),
                );
                match self.settle(conflict) {
                    Some(false) => t.cloned(),
                    _ => o.cloned(),
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
    /// The service the merged store is of.
    service: String,
}

/// The service a merge's store is of: the name of the root on the first
/// side that has one.
fn merged_store_service(documents: [&SettingsBytecode; 3]) -> String {
    documents
        .into_iter()
        .flat_map(|document| &document.instances)
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .unwrap_or_default()
}

fn prepare_settings_merge(
    base: &SettingsBytecode,
    ours: &SettingsBytecode,
    theirs: &SettingsBytecode,
    aligned_additions: &HashSet<String>,
) -> PreparedSettingsMerge {
    let mut base = base.clone();
    let mut ours = ours.clone();
    let mut theirs = theirs.clone();
    for document in [&mut base, &mut ours, &mut theirs] {
        for instance in &mut document.instances {
            if is_lua_source_class(&instance.class_name) {
                instance
                    .properties
                    .entry("Source".to_string())
                    .or_insert_with(|| Value::String(EXTERNAL_SOURCE_MARKER.to_string()));
            }
        }
    }
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
            && !aligned_additions.contains(id)
            && !base_ids.contains_key(id)
            && !vc_instance_same_identity(&ours, ours_index, &theirs, theirs_index)
        {
            let fresh = next_editor_settings_id_fast(&mut all_ids, &mut id_seed);
            theirs_id_remap.insert(id.clone(), fresh);
        }
    }
    // The remap renames instances of this store. A reference into another
    // service can carry one of the renamed ids, as the id its target has in
    // its own store, and keeps it.
    let service = merged_store_service([&ours, &theirs, &base]);
    for instance in &mut theirs.instances {
        remap_store_reference_ids(&mut instance.properties, &theirs_id_remap, &service);
        remap_store_reference_ids(&mut instance.attributes, &theirs_id_remap, &service);
    }
    PreparedSettingsMerge {
        base,
        ours,
        theirs,
        theirs_id_remap,
        service,
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

/// Where decisions placed the merged records.
#[derive(Default)]
struct Placement {
    /// The side (`true` ours) each record follows once a decision placed
    /// it: an orphan's own, a restored instance's restorer's.
    follows: HashMap<String, bool>,
    /// Instances their decision left out with their removed parent; they
    /// stay out when another decision brings that parent back.
    left_out: HashSet<String>,
    /// Orphans no decision placed; each stays a conflict when another
    /// decision brings its parent back.
    undecided: BTreeSet<String>,
    /// Instances present only by a decision: a contested removal it kept,
    /// or one it brought back. What hangs under them or refers to them is
    /// still decided as if they were removed.
    kept: HashSet<String>,
    /// The conflict whose decision placed each record, where one did.
    origins: HashMap<String, VcMergeConflict>,
    /// Instances brought back since the resolver last heard of them.
    restored: Vec<String>,
}

/// The side an orphan takes (`true` ours), with the conflict whose
/// decision chose it.
type Placed = (bool, Option<VcMergeConflict>);

/// An instance whose parent the merge left out follows the side its
/// conflict takes instead of vanishing with that parent. Where that side
/// still has the parent, the parent comes back: the other side's removal
/// loses to this side's use of it. Where that side removed the parent, the
/// instance stays where that side has it. Only an instance that side does
/// not have goes with its parent. An instance under a parent that is
/// present only by a decision is decided the same way, so its decision
/// holds whatever the parent's. `side_of` answers the side for an orphan,
/// or `None` to leave it; a restored parent follows the side that restored
/// it.
fn reattach_orphans(
    merged: &mut Vec<VcMergedInstance>,
    sides: &MergeSides<'_>,
    placement: &mut Placement,
    mut side_of: impl FnMut(&VcMergedInstance) -> Option<Placed>,
) {
    let Placement {
        follows,
        left_out,
        undecided,
        kept,
        origins,
        restored,
    } = placement;
    let mut present = merged
        .iter()
        .map(|record| record.settings_id.clone())
        .collect::<HashSet<_>>();
    let mut decide = |record: &VcMergedInstance,
                      follows: &mut HashMap<String, bool>,
                      origins: &mut HashMap<String, VcMergeConflict>| {
        if let Some(side) = follows.get(&record.settings_id) {
            return Some(*side);
        }
        let (side, origin) = side_of(record)?;
        follows.insert(record.settings_id.clone(), side);
        if let Some(origin) = origin {
            origins.insert(record.settings_id.clone(), origin);
        }
        Some(side)
    };
    // Every orphan takes its side before any parent comes back, so a parent
    // one decision brings back does not overrule another that put its
    // instance where its own side holds it, or left it out.
    let mut moves = Vec::new();
    for (index, record) in merged.iter().enumerate() {
        let Some(parent) = record
            .parent_id
            .as_ref()
            .filter(|parent| !present.contains(*parent) || kept.contains(*parent))
        else {
            continue;
        };
        let Some(side) = decide(record, follows, origins) else {
            undecided.insert(record.settings_id.clone());
            continue;
        };
        if sides.index(side, parent).is_some() {
            continue;
        }
        match sides.index(side, &record.settings_id) {
            Some(own_index) => {
                moves.push((index, settings_parent_id(sides.document(side), own_index)));
            }
            None => {
                left_out.insert(record.settings_id.clone());
            }
        }
    }
    for (index, parent) in moves {
        merged[index].parent_id = parent;
    }
    let mut index = 0;
    // A restored parent is appended and visited in turn, so a chain of
    // removed ancestors comes back link by link.
    while index < merged.len() {
        let Some(parent) = merged[index]
            .parent_id
            .clone()
            .filter(|parent| !present.contains(parent))
        else {
            index += 1;
            continue;
        };
        let id = merged[index].settings_id.clone();
        let Some(side) = decide(&merged[index], follows, origins) else {
            index += 1;
            continue;
        };
        if let Some(parent_index) = sides.index(side, &parent) {
            present.insert(parent.clone());
            kept.insert(parent.clone());
            if let Some(origin) = origins.get(&id).cloned() {
                origins.insert(parent.clone(), origin);
            }
            restored.push(parent.clone());
            follows.insert(parent, side);
            merged.push(sides.record(side, parent_index));
            index += 1;
        } else if let Some(own_index) = sides.index(side, &id) {
            // That side's parent of its own instance exists on that side, so
            // the next pass finds it present or restores it.
            merged[index].parent_id = settings_parent_id(sides.document(side), own_index);
        } else {
            left_out.insert(id);
            index += 1;
        }
    }
}

/// Tells the resolver which instances decisions brought back, and by
/// which conflict, since it last heard.
fn report_restores(placement: &mut Placement, resolver: &mut dyn VcResolve) {
    for id in std::mem::take(&mut placement.restored) {
        if let Some(origin) = placement.origins.get(&id) {
            resolver.restored(&id, origin);
        }
    }
}

/// The conflict of an instance whose parent the merge left out, with what
/// each side's decision does to it and to that parent.
fn orphan_conflict(
    record: &VcMergedInstance,
    sides: &MergeSides<'_>,
    removals: &Removals,
) -> VcMergeConflict {
    let parent = record.parent_id.as_deref().unwrap_or_default();
    let effect = |ours: bool| match (
        sides.path_on(ours, parent),
        sides.index(ours, &record.settings_id),
    ) {
        (Some(parent), _) => format!("keeps it, bringing {parent} back if removed"),
        (None, Some(own)) => match parent_path(sides.document(ours), own) {
            Some(Value::String(path)) => format!("puts it under {path}"),
            _ => "keeps it where that side holds it".to_string(),
        },
        (None, None) => "removes it".to_string(),
    };
    VcMergeConflict {
        path: record.name.clone(),
        detail: format!("added under a parent (id={parent}) deleted on the other side"),
        kind: VcConflictKind::Orphan,
        settings_id: record.settings_id.clone(),
        instance_path: sides.child_path(parent, &record.name),
        field: None,
        values: Some(sides.paths(parent)),
        follows: removals.get(parent).and_then(|removal| removal.side),
        decidable: true,
        part_of_removal: removals.contains_key(&record.settings_id),
        effects: Some([effect(true), effect(false)]),
    }
}

/// A removal both sides contested: the side its decision took (`true`
/// ours), or `None` while open, and its conflict.
struct Removal {
    side: Option<bool>,
    conflict: VcMergeConflict,
}

/// The removals both sides contested, by the removed instance's id.
type Removals = HashMap<String, Removal>;

/// The side an orphan takes: the decision on its own removal when it is
/// one side's removal (that conflict places it), else its conflict's.
fn orphan_side(
    resolver: &mut dyn VcResolve,
    sides: &MergeSides<'_>,
    removals: &Removals,
    record: &VcMergedInstance,
) -> Option<Placed> {
    if let Some(Removal {
        side: Some(side),
        conflict,
    }) = removals.get(&record.settings_id)
    {
        return Some((*side, Some(conflict.clone())));
    }
    let conflict = orphan_conflict(record, sides, removals);
    let side = resolver.resolve(&conflict)?;
    Some((side, Some(conflict)))
}

/// The conflict of an instance one side changed and the other removed.
/// Its values are the fields the changing side changed since the base.
fn removal_conflict(
    base: (&SettingsBytecode, usize),
    edited: (&SettingsBytecode, usize),
    sides: &MergeSides<'_>,
    ours_edited: bool,
    source_changed: bool,
) -> VcMergeConflict {
    let (base_document, base_index) = base;
    let (document, index) = edited;
    let (before, after) = changed_fields(base, edited, source_changed);
    let path = settings_instance_path(document, index);
    let id = &base_document.instances[base_index].settings_id;
    let keep = match sides
        .parent_of(ours_edited, id)
        .filter(|parent| sides.index(!ours_edited, parent).is_none())
        .and_then(|parent| sides.path_on(ours_edited, &parent))
    {
        Some(parent) => format!("keeps it, bringing {parent} back if removed"),
        None => "keeps it".to_string(),
    };
    let remove = "removes it unless another decision brings it back".to_string();
    VcMergeConflict {
        instance_path: path.clone(),
        path,
        detail: if ours_edited {
            "modified here but deleted on the other side"
        } else {
            "deleted here but modified on the other side"
        }
        .to_string(),
        kind: VcConflictKind::Removal,
        settings_id: id.clone(),
        field: None,
        values: Some(VcConflictValues {
            base: Some(before),
            ours: ours_edited.then(|| after.clone()),
            theirs: (!ours_edited).then_some(after),
        }),
        follows: None,
        decidable: true,
        part_of_removal: false,
        effects: Some(if ours_edited {
            [keep, remove]
        } else {
            [remove, keep]
        }),
    }
}

/// The fields of an instance one side changed since the base, as the base
/// and that side hold them: `Name`, `ClassName`, `Parent` (a path), each
/// property, `@` and each attribute, and `Source` when only its script
/// text changed.
fn changed_fields(
    base: (&SettingsBytecode, usize),
    edited: (&SettingsBytecode, usize),
    source_changed: bool,
) -> (Value, Value) {
    let (base_document, base_index) = base;
    let (document, index) = edited;
    let (old, new) = (
        &base_document.instances[base_index],
        &document.instances[index],
    );
    let mut before = Map::new();
    let mut after = Map::new();
    let mut compare = |name: String, left: Option<&Value>, right: Option<&Value>| {
        if left != right {
            before.insert(name.clone(), left.cloned().unwrap_or(Value::Null));
            after.insert(name, right.cloned().unwrap_or(Value::Null));
        }
    };
    compare(
        "Name".into(),
        Some(&json!(old.name)),
        Some(&json!(new.name)),
    );
    compare(
        "ClassName".into(),
        Some(&json!(old.class_name)),
        Some(&json!(new.class_name)),
    );
    compare(
        "Parent".into(),
        parent_path(base_document, base_index).as_ref(),
        parent_path(document, index).as_ref(),
    );
    for (prefix, left, right) in [
        ("", &old.properties, &new.properties),
        ("@", &old.attributes, &new.attributes),
    ] {
        let mut keys = left.keys().chain(right.keys()).collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        for key in keys {
            compare(format!("{prefix}{key}"), left.get(key), right.get(key));
        }
    }
    if source_changed && !after.contains_key("Source") {
        after.insert("Source".into(), json!("script text edited"));
    }
    (Value::Object(before), Value::Object(after))
}

/// The conflict of a cycle of parents; `decidable` is false for a cycle a
/// decision left behind.
fn cycle_conflict(
    merged: &[VcMergedInstance],
    cycle: &[usize],
    sides: &MergeSides<'_>,
    decidable: bool,
) -> VcMergeConflict {
    let names = cycle
        .iter()
        .map(|index| merged[*index].name.as_str())
        .collect::<Vec<_>>();
    // The id with the least value names the cycle whichever member the
    // search met first.
    let first = cycle
        .iter()
        .copied()
        .min_by(|left, right| merged[*left].settings_id.cmp(&merged[*right].settings_id))
        .unwrap_or_default();
    let record = &merged[first];
    let parent_on = |ours: bool| {
        sides
            .index(ours, &record.settings_id)
            .and_then(|index| sides.document(ours).instances[index].parent_index)
            .map(|parent| json!(settings_instance_path(sides.document(ours), parent)))
    };
    VcMergeConflict {
        path: names.first().copied().unwrap_or_default().to_string(),
        detail: format!(
            "Parent: combined moves create a cycle among {}",
            names.join(", ")
        ),
        kind: VcConflictKind::Cycle,
        settings_id: record.settings_id.clone(),
        instance_path: sides.instance_path(&record.settings_id, &record.name),
        field: None,
        values: Some(VcConflictValues {
            base: None,
            ours: parent_on(true),
            theirs: parent_on(false),
        }),
        follows: None,
        decidable,
        part_of_removal: false,
        effects: None,
    }
}

/// The preferred side's record of an instance, with that side's ids as the
/// merge uses them.
fn preferred_instance(
    preferred: &SettingsBytecode,
    index: usize,
    id_remap: &HashMap<String, String>,
    service: &str,
) -> VcMergedInstance {
    let mut record = merged_instance_from(preferred, index);
    record.parent_id = record
        .parent_id
        .map(|id| id_remap.get(&id).cloned().unwrap_or(id));
    remap_store_reference_ids(&mut record.properties, id_remap, service);
    remap_store_reference_ids(&mut record.attributes, id_remap, service);
    record
}

/// The id a reference object names when it points into the store being
/// merged. A reference with a path into another service is that store's.
fn same_store_reference_id<'a>(object: &'a Map<String, Value>, service: &str) -> Option<&'a str> {
    if reference_leads_out_of(object, service) {
        return None;
    }
    object
        .get("settingsId")
        .or_else(|| object.get("instanceId"))
        .and_then(Value::as_str)
}

/// The references of the merged store that name an instance it does not
/// hold, as (referring id, target id).
fn dangling_references(merged: &[VcMergedInstance], service: &str) -> HashSet<(String, String)> {
    let present = merged
        .iter()
        .map(|record| record.settings_id.as_str())
        .collect::<HashSet<_>>();
    let mut dangling = HashSet::new();
    for record in merged {
        for value in record.properties.values().chain(record.attributes.values()) {
            for_each_reference(value, &mut |object| {
                if let Some(target) = same_store_reference_id(object, service)
                    && !present.contains(target)
                {
                    dangling.insert((record.settings_id.clone(), target.to_string()));
                }
            });
        }
    }
    dangling
}

/// The references one side holds to instances it has, as (referring id,
/// target id) under the ids the merge uses.
fn valid_references(
    side: &SettingsBytecode,
    id_remap: &HashMap<String, String>,
    service: &str,
    valid: &mut HashSet<(String, String)>,
) {
    let ids = settings_ids_by_index(side);
    let merged_id = |id: &str| id_remap.get(id).cloned().unwrap_or_else(|| id.to_string());
    for instance in &side.instances {
        for value in instance
            .properties
            .values()
            .chain(instance.attributes.values())
        {
            for_each_reference(value, &mut |object| {
                if let Some(target) = same_store_reference_id(object, service)
                    && ids.contains_key(target)
                {
                    valid.insert((merged_id(&instance.settings_id), merged_id(target)));
                }
            });
        }
    }
}

/// The conflict of a reference to an instance the other side removed.
fn reference_conflict(
    referring: (&str, &str),
    field: &str,
    target: &str,
    sides: &MergeSides<'_>,
    follows: Option<bool>,
) -> VcMergeConflict {
    let (referring, name) = referring;
    VcMergeConflict {
        path: name.to_string(),
        detail: format!("{field}: refers to an instance (id={target}) removed on the other side"),
        kind: VcConflictKind::Reference,
        settings_id: referring.to_string(),
        instance_path: sides.instance_path(referring, name),
        field: Some(field.to_string()),
        values: Some(sides.paths(target)),
        follows,
        decidable: true,
        part_of_removal: false,
        effects: Some([true, false].map(|ours| match sides.path_on(ours, target) {
            Some(path) => format!("keeps {field}, bringing {path} back if removed"),
            None => format!("clears {field}"),
        })),
    }
}

/// What the settlement of references decided, by (referring id, field,
/// target id): the side each one's conflict took, or `None` when it is
/// open.
type ReferenceDecisions = HashMap<(String, String, String), Option<bool>>;

/// What settling references needs beyond the merge's sides.
struct ReferenceSettlement<'a> {
    sides: &'a MergeSides<'a>,
    removals: &'a Removals,
    placement: &'a mut Placement,
    resolver: &'a mut dyn VcResolve,
    decided: ReferenceDecisions,
}

impl ReferenceSettlement<'_> {
    /// The side a reference's conflict takes, asked once per reference.
    fn side(&mut self, referring: (&str, &str), field: &str, target: &str) -> Option<bool> {
        let key = (
            referring.0.to_string(),
            field.to_string(),
            target.to_string(),
        );
        if let Some(side) = self.decided.get(&key) {
            return *side;
        }
        let conflict = reference_conflict(
            referring,
            field,
            target,
            self.sides,
            self.placement.follows.get(referring.0).copied(),
        );
        let side = self.resolver.resolve(&conflict);
        self.decided.insert(key, side);
        side
    }
}

/// A merge can remove an instance one side still refers to: one side deletes
/// it, the other adds a reference to it, and no field was changed on both
/// sides. The reference's conflict decides: the instance comes back when the
/// side it takes still has it, and the reference is cleared when that side
/// removed it; an open one stays a conflict. A reference to an instance
/// present only by a decision is decided the same way, so its decision
/// holds whatever the instance's. A restored instance's own references
/// follow the side that restored it. A reference that was already broken
/// on the side it came from is not the merge's doing and stays as it is.
fn settle_references_to_removed_instances(
    merged: &mut Vec<VcMergedInstance>,
    settlement: &mut ReferenceSettlement<'_>,
    conflicts: &mut Vec<VcMergeConflict>,
) {
    let Some(service) = merged
        .iter()
        .find(|record| record.parent_id.is_none())
        .map(|record| record.name.clone())
    else {
        return;
    };
    if settlement.placement.kept.is_empty() && dangling_references(merged, &service).is_empty() {
        return;
    }
    let sides = settlement.sides;
    let mut broken_by_merge = HashSet::new();
    valid_references(
        sides.document(true),
        &HashMap::new(),
        &service,
        &mut broken_by_merge,
    );
    valid_references(
        sides.document(false),
        sides.remaps[1],
        &service,
        &mut broken_by_merge,
    );
    // A restored instance can itself refer to a removed one.
    let dangling = loop {
        let dangling = dangling_references(merged, &service);
        let mut present = merged
            .iter()
            .map(|record| record.settings_id.clone())
            .collect::<HashSet<_>>();
        let mut restored = Vec::new();
        for record in merged.iter() {
            for (field, value) in record.properties.iter().chain(&record.attributes) {
                for_each_reference(value, &mut |object| {
                    let Some(target) = same_store_reference_id(object, &service) else {
                        return;
                    };
                    let pair = (record.settings_id.clone(), target.to_string());
                    if !broken_by_merge.contains(&pair)
                        || !dangling.contains(&pair) && !settlement.placement.kept.contains(target)
                    {
                        return;
                    }
                    let referring = (record.settings_id.as_str(), record.name.as_str());
                    if let Some(side) = settlement.side(referring, field, target)
                        && let Some(index) = sides.index(side, target)
                        && present.insert(target.to_string())
                    {
                        restored.push((
                            side,
                            index,
                            reference_conflict(referring, field, target, sides, Some(side)),
                        ));
                    }
                });
            }
        }
        if restored.is_empty() {
            break dangling;
        }
        for (side, index, origin) in restored {
            let record = sides.record(side, index);
            let placement = &mut *settlement.placement;
            placement.follows.insert(record.settings_id.clone(), side);
            placement.kept.insert(record.settings_id.clone());
            placement.restored.push(record.settings_id.clone());
            placement.origins.insert(record.settings_id.clone(), origin);
            merged.push(record);
        }
        let removals = settlement.removals;
        let resolver = &mut *settlement.resolver;
        reattach_orphans(merged, sides, settlement.placement, |record| {
            orphan_side(resolver, sides, removals, record)
        });
        report_restores(settlement.placement, &mut *settlement.resolver);
    };
    for record in merged.iter_mut() {
        let referring = record.settings_id.clone();
        let name = record.name.clone();
        for (field, value) in record
            .properties
            .iter_mut()
            .chain(record.attributes.iter_mut())
        {
            for_each_reference_mut(value, &mut |object| {
                let Some(target) = same_store_reference_id(object, &service).map(str::to_string)
                else {
                    return;
                };
                let pair = (referring.clone(), target.clone());
                if !broken_by_merge.contains(&pair)
                    || !dangling.contains(&pair) && !settlement.placement.kept.contains(&target)
                {
                    return;
                }
                match settlement.side((&referring, &name), field, &target) {
                    Some(side) if sides.index(side, &target).is_none() => {
                        for selector in SETTINGS_REFERENCE_SELECTOR_KEYS {
                            object.remove(selector);
                        }
                    }
                    Some(_) => {}
                    None => conflicts.push(reference_conflict(
                        (&referring, &name),
                        field,
                        &target,
                        sides,
                        settlement.placement.follows.get(&referring).copied(),
                    )),
                }
            });
        }
    }
}

/// Puts instances a merge removed from a store back into it, with the
/// ancestors the merged store lacks. `source` is the store as the merge's
/// base had it, so the instances return under the ids the merged stores use
/// for them.
pub(crate) fn restore_removed_instances(
    merged: &SettingsBytecode,
    source: &SettingsBytecode,
    settings_ids: &HashSet<String>,
) -> SettingsBytecode {
    let service = merged_store_service([merged, source, source]);
    let no_remap = HashMap::new();
    let mut records = (0..merged.instances.len())
        .map(|index| merged_instance_from(merged, index))
        .collect::<Vec<_>>();
    let mut present = merged
        .instances
        .iter()
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    for (index, instance) in source.instances.iter().enumerate() {
        if settings_ids.contains(&instance.settings_id)
            && present.insert(instance.settings_id.as_str())
        {
            records.push(preferred_instance(source, index, &no_remap, &service));
        }
    }
    let sides = MergeSides::new([source, source], [&no_remap, &no_remap], &service);
    reattach_orphans(&mut records, &sides, &mut Placement::default(), |_| {
        Some((true, None))
    });
    for record in &mut records {
        if is_lua_source_class(&record.class_name) {
            record
                .properties
                .entry("Source".to_string())
                .or_insert_with(|| Value::String(EXTERNAL_SOURCE_MARKER.to_string()));
        }
    }
    finish_settings_merge(
        records,
        merged.version,
        Vec::new(),
        (&HashSet::new(), |_| None),
    )
    .0
}

fn parent_cycles(instances: &[VcMergedInstance]) -> Vec<Vec<usize>> {
    let index_by_id = instances
        .iter()
        .enumerate()
        .map(|(index, instance)| (instance.settings_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut state = vec![0u8; instances.len()];
    let mut path_positions = vec![usize::MAX; instances.len()];
    let mut path = Vec::new();
    let mut cycles = Vec::new();
    for start in 0..instances.len() {
        if state[start] != 0 {
            continue;
        }
        let mut current = Some(start);
        while let Some(index) = current {
            match state[index] {
                0 => {
                    state[index] = 1;
                    path_positions[index] = path.len();
                    path.push(index);
                    current = instances[index]
                        .parent_id
                        .as_deref()
                        .and_then(|parent| index_by_id.get(parent))
                        .copied();
                }
                1 => {
                    cycles.push(path[path_positions[index]..].to_vec());
                    break;
                }
                _ => break,
            }
        }
        for index in path.drain(..) {
            state[index] = 2;
            path_positions[index] = usize::MAX;
        }
    }
    cycles
}

/// Breaks the cycles combined moves made. A cycle whose conflict takes a
/// side gets that side's parents; a cycle still standing after that, or one
/// left open, is a conflict and its first member moves to the root.
fn resolve_parent_cycles(
    merged: &mut [VcMergedInstance],
    sides: &MergeSides<'_>,
    placement: &mut Placement,
    resolver: &mut dyn VcResolve,
    conflicts: &mut Vec<VcMergeConflict>,
) {
    let cycles = parent_cycles(merged);
    if cycles.is_empty() {
        return;
    }
    let members = |merged: &[VcMergedInstance], cycle: &[usize]| {
        cycle
            .iter()
            .map(|index| merged[*index].settings_id.clone())
            .collect::<BTreeSet<_>>()
    };
    let mut open = Vec::new();
    let mut decided = false;
    for cycle in &cycles {
        let conflict = cycle_conflict(merged, cycle, sides, true);
        let Some(side) = resolver.resolve(&conflict) else {
            open.push((members(merged, cycle), conflict));
            continue;
        };
        decided = true;
        for index in cycle {
            let id = merged[*index].settings_id.clone();
            if let Some(own_index) = sides.index(side, &id) {
                merged[*index].parent_id = settings_parent_id(sides.document(side), own_index);
            }
            placement.follows.insert(id.clone(), side);
            placement.origins.insert(id, conflict.clone());
        }
    }
    let remaining = if decided {
        parent_cycles(merged)
    } else {
        cycles
    };
    let root_id = merged
        .iter()
        .find(|instance| instance.parent_id.is_none())
        .map(|instance| instance.settings_id.clone());
    for cycle in remaining {
        let found = members(merged, &cycle);
        let conflict = match open.iter().position(|(ids, _)| *ids == found) {
            Some(position) => open.swap_remove(position).1,
            None => cycle_conflict(merged, &cycle, sides, false),
        };
        conflicts.push(conflict);
        merged[cycle[0]].parent_id.clone_from(&root_id);
    }
}

/// Orders the merged records into a store. The instances in `orphans.0`
/// stay out with what hangs under them; an instance whose parent is missing
/// is left out, and `orphans.1` gives its conflict, or `None` when a
/// decision left it out on purpose.
fn finish_settings_merge(
    mut merged: Vec<VcMergedInstance>,
    version: u8,
    mut conflicts: Vec<VcMergeConflict>,
    orphans: (
        &HashSet<String>,
        impl FnMut(&VcMergedInstance) -> Option<VcMergeConflict>,
    ),
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    let (left_out, mut orphan) = orphans;
    let index_by_id = merged
        .iter()
        .enumerate()
        .map(|(index, record)| (record.settings_id.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (index, record) in merged.iter().enumerate() {
        if left_out.contains(&record.settings_id) {
            continue;
        }
        match record
            .parent_id
            .as_deref()
            .and_then(|parent| index_by_id.get(parent))
        {
            Some(parent_index) => children.entry(Some(*parent_index)).or_default().push(index),
            None if record.parent_id.is_none() => children.entry(None).or_default().push(index),
            None => conflicts.extend(orphan(record)),
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
    let service = instances
        .iter()
        .find(|instance| instance.parent_index.is_none())
        .map(|instance| instance.name.clone())
        .unwrap_or_default();
    for instance in &mut instances {
        reindex_store_reference_indices(&mut instance.properties, &indices_by_id, &service);
        reindex_store_reference_indices(&mut instance.attributes, &indices_by_id, &service);
    }
    (SettingsBytecode { version, instances }, conflicts)
}

pub(crate) fn merge_settings_documents(
    base: &SettingsBytecode,
    ours: &SettingsBytecode,
    theirs: &SettingsBytecode,
    prefer: Option<bool>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    merge_settings_documents_with_policy(base, ours, theirs, prefer, prefer)
}

pub(crate) fn merge_settings_documents_with_policy(
    base: &SettingsBytecode,
    ours: &SettingsBytecode,
    theirs: &SettingsBytecode,
    value_prefer: Option<bool>,
    structural_prefer: Option<bool>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    merge_settings_documents_with_policy_and_source_changes(
        base,
        ours,
        theirs,
        value_prefer,
        structural_prefer,
        &HashSet::new(),
        &HashSet::new(),
    )
}

pub(crate) fn merge_settings_documents_with_policy_and_source_changes(
    base: &SettingsBytecode,
    ours: &SettingsBytecode,
    theirs: &SettingsBytecode,
    value_prefer: Option<bool>,
    structural_prefer: Option<bool>,
    ours_source_changes: &HashSet<String>,
    theirs_source_changes: &HashSet<String>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    merge_prepared_settings_documents(
        prepare_settings_merge(base, ours, theirs, &HashSet::new()),
        &mut VcPreference {
            value: value_prefer,
            structural: structural_prefer,
        },
        ours_source_changes,
        theirs_source_changes,
    )
}

// Reconciliation has already matched live/file identities. Unlike independently
// created Git IDs, different values on a matched addition must not create a copy.
pub(crate) fn merge_aligned_settings_documents(
    base: &SettingsBytecode,
    ours: &SettingsBytecode,
    theirs: &SettingsBytecode,
    resolver: &mut dyn VcResolve,
    source_changes: (&HashSet<String>, &HashSet<String>),
    aligned_additions: &HashSet<String>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    merge_prepared_settings_documents(
        prepare_settings_merge(base, ours, theirs, aligned_additions),
        resolver,
        source_changes.0,
        source_changes.1,
    )
}

fn merge_prepared_settings_documents(
    prepared: PreparedSettingsMerge,
    resolver: &mut dyn VcResolve,
    ours_source_changes: &HashSet<String>,
    theirs_source_changes: &HashSet<String>,
) -> (SettingsBytecode, Vec<VcMergeConflict>) {
    let PreparedSettingsMerge {
        base,
        ours,
        theirs,
        theirs_id_remap,
        service,
    } = prepared;
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
    let empty_properties = Map::new();
    let mut removals = Removals::new();
    let no_remap = HashMap::new();
    let sides = MergeSides::new([ours, theirs], [&no_remap, &theirs_id_remap], &service);

    for (ours_index, instance) in ours.instances.iter().enumerate() {
        let id = &instance.settings_id;
        if merged_ids.contains(id) {
            continue;
        }
        let theirs_index = theirs_ids
            .get(id)
            .copied()
            .filter(|_| !theirs_id_remap.contains_key(id));
        match (base_ids.get(id).copied(), theirs_index) {
            (base_index, Some(theirs_index)) => {
                let base_inst = base_index.map(|index| &base.instances[index]);
                let theirs_inst = &theirs.instances[theirs_index];
                let mut merge_context = VcMergeContext {
                    resolver: &mut *resolver,
                    document: ours,
                    index: ours_index,
                    conflicts: &mut conflicts,
                };
                let texts = |pick: fn(&SettingsBytecodeInstance) -> &String| VcConflictValues {
                    base: base_inst.map(|base| json!(pick(base))),
                    ours: Some(json!(pick(instance))),
                    theirs: Some(json!(pick(theirs_inst))),
                };
                let name = merge_context.merge_scalar(
                    base_inst.map(|instance| &instance.name),
                    &instance.name,
                    &theirs_inst.name,
                    |context| {
                        context.conflict(
                            VcConflictKind::Name,
                            None,
                            texts(|instance| &instance.name),
                            format!("Name: ours={} theirs={}", instance.name, theirs_inst.name),
                        )
                    },
                );
                let class_name = merge_context.merge_scalar(
                    base_inst.map(|instance| &instance.class_name),
                    &instance.class_name,
                    &theirs_inst.class_name,
                    |context| {
                        context.conflict(
                            VcConflictKind::ClassName,
                            None,
                            texts(|instance| &instance.class_name),
                            format!(
                                "ClassName: ours={} theirs={}",
                                instance.class_name, theirs_inst.class_name
                            ),
                        )
                    },
                );
                let ours_parent = settings_parent_id(ours, ours_index);
                let base_parent = base_index.map(|index| settings_parent_id(base, index));
                let theirs_parent = settings_parent_id(theirs, theirs_index);
                let parent_id = merge_context.merge_scalar(
                    base_parent.as_ref(),
                    &ours_parent,
                    &theirs_parent,
                    |context| {
                        context.conflict(
                            VcConflictKind::Parent,
                            None,
                            VcConflictValues {
                                base: base_index.and_then(|index| parent_path(base, index)),
                                ours: parent_path(ours, ours_index),
                                theirs: parent_path(theirs, theirs_index),
                            },
                            "Parent: moved to different parents on both sides".to_string(),
                        )
                    },
                );
                let properties = merge_context.merge_maps(
                    base_inst.map_or(&empty_properties, |instance| &instance.properties),
                    &instance.properties,
                    &theirs_inst.properties,
                    "property",
                );
                let attributes = merge_context.merge_maps(
                    base_inst.map_or(&empty_properties, |instance| &instance.attributes),
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
                if vc_instance_equal(base, base_index, ours, ours_index)
                    && !ours_source_changes.contains(id)
                {
                    continue;
                }
                let conflict = removal_conflict(
                    (base, base_index),
                    (ours, ours_index),
                    &sides,
                    true,
                    ours_source_changes.contains(id),
                );
                let side = resolver.resolve(&conflict);
                removals.insert(
                    id.clone(),
                    Removal {
                        side,
                        conflict: conflict.clone(),
                    },
                );
                if side.is_none() {
                    conflicts.push(conflict);
                }
                if side != Some(false) {
                    merged.push(merged_instance_from(ours, ours_index));
                    merged_ids.insert(id.clone());
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
        let remapped_parent = settings_parent_id(theirs, theirs_index)
            .map(|pid| theirs_id_remap.get(&pid).cloned().unwrap_or(pid));
        if ours_ids.contains_key(id) {
            if let Some(fresh) = theirs_id_remap.get(id).cloned() {
                let mut properties = instance.properties.clone();
                let mut attributes = instance.attributes.clone();
                remap_store_reference_ids(&mut properties, &theirs_id_remap, &service);
                remap_store_reference_ids(&mut attributes, &theirs_id_remap, &service);
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
            if vc_instance_equal(base, base_index, theirs, theirs_index)
                && !theirs_source_changes.contains(id)
            {
                continue;
            }
            let conflict = removal_conflict(
                (base, base_index),
                (theirs, theirs_index),
                &sides,
                false,
                theirs_source_changes.contains(id),
            );
            let side = resolver.resolve(&conflict);
            removals.insert(
                id.clone(),
                Removal {
                    side,
                    conflict: conflict.clone(),
                },
            );
            match side {
                Some(true) => {}
                Some(false) => {
                    let mut properties = instance.properties.clone();
                    let mut attributes = instance.attributes.clone();
                    remap_store_reference_ids(&mut properties, &theirs_id_remap, &service);
                    remap_store_reference_ids(&mut attributes, &theirs_id_remap, &service);
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
                None => conflicts.push(conflict),
            }
            continue;
        }
        let mut properties = instance.properties.clone();
        let mut attributes = instance.attributes.clone();
        remap_store_reference_ids(&mut properties, &theirs_id_remap, &service);
        remap_store_reference_ids(&mut attributes, &theirs_id_remap, &service);
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

    let mut placement = Placement {
        kept: removals
            .iter()
            .filter(|(id, removal)| removal.side.is_some() && merged_ids.contains(*id))
            .map(|(id, _)| id.clone())
            .collect(),
        ..Placement::default()
    };
    reattach_orphans(&mut merged, &sides, &mut placement, |record| {
        orphan_side(&mut *resolver, &sides, &removals, record)
    });
    report_restores(&mut placement, &mut *resolver);
    // Breaking a cycle moves an instance to the decided side's parent, which
    // the merge may have removed, and restoring a removed parent can close a
    // cycle; the two steps repeat until neither applies. Every round but the
    // last restores an instance of a side, which bounds the rounds.
    for _ in 0..=ours.instances.len() + theirs.instances.len() {
        if parent_cycles(&merged).is_empty() {
            break;
        }
        resolve_parent_cycles(
            &mut merged,
            &sides,
            &mut placement,
            &mut *resolver,
            &mut conflicts,
        );
        reattach_orphans(&mut merged, &sides, &mut placement, |record| {
            orphan_side(&mut *resolver, &sides, &removals, record)
        });
        report_restores(&mut placement, &mut *resolver);
    }
    let mut settlement = ReferenceSettlement {
        sides: &sides,
        removals: &removals,
        placement: &mut placement,
        resolver,
        decided: HashMap::new(),
    };
    settle_references_to_removed_instances(&mut merged, &mut settlement, &mut conflicts);
    let Placement {
        follows,
        left_out,
        undecided,
        ..
    } = &placement;
    let present = merged
        .iter()
        .map(|record| record.settings_id.as_str())
        .collect::<HashSet<_>>();
    // An orphan whose parent another decision brought back is still its
    // own conflict; one whose parent stays out is reported as it is left.
    conflicts.extend(
        merged
            .iter()
            .filter(|record| {
                undecided.contains(&record.settings_id)
                    && !follows.contains_key(&record.settings_id)
                    && record
                        .parent_id
                        .as_deref()
                        .is_some_and(|parent| present.contains(parent))
            })
            .map(|record| orphan_conflict(record, &sides, &removals)),
    );
    finish_settings_merge(
        merged,
        version,
        conflicts,
        (left_out, |record: &VcMergedInstance| {
            (!follows.contains_key(&record.settings_id))
                .then(|| orphan_conflict(record, &sides, &removals))
        }),
    )
}

/// Where a document holds an instance's parent.
fn parent_path(document: &SettingsBytecode, index: usize) -> Option<Value> {
    document.instances[index]
        .parent_index
        .map(|parent| json!(settings_instance_path(document, parent)))
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

pub(crate) fn vc_merge(args: VcMergeArgs) -> Result<()> {
    let prefer = match args.prefer.as_deref() {
        None => None,
        Some("ours") => Some(true),
        Some("theirs") => Some(false),
        Some(other) => bail!("--prefer must be `ours` or `theirs`, got `{other}`"),
    };
    let label = args
        .path
        .clone()
        .unwrap_or_else(|| args.ours.display().to_string());
    // Whatever keeps the stores from being merged by their rows is said by
    // the merge of the whole documents. The merge by rows writes over ours,
    // in its blocks.
    let cut_whole = |file: &[u8]| {
        crate::settings::bytecode::blocks::BlockStore::open(file)
            .is_ok_and(|store| store.cut_whole())
    };
    let by_rows = match &[&args.base, &args.ours, &args.theirs].map(fs::read) {
        [Ok(base), Ok(ours), Ok(theirs)] if cut_whole(ours) => {
            merge_stores_by_rows(base, ours, theirs, prefer)
        }
        _ => Ok(None),
    };
    let (merged, conflicts) = match by_rows.ok().flatten() {
        Some((file, instances, conflicts)) => (MergedStore::File(file, instances), conflicts),
        None => {
            let base = read_settings_doc_or_empty(&args.base, "base")?;
            let ours = read_settings_doc_or_empty(&args.ours, "ours")?;
            let theirs = read_settings_doc_or_empty(&args.theirs, "theirs")?;
            let (merged, conflicts) = merge_settings_documents(&base, &ours, &theirs, prefer);
            (MergedStore::Document(merged), conflicts)
        }
    };

    if !conflicts.is_empty() {
        for conflict in &conflicts {
            eprintln!(
                "[renium vc-merge] conflict in {label}: {} -- {}",
                conflict.path, conflict.detail
            );
        }
        bail!(
            "{} merge conflict(s) in {label}. Re-run with --prefer ours|theirs, or resolve the listed fields manually and retry.",
            conflicts.len()
        );
    }

    let out_path = args.output.clone().unwrap_or_else(|| args.ours.clone());
    let instances = match merged {
        MergedStore::Document(merged) => {
            merged.write_file(&out_path)?;
            merged.instances.len()
        }
        MergedStore::File(file, instances) => {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create {}", parent.display()))?;
            }
            write_bytes_if_changed(&out_path, &file)?;
            instances
        }
    };
    if args.output.is_none() && !args.pretty {
        return Ok(());
    }
    print_json_output(
        &json!({
            "ok": true,
            "path": label,
            "instances": instances,
            "output": out_path,
        }),
        args.pretty,
    )
}

/// How many instances the merges by rows of this process left unread.
#[cfg(test)]
pub(crate) static ROWS_NOT_READ: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// What a merge leaves to write: the merged document, or the file of a
/// merge by rows with the number of its instances.
enum MergedStore {
    Document(SettingsBytecode),
    File(Vec<u8>, usize),
}

/// The file of a merge by rows, the number of its instances, and its
/// conflicts.
type RowMerge = (Vec<u8>, usize, Vec<VcMergeConflict>);

/// The rows of the base, ours and theirs that a merge reads, and the ids all
/// three hold.
type MergeReads = ([Vec<usize>; 3], Vec<String>);

/// The merge of three stores in blocks, read by the instances that differ:
/// its file, the number of its instances and its conflicts, as
/// `merge_settings_documents` of the three documents and the whole encode
/// of what it gives. An instance all three hold under one name, class and
/// parent with the same values comes out of that merge as it stands, so the
/// merge runs on the documents without the values of those instances, and
/// its file is ours with the instances the merge read (`EditedStore`).
/// `None` where the whole documents have to be merged: `rows_a_merge_reads`
/// says so, or the merge left out an instance all three hold, which an
/// instance that was not read may name.
pub(crate) fn merge_stores_by_rows(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    prefer: Option<bool>,
) -> Result<Option<RowMerge>> {
    use crate::settings::bytecode::blocks::{EditedStore, RowReader};

    let Some(([base_read, ours_read, theirs_read], in_all)) =
        rows_a_merge_reads(base, ours, theirs)?
    else {
        return Ok(None);
    };
    let (mut ours_document, edited) = EditedStore::decode(ours)?;
    let mut edited = edited.rewriting_moved_blocks();
    edited.materialize_in_passes(&mut ours_document, &ours_read)?;
    let beside = |file: &[u8], read: &[usize]| -> Result<SettingsBytecode> {
        let (mut document, mut rows) = RowReader::decode(file)?;
        rows.fill(&mut document, read)?;
        Ok(document)
    };
    let (base_document, theirs_document) =
        rayon::join(|| beside(base, &base_read), || beside(theirs, &theirs_read));
    let (merged, conflicts) =
        merge_settings_documents(&base_document?, &ours_document, &theirs_document?, prefer);
    if !conflicts.is_empty() {
        return Ok(Some((Vec::new(), merged.instances.len(), conflicts)));
    }
    let merged_ids = (merged.instances.iter())
        .map(|instance| instance.settings_id.as_str())
        .collect::<HashSet<_>>();
    if !in_all.iter().all(|id| merged_ids.contains(id.as_str())) {
        return Ok(None);
    }
    let file = edited.encode_restructured(&merged)?;
    Ok(Some((file, merged.instances.len(), conflicts)))
}

/// The rows of the base, ours and theirs that their merge reads, and the
/// ids all three hold. Every other row is an instance all three hold with
/// one digest of its whole row and of its UniqueId, each side digested as
/// its instances stand in the base, so that a parent and a reference by
/// place say the same of one instance in all three.
///
/// Read beside the instances that differ: the root, a script, which a merge
/// gives a Source, and an instance that names another by an id, which a
/// merge may settle. `None` where the whole documents have to be merged: a
/// store is no store in blocks of this writer, has a frame that is not as its
/// head says, which a whole decode refuses, or holds one id twice, or ours
/// holds the name of a reference by place outside a reference, where a merge
/// writes the id of that place beside it and `Moved` places nothing.
fn rows_a_merge_reads(base: &[u8], ours: &[u8], theirs: &[u8]) -> Result<Option<MergeReads>> {
    use crate::settings::bytecode::blocks::BlockStore;
    use crate::settings::bytecode::property_edit::{Moved, StoreRows};

    let open = |file| BlockStore::open(file).map(BlockStore::holding);
    let stores = [open(base)?, open(ours)?, open(theirs)?];
    let by_place = [vec!["instanceIndex".to_string()]];
    let whole = |store: &BlockStore<'_>| !store.of_this_writer() || !store.intact();
    if stores.iter().any(whole) || stores[1].holds_any(&by_place)? {
        return Ok(None);
    }
    // A block all three hold byte for byte under one structure is not read.
    let shared = (stores[1].blocks_held_by(&stores[0]).into_iter())
        .zip(stores[2].blocks_held_by(&stores[0]))
        .map(|(ours, theirs)| ours && theirs)
        .collect::<Vec<_>>();
    let rows_by_id = |rows: &StoreRows| {
        let by_id = (rows.instances.iter().enumerate())
            .map(|(row, instance)| (instance.settings_id.clone(), row))
            .collect::<HashMap<_, _>>();
        (by_id.len() == rows.instances.len()).then_some(by_id)
    };
    let Some(base_rows) = stores[0].row_digests(true, &shared, None, |_| Some((None, None)))?
    else {
        return Ok(None);
    };
    let Some(in_base) = rows_by_id(&base_rows) else {
        return Ok(None);
    };
    let beside = |store: &BlockStore<'_>| {
        store.row_digests(true, &shared, None, |instances| {
            let ids = |rows: &[SettingsBytecodeInstance]| {
                rows.iter()
                    .map(|row| row.settings_id.clone())
                    .collect::<Vec<_>>()
            };
            let placed = ids(instances) != ids(&base_rows.instances);
            let in_state = (instances.iter())
                .map(|row| in_base.get(&row.settings_id).copied())
                .collect();
            let moved = placed.then(|| Moved::paired(in_state, base_rows.instances.len()));
            Some((None, moved))
        })
    };
    let (Some(ours_rows), Some(theirs_rows)) = (beside(&stores[1])?, beside(&stores[2])?) else {
        return Ok(None);
    };
    let (Some(in_ours), Some(in_theirs)) = (rows_by_id(&ours_rows), rows_by_id(&theirs_rows))
    else {
        return Ok(None);
    };
    let sides = [&base_rows, &ours_rows, &theirs_rows];
    let same_as_base = |row: usize, side: &StoreRows, at: Option<&usize>| {
        at.is_some_and(|at| {
            side.digests[*at] == base_rows.digests[row]
                && side.unique_ids[*at] == base_rows.unique_ids[row]
        })
    };
    let mut same = (base_rows.instances.iter().enumerate())
        .map(|(row, instance)| {
            instance.parent_index.is_some()
                && !is_lua_source_class(&instance.class_name)
                && same_as_base(row, &ours_rows, in_ours.get(&instance.settings_id))
                && same_as_base(row, &theirs_rows, in_theirs.get(&instance.settings_id))
        })
        .collect::<Vec<_>>();
    for side in sides {
        for row in &side.named {
            if let Some(row) = in_base.get(&side.instances[*row].settings_id) {
                same[*row] = false;
            }
        }
    }
    #[cfg(test)]
    ROWS_NOT_READ.fetch_add(
        same.iter().filter(|same| **same).count(),
        std::sync::atomic::Ordering::Relaxed,
    );
    let read = sides.map(|side| {
        (0..side.instances.len())
            .filter(|row| {
                in_base
                    .get(&side.instances[*row].settings_id)
                    .is_none_or(|row| !same[*row])
            })
            .collect::<Vec<_>>()
    });
    let in_all = (in_base.into_keys())
        .filter(|id| in_ours.contains_key(id) && in_theirs.contains_key(id))
        .collect();
    Ok(Some((read, in_all)))
}

#[cfg(test)]
#[path = "text_sim.rs"]
mod text_sim;
