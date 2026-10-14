//! Small pieces of plumbing the rest of the crate would otherwise repeat.

use std::sync::{Mutex, MutexGuard};

/// Lock `mutex`, recovering from poisoning.
///
/// The release profile is built with `panic = "abort"`, so a thread that panicked
/// while holding a lock took the whole process with it and there is nothing left
/// to recover: under the profile the phone actually ships, this cannot fail.
///
/// The test and development profiles do unwind, and there a poisoned lock is
/// still perfectly usable. Poisoning only records that *some* thread died; what
/// the lock was protecting is the data itself, which no panic has touched. So
/// the recovery hands back the inner value instead of propagating a second panic
/// that would only bury the one that caused it - a shutdown path or an audio
/// thread that gives up is a much better outcome than a phone that is gone.
///
/// Collecting this in one place is the other half of the reason: the lock sites
/// say what they protect, not how they behave when the thread holding them dies.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}