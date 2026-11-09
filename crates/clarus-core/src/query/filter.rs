// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Query search-key filter and the DICOM tag mapping.
//!
//! The 14 PS3.18 study-level search keys are represented as optional strings
//! (the wire form) and translated into `(u32 tag, predicate)` pairs used to
//! filter a parsed manifest. Wildcards (`*`, `?`) are honoured the way the
//! QIDO handler does today - `*` matches any runs, `?` exactly one
//! character - so a manifest-filtered result agrees with the SQLite baseline.

use crate::manifest::{AttrValue, Manifest};
use crate::query::bitap::bitap_match;
use std::collections::HashMap;

/// DICOM attribute tags used by the study-level QIDO search keys.
///
/// Values are the standard (group, element) tag integer, e.g. `0x0010_0010`
/// for PatientName.
pub mod tag {
    pub const PATIENT_NAME: u32 = 0x0010_0010;
    pub const PATIENT_ID: u32 = 0x0010_0020;
    /// IssuerOfPatientID (0010,0021) - LO.
    ///
    /// Captured because AIW-I 4.80.4.1.2.1 requires the internal requestor
    /// to state an issuer for every Patient ID its workitem carries, and the
    /// requestor's only source for it is the study it has just stored. It
    /// sorts between PatientID and PatientBirthDate, so the ascending-tag
    /// contract of the manifest writer holds.
    pub const PATIENT_ID_ISSUER: u32 = 0x0010_0021;
    pub const PATIENT_BIRTH_DATE: u32 = 0x0010_0030;
    pub const PATIENT_SEX: u32 = 0x0010_0040;
    /// ResponsiblePerson (0010,2297) - PN veterinary search key (FR-vet stage 1).
    pub const RESPONSIBLE_PERSON: u32 = 0x0010_2297;
    /// OtherPatientIDsSequence (0010,1002) - SQ; the study key
    /// `00101002.00100020` asks about the Patient IDs it lists.
    ///
    /// The manifest stores the sequence's values as ONE comma-joined scalar
    /// at this tag (the `modalities_in_study` shape), because the key's
    /// semantics is set intersection and not an ordered path.
    pub const OTHER_PATIENT_IDS: u32 = 0x0010_1002;
    /// SpecificCharacterSet (0008,0005) - heuristic-emitted, not stored.
    pub const SPECIFIC_CHARACTER_SET: u32 = 0x0008_0005;
    /// NumberOfStudyRelatedSeries (0020,1206) - derived from the series array.
    pub const NUM_STUDY_SERIES: u32 = 0x0020_1206;
    /// NumberOfStudyRelatedInstances (0020,1208) - derived from instance arrays.
    pub const NUM_STUDY_INSTANCES: u32 = 0x0020_1208;
    pub const STUDY_DATE: u32 = 0x0008_0020;
    pub const STUDY_TIME: u32 = 0x0008_0030;
    pub const ACCESSION_NUMBER: u32 = 0x0008_0050;
    pub const MODALITY: u32 = 0x0008_0060;
    pub const MODALITIES_IN_STUDY: u32 = 0x0008_0061;
    /// Manufacturer (0008,0070) - 08.09 capture + series search key.
    pub const MANUFACTURER: u32 = 0x0008_0070;
    /// InstitutionName (0008,0080) - 08.09 capture + study search key.
    pub const INSTITUTION_NAME: u32 = 0x0008_0080;
    pub const REFERRING_PHYSICIAN: u32 = 0x0008_0090;
    pub const STUDY_DESCRIPTION: u32 = 0x0008_1030;
    /// InstitutionalDepartmentName (0008,1040) - 08.09 capture + series key.
    pub const INSTITUTIONAL_DEPARTMENT_NAME: u32 = 0x0008_1040;
    pub const PERFORMING_PHYSICIAN: u32 = 0x0008_1050;
    pub const NAME_OF_READING_PHYSICIANS: u32 = 0x0008_1060;
    /// OperatorsName (0008,1070) - 08.09 capture + series search key.
    pub const OPERATORS_NAME: u32 = 0x0008_1070;
    pub const STUDY_INSTANCE_UID: u32 = 0x0020_000D;
    pub const STUDY_ID: u32 = 0x0020_0010;
    /// RequestingPhysician (0032,1032) - 08.09 capture + study search key.
    pub const REQUESTING_PHYSICIAN: u32 = 0x0032_1032;
    /// RequestedProcedureDescription (0032,1060) - 08.09 capture + key.
    pub const REQUESTED_PROCEDURE_DESCRIPTION: u32 = 0x0032_1060;
    pub const SERIES_INSTANCE_UID: u32 = 0x0020_000E;
    pub const SERIES_NUMBER: u32 = 0x0020_0011;
    pub const SOP_INSTANCE_UID: u32 = 0x0008_0018;
    // capture expansion: series-level + instance-level attributes.
    pub const SOP_CLASS_UID: u32 = 0x0008_0016;
    pub const SERIES_DATE: u32 = 0x0008_0021;
    pub const SERIES_TIME: u32 = 0x0008_0031;
    // PerformedProcedureStepStartDate/Time (0040,0244/0040,0245) are
    // captured SERIES matching keys.
    pub const PPS_START_DATE: u32 = 0x0040_0244;
    pub const PPS_START_TIME: u32 = 0x0040_0245;
    /// ScheduledProcedureStepID (0040,0009) - the scalar read out of
    /// RequestAttributesSequence (0040,0275) and stored under its own tag.
    pub const SCHEDULED_PROCEDURE_STEP_ID: u32 = 0x0040_0009;
    /// RequestedProcedureID (0040,1001) - the second scalar of the same
    /// sequence, stored under its own tag.
    pub const REQUESTED_PROCEDURE_ID: u32 = 0x0040_1001;
    pub const STATION_NAME: u32 = 0x0008_1010;
    pub const SERIES_DESCRIPTION: u32 = 0x0008_103E;
    pub const BODY_PART_EXAMINED: u32 = 0x0018_0015;
    /// ProtocolName (0018,1030) - 08.09 capture + series search key.
    pub const PROTOCOL_NAME: u32 = 0x0018_1030;
    pub const INSTANCE_NUMBER: u32 = 0x0020_0013;
    pub const LATERALITY: u32 = 0x0020_0060;

