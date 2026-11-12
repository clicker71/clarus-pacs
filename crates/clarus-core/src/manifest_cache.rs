// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! In-process decoded-manifest cache for the WADO read path.
//! The cache only decodes the manifest.
//!
//! One `RwLock<HashMap>` with per-study slots carrying a monotonic version
//! and an optional cached entry. The discipline (and the ONLY discipline that
//! keeps the invariant "entry present => entry == disk content"):
//!
//! - every manifest mutation calls [`begin_write`] inside the manifest lease
//!   AFTER the tombstone guard and BEFORE touching the file; a successful
//!   A9-guarded commit calls [`end_write`];
//! - the read side is [`get_or_load`]: hit = `Arc::clone` (0 alloc), miss =
//!   decode from disk, insert ONLY if the slot version did not move during
//!   the decode AND the slot is still empty (a commit raced us);
//! - failed writes never `end_write` (no entry - the next read re-decodes).
//!
//! LRU bounds: [`MAX_ENTRIES`] studies OR [`BYTE_BUDGET`] summed ENCODED
//! bytes (on-disk file sizes, not decoded heap). Eviction scans by
//! `last_use` under the write lock on insert; a single oversized entry is
//! always kept.
//!
//! This module is core-only: no server features (metrics/clog). The
//! corrupt branch lives in the server wrapper
//! (`handlers/wado/helpers.rs::load_manifest`), which maps the
//! [`ManifestReadError`] of a failed decode - the cache stores only
//! successful decodes.

use crate::manifest::{Manifest, ManifestReadError, read_manifest};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// LRU bound: maximum number of cached studies.
pub const MAX_ENTRIES: usize = 64;
/// LRU bound: maximum summed ENCODED bytes (manifest file sizes on disk).
pub const BYTE_BUDGET: u64 = 8 * 1024 * 1024;

/// Bound on the map's SLOT count (occupied + empty).
///
/// Eviction clears a slot's entry and keeps the slot, because the slot's `v`
/// is the version guard a writer in flight holds. That is correct and must
/// stay. What it does not do is bound the MAP: every study UID ever read
/// leaves a slot behind, so an archive with a million studies grows this
/// HashMap by roughly a hundred bytes per UID and never shrinks. An empty
/// slot is only worth keeping while a writer can still be holding its guard,
/// so the bound is set well above [`MAX_ENTRIES`] - eight times the entry
/// budget leaves room for a burst of evictions plus the writers that were
/// in flight during them - and the slots dropped are the least recently
/// used ones with no entry, which nobody can be waiting on for content.
pub const MAX_SLOTS: usize = MAX_ENTRIES * 8;

/// One cached study: decoded manifest + bookkeeping.
struct ArcEntry {
    manifest: Arc<Manifest>,
    encoded_len: u64,
    /// The manifests dir this entry was decoded from. The cache is a
    /// process-global static while multiple servers (each with its own
    /// manifest dir) can live in one process (tests, future multi-tenant
    /// embedding): a hit must belong to the ASKING dir, or the entry is
    /// treated as a miss. PathBuf equality allocates nothing, so the gated
    /// hit path stays 0-alloc.
    manifests_dir: PathBuf,
    /// Last-use ticket (monotonic process counter; order is what eviction
    /// needs, a wall clock would only add drift).
    last_use: AtomicU64,
}

/// Per-study slot: monotonic version + optional entry.
struct CacheSlot {
    v: u64,
    entry: Option<ArcEntry>,
    /// When this slot was last held live (ticket of its last entry).
    ///
    /// An EMPTY slot has no `ArcEntry` to read a `last_use` from, and the
    /// slot bound has to pick which empty slots to drop. Recording the
    /// ticket on the slot itself means "least recently used" is answerable
    /// for both kinds without a second pass over the entries.
    last_live: u64,
    /// Writers that called `begin_write` and have not yet `end_write`d.
    /// Never evicted while > 0: the slot holds their version guard.
    writers: u32,
    /// Readers that snapshotted this slot's version and are still decoding.
    ///
    /// `v0` alone is not a generation identity: an evicted slot FREES the
    /// version, and a new slot for the same study starts again at 0, so a
    /// reader that paused after snapshotting `v0` can resume and match a
    /// generation it never saw. Counting the reader keeps the slot - and its
    /// version - alive until the miss resolves.
    readers: u32,
}

static CACHE: std::sync::OnceLock<RwLock<HashMap<String, CacheSlot>>> = std::sync::OnceLock::new();
/// Monotonic last-use ticket source. 0 is reserved for "never used".
static TICKET: AtomicU64 = AtomicU64::new(1);

fn cache() -> &'static RwLock<HashMap<String, CacheSlot>> {
    // First access initializes; afterwards this is a branch + an already-set
    // atomic load (0 alloc on the gated hit path).
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn read_lock() -> std::sync::RwLockReadGuard<'static, HashMap<String, CacheSlot>> {
    cache().read().unwrap_or_else(|p| p.into_inner())
}

fn write_lock() -> std::sync::RwLockWriteGuard<'static, HashMap<String, CacheSlot>> {
    cache().write().unwrap_or_else(|p| p.into_inner())
}

fn touch() -> u64 {
    TICKET.fetch_add(1, Ordering::Relaxed)
}

