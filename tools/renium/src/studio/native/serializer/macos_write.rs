//! Verify a mutation with a new read on the same identity-checked DataModel queue.
use std::time::Duration;

use anyhow::Result;

#[derive(Debug)]
pub(super) struct WriteUnverified {
    class: String,
    property: String,
    expected: String,
    actual: Option<String>,
    reason: String,
}

impl std::fmt::Display for WriteUnverified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{} write was not verified (requested {:?}, last queued read {:?}): {}; inspect the value before repeating the write",
            self.class, self.property, self.expected, self.actual, self.reason,
        )
    }
}

impl std::error::Error for WriteUnverified {}

/// The setter's response is deliberately not an input: even its immediate
/// getter may precede deferred engine work. Only subsequent queued reads can
/// confirm success. Poll reads within the original budget; never replay a write.
pub(super) fn verify_readback(
    class: &str,
    property: &str,
    expected: &str,
    mut read: impl FnMut() -> Result<String>,
    mut wait: impl FnMut(Duration) -> Result<()>,
) -> Result<String> {
    let mut actual = None;
    let mut pause_ms = 0;
    loop {
        let value = wait(Duration::from_millis(pause_ms)).and_then(|()| read());
        let value = value.map_err(|error| WriteUnverified {
            class: class.into(),
            property: property.into(),
            expected: expected.into(),
            actual: actual.clone(),
            // This is a mutation failure, even if verification timed out or
            // lost its locator. Do not expose a read-only fallback verdict.
            reason: format!("{error:#}"),
        })?;
        if crate::automation::property_access::property_text_matches(
            class, property, expected, &value,
        ) {
            return Ok(value);
        }
        actual = Some(value);
        pause_ms = (pause_ms * 2).clamp(1, 8);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{TaskContextUnavailable, macos_deadline, optional_native};
    use super::*;
    use crate::studio::native::capture::CaptureUnavailable;

    #[test]
    fn saved_field_success_requires_a_fresh_matching_queue_read() {
        // A setter returning success is insufficient: an asynchronous setter
        // can still report the old value from the next queued getter.
        let mut values = ["Voxel", "Voxel", "Future"].into_iter();
        let mut waits = Vec::new();
        let value = verify_readback(
            "Lighting",
            "Technology",
            "Future",
            || Ok(values.next().expect("only read until verified").into()),
            |pause| {
                waits.push(pause.as_millis());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(value, "Future");
        assert_eq!(waits, [0, 1, 2]);
        assert!(values.next().is_none());
    }

    #[test]
    fn an_acknowledged_noop_setter_is_a_typed_failure_not_a_fallback() {
        let mut polls = 0;
        let error = verify_readback(
            "Lighting",
            "Technology",
            "Future",
            || Ok("Voxel".into()),
            |_| {
                polls += 1;
                if polls < 3 {
                    Ok(())
                } else {
                    Err(macos_deadline::DeadlineExceeded.into())
                }
            },
        )
        .unwrap_err();
        let failure = error.downcast_ref::<WriteUnverified>().unwrap();
        assert_eq!(failure.actual.as_deref(), Some("Voxel"));
        assert_eq!(failure.expected, "Future");
        let error = optional_native::<()>(Err(error)).unwrap_err();
        assert!(error.is::<WriteUnverified>());
        assert!(!macos_deadline::identity_capture_error(0, error).is::<CaptureUnavailable>());
    }

    #[test]
    fn an_unreadable_write_cannot_become_a_read_only_fallback() {
        for cause in [
            anyhow::Error::new(macos_deadline::DeadlineExceeded),
            TaskContextUnavailable.into(),
            std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into(),
        ] {
            let reason = cause.to_string();
            let mut cause = Some(cause);
            let error = verify_readback(
                "Lighting",
                "Technology",
                "Future",
                || Err(cause.take().expect("a failed queue read is not retried")),
                |_| Ok(()),
            )
            .unwrap_err();
            assert!(error.to_string().contains(&reason));
            let error = optional_native::<()>(Err(error)).unwrap_err();
            assert!(error.is::<WriteUnverified>());
            assert!(!macos_deadline::identity_capture_error(0, error).is::<CaptureUnavailable>());
        }
    }
}
