// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! On-disk study indexes (`idx/`) - the shadow B-tree tier.
//!
//! Each index is a set of append-only files, one per content-addressed
//! bucket key and day. Every row is a fixed-size record that is lexicographically
//! sorted when the bucket key is a BLAKE3 hash (chronological order), enabling
//! binary search on read. Writes use `O_APPEND` (a single `write()` syscall,
//! never overwrites, safe under a crash because a torn row is only ever at
//! the tail and is skipped on read).
//!
//! # Immutability model
//!
//! - Today's files are `O_APPEND` (mutable, written by STOW).
//! - A midnight rollover ([`rollover`]) closes today's files and renames them
//!   to the immutable set that read-only mmap consumers may map forever.
//!
//! # Zero-allocation hotpath
//!
//! `append` writes into a caller-provided fixed buffer (a hex blob + a byte)
//! then issues one `write()`. No `Vec`, no `format!`, no heap. See
//! [`tests`](crate::idx::tests) for the `TestAllocator` gate (B-011).
//!
//! # Directory layout
//!
//! ```text
//! idx/
//! +-- dates/YYYYMMDD.idx              <- blake3_hash_hex\n per study
//! +-- modality/{CT,MR,...}/YYYYMMDD.idx <- study_uid\n per study
//! +-- sop_owner/YYYYMMDD.idx          <- SOP(64)|Study(64)|\n fixed rows
//! ```

pub mod date;
pub mod intersect;
pub mod modality;
pub mod ngram;
pub mod preload;
pub mod rollover;
pub mod sop_owner;

/// A study identity used to maintain the shadow indexes.
///
/// Carries exactly the keys the index layer needs: the content hash that
/// dates/ indexes on and the modality + study UID that modality/ indexes on.
#[derive(Debug, Clone)]
pub struct StudyRef {
    /// BLAKE3 hash of the stored instance's bytes (hex form derived on write).
    pub blake3: [u8; 32],
    /// DICOM Modality (0008,0060), e.g. "CT". Not normalized here.
    pub modality: String,
    /// DICOM StudyInstanceUID (0020,000D), the value appended to modality rows.
    pub study_uid: String,
}

/// Result of an append to a per-day index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The row was appended to the mutable (today's) file.
    Appended,
    /// A concurrent rollover moved today's file; the caller should retry.
    Retry,
}
