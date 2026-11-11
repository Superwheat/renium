//! `rbx se`: replaces text in script files, checks the result as `rbx ck`
//! does and pushes the changed files, in one call. Nothing is written unless
//! every replacement of every file applies and every changed file parses.

use std::borrow::Cow;
use std::io::Read;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use serde_json::{Value, json};

use super::tools::{
    ScriptMapping, has_script_extension, read_script, relative, resolve_read_target,
};
use crate::app::output::{ReportedFailure, emit_global_output};
use crate::bytecode::edit::{PushedScope, ScriptFileEntry, register_script_file};
use crate::cli::syntax::syntax_failure;
use crate::cli::{BridgeConnectionArgs, ProjectSourceArgs, PushEditorChangesArgs};
use crate::project::config::{self, LoadedProject};
use crate::system::files::write_utf8_file;

/// Lines of the file printed with a refusal, across all of its places.
const SHOWN_LINES: usize = 40;
/// Line numbers listed for an OLD that occurs several times.
const LISTED_OCCURRENCES: usize = 20;
/// The share of an OLD's text a place must hold to be named as the closest.
const CLOSE_ENOUGH: f64 = 0.6;

#[derive(Args)]
pub(crate) struct ScriptEditArgs {
    #[arg(
        help = "Script file or instance path (ServerScriptService.Main)",
        value_name = "FILE"
    )]
    file: String,
    #[arg(
        help = "Text to find and its replacement, in pairs applied in order: OLD must occur exactly once in the file as the earlier pairs left it, an empty NEW deletes it. Other options go before the first OLD",
        value_name = "OLD NEW",
        num_args = 0..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    edits: Vec<String>,
    #[arg(
        help = "After a file's pairs: go on with the pairs of another file (rbx se FILE OLD NEW -f FILE OLD NEW)",
        short = 'f',
        long = "file",
        value_name = "FILE"
    )]
    more_files: Vec<String>,
    #[arg(
        help = "Replace every occurrence of each OLD (at least one) instead of exactly one",
        long
    )]
    all: bool,
    #[arg(help = "Write the files and push nothing", long)]
    no_push: bool,
    #[arg(
        help = "Create FILE as a new script from the source on stdin, with its store entry (the class comes from the file name: .server.luau Script, .client.luau LocalScript, .luau ModuleScript)",
        long,
        conflicts_with = "all"
    )]
    new: bool,
}

struct Pair {
    old: String,
    new: String,
}

struct Target {
    file: String,
    pairs: Vec<Pair>,
}

struct Request {
    targets: Vec<Target>,
    all: bool,
    no_push: bool,
}

/// A value that reads like an option where an OLD was expected.
fn option_hint(value: &str) -> &'static str {
    if value.starts_with('-') && !value.contains(char::is_whitespace) {
        "; options other than -f, --all and --no-push go before FILE"
    } else {
        ""
    }
}

/// The files and pairs of a command line. Everything after the first OLD
/// reaches this as plain values, so `-f FILE`, `--all` and `--no-push` are
/// read here, and only where an OLD would start: a NEW may be any text.
fn edit_request(args: ScriptEditArgs) -> Result<Request> {
    if !args.more_files.is_empty() {
        bail!(
            "-f FILE comes after the pairs of the file before it (rbx se FILE OLD NEW -f FILE OLD NEW); put -- before an OLD that starts with -f"
        );
    }
    let mut request = Request {
        targets: Vec::new(),
        all: args.all,
        no_push: args.no_push,
    };
    let mut target = Target {
        file: args.file,
        pairs: Vec::new(),
    };
    let mut values = args.edits.into_iter();
    while let Some(value) = values.next() {
        match value.as_str() {
            "--all" => request.all = true,
            "--no-push" => request.no_push = true,
            "-f" | "--file" => {
                let file = values.next().context("-f needs a FILE after it")?;
                let finished = std::mem::replace(
                    &mut target,
                    Target {
                        file,
                        pairs: Vec::new(),
                    },
                );
                request.targets.push(finished);
            }
            "" => bail!(
                "Pair {} of {} has an empty OLD",
                target.pairs.len() + 1,
                target.file
            ),
            _ => {
                let Some(new) = values.next() else {
                    bail!(
                        "The last OLD of {} has no NEW: values come in OLD NEW pairs{}",
                        target.file,
                        option_hint(&value)
                    );
                };
                target.pairs.push(Pair { old: value, new });
            }
        }
    }
    request.targets.push(target);
    if let Some(empty) = request
        .targets
        .iter()
        .find(|target| target.pairs.is_empty())
    {
        bail!(
            "{} has no OLD NEW pair (rbx se FILE OLD NEW; --new creates a script from stdin)",
            empty.file
        );
    }
    Ok(request)
}

/// A script's text with every CRLF read as LF, and where each CR was left
/// out, so text is found whichever line ending the file or the pair uses.
struct LfView<'a> {
    text: Cow<'a, str>,
    dropped: Vec<usize>,
}

impl<'a> LfView<'a> {
    fn new(text: &'a str) -> Self {
        if !text.contains("\r\n") {
            return Self {
                text: Cow::Borrowed(text),
                dropped: Vec::new(),
            };
        }
        let mut lf = String::with_capacity(text.len());
        let mut dropped = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("\r\n") {
            lf.push_str(&rest[..at]);
            dropped.push(lf.len());
            lf.push('\n');
            rest = &rest[at + 2..];
        }
        lf.push_str(rest);
        Self {
            text: Cow::Owned(lf),
            dropped,
        }
    }

    /// The offset in the script's own text of an offset in the LF text; the
    /// start of a line break is its CR.
    fn original(&self, offset: usize) -> usize {
        offset + self.dropped.partition_point(|&at| at < offset)
    }

    /// Whether most of the script's line breaks are CRLF.
    fn crlf(&self) -> bool {
        self.dropped.len() * 2 > self.text.matches('\n').count()
    }
}

fn lf(text: &str) -> Cow<'_, str> {
    if text.contains("\r\n") {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// The 1-based line of a byte offset; an offset after the last line counts
/// as that line.
struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    fn of(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
        if starts.len() > 1 && starts.last() == Some(&text.len()) {
            starts.pop();
        }
        Self { starts }
    }

    fn at(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset).max(1)
    }

    fn count(&self) -> usize {
        self.starts.len()
    }
}

struct Replaced {
    text: String,
    /// What the replacement took out, as ranges of the text before it.
    removed: Vec<Range<usize>>,
    /// The length of what went in at each of them.
    inserted: usize,
    /// Where that lies in the text after it.
    spans: Vec<Range<usize>>,
}

enum Miss {
    Absent,
    /// The lines the occurrences start on.
    Several(Vec<usize>),
}

/// Replaces `old` in `text` by `new`, both given with LF line breaks: its one
/// occurrence, or every one with `all`. A line break of `new` is written the
/// way most of the file's are, and the text around a replacement is kept
/// byte for byte.
fn replace(text: &str, old: &str, new: &str, all: bool) -> Result<Replaced, Miss> {
    let view = LfView::new(text);
    let starts = view
        .text
        .match_indices(old)
        .map(|(at, _)| at)
        .collect::<Vec<_>>();
    if starts.is_empty() {
        return Err(Miss::Absent);
    }
    if starts.len() > 1 && !all {
        let lines = Lines::of(&view.text);
        return Err(Miss::Several(
            starts.iter().map(|&at| lines.at(at)).collect(),
        ));
    }
    let inserted = if view.crlf() {
        Cow::Owned(new.replace('\n', "\r\n"))
    } else {
        Cow::Borrowed(new)
    };
    let mut out = String::with_capacity(text.len() + inserted.len() * starts.len());
    let mut removed = Vec::with_capacity(starts.len());
    let mut spans = Vec::with_capacity(starts.len());
    let mut cursor = 0;
    for start in starts {
        let range = view.original(start)..view.original(start + old.len());
        out.push_str(&text[cursor..range.start]);
        spans.push(out.len()..out.len() + inserted.len());
        out.push_str(&inserted);
        cursor = range.end;
        removed.push(range);
    }
    out.push_str(&text[cursor..]);
    Ok(Replaced {
        text: out,
        removed,
        inserted: inserted.len(),
        spans,
    })
}

