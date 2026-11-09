// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: STOW ingest hot path gates (isolated binary, inline window +
//! set_enabled(false); ALL fixtures built OUTSIDE the window).
//!
//! Windows:
//! - W1a/W1b: blake3_of_file hash core / end-to-end budget.
//! - W2: shadow.dual_write, warm writers, 3 appends - 0 alloc (covers the
//!   sop_owner append + B1 snapshot fold).
//! - W3: warm SopOwnerSnapshot::find (binary search) - 0 alloc.
//! - W4a: warm sop_owner_study decision lookup, fresh SOP - 0 alloc.
//! - W4b: decide_insert_result Inserted arm - the InsertResult owns 5
//!   Strings, so the window asserts the EXACT 5-allocation budget
//!   (BUDGETED, not a lie about 0-alloc).

use clarus_core::idx::rollover::{DailyIndexSet, day_key_from_system_time};
use clarus_core::idx::sop_owner::SopOwnerSnapshot;
use clarus_ports::dicom::InstanceMeta;
use clarus_ports::stow_response::InsertResult;
use clarus_server::config::Config;
use clarus_server::handlers::stow_pure::{blake3_of_file, decide_insert_result};
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use ferrite_testkit::heap::TestAllocator;
use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::{Instant, SystemTime};

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

fn allocate_env<F, R>(f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let result = f();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (before == after, result)
}

/// Window that asserts an EXACT allocation budget (for the BUDGETED arm).
fn budget_env<F, R>(expect: usize, f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n == expect, result)
}

/// Window that RETURNS the allocation count (self-calibrating gates: two
/// identical warm calls must cost the SAME - a fixed budget, no growth).
fn count_env<F, R>(f: F) -> (usize, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n, result)
}

/// Print the two dependency-free environment labels every gate reports:
/// the build profile and the target architecture. A gate result is only
/// meaningful together with these, the way a benchmark is meaningless
/// without its hardware.
fn print_gate_env(gate: &str) {
    println!(
        "GATE {gate} | profile={} | arch={}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::env::consts::ARCH
    );
}

/// A minimal AppState rooted in a temp dir (same shape as the smoke-test
/// fixtures; all fixture allocation happens here, OUTSIDE the windows).
fn app_state(root: &Path) -> AppState {
    let btree_root = root.join("btree");
    let manifest_dir = btree_root.join("manifests");
    let idx_dir = btree_root.join("idx");
    std::fs::create_dir_all(&manifest_dir).expect("manifests dir");
    std::fs::create_dir_all(&idx_dir).expect("idx dir");
    let storage_dir = root.join("cas");
    std::fs::create_dir_all(&storage_dir).expect("cas dir");

    let mut config = Config::test_default();
    config.storage_dir = storage_dir.to_string_lossy().to_string();
    config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
    config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
    config.min_free_disk_mb = 0;
    config.min_free_disk_pct = 0.0;

    AppState {
        cas: clarus_adapters::storage::ContentAddressedStorage::new(storage_dir).expect("open cas"),
        codec_registry: clarus_adapters::codec::CodecRegistry::default(),
        ups_events: DualEventSink::new(),
        start_time: Instant::now(),
        metrics_ring: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
        log_buffer: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
        maintenance: Arc::new(StdRwLock::new(
            clarus_server::state::MaintenanceState::default(),
        )),
        #[cfg(feature = "s3")]
        s3: Arc::new(std::sync::OnceLock::new()),
        cold_cas: None,
        shadow: ShadowTier::new(idx_dir.clone()),
        study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
        config,
        workitems_dir: btree_root.join("workitems"),
        commitments_dir: btree_root.join("commitments"),
        transcode_pool: clarus_server::state::TranscodePool::new(32),
        readiness: clarus_server::handlers::readiness::Readiness::new(),
    }
}

