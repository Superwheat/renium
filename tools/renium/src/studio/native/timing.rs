//! Remote measurements are diagnostics, never evidence that a capture is valid.
use std::time::{Duration, Instant};

use crate::app::profile;

/// Luau has no shared epoch with Rust. Preserve measured durations and nesting,
/// placed at receipt; the remaining round trip bounds the placement uncertainty.
pub(crate) fn record_reply(
    method: &str,
    started: Instant,
    received: Instant,
    value: &serde_json::Value,
) {
    if !profile::enabled() {
        return;
    }
    let Some((origin, elapsed, spans)) = reply_intervals(started, received, value) else {
        return;
    };
    let uncertainty = origin.saturating_duration_since(started);
    profile::record_between(
        &format!(
            "{method}: transport/scheduling (plugin placement uncertainty {:.2}ms)",
            uncertainty.as_secs_f64() * 1000.0
        ),
        started,
        origin,
    );
    profile::record_between(
        &format!("{method}: Studio response work"),
        origin,
        origin + elapsed,
    );
    for (label, from, to) in spans {
        profile::record_between(&format!("{method}: {label}"), origin + from, origin + to);
    }
}

type ReplyIntervals = (Instant, Duration, Vec<(String, Duration, Duration)>);

fn reply_intervals(
    started: Instant,
    received: Instant,
    value: &serde_json::Value,
) -> Option<ReplyIntervals> {
    if value["version"] != 1 {
        return None;
    }
    let elapsed_ms = value["elapsedMs"].as_f64()?;
    if !elapsed_ms.is_finite()
        || elapsed_ms < 0.0
        || elapsed_ms > received.saturating_duration_since(started).as_secs_f64() * 1000.0
    {
        return None;
    }
    let elapsed = Duration::from_secs_f64(elapsed_ms / 1000.0);
    let spans = value["spans"].as_array()?;
    if spans.len() > 4096 {
        return None;
    }
    let spans = spans
        .iter()
        .map(|span| {
            let label = span[0].as_str()?;
            let (from, to) = (span[1].as_f64()?, span[2].as_f64()?);
            if label.len() > 512
                || !from.is_finite()
                || !to.is_finite()
                || from < 0.0
                || to < from
                || to > elapsed_ms
            {
                return None;
            }
            let mut label = label.to_owned();
            for key in [
                "services",
                "instances",
                "bytes",
                "calls",
                "arrived",
                "cacheHit",
            ] {
                let count = &span[3][key];
                if count.is_u64() || count.is_boolean() {
                    label.push_str(&format!(" {key}={count}"));
                }
            }
            Some((
                label,
                Duration::from_secs_f64(from / 1000.0),
                Duration::from_secs_f64(to / 1000.0),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    Some((received - elapsed, elapsed, spans))
}

#[cfg(windows)]
pub(crate) struct CounterAnchor {
    instant: Instant,
    ticks: u64,
    frequency: u64,
}

#[cfg(windows)]
impl CounterAnchor {
    pub(crate) fn now() -> Self {
        use windows_sys::Win32::System::Performance::{
            QueryPerformanceCounter, QueryPerformanceFrequency,
        };
        let mut ticks = 0;
        let mut frequency = 0;
        let instant = Instant::now();
        unsafe {
            QueryPerformanceCounter(&mut ticks);
            QueryPerformanceFrequency(&mut frequency);
        }
        Self {
            instant,
            ticks: ticks as u64,
            frequency: frequency as u64,
        }
    }

    pub(crate) fn record(&self, label: &str, frequency: u64, ticks: &[u64], names: &[&str]) {
        let Some(intervals) = counter_intervals(self.ticks, self.frequency, frequency, ticks)
        else {
            crate::app::output::log_global(
                4,
                format_args!("[renium] {label}: invalid helper timing clock; timing omitted"),
            );
            return;
        };
        if intervals.len() != names.len() {
            return;
        }
        for ((start, end), name) in intervals.into_iter().zip(names) {
            profile::record_between(
                &format!("{label}: {name}"),
                self.instant + start,
                self.instant + end,
            );
        }
    }

    /// The injected thread starts after dispatch and exits after its last tick.
    /// Those OS handoffs belong to transport, not to the engine serializer.
    pub(crate) fn record_transport(
        &self,
        label: &str,
        frequency: u64,
        ticks: &[u64],
        returned: Instant,
    ) {
        let Some((start, end)) = counter_transport_bounds(
            self.ticks,
            self.frequency,
            frequency,
            ticks,
            returned.saturating_duration_since(self.instant),
        ) else {
            return;
        };
        profile::record_between(
            &format!("{label}: remote thread launch"),
            self.instant,
            self.instant + start,
        );
        profile::record_between(
            &format!("{label}: remote thread completion"),
            self.instant + end,
            returned,
        );
    }
}

#[cfg(any(windows, test))]
fn counter_transport_bounds(
    anchor: u64,
    frequency: u64,
    remote_frequency: u64,
    ticks: &[u64],
    elapsed: Duration,
) -> Option<(Duration, Duration)> {
    let intervals = counter_intervals(anchor, frequency, remote_frequency, ticks)?;
    let bounds = (intervals.first()?.0, intervals.last()?.1);
    (bounds.1 <= elapsed).then_some(bounds)
}

#[cfg(any(windows, test))]
fn counter_intervals(
    anchor: u64,
    frequency: u64,
    remote_frequency: u64,
    ticks: &[u64],
) -> Option<Vec<(Duration, Duration)>> {
    if frequency == 0
        || remote_frequency != frequency
        || ticks.len() < 2
        || ticks[0] < anchor
        || ticks.windows(2).any(|p| p[0] > p[1])
        || ticks.last()?.saturating_sub(anchor) / frequency > 120
    {
        return None;
    }
    let elapsed = |tick| Duration::from_secs_f64((tick - anchor) as f64 / frequency as f64);
    Some(
        ticks
            .windows(2)
            .map(|p| (elapsed(p[0]), elapsed(p[1])))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_timing_preserves_nested_measurements_and_bounds_clock_uncertainty() {
        let start = Instant::now();
        let end = start + Duration::from_millis(10);
        let value = serde_json::json!({"version":1,"elapsedMs":8,"spans":[["handler",1,8],["overlay",2,7,{"instances":20,"bytes":400,"calls":2,"futureField":1}],["identities",3,4,{"instances":9,"calls":1,"arrived":6}]]});
        let (origin, elapsed, spans) = reply_intervals(start, end, &value).unwrap();
        assert_eq!(origin - start, Duration::from_millis(2));
        assert_eq!(elapsed, Duration::from_millis(8));
        assert_eq!(spans[1].2 - spans[1].1, Duration::from_millis(5));
        assert_eq!(spans[1].0, "overlay instances=20 bytes=400 calls=2");
        assert_eq!(spans[2].0, "identities instances=9 calls=1 arrived=6");
        for invalid in [
            serde_json::json!({"version":2,"elapsedMs":8,"spans":[]}),
            serde_json::json!({"version":1,"elapsedMs":11,"spans":[]}),
            serde_json::json!({"version":1,"elapsedMs":8,"spans":[["bad",2,9]]}),
        ] {
            assert!(reply_intervals(start, end, &invalid).is_none());
        }
    }

    #[test]
    fn remote_counter_intervals_preserve_queue_and_execution_positions() {
        let intervals = counter_intervals(1000, 1000, 1000, &[1010, 1020, 1100, 1300]).unwrap();
        assert_eq!(
            intervals[0],
            (Duration::from_millis(10), Duration::from_millis(20))
        );
        assert_eq!(
            intervals[2],
            (Duration::from_millis(100), Duration::from_millis(300))
        );
        for (frequency, ticks) in [
            (0, vec![1010, 1020]),
            (999, vec![1010, 1020]),
            (1000, vec![990, 1020]),
            (1000, vec![1010, 1009]),
            (1000, vec![1010, 999999]),
        ] {
            assert!(counter_intervals(1000, 1000, frequency, &ticks).is_none());
        }
    }

    #[test]
    fn native_transport_timing_requires_a_helper_inside_the_round_trip() {
        let elapsed = Duration::from_millis(20);
        assert_eq!(
            counter_transport_bounds(1000, 1000, 1000, &[1003, 1018], elapsed),
            Some((Duration::from_millis(3), Duration::from_millis(18)))
        );
        for ticks in [&[1003, 1021][..], &[999, 1018], &[1018, 1003], &[]] {
            assert!(counter_transport_bounds(1000, 1000, 1000, ticks, elapsed).is_none());
        }
        assert!(counter_transport_bounds(1000, 1000, 999, &[1003, 1018], elapsed).is_none());
    }
}
