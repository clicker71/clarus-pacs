// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The B-tree study search - candidate generation from `.idx`, then manifest
//! mmap + filter: the study search path.
//!
//! # Query plan
//!
//! 1. **Modality acceleration.** When `QueryFilter::modality()` is set, the
//!    `modality/{MOD}/YYYYMMDD.idx` rows (restricted to the filter's date
//!    range when present) yield the candidate `study_uid`s directly.
//! 2. **Direct UID lookup.** `StudyInstanceUID` names exactly one manifest.
//! 3. **Cold path.** With no accelerator, the whole `manifests/` directory is
//!    scanned and each manifest is parsed and filtered (the plan's <10 ms P99
//!    full-scan rule for <100K studies).
//! 4. **Manifest filter + pagination.** Each candidate manifest is mmap-ed,
//!    filtered by the remaining search keys, collected in study-UID order, and
//!    paginated with `offset`/`limit`.
//!
//! The manifest is the source of truth; `.idx` only narrows the candidate set,
//! so a stale index never loses a study (correctness never depends on index
//! freshness).

use crate::query::filter::QueryFilter;
use std::io;
use std::path::PathBuf;

/// Result of a study search - the manifest plus its study-UID key.
#[derive(Debug, Clone)]
pub struct StudySearchResult {
    /// The study UID this result belongs to (also the manifest file stem).
    pub study_uid: String,
    /// The parsed study-level QIDO attributes.
    pub qido: crate::manifest::StudyQido,
}

/// Deterministic sort key for a `/studies` result: `(study_date, study_uid)`.
///
/// `study_date` is the 8-digit YYYYMMDD held as `AttrValue::UInt` on tag
/// 0008,0020; a study with no date yields `u64::MAX` so it sorts LAST (mirror
/// of SQLite `ORDER BY study_date ASC` NULLS-last semantics). `study_uid` is
/// the stable secondary key (SQLite used a surrogate `id`; the B-tree tier has
/// none, so the StudyInstanceUID/manifest-stem is the contract).
fn study_sort_key(r: &StudySearchResult) -> (u64, &str) {
    let date = r
        .qido
        .attrs
        .iter()
        .find(|(t, _)| *t == 0x0008_0020)
        .and_then(|(_, v)| match v {
            crate::manifest::AttrValue::UInt(u) => Some(*u),
            crate::manifest::AttrValue::Text(s) => s.parse().ok(),
            _ => None,
        })
        .unwrap_or(u64::MAX);
    (date, r.study_uid.as_str())
}

/// Aggregated error for a study search.
#[derive(Debug)]
pub enum StudySearchError {
    /// The manifests root does not exist or is not a directory.
    BogusRoot(PathBuf),
    /// Filesystem error while scanning or reading.
    Io(io::Error),
    /// A manifest named a valid study UID but could not be decoded.
    Manifest(String),
}

impl std::fmt::Display for StudySearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StudySearchError::BogusRoot(p) => write!(f, "manifests root is not a directory: {p:?}"),
            StudySearchError::Io(e) => write!(f, "study search I/O error: {e}"),
            StudySearchError::Manifest(uid) => write!(f, "undecodable manifest for study {uid}"),
        }
    }
}

impl std::error::Error for StudySearchError {}

/// The collection engine over `manifests/` and `idx/`.
///
/// Cheap to clone (holds two `PathBuf`s); callers may build one per request.
#[derive(Debug, Clone)]
pub struct StudyScanner {
    /// The `manifests/` directory (source of truth for QIDO attributes).
    pub manifests_dir: PathBuf,
    /// The `idx/` directory (accelerators: `dates/`, `modality/`).
    pub idx_dir: PathBuf,
    /// an optional in-RAM preload of study QIDO records. When `ready`,
    /// the cold (no-accelerator) path resolves candidates and filters from
    /// memory instead of a `read_dir` + per-manifest mmap/decode. `None` means
    /// "no index attached" - cold reads fall back to the manifest scan.
    index: Option<crate::query::study_index::StudyQidoIndex>,
}

impl StudyScanner {
    /// Construct a scanner rooted at the manifests and idx directories.
    pub fn new(manifests_dir: PathBuf, idx_dir: PathBuf) -> Self {
        Self {
            manifests_dir,
            idx_dir,
            index: None,
        }
    }