/// a. The 64 KiB hash core of blake3_of_file (Hasher::new, chunked
/// update, finalize) - 0 alloc, same chunk pattern as a file.
#[test]
fn blake3_hash_core_alloc_zero() {
    print_gate_env("W1a blake3_hash_core");
    const CHUNK: usize = 64 * 1024; // same chunk size as blake3_of_file
    let data = vec![0xAAu8; 300_000]; // fixture OUTSIDE the window
    let mut hasher = blake3::Hasher::new(); // warm-up outside
    hasher.update(&data);
    let (zero, digest) = allocate_env(|| {
        ferrite_macros::zero_alloc!({
            let mut h = blake3::Hasher::new();
            for chunk in data.chunks(CHUNK) {
                h.update(chunk);
            }
            *h.finalize().as_bytes()
        })
    });
    assert_eq!(digest.len(), 32);
    assert!(zero, "the blake3 hash core must be zero-allocation");
}

/// b. blake3_of_file end to end: exactly the platform cost of File::open,
/// not one allocation more. The open cost itself is platform- and
/// std-dependent (Windows: 1 wide-path Vec; Linux with recent std: 0 -
/// short paths open without a heap CString), so the gate self-calibrates:
/// measure a bare warm File::open on THIS platform, then demand
/// blake3_of_file costs exactly that. The read loop and the hash core add
/// zero (W1a proves the core is 0-alloc), so any growth is a Clarus
/// regression, not a std detail.
#[test]
fn blake3_of_file_alloc_budget_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let f = tmp.path().join("sop.dcm");
    std::fs::write(&f, vec![0xAAu8; 300_000]).expect("write fixture");
    // Warm the platform's File::open path, then measure its fixed cost.
    let _ = std::fs::File::open(&f).expect("warm open");
    let (open_cost, _) = count_env(|| {
        let _ = std::fs::File::open(&f).expect("open");
    });
    let (n, digest) = count_env(|| blake3_of_file(&f).expect("hash"));
    assert_eq!(digest.len(), 32);
    assert_eq!(
        n, open_cost,
        "blake3_of_file must cost exactly File::open on this platform"
    );
}

/// . WARM shadow append (`ShadowTier::dual_write_append`): the date,
/// modality and sop_owner leaf appends with no cold setup - 0 alloc per call.
///
/// 0810: after the warm/cold split this exercises the WARM function directly
/// (the appenders are opened OUTSIDE the window, as the real cold caller
/// does once per day). The static no-alloc gate asserts the same function's
/// body has no allocator call in ANY branch.
#[test]
fn dual_write_alloc_zero() {
    print_gate_env("W2 dual_write_append");
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = app_state(tmp.path());
    let idx_dir = app.config.btree.idx_dir.clone();
    let day = day_key_from_system_time(SystemTime::now());

    // COLD setup outside the window: open the three appenders once.
    let mut index = DailyIndexSet::new(std::path::PathBuf::from(&idx_dir), day);
    let (date, modality, sop_owner) = index.writers_mut("CT").expect("open appenders");
    // Warm the very first append (empty -> first row) outside the window too.
    ShadowTier::dual_write_append(date, modality, sop_owner, &[1u8; 32], "1.2.3", "1.2.3.1")
        .expect("warm append");

    let b3a = [2u8; 32];
    let b3b = [3u8; 32];
    let b3c = [4u8; 32];
    let sops = ["1.2.3.2", "1.2.3.3", "1.2.3.4"];
    let (zero, ()) = allocate_env(|| {
        ferrite_macros::zero_alloc!({
            for (b3, sop) in [(&b3a, sops[0]), (&b3b, sops[1]), (&b3c, sops[2])] {
                ShadowTier::dual_write_append(date, modality, sop_owner, b3, "1.2.3", sop)
                    .expect("append");
            }
        })
    });
    assert!(zero, "warm dual_write_append must be zero-allocation");
}

/// . Warm SopOwnerSnapshot::find - binary search over the sorted rows,
/// 0 alloc (snapshot built outside the window).
#[test]
fn warm_sop_owner_find_alloc_zero() {
    print_gate_env("W3 sop_owner_find");
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = app_state(tmp.path());
    app.shadow
        .dual_write([1u8; 32], "CT", "1.2.3", "1.2.3.1")
        .expect("append");
    let idx_dir = app.config.btree.idx_dir.clone();
    let day = day_key_from_system_time(SystemTime::now());
    let snap = SopOwnerSnapshot::load(std::path::Path::new(&idx_dir), &day)
        .expect("load")
        .expect("segment exists");

    let (zero, (hit, miss)) = allocate_env(|| {
        ferrite_macros::zero_alloc!({
            let hit = snap.find_warm(b"1.2.3.1").is_some();
            let miss = snap.find_warm(b"9.9.9.9").is_some();
            (hit, miss)
        })
    });
    assert!(hit && !miss, "find returns the owner only for known rows");
    assert!(zero, "warm sop_owner find must be zero-allocation");
}

