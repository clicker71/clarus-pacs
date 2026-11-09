// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Graceful shutdown coordination (E-008).
//!
//! HTTP runs on its own threads, so shutdown cannot rely on a tokio
//! runtime draining. The coordination here uses plain atomics + a std Mutex
//! condition:
//!
//! - A signal thread sets a flag and wakes the shutdown gate (no timer inside;
//!   a second signal during drain forces immediate exit).
//! - The accept thread observes the flag and stops calling `accept`.
//! - Worker threads finish the in-flight request, then exit.
//! - `main` waits for the active-connection count to reach zero or a drain
//!   ceiling before joining and stopping the DbActor.
//!
//! This module is deliberately small and dependency-free (std only).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Shared shutdown state, cloned into the accept thread and each worker.
#[derive(Clone)]
pub struct Shutdown {
    inner: Arc<Inner>,
}

/// `#[repr(C)]` pins the field order (Rust otherwise reorders to minimize
/// padding). The 32-byte pad between the signal booleans and `active` splits
/// the struct across two 64B cache lines: `active` is bumped by every
/// connection worker on each keep-alive cycle, while `stop`/`force` are polled
/// by the accept thread each iteration. Without the split, every worker's
/// `active.fetch_add` would invalidate the line the accept loop keeps reading.
#[repr(C)]
struct Inner {
    /// Raised when a termination signal arrives (or the drain ceiling).
    stop: AtomicBool,
    /// Forced immediate-exit (second signal during drain).
    force: AtomicBool,
    /// Padding: pushes `active` onto a distinct 64B cache line from
    /// `stop`/`force` (offset 64 past the struct base under #[repr(C)]).
    _pad: [u8; 56],
    /// Active connection count (worker threads holding a Conn).
    active: AtomicUsize,
    /// Wake-up gate for the signal thread -> shutdown coordinator.
    gate: Mutex<()>,
    cv: Condvar,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                stop: AtomicBool::new(false),
                force: AtomicBool::new(false),
                _pad: [0u8; 56],
                active: AtomicUsize::new(0),
                gate: Mutex::new(()),
                cv: Condvar::new(),
            }),
        }
    }

    /// Begin shutdown (from the signal handler). Idempotent.
    pub fn request_stop(&self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        // Wake any waiter so main re-checks active == 0 promptly.
        let _lock = self.inner.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.inner.cv.notify_all();
    }

    /// Force immediate exit (second signal during drain).
    pub fn request_force(&self) {
        self.inner.force.store(true, Ordering::Relaxed);
        self.request_stop();
    }

    /// Has a stop been requested?
    pub fn stopping(&self) -> bool {
        self.inner.stop.load(Ordering::Relaxed)
    }

    /// Is an immediate (forced) exit required?
    pub fn forced(&self) -> bool {
        self.inner.force.load(Ordering::Relaxed)
    }

    /// Increment the active-connection count (worker entry).
    pub fn enter(&self) {
        self.inner.active.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the active-connection count (worker exit).
    pub fn leave(&self) {
        self.inner.active.fetch_sub(1, Ordering::Relaxed);
    }

    /// Current active-connection count.
    pub fn active(&self) -> usize {
        self.inner.active.load(Ordering::Relaxed)
    }

    /// Block until the stop flag is raised or `force`.
    ///
    /// Returns `true` when shutdown was requested, `false` on a spurious wake
    /// that did not set the flag (caller loops).
    pub fn wait_for_stop(&self) -> bool {
        let guard = self.inner.gate.lock().unwrap_or_else(|e| e.into_inner());
        // Re-check the flag UNDER the lock. `notify_all` can be lost between a
        // caller's `request_stop()` and this wait acquiring the lock, and a
        // bare `wait_timeout` then slept the full 30 s on a shutdown that had
        // already been requested.
        let _ = self
            .inner
            .cv
            .wait_timeout_while(guard, std::time::Duration::from_secs(30), |_| {
                !self.stopping()
            })
            .unwrap_or_else(|e| e.into_inner());
        self.stopping()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cache-line base (64B) of a field reference.
    fn line<T>(v: &T) -> usize {
        (v as *const T as usize) & !63usize
    }

    #[test]
    fn stop_and_force_are_on_distinct_cache_line_from_active() {
        // `active` is bumped by every connection worker; `stop`/`force` are
        // polled by the accept loop. #[repr(C)] + the 56B pad must keep them
        // on separate 64B lines so worker RMWs do not invalidate the line the
        // accept loop keeps reading.
        let s = Shutdown::new();
        let stop = line(&s.inner.stop);
        let force = line(&s.inner.force);
        let active = line(&s.inner.active);
        assert_ne!(stop, active, "stop and active share a cache line");
        assert_ne!(force, active, "force and active share a cache line");
    }
}
