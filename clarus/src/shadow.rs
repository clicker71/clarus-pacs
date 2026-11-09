// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! B-tree index writer - the `.idx` write side of the STOW path.
//!
//! Writes the three index kinds for every accepted STOW: dates, modality and
//! `sop_owner` rows (append-only files under `idx/`), and buffers rejected-
//! STOW events in an in-memory ring drained by the journal-flush thread into
//! the plaintext log. A write error is logged, never surfaced to the client -
//! the manifest (source of truth) committed before these appends. This module
//! is the PRIMARY index writer: the mirrored tier
//! dual-write) was excised in.
//!
//! All mutable state (index appenders, user registry, log ring) lives behind
//! one `Mutex` so concurrent handlers serialize the short hotpath.
//! The mutex is uncontended in the common case.

#![forbid(unsafe_code)]

use clarus_core::auth::UserRegistry;
use clarus_core::idx::date::DateIndex;
use clarus_core::idx::modality::ModalityIndex;
use clarus_core::idx::rollover::{DailyIndexSet, day_key_from_system_time};
use clarus_core::idx::sop_owner::{DayKey, SopOwnerIndex, SopOwnerSnapshot};
use clarus_core::journal::{self, EventCode, SpscRing};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Thread-safe shadow tier shared via `AppState`.
#[derive(Clone)]
pub struct ShadowTier {
    inner: Arc<Mutex<ShadowInner>>,
    /// Resolved path of the `idx/` directory.
    pub idx_dir: PathBuf,
}

/// The mutable portion of the shadow tier.
pub struct ShadowInner {
    index: DailyIndexSet,
    users: UserRegistry,
    journal: SpscRing,
    /// Memoized `sop_owner` day-segment snapshots (re-loaded on file growth).
    sop_owner_cache: HashMap<DayKey, SopOwnerSnapshot>,
    /// A5: cached sorted day-key list of `sop_owner` segments. Rebuilt once
    /// per day (rollover detection via `sop_owner_days_for`) instead of
    /// `read_dir` + sort per lookup.
    sop_owner_days: Vec<DayKey>,
    /// A5: the day key `sop_owner_days` was built for (rollover detection).
    sop_owner_days_for: Option<DayKey>,
    /// A5: cached on-disk segment length per day. `dual_write` keeps it fresh
    /// (B1), so lookups skip the per-day `stat`; a mismatch with the
    /// snapshot still triggers the len-based reload fallback.
    sop_owner_lens: HashMap<DayKey, u64>,
}

#[derive(Debug)]
pub enum ShadowError {
    Date(String),
    Modality(String),
    SopOwner(String),
}

impl ShadowError {
    /// Build a [`ShadowError`] from a non-allocating warm-append failure tag.
    ///
    /// The warm function may not allocate (it is statically gated), so it
    /// returns only a `Copy` stage tag; the message string is built HERE, on
    /// the already-allocating error path of the caller.
    pub(crate) fn from_stage(stage: WarmAppendStage, what: &str) -> Self {
        let msg = format!("{what} failed");
        match stage {
            WarmAppendStage::Date => ShadowError::Date(msg),
            WarmAppendStage::Modality => ShadowError::Modality(msg),
            WarmAppendStage::SopOwner => ShadowError::SopOwner(msg),
        }
    }

    /// Build a [`ShadowError`] from a cold appender-open failure.
    pub(crate) fn from_writers(e: &clarus_core::idx::rollover::WritersError) -> Self {
        use clarus_core::idx::rollover::WritersError;
        match e {
            WritersError::Date(inner) => ShadowError::Date(inner.to_string()),
            WritersError::Modality(inner) => ShadowError::Modality(inner.to_string()),
            WritersError::SopOwner(inner) => ShadowError::SopOwner(inner.to_string()),
        }
    }
}

/// Which leaf append failed inside a warm (zero-allocation) writer.
///
/// A `Copy` tag, not a `String`: the warm function must not allocate, so it
/// reports the failing stage and the caller renders the message.
#[derive(Debug, Clone, Copy)]
pub enum WarmAppendStage {
    Date,
    Modality,
    SopOwner,
}

impl std::fmt::Display for ShadowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShadowError::Date(e) => write!(f, "date index: {e}"),
            ShadowError::Modality(e) => write!(f, "modality index: {e}"),
            ShadowError::SopOwner(e) => write!(f, "sop_owner index: {e}"),
        }
    }
}

impl std::error::Error for ShadowError {}