/// Writer entry point: bump the slot version and TAKE the cached entry.
///
/// Must be called by every manifest writer inside the manifest lease, after
/// the tombstone guard, before touching the manifest file. Returns the new
/// version and the previously cached manifest (if any) - the entry is
/// removed so a concurrent reader can never be served pre-mutation content.
pub fn begin_write(study: &str) -> (u64, Option<Arc<Manifest>>) {
    let mut cache = write_lock();
    let stamp = touch();
    let slot = cache.entry(study.to_string()).or_insert(CacheSlot {
        v: 0,
        entry: None,
        last_live: stamp,
        writers: 0,
        readers: 0,
    });
    slot.v += 1;
    slot.writers += 1;
    // A writer that runs for a long time must not look like the coldest slot:
    // the slot bound orders by this ticket, and a slot created before a burst
    // of unrelated reads would otherwise be the first one dropped.
    slot.last_live = stamp;
    (slot.v, slot.entry.take().map(|e| e.manifest))
}

/// Writer completion: install the post-commit manifest.
///
/// Called ONLY after a successful A9-guarded commit. Installs the entry only
/// if the slot version is still `v` (a newer writer began in the meantime -
/// the disk may already carry its content, so ours would be stale). A slot
/// that disappeared (LRU-evicted) is left absent: the next read re-decodes.
///
/// The in-flight count is released on BOTH paths: a losing writer must not
/// leave its count behind, or the slot could never be reclaimed.
pub fn end_write(
    study: &str,
    v: u64,
    manifest: Arc<Manifest>,
    encoded_len: u64,
    manifests_dir: &Path,
) {
    let mut cache = write_lock();
    let Some(slot) = cache.get_mut(study) else {
        return;
    };
    // Release the count on BOTH paths: a losing writer must not leak it.
    slot.writers = slot.writers.saturating_sub(1);
    if slot.v != v {
        return;
    }
    let stamp = touch();
    slot.last_live = stamp;
    slot.entry = Some(ArcEntry {
        manifest,
        encoded_len,
        manifests_dir: manifests_dir.to_path_buf(),
        last_use: AtomicU64::new(stamp),
    });
    evict_lru_locked(&mut cache, study);
}

/// Read path: return a cached `Arc<Manifest>` for `study`.
///
/// Hit: `Arc::clone` + a Relaxed last-use store - zero allocations (pinned
/// by the `ap101_manifest_cache` gate). Miss: decode from disk, then insert
/// ONLY if the slot version is unchanged since the decode started AND the
/// slot is still empty; otherwise discard (a writer committed during our
/// decode - returning the decoded pre-commit content is exactly what a
/// reader that opened the file before the atomic rename would have seen).
pub fn get_or_load(manifests_dir: &Path, study: &str) -> Result<Arc<Manifest>, ManifestReadError> {
    // Fast path: hit. No path construction, no decode, no alloc.
    {
        let cache = read_lock();
        if let Some(entry) = cache
            .get(study)
            .and_then(|s| s.entry.as_ref())
            .filter(|e| e.manifests_dir == manifests_dir)
        {
            entry.last_use.store(touch(), Ordering::Relaxed);
            return Ok(Arc::clone(&entry.manifest));
        }
    }
    // Snapshot the generation AND register as an in-flight reader in the same
    // lock section. Without the registration an eviction could drop the slot,
    // free version `v0`, and a fresh slot for this study would start again at
    // 0 - our insert would then match a generation that never existed for the
    // content we decoded.
    let v0 = {
        let mut cache = write_lock();
        let slot = cache.entry(study.to_string()).or_insert(CacheSlot {
            v: 0,
            entry: None,
            last_live: touch(),
            writers: 0,
            readers: 0,
        });
        slot.readers += 1;
        slot.v
    };
    // Miss path (allocates by design - decode + owned strings; not gated).
    // Every early return below must release the reader count.
    let manifest_path = manifests_dir.join(format!("{study}.manifest"));
    let decoded = read_manifest(&manifest_path);
    let encoded_len = std::fs::metadata(&manifest_path)
        .map(|m| m.len())
        .unwrap_or(0);
    // Release the in-flight registration before deciding, so BOTH outcomes
    // leave the slot reclaimable.
    {
        let mut cache = write_lock();
        if let Some(slot) = cache.get_mut(study) {
            slot.readers = slot.readers.saturating_sub(1);
        }
    }
    let manifest = Arc::new(decoded?);
    {
        let mut cache = write_lock();
        let slot = cache.entry(study.to_string()).or_insert(CacheSlot {
            v: 0,
            entry: None,
            last_live: touch(),
            writers: 0,
            readers: 0,
        });
        if try_insert(
            slot,
            v0,
            manifests_dir,
            ArcEntry {
                manifest: Arc::clone(&manifest),
                encoded_len,
                manifests_dir: manifests_dir.to_path_buf(),
                last_use: AtomicU64::new(touch()),
            },
        ) {
            evict_lru_locked(&mut cache, study);
        }
    }
    Ok(manifest)
}

