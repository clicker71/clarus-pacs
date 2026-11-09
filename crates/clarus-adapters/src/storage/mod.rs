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
/// failure. `PermissionDenied` belongs here on Windows: a file with a pending
/// delete is reported that way rather than as `NotFound`.
fn is_gone(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    )
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
            let _ = std::fs::remove_file(self.path);
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
        // `create_dir_all` is idempotent; safe to call here as well.
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
            if let Some(parent) = target_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::rename(&temp_path, &target_path) {
                Ok(()) => {
                    temp_guard.disarm();
                    break;
                }
                Err(_) if target_path.exists() => {
                    // Dedup: the content is already stored. Covers both
                    // `AlreadyExists` and the Windows case, where replacing an
                    // existing target surfaces as an access-denied error rather
                    // than a dedicated kind. The guard removes the temp file.
                    break;
                }
                Err(e) if attempt == 0 && e.kind() == std::io::ErrorKind::NotFound => {
                    // The bucket vanished under us; recreate and retry once.
                    attempt += 1;
                }
                Err(e) => return Err(e.into()),
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
    /// deleted (best-effort; failure is silently ignored).
    ///
    /// # Errors
    ///
    /// - `InvalidHash` if `blake3` is not valid hex.
    /// - `NotFound` if no file exists.
    /// - `Io` on filesystem errors during deletion.
    pub fn remove(&self, blake3: &str) -> Result<(), StorageError> {
        validate_hash_hex(blake3)?;

        let path = self.cas_path_two_level(blake3);
        if !path.exists() {
            return Err(StorageError::NotFound(blake3.to_string()));
        }

        std::fs::remove_file(&path)?;

        // Best-effort bucket cleanup: remove the second-level dir first,
        // then the first-level bucket if empty.
        if let Some(bucket) = path.parent() {
            let _ = std::fs::remove_dir(bucket);
        }
        if let Some(first_level) = path.parent().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir(first_level);
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
                let _ = std::fs::remove_file(&probe);
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
        for entry in std::fs::read_dir(&dir)? {
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
    /// never drift from cold/S3.
    fn cas_path_two_level(&self, hash: &str) -> PathBuf {
        self.root.join(cas_rel_path(hash))
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
    fn temp_cas() -> ContentAddressedStorage {
        let dir = std::env::temp_dir().join(format!("clarus-cas-test-{}", unique_id()));
        ContentAddressedStorage::new(dir).expect("create temp CAS")
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
        let leftover: Vec<String> = std::fs::read_dir(cas.root.join("_tmp"))
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".dcm"))
                    .collect()
            })
            .unwrap_or_default();
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

        let leftover: Vec<String> = std::fs::read_dir(cas.root.join("_tmp"))
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".dcm"))
                    .collect()
            })
            .unwrap_or_default();
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
}
