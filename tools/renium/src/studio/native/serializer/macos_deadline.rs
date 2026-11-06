//! One deadline for macOS reflection discovery, queueing and response reads.
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;

// Emitted only for an expired property task by renium_studio_helper_macos.cpp.
pub(super) const TASK_DEADLINE_STATUS: u32 = 21;

#[derive(Debug)]
pub(super) struct DeadlineExceeded;

impl std::fmt::Display for DeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Native reflection operation exceeded its deadline")
    }
}

impl std::error::Error for DeadlineExceeded {}

pub(super) fn remaining(deadline: Instant, now: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| DeadlineExceeded.into())
}

pub(super) fn task_timeout_ms(remaining: Duration) -> u32 {
    remaining.as_millis().clamp(1, u128::from(u32::MAX)) as u32
}

pub(super) fn task_error(status: u32, reason: String) -> anyhow::Error {
    if status == TASK_DEADLINE_STATUS {
        anyhow::Error::new(DeadlineExceeded).context(reason)
    } else {
        anyhow::anyhow!(reason)
    }
}

/// A helper can close at the deadline before its status header reaches us.
/// Retain that expiry as a typed cause, including through transport context.
/// An early disconnect or an unrelated I/O failure is still a hard failure.
pub(super) fn io_error_until(error: io::Error, deadline: Instant, now: Instant) -> anyhow::Error {
    if now >= deadline
        && matches!(
            error.kind(),
            io::ErrorKind::TimedOut
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
        )
    {
        anyhow::Error::new(DeadlineExceeded).context(error)
    } else {
        error.into()
    }
}

/// XNU refuses SO_RCVTIMEO with EINVAL after the peer disconnects, even while
/// its final response bytes remain buffered. The socket already has a timeout;
/// drain those bytes (or observe EOF) instead of losing the response.
pub(super) fn read_after_timeout_refresh(
    refreshed: io::Result<()>,
    read: impl FnOnce() -> io::Result<usize>,
) -> io::Result<usize> {
    if let Err(error) = refreshed
        && error.kind() != io::ErrorKind::InvalidInput
        && error.raw_os_error() != Some(22)
    {
        return Err(error);
    }
    read()
}