/// Where an offset of the text before a replacement lies after it. An offset
/// inside a removed range goes to the start of what replaced it, or to its
/// end when the offset ends a span.
fn moved(offset: usize, replaced: &Replaced, end: bool) -> usize {
    let mut shift = 0isize;
    for range in &replaced.removed {
        if range.end <= offset {
            shift += replaced.inserted as isize - range.len() as isize;
        } else if range.start < offset {
            let start = range.start.saturating_add_signed(shift);
            return if end {
                start + replaced.inserted
            } else {
                start
            };
        } else {
            break;
        }
    }
    offset.saturating_add_signed(shift)
}

/// The line ranges the spans cover in `text`, in line order, with ranges
/// that touch joined. A span without text (a deletion) is the line it was
/// taken from.
fn line_ranges(text: &str, spans: &[Range<usize>]) -> Vec<(usize, usize)> {
    let lines = Lines::of(text);
    let mut ranges = spans
        .iter()
        .map(|span| {
            let first = lines.at(span.start);
            let last = if span.is_empty() {
                first
            } else {
                lines.at(span.end - 1)
            };
            (first, last)
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable();
    let mut joined: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (first, last) in ranges {
        match joined.last_mut() {
            Some(previous) if first <= previous.1 + 1 => previous.1 = previous.1.max(last),
            _ => joined.push((first, last)),
        }
    }
    joined
}

fn span_text(first: usize, last: usize) -> String {
    if first == last {
        first.to_string()
    } else {
        format!("{first}-{last}")
    }
}

/// Line ranges as `line 12` or `lines 120-134, 201`.
fn lines_text(ranges: &[(usize, usize)]) -> String {
    let listed = ranges
        .iter()
        .map(|&(first, last)| span_text(first, last))
        .collect::<Vec<_>>()
        .join(", ");
    let one_line = matches!(ranges, [(first, last)] if first == last);
    format!("{} {listed}", if one_line { "line" } else { "lines" })
}

/// Where a line of an OLD stands in it: a first line may start inside a
/// line of the file and a last one may end inside one.
#[derive(Clone, Copy)]
enum Edge {
    Only,
    First,
    Middle,
    Last,
}

impl Edge {
    fn of(index: usize, count: usize) -> Self {
        match (index, count) {
            (_, 1) => Self::Only,
            (0, _) => Self::First,
            (index, count) if index + 1 == count => Self::Last,
            _ => Self::Middle,
        }
    }

    fn fits(self, old: &str, line: &str) -> bool {
        match self {
            Self::Only => line.contains(old),
            Self::First => line.ends_with(old),
            Self::Middle => line == old,
            Self::Last => line.starts_with(old),
        }
    }
}

/// How much of a line of an OLD a line of the file holds, both without the
/// space around them: 1 when it fits, else the share that their common start
/// and common end cover.
fn likeness(edge: Edge, old: &str, line: &str) -> f64 {
    if edge.fits(old, line) {
        return 1.0;
    }
    if old.is_empty() || line.is_empty() {
        return 0.0;
    }
    let shorter = old.len().min(line.len());
    let start = old
        .bytes()
        .zip(line.bytes())
        .take_while(|(old, line)| old == line)
        .count();
    let end = old
        .bytes()
        .rev()
        .zip(line.bytes().rev())
        .take(shorter - start)
        .take_while(|(old, line)| old == line)
        .count();
    (start + end) as f64 / old.len().max(line.len()) as f64
}

/// The place of a file closest to an OLD that does not occur in it.
struct Closest {
    first: usize,
    last: usize,
    /// Every line fits once the space at the ends of the lines is left out.
    spacing_only: bool,
    matched: usize,
    of: usize,
    /// The runs of lines whose text is not the OLD's.
    differing: Vec<(usize, usize)>,
}

/// Finds the lines of `text` that an absent `old` most likely meant, both
/// with LF line breaks: the window of lines that holds most of the OLD's
/// text when lines are compared without the space around them. None when no
/// window holds enough of it.
fn closest_place(text: &str, old: &str) -> Option<Closest> {
    let file = text.lines().collect::<Vec<_>>();
    let mut wanted = old.split('\n').collect::<Vec<_>>();
    while wanted.last().is_some_and(|line| line.trim().is_empty()) {
        wanted.pop();
    }
    let blank = wanted
        .iter()
        .take_while(|line| line.trim().is_empty())
        .count();
    let wanted = &wanted[blank..];
    if wanted.is_empty() || file.is_empty() {
        return None;
    }
    let count = wanted.len();
    let bare_file = file.iter().map(|line| line.trim()).collect::<Vec<_>>();
    let bare_wanted = wanted.iter().map(|line| line.trim()).collect::<Vec<_>>();
    let weight = |index: usize| (bare_wanted[index].len() + 1) as f64;
    let total = (0..count).map(weight).sum::<f64>();
    let in_file = |start: isize, index: usize| {
        usize::try_from(start + index as isize)
            .ok()
            .filter(|line| *line < file.len())
    };
    let mut best = (0.0, 0isize);
    for start in 1 - count as isize..file.len() as isize {
        let score = (0..count)
            .filter_map(|index| Some((index, in_file(start, index)?)))
            .map(|(index, line)| {
                weight(index)
                    * likeness(Edge::of(index, count), bare_wanted[index], bare_file[line])
            })
            .sum::<f64>();
        if score > best.0 {
            best = (score, start);
        }
    }
    let (score, start) = best;
    if score < CLOSE_ENOUGH * total {
        return None;
    }
    let window = (0..count)
        .filter_map(|index| Some((index, in_file(start, index)?)))
        .collect::<Vec<_>>();
    let matched = window
        .iter()
        .filter(|(index, line)| Edge::of(*index, count).fits(bare_wanted[*index], bare_file[*line]))
        .count();
    let mut differing: Vec<(usize, usize)> = Vec::new();
    for &(index, line) in &window {
        if Edge::of(index, count).fits(wanted[index], file[line]) {
            continue;
        }
        match differing.last_mut() {
            Some(run) if run.1 == line => run.1 = line + 1,
            _ => differing.push((line + 1, line + 1)),
        }
    }
    let (first, last) = (window.first()?.1 + 1, window.last()?.1 + 1);
    if differing.is_empty() {
        differing.push((first, last));
    }
    Some(Closest {
        first,
        last,
        spacing_only: matched == count,
        matched,
        of: count,
        differing,
    })
}

/// A run of the file's lines as a refusal prints it.
struct Shown {
    first: usize,
    last: usize,
    text: String,
}

/// The text of the runs, `SHOWN_LINES` lines at most, and how many lines of
/// them that left out.
fn shown_runs(text: &str, runs: &[(usize, usize)]) -> (Vec<Shown>, usize) {
    let lines = text.lines().collect::<Vec<_>>();
    let mut shown = Vec::new();
    let mut left = SHOWN_LINES;
    let mut hidden = 0;
    for &(first, last) in runs {
        let length = last + 1 - first;
        let taken = length.min(left);
        left -= taken;
        hidden += length - taken;
        if taken > 0 {
            shown.push(Shown {
                first,
                last: first + taken - 1,
                text: lines[first - 1..first - 1 + taken].join("\n"),
            });
        }
    }
    (shown, hidden)
}

/// Why nothing was written.
enum Refusal {
    Absent {
        file: String,
        pair: usize,
        /// The file held the OLD until an earlier pair of the call changed it.
        taken_by_earlier_pair: bool,
        closest: Option<(Closest, Vec<Shown>, usize)>,
        /// Earlier pairs moved the lines, so the numbers are not the file's.
        renumbered: bool,
        hint: &'static str,
    },
    Several {
        file: String,
        pair: usize,
        lines: Vec<usize>,
        renumbered: bool,
    },
    Syntax {
        file: String,
        edited: bool,
        line: Option<usize>,
        reason: String,
        text: Option<String>,
        /// The file's own syntax error, when it did not parse before either.
        before: Option<String>,
    },
}

const RENUMBERED: &str = " (line numbers as the earlier pairs leave them)";

impl Refusal {
    fn text(&self) -> String {
        match self {
            Self::Absent {
                file,
                pair,
                taken_by_earlier_pair,
                closest,
                renumbered,
                hint,
            } => {
                let mut text = format!("{file} pair {pair}: OLD not found");
                if *taken_by_earlier_pair {
                    text.push_str("; an earlier pair of this call changed that text, and pairs apply in order");
                    return text;
                }
                let Some((closest, shown, hidden)) = closest else {
                    text.push_str(&format!("; nothing in the file is close to it{hint}"));
                    return text;
                };
                let place = lines_text(&[(closest.first, closest.last)]);
                let how = if closest.spacing_only {
                    " (only indentation or trailing space differs)".to_string()
                } else if closest.of > 1 {
                    format!(" ({} of OLD's {} lines match)", closest.matched, closest.of)
                } else {
                    String::new()
                };
                let note = if *renumbered { RENUMBERED } else { "" };
                text.push_str(&format!("; closest is {place}{how}{note}. The file has:"));
                for run in shown {
                    text.push_str(&format!(
                        "\n{}:\n{}",
                        span_text(run.first, run.last),
                        run.text
                    ));
                }
                if *hidden > 0 {
                    text.push_str(&format!("\n{hidden} more lines differ"));
                }
                text
            }
            Self::Several {
                file,
                pair,
                lines,
                renumbered,
            } => {
                let listed = lines
                    .iter()
                    .take(LISTED_OCCURRENCES)
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = match lines.len().saturating_sub(LISTED_OCCURRENCES) {
                    0 => String::new(),
                    more => format!(" and {more} more"),
                };
                let note = if *renumbered { RENUMBERED } else { "" };
                format!(
                    "{file} pair {pair}: OLD occurs {} times, at lines {listed}{more}{note}; give more of the text around the one you mean, or --all for every one",
                    lines.len()
                )
            }
            Self::Syntax {
                file,
                edited,
                line,
                reason,
                text,
                before,
            } => {
                let when = if *edited { " after the edits" } else { "" };
                let mut message = match line {
                    Some(line) => format!("{file}: syntax error{when}, line {line}: {reason}"),
                    None => format!("{file}{when}: {reason}"),
                };
                if let Some(before) = before {
                    message.push_str(&format!(
                        " (the file did not parse before either: {before})"
                    ));
                }
                if let (Some(line), Some(text)) = (line, text) {
                    message.push_str(&format!("\n{line}:\n{text}"));
                }
                message
            }
        }
    }

    fn json(&self) -> Value {
        match self {
            Self::Absent {
                file,
                pair,
                taken_by_earlier_pair,
                closest,
                renumbered,
                ..
            } => {
                let mut value = json!({ "file": file, "pair": pair, "error": "OLD not found" });
                if *taken_by_earlier_pair {
                    value["changedByEarlierPair"] = json!(true);
                }
                if let Some((closest, shown, _)) = closest {
                    value["closest"] = json!({
                        "lines": [closest.first, closest.last],
                        "spacingOnly": closest.spacing_only,
                        "matched": closest.matched,
                        "of": closest.of,
                        "fileText": shown
                            .iter()
                            .map(|run| json!({ "lines": [run.first, run.last], "text": run.text }))
                            .collect::<Vec<_>>(),
                    });
                    if *renumbered {
                        value["linesAfterEarlierPairs"] = json!(true);
                    }
                }
                value
            }
            Self::Several {
                file,
                pair,
                lines,
                renumbered,
            } => {
                let mut value = json!({
                    "file": file,
                    "pair": pair,
                    "error": format!("OLD occurs {} times", lines.len()),
                    "lines": lines,
                });
                if *renumbered {
                    value["linesAfterEarlierPairs"] = json!(true);
                }
                value
            }
            Self::Syntax {
                file,
                line,
                reason,
                before,
                ..
            } => {
                let mut value = json!({ "file": file, "error": reason });
                if let Some(line) = line {
                    value["line"] = json!(line);
                }
                if let Some(before) = before {
                    value["errorBefore"] = json!(before);
                }
                value
            }
        }
    }
}

/// Prints why nothing was written and fails the command.
fn refuse(refusals: &[Refusal]) -> Result<()> {
    let mut text = refusals
        .iter()
        .map(Refusal::text)
        .collect::<Vec<_>>()
        .join("\n");
    text.push_str("\nnothing written");
    let value = json!({
        "ok": false,
        "written": false,
        "failures": refusals.iter().map(Refusal::json).collect::<Vec<_>>(),
    });
    emit_global_output(&value, &text)?;
    Err(ReportedFailure.into())
}

fn syntax_refusal(file: &str, source: &str, original: Option<&str>) -> Option<Refusal> {
    let failure = syntax_failure(source)?;
    let text = failure
        .line
        .and_then(|line| source.lines().nth(line.checked_sub(1)?))
        .map(str::to_string);
    let before = original
        .and_then(syntax_failure)
        .map(|failure| match failure.line {
            Some(line) => format!("line {line}: {}", failure.reason),
            None => failure.reason,
        });
    Some(Refusal::Syntax {
        file: file.to_string(),
        edited: original.is_some(),
        line: failure.line,
        reason: failure.reason,
        text,
        before,
    })
}

/// Why pair `pair` of `file` did not apply to `current`, the file's text as
/// the earlier pairs left it.
fn miss_refusal(
    file: &str,
    pair: usize,
    original: &str,
    current: &str,
    old: &str,
    miss: Miss,
) -> Refusal {
    let view = LfView::new(current);
    let renumbered = Lines::of(&view.text).count() != Lines::of(&lf(original)).count();
    if let Miss::Several(lines) = miss {
        return Refusal::Several {
            file: file.to_string(),
            pair,
            lines,
            renumbered,
        };
    }
    let taken_by_earlier_pair = current != original && lf(original).contains(old);
    let closest = closest_place(&view.text, old).map(|closest| {
        let (shown, hidden) = shown_runs(&view.text, &closest.differing);
        (closest, shown, hidden)
    });
    Refusal::Absent {
        file: file.to_string(),
        pair,
        taken_by_earlier_pair,
        closest,
        renumbered,
        hint: option_hint(old),
    }
}

struct Edited {
    text: String,
    edits: usize,
    lines: Vec<(usize, usize)>,
}

/// Applies a file's pairs in order to its text and checks the result's
/// syntax. A pair that does not apply or a result that does not parse is
/// added to `refusals`, and the pairs after a failed one are still tried so
/// one call reports every miss.
fn edit_text(
    file: &str,
    original: &str,
    pairs: &[&Pair],
    all: bool,
    refusals: &mut Vec<Refusal>,
) -> Option<Edited> {
    let refused_before = refusals.len();
    let mut text = original.to_string();
    let mut spans: Vec<Range<usize>> = Vec::new();
    for (index, pair) in pairs.iter().enumerate() {
        let old = lf(&pair.old);
        match replace(&text, &old, &lf(&pair.new), all) {
            Ok(replaced) => {
                for span in &mut spans {
                    *span = moved(span.start, &replaced, false)..moved(span.end, &replaced, true);
                }
                spans.extend(replaced.spans.iter().cloned());
                text = replaced.text;
            }
            Err(miss) => {
                refusals.push(miss_refusal(file, index + 1, original, &text, &old, miss));
            }
        }
    }
    if refusals.len() > refused_before {
        return None;
    }
    if text != original
        && let Some(refusal) = syntax_refusal(file, &text, Some(original))
    {
        refusals.push(refusal);
        return None;
    }
    Some(Edited {
        lines: line_ranges(&text, &spans),
        edits: spans.len(),
        text,
    })
}

struct Change {
    path: PathBuf,
    file: String,
    original: String,
    edited: Edited,
}

impl Change {
    fn changed(&self) -> bool {
        self.edited.text != self.original
    }

    fn text(&self) -> String {
        if !self.changed() {
            return format!("{}: unchanged", self.file);
        }
        let edits = if self.edited.edits == 1 {
            "edit"
        } else {
            "edits"
        };
        format!(
            "{}: {} {edits}, {}",
            self.file,
            self.edited.edits,
            lines_text(&self.edited.lines)
        )
    }

    fn json(&self) -> Value {
        let mut value = json!({ "file": self.file, "edits": self.edited.edits });
        if self.changed() {
            value["lines"] = json!(self.edited.lines);
        } else {
            value["unchanged"] = json!(true);
        }
        value
    }
}

fn failed(failure: crate::automation::Failure) -> anyhow::Error {
    anyhow!(failure.0.m)
}

/// Every file of the request with its pairs applied, or every reason one of
/// them cannot be. A file named twice gets its pairs in the order given.
fn plan(
    root: &Path,
    source: &Path,
    request: &Request,
) -> Result<Result<Vec<Change>, Vec<Refusal>>> {
    let mapping = ScriptMapping::new(root, source);
    let mut files: Vec<(PathBuf, Vec<&Pair>)> = Vec::new();
    for target in &request.targets {
        let path = resolve_read_target(&mapping, &target.file).map_err(failed)?;
        match files.iter_mut().find(|(known, _)| *known == path) {
            Some((_, pairs)) => pairs.extend(&target.pairs),
            None => files.push((path, target.pairs.iter().collect())),
        }
    }
    let mut changes = Vec::new();
    let mut refusals = Vec::new();
    for (path, pairs) in files {
        let file = relative(root, &path);
        let original = read_script(&path, "script-edit").map_err(failed)?;
        if let Some(edited) = edit_text(&file, &original, &pairs, request.all, &mut refusals) {
            changes.push(Change {
                path,
                file,
                original,
                edited,
            });
        }
    }
    Ok(if refusals.is_empty() {
        Ok(changes)
    } else {
        Err(refusals)
    })
}

/// What a push carries: script files by path, or a new script by its store
/// instance.
pub(crate) enum PushTarget<'a> {
    Files(&'a [PathBuf]),
    Stored {
        settings_file: &'a Path,
        settings_id: &'a str,
    },
}

/// Sends the target to Studio for the project at `root` and returns the push
/// summary.
pub(crate) type Push<'a> = &'a dyn Fn(&Path, &PushTarget) -> Result<Value>;

/// The push `rbx ps FILES` sends for named script files (no large-push hold,
/// sources read back), or the scoped push `rbx ba --push` sends for a new
/// instance; a Live Sync that is writing is left to deliver the files.
fn push_to_studio(root: &Path, target: &PushTarget) -> Result<Value> {
    let mut args = match target {
        PushTarget::Files(paths) => {
            let mut args = PushEditorChangesArgs::new(
                ProjectSourceArgs::current(),
                BridgeConnectionArgs::default_connection(),
            );
            args.paths = paths.to_vec();
            args
        }
        PushTarget::Stored {
            settings_file,
            settings_id,
        } => crate::bytecode::edit::stored_instances_push(
            settings_file.to_path_buf(),
            vec![settings_id.to_string()],
            PushedScope::Subtree,
        ),
    };
    args.project.project_root = root.to_path_buf();
    args.defer_to_live_sync = true;
    crate::editor::sync::push_editor_changes_result(args)
}

fn no_studio(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_lowercase();
    ["no studio runtime", "no connected", "no plugin bridge"]
        .iter()
        .any(|sign| text.contains(sign))
}

/// What became of the push, as the result's `push`: `pushed` with the push
/// summary, or the `reason` nothing was pushed. Without a Studio the files
/// stay written and the command succeeds; any other push failure fails it.
fn push_outcome(pushed: Result<Value>) -> Result<Value> {
    let summary = match pushed {
        Ok(summary) => summary,
        Err(error) if no_studio(&error) => {
            return Ok(json!({ "pushed": false, "reason": "noStudio" }));
        }
        Err(error) => {
            return Err(error.context(
                "The files were written but the push failed; `rbx ps FILES` pushes them",
            ));
        }
    };
    if summary["liveSync"] == json!(true) {
        return Ok(json!({ "pushed": false, "reason": "liveSync" }));
    }
    if summary["unchanged"] == json!(true) {
        return Ok(json!({ "pushed": false, "reason": "unchanged" }));
    }
    let mut outcome = json!({ "pushed": true, "verified": summary["verified"] == json!(true) });
    for (key, value) in summary.as_object().into_iter().flatten() {
        if !matches!(key.as_str(), "ok" | "verified") {
            outcome[key] = value.clone();
        }
    }
    Ok(outcome)
}

fn push_text(push: &Value) -> String {
    if push["pushed"] != json!(true) {
        return match push["reason"].as_str() {
            Some("liveSync") => "not pushed: Live Sync delivers it",
            Some("noStudio") => "not pushed: no Studio is connected",
            Some("unchanged") => "not pushed: Studio already has this text",
            Some("noChange") => "nothing to push",
            _ => "not pushed (--no-push)",
        }
        .to_string();
    }
    let mut text = if push["verified"] == json!(true) {
        "pushed, verified".to_string()
    } else {
        "pushed, not verified".to_string()
    };
    if let Some(packages) = push.get("autoDesyncedPackages") {
        text.push_str(&format!("; autoDesyncedPackages: {packages}"));
    }
    text
}

fn finish(files: Vec<(String, Value)>, push: Value) -> Result<()> {
    let (lines, records): (Vec<String>, Vec<Value>) = files.into_iter().unzip();
    let text = format!("{}\n{}", lines.join("\n"), push_text(&push));
    emit_global_output(
        &json!({ "ok": true, "files": records, "push": push }),
        &text,
    )
}

/// The path of a script that does not exist yet.
fn new_script_path(root: &Path, requested: &str) -> Result<PathBuf> {
    let given = PathBuf::from(requested);
    if !has_script_extension(&given) {
        bail!(
            "--new takes a script file path such as src/ServerScriptService/Name.server.luau, not {requested}"
        );
    }
    if given
        .components()
        .any(|component| component == Component::ParentDir)
    {
        bail!("{requested}: give the path without `..`");
    }
    let path = if given.is_absolute() {
        given
    } else {
        root.join(given)
    };
    if path.exists() {
        bail!("{requested} already exists; `rbx se FILE OLD NEW` edits it");
    }
    Ok(path)
}

struct Created {
    path: PathBuf,
    class: &'static str,
    /// The store file and id of the new instance; None for a script that a
    /// tree mapping or mount of the project reads from its file alone.
    stored: Option<(PathBuf, String)>,
}

/// Writes a new script: under the plain source folder with its store
/// instance, both written together by the code `rbx ba` adds a script with,
/// elsewhere as the file the project's mapping reads.
fn add_script(loaded: &LoadedProject, path: &Path, source: &str) -> Result<Created> {
    let file = relative(&loaded.root, path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{file} has no file name"))?;
    config::cache_script_naming(&loaded.root, &loaded.project);
    let naming = config::project_script_naming(&loaded.project);
    let Some((class, stem, _)) = crate::editor::paths::infer_source_script(file_name, &naming)
    else {
        bail!("{file_name} is not a script file name this project reads");
    };
    let source_root = loaded.root.join(&loaded.project.source_root);
    let staged = config::project_source_to_staged_relatives(loaded, path)?;
    if staged.is_empty() {
        bail!("{file} is not under a source folder of the project");
    }
    let plain = path.strip_prefix(&source_root).ok();
    if plain.is_none_or(|plain| staged != [plain.to_path_buf()]) {
        write_utf8_file(path, source)?;
        return Ok(Created {
            path: path.to_path_buf(),
            class,
            stored: None,
        });
    }
    if stem.is_none() {
        bail!(
            "{file_name} names its parent instance; create that instance with `rbx ba` and its class"
        );
    }
    let spec = crate::editor::paths::service_from_changed_path(&source_root, path)
        .and_then(|service| {
            crate::editor::paths::infer_editor_source_path_spec(&source_root, &service, path)
        })
        .with_context(|| {
            format!("{file} is not inside a service folder (src/ServerScriptService/Name.luau)")
        })?;
    let added = register_script_file(
        loaded,
        ScriptFileEntry {
            segments: &spec.path_segments,
            file_name,
            source: Some(source),
        },
    )?
    .with_context(|| format!("{file} names no instance below a service"))?;
    let text = |key: &str| added.get(key).and_then(Value::as_str);
    let written = added["sourceWrites"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .find(|written| has_script_extension(written));
    Ok(Created {
        path: written.unwrap_or_else(|| path.to_path_buf()),
        class,
        stored: text("settingsFile")
            .zip(text("settingsId"))
            .map(|(file, id)| (PathBuf::from(file), id.to_string())),
    })
}

fn create(
    loaded: &LoadedProject,
    args: ScriptEditArgs,
    stdin: &mut dyn Read,
    push: Push,
) -> Result<()> {
    if !args.edits.is_empty() || !args.more_files.is_empty() {
        bail!("--new takes one FILE and its source on stdin, no OLD NEW pairs");
    }
    let mut source = String::new();
    stdin
        .read_to_string(&mut source)
        .context("Could not read the new script's source from stdin as UTF-8 text")?;
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    if source.trim().is_empty() {
        bail!("--new reads the new script's source from stdin, and stdin was empty");
    }
    let path = new_script_path(&loaded.root, &args.file)?;
    if let Some(refusal) = syntax_refusal(&relative(&loaded.root, &path), source, None) {
        return refuse(&[refusal]);
    }
    let created = add_script(loaded, &path, source)?;
    let outcome = if args.no_push {
        json!({ "pushed": false, "reason": "noPush" })
    } else {
        let files = [created.path.clone()];
        let target = match &created.stored {
            Some((settings_file, settings_id)) => PushTarget::Stored {
                settings_file,
                settings_id,
            },
            None => PushTarget::Files(&files),
        };
        push_outcome(push(&loaded.root, &target))?
    };
    let file = relative(&loaded.root, &created.path);
    let lines = source.lines().count();
    let unit = if lines == 1 { "line" } else { "lines" };
    let text = format!("{file}: new {}, {lines} {unit}", created.class);
    let record = json!({
        "file": file,
        "created": true,
        "class": created.class,
        "lineCount": lines,
    });
    finish(vec![(text, record)], outcome)
}

pub(crate) fn script_edit_command(args: ScriptEditArgs, project: Option<&Path>) -> Result<()> {
    run(args, project, &mut std::io::stdin().lock(), &push_to_studio)
}

/// `script_edit_command` with the stdin a new script's source is read from
/// and the push to Studio given by the caller.
pub(crate) fn run(
    args: ScriptEditArgs,
    project: Option<&Path>,
    stdin: &mut dyn Read,
    push: Push,
) -> Result<()> {
    let loaded = config::load_project(project, None)?;
    if args.new {
        return create(&loaded, args, stdin, push);
    }
    let request = edit_request(args)?;
    let source = loaded.root.join(&loaded.project.source_root);
    let changes = match plan(&loaded.root, &source, &request)? {
        Ok(changes) => changes,
        Err(refusals) => return refuse(&refusals),
    };
    let mut written = Vec::new();
    for change in changes.iter().filter(|change| change.changed()) {
        write_utf8_file(&change.path, &change.edited.text)?;
        written.push(change.path.clone());
    }
    let outcome = if written.is_empty() {
        json!({ "pushed": false, "reason": "noChange" })
    } else if request.no_push {
        json!({ "pushed": false, "reason": "noPush" })
    } else {
        push_outcome(push(&loaded.root, &PushTarget::Files(&written)))?
    };
    let files = changes
        .iter()
        .map(|change| (change.text(), change.json()))
        .collect();
    finish(files, outcome)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;

    use clap::Parser;

    use super::*;
    use crate::app::output::{CaptureMode, capture_output};
    use crate::cli::{Cli, Commands};

    const SHOP: &str = "local Shop = {}\nfunction Shop.buy(player, id)\n\tlocal price = 5\n\treturn price\nend\nfunction Shop.sell(player)\n\treturn 5\nend\nreturn Shop\n";

    /// One file's pairs applied to `original`: the new text and its edits
    /// and lines as the result line prints them, or the refusals' text.
    fn edited(
        original: &str,
        pairs: &[(&str, &str)],
        all: bool,
    ) -> Result<(String, String), String> {
        let pairs = pairs
            .iter()
            .map(|(old, new)| Pair {
                old: (*old).to_string(),
                new: (*new).to_string(),
            })
            .collect::<Vec<_>>();
        let mut refusals = Vec::new();
        let pairs = pairs.iter().collect::<Vec<_>>();
        match edit_text("src/A.luau", original, &pairs, all, &mut refusals) {
            Some(edited) => {
                let summary = format!("{} {}", edited.edits, lines_text(&edited.lines));
                Ok((edited.text, summary))
            }
            None => Err(refusals
                .iter()
                .map(Refusal::text)
                .collect::<Vec<_>>()
                .join("\n")),
        }
    }

    #[test]
    fn one_occurrence_is_replaced_and_its_new_lines_are_reported() {
        assert_eq!(
            edited(SHOP, &[("local price = 5", "local price = 7")], false),
            Ok((
                SHOP.replace("local price = 5", "local price = 7"),
                "1 line 3".to_string()
            ))
        );
        assert_eq!(
            edited(
                SHOP,
                &[("\treturn price\n", "\tprint(price)\n\treturn price\n")],
                false
            ),
            Ok((
                SHOP.replace("\treturn price\n", "\tprint(price)\n\treturn price\n"),
                "1 lines 4-5".to_string()
            ))
        );
        assert_eq!(
            edited(SHOP, &[("\tlocal price = 5\n", "")], false),
            Ok((
                SHOP.replace("\tlocal price = 5\n", ""),
                "1 line 3".to_string()
            ))
        );
        assert_eq!(
            edited(SHOP, &[("return Shop\n", "return Shop")], false),
            Ok((SHOP.trim_end().to_string(), "1 line 9".to_string()))
        );
    }

    #[test]
    fn several_occurrences_are_refused_with_their_lines_unless_all_is_given() {
        assert_eq!(
            edited(SHOP, &[("return", "error")], false),
            Err("src/A.luau pair 1: OLD occurs 3 times, at lines 4, 7, 9; give more of the text around the one you mean, or --all for every one".to_string())
        );
        assert_eq!(
            edited(SHOP, &[("5", "6")], true),
            Ok((SHOP.replace('5', "6"), "2 lines 3, 7".to_string()))
        );
        assert_eq!(
            edited(SHOP, &[("Shop.", "Store.")], true).unwrap().1,
            "2 lines 2, 6"
        );
        assert!(
            edited(SHOP, &[("absent", "x")], true)
                .unwrap_err()
                .starts_with("src/A.luau pair 1: OLD not found")
        );
        let many = "local n = 0\n".repeat(LISTED_OCCURRENCES + 2);
        let refused = edited(&many, &[("local n", "local m")], false).unwrap_err();
        assert!(
            refused.contains("occurs 22 times, at lines 1, 2,")
                && refused.contains(", 20 and 2 more;"),
            "{refused}"
        );
    }

    #[test]
    fn an_absent_old_names_the_closest_place_with_the_files_text() {
        assert_eq!(
            edited(
                SHOP,
                &[("    local price = 5\n    return price", "x")],
                false
            ),
            Err("src/A.luau pair 1: OLD not found; closest is lines 3-4 (only indentation or trailing space differs). The file has:\n3-4:\n\tlocal price = 5\n\treturn price".to_string())
        );
        assert_eq!(
            edited(SHOP, &[("local price = 5 ", "x")], false),
            Err("src/A.luau pair 1: OLD not found; closest is line 3 (only indentation or trailing space differs). The file has:\n3:\n\tlocal price = 5".to_string())
        );
        assert_eq!(
            edited(
                SHOP,
                &[("\tlocal price = 6\n\treturn price\nend", "x")],
                false
            ),
            Err("src/A.luau pair 1: OLD not found; closest is lines 3-5 (2 of OLD's 3 lines match). The file has:\n3:\n\tlocal price = 5".to_string())
        );
        assert_eq!(
            edited(SHOP, &[("local cost = 5", "x")], false),
            Err("src/A.luau pair 1: OLD not found; closest is line 3. The file has:\n3:\n\tlocal price = 5".to_string())
        );
        assert_eq!(
            edited(SHOP, &[("task.wait(1)", "x")], false),
            Err("src/A.luau pair 1: OLD not found; nothing in the file is close to it".to_string())
        );
        assert_eq!(
            edited(SHOP, &[("--output-mode", "json")], false),
            Err("src/A.luau pair 1: OLD not found; nothing in the file is close to it; options other than -f, --all and --no-push go before FILE".to_string())
        );
    }

    #[test]
    fn a_long_closest_place_prints_only_its_first_lines() {
        let body = (0..SHOWN_LINES + 5)
            .map(|index| format!("\tlocal value{index} = {index}\n"))
            .collect::<String>();
        let file = format!("do\n{body}end\n");
        let refused = edited(&file, &[(&body.replace('\t', "  "), "x")], false).unwrap_err();
        let lines = refused.lines().collect::<Vec<_>>();
        assert_eq!(lines[1], format!("2-{}:", SHOWN_LINES + 1));
        assert_eq!(lines.len(), SHOWN_LINES + 3);
        assert_eq!(lines[SHOWN_LINES + 2], "5 more lines differ");
    }

    #[test]
    fn line_endings_do_not_matter_for_a_match_and_the_file_keeps_its_own() {
        let crlf = "local a = 1\r\nlocal b = 2\r\nreturn a + b\r\n";
        assert_eq!(
            edited(
                crlf,
                &[(
                    "local a = 1\nlocal b = 2",
                    "local a = 3\nlocal b = 4\nlocal c = 5"
                )],
                false
            ),
            Ok((
                "local a = 3\r\nlocal b = 4\r\nlocal c = 5\r\nreturn a + b\r\n".to_string(),
                "1 lines 1-3".to_string()
            ))
        );
        assert_eq!(
            edited(crlf, &[("local b = 2\n", "")], false),
            Ok((
                "local a = 1\r\nreturn a + b\r\n".to_string(),
                "1 line 2".to_string()
            ))
        );
        assert_eq!(
            edited(
                "local a = 1\nreturn a\n",
                &[("local a = 1\r\nreturn a", "local a = 2\r\nreturn a")],
                false
            ),
            Ok((
                "local a = 2\nreturn a\n".to_string(),
                "1 lines 1-2".to_string()
            ))
        );
        let mixed = "local one = 1\r\nlocal two = 2\nlocal three = 3\nreturn one\n";
        assert_eq!(
            edited(
                mixed,
                &[("local three = 3", "local three = 4\nlocal four = 4")],
                false
            ),
            Ok((
                "local one = 1\r\nlocal two = 2\nlocal three = 4\nlocal four = 4\nreturn one\n"
                    .to_string(),
                "1 lines 3-4".to_string()
            ))
        );
        assert_eq!(
            edited(crlf, &[("    local b = 2\nreturn a + b", "x")], false),
            Err("src/A.luau pair 1: OLD not found; closest is lines 2-3 (only indentation or trailing space differs). The file has:\n2:\nlocal b = 2".to_string())
        );
    }

    #[test]
    fn pairs_apply_in_order_to_the_text_the_earlier_ones_left() {
        assert_eq!(
            edited(
                SHOP,
                &[
                    ("local price = 5", "local total = 5"),
                    ("return price", "return total"),
                    ("local total = 5", "local total = 6\n\tlocal tax = 1"),
                    ("return 5", "return 4"),
                ],
                false
            ),
            Ok((
                SHOP.replace("local price = 5", "local total = 6\n\tlocal tax = 1")
                    .replace("return price", "return total")
                    .replace("return 5", "return 4"),
                "4 lines 3-5, 8".to_string()
            ))
        );
        assert_eq!(
            edited(
                SHOP,
                &[
                    ("local price = 5", "local cost = 5"),
                    ("price = 5", "price = 6")
                ],
                false
            ),
            Err("src/A.luau pair 2: OLD not found; an earlier pair of this call changed that text, and pairs apply in order".to_string())
        );
        assert_eq!(
            edited(
                SHOP,
                &[
                    ("local Shop = {}\n", "local Shop = {}\nlocal extra = 1\n"),
                    ("task.wait(1)", "x"),
                    ("return", "error"),
                ],
                false
            ),
            Err("src/A.luau pair 2: OLD not found; nothing in the file is close to it\nsrc/A.luau pair 3: OLD occurs 3 times, at lines 5, 8, 10 (line numbers as the earlier pairs leave them); give more of the text around the one you mean, or --all for every one".to_string())
        );
    }

    #[test]
    fn a_result_that_does_not_parse_is_refused_with_its_line() {
        assert_eq!(
            edited(SHOP, &[("\treturn price\nend", "\treturn price")], false),
            Err("src/A.luau: syntax error after the edits, line 5: Expected 'end' (to close 'function' at line 2), got 'function'\n5:\nfunction Shop.sell(player)".to_string())
        );
        let broken = "local a = 1\nlocal b =\n";
        assert_eq!(
            edited(broken, &[("local a = 1", "local a = 2")], false),
            Err("src/A.luau: syntax error after the edits, line 3: Expected identifier when parsing expression, got <eof> (the file did not parse before either: line 3: Expected identifier when parsing expression, got <eof>)".to_string())
        );
        assert_eq!(
            edited(broken, &[("local b =", "local b = 2")], false),
            Ok((
                "local a = 1\nlocal b = 2\n".to_string(),
                "1 line 2".to_string()
            ))
        );
    }

    fn parsed(args: &[&str]) -> Result<ScriptEditArgs, String> {
        let cli = Cli::try_parse_from(["rbx", "se"].into_iter().chain(args.iter().copied()))
            .map_err(|error| error.to_string())?;
        match cli.command {
            Commands::ScriptEdit(args) => Ok(args),
            _ => Err("not the se command".to_string()),
        }
    }

    type Listed = Vec<(String, Vec<(String, String)>)>;

    /// The files and pairs of a command line, with `--all` and `--no-push`.
    fn requested(args: &[&str]) -> Result<(Listed, bool, bool), String> {
        let request = edit_request(parsed(args)?).map_err(|error| error.to_string())?;
        let listed = request
            .targets
            .into_iter()
            .map(|target| {
                let pairs = target
                    .pairs
                    .into_iter()
                    .map(|pair| (pair.old, pair.new))
                    .collect();
                (target.file, pairs)
            })
            .collect();
        Ok((listed, request.all, request.no_push))
    }

    fn listed(files: &[(&str, &[(&str, &str)])]) -> Listed {
        files
            .iter()
            .map(|(file, pairs)| {
                let pairs = pairs
                    .iter()
                    .map(|(old, new)| ((*old).to_string(), (*new).to_string()))
                    .collect();
                ((*file).to_string(), pairs)
            })
            .collect()
    }

    #[test]
    fn the_command_line_gives_files_with_their_pairs_and_takes_text_that_starts_with_a_hyphen() {
        assert_eq!(
            requested(&["src/A.luau", "old", "new"]),
            Ok((listed(&[("src/A.luau", &[("old", "new")])]), false, false))
        );
        assert_eq!(
            requested(&[
                "src/A.luau",
                "-- old comment",
                "-- new\ncomment",
                "x",
                "",
                "-f",
                "ServerScriptService.B",
                "--all",
                "y",
                "-f",
                "--no-push",
                "--file",
                "src/C.luau",
                "z",
                "--all",
            ]),
            Ok((
                listed(&[
                    (
                        "src/A.luau",
                        &[("-- old comment", "-- new\ncomment"), ("x", "")]
                    ),
                    ("ServerScriptService.B", &[("y", "-f")]),
                    ("src/C.luau", &[("z", "--all")]),
                ]),
                true,
                true
            ))
        );
        assert_eq!(
            requested(&["--all", "--no-push", "src/A.luau", "a", "b"]),
            Ok((listed(&[("src/A.luau", &[("a", "b")])]), true, true))
        );
        assert_eq!(
            requested(&["src/A.luau", "--", "-fuel", "-fuel * 2"]),
            Ok((
                listed(&[("src/A.luau", &[("-fuel", "-fuel * 2")])]),
                false,
                false
            ))
        );
        let created = parsed(&["src/A.luau", "--new", "--no-push"]).unwrap();
        assert!(created.new && created.no_push && created.edits.is_empty());
    }

    #[test]
    fn a_command_line_without_whole_pairs_is_refused() {
        let refused = |args: &[&str]| requested(args).unwrap_err();
        assert_eq!(
            refused(&["src/A.luau", "a", "b", "c"]),
            "The last OLD of src/A.luau has no NEW: values come in OLD NEW pairs"
        );
        assert_eq!(
            refused(&["src/A.luau", "a", "b", "--yes"]),
            "The last OLD of src/A.luau has no NEW: values come in OLD NEW pairs; options other than -f, --all and --no-push go before FILE"
        );
        assert_eq!(
            refused(&["src/A.luau", "", "b"]),
            "Pair 1 of src/A.luau has an empty OLD"
        );
        assert_eq!(
            refused(&["src/A.luau"]),
            "src/A.luau has no OLD NEW pair (rbx se FILE OLD NEW; --new creates a script from stdin)"
        );
        assert_eq!(
            refused(&["src/A.luau", "a", "b", "-f", "src/B.luau"]),
            "src/B.luau has no OLD NEW pair (rbx se FILE OLD NEW; --new creates a script from stdin)"
        );
        assert_eq!(
            refused(&["src/A.luau", "a", "b", "-f"]),
            "-f needs a FILE after it"
        );
        assert!(
            refused(&["src/A.luau", "-f", "src/B.luau", "a", "b"])
                .starts_with("-f FILE comes after the pairs of the file before it")
        );
        assert!(refused(&["src/A.luau", "--new", "--all"]).contains("cannot be used with"));
        assert!(refused(&[]).contains("<FILE>"));
    }

    /// A project folder with script files under `src`.
    struct Project {
        root: PathBuf,
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct Ran {
        error: Option<String>,
        value: Value,
        stdout: String,
        /// The file names of each push, in order.
        pushes: Vec<Vec<String>>,
    }

    impl Project {
        fn new(tag: &str, files: &[(&str, &str)]) -> Self {
            let root = crate::tests::support::temp_dir(tag);
            fs::write(
                root.join("renium.project.jsonc"),
                "{\n  \"schemaVersion\": 1\n}\n",
            )
            .unwrap();
            fs::create_dir_all(root.join("src")).unwrap();
            for (name, text) in files {
                fs::write(root.join("src").join(name), text).unwrap();
            }
            Self { root }
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.root.join("src").join(name)).unwrap()
        }

        /// Runs `rbx se` in the project with `pushed` standing in for what
        /// the push to Studio returns.
        fn run(&self, mode: CaptureMode, args: &[&str], pushed: &dyn Fn() -> Result<Value>) -> Ran {
            let pushes = RefCell::new(Vec::new());
            let push = |_: &Path, target: &PushTarget| {
                let PushTarget::Files(paths) = target else {
                    bail!("an edit pushes files");
                };
                pushes.borrow_mut().push(
                    paths
                        .iter()
                        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                        .collect(),
                );
                pushed()
            };
            let project = self.root.join("renium.project.jsonc");
            let args = parsed(args).unwrap();
            let (result, value, stdout) = capture_output(mode, || {
                run(args, Some(&project), &mut std::io::empty(), &push)
            });
            Ran {
                error: result.err().map(|error| format!("{error:#}")),
                value: value.unwrap_or(Value::Null),
                stdout,
                pushes: pushes.into_inner(),
            }
        }

        fn text(&self, args: &[&str]) -> Ran {
            self.run(CaptureMode::Text, args, &|| {
                Ok(json!({ "ok": true, "sourceVerified": 1, "verified": true }))
            })
        }
    }

    const A: &str = "local a = 1\nreturn a\n";
    const B: &str = "local b = 1\nlocal c = 1\nreturn b + c\n";

    #[test]
    fn several_files_are_edited_checked_and_pushed_in_one_call() {
        let project = Project::new("se-files", &[("A.luau", A), ("B.luau", B)]);
        let args = [
            "src/A.luau",
            "local a = 1",
            "local a = 2",
            "-f",
            "src/B.luau",
            "local b = 1",
            "local b = 2",
            "return b + c",
            "return b - c",
            "-f",
            "src/A.luau",
            "return a",
            "return a + 1",
        ];
        let ran = project.text(&args);
        assert_eq!(ran.error, None);
        assert_eq!(
            ran.stdout,
            "src/A.luau: 2 edits, lines 1-2\nsrc/B.luau: 2 edits, lines 1, 3\npushed, verified\n"
        );
        assert_eq!(ran.pushes, [["A.luau", "B.luau"]]);
        assert_eq!(project.read("A.luau"), "local a = 2\nreturn a + 1\n");
        assert_eq!(
            project.read("B.luau"),
            "local b = 2\nlocal c = 1\nreturn b - c\n"
        );

        let again = Project::new("se-files-json", &[("A.luau", A), ("B.luau", B)]);
        let ran = again.run(CaptureMode::Json, &args, &|| {
            Ok(json!({ "ok": true, "sourceVerified": 2, "verified": true }))
        });
        assert_eq!(ran.error, None);
        assert_eq!(
            ran.value,
            json!({
                "ok": true,
                "files": [
                    { "file": "src/A.luau", "edits": 2, "lines": [[1, 2]] },
                    { "file": "src/B.luau", "edits": 2, "lines": [[1, 1], [3, 3]] },
                ],
                "push": { "pushed": true, "verified": true, "sourceVerified": 2 },
            })
        );
    }

    #[test]
    fn one_pair_that_does_not_apply_leaves_every_file_as_it_was() {
        let project = Project::new("se-nothing", &[("A.luau", A), ("B.luau", B)]);
        let ran = project.text(&[
            "src/A.luau",
            "local a = 1",
            "local a = 2",
            "-f",
            "src/B.luau",
            "local d = 1",
            "local d = 2",
        ]);
        assert_eq!(
            ran.error.as_deref(),
            Some("command reported a structured failure")
        );
        assert_eq!(
            ran.stdout,
            "src/B.luau pair 1: OLD not found; closest is line 1. The file has:\n1:\nlocal b = 1\nnothing written\n"
        );
        assert!(ran.pushes.is_empty());
        assert_eq!(project.read("A.luau"), A);
        assert_eq!(project.read("B.luau"), B);

        let ran = project.run(
            CaptureMode::Json,
            &[
                "src/A.luau",
                "return a",
                "return a +",
                "-f",
                "src/B.luau",
                "local",
                "x",
            ],
            &|| bail!("nothing is pushed"),
        );
        assert_eq!(
            ran.value,
            json!({
                "ok": false,
                "written": false,
                "failures": [
                    {
                        "file": "src/A.luau",
                        "line": 3,
                        "error": "Expected identifier when parsing expression, got <eof>",
                    },
                    {
                        "file": "src/B.luau",
                        "pair": 1,
                        "error": "OLD occurs 2 times",
                        "lines": [1, 2],
                    },
                ],
            })
        );
        assert!(ran.pushes.is_empty());
        assert_eq!(project.read("A.luau"), A);
        assert_eq!(project.read("B.luau"), B);

        let missing = project.text(&[
            "src/A.luau",
            "local a = 1",
            "local a = 2",
            "-f",
            "src/C.luau",
            "a",
            "b",
        ]);
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("Failed to resolve")),
            "{:?}",
            missing.error
        );
        assert_eq!(project.read("A.luau"), A);
    }

    #[test]
    fn a_syntax_error_after_the_edits_writes_nothing() {
        let project = Project::new("se-syntax", &[("A.luau", A)]);
        let ran = project.text(&["src/A.luau", "return a", "return a +"]);
        assert_eq!(
            ran.error.as_deref(),
            Some("command reported a structured failure")
        );
        assert_eq!(
            ran.stdout,
            "src/A.luau: syntax error after the edits, line 3: Expected identifier when parsing expression, got <eof>\nnothing written\n"
        );
        assert!(ran.pushes.is_empty());
        assert_eq!(project.read("A.luau"), A);
    }

    #[test]
    fn the_push_line_says_what_became_of_the_written_files() {
        let edit = ["src/A.luau", "local a = 1", "local a = 2"];
        let after = "local a = 2\nreturn a\n";
        let outcome = |tag: &str, args: &[&str], pushed: &dyn Fn() -> Result<Value>| {
            let project = Project::new(tag, &[("A.luau", A)]);
            let ran = project.run(CaptureMode::Text, args, pushed);
            (ran, project.read("A.luau"))
        };

        let (ran, text) = outcome("se-live", &edit, &|| {
            Ok(json!({ "ok": true, "pushed": false, "liveSync": true }))
        });
        assert_eq!(
            ran.stdout,
            "src/A.luau: 1 edit, line 1\nnot pushed: Live Sync delivers it\n"
        );
        assert_eq!((ran.error, text.as_str()), (None, after));

        let (ran, text) = outcome("se-no-studio", &edit, &|| {
            bail!("No Studio runtime connected to this project. Studio is not running")
        });
        assert_eq!(
            ran.stdout,
            "src/A.luau: 1 edit, line 1\nnot pushed: no Studio is connected\n"
        );
        assert_eq!((ran.error, text.as_str()), (None, after));

        let (ran, text) = outcome("se-unchanged", &edit, &|| {
            Ok(json!({ "ok": true, "unchanged": true }))
        });
        assert_eq!(
            ran.stdout,
            "src/A.luau: 1 edit, line 1\nnot pushed: Studio already has this text\n"
        );
        assert_eq!((ran.error, text.as_str()), (None, after));

        let (ran, text) = outcome("se-failed", &edit, &|| {
            bail!("Source verification failed for A")
        });
        assert_eq!(
            ran.error.as_deref(),
            Some(
                "The files were written but the push failed; `rbx ps FILES` pushes them: Source verification failed for A"
            )
        );
        assert_eq!((ran.stdout.as_str(), text.as_str()), ("", after));

        let (ran, text) = outcome(
            "se-no-push",
            &["src/A.luau", "local a = 1", "local a = 2", "--no-push"],
            &|| bail!("nothing is pushed"),
        );
        assert_eq!(
            ran.stdout,
            "src/A.luau: 1 edit, line 1\nnot pushed (--no-push)\n"
        );
        assert!(ran.pushes.is_empty() && ran.error.is_none());
        assert_eq!(text, after);

        let (ran, text) = outcome(
            "se-same",
            &["src/A.luau", "local a = 1", "local a = 1"],
            &|| bail!("nothing is pushed"),
        );
        assert_eq!(ran.stdout, "src/A.luau: unchanged\nnothing to push\n");
        assert!(ran.pushes.is_empty() && ran.error.is_none());
        assert_eq!(text, A);

        let packages = push_outcome(Ok(json!({
            "ok": true,
            "verified": false,
            "autoDesyncedPackages": ["Shared"],
        })))
        .unwrap();
        assert_eq!(
            push_text(&packages),
            "pushed, not verified; autoDesyncedPackages: [\"Shared\"]"
        );
    }

    #[test]
    fn a_crlf_file_keeps_its_line_endings_on_disk() {
        let project = Project::new("se-crlf", &[("A.luau", "local a = 1\r\nreturn a\r\n")]);
        let ran = project.text(&[
            "src/A.luau",
            "local a = 1\nreturn a",
            "local a = 2\nlocal b = a\nreturn b",
        ]);
        assert_eq!(
            ran.stdout,
            "src/A.luau: 1 edit, lines 1-3\npushed, verified\n"
        );
        assert_eq!(
            project.read("A.luau"),
            "local a = 2\r\nlocal b = a\r\nreturn b\r\n"
        );
    }
}
