// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Runtime B-tree modality-idx fixer (lazy heal on exact lookup).
//!
//! Lazy recovery (C-008) is a ONE-TIME pass at startup: it rebuilds the
//! modality-idx entries missing at load, but staleness that appears in
//! *runtime* (a crash between the manifest commit and the idx append during
//! STOW, or a lost `.idx`) hangs until restart. This module heals it lazily:
//! when an **exact study-UID QIDO** confirms the study is in `manifests/` but
//! its UID is absent from `modality/{MOD}/{YYYYMMDD}.idx`, the hot path just
//! `try_send`s a small heal request to a dedicated std thread (log-writer /
//! erase-worker pattern) and counts the detection. The fixer thread appends
//! the missing row (append is idempotent by read-bucket-then-append), keeping
//! the accelerator consistent with the manifest truth.
//!
//! Philosophy (1:1 with `clarus_core::recovery::reconcile_idx`):
//! idx is an accelerator, manifest is the truth. A stale row nobody reads is
//! harmless; the first exact lookup of a study repairs its own row in the
//! background. Duplicate detects / dropped `try_send`s are normal - the append
//! is idempotent and the next exact lookup re-pokes.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::{OnceLock, mpsc::SyncSender, mpsc::sync_channel};

/// A single heal request: append `study_uid` to `modality/{MOD}/{DAY}.idx`.
///
/// The fixer thread owns the `idx/` root (passed to [`spawn`]); a request
/// carries only the row identity so a stray request never dictates where the
/// fixer writes.
#[derive(Debug, Clone)]
pub struct HealRequest {
    pub modality: String,
    pub day: [u8; 8],
    pub study_uid: String,
}

/// Bounded channel capacity: a stray burst of stale detections must never grow
/// memory without bound. Overflow is dropped silently (self-heal re-pokes on
/// the next exact lookup).
const RING_SIZE: usize = 1024;

/// The shared heal channel sender, installed once by [`spawn`].
static TX: OnceLock<SyncSender<HealRequest>> = OnceLock::new();

/// True once the fixer channel is installed (used to skip work before init).
pub fn is_ready() -> bool {
    TX.get().is_some()
}

/// Spawn the dedicated fixer thread and install the global sender.
///
/// Must be called once at startup (from `main.rs`), rooted at the server's
/// `idx/` directory. The thread drains the bounded channel, healing each
/// request's missing modality row via
/// [`clarus_core::recovery::heal_modality_row_if_missing`] and counting the
/// heal in the live gauge on completion. A request whose row is already present
/// (a duplicate, or healed by an earlier pass) still closes its outstanding
/// detect (no append needed, but the gauge is decremented) so the live staleness
/// converges to 0 even under concurrent detects of the same study.
pub fn spawn(idx_dir: PathBuf) {
    let (tx, rx) = sync_channel::<HealRequest>(RING_SIZE);
    let _ = TX.set(tx);
    std::thread::Builder::new()
        .name("idx-fixer".into())
        .spawn(move || {
            loop {
                match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                    Ok(req) => heal_with_retry(&idx_dir, req),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        })
        .expect("spawn idx-fixer thread");
}

/// How many times one request is retried before it is abandoned.
///
/// A transient `ENOSPC`/lock on the idx directory is the realistic failure, and
/// those clear on their own. Three attempts spans roughly 300 ms - long enough
/// to outlast a filesystem stall, short enough that a genuinely broken idx does
/// not wedge the fixer thread behind one poison request. Abandoning keeps the
/// gauge incremented (the staleness is real), and the next exact lookup of the
/// same study re-pokes, so no repair is lost forever.
const HEAL_ATTEMPTS: u32 = 3;

/// Retry a failed heal with linear backoff before giving up.
///
/// The error was previously discarded (`let _ = heal_one(..)`), which lost the
/// request outright: nothing in the channel could resend it, and because
/// `record_idx_stale_detect` had already fired, `CLARUS_IDX_RUNTIME_STALE` kept
/// a +1 that no later completion would ever close. That is the failure mode the
/// gauge exists to show, so the request must be retried rather than dropped.
fn heal_with_retry(idx_dir: &std::path::Path, req: HealRequest) {
    for attempt in 1..=HEAL_ATTEMPTS {
        match heal_one(idx_dir, req.clone()) {
            Ok(_) => return,
            Err(e) => {
                if attempt == HEAL_ATTEMPTS {
                    // The gauge stays counted: the row really is still stale,
                    // and the next exact lookup re-pokes this study.
                    crate::clog_warn!(
                        modality = %req.modality,
                        error = %e,
                        "idx fixer: heal failed after {HEAL_ATTEMPTS} attempts"
                    );
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)));
            }
        }
    }
}

