//! One poisoning policy for the tree's mutexes: a panic while holding a
//! lock must not cascade — the guarded state is still structurally valid
//! (a Vec mid-push, a String mid-append), so the guard is recovered and
//! the process carries on. A poisoned lock aborting every later lock
//! holder would turn one panicking stream into a dead session.

use std::sync::{Mutex, MutexGuard};

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
