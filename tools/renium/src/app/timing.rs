use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::app::output::{global_log_enabled, log_global};

static QUIET_TIMINGS: AtomicBool = AtomicBool::new(false);
const TIMING_LOG_LEVEL: u8 = 4;
static TRACE_CLOCK: OnceLock<(Instant, u64)> = OnceLock::new();
static NEXT_TRACE_THREAD: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static TRACE_THREAD: u64 = NEXT_TRACE_THREAD.fetch_add(1, Ordering::Relaxed);
    static TRACE_CONTEXT: Cell<Option<TraceContext>> = const { Cell::new(None) };
    static LAST_TRACE_WRITE: Cell<Option<TraceWrite>> = const { Cell::new(None) };
}

#[derive(Clone, Copy)]
struct TraceWrite {
    started: Instant,
    finished: Instant,
    context: Option<TraceContext>,
}

#[derive(Serialize)]
struct OutputTiming {
    ts: u64,
    dur: u64,
    args: Option<TraceContext>,
}

fn previous_output_timing() -> Option<OutputTiming> {
    let previous = LAST_TRACE_WRITE.get()?;
    let (anchor, anchor_us) = TRACE_CLOCK.get()?;
    let ts = trace_timestamp(previous.started, *anchor, *anchor_us);
    Some(OutputTiming {
        ts,
        dur: trace_timestamp(previous.finished, *anchor, *anchor_us).saturating_sub(ts),
        args: previous.context,
    })
}

fn record_trace_write(started: Instant) {
    LAST_TRACE_WRITE.set(Some(TraceWrite {
        started,
        finished: Instant::now(),
        context: TRACE_CONTEXT.get(),
    }));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TraceContext {
    pub request_id: u64,
    pub context_id: Option<u64>,
}

pub(crate) fn trace_context() -> Option<TraceContext> {
    global_log_enabled(5).then(|| TRACE_CONTEXT.get()).flatten()
}

// Thread-local ownership must be explicitly copied at worker boundaries. The
// guard is not Send: restoring it on a different thread would corrupt attribution.
pub(crate) struct TraceContextGuard {
    previous: Option<TraceContext>,
    _thread_bound: PhantomData<*mut ()>,
}

pub(crate) fn enter_trace_context(context: Option<TraceContext>) -> TraceContextGuard {
    TraceContextGuard {
        previous: TRACE_CONTEXT.replace(context),
        _thread_bound: PhantomData,
    }
}

impl Drop for TraceContextGuard {
    fn drop(&mut self) {
        TRACE_CONTEXT.set(self.previous);
    }
}

// Chrome trace complete events use wall durations, not CPU time. Timestamped
// intervals let analysis subtract nested/overlapping work rather than sum it.
#[derive(Serialize)]
struct TimingEvent<'a> {
    name: &'a str,
    cat: &'a str,
    ph: &'static str,
    ts: u64,
    dur: u64,
    pid: u32,
    tid: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<TraceContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<OutputTiming>,
}

fn trace_timestamp(started: Instant, anchor: Instant, anchor_us: u64) -> u64 {
    if started >= anchor {
        anchor_us.saturating_add(started.duration_since(anchor).as_micros() as u64)
    } else {
        anchor_us.saturating_sub(anchor.duration_since(started).as_micros() as u64)
    }
}

#[cfg(target_os = "windows")]
#[derive(Serialize)]
pub(crate) struct TraceRange {
    ts: u64,
    dur: u64,
}

#[cfg(target_os = "windows")]
pub(crate) fn trace_range(started: Instant, finished: Instant) -> Option<TraceRange> {
    if !global_log_enabled(5) {
        return None;
    }
    let (anchor, anchor_us) = trace_clock();
    let ts = trace_timestamp(started, *anchor, *anchor_us);
    Some(TraceRange {
        ts,
        dur: trace_timestamp(finished, *anchor, *anchor_us).saturating_sub(ts),
    })
}

fn trace_clock() -> &'static (Instant, u64) {
    TRACE_CLOCK.get_or_init(|| {
        (
            Instant::now(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
        )
    })
}

pub(crate) fn trace_timing(category: &str, label: &str, started: Instant) {
    trace_interval(category, label, started, Instant::now());
}

fn trace_interval(category: &str, label: &str, started: Instant, finished: Instant) {
    if !global_log_enabled(5) {
        return;
    }
    let write_started = Instant::now();
    let (anchor, anchor_us) = trace_clock();
    let timestamp = trace_timestamp(started, *anchor, *anchor_us);
    let event = TimingEvent {
        name: label,
        cat: category,
        ph: "X",
        ts: timestamp,
        // Quantize endpoints on the same grid. Separately truncating duration
        // invents one-microsecond gaps between otherwise adjacent stages.
        dur: trace_timestamp(finished, *anchor, *anchor_us).saturating_sub(timestamp),
        pid: std::process::id(),
        tid: TRACE_THREAD.with(|id| *id),
        args: TRACE_CONTEXT.get(),
        output: previous_output_timing(),
    };
    // Only primitive fields are serialized; no source, payload, auth token or
    // user property value is included. Output stays in opt-in diagnostic logs.
    if let Ok(encoded) = serde_json::to_string(&event) {
        log_global(5, format_args!("[renium] span {encoded}"));
    }
    record_trace_write(write_started);
}

