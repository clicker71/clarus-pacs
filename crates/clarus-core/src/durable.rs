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
            let dir = match std::fs::File::open(current) {
                Ok(d) => d,
                Err(_) => return Ok(()),
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
                let dir = match std::fs::File::open(current) {
                    Ok(d) => d,
                    Err(_) => return Ok(()),
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
    let d = match std::fs::File::open(dir) {
        Ok(d) => d,
        Err(_) => return Ok(()),
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

    /// A directory that cannot be OPENED for read (mode 0300: write+search,
    /// no read) cannot be fsync'd at all, so the swallow must SKIP the
    /// barrier - not mask its failure. Proves the policy by the counter: the
    /// call succeeds AND `syncs_observed` does not advance. Reverting the
    /// swallow (propagating the open failure) makes this test fail.
    #[cfg(unix)]
    #[test]
    fn sync_dir_entry_swallows_open_failure_without_a_barrier() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o300))
            .expect("chmod 0300");
        let before = syncs_observed();
        sync_dir_entry(dir.path()).expect("open failure is swallowed");
        assert_eq!(
            syncs_observed(),
            before,
            "a swallowed open must not run the barrier"
        );
    }

    /// The terminal `.` open is swallowed by the SAME policy as the loop and
    /// [`sync_dir_entry`]: a process whose cwd cannot be opened for read
    /// (mode 0300) flushes the readable `new` level and SKIPS `.`, so a
    /// relative `new/commitments` write succeeds instead of erroring every
    /// publish. Reverting the swallow (keeping the `?` on the `.` open) makes
    /// `sync_new_root_name` return the propagated error and this test fails.
    #[cfg(unix)]
    #[test]
    fn sync_new_root_name_swallows_the_terminal_dot_open() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(base.path().join("new")).expect("new dir");

        // chdir is process-wide: serialize it so no sibling test observes a
        // moved cwd mid-assert.
        static CHDIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
        // Mode 0300: write+search, no read - `.` (base itself) cannot be
        // opened for the O_RDONLY directory fsync.
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o300))
            .expect("chmod 0300");

        let before = syncs_observed();
        sync_new_root_name(Path::new("new/commitments")).expect("open failure is swallowed");
        // The counter is process-wide and sibling tests flush concurrently, so
        // only a LOWER bound is observable here: the walk flushed the readable
        // `new` level and did not error on the unreadable `.` (the `expect`
        // above is what proves the swallow).
        assert!(
            syncs_observed() >= before + 1,
            "the readable `new` flush must run even though `.` cannot be opened"
        );

        // Restore permissions so the tempdir can be removed on drop.
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
    }
}