/// Evict by `last_use` until both LRU bounds hold, then drop the least
/// recently live EMPTY slots until the map itself is bounded. Never evicts
/// `keep` (the entry just inserted); a single oversized entry is always kept.
///
/// Eviction CLEARS the slot instead of removing it from the map, and counts
/// only occupied slots when testing the bounds. Both halves are the same bug:
/// `cache.len()` includes the empty slots eviction itself leaves behind, so
/// after a burst of evictions the map never looked small enough and the loop
/// kept going; and `remove` threw away the slot's `v`, which is the version
/// guard `begin_write`/`end_write` hand out. A writer that took `v` before the
/// eviction and reinstalled after it would then match a guard that no longer
/// described the cache - exactly the staleness the guard exists to prevent.
///
/// The slot bound is separate because clearing is not shrinking: the entry
/// budget leaves one empty slot per study ever read, so the map grows with
/// the ARCHIVE rather than with the cache, and a long-lived server ends up
/// holding a slot for every study UID it has ever served. Empty slots are
/// dropped least-recently-live first, and only once the map is past
/// [`MAX_SLOTS`]; `keep` is never dropped, because the caller may be inside
/// the item guard that protects the entry it just inserted.
fn evict_lru_locked(cache: &mut HashMap<String, CacheSlot>, keep: &str) {
    loop {
        let occupied = cache.values().filter(|s| s.entry.is_some()).count();
        let total: u64 = cache
            .values()
            .filter_map(|s| s.entry.as_ref())
            .map(|e| e.encoded_len)
            .sum();
        if occupied <= MAX_ENTRIES && total <= BYTE_BUDGET {
            break;
        }
        // Candidate: the entry with the smallest last-use ticket, other than
        // the just-inserted one.
        let candidate = cache
            .iter()
            .filter(|(k, s)| k.as_str() != keep && s.entry.is_some())
            .filter_map(|(k, s)| {
                s.entry
                    .as_ref()
                    .map(|e| (e.last_use.load(Ordering::Relaxed), k.clone()))
            })
            .min();
        match candidate {
            Some((_, victim)) => {
                if let Some(slot) = cache.get_mut(&victim) {
                    // Keep `slot.v`: the counter is the cache's version for that
                    // study, and an eviction is not a reason to let a stale
                    // writer believe nothing happened.
                    slot.entry = None;
                }
            }
            None => break, // only the kept entry remains (oversized single)
        }
    }
    evict_empty_slots_locked(cache, keep);
}

/// Drop the least recently live empty slots until the map fits [`MAX_SLOTS`].
///
/// Only empty slots are candidates, and only UNVERSIONED ones: a slot whose
/// `v` is non-zero is a study some writer has already begun against, and
/// `begin_write` may still be holding that number. Dropping it would make the
/// writer's `end_write` match nothing, which is indistinguishable from a
/// concurrent writer having won - a silent lost install rather than a
/// reportable one. The distinction costs nothing: a slot that was only ever
/// READ has `v == 0` (readers never bump it), and read-only studies are what
/// an archive sweep produces in bulk.
fn evict_empty_slots_locked(cache: &mut HashMap<String, CacheSlot>, keep: &str) {
    let mut slots = cache.len();
    while slots > MAX_SLOTS {
        // Unversioned first: a slot with `v == 0` is one no writer has ever
        // begun against, so dropping it can never invalidate a live
        // `begin_write`/`end_write` pair.
        let unversioned = cache
            .iter()
            .filter(|(k, s)| {
                k.as_str() != keep
                    && s.entry.is_none()
                    && s.v == 0
                    && s.writers == 0
                    && s.readers == 0
            })
            .map(|(k, s)| (s.last_live, k.clone()))
            .min();
        // Otherwise the least recently LIVE empty slot, whatever its version.
        // Returning early here left the map above `MAX_SLOTS` for as long as
        // the store kept being written to: every study acquires a versioned
        // empty slot the first time it is written, so a store with more than
        // `MAX_SLOTS` written studies could never shrink again - the exact
        // unbounded growth this bound exists to prevent.
        //
        // `writers == 0` is what makes the fallback safe: a slot whose writer
        // has not called `end_write` yet is empty and versioned exactly like a
        // finished, evicted study, but it still holds that writer's version
        // guard. Dropping it would send `end_write` down its `v != v` path on
        // a slot the reader recreated with `v == 0`, and the committed
        // manifest would never be installed.
        let versioned = cache
            .iter()
            .filter(|(k, s)| {
                k.as_str() != keep && s.entry.is_none() && s.writers == 0 && s.readers == 0
            })
            .map(|(k, s)| (s.last_live, k.clone()))
            .min();
        let victim = unversioned.or(versioned);
        let Some((_, victim)) = victim else { return };
        cache.remove(&victim);
        slots -= 1;
    }
}

/// The insert-decision guard of [`get_or_load`]: accept ONLY when the slot
/// version did not move during the decode AND the slot is either empty or
/// holds an entry of ANOTHER manifests dir (a different store's cache line -
/// same study UID, different server).
fn try_insert(slot: &mut CacheSlot, v0: u64, manifests_dir: &Path, entry: ArcEntry) -> bool {
    let foreign = slot
        .entry
        .as_ref()
        .is_some_and(|e| e.manifests_dir != manifests_dir);
    if slot.v == v0 && (slot.entry.is_none() || foreign) {
        slot.last_live = entry.last_use.load(Ordering::Relaxed);
        slot.entry = Some(entry);
        true
    } else {
        false
    }
}

