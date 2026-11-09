// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Pipeline worker - reads raw/ spool, runs pipeline, writes cooked/.
//!
//! Single thread. Polls raw/ every 100ms. No mpsc - spool on disk connects phases.

#![deny(unsafe_code)]

use crate::pipeline::Pipeline;
use crate::spool::Spool;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

pub fn run_pipeline_worker(spool: Arc<Spool>, pipeline: Arc<Pipeline>, shutdown: Arc<AtomicBool>) {
    log::info!("pipeline_worker: started");
    while !shutdown.load(Ordering::Relaxed) {
        let files = match spool.list_raw() {
            Ok(f) => f,
            Err(e) => {
                log::error!("pipeline_worker: list_raw error: {}", e);
                sleep(Duration::from_millis(100));
                continue;
            }
        };

        for f in files {
            let bytes = match fs::read(&f) {
                Ok(b) => b,
                Err(e) => {
                    log::error!("pipeline_worker: read {:?} error: {}", f, e);
                    continue;
                }
            };

            let processed = match pipeline.run(bytes) {
                Ok(p) => p,
                Err(e) => {
                    log::error!("pipeline_worker: pipeline error on {:?}: {}", f, e);
                    let _ = spool.remove_raw(&f);
                    continue;
                }
            };
            // Wrap in proper DICOM Part 10 format if needed.
            // Transcoded images already have preamble + DICM + meta-header.
            // Non-pixel objects skip transcode and arrive as raw DIMSE data.
            let processed = if processed.len() >= 132 && &processed[128..132] == b"DICM" {
                processed // Already proper Part 10 -- don't touch
            } else {
                crate::dicom_utils::wrap_part10(crate::dicom_utils::strip_group_0000(processed))
            };

            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
            if let Err(e) = spool.write_cooked(stem, &processed) {
                log::error!("pipeline_worker: write_cooked {} error: {}", stem, e);
                continue;
            }

            if let Err(e) = spool.remove_raw(&f) {
                log::error!("pipeline_worker: remove_raw {:?} error: {}", f, e);
            }

            log::info!(
                "pipeline_worker: processed {} ({} bytes)",
                stem,
                processed.len()
            );
        }

        sleep(Duration::from_millis(100));
    }
    log::info!("pipeline_worker: shutting down");
}