    /// Attach a shared RAM study index. Replacing an existing index is atomic
    /// (the index itself is behind an `Arc<RwLock<..>>`).
    pub fn attach_index(&mut self, index: crate::query::study_index::StudyQidoIndex) {
        self.index = Some(index);
    }

    /// The optional shared index handle, if one is attached.
    pub fn index(&self) -> Option<&crate::query::study_index::StudyQidoIndex> {
        self.index.as_ref()
    }

    // -- invalidation surface ---------------------------------------
    //
    // The index is a cache of the manifest study QIDO records. Whenever the
    // study set changes (a STOW committed a study, or an erase tombstoned it),
    // the caller reflects that in the attached index. These are no-ops when no
    // index is attached (the cold path then just scans from disk, correct).

    /// Reflect a committed STOW (new study or re-STOW merge): upsert the
    /// study's QIDO record.
    pub fn on_study_committed(&self, study_uid: &str, attrs: &crate::stow::StudyManifestAttrs) {
        if let Some(index) = self.index.as_ref() {
            index.upsert(study_uid, attrs);
        }
    }

    /// Reflect an erase tombstone: remove the study's QIDO record.
    pub fn on_study_erased(&self, study_uid: &str) {
        if let Some(index) = self.index.as_ref() {
            index.remove(study_uid);
        }
    }

    /// Lazily (re)build the attached index from the manifests directory. Safe
    /// to call from a background thread: until `ready()`, cold reads fall back
    /// to scanning. No-op when no index is attached.
    pub fn rebuild_index(&self) {
        if let Some(index) = self.index.as_ref() {
            index.build_and_swap(&self.manifests_dir);
        }
    }

    /// Run a study search, returning the matching studies in study-UID order
    /// together with the TOTAL number of matches BEFORE pagination.
    ///
    /// The total is what PS3.18 8.3.4.4.1 needs for the truncation Warning
    /// (`remaining = matches - offset - returned`); pagination alone cannot
    /// recover it because the tail is discarded.
    ///
    /// A missing `manifests/` directory yields an empty result (the tier is
    /// empty), not an error - QIDO against a fresh store must return `[]`.
    pub fn search(
        &self,
        filter: &QueryFilter,
    ) -> Result<(Vec<StudySearchResult>, usize), StudySearchError> {
        // 1) Candidate set.
        let candidates = self.candidate_study_uids(filter)?;

        // 1b) Pre-build the expected (tag, predicate) map once - avoids
        //     a HashMap allocation per candidate manifest.
        let expected = filter.build_expected();

        // 2) Resolve each candidate: mmap manifest, shallow-decode, filter.
        let mut out = Vec::new();
        for uid in &candidates {
            if let Some(res) = self.load_if_matches(uid, filter, &expected)? {
                out.push(res);
            }
        }

        // 2b) Deterministic /studies ordering: study_date ASC, then study_uid
        //     ASC tiebreak. This mirrors the SQLite-era `find_studies`
        //     (ORDER BY study_date ASC, id ASC): the B-tree tier has no
        //     surrogate id, so the stable secondary key is the StudyInstanceUID
        //     (the manifest stem). Contract: study date is the 8-digit YYYYMMDD
        //     held as AttrValue::UInt; a study with NO date sorts LAST (mirrors
        // SQLite NULLS LAST). Both the cold manifest scan and the RAM
        //     index route through this single assembly point, so both produce
        //     byte-identical ordering to the SQLite fallback.
        out.sort_unstable_by(|a, b| study_sort_key(a).cmp(&study_sort_key(b)));

        // 3) Pagination (offset then limit) after deterministic ordering. The
        //    pre-pagination length is reported so the caller can compute the
        //    PS3.18 8.3.4.4.1 `remaining` count for the truncation Warning.
        let matches = out.len();
        let offset = filter.offset.unwrap_or(0) as usize;
        if offset >= matches {
            return Ok((Vec::new(), matches));
        }
        let limit = filter.limit.map(|l| l as usize).unwrap_or(matches - offset);
        Ok((out.into_iter().skip(offset).take(limit).collect(), matches))
    }