    // -- Captured derived-object references -------------------------------
    //
    // A derived object (a presentation state, a segmentation) is deleted with
    // the study or series it annotates. Its references are captured at STOW
    // into the instance attribute list under these keys, so the deletion can
    // read where the object points without parsing DICOM bytes.
    //
    // The keys sit above every DICOM tag on purpose: the attribute list is
    // kept in ascending tag order for the decoder's binary search, so a key
    // that sorts last appends without disturbing the order of the rest.
    //
    // These are manifest-local keys, not DICOM attributes: nothing serves
    // them to a client, and no client asks for them.

    /// Referenced Series Instance UIDs (from 0008,1115 / 0008,2112 per segment).
    pub const REFERENCED_SERIES_UIDS: u32 = 0xFFF0_0001;
    /// Referenced SOP Instance UIDs (from 0008,1140 / 0008,2112).
    pub const REFERENCED_SOP_INSTANCE_UIDS: u32 = 0xFFF0_0002;
}

/// The study-level search keys accepted by the QIDO query engine.
///
/// Fields mirror the DICOMweb parameter names. `limit` / `offset` pagination
/// is applied by the caller after ordering.
#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    pub patient_name: Option<String>,
    pub patient_id: Option<String>,
    pub study_date_from: Option<String>,
    pub study_date_to: Option<String>,
    pub modality: Option<String>,
    pub accession_number: Option<String>,
    pub study_description: Option<String>,
    pub referring_physician: Option<String>,
    pub performing_physician: Option<String>,
    pub name_of_reading_physicians: Option<String>,
    /// InstitutionName (0008,0080) - wildcard key (08.09 extension).
    pub institution_name: Option<String>,
    /// RequestingPhysician (0032,1032) - PN wildcard key (08.09 extension,
    /// ReferringPhysicianName template: fuzzymatch + case_sensitive_pn apply).
    pub requesting_physician: Option<String>,
    /// RequestedProcedureDescription (0032,1060) - wildcard key (08.09).
    pub requested_procedure_description: Option<String>,
    /// ResponsiblePerson (0010,2297) - PN veterinary search key (FR-vet stage 1).
    pub responsible_person: Option<String>,
    pub study_instance_uid: Option<String>,
    pub study_time: Option<String>,
    pub study_id: Option<String>,
    /// PatientBirthDate (0010,0030) range lower bound, `YYYYMMDD` (08.09
    /// extension, dcm4chee parity: the key accepts `YYYYMMDD[-YYYYMMDD]`;
    /// an exact value populates both bounds).
    pub birth_date_from: Option<String>,
    /// PatientBirthDate (0010,0030) range upper bound, `YYYYMMDD`.
    pub birth_date_to: Option<String>,
    pub patient_sex: Option<String>,
    pub modalities_in_study: Option<String>,
    /// OtherPatientIDsSequence (0010,1002) > PatientID (0010,0020) - the
    /// comma-joined aggregate the `00101002.00100020` key matches against.
    pub other_patient_ids: Option<String>,
    pub series_instance_uid: Option<String>,
    /// SeriesNumber (0020,0011) search key (IS; wildcard `*`/`?` allowed).
    pub series_number: Option<String>,
    /// Maximum results returned. `None` = all.
    pub limit: Option<u64>,
    /// Zero-based offset into the ordered result set.
    pub offset: Option<u64>,
    /// Enable Levenshtein-1 matching on `PatientName` / `ReferringPhysicianName`
    /// when those search keys carry no `*`/`?` wildcard. `false` (default)
    /// keeps the exact/glob semantics of prior sprints. Set by the client's
    /// `fuzzymatching=true` parameter (PS3.18 8.3.3.2).
    pub fuzzymatch: bool,
    /// Force Levenshtein-1 on PN keys even without the client flag
    /// (`[query] fuzzymatch_override`). A convenience deviation from the
    /// standard: Weasis never sends `fuzzymatching=true`, so typed-name
    /// search stays exact. `false` (default) = standard.
    pub fuzzymatch_override: bool,
    /// Case-insensitive glob on PN keys (`[query] case_sensitive_pn = false`).
    /// A convenience deviation: PS3.4 wildcard matching is case-sensitive.
    /// `false` (default) = standard.
    pub pn_case_insensitive: bool,
}

impl QueryFilter {
    /// True when the filter carries at least one search key.
    pub fn has_constraints(&self) -> bool {
        self.patient_name.is_some()
            || self.patient_id.is_some()
            || self.study_date_from.is_some()
            || self.study_date_to.is_some()
            || self.modality.is_some()
            || self.accession_number.is_some()
            || self.study_description.is_some()
            || self.referring_physician.is_some()
            || self.performing_physician.is_some()
            || self.name_of_reading_physicians.is_some()
            || self.institution_name.is_some()
            || self.requesting_physician.is_some()
            || self.requested_procedure_description.is_some()
            || self.responsible_person.is_some()
            || self.other_patient_ids.is_some()
            || self.study_instance_uid.is_some()
            || self.study_time.is_some()
            || self.study_id.is_some()
            || self.birth_date_from.is_some()
            || self.birth_date_to.is_some()
            || self.patient_sex.is_some()
            || self.modalities_in_study.is_some()
            || self.series_instance_uid.is_some()
            || self.series_number.is_some()
    }

