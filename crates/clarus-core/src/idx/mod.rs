// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! On-disk study indexes (`idx/`) - the shadow B-tree tier.
//!
//! Each index is a set of append-only files, one per content-addressed
//! bucket key and day. Writes use `O_APPEND` (a single `write()` syscall,
//! never overwrites, safe under a crash because a torn row is only ever at
//! the tail and is skipped on read).
//!
//! # Row formats, and what may be binary-searched
//!
//! The three files do NOT share one shape, and the difference decides how a
//! reader may search them:
//!
//! - `dates/YYYYMMDD.idx`: fixed [`date::ROW_LEN`] bytes, 64 lowercase hex
//!   chars + `\n`. Rows are appended in ARRIVAL order, so the file is NOT
//!   sorted by hash - a reader that wants binary search must sort it first
//!   (or search the sorted in-memory copy). The fixed width is what makes a
//!   torn tail detectable as a non-multiple length.
//! - `modality/{CS}/YYYYMMDD.idx`: VARIABLE length, `study_uid\n` per row, in
//!   arrival order. There is no fixed stride to align on, so a reader must
//!   split on `\n`; a missing final newline is a torn append and the fragment
//!   is not a row.
//! - `sop_owner/YYYYMMDD.idx`: fixed 129 bytes, `SOP(64)|Study(64)|\n`, and
//!   the rows are IMMUTABLE once the day is sealed.
//!
//! # Immutability model
//!
//! - Today's files are `O_APPEND` (mutable, written by STOW).
//! - A midnight rollover ([`rollover`]) CLOSES today's handles and stops
//!   writing to them; it does not rename or move the files. The day's files
//!   then become the immutable set that read-only mmap consumers may map.
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
