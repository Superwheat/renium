//! Bounded native capture recovery. Only a completed, specifically identified
//! context refusal is retried; timeouts and ownership failures are not replayed.
use std::cell::RefCell;
use std::fmt;
#[cfg(any(windows, test))]
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;

#[cfg(any(windows, test))]
pub(crate) const MAX_CAPTURE_ATTEMPTS: usize = 3;
#[cfg(any(windows, test))]
pub(crate) const CONTEXT_CHANGED: &str = "DataModel task context changed before capture";

#[cfg(any(windows, test))]
#[derive(Debug)]
pub(crate) struct CaptureResponseError {
    pub(crate) status: u32,
    pub(crate) exit: u32,
    pub(crate) message: String,
}

#[cfg(any(windows, test))]
impl fmt::Display for CaptureResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Studio native capture failed with status 0x{:X}, exit 0x{:X}: {}",
            self.status, self.exit, self.message
        )
    }
}

#[cfg(any(windows, test))]
impl std::error::Error for CaptureResponseError {}

#[derive(Clone, Debug)]
pub(crate) struct CaptureUnavailable {
    pub(crate) attempts: usize,
    pub(crate) reason: String,
}

impl fmt::Display for CaptureUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Native capture unavailable after {} attempt(s): {}",
            self.attempts, self.reason
        )
    }
}

impl std::error::Error for CaptureUnavailable {}

#[cfg(any(windows, test))]
fn retry_delay(error: &anyhow::Error, attempt: usize, remaining: Duration) -> Option<Duration> {
    let response = error.downcast_ref::<CaptureResponseError>()?;
    if response.status != 0xE004
        || response.exit != 0xE004
        || response.message != CONTEXT_CHANGED
        || !(1..MAX_CAPTURE_ATTEMPTS).contains(&attempt)
    {
        return None;
    }
    let delay = Duration::from_millis(50 * attempt as u64);
    // Reserve at least one millisecond for discovery/dispatch after the wait.
    (remaining > delay + Duration::from_millis(1)).then_some(delay)
}

#[cfg(any(windows, test))]
pub(crate) fn retry_capture<T>(
    timeout: Duration,
    mut capture: impl FnMut(Duration) -> Result<T>,
    mut observe: impl FnMut(usize, &Result<T>, Option<Duration>),
) -> std::result::Result<(T, usize), CaptureUnavailable> {
    let started = Instant::now();
    let mut attempts = 0;
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.as_millis() == 0 {
            return Err(CaptureUnavailable {
                attempts,
                reason: "Native capture exceeded its deadline".into(),
            });
        }
        attempts += 1;
        let result = capture(remaining);
        let delay = result.as_ref().err().and_then(|error| {
            retry_delay(error, attempts, timeout.saturating_sub(started.elapsed()))
        });
        observe(attempts, &result, delay);
        match result {
            Ok(value) => return Ok((value, attempts)),
            Err(error) => {
                let Some(delay) = delay else {
                    return Err(CaptureUnavailable {
                        attempts,
                        reason: format!("{error:#}"),
                    });
                };
                // Host-side only, after the completed attempt and its transport
                // have been dropped. Never sleep on the Studio DataModel queue.
                std::thread::sleep(delay);
            }
        }
    }
}

