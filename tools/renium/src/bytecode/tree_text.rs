//! The text forms of `rbx tr` and `rbx f`.

use std::collections::HashSet;
use std::fmt::Write;

use crate::bytecode::explorer::compact_path_string;
use crate::settings::bytecode::SettingsBytecode;

struct MatchLine<'a> {
    parent: String,
    own: String,
    class_name: &'a str,
}

/// Search matches as text and a last line when the search stopped at `limit`
/// (0 is no limit). A match prints as `path Class`; matches in a row under
/// one parent print the parent's path once, as `path:`, and then one
/// indented `Name Class` line each. `None` when a match has a path the
/// dotted form cannot hold.
pub(super) fn match_lines(
    document: &SettingsBytecode,
    path_segments_by_index: &[Option<Vec<String>>],
    path_ordinals_by_index: &[Option<Vec<usize>>],
    matches: &[usize],
    limit: usize,
) -> Option<String> {
    if matches.is_empty() {
        return Some("no matches".to_string());
    }
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
        if !plain_name(name) || parents.iter().any(|segment| !plain_name(segment)) {
            return None;
        }
        lines.push(MatchLine {
            parent,
            own: compact_path_string(std::slice::from_ref(name), Some(&[ordinal]))?,
            class_name: &document.instances.get(*index)?.class_name,
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
                let _ = writeln!(text, " {} {}", line.own, line.class_name);
            }
        } else {
            let separator = if parent.is_empty() { "" } else { "." };
            for line in &lines[start..end] {
                let _ = writeln!(text, "{parent}{separator}{} {}", line.own, line.class_name);
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

/// The shown part of the subtree under `root`, one instance per line as
/// `Name Class`, with one space of indent per level. A later sibling of the
/// same name carries its ordinal as `Name[2]`, the form path arguments take;
/// a run of siblings alike in name, class, hidden child count and shown
/// subtree folds into `Name[1-4]`, and ` +N` counts the children the depth or
/// the node limit left out.
pub(super) fn text_tree(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    root: usize,
    shown: &HashSet<usize>,
) -> String {
    let mut text = String::new();
    let Some(instance) = document.instances.get(root) else {
        return text;
    };
    let children = children_of(children_by_parent, root);
    let hidden = children
        .iter()
        .filter(|child| !shown.contains(child))
        .count();
    write_line(
        &mut text,
        0,
        &instance.name,
        None,
        &instance.class_name,
        hidden,
    );
    write_children(&mut text, document, children_by_parent, root, 1, shown);
    text.truncate(text.trim_end().len());
    text
}

fn children_of(children_by_parent: &[Vec<usize>], index: usize) -> &[usize] {
    children_by_parent.get(index).map_or(&[][..], Vec::as_slice)
}

fn write_children(
    text: &mut String,
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    parent: usize,
    depth: usize,
    shown: &HashSet<usize>,
) {
    for row in rows(document, children_by_parent, parent, depth, shown) {
        let instance = &document.instances[row.index];
        write_line(
            text,
            depth,
            &instance.name,
            Some((row.first_ordinal, row.last_ordinal)),
            &instance.class_name,
            row.hidden_children,
        );
        text.push_str(&row.subtree);
    }
}

fn rows(
    document: &SettingsBytecode,
    children_by_parent: &[Vec<usize>],
    parent: usize,
    depth: usize,
    shown: &HashSet<usize>,
) -> Vec<Row> {
    let children = children_of(children_by_parent, parent);
    let mut rows: Vec<Row> = Vec::new();
    for (position, child) in children.iter().copied().enumerate() {
        let Some(instance) = document.instances.get(child) else {
            continue;
        };
        if !shown.contains(&child) {
            continue;
        }
        let ordinal = children[..position]
            .iter()
            .filter(|other| {
                document
                    .instances
                    .get(**other)
                    .is_some_and(|sibling| sibling.name == instance.name)
            })
            .count()
            + 1;
        let hidden_children = children_of(children_by_parent, child)
            .iter()
            .filter(|grandchild| !shown.contains(grandchild))
            .count();
        let mut subtree = String::new();
        write_children(
            &mut subtree,
            document,
            children_by_parent,
            child,
            depth + 1,
            shown,
        );
        if let Some(previous) = rows.last_mut() {
            let before = &document.instances[previous.index];
            if previous.last_ordinal + 1 == ordinal
                && previous.hidden_children == hidden_children
                && before.name == instance.name
                && before.class_name == instance.class_name
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
    text: &mut String,
    depth: usize,
    name: &str,
    ordinals: Option<(usize, usize)>,
    class_name: &str,
    hidden_children: usize,
) {
    text.extend(std::iter::repeat_n(' ', depth));
    if plain_name(name) {
        text.push_str(name);
    } else {
        text.push_str(&serde_json::Value::String(name.to_string()).to_string());
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
    text.push_str(class_name);
    if hidden_children > 0 {
        let _ = write!(text, " +{hidden_children}");
    }
    text.push('\n');
}

/// A name that reads unambiguously without quotes at the start of a line: a
/// bracket in a name would read as the ordinal a line adds.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && name.trim() == name && !name.contains(['\n', '\r', '"', ':', '[', ']'])
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{match_lines, text_tree};
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
        let children = settings_children_by_parent(document);
        text_tree(
            document,
            &children,
            0,
            &shown.iter().copied().collect::<HashSet<_>>(),
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
        assert_eq!(
            match_lines(&document, &paths, &ordinals, &[2, 3, 5], 20).as_deref(),
            Some("Workspace.Lobby:\n Wood Crate Model\n Wood Crate[2] Model\nWorkspace.Sign Part")
        );
        assert_eq!(
            match_lines(&document, &paths, &ordinals, &[1, 2], 2).as_deref(),
            Some(
                "Workspace.Lobby Folder\nWorkspace.Lobby.Wood Crate Model\n2 shown, the limit; --all or -l N for more"
            )
        );
        assert_eq!(
            match_lines(&document, &paths, &ordinals, &[0], 20).as_deref(),
            Some("Workspace Workspace")
        );
        assert_eq!(
            match_lines(&document, &paths, &ordinals, &[], 20).as_deref(),
            Some("no matches")
        );
        assert_eq!(match_lines(&document, &paths, &ordinals, &[4], 20), None);
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
}
