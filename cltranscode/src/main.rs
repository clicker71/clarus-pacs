// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! cltranscode - DICOM anonymization and transcoding proxy.
//!
//! Usage:
//!   cltranscode --version          Print version and exit.
//!   cltranscode --help             Print this help.
//!   cltranscode [CONFIG]           Run with optional config file.

#![forbid(unsafe_code)]

use clarus_adapters::codec::CodecRegistry;
use cltranscode::adapters::{DicomwebInput, DicomwebOutput, DimseScp, DimseScu};
use cltranscode::config::Config;
use cltranscode::health::{AtomicHealth, Health};
use cltranscode::metrics;
use cltranscode::outbox_worker::run_outbox_worker;
use cltranscode::pipeline::Pipeline;
use cltranscode::pipeline_worker::run_pipeline_worker;
use cltranscode::spool::Spool;
use cltranscode::stages::{Anonymizer, Transcoder};
use std::env;
use std::io::Write;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const HELP_TEXT: &str = "\
cltranscode — DICOM anonymization and transcoding proxy.

USAGE:
    cltranscode [OPTIONS] [CONFIG]

OPTIONS:
    --version          Print version information and exit.
    --help             Print this help message and exit.

    If no config file is provided, cltranscode looks for
    cltranscode.conf in the current directory.

EXAMPLES:
    cltranscode --version
    cltranscode --help
    cltranscode ./my-config.conf

REPORTING BUGS:
    https://github.com/clicker71/clarus/issues
";

