use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use similar::{DiffOp, TextDiff};
use yrs::{
    Any, Doc, GetString, In, Map, MapRef, Out, ReadTxn, Text, TextPrelim, Transact, TransactionMut,
};

pub(crate) const FILES: &str = "files";
pub(crate) const META: &str = "meta";
pub(crate) const LOCAL_ORIGIN: &str = "local";

const TEXT_EXTENSIONS: &[&str] = &[
    "lua",
    "luau",
    "json",
    "jsonc",
    "md",
    "txt",
    "toml",
    "yml",
    "yaml",
    "csv",
    "xml",
    "rbxlx",
    "rbxmx",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "lock",
    "project",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Content {
    Text(String),
    Binary(Vec<u8>),
}

impl Content {
    pub(crate) fn from_bytes(path: &Path, bytes: Vec<u8>) -> Content {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase());
        let looks_text = extension
            .as_deref()
            .is_some_and(|value| TEXT_EXTENSIONS.contains(&value))
            || path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with('.'));
        if looks_text && !bytes.contains(&0) {
            return match String::from_utf8(bytes) {
                Ok(text) => Content::Text(text),
                Err(error) => Content::Binary(error.into_bytes()),
            };
        }
        Content::Binary(bytes)
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        match self {
            Content::Text(text) => text.as_bytes().to_vec(),
            Content::Binary(bytes) => bytes.clone(),
        }
    }
}

pub(crate) fn files_map(doc: &Doc) -> MapRef {
    doc.get_or_insert_map(FILES)
}

pub(crate) fn meta_map(doc: &Doc) -> MapRef {
    doc.get_or_insert_map(META)
}

pub(crate) fn read_entry<T: ReadTxn>(txn: &T, files: &MapRef, key: &str) -> Option<Content> {
    match files.get(txn, key)? {
        Out::YText(text) => Some(Content::Text(text.get_string(txn))),
        Out::Any(Any::Buffer(bytes)) => Some(Content::Binary(bytes.to_vec())),
        Out::Any(Any::String(text)) => Some(Content::Text(text.to_string())),
        _ => None,
    }
}

pub(crate) fn snapshot<T: ReadTxn>(txn: &T, files: &MapRef) -> BTreeMap<String, Content> {
    files
        .keys(txn)
        .filter_map(|key| read_entry(txn, files, key).map(|content| (key.to_string(), content)))
        .collect()
}

pub(crate) fn entry_count<T: ReadTxn>(txn: &T, files: &MapRef) -> usize {
    files.len(txn) as usize
}

pub(crate) fn remove_entry(txn: &mut TransactionMut, files: &MapRef, key: &str) -> bool {
    files.remove(txn, key).is_some()
}

pub(crate) fn write_entry(
    txn: &mut TransactionMut,
    files: &MapRef,
    key: &str,
    content: &Content,
) -> bool {
    match content {
        Content::Binary(bytes) => {
            let current = files.get(txn, key);
            if let Some(Out::Any(Any::Buffer(existing))) = &current
                && existing.as_ref() == bytes.as_slice()
            {
                return false;
            }
            files.insert(txn, key, In::Any(Any::Buffer(Arc::from(bytes.as_slice()))));
            true
        }
        Content::Text(text) => {
            if let Some(Out::YText(existing)) = files.get(txn, key) {
                return apply_text_diff(txn, &existing, text);
            }
            files.insert(txn, key, TextPrelim::new(text.clone()));
            true
        }
    }
}

fn apply_text_diff(txn: &mut TransactionMut, target: &yrs::TextRef, wanted: &str) -> bool {
    let current = target.get_string(txn);
    if current == wanted {
        return false;
    }
    let diff = TextDiff::from_lines(current.as_str(), wanted);
    let old_lines = split_lines(&current);
    let new_lines = split_lines(wanted);
    let old_offsets = line_offsets(&old_lines);
    let mut shift: i64 = 0;
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                let start = old_offsets[old_index];
                let length = old_offsets[old_index + old_len] - start;
                target.remove_range(txn, offset(start, shift), length as u32);
                shift -= length as i64;
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                let inserted = new_lines[new_index..new_index + new_len].concat();
                target.insert(txn, offset(old_offsets[old_index], shift), &inserted);
                shift += inserted.len() as i64;
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let start = old_offsets[old_index];
                let length = old_offsets[old_index + old_len] - start;
                let at = offset(start, shift);
                target.remove_range(txn, at, length as u32);
                let inserted = new_lines[new_index..new_index + new_len].concat();
                target.insert(txn, at, &inserted);
                shift += inserted.len() as i64 - length as i64;
            }
        }
    }
    true
}

struct Hunk {
    old_index: usize,
    old_len: usize,
    lines: Vec<String>,
}