    /// Series-level QIDO over one study manifest.
    ///
    /// The study is known from the path: the caller decodes the single
    /// `manifests/{study_uid}.manifest` and passes it in. Results are **borrowed
    /// references** written into the caller-owned `out` (cleared first) and
    /// sorted in place - steady-state zero allocation. See
    /// [`crate::query::series::search_series`].
    pub fn search_series<'a>(
        &self,
        manifest: &'a crate::manifest::Manifest,
        filter: &crate::query::SeriesFilter,
        out: &mut Vec<&'a crate::manifest::SeriesRecord>,
    ) {
        crate::query::series::search_series(manifest, filter, out)
    }

    /// Instance-level QIDO over one study manifest.
    ///
    /// Returns `false` when no series with `series_uid` exists. See
    /// [`crate::query::series::search_instances`].
    ///
    /// `expose` is threaded straight through: the caller decides whether this
    /// request may see sequestered instances, and which reasons it may lift.
    pub fn search_instances<'a>(
        &self,
        manifest: &'a crate::manifest::Manifest,
        series_uid: &str,
        filter: &crate::query::InstanceFilter,
        out: &mut Vec<&'a crate::manifest::InstanceRecord>,
        expose: bool,
    ) -> bool {
        crate::query::series::search_instances(manifest, series_uid, filter, out, expose)
    }

    /// Determine the candidate study UIDs from the `.idx` accelerators.
    ///
    /// Priority: `StudyInstanceUID` exact -> modality idx (date-restricted) ->
    /// cold full scan of `manifests/`.
    fn candidate_study_uids(&self, filter: &QueryFilter) -> Result<Vec<String>, StudySearchError> {
        // Direct UID: exactly one candidate.
        if let Some(uid) = filter.study_instance_uid.as_deref() {
            // Only candidate if the manifest actually exists.
            let path = self.manifests_dir.join(format!("{uid}.manifest"));
            if path.is_file() {
                return Ok(vec![uid.to_string()]);
            }
            return Ok(Vec::new());
        }

        // Modality acceleration: study UIDs direct from the idx rows.
        //
        // PS3.18 makes `Modality` multi-valued (`Modality=CT,MR`), and the
        // buckets are per modality: passing the raw value built the path
        // `modality/CT,MR`, which exists for no study, so a legitimate query
        // answered with an empty candidate set. Each element is looked up in
        // its own bucket and the results unioned.
        if let Some(modality) = filter.modality() {
            let mut union: Vec<String> = Vec::new();
            for value in modality.split(',') {
                let value = value.trim();
                // A path element that can escape the index tree is not a
                // modality; a CS value cannot contain `/`, `\` or `..`.
                if value.is_empty() || value.contains('/') || value.contains('\\') || value == ".."
                {
                    continue;
                }
                union.extend(self.modality_candidates(value, filter)?);
            }
            if !union.is_empty() {
                union.sort();
                union.dedup();
                return Ok(union);
            }
            // No usable bucket: fall through to the full scan rather than
            // answering "nothing" for a query the archive may well satisfy.
        }

        // Fuzzymatch acceleration: union the per-gram `.idx` rows (a safe
        // superset for 4+ grams; bitap is the final judge). Falls back to a
        // full scan when the n-gram index has no rows or the name is too short.
        if filter.fuzzymatch
            && let Some(uid) = self.fuzzy_patient_candidates(filter)?
        {
            return Ok(uid);
        }

        // Cold path: no accelerator - list the manifests directory.
        self.list_all_study_uids()
    }

    /// Candidate study UIDs for a `PatientName` fuzzymatch, from the n-gram `.idx`.
    ///
    /// The query name is broken into n-grams; each gram's daily `.idx` rows are
    /// read and UNIONED - a study appearing in any query gram is a candidate.
    /// This is a safe superset for 4+ grams (a 1-edit match keeps at least one
    /// intact gram); `bitap` remains the final judge. Names with fewer than 4
    /// grams (<=5 scalars) fall back. Returns `Ok(None)` to signal "no usable
    /// index", which the caller treats as a fall through to the full manifest
    /// scan.
    fn fuzzy_patient_candidates(
        &self,
        filter: &QueryFilter,
    ) -> Result<Option<Vec<String>>, StudySearchError> {
        // The accelerator only narrows a no-wildcard PatientName fuzzymatch.
        let Some(name) = filter.patient_name.as_deref() else {
            return Ok(None);
        };
        if name.bytes().any(|b| b == b'*' || b == b'?') {
            return Ok(None);
        }
        // Fuzzy path only: normalize PN separators to spaces so the query grams
        // line up with the space-separated keys written at indexing time.
        let normalized = crate::idx::ngram::fuzzy_normalize(name);
        let grams = crate::idx::ngram::name_ngrams(&normalized);
        if grams.is_empty() {
            return Ok(None);
        }
        // Union soundness guard: at 4+ grams a 1-edit match (which perturbs at
        // most 3 windows) necessarily leaves at least one n-gram intact, so
        // the union of the per-gram row sets is a safe superset. Below 4 grams
        // (names of <=5 chars/scalars) one edit can touch EVERY gram, so the
        // union is NOT guaranteed to contain the true match - fall back to the
        // full manifest scan (bitap is still the final judge).
        if grams.len() < 4 {
            return Ok(None);
        }

        // Union candidates: a study counts as a candidate if it appears in ANY
        // of the query's n-grams (a 1-edit match shares at least one intact
        // gram). This is deliberately a superset; bitap filters it down.
        let mut candidates: Vec<String> = Vec::new();
        for gram in &grams {
            let rows = self.ngram_day_rows(crate::idx::ngram::NameKind::Patient, gram)?;
            // A missing gram's index means we cannot narrow honestly - fall back.
            if rows.is_empty() {
                return Ok(None);
            }
            candidates.extend(rows);
        }
        candidates.sort();
        candidates.dedup();
        Ok(Some(candidates))
    }

    /// Gather every study UID stored for one n-gram across all daily `.idx` files.
    ///
    /// Days are discovered by listing the n-gram's bucket directory, so a
    /// missing file for a particular day is irrelevant - whatever buckets exist
    /// are the rows. `Ok(Vec::new())` means the n-gram has no index at all.
    fn ngram_day_rows(
        &self,
        kind: crate::idx::ngram::NameKind,
        gram: &str,
    ) -> Result<Vec<String>, StudySearchError> {
        let bucket = self
            .idx_dir
            .join(crate::idx::ngram::kind_root(kind))
            .join(gram);
        if !bucket.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&bucket).map_err(StudySearchError::Io)? {
            let entry = entry.map_err(StudySearchError::Io)?;
            let fname = entry.file_name().to_string_lossy().into_owned();
            let Some(day) = fname.strip_suffix(".idx") else {
                continue;
            };
            if day.len() != 8 {
                continue;
            }
            let uids = crate::idx::ngram::read_study_uids(&self.idx_dir, kind, gram, day)
                .map_err(StudySearchError::Io)?;
            out.extend(uids);
        }
        Ok(out)
    }

    /// Read candidate study UIDs out of `modality/{MOD}/YYYYMMDD.idx`.
    ///
    /// When a date range is given, only the days inside `[from,to]` are read;
    /// otherwise every day present under the modality bucket is scanned. Days
    /// are walked by directory listing so a missing `.idx` for a specific day
    /// is irrelevant - whatever buckets exist are the candidates.
    fn modality_candidates(
        &self,
        modality: &str,
        filter: &QueryFilter,
    ) -> Result<Vec<String>, StudySearchError> {
        let bucket = self.idx_dir.join("modality").join(modality);
        if !bucket.is_dir() {
            return Ok(Vec::new());
        }
        let (from, to) = filter.date_bounds();
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&bucket).map_err(StudySearchError::Io)? {
            let entry = entry.map_err(StudySearchError::Io)?;
            let fname = entry.file_name().to_string_lossy().into_owned();
            if !fname.ends_with(".idx") {
                continue;
            }
            let day = &fname[..fname.len() - ".idx".len()];
            if day.len() != 8 || !in_date_range(day, from, to) {
                continue;
            }
            let day_key = day_bytes(day);
            let uids = crate::idx::modality::read_study_uids(&self.idx_dir, modality, &day_key)
                .map_err(StudySearchError::Io)?;
            out.extend(uids);
        }
        // De-duplicate: a study may span multiple modality rows across days.
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// List every study UID from the RAM index when it is ready,
    /// else from the `manifests/` directory (cold path).
    fn list_all_study_uids(&self) -> Result<Vec<String>, StudySearchError> {
        // a ready in-RAM index answers the cold candidate list without
        // a directory walk. The index keys are the same `.manifest` file stems
        // and are returned in the same ascending order as the disk scan, so
        // candidate sets and QIDO results are byte-identical to the reference.
        if let Some(index) = self.index.as_ref()
            && index.ready()
        {
            return Ok(index.all_uids());
        }
        if !self.manifests_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.manifests_dir).map_err(StudySearchError::Io)? {
            let entry = entry.map_err(StudySearchError::Io)?;
            let fname = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = fname.strip_suffix(".manifest") {
                out.push(stem.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// mmap + decode one manifest, run the filter, and emit a result if it
    /// matches. A corrupt manifest for a candidate is an error - the plan's
    /// lazy recovery should have repaired it; surfacing it is honest.
    ///
    /// when a ready RAM index holds the study QIDO record, the candidate
    /// is resolved and filtered from memory (no mmap, no decode_study_only).
    /// The index attrs are byte-identical to the manifest study section, so the
    /// matched results are byte-identical to the disk reference.
    fn load_if_matches(
        &self,
        uid: &str,
        filter: &QueryFilter,
        expected: &std::collections::HashMap<u32, crate::query::Predicate<'_>>,
    ) -> Result<Option<StudySearchResult>, StudySearchError> {
        if let Some(index) = self.index.as_ref()
            && index.ready()
        {
            return Ok(load_if_matches_index(index, uid, filter, expected));
        }
        let path = self.manifests_dir.join(format!("{uid}.manifest"));
        // QIDO only needs study-level QIDO attributes - skip expensive
        // series/instance/frame decode via decode_study_only (shallow read).
        //
        // A manifest that is GONE is a stale index row (erased study, torn
        // tail, an n-gram line written for a study that never committed), not
        // a broken request: one such candidate used to fail the whole QIDO
        // answer. Skipping it costs that study, which no longer exists.
        let manifest = match crate::manifest::read_manifest_study_only(&path) {
            Ok(m) => m,
            Err(crate::manifest::ManifestReadError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(_) => return Err(StudySearchError::Manifest(uid.to_string())),
        };
        if !crate::query::match_study_attrs_prebuilt(filter, &manifest, expected) {
            return Ok(None);
        }
        // JSON streaming is delegated to `json.rs` (C-009); here we hand the
        // caller the study attributes so it can emit without re-parsing.
        Ok(Some(StudySearchResult {
            study_uid: uid.to_string(),
            qido: manifest.study,
        }))
    }
}

/// resolve `uid` from the RAM index and filter it. Returns `None` when
/// the study is not indexed (e.g. a corrupt manifest was skipped at build).
fn load_if_matches_index(
    index: &crate::query::study_index::StudyQidoIndex,
    uid: &str,
    filter: &QueryFilter,
    expected: &std::collections::HashMap<u32, crate::query::Predicate<'_>>,
) -> Option<StudySearchResult> {
    let qido = index.get(uid)?;
    // ModalitiesInStudy (0008,0061) is not in `attrs` (see filter.rs) - it is
    // checked against the derived aggregate field here, mirroring the cold
    // path's `match_study_attrs_prebuilt`. The same holds for the
    // (0010,1002) Other Patient IDs check: the cold path runs it through
    // `match_study_attrs_prebuilt`, so skipping it here made an indexed and a
    // non-indexed server answer DIFFERENTLY to `00101002.00100020=X`.
    if !filter.matches_prebuilt(&qido.attrs, expected)
        || !filter.modalities_in_study_match(qido.modalities_in_study.as_deref())
        || !filter.other_patient_ids_match_prebuilt(&qido.attrs)
    {
        return None;
    }
    Some(StudySearchResult {
        study_uid: uid.to_string(),
        qido: (*qido).clone(),
    })
}

/// Whether a day key lies within an inclusive `[from, to]` date range.
fn in_date_range(day: &str, from: Option<&str>, to: Option<&str>) -> bool {
    let d = day.parse::<u64>().unwrap_or(0);
    if let Some(f) = from.and_then(|f| f.parse::<u64>().ok())
        && d < f
    {
        return false;
    }
    if let Some(t) = to.and_then(|t| t.parse::<u64>().ok())
        && d > t
    {
        return false;
    }
    true
}

/// Parse an 8-char day string into a `[u8; 8]` day key.
fn day_bytes(day: &str) -> [u8; 8] {
    let mut b = [b'0'; 8];
    let bytes = day.as_bytes();
    let n = bytes.len().min(8);
    b[..n].copy_from_slice(&bytes[..n]);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idx::rollover::DailyIndexSet;
    use crate::manifest::write_manifest;
    use crate::manifest::{AttrValue, Manifest, StudyQido, SysHeader};
    use std::path::Path;

    /// Write a minimal manifest for `study_uid` into `manifests_dir`.
    fn seed_manifest(manifests_dir: &Path, study_uid: &str, name: &str, date: u64) {
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: 1,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![
                    (0x0008_0020, AttrValue::UInt(date)),
                    (0x0008_0060, AttrValue::Text("CT".into())),
                    (0x0010_0010, AttrValue::Text(name.to_string())),
                    (0x0020_000D, AttrValue::Text(study_uid.to_string())),
                ],
                ..Default::default()
            },
            series: vec![],
        };
        let path = manifests_dir.join(format!("{study_uid}.manifest"));
        write_manifest(&path, &m).expect("write manifest");
    }

    #[test]
    fn cold_path_returns_all_studies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "alice", 20260101);
        seed_manifest(&manifests, "1.2.2", "bob", 20260202);

        let scanner = StudyScanner::new(manifests.clone(), dir.path().join("idx"));
        let (res, _) = scanner.search(&QueryFilter::default()).expect("search");
        assert_eq!(res.len(), 2);
    }

    /// `Modality=CT,MR` must union the buckets, not look up one path.
    ///
    /// PS3.18 makes Modality multi-valued. Building the bucket path from the raw
    /// value produced `modality/CT,MR`, which exists for no study, so a valid
    /// query answered with an empty candidate set.
    #[test]
    fn a_multi_valued_modality_unions_its_buckets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "alice", 20260101);
        seed_manifest(&manifests, "1.2.2", "bob", 20260101);

        let idx = dir.path().join("idx");
        let mut set = DailyIndexSet::new(idx.clone(), *b"20260101");
        for (uid, modality) in [("1.2.1", "CT"), ("1.2.2", "MR")] {
            let mut row = [0u8; 128];
            let filled = crate::idx::modality::format_row(uid, &mut row);
            set.modality_writer(modality)
                .expect("mod")
                .append(filled)
                .expect("row");
        }

        let scanner = StudyScanner::new(manifests.clone(), idx);
        let f = QueryFilter {
            modality: Some("CT, MR".into()),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        let uids: Vec<&str> = res.iter().map(|r| r.study_uid.as_str()).collect();
        assert!(
            uids.contains(&"1.2.1") && uids.contains(&"1.2.2"),
            "both buckets contribute candidates: {uids:?}"
        );
    }

    #[test]
    fn modality_idx_accelerates_and_filters() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "alice", 20260101);
        seed_manifest(&manifests, "1.2.2", "bob", 20260202);

        // Build the modality accelerator for "CT" on day 20260101 with uid 1.2.1.
        let idx = dir.path().join("idx");
        let mut set = DailyIndexSet::new(idx.clone(), *b"20260101");
        let mut row = [0u8; 128];
        let filled = crate::idx::modality::format_row("1.2.1", &mut row);
        set.modality_writer("CT")
            .expect("mod")
            .append(filled)
            .expect("row");

        let scanner = StudyScanner::new(manifests.clone(), idx);
        let f = QueryFilter {
            modality: Some("CT".into()),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].study_uid, "1.2.1");
    }

    #[test]
    fn direct_uid_lookup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.9", "carol", 20260303);

        let scanner = StudyScanner::new(manifests.clone(), dir.path().join("idx"));
        let f = QueryFilter {
            study_instance_uid: Some("1.2.9".into()),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].study_uid, "1.2.9");
    }

    #[test]
    fn pagination_applies_offset_and_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        for i in 1..=5u32 {
            seed_manifest(&manifests, &format!("1.2.{i}"), "u", 20260000 + i as u64);
        }
        let scanner = StudyScanner::new(manifests.clone(), dir.path().join("idx"));
        let f = QueryFilter {
            offset: Some(1),
            limit: Some(2),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].study_uid, "1.2.2");
        assert_eq!(res[1].study_uid, "1.2.3");
    }

    #[test]
    fn fuzzymatch_cold_path_scans_manifests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "Иванов", 20260101);
        seed_manifest(&manifests, "1.2.2", "Петров", 20260202);

        // No n-gram index exists -> the fuzzymatch falls back to a full scan,
        // and the manifest filter still honors the 1-edit match.
        let scanner = StudyScanner::new(manifests.clone(), dir.path().join("idx"));
        let f = QueryFilter {
            patient_name: Some("ивонов".into()), // 1 substitution from "иванов"
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].study_uid, "1.2.1");
    }

    #[test]
    fn fuzzymatch_hot_path_uses_ngram_union() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "Иванов", 20260101);
        seed_manifest(&manifests, "1.2.2", "Петров", 20260202);

        // Build the n-gram index as the STOW writer would (fuzzy-normalized):
        // "Иванов" lowercased -> "иванов" -> four trigram keys, all for 1.2.1.
        let idx = dir.path().join("idx");
        let day = "20260101";
        for gram in crate::idx::ngram::name_ngrams(&crate::idx::ngram::fuzzy_normalize("Иванов"))
        {
            crate::idx::ngram::append(
                &idx,
                crate::idx::ngram::NameKind::Patient,
                &gram,
                "1.2.1",
                day,
            )
            .expect("append gram");
        }

        let scanner = StudyScanner::new(manifests, idx);
        // "Иванов" has 4 trigrams (>= the union guard), so the accelerator is
        // used and the union of the per-gram rows narrows to 1.2.1.
        let f = QueryFilter {
            patient_name: Some("иванов".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].study_uid, "1.2.1");
    }

    #[test]
    fn fuzzymatch_hot_path_rejects_true_nonmatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "Петров", 20260101);

        let idx = dir.path().join("idx");
        let day = "20260101";
        for gram in crate::idx::ngram::name_ngrams("Петров") {
            crate::idx::ngram::append(
                &idx,
                crate::idx::ngram::NameKind::Patient,
                &gram,
                "1.2.1",
                day,
            )
            .expect("append gram");
        }

        let scanner = StudyScanner::new(manifests, idx);
        // "ивн" is 3 scalars -> 1 trigram -> below the 4-gram union guard, so
        // it falls back to a full manifest scan; bitap ("ивн" vs "Петров") is
        // far beyond one edit, so nothing matches.
        let f = QueryFilter {
            patient_name: Some("ивн".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert!(res.is_empty());
    }

    #[test]
    fn studies_ordered_by_date_asc_then_uid_tiebreak() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        // Same date -> UID tiebreak; earlier date -> sorts first.
        seed_manifest(&manifests, "study-b", "B", 20240201);
        seed_manifest(&manifests, "study-a", "A", 20240201);
        seed_manifest(&manifests, "study-early", "E", 20230101);
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&idx).expect("mkdir idx");
        let scanner = StudyScanner::new(manifests, idx);

        let f = QueryFilter::default();
        let (res, _) = scanner.search(&f).expect("search");
        let uids: Vec<&str> = res.iter().map(|r| r.study_uid.as_str()).collect();
        // date asc: study-early(20230101) first, then the 20240201 pair by UID
        // asc (study-a < study-b).
        assert_eq!(uids, vec!["study-early", "study-a", "study-b"]);
    }

    #[test]
    fn study_sort_key_missing_date_sorts_last() {
        // Directly check the missing-date fallback: a result with NO 00080020
        // attribute must sort after any dated result.
        fn res(uid: &str, date: Option<u64>) -> StudySearchResult {
            let mut attrs = vec![(0x0008_0060, AttrValue::Text("CT".into()))];
            if let Some(d) = date {
                attrs.push((0x0008_0020, AttrValue::UInt(d)));
            }
            StudySearchResult {
                study_uid: uid.to_string(),
                qido: StudyQido {
                    attrs,
                    ..Default::default()
                },
            }
        }
        let dated = res("dated", Some(20240101));
        let missing = res("missing", None);
        assert!(study_sort_key(&missing).0 == u64::MAX);
        assert!(study_sort_key(&dated) < study_sort_key(&missing));
        // Equal dates tie-break by UID ascending.
        let a = res("a", Some(20240101));
        let b = res("b", Some(20240101));
        assert!(study_sort_key(&a) < study_sort_key(&b));
    }

    /// Index every fuzzy-normalized n-gram of `name` for `study_uid` under a
    /// day (mirrors STOW `append_ngrams`, so the accelerator keys line up with
    /// a space-separated fuzzymatch query).
    fn index_fuzzy_name(idx: &Path, name: &str, study_uid: &str, day: &str) {
        let norm = crate::idx::ngram::fuzzy_normalize(name);
        for gram in crate::idx::ngram::name_ngrams(&norm) {
            crate::idx::ngram::append(
                idx,
                crate::idx::ngram::NameKind::Patient,
                &gram,
                study_uid,
                day,
            )
            .expect("append gram");
        }
    }

    #[test]
    fn fuzzymatch_pn_separator_normalized_matches_spaced_query() {
        // Hand-typed space query matches a stored ^-separated name in one edit
        // after PN-separator normalization ("Даниил" -> "Даннил" is the only
        // edit; the ^->space is folded away).
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "СОЛГАЛОВ^ДАННИЛ^ЮРЬЕВИЧ", 20260101);
        let idx = dir.path().join("idx");
        index_fuzzy_name(&idx, "СОЛГАЛОВ^ДАННИЛ^ЮРЬЕВИЧ", "1.2.1", "20260101");

        let scanner = StudyScanner::new(manifests, idx);
        let f = QueryFilter {
            patient_name: Some("Солгалов Даниил".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(
            res.len(),
            1,
            "must find the ^-stored name with a space query"
        );
        assert_eq!(res[0].study_uid, "1.2.1");
    }

    #[test]
    fn fuzzymatch_two_edits_after_normalization_rejected() {
        // "Салгалов Даниил" is 2 edits away from "СОЛГАЛОВ^ДАННИЛ^ЮРЬЕВИЧ"
        // (С->С/а->о + Даниил->Даннил); the threshold is exactly one edit, so
        // even with normalization it must NOT match.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "СОЛГАЛОВ^ДАННИЛ^ЮРЬЕВИЧ", 20260101);
        let idx = dir.path().join("idx");
        index_fuzzy_name(&idx, "СОЛГАЛОВ^ДАННИЛ^ЮРЬЕВИЧ", "1.2.1", "20260101");

        let scanner = StudyScanner::new(manifests, idx);
        let f = QueryFilter {
            patient_name: Some("Салгалов Даниил".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert!(res.is_empty(), "2-edit name must not fuzzy-match");
    }

    #[test]
    fn fuzzymatch_union_keeps_one_edit_misspelled_match() {
        // Archive contains a REAL "Даниил" (grams дани/ани/нии/ил non-empty)
        // and a study whose stored name is the 1-edit-away "Даннил". Under the
        // old intersection the misspelled study shared only the "дан" gram and
        // was dropped; under union it is a candidate (shares >=1 gram) and bitap
        // confirms the 1-edit match. Neither the correct nor the misspelled
        // study may be lost.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "Даннил", 20260101); // misspelled
        seed_manifest(&manifests, "1.2.2", "Даниил", 20260202); // correct
        let idx = dir.path().join("idx");
        index_fuzzy_name(&idx, "Даннил", "1.2.1", "20260101");
        index_fuzzy_name(&idx, "Даниил", "1.2.2", "20260202");

        let scanner = StudyScanner::new(manifests, idx);
        let f = QueryFilter {
            patient_name: Some("Даниил".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        let uids: Vec<&str> = res.iter().map(|r| r.study_uid.as_str()).collect();
        assert!(
            uids.contains(&"1.2.1"),
            "1-edit misspelled name must survive union: {uids:?}"
        );
        assert!(
            uids.contains(&"1.2.2"),
            "the exact name must match: {uids:?}"
        );
    }

    #[test]
    fn fuzzymatch_short_name_guard_falls_back_to_full_scan() {
        // A 2-scalar CJK name yields one bigram (< 4 grams), so the union
        // guard falls back to a full manifest scan; bitap still finds the exact
        // match. No n-gram index is needed.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.7", "张伟", 20260101);
        let scanner = StudyScanner::new(manifests, dir.path().join("idx"));

        let f = QueryFilter {
            patient_name: Some("张伟".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].study_uid, "1.2.7");
    }

    #[test]
    fn fuzzymatch_equals_separator_normalized() {
        // A stored `=`-separated name (group components) matches a space query
        // after normalization, via the full scan (short name) and bitap.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.8", "Солгалов=Михайлович", 20260101);
        let scanner = StudyScanner::new(manifests, dir.path().join("idx"));

        let f = QueryFilter {
            patient_name: Some("Солгалов Михайлович".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].study_uid, "1.2.8");
    }
}
