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

use crate::manifest::{Manifest, ManifestReadError, TrustedManifestDir, read_manifest_published};
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
    /// The manifests dir this entry was decoded from.
    ///
    /// The directory is also part of the slot key, so a hit is already
    /// guaranteed to belong to the asking dir and nothing on the read path has
    /// to compare this. It stays on the entry because it is the only
    /// self-describing record of which store an entry came from, which the
    /// cache tests rely on when they assert that two stores keep separate
    /// slots.
    #[cfg_attr(not(test), allow(dead_code))]
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

/// Cache key: (manifests dir, study UID).
///
/// The DIRECTORY is part of the identity. One process can hold several stores
/// (tests, a QA/primary split, future multi-tenant embedding) and the same
/// study UID can exist in each with different content. Keying by UID alone
/// made two stores share one slot: the other store's commit published into
/// it, and a rejected `end_write` left its readers on a manifest the first
/// store had already replaced.
///
/// A struct rather than a tuple because the HIT path runs per read and must
/// not allocate (ap101): [`CacheKeyRef`] borrows the same pieces so a lookup
/// can hash and compare them without building an owned key.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct CacheKey {
    manifests_dir: PathBuf,
    study: String,
}

/// Field-by-field identity comparison against a call's `dir` + `study`.
///
/// The pieces arrive as separate slices, so comparing them directly avoids
/// building an owned key on a path that must not allocate.
impl CacheKey {
    fn matches(&self, dir: &Path, study: &str) -> bool {
        self.manifests_dir == dir && self.study == study
    }
}

/// Look up `dir` + `study` without allocating a key.
///
/// Linear over the slots: the map is bounded to a few thousand entries and the
/// hit path runs per read, where an allocation-free scan is cheaper than the
/// two allocations an owned composite key would need (ap101).
fn cache_get<'m>(
    cache: &'m HashMap<CacheKey, CacheSlot>,
    dir: &Path,
    study: &str,
) -> Option<&'m CacheSlot> {
    cache
        .iter()
        .find_map(|(key, slot)| key.matches(dir, study).then_some(slot))
}

fn cache_key(dir: &Path, study: &str) -> CacheKey {
    CacheKey {
        manifests_dir: dir.to_path_buf(),
        study: study.to_string(),
    }
}

static CACHE: std::sync::OnceLock<RwLock<HashMap<CacheKey, CacheSlot>>> =
    std::sync::OnceLock::new();
/// Monotonic last-use ticket source. 0 is reserved for "never used".
static TICKET: AtomicU64 = AtomicU64::new(1);

fn cache() -> &'static RwLock<HashMap<CacheKey, CacheSlot>> {
    // First access initializes; afterwards this is a branch + an already-set
    // atomic load (0 alloc on the gated hit path).
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn read_lock() -> std::sync::RwLockReadGuard<'static, HashMap<CacheKey, CacheSlot>> {
    cache().read().unwrap_or_else(|p| p.into_inner())
}

