// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! File-based outbox with retry and exponential backoff.
//!
//! nginx-way: filesystem = database. Status = directory name.
//!
//! Directory structure:
//! ```text
//! {outbox_dir}/
//!   pending/   - files waiting to be sent
//!   done/      - successfully delivered
//!   failed/    - gave up after max_retries
//! ```

#![deny(unsafe_code)]

use crate::adapters::{AdapterError, OutputAdapter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// File-based outbox with retry logic.
pub struct Outbox {
    dir: PathBuf,
    max_retries: u32,
}

impl Outbox {
    /// Create a new outbox, creating the directory structure if needed.
    pub fn new(dir: impl AsRef<Path>, max_retries: u32) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(dir.join("pending"))?;
        fs::create_dir_all(dir.join("done"))?;
        fs::create_dir_all(dir.join("failed"))?;
        Ok(Self { dir, max_retries })
    }

    /// Enqueue bytes for later delivery. Returns the file stem (unique ID).
    pub fn enqueue(&self, bytes: &[u8]) -> io::Result<String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let unique = clarus_adapters::unique_id();
        let stem = format!("{}_{}", ts, unique);
        let path = self.dir.join("pending").join(format!("{}.dcm", stem));
        fs::write(&path, bytes)?;
        log::info!("outbox: enqueued {} ({} bytes)", stem, bytes.len());
        Ok(stem)
    }

    /// Count files currently in pending/.
    pub fn pending_count(&self) -> u64 {
        let pending_dir = self.dir.join("pending");
        match fs::read_dir(&pending_dir) {
            Ok(iter) => iter.filter_map(|e| e.ok()).count() as u64,
            Err(_) => 0,
        }
    }

    /// Process all pending files through the output adapter.
    ///
    /// Returns the number of successfully delivered files.
    pub fn process_pending(&self, output: &mut dyn OutputAdapter) -> Result<usize, AdapterError> {
        let pending_dir = self.dir.join("pending");
        let mut delivered = 0usize;

        let mut entries: Vec<_> = match fs::read_dir(&pending_dir) {
            Ok(iter) => iter.filter_map(|e| e.ok()).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(AdapterError::Io(e)),
        };
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            let file_name = entry.file_name();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");

            log::info!("outbox: processing {}", stem);

            let bytes = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("outbox: cannot read {}: {}", stem, e);
                    continue;
                }
            };

            match self.send_with_retry(output, &bytes, stem) {
                Ok(()) => {
                    self.move_to(&path, "done", &file_name);
                    delivered += 1;
                    log::info!("outbox: {} → done", stem);
                }
                Err(e) => {
                    log::error!(
                        "outbox: {} failed after {} retries: {}",
                        stem,
                        self.max_retries,
                        e
                    );
                    self.move_to(&path, "failed", &file_name);
                    crate::metrics::record_outbox_failed();
                }
            }
        }

        Ok(delivered)
    }

    /// Send with exponential backoff: 1s -> 2s -> 4s -> ... -> max 60s.
    fn send_with_retry(
        &self,
        output: &mut dyn OutputAdapter,
        bytes: &[u8],
        stem: &str,
    ) -> Result<(), AdapterError> {
        let mut attempt = 0u32;
        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);

        loop {
            match output.send(bytes) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    attempt += 1;
                    if attempt >= self.max_retries {
                        return Err(e);
                    }
                    log::warn!(
                        "outbox: {} attempt {}/{} failed: {}. Retrying in {:?}",
                        stem,
                        attempt,
                        self.max_retries,
                        e,
                        backoff
                    );
                    thread::sleep(backoff);
                    backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                }
            }
        }
    }

    fn move_to(&self, src: &Path, subdir: &str, file_name: &std::ffi::OsString) {
        let dst = self.dir.join(subdir).join(file_name);
        if let Err(e) = fs::rename(src, &dst) {
            log::error!("outbox: cannot move to {}: {}", subdir, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AdapterError;
    use std::cell::RefCell;

    struct MockOutput {
        failures: RefCell<u32>,
        sent: RefCell<Vec<Vec<u8>>>,
    }

    impl MockOutput {
        fn new(failures_before_ok: u32) -> Self {
            Self {
                failures: RefCell::new(failures_before_ok),
                sent: RefCell::new(Vec::new()),
            }
        }
    }

    impl OutputAdapter for MockOutput {
        fn send(&mut self, bytes: &[u8]) -> Result<(), AdapterError> {
            let mut remaining = self.failures.borrow_mut();
            if *remaining > 0 {
                *remaining -= 1;
                Err(AdapterError::Protocol("mock failure".into()))
            } else {
                self.sent.borrow_mut().push(bytes.to_vec());
                Ok(())
            }
        }
    }

    #[test]
    fn enqueue_and_process_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path(), 3).unwrap();

        outbox.enqueue(b"hello").unwrap();
        let mut output = MockOutput::new(0);
        let delivered = outbox.process_pending(&mut output).unwrap();
        assert_eq!(delivered, 1);
        assert_eq!(output.sent.borrow().len(), 1);
        assert_eq!(output.sent.borrow()[0], b"hello");

        // Check pending is empty, done has the file
        assert!(
            tmp.path()
                .join("pending")
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
        assert!(tmp.path().join("done").read_dir().unwrap().next().is_some());
    }

    #[test]
    fn retry_with_backoff_eventually_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path(), 3).unwrap();

        outbox.enqueue(b"retry-me").unwrap();
        // Fail 2 times, succeed on 3rd
        let mut output = MockOutput::new(2);
        let delivered = outbox.process_pending(&mut output).unwrap();
        assert_eq!(delivered, 1);
        assert_eq!(output.sent.borrow().len(), 1);
    }

    #[test]
    fn max_retries_exceeded_moves_to_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path(), 2).unwrap();

        outbox.enqueue(b"doomed").unwrap();
        // Always fail
        let mut output = MockOutput::new(999);
        let delivered = outbox.process_pending(&mut output).unwrap();
        assert_eq!(delivered, 0);
        // Should be in failed/
        assert!(
            tmp.path()
                .join("pending")
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            tmp.path()
                .join("failed")
                .read_dir()
                .unwrap()
                .next()
                .is_some()
        );
    }

    #[test]
    fn empty_pending_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(tmp.path(), 3).unwrap();
        let mut output = MockOutput::new(0);
        let delivered = outbox.process_pending(&mut output).unwrap();
        assert_eq!(delivered, 0);
    }
}
