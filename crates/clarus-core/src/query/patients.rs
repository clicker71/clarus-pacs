// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! B-tree patient aggregation (E-014, variant A) - the `/dicomweb/patients`
//! mini-viewer, ordered by **ingestion time** (`arrived_at`).
//!
//! The endpoint is the service engineer's ping: "this modality pushed 432
//! slices - did it arrive?". Ordering is by **arrival**, not StudyDate, so a
//! retrospective study and a re-STOW both push the patient to the top.
//!
//! Contract:
//! - <= 250 patients, hard limit enforced by the handler.
//! - Sorted by `arrived_at` **desc** (the manifest `sys.arrived_at` ingestion
//!   epoch). Tiebreak: patient_uid asc (deterministic total order).
//! - Dedup by PatientID; `study_count` counts that patient's studies.
//! - Full `manifests/` scan is honest O(N): 100K x ~2 us ~ 200 ms warm.
//! - No new indexes/rollovers (variant B `idx/arrivals/` rejected - a tax for
//!   a problem that does not exist).

use std::collections::HashMap;

use crate::manifest::AttrValue;
use crate::query::filter::tag;

/// One aggregated patient row, ordered by most recent arrival.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatientRow {
    /// Deterministic identity (the 0-based rank after sorting). Not part of the
    /// DICOMweb contract - byte-stable for the fixture oracle.
    pub patient_id: i64,
    /// (0010,0020) PatientID.
    pub patient_uid: String,
    /// (0010,0010) PatientName.
    pub patient_name: String,
    /// Most recent ingestion epoch (seconds) across this patient's studies.
    pub last_arrival: u64,
    /// Number of studies for this patient.
    pub study_count: i64,
}

/// Per-patient aggregation accumulator.
struct Acc {
    name: String,
    max_arrival: u64,
    count: i64,
    /// (0010,0030) PatientBirthDate as the stored `YYYYMMDD` integer -
    /// first non-empty value wins (studies of one patient agree).
    birth: Option<u64>,
}

/// Whether a stored `YYYYMMDD` birth date falls inside the inclusive
/// `[from, to]` bounds - the exact mirror of the study-level
/// `Predicate::DateRange` semantics (filter.rs).
fn birth_in_range(birth: u64, from: Option<u64>, to: Option<u64>) -> bool {
    match (from, to) {
        (Some(lo), Some(hi)) => birth >= lo && birth <= hi,
        (Some(lo), None) => birth >= lo,
        (None, Some(hi)) => birth <= hi,
        (None, None) => true,
    }
}

/// Get a scalar `Text` value for `tag` from a manifest's study attributes.
fn attr_text(attrs: &[(u32, AttrValue)], tag: u32) -> String {
    for (t, v) in attrs {
        if *t == tag
            && let AttrValue::Text(s) = v
        {
            return s.clone();
        }
    }
    String::new()
}

/// Get a scalar birth date for `tag` from a manifest's study attributes as
/// the stored `YYYYMMDD` integer: `UInt` directly, `Text` parsed (the
/// manifest stores dates as `UInt` via `push_date`; a `Text` form is a
/// legacy/manual manifest). `None` when absent or unparsable.
fn attr_birth(attrs: &[(u32, AttrValue)], tag: u32) -> Option<u64> {
    for (t, v) in attrs {
        if *t != tag {
            continue;
        }
        return match v {
            AttrValue::UInt(u) => Some(*u),
            AttrValue::Text(s) => s.parse::<u64>().ok(),
            _ => None,
        };
    }
    None
}

