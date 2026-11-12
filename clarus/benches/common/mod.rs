// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Shared helpers for the benchmark suites.
//!
//! The DICOM corpus lives OUTSIDE the workspace at `D:\criterion\` (438 files,
//! 518.5 MB) and is not tracked by git. Benchmarks must never copy those files
//! into the repository -- they read directly from the corpus path.
//!
//! The corpus path can be overridden at runtime via `CLARUS_CRITERION_DIR`
//! so CI (which has no `D:\criterion`) can point at a checkout without editing
//! source code.
//!
//! Cargo compiles each `[[bench]]` target as an independent crate, so any item
//! used only by a sibling bench is flagged dead in the current one. This module
//! keeps the corpus helper functions shared across the benchmark suites;
//! 2.3v-2 removed the DbActor/SQLite write-path helpers together with the
//! obsolete `stow_bench` and `concurrency_bench` (they measured the SQLite
//! single-writer architecture, which no longer exists).
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Resolve the DICOM corpus directory.
///
/// Priority: `CLARUS_CRITERION_DIR` env var, then the Windows default.
/// Keeping the default colon-prefixed literal is intentional -- the corpus is
/// a local benchmark artifact, not a runtime dependency of the server.
pub fn criterion_dir() -> PathBuf {
    std::env::var("CLARUS_CRITERION_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("D:\\criterion"))
}

/// CT dental slices: `CT_*.dcm` (JPEG-LS Lossless, 1.2.840.10008.1.2.4.80).
///
/// 432 files, ~1.2 MB each. This is the "hot" read/write corpus for most
/// scenarios. Sorted by name for a deterministic per-run pick order.
pub fn ct_slices(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("criterion corpus directory must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("CT_") && n.ends_with(".dcm"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

/// Read a whole file into memory. Used once at setup time to pre-load the
/// corpus buffers that benchmarks iterate over (never inside the timed loop).
pub fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}