    /// The `Modality` search key, if present (the single hot accelerator).
    pub fn modality(&self) -> Option<&str> {
        self.modality.as_deref()
    }

    /// Whether the `ModalitiesInStudy` (0008,0061) search key matches a
    /// study's derived aggregate value.
    ///
    /// Unlike the other study keys this one never matches a tag inside
    /// `StudyQido.attrs` (the B-tree tier does not store 0008,0061 there);
    /// the value lives in the RAM-only `StudyQido.modalities_in_study` field
    /// (distinct series modalities, first-occurrence order, comma-joined).
    /// PS3.18 semantics: the study matches when ANY requested modality is
    /// present in the study's aggregate.
    pub fn modalities_in_study_match(&self, stored: Option<&str>) -> bool {
        let Some(query) = self.modalities_in_study.as_deref() else {
            return true;
        };
        let Some(stored) = stored else {
            return false;
        };
        comma_list_intersects(query, stored)
    }

    /// Whether the `OtherPatientIDs` (0010,1002 > 0010,0020) search key
    /// matches a study's aggregated identifier list.
    ///
    /// The sequence is MULTI-VALUED and the manifest stores its values as one
    /// comma-joined scalar, so the key matches when ANY requested identifier
    /// appears in the list - the same set semantics `ModalitiesInStudy` uses.
    pub fn other_patient_ids_match(&self, stored: Option<&str>) -> bool {
        let Some(query) = self.other_patient_ids.as_deref() else {
            return true;
        };
        let Some(stored) = stored else {
            return false;
        };
        comma_list_intersects(query, stored)
    }

    /// [`Self::other_patient_ids_match`] against a decoded attribute list.
    ///
    /// The RAM index path holds the study's attributes rather than the parsed
    /// manifest, and it must apply the SAME predicates as the cold path - a
    /// server with a warm index and one without used to answer
    /// `00101002.00100020=X` differently, because only the cold path ran this
    /// check.
    pub fn other_patient_ids_match_prebuilt(
        &self,
        attrs: &[(u32, crate::manifest::AttrValue)],
    ) -> bool {
        let stored = attrs
            .iter()
            .find(|(t, _)| *t == tag::OTHER_PATIENT_IDS)
            .and_then(|(_, v)| match v {
                crate::manifest::AttrValue::Text(s) => Some(s.as_str()),
                _ => None,
            });
        self.other_patient_ids_match(stored)
    }

    /// The `[from, to]` study-date bounds as `YYYYMMDD` strings.
    pub fn date_bounds(&self) -> (Option<&str>, Option<&str>) {
        (
            self.study_date_from.as_deref(),
            self.study_date_to.as_deref(),
        )
    }

    /// Whether this filter's study-level search keys match a manifest.
    ///
    /// `attrs` is the manifest's `StudyQido.attrs` - the ordered `(tag, value)`
    /// pairs. Each required search key is checked once; the attribute set is
    /// small (< 40 entries) so a single linear pass is both correct and
    /// branch-friendly. A filter with no constraints matches everything.
    pub fn matches(&self, attrs: &[(u32, AttrValue)]) -> bool {
        let expected = self.build_expected();
        if expected.is_empty() {
            return true;
        }
        let mut remaining = expected.len();
        for (t, v) in attrs {
            if let Some(pred) = expected.get(t)
                && predicate_matches(*pred, v)
            {
                remaining -= 1;
                if remaining == 0 {
                    return true;
                }
            }
        }
        false
    }

    /// Same as [`matches`] but re-uses a pre-built expected map.
    /// Callers that check many manifests against the same filter should
    /// call [`build_expected`] once and reuse the map here.
    pub fn matches_prebuilt(
        &self,
        attrs: &[(u32, AttrValue)],
        expected: &HashMap<u32, Predicate<'_>>,
    ) -> bool {
        if expected.is_empty() {
            return true;
        }
        let mut remaining = expected.len();
        for (t, v) in attrs {
            if let Some(pred) = expected.get(t)
                && predicate_matches(*pred, v)
            {
                remaining -= 1;
                if remaining == 0 {
                    return true;
                }
            }
        }
        false
    }

