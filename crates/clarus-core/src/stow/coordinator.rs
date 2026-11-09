// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Study-scoped coordination: an in-process [`Mutex`] serializing manifest
//! writes.
//!
//! All manifest writers are threads of this single process (the
//! 17.08 v2: clarus / cltranscode / clbridge share no filesystem resources),
//! so mutual exclusion is an in-process `Mutex` - a crash drops it and the
//! manifest commit itself is atomic `tmp+rename`, so there is nothing to
//! clean up.
//!
//! D3 (28.08): the `StudyCoordinator` trait, the `CoordinatorKind` enum and
//! the fence token were cut - there was exactly one backend and no token
//! consumer (writer pre-check, 28.08). `acquire` is a plain function over the
//! global [`MANIFEST_MUTEX`]; a second backend may return when one actually
//! exists.

use std::sync::{Mutex, MutexGuard};

/// A held study-scoped lease. Released on drop when the mutex guard is
/// dropped (the in-process `Mutex`).
pub struct StudyLease {
    // Guard holding the in-process mutex; released on drop.
    _guard: MutexGuard<'static, ()>,
}

/// One global mutex serializing all manifest writes of this process.
///
/// All manifest writers are threads of this single process (the
/// 17.08 v2), so a `Mutex` is the honest mutual exclusion: a crash drops it,
/// and the manifest commit itself is atomic `tmp+rename`, so there is never a
/// torn manifest to clean up. Poison (a panic inside the critical section)
/// cannot leave a torn manifest either, so a poisoned mutex is safe to use.
static MANIFEST_MUTEX: Mutex<()> = Mutex::new(());

/// Acquire the exclusive manifest-write lease (the in-process `Mutex`).
///
/// Blocks until acquired. The lease is released on drop. No filesystem work:
/// the store directory is created by
/// [`super::manifest::write_study_manifest_full`], and the lease is a mutex
/// guard.
pub fn acquire() -> StudyLease {
    // Poison: a panic inside the critical section cannot leave a torn
    // manifest (tmp+rename is atomic), so a poisoned mutex is safe to use.
    let guard = MANIFEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    StudyLease { _guard: guard }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Acquire returns a lease that leaves nothing on disk (it is a Mutex).
    #[test]
    fn acquire_returns_lease() {
        let lease = acquire();
        drop(lease);
    }

    /// Concurrent acquires on distinct studies still serialize on the single
    /// global mutex: N threads each enter the critical section exactly once,
    /// never overlapping.
    #[test]
    fn double_acquire_serializes() {
        let threads = 8;

        // Shared flag: set while a thread is inside the critical section.
        // Any overlap (two threads inside at once) increments the counter.
        let inside = Arc::new(AtomicUsize::new(0));
        let max_overlap = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..threads {
            let inside = Arc::clone(&inside);
            let max_overlap = Arc::clone(&max_overlap);
            handles.push(std::thread::spawn(move || {
                let lease = acquire();
                // Critical section: bump in/out around a tiny sleep.
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                max_overlap.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(2));
                inside.fetch_sub(1, Ordering::SeqCst);
                drop(lease);
            }));
        }
        for h in handles {
            h.join().expect("join");
        }

        // The mutex lets only one thread into the section at a time.
        assert_eq!(
            max_overlap.load(Ordering::SeqCst),
            1,
            "critical section never overlapped",
        );
        assert_eq!(inside.load(Ordering::SeqCst), 0, "no thread left inside");
    }
}