impl ShadowTier {
    /// Create the shadow tier rooted at `idx_dir`. Prepares the journal ring
    /// and an empty index registry keyed to today.
    pub fn new(idx_dir: PathBuf) -> Self {
        let today = day_key_from_system_time(SystemTime::now());
        let inner = ShadowInner {
            index: DailyIndexSet::new(idx_dir.clone(), today),
            users: UserRegistry::new(),
            journal: SpscRing::new(),
            sop_owner_cache: HashMap::new(),
            sop_owner_days: Vec::new(),
            sop_owner_days_for: None,
            sop_owner_lens: HashMap::new(),
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            idx_dir,
        }
    }

    /// Append one accepted STOW to the three index kinds: dates idx + modality
    /// idx + sop_owner.
    ///
    /// This IS the primary index write path (the SQLite tier this fn once
    /// mirrored was excised in - the name is kept for history, the
    /// behavior is the only write path).
    ///
    /// `blake3` is the BLAKE3 digest of the stored instance bytes; `study_uid`
    /// and `modality` come from the parsed DICOM metadata; `sop_instance_uid`
    /// is the instance's SOPInstanceUID (the `sop_owner` key).
    ///
    /// Deliberately best-effort: any failure here is logged by the caller as
    /// `BTREE_WRITE_FAILED` and does not affect the client response.
    ///
    /// 0810 warm/cold split: the once-per-day COLD setup (open the three
    /// appenders, allocate the modality bucket entry, maintain the memoized
    /// caches) lives HERE; the per-append publish is delegated to
    /// [`Self::dual_write_append`], the statically zero-allocation warm
    /// function the `tools/noalloc_gate.py` gate disassembles.
    pub fn dual_write(
        &self,
        blake3: [u8; 32],
        modality: &str,
        study_uid: &str,
        sop_instance_uid: &str,
    ) -> Result<(), ShadowError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // COLD: open (or reuse) the three appenders in one call. This may
        // allocate a modality bucket entry and open files on FIRST use of the
        // day; after warm-up both are no-ops.
        let (date, modality_writer, sop_owner) = guard
            .index
            .writers_mut(modality)
            .map_err(|e| ShadowError::from_writers(&e))?;

        // WARM: the three leaf appends, statically zero-allocation.
        // Non-allocating failure tag -> the caller builds the display string.
        Self::dual_write_append(
            date,
            modality_writer,
            sop_owner,
            &blake3,
            study_uid,
            sop_instance_uid,
        )
        .map_err(|stage| ShadowError::from_stage(stage, "append"))?;

        // COLD: fold this process's own append into the memoized snapshot and
        // the cached segment length (the mutex is already held), so the next
        // decision lookup skips the file re-load for this row, and make the
        // cached day list see a day created by this very append.
        let day = *guard.index.current_day();
        if let Some(snap) = guard.sop_owner_cache.get_mut(&day) {
            snap.append_row(sop_instance_uid.as_bytes(), study_uid.as_bytes());
        }
        if let Some(len) = guard.sop_owner_lens.get_mut(&day) {
            *len += clarus_core::idx::sop_owner::ROW_LEN as u64;
        }
        // The FIRST row of a day CREATES that day's file: the cached day list
        // (normally rebuilt only at rollover) must see the new key, or a
        // lookup before the next rollover would miss the day entirely (caught
        // by access_journal_smoke's U-line scenario). The list is sorted
        // ascending and today is the newest key, so the new day belongs at the
        // tail - O(1) check, no per-row scan.
        if guard.sop_owner_days.last() != Some(&day) {
            guard.sop_owner_days.push(day);
            guard.sop_owner_days.sort_unstable();
        }

