// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Content-addressed file storage (CAS) for DICOM instances.
//!
//! Files are stored as BLAKE3 content hashes in a **two-level** directory
//! hierarchy - `{xx}/{yy}/{hash}.dcm` under the store root - giving
//! 65 536 buckets. This keeps per-bucket fan-out low even at 100M files
//! (btree-friendly on ext4/XFS/NTFS), so `open()` stays fast.
//!
//! The local hot/cold roots are themselves the "store": no path prefix is
//! applied (`{root}/{xx}/{yy}/{hash}.dcm`). The S3 object store adds an
//! optional configurable prefix (`[s3] prefix`, default `store/`) to keep
//! several stores addressable inside one shared bucket, so its keys are
//! `{prefix}{xx}/{yy}/{hash}.dcm`.
//!
//! # Hashing
//!
//! All writes and reads use BLAKE3 (A-004). Files live exclusively in the
//! **two-level** layout (`{xx}/{yy}/{hash}.dcm`). The legacy SHA-256
//! one-level layout and its fallback paths were excised - the
//! store is treated as clean and re-seeded by re-upload.
//!
//! # Architecture
//!
//! - **Streaming:** files are never loaded into memory whole. Data is
//!   processed chunk-by-chunk (64 KB) with incremental BLAKE3 hashing.
//! - **Atomic writes:** data is written to a temporary file, then atomically
//!   renamed into the CAS directory. Partial or failed writes are never
//!   visible to readers.
//! - **Deduplication:** the BLAKE3 hash serves as both address and integrity
//!   check. Each unique file is stored once; reference counting prevents
//!   premature deletion.
//!
//! # Limits
//!
//! - Maximum file size: 500 MiB. Attempts to exceed this return
//!   `StorageError::PayloadTooLarge`.
//!
//! # Security
//!
//! - Path traversal protection: only hex digits (0-9, a-f, A-F) are
//!   accepted in hash arguments, preventing directory escape attacks.
//! - Temp files are cleaned up on stream error or size-limit violation.
//!
//! # Layout summary
//!
//! ```text
//! store/
//! +-- _tmp/                 # staging for atomic rename
//! +-- 00/
//!     +-- 00/               # second-level bucket (BLAKE3 two-level layout)
//!         +-- 0000...ab.dcm
//! ```
#[cfg(feature = "s3")]
pub mod s3;

#[cfg(feature = "s3")]
pub(crate) mod s3_http;

#[cfg(feature = "s3")]
pub mod s3_client;

use crate::unique_id;
use blake3::Hasher as Blake3Hasher;
use std::io::{Read, Write};
use std::path::PathBuf;

// -- Error type ---------------------------------------------------------

/// Errors emitted by the content-addressed storage layer.
///
/// Manual `Display`/`Error` (thiserror removed, local Display).
#[derive(Debug)]
pub enum StorageError {
    /// Underlying I/O failure (disk full, permission denied, etc.).
    Io(std::io::Error),

    /// The requested hash does not correspond to an existing file.
    NotFound(String),

    /// The incoming payload exceeds the configured per-file size limit.
    PayloadTooLarge(u64),

    /// The provided hash string is not a valid hex digest.
    InvalidHash(String),

    /// The upstream stream produced an error mid-transfer.
    StreamError(String),

    /// The storage volume has insufficient free space.
    StorageUnavailable,

    /// BLAKE3 of on-disk content does not match the filename hash.
    /// Indicates silent data corruption (SEU, bit rot, filesystem bug).
    IntegrityFailure { expected: String, actual: String },
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "I/O error: {e}"),
            StorageError::NotFound(s) => write!(f, "File not found: {s}"),
            StorageError::PayloadTooLarge(n) => write!(f, "Payload too large: {n} bytes"),
            StorageError::InvalidHash(s) => write!(f, "Invalid content hash: {s}"),
            StorageError::StreamError(s) => write!(f, "Stream error: {s}"),
            StorageError::StorageUnavailable => write!(f, "Storage is unavailable"),
            StorageError::IntegrityFailure { expected, actual } => {
                write!(
                    f,
                    "Integrity failure: expected {expected}, computed {actual}"
                )
            }
        }
    }
}

impl std::error::Error for StorageError {}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

// -- Content-addressed storage ------------------------------------------

/// Content-addressed storage backend.
///
/// Stores DICOM files in a two-level directory hierarchy keyed by the BLAKE3
/// hash of the file contents.
///
/// # Directory layout
///
/// ```text
/// store/
/// +-- _tmp/           # Temporary files during writes (atomic rename target)
/// +-- 00/
///     +-- 00/
///         +-- 0000a1b2...ff.dcm   # BLAKE3 two-level layout
/// ```
#[derive(Clone)]
pub struct ContentAddressedStorage {
    /// Absolute path to the `store/` root directory.
    root: PathBuf,
}

/// Whether an error means "it was removed while we looked at it".
///
/// The CAS is append/erase concurrently, so a walk regularly stats a file that
/// the erase worker has just unlinked. That is normal operation, not a store
/// failure.
///
/// `PermissionDenied` is a "gone" signal ONLY on Windows: a file whose delete
/// is still pending (another handle, typically a reader streaming it, keeps it
/// open) cannot be opened at all and Windows reports ERROR_ACCESS_DENIED -
/// there is no `NotFound` for a pending-delete name. On Unix an unlinked file
/// stays readable, so a permission error there is a real, reportable condition:
/// skipping it would make `cas_stats` (and the scrub sweep) claim a healthy,
/// fully counted store when an inaccessible bucket was simply not read.
fn is_gone(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::NotFound {
        return true;
    }
    // The pending-delete reading is Windows-only, which is why this is not a
    // plain `matches!` over both kinds: on Unix an unreadable object is a
    // condition to report, not a file that was removed underneath us.
    e.kind() == std::io::ErrorKind::PermissionDenied && cfg!(windows)
}

/// Removes a temp file unless the caller disarms it.
///
/// RAII rather than cleanup on each error path: `store_dicom` has five ways to
/// fail after the temp file exists (read, size cap, write, flush/sync, rename),
/// and the one that gets forgotten is the one that fills the disk.
struct TempFileGuard<'a> {
    path: &'a std::path::Path,
    armed: bool,
}

impl<'a> TempFileGuard<'a> {
    fn new(path: &'a std::path::Path) -> Self {
        Self { path, armed: true }
    }

    /// Keep the file (the rename succeeded).
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        // The env escape hatch exists only so the cleanup test can prove it
        // fails when the guard is inert; nothing in the server sets it.
        if self.armed && std::env::var_os("CLARUS_TMP_GUARD_OFF").is_none() {
            remove_cleanup_target(self.path);
        }
    }
}

impl ContentAddressedStorage {
    /// Create a new CAS store rooted at `root`.
    ///
    /// Ensures that `root` and `root/_tmp` directories exist.
    pub fn new(root: PathBuf) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(root.join("_tmp"))?;

