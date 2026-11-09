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
    let slot = cache.entry(study.to_string()).or_insert(CacheSlot {
        v: 0,
        entry: None,
        last_live: touch(),
    });
    slot.v += 1;
    (slot.v, slot.entry.take().map(|e| e.manifest))
}

/// Writer completion: install the post-commit manifest.
///
/// Called ONLY after a successful A9-guarded commit. Installs the entry only
/// if the slot version is still `v` (a newer writer began in the meantime -
/// the disk may already carry its content, so ours would be stale). A slot
/// that disappeared (LRU-evicted) is left absent: the next read re-decodes.
pub fn end_write(
    study: &str,
    v: u64,
    manifest: Arc<Manifest>,
    encoded_len: u64,
    manifests_dir: &Path,
) {
    let mut cache = write_lock();
    if let Some(slot) = cache.get_mut(study).filter(|s| s.v == v) {
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
    let v0 = {
        let cache = read_lock();
        cache.get(study).map(|s| s.v).unwrap_or(0)
    };
    // Miss path (allocates by design - decode + owned strings; not gated).
    let manifest_path = manifests_dir.join(format!("{study}.manifest"));
    let manifest = Arc::new(read_manifest(&manifest_path)?);
    let encoded_len = std::fs::metadata(&manifest_path)
        .map(|m| m.len())
        .unwrap_or(0);
    {
        let mut cache = write_lock();
        let slot = cache.entry(study.to_string()).or_insert(CacheSlot {
            v: 0,
            entry: None,
            last_live: touch(),
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
        let victim = cache
            .iter()
            .filter(|(k, s)| k.as_str() != keep && s.entry.is_none() && s.v == 0)
            .map(|(k, s)| (s.last_live, k.clone()))
            .min();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::write_manifest;
    use crate::stow::StudyManifestAttrs;

    /// The cache is a process-global static: tests share its state and the
    /// LRU budget. Serialize the whole module and use unique study names.
    /// `into_inner` on poison: one failing test must not cascade.
    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner())
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
        };
        assert!(try_insert(&mut slot, 0, dir, entry), "fresh slot accepts");
        // Slot version moved -> rejected.
        let mut slot = CacheSlot {
            v: 1,
            entry: None,
            last_live: 0,
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
}
