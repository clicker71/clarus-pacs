// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Atomic manifest write.
//!
//! A manifest is committed with a single `rename` so readers never observe a
//! partial file. The encoding is produced first, buffered, then written to a
//! unique sibling `*.tmp` file and atomically renamed over the target.
//!
//! The temp name is unique per writer (pid + a process-wide counter, claimed
//! with `create_new`), so two writers to the same target never share an inode
//! and the caller does NOT need to hold any coordinator lease: the atomic
//! `rename` over the final name is the only mutual exclusion.
//!
//! # Durability model
//!
//! The temporary file is synced (`sync_all`) BEFORE the rename, and the
//! directory entry is synced AFTER it on Unix (`sync_all` on a directory
//! handle fails on Windows - `FlushFileBuffers` does not take a directory),
//! so a committed `Ok` means both the data and the name are on the medium.
//! A crash after the rename can no longer leave an empty or truncated
//! manifest, and a replace-rename can no longer lose BOTH the old and the new
//! file. A failed flush removes the temp and returns the error - it never
//! reports `Ok`. `std::fs::rename` maps to `MoveFileExW` with
//! `MOVEFILE_REPLACE_EXISTING` on Windows and to `rename(2)` on POSIX,
//! giving atomic replace on both platforms.

use crate::manifest::record::Manifest;
use std::io;
use std::path::Path;

/// Errors produced by [`write_manifest`].
#[derive(Debug)]
pub enum WriteManifestError {
    /// The manifest failed to encode or a path component was invalid.
    Encode(String),
    /// Underlying filesystem I/O failure.
    Io(io::Error),
}

impl std::fmt::Display for WriteManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteManifestError::Encode(e) => write!(f, "manifest encode failed: {e}"),
            WriteManifestError::Io(e) => write!(f, "manifest write I/O error: {e}"),
        }
    }
}

impl std::error::Error for WriteManifestError {}

/// Manifest paths whose atomic write is forced to fail - the test seam for
/// "a failed rewrite must not be reported as committed". Process-wide (not
/// thread-local): a test server handles the write on a worker thread, so the
/// seam must be visible across threads. Empty in production, so a commit pays
/// one uncontended read lock and an empty-slice scan.
static BLOCKED_MANIFEST_WRITES: std::sync::RwLock<Vec<std::path::PathBuf>> =
    std::sync::RwLock::new(Vec::new());

/// Test seam: make atomic writes to `path` fail until unblocked. Always compiled
/// (like `read_manifest_for_test`) so integration tests can reach the failed-
/// rewrite branch without a cross-platform filesystem trick.
#[doc(hidden)]
pub fn block_manifest_write_for_test(path: &Path) {
    if let Ok(mut blocked) = BLOCKED_MANIFEST_WRITES.write() {
        blocked.push(path.to_path_buf());
    }
}

/// Test seam: stop failing atomic writes for exactly `path`, leaving every
/// other blocked path untouched. Tests run on parallel threads in one binary,
/// so a global clear here would unblock another test's path mid-flight.
#[doc(hidden)]
pub fn unblock_manifest_write_for_test(path: &Path) {
    if let Ok(mut blocked) = BLOCKED_MANIFEST_WRITES.write() {
        blocked.retain(|p| p != path);
    }
}

/// Whether the atomic write of `path` is blocked by the test seam.
fn manifest_write_is_blocked(path: &Path) -> bool {
    BLOCKED_MANIFEST_WRITES
        .read()
        .map(|blocked| blocked.iter().any(|p| p == path))
        .unwrap_or(false)
}

/// Atomically write `manifest` to `path`.
///
/// The encoding is done first so the serialization can never be interleaved
/// with disk writes. The payload is then flushed to `{path}.tmp` and renamed
/// over `path` in one atomic step.
///
/// Routed through [`write_bytes_atomic_validated`], not `write_bytes_atomic`:
/// encoding SUCCEEDING is not the same as this codec being able to read the
/// result back. An attribute nested past the decoder's depth limit encodes
/// without complaint, so an unvalidated rename would replace a good manifest
/// with one the next reader rejects.
pub fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), WriteManifestError> {
    let bytes = manifest
        .encode()
        .map_err(|e| WriteManifestError::Encode(e.to_string()))?;
    write_bytes_atomic_validated(path, &bytes)
}