/// a. WARM decision lookup (`ShadowTier::sop_owner_study_warm`), fresh SOP -
/// 0 alloc. The day-list cache + snapshots are built by the warm-up call
/// OUTSIDE the window.
///
/// 0810: exercises the WARM function directly; the public
/// `sop_owner_study` wraps it (cold cache maintenance + the `String`
/// materialization) and must keep behaving identically.
#[test]
fn warm_sop_owner_study_lookup_alloc_zero() {
    print_gate_env("W4a sop_owner_study_warm");
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = app_state(tmp.path());
    // Warm-up: create a sop_owner day segment, then build the cached day list
    // + lens + snapshots (all outside the window).
    app.shadow
        .dual_write([1u8; 32], "CT", "1.2.3", "1.2.3.1")
        .expect("append");
    assert!(app.shadow.sop_owner_study("9.9.9.1").is_none());
    // The warm function needs the cached day list + snapshot map. Drive it
    // through the public API's cache by calling the public lookup once more
    // (still outside the window) - the warm fn is then handed the SAME cache
    // the public path uses.
    let idx_dir = app.config.btree.idx_dir.clone();
    let day = day_key_from_system_time(SystemTime::now());
    let mut days = vec![day];
    let mut cache: std::collections::HashMap<[u8; 8], SopOwnerSnapshot> =
        std::collections::HashMap::new();
    cache.insert(
        day,
        SopOwnerSnapshot::load(std::path::Path::new(&idx_dir), &day)
            .expect("load")
            .expect("segment exists"),
    );
    let (zero, owner) = allocate_env(|| {
        ferrite_macros::zero_alloc!({
            let looked_up = ShadowTier::sop_owner_study_warm(&days, &cache, "9.9.9.2");
            std::hint::black_box(looked_up)
        })
    });
    assert!(owner.is_none());
    assert!(zero, "warm sop_owner_study lookup must be zero-allocation");
    days.clear();
}

/// b. decide_insert_result Inserted arm: the decision is free; the
/// InsertResult owns 5 Strings (BUDGETED). The window asserts the exact
/// 5-allocation budget and the correct outcome - an honest budget gate,
/// not a false 0-alloc claim.
#[test]
fn decide_insert_result_inserted_arm_exact_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = app_state(tmp.path());
    let meta = InstanceMeta {
        sop_instance_uid: "9.8.7.6".to_string(),
        sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".to_string()),
        study_instance_uid: "2.2.2".to_string(),
        series_instance_uid: "2.2.2.3".to_string(),
        patient_id: "PAT".to_string(),
        modality: "CT".to_string(),
        ..Default::default()
    };
    let blake3 = [7u8; 32];
    // Warm-up: builds the sop_owner day-list cache (outside the window).
    assert!(matches!(
        decide_insert_result(&app, &meta, &blake3),
        InsertResult::Inserted { .. }
    ));
    // Budget window: the whole call allocates exactly the 5 owned Strings
    // of the Inserted arm (sop_class_uid, sop, study, series, patient_id).
    // The decision lookup itself is 0-alloc (covered by 4a after warm-up);
    // there is no separate 0-alloc window here because the call always
    // includes the arm.
    let (exact, res) = budget_env(5, || decide_insert_result(&app, &meta, &blake3));
    assert!(
        matches!(res, InsertResult::Inserted { .. }),
        "fresh SOP decides Inserted"
    );
    assert!(
        exact,
        "the Inserted arm must cost exactly its 5 owned-String budget"
    );
}