/// Retry the read, never the request. Keep partially received bytes and refresh
/// SO_RCVTIMEO before every read; EAGAIN is not evidence the deadline expired.
pub(super) fn read_exact_until(
    mut buffer: &mut [u8],
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    mut read: impl FnMut(&mut [u8], Duration) -> io::Result<usize>,
) -> Result<()> {
    while !buffer.is_empty() {
        let timeout = remaining(deadline, now())?;
        match read(buffer, timeout) {
            Ok(0) => {
                return Err(io_error_until(
                    io::ErrorKind::UnexpectedEof.into(),
                    deadline,
                    now(),
                ));
            }
            Ok(count) => buffer = &mut buffer[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(io_error_until(error, deadline, now())),
        }
    }
    Ok(())
}

/// Context preparation is read-only and precedes every property operation.
/// A busy UI must leave time for the plugin path instead of using the entire
/// push deadline. Never apply this conversion to a queued read or mutation.
pub(super) fn prepare_context<T>(
    pid: u32,
    timeout: Duration,
    prepare: impl FnOnce(Duration) -> Result<T>,
) -> Result<T> {
    prepare(timeout.min(Duration::from_secs(1))).map_err(|error| {
        if !error.is::<DeadlineExceeded>() {
            return error;
        }
        let error = anyhow::Error::new(super::TaskContextUnavailable).context(format!(
            "Native context preparation timed out before any property operation; using plugin path: {error:#}"
        ));
        super::probes::record(
            pid,
            super::probes::NativeOperation::DescriptorBinding,
            &Err(anyhow::anyhow!("{error:#}")),
        );
        error
    })
}

/// Only the read-only identity worker may turn a verified deadline into a
/// fresh plugin export. Property reads/writes and other transport errors fail.
#[cfg(any(target_arch = "aarch64", test))]
pub(super) fn identity_capture_error(pid: u32, error: anyhow::Error) -> anyhow::Error {
    if !error.is::<DeadlineExceeded>() {
        return error;
    }
    let reason = format!("Native identity read timed out; use plugin identities: {error:#}");
    super::probes::record(
        pid,
        super::probes::NativeOperation::Capture,
        &Err(anyhow::anyhow!("{reason}")),
    );
    crate::studio::native::capture::CaptureUnavailable {
        attempts: 1,
        reason,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::native::capture::{CaptureUnavailable, export_with_fallback};
    use std::cell::Cell;

    #[test]
    fn context_preparation_reserves_the_push_budget_and_reports_plugin_fallback() {
        let pid = u32::MAX - 80;
        let now = Instant::now();
        let mut budgets = Vec::new();
        for timeout in [Duration::from_secs(10), Duration::from_millis(40)] {
            let result = prepare_context::<()>(pid, timeout, |budget| {
                budgets.push(budget);
                Err(
                    io_error_until(io::ErrorKind::UnexpectedEof.into(), now, now)
                        .context("operation 4 context preparation"),
                )
            });
            assert!(super::super::optional_native(result).unwrap().is_none());
        }
        assert_eq!(budgets, [Duration::from_secs(1), Duration::from_millis(40)]);
        let status = super::super::probes::cached(pid);
        let descriptor = status["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["operation"] == "descriptorBinding")
            .unwrap();
        assert_eq!(descriptor["verdict"], "degraded");
        let reason = descriptor["reason"].as_str().unwrap();
        assert!(reason.contains("before any property operation; using plugin path"));
        assert!(reason.contains("operation 4 context preparation"));
    }

    #[test]
    fn preparation_fallback_does_not_hide_unrelated_errors_or_license_writes() {
        assert_eq!(
            prepare_context(0, Duration::from_secs(2), |_| Ok(7)).unwrap(),
            7
        );
        for failure in [
            anyhow::anyhow!("Native reflection operation exceeded its deadline"),
            anyhow::anyhow!("Reflection target does not match the selected Studio place"),
            io::Error::from(io::ErrorKind::UnexpectedEof).into(),
        ] {
            let reason = failure.to_string();
            let result = prepare_context::<()>(0, Duration::from_secs(10), |_| Err(failure));
            assert_eq!(
                super::super::optional_native(result)
                    .unwrap_err()
                    .to_string(),
                reason
            );
        }
        let error = super::super::optional_native::<()>(Err(task_error(
            TASK_DEADLINE_STATUS,
            "property write response expired".into(),
        )))
        .unwrap_err();
        assert!(error.is::<DeadlineExceeded>());
    }

    #[test]
    fn a_disconnected_peers_buffered_response_survives_timeout_refresh_failure() {
        // Model a successful header read followed by a body read after the
        // helper closes. Each body chunk refreshes SO_RCVTIMEO again.
        for raw_errno in [false, true] {
            let mut chunks = [b"HEAD".as_slice(), b"bo", b"dy"].into_iter();
            let mut refreshes = 0;
            let mut read = |buffer: &mut [u8], _: Duration| {
                refreshes += 1;
                let refreshed = if refreshes == 1 {
                    Ok(())
                } else if raw_errno {
                    Err(io::Error::from_raw_os_error(22))
                } else {
                    Err(io::ErrorKind::InvalidInput.into())
                };
                read_after_timeout_refresh(refreshed, || {
                    let chunk = chunks.next().expect("read only the response");
                    buffer[..chunk.len()].copy_from_slice(chunk);
                    Ok(chunk.len())
                })
            };
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut header = [0; 4];
            let mut body = [0; 4];
            read_exact_until(&mut header, deadline, Instant::now, &mut read).unwrap();
            read_exact_until(&mut body, deadline, Instant::now, &mut read).unwrap();
            assert_eq!(&header, b"HEAD");
            assert_eq!(&body, b"body");
            assert_eq!(refreshes, 3);
        }
    }

    #[test]
    fn timeout_refresh_does_not_hide_read_failures_or_expired_deadlines() {
        for outcome in [Ok(0), Err(io::ErrorKind::ConnectionReset.into())] {
            let mut outcome = Some(outcome);
            let error = read_exact_until(
                &mut [0],
                Instant::now() + Duration::from_secs(3),
                Instant::now,
                |_, _| {
                    read_after_timeout_refresh(Err(io::ErrorKind::InvalidInput.into()), || {
                        outcome.take().unwrap()
                    })
                },
            )
            .unwrap_err();
            assert!(error.is::<io::Error>());
            assert!(!error.is::<DeadlineExceeded>());
        }
        assert!(
            read_after_timeout_refresh(Err(io::ErrorKind::PermissionDenied.into()), || {
                panic!("unrelated socket errors must fail")
            })
            .is_err()
        );
        let now = Instant::now();
        assert!(
            read_exact_until(
                &mut [0],
                now,
                || now,
                |_, _| panic!("deadline checked first")
            )
            .unwrap_err()
            .is::<DeadlineExceeded>()
        );
    }

    #[test]
    fn first_response_byte_can_arrive_after_an_early_one_second_timeout() {
        let started = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut timeouts = Vec::new();
        let mut header = [0; 4];
        read_exact_until(
            &mut header,
            started + Duration::from_secs(3),
            || started + elapsed.get(),
            |buffer, timeout| {
                timeouts.push(timeout);
                if elapsed.get().is_zero() {
                    elapsed.set(Duration::from_secs(1));
                    Err(io::ErrorKind::WouldBlock.into())
                } else {
                    elapsed.set(Duration::from_millis(2500));
                    buffer.copy_from_slice(b"RENM");
                    Ok(buffer.len())
                }
            },
        )
        .unwrap();
        assert_eq!(&header, b"RENM");
        assert_eq!(timeouts, [Duration::from_secs(3), Duration::from_secs(2)]);
    }

    #[test]
    fn response_reads_retry_early_timeouts_and_preserve_partial_bytes() {
        let started = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut timeouts = Vec::new();
        let mut calls = 0;
        let mut output = [0; 4];
        read_exact_until(
            &mut output,
            started + Duration::from_secs(3),
            || started + elapsed.get(),
            |buffer, timeout| {
                timeouts.push(timeout.as_millis());
                calls += 1;
                elapsed.set(elapsed.get() + Duration::from_millis(500));
                match calls {
                    1 => {
                        buffer[..2].copy_from_slice(b"ab");
                        Ok(2)
                    }
                    2 => Err(io::ErrorKind::WouldBlock.into()),
                    3 => Err(io::ErrorKind::TimedOut.into()),
                    4 => Err(io::ErrorKind::Interrupted.into()),
                    5 => {
                        buffer.copy_from_slice(b"cd");
                        Ok(2)
                    }
                    _ => panic!("read must stop at completion"),
                }
            },
        )
        .unwrap();
        assert_eq!(&output, b"abcd");
        assert_eq!(timeouts, [3000, 2500, 2000, 1500, 1000]);
    }

    #[test]
    fn response_header_and_body_share_one_deadline() {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(3);
        let mut header = [0];
        read_exact_until(
            &mut header,
            deadline,
            || started,
            |buffer, timeout| {
                assert_eq!(timeout, Duration::from_secs(3));
                buffer[0] = 1;
                Ok(1)
            },
        )
        .unwrap();
        let elapsed = Cell::new(Duration::from_secs(2));
        let error = read_exact_until(
            &mut [0; 4],
            deadline,
            || started + elapsed.get(),
            |_, timeout| {
                assert_eq!(timeout, Duration::from_secs(1));
                elapsed.set(Duration::from_secs(3));
                Err(io::ErrorKind::WouldBlock.into())
            },
        )
        .unwrap_err();
        assert!(error.is::<DeadlineExceeded>());
        for now in [deadline, deadline + Duration::from_millis(1)] {
            assert!(
                remaining(deadline, now)
                    .unwrap_err()
                    .is::<DeadlineExceeded>()
            );
        }
    }

    #[test]
    fn task_budget_tracks_the_caller_without_a_three_second_cap() {
        let started = Instant::now();
        for seconds in [3, 10, 30, 60, 90] {
            let budget = remaining(
                started + Duration::from_secs(seconds),
                started + Duration::from_millis(1250),
            )
            .unwrap();
            assert_eq!(task_timeout_ms(budget), (seconds * 1000 - 1250) as u32);
        }
        assert_eq!(task_timeout_ms(Duration::from_nanos(1)), 1);
        assert_eq!(task_timeout_ms(Duration::MAX), u32::MAX);
    }

    #[test]
    fn only_verified_identity_deadlines_fall_back_with_a_status_reason() {
        let pid = u32::MAX - 12;
        let mut modes = Vec::new();
        let (_, fallback) = export_with_fallback(
            |native| {
                modes.push(native);
                if native {
                    Err(identity_capture_error(
                        pid,
                        anyhow::Error::new(DeadlineExceeded).context("identity response"),
                    ))
                } else {
                    Ok(())
                }
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(modes, [true, false]);
        let status = super::super::probes::cached(pid);
        let capture = status["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["operation"] == "capture")
            .unwrap();
        assert_eq!(capture["verdict"], "degraded");
        assert_eq!(capture["reason"], fallback.unwrap().reason);

        // A timeout never licenses the optional protected-property path.
        assert!(
            super::super::optional_native::<()>(Err(DeadlineExceeded.into()))
                .unwrap_err()
                .is::<DeadlineExceeded>()
        );
        for error in [
            anyhow::anyhow!("Native reflection operation exceeded its deadline"),
            anyhow::anyhow!("Native identity capture context changed"),
            anyhow::anyhow!("Invalid native identity rows"),
            io::Error::from(io::ErrorKind::TimedOut).into(),
            io::Error::from(io::ErrorKind::WouldBlock).into(),
            io::Error::from(io::ErrorKind::UnexpectedEof).into(),
            task_error(20, "Protected property task timed out".into()),
            task_error(8, "Protected property call expired before execution".into()),
        ] {
            assert!(!identity_capture_error(pid, error).is::<CaptureUnavailable>());
        }
        assert!(
            task_error(
                TASK_DEADLINE_STATUS,
                "Protected property task timed out".into()
            )
            .is::<DeadlineExceeded>()
        );
    }

    #[test]
    fn a_closed_or_broken_socket_is_not_a_deadline() {
        for result in [Ok(0), Err(io::ErrorKind::ConnectionReset.into())] {
            let mut result = Some(result);
            let error = read_exact_until(
                &mut [0],
                Instant::now() + Duration::from_secs(3),
                Instant::now,
                |_, _| result.take().expect("terminal failures must not retry"),
            )
            .unwrap_err();
            assert!(error.is::<io::Error>());
            assert!(!error.is::<DeadlineExceeded>());
        }
    }

    #[test]
    fn identity_deadlines_survive_transport_context_and_the_worker_join() {
        let now = Instant::now();
        let mut failures = vec![
            task_error(TASK_DEADLINE_STATUS, "context preparation expired".into()),
            task_error(
                TASK_DEADLINE_STATUS,
                "identity capture task 4 expired".into(),
            ),
            remaining(now, now).unwrap_err().context("discovery"),
        ];
        // No header, a partial header, and a complete header with a partial
        // body must all retain expiry when the helper closes or resets.
        for received in [0, 1, 536] {
            for terminal in [io::ErrorKind::UnexpectedEof, io::ErrorKind::ConnectionReset] {
                let expired = Cell::new(false);
                let mut reads = 0;
                let error = read_exact_until(
                    &mut vec![0; 536 + 64],
                    now + Duration::from_secs(3),
                    || now + Duration::from_secs(if expired.get() { 3 } else { 0 }),
                    |buffer, _| {
                        reads += 1;
                        if reads == 1 && received > 0 {
                            buffer[..received].fill(1);
                            return Ok(received);
                        }
                        expired.set(true);
                        if terminal == io::ErrorKind::UnexpectedEof {
                            Ok(0)
                        } else {
                            Err(terminal.into())
                        }
                    },
                )
                .unwrap_err();
                failures.push(error.context("identity response header/body"));
            }
        }
        failures.push(io_error_until(io::ErrorKind::TimedOut.into(), now, now));
        failures.push(io_error_until(io::ErrorKind::WouldBlock.into(), now, now));
        for (index, error) in failures.into_iter().enumerate() {
            let pid = u32::MAX - 32 - index as u32;
            let error = error
                .context("Studio reflection transport")
                .context("Could not read identity capture");
            // Exactly this error remains fatal for a property read or write.
            let error = super::super::optional_native::<()>(Err(error)).unwrap_err();
            assert!(error.is::<DeadlineExceeded>());
            assert!(!error.is::<CaptureUnavailable>());
            let reason = format!("{error:#}");
            let mut error = Some(
                std::thread::scope(|scope| {
                    scope
                        .spawn(move || identity_capture_error(pid, error))
                        .join()
                        .unwrap()
                })
                .context("identity worker"),
            );
            let mut modes = Vec::new();
            let (_, fallback) = export_with_fallback(
                |native| {
                    modes.push(native);
                    if native {
                        Err(error.take().unwrap())
                    } else {
                        Ok(())
                    }
                },
                |_| {},
            )
            .unwrap();
            assert_eq!(modes, [true, false]);
            let fallback = fallback.unwrap();
            assert!(fallback.reason.contains(&reason));
            let status = super::super::probes::cached(pid);
            let capture = status["operations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["operation"] == "capture")
                .unwrap();
            assert_eq!(capture["verdict"], "degraded");
            assert_eq!(capture["reason"], fallback.reason);
        }
    }

    #[test]
    fn elapsed_time_does_not_turn_unrelated_io_failures_into_capture_fallbacks() {
        let deadline = Instant::now();
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidData,
            io::ErrorKind::InvalidInput,
        ] {
            let error = io_error_until(kind.into(), deadline, deadline);
            assert!(!identity_capture_error(0, error).is::<CaptureUnavailable>());
        }
        for kind in [
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::BrokenPipe,
        ] {
            let error = io_error_until(kind.into(), deadline + Duration::from_secs(1), deadline);
            assert!(!identity_capture_error(0, error).is::<CaptureUnavailable>());
        }
    }
}