fn main() {
    let args: Vec<String> = env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("cltranscode v{}", cltranscode::VERSION);
            return;
        }
        Some("--help") | Some("-h") => {
            print!("{HELP_TEXT}");
            return;
        }
        Some(arg) if arg.starts_with('-') => {
            eprintln!("cltranscode: unknown option '{arg}'");
            eprintln!("Try 'cltranscode --help' for more information.");
            process::exit(1);
        }
        _ => {}
    }

    // Parse config
    let config_path = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("cltranscode.conf");
    let config = match Config::from_file(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "cltranscode: failed to load config '{}': {}",
                config_path, e
            );
            process::exit(1);
        }
    };

    // Setup logging: stderr (compact, colored) + daily-rotated file.
    // nginx-way: env_logger for stderr, tracing_appender::rolling::daily
    // for file rotation. A TeeWriter duplicates output to both destinations.
    let level = match config.logging.level.to_lowercase().as_str() {
        "error" => log::LevelFilter::Error,
        "warn" => log::LevelFilter::Warn,
        "info" => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => log::LevelFilter::Info,
    };

    if config.logging.log_dir.is_empty() {
        // No file logging - stderr only (original behavior)
        env_logger::Builder::new()
            .filter_level(level)
            .format(|buf, record| {
                use std::io::Write;
                let now = chrono::Local::now();
                let ts = now.format("%Y-%m-%dT%H:%M:%S%.3f%:z");
                let lvl = record.level();
                let color = match lvl {
                    log::Level::Error => "\x1b[31m",
                    log::Level::Warn => "\x1b[33m",
                    log::Level::Info => "\x1b[32m",
                    log::Level::Debug => "\x1b[36m",
                    log::Level::Trace => "\x1b[35m",
                };
                let reset = "\x1b[0m";
                writeln!(
                    buf,
                    "{} {}{:<5}{} {}: {}",
                    ts,
                    color,
                    lvl,
                    reset,
                    record.target(),
                    record.args()
                )
            })
            .init();
    } else {
        // File + stderr: write to stderr (colored) AND to daily-rotated file (JSON).
        // nginx-way: no framework - just fopen, check date, fprintf.
        let log_dir = std::path::PathBuf::from(&config.logging.log_dir);
        let log_dir_display = log_dir.clone();
        let file_writer = std::sync::Arc::new(std::sync::Mutex::new(open_log_file(&log_dir)));

        let fw = Arc::clone(&file_writer);
        let ld = log_dir.clone();
        env_logger::Builder::new()
            .filter_level(level)
            .format(move |buf, record| {
                let now = chrono::Local::now();
                let ts = now.format("%Y-%m-%dT%H:%M:%S%.3f%:z");
                let lvl = record.level();
                let color = match lvl {
                    log::Level::Error => "\x1b[31m",
                    log::Level::Warn => "\x1b[33m",
                    log::Level::Info => "\x1b[32m",
                    log::Level::Debug => "\x1b[36m",
                    log::Level::Trace => "\x1b[35m",
                };
                let reset = "\x1b[0m";
                // stderr (colored)
                writeln!(
                    buf,
                    "{} {}{:<5}{} {}: {}",
                    ts,
                    color,
                    lvl,
                    reset,
                    record.target(),
                    record.args()
                )?;
                // file (JSON, one object per line - same as Clarus server)
                let mut f = fw.lock().unwrap();
                let today = now.format("%Y-%m-%d").to_string();
                if f.date != today {
                    *f = open_log_file(&ld);
                }
                let _ = writeln!(
                    f.file,
                    r#"{{"ts":"{}","level":"{}","target":"{}","msg":"{}"}}"#,
                    ts,
                    lvl,
                    record.target(),
                    record.args().to_string().replace('"', r#"\""#)
                );
                Ok(())
            })
            .init();
        eprintln!(
            "cltranscode: file logging enabled — {}",
            log_dir_display.display()
        );
    }

    // Now log macros work - both stderr and file (if configured).
    if !config.logging.log_dir.is_empty() {
        log::info!("logging: file output enabled — {}", config.logging.log_dir);
    }

    log::info!("cltranscode v{} starting", cltranscode::VERSION);

    // Initialize metrics uptime clock
    metrics::init_uptime();

    // Determine if pipeline has anonymizer & transcoder stages
    let has_anonymizer = !config.anonymizer.salt.is_empty();
    let has_transcoder = config.transcoding.enabled;

    log::info!(
        "pipeline: anonymizer={} transcoder={}",
        if has_anonymizer { "on" } else { "off" },
        if has_transcoder { "on" } else { "off" },
    );

    // Build codec registry for transcoding (clone for SCP TS negotiation)
    let registry = build_codec_registry(&config.transcoding);
    let scp_registry = registry.clone();

    // Build pipeline stages
    let mut stages: Vec<Box<dyn cltranscode::pipeline::PipelineStage>> = Vec::new();

    if has_anonymizer {
        let profile = match config.anonymizer.profile {
            cltranscode::config::AnonymizerProfile::Basic => {
                cltranscode::stages::AnonymizerProfile::Basic
            }
            cltranscode::config::AnonymizerProfile::RetainLongitudinal => {
                cltranscode::stages::AnonymizerProfile::RetainLongitudinal
            }
            cltranscode::config::AnonymizerProfile::RetainDevice => {
                cltranscode::stages::AnonymizerProfile::RetainDevice
            }
            cltranscode::config::AnonymizerProfile::RetainInstitution => {
                cltranscode::stages::AnonymizerProfile::RetainInstitution
            }
            cltranscode::config::AnonymizerProfile::Custom => {
                cltranscode::stages::AnonymizerProfile::Custom
            }
        };
        let anon = Anonymizer::new(
            profile,
            config.anonymizer.salt.clone(),
            config.input.dimse_ae_title.clone(),
        );
        stages.push(Box::new(anon));
        log::info!(
            "anonymizer: profile={}; salt=***",
            config.anonymizer.profile
        );
    }

    if has_transcoder {
        let mut ts_uid = cltranscode::config::transfer_syntax_names()
            .get(config.transcoding.target_ts.as_str())
            .copied()
            .unwrap_or("1.2.840.10008.1.2.4.70");
        // 31.08: jpegls_near > 0 means near-lossless - the pipeline target
        // becomes 4.81 even if the operator left target_ts = JPEG_LS.
        if config.transcoding.jpegls_near > 0 && config.transcoding.target_ts == "JPEG_LS" {
            ts_uid = "1.2.840.10008.1.2.4.81";
            log::info!(
                "transcoder: jpegls_near={} > 0 - target auto-switched to JPEG-LS Near-Lossless (4.81)",
                config.transcoding.jpegls_near
            );
        }
        let transcoder = Transcoder::new(
            ts_uid.to_string(),
            registry,
            config.input.dimse_ae_title.clone(),
        );
        stages.push(Box::new(transcoder));
        log::info!(
            "transcoder: target={} (uid={})",
            config.transcoding.target_ts,
            ts_uid
        );
    }

    let pipeline = Arc::new(Pipeline::new(stages));

    // Create spool (file-based queue connecting pipeline phases)
    let spool = match Spool::new(Path::new(&config.outbox.dir)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "cltranscode: failed to create spool '{}': {}",
                config.outbox.dir, e
            );
            process::exit(1);
        }
    };
    let spool = Arc::new(spool);
    log::info!("spool: dir={}", config.outbox.dir);

    // Crash recovery: count leftover files
    spool.recover_on_startup();

    // Initialize spool metrics gauges (first reading)
    metrics::update_spool_gauges(&spool);

    // Shared shutdown flag
    let shutdown = Arc::new(AtomicBool::new(false));

    // Periodic spool gauge updater - filesystem poll every 30s.
    // Avoids filesystem I/O on every /metrics scrape.
    {
        let gauge_spool = Arc::clone(&spool);
        let gauge_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            while !gauge_shutdown.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(30));
                metrics::update_spool_gauges(&gauge_spool);
            }
        });
    }

    // Periodic retention cleanup - remove old files from done/.
    // Runs at startup and every retention_hours/2 (min 1 hour).
    {
        let retention_hours = config.outbox.retention_hours;
        let clean_spool = Arc::clone(&spool);
        let clean_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            if retention_hours > 0 {
                // First cleanup at startup
                clean_spool.cleanup_done(retention_hours);
                let interval_secs = (retention_hours as u64 * 3600 / 2).max(3600);
                while !clean_shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(interval_secs));
                    clean_spool.cleanup_done(retention_hours);
                }
            }
        });
    }

    // Create output adapter (wrapped in Arc<Mutex<>> for outbox worker)
    let output_mode = config.output.mode.clone();
    let output: Box<dyn cltranscode::OutputAdapter> = match output_mode.as_str() {
        "dicomweb" => {
            let adapter = DicomwebOutput::new(
                config.output.dicomweb_url.clone(),
                &config.output.dicomweb_auth,
            );
            Box::new(adapter)
        }
        _ => {
            let adapter = DimseScu::new(
                config.output.dimse_ae_title.clone(),
                config.output.dimse_calling_ae.clone(),
                config.output.dimse_host.clone(),
                config.output.dimse_port,
            );
            Box::new(adapter)
        }
    };
    let output = Arc::new(Mutex::new(output));

    // Health gate for outbox worker
    let health = Arc::new(AtomicHealth::new(Health::Green));

    // Spawn C-ECHO health checker (DIMSE output only - no DIMSE to ping for DICOMweb)
    if output_mode != "dicomweb" {
        let health_scu = DimseScu::new(
            config.output.dimse_ae_title.clone(),
            config.output.dimse_calling_ae.clone(),
            config.output.dimse_host.clone(),
            config.output.dimse_port,
        );
        cltranscode::health::spawn_health_checker(
            Arc::new(Mutex::new(health_scu)),
            Arc::clone(&health),
        );
    }

    // Start health/metrics HTTP server
    let health_port = config.health.port;
    metrics::start_metrics_server(health_port, Some(readiness_check));

    let input_is_dimse = config.input.mode != "dicomweb";
    let output_is_dimse = config.output.mode != "dicomweb";
    let mode_code = format!(
        "{}{}",
        if input_is_dimse { 'D' } else { 'W' },
        if output_is_dimse { 'D' } else { 'W' },
    );

    let input_label = if input_is_dimse {
        format!(
            "{}@:{}",
            config.input.dimse_ae_title, config.input.dimse_port
        )
    } else {
        format!(":{}", config.input.dicomweb_port)
    };

    let output_label = if output_is_dimse {
        format!(
            "{}@{}:{}",
            config.output.dimse_ae_title, config.output.dimse_host, config.output.dimse_port
        )
    } else {
        config.output.dicomweb_url.clone()
    };

    log::info!(
        "cltranscode: [{}] {} → {}",
        mode_code,
        input_label,
        output_label,
    );
    log::info!(
        "cltranscode: metrics on port {} (/health + /metrics)",
        health_port
    );

    // Spawn input adapter
    if config.input.mode != "dicomweb" {
        // D->* path: DIMSE SCP
        let _scp = DimseScp::new(
            config.input.dimse_ae_title.clone(),
            config.input.dimse_port,
            Arc::clone(&spool),
            Arc::clone(&shutdown),
            scp_registry,
        )
        .map_err(|e| {
            eprintln!("cltranscode: failed to start DIMSE SCP: {}", e);
            process::exit(1);
        })
        .ok();
    } else {
        // W->* path: STOW-RS HTTP listener -> spool
        let _dw_input = DicomwebInput::new(
            config.input.dicomweb_port,
            Arc::clone(&spool),
            Arc::clone(&shutdown),
            config.outbox.max_pending,
        )
        .map_err(|e| {
            eprintln!("cltranscode: failed to start DICOMweb listener: {}", e);
            process::exit(1);
        })
        .ok();
    }

    // Spawn pipeline worker
    let pw_shutdown = Arc::clone(&shutdown);
    let pw_spool = Arc::clone(&spool);
    let pw_pipeline = Arc::clone(&pipeline);
    let pw_handle = std::thread::spawn(move || {
        run_pipeline_worker(pw_spool, pw_pipeline, pw_shutdown);
    });

    // Spawn outbox worker
    let ow_shutdown = Arc::clone(&shutdown);
    let ow_spool = Arc::clone(&spool);
    let ow_output = Arc::clone(&output);
    let ow_health = Arc::clone(&health);
    let ow_max_retries = config.outbox.max_retries;
    let ow_handle = std::thread::spawn(move || {
        run_outbox_worker(ow_spool, ow_output, ow_health, ow_max_retries, ow_shutdown);
    });

    log::info!("cltranscode: all workers started. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    wait_for_shutdown();

    log::info!("cltranscode: shutting down...");
    shutdown.store(true, Ordering::Relaxed);

    // Wait for workers to finish
    let _ = pw_handle.join();
    let _ = ow_handle.join();

    log::info!("cltranscode: stopped.");
}

