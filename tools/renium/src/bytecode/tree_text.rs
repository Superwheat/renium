//! The text forms of `rbx tr`, `rbx f` and `rbx in`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::bytecode::explorer::{
    BytecodeNodeProjection, TextForm, compact_path_string, text_form_prints_fields,
};
use crate::bytecode::value_text::{Place, field_texts};
use crate::settings::bytecode::SettingsBytecode;

/// What instances print after their class: the properties, attributes and
/// tags a caller asked for, as one row of fields by instance index. An
/// instance without an entry prints nothing more.
pub(super) type RowFields = HashMap<usize, String>;

/// The row fields of the listed instances for a tree or a search printed as
/// text, or `None` when the JSON records are due: text is for an uncaptured
/// text-mode run with compact keys that asks for the form's own fields
/// (`default_fields`), or for those and properties, attributes or tags.
pub(super) fn row_fields(
    projection: &BytecodeNodeProjection<'_>,
    requested: &str,
    default_fields: &str,
    form: TextForm,
    indices: &[usize],
) -> Option<RowFields> {
    if !projection.mode.uses_short_keys() || !crate::app::output::prints_text() {
        return None;
    }
    if requested == default_fields {
        return Some(RowFields::new());
    }
    text_form_prints_fields(requested, form).then(|| projected_row_fields(projection, indices))
}

fn projected_row_fields(projection: &BytecodeNodeProjection<'_>, indices: &[usize]) -> RowFields {
    indices
        .iter()
        .filter_map(|index| {
            let fields = field_texts(
                projection.properties(*index).as_ref(),
                projection.attributes(*index).as_ref(),
                Place::Row,
            );
            (!fields.is_empty()).then(|| (*index, fields.join(" ")))
        })
        .collect()
}

/// Inspected instances as text: `path Class`, then one line per property
/// (`Name=value`) and attribute (`@Name=value`) and a line of `#tag` words;
/// an empty line parts two instances. `None` when the JSON records are due:
/// the run is not an uncaptured text-mode one with compact keys, the request
/// names no property, attribute or tag or a record field these lines do not
/// state, or a path does not fit the dotted form.
pub(super) fn inspect_text(
    projection: &BytecodeNodeProjection<'_>,
    requested: &str,
    indices: &[usize],
) -> Option<String> {
    if !projection.mode.uses_short_keys()
        || !crate::app::output::prints_text()
        || !text_form_prints_fields(requested, TextForm::Paths)
    {
        return None;
    }
    inspect_blocks(projection, indices)
}

fn inspect_blocks(projection: &BytecodeNodeProjection<'_>, indices: &[usize]) -> Option<String> {
    let mut blocks = Vec::new();
    for index in indices {
        let segments = projection.path_segments_by_index.get(*index)?.as_deref()?;
        if segments.iter().any(|segment| !plain_name(segment, true)) {
            return None;
        }
        let ordinals = projection
            .path_ordinals_by_index
            .get(*index)
            .and_then(|ordinals| ordinals.as_deref());
        let mut block = format!(
            "{} {}",
            compact_path_string(segments, ordinals)?,
            projection.document.instances.get(*index)?.class_name
        );
        for field in field_texts(
            projection.properties(*index).as_ref(),
            projection.attributes(*index).as_ref(),
            Place::Line,
        ) {
            block.push('\n');
            block.push_str(&field);
        }
        blocks.push(block);
    }
    Some(blocks.join("\n\n"))
}

struct MatchLine<'a> {
    parent: String,
    own: String,
    class_name: &'a str,
    fields: Option<&'a String>,
}

