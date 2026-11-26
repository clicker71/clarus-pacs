// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Manifest read via memory mapping.
//!
//! The whole `.manifest` file is mapped into memory and parsed directly from
//! the mapping - no buffered `read()` loop, no allocation for the byte
//! buffer. This is the read side of the plan's mmap contract (the write side
//! is [`crate::manifest::write`]).
//!
//! # Why the readers take a [`TrustedManifestDir`]
//!
//! A mapping is only sound while its inode is immutable, and neither a
//! `&Path` nor a [`PublishedManifest`] can establish that: a safe constructor
//! validates only the path shape, not how the file at that path was
//! published, so any caller (or any other thread holding a writable
//! descriptor) could truncate the file under the mapping. [`PublishedManifest`]
//! is therefore a PATH VALIDATOR, not a proof of publication - it rejects a
//! UID that could escape the directory and centralizes the path join, and
//! nothing more. The publication guarantee itself is carried by the
//! [`TrustedManifestDir`] parameter: it is built only by code that vouches
//! for the tmp-file + rename protocol (the audited server voucher in
//! `clarus/src/ffi.rs`, and test fixtures that publish their own files). The
//! mmap itself happens inside [`crate::ffi::map_manifest_dir`]. Readers of an
//! unvouched path use [`read_manifest_copy`].

use crate::ffi::TrustedManifestDir;
use crate::manifest::record::Manifest;
use std::io;
use std::path::{Path, PathBuf};

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

/// A manifest path named by the two halves the publish protocol is defined on.
///
/// This type is a PATH VALIDATOR only: it rejects a `study_uid` that could
/// escape `manifest_dir` (a separator, an absolute path, empty, or a dotfile)
/// and centralizes the path join, so callers do not hand-build the file name.
/// It does NOT prove the file was published via tmp-file + rename - any safe
/// code can construct one for an arbitrary directory and UID. The immutability
/// precondition the mmap needs is carried by the [`TrustedManifestDir`]
/// parameter the readers take, not by this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedManifest {
    dir: PathBuf,
    study_uid: String,
}

impl PublishedManifest {
    /// The manifest of `study_uid` inside `manifest_dir`.
    ///
    /// Returns `None` when `study_uid` could not name a manifest file inside
    /// `manifest_dir`: a separator or an absolute path would name a file
    /// outside the directory, the empty string would name the directory
    /// itself, and a leading `.` would name a hidden file. This validates the
    /// path shape only; it does not vouch for how the file came to exist.
    ///
    /// A DRIVE PREFIX (`C:`) is rejected as well. On Windows `Path::new("C:")`
    /// is neither absolute nor does it contain a separator, so it passes every
    /// check above - and joining it onto the directory REPLACES the base,
    /// pointing the reader at the process's current directory on drive C. A
    /// StudyInstanceUID is a dotted-decimal UID (PS3.5 Table 6.2-1, VR UI), so
    /// nothing legitimate is lost by refusing the byte pair.
    #[must_use]
    pub fn new(manifest_dir: impl Into<PathBuf>, study_uid: impl Into<String>) -> Option<Self> {
        let study_uid = study_uid.into();
        if study_uid.is_empty()
            || study_uid.contains('/')
            || study_uid.contains('\\')
            || Path::new(&study_uid).is_absolute()
            || study_uid.starts_with('.')
            || has_drive_prefix(&study_uid)
        {
            return None;
        }
        Some(Self {
            dir: manifest_dir.into(),
            study_uid,
        })
    }

    /// The directory manifests are published into.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The study UID this manifest is published under.
    #[must_use]
    pub fn study_uid(&self) -> &str {
        &self.study_uid
    }

    /// The `<study>.manifest` file name.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!("{}.manifest", self.study_uid)
    }

    /// The full path the mapping opens.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(self.file_name())
    }

    /// Recover the constructor arguments from a full manifest path.
    ///
    /// Test-only: exists solely for the two test helpers in this module that
    /// read fixtures by path. Production callers address a manifest by its
    /// directory + study UID pair via [`PublishedManifest::new`], so a normal
    /// build has no arbitrary-path mapping on the public surface.
    #[cfg(test)]
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let dir = path.parent()?;
        let file_name = path.file_name()?.to_str()?;
        let study_uid = file_name.strip_suffix(".manifest")?;
        Self::new(dir.to_path_buf(), study_uid)
    }
}

/// Whether `s` starts with a Windows drive prefix (`C:`).
///
/// Checked on every platform, not under `#[cfg(windows)]`: the same string
/// reaches a manifest path on a Windows deployment, and a platform-conditional
/// guard would leave the rule untestable on the hosts that build it. A
/// StudyInstanceUID never contains `:` (PS3.5 Table 6.2-1, VR UI, digits and
/// dots only), so the check costs a legitimate caller nothing.
fn has_drive_prefix(s: &str) -> bool {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), Some(':')) => c.is_ascii_alphabetic(),
        _ => false,
    }
}

