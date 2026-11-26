// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The B-tree study search - candidate generation from `.idx`, then manifest
//! mmap + filter: the study search path.
//!
//! # Query plan
//!
//! 1. **Direct UID lookup.** `StudyInstanceUID` names exactly one manifest.
//! 2. **Cold path.** The whole `manifests/` directory is scanned and each
//!    manifest is parsed and filtered (the plan's <10 ms P99 full-scan rule
//!    for <100K studies). The modality and n-gram idx accelerators are
//!    DISABLED pending a coverage marker: without one, both union their rows
//!    with the full study set, so they never narrowed the candidate set and
//!    their reads were pure I/O.
//! 3. **Manifest filter + pagination.** Each candidate manifest is mmap-ed,
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
        let (results, matches, _skipped) = self.search_with_skips(filter)?;
        Ok((results, matches))
    }

    /// [`search`](Self::search) that also reports how many candidates could
    /// not be read.
    ///
    /// A candidate whose manifest is absent or undecodable is skipped rather
    /// than failing the page (one damaged file must not make every healthy
    /// study unanswerable). That skip is deliberate, but a persistent read
    /// failure - a permission error, an I/O error under a file that still
    /// exists - reaches the same branch and is indistinguishable from it. The
    /// query cannot fail on it, so the count is handed back for a diagnostic
    /// sink: the page stays correct and the operator still learns that the
    /// answer was short.
    pub fn search_with_skips(
        &self,
        filter: &QueryFilter,
    ) -> Result<(Vec<StudySearchResult>, usize, usize), StudySearchError> {
        // 1) Candidate set.
        let candidates = self.candidate_study_uids(filter)?;

        // 1b) Pre-build the expected (tag, predicate) map once - avoids
        //     a HashMap allocation per candidate manifest.
        let expected = filter.build_expected();

        // 2) Resolve each candidate: mmap manifest, shallow-decode, filter.
        let mut out = Vec::new();
        let mut skipped = 0usize;
        for uid in &candidates {
            match self.load_if_matches(uid, filter, &expected)? {
                LoadOutcome::Hit(res) => out.push(res),
                LoadOutcome::NoMatch => {}
                LoadOutcome::Unreadable => skipped += 1,
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
            return Ok((Vec::new(), matches, skipped));
        }
        let limit = filter.limit.map(|l| l as usize).unwrap_or(matches - offset);
        Ok((
            out.into_iter().skip(offset).take(limit).collect(),
            matches,
            skipped,
        ))
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
        // Direct UID: one candidate per list member.
        //
        // `studyInstanceUid` holds a single value per Table 10.6.1-5, but that
        // value may be a comma-separated LIST asking for the union. Validating
        // the whole string as one UID refused `1.2.1,1.2.2` (a comma is not a
        // UID character, PS3.5 9.1) and answered a legal query with no
        // candidates at all. Each member is validated on its own and only the
        // ones whose manifest exists become candidates.
        if let Some(uids) = filter.study_instance_uid.as_deref() {
            let mut out: Vec<String> = Vec::new();
            for uid in uids.split(',') {
                let uid = uid.trim();
                // The UID becomes a path component below. A query controls this
                // value, so the same rule that guards every other "is this a
                // UID" question must apply here too: the public validator
                // refuses empty values, separators, dot-escapes and
                // non-conforming digit runs (PS3.5 9.1), and a second, weaker
                // copy of that rule would let a value through that every writer
                // path rejects.
                if crate::uid::validate_path_uid(uid).is_err() {
                    continue;
                }
                // Only a candidate if the manifest actually exists.
                //
                // `is_file()` answers false for BOTH "absent" and "cannot
                // stat" (EACCES, ENOENT on a parent component), so it cannot
                // tell an erased study from a lookup that failed. The two need
                // different answers: an absent manifest is a legitimate
                // no-candidate, while a failed lookup would silently drop a
                // study the caller can still read. Skipping stays the choice
                // for NotFound, as before; any other error surfaces.
                let path = self.manifests_dir.join(format!("{uid}.manifest"));
                match std::fs::metadata(&path) {
                    Ok(m) if m.is_file() => {}
                    Ok(_) => continue,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(StudySearchError::Io(e)),
                }
                // A repeated member is ONE candidate. Pushing it twice made the
                // loader return the same study twice, so `limit=2` could spend
                // its budget on one study and omit the other.
                if out.iter().any(|seen| seen == uid) {
                    continue;
                }
                out.push(uid.to_string());
            }
            return Ok(out);
        }

        // The modality and n-gram accelerators are DISABLED pending a coverage
        // marker. Both used to union their idx rows with the FULL study set
        // (the "non-empty is not the whole answer" rule), so the accelerator
        // reads were pure I/O: the candidate set equalled the cold-path set on
        // every branch. Until the idx can honestly narrow (a marker tracking
        // which studies it covers), the full scan is the only path - it is
        // correct and allocation-cheaper than the union it replaced.
        self.list_all_study_uids()
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
        // `is_dir()` answers false both for an absent root and for a root that
        // cannot be inspected, and this branch used to read the second as an
        // empty archive - a permission error on `manifests/` became "no
        // studies". `read_dir` is called directly so the error kinds separate:
        // a MISSING root really is an empty archive (a fresh install answers
        // QIDO with an empty result set), while a root that exists but cannot
        // be read is an I/O error the caller must see.
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.manifests_dir) {
            Ok(entries) => entries,
            // Windows reports a path component that is a regular file as
            // NotFound, so the bare kind check is not enough: re-asking with
            // `metadata` separates "the root does not exist" (empty archive)
            // from "the root cannot be a directory" (an error). Unix already
            // answers the second case with NotADirectory.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Windows also reports a path COMPONENT that is a regular file
                // as NotFound, so the kind alone cannot separate "the root does
                // not exist" (an empty archive) from "the root cannot be a
                // directory" (an error). The nearest existing ancestor decides:
                // if it exists and is not a directory, the root is unreachable
                // for a reason the caller must see.
                let mut probe = self.manifests_dir.as_path();
                while let Some(parent) = probe.parent() {
                    if parent.as_os_str().is_empty() {
                        break;
                    }
                    match std::fs::metadata(parent) {
                        Ok(m) if m.is_dir() => break,
                        Ok(_) => return Err(StudySearchError::Io(e)),
                        Err(_) => probe = parent,
                    }
                }
                return Ok(out);
            }
            Err(e) => return Err(StudySearchError::Io(e)),
        };
        for entry in entries {
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
    ) -> Result<LoadOutcome, StudySearchError> {
        if let Some(index) = self.index.as_ref()
            && index.ready()
        {
            return Ok(match load_if_matches_index(index, uid, filter, expected) {
                Some(res) => LoadOutcome::Hit(res),
                None => LoadOutcome::NoMatch,
            });
        }
        // QIDO only needs study-level QIDO attributes - skip expensive
        // series/instance/frame decode via decode_study_only (shallow read).
        //
        // A manifest that is GONE is a stale index row (erased study, torn
        // tail, an n-gram line written for a study that never committed), not
        // a broken request: one such candidate used to fail the whole QIDO
        // answer. Skipping it costs that study, which no longer exists.
        //
        // A CORRUPT manifest is skipped for the same reason. It used to abort
        // the entire cold-path search, so one damaged file made every healthy
        // study unanswerable - while the RAM and patients paths already
        // skipped the same file. Reporting is the recovery pass's job; a query
        // must not be the channel that surfaces it.
        let Some(published) = crate::manifest::PublishedManifest::new(&self.manifests_dir, uid)
        else {
            return Ok(LoadOutcome::NoMatch);
        };
        let manifest = match crate::manifest::read_manifest_study_only_published(&published) {
            Ok(m) => m,
            // A decode failure is not a filter miss: the study is present and
            // unreadable, so it is reported as such instead of vanishing into
            // the same branch as a study that simply did not match.
            Err(_) => return Ok(LoadOutcome::Unreadable),
        };
        if !crate::query::match_study_attrs_prebuilt(filter, &manifest, expected) {
            return Ok(LoadOutcome::NoMatch);
        }
        // JSON streaming is delegated to `json.rs` (C-009); here we hand the
        // caller the study attributes so it can emit without re-parsing.
        Ok(LoadOutcome::Hit(StudySearchResult {
            study_uid: uid.to_string(),
            qido: manifest.study,
        }))
    }
}

