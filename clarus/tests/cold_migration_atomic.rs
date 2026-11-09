// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Patient-atomic cold migration (0210).
//!
//! OWNER INVARIANT 12.09: a patient is never half-migrated. These tests drive
//! the real two-phase protocol (COPY -> VERIFY -> FLIP -> DELETE) against a
//! real `cold_path` in a temp directory, and the mandatory case is the BREAK
//! IN THE MIDDLE OF A PATIENT: hot must be untouched, no tier flipped, no
//! partial cold left behind.
//!
//! The tier flip and the hot delete are asserted on the MANIFEST and on the
//! CAS directories - the two things a reader depends on - not on internal
//! counters, so a change that keeps the counters right and the data wrong
//! cannot pass.

#![forbid(unsafe_code)]

use clarus_server::config::Config;
use clarus_server::state::AppState;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// Build an AppState with a real cold CAS beside a real hot CAS.
///
/// `cold` is `Option` so the "no cold configured" path can be exercised too.
fn app_state_with_cold(root: &Path, cold: bool) -> AppState {
    let btree_root = root.join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
    std::fs::create_dir_all(&idx_dir).expect("idx dir");
    let storage_dir = root.join("cas");
    std::fs::create_dir_all(&storage_dir).expect("hot cas dir");

    let mut config = Config::test_default();
    config.storage_dir = storage_dir.to_string_lossy().to_string();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;

    let cold_cas = if cold {
        let cold_dir = root.join("cold");
        std::fs::create_dir_all(&cold_dir).expect("cold dir");
        Some(
            clarus_adapters::storage::ContentAddressedStorage::new(cold_dir)
                .expect("open cold cas"),
        )
    } else {
        None
    };

    AppState {
        cas: clarus_adapters::storage::ContentAddressedStorage::new(storage_dir).expect("open cas"),
        codec_registry: clarus_adapters::codec::CodecRegistry::default(),
        ups_events: clarus_server::sse_fanout::DualEventSink::new(),
        start_time: Instant::now(),
        metrics_ring: Arc::new(std::sync::RwLock::new(std::collections::VecDeque::new())),
        log_buffer: Arc::new(std::sync::RwLock::new(std::collections::VecDeque::new())),
        maintenance: Arc::new(std::sync::RwLock::new(
            clarus_server::state::MaintenanceState::default(),
        )),
        #[cfg(feature = "s3")]
        s3: Arc::new(std::sync::OnceLock::new()),
        cold_cas,
        shadow: clarus_server::shadow::ShadowTier::new(idx_dir.clone()),
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
        config,
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(2),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    }
}

// -- Fixtures ------------------------------------------------------------

/// One blob on disk in the hot CAS, with its manifest instance record.
///
/// Returns `(blake3_hex, file_size)`. The content is deterministic per blob so
/// a size mismatch or a wrong copy is detectable.
fn seed_blob(
    state: &AppState,
    study: &str,
    patient: &str,
    marker: u8,
    content_len: usize,
) -> (String, i64) {
    let content = vec![marker; content_len];
    let hash = *blake3::hash(&content).as_bytes();
    let hex = clarus_adapters::hex_encode(&hash);

    // Write the hot copy at its CAS path.
    let path = state.cas.cas_path(&hex);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, &content).expect("write hot blob");

    let mut attrs = clarus_core::stow::StudyManifestAttrs {
        patient_id: Some(patient.to_string()),
        study_instance_uid: study.to_string(),
        modality: "CT".into(),
        study_date: Some("20200101".into()), // older than any sane cutoff
        ..Default::default()
    };
    let instance = clarus_core::stow::InstanceModel {
        sop_instance_uid: format!("{study}.{hex}"),
        blake3: hash,
        storage_tier: clarus_core::manifest::TIER_HOT,
        transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
        file_size: content_len as u64,
        ..Default::default()
    };
    let series = [clarus_core::stow::SeriesModel {
        series_instance_uid: format!("{study}.s1"),
        modality: "CT".into(),
        instances: vec![instance],
        ..Default::default()
    }];
    clarus_core::stow::write_study_manifest_full(
        Path::new(&state.config.btree.manifest_dir),
        &mut attrs,
        &series,
    )
    .expect("write manifest");

    (hex, content_len as i64)
}

/// The tier of the instance holding `hex`, read back from the manifests.
fn tier_of(state: &AppState, hex: &str) -> Option<u8> {
    for entry in std::fs::read_dir(&state.config.btree.manifest_dir).ok()? {
        let path = entry.ok()?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
            continue;
        }
        let manifest = clarus_core::manifest::read_manifest(&path).ok()?;
        for inst in manifest.series.iter().flat_map(|s| &s.instances) {
            if clarus_adapters::hex_encode(&inst.blake3) == hex {
                return Some(inst.storage_tier);
            }
        }
    }
    None
}