/// Write raw bytes atomically to `path`, decoding them FIRST.
///
/// The manifest writers share one invariant: a manifest this codec cannot
/// decode must never replace a good manifest on disk. `write_bytes_atomic`
/// renames first, so a caller that decodes AFTER it has already published the
/// bytes (and clobbered the previous manifest) by the time a decode error
/// surfaces. Validating the exact bytes BEFORE the rename turns a codec
/// regression into a failed write that leaves the previous good manifest
/// intact.
pub fn write_bytes_atomic_validated(path: &Path, bytes: &[u8]) -> Result<(), WriteManifestError> {
    crate::manifest::read::read_manifest_at(bytes)
        .map_err(|e| WriteManifestError::Encode(e.to_string()))?;
    write_bytes_atomic(path, bytes)
}

/// Publish `bytes` at `path` AND retire the study, in one atomic step.
///
/// A delete that empties a study has to leave the study's final shape visible
/// under the tombstone name. Writing the new content first and renaming after
/// leaves a window in which the LIVE name carries the retired shape - readers
/// still see the study, and a crash there publishes an empty manifest as if it
/// were a live study. Renaming the LIVE name onto the tombstone closes that
/// window: the live name disappears in the same step that the tombstone
/// appears.
///
/// `bytes` is validated by decoding it before anything moves, for the same
/// reason [`write_bytes_atomic_validated`] does: a codec regression must fail
/// the delete, not publish a name nothing can read.
///
/// When the retired shape DIFFERS from the bytes the live manifest holds, the
/// differing payload is written to the tombstone AFTER the rename. Two things
/// keep that safe:
/// - the tombstone existed only because of this rename, so a failed rewrite
///   leaves the PREVIOUS content (never a half-written file - the rewrite goes
///   through the same `tmp`+rename), and
/// - a retry is still possible: the live name is gone, so the caller must treat
///   this study as already retired. That is why the failure is reported rather
///   than swallowed.
pub fn publish_tombstone_bytes(
    path: &Path,
    tombstone: &Path,
    bytes: &[u8],
) -> Result<(), WriteManifestError> {
    crate::manifest::read::read_manifest_at(bytes)
        .map_err(|e| WriteManifestError::Encode(e.to_string()))?;

    // Read the ON-DISK payload BEFORE the rename, so the "does it differ"
    // question is answered against the live manifest and not against a
    // tombstone this function has already moved into place.
    //
    // A read failure is NOT a mismatch. `unwrap_or(true)` would treat an
    // unreadable live manifest as "differs", rewrite the tombstone, and return
    // Ok - retiring a study while silently discarding the fact that nobody
    // could read it. A live manifest that cannot be read is exactly the case a
    // recovery pass needs to know about, so it fails the retirement instead.
    let on_disk = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Nothing to retire: the caller's delete is already done.
            return Err(WriteManifestError::Io(e));
        }
        Err(e) => return Err(WriteManifestError::Io(e)),
    };

    std::fs::rename(path, tombstone).map_err(WriteManifestError::Io)?;
    // The NAME of the tombstone is a new directory entry: flush the directory
    // that holds it so a crash cannot lose the retirement that the caller is
    // about to report. Unix-only by necessity, same policy as the writers.
    #[cfg(unix)]
    if let Some(parent) = crate::durable::parent_entry_dir(tombstone)
        && let Err(e) = crate::durable::sync_dir_entry(parent)
    {
        return Err(WriteManifestError::Io(e));
    }
    // A same-directory rename keeps the original inode, so the tombstone now
    // holds the bytes the live manifest held. Only a DIFFERENT retired shape
    // needs the payload rewritten, and that rewrite must be durable before the
    // caller reports the retirement.
    if on_disk != bytes {
        write_bytes_atomic(tombstone, bytes)?;
    }
    Ok(())
}