/// A crate-wide test mutex for the process-global cache.
///
/// The cache is a `static` shared by EVERY test in this binary, and more than
/// the cache module touches it: `write_study_manifest_full` calls
/// `begin_write`/`end_write`, so any test that writes a manifest installs or
/// evicts cache entries. A cache test that asserted an exact map size was
/// counting the OTHER module's entries whenever the two ran on parallel
/// threads - the number drifted test-run to test-run. This mutex is the one
/// serial point all cache-touching tests take, so size assertions see a map
/// only their own run populated.
#[cfg(test)]
pub(crate) mod test_sync {
    static CACHE_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquire the shared cache-test guard (poison-tolerant).
    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        CACHE_TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::write_manifest;
    use crate::stow::StudyManifestAttrs;

    /// The cache is a process-global static: tests share its state and the
    /// LRU budget. Serialize through the crate-wide [`test_sync`] guard and
    /// use unique study names. `into_inner` on poison: one failing test must
    /// not cascade.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        test_sync::lock()
    }

    /// Whether `study` holds a LIVE entry.
    ///
    /// Eviction clears the entry and keeps the slot (the slot's `v` is the
    /// version guard), so `contains_key` now answers "the study was seen", not
    /// "the study is cached".
    fn has_entry(cache: &std::collections::HashMap<String, CacheSlot>, study: &str) -> bool {
        cache.get(study).is_some_and(|s| s.entry.is_some())
    }

    fn sample_manifest(study: &str) -> Manifest {
        let attrs = StudyManifestAttrs {
            study_instance_uid: study.to_string(),
            ..StudyManifestAttrs::default()
        };
        // Minimal valid manifest: build through the public full-build helper.
        crate::stow::build_manifest_full(&attrs, &[])
    }

    fn write_sample(dir: &Path, study: &str) -> std::path::PathBuf {
        let path = dir.join(format!("{study}.manifest"));
        let m = sample_manifest(study);
        write_manifest(&path, &m).expect("write sample manifest");
        path
    }

    #[test]
    fn hit_clones_arc_without_decode() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path(), "hit.1");
        let first = get_or_load(dir.path(), "hit.1").expect("miss decode");
        // The entry now caches the manifest; remove the file to prove the hit
        // serves WITHOUT touching the disk.
        std::fs::remove_file(&path).unwrap();
        let second = get_or_load(dir.path(), "hit.1").expect("hit");
        assert!(
            Arc::ptr_eq(&first, &second),
            "hit must clone the cached Arc"
        );
    }

    #[test]
    fn miss_inserts_then_version_unchanged() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "miss.1");
        let _ = get_or_load(dir.path(), "miss.1").expect("miss");
        let m = get_or_load(dir.path(), "miss.1").expect("hit");
        assert_eq!(m.sys.version, 1);
        assert_eq!(read_lock().get("miss.1").map(|s| s.v), Some(0));
    }

    #[test]
    fn try_insert_requires_version_and_empty_slot() {
        // Critique p.9: the insert requires slot.v == v0 AND an empty (or
        // foreign-dir) slot.
        let dir = std::path::Path::new("dir-a");
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("guard")),
            encoded_len: 0,
            manifests_dir: dir.to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        // Fresh slot, version unchanged -> accepted.
        let mut slot = CacheSlot {
            v: 0,
            entry: None,
            last_live: 0,
            writers: 0,
            readers: 0,
        };
        assert!(try_insert(&mut slot, 0, dir, entry), "fresh slot accepts");
        // Slot version moved -> rejected.
        let mut slot = CacheSlot {
            v: 1,
            entry: None,
            last_live: 0,
            writers: 0,
            readers: 0,
        };
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("guard")),
            encoded_len: 0,
            manifests_dir: dir.to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(
            !try_insert(&mut slot, 0, dir, entry),
            "version moved rejects"
        );
        // Entry already present with the SAME dir -> rejected.
        let mut slot = CacheSlot {
            v: 1,
            entry: None,
            last_live: 0,
            writers: 0,
            readers: 0,
        };
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("guard")),
            encoded_len: 0,
            manifests_dir: dir.to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(try_insert(&mut slot, 1, dir, entry), "seeds the entry");
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("guard")),
            encoded_len: 0,
            manifests_dir: dir.to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(
            !try_insert(&mut slot, 1, dir, entry),
            "occupied same-dir slot rejects"
        );
        // Foreign-dir entry with matching version -> REPLACED (multi-server).
        let other = std::path::Path::new("dir-b");
        let mut slot = CacheSlot {
            v: 1,
            entry: None,
            last_live: 0,
            writers: 0,
            readers: 0,
        };
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("guard")),
            encoded_len: 0,
            manifests_dir: dir.to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(try_insert(&mut slot, 1, dir, entry), "seed dir-a entry");
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("guard")),
            encoded_len: 0,
            manifests_dir: other.to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(
            try_insert(&mut slot, 1, other, entry),
            "foreign-dir entry is replaced"
        );
        assert_eq!(
            slot.entry.as_ref().unwrap().manifests_dir,
            other,
            "the foreign dir won the slot"
        );
    }

    #[test]
    fn slot_version_discards_stale_insert() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "stale.1");
        let _ = get_or_load(dir.path(), "stale.1").expect("seed");
        // Reader snapshot before the writer: v0 = 0.
        let v0 = 0;
        let (v1, taken) = begin_write("stale.1");
        assert_eq!(v1, 1, "begin_write bumps the version");
        assert!(taken.is_some(), "begin_write takes the entry");
        // Writer commits and installs the post-commit content.
        end_write(
            "stale.1",
            v1,
            Arc::new(sample_manifest("stale.1")),
            0,
            dir.path(),
        );
        // The concurrent reader decoded with v0 = 0: its insert would be
        // rejected (version moved). Assert the guard decision directly.
        let mut cache = write_lock();
        let slot = cache.get_mut("stale.1").expect("slot");
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("stale.1")),
            encoded_len: 0,
            manifests_dir: dir.path().to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(
            !try_insert(slot, v0, dir.path(), entry),
            "stale reader insert must be discarded"
        );
    }

    #[test]
    fn begin_write_removes_entry_before_mutation() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "bw.1");
        let _ = get_or_load(dir.path(), "bw.1").expect("seed");
        let (_, taken) = begin_write("bw.1");
        assert!(taken.is_some(), "the cached entry must be taken out");
        {
            let cache = read_lock();
            let slot = cache.get("bw.1").expect("slot");
            assert!(slot.entry.is_none(), "no entry until end_write");
        }
    }

    #[test]
    fn end_write_restores_hit() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "ew.1");
        let _ = get_or_load(dir.path(), "ew.1").expect("seed");
        let (v, _) = begin_write("ew.1");
        let committed = sample_manifest("ew.1");
        end_write("ew.1", v, Arc::new(committed), 7, dir.path());
        {
            let cache = read_lock();
            let slot = cache.get("ew.1").expect("slot");
            let entry = slot.entry.as_ref().expect("entry");
            assert_eq!(entry.encoded_len, 7);
        }
        let m = get_or_load(dir.path(), "ew.1").expect("hit after end_write");
        assert_eq!(m.sys.version, 1);
    }

    #[test]
    fn stale_end_write_does_not_install() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "sew.1");
        let _ = get_or_load(dir.path(), "sew.1").expect("seed");
        let (v1, _) = begin_write("sew.1");
        let (v2, _) = begin_write("sew.1");
        assert_eq!(v2, v1 + 1);
        // The older writer finishes last: its version is stale -> no install.
        end_write(
            "sew.1",
            v1,
            Arc::new(sample_manifest("sew.1")),
            0,
            dir.path(),
        );
        {
            let cache = read_lock();
            let slot = cache.get("sew.1").expect("slot");
            assert!(slot.entry.is_none(), "stale end_write must not install");
        }
    }

    #[test]
    fn corrupt_manifest_never_cached() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.1.manifest");
        std::fs::write(&path, b"not a cbOR manifest").unwrap();
        let err = get_or_load(dir.path(), "corrupt.1");
        assert!(err.is_err(), "corrupt decode fails");
        assert!(matches!(err, Err(ManifestReadError::Decode(_))));
        // Fix the file: the next call must decode (nothing was cached).
        std::fs::remove_file(&path).unwrap();
        write_sample(dir.path(), "corrupt.1");
        let m = get_or_load(dir.path(), "corrupt.1").expect("decode after repair");
        assert_eq!(m.sys.version, 1);
    }

    /// C9: a slot with a reader mid-miss is never chosen for eviction, so its
    /// generation survives and the resumed insert cannot install stale bytes.
    ///
    /// The defect sequence: reader snapshots `v0`; a writer commits (bumping
    /// `v`); slot-bound eviction reclaims the slot, which FREES the version; a
    /// fresh slot for the same study starts again at 0, so the paused reader's
    /// `try_insert(slot, v0)` matches and caches the PRE-commit manifest under
    /// a generation it does not belong to.
    ///
    /// This test pushes the cache past `MAX_SLOTS` so the eviction loop really
    /// runs, then asserts the reader's slot is still there with its version and
    /// its registration intact.
    #[test]
    fn an_in_flight_miss_survives_slot_eviction() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        let study = "gen.identity";
        write_sample(dir.path(), study);

        // What `get_or_load` does on the miss path: snapshot + register.
        let v0 = {
            let mut cache = write_lock();
            let slot = cache.entry(study.to_string()).or_insert(CacheSlot {
                v: 0,
                entry: None,
                last_live: touch(),
                writers: 0,
                readers: 0,
            });
            slot.readers += 1;
            slot.v
        };
        assert_eq!(v0, 0, "fresh slot starts at version 0");

        // The reader's slot is still at v == 0 - it is decoding and no writer
        // has begun - so it is exactly what the UNVERSIONED eviction filter
        // would take. `readers` is the only thing that can save it.
        {
            let mut cache = write_lock();
            let s = cache.get_mut(study).expect("slot present");
            s.last_live = 1; // LRU order must pick this one first
            assert_eq!(s.v, 0);
            assert_eq!(s.readers, 1);
        }

        // Force the slot bound: more live slots than MAX_SLOTS.
        {
            let mut cache = write_lock();
            for i in 0..(MAX_SLOTS + 16) {
                let pad = format!("genpad.{i}");
                cache.insert(
                    pad,
                    CacheSlot {
                        v: 0,
                        entry: None,
                        last_live: touch(),
                        writers: 0,
                        readers: 0,
                    },
                );
            }
        }
        {
            let mut cache = write_lock();
            evict_lru_locked(&mut cache, "some.other.study");
            let slot = cache
                .get(study)
                .expect("a slot with a live reader must not be reclaimed");
            assert_eq!(slot.v, 0, "the generation must survive untouched");
            assert_eq!(slot.readers, 1, "the reader registration is intact");
            assert!(
                cache.len() <= MAX_SLOTS,
                "the bound still has to be enforced: len={}",
                cache.len()
            );
        }

        // A writer commits NOW. Because the slot survived, it is the same slot
        // the reader registered against, so the bump is visible to the reader's
        // eventual insert and that insert is refused.
        let (writer_v, _) = begin_write(study);
        assert_eq!(writer_v, 1, "the writer bumped the version");

        // The reader resumes: release, then attempt the stale install.
        {
            let mut cache = write_lock();
            let slot = cache.get_mut(study).unwrap();
            slot.readers -= 1;
            let accepted = try_insert(
                slot,
                v0,
                dir.path(),
                ArcEntry {
                    manifest: Arc::new(sample_manifest(study)),
                    encoded_len: 0,
                    manifests_dir: dir.path().to_path_buf(),
                    last_use: AtomicU64::new(touch()),
                },
            );
            assert!(
                !accepted,
                "a stale pre-commit manifest must not install under a newer generation"
            );
        }
    }

    #[test]
    fn evict_by_count_65th_entry() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        // Known state: this test drives the count bound alone.
        let mut cache = write_lock();
        cache.clear();
        drop(cache);
        for i in 0..MAX_ENTRIES {
            let study = format!("count.pad.{i}");
            write_sample(dir.path(), &study);
            get_or_load(dir.path(), &study).expect("pad");
        }
        assert_eq!(read_lock().len(), MAX_ENTRIES);
        write_sample(dir.path(), "count.overflow");
        get_or_load(dir.path(), "count.overflow").expect("insert overflow");
        let cache = read_lock();
        // Eviction clears the entry but KEEPS the slot, so the map length is no
        // longer the cache size - the occupied entries are.
        let occupied = cache.values().filter(|s| s.entry.is_some()).count();
        assert_eq!(occupied, MAX_ENTRIES, "count bound holds");
        assert!(
            !has_entry(&cache, "count.pad.0"),
            "oldest pad entry evicted"
        );
        assert!(has_entry(&cache, "count.overflow"), "newest kept");
    }

    #[test]
    fn evict_by_bytes_lru() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        // Reset to a known state: this test drives the byte budget alone.
        let mut cache = write_lock();
        cache.clear();
        drop(cache);
        write_sample(dir.path(), "bytes.a");
        get_or_load(dir.path(), "bytes.a").expect("seed a");
        {
            let mut cache = write_lock();
            if let Some(e) = cache.get_mut("bytes.a").and_then(|s| s.entry.as_mut()) {
                e.encoded_len = BYTE_BUDGET - 1000;
            }
        }
        write_sample(dir.path(), "bytes.b");
        get_or_load(dir.path(), "bytes.b").expect("seed b");
        {
            let mut cache = write_lock();
            if let Some(e) = cache.get_mut("bytes.b").and_then(|s| s.entry.as_mut()) {
                e.encoded_len = 2000; // a + b = budget + 1000 -> over
            }
        }
        // Trigger eviction by inserting c: the oldest (a) must go.
        write_sample(dir.path(), "bytes.c");
        get_or_load(dir.path(), "bytes.c").expect("insert c");
        let cache = read_lock();
        assert!(!has_entry(&cache, "bytes.a"), "byte-LRU evicted the oldest");
        assert!(
            has_entry(&cache, "bytes.b") && has_entry(&cache, "bytes.c"),
            "b and c kept under the byte budget"
        );
    }

    #[test]
    fn evict_keeps_oversized_single_entry() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let mut cache = write_lock();
        cache.clear();
        drop(cache);
        write_sample(dir.path(), "oversized.big");
        get_or_load(dir.path(), "oversized.big").expect("seed");
        {
            let mut cache = write_lock();
            if let Some(e) = cache
                .get_mut("oversized.big")
                .and_then(|s| s.entry.as_mut())
            {
                e.encoded_len = BYTE_BUDGET * 2; // single oversized entry
            }
        }
        // A HIT on the oversized entry must not evict it (hits do not run
        // eviction; the keep-guard also protects the lone entry on insert).
        let m = get_or_load(dir.path(), "oversized.big").expect("hit oversized");
        assert_eq!(m.sys.version, 1);
        assert!(has_entry(&read_lock(), "oversized.big"));
        // Inserting ANOTHER entry may evict the oversized one (normal LRU),
        // but the cache must never be flushed empty: the keep-guard holds.
        write_sample(dir.path(), "oversized.small");
        get_or_load(dir.path(), "oversized.small").expect("insert small");
        let cache = read_lock();
        assert!(
            has_entry(&cache, "oversized.small"),
            "eviction never empties the cache"
        );
    }

    /// C8: a long stream of unique study UIDs must not grow the map forever.
    ///
    /// Eviction keeps the slot so a writer's version guard stays meaningful,
    /// which is right - but it also means every UID ever READ leaves a slot
    /// behind. An archive whose studies are opened once each therefore grows
    /// this HashMap with the ARCHIVE, not with the cache, and an 8 GB RPi5
    /// serving a large archive never gets that memory back.
    ///
    /// The assertion is on `HashMap::len`, not on occupied entries: the map
    /// is the thing that grew, and `len` is exactly the number the count
    /// bound deliberately ignores.
    #[test]
    fn the_slot_map_is_bounded_by_unique_study_uids() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let mut cache = write_lock();
        cache.clear();
        drop(cache);

        // Far more unique studies than the slot bound, each read once: the
        // worst case for the map, because nothing is ever hit again and every
        // insert evicts a predecessor.
        let stream = MAX_SLOTS * 3;
        for i in 0..stream {
            let study = format!("sweep.{i}");
            write_sample(dir.path(), &study);
            get_or_load(dir.path(), &study).expect("sweep read");
        }

        let cache = read_lock();
        assert!(
            cache.len() <= MAX_SLOTS,
            "the map must stay bounded after {stream} unique studies, \
             it holds {} slots (bound {MAX_SLOTS})",
            cache.len()
        );
        let occupied = cache.values().filter(|s| s.entry.is_some()).count();
        assert!(
            occupied <= MAX_ENTRIES,
            "the entry budget still holds: {occupied} entries"
        );
        assert!(
            cache.contains_key(&format!("sweep.{}", stream - 1)),
            "the most recent study is still cached"
        );
    }

    /// A writer's version guard survives the slot bound.
    ///
    /// The empty-slot sweep must not be able to drop the slot a writer is
    /// holding: `begin_write` returns `v`, the slot keeps it, and
    /// `end_write` installs only if the slot still carries that version.
    /// Dropping the slot on a burst of unrelated reads would make the
    /// writer's install silently miss - which looks exactly like a
    /// concurrent writer having won, so nothing would report it.
    #[test]
    fn the_slot_bound_keeps_the_guard_of_a_writer_in_flight() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let mut cache = write_lock();
        cache.clear();
        drop(cache);

        // A writer begins and takes its version, then stalls.
        let (v, _) = begin_write("guard.writer");

        for i in 0..MAX_SLOTS * 2 {
            let study = format!("guard.sweep.{i}");
            write_sample(dir.path(), &study);
            get_or_load(dir.path(), &study).expect("sweep read");
        }

        // The stalled writer completes: its slot must still be there and
        // still carry `v`, so the install lands.
        end_write(
            "guard.writer",
            v,
            Arc::new(sample_manifest("guard.writer")),
            0,
            dir.path(),
        );
        let cache = read_lock();
        assert!(
            has_entry(&cache, "guard.writer"),
            "the post-commit install of a writer in flight was dropped by \
             the slot bound"
        );
        assert_eq!(
            cache.get("guard.writer").map(|s| s.v),
            Some(v),
            "the slot still carries the writer's version"
        );
    }

    /// Written studies must not pin the slot map forever.
    ///
    /// Every study acquires a versioned slot the first time it is written, so
    /// a store with more than [`MAX_SLOTS`] written studies has nothing but
    /// versioned empty slots to reclaim. Preferring them is fine; refusing to
    /// drop them was the leak - the map stayed above the bound for as long as
    /// the store kept growing.
    ///
    /// Falsification anchor: the in-flight guard test above still has to pass,
    /// and it does, because the sweep prefers unversioned slots and only falls
    /// back to versioned ones once none are left.
    #[test]
    fn the_slot_map_shrinks_again_when_every_slot_is_versioned() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }

        // Written studies only: each `end_write` leaves a versioned empty slot.
        let stream = MAX_SLOTS * 2;
        for i in 0..stream {
            let study = format!("written.{i}");
            let (v, _) = begin_write(&study);
            end_write(&study, v, Arc::new(sample_manifest(&study)), 0, dir.path());
            // Trim to one entry: the study's slot stays (versioned, empty).
            let mut cache = write_lock();
            evict_lru_locked(&mut cache, &study);
            if let Some(slot) = cache.get_mut(&study) {
                slot.entry = None;
            }
            evict_lru_locked(&mut cache, &study);
        }

        let cache = read_lock();
        assert!(
            cache.len() <= MAX_SLOTS,
            "a store of {stream} written studies must not pin {} slots",
            cache.len()
        );
    }

    /// A slot an in-flight writer holds is EMPTY and VERSIONED, which is also
    /// what a finished, evicted study looks like. Treating the two as one
    /// class lets the slot bound drop a live writer's version guard.
    #[test]
    fn a_versioned_slot_with_an_in_flight_writer_is_never_evicted() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }

        // A writer begins against a brand-new study and stalls. Its slot is
        // empty and versioned, so it has the oldest `last_live` of any slot
        // the sweep could pick.
        let (v, _) = begin_write("inflight.writer");

        // Fill the map past the slot bound with versioned EMPTY slots: written
        // studies whose entry was cleared, i.e. the population the versioned
        // fallback exists to reclaim.
        for i in 0..MAX_SLOTS * 2 {
            let study = format!("inflight.written.{i}");
            let (wv, _) = begin_write(&study);
            end_write(&study, wv, Arc::new(sample_manifest(&study)), 0, dir.path());
            let mut cache = write_lock();
            if let Some(slot) = cache.get_mut(&study) {
                slot.entry = None;
            }
            evict_lru_locked(&mut cache, &study);
        }

        assert!(
            read_lock().contains_key("inflight.writer"),
            "the slot holding an in-flight writer's version guard was evicted"
        );
        // And the guard still lands, which is the property the slot exists for.
        end_write(
            "inflight.writer",
            v,
            Arc::new(sample_manifest("inflight.writer")),
            0,
            dir.path(),
        );
        assert!(
            has_entry(&read_lock(), "inflight.writer"),
            "the stalled writer's install must land"
        );
    }

    /// The reviewer's four-step interleaving, driven sequentially (the
    /// interleaving is sequential - no threads, no sleeps).
    ///
    /// 1. reader snapshots `v0 = 0` and starts decoding from disk;
    /// 2. writer calls `begin_write` (slot `v = 1`, entry taken) and the sweep
    ///    removes that slot under slot pressure;
    /// 3. the reader's `or_insert(v: 0)` recreates the slot with `v = 0`, so
    ///    its `try_insert(v0 = 0)` succeeds and it caches the PRE-write
    ///    manifest;
    /// 4. `end_write` finds no slot and installs nothing -> the cache serves
    ///    pre-mutation content after a successful commit.
    #[test]
    fn the_reviewer_sequence() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        let committed_before = write_sample(dir.path(), "race.study");
        // Step 1: what `get_or_load` decoded and the version it snapped.
        let pre_write = read_manifest(&committed_before).expect("decode pre-write");
        assert_eq!(pre_write.sys.version, 1);
        // Step 2: the writer begins and takes its version guard.
        let (v, _taken) = begin_write("race.study");
        // Slot pressure: enough versioned empty slots to force the sweep, and
        // the writer's own slot is the coldest (it was created first).
        for i in 0..MAX_SLOTS * 2 {
            let study = format!("race.pressure.{i}");
            let (wv, _) = begin_write(&study);
            end_write(&study, wv, Arc::new(sample_manifest(&study)), 0, dir.path());
            let mut cache = write_lock();
            if let Some(slot) = cache.get_mut(&study) {
                slot.entry = None;
            }
            evict_lru_locked(&mut cache, &study);
        }
        // Step 3: the reader inserts what it decoded with `v0 = 0`.
        {
            let mut cache = write_lock();
            let slot = cache.entry("race.study".to_string()).or_insert(CacheSlot {
                v: 0,
                entry: None,
                last_live: touch(),
                writers: 0,
                readers: 0,
            });
            let _ = try_insert(
                slot,
                0,
                dir.path(),
                ArcEntry {
                    manifest: Arc::new(pre_write),
                    encoded_len: 0,
                    manifests_dir: dir.path().to_path_buf(),
                    last_use: AtomicU64::new(touch()),
                },
            );
        }
        // Step 4: the writer commits and installs.
        end_write(
            "race.study",
            v,
            Arc::new(sample_manifest("race.study")),
            0,
            dir.path(),
        );
        // The served manifest must be the COMMITTED one. `sample_manifest`
        // and the on-disk file both carry format version 1, so the entry is
        // identified by its encoded_len (the committed install says 0; the
        // pre-write decode reports the file size).
        let served = get_or_load(dir.path(), "race.study").expect("read after commit");
        {
            let cache = read_lock();
            let entry = cache
                .get("race.study")
                .and_then(|s| s.entry.as_ref())
                .expect("the committed manifest must be installed");
            assert_eq!(
                entry.encoded_len, 0,
                "the cache serves content that missed the commit"
            );
            assert_eq!(
                cache.get("race.study").map(|s| s.v),
                Some(v),
                "the writer's guard still describes the slot"
            );
        }
        assert_eq!(served.sys.version, 1);
    }

    /// A stale `end_write` must not match a new writer's version, and must not
    /// leave its in-flight count behind either.
    #[test]
    fn a_stale_end_write_does_not_match_a_new_writer() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        write_sample(dir.path(), "stale.ew");
        let (v1, _) = begin_write("stale.ew");
        let (v2, _) = begin_write("stale.ew");
        assert_eq!((v1, v2), (1, 2));
        // The loser finishes first: no install, but its count is released.
        end_write(
            "stale.ew",
            v1,
            Arc::new(sample_manifest("stale.ew")),
            9,
            dir.path(),
        );
        {
            let cache = read_lock();
            let slot = cache.get("stale.ew").expect("slot");
            assert!(slot.entry.is_none(), "a stale version must not install");
            assert_eq!(slot.writers, 1, "only the loser's own count is released");
        }
        // The winner finishes: installs and drops the last count.
        end_write(
            "stale.ew",
            v2,
            Arc::new(sample_manifest("stale.ew")),
            7,
            dir.path(),
        );
        let cache = read_lock();
        let slot = cache.get("stale.ew").expect("slot");
        assert_eq!(slot.entry.as_ref().map(|e| e.encoded_len), Some(7));
        assert_eq!(slot.writers, 0, "no writer remains in flight");
    }
}