/// Scan every study manifest under `manifests_dir` and aggregate patients.
///
/// Returns up to `limit` patients sorted by most-recent `arrived_at` desc
/// (tiebreak patient_uid asc for a deterministic total order). Old manifests
/// without the `SYS_ARRIVED_AT` key decode to `arrived_at = 0` and sort last.
///
/// `birth_range` (08.09 extension) is the optional inclusive
/// PatientBirthDate bound `(from, to)` as `YYYYMMDD` integers; when set, a
/// patient is kept only when its birth date (first non-empty across its
/// studies) falls inside. A patient with no captured birth date never
/// matches a set range.
pub fn find_recent_patients(
    manifests_dir: &std::path::Path,
    limit: usize,
    birth_range: Option<(Option<u64>, Option<u64>)>,
) -> Result<Vec<PatientRow>, String> {
    // A fresh store legitimately has no manifests dir yet - an empty result,
    // not an error. Real failures (permission denied, not-a-directory, ...)
    // still propagate so the handler 500s honestly.
    let dir = match std::fs::read_dir(manifests_dir) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read manifests dir: {e}")),
    };

    let mut by_patient: HashMap<String, Acc> = HashMap::new();
    for entry in dir {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let path = entry.path();
        // Only `*.manifest` files participate.
        if path.extension().and_then(|e| e.to_str()) != Some("manifest") {
            continue;
        }
        let study = match crate::manifest::read_manifest_study_only(&path) {
            Ok(s) => s,
            // A manifest that will not decode is SKIPPED, not fatal. The
            // patient list is a browse view over the whole archive, and one
            // unreadable file - a half-written manifest from a crash, a bad
            // sector - used to fail the entire request with a 500, so the
            // operator lost the list of every other patient in the store
            // because of one. The scanner's own recovery pass reports the
            // corrupt file; this view carries the patients that are readable.
            //
            // Silent here on purpose: `clarus-core` has no logger, and this
            // runs once per patient-list request - a `println!` would be both
            // a format the server cannot parse and a line per row.
            Err(_) => continue,
        };
        let pid = attr_text(&study.study.attrs, tag::PATIENT_ID);
        if pid.is_empty() {
            continue; // no PatientID -- nothing to group on
        }
        let pname = attr_text(&study.study.attrs, tag::PATIENT_NAME);
        // The sys header carries the ingestion epoch (0 for old manifests).
        let arrived = study.sys.arrived_at;

        let acc = by_patient.entry(pid).or_insert_with(|| Acc {
            name: pname,
            max_arrival: 0,
            count: 0,
            birth: None,
        });
        // Keep the latest arrival; a retrospective or re-STOW with an older
        // StudyDate but a newer arrived_at must still win the grouping.
        if arrived > acc.max_arrival {
            acc.max_arrival = arrived;
        }
        if acc.birth.is_none() {
            acc.birth = attr_birth(&study.study.attrs, tag::PATIENT_BIRTH_DATE);
        }
        acc.count += 1;
    }

    let mut rows: Vec<PatientRow> = by_patient
        .into_iter()
        .filter(|(_, acc)| match birth_range {
            Some((from, to)) => acc.birth.is_some_and(|b| birth_in_range(b, from, to)),
            None => true,
        })
        .map(|(pid, acc)| PatientRow {
            patient_id: 0, // assigned after sort
            patient_uid: pid,
            patient_name: acc.name,
            last_arrival: acc.max_arrival,
            study_count: acc.count,
        })
        .collect();

    // Most-recent arrival first; a deterministic patient_uid tiebreak keeps the
    // total order stable regardless of HashMap iteration order.
    rows.sort_by(|a, b| {
        b.last_arrival
            .cmp(&a.last_arrival)
            .then_with(|| a.patient_uid.cmp(&b.patient_uid))
    });

    // Deterministic rank identity; bound the result.
    rows.truncate(limit);
    for (i, r) in rows.iter_mut().enumerate() {
        r.patient_id = i as i64;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::write_manifest as write_manifest_file;
    use crate::manifest::{AttrValue, Manifest, StudyQido, SysHeader, TIER_HOT};

    #[test]
    fn retrospective_with_old_study_date_rises_to_top() {
        // A fresh arrival with a very old StudyDate must still win by arrival.
        let dir = std::env::temp_dir().join("clarus_ep14_retro");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // petrov arrived later (epoch 200) but has an older StudyDate than ivanov.
        write_test_manifest(&dir, "1.2.3.1", "ivanov", 100, "20260101");
        write_test_manifest(&dir, "1.2.3.2", "petrov", 200, "19990101");

        let rows = find_recent_patients(&dir, 10, None).unwrap();
        // petrov (arrived 200) must be first, despite the 1999 StudyDate.
        assert_eq!(rows[0].patient_uid, "petrov");
        assert_eq!(rows[1].patient_uid, "ivanov");
    }

    #[test]
    fn re_stow_updates_arrival_and_increments_count() {
        let dir = std::env::temp_dir().join("clarus_ep14_restow");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_test_manifest(&dir, "1.2.3.1", "ivanov", 100, "20260101");
        write_test_manifest(&dir, "1.2.3.2", "ivanov", 300, "20260102");

        let rows = find_recent_patients(&dir, 10, None).unwrap();
        assert_eq!(rows[0].patient_uid, "ivanov");
        assert_eq!(rows[0].last_arrival, 300); // refreshed by the re-STOW
        assert_eq!(rows[0].study_count, 2);
    }

    #[test]
    fn enforces_limit_and_binds_rank() {
        let dir = std::env::temp_dir().join("clarus_ep14_limit");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_test_manifest(&dir, "1.2.3.1", "a", 100, "20260201");
        write_test_manifest(&dir, "1.2.3.2", "b", 200, "20260202");
        let rows = find_recent_patients(&dir, 1, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].patient_uid, "b");
        assert_eq!(rows[0].patient_id, 0);
    }

    #[test]
    fn empty_manifests_dir_returns_empty() {
        let dir = std::env::temp_dir().join("clarus_ep14_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(find_recent_patients(&dir, 10, None).unwrap().is_empty());
    }

    #[test]
    fn missing_manifests_dir_returns_empty_not_err() {
        // Fresh store: the manifests dir does not exist yet. That is a legal
        // empty state, not a server error. RED pre-fix: read_dir on the
        // missing dir returned Err -> handler 500. GREEN post-fix: Ok(empty).
        let dir = std::env::temp_dir().join("clarus_ep14_missing");
        let _ = std::fs::remove_dir_all(&dir); // ensure absent
        assert!(!dir.exists(), "precondition: manifests dir must be absent");
        let rows = find_recent_patients(&dir, 10, None);
        assert!(
            matches!(&rows, Ok(p) if p.is_empty()),
            "missing manifests dir must be an empty result, got {rows:?}"
        );
    }

    #[test]
    fn old_manifest_without_arrived_at_sorts_last() {
        let dir = std::env::temp_dir().join("clarus_ep14_old");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // New patient with a real arrival.
        write_test_manifest(&dir, "1.2.3.1", "fresh", 500, "20260201");
        // "Old" manifest - encodes arrived_at as 0 (equivalent to a pre-S6c
        // manifest lacking the key, which decodes to 0).
        write_test_manifest(&dir, "1.2.3.2", "legacy", 0, "20260101");

        let rows = find_recent_patients(&dir, 10, None).unwrap();
        assert_eq!(rows[0].patient_uid, "fresh");
        assert_eq!(rows[0].last_arrival, 500);
        assert_eq!(rows[1].patient_uid, "legacy");
    }
    /// 08.09 extension: an inclusive PatientBirthDate range filters patients
    /// before aggregation; inclusive bounds, absent birth date never matches,
    /// and the study_count stays the patient's total (not the matched subset).
    #[test]
    fn birth_date_range_filters_patients() {
        let dir = std::env::temp_dir().join("clarus_ep14_birth");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // ivanov 19800101 (arrived 100), petrov 19900101 (arrived 200),
        // smith without a birth date (arrived 300).
        write_test_manifest_birth(&dir, "1.2.3.1", "ivanov", 100, "20260101", Some(19_800_101));
        write_test_manifest_birth(&dir, "1.2.3.2", "petrov", 200, "20260102", Some(19_900_101));
        write_test_manifest_birth(&dir, "1.2.3.3", "smith", 300, "20260103", None);

        // Inclusive bounds: the range covers exactly ivanov's birthday.
        let rows =
            find_recent_patients(&dir, 10, Some((Some(19_800_101), Some(19_800_101)))).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].patient_uid, "ivanov");

        // Open-ended upper bound: births <= 19850101 -> only ivanov (1980).
        let rows = find_recent_patients(&dir, 10, Some((None, Some(19_850_101)))).unwrap();
        let uids: Vec<&str> = rows.iter().map(|r| r.patient_uid.as_str()).collect();
        assert_eq!(uids, vec!["ivanov"]);

        // Lower-only bound: both dated patients, the undated one never matches.
        let rows = find_recent_patients(&dir, 10, Some((Some(19_700_101), None))).unwrap();
        let uids: Vec<&str> = rows.iter().map(|r| r.patient_uid.as_str()).collect();
        assert_eq!(uids, vec!["petrov", "ivanov"]);

        // A patient with two studies (one in range) keeps its total count.
        write_test_manifest_birth(&dir, "1.2.3.4", "ivanov", 400, "20260201", Some(19_800_101));
        let rows =
            find_recent_patients(&dir, 10, Some((Some(19_800_101), Some(19_800_101)))).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].patient_uid, "ivanov");
        assert_eq!(rows[0].study_count, 2);
    }

    fn write_test_manifest_birth(
        dir: &std::path::Path,
        uid: &str,
        pid: &str,
        arrived_at: u64,
        date: &str,
        birth: Option<u64>,
    ) {
        let mut attrs = vec![
            (tag::PATIENT_ID, AttrValue::Text(pid.to_string())),
            (tag::PATIENT_NAME, AttrValue::Text(format!("{pid}^test"))),
            (
                tag::STUDY_DATE,
                AttrValue::UInt(date.parse().unwrap_or_default()),
            ),
        ];
        if let Some(b) = birth {
            attrs.push((tag::PATIENT_BIRTH_DATE, AttrValue::UInt(b)));
        }
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at,
            },
            study: StudyQido {
                attrs,
                ..Default::default()
            },
            series: vec![],
        };
        let path = dir.join(format!("{uid}.manifest"));
        write_manifest_file(&path, &m).expect("write manifest");
    }
    fn write_test_manifest(
        dir: &std::path::Path,
        uid: &str,
        pid: &str,
        arrived_at: u64,
        date: &str,
    ) {
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at,
            },
            study: StudyQido {
                attrs: vec![
                    (tag::PATIENT_ID, AttrValue::Text(pid.to_string())),
                    (tag::PATIENT_NAME, AttrValue::Text(format!("{pid}^test"))),
                    (
                        tag::STUDY_DATE,
                        AttrValue::UInt(date.parse().unwrap_or_default()),
                    ),
                ],
                ..Default::default()
            },
            series: vec![],
        };
        let path = dir.join(format!("{uid}.manifest"));
        write_manifest_file(&path, &m).expect("write manifest");
    }
}
