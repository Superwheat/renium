use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

static CLI_PROJECT: OnceLock<Option<PathBuf>> = OnceLock::new();
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

pub(crate) fn set_cli_project(project: Option<PathBuf>) {
    let _ = CLI_PROJECT.set(project);
}

pub(crate) fn project_override() -> Option<PathBuf> {
    SELECTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .project
        .clone()
        .or_else(|| CLI_PROJECT.get().cloned().flatten())
}

#[must_use]
pub(crate) struct Selection(SelectedContext);

impl Drop for Selection {
    fn drop(&mut self) {
        *SELECTED.lock().unwrap_or_else(PoisonError::into_inner) = std::mem::take(&mut self.0);
    }
}

// Callers hold the bridge request gate. Staging can nest a different project
// inside the same operation; leaving it must restore the caller, not clear it.
pub(crate) fn select_automation(
    runtime: Option<String>,
    project: PathBuf,
    place: Option<String>,
) -> Selection {
    Selection(std::mem::replace(
        &mut *SELECTED.lock().unwrap_or_else(PoisonError::into_inner),
        SelectedContext {
            runtime,
            project: Some(project),
            place,
        },
    ))
}

pub(crate) fn automation_runtime() -> Option<String> {
    SELECTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .runtime
        .clone()
}

pub(crate) fn set_place_selector(value: Option<String>) {
    SELECTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .place = value.filter(|text| !text.trim().is_empty());
}

pub(crate) fn place_selector() -> Option<String> {
    SELECTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .place
        .clone()
}

pub(crate) fn set_automation_stdio(enabled: bool) {
    AUTOMATION_STDIO.store(enabled, Ordering::Relaxed);
}

pub(crate) fn automation_stdio() -> bool {
    AUTOMATION_STDIO.load(Ordering::Relaxed)
}
