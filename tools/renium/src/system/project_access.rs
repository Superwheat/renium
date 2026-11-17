//! Cross-daemon exclusion for project operations and import publication.
//! The OS owns the lock lifetime: process death releases it without PID tests.
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use anyhow::{Context, Result};

pub(crate) struct ProjectAccess {
    _file: Rc<File>,
}

thread_local! {
    // Nested managed calls on the same thread share ownership. Rc prevents
    // transferring this reentrant guard to a different thread.
    static HELD: RefCell<HashMap<PathBuf, Weak<File>>> = RefCell::new(HashMap::new());
}

pub(crate) fn try_acquire(root: &Path) -> Result<Option<ProjectAccess>> {
    let root = fs::canonicalize(root).context("Cannot resolve the project lock directory")?;
    if let Some(file) = HELD.with(|held| held.borrow().get(&root).and_then(Weak::upgrade)) {
        return Ok(Some(ProjectAccess { _file: file }));
    }
    let directory = root.join(".renium");
    fs::create_dir_all(&directory)?;
    // Never unlink this file: another process may already have it open.
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("project-access.lock"))?;
    match file.try_lock() {
        Ok(()) => {
            let file = Rc::new(file);
            HELD.with(|held| {
                let mut held = held.borrow_mut();
                held.retain(|_, file| file.strong_count() > 0);
                held.insert(root, Rc::downgrade(&file));
            });
            Ok(Some(ProjectAccess { _file: file }))
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(error) => {
            Err(error).context("Cannot lock project access; no project changes were made")
        }
    }
}

pub(crate) fn acquire(root: &Path, cancelled: impl Fn() -> Result<()>) -> Result<ProjectAccess> {
    loop {
        cancelled()?;
        if let Some(held) = try_acquire(root)? {
            return Ok(held);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