/// Whether a cold copy of `hex` exists.
fn cold_exists(state: &AppState, hex: &str) -> bool {
    state
        .cold_cas
        .as_ref()
        .is_some_and(|cold| cold.cas_path(hex).exists())
}

/// Whether the hot copy of `hex` still exists.
fn hot_exists(state: &AppState, hex: &str) -> bool {
    state.cas.cas_path(hex).exists()
}

const CUTOFF: &str = "29991231"; // every fixture study is older than this

// -- Grouping (T1) -------------------------------------------------------

/// A batch is ONE patient: all of that patient's HOT blobs, and nothing else.
#[test]
fn grouping_returns_the_whole_patient_and_excludes_a_foreign_study() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = app_state_with_cold(root.path(), true);

    let (a1, _) = seed_blob(&state, "1.2.3.1", "PAT-A", 0x11, 128);
    let (a2, _) = seed_blob(&state, "1.2.3.2", "PAT-A", 0x22, 256);
    let (b1, _) = seed_blob(&state, "1.2.3.3", "PAT-B", 0x33, 512);

    // READY instead of HOT: must never be collected.
    let (cold_already, _) = seed_blob(&state, "1.2.3.4", "PAT-A", 0x44, 64);
    // Flip it by hand so it is no longer HOT.
    force_tier(&state, &cold_already, clarus_core::manifest::TIER_COLD);

    let batches =
        clarus_server::migration_worker::collect_migration_patients_for_test(&state, CUTOFF, 10);

    let pat_a = batches
        .iter()
        .find(|(p, _)| p == "PAT-A")
        .expect("PAT-A batch present");
    let mut got: Vec<&str> = pat_a.1.iter().map(|(b, _, _)| b.as_str()).collect();
    got.sort_unstable();
    let mut want = vec![a1.as_str(), a2.as_str()];
    want.sort_unstable();
    assert_eq!(got, want, "PAT-A batch is exactly its two HOT blobs");

    let pat_b = batches
        .iter()
        .find(|(p, _)| p == "PAT-B")
        .expect("PAT-B batch present");
    assert_eq!(pat_b.1.len(), 1);
    assert_eq!(pat_b.1[0].0, b1, "PAT-B gets its own blob, not PAT-A's");

    // The already-cold blob is not a candidate on any side.
    assert!(
        !batches.iter().any(|(_, blobs)| blobs
            .iter()
            .any(|(b, _, _)| b.as_str() == cold_already.as_str())),
        "an already-cold instance is not a migration candidate"
    );
}

/// Two studies of one patient collapse into ONE batch (patient atomicity).
#[test]
fn grouping_merges_a_patients_studies_into_one_batch() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = app_state_with_cold(root.path(), true);

    let (s1, _) = seed_blob(&state, "1.2.9.1", "PAT-MERGE", 0x55, 100);
    let (s2, _) = seed_blob(&state, "1.2.9.2", "PAT-MERGE", 0x66, 200);

    let batches =
        clarus_server::migration_worker::collect_migration_patients_for_test(&state, CUTOFF, 10);
    assert_eq!(batches.len(), 1, "one patient -> one batch");
    assert_eq!(batches[0].0, "PAT-MERGE");
    let mut got: Vec<&str> = batches[0].1.iter().map(|(b, _, _)| b.as_str()).collect();
    got.sort_unstable();
    let mut want = vec![s1.as_str(), s2.as_str()];
    want.sort_unstable();
    assert_eq!(got, want, "both studies are in the same batch");
}

/// The same manifests always produce the same batch order (resumability).
#[test]
fn grouping_is_deterministic() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = app_state_with_cold(root.path(), true);
    for (i, marker) in [0x71u8, 0x72, 0x73].iter().enumerate() {
        seed_blob(&state, &format!("1.2.7.{i}"), "PAT-D", *marker, 64 + i);
        seed_blob(&state, &format!("1.2.8.{i}"), "PAT-E", *marker, 64 + i);
    }

    let first =
        clarus_server::migration_worker::collect_migration_patients_for_test(&state, CUTOFF, 10);
    let second =
        clarus_server::migration_worker::collect_migration_patients_for_test(&state, CUTOFF, 10);
    assert_eq!(first, second, "same manifests -> same batches, same order");
}