        Ok(Self { root })
    }

    /// Return the root directory of the CAS store.
    pub fn root_dir(&self) -> &std::path::Path {
        &self.root
    }

    /// Count `.dcm` files and their total on-disk size across the whole store.
    ///
    /// Walks the BLAKE3 two-level (`{root}/{00}/{00}/{hash}.dcm`) bucket layout
    /// and aggregates `(count, bytes)` from file metadata - no integrity
    /// hashing. Returns `(count, total_bytes)`. Deleted files are physically
    /// unlinked by the erase-worker, so a walk of the live store is the source
    /// of truth (post-SQLite).
    pub fn cas_stats(&self) -> Result<(u64, u64), StorageError> {
        let mut count: u64 = 0;
        let mut bytes: u64 = 0;

        for entry in std::fs::read_dir(&self.root)? {
            // A bucket that disappears mid-walk (the erase worker unlinks
            // empty buckets) must not abort the whole statistics pass: the
            // count would be wrong AND the caller would see a store failure
            // for a store that is fine.
            let entry = match entry {
                Ok(e) => e,
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            if !file_type.is_dir() {
                continue;
            }
            let dir_name = entry.file_name();
            let dir_name_str = dir_name.to_string_lossy();
            if dir_name_str.len() != 2 || !dir_name_str.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let bucket_path = entry.path();

            let bucket_entries = match std::fs::read_dir(&bucket_path) {
                Ok(d) => d,
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            for bucket_entry in bucket_entries {
                let bucket_entry = match bucket_entry {
                    Ok(e) => e,
                    Err(e) if is_gone(&e) => continue,
                    Err(e) => return Err(e.into()),
                };
                let ft = match bucket_entry.file_type() {
                    Ok(t) => t,
                    Err(e) if is_gone(&e) => continue,
                    Err(e) => return Err(e.into()),
                };
                if !ft.is_dir() {
                    continue;
                }
                let sub_name = bucket_entry.file_name();
                let sub_str = sub_name.to_string_lossy();
                if sub_str.len() != 2 || !sub_str.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                self.cas_stats_dir(&mut count, &mut bytes, bucket_entry.path())?;
            }
        }

        Ok((count, bytes))
    }

    /// Accumulate `count`/`bytes` over every `.dcm` file directly inside `dir`
    /// (a second-level sub-bucket of the BLAKE3 two-level layout).
    fn cas_stats_dir(
        &self,
        count: &mut u64,
        bytes: &mut u64,
        dir: std::path::PathBuf,
    ) -> Result<(), StorageError> {
        let entries = match std::fs::read_dir(&dir) {
            Ok(d) => d,
            Err(e) if is_gone(&e) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.ends_with(".dcm") {
                continue;
            }
            let len = match entry.metadata() {
                Ok(m) => m.len(),
                // The file was erased between `read_dir` and `metadata`.
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            *count += 1;
            *bytes += len;
        }
        Ok(())
    }

    // -- Public API -------------------------------------------------

    /// Stream a DICOM file into the store.
    ///
    /// The entire stream is consumed, hashed with BLAKE3 on the fly, and
    /// written to a temporary file. On success the file is atomically
    /// renamed into its content-addressed path (two-level BLAKE3 layout).
    ///
    /// If a file with the same hash already exists the write is skipped
    /// (deduplication) and the hash is still returned.
    ///
    /// # Errors
    ///
    /// - `PayloadTooLarge` if `max_size` is exceeded.
    /// - `Io` on filesystem failures (or upstream read failure).
    pub fn store_dicom(
        &self,
        mut reader: impl std::io::Read,
        max_size: u64,
    ) -> Result<String, StorageError> {
        // -- 1. Create temp file ----------------------------------
        let temp_dir = self.root.join("_tmp");
        // `create_dir_all` is idempotent; safe to call here as well. Its
        // failure must not be replaced by a cleanup report: the caller asked
        // why the STORE failed, and the temp file does not exist yet.
        std::fs::create_dir_all(&temp_dir)?;

        let temp_name = format!("{}.dcm", unique_id());
        let temp_path = temp_dir.join(&temp_name);

        // The temp file must not outlive this call on ANY error path. A read,
        // write, flush, sync or rename failure used to leave the partial file
        // behind, and with a 500 MiB instance cap those accumulate until the
        // volume fills. The guard is disarmed only after the rename succeeds.
        let mut temp_guard = TempFileGuard::new(&temp_path);

        let mut file = std::fs::File::create(&temp_path)?;
        let mut hasher = Blake3Hasher::new();
        let mut total: u64 = 0;
        let mut buf = vec![0u8; 65536];

        // -- 2. Consume reader, hash, write (sync, std::io::Read) --
        let hash_hex = loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                // End of input - finalise.
                break crate::hex_encode(hasher.finalize().as_bytes());
            }
            total = total.saturating_add(n as u64);
            if total > max_size {
                // The guard removes the partial file when this returns.
                drop(file);
                return Err(StorageError::PayloadTooLarge(total));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
        };

        // -- 3. Flush to disk -------------------------------------
        file.flush()?;
        // `sync_data` ensures durability on Linux; on Windows it is a
        // no-op for rename-based atomicity but harmless to call.
        file.sync_data()?;
        drop(file);

        // -- 4. Ensure bucket directory ---------------------------
        // Bucket cleanup runs concurrently and may remove a bucket between the
        // `create_dir_all` and the rename, so one retry of both is required:
        // the upload is valid and the only thing that changed is which
        // directory existed a moment ago.
        let target_path = self.cas_path(&hash_hex);
        let mut attempt = 0u8;
        loop {
            let created_first_level = match target_path.parent() {
                Some(bucket) => create_bucket_dir(&self.root, bucket)?,
                None => None,
            };
            match std::fs::rename(&temp_path, &target_path) {
                Ok(()) => {
                    // A caller-visible precondition: `store_dicom` consumes a
                    // reader and hands back a key, so the object must BE there.
                    // On a volume whose paths the process cannot address past
                    // MAX_PATH (a deep root under a long user directory, the
                    // usual Windows case) an intermediate call can report
                    // success while nothing moved; without this check the ingest
                    // would be acked and the payload silently lost.
                    if !content_matches(&target_path, &hash_hex) {
                        return Err(StorageError::Io(std::io::Error::other(
                            "rename reported success but the object is not readable under its key",
                        )));
                    }
                    // The file BODY was flushed before the rename, which fixes
                    // its contents forever. The NAME is a separate, later
                    // commitment: until the directory entry reaches the medium a
                    // crash can recover an empty store while the ingest was
                    // acked. Only the guard is disarmed afterwards, so a sync
                    // failure still removes the temp file.
                    let synced = sync_stored_name(
                        &self.root,
                        created_first_level.as_deref(),
                        parent_entry_dir(&target_path),
                    );
                    match synced {
                        Ok(()) => {
                            temp_guard.disarm();
                            break;
                        }
                        Err(e) => return Err(sync_failure(e)),
                    }
                }
                Err(e) => {
                    // Dedup is credited ONLY to a target that is a regular file
                    // whose bytes hash to THIS key. Keying it on `exists()`
                    // alone accepted a directory (a rename onto a non-empty
                    // directory always fails) or a corrupt blob sitting at
                    // `{hash}.dcm`, discarded the valid payload and acked
                    // content the store does not hold. Where the platform can
                    // replace the target (Windows MoveFileEx) the rename
                    // succeeds and never reaches this arm, so the content is
                    // rewritten rather than silently trusted.
                    if content_matches(&target_path, &hash_hex) {
                        // Dedup: identical content is already stored. The guard
                        // removes the temp file.
                        break;
                    }
                    if e.kind() == std::io::ErrorKind::NotFound && attempt == 0 {
                        // The bucket vanished under us; recreate and retry once.
                        // Bounded because the loser of a concurrent
                        // `create_dir_all` can report NotFound on Windows; a
                        // second failure is a real error, not another race.
                        attempt += 1;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }

        Ok(hash_hex)
    }

    /// Open a stored DICOM file for reading.
    ///
    /// Returns the `std::fs::File` handle and the file size in bytes.
    /// The caller is expected to stream the file into an HTTP response
    /// (e.g. via a synchronous read loop).
    ///
    /// # Errors
    ///
    /// - `InvalidHash` if `blake3` is not a 64-character hex string.
    /// - `NotFound` if no file exists for the given hash.
    /// - `Io` on other filesystem errors.
    pub fn read_dicom(&self, blake3: &str) -> Result<(std::fs::File, u64), StorageError> {
        validate_hash_hex(blake3)?;

        let file = self.open_path(blake3)?;

        let metadata = file.metadata()?;
        let size = metadata.len();

        Ok((file, size))
    }

    /// Read an entire DICOM file into memory.
    ///
    /// Used for transcoding (G-05) which requires the full dataset
    /// in memory. For normal WADO streaming, prefer [`read_dicom`].
    ///
    /// # Errors
    ///
    /// - `InvalidHash` if `blake3` is not a 64-character hex string.
    /// - `NotFound` if no file exists for the given hash.
    /// - `Io` on other filesystem errors.
    pub fn read_all(&self, blake3: &str) -> Result<Vec<u8>, StorageError> {
        validate_hash_hex(blake3)?;

        let file = self.open_path(blake3)?;

        let mut bytes = Vec::new();
        file.take(u64::MAX).read_to_end(&mut bytes)?;

        // Verify on-disk integrity (SEU detection) with BLAKE3.
        let computed = {
            let mut h = Blake3Hasher::new();
            h.update(&bytes);
            crate::hex_encode(h.finalize().as_bytes())
        };
        let expected_lower = blake3.to_lowercase();
        if !computed.eq_ignore_ascii_case(&expected_lower) {
            return Err(StorageError::IntegrityFailure {
                expected: expected_lower,
                actual: computed,
            });
        }

        Ok(bytes)
    }

    /// Stream a stored DICOM file in 64 KB chunks.
    ///
    /// Returns a `std::io::Read` reader over the file and the total size.
    /// Does NOT verify integrity - caller should call
    /// [`verify_integrity`](Self::verify_integrity) first if SEU
    /// detection is needed (or use [`read_all`](Self::read_all) for
    /// small files which verifies automatically).
    ///
    /// # Errors
    ///
    /// - `InvalidHash` if `blake3` is not a 64-character hex string.
    /// - `NotFound` if no file exists for the given hash.
    /// - `Io` on other filesystem errors.
    pub fn read_stream(
        &self,
        blake3: &str,
    ) -> Result<(impl std::io::Read + '_ + 'static, u64), StorageError> {
        validate_hash_hex(blake3)?;

        let file = self.open_path(blake3)?;

        let metadata = file.metadata()?;
        let size = metadata.len();

        Ok((file, size))
    }

    /// Check whether a file with the given hash exists in the layout.
    pub fn exists(&self, blake3: &str) -> bool {
        // Ignore validation errors - invalid hash implies non-existent.
        if validate_hash_hex(blake3).is_err() {
            return false;
        }
        self.cas_path_two_level(blake3).exists()
    }

    /// Remove the file associated with `blake3`.
    ///
    /// If the bucket directory becomes empty after removal it is also
    /// deleted (best-effort; an unexpected failure there is reported, not
    /// fatal).
    ///
    /// # Errors
    ///
    /// - `InvalidHash` if `blake3` is not valid hex.
    /// - `NotFound` if no file exists.
    /// - `Io` on filesystem errors during deletion.
    pub fn remove(&self, blake3: &str) -> Result<(), StorageError> {
        validate_hash_hex(blake3)?;

        let normalized = blake3.to_ascii_lowercase();
        let path = self.cas_path_two_level(&normalized);
        // No `exists()` pre-check: it is a check/use race (the erase worker can
        // unlink between the check and the unlink), and an inaccessible
        // ANCESTOR would be reported as NotFound - "this object was never
        // stored" - which the caller cannot tell from a permission problem on
        // the volume. The unlink itself is the atomic test.
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(normalized));
            }
            // Every other kind is propagated as an I/O failure, including the
            // Windows pending-delete ACCESS_DENIED: reporting that unlink as
            // done would ack a delete whose name may still block a re-store of
            // the same instance, while the scan paths classify it as a race
            // only because a READER never has to claim success for it.
            Err(e) => return Err(e.into()),
        }

        // Best-effort bucket cleanup: the second-level dir first, then the
        // first-level bucket if empty. Both re-create lazily on the next write.
        if let Some(bucket) = path.parent() {
            remove_cleanup_dir(bucket);
        }
        if let Some(first_level) = path.parent().and_then(|p| p.parent()) {
            remove_cleanup_dir(first_level);
        }

        Ok(())
    }

    /// Heuristic free-space check.
    ///
    /// Creates a small temporary file, writes a few bytes, and removes it.
    /// Returns `false` if any step fails (disk full, permission denied,
    /// etc.).
    pub fn is_storage_available(&self) -> bool {
        let probe = self.root.join("_health_probe");
        match std::fs::File::create(&probe) {
            Ok(mut f) => {
                let ok = f.write_all(b"ok").is_ok() && f.sync_data().is_ok();
                drop(f);
                remove_cleanup_target(&probe);
                ok
            }
            Err(_) => false,
        }
    }

    /// Return the temporary directory path used during writes.
    ///
    /// This is `{root}/_tmp/`.
    pub fn temp_dir(&self) -> PathBuf {
        self.root.join("_tmp")
    }

    /// Verify that the on-disk content matches the expected content hash.
    ///
    /// Reads the file in 64 KB chunks, computes the BLAKE3 hash incrementally,
    /// and compares with the expected hash (the filename).
    ///
    /// Returns the file size in bytes on success.
    ///
    /// # Errors
    ///
    /// - `InvalidHash` if `blake3` is not a 64-character hex string.
    /// - `NotFound` if no file exists for the given hash.
    /// - `IntegrityFailure` if the computed hash differs from expected.
    /// - `Io` on other filesystem errors.
    pub fn verify_integrity(&self, blake3: &str) -> Result<u64, StorageError> {
        validate_hash_hex(blake3)?;

        let mut file = self.open_path(blake3)?;
        let mut buf = vec![0u8; 65536];
        let mut total: u64 = 0;

        let mut hasher = Blake3Hasher::new();
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total = total.saturating_add(n as u64);
        }
        let computed = crate::hex_encode(hasher.finalize().as_bytes());
        let expected_lower = blake3.to_lowercase();

        if computed.eq_ignore_ascii_case(&expected_lower) {
            Ok(total)
        } else {
            Err(StorageError::IntegrityFailure {
                expected: expected_lower,
                actual: computed,
            })
        }
    }

    /// Walk all CAS buckets and verify every stored file's integrity.
    ///
    /// This is a background maintenance operation (like ZFS scrub).
    /// For large stores, consider running this during low-traffic periods.
    pub fn scrub(&self) -> Result<ScrubReport, StorageError> {
        let mut report = ScrubReport {
            total: 0,
            healthy: 0,
            corrupted: Vec::new(),
            unreadable: Vec::new(),
        };

        for entry in std::fs::read_dir(&self.root)? {
            // A bucket that disappears mid-walk (the erase worker unlinks
            // empty buckets) must not abort the whole statistics pass: the
            // count would be wrong AND the caller would see a store failure
            // for a store that is fine.
            let entry = match entry {
                Ok(e) => e,
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            if !file_type.is_dir() {
                continue;
            }

            let dir_name = entry.file_name();
            let dir_name_str = dir_name.to_string_lossy();
            // Buckets are exactly 2 hex digits (00..ff)
            if dir_name_str.len() != 2 || !dir_name_str.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let bucket_path = entry.path();

            let bucket_entries = match std::fs::read_dir(&bucket_path) {
                Ok(d) => d,
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            for bucket_entry in bucket_entries {
                let bucket_entry = match bucket_entry {
                    Ok(e) => e,
                    Err(e) if is_gone(&e) => continue,
                    Err(e) => return Err(e.into()),
                };
                let ft = match bucket_entry.file_type() {
                    Ok(t) => t,
                    Err(e) if is_gone(&e) => continue,
                    Err(e) => return Err(e.into()),
                };
                if !ft.is_dir() {
                    continue;
                }
                // Two-level layout: this second-level dir holds .dcm files.
                let sub_name = bucket_entry.file_name();
                let sub_str = sub_name.to_string_lossy();
                if sub_str.len() != 2 || !sub_str.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                self.scrub_dir(&mut report, bucket_entry.path())?;
            }
        }

        Ok(report)
    }

    /// Verify every `.dcm` file inside `dir` and fold results into `report`.
    ///
    /// `dir` is a second-level sub-bucket of the BLAKE3 two-level layout.
    /// The BLAKE3 hasher is used for every file.
    fn scrub_dir(
        &self,
        report: &mut ScrubReport,
        dir: std::path::PathBuf,
    ) -> Result<(), StorageError> {
        // The bucket may vanish between the parent listing and this call
        // (concurrent erase/cleanup). A missing bucket dir is not a scrub
        // failure: skip it instead of aborting the whole sweep.
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Only process .dcm files
            if !name_str.ends_with(".dcm") {
                continue;
            }

            // Extract the content hash from the filename (strip .dcm suffix).
            let hash = &name_str[..name_str.len() - 4];

            report.total += 1;

            match self.verify_integrity(hash) {
                Ok(_) => {
                    report.healthy += 1;
                }
                Err(StorageError::IntegrityFailure { .. }) => {
                    report.corrupted.push(hash.to_string());
                }
                Err(StorageError::NotFound(_)) => {
                    // File disappeared between read_dir and open - skip
                }
                Err(_) => {
                    report.unreadable.push(hash.to_string());
                }
            }
        }
        Ok(())
    }

    // -- Private helpers ------------------------------------------

    /// Build the content-addressed file path - the **two-level layout**.
    ///
    /// Geometry is the canonical [`cas_rel_path`] under `root`:
    /// `{root}/{xx}/{yy}/{hash}.dcm`.
    pub fn cas_path(&self, hash: &str) -> PathBuf {
        self.cas_path_two_level(hash)
    }

    /// The BLAKE3 two-level layout path.
    ///
    /// Built on the single canonical [`cas_rel_path`] so hot geometry can
    /// never drift from cold/S3. The hash is folded to lowercase here: writes
    /// always name buckets with the lowercase digest while [`validate_hash_hex`]
    /// accepts either case, so on a case-sensitive filesystem an uppercase
    /// (still valid) key would address a different, absent path. The read and
    /// verify paths already compare with `eq_ignore_ascii_case`; the path must
    /// agree with that.
    fn cas_path_two_level(&self, hash: &str) -> PathBuf {
        self.root.join(cas_rel_path(&hash.to_ascii_lowercase()))
    }

    /// Open the stored file for `hash` in the BLAKE3 two-level layout.
    ///
    /// # Errors
    ///
    /// - `NotFound` if no file exists for the hash.
    /// - `Io` on other filesystem or open failures.
    fn open_path(&self, hash: &str) -> Result<std::fs::File, StorageError> {
        let path = self.cas_path_two_level(hash);
        match std::fs::File::open(&path) {
            Ok(file) => Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(hash.to_string()))
            }
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    /// Build a temporary file path for atomic write-then-rename.
    ///
    /// Format: `{root}/_tmp/{unique_id}.{ext}`
    pub fn tmp_path(&self, unique_id: &str) -> PathBuf {
        self.root.join("_tmp").join(unique_id)
    }
}

// -- Durability of the published name -----------------------------------

/// Report a cleanup failure that was supposed to be impossible.
///
/// The channel is `log::warn`, the one the parser and the transcoder use for
/// recoverable problems, plus a counter so a store leaking temp files is
/// visible in metrics instead of only in a log nobody greps. The expected
/// races are filtered by the callers below and never reach this point.
fn report_cleanup_failure(what: &str, path: &std::path::Path, e: &std::io::Error) {
    log::warn!(
        "storage cleanup: could not remove {what} {}: {e}",
        path.display()
    );
    CLEANUP_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Cleanup failures observed, process-wide.
///
/// A log line leaves no trace a test can assert on; this count is what makes
/// the reporting falsifiable.
static CLEANUP_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many unexpected cleanup failures have been reported so far.
#[cfg(test)]
pub(crate) fn cleanup_failures_observed() -> u64 {
    CLEANUP_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Report a failed `remove_file` unless the file was already gone.
///
/// `NotFound` is the expected outcome when the erase worker (or a concurrent
/// `remove`) unlinked the temp file first; every other kind - a permission
/// error, a busy file on Windows, EIO - means the store is keeping a partial
/// `.dcm` that nothing will ever look at again.
fn remove_cleanup_target(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        report_cleanup_failure("temp file", path, &e);
    }
}

/// Report a failed bucket `remove_dir` unless the bucket still holds records.
///
/// Two kinds are expected: `NotFound` (an earlier cleanup took it) and
/// `DirectoryNotEmpty` (a write landed between our unlink and this call - the
/// bucket MUST stay). Everything else is reported, otherwise the accidental
/// failures leak empty buckets until the store has 65 536 of them.
fn remove_cleanup_dir(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_dir(path)
        && !matches!(
            e.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::DirectoryNotEmpty
                | std::io::ErrorKind::NotADirectory
        )
    {
        report_cleanup_failure("bucket directory", path, &e);
    }
}

/// Create `bucket` (`root/{xx}/{yy}`, idempotent) and return the first-level
/// bucket name when THIS call created that level.
///
/// Only a directory created here needs its own NAME flushed in the store root,
/// which is why the name - not the bare component - is returned: a relative
/// `xx` would resolve against the process CWD, a different directory whenever
/// the store root is absolute (the normal production case). `Path::exists`
/// after `create_dir_all` is a racy answer, but a wrong one costs a redundant
/// directory fsync and never correctness.
fn create_bucket_dir(
    root: &std::path::Path,
    bucket: &std::path::Path,
) -> Result<Option<String>, StorageError> {
    let first_level = bucket.parent().filter(|p| p.starts_with(root));
    let existed = first_level.is_some_and(|level| level.is_dir());
    std::fs::create_dir_all(bucket)?;
    Ok(match first_level {
        Some(level) if !existed => Some(level.as_os_str().to_string_lossy().into_owned()),
        _ => None,
    })
}

/// The directory whose entry names `path` for [`sync_stored_name`].
///
/// A bare relative path has an EMPTY parent, but the directory carrying the
/// name is `"."`; `None` only for a path with no parent component at all.
fn parent_entry_dir(path: &std::path::Path) -> Option<&std::path::Path> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => Some(p),
        Some(_) => Some(std::path::Path::new(".")),
        None => None,
    }
}