// A sequential partition, not a collection of overlapping stopwatches. Buffer
// output until the operation ends so logging is not charged to the next stage.
pub(crate) struct TraceStages {
    category: &'static str,
    stages: Option<Vec<(&'static str, Instant)>>,
}

pub(crate) fn trace_stages(category: &'static str, first: &'static str) -> TraceStages {
    TraceStages {
        category,
        stages: global_log_enabled(5).then(|| vec![(first, Instant::now())]),
    }
}

impl TraceStages {
    pub(crate) fn next(&mut self, name: &'static str) {
        if let Some(stages) = &mut self.stages {
            stages.push((name, Instant::now()));
        }
    }
}

impl Drop for TraceStages {
    fn drop(&mut self) {
        if let Some(stages) = &self.stages {
            let finished = Instant::now();
            for (index, (name, started)) in stages.iter().enumerate() {
                let end = stages.get(index + 1).map_or(finished, |(_, time)| *time);
                trace_interval(self.category, name, *started, end);
            }
            trace_timing(
                "trace.output",
                "write buffered stage timing records",
                finished,
            );
        }
    }
}

pub(crate) fn trace_profile(label: &str, profile: &serde_json::Value) {
    if !global_log_enabled(5) {
        return;
    }
    let started = Instant::now();
    let context = TRACE_CONTEXT.get();
    log_global(
        5,
        format_args!(
            "[renium] profile {}",
            serde_json::json!({
                "name": label, "pid": std::process::id(), "tid": TRACE_THREAD.with(|id| *id),
                "context": context, "data": profile, "output": previous_output_timing(),
            })
        ),
    );
    record_trace_write(started);
}

pub(crate) struct TimingScope<'a> {
    category: &'a str,
    label: &'a str,
    started: Instant,
}

pub(crate) fn trace_scope<'a>(category: &'a str, label: &'a str) -> Option<TimingScope<'a>> {
    global_log_enabled(5).then(|| TimingScope {
        category,
        label,
        started: Instant::now(),
    })
}

impl Drop for TimingScope<'_> {
    fn drop(&mut self) {
        trace_timing(self.category, self.label, self.started);
    }
}

pub(crate) fn current_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

pub(crate) fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

pub(crate) fn log_timing(label: &str, started: Instant) {
    trace_timing("step", label, started);
    if quiet_timings() && global_log_enabled(5) {
        log_global(
            5,
            format_args!("[renium] timing: {label} took {:.1}ms", elapsed_ms(started)),
        );
        return;
    }
    if quiet_timings() || !global_log_enabled(TIMING_LOG_LEVEL) {
        return;
    }
    println!("[renium] timing: {label} took {:.1}ms", elapsed_ms(started));
}

pub(crate) fn log_timing_ms(label: &str, elapsed_ms: f64) {
    if !global_log_enabled(TIMING_LOG_LEVEL) {
        return;
    }
    if quiet_timings()
        && !matches!(
            label,
            "cli start to bridge listen"
                | "bridge listen to all channels connected"
                | "bridge bind/listen setup"
                | "all channels connected to bridge info"
                | "bridge info to property schema ready"
                | "property schema ready to first service export"
                | "first service export to last service export"
                | "last service export to dispatcher drain start"
                | "direct import dispatcher drain"
                | "write generated project"
                | "sourcemap finalize"
                | "full export-snapshots run"
        )
    {
        return;
    }
    println!("[renium] timing: {label} took {elapsed_ms:.1}ms");
}

pub(crate) fn quiet_timings() -> bool {
    QUIET_TIMINGS.load(Ordering::Relaxed)
}

pub(crate) fn set_quiet_timings(quiet: bool) {
    QUIET_TIMINGS.store(quiet, Ordering::Relaxed);
}

pub(crate) fn verbose_timing_logs() -> bool {
    global_log_enabled(5) || (!quiet_timings() && global_log_enabled(TIMING_LOG_LEVEL))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn trace_events_preserve_timestamp_origin_and_escape_labels() {
        let anchor = Instant::now();
        assert_eq!(
            trace_timestamp(anchor - Duration::from_micros(20), anchor, 100),
            80
        );
        assert_eq!(
            trace_timestamp(anchor + Duration::from_micros(30), anchor, 100),
            130
        );
        let event = TimingEvent {
            name: "quoted \"stage\"",
            cat: "bridge.wait",
            ph: "X",
            ts: 80,
            dur: 50,
            pid: 1,
            tid: 2,
            args: Some(TraceContext {
                request_id: 9,
                context_id: Some(3),
            }),
            output: None,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(json["name"], "quoted \"stage\"");
        assert_eq!(json["ts"], 80);
        assert_eq!(json["dur"], 50);
        assert_eq!(json["tid"], 2);
        assert_eq!(json["args"]["request_id"], 9);
    }

    #[test]
    fn trace_context_is_nested_and_explicitly_transferred_to_workers() {
        let parent = Some(TraceContext {
            request_id: 1,
            context_id: Some(2),
        });
        let child = Some(TraceContext {
            request_id: 3,
            context_id: Some(4),
        });
        let _parent = enter_trace_context(parent);
        {
            let _child = enter_trace_context(child);
            assert_eq!(TRACE_CONTEXT.get(), child);
        }
        assert_eq!(TRACE_CONTEXT.get(), parent);
        std::thread::spawn(move || {
            assert_eq!(TRACE_CONTEXT.get(), None);
            {
                let _worker = enter_trace_context(parent);
                assert_eq!(TRACE_CONTEXT.get(), parent);
            }
            assert_eq!(TRACE_CONTEXT.get(), None);
        })
        .join()
        .unwrap();
        assert_eq!(TRACE_CONTEXT.get(), parent);
    }
}