    /// Build the set of `(tag, predicate)` this filter requires.
    /// The returned map is cheap to reuse across many `matches_prebuilt` calls.
    pub fn build_expected(&self) -> HashMap<u32, Predicate<'_>> {
        let mut m = HashMap::new();
        if let Some(v) = self.patient_name.as_deref() {
            m.insert(tag::PATIENT_NAME, self.name_predicate(v));
        }
        if let Some(v) = self.patient_id.as_deref() {
            m.insert(tag::PATIENT_ID, Predicate::Glob(v));
        }
        if let Some(v) = self.accession_number.as_deref() {
            m.insert(tag::ACCESSION_NUMBER, Predicate::Glob(v));
        }
        if let Some(v) = self.study_description.as_deref() {
            m.insert(tag::STUDY_DESCRIPTION, Predicate::Glob(v));
        }
        if let Some(v) = self.referring_physician.as_deref() {
            m.insert(tag::REFERRING_PHYSICIAN, self.name_predicate(v));
        }
        // 08.09: InstitutionName (0008,0080) is a plain wildcard key
        // (Glob, case-sensitive - no fuzzy wiring, YAGNI).
        if let Some(v) = self.institution_name.as_deref() {
            m.insert(tag::INSTITUTION_NAME, Predicate::Glob(v));
        }
        // 08.09: RequestingPhysician (0032,1032) follows the
        // ReferringPhysicianName PN template (fuzzymatch / case_sensitive_pn).
        if let Some(v) = self.requesting_physician.as_deref() {
            m.insert(tag::REQUESTING_PHYSICIAN, self.name_predicate(v));
        }
        // 08.09: RequestedProcedureDescription (0032,1060) - plain wildcard.
        if let Some(v) = self.requested_procedure_description.as_deref() {
            m.insert(tag::REQUESTED_PROCEDURE_DESCRIPTION, Predicate::Glob(v));
        }
        if let Some(v) = self.performing_physician.as_deref() {
            m.insert(tag::PERFORMING_PHYSICIAN, self.name_predicate(v));
        }
        if let Some(v) = self.name_of_reading_physicians.as_deref() {
            m.insert(tag::NAME_OF_READING_PHYSICIANS, self.name_predicate(v));
        }
        if let Some(v) = self.responsible_person.as_deref() {
            m.insert(tag::RESPONSIBLE_PERSON, self.name_predicate(v));
        }
        if let Some(v) = self.study_instance_uid.as_deref() {
            m.insert(tag::STUDY_INSTANCE_UID, Predicate::Glob(v));
        }
        if let Some(v) = self.study_id.as_deref() {
            m.insert(tag::STUDY_ID, Predicate::Glob(v));
        }
        if let Some(v) = self.study_time.as_deref() {
            m.insert(tag::STUDY_TIME, Predicate::Glob(v));
        }
        // 08.09: PatientBirthDate accepts a range like StudyDate. The stored
        // value is a numeric `YYYYMMDD` UInt (push_date, manifest.rs), so the
        // inclusive `DateRange` predicate compares directly without renders.
        if self.birth_date_from.is_some() || self.birth_date_to.is_some() {
            m.insert(
                tag::PATIENT_BIRTH_DATE,
                Predicate::DateRange(
                    self.birth_date_from.as_deref(),
                    self.birth_date_to.as_deref(),
                ),
            );
        }
        if let Some(v) = self.patient_sex.as_deref() {
            m.insert(tag::PATIENT_SEX, Predicate::Glob(v));
        }
        // ModalitiesInStudy (0008,0061) is deliberately NOT inserted here:
        // the B-tree tier stores it in the derived RAM-only
        // `StudyQido.modalities_in_study` field, never in `attrs`
        // (`study_attrs` keeps only Modality 0008,0060). It is checked
        // separately by `modalities_in_study_match` via
        // `match_study_attrs[_prebuilt]`.
        if let Some(v) = self.series_instance_uid.as_deref() {
            m.insert(tag::SERIES_INSTANCE_UID, Predicate::Glob(v));
        }
        if let Some(v) = self.series_number.as_deref() {
            m.insert(tag::SERIES_NUMBER, Predicate::Glob(v));
        }
        if let Some(v) = self.modality.as_deref() {
            m.insert(tag::MODALITY, Predicate::CommaList(v));
        }
        let (from, to) = self.date_bounds();
        if from.is_some() || to.is_some() {
            m.insert(tag::STUDY_DATE, Predicate::DateRange(from, to));
        }
        m
    }

    /// Pick the predicate for a PersonName search key.
    ///
    /// Fuzzymatch (one Levenshtein edit) applies when it is enabled (client
    /// flag or `fuzzymatch_override`) and the pattern carries no wildcard; a
    /// wildcard always means glob. Case-insensitive glob applies only when
    /// `pn_case_insensitive` is set (deviation from the standard).
    fn name_predicate<'a>(&self, v: &'a str) -> Predicate<'a> {
        if (self.fuzzymatch || self.fuzzymatch_override) && !has_wildcard(v) {
            Predicate::Fuzzy(v)
        } else if self.pn_case_insensitive {
            Predicate::GlobCi(v)
        } else {
            Predicate::Glob(v)
        }
    }
}

/// Whether a search pattern contains a `*` or `?` wildcard character.
fn has_wildcard(pat: &str) -> bool {
    pat.bytes().any(|b| b == b'*' || b == b'?')
}

