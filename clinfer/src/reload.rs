// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Configuration reload.
//!
//! The file is re-read when its modification time moves, no more often
//! than every [`CHECK_INTERVAL`]. Polling a timestamp is the whole
//! mechanism: an inotify-style watch would add a dependency and a set of
//! platform behaviours to a file that administrators edit a few times a
//! year.
//!
//! A reload that fails to parse KEEPS the running configuration and warns.
//! The alternative - dropping to a default or refusing to serve - would
//! turn a typo in an editor into an outage of the whole sidecar, and the
//! running configuration is a valid one that was already accepted at
//! startup.

use crate::conf::Conf;
use crate::value::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How often the modification time is looked at.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// The configuration currently in force, shared with the workers.
#[derive(Debug)]
pub struct Reloader {
    path: PathBuf,
    current: Arc<Conf>,
    /// Timestamp of the file content that produced `current`.
    stamp: Option<SystemTime>,
    /// When the timestamp was last read.
    checked_at: Option<SystemTime>,
    /// Incremented on every successful reload, so a worker can notice that
    /// its rules changed without comparing configurations.
    generation: u64,
}

impl Reloader {
    /// Load the file and take its modification time as the starting stamp.
    pub fn new(path: &Path) -> Result<Self> {
        let conf = crate::conf::load(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            current: Arc::new(conf),
            stamp: modified_at(path),
            checked_at: None,
            generation: 0,
        })
    }

    #[must_use]
    pub fn current(&self) -> &Arc<Conf> {
        &self.current
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bumped by every accepted reload.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Re-read the file if its timestamp moved. `Ok(None)` means nothing
    /// changed; `Ok(Some(generation))` means the configuration was
    /// replaced; `Err` carries a parse failure and leaves the running
    /// configuration untouched.
    ///
    /// `now` is passed in rather than read inside, so the cadence can be
    /// tested without waiting.
    pub fn poll(&mut self, now: SystemTime) -> Result<Option<u64>> {
        if let Some(checked_at) = self.checked_at
            && let Ok(elapsed) = now.duration_since(checked_at)
            && elapsed < CHECK_INTERVAL
        {
            return Ok(None);
        }
        self.checked_at = Some(now);

        let stamp = modified_at(&self.path);
        if stamp == self.stamp {
            return Ok(None);
        }

        let conf = crate::conf::load(&self.path)?;
        self.current = Arc::new(conf);
        self.stamp = stamp;
        self.generation += 1;
        Ok(Some(self.generation))
    }
}

/// On Windows the modification time moves on write. A file replaced by a
/// rename may briefly have no timestamp at all; `None` then differs from
/// the remembered value and triggers one reload attempt, which is the
/// desired direction to fail in.
fn modified_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const MINIMAL: &str = "\
server:
  base_url: \"http://127.0.0.1:8024/dicomweb\"
  identity_user: \"clinfer\"
engine:
  cache_dir: \"./cache\"
models:
  m:
    script: \"proc.py\"
rules:
  - match:
      modality: [DX]
    model: \"m\"
    result:
      kind: pr
      color: \"#FF3B30\"
default_rule: cancel
";

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(content: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "clinfer-reload-{}-{:?}.conf",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            let mut file = std::fs::File::create(&path).expect("create");
            file.write_all(content.as_bytes()).expect("write");
            Self(path)
        }

        fn rewrite(&self, content: &str) {
            let mut file = std::fs::File::create(&self.0).expect("reopen");
            file.write_all(content.as_bytes()).expect("write");
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn an_unmoved_timestamp_is_not_a_reload() {
        let file = TempFile::new(MINIMAL);
        let mut reloader = Reloader::new(&file.0).expect("loads");
        let t0 = SystemTime::now();
        assert_eq!(reloader.poll(t0).expect("poll"), None);
        assert_eq!(reloader.generation(), 0);
    }

    #[test]
    fn the_interval_bounds_how_often_the_timestamp_is_read() {
        let file = TempFile::new(MINIMAL);
        let mut reloader = Reloader::new(&file.0).expect("loads");
        let t0 = SystemTime::now();
        reloader.poll(t0).expect("first poll sets the clock");
        let within = t0 + CHECK_INTERVAL / 2;
        assert_eq!(reloader.poll(within).expect("poll"), None);
    }

    #[test]
    fn a_changed_file_is_reloaded() {
        let file = TempFile::new(MINIMAL);
        let mut reloader = Reloader::new(&file.0).expect("loads");
        let t0 = SystemTime::now();
        reloader.poll(t0).expect("polls");

        // The default rule is the cheapest visible field to move.
        let changed = MINIMAL.replace("default_rule: cancel", "default_rule: off");
        file.rewrite(&changed);
        // Force a different timestamp without sleeping on filesystem
        // resolution.
        let later = t0 + CHECK_INTERVAL * 2;
        let generation = reloader.poll(later).expect("reloads");
        assert_eq!(generation, Some(1));
        assert_eq!(
            reloader.current().default_rule,
            crate::conf::DefaultRule::Off
        );
    }

    #[test]
    fn an_invalid_file_keeps_the_running_configuration() {
        let file = TempFile::new(MINIMAL);
        let mut reloader = Reloader::new(&file.0).expect("loads");
        let t0 = SystemTime::now();
        reloader.poll(t0).expect("polls");

        file.rewrite(&format!("{MINIMAL}unknown_top_level: 1\n"));
        let later = t0 + CHECK_INTERVAL * 2;
        let err = reloader.poll(later).expect_err("refuses");
        assert!(err.to_string().contains("unknown_top_level"), "{err}");
        // The running configuration is the one that was accepted.
        assert_eq!(
            reloader.current().default_rule,
            crate::conf::DefaultRule::Cancel
        );
        assert_eq!(reloader.generation(), 0);
    }
}