/// Resolution outcome for one candidate, before pagination.
///
/// `NoMatch` and `Unreadable` used to be the same `None`, so a study that
/// could not be read was counted as one that did not match. They need
/// different accounting: only the second one means the answer is short.
enum LoadOutcome {
    /// The manifest decoded and the study matched the filter.
    Hit(StudySearchResult),
    /// The manifest decoded (or is absent) and the study did not match.
    NoMatch,
    /// The manifest exists but could not be decoded.
    Unreadable,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idx::rollover::DailyIndexSet;
    use crate::manifest::write_manifest;
    use crate::manifest::{AttrValue, Manifest, StudyQido, SysHeader};
    use std::path::Path;

    /// Write a minimal manifest for `study_uid` into `manifests_dir`.
    fn seed_manifest(manifests_dir: &Path, study_uid: &str, name: &str, date: u64) {
        seed_manifest_modality(manifests_dir, study_uid, name, date, "CT");
    }

    /// [`seed_manifest`] with an explicit modality.
    ///
    /// The fixed `CT` made every modality test vacuous: the scanner matched
    /// regardless of modality, so a broken matcher would still have answered
    /// the same set. Tests that assert on modality must seed at least one
    /// value that does NOT match.
    fn seed_manifest_modality(
        manifests_dir: &Path,
        study_uid: &str,
        name: &str,
        date: u64,
        modality: &str,
    ) {
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
                    (0x0008_0060, AttrValue::Text(modality.to_string())),
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

    /// F11: one CORRUPT manifest must not fail the whole cold-path search.
    ///
    /// A single undecodable file used to return `Err(Manifest)` and abort the
    /// answer, so every healthy study became unqueryable while the RAM and
    /// patients paths happily skipped the same file. A query is not the channel
    /// that reports corruption; the recovery pass is.
    #[test]
    fn a_corrupt_manifest_does_not_fail_the_whole_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "alice", 20260101);
        seed_manifest(&manifests, "1.2.2", "bob", 20260202);
        // A present-but-undecodable manifest among the healthy ones.
        std::fs::write(
            manifests.join("1.2.9.manifest"),
            b"\x00\xff\x00\xfe not a manifest",
        )
        .expect("corrupt manifest");

