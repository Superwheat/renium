use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use similar::{DiffOp, TextDiff};
use yrs::{
    Any, ClientID, Doc, GetString, In, Map, MapRef, Out, ReadTxn, Text, TextPrelim, Transact,
    TransactionMut,
};

pub(crate) const FILES: &str = "files";
pub(crate) const META: &str = "meta";
pub(crate) const BASES: &str = "bases";
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

/// Per participant and file, the hash of the shared text its last save was
/// folded into: the common base a receiver needs to tell two saves made from
/// one text apart from an edit made after the other arrived.
pub(crate) fn bases_map(doc: &Doc) -> MapRef {
    doc.get_or_insert_map(BASES)
}

fn base_key(client: ClientID, key: &str) -> String {
    format!("{:x} {key}", client.get())
}

pub(crate) fn record_base(
    txn: &mut TransactionMut,
    bases: &MapRef,
    client: ClientID,
    key: &str,
    hash: u64,
) {
    bases.insert(
        txn,
        base_key(client, key),
        In::Any(Any::String(Arc::from(format!("{hash:016x}")))),
    );
}

pub(crate) fn recorded_base<T: ReadTxn>(
    txn: &T,
    bases: &MapRef,
    client: ClientID,
    key: &str,
) -> Option<u64> {
    match bases.get(txn, &base_key(client, key))? {
        Out::Any(Any::String(hash)) => u64::from_str_radix(&hash, 16).ok(),
        _ => None,
    }
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
    new_index: usize,
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
                old_index,
                old_len,
                new_index,
            } => Some(Hunk {
                old_index,
                old_len,
                new_index,
                lines: Vec::new(),
            }),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => Some(Hunk {
                old_index,
                old_len: 0,
                new_index,
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
                new_index,
                lines: replacement(new_index, new_len),
            }),
        })
        .collect()
}

impl Hunk {
    fn span(&self) -> Span {
        Span {
            start: self.old_index,
            len: self.old_len,
        }
    }

    // Insertions sort before changes at the same base line, so a line added
    // above a changed line stays above it.
    fn order(&self) -> (usize, bool) {
        (self.old_index, self.old_len > 0)
    }
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    len: usize,
}

impl Span {
    fn end(self) -> usize {
        self.start + self.len
    }

    fn overlaps(self, other: Span) -> bool {
        match (self.len, other.len) {
            (0, 0) => self.start == other.start,
            (0, _) => other.start < self.start && self.start < other.end(),
            (_, 0) => self.start < other.start && other.start < self.end(),
            _ => self.start < other.end() && other.start < self.end(),
        }
    }

    fn union(self, other: Span) -> Span {
        let start = self.start.min(other.start);
        Span {
            start,
            len: self.end().max(other.end()) - start,
        }
    }
}

/// Names the two versions a conflict keeps between markers.
pub(crate) struct MergeLabels<'a> {
    pub(crate) local: &'a str,
    pub(crate) remote: &'a str,
}

/// Three-way merge by line: every local and remote change since `base` is
/// kept. Where both sides changed the same lines differently, both versions
/// stay in the file between Git-style conflict markers.
pub(crate) fn merge_lines(
    base: &str,
    local: &str,
    remote: &str,
    labels: &MergeLabels<'_>,
) -> String {
    let base_lines = split_lines(base);
    let local_hunks = hunks(base, local);
    let remote_hunks = hunks(base, remote);
    let (mut next_local, mut next_remote) = (0, 0);
    let mut merged = String::new();
    let mut cursor = 0;
    while next_local < local_hunks.len() || next_remote < remote_hunks.len() {
        let (local_start, remote_start) = (next_local, next_remote);
        let local_first = match (local_hunks.get(next_local), remote_hunks.get(next_remote)) {
            (Some(local), Some(remote)) => local.order() <= remote.order(),
            (local, _) => local.is_some(),
        };
        let mut span = if local_first {
            next_local += 1;
            local_hunks[local_start].span()
        } else {
            next_remote += 1;
            remote_hunks[remote_start].span()
        };
        loop {
            if let Some(hunk) = local_hunks
                .get(next_local)
                .filter(|hunk| span.overlaps(hunk.span()))
            {
                span = span.union(hunk.span());
                next_local += 1;
            } else if let Some(hunk) = remote_hunks
                .get(next_remote)
                .filter(|hunk| span.overlaps(hunk.span()))
            {
                span = span.union(hunk.span());
                next_remote += 1;
            } else {
                break;
            }
        }
        if span.start > cursor {
            merged.push_str(&base_lines[cursor..span.start].concat());
        }
        let mine = apply_hunks(&base_lines, span, &local_hunks[local_start..next_local]);
        let theirs = apply_hunks(&base_lines, span, &remote_hunks[remote_start..next_remote]);
        if local_start == next_local || mine == theirs {
            merged.push_str(&theirs);
        } else if remote_start == next_remote {
            merged.push_str(&mine);
        } else if span.len == 0 {
            push_terminated(&mut merged, &mine);
            merged.push_str(&theirs);
        } else {
            push_conflict(&mut merged, labels, &mine, &theirs);
        }
        cursor = cursor.max(span.end());
    }
    if cursor < base_lines.len() {
        merged.push_str(&base_lines[cursor..].concat());
    }
    merged
}

