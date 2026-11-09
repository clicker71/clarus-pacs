// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Manifest read via memory mapping.
//!
//! The whole `.manifest` file is mapped into memory and parsed directly from
//! the mapping - no buffered `read()` loop, no allocation for the byte
//! buffer. This is the read side of the plan's mmap contract (the write side
//! is [`crate::manifest::write`]).

use crate::manifest::record::Manifest;
use std::io;
use std::path::Path;

/// Errors produced by [`read_manifest`] / [`read_manifest_at`].
#[derive(Debug)]
pub enum ManifestReadError {
    /// Underlying filesystem or mmap failure.
    Io(io::Error),
    /// The mapped bytes did not form a valid manifest.
    Decode(String),
}

impl std::fmt::Display for ManifestReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestReadError::Io(e) => write!(f, "manifest mmap error: {e}"),
            ManifestReadError::Decode(e) => write!(f, "manifest decode error: {e}"),
        }
    }
}

impl std::error::Error for ManifestReadError {}

/// Read and decode a manifest from `path` by mmap-ing the whole file.
pub fn read_manifest(path: &Path) -> Result<Manifest, ManifestReadError> {
    let file = std::fs::File::open(path).map_err(ManifestReadError::Io)?;
    // The mmap itself lives in the audited OS boundary (crate::ffi): `file`
    // is a read-only handle, manifests are written temp+rename (no in-place
    // writer), and the mapping is dropped when this function returns.
    let mmap = crate::ffi::map_readonly(&file).map_err(ManifestReadError::Io)?;
    read_manifest_at(&mmap)
}

/// Same as [`read_manifest`] but parses **only** the study-level QIDO
/// attributes - series, instance, and frame records are skipped via
/// [`CborReader::skip_value`]. This is the QIDO hotpath.
pub fn read_manifest_study_only(path: &Path) -> Result<Manifest, ManifestReadError> {
    let file = std::fs::File::open(path).map_err(ManifestReadError::Io)?;
    let mmap = crate::ffi::map_readonly(&file).map_err(ManifestReadError::Io)?;
    Manifest::decode_study_only(&mmap).map_err(|e| ManifestReadError::Decode(e.to_string()))
}

/// Decode a manifest from an already-mapped byte range.
///
/// The caller owns the mapping; this just parses it. Useful when several
/// manifests share one mapping or when the bytes come from a non-file source.
pub fn read_manifest_at(bytes: &[u8]) -> Result<Manifest, ManifestReadError> {
    Manifest::decode(bytes).map_err(|e| ManifestReadError::Decode(e.to_string()))
}

impl Manifest {
    /// Find the instance record with `sop_instance_uid` across every series.
    ///
    /// This is the WADO-facing lookup: the handler receives
    /// `study_uid/series_uid/sop_uid` on the path, opens this study manifest,
    /// and resolves the instance's CAS address, storage tier and transfer
    /// syntax entirely from the manifest - no SQLite lookup. A missing
    /// instance yields `None` (a 404, not a 503).
    ///
    /// The search is a short linear scan over the series/instance sets; the
    /// attributes were decoded from the mmap and are borrowed back out of the
    /// manifest, so no allocation happens here.
    pub fn find_instance(
        &self,
        sop_instance_uid: &str,
    ) -> Option<&crate::manifest::InstanceRecord> {
        for series in &self.series {
            if let Some(inst) = series.instances.iter().find(|i| {
                // The SOP UID lives in the instance attrs under tag
                // (0008,0018); compare against its text value.
                i.attrs.iter().any(|(t, v)| {
                    *t == crate::query::tag::SOP_INSTANCE_UID
                        && matches!(v, crate::manifest::AttrValue::Text(s) if s == sop_instance_uid)
                })
            }) {
                return Some(inst);
            }
        }
        None
    }

    /// (0020,000D) Study Instance UID, when the manifest carries it.
    ///
    /// Read from the study attrs rather than taken from the file name: the
    /// stem is the caller's key, and a caller that already has it does not
    /// need this. A caller that has only the MANIFEST - which is the case
    /// when it walked the directory - needs the UID the document itself
    /// declares, and a file whose name and content disagreed would otherwise
    /// be grouped under the wrong study with nothing to notice it.
    pub fn study_instance_uid(&self) -> Option<&str> {
        for (t, v) in &self.study.attrs {
            if *t == crate::query::tag::STUDY_INSTANCE_UID
                && let crate::manifest::AttrValue::Text(s) = v
            {
                return Some(s.as_str());
            }
        }
        None
    }

