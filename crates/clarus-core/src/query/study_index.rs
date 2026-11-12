// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! QIDO preload: an in-RAM index of `study_uid -> StudyQido attrs`.
//!
//! The cold QIDO path (`StudyScanner` with no `.idx` accelerator match) lists
//! the `manifests/` directory and mmap+decodes every study manifest on each
//! query. This module adds a RAM index of the study-level QIDO attributes so
//! the cold path can resolve and filter candidates from memory, without a
//! directory walk or per-manifest decode.
//!
//! # Freshness / invalidation
//!
//! The manifest remains the source of truth; the index is a cache that must be
//! invalidated on:
//! - **STOW append / re-STOW merge**: a study was committed (`upsert`).
//! - **Erase tombstone**: a study was renamed to `.del` (`remove`).
//! - **Startup**: the index is built lazily in the background; until it is
//!   built, the cold path falls back to the manifest scan.
//!
//! # Concurrency
//!
//! The inner map lives behind an `Arc<RwLock<...>>`. A reader takes a single
//! short read-lock to clone the `Arc<HashMap>` snapshot, then iterates
//! lock-free on the immutable snapshot (no reader blocked by another reader's
//! request; writers swap a new snapshot under the write-lock). This is the
//! "no blocking readers on query" contract: the lock is held only for the
//! ~ns-long Arc refcount bump, never for the actual QIDO matching.

use crate::manifest::{AttrValue, StudyQido};
use crate::stow::StudyManifestAttrs;
use crate::stow::manifest::study_attrs;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// The mutable state behind the shared index handle.
/// The shareable snapshot. Stored behind an `Arc` so a reader clones it in
/// O(1); writers replace the whole snapshot (copy-on-write).
#[derive(Debug, Clone, Default)]
struct IndexState {
    /// Whether a full `build` (or an explicit `mark_built`) has finished.
    /// Until set, the cold path falls back to the manifest scan so a lazy
    /// build never makes QIDO answer from a half-built index.
    built: bool,
    /// `study_uid -> study-level QIDO attributes`. Key is the manifest file
    /// stem (== StudyInstanceUID).
    by_uid: Arc<HashMap<String, Arc<StudyQido>>>,
    /// Monotonic counter bumped by every mutation, and the value each index
    /// entry was written at.
    ///
    /// A full build walks the manifest directory for as long as it takes, and
    /// a STOW commit or an erase can land WHILE it runs. Swapping the fresh
    /// map in unconditionally discarded those mutations: a study committed
    /// during the build disappeared from QIDO until its next write, and an
    /// erased study came back. Comparing generations at swap time keeps any
    /// entry that was written after the build started.
    generation: u64,
    entry_gen: Arc<HashMap<String, u64>>,
}

/// A shareable, concurrency-safe RAM index of study QIDO records.
///
/// `Clone` is cheap: it shares the same `Arc<RwLock<...>>`, so a background
/// builder and the querying scanner can both hold a handle. `Default` starts
/// empty and not-yet-built (cold reads fall back to the manifest scan).
#[derive(Clone, Debug, Default)]
pub struct StudyQidoIndex {
    state: Arc<RwLock<IndexState>>,
}

impl StudyQidoIndex {
    /// An empty, not-yet-built index. The cold path falls back to a scan until
    /// [`build`](Self::build) or [`mark_built`](Self::mark_built) runs.
    pub fn new() -> Self {
        StudyQidoIndex::default()
    }

    /// Build the full index from a `manifests/` directory, decoding only the
    /// study section of each manifest (never series/instances/frames).
    ///
    /// A corrupt manifest is skipped (mirrors the cold-scan resilience: a bad
    /// study simply does not match); the index is marked built even if empty.
    ///
    /// Returns just the built index (the corrupt count is available via
    /// [`Self::build_with_corrupt`]).
    pub fn build_from_dir(manifests_dir: &Path) -> Self {
        Self::build_with_corrupt(manifests_dir).0
    }