        Ok(())
    }

    /// WARM per-append publish: write the date, modality and `sop_owner` rows.
    ///
    /// ZERO-ALLOCATION IN EVERY BRANCH - enforced statically by
    /// `tools/noalloc_gate.py` (which disassembles this symbol in the linked
    /// gate profile). It therefore:
    /// - takes the already-open appenders (the caller did the fallible,
    ///   allocating OPEN),
    /// - uses a fixed staging buffer for each row (no `Vec`),
    /// - never indexes (only `.get()`-free fixed-size `[u8; N]` writes),
    /// - never panics or `unwrap`s: every fallible step returns a
    ///   [`WarmAppendStage`] tag, and the caller renders the message.
    ///
    /// `#[inline(never)]` keeps the symbol materialized so the gate can find
    /// it: the body is small enough that LTO would otherwise fold it into the
    /// caller and leave the gate vacuous.
    #[inline(never)]
    pub fn dual_write_append(
        date: &mut DateIndex,
        modality: &mut ModalityIndex,
        sop_owner: &mut SopOwnerIndex,
        blake3: &[u8; 32],
        study_uid: &str,
        sop_instance_uid: &str,
    ) -> Result<(), WarmAppendStage> {
        // 1) Date index: blake3 hash row for the study.
        let mut date_row = [0u8; clarus_core::idx::date::ROW_LEN];
        date.append_warm(blake3, &mut date_row)
            .map_err(|_| WarmAppendStage::Date)?;

        // 2) Modality index: study UID row under the modality bucket. The warm
        // encoder never panics or bounds-checks: a UID that does not fit the
        // fixed buffer is a cold writer's concern (it already validated the
        // length), reported here as a plain failure tag.
        let mut modality_row = [0u8; 128];
        let filled = clarus_core::idx::modality::format_row_warm(study_uid, &mut modality_row)
            .ok_or(WarmAppendStage::Modality)?;
        modality
            .append_warm(filled)
            .map_err(|_| WarmAppendStage::Modality)?;

        // 3) sop_owner index: SOPInstanceUID -> StudyInstanceUID row (the
        // conflict oracle).
        let mut sop_row = [0u8; clarus_core::idx::sop_owner::ROW_LEN];
        let row = clarus_core::idx::sop_owner::encode_row_warm(
            sop_instance_uid.as_bytes(),
            study_uid.as_bytes(),
            &mut sop_row,
        )
        // A row that does not fit the day segment's fixed geometry is the cold
        // writer's concern; it never allocates here.
        .map_err(|_| WarmAppendStage::SopOwner)?;
        sop_owner
            .append_warm(row)
            .map_err(|_| WarmAppendStage::SopOwner)?;

        Ok(())
    }

    /// Record a fully rejected STOW request as a code-2 journal token.
    ///
    /// The caller guards this with `accum.accepted == 0`; used only when the
    /// whole request failed or conflicted. Payload is TEXT `failed=N
    /// conflicts=M`, kept within [`journal::MAX_PAYLOAD`] so the token always
    /// fits (the widest u32 pair is exactly 38 bytes). `format_plaintext` prints the
    /// payload verbatim after the tag -> `[ERROR] STOW rejected
    /// failed=2 conflicts=1`. Best-effort: never surfaces to the client.
    pub fn record_stow_rejected(&self, failed: u32, conflicts: u32, user: Option<&str>, ip: &[u8]) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let token = guard.users.resolve(user.unwrap_or("anonymous"));
        let payload = format!("failed={failed} conflicts={conflicts}");
        let t = journal::make_token(EventCode::STOW_REJECTED, token, ip, payload.as_bytes());
        // `push` returns `false` when it had to evict the oldest buffered event
        // to make room. That is the documented fail-open policy (a full journal
        // must never block a STOW), but the loss has to be visible: without a
        // counter an audit trail with a hole is indistinguishable from a
        // complete one.
        if !guard.journal.push(t) {
            crate::metrics::JOURNAL_EVICTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Drain buffered journal tokens into `out` (for the plaintext flush task).
    pub fn drain_tokens(&self, out: &mut Vec<u8>) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(t) = guard.journal.pop() {
            let ev = t.decode();
            let user = guard.users.by_token(ev.user_token).map(|u| u.name.clone());
            journal::format_plaintext(&ev, user.as_deref(), out);
            out.push(b'\n');
        }
    }

    /// Number of buffered (unflushed) journal tokens.
    pub fn pending_tokens(&self) -> usize {
        self.inner.lock().map(|g| g.journal.len()).unwrap_or(0)
    }

    /// Look up the study owner of a SOPInstanceUID in the `sop_owner` shadow
    /// index. `None` when the SOP was never seen.
    ///
    /// A5: the sorted day list and per-day segment lengths are cached in
    /// [`ShadowInner`]; the day list is rebuilt only at midnight rollover
    /// (or first use), and `dual_write` keeps the lengths fresh (B1), so a
    /// lookup costs no `read_dir` and no per-day `stat`. The len-based
    /// reload stays as the fallback: a snapshot whose length disagrees with
    /// the cache is re-loaded from disk.
    ///
    /// 0810 warm/cold split: the COLD fallible steps (day-list rebuild,
    /// snapshot load/reload) stay HERE; the pure search over the already
    /// cached snapshots is delegated to [`Self::sop_owner_study_warm`], the
    /// statically zero-allocation warm function the no-alloc gate disassembles.
    pub fn sop_owner_study(&self, sop: &str) -> Option<String> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // COLD: rebuild the day list when the writer's day changed (rollover)
        // or on first use. Old days are immutable, so their snapshots stay
        // valid.
        let cur_day = *guard.index.current_day();
        if guard.sop_owner_days_for != Some(cur_day) {
            guard.sop_owner_days = list_sop_owner_days(&self.idx_dir);
            guard.sop_owner_days_for = Some(cur_day);
        }

        // COLD: make sure every candidate day has a valid cached snapshot.
        // Most-recent day-segments are searched first (warm fn searches the
        // same order); a first-use load or a len-based reload may allocate.
        // Index-based loop: the 8-byte day key is Copy, so the immutable
        // borrow of the cached list ends at the copy and `guard` stays mutable
        // inside the body (no per-lookup Vec clone).
        let day_count = guard.sop_owner_days.len();
        for i in 0..day_count {
            let day = guard.sop_owner_days[i];
            if !guard.sop_owner_cache.contains_key(&day)
                && let Ok(Some(snap)) = SopOwnerSnapshot::load(&self.idx_dir, &day)
            {
                guard.sop_owner_lens.insert(day, snap.current_len());
                guard.sop_owner_cache.insert(day, snap);
            }
            // Len-based reload fallback: the cached length is the truth for
            // this process's appends; a disagreement means the file grew
            // behind our back - re-load exactly like the old path did.
            let stale = match guard.sop_owner_cache.get(&day) {
                Some(snap) => guard.sop_owner_lens.get(&day) != Some(&snap.current_len()),
                None => false,
            };
            if stale {
                guard.sop_owner_cache.remove(&day);
                if let Ok(Some(snap)) = SopOwnerSnapshot::load(&self.idx_dir, &day) {
                    guard.sop_owner_lens.insert(day, snap.current_len());
                    guard.sop_owner_cache.insert(day, snap);
                }
            }
        }

        // WARM: search the cached snapshots, newest day first.
        let owner = Self::sop_owner_study_warm(&guard.sop_owner_days, &guard.sop_owner_cache, sop)?;
        // Materialize the answer (allocates once: the caller's `String`).
        Some(String::from_utf8_lossy(owner).into_owned())
    }

    /// WARM lookup: search the cached `sop_owner` snapshots, newest day first.
    ///
    /// ZERO-ALLOCATION IN EVERY BRANCH - statically gated by
    /// `tools/noalloc_gate.py`. It borrows the cached day list and snapshot map
    /// (no clone), iterates by index over `[u8; 8]` day keys (no `Vec`), and
    /// returns a BORROWED owner slice: the `String` materialization stays in
    /// the caller, so this body never calls the allocator.
    ///
    /// `#[inline(never)]` keeps the symbol materialized for the gate (the body
    /// is small and LTO would otherwise fold it into the caller).
    #[inline(never)]
    pub fn sop_owner_study_warm<'a>(
        days: &[DayKey],
        cache: &'a HashMap<DayKey, SopOwnerSnapshot>,
        sop: &str,
    ) -> Option<&'a [u8]> {
        let mut i = days.len();
        while i > 0 {
            i -= 1;
            // Bounds-safe: `get` + `?` instead of `[]`, so no
            // `panic_bounds_check` is emitted.
            let day = days.get(i)?;
            if let Some(owner) = cache.get(day).and_then(|s| s.find_warm(sop.as_bytes())) {
                return Some(owner);
            }
        }
        None
    }
}