fn hunks(base: &str, side: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(base, side);
    let side_lines = split_lines(side);
    let replacement = |new_index: usize, new_len: usize| {
        side_lines[new_index..new_index + new_len]
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
    };
    diff.ops()
        .iter()
        .filter_map(|op| match *op {
            DiffOp::Equal { .. } => None,
            DiffOp::Delete {
                old_index, old_len, ..
            } => Some(Hunk {
                old_index,
                old_len,
                lines: Vec::new(),
            }),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => Some(Hunk {
                old_index,
                old_len: 0,
                lines: replacement(new_index, new_len),
            }),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => Some(Hunk {
                old_index,
                old_len,
                lines: replacement(new_index, new_len),
            }),
        })
        .collect()
}

fn overlaps(a: &Hunk, b: &Hunk) -> bool {
    let (a_start, a_end) = (a.old_index, a.old_index + a.old_len);
    let (b_start, b_end) = (b.old_index, b.old_index + b.old_len);
    match (a.old_len, b.old_len) {
        (0, 0) => false,
        (0, _) => b_start < a_start && a_start < b_end,
        (_, 0) => a_start < b_start && b_start < a_end,
        _ => a_start < b_end && b_start < a_end,
    }
}

/// Three-way merge by line: every local and remote change since `base` is
/// kept; where both changed the same lines the local change stands, since a
/// pending local save is the user's own work and the remote side still holds
/// its version.
pub(crate) fn merge_lines(base: &str, local: &str, remote: &str) -> String {
    let base_lines = split_lines(base);
    let local_hunks = hunks(base, local);
    let remote_hunks = hunks(base, remote)
        .into_iter()
        .filter(|remote| !local_hunks.iter().any(|local| overlaps(local, remote)))
        .collect::<Vec<_>>();
    let mut ordered = local_hunks
        .iter()
        .map(|hunk| (hunk, true))
        .chain(remote_hunks.iter().map(|hunk| (hunk, false)))
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(hunk, local)| (hunk.old_index, !local));
    let mut merged = String::new();
    let mut cursor = 0;
    for (hunk, _) in ordered {
        if hunk.old_index > cursor {
            merged.push_str(&base_lines[cursor..hunk.old_index].concat());
        }
        for line in &hunk.lines {
            merged.push_str(line);
        }
        cursor = cursor.max(hunk.old_index + hunk.old_len);
    }
    merged.push_str(&base_lines[cursor..].concat());
    merged
}

fn offset(start: usize, shift: i64) -> u32 {
    (start as i64 + shift).max(0) as u32
}

fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(index) => {
                lines.push(&rest[..=index]);
                rest = &rest[index + 1..];
            }
            None => {
                lines.push(rest);
                break;
            }
        }
    }
    lines
}

fn line_offsets(lines: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len() + 1);
    let mut total = 0;
    offsets.push(0);
    for line in lines {
        total += line.len();
        offsets.push(total);
    }
    offsets
}

pub(crate) fn set_meta(txn: &mut TransactionMut, meta: &MapRef, key: &str, value: &str) {
    meta.insert(txn, key, In::Any(Any::String(Arc::from(value))));
}

pub(crate) fn meta_value<T: ReadTxn>(txn: &T, meta: &MapRef, key: &str) -> Option<String> {
    match meta.get(txn, key)? {
        Out::Any(Any::String(value)) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) fn new_doc() -> Doc {
    Doc::new()
}

pub(crate) fn transact_local(doc: &Doc) -> TransactionMut<'_> {
    doc.transact_mut_with(LOCAL_ORIGIN)
}