/// Search matches as text and a last line when the search stopped at `limit`
/// (0 is no limit). A match prints as `path Class` and then its row fields;
/// matches in a row under one parent print the parent's path once, as
/// `path:`, and then one indented `Name Class` line each. `None` when a
/// match has a path the dotted form cannot hold.
pub(super) fn match_lines(
    document: &SettingsBytecode,
    path_segments_by_index: &[Option<Vec<String>>],
    path_ordinals_by_index: &[Option<Vec<usize>>],
    matches: &[usize],
    limit: usize,
    fields: &RowFields,
) -> Option<String> {
    if matches.is_empty() {
        return Some("no matches".to_string());
    }
    let before_fields = !fields.is_empty();
    let mut lines = Vec::new();
    for index in matches {
        let segments = path_segments_by_index.get(*index)?.as_deref()?;
        let ordinals = path_ordinals_by_index
            .get(*index)
            .and_then(|ordinals| ordinals.as_deref());
        let (name, parents) = segments.split_last()?;
        let ordinal = ordinals
            .and_then(|ordinals| ordinals.get(parents.len()))
            .copied()
            .unwrap_or(1);
        let parent = match parents {
            [] => String::new(),
            parents => compact_path_string(parents, ordinals)?,
        };
        if segments
            .iter()
            .any(|segment| !plain_name(segment, before_fields))
        {
            return None;
        }
        lines.push(MatchLine {
            parent,
            own: compact_path_string(std::slice::from_ref(name), Some(&[ordinal]))?,
            class_name: &document.instances.get(*index)?.class_name,
            fields: fields.get(index),
        });
    }
    let mut text = String::new();
    let mut start = 0;
    while start < lines.len() {
        let parent = &lines[start].parent;
        let end = lines[start..]
            .iter()
            .position(|line| line.parent != *parent)
            .map_or(lines.len(), |offset| start + offset);
        if end - start > 1 && !parent.is_empty() {
            let _ = writeln!(text, "{parent}:");
            for line in &lines[start..end] {
                let _ = write!(text, " {} {}", line.own, line.class_name);
                end_line(&mut text, line.fields);
            }
        } else {
            let separator = if parent.is_empty() { "" } else { "." };
            for line in &lines[start..end] {
                let _ = write!(text, "{parent}{separator}{} {}", line.own, line.class_name);
                end_line(&mut text, line.fields);
            }
        }
        start = end;
    }
    if limit > 0 && matches.len() >= limit {
        let _ = writeln!(text, "{limit} shown, the limit; --all or -l N for more");
    }
    text.truncate(text.trim_end().len());
    Some(text)
}

/// A shown child of one parent with the lines of its shown subtree, or a run
/// of look-alike children folded into one.
struct Row {
    index: usize,
    first_ordinal: usize,
    last_ordinal: usize,
    hidden_children: usize,
    subtree: String,
}

/// What a tree prints: a store, the instances the depth and the node limit
/// left in, and the row fields of each.
struct Tree<'a> {
    document: &'a SettingsBytecode,
    children_by_parent: &'a [Vec<usize>],
    shown: &'a HashSet<usize>,
    fields: &'a RowFields,
}

/// The shown part of the subtree under `root`, one instance per line as
/// `Name Class` and then its row fields, with one space of indent per level.
/// A later sibling of the same name carries its ordinal as `Name[2]`, the
/// form path arguments take; a run of siblings alike in name, class, hidden
/// child count, row fields and shown subtree folds into `Name[1-4]`, and
/// ` +N` counts the children the depth or the node limit left out.
pub(super) fn text_tree(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    root: usize,
    shown: &HashSet<usize>,
    fields: &RowFields,
) -> String {
    let tree = Tree {
        document,
        children_by_parent,
        shown,
        fields,
    };
    let mut text = String::new();
    if root >= document.instances.len() {
        return text;
    }
    tree.write_line(&mut text, 0, root, None, tree.hidden_children(root));
    tree.write_children(&mut text, root, 1);
    text.truncate(text.trim_end().len());
    text
}