/// Append one request's missing row (idempotent). Returns the heal's outcome:
/// `Ok(true)` if the row was missing and is now appended, `Ok(false)` if it was
/// already present, `Err` on I/O failure.
///
/// Counts the heal gauge ON COMPLETION of the job - both a real append
/// (`Ok(true)`) and a duplicate already-present row (`Ok(false)`) close one
/// outstanding detect, so two concurrent detects of one study converge to 0
/// (first task appends -> -1, second sees the row present -> -1). An `Err` does
/// NOT zero the gauge: the work did not complete, so the staleness stays
/// visible (correct, not falsely clean).
fn heal_one(idx_dir: &std::path::Path, req: HealRequest) -> std::io::Result<bool> {
    match clarus_core::recovery::heal_modality_row_if_missing(
        idx_dir,
        &req.modality,
        req.day,
        &req.study_uid,
    ) {
        Ok(healed) => {
            crate::metrics::record_idx_stale_heal();
            Ok(healed)
        }
        Err(e) => Err(e),
    }
}

/// Enqueue one heal request and count a detect ONLY on successful enqueue.
///
/// Returns `true` when the request was enqueued (a detect was recorded) and
/// `false` when the channel was full and the request dropped (NO detect - a
/// dropped request is never healed, so counting it would leak a permanent +1
/// on `CLARUS_IDX_RUNTIME_STALE`; the next exact lookup re-pokes instead).
#[inline]
fn try_request(tx: &SyncSender<HealRequest>, req: HealRequest) -> bool {
    if tx.try_send(req).is_ok() {
        crate::metrics::record_idx_stale_detect();
        true
    } else {
        false
    }
}

/// Hot-path detection: register that an exact lookup found a study missing from
/// the modality-idx and enqueue its repair. Non-blocking (`try_send`); if the
/// channel is full the request is dropped and NO detect is counted (a dropped
/// request is never healed, so counting it would leak a permanent +1 on
/// `CLARUS_IDX_RUNTIME_STALE`; the next exact lookup re-pokes instead).
///
/// MUST only be called after [`needs_heal`] confirmed the study is absent from
/// the modality-idx (otherwise a healthy study would count a false detect and
/// the gauge would never converge to 0).
///
/// The caller supplies `modality`, the `YYYYMMDD` day key (from the manifest's
/// `StudyDate`), and the study UID - all available to the exact-UID QIDO path.
/// The fixer thread owns the `idx/` root (captured at [`spawn`]).
pub fn request(modality: &str, day: [u8; 8], study_uid: &str) {
    if let Some(tx) = TX.get() {
        let _ = try_request(
            tx,
            HealRequest {
                modality: modality.to_string(),
                day,
                study_uid: study_uid.to_string(),
            },
        );
    }
}