/// Read and decode a manifest by mmap-ing the whole file.
///
/// `trusted` vouches that the directory the manifest lives in is published
/// via tmp-file + rename and is never rewritten or truncated in place while
/// the mapping is alive (the immutability invariant of
/// [`crate::ffi::map_manifest_dir`]). [`PublishedManifest`] only validates
/// the path shape, so the publication proof has to arrive through the trusted
/// directory.
pub fn read_manifest(
    trusted: &TrustedManifestDir,
    manifest: &PublishedManifest,
) -> Result<Manifest, ManifestReadError> {
    let mapped = crate::ffi::map_manifest_dir(trusted.path(), &manifest.file_name())
        .map_err(ManifestReadError::Io)?;
    read_manifest_at(&mapped)
}

/// Same as [`read_manifest`] but parses **only** the study-level QIDO
/// attributes - series, instance, and frame records are skipped via
/// [`CborReader::skip_value`]. This is the QIDO hotpath.
pub fn read_manifest_study_only(
    trusted: &TrustedManifestDir,
    manifest: &PublishedManifest,
) -> Result<Manifest, ManifestReadError> {
    let mapped = crate::ffi::map_manifest_dir(trusted.path(), &manifest.file_name())
        .map_err(ManifestReadError::Io)?;
    Manifest::decode_study_only(&mapped).map_err(|e| ManifestReadError::Decode(e.to_string()))
}

/// The crate-internal read of a manifest published by `stow/manifest.rs`.
///
/// The writer publishes `<study>.manifest.tmp` then renames it into place,
/// and never rewrites or truncates a manifest in place. That is the voucher
/// [`read_manifest`] requires, and it is a property of the crate-owned
/// manifest directory, not of the caller: every manifest in that directory was
/// published by the writer. This helper is `pub(crate)` - only this crate's
/// own code, which the server feeds the configured manifest directory, may
/// call it (the mirror of `idx/preload.rs`'s `SealedIdxRoot::from_writer`).
pub(crate) fn read_manifest_published(
    manifest: &PublishedManifest,
) -> Result<Manifest, ManifestReadError> {
    let mapped = crate::ffi::map_manifest_dir(manifest.dir(), &manifest.file_name())
        .map_err(ManifestReadError::Io)?;
    read_manifest_at(&mapped)
}

/// [`read_manifest_published`] that also reports how many bytes were decoded.
///
/// The cache charges this length to its budget, so a separate `metadata` call
/// would be a second observation of the path: it can fail (letting an
/// oversized manifest look free) or describe a different file, since the name
/// is re-resolved between the read and the stat. Measuring the mapping that
/// actually produced the buffer makes the number exact by construction.
pub(crate) fn read_manifest_published_sized(
    manifest: &PublishedManifest,
) -> Result<(Manifest, usize), ManifestReadError> {
    let mapped = crate::ffi::map_manifest_dir(manifest.dir(), &manifest.file_name())
        .map_err(ManifestReadError::Io)?;
    let len = mapped.len();
    let decoded = read_manifest_at(&mapped)?;
    Ok((decoded, len))
}

/// Shallow twin of [`read_manifest_published`] for the QIDO hotpath.
pub(crate) fn read_manifest_study_only_published(
    manifest: &PublishedManifest,
) -> Result<Manifest, ManifestReadError> {
    let mapped = crate::ffi::map_manifest_dir(manifest.dir(), &manifest.file_name())
        .map_err(ManifestReadError::Io)?;
    Manifest::decode_study_only(&mapped).map_err(|e| ManifestReadError::Decode(e.to_string()))
}

/// Read a manifest from an owned byte buffer.
///
/// The escape hatch for a caller that must read a path it cannot vouch for - a
/// test, a repair tool, a file already in memory. It copies instead of mapping,
/// which costs one allocation and is the reason it is not the default: the
/// mapped readers pay nothing, this one pays the file size.
pub fn read_manifest_copy(path: &Path) -> Result<Manifest, ManifestReadError> {
    let bytes = std::fs::read(path).map_err(ManifestReadError::Io)?;
    read_manifest_at(&bytes)
}

