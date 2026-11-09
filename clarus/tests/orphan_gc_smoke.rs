// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! 3008 orphan GC smoke (background reclaim of replaced-version blobs).
//!
//! The erase-worker's scheduled pass calls `run_orphan_gc` (pub for tests):
//! drains `*.orphans` sidecars of LIVE studies refcount-checked, removes a
//! fully-drained sidecar, keeps it on a retryable failure or while a line's
//! hash is still referenced by another live manifest. NEVER touches
//! `*.audited` / `.del`.

#![forbid(unsafe_code)]

use clarus_adapters::storage::ContentAddressedStorage;
use clarus_server::config::Config;
use clarus_server::shadow::ShadowTier;
use clarus_server::state::AppState;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Instant;

struct TestStore {
    state: AppState,
    cas: ContentAddressedStorage,
    manifest_dir: PathBuf,
    _temp: tempfile::TempDir,
}

impl TestStore {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let storage_dir = temp.path().join("cas");
        std::fs::create_dir_all(&storage_dir).expect("cas dir");
        let btree_root = temp.path().join("btree");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
        std::fs::create_dir_all(&idx_dir).expect("idx dir");

        let cas = ContentAddressedStorage::new(storage_dir).expect("open cas");
        let mut config = Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.btree.audit_dir = btree_root.join("audit").to_string_lossy().to_string();

        let state = AppState {
            cas: cas.clone(),
            codec_registry: clarus_adapters::codec::CodecRegistry::new(),
            ups_events: clarus_server::sse_fanout::DualEventSink::new(),
            start_time: Instant::now(),
            metrics_ring: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            log_buffer: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            maintenance: Arc::new(StdRwLock::new(
                clarus_server::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir.clone(), idx_dir),
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        Self {
            state,
            cas,
            manifest_dir,
            _temp: temp,
        }
    }

    fn write_cas(&self, hex: &str) -> PathBuf {
        let path = self.cas.cas_path(hex);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("bucket");
        }
        std::fs::write(&path, b"blob").expect("write cas blob");
        path
    }

    fn orphan_sidecar(&self, study: &str) -> PathBuf {
        self.manifest_dir.join(format!("{study}.orphans"))
    }

    fn write_orphan_sidecar(&self, study: &str, lines: &str) {
        std::fs::write(self.orphan_sidecar(study), lines).expect("write sidecar");
    }

    /// Write a LIVE manifest referencing `hex` (the refcount source).
    fn write_manifest_referencing(&self, study: &str, hex: &str) {
        let mut attrs = clarus_core::stow::StudyManifestAttrs {
            patient_name: Some("GC^LIVE".into()),
            patient_id: Some("GCLIVE".into()),
            study_instance_uid: study.to_string(),
            modality: "CT".into(),
            ..Default::default()
        };
        let blake3 = hex_bytes(hex);
        let instance = clarus_core::stow::InstanceModel {
            sop_instance_uid: format!("{study}.1.1"),
            blake3,
            storage_tier: clarus_core::manifest::TIER_HOT,
            transfer_syntax_uid: "1.2.840.10008.1.2.1".to_string(),
            file_size: 64,
            ..Default::default()
        };
        let series_model = [clarus_core::stow::SeriesModel {
            series_instance_uid: format!("{study}.1"),
            modality: "CT".into(),
            series_number: None,
            instances: vec![instance],
            ..Default::default()
        }];
        clarus_core::stow::write_study_manifest_full(&self.manifest_dir, &mut attrs, &series_model)
            .expect("write manifest");
    }

    fn orphans_pending(&self) -> u64 {
        clarus_server::metrics::ORPHANS_PENDING.load(Ordering::Relaxed)
    }
}

fn hex_bytes(hex: &str) -> [u8; 32] {
    let bytes = hex.as_bytes();
    assert_eq!(bytes.len(), 64, "64-hex input");
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = (bytes[2 * i] as char).to_digit(16).expect("hex") as u8;
        let lo = (bytes[2 * i + 1] as char).to_digit(16).expect("hex") as u8;
        out[i] = (hi << 4) | lo;
    }
    out
}

fn sidecar_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

// -- Scenarios ------------------------------------------------------------

