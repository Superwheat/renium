use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::app::output::global_log_enabled;

static QUIET_TIMINGS: AtomicBool = AtomicBool::new(false);

pub(crate) fn current_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

pub(crate) fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

pub(crate) fn log_timing(label: &str, started: Instant) {
    if quiet_timings() || !global_log_enabled(3) {
        return;
    }
    println!("[renium] timing: {label} took {:.1}ms", elapsed_ms(started));
}

pub(crate) fn log_timing_ms(label: &str, elapsed_ms: f64) {
    if !global_log_enabled(3) {
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
    !quiet_timings() && global_log_enabled(3)
}