/// Wait for Ctrl+C or SIGTERM.
fn wait_for_shutdown() {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let _ = ctrlc::set_handler(move || {
        let _ = tx.send(());
    });
    let _ = rx.recv();
}

/// Daily-rotated log file handle.
///
/// On each write, checks if the date changed - if so, opens a new file.
/// Old files are NOT cleaned up (retention is filesystem-level, not app-level).
struct LogFile {
    file: std::fs::File,
    date: String,
}

/// Open today's log file, creating the directory if needed.
fn open_log_file(log_dir: &std::path::Path) -> LogFile {
    let _ = std::fs::create_dir_all(log_dir);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let path = log_dir.join(format!("cltranscode-{}.log", today));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap_or_else(|e| {
            eprintln!("cltranscode: cannot open log file {:?}: {}", path, e);
            std::process::exit(1);
        });
    LogFile { file, date: today }
}

/// Readiness check: returns true if the system is ready to accept work.
///
/// Currently: always true (liveness = process alive, readiness = always ready).
/// P30 (deferred): check input listener + output reachability.
fn readiness_check() -> bool {
    true
}

/// Build a codec registry for the configured JPEG-LS profile (31.08).
///
/// The clinic selects the encode profile and NEAR in cltranscode.conf:
/// - `jpegls_profile = t87` (default) + `jpegls_near = 0` ->
///   JpeglsLosslessCodec (strict T87, 4.80 - Weasis CharLS renders it);
/// - `jpegls_profile = t87` + `jpegls_near > 0` ->
///   JpeglsNearLosslessCodec (4.81 near-lossless, T87);
/// - `jpegls_profile = legacygo` (only near=0, validated in config) ->
///   JpeglsLegacyGoCodec (frozen 1.0.0 bytes, 4.80).
///
/// The DIMSE SCP proposes whatever the registry supports, so exactly the
/// SELECTED syntax is offered (4.80 at near=0, 4.81 at near>0).
fn build_codec_registry(tc: &cltranscode::config::TranscodingConfig) -> CodecRegistry {
    let mut registry = CodecRegistry::new();

    // Register uncompressed codecs (always available)
    registry.register(Box::new(
        clarus_adapters::codec::explicit_vr_le::ExplicitVrLeCodec,
    ));
    registry.register(Box::new(
        clarus_adapters::codec::explicit_vr_be::BigEndianCodec,
    ));
    registry.register(Box::new(
        clarus_adapters::codec::implicit_vr_le::ImplicitVrLeCodec,
    ));
    registry.register(Box::new(clarus_adapters::codec::rle::RleCodec));

    // JPEG-LS - the SELECTED profile/near (single active backend)
    use clarus_adapters::codec::jpegls::{
        JpeglsLegacyGoCodec, JpeglsLosslessCodec, JpeglsNearLosslessCodec,
    };
    if tc.jpegls_profile == "legacygo" {
        registry.register(Box::new(JpeglsLegacyGoCodec));
        log::info!("jpegls: profile=legacygo (frozen 1.0.0 bytes), near=0");
    } else if tc.jpegls_near == 0 {
        registry.register(Box::new(JpeglsLosslessCodec));
        log::info!("jpegls: profile=t87 (strict ITU-T T.87), near=0");
    } else {
        registry.register(Box::new(JpeglsNearLosslessCodec::new(tc.jpegls_near)));
        log::info!(
            "jpegls: profile=t87, near={} (4.81 near-lossless)",
            tc.jpegls_near
        );
    }

    #[cfg(feature = "transcode")]
    {
        registry.register(Box::new(
            clarus_adapters::codec::jpeg_baseline::JpegBaselineCodec,
        ));
        registry.register(Box::new(
            clarus_adapters::codec::jpeg_lossless::JpegLosslessCodec,
        ));
        registry.register(Box::new(
            clarus_adapters::codec::jpeg_lossless::JpegLosslessSv1Codec,
        ));
        registry.register(Box::new(
            clarus_adapters::codec::jpeg2000_native::Jpeg2000NativeLosslessCodec,
        ));
    }

    registry
}