/// The native refusal must occur before any service output is emitted. The
/// caller joins workers and drops the old export guard before returning it.
/// Decode, identity, overlay, publication and plugin errors are not fallbacks.
pub(crate) fn export_with_fallback<T>(
    mut export: impl FnMut(bool) -> Result<T>,
    on_fallback: impl FnOnce(&CaptureUnavailable),
) -> Result<(T, Option<CaptureUnavailable>)> {
    match export(true) {
        Ok(value) => Ok((value, None)),
        Err(error) => {
            let Some(unavailable) = error.downcast_ref::<CaptureUnavailable>().cloned() else {
                return Err(error);
            };
            on_fallback(&unavailable);
            let value = export(false)
                .with_context(|| format!("Studio export fallback failed after {unavailable}"))?;
            Ok((value, Some(unavailable)))
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureReport {
    pub(crate) attempts: usize,
    pub(crate) verdict: &'static str,
    pub(crate) fallback: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

thread_local! {
    // Collected on the export's calling thread after its workers have joined.
    // Scoped to a particular start, never a process/global last-result cache.
    static REPORTS: RefCell<Option<Vec<CaptureReport>>> = const { RefCell::new(None) };
}

pub(crate) fn record_report(report: CaptureReport) {
    REPORTS.with_borrow_mut(|reports| {
        if let Some(reports) = reports {
            reports.push(report);
        }
    });
}

pub(crate) fn collect_reports<T>(run: impl FnOnce() -> T) -> (T, Vec<CaptureReport>) {
    let previous = REPORTS.replace(Some(Vec::new()));
    let _restore = crate::system::files::OnDrop::new(|| REPORTS.set(previous));
    let result = run();
    (result, REPORTS.take().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(status: u32, exit: u32, message: &str) -> anyhow::Error {
        CaptureResponseError {
            status,
            exit,
            message: message.into(),
        }
        .into()
    }

    #[test]
    fn only_the_exact_completed_context_refusal_is_retryable() {
        let budget = Duration::from_secs(1);
        let error = refusal(0xE004, 0xE004, CONTEXT_CHANGED).context("capture");
        assert_eq!(
            retry_delay(&error, 1, budget),
            Some(Duration::from_millis(50))
        );
        assert_eq!(
            retry_delay(&error, 2, budget),
            Some(Duration::from_millis(100))
        );
        for attempt in [0, 3, 4, usize::MAX] {
            assert_eq!(retry_delay(&error, attempt, budget), None);
        }
        for error in [
            refusal(0xE004, 0, CONTEXT_CHANGED),
            refusal(0xE00A, 0xE004, CONTEXT_CHANGED),
            refusal(0xE004, 0xE004, "DataModel owner expired"),
            refusal(0xE004, 0xE004, &format!("{CONTEXT_CHANGED} extra")),
            anyhow::anyhow!(
                "Studio native capture failed with status 0xE004, exit 0xE004: {CONTEXT_CHANGED}"
            ),
        ] {
            assert_eq!(retry_delay(&error, 1, budget), None);
        }
        for millis in [0, 50, 51] {
            assert_eq!(retry_delay(&error, 1, Duration::from_millis(millis)), None);
        }
    }

    #[test]
    fn capture_rediscovery_stops_at_success_or_three_attempts() {
        let mut discovered = 0;
        let mut observations = Vec::new();
        let (value, attempts) = retry_capture(
            Duration::from_secs(2),
            |_| {
                discovered += 1;
                if discovered == 2 {
                    Ok(42)
                } else {
                    Err(refusal(0xE004, 0xE004, CONTEXT_CHANGED))
                }
            },
            |attempt, result, delay| observations.push((attempt, result.is_ok(), delay)),
        )
        .unwrap();
        assert_eq!((value, attempts, discovered), (42, 2, 2));
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[1], (2, true, None));

        discovered = 0;
        let error = retry_capture::<()>(
            Duration::from_secs(2),
            |_| {
                discovered += 1;
                Err(refusal(0xE004, 0xE004, CONTEXT_CHANGED))
            },
            |_, _, _| {},
        )
        .unwrap_err();
        assert_eq!((error.attempts, discovered), (3, 3));
        assert!(error.reason.contains(CONTEXT_CHANGED));
    }

    #[test]
    fn capture_deadlines_and_other_failures_do_not_replay_work() {
        let mut calls = 0;
        let error = retry_capture::<()>(
            Duration::ZERO,
            |_| {
                calls += 1;
                Ok(())
            },
            |_, _, _| {},
        )
        .unwrap_err();
        assert_eq!((calls, error.attempts), (0, 0));
        for message in [
            "Native snapshot exceeded its response deadline",
            "DataModel owner expired",
        ] {
            let error = retry_capture::<()>(
                Duration::from_secs(1),
                |_| {
                    calls += 1;
                    Err(refusal(0xE004, 0xE004, message))
                },
                |_, _, _| {},
            )
            .unwrap_err();
            assert_eq!(error.attempts, 1);
        }
        assert_eq!(calls, 2);
        let error = retry_capture::<()>(
            Duration::from_millis(10),
            |_| Err(refusal(0xE004, 0xE004, CONTEXT_CHANGED)),
            |_, _, delay| assert!(delay.is_none()),
        )
        .unwrap_err();
        assert_eq!(error.attempts, 1);
    }

    #[test]
    fn export_fallback_starts_fresh_once_and_retains_attempt_count() {
        let mut modes = Vec::new();
        let (value, unavailable) = export_with_fallback(
            |native| {
                modes.push(native);
                if native {
                    Err(CaptureUnavailable {
                        attempts: 3,
                        reason: CONTEXT_CHANGED.into(),
                    }
                    .into())
                } else {
                    Ok(42)
                }
            },
            |failure| assert_eq!(failure.attempts, 3),
        )
        .unwrap();
        assert_eq!(modes, [true, false]);
        assert_eq!(value, 42);
        assert_eq!(unavailable.unwrap().attempts, 3);
        let (value, unavailable) = export_with_fallback(
            |native| {
                assert!(native);
                Ok(43)
            },
            |_| panic!("successful capture must not fall back"),
        )
        .unwrap();
        assert_eq!(value, 43);
        assert!(unavailable.is_none());
    }

    #[test]
    fn export_integrity_failures_are_not_fallbacks_and_plugin_failure_is_final() {
        let error = export_with_fallback::<()>(
            |native| {
                assert!(native);
                anyhow::bail!("Native serialized parent disagrees with identity capture")
            },
            |_| panic!("identity failure must not be swallowed"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity capture"));
        let mut calls = 0;
        let error = export_with_fallback::<()>(
            |native| {
                calls += 1;
                if native {
                    Err(CaptureUnavailable {
                        attempts: 2,
                        reason: CONTEXT_CHANGED.into(),
                    }
                    .into())
                } else {
                    anyhow::bail!("plugin disconnected")
                }
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(calls, 2);
        let message = format!("{error:#}");
        assert!(message.contains("2 attempt(s)") && message.contains("plugin disconnected"));
    }

    #[test]
    fn startup_reports_are_scoped_and_do_not_cross_threads() {
        let report = || CaptureReport {
            attempts: 2,
            verdict: "native",
            fallback: None,
            reason: None,
        };
        let ((), outer) = collect_reports(|| {
            record_report(report());
            let ((), inner) = collect_reports(|| record_report(report()));
            assert_eq!(inner.len(), 1);
            std::thread::spawn(move || record_report(report()))
                .join()
                .unwrap();
        });
        assert_eq!(outer.len(), 1);
        assert_eq!(serde_json::to_value(&outer).unwrap()[0]["attempts"], 2);
        assert!(collect_reports(|| ()).1.is_empty());
    }

    #[test]
    fn startup_attempts_and_degraded_verdict_survive_compact_output() {
        let reports = serde_json::json!([{
            "attempts": 3,
            "verdict": "degraded",
            "fallback": "plugin-export",
            "reason": CONTEXT_CHANGED,
        }]);
        let status = crate::studio::automation::compact_live_status(serde_json::json!({
            "ok": true,
            "tracking": true,
            "daemon": { "running": true, "nativeCapture": reports },
        }));
        assert_eq!(status["daemon"]["nativeCapture"], reports);
        assert_eq!(status["ok"], true);
        assert_eq!(status["tracking"], true);
    }
}