fn write_lock() -> std::sync::RwLockWriteGuard<'static, HashMap<CacheKey, CacheSlot>> {
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
pub fn begin_write(manifests_dir: &Path, study: &str) -> (u64, Option<Arc<Manifest>>) {
    let mut cache = write_lock();
    let stamp = touch();
    let slot = cache
        .entry(cache_key(manifests_dir, study))
        .or_insert(CacheSlot {
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

/// Release a writer registration WITHOUT installing an entry.
///
/// The abort half of [`end_write`]. A writer that fails between `begin_write`
/// and a successful commit must still give the count back: a leaked
/// registration leaves `writers != 0` forever, and both empty-slot eviction
/// filters exclude such a slot, so the map could only grow.
pub fn abort_write(manifests_dir: &Path, study: &str) {
    let mut cache = write_lock();
    let key = cache_key(manifests_dir, study);
    if let Some(slot) = cache.get_mut(&key) {
        slot.writers = slot.writers.saturating_sub(1);
        // Closing the LAST writer registration must also burn the generation.
        // A reader that snapshotted `v0` == the version this writer registered
        // at (post-`begin_write`) may already be decoding the PRE-rename file;
        // if the write failed AFTER the rename (a Unix dir-fsync failure, or
        // the A9 round-trip failing post-rename), disk is AHEAD of that reader.
        // Without the bump its `try_insert` sees `slot.v == v0` and an empty
        // entry and installs the stale pre-commit manifest. Advancing `v` makes
        // that insert fail so the next read re-decodes. Safe because every
        // manifest writer holds the global coordinator lease, so `writers` is
        // 0 or 1 and the bump cannot clobber a concurrent writer's guard.
        if slot.writers == 0 {
            slot.v += 1;
        }
    }
    // A failed write released the writer count and installed no entry, so the
    // slot is now the reclaimable class the empty-slot bound exists for.
    // `end_write` bounds the success path and `get_or_load` the read path, but
    // a study whose writes ALWAYS fail was on neither - without this the map
    // grew one versioned empty slot per failed study, bounded only by the
    // number of distinct failing studies. A no-op while under [`MAX_SLOTS`].
    evict_empty_slots_locked(&mut cache, &key);
}

/// RAII writer registration: releases on EVERY exit, committed or not.
///
/// The five manifest writers each have several `?` exits between
/// `begin_write` and `end_write` (`read_manifest`, `encode`, the atomic write,
/// the A9 round-trip). Handling those by hand meant a new early return could
/// silently leak the registration, and a leak is invisible until the cache map
/// stops shrinking. Holding this guard makes the release automatic, and the
/// explicit [`WriteGuard::commit`] marks the one path that installs an entry.
///
/// `commit` consumes the guard, so installing twice is a compile error rather
/// than a runtime double-decrement.
pub struct WriteGuard {
    study: String,
    manifests_dir: PathBuf,
    version: u64,
    committed: bool,
}

impl WriteGuard {
    /// Register as a writer; mirrors [`begin_write`].
    pub fn begin(manifests_dir: &Path, study: &str) -> (Self, Option<Arc<Manifest>>) {
        let (version, taken) = begin_write(manifests_dir, study);
        (
            Self {
                study: study.to_string(),
                manifests_dir: manifests_dir.to_path_buf(),
                version,
                committed: false,
            },
            taken,
        )
    }

    /// The slot version this writer was registered at.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Install the post-commit entry. Consumes the guard: the registration is
    /// released by [`end_write`] itself.
    pub fn commit(mut self, manifest: Arc<Manifest>, encoded_len: u64, manifests_dir: &Path) {
        end_write(
            &self.study,
            self.version,
            manifest,
            encoded_len,
            manifests_dir,
        );
        self.committed = true;
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        if !self.committed {
            // Every error path lands here: no entry is installed, but the
            // count is returned so the slot stays evictable.
            abort_write(&self.manifests_dir, &self.study);
        }
    }
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
    let key = cache_key(manifests_dir, study);
    let Some(slot) = cache.get_mut(&key) else {
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
    evict_lru_locked(&mut cache, &key);
}

/// Read path: return a cached `Arc<Manifest>` for `study`.
///
/// Hit: `Arc::clone` + a Relaxed last-use store - zero allocations (pinned
/// by the `ap101_manifest_cache` gate). Miss: decode from disk, then insert
/// ONLY if the slot version is unchanged since the decode started AND the
/// slot is still empty; otherwise discard (a writer committed during our
/// decode - returning the decoded pre-commit content is exactly what a
/// reader that opened the file before the atomic rename would have seen).
pub fn get_or_load(
    manifests_dir: &TrustedManifestDir,
    study: &str,
) -> Result<Arc<Manifest>, ManifestReadError> {
    // The trust is asserted at the CALL SITE (the caller holds a
    // `TrustedManifestDir` built at the audited boundary); the cache itself
    // only reads the inner path and never re-proves publication per read.
    let dir = manifests_dir.path();
    // Fast path: hit. No path construction, no decode, no alloc.
    {
        let cache = read_lock();
        if let Some(entry) = cache_get(&cache, dir, study).and_then(|s| s.entry.as_ref()) {
            entry.last_use.store(touch(), Ordering::Relaxed);
            return Ok(Arc::clone(&entry.manifest));
        }
    }
    // Snapshot the generation AND register as an in-flight reader in the same
    // lock section. Without the registration an eviction could drop the slot,
    // free version `v0`, and a fresh slot for this study would start again at
    // 0 - our insert would then match a generation that never existed for the
    // content we decoded.
    //
    // The ordering ticket is taken HERE, not at insert time: `try_insert` uses
    // it to order this decode against a foreign entry published while it was
    // paused, and that comparison is only meaningful if the ticket marks when
    // the decode STARTED. Taking it after the decode gives a slow reader of
    // OLD content the NEWEST ticket, which then beats a manifest committed
    // after the read - exactly the staleness the ticket exists to prevent.
    let (v0, ticket) = {
        let mut cache = write_lock();
        let ticket = touch();
        let key = cache_key(dir, study);
        let slot = cache.entry(key).or_insert(CacheSlot {
            v: 0,
            entry: None,
            last_live: ticket,
            writers: 0,
            readers: 0,
        });
        slot.readers += 1;
        (slot.v, ticket)
    };
    // Miss path (allocates by design - decode + owned strings; not gated).
    let manifest_path = dir.join(format!("{study}.manifest"));
    // The cache is keyed by (directory, study) and only ever loads a manifest
    // the stow writer published - the same two halves `PublishedManifest` is
    // built from, so a caller cannot route an arbitrary path into a mapping.
    let published = crate::manifest::PublishedManifest::new(dir, study);
    let decoded = match published.as_ref() {
        Some(p) => read_manifest_published(p),
        None => Err(ManifestReadError::Io(std::io::Error::from(
            std::io::ErrorKind::InvalidInput,
        ))),
    };
    let encoded_len = std::fs::metadata(&manifest_path)
        .map(|m| m.len())
        .unwrap_or(0);
    // The registration, the insert decision and the release all happen under
    // ONE write lock. Releasing the count first opened a window in which an
    // eviction could clear the slot, a fresh one could start at `v = 0` again,
    // and our stale decode would then satisfy the version guard and be
    // installed over a newer commit - the staleness the guard exists to stop.
    //
    // Both outcomes now leave the slot reclaimable: success keeps the inserted
    // entry, and a FAILED decode drops the slot it created. The error path
    // used to leave an empty (v = 0, readers = 0) slot behind forever, because
    // eviction only ran after a successful insert - so asking for a stream of
    // studies that do not exist grew the map without bound.
    let decoded = {
        let mut cache = write_lock();
        match decoded {
            Ok(manifest) => {
                let manifest = Arc::new(manifest);
                let key = cache_key(dir, study);
                let slot = cache.entry(key.clone()).or_insert(CacheSlot {
                    v: 0,
                    entry: None,
                    last_live: touch(),
                    writers: 0,
                    readers: 0,
                });
                slot.readers = slot.readers.saturating_sub(1);
                if try_insert(
                    slot,
                    v0,
                    ArcEntry {
                        manifest: Arc::clone(&manifest),
                        encoded_len,
                        manifests_dir: dir.to_path_buf(),
                        // The ticket captured BEFORE the decode, so the
                        // ordering in `try_insert` reflects when this read
                        // began rather than when it happened to finish.
                        last_use: AtomicU64::new(ticket),
                    },
                ) {
                    evict_lru_locked(&mut cache, &key);
                }
                manifest
            }
            Err(error) => {
                let key = cache_key(dir, study);
                if let Some(slot) = cache.get_mut(&key) {
                    slot.readers = slot.readers.saturating_sub(1);
                    // Only a slot this load created and nobody else touches:
                    // an entry means a concurrent writer committed, and a
                    // remaining reader or a writer means somebody else owns
                    // the slot's fate.
                    if slot.entry.is_none() && slot.readers == 0 && slot.writers == 0 {
                        cache.remove(&key);
                    }
                }
                return Err(error);
            }
        }
    };
    Ok(decoded)
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
fn evict_lru_locked(cache: &mut HashMap<CacheKey, CacheSlot>, keep: &CacheKey) {
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
        //
        // `readers == 0` is required: a reader that snapshotted `v0` is still
        // decoding the PRE-commit file, and evicting the slot underneath it
        // would let its `try_insert` see an EMPTY slot with `v == v0` and
        // install the stale manifest it just decoded. Keeping the entry until
        // its readers drain takes precedence over the LRU budget - one
        // over-budget slot is cheaper than serving a superseded manifest.
        let candidate = cache
            .iter()
            .filter(|(k, s)| *k != keep && s.entry.is_some() && s.readers == 0)
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
fn evict_empty_slots_locked(cache: &mut HashMap<CacheKey, CacheSlot>, keep: &CacheKey) {
    let mut slots = cache.len();
    while slots > MAX_SLOTS {
        // Unversioned first: a slot with `v == 0` is one no writer has ever
        // begun against, so dropping it can never invalidate a live
        // `begin_write`/`end_write` pair.
        let unversioned = cache
            .iter()
            .filter(|(k, s)| {
                *k != keep && s.entry.is_none() && s.v == 0 && s.writers == 0 && s.readers == 0
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
            .filter(|(k, s)| *k != keep && s.entry.is_none() && s.writers == 0 && s.readers == 0)
            .map(|(k, s)| (s.last_live, k.clone()))
            .min();
        let victim = unversioned.or(versioned);
        let Some((_, victim)) = victim else { return };
        cache.remove(&victim);
        slots -= 1;
    }
}

/// The insert-decision guard of [`get_or_load`]: accept ONLY when the slot
/// version did not move during the decode, the slot is still empty, AND no
/// writer is mid-flight.
///
/// `slot.v == v0` proves no writer began against this key since the reader
/// snapshotted the version, so what was decoded is still the newest this
/// cache has seen for it. A slot that already holds an entry was filled by a
/// writer or a faster reader while this decode was paused, and that entry is
/// newer than the bytes just read.
///
/// `slot.writers == 0` closes the writer-window hole in the module invariant
/// ("entry present => entry == disk content", see the module doc): a reader
/// that snapshotted `v0` AFTER `begin_write` (so `v0` equals the writer's
/// version) and finished decoding BEFORE the writer completes would otherwise
/// pass both guards and install the PRE-commit bytes. `begin_write` already
/// removed the entry, so while `writers != 0` the slot must stay empty until
/// `end_write` installs the committed manifest (or the writer aborts and the
/// next read re-decodes). The abort path's version bump (see [`abort_write`])
/// only rejects a reader that finishes AFTER the abort; this guard rejects
/// the one that finishes BEFORE it, on the success path too.
///
/// The directory is part of the key, so an entry in this slot always belongs
/// to the SAME manifests dir: the cross-store case the last-use ticket used
/// to arbitrate cannot reach here any more.
fn try_insert(slot: &mut CacheSlot, v0: u64, entry: ArcEntry) -> bool {
    if slot.v == v0 && slot.entry.is_none() && slot.writers == 0 {
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
    /// The manifests dir the raw-map assertions in this module use.
    ///
    /// Slots are keyed by (manifests dir, study UID), so a test that pokes the
    /// global map has to name the dir along with the UID.
    fn k(dir: impl AsRef<std::path::Path>, study: &str) -> CacheKey {
        cache_key(dir.as_ref(), study)
    }

    /// Whether any slot for `study` holds a LIVE entry, in any directory.
    ///
    /// Eviction clears the entry and keeps the slot (the slot's `v` is the
    /// version guard), so `contains_key` answers "the study was seen", not
    /// "the study is cached". Scanning rather than naming a dir keeps the
    /// helper usable from tests that never created one.
    fn has_entry(cache: &std::collections::HashMap<CacheKey, CacheSlot>, study: &str) -> bool {
        cache
            .iter()
            .any(|(key, s)| key.study == study && s.entry.is_some())
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

    /// A trusted-dir voucher for a temp dir this test publishes into itself.
    fn tdir(dir: &Path) -> TrustedManifestDir {
        crate::manifest::trusted_manifest_dir_for_test(dir)
    }

    /// `get_or_load` for a raw `&Path`: builds the test-published voucher so
    /// the call sites below stay readable without repeating the construction.
    fn get(dir: &Path, study: &str) -> Result<Arc<Manifest>, ManifestReadError> {
        get_or_load(&tdir(dir), study)
    }

    #[test]
    fn hit_clones_arc_without_decode() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = write_sample(dir.path(), "hit.1");
        let trusted = tdir(dir.path());
        let first = get_or_load(&trusted, "hit.1").expect("miss decode");
        // The entry now caches the manifest; remove the file to prove the hit
        // serves WITHOUT touching the disk.
        std::fs::remove_file(&path).unwrap();
        let second = get_or_load(&trusted, "hit.1").expect("hit");
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
        let trusted = tdir(dir.path());
        let _ = get_or_load(&trusted, "miss.1").expect("miss");
        let m = get_or_load(&trusted, "miss.1").expect("hit");
        assert_eq!(m.sys.version, 1);
        assert_eq!(read_lock().get(&k(&dir, "miss.1")).map(|s| s.v), Some(0));
    }

    /// Two manifests dirs holding the same study UID are different files.
    ///
    /// The manifests directory is a `recovery::scan` input, so a tree can be
    /// pointed at an alternate root. Keying the cache on the UID alone makes a
    /// read of `<other>/X.manifest` return the entry cached from `<default>/`,
    /// i.e. one root's study served in place of the other's.
    #[test]
    fn the_same_uid_under_two_dirs_does_not_share_an_entry() {
        let _g = serial();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let study = "shared.uid";
        write_sample(dir_a.path(), study);
        write_sample(dir_b.path(), study);

        let ta = tdir(dir_a.path());
        let tb = tdir(dir_b.path());
        let a = get_or_load(&ta, study).expect("dir a miss");
        let b = get_or_load(&tb, study).expect("dir b miss");
        assert!(
            !Arc::ptr_eq(&a, &b),
            "each dir must decode its own manifest"
        );

        // Re-reads must stay bound to the dir they asked for.
        let a2 = get_or_load(&ta, study).expect("dir a hit");
        let b2 = get_or_load(&tb, study).expect("dir b hit");
        assert!(Arc::ptr_eq(&a, &a2), "dir a must keep serving dir a");
        assert!(Arc::ptr_eq(&b, &b2), "dir b must keep serving dir b");
    }

    #[test]
    fn try_insert_requires_version_and_empty_slot() {
        // The tail of this test pokes the GLOBAL cache (`cache.clear()` then
        // `get_or_load`), so it has to hold the serial lock: another cache test
        // paused between its load and its slot check would otherwise lose its
        // state to this clear.
        let _g = serial();
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
        assert!(try_insert(&mut slot, 0, entry), "fresh slot accepts");
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
        assert!(!try_insert(&mut slot, 0, entry), "version moved rejects");
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
        assert!(try_insert(&mut slot, 1, entry), "seeds the entry");
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("guard")),
            encoded_len: 0,
            manifests_dir: dir.to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(
            !try_insert(&mut slot, 1, entry),
            "occupied same-dir slot rejects"
        );
        // A slot belongs to ONE directory now (the dir is part of the key),
        // so a second store's read gets its OWN slot and never contends for
        // this one. Two stores sharing a study UID is therefore two keys,
        // not one key with a foreign entry to arbitrate.
        let study = "shared.uid";
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        for d in [dir_a.path(), dir_b.path()] {
            let mut m = StudyManifestAttrs {
                study_instance_uid: study.into(),
                patient_name: Some("ivanov".into()),
                modality: "CT".into(),
                ..Default::default()
            };
            crate::stow::write_study_manifest(d, &mut m).expect("write");
        }
        {
            let mut cache = write_lock();
            cache.clear();
        }
        get(dir_a.path(), study).expect("a");
        get(dir_b.path(), study).expect("b");
        for d in [dir_a.path(), dir_b.path()] {
            let cache = read_lock();
            let entry = cache
                .get(&cache_key(d, study))
                .and_then(|s| s.entry.as_ref())
                .expect("each dir keeps its own slot");
            assert_eq!(entry.manifests_dir, d, "the slot belongs to its own dir");
        }
    }

    #[test]
    fn slot_version_discards_stale_insert() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "stale.1");
        let _ = get(dir.path(), "stale.1").expect("seed");
        // Reader snapshot before the writer: v0 = 0.
        let v0 = 0;
        let (v1, taken) = begin_write(dir.path(), "stale.1");
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
        let slot = cache.get_mut(&k(&dir, "stale.1")).expect("slot");
        let entry = ArcEntry {
            manifest: Arc::new(sample_manifest("stale.1")),
            encoded_len: 0,
            manifests_dir: dir.path().to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(
            !try_insert(slot, v0, entry),
            "stale reader insert must be discarded"
        );
    }

    #[test]
    fn begin_write_removes_entry_before_mutation() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "bw.1");
        let _ = get(dir.path(), "bw.1").expect("seed");
        let (_, taken) = begin_write(dir.path(), "bw.1");
        assert!(taken.is_some(), "the cached entry must be taken out");
        {
            let cache = read_lock();
            let slot = cache.get(&k(&dir, "bw.1")).expect("slot");
            assert!(slot.entry.is_none(), "no entry until end_write");
        }
    }

    #[test]
    fn end_write_restores_hit() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "ew.1");
        let _ = get(dir.path(), "ew.1").expect("seed");
        let (v, _) = begin_write(dir.path(), "ew.1");
        let committed = sample_manifest("ew.1");
        end_write("ew.1", v, Arc::new(committed), 7, dir.path());
        {
            let cache = read_lock();
            let slot = cache.get(&k(&dir, "ew.1")).expect("slot");
            let entry = slot.entry.as_ref().expect("entry");
            assert_eq!(entry.encoded_len, 7);
        }
        let m = get(dir.path(), "ew.1").expect("hit after end_write");
        assert_eq!(m.sys.version, 1);
    }

    #[test]
    fn stale_end_write_does_not_install() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        write_sample(dir.path(), "sew.1");
        let _ = get(dir.path(), "sew.1").expect("seed");
        let (v1, _) = begin_write(dir.path(), "sew.1");
        let (v2, _) = begin_write(dir.path(), "sew.1");
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
            let slot = cache.get(&k(&dir, "sew.1")).expect("slot");
            assert!(slot.entry.is_none(), "stale end_write must not install");
        }
    }

    /// Closing the last writer registration on the ABORT path burns the
    /// generation, so a reader that snapshotted the writer's version and
    /// decoded the PRE-rename file cannot install that stale content after
    /// the abort (the rename may already have landed - disk is ahead of it).
    #[test]
    fn abort_write_burns_the_version_after_the_last_writer() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let study = "abort.burn";
        write_sample(dir.path(), study);
        let _ = get(dir.path(), study).expect("seed");

        let (v1, _) = begin_write(dir.path(), study);
        assert_eq!(v1, 1, "begin_write bumps the version to 1");
        {
            let cache = read_lock();
            let slot = cache.get(&k(&dir, study)).expect("slot");
            assert_eq!(slot.v, 1, "the slot is at the writer's version");
            assert_eq!(slot.writers, 1, "one writer registered");
        }

        abort_write(dir.path(), study);

        {
            let cache = read_lock();
            let slot = cache.get(&k(&dir, study)).expect("slot");
            assert_eq!(slot.writers, 0, "the writer count is released");
            assert_eq!(
                slot.v, 2,
                "abort must burn the generation after closing the last writer"
            );
        }

        // A reader that snapshotted v0 == 1 (the aborted writer's version) and
        // decoded the OLD file must now be rejected by `try_insert`.
        let mut cache = write_lock();
        let slot = cache.get_mut(&k(&dir, study)).expect("slot");
        let stale = ArcEntry {
            manifest: Arc::new(sample_manifest(study)),
            encoded_len: 0,
            manifests_dir: dir.path().to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        assert!(
            !try_insert(slot, 1, stale),
            "a reader on the aborted writer's version must not install"
        );
        drop(cache);

        // A fresh read re-decodes and installs the current disk content.
        let m = get(dir.path(), study).expect("fresh read after abort");
        assert_eq!(m.sys.version, 1, "the re-decoded manifest is served");
    }

    /// A reader that finished BEFORE the post-rename failure must not install
    /// while the writer is still registered: `try_insert` requires
    /// `writers == 0`. The abort's version bump only rejects a reader that
    /// finishes AFTER the abort; this guard rejects the one that finishes
    /// during the writer window (the success path too: a reader installing
    /// between the rename and `end_write` would serve stale bytes until the
    /// commit overwrites it).
    #[test]
    fn a_reader_cannot_insert_while_a_writer_is_in_flight() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let study = "midflight.1";
        write_sample(dir.path(), study);
        let _ = get(dir.path(), study).expect("seed");

        let (v1, taken) = begin_write(dir.path(), study);
        assert_eq!(v1, 1, "begin_write bumps the version to 1");
        assert!(taken.is_some(), "begin_write takes the entry");

        // The rabbit interleaving: a reader snapshotted v0 == 1 (the writer's
        // version) and decoded the pre-rename bytes; its `try_insert` runs
        // BEFORE the writer commits or aborts, while `writers == 1`.
        let stale = ArcEntry {
            manifest: Arc::new(sample_manifest(study)),
            encoded_len: 0,
            manifests_dir: dir.path().to_path_buf(),
            last_use: AtomicU64::new(touch()),
        };
        {
            let mut cache = write_lock();
            let slot = cache.get_mut(&k(&dir, study)).expect("slot");
            assert_eq!(slot.writers, 1, "the writer is still registered");
            assert!(
                !try_insert(slot, 1, stale),
                "a reader must not insert while a writer is in flight"
            );
        }

        // The writer then fails: abort must leave no entry behind.
        abort_write(dir.path(), study);
        assert!(
            !has_entry(&read_lock(), study),
            "abort after a blocked insert must leave no entry"
        );

        // A fresh read re-decodes and installs the current disk content.
        let m = get(dir.path(), study).expect("fresh read after abort");
        assert_eq!(m.sys.version, 1, "the re-decoded manifest is served");
    }

    #[test]
    fn corrupt_manifest_never_cached() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.1.manifest");
        std::fs::write(&path, b"not a cbOR manifest").unwrap();
        let err = get(dir.path(), "corrupt.1");
        assert!(err.is_err(), "corrupt decode fails");
        assert!(matches!(err, Err(ManifestReadError::Decode(_))));
        // Fix the file: the next call must decode (nothing was cached).
        std::fs::remove_file(&path).unwrap();
        write_sample(dir.path(), "corrupt.1");
        let m = get(dir.path(), "corrupt.1").expect("decode after repair");
        assert_eq!(m.sys.version, 1);
    }

    /// A failed load leaves NO slot behind.
    ///
    /// The miss path registers a reader by creating an empty slot, and the
    /// error arm used to return with that slot still in the map - eviction
    /// only ran after a successful insert. A client probing study UIDs that do
    /// not exist (a viewer's typo, a scanner, a hostile loop) therefore grew
    /// the map one slot per probe, forever. The slot is now dropped when the
    /// load fails, so repeated misses leave the cache exactly as they found it.
    #[test]
    fn a_failed_load_leaves_no_slot_behind() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let study = "missing.slot.1";
        assert!(
            !read_lock().contains_key(&k(&dir, study)),
            "precondition: the study must not be cached yet"
        );
        for _ in 0..5 {
            assert!(get(dir.path(), study).is_err());
            assert!(
                !read_lock().contains_key(&k(&dir, study)),
                "a failed load must not leave a slot in the cache"
            );
        }
        // Many distinct absent studies: the map must not grow per attempt.
        for n in 0..25 {
            let _ = get(dir.path(), &format!("missing.slot.{n}"));
        }
        let lingering = read_lock()
            .keys()
            .filter(|key| key.study.starts_with("missing.slot."))
            .count();
        assert_eq!(
            lingering, 0,
            "failed loads must not accumulate slots, found {lingering}"
        );
    }

    /// Release and insert happen in ONE lock section, so the slot is never
    /// briefly unregistered between them.
    ///
    /// The defect: `readers` was decremented under one write lock and the
    /// insert decided under a second. In that window the slot looked idle, so
    /// an eviction pass could clear it and a fresh slot for the same study
    /// would restart at version 0 - which the paused decode's snapshot then
    /// matched, installing pre-commit bytes under a current generation.
    ///
    /// The property is stated as a code invariant rather than a race probe: a
    /// test that slept in the window would be timing-dependent and would pass
    /// on an unlucky scheduler, so the source itself is pinned.
    #[test]
    fn the_miss_path_takes_one_write_lock_for_release_and_insert() {
        let source = include_str!("manifest_cache.rs");
        let body_start = source
            .find("pub fn get_or_load")
            .expect("get_or_load must exist");
        let body = &source[body_start..];
        let body_end = body.find("\nfn evict_lru_locked").unwrap_or(body.len());
        let body = &body[..body_end];
        // One lock for the snapshot at the top, one for release+insert.
        let locks = body.matches("write_lock()").count();
        assert_eq!(
            locks, 2,
            "get_or_load must take the snapshot lock and ONE further write \
             lock covering both the release and the insert; found {locks}"
        );
        let release = body
            .find("readers = slot.readers.saturating_sub(1)")
            .expect("the release must be visible");
        let insert = body
            .find("try_insert(")
            .expect("the insert must be visible");
        assert!(
            release < insert,
            "the reader count must be released in the same decision block"
        );
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
            let slot = cache.entry(k(dir.path(), study)).or_insert(CacheSlot {
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
            let s = cache.get_mut(&k(&dir, study)).expect("slot present");
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
                    k(&dir, &pad),
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
            evict_lru_locked(&mut cache, &k(&dir, "some.other.study"));
            let slot = cache
                .get(&k(&dir, study))
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
        let (writer_v, _) = begin_write(dir.path(), study);
        assert_eq!(writer_v, 1, "the writer bumped the version");

        // The reader resumes: release, then attempt the stale install.
        {
            let mut cache = write_lock();
            let slot = cache.get_mut(&k(&dir, study)).unwrap();
            slot.readers -= 1;
            let accepted = try_insert(
                slot,
                v0,
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
            get(dir.path(), &study).expect("pad");
        }
        assert_eq!(read_lock().len(), MAX_ENTRIES);
        write_sample(dir.path(), "count.overflow");
        get(dir.path(), "count.overflow").expect("insert overflow");
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
        get(dir.path(), "bytes.a").expect("seed a");
        {
            let mut cache = write_lock();
            if let Some(e) = cache
                .get_mut(&k(&dir, "bytes.a"))
                .and_then(|s| s.entry.as_mut())
            {
                e.encoded_len = BYTE_BUDGET - 1000;
            }
        }
        write_sample(dir.path(), "bytes.b");
        get(dir.path(), "bytes.b").expect("seed b");
        {
            let mut cache = write_lock();
            if let Some(e) = cache
                .get_mut(&k(&dir, "bytes.b"))
                .and_then(|s| s.entry.as_mut())
            {
                e.encoded_len = 2000; // a + b = budget + 1000 -> over
            }
        }
        // Trigger eviction by inserting c: the oldest (a) must go.
        write_sample(dir.path(), "bytes.c");
        get(dir.path(), "bytes.c").expect("insert c");
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
        get(dir.path(), "oversized.big").expect("seed");
        {
            let mut cache = write_lock();
            if let Some(e) = cache
                .get_mut(&k(&dir, "oversized.big"))
                .and_then(|s| s.entry.as_mut())
            {
                e.encoded_len = BYTE_BUDGET * 2; // single oversized entry
            }
        }
        // A HIT on the oversized entry must not evict it (hits do not run
        // eviction; the keep-guard also protects the lone entry on insert).
        let m = get(dir.path(), "oversized.big").expect("hit oversized");
        assert_eq!(m.sys.version, 1);
        assert!(has_entry(&read_lock(), "oversized.big"));
        // Inserting ANOTHER entry may evict the oversized one (normal LRU),
        // but the cache must never be flushed empty: the keep-guard holds.
        write_sample(dir.path(), "oversized.small");
        get(dir.path(), "oversized.small").expect("insert small");
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
            get(dir.path(), &study).expect("sweep read");
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
            cache.contains_key(&k(&dir, &format!("sweep.{}", stream - 1))),
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
        let (v, _) = begin_write(dir.path(), "guard.writer");

        for i in 0..MAX_SLOTS * 2 {
            let study = format!("guard.sweep.{i}");
            write_sample(dir.path(), &study);
            get(dir.path(), &study).expect("sweep read");
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
            cache.get(&k(&dir, "guard.writer")).map(|s| s.v),
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
            let (v, _) = begin_write(dir.path(), &study);
            end_write(&study, v, Arc::new(sample_manifest(&study)), 0, dir.path());
            // Trim to one entry: the study's slot stays (versioned, empty).
            let mut cache = write_lock();
            evict_lru_locked(&mut cache, &k(&dir, &study));
            if let Some(slot) = cache.get_mut(&k(&dir, &study)) {
                slot.entry = None;
            }
            evict_lru_locked(&mut cache, &k(&dir, &study));
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
        let (v, _) = begin_write(dir.path(), "inflight.writer");

        // Fill the map past the slot bound with versioned EMPTY slots: written
        // studies whose entry was cleared, i.e. the population the versioned
        // fallback exists to reclaim.
        for i in 0..MAX_SLOTS * 2 {
            let study = format!("inflight.written.{i}");
            let (wv, _) = begin_write(dir.path(), &study);
            end_write(&study, wv, Arc::new(sample_manifest(&study)), 0, dir.path());
            let mut cache = write_lock();
            if let Some(slot) = cache.get_mut(&k(&dir, &study)) {
                slot.entry = None;
            }
            evict_lru_locked(&mut cache, &k(&dir, &study));
        }

        assert!(
            read_lock().contains_key(&k(&dir, "inflight.writer")),
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
        let pre_write =
            crate::manifest::read_manifest_for_test(&committed_before).expect("decode pre-write");
        assert_eq!(pre_write.sys.version, 1);
        // Step 2: the writer begins and takes its version guard.
        let (v, _taken) = begin_write(dir.path(), "race.study");
        // Slot pressure: enough versioned empty slots to force the sweep, and
        // the writer's own slot is the coldest (it was created first).
        for i in 0..MAX_SLOTS * 2 {
            let study = format!("race.pressure.{i}");
            let (wv, _) = begin_write(dir.path(), &study);
            end_write(&study, wv, Arc::new(sample_manifest(&study)), 0, dir.path());
            let mut cache = write_lock();
            if let Some(slot) = cache.get_mut(&k(&dir, &study)) {
                slot.entry = None;
            }
            evict_lru_locked(&mut cache, &k(&dir, &study));
        }
        // Step 3: the reader inserts what it decoded with `v0 = 0`.
        {
            let mut cache = write_lock();
            let slot = cache.entry(k(&dir, "race.study")).or_insert(CacheSlot {
                v: 0,
                entry: None,
                last_live: touch(),
                writers: 0,
                readers: 0,
            });
            let _ = try_insert(
                slot,
                0,
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
        let served = get(dir.path(), "race.study").expect("read after commit");
        {
            let cache = read_lock();
            let entry = cache
                .get(&k(&dir, "race.study"))
                .and_then(|s| s.entry.as_ref())
                .expect("the committed manifest must be installed");
            assert_eq!(
                entry.encoded_len, 0,
                "the cache serves content that missed the commit"
            );
            assert_eq!(
                cache.get(&k(&dir, "race.study")).map(|s| s.v),
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
        let (v1, _) = begin_write(dir.path(), "stale.ew");
        let (v2, _) = begin_write(dir.path(), "stale.ew");
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
            let slot = cache.get(&k(&dir, "stale.ew")).expect("slot");
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
        let slot = cache.get(&k(&dir, "stale.ew")).expect("slot");
        assert_eq!(slot.entry.as_ref().map(|e| e.encoded_len), Some(7));
        assert_eq!(slot.writers, 0, "no writer remains in flight");
    }

    /// F5: an entry with a live READER must survive eviction pressure.
    ///
    /// The race: a reader snapshots `v0` and starts decoding the pre-commit
    /// file; the new entry is installed and LRU-evicted before the reader
    /// resumes. `try_insert` then sees `slot.v == v0` and an EMPTY slot - which
    /// is exactly the accept condition - so the reader installs the superseded
    /// manifest it decoded, and every later read serves stale data until the
    /// study is written again.
    ///
    /// Eviction filtering on `readers == 0` is the whole defence, so the test
    /// puts a reader on the victim and shows the entry still there.
    #[test]
    fn an_entry_with_a_live_reader_is_not_evicted() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        let study = "reader.pin";
        write_sample(dir.path(), study);

        // Install a committed entry, then register a reader on it.
        let (v, _) = begin_write(dir.path(), study);
        end_write(study, v, Arc::new(sample_manifest(study)), 11, dir.path());
        {
            let mut cache = write_lock();
            let s = cache.get_mut(&k(&dir, study)).expect("slot");
            s.readers += 1;
            s.last_live = 1; // oldest, so LRU must want this one first
        }

        // Drive the entry count over the bound with other occupied entries.
        {
            let mut cache = write_lock();
            for i in 0..(MAX_ENTRIES + 16) {
                let pad = format!("pad.reader.{i}");
                cache.insert(
                    k(&dir, &pad),
                    CacheSlot {
                        v: 0,
                        entry: Some(ArcEntry {
                            manifest: Arc::new(sample_manifest(study)),
                            manifests_dir: dir.path().to_path_buf(),
                            encoded_len: 1,
                            last_use: std::sync::atomic::AtomicU64::new(touch()),
                        }),
                        last_live: touch(),
                        writers: 0,
                        readers: 0,
                    },
                );
            }
        }

        {
            let mut cache = write_lock();
            evict_lru_locked(&mut cache, &k(&dir, "some.other.study"));
            let slot = cache
                .get(&k(&dir, study))
                .expect("the slot must not be dropped while a reader holds it");
            assert!(
                slot.entry.is_some(),
                "an entry with a live reader must not be evicted: the reader \
                 would then install the pre-commit manifest it decoded"
            );
            assert_eq!(slot.readers, 1);
        }

        // Once the reader drains, the entry is a normal LRU candidate again.
        {
            let mut cache = write_lock();
            let s = cache.get_mut(&k(&dir, study)).expect("slot");
            s.readers = 0;
            s.last_live = 1; // still the least recently live
            // The previous pass already shrank the map to the bound, so the
            // pressure has to be re-applied for a second eviction to happen.
            for i in 0..(MAX_ENTRIES + 16) {
                let pad = format!("pad.again.{i}");
                cache.insert(
                    k(&dir, &pad),
                    CacheSlot {
                        v: 0,
                        entry: Some(ArcEntry {
                            manifest: Arc::new(sample_manifest(study)),
                            manifests_dir: dir.path().to_path_buf(),
                            encoded_len: 1,
                            last_use: std::sync::atomic::AtomicU64::new(touch()),
                        }),
                        last_live: touch(),
                        writers: 0,
                        readers: 0,
                    },
                );
            }
        }
        {
            let mut cache = write_lock();
            evict_lru_locked(&mut cache, &k(&dir, "some.other.study"));
            let slot = cache.get(&k(&dir, study)).expect("slot");
            assert!(
                slot.entry.is_none(),
                "with no reader left the entry is evictable again"
            );
        }
    }

    /// F1: a write that FAILS must not leak its writer registration.
    ///
    /// `begin_write` increments `writers` and only `end_write` decrements it.
    /// The manifest writers have several `?` exits in between (`read_manifest`,
    /// `encode`, the atomic write, the A9 round-trip), so each failure used to
    /// leave `writers == 1` on the slot forever. Both empty-slot eviction
    /// filters exclude such a slot, so failures across distinct studies could
    /// only GROW the map past `MAX_SLOTS` with no recovery - a slow leak that
    /// looks like an unrelated memory report.
    ///
    /// The guard releases on every exit, so a dropped-without-commit guard must
    /// leave the slot exactly as evictable as it was.
    #[test]
    fn a_write_that_never_commits_releases_its_registration() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        let study = "guard.abort";
        write_sample(dir.path(), study);

        {
            let (guard, _taken) = WriteGuard::begin(dir.path(), study);
            // The writer fails here - encode/atomic-write/A9 all return `?` in
            // the real code paths. Dropping the guard IS that exit.
            assert_eq!(guard.version(), 1, "the registration took a version");
            let cache = read_lock();
            let slot = cache.get(&k(&dir, study)).expect("slot exists");
            assert_eq!(slot.writers, 1, "in flight while the guard lives");
        }

        {
            let cache = read_lock();
            let slot = cache.get(&k(&dir, study)).expect("slot still exists");
            assert_eq!(
                slot.writers, 0,
                "a writer that never commits must give its registration back, \
                 or the slot can never be evicted"
            );
            assert!(
                slot.entry.is_none(),
                "a failed write must NOT install an entry"
            );
        }

        // And the slot is EVICTABLE again: both empty-slot filters require
        // `writers == 0`, so a leaked registration is exactly what pinned it.
        // The assertion is on ELIGIBILITY rather than "this slot was the one
        // chosen": eviction order is LRU-ticket arithmetic, and a test that
        // depended on it would be pinning the scheduler, not the leak.
        {
            let cache = read_lock();
            let slot = cache.get(&k(&dir, study)).expect("slot");
            assert_eq!(slot.writers, 0, "no writer registration may remain");
            assert_eq!(slot.readers, 0, "no reader registration may remain");
            assert!(slot.entry.is_none(), "no entry was installed");
        }
    }

    /// F1 (bound): a stream of FAILED writes must not grow the map past
    /// [`MAX_SLOTS`].
    ///
    /// `abort_write` releases the writer count, but before this fix it never
    /// ran the empty-slot bound. Eviction ran only from `end_write` (success)
    /// and `get_or_load` (read), so a study whose writes always fail (ENOSPC,
    /// an encode failure) left a versioned empty slot behind and nothing ever
    /// reclaimed it - the map grew one slot per distinct failing study.
    #[test]
    fn a_failed_write_reclaims_its_slot_under_the_bound() {
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        // begin_write + abort_write, never end_write: the abort path is the
        // only release these studies get, exactly like a real failed commit.
        for i in 0..(MAX_SLOTS * 2) {
            let study = format!("abort.pad.{i}");
            begin_write(dir.path(), &study);
            abort_write(dir.path(), &study);
        }
        let cache = read_lock();
        assert!(
            cache.len() <= MAX_SLOTS,
            "failed writes must not grow the map past the slot bound: {} slots",
            cache.len()
        );
    }

    /// F2: a stale decode must not overwrite a NEWER entry from another store.
    ///
    /// Two stores share one study UID (the QA vs primary split). An A-reader
    /// snapshots `v0` and starts decoding the old A file. While it is paused, a
    /// B-reader installs a much newer manifest and A's own writer commits. When
    /// the A-reader resumes, its version still matches (`v0` never moved for a
    /// reader) and the slot's entry is `foreign`, so an unconditional foreign
    /// accept installs the PRE-COMMIT A manifest over B - and later A reads
    /// serve stale content until the next write.
    ///
    /// The ordering that makes this decidable is the last-use ticket: the
    /// incoming entry's ticket is taken when its decode STARTED, so an entry
    /// with a NEWER ticket was published after that decode began.
    #[test]
    fn a_stale_decode_cannot_overwrite_a_newer_foreign_entry() {
        let _g = serial();
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        let study = "foreign.race";

        // The A-reader snapshots the version and registers (get_or_load miss).
        let v0 = {
            let mut cache = write_lock();
            let slot = cache.entry(k(dir_a.path(), study)).or_insert(CacheSlot {
                v: 0,
                entry: None,
                last_live: touch(),
                writers: 0,
                readers: 0,
            });
            slot.readers += 1;
            slot.v
        };

        // A NEWER manifest is published by another store while we decode.
        let newer_ticket = touch();
        {
            let mut cache = write_lock();
            let slot = cache.get_mut(&k(dir_a.path(), study)).expect("slot");
            slot.entry = Some(ArcEntry {
                manifest: Arc::new(sample_manifest(study)),
                manifests_dir: dir_b.path().to_path_buf(),
                encoded_len: 1,
                last_use: AtomicU64::new(newer_ticket),
            });
        }

        // The paused A-reader resumes with a STALE decode (older ticket).
        let stale_ticket = newer_ticket.saturating_sub(10);
        let stale = ArcEntry {
            manifest: Arc::new(sample_manifest("stale-content")),
            manifests_dir: dir_a.path().to_path_buf(),
            encoded_len: 1,
            last_use: AtomicU64::new(stale_ticket),
        };
        let installed = {
            let mut cache = write_lock();
            let slot = cache.get_mut(&k(dir_a.path(), study)).expect("slot");
            try_insert(slot, v0, stale)
        };

        assert!(
            !installed,
            "a decode that began BEFORE the current entry must not replace it"
        );
        let cache = read_lock();
        let slot = cache.get(&k(dir_a.path(), study)).expect("slot");
        assert_eq!(
            slot.entry.as_ref().map(|e| e.manifests_dir.clone()),
            Some(dir_b.path().to_path_buf()),
            "the newer store's entry must survive"
        );
    }

    /// The ordering ticket must be taken when the read STARTS.
    ///
    /// `try_insert` orders the incoming entry against a foreign one by their
    /// tickets, and that comparison only means anything if the ticket marks
    /// when the READ began. Taking it after the decode gives a slow reader of
    /// old content the newest ticket, so it beats a manifest another store
    /// published while it was paused, and the reader serves pre-commit content.
    ///
    /// This drives the interleaving through the same lock sections
    /// `get_or_load` uses, so the ticket really is the read-start one.
    #[test]
    fn a_decode_ticket_is_taken_at_read_start() {
        let _g = serial();
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        {
            let mut cache = write_lock();
            cache.clear();
        }
        let study = "ticket.order";

        // Read start: version snapshot + registration + ticket in ONE lock
        // section, exactly as `get_or_load` does before decoding.
        let (v0, read_ticket) = {
            let mut cache = write_lock();
            let ticket = touch();
            let slot = cache.entry(k(dir_a.path(), study)).or_insert(CacheSlot {
                v: 0,
                entry: None,
                last_live: ticket,
                writers: 0,
                readers: 0,
            });
            slot.readers += 1;
            (slot.v, ticket)
        };

        // While the decode is paused, another store publishes an entry whose
        // ticket is minted AFTER the read started - that is what makes it
        // newer.
        let publish_ticket = touch();
        assert!(publish_ticket > read_ticket, "publisher must be newer");
        {
            let mut cache = write_lock();
            let slot = cache.get_mut(&k(dir_a.path(), study)).expect("slot");
            slot.entry = Some(ArcEntry {
                manifest: Arc::new(sample_manifest("newer-published")),
                manifests_dir: dir_b.path().to_path_buf(),
                encoded_len: 1,
                last_use: AtomicU64::new(publish_ticket),
            });
        }

        // The paused read resumes carrying its READ-START ticket - the value
        // `get_or_load` now passes. A ticket taken at insert time would sit
        // above `publish_ticket` and would overwrite the newer entry.
        let installed = {
            let mut cache = write_lock();
            let slot = cache.get_mut(&k(dir_a.path(), study)).expect("slot");
            try_insert(
                slot,
                v0,
                ArcEntry {
                    manifest: Arc::new(sample_manifest("old-content")),
                    manifests_dir: dir_a.path().to_path_buf(),
                    encoded_len: 1,
                    last_use: AtomicU64::new(read_ticket),
                },
            )
        };

        assert!(
            !installed,
            "a decode that began before the newer publish must not replace it"
        );
        let cache = read_lock();
        assert_eq!(
            cache
                .get(&k(dir_a.path(), study))
                .and_then(|s| s.entry.as_ref())
                .map(|e| e.manifests_dir.clone()),
            Some(dir_b.path().to_path_buf()),
            "the newer entry must survive the stale decode"
        );
    }
}
