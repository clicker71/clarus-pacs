// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shared durability primitives for the file-backed stores.
//!
//! The commitment, workflow (MWL/MPPS), UPS and study-manifest stores all
//! publish a record with the same barrier order: the temp file is flushed
//! (`sync_all`) BEFORE it is renamed/linked into place, and the directory entry
//! that names the record is flushed after. This module owns the flush itself
//! and the one half of the order the stores used to each miss: persisting the
//! NAME of a directory `create_dir_all` just created.

use std::io;
use std::path::Path;

/// The flush itself, split out so a test can pin that it RUNS.
///
/// `sync_all` leaves no trace a test can read back - the bytes are in the page
/// cache whether or not it was called - so a test that only re-reads the file
/// would pass with the barrier deleted. The counter below is the observable
/// side of the call, which is what makes the barrier falsifiable.
pub(crate) fn sync_barrier(file: &std::fs::File) -> io::Result<()> {
    let result = file.sync_all();
    SYNC_BARRIERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(all(unix, test))]
    note_scoped_barrier();
    result
}

/// How many times a write has been flushed to the medium.
///
/// Test-observable proof that the barrier runs; the count is otherwise unused.
static SYNC_BARRIERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Number of files flushed so far, for the durability tests.
#[cfg(test)]
pub(crate) fn syncs_observed() -> u64 {
    SYNC_BARRIERS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Directory barriers SKIPPED because the directory could not be opened for
/// read.
///
/// The open failure itself stays swallowed (the availability tradeoff
/// documented on [`sync_new_root_name`]), but it is not invisible: a
/// deployment whose archive sits under a mode-0300 parent is publishing names
/// it cannot make durable, and that is a fact an operator must be able to see
/// rather than derive from a crash.
///
/// Unix-only because the barrier itself is: Windows cannot open a directory
/// for `FlushFileBuffers`, so nothing is ever skipped there.
#[cfg(unix)]
static SYNC_BARRIERS_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many directory barriers have been skipped for an unopenable directory.
///
/// Non-zero means the archive holds at least one name that will not survive a
/// crash. Steady growth is the signal; a single bump on an otherwise healthy
/// host is a permissions problem worth reporting.
#[cfg(all(unix, test))]
pub(crate) fn skipped_barriers() -> u64 {
    SYNC_BARRIERS_SKIPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record a directory-barrier skip. Kept next to `syncs_observed` so the two
/// halves of the barrier story stay in one place.
#[cfg(unix)]
fn note_skipped_barrier() {
    SYNC_BARRIERS_SKIPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    note_scoped_skip();
}

/// Run `f` while counting the barriers AND the skipped barriers it triggers.
///
/// [`skipped_barriers`] is process-wide like [`syncs_observed`], so a sibling
/// test that hits an unopenable directory between this test's `before` and
/// `after` readings breaks any equality assertion - measured: the version of
/// this helper that returned only the barrier count reported 2 skipped where
/// the operation under test skipped 1, and the test failed under the parallel
/// runner while passing in isolation. Returning both counts from ONE scope
/// closes that hole for the skip side as well as the barrier side.
#[cfg(all(unix, test))]
pub(crate) fn count_barriers_and_skips_in_scope<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
    SCOPED_BARRIERS.with(|c| c.set(Some(0)));
    SCOPED_SKIPS.with(|c| c.set(Some(0)));
    let out = f();
    let n = SCOPED_BARRIERS.with(|c| c.replace(None)).unwrap_or(0);
    let s = SCOPED_SKIPS.with(|c| c.replace(None)).unwrap_or(0);
    (out, n, s)
}

#[cfg(all(unix, test))]
thread_local! {
    static SCOPED_SKIPS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Attribute one skipped barrier to the current thread's scope, if one is open.
#[cfg(all(unix, test))]
fn note_scoped_skip() {
    SCOPED_SKIPS.with(|c| {
        if let Some(n) = c.get() {
            c.set(Some(n + 1));
        }
    });
}

/// Run `f` while counting ONLY the barriers `f` itself triggers.
///
/// [`syncs_observed`] is process-wide, so a parallel test that flushes a
/// barrier of its own between this test's `before` and `after` readings makes
/// any equality assertion meaningless. The counting is thread-local here
/// because the tests that need it call the sync path directly on their own
/// thread; a barrier on a worker thread is not attributed to the scope, which
/// is exactly the isolation these assertions need.
///
/// Test-only: the production path stays a bare relaxed increment.
#[cfg(all(unix, test))]
pub(crate) fn count_barriers_in_scope<T>(f: impl FnOnce() -> T) -> (T, u64) {
    SCOPED_BARRIERS.with(|c| c.set(Some(0)));
    let out = f();
    let n = SCOPED_BARRIERS.with(|c| c.replace(None)).unwrap_or(0);
    (out, n)
}

#[cfg(all(unix, test))]
thread_local! {
    static SCOPED_BARRIERS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Attribute one barrier to the current thread's scope, if one is open.
#[cfg(all(unix, test))]
fn note_scoped_barrier() {
    SCOPED_BARRIERS.with(|c| {
        if let Some(n) = c.get() {
            c.set(Some(n + 1));
        }
    });
}

/// Persist a directory's NAME chain, entry by entry.
///
/// `create_dir_all` makes the directories and flushing a directory persists the
/// entries INSIDE it; neither makes a directory's own name durable in its
/// PARENT. A crash can therefore leave the contents durable while the path from
/// a durable ancestor down to the store is missing, so the whole store looks
/// empty.
///
/// Two gaps this closes:
/// - a bare relative root (`"commitments"`) has an EMPTY parent, but the
///   directory whose entry must be persisted is `"."` - filtering it out left
///   the name unflushed;
/// - `create_dir_all("/a/new/commitments")` can create several levels, and only
///   flushing the immediate parent leaves `/a` without its new `new` entry.
///
/// So every level from the root up to the first ancestor that already exists is
/// flushed. The policy is uniform across the whole module and stated once here:
/// an OPEN failure at ANY level - the loop below, the terminal `.`, and
/// [`sync_dir_entry`] - is swallowed (the name is simply not makeable-durable
/// there), while a BARRIER failure on a directory we DID open is propagated.
/// The swallow is a deliberate availability/durability tradeoff, not an
/// accidental `unwrap_or_default`-style swallow:
///
/// - On Unix a directory is fsync'd through an `O_RDONLY` open (`O_WRONLY` on a
///   directory is `EISDIR`, an `O_PATH` fd cannot be fsync'd). A parent that
///   grants write+search but NOT read (e.g. mode 0300) cannot be opened, so its
///   entry CANNOT be fsync'd by this process - the name is simply not
///   makeable-durable there.
/// - Propagating that open failure would turn a narrow crash window (the name
///   may not survive a crash) into a hard availability failure (EVERY publish
///   errors in that deployment), while the record DATA is already durable.
///
/// Unix-only by necessity: Windows cannot open a directory for
/// `FlushFileBuffers`, so the whole walk is `#[cfg(unix)]` and on Windows this
/// function is the documented no-op.
pub(crate) fn sync_new_root_name(root: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        // Deepest first: the level holding the store's name, then upward.
        let mut current: &Path = match root.parent() {
            // An empty parent means the root sits in the current directory, and
            // `"."` IS the directory whose entry carries the name.
            Some(p) if p.as_os_str().is_empty() => Path::new("."),
            Some(p) => p,
            None => return Ok(()),
        };
        loop {
            // Through `sync_barrier`, not a bare `sync_all`: the counter is the
            // only observable side of a flush.
            //
            // The two failures are NOT the same. Failing to OPEN a level means
            // the directory is not openable for read - the canonical case is a
            // parent granting write+search but not read (mode 0300), whose
            // entry cannot be fsync'd at all (a directory is only fsync'able
            // through an O_RDONLY open). Stopping the walk there is the chosen
            // tradeoff: the name may not survive a crash, but refusing the
            // write would fail every publish in that deployment while the
            // record DATA is durable. A barrier FAILURE on a level we DID open
            // is different: that directory holds the newly created name, so its
            // entry may still be in the page cache, and reporting success would
            // let a write answer 200 for a store the next boot cannot find.
            let opened = match take_injected_open_error(current) {
                Some(err) => Err(err),
                None => std::fs::File::open(current),
            };
            let dir = match opened {
                Ok(d) => d,
                Err(_) => {
                    note_skipped_barrier();
                    return Ok(());
                }
            };
            sync_barrier(&dir)?;
            drop(dir);
            match current.parent() {
                Some(p) if !p.as_os_str().is_empty() => current = p,
                Some(_) => current = Path::new("."),
                None => return Ok(()),
            }
            // `.` is the terminal level: flushing it above ends the walk.
            // The open is swallowed by the SAME policy as the loop above and
            // `sync_dir_entry`: a cwd that cannot be opened for read cannot
            // be fsync'd at all, so the name is simply not makeable-durable
            // there. The barrier below still propagates - `.` is a directory
            // we DID open, so its entry may still be in the page cache.
            if current == Path::new(".") {
                let opened = match take_injected_open_error(current) {
                    Some(err) => Err(err),
                    None => std::fs::File::open(current),
                };
                let dir = match opened {
                    Ok(d) => d,
                    Err(_) => {
                        note_skipped_barrier();
                        return Ok(());
                    }
                };
                sync_barrier(&dir)?;
                return Ok(());
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(())
    }
}

/// Directory paths whose NEXT barrier is forced to fail - the test seam for
/// "a barrier failure must not be reported as stored". Process-wide (not
/// thread-local): a store write can run on a worker thread. Empty in
/// production, so a store pays one read lock and an empty-slice scan.
///
/// Unix-only like the directory barrier itself: Windows has no directory
/// fsync, so the seam (and the tests that drive it) have no target there.
#[cfg(unix)]
static BARRIER_FAIL_NEXT: std::sync::RwLock<Vec<std::path::PathBuf>> =
    std::sync::RwLock::new(Vec::new());

/// Test seam: make the NEXT directory barrier on `dir` fail with
/// `StorageFull`, once.
///
/// Test-only (like [`syncs_observed`]): a store test injects the
/// post-`hard_link` barrier failure without a filesystem trick. One-shot by
/// design: the injected error is consumed by the next barrier on that
/// directory, so a retry sees the healthy barrier.
#[cfg(all(unix, test))]
pub(crate) fn fail_next_barrier_for_test(dir: &Path) {
    if let Ok(mut list) = BARRIER_FAIL_NEXT.write() {
        list.push(dir.to_path_buf());
    }
}

/// Consume a pending one-shot barrier failure for `dir`, if one is armed.
#[cfg(unix)]
fn take_injected_barrier_error(dir: &Path) -> Option<io::Error> {
    let mut list = BARRIER_FAIL_NEXT.write().ok()?;
    let pos = list.iter().position(|p| p == dir)?;
    list.remove(pos);
    Some(io::Error::new(
        io::ErrorKind::StorageFull,
        "injected dir barrier failure",
    ))
}

/// Directory paths whose NEXT OPEN for the barrier is forced to fail.
///
/// Separate from [`BARRIER_FAIL_NEXT`] because the two exercise OPPOSITE
/// policies: a barrier failure on a directory we opened is PROPAGATED, while an
/// open failure is SWALLOWED. Injecting the open failure is the only way to
/// test the swallow on every host - the filesystem route (mode 0300) depends on
/// the process NOT holding `CAP_DAC_READ_SEARCH`, and a privileged host gets a
/// different (also correct) path, so the assertion had to be conditional and
/// the covered-ness of the policy varied with how the test was launched.
#[cfg(unix)]
static OPEN_FAIL_NEXT: std::sync::RwLock<Vec<std::path::PathBuf>> =
    std::sync::RwLock::new(Vec::new());

/// Test seam: make the NEXT directory OPEN on `dir` fail with `PermissionDenied`,
/// once.
///
/// This reproduces exactly what a mode-0300 parent does to an unprivileged
/// process, without depending on the caller's capabilities.
#[cfg(all(unix, test))]
pub(crate) fn fail_next_open_for_test(dir: &Path) {
    if let Ok(mut list) = OPEN_FAIL_NEXT.write() {
        list.push(dir.to_path_buf());
    }
}

/// Consume a pending one-shot open failure for `dir`, if one is armed.
#[cfg(unix)]
fn take_injected_open_error(dir: &Path) -> Option<io::Error> {
    let mut list = OPEN_FAIL_NEXT.write().ok()?;
    let pos = list.iter().position(|p| p == dir)?;
    list.remove(pos);
    Some(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "injected dir open failure",
    ))
}

/// Flush the directory entry that names `dir` through [`sync_barrier`].
///
/// The seam above is honored first: a test can force exactly one failure, so
/// a failed Ok-arm barrier (which leaves the linked target in place) is
/// followed by a retry whose conflict-path barrier is healthy. The three
/// record stores share this so their Ok and Duplicate/CasConflict arms cannot
/// drift apart. Unix-only by necessity: Windows cannot open a directory for
/// `FlushFileBuffers`.
///
/// The open failure is SWALLOWED, with the same tradeoff `sync_new_root_name`
/// documents: a directory that cannot be OPENED for read (a mode-0300 parent,
/// or a bare relative path whose cwd is unreadable) cannot be fsync'd at all,
/// so the name is simply not makeable-durable there. Propagating that open
/// failure would turn a narrow crash window into a hard availability failure
/// (every publish errors in that deployment) while the record DATA is already
/// durable. A barrier failure on a directory we DID open is still propagated
/// - that directory holds the newly created name, so reporting success would
///   answer 200 for a store the next boot cannot find.
#[cfg(unix)]
pub(crate) fn sync_dir_entry(dir: &Path) -> io::Result<()> {
    if let Some(err) = take_injected_barrier_error(dir) {
        return Err(err);
    }
    // The OPEN-failure seam is checked before the real open so the swallow is
    // reachable on any host; see [`fail_next_open_for_test`].
    let opened = match take_injected_open_error(dir) {
        Some(err) => Err(err),
        None => std::fs::File::open(dir),
    };
    let d = match opened {
        Ok(d) => d,
        Err(_) => {
            note_skipped_barrier();
            return Ok(());
        }
    };
    sync_barrier(&d)
}

/// The directory whose entry must be flushed after a rename/hard_link names
/// `path`. A bare relative path ("study.manifest") has an EMPTY parent, which
/// normalises to "." - the directory that actually carries the name. `None`
/// only for a path with no parent component at all (the filesystem root).
// On Windows every call site is `#[cfg(unix)]`, so the non-test lib build
// sees no use; the path logic itself is platform-free and is exercised by the
// unit test on every platform.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn parent_entry_dir(path: &Path) -> Option<&Path> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => Some(p),
        Some(_) => Some(Path::new(".")),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that change the process cwd.
    ///
    /// `set_current_dir` is process-wide, and both tests that use it also arm
    /// an open failure on `.`. One shared lock is what keeps their cwd and
    /// their injected failure from being observed by each other.
    #[cfg(unix)]
    static CHDIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn parent_entry_dir_normalises_the_empty_parent() {
        assert_eq!(
            parent_entry_dir(Path::new("study.manifest")),
            Some(Path::new("."))
        );
        assert_eq!(
            parent_entry_dir(Path::new("manifests/1.2.3.manifest")),
            Some(Path::new("manifests"))
        );
        assert_eq!(parent_entry_dir(Path::new("/")), None);
    }

    /// The operator-facing skip counter advances when a skip happens.
    ///
    /// `skipped_barriers` is the ONLY signal that a deployment is publishing
    /// names it cannot make durable, so it must be reachable and non-static.
    /// It is read as a MONOTONIC bound rather than a delta: the counter is
    /// process-wide, so an exact delta would race a sibling test (the defect
    /// [`count_barriers_and_skips_in_scope`] exists to remove).
    #[cfg(unix)]
    #[test]
    fn the_skip_counter_advances() {
        let dir = tempfile::tempdir().expect("tempdir");
        let before = skipped_barriers();
        fail_next_open_for_test(dir.path());
        sync_dir_entry(dir.path()).expect("the swallow succeeds");
        assert!(
            skipped_barriers() > before,
            "a skipped barrier must be visible to an operator"
        );
    }

    /// A directory that cannot be OPENED for read (mode 0300: write+search,
    /// no read) cannot be fsync'd at all, so the swallow must SKIP the
    /// barrier - not mask its failure. Proves the policy by the barrier count:
    /// the call succeeds AND no barrier ran. Reverting the swallow
    /// (propagating the open failure) makes this test fail.
    ///
    /// The count is taken from a THREAD-LOCAL scope, not the process-wide
    /// [`syncs_observed`]: sibling tests flush concurrently, so `after ==
    /// before` against the global counter would fail even when the swallow is
    /// correct.
    ///
    /// `open` must be shown to fail before the skip means anything.
    /// `CAP_DAC_READ_SEARCH` bypasses the mode bits, and on that host `open`
    /// succeeds and the barrier legitimately RUNS - so the precondition is
    /// asserted rather than assumed, and this test is meaningless without it.
    ///
    /// The filesystem route cannot establish that precondition on every host:
    /// `CAP_DAC_READ_SEARCH` / `CAP_DAC_OVERRIDE` bypass the mode bits, and the
    /// old version then returned early, leaving the policy UNTESTED wherever
    /// the suite ran privileged. The failure is now INJECTED, which yields the
    /// same `PermissionDenied` an unprivileged process sees, so the assertion
    /// is unconditional and the test always covers the swallow.
    #[cfg(unix)]
    #[test]
    fn sync_dir_entry_swallows_open_failure_without_a_barrier() {
        let dir = tempfile::tempdir().expect("tempdir");
        fail_next_open_for_test(dir.path());

        let (res, barriers, skips) =
            count_barriers_and_skips_in_scope(|| sync_dir_entry(dir.path()));
        res.expect("open failure is swallowed");
        assert_eq!(barriers, 0, "a swallowed open must not run the barrier");
        // The SKIP count comes from the same scope, not from the process-wide
        // counter: a sibling test that hits an unopenable directory between
        // the two readings made this delta 2 instead of 1 and failed under the
        // parallel runner.
        assert_eq!(
            skips, 1,
            "the skipped barrier is counted, not silently dropped"
        );
    }

    /// The terminal `.` open is swallowed by the SAME policy as the loop and
    /// [`sync_dir_entry`]: a process whose cwd cannot be opened for read
    /// flushes the readable `new` level and SKIPS `.`, so a relative
    /// `new/commitments` write succeeds instead of erroring every publish.
    /// Reverting the swallow (keeping the `?` on the `.` open) makes
    /// `sync_new_root_name` return the propagated error and this test fails.
    ///
    /// The failure is INJECTED rather than produced with mode 0300: a
    /// capability-carrying process opens the directory anyway, and the old
    /// conditional (`if dot_unopenable .. else ..`) then asserted a DIFFERENT
    /// path, so the swallow itself went uncovered on such a host. Injection
    /// makes the exercised path identical everywhere.
    ///
    /// The counts come from the thread-local scope, so the assertions are
    /// exact rather than lower bounds: exactly ONE barrier - the `new` level -
    /// with `.` skipped.
    #[cfg(unix)]
    #[test]
    fn sync_new_root_name_swallows_the_terminal_dot_open() {
        let base = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(base.path().join("new")).expect("new dir");

        // chdir is process-wide: serialize it so no sibling test observes a
        // moved cwd mid-assert. The SAME lock is shared with the readable-walk
        // test below: both arm an open failure on `.` and both change the cwd,
        // so two distinct locks let them interleave and consume each other's
        // injection (measured: the skip test saw 2 barriers instead of 1).
        let _lock = CHDIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prev = std::env::current_dir().expect("cwd");
        // Restore the cwd on every exit path (assertions panic too).
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd = CwdGuard(prev);

        std::env::set_current_dir(base.path()).expect("chdir into base");
        // Arm the OPEN failure for `.` only: the `new` level opens normally and
        // flushes, `.` cannot be opened and is skipped.
        fail_next_open_for_test(Path::new("."));

        let (res, barriers, skips) =
            count_barriers_and_skips_in_scope(|| sync_new_root_name(Path::new("new/commitments")));
        res.expect("open failure is swallowed");
        assert_eq!(
            barriers, 1,
            "only the readable `new` level flushes; the unopenable `.` is skipped"
        );
        assert_eq!(
            skips, 1,
            "the skipped `.` barrier is counted, not silently dropped"
        );
    }

    /// With no injected failure the walk flushes BOTH levels.
    ///
    /// The counterpart to the test above: it proves the skip in that test comes
    /// from the poisoned open and not from the walk stopping early for another
    /// reason.
    #[cfg(unix)]
    #[test]
    fn sync_new_root_name_flushes_every_level_when_all_are_readable() {
        let base = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(base.path().join("new")).expect("new dir");

        let _lock = CHDIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prev = std::env::current_dir().expect("cwd");
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd = CwdGuard(prev);

        std::env::set_current_dir(base.path()).expect("chdir into base");
        let (res, barriers, skips) =
            count_barriers_and_skips_in_scope(|| sync_new_root_name(Path::new("new/commitments")));
        res.expect("a readable walk succeeds");
        assert_eq!(barriers, 2, "`new` and `.` both flush");
        // Only the SCOPED count is asserted: `skipped_barriers()` is
        // process-wide, so a sibling test's skip moves it between two readings
        // and the assertion becomes a race. That is exactly the defect this
        // helper exists to remove.
        assert_eq!(skips, 0, "nothing is skipped when every level is readable");
    }
}
