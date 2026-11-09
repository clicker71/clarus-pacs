// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! B-tree query engine.
//!
//! SQLite was the primary tier for QIDO. This module is the replacement: it
//! answers study-level searches straight from the B-tree artifacts - the
//! per-study CBOR manifests (`manifests/{study_uid}.manifest`) and the daily
//! `.idx` accelerators (`dates/`, `modality/`) written by the
//! dual-write path.
//!
//! # Query plan (mirrors the plan's algorithm)
//!
//! 1. **Modality acceleration (hot path).** A `Modality` search key opens the
//!    `modality/{MOD}/YYYYMMDD.idx` rows (optionally restricted to a study
//!    date range) whose rows carry the candidate `study_uid`s directly.
//! 2. **Manifest filtering.** Each candidate (plus the whole `manifests/`
//!    directory when no accelerator applies - the cold path) has its manifest
//!    mmap-ed and parsed, then every remaining search key is matched against
//!    the study attributes.
//! 3. **Streaming JSON.** Matching studies are emitted as DICOMweb JSON into a
//!    caller-provided reusable buffer (no `serde_json::Value` on the study
//!    hotpath) - the primitive behind the QIDO streaming response.
//!
//! # Honest scope of the date `.idx`
//!
//! The `dates/YYYYMMDD.idx` files store BLAKE3 **instance** hashes, not study
//! UIDs. There is no hash->study-UID side index, so a date-only query cannot
//! name its candidates without a manifest scan. Per the plan's cold-path rule
//! ("full scan `manifests/`, <10 ms P99 for <100K studies") a date-only search
//! falls back to scanning manifests and filtering on `StudyDate`. The modality
//! index remains the true hot accelerator because its rows are study UIDs.
//!
//! # Invariant
//!
//! The manifest is the source of truth for QIDO attributes. `.idx` files only
//! narrow the candidate set; a stale or missing `.idx` never loses a study,
//! it just widens the scan. Correctness never depends on index freshness.

pub mod bitap;
mod filter;
pub mod json;
pub mod patients;
mod scan;
pub mod series;
pub mod study_index;
pub mod xml;

pub use filter::{
    Predicate, QueryFilter, glob_match, match_study_attrs, match_study_attrs_prebuilt, tag,
};
pub use patients::{PatientRow, find_recent_patients};
pub use scan::{StudyScanner, StudySearchError, StudySearchResult};
pub use series::{
    GlobalInstance, InstanceFilter, SeriesFilter, search_all_instances, search_all_series,
    search_instances, search_series,
};
pub use study_index::StudyQidoIndex;