impl Tree<'_> {
    fn children_of(&self, index: usize) -> &[usize] {
        self.children_by_parent
            .get(index)
            .map_or(&[][..], Vec::as_slice)
    }

    fn hidden_children(&self, index: usize) -> usize {
        self.children_of(index)
            .iter()
            .filter(|child| !self.shown.contains(child))
            .count()
    }

    fn write_children(&self, text: &mut String, parent: usize, depth: usize) {
        for row in self.rows(parent, depth) {
            self.write_line(
                text,
                depth,
                row.index,
                Some((row.first_ordinal, row.last_ordinal)),
                row.hidden_children,
            );
            text.push_str(&row.subtree);
        }
    }

    fn rows(&self, parent: usize, depth: usize) -> Vec<Row> {
        let children = self.children_of(parent);
        let mut rows: Vec<Row> = Vec::new();
        for (position, child) in children.iter().copied().enumerate() {
            let Some(instance) = self.document.instances.get(child) else {
                continue;
            };
            if !self.shown.contains(&child) {
                continue;
            }
            let ordinal = children[..position]
                .iter()
                .filter(|other| {
                    self.document
                        .instances
                        .get(**other)
                        .is_some_and(|sibling| sibling.name == instance.name)
                })
                .count()
                + 1;
            let hidden_children = self.hidden_children(child);
            let mut subtree = String::new();
            self.write_children(&mut subtree, child, depth + 1);
            if let Some(previous) = rows.last_mut() {
                let before = &self.document.instances[previous.index];
                if previous.last_ordinal + 1 == ordinal
                    && previous.hidden_children == hidden_children
                    && before.name == instance.name
                    && before.class_name == instance.class_name
                    && self.fields.get(&previous.index) == self.fields.get(&child)
                    && previous.subtree == subtree
                {
                    previous.last_ordinal = ordinal;
                    continue;
                }
            }
            rows.push(Row {
                index: child,
                first_ordinal: ordinal,
                last_ordinal: ordinal,
                hidden_children,
                subtree,
            });
        }
        rows
    }

    fn write_line(
        &self,
        text: &mut String,
        depth: usize,
        index: usize,
        ordinals: Option<(usize, usize)>,
        hidden_children: usize,
    ) {
        let instance = &self.document.instances[index];
        let fields = self.fields.get(&index);
        text.extend(std::iter::repeat_n(' ', depth));
        if plain_name(&instance.name, fields.is_some()) {
            text.push_str(&instance.name);
        } else {
            text.push_str(&serde_json::Value::String(instance.name.clone()).to_string());
        }
        match ordinals {
            Some((first, last)) if first != last => {
                let _ = write!(text, "[{first}-{last}]");
            }
            Some((first, _)) if first > 1 => {
                let _ = write!(text, "[{first}]");
            }
            _ => {}
        }
        text.push(' ');
        text.push_str(&instance.class_name);
        if hidden_children > 0 {
            let _ = write!(text, " +{hidden_children}");
        }
        end_line(text, fields);
    }
}

fn end_line(text: &mut String, fields: Option<&String>) {
    if let Some(fields) = fields {
        text.push(' ');
        text.push_str(fields);
    }
    text.push('\n');
}