/// Write raw bytes atomically to `path` via a `*.tmp` sibling + rename.
///
/// Exposed separately for callers that already hold an encoded manifest.
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), WriteManifestError> {
    // Test seam: report a failed write without touching the filesystem. The
    // check is a thread-local lookup on an empty list in production.
    if manifest_write_is_blocked(path) {
        return Err(WriteManifestError::Io(io::Error::new(
            io::ErrorKind::StorageFull,
            "injected manifest write failure",
        )));
    }

    // Ensure parent directories exist (first STOW for a study). Idempotent.
    // Guard against a bare empty parent (a relative path with no directory).
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(WriteManifestError::Io)?;
        // `create_dir_all` does not persist the NAME of the directories it
        // creates; a first write would otherwise flush the record and its
        // parent while `manifests/` could still vanish from ITS parent.
        crate::durable::sync_new_root_name(parent).map_err(WriteManifestError::Io)?;
    }

    // The temp is claimed with `create_new` (unique per writer), so two
    // writers to the same target never share an inode and a mapped reader can
    // never see one writer's partial bytes through the other's temp. The
    // claimed file is then filled through an OPEN handle and flushed before
    // the rename: on a re-STOW the rename replaces the existing inode, so a
    // crash between rename and flush used to lose BOTH the old and the new
    // manifest. A failed flush removes the temp and is a failed write.
    let tmp_path = tmp_sibling(path)?;
    let written = (|| -> io::Result<()> {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().write(true).open(&tmp_path)?;
        f.write_all(bytes)?;
        crate::durable::sync_barrier(&f)
    })();
    if let Err(e) = written {
        report_stale_temp(&tmp_path, &e);
        return Err(WriteManifestError::Io(e));
    }

    // Guard against leaving a stale tmp behind if rename fails.
    match std::fs::rename(&tmp_path, path) {
        Ok(()) => {
            // The data is durable; persist the directory entry that names it
            // too. Unix-only by necessity: Windows cannot open a directory
            // for `FlushFileBuffers`. A bare relative name normalises its
            // empty parent to `.` (the directory that carries the name).
            #[cfg(unix)]
            if let Some(parent) = crate::durable::parent_entry_dir(path)
                && let Err(e) = crate::durable::sync_dir_entry(parent)
            {
                return Err(WriteManifestError::Io(e));
            }
            Ok(())
        }
        Err(e) => {
            report_stale_temp(&tmp_path, &e);
            Err(WriteManifestError::Io(e))
        }
    }
}