        let scanner = StudyScanner::new(manifests.clone(), dir.path().join("idx"));
        let (res, _) = scanner
            .search(&QueryFilter::default())
            .expect("a corrupt sibling must not abort the search");
        let mut uids: Vec<String> = res.into_iter().map(|r| r.study_uid).collect();
        uids.sort();
        assert_eq!(
            uids,
            vec!["1.2.1".to_string(), "1.2.2".to_string()],
            "the healthy studies must still be answered"
        );
    }

    /// F12: the n-gram accelerator must not drop a study it has no rows for.
    ///
    /// The rows only exist for studies whose names were indexed. A study whose
    /// manifest exists but whose rows are missing (an index written before that
    /// key existed, or a partially repaired store) was silently omitted, which
    /// answers a correct query with an incomplete set - the same "non-empty is
    /// not the whole answer" defect the modality branch already had.
    #[test]
    fn an_unindexed_study_still_matches_a_name_query() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        std::fs::create_dir_all(&idx).expect("mkdir");

        // EVERY gram of the query must have rows, or the accelerator bails out
        // to a full scan and the union is never exercised. So the indexed study
        // carries the query name itself, and the UNINDEXED study is the plain
        // study under test: it matches the query (one edit away) but has no
        // n-gram rows of its own.
        seed_manifest(&manifests, "1.2.1", "atroshchenkolong", 20260101);
        seed_manifest(&manifests, "1.2.2", "atroshchenkolonq", 20260202);

        // Index ONLY the first study's name; the second has no rows at all.
        index_fuzzy_name(&idx, "atroshchenkolong", "1.2.1", "20260101");

        let scanner = StudyScanner::new(manifests.clone(), idx);
        let filter = QueryFilter {
            patient_name: Some("atroshchenkolong".to_string()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner.search(&filter).expect("search");
        let uids: Vec<String> = res.into_iter().map(|r| r.study_uid).collect();
        assert!(
            uids.contains(&"1.2.2".to_string()),
            "a study with no n-gram rows must still be a candidate, got {uids:?}"
        );
    }

    /// A StudyInstanceUID a query controls must not become a path escape.
    ///
    /// PS3.5 9.1: a UID is digits and dots only, so `/`, `\` and `..` never
    /// occur in one. A value that carries them is malformed input, not a study;
    /// joining it into the manifest path would read a file outside the
    /// directory the scanner owns.
    #[test]
    fn a_uid_that_is_a_path_escape_answers_no_candidates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        // A study OUTSIDE the manifests dir, reachable only by escaping.
        let outside = dir.path().join("1.2.99.manifest");
        write_manifest(
            &outside,
            &Manifest {
                sys: SysHeader {
                    version: 1,
                    storage_tier: 1,
                    total_pixel_bytes: 0,
                    arrived_at: 0,
                },
                study: StudyQido {
                    attrs: vec![(0x0020_000D, AttrValue::Text("1.2.99".into()))],
                    ..Default::default()
                },
                series: vec![],
            },
        )
        .expect("outside manifest");

        let scanner = StudyScanner::new(manifests.clone(), dir.path().join("idx"));
        for uid in ["../1.2.99", "..\\1.2.99", "..", ".", ""] {
            let f = QueryFilter {
                study_instance_uid: Some(uid.to_string()),
                ..Default::default()
            };
            let (res, _) = scanner.search(&f).expect("search");
            assert!(
                res.is_empty(),
                "a malformed UID {uid:?} must not resolve anything"
            );
        }

        // A well-formed UID still resolves.
        seed_manifest(&manifests, "1.2.1", "alice", 20260101);
        let f = QueryFilter {
            study_instance_uid: Some("1.2.1".into()),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 1);
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

    /// The modality accelerator narrows the scan WITHOUT dropping studies it
    /// does not know about.
    ///
    /// The idx bucket here covers `1.2.1` only; `1.2.2` is a CT study in the
    /// manifests that the accelerator has no row for (the shape a torn day, a
    /// crash before the append, or a study stored before the idx existed all
    /// produce). A non-empty accelerator set used to be returned as the whole
    /// answer, so `?Modality=CT` silently omitted `1.2.2`.
    #[test]
    fn an_unindexed_study_still_matches_a_modality_query() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        // Two matching CT studies and one that must NOT be returned: without a
        // non-matching control the assertion held even when the modality key
        // was ignored entirely.
        seed_manifest_modality(&manifests, "1.2.1", "alice", 20260101, "CT");
        seed_manifest_modality(&manifests, "1.2.2", "bob", 20260202, "CT");
        seed_manifest_modality(&manifests, "1.2.3", "carol", 20260303, "MR");

        // The accelerator knows only 1.2.1 (a partially written day).
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
        let uids: Vec<&str> = res.iter().map(|r| r.study_uid.as_str()).collect();
        assert_eq!(
            uids,
            vec!["1.2.1", "1.2.2"],
            "both CT studies must answer and the MR study must not"
        );
    }

    /// An UNREADABLE accelerator day must degrade to the full scan, not abort.
    ///
    /// The idx is an accelerator, not a source of truth: a day entry the
    /// modality reader cannot read (here a DIRECTORY named like a day file) is
    /// an I/O error on a bucket that EXISTS. The `?` used to propagate it and
    /// abort the whole search, so every readable CT study became unavailable
    /// because an optional index had one bad day.
    #[test]
    fn an_unreadable_modality_day_degrades_to_the_full_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "alice", 20260101);
        seed_manifest(&manifests, "1.2.2", "bob", 20260101);

        // `bucket.is_dir()` is true, `read_dir` lists this entry, and
        // `read_study_uids` fails on it - the error that used to propagate.
        let day_dir = idx.join("modality").join("CT").join("20260101.idx");
        std::fs::create_dir_all(&day_dir).expect("mkdir day-dir");

        let scanner = StudyScanner::new(manifests.clone(), idx);
        let f = QueryFilter {
            modality: Some("CT".into()),
            ..Default::default()
        };
        let (res, _) = scanner
            .search(&f)
            .expect("an unreadable accelerator must degrade, not abort");
        let uids: Vec<&str> = res.iter().map(|r| r.study_uid.as_str()).collect();
        assert_eq!(
            uids,
            vec!["1.2.1", "1.2.2"],
            "the full scan must answer every CT study"
        );
    }

    /// The n-gram accelerator degrades the same way: one unreadable day falls
    /// back to the full scan instead of aborting the fuzzymatch query.
    #[test]
    fn an_unreadable_ngram_day_degrades_to_the_full_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "atroshchenkolong", 20260101);

        // The query name's first gram bucket holds a DIRECTORY named like a
        // day file; reading it fails (the error the n-gram path used to
        // propagate via the `?` at the call site).
        let norm = crate::idx::ngram::fuzzy_normalize("atroshchenkolong");
        let grams = crate::idx::ngram::name_ngrams(&norm);
        assert!(grams.len() >= 4, "the union guard needs 4+ grams");
        let day_dir = idx
            .join(crate::idx::ngram::kind_root(
                crate::idx::ngram::NameKind::Patient,
            ))
            .join(&grams[0])
            .join("20260101.idx");
        std::fs::create_dir_all(&day_dir).expect("mkdir day-dir");

        let scanner = StudyScanner::new(manifests, idx);
        let f = QueryFilter {
            patient_name: Some("atroshchenkolong".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        let (res, _) = scanner
            .search(&f)
            .expect("an unreadable n-gram bucket must degrade, not abort");
        assert_eq!(res.len(), 1, "the full scan must still match the study");
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

    /// A StudyInstanceUID the scanner accepts must be one the public UID
    /// validator accepts too: two code paths disagreeing about what a study
    /// UID is means a QIDO answer can name a "study" that every writer path
    /// would refuse.
    ///
    /// The fixture is deliberately a file that DOES exist under
    /// `manifests/` (`1..2.manifest`) so the test cannot pass just because
    /// nothing was found: the rejection has to come from the validator.
    #[test]
    fn a_uid_the_validator_rejects_answers_no_candidates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        // A literal `1..2.manifest` on disk: the ad-hoc guard (no `/`, no
        // `..` as the WHOLE value) used to let this one through.
        let bogus = manifests.join("1..2.manifest");
        write_manifest(
            &bogus,
            &Manifest {
                sys: SysHeader {
                    version: 1,
                    storage_tier: 1,
                    total_pixel_bytes: 0,
                    arrived_at: 0,
                },
                study: StudyQido {
                    attrs: vec![(0x0020_000D, AttrValue::Text("1..2".into()))],
                    ..Default::default()
                },
                series: vec![],
            },
        )
        .expect("bogus manifest");

        let scanner = StudyScanner::new(manifests.clone(), dir.path().join("idx"));
        for uid in ["1..2", "study-a", "1.02"] {
            let f = QueryFilter {
                study_instance_uid: Some(uid.to_string()),
                ..Default::default()
            };
            let (res, _) = scanner.search(&f).expect("search");
            assert!(
                res.is_empty(),
                "the validator rejects {uid:?}, the scanner must agree"
            );
        }

        // Non-vacuity half: a well-formed UID that exists resolves.
        seed_manifest(&manifests, "1.2.5", "dave", 20260404);
        let f = QueryFilter {
            study_instance_uid: Some("1.2.5".into()),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].study_uid, "1.2.5");
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

    /// H5: a study UID LIST is split before validation.
    ///
    /// `studyInstanceUid` may carry a comma-separated list asking for the union,
    /// but a comma is not a UID character (PS3.5 9.1), so validating the whole
    /// string as one UID rejected it and the query answered with no candidates.
    #[test]
    fn a_study_uid_list_yields_every_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "A", 20260101);
        seed_manifest(&manifests, "1.2.2", "B", 20260102);
        seed_manifest(&manifests, "1.2.3", "C", 20260103);
        let scanner = StudyScanner::new(manifests, dir.path().join("idx"));

        let f = QueryFilter {
            study_instance_uid: Some("1.2.1,1.2.2".into()),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        let mut uids: Vec<&str> = res.iter().map(|r| r.study_uid.as_str()).collect();
        uids.sort_unstable();
        assert_eq!(uids, vec!["1.2.1", "1.2.2"], "both list members must match");
    }

    /// A7: a repeated list member is one candidate, not two.
    ///
    /// The candidates are collected one per comma member without dedup, so
    /// `1.2.1,1.2.1,1.2.2` produced two entries for `1.2.1`. A `limit` then
    /// spent its budget on the same study twice and omitted the other one.
    #[test]
    fn a_repeated_study_uid_list_member_appears_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "A", 20260101);
        seed_manifest(&manifests, "1.2.2", "B", 20260102);
        let scanner = StudyScanner::new(manifests, dir.path().join("idx"));

        let f = QueryFilter {
            study_instance_uid: Some("1.2.1,1.2.1,1.2.2".into()),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        let uids: Vec<&str> = res.iter().map(|r| r.study_uid.as_str()).collect();
        assert_eq!(
            uids,
            vec!["1.2.1", "1.2.2"],
            "each distinct study must appear exactly once"
        );
    }

    /// A8: a PADDED list member still matches.
    ///
    /// The scanner trims each member, but the predicate behind
    /// `Predicate::CommaList` did not: `1.2.1, 1.2.2` accepted both candidates
    /// and then rejected the second one for its leading space. The two paths
    /// now share the one trim rule.
    #[test]
    fn a_padded_study_uid_list_member_still_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.1", "A", 20260101);
        seed_manifest(&manifests, "1.2.2", "B", 20260102);
        let scanner = StudyScanner::new(manifests, dir.path().join("idx"));

        let f = QueryFilter {
            study_instance_uid: Some("1.2.1, 1.2.2".into()),
            ..Default::default()
        };
        let (res, _) = scanner.search(&f).expect("search");
        let mut uids: Vec<&str> = res.iter().map(|r| r.study_uid.as_str()).collect();
        uids.sort_unstable();
        assert_eq!(
            uids,
            vec!["1.2.1", "1.2.2"],
            "a space after the comma must not drop the member"
        );
    }

    /// A manifests root that is ABSENT is an empty archive, and one that
    /// cannot be read is an error. `is_dir()` answered false for both and the
    /// cold path read the second as "no studies", so a permission failure on
    /// the directory produced a well-formed empty QIDO answer.
    #[test]
    fn an_absent_manifests_root_is_empty_but_a_failed_scan_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Absent root: an empty tier, not a failure.
        let absent = StudyScanner::new(dir.path().join("nope"), dir.path().join("idx"));
        let (res, matches) = absent.search(&QueryFilter::default()).expect("search");
        assert!(res.is_empty());
        assert_eq!(matches, 0);

        // Present-but-unreadable needs a real permission failure, which only
        // exists on Unix for a non-root process; the injectable form is a path
        // whose parent is a regular FILE, which fails with ENOTDIR on every
        // platform and is reported rather than swallowed.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write");
        let broken = StudyScanner::new(blocker.join("manifests"), dir.path().join("idx"));
        match broken.search(&QueryFilter::default()) {
            Err(StudySearchError::Io(e)) => {
                // Windows surfaces a path component that is a regular file as
                // NotFound; Unix as NotADirectory. Both mean "the root cannot
                // be a directory", which is the property under test.
                assert!(
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::NotFound
                    ),
                    "unexpected error kind: {:?}",
                    e.kind()
                );
            }
            other => panic!("an unreadable root must be an error, got {other:?}"),
        }
    }

    /// A manifest that cannot be decoded is reported as a SKIP, not silently
    /// counted as a study that did not match.
    #[test]
    fn an_undecodable_manifest_is_counted_as_a_skip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest(&manifests, "1.2.2", "healthy", 20260202);
        // A manifest stem that is a valid UID but whose bytes are not a
        // manifest - the shape a torn write leaves behind.
        std::fs::write(manifests.join("1.2.1.manifest"), b"not a manifest").expect("write");

        let scanner = StudyScanner::new(manifests, dir.path().join("idx"));
        let (res, matches, skipped) = scanner
            .search_with_skips(&QueryFilter::default())
            .expect("search");
        assert_eq!(res.len(), 1, "the healthy study still answers");
        assert_eq!(matches, 1);
        assert_eq!(
            skipped, 1,
            "the undecodable candidate must be reported, not absorbed"
        );
    }

    /// A same-SOP-era control: a decoded manifest that merely fails the filter
    /// is NOT a skip. Without this the counter above would be satisfied by any
    /// non-matching study.
    #[test]
    fn a_filtered_out_study_is_not_counted_as_a_skip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        seed_manifest_modality(&manifests, "1.2.1", "alice", 20260101, "CT");
        seed_manifest_modality(&manifests, "1.2.2", "bob", 20260202, "MR");

        let scanner = StudyScanner::new(manifests, dir.path().join("idx"));
        let f = QueryFilter {
            modality: Some("CT".into()),
            ..Default::default()
        };
        let (res, matches, skipped) = scanner.search_with_skips(&f).expect("search");
        assert_eq!(res.len(), 1);
        assert_eq!(matches, 1);
        assert_eq!(skipped, 0, "a non-matching study is not an unreadable one");
    }
}
