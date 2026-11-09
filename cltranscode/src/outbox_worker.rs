// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Outbox worker - reads cooked/ spool, sends via output adapter, marks done/failed.
//!
//! Single thread. Exponential backoff on retry. Respects health gate.

#![deny(unsafe_code)]

use crate::adapters::AdapterError;
use crate::adapters::OutputAdapter;
use crate::health::{AtomicHealth, Health};
use crate::spool::Spool;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

pub fn run_outbox_worker(
    spool: Arc<Spool>,
    output: Arc<Mutex<Box<dyn OutputAdapter>>>,
    health: Arc<AtomicHealth>,
    max_retries: u32,
    shutdown: Arc<AtomicBool>,
) {
    log::info!("outbox_worker: started (max_retries={})", max_retries);
    while !shutdown.load(Ordering::Relaxed) {
        if health.load() == Health::Black {
            sleep(Duration::from_secs(5));
            continue;
        }

        let files = match spool.list_cooked() {
            Ok(f) => f,
            Err(e) => {
                log::error!("outbox_worker: list_cooked error: {}", e);
                sleep(Duration::from_millis(100));
                continue;
            }
        };

        for f in files {
            let bytes = match fs::read(&f) {
                Ok(b) => b,
                Err(e) => {
                    log::error!("outbox_worker: read {:?} error: {}", f, e);
                    continue;
                }
            };

            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
            let mut retries: u32 = 0;

            loop {
                match output.lock().unwrap().send(bytes.as_slice()) {
                    Ok(()) => {
                        if let Err(e) = spool.mark_done(&f) {
                            log::error!("outbox_worker: mark_done {} error: {}", stem, e);
                        }
                        log::info!("outbox_worker: delivered {}", stem);
                        break;
                    }
                    Err(ref e) if is_fatal_status(e) => {
                        log::error!("outbox_worker: {} failed with fatal status: {}", stem, e);
                        if let Err(e2) = spool.mark_failed(&f) {
                            log::error!("outbox_worker: mark_failed {} error: {}", stem, e2);
                        }
                        break;
                    }
                    Err(e) if retries < max_retries => {
                        retries += 1;
                        let backoff_secs = 2u64.pow(retries).min(120);
                        log::warn!(
                            "outbox_worker: {} attempt {}/{} failed: {}. Retrying in {}s",
                            stem,
                            retries,
                            max_retries,
                            e,
                            backoff_secs
                        );
                        sleep(Duration::from_secs(backoff_secs));
                    }
                    Err(e) => {
                        log::error!(
                            "outbox_worker: {} failed after {} retries: {}",
                            stem,
                            max_retries,
                            e
                        );
                        if let Err(e2) = spool.mark_failed(&f) {
                            log::error!("outbox_worker: mark_failed {} error: {}", stem, e2);
                        }
                        break;
                    }
                }
            }
        }

        sleep(Duration::from_millis(100));
    }
    log::info!("outbox_worker: shutting down");
}

/// Check if an AdapterError represents a fatal (non-retryable) failure.
///
/// DIMSE: C000 (Cannot understand), A700 (Out of resources), A900 (SOP class).
/// DICOMweb: HTTP 4xx - retry is pointless; data or request is malformed.
/// Transient errors (HTTP 5xx, connection refused, timeout) are retried.
fn is_fatal_status(err: &AdapterError) -> bool {
    let msg = err.to_string();
    msg.contains("status C000")
        || msg.contains("status A700")
        || msg.contains("status A900")
        || msg.contains("STOW-RS: HTTP 4")
        || msg.contains("server unreachable")
}
