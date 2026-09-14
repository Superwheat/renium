pub(crate) mod files;
pub(crate) mod net;
pub(crate) mod text;
pub(crate) mod tools;
pub(crate) mod watch;

use std::sync::{Mutex, MutexGuard, PoisonError};

// A poisoned lock still guards valid data here: every holder either finished
// its update or left state a later request re-validates, so waiting callers
// recover the guard instead of propagating the panic.
pub(crate) trait LockRecover<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockRecover<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
