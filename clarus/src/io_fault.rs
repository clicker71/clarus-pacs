// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Test-only I/O fault injection.
//!
//! Several production paths are only correct because of what they do when a
//! write FAILS: the audit trail rolls a torn record back, the log writer
//! retries opening a file, the access journal reports a batch it could not
//! persist. None of those branches can be reached through `std::fs`, which
//! cannot be told to fail on demand, so the tests for them used to be written
//! against timing and passed with the fix removed - the vacuity this module
//! exists to remove.
//!
//! One seam, keyed by a short name, so every I/O-error finding points at the
//! SAME mechanism instead of each growing its own hack. Nothing here is
//! compiled outside `cfg(test)`, and no product path branches on it in a
//! release build.

#![cfg(test)]

use std::collections::HashSet;
use std::sync::Mutex;

/// Which named write path should fail, and with what error kind.
static ARMED: Mutex<Option<(String, std::io::ErrorKind)>> = Mutex::new(None);

/// Names that have already been fired, so a single arming fails exactly one
/// write. A retrying caller therefore observes the failure once and then
/// succeeds - which is what the "retry after a failed open" tests need.
static FIRED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// The thread that called `arm`, for [`take_failure_on_arming_thread`].
static OWNER: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

/// Fail the next write to `site` with `kind`.
///
/// `kind` is usually `ErrorKind::StorageFull` (ENOSPC), the trigger every
/// durability finding names. Call [`clear`] afterwards; the guard returned by
/// [`arm_scoped`] does it for you.
pub fn arm(site: &str, kind: std::io::ErrorKind) {
    *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = Some((site.to_string(), kind));
    *FIRED.lock().unwrap_or_else(|e| e.into_inner()) = Some(HashSet::new());
    *OWNER.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
}

/// Disarm. Every armed site succeeds again.
pub fn clear() {
    *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *FIRED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *OWNER.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Arm a site for the duration of a closure, clearing even on panic.
///
/// A test that panics mid-assert would otherwise leave the seam armed and
/// poison every later test that touches the same path.
pub fn arm_scoped<F: FnOnce()>(site: &str, kind: std::io::ErrorKind, f: F) {
    // Serialise armed tests: one process-wide arming, so two of them running
    // in parallel would see each other's failure.
    static SEAM_LOCK: Mutex<()> = Mutex::new(());
    let _guard = SEAM_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    arm(site, kind);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    clear();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Hold the injected window open long enough for a competing writer to land
/// inside it.
///
/// A real failing write is over in microseconds, so a test cannot interleave
/// with it by luck. This pause runs only on the path where a failure was just
/// injected, so a normal append never pays it.
pub fn hold_window_open() {
    std::thread::sleep(std::time::Duration::from_millis(250));
}

/// The error this seam should produce for `site` right now, if any.
///
/// Returns `Some` at most ONCE per arming: the failure fires, then the seam
/// goes quiet, so a retry path can be observed recovering.
#[must_use]
pub fn take_failure(site: &str) -> Option<std::io::Error> {
    with_armed_site(site, std::io::Error::from)
}

/// As [`take_failure`], but only for the thread that armed the site.
///
/// A plain arming is process-wide, so in a test that spawns a competing writer
/// the failure could land in EITHER thread - and a test that needs the failure
/// in a specific writer (the one whose rollback must not erase the other's row)
/// cannot express that. The owning thread is recorded at `arm` time so the
/// competing writer is unaffected.
#[must_use]
pub fn take_failure_on_arming_thread(site: &str) -> Option<std::io::Error> {
    let owner = *OWNER.lock().unwrap_or_else(|e| e.into_inner());
    if owner != Some(std::thread::current().id()) {
        return None;
    }
    with_armed_site(site, std::io::Error::from)
}

/// Shared body of the two `take_failure` forms.
fn with_armed_site<F>(site: &str, make: F) -> Option<std::io::Error>
where
    F: FnOnce(std::io::ErrorKind) -> std::io::Error,
{
    let kind = {
        let armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        let (want, kind) = armed.as_ref()?;
        if want != site {
            return None;
        }
        *kind
    };

    let mut fired = FIRED.lock().unwrap_or_else(|e| e.into_inner());
    let fired = fired.get_or_insert_with(HashSet::new);
    if !fired.insert(site.to_string()) {
        // Already fired once for this arming: let the retry succeed.
        return None;
    }
    Some(make(kind))
}

/// Whether `site` is currently armed (for assertions about call order).
#[must_use]
pub fn is_armed(site: &str) -> bool {
    ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|(want, _)| want == site)
}

// -- The production write paths this seam serves -------------------------

/// Site name for the audit trail's record append.
pub const SITE_AUDIT_APPEND: &str = "audit.append";
/// Site name for the audit trail's daily-file open.
pub const SITE_AUDIT_OPEN: &str = "audit.open";
/// Site name for the log writer's daily-file open.
pub const SITE_LOG_OPEN: &str = "log.open";
/// Site name for the log writer's line write.
pub const SITE_LOG_WRITE: &str = "log.write";
/// Site name for the access journal's batch write.
pub const SITE_ACCESS_JOURNAL: &str = "access.journal";

#[cfg(test)]
mod tests {
    use super::*;

    /// The single arming is process-wide, so the tests that use it must not
    /// run in parallel - a second test arming a different site would clear or
    /// consume the first one's failure.
    static SEAM_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn locked<F: FnOnce()>(f: F) {
        let _g = SEAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        f();
        clear();
    }

    /// The seam fails once, then lets the retry through.
    ///
    /// A seam that failed forever could not express "open failed, then the
    /// operator fixed the permissions and the next message succeeded", which
    /// is exactly the log-writer retry rule.
    #[test]
    fn an_armed_site_fails_exactly_once() {
        locked(|| {
            arm("probe.once", std::io::ErrorKind::StorageFull);
            let first = take_failure("probe.once").expect("first write fails");
            assert_eq!(first.kind(), std::io::ErrorKind::StorageFull);
            assert!(
                take_failure("probe.once").is_none(),
                "the retry must succeed, or the retry path is untestable"
            );
        });
    }

    /// An unarmed or differently-armed site is untouched.
    #[test]
    fn only_the_armed_site_fails() {
        locked(|| {
            assert!(take_failure("probe.a").is_none(), "nothing armed");
            arm("probe.a", std::io::ErrorKind::StorageFull);
            assert!(
                take_failure("probe.b").is_none(),
                "a different site must not be affected"
            );
            assert!(take_failure("probe.a").is_some(), "the armed site fails");
        });
    }

    /// A panicking test must not leave the seam armed.
    #[test]
    fn a_panicking_scope_disarms() {
        let _g = SEAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let caught = std::panic::catch_unwind(|| {
            arm_scoped("probe.panic", std::io::ErrorKind::StorageFull, || {
                panic!("boom")
            })
        });
        assert!(caught.is_err(), "the panic propagates");
        assert!(
            take_failure("probe.panic").is_none(),
            "the seam must be clear after a panicking scope"
        );
        assert!(!is_armed("probe.panic"));
    }
}
