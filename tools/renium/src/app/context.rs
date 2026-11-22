use crate::system::LockRecover;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static CLI_PROJECT: OnceLock<Option<PathBuf>> = OnceLock::new();
static PERSISTENT_PLACE: OnceLock<Option<String>> = OnceLock::new();
#[derive(Default)]
struct SelectedContext {
    project: Option<PathBuf>,
    runtime: Option<String>,
    place: Option<String>,
}

static SELECTED: Mutex<SelectedContext> = Mutex::new(SelectedContext {
    project: None,
    runtime: None,
    place: None,
});
static AUTOMATION_STDIO: AtomicBool = AtomicBool::new(false);

thread_local! {
    static THREAD_SELECTED: RefCell<Option<SelectedContext>> = const { RefCell::new(None) };
}

/// The selection the calling thread works under: the one a request bound to
/// this thread, else the process-wide one.
fn selected<T>(read: impl Fn(&SelectedContext) -> T) -> T {
    THREAD_SELECTED
        .with(|selected| selected.borrow().as_ref().map(&read))
        .unwrap_or_else(|| read(&SELECTED.lock_recover()))
}

pub(crate) fn set_cli_project(project: Option<PathBuf>) {
    let _ = CLI_PROJECT.set(project);
}

pub(crate) fn project_override() -> Option<PathBuf> {
    selected(|selected| selected.project.clone()).or_else(|| CLI_PROJECT.get().cloned().flatten())
}

enum Restore {
    Process(SelectedContext),
    Thread(Option<SelectedContext>),
}

#[must_use]
pub(crate) struct Selection(Restore);

impl Drop for Selection {
    fn drop(&mut self) {
        match &mut self.0 {
            Restore::Process(previous) => *SELECTED.lock_recover() = std::mem::take(previous),
            Restore::Thread(previous) => {
                THREAD_SELECTED.with(|selected| *selected.borrow_mut() = previous.take());
            }
        }
    }
}

// Callers hold the bridge request gate. Staging can nest a different project
// inside the same operation; leaving it must restore the caller, not clear it.
pub(crate) fn select_automation(
    runtime: Option<String>,
    project: PathBuf,
    place: Option<String>,
) -> Selection {
    // A test with a thread-local mock Studio also stages nested requests on
    // that thread. Its temporary project must never select a peer for another test.
    #[cfg(test)]
    if THREAD_SELECTED.with(|selected| selected.borrow().is_some()) {
        return select_automation_for_thread(runtime, project, place);
    }
    Selection(Restore::Process(std::mem::replace(
        &mut *SELECTED.lock_recover(),
        SelectedContext {
            runtime,
            project: Some(project),
            place,
        },
    )))
}

/// A selection that only the calling thread sees, for a request that runs
/// beside requests of other Studios: it leaves the process-wide selection,
/// and the threads that read it, untouched.
pub(crate) fn select_automation_for_thread(
    runtime: Option<String>,
    project: PathBuf,
    place: Option<String>,
) -> Selection {
    Selection(Restore::Thread(THREAD_SELECTED.with(|selected| {
        selected.borrow_mut().replace(SelectedContext {
            runtime,
            project: Some(project),
            place,
        })
    })))
}

pub(crate) fn automation_runtime() -> Option<String> {
    selected(|selected| selected.runtime.clone())
}

pub(crate) fn set_place_selector(value: Option<String>) {
    SELECTED.lock_recover().place = value.filter(|text| !text.trim().is_empty());
}

pub(crate) fn place_selector() -> Option<String> {
    selected(|selected| selected.place.clone())
}

/// Records the place pin every command run in this environment carries:
/// RENIUM_PLACE, else the stored `place` setting, whatever `--place` says.
pub(crate) fn set_persistent_place_selector(value: Option<String>) {
    let _ = PERSISTENT_PLACE.set(value.filter(|text| !text.trim().is_empty()));
}

pub(crate) fn persistent_place_selector() -> Option<String> {
    PERSISTENT_PLACE.get().cloned().flatten()
}

pub(crate) fn set_automation_stdio(enabled: bool) {
    AUTOMATION_STDIO.store(enabled, Ordering::Relaxed);
}

pub(crate) fn automation_stdio() -> bool {
    AUTOMATION_STDIO.load(Ordering::Relaxed)
}
