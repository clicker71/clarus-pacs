// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! File-based spool queue - connects pipeline phases via the filesystem.
//!
//! nginx-way: filesystem = database. Directory = state.
//! Four dirs: raw/ (unprocessed), cooked/ (processed), done/ (delivered), failed/ (gave up).
//!
//! `fs::rename` is atomic - no half-done states.

#![deny(unsafe_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    pub fn new(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir.join("raw"))?;
        fs::create_dir_all(dir.join("cooked"))?;
        fs::create_dir_all(dir.join("done"))?;
        fs::create_dir_all(dir.join("failed"))?;
        // Clean up stale .tmp files from crashed atomic writes
        for sub in &["raw", "cooked", "done", "failed"] {
            if let Ok(entries) = fs::read_dir(dir.join(sub)) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().map(|e| e == "tmp").unwrap_or(false) {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    pub fn write_raw(&self, bytes: &[u8]) -> io::Result<String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let unique = clarus_adapters::unique_id();
        let stem = format!("{}_{}", ts, unique);
        let path = self.dir.join("raw").join(format!("{}.dcm", stem));
        // Atomic write: write to temp, then rename.
        // Prevents pipeline worker from picking up a partially-written file.
        let tmp = self.dir.join("raw").join(format!(".{}.tmp", stem));
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(stem)
    }

    pub fn write_cooked(&self, stem: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.dir.join("cooked").join(format!("{}.dcm", stem));
        let tmp = self.dir.join("cooked").join(format!(".{}.tmp", stem));
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn list_raw(&self) -> io::Result<Vec<PathBuf>> {
        let mut entries: Vec<PathBuf> = fs::read_dir(self.dir.join("raw"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "dcm").unwrap_or(false))
            .collect();
        entries.sort();
        Ok(entries)
    }

    pub fn list_cooked(&self) -> io::Result<Vec<PathBuf>> {
        let mut entries: Vec<PathBuf> = fs::read_dir(self.dir.join("cooked"))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "dcm").unwrap_or(false))
            .collect();
        entries.sort();
        Ok(entries)
    }

    pub fn remove_raw(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)?;
        Ok(())
    }

    pub fn mark_done(&self, path: &Path) -> io::Result<()> {
        let file_name = path.file_name().unwrap();
        let dst = self.dir.join("done").join(file_name);
        fs::rename(path, &dst)?;
        Ok(())
    }

    pub fn mark_failed(&self, path: &Path) -> io::Result<()> {
        let file_name = path.file_name().unwrap();
        let dst = self.dir.join("failed").join(file_name);
        fs::rename(path, &dst)?;
        Ok(())
    }

    pub fn count_raw(&self) -> u64 {
        count_files(&self.dir.join("raw"))
    }

    pub fn count_cooked(&self) -> u64 {
        count_files(&self.dir.join("cooked"))
    }

    pub fn count_done(&self) -> u64 {
        count_files(&self.dir.join("done"))
    }

    pub fn count_failed(&self) -> u64 {
        count_files(&self.dir.join("failed"))
    }

    /// Remove files from `done/` older than `retention_hours`.
    ///
    /// Returns (removed, remaining). `retention_hours = 0` means no cleanup.
    pub fn cleanup_done(&self, retention_hours: u32) -> (u64, u64) {
        if retention_hours == 0 {
            let count = self.count_done();
            return (0, count);
        }
        let cutoff = std::time::SystemTime::now()
            - std::time::Duration::from_secs(retention_hours as u64 * 3600);
        let mut removed: u64 = 0;
        let mut remaining: u64 = 0;
        let done_dir = self.dir.join("done");
        if let Ok(iter) = fs::read_dir(&done_dir) {
            for entry in iter.filter_map(|e| e.ok()) {
                match entry.metadata() {
                    Ok(meta) if meta.modified().is_ok_and(|t| t < cutoff) => {
                        let path = entry.path();
                        if fs::remove_file(&path).is_ok() {
                            removed += 1;
                        } else {
                            remaining += 1;
                        }
                    }
                    _ => remaining += 1,
                }
            }
        }
        if removed > 0 {
            log::info!(
                "spool: cleaned {} files from done/ ({} remaining, retention={}h)",
                removed,
                remaining,
                retention_hours
            );
        }
        (removed, remaining)
    }

    pub fn recover_on_startup(&self) -> (u64, u64) {
        let raw = self.count_raw();
        let cooked = self.count_cooked();
        log::info!(
            "spool: recovery found {} raw + {} cooked files",
            raw,
            cooked
        );
        (raw, cooked)
    }
}

fn count_files(dir: &Path) -> u64 {
    match fs::read_dir(dir) {
        Ok(iter) => iter.filter_map(|e| e.ok()).count() as u64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spool_write_and_read_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::new(tmp.path()).unwrap();

        let stem = spool.write_raw(b"hello").unwrap();
        assert!(!stem.is_empty());

        let raw_files = spool.list_raw().unwrap();
        assert_eq!(raw_files.len(), 1);
        assert!(raw_files[0].to_str().unwrap().contains(&stem));

        let data = fs::read(&raw_files[0]).unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn test_spool_write_cooked_and_mark_done() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::new(tmp.path()).unwrap();

        let stem = spool.write_raw(b"raw_data").unwrap();
        spool.write_cooked(&stem, b"cooked_data").unwrap();

        let cooked_files = spool.list_cooked().unwrap();
        assert_eq!(cooked_files.len(), 1);

        spool.mark_done(&cooked_files[0]).unwrap();
        assert_eq!(spool.list_cooked().unwrap().len(), 0);

        let done_dir = tmp.path().join("done");
        let done_entries: Vec<_> = fs::read_dir(&done_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(done_entries.len(), 1);
    }

    #[test]
    fn test_spool_mark_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::new(tmp.path()).unwrap();

        let stem = spool.write_raw(b"data").unwrap();
        spool.write_cooked(&stem, b"cooked").unwrap();

        let cooked = spool.list_cooked().unwrap();
        spool.mark_failed(&cooked[0]).unwrap();

        assert_eq!(spool.list_cooked().unwrap().len(), 0);
        let failed_entries: Vec<_> = fs::read_dir(tmp.path().join("failed"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(failed_entries.len(), 1);
    }

    #[test]
    fn test_spool_count() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::new(tmp.path()).unwrap();

        assert_eq!(spool.count_raw(), 0);
        assert_eq!(spool.count_cooked(), 0);

        spool.write_raw(b"a").unwrap();
        spool.write_raw(b"b").unwrap();
        assert_eq!(spool.count_raw(), 2);

        let stem = spool.write_raw(b"c").unwrap();
        spool.write_cooked(&stem, b"d").unwrap();
        assert_eq!(spool.count_cooked(), 1);

        spool.remove_raw(&spool.list_raw().unwrap()[0]).unwrap();
        assert_eq!(spool.count_raw(), 2);
    }

    #[test]
    fn test_spool_recover_on_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = Spool::new(tmp.path()).unwrap();

        spool.write_raw(b"x").unwrap();
        spool.write_raw(b"y").unwrap();
        let stem = spool.write_raw(b"z").unwrap();
        spool.write_cooked(&stem, b"w").unwrap();

        // Re-open same dir
        let spool2 = Spool::new(tmp.path()).unwrap();
        let (raw, cooked) = spool2.recover_on_startup();
        assert_eq!(raw, 3);
        assert_eq!(cooked, 1);
    }
}