    /// Every SOP Instance UID the manifest lists, series by series.
    ///
    /// This is the COMPLETE stored set for the study, which is what the
    /// Storage Commitment coverage rule compares a committed set against: a
    /// study is ready only once every instance the archive holds has been
    /// committed, and the archive cannot know about instances it has not
    /// received. An instance with no (0008,0018) text value is skipped - it
    /// could not be referenced by UID anyway.
    #[must_use]
    pub fn sop_instance_uids(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for series in &self.series {
            for instance in &series.instances {
                for (t, v) in &instance.attrs {
                    if *t == crate::query::tag::SOP_INSTANCE_UID
                        && let crate::manifest::AttrValue::Text(s) = v
                    {
                        out.push(s.as_str());
                        break;
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::record::tests::sample_manifest;
    use crate::manifest::write::write_manifest;

    #[test]
    fn reads_manifest_back_exactly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        let manifest = sample_manifest();
        write_manifest(&manifest_path, &manifest).expect("write");

        let decoded = read_manifest(&manifest_path).expect("read");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn read_missing_file_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.manifest");
        assert!(matches!(
            read_manifest(&path),
            Err(ManifestReadError::Io(_))
        ));
    }

    #[test]
    fn read_garbage_bytes_errors() {
        let bytes = b"this is not cbor at all, definitely garbage bytes here";
        assert!(matches!(
            read_manifest_at(bytes),
            Err(ManifestReadError::Decode(_))
        ));
    }

    #[test]
    fn find_instance_by_sop_uid() {
        let manifest = sample_manifest();
        // The sample carries SOP (0008,0018) = "1.2.840.1.3".
        let inst = manifest
            .find_instance("1.2.840.1.3")
            .expect("instance present");
        assert_eq!(inst.transfer_syntax, "1.2.840.10008.1.2.1");
        assert_eq!(inst.storage_tier, crate::manifest::TIER_HOT);
        assert_eq!(inst.file_size, 2_000_000);
        // The WADO-facing CAS address round-trips.
        assert_eq!(inst.blake3[0], 0xAB);
        assert_eq!(inst.blake3[31], 0xEF);
    }

    #[test]
    fn find_instance_missing_returns_none() {
        let manifest = sample_manifest();
        assert!(manifest.find_instance("9.9.9.9").is_none());
        assert!(manifest.find_instance("").is_none());
    }

    // P2-C: zero-instance study - WADO 404 contract, B-tree tier.
    //
    // A study whose manifest carries NO series/instances is representable at
    // the data layer (STOW is INFEASIBLE for it - STOW always creates >=1
    // instance and the B-tree only materialises a study from its instances),
    // but it yields no instances, so the WADO-RS study retriever can locate no
    // CAS object and must answer 404. This pins that contract at the layer the
    // state lives (the B-tree manifest), replacing the SQLite-era unit.
    #[test]
    fn empty_study_manifest_yields_no_instances_for_wado_404() {
        use crate::manifest::record::tests::sample_manifest;
        use crate::manifest::write::write_manifest;

        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_path = dir.path().join("study.manifest");

        // A study with a well-formed header/study attrs but ZERO series -> zero
        // instances. Preserve the StudyInstanceUID so the study node exists in
        // the B-tree state, mirroring "study is findable by UID".
        let mut manifest = sample_manifest();
        manifest.series.clear();
        manifest.study.instance_count = 0;
        manifest.study.series_count = 0;
        // No modality remains: an empty study has no series to derive one from.
        manifest.study.modalities_in_study = None;

        write_manifest(&manifest_path, &manifest).expect("write empty study manifest");
        let decoded = read_manifest(&manifest_path).expect("read empty study manifest");

        // The empty study round-trips its state exactly (findable by UID later).
        assert_eq!(decoded, manifest);
        // ...but exposes zero instances / series to the WADO study retriever,
        // which therefore locates no CAS object -> the WADO handler 404s.
        assert_eq!(
            decoded
                .series
                .iter()
                .map(|s| s.instances.len())
                .sum::<usize>(),
            0
        );
        assert_eq!(decoded.study.instance_count, 0);
        assert_eq!(decoded.study.series_count, 0);
        // The instance lookup (used when serving a specific instance/frames)
        // finds nothing.
        assert!(decoded.find_instance("1.2.840.1.3").is_none());
        for inst in decoded.series.iter().flat_map(|s| s.instances.iter()) {
            // Unreachable: an empty study has no instances to yield.
            let _ = inst;
        }
    }
}
