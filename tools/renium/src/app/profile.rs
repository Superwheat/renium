//! Where the time of one command went.
//!
//! Every timed step (`log_timing`, a `span`) is kept with its thread and its
//! start and end. When the command ends, the steps are laid out as a tree by
//! which step ran inside which, and each step that has steps inside it says
//! how much of its own time none of them covers. That remainder is what is
//! still unmeasured: a profile is complete when no remainder matters.
//!
//! Only kept while the trace log is on.

use std::fmt::Write as _;
use std::sync::Mutex;
use std::thread::ThreadId;
use std::time::Instant;

use crate::app::output::global_log_enabled;
use crate::system::LockRecover;

const PROFILE_LOG_LEVEL: u8 = 4;
const MAX_RECORDS: usize = 200_000;
/// Clock reads on two threads do not order perfectly.
const SLACK_NANOS: u64 = 20_000;

#[derive(Clone)]
struct Record {
    label: String,
    thread: ThreadId,
    started: Instant,
    ended: Instant,
}

struct ProfileState {
    records: Vec<Record>,
    requests: Vec<Instant>,
}

static STATE: Mutex<ProfileState> = Mutex::new(ProfileState {
    records: Vec::new(),
    requests: Vec::new(),
});

pub(crate) struct Request(Option<Instant>);

/// Keep a shorter concurrent request from consuming an ongoing request's steps.
pub(crate) fn request(started: Instant) -> Request {
    if enabled() {
        STATE.lock_recover().requests.push(started);
        Request(Some(started))
    } else {
        Request(None)
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        let Some(started) = self.0 else { return };
        let mut state = STATE.lock_recover();
        if let Some(index) = state.requests.iter().position(|time| *time == started) {
            state.requests.swap_remove(index);
        }
        let earliest = state.requests.iter().copied().min();
        state
            .records
            .retain(|record| earliest.is_some_and(|start| record.started >= start));
    }
}

pub(crate) fn enabled() -> bool {
    global_log_enabled(PROFILE_LOG_LEVEL)
}

/// A step that began at `started` and ends now.
pub(crate) fn record(label: &str, started: Instant) {
    record_between(label, started, Instant::now());
}

pub(crate) fn record_between(label: &str, started: Instant, ended: Instant) {
    if !enabled() {
        return;
    }
    let label = match committed_megabytes() {
        Some(megabytes) => format!("{label} [{megabytes} MB held]"),
        None => label.to_string(),
    };
    let mut state = STATE.lock_recover();
    if state.records.len() < MAX_RECORDS {
        state.records.push(Record {
            label,
            thread: std::thread::current().id(),
            started,
            ended,
        });
    }
}

/// A build with the `count-live-memory` feature counts every allocation, so a
/// profile can say how many bytes are in use, apart from what the allocator
/// has freed and not yet given back. Counting costs time: such a build is for
/// finding what holds memory, never for timing.
#[cfg(feature = "count-live-memory")]
pub(crate) struct CountingAllocator;

#[cfg(feature = "count-live-memory")]
static LIVE_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "count-live-memory")]
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        LIVE_BYTES.fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
        unsafe { mimalloc::MiMalloc.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        LIVE_BYTES.fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
        unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
        unsafe { mimalloc::MiMalloc.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        LIVE_BYTES.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
        LIVE_BYTES.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
        unsafe { mimalloc::MiMalloc.realloc(pointer, layout, size) }
    }
}

/// The memory the process holds as a step ends, when `RENIUM_PROFILE_MEMORY`
/// is set: each step of the profile then says where the memory stood, which
/// shows the step that grows it. A `count-live-memory` build gives the bytes
/// in use; any other build what the allocator has committed.
fn committed_megabytes() -> Option<usize> {
    static WANTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*WANTED.get_or_init(|| std::env::var_os("RENIUM_PROFILE_MEMORY").is_some()) {
        return None;
    }
    #[cfg(feature = "count-live-memory")]
    return Some(LIVE_BYTES.load(std::sync::atomic::Ordering::Relaxed) >> 20);
    #[cfg(not(feature = "count-live-memory"))]
    committed_by_the_allocator()
}

#[cfg(not(feature = "count-live-memory"))]
fn committed_by_the_allocator() -> Option<usize> {
    let mut committed = 0usize;
    let mut unused = [0usize; 7];
    let [
        elapsed,
        user,
        system,
        resident,
        peak_resident,
        peak_committed,
        faults,
    ] = &mut unused;
    unsafe {
        libmimalloc_sys::mi_process_info(
            elapsed,
            user,
            system,
            resident,
            peak_resident,
            &mut committed,
            peak_committed,
            faults,
        );
    }
    Some(committed >> 20)
}

/// A step that lasts until the value is dropped.
pub(crate) struct Span {
    label: Option<String>,
    started: Instant,
}

pub(crate) fn span(label: &str) -> Span {
    Span {
        label: enabled().then(|| label.to_string()),
        started: Instant::now(),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(label) = self.label.take() {
            record(&label, self.started);
        }
    }
}

struct Node {
    label: String,
    thread: ThreadId,
    start: u64,
    end: u64,
    children: Vec<usize>,
}