/// A5: scan the `sop_owner` directory once and return the sorted day keys
/// (the `read_dir` + sort tax moved out of the per-lookup path).
fn list_sop_owner_days(idx_dir: &Path) -> Vec<DayKey> {
    let sop_owner_dir = idx_dir.join("sop_owner");
    let mut days: Vec<DayKey> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&sop_owner_dir) {
        for ent in rd.flatten() {
            let name = ent.file_name();
            let name = name.to_string_lossy();
            // `YYYYMMDD.idx` (12 chars). Guard the slice to keep the
            // compiler happy about the `[..8]` prefix split.
            if name.ends_with(".idx")
                && name.len() == 12
                && let Ok(bytes) = name.as_bytes()[..8].try_into()
            {
                days.push(bytes);
            }
        }
    }
    days.sort_unstable();
    days
}

/// Compute the BLAKE3 digest of the stored DICOM bytes (the date-index key).
/// Routed through the `CasHasher` facade so every content-derived key uses the
/// single hash backend.
pub fn blake3_of(bytes: &[u8]) -> [u8; 32] {
    clarus_core::hash::cas_hash_one(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_stow_rejected_emits_code2_plaintext() {
        // The journal ring and user registry are purely in-memory, so the
        // on-disk dirs are never touched by this path.
        let tier = ShadowTier::new(PathBuf::from("idx-test"));
        tier.record_stow_rejected(2, 1, Some("dr.smith"), &[127, 0, 0, 1]);
        assert_eq!(tier.pending_tokens(), 1);

        let mut out = Vec::new();
        tier.drain_tokens(&mut out);
        let text = String::from_utf8_lossy(&out);
        // The token decoded as code-2 STOW_REJECTED and formatted as
        // plaintext carries the tag and the `failed=`/`conflicts=` payload.
        assert!(text.contains("[ERROR] STOW rejected"), "got: {text}");
        assert!(text.contains("failed=2"), "got: {text}");
        assert!(text.contains("conflicts=1"), "got: {text}");
        assert_eq!(tier.pending_tokens(), 0, "token drained exactly once");
    }
}