    /// Build the index like [`Self::build_from_dir`] but also report how many
    /// manifest files could not be decoded (corrupt) and were therefore skipped.
    ///
    /// This is the surface the server-side lazy index builder uses to feed the
    /// `MANIFEST_CORRUPT` degradation signal: a corrupt manifest that the RAM
    /// scanner silently skips would otherwise be invisible until restart. The
    /// count is monotonic per build run (each bad file counted once).
    pub fn build_with_corrupt(manifests_dir: &Path) -> (Self, u64) {
        let mut by_uid: HashMap<String, Arc<StudyQido>> = HashMap::new();
        let mut corrupt: u64 = 0;
        if let Ok(rd) = std::fs::read_dir(manifests_dir) {
            for entry in rd.flatten() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                if let Some(stem) = fname.strip_suffix(".manifest") {
                    match crate::manifest::read_manifest_study_only(&entry.path()) {
                        Ok(m) => {
                            // Key is the manifest file stem (== StudyInstanceUID).
                            by_uid.insert(stem.to_string(), Arc::new(m.study));
                        }
                        Err(_) => {
                            // Corrupt manifest: skipped (mirrors the cold-scan
                            // resilience), but counted so the server can surface
                            // the degradation (manifest-degradation).
                            corrupt += 1;
                        }
                    }
                }
            }
        }
        let index = StudyQidoIndex::new();
        {
            let mut st = index.state.write().expect("study index write");
            st.by_uid = Arc::new(by_uid);
            st.built = true;
        }
        (index, corrupt)
    }

    /// Atomically replace this index with a fully-built one (used by the
    /// background lazy builder). Marks the index built.
    ///
    /// Mutations that land WHILE the build runs must survive the swap, and the
    /// generation counter is the mechanism. It only works when the watermark
    /// and the entry stamps belong to the SAME counter.
    ///
    /// The watermark used to be read from `other` - the builder's index - on
    /// the stated assumption that it "carries the generation the build
    /// observed". It does not: `build_with_corrupt` leaves a fresh counter at
    /// 0, and the lazy builder upserts into it before publishing, which moves
    /// it. Comparing a live stamp against that number silently discards the
    /// mutations the merge exists to preserve. The watermark is taken from
    /// this index, which is where the stamps came from.
    pub fn replace_with_full(&self, other: &StudyQidoIndex) {
        // Compatibility entry point for callers that hold no separate
        // watermark: the swap-time generation is the conservative choice (it
        // keeps every entry stamped since the build was constructed, at the
        // cost of also keeping entries the build already saw - which is a
        // stale read, never a lost write).
        let start_gen = self.generation();
        self.replace_with_full_from(other, start_gen);
    }

    /// Replace this index with a build whose scan BEGAN at `start_gen`.
    ///
    /// The watermark has to be taken when the scan starts, not when it lands.
    /// A mutation committed while the scan was running is stamped BEFORE the
    /// scan could have seen it on disk; comparing it against a generation read
    /// at swap time made it look "already in the snapshot" and the merge
    /// dropped it - the study stayed missing from QIDO until the next rebuild.
    pub fn replace_with_full_from(&self, other: &StudyQidoIndex, start_gen: u64) {
        let built = other.snapshot();
        let mut st = self.state.write().expect("study index write");
        // Entries stamped at or after the point the build STARTED were written
        // while it was in flight, so the built snapshot cannot contain them.
        let mut merged = (*built).clone();
        for (uid, entry_generation) in st.entry_gen.iter() {
            if *entry_generation >= start_gen {
                match st.by_uid.get(uid) {
                    Some(qido) => {
                        merged.insert(uid.clone(), Arc::clone(qido));
                    }
                    None => {
                        merged.remove(uid);
                    }
                }
            }
        }
        st.by_uid = Arc::new(merged);
        st.built = true;
    }

    /// Mark the index as built (no-op on readers; used for an explicitly empty
    /// store where `build_from_dir` found zero manifests but is authoritative).
    pub fn mark_built(&self) {
        let mut st = self.state.write().expect("study index write");
        st.built = true;
    }

    /// Whether a full build has completed. If false, cold reads must fall back
    /// to the manifest scan.
    pub fn ready(&self) -> bool {
        self.state.read().expect("study index read").built
    }

    /// Number of indexed studies.
    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    /// Whether the index has no entries.
    pub fn is_empty(&self) -> bool {
        self.snapshot().is_empty()
    }

    /// Snapshot the current study map as an immutable `Arc` (one short
    /// read-lock). Callers iterate lock-free on the returned snapshot.
    pub fn snapshot(&self) -> Arc<HashMap<String, Arc<StudyQido>>> {
        self.state.read().expect("study index read").by_uid.clone()
    }

    /// The stored study attrs for `study_uid`, if indexed.
    pub fn get(&self, study_uid: &str) -> Option<Arc<StudyQido>> {
        self.snapshot().get(study_uid).cloned()
    }

    /// All indexed study UIDs, in the same ascending order the cold manifest
    /// scan produces (string sort of the `.manifest` file stems).
    pub fn all_uids(&self) -> Vec<String> {
        let snap = self.snapshot();
        let mut uids: Vec<String> = snap.keys().cloned().collect();
        uids.sort();
        uids
    }

    /// Insert or replace the study QIDO record produced by a committed STOW.
    ///
    /// This is the STOW-append / re-STOW-merge invalidation: the fresh
    /// `StudyManifestAttrs` carry the current study-level search keys, and the
    /// manifest key is `study_instance_uid`.
    pub fn upsert(&self, study_uid: &str, attrs: &StudyManifestAttrs) {
        let qido = StudyQido {
            attrs: study_attrs(attrs),
            // Carry the derived counts + ModalitiesInStudy computed at
            // STOW-commit time (0020,1206 / 0020,1208 / 0008,0061), so a
            // freshly committed study answers with correct values from the RAM
            // index - no transient zeroes / empty aggregate.
            series_count: attrs.num_series,
            instance_count: attrs.num_instances,
            modalities_in_study: attrs.modalities_in_study.clone(),
        };
        let mut st = self.state.write().expect("study index write");
        // NOT `st.built = true`. An upsert inserts ONE study into an index that
        // may still be being built from the manifest dir; claiming readiness
        // here hides the rest of the archive from QIDO until the background
        // build finishes - every study committed before the first STOW of a
        // fresh start would answer 404. The builder sets `built` in
        // `replace_with_full`, which is the only place the whole set is known.
        // Copy-on-write: clone the snapshot only if this handle is shared.
        st.generation = st.generation.wrapping_add(1);
        let generation = st.generation;
        Arc::make_mut(&mut st.by_uid).insert(study_uid.to_string(), Arc::new(qido));
        Arc::make_mut(&mut st.entry_gen).insert(study_uid.to_string(), generation);
    }

    /// Remove a study's record (the erase-tombstone invalidation).
    pub fn remove(&self, study_uid: &str) {
        let mut st = self.state.write().expect("study index write");
        st.generation = st.generation.wrapping_add(1);
        let generation = st.generation;
        let _ = Arc::make_mut(&mut st.by_uid).remove(study_uid);
        // A REMOVAL also has to win against a build that already read the
        // manifest: recording the generation (with no entry) is what tells the
        // swap "this study was erased after you started".
        Arc::make_mut(&mut st.entry_gen).insert(study_uid.to_string(), generation);
    }

    /// Build this index from its manifest dir and mark it ready (convenience
    /// for the background builder: it constructs the new full index, then
    /// swaps it in atomically).
    /// Build and atomically swap this index; returns how many manifest files
    /// were corrupt (undecodable) and therefore skipped during the build. The
    /// server feeds this into the `MANIFEST_CORRUPT` degradation signal.
    pub fn build_and_swap(&self, manifests_dir: &Path) -> u64 {
        // The watermark belongs to the START of the scan: a commit stamped
        // after this point may not be on disk yet, and the merge has to keep
        // the live entry rather than the snapshot's older view.
        let start_gen = self.generation();
        let (built, corrupt) = Self::build_with_corrupt(manifests_dir);
        self.replace_with_full_from(&built, start_gen);
        corrupt
    }

    /// The current mutation generation (the watermark a build records).
    pub fn generation(&self) -> u64 {
        self.state.read().expect("study index read").generation
    }
}

