// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! MWL and MPPS must each leave ONE tagged INFO line in the log.
//!
//! The trap this guards is the `UPS_EVENT` bug: an INFO line whose tag is not
//! on the `INFO_WHITELIST` is dropped in RELEASE builds, so the feature logs
//! nothing in production while working perfectly in a developer's debug run.
//! `clog.rs` carries one whitelist entry per tag; this test proves the two new
//! tags (`MWL_SEARCH`, `MPPS_EVENT`) are on it and reach the writer channel.
//!
//! RELEASE ONLY (`#![cfg(not(debug_assertions))]`): in a debug build `clog::emit`
//! writes straight to stderr and never pushes to the writer channel, so there
//! is nothing to read back. The line SHAPE is pinned in every mode by the unit
//! tests beside the handlers.
//!
//! Run it with: cargo test --release -p clarus --test workflow_event_log

#![cfg(not(debug_assertions))]
#![forbid(unsafe_code)]

use clarus_server::config::Config;
use clarus_server::handlers::{mpps_pure, mwl_pure};
use clarus_server::shadow::ShadowTier;
use clarus_server::state::AppState;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

/// Collect everything the clog channel has received so far.
fn drain(rx: &Receiver<Vec<u8>>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(record) = rx.try_recv() {
        out.push(String::from_utf8_lossy(&record).into_owned());
    }
    out
}

/// A minimal `AppState` with the workflow surface ENABLED and its store under
/// a fresh temp directory, so the handlers run their real code path.
fn state(temp: &std::path::Path) -> Arc<AppState> {
    let btree_root = temp.join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::create_dir_all(&idx_dir).unwrap();

    let mut config = Config::test_default();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.port = 0;
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;
    config.workflow.enabled = true;
    config.workflow.dir = temp.join("workflow").to_string_lossy().to_string();
    let storage_dir = temp.join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();
    config.storage_dir = storage_dir.to_string_lossy().to_string();

    let cas =
        clarus_adapters::storage::ContentAddressedStorage::new(temp.join("cas")).expect("open cas");

    Arc::new(AppState {
        cas,
        codec_registry: clarus_adapters::codec::CodecRegistry::default(),
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
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
        config,
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(32),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    })
}

#[test]
fn mwl_search_and_mpps_create_each_log_one_tagged_line() {
    // The writer channel is a plain sync_channel and the server's clog writer
    // thread is NOT started here, so every record lands directly in the
    // channel and is read back below.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4096);
    clarus_server::clog::set_sender(tx);

    let temp = tempfile::tempdir().expect("tempdir");
    let state = state(temp.path());

    // MWL search: an empty store, so the answer is an empty list - but the
    // line must still be emitted (it records the search, not the hits). The
    // last argument is the `<service>` the PS3.18 8.3.4.4.1 truncation Warning
    // would name; it is unused on an empty result.
    if mwl_pure::search(
        &state,
        Some("ScheduledStationAETitle=CT01"),
        None,
        true,
        "localhost",
    )
    .is_err()
    {
        panic!("the MWL search must answer");
    }

    std::thread::sleep(Duration::from_millis(100));
    let mwl_lines: Vec<String> = drain(&rx)
        .into_iter()
        .filter(|l| l.contains("MWL_SEARCH"))
        .collect();
    assert_eq!(
        mwl_lines.len(),
        1,
        "one search -> one MWL_SEARCH line, got {mwl_lines:#?}"
    );
    assert!(
        mwl_lines[0].contains("station=CT01"),
        "the line must name the station: {}",
        mwl_lines[0]
    );
    assert!(
        mwl_lines[0].contains("n_matched=0"),
        "the line must count the matches: {}",
        mwl_lines[0]
    );

    // MPPS create: a valid IN PROGRESS N-CREATE.
    let body = br#"[{"00400252":{"vr":"CS","Value":["IN PROGRESS"]},
                    "00080060":{"vr":"CS","Value":["CT"]}}]"#;
    if mpps_pure::create(&state, "1.2.826.0.1.3680043.10.1234.9601", body, None, true).is_err() {
        panic!("the MPPS create must succeed");
    }

    std::thread::sleep(Duration::from_millis(100));
    let mpps_lines: Vec<String> = drain(&rx)
        .into_iter()
        .filter(|l| l.contains("MPPS_EVENT"))
        .collect();
    assert_eq!(
        mpps_lines.len(),
        1,
        "one create -> one MPPS_EVENT line, got {mpps_lines:#?}"
    );
    assert!(
        mpps_lines[0].contains("mpps_uid=1.2.826.0.1.3680043.10.1234.9601")
            && mpps_lines[0].contains("op=create")
            && mpps_lines[0].contains("previous=created")
            && mpps_lines[0].contains("new=IN PROGRESS"),
        "the line must carry the operation and the transition: {}",
        mpps_lines[0]
    );
}