/// A study with no Patient ID cannot be attributed and is skipped, not guessed.
#[test]
fn grouping_skips_a_study_without_a_patient_id() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = app_state_with_cold(root.path(), true);

    let (known, _) = seed_blob(&state, "1.2.6.1", "PAT-KNOWN", 0x81, 64);
    // A study whose Patient ID is empty: unattributable.
    let mut attrs = clarus_core::stow::StudyManifestAttrs {
        patient_id: Some(String::new()),
        study_instance_uid: "1.2.6.2".into(),
        modality: "CT".into(),
        study_date: Some("20200101".into()),
        ..Default::default()
    };
    let hash = *blake3::hash(b"orphan-study").as_bytes();
    let series = [clarus_core::stow::SeriesModel {
        series_instance_uid: "1.2.6.2.s1".into(),
        modality: "CT".into(),
        instances: vec![clarus_core::stow::InstanceModel {
            sop_instance_uid: "1.2.6.2.1".into(),
            blake3: hash,
            storage_tier: clarus_core::manifest::TIER_HOT,
            transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
            file_size: 10,
            ..Default::default()
        }],
        ..Default::default()
    }];
    clarus_core::stow::write_study_manifest_full(
        Path::new(&state.config.btree.manifest_dir),
        &mut attrs,
        &series,
    )
    .expect("write unattributed manifest");

    let batches =
        clarus_server::migration_worker::collect_migration_patients_for_test(&state, CUTOFF, 10);
    assert_eq!(batches.len(), 1, "only the attributable patient is batched");
    assert_eq!(batches[0].0, "PAT-KNOWN");
    assert_eq!(batches[0].1.len(), 1);
    assert_eq!(batches[0].1[0].0, known);
}

// -- The mandatory mid-patient break (T4) --------------------------------

/// ABORT in the middle of a patient: hot untouched, tiers untouched, no
/// partial cold left behind.
///
/// The break is injected by DELETING one blob's hot file after the batch is
/// collected but before the protocol copies it, and by pointing its recorded
/// size at a value the copy cannot satisfy. Either way the COPY phase fails on
/// that blob, which is exactly the "disconnect plug mid-patient" case.
#[test]
fn abort_mid_patient_leaves_hot_and_tiers_untouched() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = app_state_with_cold(root.path(), true);

    // Three blobs of ONE patient. The middle one will be broken.
    let (b1, size1) = seed_blob(&state, "1.2.5.1", "PAT-BREAK", 0xA1, 256);
    let (b2, _size2) = seed_blob(&state, "1.2.5.2", "PAT-BREAK", 0xA2, 256);
    let (b3, _size3) = seed_blob(&state, "1.2.5.3", "PAT-BREAK", 0xA3, 256);

    // Inject the break: remove the hot file of the MIDDLE blob, so its COPY
    // fails after the first blob has already been copied to cold.
    let broken_path = state.cas.cas_path(&b2);
    std::fs::remove_file(&broken_path).expect("remove hot blob");

    // The batch is otherwise ordered by blake3, so ensure the broken blob is
    // not first: run the protocol with an explicit order that copies b1 first.
    let (migrated, skipped, abort) =
        clarus_server::migration_worker::migrate_patient_to_cold_for_test(
            &state,
            "PAT-BREAK",
            vec![
                (b1.clone(), size1, "1.2.5.1".into()),
                (b2.clone(), 256, "1.2.5.2".into()),
                (b3.clone(), 256, "1.2.5.3".into()),
            ],
        );

    assert!(
        abort.is_some(),
        "the patient must abort, not silently succeed"
    );
    assert_eq!(migrated, 0, "nothing migrated");
    assert_eq!(skipped, 0, "the surviving blobs are not 'skipped' either");

    // THE INVARIANT: every blob is still HOT, so the patient is still whole.
    for hex in [&b1, &b3] {
        assert_eq!(
            tier_of(&state, hex),
            Some(clarus_core::manifest::TIER_HOT),
            "tier of {hex} must be untouched by an aborted patient"
        );
        assert!(
            hot_exists(&state, hex),
            "hot copy of {hex} must survive an aborted patient"
        );
    }
    // The aborted patient leaves NO partial cold work behind (the abort path
    // discards the cold copies it made, including the first blob's).
    for hex in [&b1, &b3] {
        assert!(
            !cold_exists(&state, hex),
            "no partial cold copy of {hex} may survive an abort"
        );
    }
}