/// Confirm what an exact study-UID lookup can only infer: is `study_uid`
/// ABSENT from its `modality/{MOD}/{DAY}.idx` bucket?
///
/// Returns `true` ONLY when the study's modality row is actually missing (i.e.
/// a genuine runtime staleness to heal). A study that is present in the bucket
/// returns `false` - the caller must NOT poke the fixer or count a detect, so
/// a healthy archive never inflates `CLARUS_IDX_RUNTIME_STALE`.
///
/// This is the hot-path confirmation read (a single small bucket file), kept
/// to the exact-UID lookup path only; broad queries never reach here.
pub fn needs_heal(
    idx_dir: &std::path::Path,
    modality: &str,
    day: [u8; 8],
    study_uid: &str,
) -> bool {
    if modality.is_empty() {
        return false;
    }
    let bucket =
        clarus_core::idx::modality::read_study_uids(idx_dir, modality, &day).unwrap_or_default();
    !bucket.iter().any(|u| u == study_uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clarus_core::idx::modality;
    use std::sync::Mutex;
    use std::sync::mpsc;
    use tempfile::tempdir;

    /// Serialises the tests that mutate the process-global
    /// `CLARUS_IDX_RUNTIME_STALE` gauge via detect/heal. The harness runs tests
    /// in parallel threads over one shared global, so an absolute-delta assert
    /// would race another test's concurrent `record_idx_stale_detect/heal`;
    /// holding this lock (and reading `before` inside it) gives each gauge-test
    /// an isolated snapshot. Non-gauge tests (`needs_heal_*`) don't take it.
    static IDX_METRIC_LOCK: Mutex<()> = Mutex::new(());

    /// Drive the heal drain against a real temp idx dir (parallel safe: no
    /// shared global channel).
    #[test]
    fn heal_one_appends_missing_row_idempotently() {
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        // Balance the two heals this test triggers (each heal is -1 on the
        // process-global gauge) with two covering detects so the gauge never
        // underflows from heal-only calls in isolation.
        crate::metrics::record_idx_stale_detect();
        crate::metrics::record_idx_stale_detect();
        let dir = tempdir().expect("tempdir");
        let uid = "1.2.840.1.2.3.4.5.100";
        let day = *b"20240115";
        // First heal: row is missing -> appended, returns true.
        let req = HealRequest {
            modality: "CT".into(),
            day,
            study_uid: uid.into(),
        };
        assert!(heal_one(dir.path(), req).expect("heal"));
        let bucket = modality::read_study_uids(dir.path(), "CT", &day).unwrap();
        assert!(bucket.iter().any(|u| u == uid));
        // Second heal: row now present -> no-op (false), no duplicate row.
        let req2 = HealRequest {
            modality: "CT".into(),
            day,
            study_uid: uid.into(),
        };
        assert!(!heal_one(dir.path(), req2).expect("heal2"));
        let bucket2 = modality::read_study_uids(dir.path(), "CT", &day).unwrap();
        assert_eq!(bucket2.iter().filter(|u| *u == uid).count(), 1);
    }

    #[test]
    fn heal_one_empty_modality_is_noop() {
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        // The no-op heal is still counted (Ok(false), -1 on the global gauge);
        // balance it with a covering detect so the gauge doesn't underflow.
        crate::metrics::record_idx_stale_detect();
        let dir = tempdir().expect("tempdir");
        let req = HealRequest {
            modality: String::new(),
            day: *b"20240115",
            study_uid: "1.2.3".into(),
        };
        assert!(!heal_one(dir.path(), req).expect("noop"));
    }

    #[test]
    fn request_enqueues_and_processes_background_heal() {
        // Stand up a private channel to validate the request -> drain contract
        // without touching the process-global sender (parallel safe).
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        // The heal below is -1 on the global gauge; balance it with a covering
        // detect so the gauge doesn't underflow from heal-only isolation.
        crate::metrics::record_idx_stale_detect();
        let dir = tempdir().expect("tempdir");
        let (tx, rx) = mpsc::channel::<HealRequest>();
        let uid = "9.8.7.6.5";
        let req = HealRequest {
            modality: "MR".into(),
            day: *b"20240301",
            study_uid: uid.into(),
        };
        tx.send(req).unwrap();
        let got = rx.recv().unwrap();
        assert_eq!(got.study_uid, uid);
        assert!(heal_one(dir.path(), got).expect("heal"));
        let bucket = modality::read_study_uids(dir.path(), "MR", b"20240301").unwrap();
        assert!(bucket.iter().any(|u| u == uid));
    }

    // -- Regression: a failed heal is retried, not discarded --

    /// A heal that fails must be RETRIED, because its detect was already
    /// counted and nothing else can resend the request.
    ///
    /// The drain used to run `let _ = heal_one(..)`: a transient I/O failure
    /// dropped the request while `record_idx_stale_detect` had already added a
    /// +1 to `CLARUS_IDX_RUNTIME_STALE`. No later completion could close it, so
    /// the gauge showed permanent staleness that no lookup would ever clear.
    /// `heal_with_retry` re-attempts instead.
    ///
    /// The obstacle is a FILE where the modality-idx directory must be: the
    /// first attempt fails, the test removes it, and a retry inside the same
    /// call must then succeed. If the retry loop is removed, only the first
    /// attempt runs and the row is never written.
    #[test]
    fn a_failed_heal_is_retried_and_then_succeeds() {
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        crate::metrics::record_idx_stale_detect();
        let dir = tempdir().expect("tempdir");
        let day = *b"20240501";
        let uid = "1.2.3.4.5.6.7";
        // `modality/` as a regular FILE makes the append fail on the first try.
        std::fs::write(dir.path().join("modality"), b"not a directory").expect("blocker");
        assert!(
            heal_one(
                dir.path(),
                HealRequest {
                    modality: "CT".into(),
                    day,
                    study_uid: uid.into(),
                }
            )
            .is_err(),
            "the blocker must make the first attempt fail"
        );

        // Clear the obstacle from another thread so a later attempt succeeds:
        // this is what a transient filesystem stall looks like.
        let path = dir.path().to_path_buf();
        let clearer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::fs::remove_file(path.join("modality")).expect("clear blocker");
        });

        heal_with_retry(
            dir.path(),
            HealRequest {
                modality: "CT".into(),
                day,
                study_uid: uid.into(),
            },
        );
        clearer.join().expect("clearer");

        let bucket = modality::read_study_uids(dir.path(), "CT", &day).expect("bucket");
        assert!(
            bucket.iter().any(|u| u == uid),
            "the retried heal must have written the row"
        );
    }

    // -- Regression: detect only on CONFIRMED absence (owner Fix 1) --
    #[test]
    fn needs_heal_healthy_study_is_false() {
        // A study whose modality-idx row IS present is healthy: the exact-UID
        // lookup must NOT count a detect (provenance already in the bucket).
        let dir = tempdir().expect("tempdir");
        let day = *b"20240115";
        let uid = "1.2.3.4.5";
        // Seed the bucket exactly as the STOW dual-write would.
        let mut set = clarus_core::idx::rollover::DailyIndexSet::new(dir.path().to_path_buf(), day);
        let mut row = [0u8; 65];
        let filled = clarus_core::idx::modality::format_row(uid, &mut row);
        set.modality_writer("CT").unwrap().append(filled).unwrap();
        assert!(!needs_heal(dir.path(), "CT", day, uid));
    }

    #[test]
    fn needs_heal_stale_study_is_true() {
        // A study in the manifest but ABSENT from an empty modality-idx bucket
        // is genuinely stale: the exact-UID lookup must count a detect.
        let dir = tempdir().expect("tempdir");
        let day = *b"20240301";
        assert!(needs_heal(dir.path(), "MR", day, "9.8.7.6"));
    }

    #[test]
    fn detect_then_heal_gauge_returns_to_zero() {
        // A stale detect followed by a successful heal must leave the live
        // gauge unchanged (returns to 0 on a healthy archive). Isolated via
        // the shared metric lock so the harness's parallel threads can't race
        // our absolute reading of the process-global gauge.
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        let before = crate::metrics::idx_runtime_stale();
        crate::metrics::record_idx_stale_detect();
        crate::metrics::record_idx_stale_heal();
        assert_eq!(
            crate::metrics::idx_runtime_stale(),
            before,
            "detect+heal must cancel to the prior value (0 = clean)"
        );
    }

    // -- OWNER FIX 2 regression --
    // 1) detect ONLY on a successful enqueue (a dropped request must not leak a
    //    permanent +1 - it is never healed).
    // 2) heal counts on COMPLETION: both Ok(true) and Ok(false) close one
    //    outstanding detect (so two concurrent detects of one study converge),
    //    while Err keeps the gauge (work not done, not falsely clean).

    #[test]
    fn try_request_full_channel_drops_without_detect() {
        // A full bounded channel must drop the request and count NO detect:
        // a dropped request is never healed, so counting it would leak a
        // permanent +1 on CLARUS_IDX_RUNTIME_STALE. Isolated via the shared
        // metric lock so the parallel harness can't race the global gauge.
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        let (tx, _rx) = mpsc::sync_channel::<HealRequest>(1);
        // Fill the single slot so the next send fails with Full.
        tx.send(HealRequest {
            modality: "CT".into(),
            day: *b"20240115",
            study_uid: "1.2.3".into(),
        })
        .unwrap();
        let before = crate::metrics::idx_runtime_stale();
        let pending = try_request(
            &tx,
            HealRequest {
                modality: "CT".into(),
                day: *b"20240115",
                study_uid: "1.2.3.4".into(),
            },
        );
        assert!(!pending, "full channel must drop the request");
        assert_eq!(
            crate::metrics::idx_runtime_stale(),
            before,
            "a dropped (unhealed) request must NOT count a detect"
        );
    }

    #[test]
    fn try_request_success_counts_detect() {
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        let (tx, _rx) = mpsc::sync_channel::<HealRequest>(1);
        let before = crate::metrics::idx_runtime_stale();
        let queued = try_request(
            &tx,
            HealRequest {
                modality: "CT".into(),
                day: *b"20240115",
                study_uid: "1.2.3".into(),
            },
        );
        assert!(queued, "an empty channel must accept the request");
        assert_eq!(
            crate::metrics::idx_runtime_stale(),
            before + 1,
            "a queued request counts exactly one detect"
        );
    }

    #[test]
    fn two_concurrent_detects_converge_after_two_heals() {
        // Two concurrent detects of ONE study (+2) must fully converge after both
        // fixer jobs complete: the first appends (Ok(true), -1) and the second
        // finds the row already present (Ok(false), -1). No permanent +1 stuck.
        // Isolated via the shared metric lock (process-global gauge).
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        let dir = tempdir().expect("tempdir");
        let uid = "1.2.3.4.5.6";
        let day = *b"20240115";
        let before = crate::metrics::idx_runtime_stale();
        // Two concurrent detects of the same study.
        crate::metrics::record_idx_stale_detect();
        crate::metrics::record_idx_stale_detect();
        assert_eq!(crate::metrics::idx_runtime_stale(), before + 2);
        // First fixer job: appends the missing row.
        let req1 = HealRequest {
            modality: "CT".into(),
            day,
            study_uid: uid.into(),
        };
        assert!(heal_one(dir.path(), req1).expect("heal1"));
        assert_eq!(crate::metrics::idx_runtime_stale(), before + 1);
        // Second fixer job: row now present -> Ok(false), still cancels its detect.
        let req2 = HealRequest {
            modality: "CT".into(),
            day,
            study_uid: uid.into(),
        };
        assert!(!heal_one(dir.path(), req2).expect("heal2"));
        assert_eq!(
            crate::metrics::idx_runtime_stale(),
            before,
            "two detects + two heals (Ok(true), Ok(false)) must converge to 0"
        );
    }

    #[test]
    fn heal_err_does_not_zero_the_gauge() {
        // An I/O failure during heal must NOT decrement the gauge: the work did
        // not complete, so the staleness stays visible (correct, not falsely
        // clean) for the next exact lookup to re-poke. Isolated via the shared
        // metric lock (process-global gauge).
        let _g = IDX_METRIC_LOCK.lock().unwrap();
        let dir = tempdir().expect("tempdir");
        // A modality carrying a path separator is rejected by the appender
        // (PS3.3 Section C.7.3.1.1: a modality is a single CS value, never a path
        // fragment) -> heal fails with Err. The row is absent, so the appender
        // error path is reached (not the already-present short-circuit).
        let before = crate::metrics::idx_runtime_stale();
        crate::metrics::record_idx_stale_detect();
        assert_eq!(crate::metrics::idx_runtime_stale(), before + 1);
        let req = HealRequest {
            modality: "CT/../escape".into(),
            day: *b"20240115",
            study_uid: "1.2.3.7".into(),
        };
        assert!(
            heal_one(dir.path(), req).is_err(),
            "rejected modality must Err"
        );
        assert_eq!(
            crate::metrics::idx_runtime_stale(),
            before + 1,
            "an Err heal must NOT zero the gauge (staleness stays visible)"
        );
    }
}