/// One line shared with a LIVE manifest: the GC unlinks the unreferenced one
/// only, KEEPS the referenced one, and the sidecar stays until fully drained.
#[test]
fn gc_unlinks_unreferenced_keeps_referenced_sidecar_stays() {
    let store = TestStore::start();
    let study = "1.2.3.200";
    let hex_a = "aa".repeat(32);
    let hex_b = "bb".repeat(32);
    store.write_cas(&hex_a);
    let blob_b = store.write_cas(&hex_b);
    store.write_orphan_sidecar(study, &format!("{hex_a} 1\n{hex_b} 1\n"));
    store.write_manifest_referencing("1.2.3.201", &hex_b);

    let res = clarus_server::erase_worker::run_orphan_gc(&store.state, &store.manifest_dir);
    let (removed, pending) = res.expect("pass completed");
    assert_eq!(removed, 1, "only the unreferenced blob unlinked");
    assert_eq!(pending, 1, "one line still pending");
    assert_eq!(store.orphans_pending(), 1, "gauge corrected to 1");
    assert!(
        !store.cas.cas_path(&hex_a).exists(),
        "unreferenced blob gone"
    );
    assert!(blob_b.is_file(), "referenced blob kept");
    let lines = sidecar_lines(&store.orphan_sidecar(study));
    assert_eq!(lines.len(), 1, "sidecar keeps the referenced line");
    assert_eq!(
        lines[0],
        format!("{hex_b} 1"),
        "the referenced line remains"
    );
}

/// A clean drain removes the sidecar and stores 0 into ORPHANS_PENDING.
#[test]
fn gc_clean_drain_removes_sidecar_zero_gauge() {
    let store = TestStore::start();
    let study = "1.2.3.210";
    let hex_a = "cc".repeat(32);
    let blob = store.write_cas(&hex_a);
    store.write_orphan_sidecar(study, &format!("{hex_a} 1\n"));

    let res = clarus_server::erase_worker::run_orphan_gc(&store.state, &store.manifest_dir);
    let (removed, pending) = res.expect("pass completed");
    assert_eq!(removed, 1);
    assert_eq!(pending, 0);
    assert_eq!(store.orphans_pending(), 0, "gauge 0 after a clean pass");
    assert!(!blob.exists(), "orphan blob unlinked");
    assert!(!store.orphan_sidecar(study).exists(), "sidecar removed");
}

/// Open-handle retry keeps the sidecar for the next pass; after the handle is
/// released the next pass drains it (Windows semantics; on Unix an open read
/// handle does not block unlink, so only the eventual drain is asserted).
#[test]
fn gc_open_handle_keeps_sidecar_then_drains() {
    let store = TestStore::start();
    let study = "1.2.3.220";
    let hex_a = "dd".repeat(32);
    let blob = store.write_cas(&hex_a);
    store.write_orphan_sidecar(study, &format!("{hex_a} 1\n"));

    let held = std::fs::File::open(&blob).expect("open blob");
    let first = clarus_server::erase_worker::run_orphan_gc(&store.state, &store.manifest_dir);
    assert!(first.is_some(), "pass does not abort");
    // On Windows an open handle blocks unlink (retryable -> sidecar kept with
    // pending >= 1); on Unix an open READ handle does not block, so the first
    // pass already drained. Both are valid - assert the eventual state only.
    let drained_first = !store.orphan_sidecar(study).exists();
    drop(held);

    if drained_first {
        assert_eq!(store.orphans_pending(), 0, "gauge 0 after a clean pass");
        assert!(!blob.exists(), "blob unlinked");
    } else {
        assert!(
            store.orphans_pending() >= 1,
            "retryable failure keeps the sidecar pending"
        );
        let second = clarus_server::erase_worker::run_orphan_gc(&store.state, &store.manifest_dir);
        let (removed, pending) = second.expect("pass completed");
        assert_eq!(removed, 1, "blob unlinked after handle release");
        assert_eq!(pending, 0);
        assert_eq!(store.orphans_pending(), 0);
        assert!(!blob.exists());
        assert!(!store.orphan_sidecar(study).exists(), "sidecar removed");
    }
}