/// Note a temp file that survived the failure that made it garbage.
///
/// The primary error is what the caller must see, so this never replaces it.
/// What it must NOT do is stay silent: a leftover `*.tmp` sits in `manifests/`
/// forever (nothing sweeps that directory), and the only moment the reason is
/// known is here. A crate that reports through a counter rather than a logger
/// keeps the observation where an operator can already read it.
fn report_stale_temp(tmp_path: &Path, primary: &io::Error) {
    match std::fs::remove_file(tmp_path) {
        Ok(()) => {}
        Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => {}
        Err(cleanup) => {
            let _ = (tmp_path, primary, cleanup);
            STALE_TEMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Manifest temp files a failed write could not remove.
///
/// Each one is an unusable `*.tmp` left in the manifest directory for good.
/// Non-zero means the deployment leaks a temp file per failed write; reading it
/// is the only way to notice, because the caller is told about the primary
/// failure and nothing else.
static STALE_TEMPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many manifest temp files have been left behind by a failed cleanup.
pub fn stale_temp_files() -> u64 {
    STALE_TEMPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Build a unique `{path}.{pid}.{seq}.tmp` sibling, creating it exclusively.
///
/// pid + a process-wide counter makes a collision rare but not impossible
/// (PID namespaces, counter restart after a restart), so `create_new` turns a
/// collision into an error and the retry advances to the next sequence. The
/// caller then writes the payload into the claimed file and renames it over
/// the target - the same shape as `crate::ups::store::tmp_sibling`.
fn tmp_sibling(path: &Path) -> Result<std::path::PathBuf, WriteManifestError> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Bounded: each collision consumes a fresh sequence, so this only runs
    // long on a pathological directory and must not spin forever.
    const ATTEMPTS: usize = 64;
    let mut last = None;
    for _ in 0..ATTEMPTS {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let tmp = std::path::PathBuf::from(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            // The handle is dropped so the subsequent write/rename sees a
            // closed file (Windows will not rename a file open for writing).
            Ok(_) => return Ok(tmp),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(WriteManifestError::Io(e)),
        }
    }
    Err(WriteManifestError::Io(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "no free temp name")
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::syncs_observed;
    use crate::manifest::record::tests::sample_manifest;

    /// Retiring a study must never publish an empty manifest under the LIVE
    /// name, and must leave the retired shape under the tombstone name.
    ///
    /// The old order (write the emptied manifest to the live path, then rename)
    /// left a window where readers of the live name saw a study with nothing in
    /// it - and a crash inside that window left exactly that on disk.
    #[test]
    fn retiring_a_study_moves_the_live_name_in_one_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().join("study.manifest");
        let tomb = dir.path().join("study.manifest.del");
        let manifest = sample_manifest();
        let original = manifest.encode().expect("encode");
        write_bytes_atomic_validated(&live, &original).expect("seed live");

        // The retired shape: same study, no instances (what a delete produces).
        let mut retired = manifest.clone();
        retired.series.clear();
        let retired_bytes = retired.encode().expect("encode retired");
        assert_ne!(retired_bytes, original, "the shapes differ");

        publish_tombstone_bytes(&live, &tomb, &retired_bytes).expect("retire");

        assert!(!live.exists(), "the live name is gone, not emptied");
        assert_eq!(
            std::fs::read(&tomb).expect("tombstone"),
            retired_bytes,
            "the tombstone carries the retired shape for the erase accounting"
        );
        assert!(
            crate::manifest::read::read_manifest_at(&std::fs::read(&tomb).expect("read")).is_ok(),
            "the published tombstone must decode"
        );
    }

    /// A payload the codec cannot decode must fail the retirement BEFORE the
    /// name moves: a delete that leaves the study where it was is recoverable,
    /// one that publishes an unreadable tombstone is not.
    #[test]
    fn an_undecodable_retired_shape_does_not_move_the_live_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().join("study.manifest");
        let tomb = dir.path().join("study.manifest.del");
        let original = sample_manifest().encode().expect("encode");
        write_bytes_atomic_validated(&live, &original).expect("seed live");

        let err = publish_tombstone_bytes(&live, &tomb, b"\xFF\xFF\xFF\xFF");

        assert!(err.is_err(), "an undecodable payload must be refused");
        assert_eq!(
            std::fs::read(&live).expect("live manifest"),
            original,
            "the study is exactly where it was"
        );
        assert!(!tomb.exists(), "no tombstone was published");
    }

    /// A retirement that cannot complete must not publish the tombstone.
    ///
    /// Publishing it would make the study invisible on the tombstone name while
    /// the caller was told the delete failed - a refused delete that silently
    /// happened. (Atomically replacing the live name is a property of `rename`,
    /// which the test above exercises; this pins the failure contract.)
    #[test]
    fn a_refused_retirement_publishes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().join("study.manifest");
        let tomb = dir.path().join("study.manifest.del");
        std::fs::write(&live, b"live").expect("seed");
        assert!(publish_tombstone_bytes(&live, &tomb, b"garbage").is_err());
        assert!(!tomb.exists(), "a refused retirement publishes nothing");
        assert_eq!(
            std::fs::read(&live).expect("live"),
            b"live",
            "the live manifest is untouched"
        );
    }

    #[test]
    fn validated_write_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        let manifest = sample_manifest();
        let bytes = manifest.encode().expect("encode");
        write_bytes_atomic_validated(&manifest_path, &bytes).expect("validated write");

        let decoded = crate::manifest::read::read_manifest_for_test(&manifest_path).expect("read");
        assert_eq!(decoded, manifest);
    }

    /// An undecodable buffer must fail the write BEFORE the rename, leaving a
    /// previously published good manifest byte-identical. Reverting the
    /// decode-first order (write first, decode after) clobbers the file with
    /// the undecodable bytes and this test fails.
    #[test]
    fn an_undecodable_manifest_fails_before_the_rename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        // Publish a known-good manifest - the bytes an undecodable write must
        // not clobber.
        let good = sample_manifest().encode().expect("encode");
        write_bytes_atomic(&manifest_path, &good).expect("good write");
        let before = std::fs::read(&manifest_path).expect("read good");

        // A truncated CBOR buffer decodes to nothing valid.
        let bad = &good[..good.len() / 2];
        let err = write_bytes_atomic_validated(&manifest_path, bad)
            .expect_err("an undecodable manifest must fail the write");
        assert!(matches!(err, WriteManifestError::Encode(_)), "got {err:?}");

        let after = std::fs::read(&manifest_path).expect("read after");
        assert_eq!(
            after, before,
            "an undecodable write must not clobber the good manifest"
        );
    }

    #[test]
    fn atomic_write_and_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        let manifest = sample_manifest();
        write_manifest(&manifest_path, &manifest).expect("write");

        // No temp file remains: the rename consumed the unique sibling.
        let leftovers = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .count();
        assert_eq!(leftovers, 0, "no *.tmp remains after a successful write");

        // Read back and compare (uses mmap reader from crate::manifest::read).
        let decoded = crate::manifest::read::read_manifest_for_test(&manifest_path).expect("read");
        assert_eq!(decoded, manifest);
    }

    /// Two writers claim DIFFERENT temp names (pid + counter), never a shared
    /// inode - the property that prevents a torn manifest.
    #[test]
    fn temp_sibling_names_are_unique_per_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("study.manifest");
        let t1 = tmp_sibling(&target).expect("first name");
        let t2 = tmp_sibling(&target).expect("second name");
        assert_ne!(t1, t2, "two writers must claim different temp names");
    }

    /// Two writers racing on the SAME target must never publish a partial
    /// manifest: every read taken mid-race decodes to one writer's complete
    /// bytes (the atomic rename is the only mutual exclusion), never a torn
    /// mix and never an `Encode` failure.
    #[test]
    fn concurrent_writers_never_publish_a_partial_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("study.manifest");

        let a = sample_manifest();
        let mut b = sample_manifest();
        b.sys.total_pixel_bytes = 3_000_000;
        assert_ne!(a, b, "the two writers must carry different payloads");

        let spawn = |manifest: Manifest, truth: (Manifest, Manifest)| {
            let target = target.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    write_manifest(&target, &manifest).expect("write");
                    let decoded = crate::manifest::read::read_manifest_for_test(&target)
                        .expect("mid-race read must decode to a complete manifest");
                    assert!(
                        decoded == truth.0 || decoded == truth.1,
                        "a read must see one writer's complete manifest, never a torn mix"
                    );
                }
            })
        };

        let ta = spawn(a.clone(), (a.clone(), b.clone()));
        let tb = spawn(b.clone(), (a.clone(), b.clone()));
        ta.join().expect("writer a");
        tb.join().expect("writer b");

        let final_manifest =
            crate::manifest::read::read_manifest_for_test(&target).expect("final read");
        assert!(
            final_manifest == a || final_manifest == b,
            "the final manifest must be one writer's complete bytes"
        );
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        let manifest = sample_manifest();
        write_manifest(&manifest_path, &manifest).expect("first write");
        write_manifest(&manifest_path, &manifest).expect("overwrite");

        let decoded = crate::manifest::read::read_manifest_for_test(&manifest_path).expect("read");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn write_creates_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir
            .path()
            .join("nested")
            .join("deeper")
            .join("study.manifest");

        write_manifest(&manifest_path, &sample_manifest()).expect("write into new dirs");
        assert!(manifest_path.exists());
    }

    /// A committed atomic write flushes the data to the medium before the
    /// rename and (Unix) the directory entry after it.
    ///
    /// The flush is observed on the barrier counter: `sync_all` leaves no byte
    /// trace a re-read can see, so deleting the call would keep the file
    /// contents correct while the count stays flat - the silent regression
    /// this pins.
    #[test]
    fn a_committed_write_flushes_data_and_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        let before = syncs_observed();
        write_manifest(&manifest_path, &sample_manifest()).expect("write");
        assert!(
            syncs_observed() > before,
            "a committed write has to flush the manifest to the medium"
        );
        #[cfg(unix)]
        assert!(
            syncs_observed() >= before + 2,
            "data barrier + directory barrier: expected at least 2, observed {}",
            syncs_observed() - before
        );
    }

    /// A failed write (including a failed flush) is an error, not a success.
    ///
    /// The block seam forces the atomic write to fail, and the caller must see
    /// the error: the STOW path treats a failed manifest write as a failed
    /// STOW, never as a committed manifest.
    #[test]
    fn a_blocked_write_is_an_error_not_a_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");
        block_manifest_write_for_test(&manifest_path);
        let err = write_manifest(&manifest_path, &sample_manifest())
            .expect_err("a blocked write must fail");
        assert!(matches!(err, WriteManifestError::Io(_)));
        // Nothing may be published on a failed write.
        assert!(!manifest_path.exists());
        unblock_manifest_write_for_test(&manifest_path);
    }
}