/// Path-shaped reader for tests: every path a test builds points at a file it
/// published itself through `write_manifest`, so unwrapping the two halves here
/// keeps the fixtures reading like the storage they replace.
#[cfg(test)]
pub(crate) fn read_manifest_for_test(path: &Path) -> Result<Manifest, ManifestReadError> {
    let published = PublishedManifest::from_path(path)
        .ok_or_else(|| ManifestReadError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?;
    let trusted = crate::ffi::trusted_manifest_dir_for_test(
        path.parent()
            .ok_or_else(|| ManifestReadError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?,
    );
    read_manifest(&trusted, &published)
}

/// Test twin of [`read_manifest_study_only`], same argument as
/// [`read_manifest_for_test`].
#[cfg(test)]
pub(crate) fn read_manifest_study_only_for_test(
    path: &Path,
) -> Result<Manifest, ManifestReadError> {
    let published = PublishedManifest::from_path(path)
        .ok_or_else(|| ManifestReadError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?;
    let trusted = crate::ffi::trusted_manifest_dir_for_test(
        path.parent()
            .ok_or_else(|| ManifestReadError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?,
    );
    read_manifest_study_only(&trusted, &published)
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

    /// (0020,000E) Series Instance UID holding `sop_instance_uid`, when found.
    ///
    /// Annex J.2's study-shaped Storage Commitment answer is a Study Sequence
    /// whose items carry a Referenced Series Sequence, so reporting an
    /// unavailable instance needs the series as well as the study. Resolving it
    /// while the manifest is already open avoids a second walk for the same
    /// instance.
    pub fn series_of_instance(&self, sop_instance_uid: &str) -> Option<&str> {
        for series in &self.series {
            let holds = series.instances.iter().any(|i| {
                i.attrs.iter().any(|(t, v)| {
                    *t == crate::query::tag::SOP_INSTANCE_UID
                        && matches!(v, crate::manifest::AttrValue::Text(s) if s == sop_instance_uid)
                })
            });
            if !holds {
                continue;
            }
            for (t, v) in &series.attrs {
                if *t == crate::query::tag::SERIES_INSTANCE_UID
                    && let crate::manifest::AttrValue::Text(s) = v
                {
                    return Some(s.as_str());
                }
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

        let decoded = read_manifest_for_test(&manifest_path).expect("read");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn read_missing_file_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.manifest");
        assert!(matches!(
            read_manifest_for_test(&path),
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
        let decoded = read_manifest_for_test(&manifest_path).expect("read empty study manifest");

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

    /// A file truncated to a torn length is a decode error through the COPY
    /// reader, never a panic or out-of-bounds access. The mmap readers are now
    /// reachable only through the directory + study UID pair, so an arbitrary
    /// mutable path can no longer be mapped - this pins the copy reader as the
    /// degradation a torn file actually produces.
    #[test]
    fn read_manifest_copy_survives_a_torn_file() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("1.2.3.manifest");

        write_manifest(&path, &sample_manifest()).expect("write");

        // Keep a writable handle open while the file is truncated: the copy
        // reader opens a fresh handle and must degrade to a decode error.
        let mut h = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open writable");
        h.set_len(0).expect("truncate");
        h.write_all(b"torn").expect("write torn");
        drop(h);

        match read_manifest_copy(&path) {
            Err(ManifestReadError::Decode(_)) => {}
            other => panic!("torn file must be a decode error, got {other:?}"),
        }

        // The trusted pair-based reader still round-trips a re-published file.
        write_manifest(&path, &sample_manifest()).expect("rewrite");
        let published = PublishedManifest::new(dir.path(), "1.2.3").expect("published");
        let trusted = crate::ffi::trusted_manifest_dir_for_test(dir.path());
        assert_eq!(
            read_manifest(&trusted, &published).expect("read"),
            sample_manifest()
        );
    }

    /// A drive-relative name must not be accepted as a study UID.
    ///
    /// On Windows `C:` is neither absolute nor shell-separated, so it passed
    /// every check the constructor used to apply - and `dir.join("C:.manifest")`
    /// is resolved against drive C's current directory, not against
    /// `manifest_dir`. The assertion is the CONSTRUCTOR's answer, so it holds
    /// on every platform rather than only where the join misbehaves.
    #[test]
    fn a_drive_prefix_is_not_a_manifest_name() {
        for uid in ["C:", "C:1.2.3", "c:", "Z:evil"] {
            assert!(
                PublishedManifest::new("manifests", uid).is_none(),
                "a drive prefix must not name a manifest: {uid}"
            );
        }
        // A colon that is not a drive prefix is still refused: VR UI admits
        // neither ':' nor any other punctuation a UID cannot carry.
        assert!(PublishedManifest::new("manifests", "1.2.3:4").is_some());
        // The ordinary dotted-decimal form keeps working.
        assert!(PublishedManifest::new("manifests", "1.2.840.10008.5.1.4").is_some());
    }
}