/// c. known-SOP Idempotent arm (01.09 rewrite: ONE manifest read for the
/// whole decision): two identical warm calls must cost the SAME amount -
/// a fixed budget per call, no per-call growth (the hot-path property).
#[test]
fn decide_insert_result_idempotent_arm_fixed_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = app_state(tmp.path());
    // Fixtures OUTSIDE the window: sop_owner row + a real manifest whose
    // study PatientID matches and which contains the SOP with the incoming
    // BLAKE3 (the Idempotent shape).
    app.shadow
        .dual_write([7u8; 32], "CT", "2.2.2", "9.8.7.6")
        .expect("sop_owner row");
    let mut attrs = clarus_core::stow::StudyManifestAttrs {
        study_instance_uid: "2.2.2".into(),
        patient_id: Some("PAT".into()),
        ..Default::default()
    };
    let series = vec![clarus_core::stow::SeriesModel {
        series_instance_uid: "2.2.2.3".into(),
        modality: "CT".into(),
        series_number: Some("1".into()),
        instances: vec![clarus_core::stow::InstanceModel {
            sop_instance_uid: "9.8.7.6".into(),
            sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".into()),
            blake3: [7u8; 32],
            storage_tier: clarus_core::manifest::TIER_HOT,
            transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
            file_size: 1_000,
            ..Default::default()
        }],
        ..Default::default()
    }];
    clarus_core::stow::write_study_manifest_full(
        std::path::Path::new(&app.config.btree.manifest_dir),
        &mut attrs,
        &series,
    )
    .expect("manifest write");
    let meta = InstanceMeta {
        sop_instance_uid: "9.8.7.6".to_string(),
        sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".to_string()),
        study_instance_uid: "2.2.2".to_string(),
        series_instance_uid: "2.2.2.3".to_string(),
        patient_id: "PAT".to_string(),
        modality: "CT".to_string(),
        ..Default::default()
    };
    let blake3 = [7u8; 32];
    // TRUE warm-up outside the windows: shadow day-list cache, first
    // manifest read (decoder/lazy state) - all one-time costs.
    let _ = count_env(|| decide_insert_result(&app, &meta, &blake3));
    let (n1, r1) = count_env(|| decide_insert_result(&app, &meta, &blake3));
    assert!(
        matches!(r1, InsertResult::Idempotent { .. }),
        "same bytes decide Idempotent"
    );
    let (n2, r2) = count_env(|| decide_insert_result(&app, &meta, &blake3));
    assert!(matches!(r2, InsertResult::Idempotent { .. }));
    assert!(
        n1 > 0,
        "the Idempotent arm allocates (manifest read + owned enum) - the gate bounds, not lies"
    );
    assert_eq!(
        n1, n2,
        "warm Idempotent decision must have a FIXED per-call budget"
    );
    println!("AP101 W4c fixed-budget Idempotent arm: {n1} allocs/call");
}

/// d. stale sop_owner row, manifest FILE GONE (01.09 erase gap): decides
/// Inserted with a fixed per-call budget (two identical warm calls equal).
#[test]
fn decide_insert_result_stale_row_arm_fixed_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = app_state(tmp.path());
    // sop_owner row EXISTS, the owner study's manifest file does NOT
    // (the exact post-admin-delete disk state).
    app.shadow
        .dual_write([7u8; 32], "CT", "2.2.2", "9.8.7.6")
        .expect("sop_owner row");
    let meta = InstanceMeta {
        sop_instance_uid: "9.8.7.6".to_string(),
        sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".to_string()),
        study_instance_uid: "2.2.2".to_string(),
        series_instance_uid: "2.2.2.3".to_string(),
        patient_id: "PAT".to_string(),
        modality: "CT".to_string(),
        ..Default::default()
    };
    let blake3 = [7u8; 32];
    // TRUE warm-up outside the windows: shadow day-list cache + one-time
    // lazy state.
    let _ = count_env(|| decide_insert_result(&app, &meta, &blake3));
    let (n1, r1) = count_env(|| decide_insert_result(&app, &meta, &blake3));
    assert!(
        matches!(r1, InsertResult::Inserted { .. }),
        "stale row decides Inserted"
    );
    let (n2, r2) = count_env(|| decide_insert_result(&app, &meta, &blake3));
    assert!(matches!(r2, InsertResult::Inserted { .. }));
    assert_eq!(
        n1, n2,
        "stale-row decision must have a FIXED per-call budget"
    );
    println!("AP101 W4d fixed-budget stale-row arm: {n1} allocs/call");
}