/// Flush a directory, counting the barrier the way `clarus_core::durable` does.
///
/// The counter exists because `sync_all` leaves no observable trace (the bytes
/// are in the page cache whether or not it ran); it is what makes the barrier
/// falsifiable. Mirrored here rather than imported because adapters cannot
/// depend on clarus-core - core and the server depend on adapters.
#[cfg(unix)]
fn sync_barrier(dir: &std::fs::File) -> std::io::Result<()> {
    let result = dir.sync_all();
    SYNC_BARRIERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    result
}

/// Number of directory flushes the store has performed.
///
/// On Windows no directory barrier exists, so there is nothing to count.
#[cfg_attr(not(unix), allow(dead_code))]
static SYNC_BARRIERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many directory flushes have run so far.
#[cfg(all(unix, test))]
pub(crate) fn sync_barriers_observed() -> u64 {
    SYNC_BARRIERS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Directories whose NEXT barrier is forced to fail - the test seam for "a
/// barrier failure must not be acked as stored".
///
/// Process-wide, not thread-local: an ingest can run on any worker thread.
/// Empty in production, so the store pays one read lock and an empty scan.
#[cfg(all(unix, test))]
static BARRIER_FAIL_NEXT: std::sync::RwLock<Vec<PathBuf>> = std::sync::RwLock::new(Vec::new());

/// Test seam: the next directory barrier on `dir` fails once with `StorageFull`.
#[cfg(all(unix, test))]
pub(crate) fn fail_next_barrier_for_test(dir: &std::path::Path) {
    if let Ok(mut list) = BARRIER_FAIL_NEXT.write() {
        list.push(dir.to_path_buf());
    }
}

#[cfg(all(unix, test))]
fn take_injected_barrier_error(dir: &std::path::Path) -> Option<std::io::Error> {
    let mut list = BARRIER_FAIL_NEXT.write().ok()?;
    let pos = list.iter().position(|p| p == dir)?;
    list.remove(pos);
    Some(std::io::Error::new(
        std::io::ErrorKind::StorageFull,
        "injected dir barrier failure",
    ))
}

/// Flush the directory entry that names `dir` - the second half of a rename.
///
/// `fsync` on the FILE fixes its contents; only `fsync` on the DIRECTORY makes
/// the new entry survive a crash. Without it the store can answer "stored" for
/// an object the next boot cannot find. The open failure is SWALLOWED with the
/// tradeoff `clarus_core::durable` documents (same policy, kept in step on
/// purpose): a directory that cannot be opened for read - a mode-0300 parent,
/// or a bare relative path whose cwd is unreadable - cannot be fsync'd by this
/// process at all, so the name is simply not makeable-durable there, whereas
/// propagating would fail every publish for a narrow crash window. A barrier
/// failure on a directory we DID open is propagated: it holds the name just
/// created and may still be only in the page cache.
///
/// Unix-only by necessity: Windows cannot open a directory for
/// `FlushFileBuffers`, so this is the documented no-op there.
fn sync_dir_entry(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        #[cfg(test)]
        if let Some(err) = take_injected_barrier_error(dir) {
            return Err(err);
        }
        let handle = match std::fs::File::open(dir) {
            Ok(f) => f,
            Err(_) => return Ok(()),
        };
        sync_barrier(&handle)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Make the published name of a stored object durable.
///
/// Two parts, both needed: a first-level bucket (`root/xx`) created by this
/// ingest gets its own name flushed in the store root, and the directory
/// holding `{hash}.dcm` is flushed after the rename (which also persists the
/// second-level bucket's name). `created` is `None` when both levels already
/// existed, so there is nothing new to persist. Free of platform `cfg` so the
/// Windows build still compiles the caller's path.
fn sync_stored_name(
    root: &std::path::Path,
    created: Option<&str>,
    entry_dir: Option<&std::path::Path>,
) -> std::io::Result<()> {
    if let Some(name) = created {
        sync_dir_entry(&root.join(name))?;
    }
    match entry_dir {
        Some(dir) => sync_dir_entry(dir),
        None => Ok(()),
    }
}

/// Map a failed name-durability barrier to the store's error type.
fn sync_failure(e: std::io::Error) -> StorageError {
    StorageError::Io(std::io::Error::new(
        e.kind(),
        format!("CAS directory barrier: {e}"),
    ))
}

/// Whether `path` is a REGULAR file whose BLAKE3 digest is `hash_hex`.
///
/// Existence is not content: the dedup arm must prove the stored object IS the
/// one being ingested, otherwise the payload is discarded and a directory, a
/// truncated blob or a rotted file silently eats a valid upload. Streamed in
/// 64 KiB chunks so a 500 MiB instance costs one pass and no heap growth;
/// `false` on ANY error, so the original rename error stays the outcome.
fn content_matches(path: &std::path::Path, hash_hex: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        _ => return false,
    }
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut hasher = Blake3Hasher::new();
    let mut buf = [0u8; 65536];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    crate::hex_encode(hasher.finalize().as_bytes()).eq_ignore_ascii_case(hash_hex)
}

// -- File extensions ----------------------------------------------------

/// Extension for Level 1: full DICOM files.
pub const EXT_DCM: &str = "dcm";

// -- CAS geometry (single source of truth) -----------------------------

/// The canonical CAS geometry - **for ALL tiers: hot/cold/S3**.
///
/// Returns the two-level relative path `{xx}/{yy}/{hash}.dcm` as a `String`
/// **with FORWARD SLASHES** - this is CAS *notation*, always `/`, never a
/// `PathBuf` (on x86_64-pc-windows-msvc a `PathBuf` would render `\`, and an
/// S3 key built from it would silently diverge from what LIST/walk find).
///
/// Local files build on it as `root.join(cas_rel_path(hash))`; `join` splits
/// on `/` and translates to `\` for the filesystem where it does not matter.
/// S3 keys build on it too: `{prefix}` + `cas_rel_path(hash)`.
///
/// **Change the geometry ONLY here** - every other tier calls this.
///
/// A8: the byte layout lives in the zero-allocation
/// [`cas_rel_path_fill_buf`]; this returns the owned `String` form.
pub fn cas_rel_path(hash: &str) -> String {
    let mut buf = [0u8; 128];
    match cas_rel_path_fill_buf(hash, &mut buf) {
        Some(rel) => rel.to_string(),
        // Same panic class as the old `&hash[..2]` slicing for an invalid
        // (too short / too long) hash - upstream callers pass 64-hex keys.
        None => panic!("cas_rel_path: hash '{hash}' is not a full CAS key"),
    }
}

/// Zero-allocation variant of [`cas_rel_path`]: writes `{xx}/{yy}/{hash}.dcm`
/// into the caller's stack buffer and returns the filled slice.
///
/// THE byte layout lives here - [`cas_rel_path`] delegates to this writer, so
/// the geometry can never fork. Returns `None` when `hash` is shorter than 4
/// bytes or the 128-byte buffer is too small (a full 64-hex key needs 73).
/// The output is byte-identical to [`cas_rel_path`] for the same input.
pub fn cas_rel_path_fill_buf<'a>(hash: &str, out: &'a mut [u8; 128]) -> Option<&'a str> {
    if hash.len() < 4 {
        return None;
    }
    let n = hash.len() + 10; // {xx}/{yy}/{hash}.dcm = 2+1+2+1+len+4
    if n > out.len() {
        return None;
    }
    let hb = hash.as_bytes();
    out[..2].copy_from_slice(&hb[..2]);
    out[2] = b'/';
    out[3..5].copy_from_slice(&hb[2..4]);
    out[5] = b'/';
    out[6..6 + hash.len()].copy_from_slice(hb);
    out[6 + hash.len()..n].copy_from_slice(b".dcm");
    std::str::from_utf8(&out[..n]).ok()
}

