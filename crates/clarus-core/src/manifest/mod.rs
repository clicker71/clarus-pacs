// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Per-study CBOR manifest.
//!
//! A single `.manifest` file per study embeds the complete DICOM hierarchy
//! needed to reassemble the study without any separate index: the system
//! header, the study-level QIDO search keys, the series list, and for each
//! instance the full CAS frame-chunk table (BLAKE3 hashes + offsets).
//!
//! # Why one file instead of several
//!
//! - A single atomic `rename` commits the whole study at STOW time.
//! - The frame chunk table is critical for disaster recovery of CT/US/XA.
//! - Lazy recovery needs only to scan CAS: a `.manifest` implies the study
//!   is recoverable without rebuilding indexes.
//! - Size: per-instance overhead is ~64 bytes; a 1000-instance study is
//!   roughly 100-200 KB, far cheaper to mmap than to open the DICOM files.
//!
//! # CBOR shape
//!
//! ```cbor
//! {
//!   1: {1: version, 2: storage_tier, 3: total_pixel_bytes},   // sys
//!   2: {0x00080020: ..., ...},                                // qido_study
//!   3: [ { 0x0020000E: uid, 0x00080060: mod,
//!         4: [ { ..., 5: ts, 6: [ {1:no,2:hash,3:off,4:len} ] } ] } ]
//! }
//! ```
//!
//! Numeric keys keep the payload compact and let the decoder binary-search
//! map entries. System keys occupy 1-6; DICOM tags are the 32-bit tag value.

mod read;
mod record;
mod write;

pub use read::{ManifestReadError, read_manifest, read_manifest_at, read_manifest_study_only};
pub use record::{
    AttrValue, FrameRecord, InstanceRecord, Manifest, ManifestError, NOT_REJECTED_REASON,
    QUALITY_REASON_CODE, SeriesRecord, StudyQido, SysHeader, TIER_COLD, TIER_HOT, TIER_S3,
};
pub use write::WriteManifestError;
pub use write::{write_bytes_atomic, write_manifest};