/// The tree of every step recorded since `started`, as text; `None` when
/// nothing was recorded.
pub(crate) fn report(name: &str, started: Instant) -> Option<String> {
    if !enabled() {
        return None;
    }
    let ended = Instant::now();
    let taken = take_records(&mut STATE.lock_recover(), started, ended);
    if taken.is_empty() {
        return None;
    }
    Some(render(name, started, ended, taken))
}

fn take_records(state: &mut ProfileState, started: Instant, ended: Instant) -> Vec<Record> {
    let other = state
        .requests
        .iter()
        .copied()
        .filter(|time| *time != started)
        .min();
    let mut taken = Vec::new();
    let mut kept = Vec::new();
    for record in state.records.drain(..) {
        let retain = record.ended > ended || other.is_some_and(|time| record.started >= time);
        if record.started >= started && record.ended <= ended {
            if retain {
                taken.push(record.clone());
            } else {
                taken.push(record);
                continue;
            }
        }
        if retain {
            kept.push(record);
        }
    }
    state.records = kept;
    taken
}

fn render(name: &str, started: Instant, ended: Instant, mut taken: Vec<Record>) -> String {
    let nanos = |instant: Instant| instant.saturating_duration_since(started).as_nanos() as u64;
    let mut nodes = vec![Node {
        label: name.to_string(),
        thread: std::thread::current().id(),
        start: 0,
        end: nanos(ended),
        children: Vec::new(),
    }];
    taken.sort_by_key(|record| {
        (
            nanos(record.started),
            std::cmp::Reverse(nanos(record.ended)),
        )
    });
    // The steps still open on each thread, outermost first.
    let mut open: Vec<(ThreadId, Vec<usize>)> = Vec::new();
    for record in taken {
        let (start, end) = (nanos(record.started), nanos(record.ended));
        for (_, stack) in &mut open {
            while stack
                .last()
                .is_some_and(|&top| nodes[top].end + SLACK_NANOS < start)
            {
                stack.pop();
            }
        }
        let contains =
            |node: &Node| node.start <= start + SLACK_NANOS && end <= node.end + SLACK_NANOS;
        let own = open
            .iter()
            .find(|(thread, _)| *thread == record.thread)
            .and_then(|(_, stack)| {
                stack
                    .iter()
                    .rev()
                    .copied()
                    .find(|&index| contains(&nodes[index]))
            });
        // A step on a worker thread belongs to the shortest step that lasted
        // through it and names the same subject ("Workspace: ..."); failing
        // that, to the step of the command's own thread that waited for it.
        // Another worker's step that merely ran at the same time is not its
        // parent.
        let subject = step_subject(&record.label);
        let lasting = |accept: &dyn Fn(&Node) -> bool| {
            open.iter()
                .flat_map(|(_, stack)| stack.iter().copied())
                .filter(|&index| contains(&nodes[index]) && accept(&nodes[index]))
                .min_by_key(|&index| nodes[index].end - nodes[index].start)
        };
        let parent = own
            .or_else(|| {
                subject.and_then(|subject| {
                    lasting(&|node: &Node| step_subject(&node.label) == Some(subject))
                })
            })
            .or_else(|| lasting(&|node: &Node| node.thread == nodes[0].thread))
            .unwrap_or(0);
        let index = nodes.len();
        nodes.push(Node {
            label: record.label,
            thread: record.thread,
            start,
            end,
            children: Vec::new(),
        });
        nodes[parent].children.push(index);
        match open.iter_mut().find(|(thread, _)| *thread == record.thread) {
            Some((_, stack)) => stack.push(index),
            None => open.push((record.thread, vec![index])),
        }
    }
    let total = nodes[0].end.max(1);
    let mut text = String::new();
    write_node(&nodes, 0, None, 0, total, &mut text);
    text
}

/// What a step is about, when its label names it first: "Workspace" in
/// "Workspace: write settings file".
fn step_subject(label: &str) -> Option<&str> {
    label.split_once(": ").map(|(subject, _)| subject)
}

/// How much of a step's time none of the steps inside it covers.
fn uncovered(nodes: &[Node], index: usize) -> u64 {
    let node = &nodes[index];
    let mut spans = node
        .children
        .iter()
        .map(|&child| {
            (
                nodes[child].start.max(node.start),
                nodes[child].end.min(node.end),
            )
        })
        .filter(|(start, end)| end > start)
        .collect::<Vec<_>>();
    spans.sort_unstable();
    let mut covered = 0;
    let mut reach = node.start;
    for (start, end) in spans {
        if end > reach {
            covered += end - start.max(reach);
            reach = end;
        }
    }
    (node.end - node.start).saturating_sub(covered)
}