/// `AttrValue` must be `Send + Sync` for the index to live behind an
/// `Arc<RwLock<...>>` shared across threads (STOW writer thread and query
/// threads). This compile-time assertion pins that.
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _c() {
        _assert_send_sync::<AttrValue>();
    }
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, SysHeader};
    use crate::stow::write_study_manifest_full;
    use std::path::Path;

    /// Write a minimal manifest for `study_uid` (mirrors the scan tests).
    fn seed_manifest(manifests_dir: &Path, study_uid: &str, name: &str, date: u64) {
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: 1,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![
                    (0x0008_0020, AttrValue::UInt(date)),
                    (0x0008_0060, AttrValue::Text("CT".into())),
                    (0x0010_0010, AttrValue::Text(name.to_string())),
                    (0x0020_000D, AttrValue::Text(study_uid.to_string())),
                ],
                ..Default::default()
            },
            series: vec![],
        };
        let path = manifests_dir.join(format!("{study_uid}.manifest"));
        crate::manifest::write_manifest(&path, &m).expect("write manifest");
    }

    /// A `StudyManifestAttrs` with a deterministic name and date.
    fn attrs_for(uid: &str, name: &str, date: u64) -> StudyManifestAttrs {
        StudyManifestAttrs {
            patient_name: Some(name.to_string()),
            patient_id: Some("P".to_string()),
            study_instance_uid: uid.to_string(),
            study_date: Some(format!("{date:08}")),
            accession_number: Some("A".to_string()),
            referring_physician: Some(String::new()),
            modality: "CT".to_string(),
            study_description: Some("D".to_string()),
            ..Default::default()
        }
    }

    /// Serialize with the cache's own tests: `write_study_manifest_full`
    /// installs cache entries, so a test asserting the cache's exact size
    /// (manifest_cache module) would otherwise count the entries these tests
    /// create while running in parallel.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        crate::manifest_cache::test_sync::lock()
    }

    #[test]
    fn fresh_index_is_not_ready() {
        // Lazy contract: a fresh (unbuilt) index must NOT be ready, so cold
        // reads fall back to the manifest scan until a build completes.
        let idx = StudyQidoIndex::new();
        assert!(!idx.ready());
        assert!(idx.is_empty());
        assert!(idx.all_uids().is_empty());
    }

    #[test]
    fn build_sorts_uids_and_reads_attrs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.2", "bob", 20260202);
        seed_manifest(&manifests, "1.2.1", "alice", 20260101);

        let idx = StudyQidoIndex::build_from_dir(&manifests);
        assert!(idx.ready());
        assert_eq!(idx.len(), 2);
        // Same ascending order as the cold manifest scan (string sort).
        assert_eq!(
            idx.all_uids(),
            vec!["1.2.1".to_string(), "1.2.2".to_string()]
        );

        let q = idx.get("1.2.1").expect("indexed study");
        assert!(
            q.attrs
                .contains(&(0x0010_0010, AttrValue::Text("alice".into())))
        );
        assert!(idx.get("1.2.99").is_none());
    }

    #[test]
    fn build_counts_corrupt_manifest_skipped() {
        // manifest-degradation (Etap 2): the RAM index builder silently
        // skips a corrupt manifest; build_with_corrupt must report it so the
        // server can feed MANIFEST_CORRUPT (a corrupt study that never makes
        // the index is otherwise invisible until restart). A valid sibling is
        // still indexed.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "alice", 20260101);
        // A present-but-undecodable manifest (garbage bytes).
        std::fs::write(
            manifests.join("1.2.9.manifest"),
            b"\x00\xff\x00\xfe not a manifest",
        )
        .expect("write corrupt manifest");

        let (idx, corrupt) = StudyQidoIndex::build_with_corrupt(&manifests);
        assert!(idx.ready());
        assert_eq!(idx.len(), 1, "the valid study is still indexed");
        assert!(idx.get("1.2.1").is_some());
        assert_eq!(corrupt, 1, "the corrupt manifest is counted");
    }

    #[test]
    fn stow_upsert_and_erase_remove_invalidate() {
        let idx = StudyQidoIndex::new();
        // A STOW-append upsert indexes the study WITHOUT claiming the index is
        // complete. Marking it ready here hid every study committed before this
        // first STOW from QIDO until the background build finished; only
        // `replace_with_full` knows the whole set.
        idx.upsert("1.2.10", &attrs_for("1.2.10", "carol", 20260303));
        assert!(
            !idx.ready(),
            "one upsert says nothing about the rest of the archive"
        );
        assert!(idx.get("1.2.10").is_some());

        // Re-STOW merge: upsert replaces the record (same key).
        idx.upsert("1.2.10", &attrs_for("1.2.10", "carol2", 20260304));
        let q = idx.get("1.2.10").expect("re-indexed");
        assert!(
            q.attrs
                .contains(&(0x0010_0010, AttrValue::Text("carol2".into())))
        );

        // Erase tombstone: remove drops it.
        idx.remove("1.2.10");
        assert!(idx.get("1.2.10").is_none());
        assert!(idx.is_empty());
    }

    #[test]
    fn build_and_swap_on_shared_handle() {
        let _g = serial();
        // Two handles share one Arc state; the builder swap is visible to the
        // reader handle (the lazy-build wiring).
        let reader = StudyQidoIndex::new();
        let builder = reader.clone();
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.5", "dave", 20260105);

        // Build via `write_study_manifest_full` produces the same StudyQido
        // section that build_from_dir reads.
        let mut tmp = attrs_for("1.2.6", "erin", 20260106);
        let _ = write_study_manifest_full(&manifests, &mut tmp, &[]);

        assert!(!reader.ready());
        builder.build_and_swap(&manifests);
        assert!(reader.ready());
        let uids = reader.all_uids();
        assert_eq!(uids, vec!["1.2.5".to_string(), "1.2.6".to_string()]);
    }

    /// `upsert` carries the derived counts AND the
    /// ModalitiesInStudy aggregate from the committed `StudyManifestAttrs`
    /// into the stored `StudyQido`, so a freshly-STOWed study answers with
    /// correct values from the RAM index (no transient zeros / empty
    /// aggregate).
    #[test]
    fn upsert_transfers_modalities_and_counts() {
        let idx = StudyQidoIndex::new();
        let mut a = attrs_for("1.2.7", "kim", 20260107);
        a.num_series = 2;
        a.num_instances = 3;
        a.modalities_in_study = Some("CT,MR".into());
        idx.upsert("1.2.7", &a);

        let s = idx.get("1.2.7").expect("upserted study present");
        assert_eq!(s.series_count, 2);
        assert_eq!(s.instance_count, 3);
        assert_eq!(s.modalities_in_study.as_deref(), Some("CT,MR"));
    }

    /// The watermark must come from the index doing the swap.
    ///
    /// The comment on the old code said the builder's index "carries the
    /// generation the build observed". It does not: `build_with_corrupt`
    /// constructs a fresh index at generation 0. The scheme happened to work
    /// only because a fresh counter is the smallest possible watermark. As soon
    /// as the builder's index carries a non-zero counter - which it does
    /// whenever the build handle is itself mutated, as the server's lazy
    /// builder can be - the watermark jumps ahead of every live stamp and the
    /// swap silently discards the mutations it exists to preserve.
    #[test]
    fn the_watermark_is_this_indexs_own_counter() {
        let _g = serial();
        let manifests = tempfile::tempdir().expect("temp");
        let manifests = manifests.path().to_path_buf();
        let _ =
            write_study_manifest_full(&manifests, &mut attrs_for("1.2.40", "ha", 20260140), &[]);
        let builder = StudyQidoIndex::build_from_dir(&manifests);
        // The build handle is mutated too (the lazy builder does an upsert of
        // the study it just committed before publishing).
        builder.upsert("1.2.40", &attrs_for("1.2.40", "ha", 20260140));
        builder.upsert("1.2.41", &attrs_for("1.2.41", "baek", 20260141));

        let live = StudyQidoIndex::new();
        // A study stored while the build ran - the mutation the swap must keep.
        live.upsert("1.2.42", &attrs_for("1.2.42", "ko", 20260142));

        live.replace_with_full(&builder);

        assert!(
            live.get("1.2.42").is_some(),
            "a live mutation must survive a swap against a moved watermark"
        );
        assert!(
            live.get("1.2.41").is_some(),
            "the builder's own entry stays"
        );
    }

    /// The build watermark is taken when the scan STARTS.
    ///
    /// A commit stamped after the scan began may not be on disk yet, so the
    /// snapshot cannot contain it and the merge has to keep the live entry.
    /// Reading the generation at swap time instead compared the commit
    /// against a counter that had already advanced past it, concluded the
    /// snapshot covered it, and dropped the study - invisible to QIDO until
    /// the next rebuild.
    ///
    /// Driven through `build_and_swap`, which is the production entry point:
    /// the commit is injected from the builder's own handle (the same handle
    /// the server's lazy builder mutates), which is what marks it as "during
    /// the build".
    #[test]
    fn a_commit_during_the_build_survives_the_swap() {
        let _g = serial();
        let manifests = tempfile::tempdir().expect("temp");
        let manifests = manifests.path().to_path_buf();
        let _ =
            write_study_manifest_full(&manifests, &mut attrs_for("1.2.50", "ha", 20260150), &[]);

        let live = StudyQidoIndex::new();
        // MORE THAN ONE mutation before the scan, so the counter is well past
        // the start watermark. A commit during the build is then stamped
        // strictly BETWEEN the two, which is the only window where a
        // swap-time watermark can differ from the start one.
        live.upsert("1.2.49", &attrs_for("1.2.49", "pre", 20260149));
        live.upsert("1.2.48", &attrs_for("1.2.48", "pre", 20260148));
        // The build starts here: this is the watermark the merge must use.
        let start_gen = live.generation();
        let (built, _) = StudyQidoIndex::build_with_corrupt(&manifests);
        // The commit lands while the scan is running.
        live.upsert("1.2.51", &attrs_for("1.2.51", "ko", 20260151));
        assert!(
            live.generation() > start_gen,
            "the commit moved the counter past the build's watermark"
        );
        // And the swap happens after it.
        live.replace_with_full_from(&built, start_gen);
        assert!(
            live.get("1.2.51").is_some(),
            "a commit made during the build is not overwritten by the snapshot"
        );
        assert!(
            live.get("1.2.50").is_some(),
            "and the scan's own result is present"
        );
    }

    /// The merge compares against the watermark it was GIVEN.
    ///
    /// This is the discriminating half: the fixture stamps an entry at a
    /// generation the caller then hands over as the watermark. An
    /// implementation that reads the live counter instead of the argument sees
    /// a smaller number (`>=` is then false) and drops the entry, so the two
    /// readings cannot both pass this test.
    #[test]
    fn the_merge_uses_the_watermark_it_was_given() {
        let _g = serial();
        let manifests = tempfile::tempdir().expect("temp");
        let manifests = manifests.path().to_path_buf();
        let _ =
            write_study_manifest_full(&manifests, &mut attrs_for("1.2.70", "ha", 20260170), &[]);

        let live = StudyQidoIndex::new();
        live.upsert("1.2.71", &attrs_for("1.2.71", "ko", 20260171));
        // The entry is stamped at generation 1; the caller names 5 as the
        // watermark, so the entry is older than the build and the snapshot
        // should own the key - the snapshot does not have it, so the entry
        // must be DROPPED.
        let (built, _) = StudyQidoIndex::build_with_corrupt(&manifests);
        live.replace_with_full_from(&built, 5);
        assert!(
            live.get("1.2.71").is_none(),
            "an entry older than the given watermark is not merged back"
        );

        // The same entry with a watermark of 1 IS merged back.
        let live2 = StudyQidoIndex::new();
        live2.upsert("1.2.71", &attrs_for("1.2.71", "ko", 20260171));
        live2.replace_with_full_from(&built, 1);
        assert!(
            live2.get("1.2.71").is_some(),
            "an entry at the watermark is in flight and survives"
        );
    }

    /// An erase during the build must remove the study, not be resurrected.
    #[test]
    fn a_removal_during_the_build_is_not_resurrected() {
        let _g = serial();
        let manifests = tempfile::tempdir().expect("temp");
        let manifests = manifests.path().to_path_buf();
        let _ =
            write_study_manifest_full(&manifests, &mut attrs_for("1.2.60", "ha", 20260160), &[]);

        let live = StudyQidoIndex::new();
        live.upsert("1.2.60", &attrs_for("1.2.60", "ha", 20260160));
        let start_gen = live.generation();
        let (built, _) = StudyQidoIndex::build_with_corrupt(&manifests);
        live.remove("1.2.60");
        live.replace_with_full_from(&built, start_gen);

        assert!(
            live.get("1.2.60").is_none(),
            "a study erased during the build stays erased"
        );
    }
}