// -- Validation ---------------------------------------------------------

/// Result of a CAS scrub operation.
#[derive(Debug, Clone)]
pub struct ScrubReport {
    /// Total number of .dcm files checked.
    pub total: usize,
    /// Hashes of files that passed verification.
    pub healthy: usize,
    /// Hashes of files with integrity failures (SEU / bit rot).
    pub corrupted: Vec<String>,
    /// Hashes of files that could not be read (I/O error).
    pub unreadable: Vec<String>,
}

// -- Validation ---------------------------------------------------------

/// Validate that `s` is exactly 64 ASCII hex characters (0-9, a-f, A-F).
///
/// BLAKE3 hashes are 32 bytes -> 64 hex chars.
///
/// This is the primary **path traversal** defence: only hex chars can
/// appear in filesystem path components built from user-supplied hash
/// strings.
fn validate_hash_hex(s: &str) -> Result<(), StorageError> {
    if s.len() != 64 {
        return Err(StorageError::InvalidHash(s.to_string()));
    }
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StorageError::InvalidHash(s.to_string()));
    }
    Ok(())
}

// -- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// Helper: create a CAS store in a temporary directory.
    ///
    /// The returned fixture OWNS the directory: when the last clone is dropped
    /// the tree is removed. Without an owner each test run left a
    /// `clarus-cas-test-<id>` directory behind, so a full run of this module
    /// accumulated hundreds of stored payloads and buckets. Tests bind it as
    /// `let cas = temp_cas();` and still get `ContentAddressedStorage` through
    /// `Deref`, so no call site had to change.
    fn temp_cas() -> TempCas {
        let dir = std::env::temp_dir().join(format!("clarus-cas-test-{}", unique_id()));
        let cas = ContentAddressedStorage::new(dir.clone()).expect("create temp CAS");
        TempCas {
            cas,
            _dir: std::sync::Arc::new(RemoveTreeOnDrop { path: dir }),
        }
    }

    /// A CAS that deletes its directory tree when it goes out of scope.
    ///
    /// `Arc` rather than a plain field so the guard survives clones of the
    /// fixture; the tree is removed once, when the last handle drops. Files are
    /// closed by then because `ContentAddressedStorage` holds only a path, and
    /// every helper in this module drops its `File` before returning - the
    /// Windows caveat is that an open handle makes `remove_dir_all` fail, which
    /// is why the removal is best-effort and must not panic in `Drop`.
    struct TempCas {
        cas: ContentAddressedStorage,
        /// Held only for its `Drop`: the field is never read, and naming it
        /// with a leading underscore would hide that from a future reader.
        _dir: std::sync::Arc<RemoveTreeOnDrop>,
    }

    impl std::ops::Deref for TempCas {
        type Target = ContentAddressedStorage;
        fn deref(&self) -> &ContentAddressedStorage {
            &self.cas
        }
    }

    /// Best-effort directory removal with its own owner.
    struct RemoveTreeOnDrop {
        path: std::path::PathBuf,
    }

    impl Drop for RemoveTreeOnDrop {
        fn drop(&mut self) {
            // A panic inside `Drop` during unwinding aborts the process, and a
            // leftover temp directory is a nuisance, not a test failure.
            // The env escape hatch matches `TempFileGuard`'s and exists only so
            // the cleanup test can prove it fails when the guard is inert;
            // nothing in the server sets it.
            if std::env::var_os("CLARUS_TEST_GUARD_OFF").is_some() {
                return;
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Helper: a `Read` reader over the given bytes (matches `store_dicom` input).
    fn byte_stream(data: Vec<u8>) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(data)
    }

    /// Helper: BLAKE3 hex of a byte slice (matches `store_dicom` hashing).
    fn blake3_hex(data: &[u8]) -> String {
        let mut hasher = Blake3Hasher::new();
        hasher.update(data);
        crate::hex_encode(hasher.finalize().as_bytes())
    }

    // -- Basic write + read round-trip ----------------------------

    #[test]
    fn roundtrip_small_file() {
        let cas = temp_cas();
        let data = b"Hello, DICOM world!";
        let expected_hash = blake3_hex(data);

        let stream = byte_stream(data.to_vec());
        let hash = cas
            .store_dicom(stream, 500 * 1024 * 1024)
            .expect("store_dicom");

        assert_eq!(hash, expected_hash);
        assert!(cas.exists(&hash));

        let (_file, size) = cas.read_dicom(&hash).expect("read_dicom");
        assert_eq!(size, data.len() as u64);
    }

    // -- Deduplication -------------------------------------------

    #[test]
    fn dedup_same_content_same_hash() {
        let cas = temp_cas();
        let data = b"dedup test payload";

        // First write.
        let s1 = byte_stream(data.to_vec());
        let h1 = cas.store_dicom(s1, 500 * 1024 * 1024).expect("first write");

        // Second write of identical content.
        let s2 = byte_stream(data.to_vec());
        let h2 = cas
            .store_dicom(s2, 500 * 1024 * 1024)
            .expect("second write (dedup)");

        assert_eq!(h1, h2);
        assert!(cas.exists(&h1));

        // Verify only one file exists on disk.
        let path = cas.cas_path(&h1);
        assert!(path.exists());
        // The bucket should contain exactly one entry.
        let bucket = path.parent().expect("test CAS path has parent");
        let count = std::fs::read_dir(bucket).unwrap().count();
        assert_eq!(count, 1, "dedup must not create duplicate files");
    }

    // -- Payload size limit --------------------------------------

    #[test]
    fn payload_too_large_rejected() {
        let cas = temp_cas();
        // Chunk that exceeds the 10-byte limit.
        let data = vec![0u8; 100];
        let stream = byte_stream(data);
        let result = cas.store_dicom(stream, 10);
        assert!(matches!(result, Err(StorageError::PayloadTooLarge(_))));
    }

    // -- Reader error propagation --------------------------------

    /// A `Read` that yields one buffer then returns a mid-read error.
    struct FailingReader {
        first: Vec<u8>,
        done: bool,
    }

    impl std::io::Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.done {
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, "simulated"));
            }
            self.done = true;
            // Fill the WHOLE buffer so `store_dicom` has already written a
            // chunk to the temp file before the error arrives; otherwise the
            // cleanup path under test never runs with a file on disk.
            let n = self.first.len().min(buf.len());
            buf[..n].copy_from_slice(&self.first[..n]);
            Ok(n)
        }
    }

    /// Helper: the `.dcm` names present in `_tmp`, with inspection failures
    /// turned into a test failure rather than an empty list.
    ///
    /// The earlier shape - `filter_map(|e| e.ok())` over `read_dir` plus
    /// `unwrap_or_default()` - made both cleanup tests unable to fail: an
    /// unreadable `_tmp` became "no temp files", which is exactly the assertion
    /// being made. A directory that cannot be listed is NOT evidence of
    /// cleanup, so it has to abort the test.
    #[track_caller]
    fn temp_dcm_names(cas: &ContentAddressedStorage) -> Vec<String> {
        let dir = cas.root.join("_tmp");
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
        let mut names = Vec::new();
        for entry in entries {
            let entry =
                entry.unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", dir.display()));
            names.push(entry.file_name().to_string_lossy().to_string());
        }
        names.retain(|n| n.ends_with(".dcm"));
        names
    }

    /// The fixture must remove its directory tree when it goes out of scope.
    ///
    /// Falsified by neutering `RemoveTreeOnDrop::drop` (the directory stays and
    /// the test fails), so this pins the ownership the review asked for rather
    /// than merely calling `temp_dir()`.
    #[test]
    fn the_temp_cas_fixture_removes_its_directory_on_drop() {
        let dir = {
            let cas = temp_cas();
            let root = cas.root.clone();
            std::fs::write(root.join("_tmp").join("payload.dcm"), b"x").expect("seed");
            assert!(root.is_dir(), "the fixture must have created {root:?}");
            root
        };
        assert!(
            !dir.exists(),
            "the fixture left its directory behind: {}",
            dir.display()
        );
    }

    /// A directory that cannot be listed must FAIL the cleanup assertion, not
    /// silently read as "empty".
    ///
    /// This is the falsifier for the `filter_map(..ok())` /
    /// `unwrap_or_default()` shape: it proves the strict helper rejects a
    /// listing failure instead of reporting success. Unix-only because the
    /// mechanism is a permission bit; on Windows the equivalent would need an
    /// ACL and an elevated token.
    #[cfg(unix)]
    #[test]
    fn an_unlistable_tmp_dir_is_not_reported_as_clean() {
        use std::os::unix::fs::PermissionsExt;

        let cas = temp_cas();
        let tmp = cas.root.join("_tmp");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let listed = std::panic::catch_unwind(|| temp_dcm_names(&cas).is_empty());
        // Restore before asserting so a failure still leaves the tree removable.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).expect("chmod back");

        match listed {
            // Running as root ignores the mode bits; the check cannot be
            // exercised then, and claiming it passed would be a vacuous test.
            Ok(clean) if clean => eprintln!("skipped: unreadable dir still listed (root?)"),
            Ok(clean) => panic!("helper returned {clean:?} instead of failing"),
            Err(_) => {}
        }
    }

    #[test]
    fn stream_error_cleans_up_temp() {
        let cas = temp_cas();
        // A full 64 KiB chunk guarantees a write happened before the failure.
        let reader = FailingReader {
            first: vec![0xA5u8; 65536],
            done: false,
        };
        let result = cas.store_dicom(reader, 500 * 1024 * 1024);
        assert!(matches!(result, Err(StorageError::Io(_))));

        // The failed write must leave NOTHING behind. Iterating `.dcm` entries
        // and only calling `metadata()` (the earlier shape of this test) passes
        // even when the partial file is still there, so it asserted nothing.
        let leftover = temp_dcm_names(&cas);
        assert!(
            leftover.is_empty(),
            "failed store left temp files behind: {leftover:?}"
        );
    }

    #[test]
    fn an_oversized_write_leaves_no_temp_file() {
        let cas = temp_cas();
        let result = cas.store_dicom(byte_stream(vec![7u8; 4096]), 16);
        assert!(matches!(result, Err(StorageError::PayloadTooLarge(_))));

        let leftover = temp_dcm_names(&cas);
        assert!(
            leftover.is_empty(),
            "size-cap rejection left temp files behind: {leftover:?}"
        );
    }

    // -- Read non-existent ----------------------------------------

    #[test]
    fn read_not_found() {
        let cas = temp_cas();
        let fake_hash = "a".repeat(64);
        let result = cas.read_dicom(&fake_hash);
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }

    // -- Invalid hash validation ---------------------------------

    #[test]
    fn invalid_hash_rejected() {
        let cas = temp_cas();

        // Wrong length.
        assert!(matches!(
            cas.read_dicom("too-short"),
            Err(StorageError::InvalidHash(_))
        ));

        // Path traversal attempt: "../" prefixes.
        let traversal = "../".repeat(32); // 96 chars, wrong length anyway
        assert!(matches!(
            cas.read_dicom(&traversal),
            Err(StorageError::InvalidHash(_))
        ));

        // Non-hex characters in a 64-char string.
        let non_hex = format!("{}../etc/passwd", "g".repeat(50));
        assert!(matches!(
            cas.read_dicom(&non_hex),
            Err(StorageError::InvalidHash(_))
        ));
    }

    // -- Remove --------------------------------------------------

    #[test]
    fn remove_existing_file() {
        let cas = temp_cas();
        let data = b"removable content";
        let stream = byte_stream(data.to_vec());
        let hash = cas.store_dicom(stream, 500 * 1024 * 1024).expect("store");

        assert!(cas.exists(&hash));

        cas.remove(&hash).expect("remove");
        assert!(!cas.exists(&hash));
    }

    #[test]
    fn remove_non_existent_is_error() {
        let cas = temp_cas();
        let fake_hash = "b".repeat(64);
        let result = cas.remove(&fake_hash);
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }

    // -- Storage availability probe ------------------------------

    #[test]
    fn storage_available_in_temp_dir() {
        let cas = temp_cas();
        assert!(cas.is_storage_available());
    }

    // -- Concurrent STOW (C1/REAUDIT) ----------------------------

    #[test]
    fn concurrent_stow_same_file_no_data_loss() {
        let cas = temp_cas();
        let data = b"concurrent DICOM stow test payload";
        let expected_hash = blake3_hex(data);

        // 10 parallel STOWs of identical content - all must return same hash.
        let cas_ref = &cas;
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..10)
                .map(|_| {
                    let chunk = data.to_vec();
                    scope.spawn(move || cas_ref.store_dicom(byte_stream(chunk), 500 * 1024 * 1024))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("stow thread join"))
                .collect()
        });

        // All 10 must succeed.
        for (i, r) in results.iter().enumerate() {
            assert!(r.is_ok(), "task {} failed: {:?}", i, r);
        }

        // All must return the same hash - dedup worked.
        let first = results[0].as_ref().unwrap();
        for r in &results {
            assert_eq!(r.as_ref().unwrap(), first);
        }
        assert_eq!(*first, expected_hash);

        // Only one file on disk - dedup prevented duplicates.
        let path = cas.cas_path(&expected_hash);
        assert!(path.exists());
        let bucket = path.parent().unwrap();
        let count = std::fs::read_dir(bucket).unwrap().count();
        assert_eq!(count, 1, "concurrent STOW must not create duplicate files");
    }

    // -- SEU detection: verify_integrity -------------------------

    #[test]
    fn test_verify_integrity_corruption_detected() {
        let cas = temp_cas();

        // 1. STOW a file with random content (10 KB)
        let data: Vec<u8> = (0..10240).map(|i| (i % 256) as u8).collect();
        let expected_hash = blake3_hex(&data);

        let stream = byte_stream(data.clone());
        let hash = cas
            .store_dicom(stream, 500 * 1024 * 1024)
            .expect("store_dicom");
        assert_eq!(hash, expected_hash);

        // 2. verify_integrity -> OK
        let size = cas
            .verify_integrity(&hash)
            .expect("verify_integrity should pass on clean file");
        assert_eq!(size, data.len() as u64);

        // 3. Inject SEU: flip 1 bit at the middle of the file on disk
        let file_path = cas.cas_path(&hash);
        let mut on_disk = std::fs::read(&file_path).expect("read on-disk file");
        let mid = on_disk.len() / 2;
        on_disk[mid] ^= 0x01; // flip one bit
        std::fs::write(&file_path, &on_disk).expect("write corrupted file");

        // 4. verify_integrity -> Err(IntegrityFailure)
        let result = cas.verify_integrity(&hash);
        match result {
            Err(StorageError::IntegrityFailure { expected, actual }) => {
                // Both must be 64-char hex strings, but different
                assert_eq!(expected.len(), 64);
                assert_eq!(actual.len(), 64);
                assert_ne!(expected, actual);
                assert_eq!(expected, hash.to_lowercase());
            }
            other => panic!("Expected IntegrityFailure, got {:?}", other),
        }
    }

    // -- SEU detection: read_all detects corruption --------------

    #[test]
    fn test_read_all_detects_corruption() {
        let cas = temp_cas();

        // 1. STOW a file
        let data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        let stream = byte_stream(data.clone());
        let hash = cas
            .store_dicom(stream, 500 * 1024 * 1024)
            .expect("store_dicom");

        // 2. read_all -> OK (bytes match)
        let read_bytes = cas.read_all(&hash).expect("read_all should succeed");
        assert_eq!(read_bytes, data);

        // 3. Inject SEU: flip a bit on disk
        let file_path = cas.cas_path(&hash);
        let mut on_disk = std::fs::read(&file_path).expect("read on-disk file");
        on_disk[1000] ^= 0x01;
        std::fs::write(&file_path, &on_disk).expect("write corrupted file");

        // 4. read_all -> Err(IntegrityFailure)
        let result = cas.read_all(&hash);
        assert!(
            matches!(result, Err(StorageError::IntegrityFailure { .. })),
            "read_all must detect SEU corruption"
        );
    }

    // -- SEU detection: scrub finds corrupted files --------------

    #[test]
    fn test_scrub_detects_corruption() {
        let cas = temp_cas();

        // 1. STOW three files
        let data1: Vec<u8> = (0..2048).map(|i| (i % 256) as u8).collect();
        let data2: Vec<u8> = (0..2048).map(|i| ((i + 128) % 256) as u8).collect();
        let data3: Vec<u8> = (0..2048).map(|i| ((i + 200) % 256) as u8).collect();

        let _hash1 = cas
            .store_dicom(byte_stream(data1.clone()), 500 * 1024 * 1024)
            .expect("store 1");
        let hash2 = cas
            .store_dicom(byte_stream(data2.clone()), 500 * 1024 * 1024)
            .expect("store 2");
        let _hash3 = cas
            .store_dicom(byte_stream(data3.clone()), 500 * 1024 * 1024)
            .expect("store 3");

        // 2. Inject SEU in the middle file (hash2)
        let file_path = cas.cas_path(&hash2);
        let mut on_disk = std::fs::read(&file_path).expect("read on-disk file");
        on_disk[512] ^= 0x01;
        std::fs::write(&file_path, &on_disk).expect("write corrupted file");

        // 3. Scrub
        let report = cas.scrub().expect("scrub should succeed");

        // 4. Verify report
        assert!(report.total >= 3, "total={}, expected >= 3", report.total);
        assert!(
            report.healthy >= 2,
            "healthy={}, expected >= 2",
            report.healthy
        );
        assert!(
            report.corrupted.contains(&hash2),
            "corrupted must contain hash2: {:?}",
            report.corrupted
        );
        assert_eq!(
            report.corrupted.len(),
            1,
            "exactly 1 file should be corrupted: {:?}",
            report.corrupted
        );
    }

    // -- SEU detection: scrub empty store ------------------------

    #[test]
    fn test_scrub_empty_store() {
        let cas = temp_cas();

        let report = cas.scrub().expect("scrub should succeed on empty store");

        assert_eq!(report.total, 0);
        assert_eq!(report.healthy, 0);
        assert!(report.corrupted.is_empty());
        assert!(report.unreadable.is_empty());
    }

    // A bucket that vanishes between the parent listing and the scrub_dir
    // call must not abort the sweep: the erase worker unlinks empty buckets
    // concurrently. The whole `read_dir` returned NotFound, so the bucket is
    // simply skipped (nothing was lost that still needs checking).
    #[test]
    fn scrub_skips_a_vanished_bucket() {
        let cas = temp_cas();
        let mut report = ScrubReport {
            total: 0,
            healthy: 0,
            corrupted: Vec::new(),
            unreadable: Vec::new(),
        };
        let missing = cas.root_dir().join("ab").join("cd");
        assert!(!missing.exists());
        cas.scrub_dir(&mut report, missing)
            .expect("vanished bucket is skipped");
        assert_eq!(report.total, 0);
        assert!(report.unreadable.is_empty());
    }

    // -- read_stream chunked streaming ---------------------------

    #[test]
    fn test_read_stream_chunks() {
        let cas = temp_cas();

        // 1. STOW a file with random 200 KB
        let data: Vec<u8> = (0..204800).map(|i| (i % 256) as u8).collect();
        let expected_hash = blake3_hex(&data);

        let stream = byte_stream(data.clone());
        let hash = cas
            .store_dicom(stream, 500 * 1024 * 1024)
            .expect("store_dicom");
        assert_eq!(hash, expected_hash);

        // 2. read_stream -> read all bytes through the returned `Read`.
        let (mut reader, size) = cas.read_stream(&hash).expect("read_stream should succeed");

        let mut collected = Vec::new();
        reader.read_to_end(&mut collected).expect("read to end");

        // 3. Verify collected bytes match original
        assert_eq!(collected, data);
        assert_eq!(size, data.len() as u64);
        assert_eq!(collected.len() as u64, size);

        // 4. Verify chunk reads never exceed a caller-supplied 64 KB buffer.
        let (mut reader2, _size2) = cas.read_stream(&hash).expect("second read_stream");
        let mut buf = vec![0u8; 65536];
        let mut total = 0usize;
        loop {
            let n = reader2.read(&mut buf).expect("chunk should be Ok");
            if n == 0 {
                break;
            }
            assert!(n <= 65536, "read returned {n} bytes exceeding 64 KB buffer");
            total += n;
        }
        assert_eq!(total, data.len());
    }

    // -- Two-level layout (A-005) -------------------------------

    #[test]
    fn stored_file_lives_in_two_level_layout() {
        let cas = temp_cas();
        let data = b"layout check payload";
        let stream = byte_stream(data.to_vec());
        let hash = cas
            .store_dicom(stream, 500 * 1024 * 1024)
            .expect("store_dicom");

        // Path must be {root}/{hash[0..2]}/{hash[2..4]}/{hash}.dcm
        let expected = cas
            .root_dir()
            .join(&hash[0..2])
            .join(&hash[2..4])
            .join(format!("{hash}.dcm"));
        assert!(
            expected.exists(),
            "two-level path not used: {}",
            expected.display()
        );
    }

    #[test]
    fn cas_path_points_at_two_level_layout() {
        let cas = temp_cas();
        let hash = "ab".repeat(32);
        let path = cas.cas_path(&hash);
        let path_str = path.to_string_lossy().replace('\\', "/");
        assert!(
            path_str.ends_with(&format!("{}/{}/{}.dcm", &hash[0..2], &hash[2..4], hash)),
            "cas_path did not produce two-level layout: {path:?}"
        );
    }

    /// Invariant guard: the canonical geometry must be a FORWARD-SLASH string,
    /// byte-exact for a fixed hash (hot/cold/S3 all build on this).
    #[test]
    fn cas_rel_path_is_forward_slash_two_level() {
        let hash = "a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2";
        // Hand-computed: {hash[..2]}/{hash[2..4]}/{hash}.dcm with '/' separators.
        assert_eq!(
            cas_rel_path(hash),
            "a7/b8/a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2.dcm"
        );
        // Never a backslash (a PathBuf-derived key would corrupt S3 keys).
        assert!(!cas_rel_path(hash).contains('\\'));
        // cas_path builds on it (root + canonical rel path).
        let cas = temp_cas();
        assert_eq!(cas.cas_path(hash), cas.root_dir().join(cas_rel_path(hash)));
    }

    /// Helper: a CAS under the crate's own output directory, for the cases that
    /// inspect file CONTENT after a rename.
    fn scratch_cas(tag: &str) -> ContentAddressedStorage {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join(format!("cas-test-{tag}-{}", unique_id()));
        ContentAddressedStorage::new(dir).expect("create scratch CAS")
    }

    // -- F1: dedup must prove content, not existence --------------

    /// A directory where `{hash}.dcm` should be is not a stored object, and the
    /// rename onto it can never succeed. The existence-based arm acked that
    /// state as "already stored" and let the guard discard the valid payload.
    #[test]
    fn dedup_rejects_a_directory_at_the_target() {
        let cas = scratch_cas("dir-squatter");
        let data = b"directory-shadow payload";
        let hash = blake3_hex(data);
        let target = cas.cas_path(&hash);
        std::fs::create_dir_all(target.parent().expect("bucket")).expect("bucket dir");
        // Non-empty, so the rename cannot replace it on any platform.
        std::fs::create_dir(&target).expect("squat with a directory");
        std::fs::write(target.join("keep"), b"x").expect("non-empty");

        let result = cas.store_dicom(byte_stream(data.to_vec()), 500 * 1024 * 1024);
        assert!(
            result.is_err(),
            "a directory at the target must not be reported as a successful dedup: {result:?}"
        );
        assert!(!target.is_file(), "the squatter must not be removed");
        assert!(target.join("keep").is_file(), "its contents must survive");
    }

    /// A regular file at the key whose bytes hash to something else must not be
    /// credited as dedup when the rename cannot replace it: an Ok has to mean
    /// the object under the key IS the payload. Where the platform CAN replace
    /// it (Windows MoveFileEx) the rename repairs the object; where it cannot
    /// (POSIX rename onto an existing file) the failure must stand rather than
    /// be swallowed by an existence check.
    #[test]
    fn dedup_rejects_a_wrong_content_file_at_the_target() {
        let cas = scratch_cas("wrong-content");
        let data = b"payload the key was computed from";
        let hash = blake3_hex(data);
        let target = cas.cas_path(&hash);
        std::fs::create_dir_all(target.parent().expect("bucket")).expect("bucket dir");
        std::fs::write(&target, b"completely different bytes").expect("seed wrong content");

        let result = cas.store_dicom(byte_stream(data.to_vec()), 500 * 1024 * 1024);
        match result {
            Ok(key) => {
                assert_eq!(key, hash);
                assert_eq!(
                    std::fs::read(&target).expect("read target"),
                    data,
                    "an Ok must mean the object under the key IS the payload"
                );
            }
            Err(_) => assert_eq!(
                std::fs::read(&target).expect("read target"),
                b"completely different bytes",
                "a rejected ingest must leave the wrong object for scrub to find"
            ),
        }
    }
    /// The second write of identical content is a dedup, not a duplicate.
    ///
    /// The platform decides how it arrives: Windows reports `AlreadyExists`
    /// for the rename and takes the dedup arm, POSIX cannot replace so the
    /// same arm runs with the same test. Either way the object under the key
    /// must still be intact and readable, and the temp file must be gone.
    #[test]
    fn dedup_accepts_an_identical_file_and_keeps_its_inode() {
        let cas = temp_cas();
        let data = b"already stored, byte for byte";
        let hash = blake3_hex(data);
        let first = cas
            .store_dicom(byte_stream(data.to_vec()), 500 * 1024 * 1024)
            .expect("first store");
        assert_eq!(first, hash);
        let target = cas.cas_path(&hash);

        let second = cas
            .store_dicom(byte_stream(data.to_vec()), 500 * 1024 * 1024)
            .expect("dedup store");
        assert_eq!(second, hash);

        assert_eq!(std::fs::read(&target).expect("read"), data);
        let leftover = temp_dcm_names(&cas).len();
        assert_eq!(leftover, 0, "dedup must clean its temp file");
    }

    // -- F2: the published name is durable ------------------------

    /// The store must flush the directory that names the new object, proven by
    /// the barrier counter: a store into a fresh two-level bucket flushes at
    /// least one directory. Making `sync_dir_entry` a no-op drops it to zero.
    #[cfg(unix)]
    #[test]
    fn store_flushes_the_directory_entry() {
        let cas = temp_cas();
        let before = sync_barriers_observed();
        let hash = cas
            .store_dicom(byte_stream(b"durable name".to_vec()), 500 * 1024 * 1024)
            .expect("store");
        assert!(cas.exists(&hash));
        assert!(
            sync_barriers_observed() > before,
            "the destination directory must be fsynced after the rename"
        );
    }

    /// A store into an EXISTING bucket has no new directory names to persist
    /// but must still flush the holding directory, so the counter cannot come
    /// from the bucket walk alone.
    #[cfg(unix)]
    #[test]
    fn a_second_store_still_flushes_the_holding_directory() {
        let cas = temp_cas();
        let hash = blake3_hex(b"first");
        cas.store_dicom(byte_stream(b"first".to_vec()), 500 * 1024 * 1024)
            .expect("first store");
        let bucket = cas.cas_path(&hash);
        let bucket = bucket.parent().expect("bucket").to_path_buf();
        assert!(bucket.is_dir(), "the bucket must already exist");

        let before = sync_barriers_observed();
        cas.store_dicom(byte_stream(b"second, new key".to_vec()), 500 * 1024 * 1024)
            .expect("second store");
        assert!(
            sync_barriers_observed() > before,
            "an existing bucket still needs its new entry flushed"
        );
    }

    /// A barrier failure is a store failure: the ack must be an error, never Ok,
    /// and the guard still owns the temp file.
    ///
    /// The seam is armed on the directory that HOLDS the object, not on its
    /// parent bucket: a store into a fresh bucket flushes the bucket first, and
    /// if that one fails the store is already rejected before the rename, which
    /// would test the wrong arm.
    #[cfg(unix)]
    #[test]
    fn a_failed_directory_barrier_is_not_acked_as_stored() {
        let cas = temp_cas();
        let data = b"barrier failure payload".to_vec();
        let hash = blake3_hex(&data);
        let target = cas.cas_path(&hash);
        let bucket = target.parent().expect("bucket");
        std::fs::create_dir_all(bucket).expect("bucket dir");

        fail_next_barrier_for_test(bucket);
        let result = cas.store_dicom(byte_stream(data), 500 * 1024 * 1024);
        assert!(
            matches!(result, Err(StorageError::Io(ref e)) if e.kind() == std::io::ErrorKind::StorageFull),
            "an unsynced directory must not be reported as stored: {result:?}"
        );

        let leftover = temp_dcm_names(&cas).len();
        assert_eq!(leftover, 0, "a failed store must not leak its temp file");
    }

    // -- F3: permission failures are not "deleted" ----------------

    /// A bucket the process cannot read must not be silently skipped: the old
    /// `is_gone` classified the failure as a normal race and `cas_stats`
    /// reported a healthy partial count.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_bucket_is_reported_not_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let cas = temp_cas();
        let bucket = cas.root_dir().join("ab").join("cd");
        std::fs::create_dir_all(&bucket).expect("bucket dir");
        std::fs::set_permissions(&bucket, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");

        let result = cas.cas_stats();

        // Restore first so a failure still cleans the directory up.
        std::fs::set_permissions(&bucket, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 700");
        assert!(
            matches!(result, Err(StorageError::Io(_))),
            "an unreadable bucket must surface as an I/O error: {result:?}"
        );
    }

    // -- F4: remove maps the unlink result ------------------------

    /// An ancestor that cannot be traversed is a permission problem, not
    /// "this object was never stored".
    #[cfg(unix)]
    #[test]
    fn remove_through_an_inaccessible_ancestor_is_io_not_not_found() {
        use std::os::unix::fs::PermissionsExt;
        let cas = temp_cas();
        let hash = cas
            .store_dicom(byte_stream(b"locked away".to_vec()), 500 * 1024 * 1024)
            .expect("store");
        let first_level = cas.root_dir().join(&hash[0..2]);
        std::fs::set_permissions(&first_level, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");

        let result = cas.remove(&hash);

        std::fs::set_permissions(&first_level, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 700");
        assert!(
            matches!(result, Err(StorageError::Io(_))),
            "an inaccessible ancestor must not be mapped to NotFound: {result:?}"
        );
    }

    // -- F5: the key is case-normalized ---------------------------

    /// Writes name buckets with the lowercase digest, so every accepted
    /// spelling must resolve to the same object.
    #[test]
    fn an_uppercase_key_resolves_to_the_lowercase_object() {
        let cas = temp_cas();
        let data = b"case folding payload";
        let lower = blake3_hex(data);
        let upper = lower.to_ascii_uppercase();
        cas.store_dicom(byte_stream(data.to_vec()), 500 * 1024 * 1024)
            .expect("store");

        // The path is the falsifiable half, and the only one this test can
        // tell apart on a case-INSENSITIVE filesystem (NTFS, the default CI
        // volume): there both spellings open the same file for free, while on
        // a case-sensitive one an un-normalized key is a different, absent
        // path - a silent "already deleted" for every reader.
        assert_eq!(
            cas.cas_path(&upper),
            cas.cas_path(&lower),
            "an uppercase key must address the stored (lowercase) object"
        );

        assert!(cas.exists(&upper), "an uppercase key must find the object");
        assert_eq!(
            cas.verify_integrity(&upper).expect("verify"),
            data.len() as u64
        );
        assert_eq!(cas.read_all(&upper).expect("read_all"), data);
        cas.remove(&upper).expect("remove by uppercase key");
        assert!(!cas.exists(&lower), "the lowercase object must be gone");
    }

    // -- F6: unexpected cleanup failures are reported -------------

    /// A cleanup step that fails for an unexpected reason must be counted, not
    /// dropped; the expected races (already gone, non-empty bucket) are not.
    #[cfg(unix)]
    #[test]
    fn unexpected_cleanup_failures_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        let cas = temp_cas();
        let before = cleanup_failures_observed();

        // A directory the process cannot write into: the unlink of a child
        // fails with EACCES, neither NotFound nor a non-empty race.
        let locked = cas.root_dir().join("_tmp2");
        std::fs::create_dir_all(&locked).expect("dir");
        std::fs::write(locked.join("child.dcm"), b"x").expect("child");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))
            .expect("chmod 500");
        remove_cleanup_target(&locked.join("child.dcm"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 700");

        assert!(
            cleanup_failures_observed() > before,
            "a failed temp cleanup must be reported"
        );

        // The expected cases stay silent.
        let middle = cleanup_failures_observed();
        remove_cleanup_target(&locked.join("never-existed.dcm"));
        let occupied = cas.root_dir().join("ab").join("cd");
        std::fs::create_dir_all(&occupied).expect("bucket");
        std::fs::write(occupied.join("x.dcm"), b"x").expect("record");
        remove_cleanup_dir(&occupied);
        assert_eq!(
            cleanup_failures_observed(),
            middle,
            "NotFound and a non-empty bucket are expected, not reported"
        );
    }
    /// An unexpected cleanup failure is reported, and the expected races are
    /// not.
    ///
    /// The failed step is a REMOVE. The bucket is created up front so nothing
    /// here goes through `create_dir_all`, whose Windows residue (a directory
    /// left in a transient state) would otherwise be credited to the removal.
    #[test]
    fn unexpected_cleanup_failures_are_reported_on_every_platform() {
        let cas = temp_cas();
        let bucket = cas.root_dir().join("ab").join("cd");
        std::fs::create_dir_all(&bucket).expect("bucket");

        let before = cleanup_failures_observed();
        // `remove_file` on a directory fails with EISDIR / ERROR_ACCESS_DENIED.
        remove_cleanup_target(&bucket);
        assert!(
            cleanup_failures_observed() > before,
            "a cleanup that cannot succeed must be reported"
        );

        let middle = cleanup_failures_observed();
        remove_cleanup_target(&bucket.join("never-existed.dcm"));
        assert_eq!(
            cleanup_failures_observed(),
            middle,
            "NotFound is the expected race, not a reportable failure"
        );

        std::fs::write(bucket.join("x.dcm"), b"x").expect("record");
        remove_cleanup_dir(&bucket);
        assert_eq!(
            cleanup_failures_observed(),
            middle,
            "a bucket that still holds records must be left alone silently"
        );
    }

    /// The primary error wins: a stream failure is returned even though the
    /// guard's cleanup also ran, and the partial file is gone.
    #[test]
    fn a_primary_upload_error_is_still_the_returned_error() {
        let cas = temp_cas();
        let reader = FailingReader {
            first: vec![0x5Au8; 65536],
            done: false,
        };
        let result = cas.store_dicom(reader, 500 * 1024 * 1024);
        match result {
            Err(StorageError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
            other => panic!("expected the upstream Io error, got {other:?}"),
        }
        let leftover = temp_dcm_names(&cas).len();
        assert_eq!(leftover, 0);
    }
}
