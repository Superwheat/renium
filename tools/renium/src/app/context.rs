use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static CLI_PROJECT: OnceLock<Option<PathBuf>> = OnceLock::new();
static AUTOMATION_PROJECT: Mutex<Option<PathBuf>> = Mutex::new(None);
static AUTOMATION_RUNTIME: Mutex<Option<String>> = Mutex::new(None);
static PLACE_SELECTOR: Mutex<Option<String>> = Mutex::new(None);
static AUTOMATION_STDIO: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_cli_project(project: Option<PathBuf>) {
    let _ = CLI_PROJECT.set(project);
}

pub(crate) fn project_override() -> Option<PathBuf> {
    AUTOMATION_PROJECT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .or_else(|| CLI_PROJECT.get().cloned().flatten())
}

pub(crate) fn select_automation(runtime: Option<String>, project: PathBuf) {
    *AUTOMATION_RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = runtime;
    *AUTOMATION_PROJECT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(project);
}

pub(crate) fn clear_automation() {
    *AUTOMATION_RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    *AUTOMATION_PROJECT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

pub(crate) fn automation_runtime() -> Option<String> {
    AUTOMATION_RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

pub(crate) fn set_place_selector(value: Option<String>) {
    *PLACE_SELECTOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        value.filter(|text| !text.trim().is_empty());
}

pub(crate) fn place_selector() -> Option<String> {
    PLACE_SELECTOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

pub(crate) fn set_automation_stdio(enabled: bool) {
    AUTOMATION_STDIO.store(enabled, Ordering::Relaxed);
}

pub(crate) fn automation_stdio() -> bool {
    AUTOMATION_STDIO.load(Ordering::Relaxed)
}