fn apply_hunks(base_lines: &[&str], span: Span, hunks: &[Hunk]) -> String {
    let mut text = String::new();
    let mut at = span.start;
    for hunk in hunks {
        text.push_str(&base_lines[at..hunk.old_index].concat());
        text.push_str(&hunk.lines.concat());
        at = hunk.old_index + hunk.old_len;
    }
    text.push_str(&base_lines[at..span.end()].concat());
    text
}

fn push_terminated(merged: &mut String, text: &str) {
    merged.push_str(text);
    if !text.is_empty() && !text.ends_with('\n') {
        merged.push('\n');
    }
}

fn push_conflict(merged: &mut String, labels: &MergeLabels<'_>, mine: &str, theirs: &str) {
    merged.push_str(&format!("<<<<<<< {}\n", labels.local));
    push_terminated(merged, mine);
    merged.push_str("=======\n");
    push_terminated(merged, theirs);
    merged.push_str(&format!(">>>>>>> {}\n", labels.remote));
}

/// Marks the lines two saves made from the same text replaced differently
/// after the room merged them side by side. `local` is what this side wrote
/// from `base`; `merged` is the shared text once the other side's save from
/// that same base arrived. Lines the other side inserted next to lines this
/// side changed, replacing nothing of `local`, are its version of those lines:
/// the side whose lines lead puts both versions between markers, and a copy
/// identical to this side's lines collapses into one. None when the merged
/// text stays as it is.
pub(crate) fn mark_merged_conflicts(
    base: &str,
    local: &str,
    merged: &str,
    labels: &MergeLabels<'_>,
) -> Option<String> {
    let merged_lines = split_lines(merged);
    let mine = hunks(base, local);
    let mut text = String::new();
    let mut cursor = 0;
    let mut marked = false;
    for theirs in hunks(local, merged) {
        if theirs.old_len > 0 {
            continue;
        }
        let Some(own) = mine.iter().find(|own| {
            own.new_index + own.lines.len() == theirs.old_index
                || own.new_index == theirs.old_index && own.lines == theirs.lines
        }) else {
            continue;
        };
        let (ours, others) = (own.lines.concat(), theirs.lines.concat());
        if ours == others {
            text.push_str(&merged_lines[cursor..theirs.new_index].concat());
        } else {
            let start = theirs.new_index.saturating_sub(own.lines.len()).max(cursor);
            text.push_str(&merged_lines[cursor..start].concat());
            push_conflict(&mut text, labels, &ours, &others);
        }
        cursor = theirs.new_index + theirs.lines.len();
        marked = true;
    }
    text.push_str(&merged_lines[cursor..].concat());
    marked.then_some(text)
}

