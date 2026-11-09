// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Atomic manifest write.
//!
//! A manifest is committed with a single `rename` so readers never observe a
//! partial file. The encoding is produced first, buffered, then written to a
//! sibling `*.tmp` file and atomically renamed over the target.
//!
//! # Durability model
//!
//! Per the plan, we deliberately do **not** `fsync` the temporary file. A
//! crash after `rename` can lose the manifest on some filesystems, but a
//! partial manifest is never visible - the same guarantee the rest of the
//! CAS layer relies on. `std::fs::rename` maps to `MoveFileExW` with
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

/// Atomically write `manifest` to `path`.
///
/// The encoding is done first so the serialization can never be interleaved
/// with disk writes. The payload is then flushed to `{path}.tmp` and renamed
/// over `path` in one atomic step.
pub fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), WriteManifestError> {
    let bytes = manifest
        .encode()
        .map_err(|e| WriteManifestError::Encode(e.to_string()))?;
    write_bytes_atomic(path, &bytes)
}

/// Write raw bytes atomically to `path` via a `*.tmp` sibling + rename.
///
/// Exposed separately for callers that already hold an encoded manifest.
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), WriteManifestError> {
    // Ensure parent directories exist (first STOW for a study). Idempotent.
    // Guard against a bare empty parent (a relative path with no directory).
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(WriteManifestError::Io)?;
    }

    let tmp_path = tmp_sibling(path);
    std::fs::write(&tmp_path, bytes).map_err(WriteManifestError::Io)?;

    // Guard against leaving a stale tmp behind if rename fails.
    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(WriteManifestError::Io(e))
        }
    }
}

/// Build `{path}.tmp` - the sibling used by the atomic rename.
fn tmp_sibling(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    std::path::PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::record::tests::sample_manifest;

    #[test]
    fn atomic_write_and_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        let manifest = sample_manifest();
        write_manifest(&manifest_path, &manifest).expect("write");

        // No tmp file left behind.
        assert!(!tmp_sibling(&manifest_path).exists());

        // Read back and compare (uses mmap reader from crate::manifest::read).
        let decoded = crate::manifest::read::read_manifest(&manifest_path).expect("read");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        let manifest = sample_manifest();
        write_manifest(&manifest_path, &manifest).expect("first write");
        write_manifest(&manifest_path, &manifest).expect("overwrite");

        let decoded = crate::manifest::read::read_manifest(&manifest_path).expect("read");
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
}