/// A name that reads unambiguously without quotes at the start of a line: a
/// bracket in a name would read as the ordinal a line adds and, on a line
/// that goes on with fields, an equals sign as the first of them.
fn plain_name(name: &str, before_fields: bool) -> bool {
    let reads_as_a_field = before_fields && name.contains('=');
    !name.is_empty()
        && name.trim() == name
        && !name.contains(['\n', '\r', '"', ':', '[', ']'])
        && !reads_as_a_field
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::{Value, json};

    use super::{RowFields, inspect_blocks, match_lines, projected_row_fields, text_tree};
    use crate::app::output::OutputMode;
    use crate::bytecode::explorer::{
        BytecodeNodeProjection, TextForm, parse_requested_fields, text_form_prints_fields,
    };
    use crate::editor::paths::build_editor_instance_path_parts;
    use crate::settings::bytecode::{
        SETTINGS_BINARY_VERSION, SettingsBytecode, SettingsBytecodeInstance,
    };
    use crate::settings::tree::settings_children_by_parent;

    fn document(instances: &[(&str, &str, Option<usize>)]) -> SettingsBytecode {
        SettingsBytecode {
            version: SETTINGS_BINARY_VERSION,
            instances: instances
                .iter()
                .enumerate()
                .map(|(index, (name, class_name, parent))| {
                    SettingsBytecodeInstance::new(
                        format!("editor:{index}"),
                        (*name).to_string(),
                        (*class_name).to_string(),
                        *parent,
                    )
                })
                .collect(),
        }
    }

    fn tree(document: &SettingsBytecode, shown: &[usize]) -> String {
        tree_with_fields(document, shown, &RowFields::new())
    }

    fn tree_with_fields(
        document: &SettingsBytecode,
        shown: &[usize],
        fields: &RowFields,
    ) -> String {
        let children = settings_children_by_parent(document);
        text_tree(
            document,
            &children,
            0,
            &shown.iter().copied().collect::<HashSet<_>>(),
            fields,
        )
    }

    fn set_property(document: &mut SettingsBytecode, index: usize, name: &str, value: Value) {
        document.instances[index]
            .properties
            .insert(name.to_string(), value);
    }

    /// What a text form prints for `fields` asked of a Workspace store: the
    /// tree under its root, the listed matches and the listed instances
    /// inspected.
    fn text_forms(
        document: &SettingsBytecode,
        fields: &str,
        indices: &[usize],
    ) -> (String, Option<String>, Option<String>) {
        let children = settings_children_by_parent(document);
        let (segments, ordinals) = build_editor_instance_path_parts(document, "Workspace");
        let requested = parse_requested_fields(Some(fields));
        let projection = BytecodeNodeProjection {
            document,
            children_by_parent: &children,
            path_segments_by_index: &segments,
            path_ordinals_by_index: &ordinals,
            canonical_settings_ids_by_index: &[],
            source_paths_by_index: &[],
            mode: OutputMode::Compact,
            fields: requested.as_ref(),
        };
        let everything = (0..document.instances.len()).collect::<Vec<_>>();
        let row_fields = projected_row_fields(&projection, &everything);
        (
            text_tree(
                document,
                &children,
                0,
                &everything.iter().copied().collect(),
                &row_fields,
            ),
            match_lines(document, &segments, &ordinals, indices, 20, &row_fields),
            inspect_blocks(&projection, indices),
        )
    }

    #[test]
    fn a_tree_indents_one_space_per_level_and_counts_hidden_children() {
        let document = document(&[
            ("Lobby", "Folder", None),
            ("Door", "Model", Some(0)),
            ("Handle", "Part", Some(1)),
            ("Hinge", "Part", Some(1)),
            ("Sign", "Part", Some(0)),
        ]);
        assert_eq!(
            tree(&document, &[0, 1, 4]),
            "Lobby Folder\n Door Model +2\n Sign Part"
        );
        assert_eq!(
            tree(&document, &[0, 1, 2, 3, 4]),
            "Lobby Folder\n Door Model\n  Handle Part\n  Hinge Part\n Sign Part"
        );
        assert_eq!(tree(&document, &[0]), "Lobby Folder +2");
    }

    #[test]
    fn same_named_siblings_carry_ordinals_and_alike_runs_fold() {
        let document = document(&[
            ("Crates", "Model", None),
            ("Crate", "Model", Some(0)),
            ("Crate", "Model", Some(0)),
            ("Crate", "Model", Some(0)),
            ("Lid", "Part", Some(3)),
            ("Crate", "Part", Some(0)),
            ("Crate", "Model", Some(0)),
        ]);
        assert_eq!(
            tree(&document, &[0, 1, 2, 3, 5, 6]),
            "Crates Model\n Crate[1-2] Model\n Crate[3] Model +1\n Crate[4] Part\n Crate[5] Model"
        );
        assert_eq!(
            tree(&document, &[0, 1, 2, 3, 4, 5, 6]),
            "Crates Model\n Crate[1-2] Model\n Crate[3] Model\n  Lid Part\n Crate[4] Part\n Crate[5] Model"
        );
        assert_eq!(
            tree(&document, &[0, 1, 3, 4]),
            "Crates Model +3\n Crate Model\n Crate[3] Model\n  Lid Part"
        );
    }

    #[test]
    fn siblings_with_the_same_shown_subtree_fold_into_one() {
        let document = document(&[
            ("Fences", "Model", None),
            ("Fence", "Model", Some(0)),
            ("Union", "UnionOperation", Some(1)),
            ("Fence", "Model", Some(0)),
            ("Union", "UnionOperation", Some(3)),
            ("Fence", "Model", Some(0)),
            ("Post", "Part", Some(5)),
        ]);
        assert_eq!(
            tree(&document, &[0, 1, 2, 3, 4, 5, 6]),
            "Fences Model\n Fence[1-2] Model\n  Union UnionOperation\n Fence[3] Model\n  Post Part"
        );
    }

    #[test]
    fn matches_print_a_path_and_class_and_share_a_parent_line() {
        let document = document(&[
            ("Workspace", "Workspace", None),
            ("Lobby", "Folder", Some(0)),
            ("Wood Crate", "Model", Some(1)),
            ("Wood Crate", "Model", Some(1)),
            ("a.b", "Part", Some(1)),
            ("Sign", "Part", Some(0)),
        ]);
        let segments =
            |names: &[&str]| Some(names.iter().map(|name| (*name).to_string()).collect());
        let paths: Vec<Option<Vec<String>>> = vec![
            segments(&["Workspace"]),
            segments(&["Workspace", "Lobby"]),
            segments(&["Workspace", "Lobby", "Wood Crate"]),
            segments(&["Workspace", "Lobby", "Wood Crate"]),
            segments(&["Workspace", "Lobby", "a.b"]),
            segments(&["Workspace", "Sign"]),
        ];
        let ordinals = vec![
            Some(vec![1]),
            Some(vec![1, 1]),
            Some(vec![1, 1, 1]),
            Some(vec![1, 1, 2]),
            Some(vec![1, 1, 1]),
            Some(vec![1, 1]),
        ];
        let matched = |matches: &[usize], limit: usize| {
            match_lines(
                &document,
                &paths,
                &ordinals,
                matches,
                limit,
                &RowFields::new(),
            )
        };
        assert_eq!(
            matched(&[2, 3, 5], 20).as_deref(),
            Some("Workspace.Lobby:\n Wood Crate Model\n Wood Crate[2] Model\nWorkspace.Sign Part")
        );
        assert_eq!(
            matched(&[1, 2], 2).as_deref(),
            Some(
                "Workspace.Lobby Folder\nWorkspace.Lobby.Wood Crate Model\n2 shown, the limit; --all or -l N for more"
            )
        );
        assert_eq!(matched(&[0], 20).as_deref(), Some("Workspace Workspace"));
        assert_eq!(matched(&[], 20).as_deref(), Some("no matches"));
        assert_eq!(matched(&[4], 20), None);
    }

    #[test]
    fn a_name_that_would_not_read_as_one_is_quoted() {
        let document = document(&[
            ("Root", "Folder", None),
            (" padded", "Part", Some(0)),
            ("two\nlines", "Part", Some(0)),
            ("Wood Crate", "Model", Some(0)),
            ("Box", "Part", Some(0)),
            ("Box[2]", "Part", Some(0)),
            ("Box", "Part", Some(0)),
        ]);
        assert_eq!(
            tree(&document, &[0, 1, 2, 3, 4, 5, 6]),
            "Root Folder\n \" padded\" Part\n \"two\\nlines\" Part\n Wood Crate Model\n Box Part\n \"Box[2]\" Part\n Box[2] Part"
        );
    }

    #[test]
    fn alike_siblings_fold_only_when_their_fields_are_alike_too() {
        let crates = document(&[
            ("Crates", "Model", None),
            ("Crate", "Part", Some(0)),
            ("Crate", "Part", Some(0)),
            ("Crate", "Part", Some(0)),
            ("Crate", "Part", Some(0)),
            ("Crate", "Part", Some(0)),
        ]);
        let fields = RowFields::from([
            (0, "Visible=true".to_string()),
            (1, "Anchored=true".to_string()),
            (2, "Anchored=true".to_string()),
            (3, "Anchored=false".to_string()),
        ]);
        assert_eq!(
            tree_with_fields(&crates, &[0, 1, 2, 3, 4], &fields),
            "Crates Model +1 Visible=true\n Crate[1-2] Part Anchored=true\n Crate[3] Part Anchored=false\n Crate[4] Part"
        );

        let fences = document(&[
            ("Fences", "Model", None),
            ("Fence", "Model", Some(0)),
            ("Post", "Part", Some(1)),
            ("Fence", "Model", Some(0)),
            ("Post", "Part", Some(3)),
        ]);
        let posts = |first: &str, second: &str| {
            let fields = RowFields::from([(2, first.to_string()), (4, second.to_string())]);
            tree_with_fields(&fences, &[0, 1, 2, 3, 4], &fields)
        };
        assert_eq!(
            posts("Size=1,2,1", "Size=1,2,1"),
            "Fences Model\n Fence[1-2] Model\n  Post Part Size=1,2,1"
        );
        assert_eq!(
            posts("Size=1,2,1", "Size=1,3,1"),
            "Fences Model\n Fence Model\n  Post Part Size=1,2,1\n Fence[2] Model\n  Post Part Size=1,3,1"
        );
    }

    #[test]
    fn a_name_with_an_equals_sign_is_quoted_before_fields() {
        let document = document(&[
            ("Root", "Folder", None),
            ("A=B", "Part", Some(0)),
            ("A=B", "Part", Some(0)),
        ]);
        let fields = RowFields::from([(1, "Anchored=true".to_string())]);
        assert_eq!(
            tree_with_fields(&document, &[0, 1, 2], &fields),
            "Root Folder\n \"A=B\" Part Anchored=true\n A=B[2] Part"
        );
    }

    #[test]
    fn requested_properties_attributes_and_tags_follow_each_line() {
        let mut document = document(&[
            ("Workspace", "Workspace", None),
            ("Lobby", "Folder", Some(0)),
            ("Door", "Part", Some(1)),
            ("Door", "Part", Some(1)),
            ("Door", "Part", Some(1)),
            ("Sign", "Part", Some(1)),
            ("Target", "ObjectValue", Some(1)),
        ]);
        for door in [2, 3, 4] {
            set_property(&mut document, door, "Anchored", json!(door != 4));
            set_property(
                &mut document,
                door,
                "Size",
                json!({"_type": "Vector3", "x": 4.0, "y": 1.0, "z": 2.0}),
            );
            set_property(&mut document, door, "Transparency", json!(0.5));
        }
        set_property(&mut document, 5, "Anchored", json!(true));
        set_property(
            &mut document,
            5,
            "Size",
            json!({"_type": "Vector3", "x": 2.0, "y": 2.0, "z": 0.20000000298023224}),
        );
        set_property(&mut document, 5, "Tags", json!(["Readable"]));
        document.instances[5]
            .attributes
            .insert("Health".to_string(), json!(100.0));
        set_property(
            &mut document,
            6,
            "Value",
            json!({"_type": "Ref", "instanceIndex": 4}),
        );

        let (tree, matches, inspected) = text_forms(
            &document,
            "tree,Size,Anchored,Value,Tags,attrs",
            &[2, 3, 4, 5, 6],
        );
        assert_eq!(
            tree,
            "Workspace Workspace\n Lobby Folder\n  Door[1-2] Part Anchored=true Size=4,1,2\n  Door[3] Part Anchored=false Size=4,1,2\n  Sign Part Anchored=true Size=2,2,.2 @Health=100 #Readable\n  Target ObjectValue Value=Workspace.Lobby.Door[2]"
        );
        assert_eq!(
            matches.as_deref(),
            Some(
                "Workspace.Lobby:\n Door Part Anchored=true Size=4,1,2\n Door[2] Part Anchored=true Size=4,1,2\n Door[3] Part Anchored=false Size=4,1,2\n Sign Part Anchored=true Size=2,2,.2 @Health=100 #Readable\n Target ObjectValue Value=Workspace.Lobby.Door[2]"
            )
        );
        assert_eq!(
            inspected.as_deref(),
            Some(
                "Workspace.Lobby.Door Part\nAnchored=true\nSize=4,1,2\n\nWorkspace.Lobby.Door[2] Part\nAnchored=true\nSize=4,1,2\n\nWorkspace.Lobby.Door[3] Part\nAnchored=false\nSize=4,1,2\n\nWorkspace.Lobby.Sign Part\nAnchored=true\nSize=2,2,.2\n@Health=100\n#Readable\n\nWorkspace.Lobby.Target ObjectValue\nValue=Workspace.Lobby.Door[2]"
            )
        );

        let (_, single, inspected) = text_forms(&document, "lookup,Size", &[1, 5]);
        assert_eq!(
            single.as_deref(),
            Some("Workspace.Lobby Folder\nWorkspace.Lobby.Sign Part Size=2,2,.2")
        );
        assert_eq!(
            inspected.as_deref(),
            Some("Workspace.Lobby Folder\n\nWorkspace.Lobby.Sign Part\nSize=2,2,.2")
        );
    }

    #[test]
    fn a_text_form_takes_property_fields_and_leaves_other_record_fields_to_json() {
        for fields in [
            "tree,Visible,Size",
            "lookup,ords,Size,Anchored",
            "props",
            "attrs, props",
            "n,c,path,a:Health,p:Size,Tags",
        ] {
            assert!(text_form_prints_fields(fields, TextForm::Tree), "{fields}");
        }
        for fields in ["lookup,ords,Size,Anchored", "props", "path,c,attrs"] {
            assert!(text_form_prints_fields(fields, TextForm::Paths), "{fields}");
        }
        for fields in [
            "tree,ords",
            "lookup,ords",
            "n,c",
            "id,Size",
            "tree,x,Size",
            "lookup,src,Size",
            "props,f",
            "Size,canonical",
            "Size,hpl",
            "pid,Size",
            "Size,Source",
            "Size,p:Source",
        ] {
            assert!(!text_form_prints_fields(fields, TextForm::Tree), "{fields}");
            assert!(
                !text_form_prints_fields(fields, TextForm::Paths),
                "{fields}"
            );
        }
        for fields in ["tree,Size", "brief,props", "cc,Size", "ch,Size"] {
            assert!(text_form_prints_fields(fields, TextForm::Tree), "{fields}");
            assert!(
                !text_form_prints_fields(fields, TextForm::Paths),
                "{fields}"
            );
        }
    }
}
