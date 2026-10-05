use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::app::output::log_global;
use crate::system::LockRecover;

const IDLE_BEFORE_RELEASE: Duration = Duration::from_secs(1);
const HEAVY_REQUEST: Duration = Duration::from_millis(50);
const PAYLOAD_CACHE_IDLE_LIMIT: Duration = Duration::from_secs(5 * 60);

static ACTIVE_REQUESTS: Mutex<Vec<(u64, &'static str, Instant)>> = Mutex::new(Vec::new());
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);
static RELEASE_PENDING: AtomicBool = AtomicBool::new(false);
static PAYLOAD_CACHES_HELD: AtomicBool = AtomicBool::new(false);
static LAST_FINISHED: Mutex<Option<Instant>> = Mutex::new(None);

pub(crate) struct RequestGuard {
    id: u64,
    started: Instant,
}

pub(crate) fn begin_request(operation: &'static str) -> RequestGuard {
    let guard = RequestGuard {
        id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
        started: Instant::now(),
    };
    ACTIVE_REQUESTS
        .lock_recover()
        .push((guard.id, operation, guard.started));
    guard
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        ACTIVE_REQUESTS
            .lock_recover()
            .retain(|(id, _, _)| *id != self.id);
        if self.started.elapsed() >= HEAVY_REQUEST {
            note_work_finished();
        }
    }
}

/// The operations the daemon is serving, longest-running first.
pub(crate) fn active_requests() -> Vec<(&'static str, Duration)> {
    let mut active = ACTIVE_REQUESTS
        .lock_recover()
        .iter()
        .map(|(_, operation, started)| (*operation, started.elapsed()))
        .collect::<Vec<_>>();
    active.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    active
}

pub(crate) fn after_background_work() {
    collect_thread_heap();
    note_work_finished();
}

fn note_work_finished() {
    *LAST_FINISHED.lock_recover() = Some(Instant::now());
    RELEASE_PENDING.store(true, Ordering::Relaxed);
    PAYLOAD_CACHES_HELD.store(true, Ordering::Relaxed);
}

pub(crate) fn release_when_idle() {
    if !ACTIVE_REQUESTS.lock_recover().is_empty() {
        return;
    }
    let Some(idle_for) = LAST_FINISHED
        .lock_recover()
        .map(|finished| finished.elapsed())
    else {
        return;
    };
    if RELEASE_PENDING.load(Ordering::Relaxed) && idle_for >= IDLE_BEFORE_RELEASE {
        RELEASE_PENDING.store(false, Ordering::Relaxed);
        release_now();
    }
    if PAYLOAD_CACHES_HELD.load(Ordering::Relaxed) && idle_for >= PAYLOAD_CACHE_IDLE_LIMIT {
        PAYLOAD_CACHES_HELD.store(false, Ordering::Relaxed);
        let payload_bytes = crate::snapshot::export::drop_bridge_text_payload_cache()
            + crate::studio::native::editor::drop_native_payload_cache();
        let (store_bytes, store_instances) =
            crate::settings::bytecode::drop_settings_document_cache();
        let projections = crate::project::config::drop_projection_cache();
        release_now();
        log_global(
            3,
            format_args!(
                "[renium] dropped idle caches after {} minutes: payloads {:.1} MB, stores {:.1} MB ({store_instances} instances), {projections} projections",
                PAYLOAD_CACHE_IDLE_LIMIT.as_secs() / 60,
                payload_bytes as f64 / (1024.0 * 1024.0),
                store_bytes as f64 / (1024.0 * 1024.0)
            ),
        );
    }
}

fn release_now() {
    rayon::spawn_broadcast(|_| collect_thread_heap());
    collect_thread_heap();
}

fn collect_thread_heap() {
    unsafe { libmimalloc_sys::mi_collect(true) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_requests_list_running_operations_until_they_finish() {
        let outer = begin_request("memory-test-outer");
        std::thread::sleep(Duration::from_millis(5));
        let inner = begin_request("memory-test-inner");
        let names = || {
            active_requests()
                .into_iter()
                .map(|(name, _)| name)
                .filter(|name| name.starts_with("memory-test-"))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(), ["memory-test-outer", "memory-test-inner"]);
        drop(outer);
        assert_eq!(names(), ["memory-test-inner"]);
        drop(inner);
        assert!(names().is_empty());
    }
}
