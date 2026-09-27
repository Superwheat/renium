use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::app::output::log_global;
use crate::system::LockRecover;

const IDLE_BEFORE_RELEASE: Duration = Duration::from_secs(1);
const HEAVY_REQUEST: Duration = Duration::from_millis(50);
const PAYLOAD_CACHE_IDLE_LIMIT: Duration = Duration::from_secs(5 * 60);

static ACTIVE_REQUESTS: AtomicUsize = AtomicUsize::new(0);
static RELEASE_PENDING: AtomicBool = AtomicBool::new(false);
static PAYLOAD_CACHES_HELD: AtomicBool = AtomicBool::new(false);
static LAST_FINISHED: Mutex<Option<Instant>> = Mutex::new(None);

pub(crate) struct RequestGuard {
    started: Instant,
}

pub(crate) fn begin_request() -> RequestGuard {
    ACTIVE_REQUESTS.fetch_add(1, Ordering::Relaxed);
    RequestGuard {
        started: Instant::now(),
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        ACTIVE_REQUESTS.fetch_sub(1, Ordering::Relaxed);
        if self.started.elapsed() >= HEAVY_REQUEST {
            note_work_finished();
        }
    }
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
    if ACTIVE_REQUESTS.load(Ordering::Relaxed) != 0 {
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