fn write_node(
    nodes: &[Node],
    index: usize,
    parent: Option<usize>,
    depth: usize,
    total: u64,
    text: &mut String,
) {
    let node = &nodes[index];
    let millis = |nanos: u64| nanos as f64 / 1_000_000.0;
    let duration = node.end - node.start;
    let _ = write!(
        text,
        "{}{:>9.1}ms {}",
        "  ".repeat(depth),
        millis(duration),
        node.label
    );
    if depth == 0 {
        text.insert_str(0, "[renium] profile\n");
    }
    if !node.children.is_empty() {
        let rest = uncovered(nodes, index);
        let summed = node
            .children
            .iter()
            .map(|&child| nodes[child].end - nodes[child].start)
            .sum::<u64>();
        let _ = write!(text, "  [unmeasured {:.1}ms", millis(rest));
        if summed > duration + duration / 10 {
            let _ = write!(text, ", in parallel {:.1}ms", millis(summed));
        }
        text.push(']');
    }
    if parent.is_some_and(|parent| nodes[parent].thread != node.thread) {
        text.push_str("  (other thread)");
    }
    text.push('\n');
    // Keep small-step summaries, but never hide a step of twenty milliseconds
    // merely because the enclosing command is long.
    let floor = (total / 200).clamp(1_000_000, 20_000_000);
    let (mut hidden, mut hidden_nanos) = (0, 0);
    for &child in &node.children {
        let child_duration = nodes[child].end - nodes[child].start;
        if child_duration >= floor {
            write_node(nodes, child, Some(index), depth + 1, total, text);
        } else {
            hidden += 1;
            hidden_nanos += child_duration;
        }
    }
    if hidden > 0 {
        let _ = writeln!(
            text,
            "{}{:>9.1}ms in {hidden} smaller steps",
            "  ".repeat(depth + 1),
            millis(hidden_nanos)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn node(label: &str, thread: ThreadId, start: u64, end: u64) -> Node {
        Node {
            label: label.to_string(),
            thread,
            start,
            end,
            children: Vec::new(),
        }
    }

    #[test]
    fn a_short_concurrent_request_cannot_discard_the_ongoing_import_timeline() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let record = |label: &str, from, to| Record {
            label: label.into(),
            thread: std::thread::current().id(),
            started: at(from),
            ended: at(to),
        };
        let mut state = ProfileState {
            records: vec![record("load", 1, 5), record("settle", 12, 18)],
            requests: vec![start, at(10)],
        };
        assert!(take_records(&mut state, at(10), at(11)).is_empty());
        assert_eq!(state.records.len(), 2);
        state.requests.pop();
        let imported = take_records(&mut state, start, at(20));
        assert_eq!(
            imported
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["load", "settle"]
        );
        assert!(state.records.is_empty());
    }

    #[test]
    fn long_commands_still_show_each_twenty_millisecond_step() {
        let start = Instant::now();
        let output = render(
            "command",
            start,
            start + Duration::from_secs(30),
            vec![Record {
                label: "transport write".into(),
                thread: std::thread::current().id(),
                started: start,
                ended: start + Duration::from_millis(25),
            }],
        );
        assert!(output.contains("25.0ms transport write"));
    }

    #[test]
    fn the_time_no_inner_step_covers_counts_overlaps_once() {
        let thread = std::thread::current().id();
        let mut nodes = vec![
            node("outer", thread, 0, 100),
            node("a", thread, 10, 40),
            node("b", thread, 30, 60),
            node("c", thread, 80, 90),
        ];
        nodes[0].children = vec![1, 2, 3];
        assert_eq!(uncovered(&nodes, 0), 100 - 50 - 10);
    }

    #[test]
    fn steps_nest_by_the_time_they_ran_in_and_a_worker_step_joins_what_waited_for_it() {
        let started = Instant::now();
        let at = |millis: u64| started + Duration::from_millis(millis);
        let other_thread = || {
            std::thread::spawn(|| std::thread::current().id())
                .join()
                .unwrap()
        };
        let (worker, second_worker) = (other_thread(), other_thread());
        let own = std::thread::current().id();
        let record = |label: &str, thread, from, to| Record {
            label: label.to_string(),
            thread,
            started: at(from),
            ended: at(to),
        };
        let text = render(
            "command",
            started,
            at(100),
            vec![
                record("inner", own, 20, 30),
                record("worker step", worker, 40, 70),
                record("outer", own, 10, 90),
                record("wait for the worker", own, 35, 80),
                record("tiny", own, 12, 12),
                record("Lighting: all of it", second_worker, 36, 79),
            ],
        );
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "[renium] profile");
        assert!(lines[1].contains("100.0ms command") && lines[1].contains("[unmeasured 20.0ms]"));
        assert!(lines[2].starts_with("  ") && lines[2].contains("80.0ms outer"));
        assert!(lines[2].contains("[unmeasured 25.0ms]"), "{}", lines[2]);
        assert!(lines[3].starts_with("    ") && lines[3].contains("10.0ms inner"));
        assert!(lines[4].contains("45.0ms wait for the worker"));
        assert!(lines[4].contains("[unmeasured 2.0ms, in parallel 73.0ms]"));
        // The other worker's step lasted through the worker step and is not
        // its parent: both hang under the step that waited.
        assert!(lines[5].starts_with("      ") && lines[5].contains("43.0ms Lighting: all of it"));
        assert!(lines[6].starts_with("      ") && lines[6].contains("30.0ms worker step"));
        assert!(lines[6].ends_with("(other thread)"));
        assert!(lines[7].contains("in 1 smaller steps"));
    }
}