/// The FULL path: every blob copied, verified, flipped and its hot copy gone.
#[test]
fn full_success_migrates_the_whole_patient() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = app_state_with_cold(root.path(), true);

    let (b1, s1) = seed_blob(&state, "1.2.4.1", "PAT-OK", 0xB1, 1024);
    let (b2, s2) = seed_blob(&state, "1.2.4.2", "PAT-OK", 0xB2, 4096);

    let (migrated, _skipped, abort) =
        clarus_server::migration_worker::migrate_patient_to_cold_for_test(
            &state,
            "PAT-OK",
            vec![
                (b1.clone(), s1, "1.2.4.1".into()),
                (b2.clone(), s2, "1.2.4.2".into()),
            ],
        );
    assert!(
        abort.is_none(),
        "a healthy patient must not abort: {abort:?}"
    );
    assert_eq!(migrated, 2, "both blobs changed tier");

    for (hex, size) in [(&b1, s1), (&b2, s2)] {
        assert_eq!(
            tier_of(&state, hex),
            Some(clarus_core::manifest::TIER_COLD),
            "{hex} must be COLD"
        );
        assert!(cold_exists(&state, hex), "cold copy of {hex} must exist");
        assert!(
            !hot_exists(&state, hex),
            "hot copy of {hex} must be deleted after the whole patient flipped"
        );
        // The cold copy is byte-complete (the size the manifest records).
        let cold = state.cold_cas.as_ref().expect("cold");
        assert_eq!(
            std::fs::metadata(cold.cas_path(hex))
                .expect("stat cold")
                .len(),
            size as u64,
            "cold copy of {hex} has the recorded size"
        );
    }
}

/// A break between FLIP and DELETE: the data is readable from BOTH tiers, and
/// re-running is idempotent (`resolve_file` already probes the opposite tier).
#[test]
fn a_crash_after_flip_keeps_every_blob_readable_and_resumes() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = app_state_with_cold(root.path(), true);

    let (b1, s1) = seed_blob(&state, "1.2.2.1", "PAT-CRASH", 0xC1, 512);
    let (b2, s2) = seed_blob(&state, "1.2.2.2", "PAT-CRASH", 0xC2, 512);

    // Simulate the crash: COPY + VERIFY done by hand, then the tier flipped,
    // and the hot copies deliberately LEFT in place (DELETE never ran).
    let cold = state.cold_cas.as_ref().expect("cold").clone();
    for (hex, size) in [(&b1, s1), (&b2, s2)] {
        let hot = std::fs::read(state.cas.cas_path(hex)).expect("read hot");
        let target = cold.cas_path(hex);
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        std::fs::write(&target, &hot).expect("write cold");
        assert_eq!(hot.len(), size as usize);
        force_tier(&state, hex, clarus_core::manifest::TIER_COLD);
    }

    // Both tiers hold the bytes: a reader can open the blob whichever tier it
    // believes in. This is the "metadata half-state, never data loss" claim.
    for hex in [&b1, &b2] {
        assert!(
            hot_exists(&state, hex),
            "hot copy still there after a crash"
        );
        assert!(cold_exists(&state, hex), "cold copy present");
    }

    // Resume: the protocol must complete the patient without duplicating work.
    let (migrated, _skipped, abort) =
        clarus_server::migration_worker::migrate_patient_to_cold_for_test(
            &state,
            "PAT-CRASH",
            vec![
                (b1.clone(), s1, "1.2.2.1".into()),
                (b2.clone(), s2, "1.2.2.2".into()),
            ],
        );
    assert!(abort.is_none(), "resume must not abort: {abort:?}");
    let _ = migrated; // already-COLD instances match nothing on the flip pass

    for hex in [&b1, &b2] {
        assert_eq!(
            tier_of(&state, hex),
            Some(clarus_core::manifest::TIER_COLD),
            "{hex} stays COLD"
        );
        assert!(
            !hot_exists(&state, hex),
            "the resumed run finally deletes the hot copy of {hex}"
        );
        assert!(cold_exists(&state, hex), "and keeps the cold copy");
    }
}

/// With no cold tier configured the batch refuses rather than deleting hot.
#[test]
fn no_cold_configured_is_an_error_not_a_silent_skip() {
    let root = tempfile::tempdir().expect("tempdir");
    let state = app_state_with_cold(root.path(), false);
    let err = clarus_server::migration_worker::migrate_batch_cold_for_test(&state, 1);
    assert!(
        err.is_err(),
        "a missing cold_path must be reported, never silently treated as done"
    );
}

// -- Helpers -------------------------------------------------------------

/// Rewrite every manifest instance holding `hex` to `tier`.
fn force_tier(state: &AppState, hex: &str, tier: u8) {
    let dir = Path::new(&state.config.btree.manifest_dir);
    for entry in std::fs::read_dir(dir).expect("read manifests") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
            continue;
        }
        let Ok(mut manifest) = clarus_core::manifest::read_manifest(&path) else {
            continue;
        };
        let mut touched = false;
        for inst in manifest
            .series
            .iter_mut()
            .flat_map(|s| s.instances.iter_mut())
        {
            if clarus_adapters::hex_encode(&inst.blake3) == hex {
                inst.storage_tier = tier;
                touched = true;
            }
        }
        if touched {
            clarus_core::manifest::write_manifest(&path, &manifest).expect("rewrite manifest");
        }
    }
}
