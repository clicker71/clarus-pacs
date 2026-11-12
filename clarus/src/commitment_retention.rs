// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Retention sweep for Storage Commitment results (PS3.18 Section 13 policy).
//!
//! PS3.18 13.2 fixes no duration for how long a Commit result must remain
//! available; it instead requires the origin to state the duration in its
//! Conformance Statement, and Table 13.5.3-1's 404 covers "the origin server
//! may have deleted the applicable result". Retention is therefore a product
//! policy, and the sweep here is that policy applied on a timer.
//!
//! The window is `[commitment] result_retention_days`; `0` disables the sweep
//! entirely (keep results forever), the same convention as the UPS retention
//! dial. After a result is removed, Check Commit Result answers 404 - no 410
//! tombstone is kept, since 404 is the status the standard explicitly allows
//! for a deleted result and a tombstone would be state with no reader.

#![forbid(unsafe_code)]

use std::time::{SystemTime, UNIX_EPOCH};

use crate::state::AppState;

/// How long the sweep thread waits between passes.
const SWEEP_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Run one retention pass over the commitment store.
///
/// Returns the number of results removed. The comparison is on each record's
/// own `requested_at`, not the file mtime - a backup restore rewrites mtimes,
/// and retention must follow the answer's age rather than the file's.
fn sweep_once(state: &AppState) -> usize {
    let store = clarus_core::commitment::CommitmentStore::new(state.commitments_dir.clone());
    match clarus_core::commitment::store::sweep_expired(
        &store,
        state.config.commitment.result_retention_days,
        now_secs(),
    ) {
        Ok((removed, failed)) => {
            if failed > 0 {
                crate::clog_warn!("commitment retention: {failed} result(s) could not be deleted");
            }
            removed.len()
        }
        Err(e) => {
            // A sweep failure must not take the server down; the next pass
            // retries, and the log says why nothing was removed.
            crate::clog_warn!("commitment retention sweep failed: {e}");
            0
        }
    }
}

/// Start the retention thread: one pass now, then one every 24 hours.
///
/// The same `std::thread` shape as the UPS retention and push workers - no
/// runtime, no new dependency. When the retention days are `0` the thread is
/// not started at all.
pub fn spawn_commitment_retention(state: AppState) {
    if state.config.commitment.result_retention_days == 0 {
        return;
    }
    std::thread::Builder::new()
        .name("commitment-retention".into())
        .spawn(move || {
            loop {
                let removed = sweep_once(&state);
                if removed > 0 {
                    // Tagged, not bare: the retention sweep is a background
                    // thread and `clog_info!` at INFO is dropped in a release
                    // build unless its tag is whitelisted. A bare INFO line
                    // therefore proved the sweep worked in a debug run and
                    // left no trace in production, which is where the operator
                    // needs to know that results were expired.
                    crate::clog_event!(
                        crate::clog::Level::Info,
                        "COMMITMENT_RETENTION",
                        "",
                        "removed={}",
                        removed
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
            }
        })
        .expect("Failed to spawn commitment retention thread");
}

/// Seconds since the Unix epoch.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_is_a_plausible_epoch_second() {
        // Guards against a unit mix-up (milliseconds would be ~1000x this).
        let now = now_secs();
        assert!(now > 1_700_000_000, "now_secs returned {now}");
        assert!(now < 4_000_000_000, "now_secs returned {now}");
    }
}