/// Lines added plus lines removed between two texts.
pub(crate) fn changed_lines(from: &str, to: &str) -> usize {
    TextDiff::from_lines(from, to)
        .ops()
        .iter()
        .map(|op| match *op {
            DiffOp::Equal { .. } => 0,
            DiffOp::Delete { old_len, .. } => old_len,
            DiffOp::Insert { new_len, .. } => new_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => old_len + new_len,
        })
        .sum()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineEndings {
    None,
    Lf,
    Crlf,
    Mixed,
}

fn line_endings(text: &str) -> LineEndings {
    let crlf = text.matches("\r\n").count();
    match (text.matches('\n').count() - crlf, crlf) {
        (0, 0) => LineEndings::None,
        (_, 0) => LineEndings::Lf,
        (0, _) => LineEndings::Crlf,
        _ => LineEndings::Mixed,
    }
}

/// A save that switched a whole file between LF and CRLF takes the line
/// endings of the shared copy again, so the switch does not count as a change
/// to every line and does not collide with concurrent edits.
pub(crate) fn with_line_endings_of(text: String, shared: &str) -> String {
    match (line_endings(&text), line_endings(shared)) {
        (LineEndings::Crlf, LineEndings::Lf) => text.replace("\r\n", "\n"),
        (LineEndings::Lf, LineEndings::Crlf) => text.replace('\n', "\r\n"),
        _ => text,
    }
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
pub(crate) fn new_doc_with_client(client: u64) -> Doc {
    Doc::with_client_id(client)
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
    fn three_way_line_merge_keeps_both_sides_and_marks_conflicts() {
        let labels = MergeLabels {
            local: "host",
            remote: "room",
        };
        let base = "local=0\nremote=0\n";
        let cases = [
            (
                base,
                "local=1\nremote=0\n",
                "local=0\nremote=1\n",
                "local=1\nremote=1\n",
            ),
            (
                base,
                "local=1\nremote=0\n",
                "local=2\nremote=0\n",
                "<<<<<<< host\nlocal=1\n=======\nlocal=2\n>>>>>>> room\nremote=0\n",
            ),
            (
                base,
                "local=1\nremote=0\n",
                "local=1\nremote=0\n",
                "local=1\nremote=0\n",
            ),
            (base, base, "local=0\nremote=1\n", "local=0\nremote=1\n"),
            (
                base,
                "top\nlocal=0\nremote=0\n",
                "local=0\nremote=0\nbottom\n",
                "top\nlocal=0\nremote=0\nbottom\n",
            ),
            (
                base,
                "remote=0\n",
                "local=0\nremote=0\nend\n",
                "remote=0\nend\n",
            ),
            ("", "a\n", "b\n", "a\nb\n"),
            (
                "a\nb\n",
                "a\nb\nlocal",
                "a\nb\nremote",
                "a\nb\nlocal\nremote",
            ),
            ("a\nb\nc\n", "a\nB\nc\n", "a\nb\nnew\nc\n", "a\nB\nnew\nc\n"),
            ("a\nb\nc\n", "x\nb\nc\n", "a\nb\ny", "x\nb\ny"),
            (
                "a\nb\nc\nd\n",
                "a\nB\nc\nD\n",
                "a\nb2\nc2\nd\n",
                "a\n<<<<<<< host\nB\nc\n=======\nb2\nc2\n>>>>>>> room\nD\n",
            ),
            (
                "a\nb",
                "a\nlocal",
                "a\nremote",
                "a\n<<<<<<< host\nlocal\n=======\nremote\n>>>>>>> room\n",
            ),
        ];
        for (base, local, remote, merged) in cases {
            assert_eq!(
                merge_lines(base, local, remote, &labels),
                merged,
                "{local:?} + {remote:?}"
            );
        }
    }

    #[test]
    fn a_line_ending_switch_does_not_collide_with_concurrent_edits() {
        let labels = MergeLabels {
            local: "guest",
            remote: "room",
        };
        let base = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        let remote = "line 1 host\nline 2\nline 3\nline 4\nline 5\n";
        let saved = "line 1\r\nline 2\r\nline 3\r\nline 4\r\nline 5 guest\r\n";
        let local = with_line_endings_of(saved.to_string(), remote);
        assert_eq!(local, "line 1\nline 2\nline 3\nline 4\nline 5 guest\n");
        assert_eq!(
            merge_lines(base, &local, remote, &labels),
            "line 1 host\nline 2\nline 3\nline 4\nline 5 guest\n"
        );
        assert_eq!(
            with_line_endings_of("a\nb\n".to_string(), "x\r\n"),
            "a\r\nb\r\n"
        );
        assert_eq!(
            with_line_endings_of("a\r\nb\n".to_string(), "x\n"),
            "a\r\nb\n"
        );
        assert_eq!(with_line_endings_of("a\r\n".to_string(), "x"), "a\r\n");
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
