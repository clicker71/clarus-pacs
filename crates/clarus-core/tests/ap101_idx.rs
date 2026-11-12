// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! zero-heap guarantee for the `.idx` O_APPEND hotpath.
//!
//! Appending a BLAKE3-hash row (dates) or a study-UID row (modality) must not
//! allocate: the row is assembled into a fixed stack buffer and written with a
//! single O_APPEND syscall. This binary enforces that via `TestAllocator`.
//!
//! # Why a dedicated binary
//!
//! `TestAllocator` is a process-wide `#[global_allocator]`; it works only when
//! the measured code runs in an isolated single-threaded binary (ap101
//! DEVELOPER_GUIDE Section 9). All checks therefore live in one test function that
//! libtest runs on a single thread.

use clarus_core::idx::date::{self, DateIndex};
use clarus_core::idx::modality::{self, ModalityIndex};
use clarus_core::idx::rollover::DailyIndexSet;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Enable counting, run `f`, disable counting, report whether `f` allocated.
fn allocate_env<F, R>(f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let result = f();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (before == after, result)
}

/// Warm line-buffered async-less append helper for the date index.
fn append_dates(idx: &mut DateIndex, hashes: &[[u8; 32]]) {
    let mut staging = [0u8; date::ROW_LEN];
    for h in hashes {
        idx.append(h, &mut staging).expect("append");
    }
}

/// Warm append helper for a modality index.
fn append_modalities(idx: &mut ModalityIndex, rows: &[&str]) {
    let mut buf = [0u8; 128];
    for uid in rows {
        let row = modality::format_row(uid, &mut buf);
        idx.append(row).expect("append");
    }
}

#[test]
fn idx_oappend_never_allocates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let day = *b"20240115";

    // Warm up: create dirs + file handles (allocation on first open).
    let mut date_idx = date::open_appender(dir.path(), &day).expect("open date");
    let mut mod_idx = modality::open_appender(dir.path(), "CT", &day).expect("open mod");
    let mut hashes = [[0u8; 32]; 64];
    for (i, h) in hashes.iter_mut().enumerate() {
        h[0] = i as u8;
    }
    append_dates(&mut date_idx, &hashes);
    append_modalities(&mut mod_idx, &["uid0", "uid1"]);

    // Measured window: appends must allocate nothing.
    let (zero_alloc, _) = allocate_env(|| {
        append_dates(&mut date_idx, &hashes);
        append_modalities(&mut mod_idx, &["uid-x", "uid-y"]);
    });
    assert!(zero_alloc, ".idx O_APPEND must not allocate in the hotpath");

    // The registry-driven rollover path must equally allocate nothing.
    let mut set = DailyIndexSet::new(dir.path().to_path_buf(), day);
    // Warm up: open both writers (first open allocates registry slots / file
    // handles - cold path, once per bucket, outside the measured window).
    {
        let mut staging = [0u8; date::ROW_LEN];
        set.date_writer()
            .expect("writer")
            .append(&[0u8; 32], &mut staging)
            .expect("warm date");
        let mut row = [0u8; 128];
        set.modality_writer("MR")
            .expect("mod")
            .append(modality::format_row("warm", &mut row))
            .expect("warm mod");
    }
    let (zero_alloc_roll, _) = allocate_env(|| {
        let mut staging = [0u8; date::ROW_LEN];
        let blob = [0xABu8; 32];
        set.date_writer()
            .expect("writer")
            .append(&blob, &mut staging)
            .expect("append");
        let mut row = [0u8; 128];
        set.modality_writer("MR")
            .expect("mod")
            .append(modality::format_row("uid-mr", &mut row))
            .expect("append");
    });
    assert!(zero_alloc_roll, "registry append must not allocate");
}