#[cfg(test)]
pub(crate) fn transact_remote(doc: &Doc) -> TransactionMut<'_> {
    doc.transact_mut_with("remote")
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::updates::decoder::Decode;

    fn text_of(doc: &Doc, key: &str) -> Option<Content> {
        let files = files_map(doc);
        let txn = doc.transact();
        read_entry(&txn, &files, key)
    }

    #[test]
    fn text_like_files_that_are_not_utf8_keep_their_bytes() {
        let bytes = vec![0x63, 0x61, 0x66, 0xe9, 0x0a];
        assert_eq!(
            Content::from_bytes(Path::new("legacy.txt"), bytes.clone()),
            Content::Binary(bytes)
        );
        assert_eq!(
            Content::from_bytes(
                Path::new("a.luau"),
                b"return 1
"
                .to_vec()
            ),
            Content::Text(
                "return 1
"
                .into()
            )
        );
    }

    #[test]
    fn three_way_line_merge_keeps_both_sides_and_prefers_local_on_conflict() {
        let base = "local=0
remote=0
";
        assert_eq!(
            merge_lines(
                base,
                "local=1
remote=0
",
                "local=0
remote=1
"
            ),
            "local=1
remote=1
"
        );
        assert_eq!(
            merge_lines(
                base,
                "local=1
remote=0
",
                "local=2
remote=0
"
            ),
            "local=1
remote=0
"
        );
        assert_eq!(
            merge_lines(
                base,
                base,
                "local=0
remote=1
"
            ),
            "local=0
remote=1
"
        );
        assert_eq!(
            merge_lines(
                base,
                "top
local=0
remote=0
",
                "local=0
remote=0
bottom
"
            ),
            "top
local=0
remote=0
bottom
"
        );
        assert_eq!(
            merge_lines(
                base,
                "remote=0
",
                "local=0
remote=0
end
"
            ),
            "remote=0
end
"
        );
        assert_eq!(
            merge_lines(
                "", "a
", "b
"
            ),
            "a
b
"
        );
    }

    #[test]
    fn text_entries_diff_by_line() {
        let doc = new_doc();
        let files = files_map(&doc);
        {
            let mut txn = transact_local(&doc);
            assert!(write_entry(
                &mut txn,
                &files,
                "a.luau",
                &Content::Text("one\ntwo\nthree\n".into())
            ));
        }
        {
            let mut txn = transact_local(&doc);
            assert!(write_entry(
                &mut txn,
                &files,
                "a.luau",
                &Content::Text("one\n2\nthree\nfour".into())
            ));
            assert!(!write_entry(
                &mut txn,
                &files,
                "a.luau",
                &Content::Text("one\n2\nthree\nfour".into())
            ));
        }
        assert_eq!(
            text_of(&doc, "a.luau"),
            Some(Content::Text("one\n2\nthree\nfour".into()))
        );
        {
            let mut txn = transact_local(&doc);
            assert!(write_entry(
                &mut txn,
                &files,
                "a.luau",
                &Content::Text(String::new())
            ));
        }
        assert_eq!(text_of(&doc, "a.luau"), Some(Content::Text(String::new())));
        {
            let mut txn = transact_local(&doc);
            assert!(write_entry(
                &mut txn,
                &files,
                "a.luau",
                &Content::Text("x\ny".into())
            ));
        }
        assert_eq!(text_of(&doc, "a.luau"), Some(Content::Text("x\ny".into())));
    }

    #[test]
    fn binary_entries_replace_whole_values() {
        let doc = new_doc();
        let files = files_map(&doc);
        {
            let mut txn = transact_local(&doc);
            assert!(write_entry(
                &mut txn,
                &files,
                "store.renium",
                &Content::Binary(vec![1, 2, 3])
            ));
            assert!(!write_entry(
                &mut txn,
                &files,
                "store.renium",
                &Content::Binary(vec![1, 2, 3])
            ));
            assert!(write_entry(
                &mut txn,
                &files,
                "store.renium",
                &Content::Binary(vec![9])
            ));
        }
        assert_eq!(
            text_of(&doc, "store.renium"),
            Some(Content::Binary(vec![9]))
        );
        {
            let mut txn = transact_local(&doc);
            assert!(remove_entry(&mut txn, &files, "store.renium"));
            assert!(!remove_entry(&mut txn, &files, "store.renium"));
        }
        assert_eq!(text_of(&doc, "store.renium"), None);
    }

    #[test]
    fn content_classification_prefers_text_extensions() {
        assert_eq!(
            Content::from_bytes(Path::new("a/b.luau"), b"print(1)".to_vec()),
            Content::Text("print(1)".into())
        );
        assert_eq!(
            Content::from_bytes(Path::new("a/b.renium"), vec![0, 1, 2]),
            Content::Binary(vec![0, 1, 2])
        );
        assert_eq!(
            Content::from_bytes(Path::new("a/.gitignore"), b"x\n".to_vec()),
            Content::Text("x\n".into())
        );
    }

    #[test]
    fn concurrent_line_edits_merge() {
        let left = new_doc();
        let right = new_doc();
        let base = Content::Text("a\nb\nc\n".to_string());
        {
            let files = files_map(&left);
            let mut txn = transact_local(&left);
            write_entry(&mut txn, &files, "f", &base);
        }
        let update = left
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        {
            let mut txn = transact_remote(&right);
            txn.apply_update(yrs::Update::decode_v1(&update).unwrap())
                .unwrap();
        }
        {
            let files = files_map(&left);
            let mut txn = transact_local(&left);
            write_entry(&mut txn, &files, "f", &Content::Text("A\nb\nc\n".into()));
        }
        {
            let files = files_map(&right);
            let mut txn = transact_local(&right);
            write_entry(&mut txn, &files, "f", &Content::Text("a\nb\nC\n".into()));
        }
        let from_left = left
            .transact()
            .encode_state_as_update_v1(&right.transact().state_vector());
        let from_right = right
            .transact()
            .encode_state_as_update_v1(&left.transact().state_vector());
        {
            let mut txn = transact_remote(&right);
            txn.apply_update(yrs::Update::decode_v1(&from_left).unwrap())
                .unwrap();
        }
        {
            let mut txn = transact_remote(&left);
            txn.apply_update(yrs::Update::decode_v1(&from_right).unwrap())
                .unwrap();
        }
        assert_eq!(text_of(&left, "f"), Some(Content::Text("A\nb\nC\n".into())));
        assert_eq!(text_of(&left, "f"), text_of(&right, "f"));
    }
}