// -- JPEG-LS profile: registry selection tests ---------------------------

#[cfg(test)]
mod tests {
    use super::build_codec_registry;
    use cltranscode::config::TranscodingConfig;

    const TS_480: &str = "1.2.840.10008.1.2.4.80";
    const TS_481: &str = "1.2.840.10008.1.2.4.81";

    fn tc(profile: &str, near: u8) -> TranscodingConfig {
        TranscodingConfig {
            enabled: true,
            target_ts: "JPEG_LS".into(),
            jpegls_profile: profile.into(),
            jpegls_near: near,
        }
    }

    #[test]
    fn registry_t87_near0_proposes_480_only() {
        let r = build_codec_registry(&tc("t87", 0));
        assert!(r.supports(TS_480), "4.80 must be registered (lossless)");
        assert!(!r.supports(TS_481), "4.81 must NOT be registered at near=0");
    }

    #[test]
    fn registry_t87_near4_proposes_481() {
        let r = build_codec_registry(&tc("t87", 4));
        assert!(!r.supports(TS_480), "4.80 must NOT be registered at near>0");
        assert!(
            r.supports(TS_481),
            "4.81 must be registered (near-lossless)"
        );
    }

    #[test]
    fn registry_legacygo_proposes_480() {
        let r = build_codec_registry(&tc("legacygo", 0));
        assert!(r.supports(TS_480), "LegacyGo codec carries 4.80");
        assert!(!r.supports(TS_481));
    }
}