/// The predicate form of an expected search-key value.
#[derive(Debug, Clone, Copy)]
pub enum Predicate<'a> {
    /// Glob match with `*` / `?` wildcards (case-sensitive, PS3.4).
    Glob(&'a str),
    /// Case-insensitive glob (PN convenience deviation,
    /// `[query] case_sensitive_pn = false`). Lowercases both sides per match.
    GlobCi(&'a str),
    /// Levenshtein-1 match on a PersonName (no wildcard, fuzzymatch on).
    Fuzzy(&'a str),
    /// Comma-separated list where any element may match exactly.
    CommaList(&'a str),
    /// Inclusive numeric season-date range `[from, to]` (`YYYYMMDD` as u64).
    DateRange(Option<&'a str>, Option<&'a str>),
}

/// Whether a manifest attribute value satisfies an expected predicate.
fn predicate_matches(pred: Predicate<'_>, have: &AttrValue) -> bool {
    match pred {
        Predicate::Glob(pat) => match have {
            AttrValue::Text(s) => glob_match(pat, s),
            AttrValue::UInt(u) => {
                let mut buf = itoa_buf();
                let s = write_u64(&mut buf, *u);
                glob_match(pat, s)
            }
            _ => false,
        },
        Predicate::GlobCi(pat) => match have {
            AttrValue::Text(s) => glob_match(&pat.to_lowercase(), &s.to_lowercase()),
            AttrValue::UInt(u) => {
                let mut buf = itoa_buf();
                let s = write_u64(&mut buf, *u);
                glob_match(&pat.to_lowercase(), &s.to_lowercase())
            }
            _ => false,
        },
        Predicate::Fuzzy(pat) => match have {
            AttrValue::Text(s) => {
                // Bitap is ASCII-case-insensitive only, so normalize both sides
                // to lowercase for a Unicode-consistent match (the n-gram index
                // is built from lowercased names too). For the fuzzy path the
                // PN separators (`^`/`=`) are replaced with a space first so a
                // hand-typed space query aligns with a stored `^`/`=` name; a
                // stray separator would otherwise consume the single-edit
                // budget. The allocation happens here, in the predicate, never
                // in `bitap_match`.
                bitap_match(
                    &crate::idx::ngram::fuzzy_normalize(pat).to_lowercase(),
                    &crate::idx::ngram::fuzzy_normalize(s).to_lowercase(),
                )
            }
            _ => false,
        },
        Predicate::CommaList(pat) => {
            let have_txt = match have {
                AttrValue::Text(s) => s,
                AttrValue::UInt(u) => {
                    let mut buf = itoa_buf();
                    let s = write_u64(&mut buf, *u);
                    return comma_list_match(pat, s);
                }
                _ => return false,
            };
            comma_list_match(pat, have_txt)
        }
        Predicate::DateRange(from, to) => {
            let date: Option<u64> = match have {
                AttrValue::UInt(u) => Some(*u),
                AttrValue::Text(s) => s.parse().ok(),
                _ => None,
            };
            let Some(date) = date else { return false };
            let lo = from.and_then(|f| f.parse::<u64>().ok());
            let hi = to.and_then(|t| t.parse::<u64>().ok());
            match (lo, hi) {
                (Some(lo), Some(hi)) => date >= lo && date <= hi,
                (Some(lo), None) => date >= lo,
                (None, Some(hi)) => date <= hi,
                (None, None) => true,
            }
        }
    }
}

/// Whether `text` matches `pat` with `*` (any run) and `?` (one char) wildcards.
///
/// Iterative two-pointer matcher: the pattern is walked once and `*` leaves a
/// single backtrack point. The previous implementation was a recursive
/// backtracker that tried EVERY split of every `*`, which is C(n+k, k) calls on
/// a pattern like `*a*a*a*a*a*a` - a QIDO query parameter is attacker-supplied,
/// so that was a one-request CPU burn. Case-sensitive, matching DICOMweb's
/// literal match.
pub fn glob_match(pat: &str, text: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    let text: Vec<char> = text.chars().collect();

    let (mut pi, mut ti) = (0usize, 0usize);
    // Last `*` seen, and the text position it is currently assumed to swallow.
    let mut star: Option<(usize, usize)> = None;

    while ti < text.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star = Some((pi, ti));
            pi += 1; // try with the `*` consuming nothing yet
        } else if let Some((sp, st)) = star {
            // Backtrack: let that `*` swallow one more character and retry.
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    // Trailing `*`s may still match the empty tail.
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Whether any comma-separated element of `pat` equals `text` exactly.
fn comma_list_match(pat: &str, text: &str) -> bool {
    pat.split(',').any(|e| e == text)
}

/// Whether ANY comma-separated element of `pat` also appears among the
/// comma-separated elements of `text` (both may be lists).
///
/// Used for `ModalitiesInStudy` (0008,0061): the stored value is the study's
/// distinct-modality aggregate (`"CT,MR"`) and the query may be
/// multi-valued (`"CT,DX"`) - the study matches when any requested
/// modality is present. Elements are trimmed; the match is case-sensitive
/// (DICOM CS values are uppercase on the wire).
fn comma_list_intersects(pat: &str, text: &str) -> bool {
    pat.split(',').any(|e| {
        let e = e.trim();
        !e.is_empty() && text.split(',').any(|t| t.trim() == e)
    })
}

/// A stack buffer for formatting an integer; `u64` fits in 20 decimal digits.
fn itoa_buf() -> [u8; 24] {
    [0u8; 24]
}

/// Write the decimal form of `v` into `buf`, returning the slice up to (but
/// not including) the terminating NUL. Digits are ASCII; the slice is UTF-8.
fn write_u64(buf: &mut [u8; 24], mut v: u64) -> &str {
    let mut i = 24;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    // SAFETY: digits are ASCII; slice is valid UTF-8.
    std::str::from_utf8(&buf[i..]).unwrap_or("")
}

/// Public helper: does this manifest pass the filter's study-level predicate?
///
/// Uses a pre-built expected map when provided (hotpath), building one
/// inline only for callers that do not have it.
pub fn match_study_attrs(filter: &QueryFilter, m: &Manifest) -> bool {
    filter.matches(&m.study.attrs)
        && filter.modalities_in_study_match(m.study.modalities_in_study.as_deref())
        && filter.other_patient_ids_match(attr_text(&m.study.attrs, tag::OTHER_PATIENT_IDS))
}

/// Same as [`match_study_attrs`] but re-uses a `build_expected` map so it
/// is never rebuilt across candidate manifests.
pub fn match_study_attrs_prebuilt(
    filter: &QueryFilter,
    m: &Manifest,
    expected: &HashMap<u32, Predicate<'_>>,
) -> bool {
    filter.matches_prebuilt(&m.study.attrs, expected)
        && filter.modalities_in_study_match(m.study.modalities_in_study.as_deref())
        && filter.other_patient_ids_match(attr_text(&m.study.attrs, tag::OTHER_PATIENT_IDS))
}

/// The text of one stored attribute, for the multi-valued study keys.
///
/// Those keys are matched by their own set comparison rather than through the
/// expected-map, so the caller fetches the stored value directly.
fn attr_text(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&str> {
    attrs
        .iter()
        .find(|(t, _)| *t == tag)
        .and_then(|(_, v)| match v {
            AttrValue::Text(s) => Some(s.as_str()),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(list: &[(u32, AttrValue)]) -> Vec<(u32, AttrValue)> {
        list.to_vec()
    }

    #[test]
    fn empty_filter_matches_everything() {
        let f = QueryFilter::default();
        assert!(f.matches(&attrs(&[(tag::PATIENT_NAME, AttrValue::Text("x".into()))])));
    }

    #[test]
    fn exact_text_match() {
        let f = QueryFilter {
            patient_name: Some("ivanov".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("ivanov".into())
        )])));
        assert!(!f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("petrov".into())
        )])));
    }

    #[test]
    fn wildcard_match() {
        let f = QueryFilter {
            patient_name: Some("iva*".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("ivanov".into())
        )])));
    }

    #[test]
    fn date_range_numeric() {
        let f = QueryFilter {
            study_date_from: Some("20260101".into()),
            study_date_to: Some("20261231".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(tag::STUDY_DATE, AttrValue::UInt(20260802))])));
        assert!(!f.matches(&attrs(&[(tag::STUDY_DATE, AttrValue::UInt(20251231))])));
    }

    /// 08.09: PatientBirthDate range over the stored UInt - inclusive bounds
    /// on both ends, exact form as a single-day range, and `*` as unbounded.
    #[test]
    fn birth_date_range_inclusive_bounds() {
        // Inclusive both ends: the stored day equal to a bound matches.
        let f = QueryFilter {
            birth_date_from: Some("19800101".into()),
            birth_date_to: Some("19901231".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19800101)
        )])));
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19901231)
        )])));
        assert!(!f.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19791231)
        )])));
        assert!(!f.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19910101)
        )])));

        // Exact form populates both bounds -> a single-day range.
        let f = QueryFilter {
            birth_date_from: Some("19800315".into()),
            birth_date_to: Some("19800315".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19800315)
        )])));
        assert!(!f.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19800316)
        )])));

        // Open-ended bounds mirror the StudyDate machinery.
        let lo_only = QueryFilter {
            birth_date_from: Some("19900101".into()),
            ..Default::default()
        };
        assert!(lo_only.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19900101)
        )])));
        assert!(!lo_only.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19891231)
        )])));
        let hi_only = QueryFilter {
            birth_date_to: Some("19801231".into()),
            ..Default::default()
        };
        assert!(hi_only.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19801231)
        )])));
        assert!(!hi_only.matches(&attrs(&[(
            tag::PATIENT_BIRTH_DATE,
            AttrValue::UInt(19810101)
        )])));

        // A missing birth date never matches a set range.
        assert!(!f.matches(&attrs(&[(tag::PATIENT_NAME, AttrValue::Text("x".into()))])));
    }

    /// 08.09 Stage B study keys: InstitutionName + RequestedProcedureDescription
    /// are plain wildcards; RequestingPhysician follows the PN template
    /// (wildcard glob, and `case_sensitive_pn` applies).
    #[test]
    fn stage_b_study_keys_match() {
        let m = attrs(&[
            (
                tag::INSTITUTION_NAME,
                AttrValue::Text("City Hospital".into()),
            ),
            (
                tag::REQUESTED_PROCEDURE_DESCRIPTION,
                AttrValue::Text("CT Chest WO".into()),
            ),
            (
                tag::REQUESTING_PHYSICIAN,
                AttrValue::Text("REQ^Paul".into()),
            ),
        ]);

        // Plain wildcards, case-sensitive.
        let f = QueryFilter {
            institution_name: Some("City*".into()),
            requested_procedure_description: Some("CT*".into()),
            ..Default::default()
        };
        assert!(f.matches(&m));
        assert!(
            !QueryFilter {
                institution_name: Some("city*".into()),
                ..Default::default()
            }
            .matches(&m)
        );

        // RequestingPhysician: exact PN glob and a mismatch.
        let f = QueryFilter {
            requesting_physician: Some("REQ^Paul".into()),
            ..Default::default()
        };
        assert!(f.matches(&m));
        assert!(
            !QueryFilter {
                requesting_physician: Some("REQ^Anna".into()),
                ..Default::default()
            }
            .matches(&m)
        );

        // The case_sensitive_pn knob applies exactly as on
        // ReferringPhysicianName: lowercase pattern matches with the knob on.
        let ci = QueryFilter {
            requesting_physician: Some("req^paul".into()),
            pn_case_insensitive: true,
            ..Default::default()
        };
        assert!(ci.matches(&m));

        // A missing stored attribute never matches a set key.
        let f = QueryFilter {
            institution_name: Some("City*".into()),
            ..Default::default()
        };
        assert!(!f.matches(&attrs(&[(tag::PATIENT_NAME, AttrValue::Text("x".into()))])));
    }

    #[test]
    fn modality_comma_list() {
        let f = QueryFilter {
            modality: Some("CT,MR".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(tag::MODALITY, AttrValue::Text("MR".into()))])));
        assert!(!f.matches(&attrs(&[(tag::MODALITY, AttrValue::Text("US".into()))])));
    }

    #[test]
    fn modalities_in_study_list_intersection() {
        // 0008,0061 lives in the derived aggregate field, not in `attrs`.
        // The study matches when ANY requested modality is present.
        let mk = |q: &str| QueryFilter {
            modalities_in_study: Some(q.to_string()),
            ..Default::default()
        };
        assert!(mk("CT").modalities_in_study_match(Some("CT,MR")));
        assert!(mk("MR").modalities_in_study_match(Some("CT,MR")));
        assert!(!mk("DX").modalities_in_study_match(Some("CT,MR")));
        // Multi-valued query (Weasis sends 00080061=CT,DX).
        assert!(mk("CT,DX").modalities_in_study_match(Some("CT,MR")));
        assert!(mk("DX,CT").modalities_in_study_match(Some("CT,MR")));
        // Elements are trimmed.
        assert!(mk(" CT , DX ").modalities_in_study_match(Some("CT,MR")));
        assert!(!mk(" , ").modalities_in_study_match(Some("CT")));
        // No query key -> always matches; query key vs no stored value -> no.
        assert!(QueryFilter::default().modalities_in_study_match(Some("CT")));
        assert!(QueryFilter::default().modalities_in_study_match(None));
        assert!(!mk("CT").modalities_in_study_match(None));
    }

    #[test]
    fn match_study_attrs_honors_derived_modalities_field() {
        use crate::manifest::{StudyQido, SysHeader};
        let mk = |modalities: Option<&str>| Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: 1,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![],
                modalities_in_study: modalities.map(String::from),
                ..Default::default()
            },
            series: vec![],
        };
        let f = QueryFilter {
            modalities_in_study: Some("CT".into()),
            ..Default::default()
        };
        assert!(match_study_attrs(&f, &mk(Some("CT,MR"))));
        assert!(!match_study_attrs(&f, &mk(Some("MR"))));
        assert!(!match_study_attrs(&f, &mk(None)));
        // A 0008,0061 entry inside `attrs` alone must NOT satisfy the key
        // (production manifests never store it there; the derived field is
        // the only oracle - mirrors the golden-era attrs entry being dead).
        let attrs_only = Manifest {
            study: StudyQido {
                attrs: vec![(tag::MODALITIES_IN_STUDY, AttrValue::Text("CT".into()))],
                modalities_in_study: None,
                ..Default::default()
            },
            ..mk(None)
        };
        assert!(!match_study_attrs(&f, &attrs_only));
        // ...but when the derived field agrees the study matches even with a
        // stale attrs entry present.
        let both = Manifest {
            study: StudyQido {
                attrs: vec![(tag::MODALITIES_IN_STUDY, AttrValue::Text("MR".into()))],
                modalities_in_study: Some("CT,MR".into()),
                ..Default::default()
            },
            ..mk(None)
        };
        assert!(match_study_attrs(&f, &both));
    }

    #[test]
    fn glob_semantics() {
        assert!(glob_match("a*b", "axxxb"));
        assert!(glob_match("a?b", "axb"));
        assert!(!glob_match("a?b", "axxb"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn u64_predicate_glob() {
        let f = QueryFilter {
            study_time: Some("12*".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(tag::STUDY_TIME, AttrValue::UInt(123456))])));
        assert!(!f.matches(&attrs(&[(tag::STUDY_TIME, AttrValue::UInt(90000))])));
    }

    #[test]
    fn series_number_exact() {
        let f = QueryFilter {
            series_number: Some("5".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(tag::SERIES_NUMBER, AttrValue::UInt(5))])));
        assert!(f.matches(&attrs(&[(tag::SERIES_NUMBER, AttrValue::Text("5".into()))])));
        assert!(!f.matches(&attrs(&[(tag::SERIES_NUMBER, AttrValue::UInt(6))])));
    }

    #[test]
    fn series_number_prefix_wildcard() {
        let f = QueryFilter {
            series_number: Some("5*".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(tag::SERIES_NUMBER, AttrValue::UInt(57))])));
        assert!(f.matches(&attrs(&[(
            tag::SERIES_NUMBER,
            AttrValue::Text("53".into())
        )])));
        assert!(!f.matches(&attrs(&[(tag::SERIES_NUMBER, AttrValue::UInt(4))])));
    }

    #[test]
    fn series_number_absent_field_no_match() {
        // Filter asks for a series number but the attribute set has none.
        let f = QueryFilter {
            series_number: Some("5".into()),
            ..Default::default()
        };
        assert!(!f.matches(&attrs(&[(
            tag::SERIES_INSTANCE_UID,
            AttrValue::Text("1.2.3".into())
        )])));
    }

    #[test]
    fn fuzzymatch_finds_single_edit() {
        let f = QueryFilter {
            patient_name: Some("ивонов".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        // 1 substitution о->а: matches "иванов".
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("иванов".into())
        )])));
    }

    #[test]
    fn fuzzymatch_rejects_true_nonmatches() {
        // Short pattern inside a longer name matches (approximate substring).
        let short = QueryFilter {
            patient_name: Some("ивн".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        assert!(short.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("иванов".into())
        )])));

        // But a pattern longer than the value, or with 3 substitutions, does not.
        let long = QueryFilter {
            patient_name: Some("иванов".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        assert!(!long.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("ивн".into())
        )])));

        let three_subs = QueryFilter {
            patient_name: Some("ивн".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        assert!(!three_subs.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("сидоров".into())
        )])));
    }

    #[test]
    fn fuzzymatch_off_keeps_exact_semantics() {
        let f = QueryFilter {
            patient_name: Some("иванов".into()),
            fuzzymatch: false,
            ..Default::default()
        };
        // Without fuzzymatch, a different spelling does not match.
        assert!(!f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("ивонов".into())
        )])));
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("иванов".into())
        )])));
    }

    #[test]
    fn wildcard_takes_precedence_over_fuzzymatch() {
        let f = QueryFilter {
            patient_name: Some("ива*".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("иванов".into())
        )])));
        // A two-char pattern with a wildcard still glob-matches, never Bitap.
        assert!(!f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("петров".into())
        )])));
    }

    #[test]
    fn fuzzymatch_applies_to_referring_physician() {
        let f = QueryFilter {
            referring_physician: Some("смитнова".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::REFERRING_PHYSICIAN,
            AttrValue::Text("смитнова".into())
        )])));
    }

    // FR-vet stage 1: ResponsiblePerson (0010,2297) is a PN search key routed
    // through `name_predicate`, so it inherits the same exact/glob/fuzzy and
    // case semantics as PatientName / ReferringPhysicianName for free.
    #[test]
    fn responsible_person_exact_match() {
        let f = QueryFilter {
            responsible_person: Some("иванов^иван".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::RESPONSIBLE_PERSON,
            AttrValue::Text("иванов^иван".into())
        )])));
        assert!(!f.matches(&attrs(&[(
            tag::RESPONSIBLE_PERSON,
            AttrValue::Text("petrov^petr".into())
        )])));
    }

    #[test]
    fn responsible_person_glob_match() {
        let f = QueryFilter {
            responsible_person: Some("ивано*".into()),
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::RESPONSIBLE_PERSON,
            AttrValue::Text("иванов^иван".into())
        )])));
    }

    #[test]
    fn responsible_person_fuzzy_single_edit() {
        let f = QueryFilter {
            responsible_person: Some("иваноф".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        // 1 substitution -> matches "иванов".
        assert!(f.matches(&attrs(&[(
            tag::RESPONSIBLE_PERSON,
            AttrValue::Text("иванов^иван".into())
        )])));
    }

    #[test]
    fn responsible_person_absent_attr_never_matches() {
        // A query for a responsible person returns nothing when the manifest
        // carries no (0010,2297) attribute.
        let f = QueryFilter {
            responsible_person: Some("иванов".into()),
            ..Default::default()
        };
        assert!(!f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("иванов".into())
        )])));
        assert!(!f.matches(&attrs(&[])));
    }

    #[test]
    fn performing_and_reading_physician_keys_are_expected() {
        let f = QueryFilter {
            performing_physician: Some("Perf*".into()),
            name_of_reading_physicians: Some("Read^Doc".into()),
            ..Default::default()
        };
        let exp = f.build_expected();
        assert!(exp.contains_key(&tag::PERFORMING_PHYSICIAN));
        assert!(exp.contains_key(&tag::NAME_OF_READING_PHYSICIANS));
        // Manifest carrying both tags matches.
        assert!(f.matches(&attrs(&[
            (
                tag::PERFORMING_PHYSICIAN,
                AttrValue::Text("Performing^One".into())
            ),
            (
                tag::NAME_OF_READING_PHYSICIANS,
                AttrValue::Text("Read^Doc".into())
            ),
        ])));
        // Manifest missing the reading key does not match.
        assert!(!f.matches(&attrs(&[(
            tag::PERFORMING_PHYSICIAN,
            AttrValue::Text("Performing^One".into())
        )])));
        // Manifest without either tag does not match.
        assert!(!f.matches(&attrs(&[(
            tag::STUDY_DESCRIPTION,
            AttrValue::Text("Chest".into())
        )])));
    }

    #[test]
    fn fuzzymatch_applies_to_performing_physician() {
        let f = QueryFilter {
            performing_physician: Some("petrova".into()),
            fuzzymatch: true,
            ..Default::default()
        };
        assert!(f.matches(&attrs(&[(
            tag::PERFORMING_PHYSICIAN,
            AttrValue::Text("petrova".into())
        )])));
    }

    #[test]
    fn case_insensitive_pn_glob() {
        // Standard (default): case-sensitive - lowercase does NOT match.
        let strict = QueryFilter {
            patient_name: Some("solgalov*".into()),
            ..Default::default()
        };
        assert!(!strict.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("Solgalov^Daniil^Yur'evich".into())
        )])));

        // Deviation: pn_case_insensitive - lowercase matches.
        let ci = QueryFilter {
            patient_name: Some("solgalov*".into()),
            pn_case_insensitive: true,
            ..Default::default()
        };
        assert!(ci.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("Solgalov^Daniil^Yur'evich".into())
        )])));

        // Applies to ReferringPhysicianName too (both PN keys).
        let ci_rp = QueryFilter {
            referring_physician: Some("petrov*".into()),
            pn_case_insensitive: true,
            ..Default::default()
        };
        assert!(ci_rp.matches(&attrs(&[(
            tag::REFERRING_PHYSICIAN,
            AttrValue::Text("PETROV^P".into())
        )])));
    }

    #[test]
    fn fuzzymatch_override_without_client_flag() {
        let f = QueryFilter {
            patient_name: Some("Salgalov".into()),
            fuzzymatch: false,
            fuzzymatch_override: true,
            ..Default::default()
        };
        // One-edit match on "Solgalov^...".
        assert!(f.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("Solgalov^Daniil^Yur'evich".into())
        )])));

        // Wildcard still wins over the override (never Bitap).
        let g = QueryFilter {
            patient_name: Some("Solgal*".into()),
            fuzzymatch: false,
            fuzzymatch_override: true,
            ..Default::default()
        };
        assert!(g.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("Solgalov^Daniil^Yur'evich".into())
        )])));
        assert!(!g.matches(&attrs(&[(
            tag::PATIENT_NAME,
            AttrValue::Text("Petrov^P".into())
        )])));
    }
}
