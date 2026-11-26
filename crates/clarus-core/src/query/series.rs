// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Series/instance-level QIDO search and sorting over a decoded study
//! manifest.
//!
//! The study is known from the request path - the handler opens and decodes
//! the one `manifests/{study_uid}.manifest`. There is no candidate search:
//! these functions run a single linear pass over the manifest's `series` /
//! `instances` sets, filter by the series/instance search keys, and sort
//! Per the query contract below.
//!
//! # Zero-heap borrow form
//!
//! Matching and sorting write **borrowed references** into a caller-owned
//! `out: &mut Vec<...>` and sort in place via `sort_unstable_by` (no heap).
//! At steady state these functions allocate nothing; the caller's `out`
//! buffer may grow on its first call. The handler
//! owns the decoded `Manifest`, so the borrows stay valid for the
//! response lifetime.
//!
//! # Sort contract (12.08, mirrors the SQLite oracle's `ORDER BY`)
//!
//! - Series: `SeriesNumber` numeric ASC, secondary `SeriesInstanceUID` ASC,
//!   missing / empty / non-numeric last.
//! - Instances: `InstanceNumber` numeric ASC, secondary `SOPInstanceUID` ASC,
//!   missing / empty / non-numeric last.

use crate::manifest::{AttrValue, InstanceRecord, Manifest, SeriesRecord};

/// Series-level search keys (mirrors the SQLite `SeriesFilter`).
#[derive(Debug, Clone, Default)]
pub struct SeriesFilter {
    /// SeriesInstanceUID (0020,000E) - exact match.
    pub series_instance_uid: Option<String>,
    /// SeriesNumber (0020,0011) - glob over the numeric render, the same
    /// semantics as [`InstanceFilter::instance_number`].
    ///
    /// PS3.18 Table 10.6.1-5 lists SeriesNumber as a Series-level matching key.
    /// It was previously a sort key only, so `SeriesNumber=2` returned every
    /// series in the study.
    pub series_number: Option<String>,
    /// Modality (0008,0060) - comma-separated list, any element exact.
    pub modality: Option<String>,
    /// SeriesDate (0008,0021) - exact match.
    pub series_date: Option<String>,
    /// SeriesTime (0008,0031) - LIKE match (`*` / `?` wildcards).
    pub series_time: Option<String>,
    /// SeriesDescription (0008,103E) - LIKE match (`*` / `?` wildcards).
    pub series_description: Option<String>,
    /// Manufacturer (0008,0070) - LIKE match (08.09 vendor extension).
    pub manufacturer: Option<String>,
    /// InstitutionalDepartmentName (0008,1040) - LIKE match (08.09).
    pub institutional_department_name: Option<String>,
    /// OperatorsName (0008,1070) - PN wildcard (08.09 vendor extension,
    /// PerformingPhysicianName template: no fuzzy, `case_sensitive_pn` applies
    /// via [`Self::pn_case_insensitive`] exactly as on the study PN keys).
    pub operators_name: Option<String>,
    /// ProtocolName (0018,1030) - LIKE match (08.09 vendor extension).
    pub protocol_name: Option<String>,
    /// StationName (0008,1010) - LIKE match (`*` / `?` wildcards).
    /// Vendor extension 08.09 (dcm4chee/Orthanc parity); declared in
    /// docs/CONFORMANCE.md. Same semantics as `series_description`.
    pub station_name: Option<String>,
    /// BodyPartExamined (0018,0015) - LIKE match (`*` / `?` wildcards).
    /// Vendor extension 08.09; same semantics as `series_description`.
    pub body_part_examined: Option<String>,
    /// PerformedProcedureStepStartDate (0040,0244) - exact match.
    pub pps_start_date: Option<String>,
    /// PerformedProcedureStepStartTime (0040,0245) - LIKE match.
    pub pps_start_time: Option<String>,
    /// RequestAttributesSequence (0040,0275) > ScheduledProcedureStepID
    /// (0040,0009) - exact match.
    ///
    /// PS3.18 Table 10.6.1-5 makes `00400275.00400009` a REQUIRED series
    /// matching key, so the manifest stores the scalar under its own tag and
    /// this filters on it directly.
    pub scheduled_procedure_step_id: Option<String>,
    /// RequestAttributesSequence (0040,0275) > RequestedProcedureID
    /// (0040,1001) - exact match. The second REQUIRED nested key.
    pub requested_procedure_id: Option<String>,
    /// Case-insensitive glob on the PN series key (`OperatorsName`),
    /// mirroring `QueryFilter.pn_case_insensitive` (the
    /// `[query] case_sensitive_pn = false` knob). `false` (default) =
    /// standard case-sensitive matching. Case-insensitive matching
    /// lowercases both sides and allocates by construction - the same
    /// documented deviation as the study-level PN keys.
    pub pn_case_insensitive: bool,
}

/// Instance-level search keys. PS3.18 2026c Table 10.6.1-5 makes SOPClassUID
/// and InstanceNumber matching keys alongside SOPInstanceUID - the old 12.08
/// contract ("InstanceNumber is a sort key only, never a filter") is
/// InstanceNumber remains the sort key too.
#[derive(Debug, Clone, Default)]
pub struct InstanceFilter {
    /// SOPInstanceUID (0008,0018) - exact match.
    pub sop_instance_uid: Option<String>,
    /// SOPClassUID (0008,0016) - exact match.
    pub sop_class_uid: Option<String>,
    /// InstanceNumber (0020,0013) - glob over the numeric render, the same
    /// semantics as the study-level SeriesNumber `Predicate::Glob`.
    pub instance_number: Option<String>,
}

/// Collect the series of `manifest` matching `filter`, in sorted order, into
/// `out` (cleared first). Borrowed references, steady-state zero allocation.
pub fn search_series<'a>(
    manifest: &'a Manifest,
    filter: &SeriesFilter,
    out: &mut Vec<&'a SeriesRecord>,
) {
    out.clear();
    if !filter.has_constraints() {
        // No filters: still sorted (numeric ASC, secondary UID ASC, missing
        // last) per the contract - never the raw STOW order.
        out.extend(manifest.series.iter());
        out.sort_unstable_by(|a, b| cmp_series(a, b));
        return;
    }
    for s in &manifest.series {
        if series_matches(filter, &s.attrs) {
            out.push(s);
        }
    }
    out.sort_unstable_by(|a, b| cmp_series(a, b));
}

/// Whether `inst` must stay hidden from this caller.
///
/// Two independent questions, deliberately kept apart:
/// `expose` is the SURFACE asking to see hidden instances at all (the QA
/// root);
/// `inst.is_exposable()` is the RECORD saying its hide is liftable.
///
/// A Quality rejection is liftable (AIRA 57.2.1: a webservice endpoint
/// configured to provide access to hidden instances). The IOCM AnyClass
/// reasons are not: patient-safety and incorrect-MWL hide diagnostic images
/// with no expose path, so they stay hidden on every surface - including the
/// one that exists to lift Quality. Reading the reason from the record is what
/// makes that impossible to get wrong at a call site.
#[inline]
fn hides(inst: &InstanceRecord, expose: bool) -> bool {
    inst.is_rejected() && !(expose && inst.is_exposable())
}
/// Collect the instances of `series_uid` within `manifest` matching `filter`,
/// in sorted order, into `out` (cleared first). Borrowed references,
/// steady-state zero allocation.
///
/// `expose` lifts the hide for reasons that permit it (Quality only - see
/// [`hides`]); with it `false` every sequestered instance stays hidden. It is
/// a plain field read on the borrowed record, so the zero-allocation property
/// holds in every branch.
///
/// Returns `false` when no series with `series_uid` exists in the manifest.
pub fn search_instances<'a>(
    manifest: &'a Manifest,
    series_uid: &str,
    filter: &InstanceFilter,
    out: &mut Vec<&'a InstanceRecord>,
    expose: bool,
) -> bool {
    out.clear();
    let Some(series) = manifest
        .series
        .iter()
        .find(|s| text_attr(&s.attrs, crate::query::tag::SERIES_INSTANCE_UID) == Some(series_uid))
    else {
        return false;
    };
    // `hides` is the whole decision. The earlier shape
    // `!hide_rejected || !hides(i, expose)` made `expose` dead whenever the
    // surface also stopped hiding: the left operand short-circuited to "keep"
    // and the reason was never consulted, so the QA root showed every
    // sequestered instance - including the ones no surface may reveal.
    if filter.has_constraints() {
        for i in &series.instances {
            if !hides(i, expose) && filter.matches(&i.attrs) {
                out.push(i);
            }
        }
    } else {
        out.extend(series.instances.iter().filter(|i| !hides(i, expose)));
    }
    out.sort_unstable_by(|a, b| cmp_instance(a, b));
    true
}

/// One instance matched by a **top-level** (global) instance QIDO scan, with
/// the owning Study/Series Instance UIDs **borrowed** from the decoded
/// manifest so the renderer can inject them without cloning (ap101).
#[derive(Debug, Clone, Copy)]
pub struct GlobalInstance<'a> {
    /// The matched instance record (borrowed from `manifests`).
    pub instance: &'a InstanceRecord,
    /// The study's StudyInstanceUID (0020,000D), borrowed from the manifest.
    pub study_uid: &'a str,
    /// The series' SeriesInstanceUID (0020,000E), borrowed from the manifest.
    pub series_uid: &'a str,
}

/// One series matched by a **top-level** (global) series QIDO scan, with the
/// owning Study Instance UID **borrowed** from the decoded manifest so the
/// renderer can inject it without cloning (ap101). Mirrors [`GlobalInstance`]
/// for the `/series` surface.
#[derive(Debug, Clone, Copy)]
pub struct GlobalSeries<'a> {
    /// The matched series record (borrowed from `manifests`).
    pub series: &'a SeriesRecord,
    /// The study's StudyInstanceUID (0020,000D), borrowed from the manifest.
    pub study_uid: &'a str,
}

/// Collect all series across **every** manifest in `manifests` that satisfy
/// `filter` (no study scope), globally sorted by `SeriesInstanceUID` ASC.
///
/// This backs the top-level `GET /dicomweb/series` route.
/// There is no candidate acceleration and no study filter - every study's
/// series are scanned and the whole archive's matches are merged into `out`
/// (cleared first). The caller owns `manifests` (it decoded and holds them
/// for the request) and paginates the returned borrowed slice in place, so
/// `offset` pages are deterministic (a stable global UID order).
///
/// Borrow-only: `out` holds `GlobalSeries` references (no UID/attr clones);
/// the sort keys are borrowed `&str` views of the SeriesInstanceUID attrs, so
/// matching and sorting allocate nothing at steady state.
pub fn search_all_series<'a>(
    manifests: &'a [Manifest],
    filter: &SeriesFilter,
    out: &mut Vec<GlobalSeries<'a>>,
) {
    search_all_series_matching(manifests, filter, None, out)
}

/// [`search_all_series`] restricted to manifests that pass a STUDY predicate.
///
/// PS3.18 10.6.1.2.1 makes the Study IE search keys legal on the All Series
/// resource, so a `/series?PatientID=...` query must narrow the archive before
/// its series are considered. The predicate is applied per manifest here rather
/// than by pre-filtering the caller's slice, because the returned series
/// borrow from the manifests and a filtered `Vec<&Manifest>` cannot back a
/// `&[Manifest]` parameter.
pub fn search_all_series_matching<'a>(
    manifests: &'a [Manifest],
    filter: &SeriesFilter,
    study: Option<&crate::query::filter::QueryFilter>,
    out: &mut Vec<GlobalSeries<'a>>,
) {
    out.clear();
    for manifest in manifests {
        if let Some(study) = study
            && !crate::query::filter::match_study_attrs(study, manifest)
        {
            continue;
        }
        let study_uid =
            text_attr(&manifest.study.attrs, crate::query::tag::STUDY_INSTANCE_UID).unwrap_or("");
        for s in &manifest.series {
            if filter.matches(&s.attrs) {
                out.push(GlobalSeries {
                    series: s,
                    study_uid,
                });
            }
        }
    }
    // Global deterministic order: SeriesInstanceUID ASC (the offset-pagination
    // contract for the top-level route - no numeric secondary key).
    out.sort_unstable_by(|a, b| {
        let au = text_attr(&a.series.attrs, crate::query::tag::SERIES_INSTANCE_UID).unwrap_or("");
        let bu = text_attr(&b.series.attrs, crate::query::tag::SERIES_INSTANCE_UID).unwrap_or("");
        au.cmp(bu)
    });
}

/// Collect every instance across **all** series of **every** manifest in
/// `manifests` that satisfies `filter` (no study scope), globally sorted by
/// `SOPInstanceUID` ASC, each paired with its borrowed Study/Series UIDs.
///
/// This backs the top-level `GET /dicomweb/instances` route.
/// The Study and Series Instance UIDs are borrowed from the manifest (the
/// study QIDO `0020,000D` and each series' `0020,000E`) so the renderer can
/// inject them without cloning. The caller owns `manifests` and paginates the
/// returned borrowed slice in place.
///
/// Borrow-only: `out` holds `GlobalInstance` references (no UID/attr clones);
/// matching and sorting allocate nothing at steady state, EXCEPT the study
/// predicate map below.
///
/// # Allocation contract
///
/// When `study_filter` is present, this function calls
/// `QueryFilter::build_expected()` once per call, which allocates one small
/// map for the filter's attribute keys. The map is not cached across calls and
/// is not passed in by the caller: reusing it would mean a threaded-through
/// buffer that every caller (there is one route) has to own and size, which
/// buys nothing for a single map per HTTP request. Callers that later need the
/// zero-allocation guarantee on this path must pass the map in; the signature
/// deliberately does not anticipate that.
pub fn search_all_instances<'a>(
    manifests: &'a [Manifest],
    filter: &InstanceFilter,
    series_filter: &SeriesFilter,
    study_filter: Option<&crate::query::filter::QueryFilter>,
    out: &mut Vec<GlobalInstance<'a>>,
    expose: bool,
) {
    out.clear();
    // P1: the GLOBAL /instances route accepts Study IE keys (PS3.18 10.6.1.2.1),
    // and accepting a key without applying it answers a filtered query with
    // every patient's instances - strictly wider than the ask. The study gate
    // runs FIRST, on `StudyQido.attrs`, through the same `QueryFilter` the
    // study route uses, so both routes agree on what a key means.
    let study_expected = study_filter.map(|f| f.build_expected());
    for manifest in manifests {
        // `match_study_attrs_prebuilt` rather than `matches_prebuilt`: the
        // expected map covers the attribute keys, but ModalitiesInStudy and
        // OtherPatientIDs are matched by their own SET comparisons (their
        // stored value lives in a dedicated field / a multi-valued key), so a
        // gate that consults only the map ignores them. The previous gate also
        // skipped the predicate entirely when the map was empty, which meant
        // `?ModalitiesInStudy=CT` on the global `/instances` route returned
        // every study's instances - strictly wider than the ask.
        if let (Some(sf), Some(expected)) = (study_filter, study_expected.as_ref())
            && !crate::query::filter::match_study_attrs_prebuilt(sf, manifest, expected)
        {
            continue;
        }
        let study_uid =
            text_attr(&manifest.study.attrs, crate::query::tag::STUDY_INSTANCE_UID).unwrap_or("");
        for series in &manifest.series {
            // Series-level keys (Modality on the relational instance routes)
            // gate the whole series: a non-matching series contributes no
            // instances. A filter with no series constraint matches every
            // series.
            if !series_filter.matches(&series.attrs) {
                continue;
            }
            let series_uid =
                text_attr(&series.attrs, crate::query::tag::SERIES_INSTANCE_UID).unwrap_or("");
            for inst in &series.instances {
                if hides(inst, expose) {
                    continue;
                }
                if filter.matches(&inst.attrs) {
                    out.push(GlobalInstance {
                        instance: inst,
                        study_uid,
                        series_uid,
                    });
                }
            }
        }
    }
    // Global deterministic order: SOPInstanceUID ASC.
    out.sort_unstable_by(|a, b| {
        let au = text_attr(&a.instance.attrs, crate::query::tag::SOP_INSTANCE_UID).unwrap_or("");
        let bu = text_attr(&b.instance.attrs, crate::query::tag::SOP_INSTANCE_UID).unwrap_or("");
        au.cmp(bu)
    });
}

impl SeriesFilter {
    /// Whether the filter carries at least one search key.
    fn has_constraints(&self) -> bool {
        self.series_instance_uid.is_some()
            || self.modality.is_some()
            || self.series_date.is_some()
            || self.series_time.is_some()
            || self.series_description.is_some()
            || self.manufacturer.is_some()
            || self.institutional_department_name.is_some()
            || self.operators_name.is_some()
            || self.protocol_name.is_some()
            || self.station_name.is_some()
            || self.body_part_examined.is_some()
            || self.pps_start_date.is_some()
            || self.pps_start_time.is_some()
            || self.scheduled_procedure_step_id.is_some()
            || self.requested_procedure_id.is_some()
            || self.series_number.is_some()
    }

    /// Whether a series' attribute vector satisfies the filter (allocation-free).
    ///
    /// This is the single source of truth shared by [`search_series`] and the
    /// SQLite-parity reference; keeping it public lets a DB-free proptest reuse
    /// it as the in-memory oracle without importing a database.
    pub fn matches(&self, attrs: &[(u32, AttrValue)]) -> bool {
        series_matches(self, attrs)
    }
}

impl InstanceFilter {
    /// Whether the filter carries at least one search key.
    fn has_constraints(&self) -> bool {
        self.sop_instance_uid.is_some()
            || self.sop_class_uid.is_some()
            || self.instance_number.is_some()
    }

    /// Whether an instance's attribute vector satisfies the filter
    /// (allocation-free): SOPInstanceUID exact, SOPClassUID exact, and
    /// InstanceNumber as a glob over the decimal render (Table
    /// 10.6.1-5). A filter with no key matches every instance.
    pub fn matches(&self, attrs: &[(u32, AttrValue)]) -> bool {
        if let Some(uids) = self.sop_instance_uid.as_deref()
            && !uid_list_contains(uids, text_attr(attrs, crate::query::tag::SOP_INSTANCE_UID))
        {
            return false;
        }
        if let Some(cls) = self.sop_class_uid.as_deref()
            && !uid_list_contains(cls, text_attr(attrs, crate::query::tag::SOP_CLASS_UID))
        {
            return false;
        }
        if let Some(num) = self.instance_number.as_deref()
            && !instance_number_matches(attrs, num)
        {
            return false;
        }
        true
    }
}

/// Whether `have` is one member of a comma-separated UID list.
///
/// A UID key is single-valued in Table 10.6.1-5, but the value itself may be a
/// list: `sopInstanceUid=1.2.3,1.2.4` asks for the union of those instances.
/// Comparing the whole string against one stored UID matched neither member, so
/// a legal list query returned nothing. A missing list or a missing attribute
/// does not match.
fn uid_list_contains(list: &str, have: Option<&str>) -> bool {
    match have {
        Some(have) => list.split(',').any(|member| member.trim() == have),
        None => false,
    }
}

/// Whether a series' attribute vector satisfies the filter (allocation-free).
fn series_matches(filter: &SeriesFilter, attrs: &[(u32, AttrValue)]) -> bool {
    if let Some(uids) = filter.series_instance_uid.as_deref()
        && !uid_list_contains(
            uids,
            text_attr(attrs, crate::query::tag::SERIES_INSTANCE_UID),
        )
    {
        return false;
    }
    if let Some(num) = filter.series_number.as_deref()
        && !series_number_matches(attrs, num)
    {
        return false;
    }
    if let Some(mods) = filter.modality.as_deref()
        && !mods.is_empty()
    {
        let Some(have) = text_attr(attrs, crate::query::tag::MODALITY) else {
            return false;
        };
        if !mods.split(',').any(|m| m.trim() == have) {
            return false;
        }
    }
    if let Some(d) = filter.series_date.as_deref()
        && !date_attr_equals(attrs, crate::query::tag::SERIES_DATE, d)
    {
        return false;
    }
    if let Some(t) = filter.series_time.as_deref()
        && !text_attr_like(attrs, crate::query::tag::SERIES_TIME, t)
    {
        return false;
    }
    if let Some(desc) = filter.series_description.as_deref()
        && !text_attr_like(attrs, crate::query::tag::SERIES_DESCRIPTION, desc)
    {
        return false;
    }
    // 08.09 vendor extensions: Manufacturer (0008,0070),
    // InstitutionalDepartmentName (0008,1040) and ProtocolName (0018,1030)
    // mirror SeriesDescription - LIKE match (`*` / `?`), case-sensitive,
    // allocation-free, no fuzzy wiring (YAGNI).
    if let Some(m) = filter.manufacturer.as_deref()
        && !text_attr_like(attrs, crate::query::tag::MANUFACTURER, m)
    {
        return false;
    }
    if let Some(dn) = filter.institutional_department_name.as_deref()
        && !text_attr_like(attrs, crate::query::tag::INSTITUTIONAL_DEPARTMENT_NAME, dn)
    {
        return false;
    }
    if let Some(p) = filter.protocol_name.as_deref()
        && !text_attr_like(attrs, crate::query::tag::PROTOCOL_NAME, p)
    {
        return false;
    }
    // 08.09 vendor extension: OperatorsName (0008,1070) is a PN key and
    // follows the PN wildcard template - no fuzzy wiring, but
    // `case_sensitive_pn` applies exactly as on ReferringPhysicianName /
    // PerformingPhysicianName (case-insensitive matching lowercases both
    // sides and allocates by construction, identical to the study path).
    if let Some(op) = filter.operators_name.as_deref() {
        if !filter.pn_case_insensitive {
            if !text_attr_like(attrs, crate::query::tag::OPERATORS_NAME, op) {
                return false;
            }
        } else if !pn_attr_like_ci(attrs, crate::query::tag::OPERATORS_NAME, op) {
            return false;
        }
    }
    // 08.09 vendor extensions (dcm4chee/Orthanc parity): StationName
    // (0008,1010) and BodyPartExamined (0018,0015) mirror SeriesDescription -
    // LIKE match (`*` / `?`), case-sensitive, allocation-free. No fuzzy
    // wiring and no n-gram indexing for these keys (YAGNI).
    if let Some(st) = filter.station_name.as_deref()
        && !text_attr_like(attrs, crate::query::tag::STATION_NAME, st)
    {
        return false;
    }
    if let Some(bp) = filter.body_part_examined.as_deref()
        && !text_attr_like(attrs, crate::query::tag::BODY_PART_EXAMINED, bp)
    {
        return false;
    }
    // The PerformedProcedureStepStartDate/Time series matching keys mirror
    // SeriesDate (DA, exact) / SeriesTime (TM, LIKE). The two nested keys of
    // RequestAttributesSequence (0040,0275) are stored as their own scalars
    // (0040,0009, 0040,1001) - PS3.18 Table 10.6.1-5 types both as REQUIRED
    // series matching attributes - and match exactly, since they are SH
    // identifiers rather than patterns.
    // PPS Start Date/Time are REQUIRED series matching keys (PS3.18 Table
    // 10.6.1-5) and must support PS3.4 C.2.2.2.5 range matching, so they go
    // through the shared DA/TM range predicates rather than the exact/LIKE
    // helpers. SeriesDate/SeriesTime are NOT in that table and keep their
    // exact/LIKE semantics above.
    if let Some(d) = filter.pps_start_date.as_deref()
        && !date_attr_range(attrs, crate::query::tag::PPS_START_DATE, d)
    {
        return false;
    }
    if let Some(t) = filter.pps_start_time.as_deref()
        && !time_attr_range(attrs, crate::query::tag::PPS_START_TIME, t)
    {
        return false;
    }
    if let Some(v) = filter.scheduled_procedure_step_id.as_deref()
        && text_attr(attrs, crate::query::tag::SCHEDULED_PROCEDURE_STEP_ID) != Some(v)
    {
        return false;
    }
    if let Some(v) = filter.requested_procedure_id.as_deref()
        && text_attr(attrs, crate::query::tag::REQUESTED_PROCEDURE_ID) != Some(v)
    {
        return false;
    }
    true
}

/// Comparator for series: numeric SeriesNumber ASC, secondary UID ASC,
/// missing/non-numeric last.
fn cmp_series(a: &SeriesRecord, b: &SeriesRecord) -> std::cmp::Ordering {
    let (ap, an, au) = series_sort_key(a);
    let (bp, bn, bu) = series_sort_key(b);
    // Present (`true`) sorts before missing (`false`); the reversed flag
    // comparison places numeric rows ahead of those with no usable number.
    bp.cmp(&ap)
        .then_with(|| an.cmp(&bn))
        .then_with(|| au.cmp(bu))
}

/// Comparator for instances: numeric InstanceNumber ASC, secondary UID ASC,
/// missing/non-numeric last.
fn cmp_instance(a: &InstanceRecord, b: &InstanceRecord) -> std::cmp::Ordering {
    let (ap, an, au) = instance_sort_key(a);
    let (bp, bn, bu) = instance_sort_key(b);
    bp.cmp(&ap)
        .then_with(|| an.cmp(&bn))
        .then_with(|| au.cmp(bu))
}

/// `(present, numeric, uid)` sort key for a series. `present=false` (absent,
/// empty, or non-numeric SeriesNumber) sorts last.
fn series_sort_key(s: &SeriesRecord) -> (bool, i64, &str) {
    let uid = text_attr(&s.attrs, crate::query::tag::SERIES_INSTANCE_UID).unwrap_or("");
    match numeric_attr(&s.attrs, crate::query::tag::SERIES_NUMBER) {
        Some(n) => (true, n, uid),
        None => (false, 0, uid),
    }
}

/// `(present, numeric, uid)` sort key for an instance. `present=false`
/// (absent, empty, or non-numeric InstanceNumber) sorts last.
fn instance_sort_key(i: &InstanceRecord) -> (bool, i64, &str) {
    let uid = text_attr(&i.attrs, crate::query::tag::SOP_INSTANCE_UID).unwrap_or("");
    match numeric_attr(&i.attrs, crate::query::tag::INSTANCE_NUMBER) {
        Some(n) => (true, n, uid),
        None => (false, 0, uid),
    }
}

/// Parse the signed numeric sort-key attribute `tag` (SeriesNumber /
/// InstanceNumber) from `attrs`. `Some(n)` for an integer; `None` when absent,
/// empty, or non-numeric - such records sort last.
///
/// IS is a SIGNED integer string (PS3.5 Table 6.2-1), and a negative value
/// really can reach the manifest: the parser reads the attribute as `i32`
/// (`opt_int` in the dicom_rs parser) and STOW stores it as text, so no
/// validation rejects `-1` on the way in. Parsing as `u64` therefore pushed a
/// negative number into the MISSING group, sorting it after every real number
/// instead of before 0 - a wrong order, not a tolerated one. The signed key
/// keeps every non-negative value where it was and orders negatives below
/// them.
fn numeric_attr(attrs: &[(u32, AttrValue)], tag: u32) -> Option<i64> {
    for (t, v) in attrs {
        if *t != tag {
            continue;
        }
        return match v {
            AttrValue::Text(s) => s.trim().parse::<i64>().ok(),
            AttrValue::UInt(u) => i64::try_from(*u).ok(),
            _ => None,
        };
    }
    None
}

/// Whether the instance number attribute (0020,0013) glob-matches `pat` in
/// either stored form (`Text` directly, `UInt` via a stack decimal render) -
/// the mirror of the study-level SeriesNumber `Predicate::Glob` matching
/// (Table 10.6.1-5). No heap: the render buffer is on the stack.
fn instance_number_matches(attrs: &[(u32, AttrValue)], pat: &str) -> bool {
    number_matches(attrs, crate::query::tag::INSTANCE_NUMBER, pat)
}

/// Whether the series number attribute (0020,0011) glob-matches `pat`.
///
/// The same number semantics as [`instance_number_matches`]: PS3.18 Table
/// 10.6.1-5 puts SeriesNumber in the Series-level matching set, so it has to
/// filter, not only sort.
fn series_number_matches(attrs: &[(u32, AttrValue)], pat: &str) -> bool {
    number_matches(attrs, crate::query::tag::SERIES_NUMBER, pat)
}

/// Glob matching for a numeric attribute, in either stored form.
fn number_matches(attrs: &[(u32, AttrValue)], tag: u32, pat: &str) -> bool {
    for (t, v) in attrs {
        if *t != tag {
            continue;
        }
        return match v {
            AttrValue::Text(s) => glob_match_bytes(pat, s),
            AttrValue::UInt(u) => {
                let mut buf = [0u8; 20];
                let mut i = buf.len();
                let mut rem = *u;
                loop {
                    i -= 1;
                    buf[i] = b'0' + (rem % 10) as u8;
                    rem /= 10;
                    if rem == 0 {
                        break;
                    }
                }
                // ASCII digits are always valid UTF-8.
                let digits = std::str::from_utf8(&buf[i..]).unwrap_or("");
                glob_match_bytes(pat, digits)
            }
            _ => false,
        };
    }
    false
}

/// The text value of attribute `tag`, if any.
fn text_attr(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&str> {
    for (t, v) in attrs {
        if *t == tag {
            if let AttrValue::Text(s) = v {
                return Some(s.as_str());
            }
            return None;
        }
    }
    None
}

/// Whether the text attribute `tag` glob-matches `pat` (LIKE; `*`/`?`).
fn text_attr_like(attrs: &[(u32, AttrValue)], tag: u32, pat: &str) -> bool {
    match text_attr(attrs, tag) {
        Some(have) => glob_match_bytes(pat, have),
        None => false,
    }
}

/// Case-insensitive glob over the text attribute `tag` (`case_sensitive_pn =
/// false` knob). Lowercases both sides, so this allocates by construction -
/// the documented PN-key deviation, identical to the study-level `GlobCi`
/// predicate. The default (standard, case-sensitive) path stays
/// allocation-free via [`text_attr_like`].
fn pn_attr_like_ci(attrs: &[(u32, AttrValue)], tag: u32, pat: &str) -> bool {
    match text_attr(attrs, tag) {
        Some(have) => glob_match_bytes(&pat.to_lowercase(), &have.to_lowercase()),
        None => false,
    }
}

/// The stored value of attribute `tag`, if any (either form).
///
/// The DA/TM range predicates read the RAW `AttrValue` (a date is `UInt`,
/// a time is `Text`), unlike [`text_attr`] which only sees `Text`.
fn attr(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&AttrValue> {
    attrs.iter().find_map(|(t, v)| (*t == tag).then_some(v))
}

/// Whether the DA attribute `tag` falls inside the range pattern `pat`
/// (PS3.4 C.2.2.2.5: inclusive, either bound allowed absent). Delegates to
/// the shared [`crate::query::range::da_range_matches`], which reads the
/// stored `UInt`/`Text` and compares against the pattern.
fn date_attr_range(attrs: &[(u32, AttrValue)], tag: u32, pat: &str) -> bool {
    match attr(attrs, tag) {
        Some(have) => crate::query::range::da_range_matches(have, pat),
        None => false,
    }
}

/// Whether the TM attribute `tag` falls inside the range pattern `pat` - the
/// TM sibling of [`date_attr_range`] via
/// [`crate::query::range::tm_range_matches`].
fn time_attr_range(attrs: &[(u32, AttrValue)], tag: u32, pat: &str) -> bool {
    match attr(attrs, tag) {
        Some(have) => crate::query::range::tm_range_matches(have, pat),
        None => false,
    }
}

/// Whether the date attribute `tag` equals `want`.
///
/// The captured value is a numeric `YYYYMMDD` `UInt`. For parity with
/// the SQLite oracle (which compares the TEXT `series_date` column with `=`),
/// the stored `UInt` is rendered to its canonical zero-padded 8-digit string
/// and compared string-wise to `want`. A `Text` fallback compares directly.
fn date_attr_equals(attrs: &[(u32, AttrValue)], tag: u32, want: &str) -> bool {
    for (t, v) in attrs {
        if *t != tag {
            continue;
        }
        return match v {
            AttrValue::UInt(u) => date_matches(*u, want),
            AttrValue::Text(s) => s == want,
            _ => false,
        };
    }
    false
}

/// Whether a `YYYYMMDD` integer equals a canonical 8-digit date string,
/// comparing the zero-padded render byte-wise (allocation-free). DICOM dates
/// are always 8 digits; the canonical form matches the SQLite TEXT column.
fn date_matches(v: u64, want: &str) -> bool {
    // Start from ASCII `'0'` so the unused high-order digits read as leading
    // zeros ("00020315"), matching the canonical DICOM 8-digit date form.
    let mut buf = [b'0'; 8];
    let mut rem = v;
    let mut i = 8;
    while i > 0 {
        i -= 1;
        buf[i] = b'0' + (rem % 10) as u8;
        rem /= 10;
        if rem == 0 {
            break;
        }
    }
    want.len() == 8 && want.as_bytes() == &buf[..]
}

/// Allocation-free glob match over byte slices, honouring `*` (any run) and
/// `?` (exactly one CHARACTER). Two-pointer with backtracking, no heap.
///
/// The walk is over scalars, not bytes: `?` means one character, and matching
/// bytes made it swallow a single UTF-8 continuation byte, so a `?` pattern
/// either failed against Cyrillic text or split a letter. A literal `*` in the
/// text also needs its own comparison path - it is only a wildcard on the
/// PATTERN side, and without the guard the two sides were compared with the
/// wildcard still live.
///
/// Scalars are walked in place. Collecting the pattern and text into `Vec<char>`
/// first was the obvious way to do this and broke the ap101 zero-allocation
/// gate on the user-supplied `Modality`/`SOPClassUID` search path, so the two
/// sides advance with `char_indices`-style stepping over the borrowed slices.
/// Glob match over a stored text value, one scalar at a time.
///
/// Public so the worst-case bench can drive THIS walk rather than the
/// allocating `filter::glob_match`: the quadratic cost lived in the per-step
/// scalar read, and a benchmark that exercised a different matcher would
/// measure nothing.
pub fn glob_match_bytes(pat: &str, text: &str) -> bool {
    let p = pat.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None, None);

    while ti < t.len() {
        let p_ch = next_scalar(p, pi);
        // `p_ch != '*'` in the literal branch: a `*` can only ever be a
        // wildcard here, so it must not be compared as a character.
        if let Some((ch, plen)) = p_ch
            && ch != '*'
        {
            if ch == '?' {
                pi += plen;
                ti += next_scalar(t, ti).map(|(_, n)| n).unwrap_or(1);
                continue;
            }
            let t_ch = next_scalar(t, ti);
            if let Some((tch, tlen)) = t_ch
                && tch == ch
            {
                pi += plen;
                ti += tlen;
                continue;
            }
        }
        if p_ch.is_some_and(|(ch, _)| ch == '*') {
            star_pi = Some(pi);
            star_ti = Some(ti);
            pi += 1; // '*' is one byte
            continue;
        }
        if let (Some(sp), Some(st)) = (star_pi, star_ti) {
            // Let that `*` swallow one more CHARACTER and retry.
            pi = sp + 1;
            ti = st + next_scalar(t, st).map(|(_, n)| n).unwrap_or(1);
            if ti > t.len() {
                return false;
            }
            star_ti = Some(ti);
            continue;
        }
        return false;
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// The next scalar at `bytes[pos]` and its UTF-8 length, without allocating.///
/// Falls back to "one byte, as a scalar" for an invalid sequence: the caller
/// compares scalars, and a malformed value should compare literally rather than
/// vanish from the walk.
///
/// Only the scalar's OWN bytes are validated. This used to call
/// `str::from_utf8(bytes[pos..])`, i.e. it re-validated the whole remaining
/// value on every step of the wildcard walk - quadratic in the value length,
/// and the walk retries positions ("let that `*` swallow one more scalar"), so
/// a long Patient Name with an early `*` made the pattern check cost
/// O(len^2). The lead byte says how many bytes the scalar occupies, and that
/// window is all this function needs.
fn next_scalar(bytes: &[u8], pos: usize) -> Option<(char, usize)> {
    let rest = bytes.get(pos..)?;
    let first = *rest.first()?;
    // PS/UTF-8: a continuation byte cannot start a scalar, so it is malformed
    // at this position regardless of what follows.
    let width = match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 0,
    };
    if width > 0
        && let Some(window) = rest.get(..width)
        && let Ok(s) = std::str::from_utf8(window)
        && let Some(ch) = s.chars().next()
    {
        return Some((ch, width));
    }
    // Invalid UTF-8 at this position: compare the byte as Latin-1.
    Some((first as char, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        AttrValue, InstanceRecord, Manifest, SeriesRecord, StudyQido, SysHeader, TIER_HOT,
    };
    use crate::query::tag;

    /// ap101 gate: `TestAllocator` counters are thread-local, so sibling
    /// libtest threads cannot pollute the measured windows below.
    #[global_allocator]
    static ALLOC: ferrite_testkit::heap::TestAllocator =
        ferrite_testkit::heap::TestAllocator::new();

    /// [`series`] with an explicit Modality (0008,0060).
    ///
    /// The plain fixture carries no modality, so a test that asserted on a
    /// modality filter could not tell a working filter from an ignored key.
    fn series_with_modality(
        uid: &str,
        num: Option<&str>,
        desc: Option<&str>,
        modality: &str,
    ) -> SeriesRecord {
        let mut s = series(uid, num, desc, None);
        s.attrs
            .push((tag::MODALITY, AttrValue::Text(modality.into())));
        s.attrs.sort_by_key(|(t, _)| *t);
        s
    }

    fn series(
        uid: &str,
        num: Option<&str>,
        desc: Option<&str>,
        time: Option<&str>,
    ) -> SeriesRecord {
        let mut attrs = Vec::new();
        if let Some(n) = num {
            attrs.push((tag::SERIES_NUMBER, AttrValue::Text(n.into())));
        }
        attrs.push((tag::SERIES_INSTANCE_UID, AttrValue::Text(uid.into())));
        if let Some(d) = desc {
            attrs.push((tag::SERIES_DESCRIPTION, AttrValue::Text(d.into())));
        }
        if let Some(t) = time {
            attrs.push((tag::SERIES_TIME, AttrValue::Text(t.into())));
        }
        // Keep ascending order for realism.
        attrs.sort_by_key(|(t, _)| *t);
        SeriesRecord {
            attrs,
            instances: Vec::new(),
        }
    }

    fn instance(uid: &str, num: Option<&str>) -> InstanceRecord {
        instance_with_flag(uid, num, false)
    }

    /// The same fixture, with the AIRA `rejected` flag under control.
    fn instance_with_reason(sop: &str, number: u64, reason: u32) -> InstanceRecord {
        let mut rec = instance_with_flag(sop, Some(&number.to_string()), false);
        rec.rejection_reason = reason;
        rec
    }

    fn instance_with_flag(uid: &str, num: Option<&str>, rejected: bool) -> InstanceRecord {
        let mut attrs = vec![(tag::SOP_INSTANCE_UID, AttrValue::Text(uid.into()))];
        if let Some(n) = num {
            attrs.push((tag::INSTANCE_NUMBER, AttrValue::Text(n.into())));
        }
        attrs.sort_by_key(|(t, _)| *t);
        InstanceRecord {
            attrs,
            transfer_syntax: "1.2.840.10008.1.2.1".into(),
            storage_tier: TIER_HOT,
            rejection_reason: if rejected {
                crate::manifest::QUALITY_REASON_CODE
            } else {
                crate::manifest::NOT_REJECTED_REASON
            },
            blake3: [0u8; 32],
            file_size: 100,
            frames: Vec::new(),
        }
    }

    /// An instance carrying an explicit SOPClassUID plus an InstanceNumber in
    /// either stored form (UInt or Text) - the matching keys.
    fn instance_full(uid: &str, class_uid: &str, num: Option<AttrValue>) -> InstanceRecord {
        let mut attrs = vec![(tag::SOP_INSTANCE_UID, AttrValue::Text(uid.into()))];
        attrs.push((tag::SOP_CLASS_UID, AttrValue::Text(class_uid.into())));
        if let Some(n) = num {
            attrs.push((tag::INSTANCE_NUMBER, n));
        }
        attrs.sort_by_key(|(t, _)| *t);
        InstanceRecord {
            attrs,
            transfer_syntax: "1.2.840.10008.1.2.1".into(),
            storage_tier: TIER_HOT,
            rejection_reason: crate::manifest::NOT_REJECTED_REASON,
            blake3: [0u8; 32],
            file_size: 100,
            frames: Vec::new(),
        }
    }

    fn manifest_with(series: Vec<SeriesRecord>) -> Manifest {
        Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: Vec::new(),
                ..Default::default()
            },
            series,
        }
    }

    fn uids<'a>(out: &[&'a SeriesRecord]) -> Vec<&'a str> {
        out.iter()
            .map(|s| text_attr(&s.attrs, tag::SERIES_INSTANCE_UID).unwrap())
            .collect()
    }

    #[test]
    fn series_sort_numeric_secondary_uid_missing_last() {
        let m = manifest_with(vec![
            series("B", Some("10"), None, None),
            series("A", Some("2"), None, None),
            series("D", None, None, None), // missing number -> last
            series("C", Some("2"), None, None), // ties broken by UID
        ]);
        let mut out = Vec::new();
        search_series(&m, &SeriesFilter::default(), &mut out);
        assert_eq!(uids(&out), vec!["A", "C", "B", "D"]);
    }

    /// IS is a SIGNED integer string (PS3.5 Table 6.2-1), so a negative
    /// SeriesNumber sorts BELOW zero rather than into the missing group.
    ///
    /// A negative value is reachable: the dicom_rs parser reads (0020,0011)
    /// with `opt_int` into an `i32`, STOW stores it as text without a range
    /// check, and the manifest therefore carries `-1`. Parsing the sort key as
    /// `u64` failed that string and filed it with the records that have no
    /// number at all - so every real number sorted ahead of it, including 0,
    /// which is not the DICOM ordering.
    #[test]
    fn a_negative_number_sorts_below_zero_not_into_the_missing_group() {
        let m = manifest_with(vec![
            series("A", Some("2"), None, None),
            series("B", Some("-1"), None, None),
            series("C", Some("0"), None, None),
            series("D", None, None, None),
        ]);
        let mut out = Vec::new();
        search_series(&m, &SeriesFilter::default(), &mut out);
        assert_eq!(
            uids(&out),
            vec!["B", "C", "A", "D"],
            "-1 < 0 < 2, and only the truly absent number sorts last"
        );
    }

    /// The instance path has the same key and the same defect; it is a
    /// sibling of the series fix, not a separate finding.
    #[test]
    fn a_negative_instance_number_sorts_below_zero() {
        let mut s = series("S", None, None, None);
        s.instances = vec![
            instance("i-missing", None),
            instance("i-neg", Some("-3")),
            instance("i-zero", Some("0")),
            instance("i-2", Some("2")),
        ];
        let m = manifest_with(vec![s]);
        let mut out = Vec::new();
        let found = search_instances(&m, "S", &InstanceFilter::default(), &mut out, false);
        assert!(found);
        let got: Vec<&str> = out
            .iter()
            .map(|i| text_attr(&i.attrs, tag::SOP_INSTANCE_UID).unwrap())
            .collect();
        assert_eq!(got, vec!["i-neg", "i-zero", "i-2", "i-missing"]);
    }

    #[test]
    fn instance_sort_numeric_missing_last() {
        let mut s = series("S", None, None, None);
        s.instances = vec![
            instance("i-missing", None),
            instance("i-10", Some("10")),
            instance("i-2", Some("2")),
        ];
        let m = manifest_with(vec![s]);
        let mut out = Vec::new();
        let found = search_instances(&m, "S", &InstanceFilter::default(), &mut out, false);
        assert!(found);
        let got: Vec<&str> = out
            .iter()
            .map(|i| text_attr(&i.attrs, tag::SOP_INSTANCE_UID).unwrap())
            .collect();
        assert_eq!(got, vec!["i-2", "i-10", "i-missing"]);
    }

    #[test]
    fn series_filters_modality_and_description_glob() {
        let m = manifest_with(vec![
            series_with_modality("S1", Some("1"), Some("Chest CT"), "CT"),
            series_with_modality("S2", Some("2"), Some("Head"), "CT"),
            series_with_modality("S3", Some("3"), Some("Chest MR"), "MR"),
        ]);
        // Modality filter. The fixtures must DIFFER in modality, otherwise a
        // scanner that ignored the key would answer the same three series and
        // this assertion would pass either way - which is exactly what the
        // previous version of this test did (default filter, output discarded).
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                modality: Some("CT".into()),
                series_instance_uid: None,
                series_description: None,
                series_date: None,
                series_time: None,
                manufacturer: None,
                institutional_department_name: None,
                operators_name: None,
                protocol_name: None,
                station_name: None,
                body_part_examined: None,
                pps_start_date: None,
                pps_start_time: None,
                scheduled_procedure_step_id: None,
                requested_procedure_id: None,
                series_number: None,
                pn_case_insensitive: false,
            },
            &mut out,
        );
        assert_eq!(
            uids(&out),
            vec!["S1", "S2"],
            "only CT series may match a CT query"
        );
        // Description glob.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                series_description: Some("Chest*".into()),
                series_instance_uid: None,
                modality: None,
                series_date: None,
                series_time: None,
                manufacturer: None,
                institutional_department_name: None,
                operators_name: None,
                protocol_name: None,
                station_name: None,
                body_part_examined: None,
                pps_start_date: None,
                pps_start_time: None,
                scheduled_procedure_step_id: None,
                requested_procedure_id: None,
                series_number: None,
                pn_case_insensitive: false,
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["S1", "S3"]);
    }

    /// 08.09 vendor extensions (dcm4chee/Orthanc parity): StationName
    /// (0008,1010) and BodyPartExamined (0018,0015) are series matching keys
    /// with SeriesDescription semantics - glob `*`/`?`, case-sensitive,
    /// allocation-free, no fuzzy wiring.
    #[test]
    fn station_name_and_body_part_series_keys() {
        fn with_attrs(uid: &str, station: Option<&str>, body_part: Option<&str>) -> SeriesRecord {
            let mut s = series(uid, Some("1"), Some("Axial"), None);
            if let Some(v) = station {
                s.attrs.push((tag::STATION_NAME, AttrValue::Text(v.into())));
            }
            if let Some(v) = body_part {
                s.attrs
                    .push((tag::BODY_PART_EXAMINED, AttrValue::Text(v.into())));
            }
            s.attrs.sort_by_key(|(t, _)| *t);
            s
        }
        let m = manifest_with(vec![
            with_attrs("A", Some("CT_ROOM1"), Some("CHEST")),
            with_attrs("B", Some("MR_ROOM1"), Some("BRAIN")),
            with_attrs("C", None, None),
        ]);
        // StationName wildcard: CT_* -> A only.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                station_name: Some("CT_*".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A"]);
        // BodyPartExamined `?` single-char wildcard.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                body_part_examined: Some("BRAI?".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["B"]);
        // Case-sensitive literal match (mirror of SeriesDescription).
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                station_name: Some("ct_room1".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert!(out.is_empty());
        // Combined keys: both must match (AND across keys).
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                station_name: Some("CT_*".into()),
                body_part_examined: Some("CHEST".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A"]);
        // A series without the attribute never matches a set key.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                station_name: Some("*".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A", "B"]);
    }

    /// 08.09 Stage B series keys: Manufacturer / ProtocolName /
    /// InstitutionalDepartmentName are plain wildcards; OperatorsName is the
    /// PN key whose matching honors `pn_case_insensitive` (the
    /// `case_sensitive_pn` knob), with NO fuzzy wiring.
    #[test]
    fn stage_b_series_keys_match() {
        fn with_attrs(
            uid: &str,
            manufacturer: Option<&str>,
            department: Option<&str>,
            operators: Option<&str>,
            protocol: Option<&str>,
        ) -> SeriesRecord {
            let mut s = series(uid, Some("1"), Some("Axial"), None);
            if let Some(v) = manufacturer {
                s.attrs.push((tag::MANUFACTURER, AttrValue::Text(v.into())));
            }
            if let Some(v) = department {
                s.attrs.push((
                    tag::INSTITUTIONAL_DEPARTMENT_NAME,
                    AttrValue::Text(v.into()),
                ));
            }
            if let Some(v) = operators {
                s.attrs
                    .push((tag::OPERATORS_NAME, AttrValue::Text(v.into())));
            }
            if let Some(v) = protocol {
                s.attrs
                    .push((tag::PROTOCOL_NAME, AttrValue::Text(v.into())));
            }
            s.attrs.sort_by_key(|(t, _)| *t);
            s
        }
        let m = manifest_with(vec![
            with_attrs(
                "A",
                Some("ACME"),
                Some("RAD"),
                Some("OPS^ANNA"),
                Some("CHEST_RTN"),
            ),
            with_attrs(
                "B",
                Some("SIEMENS"),
                Some("NEURO"),
                Some("OPS^BOB"),
                Some("HEAD_RTN"),
            ),
            with_attrs("C", None, None, None, None),
        ]);

        // Manufacturer wildcard -> A only.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                manufacturer: Some("AC*".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A"]);
        // ProtocolName glob -> B only.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                protocol_name: Some("HEAD_*".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["B"]);
        // InstitutionalDepartmentName exact -> A only.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                institutional_department_name: Some("RAD".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A"]);
        // OperatorsName: standard case-sensitive glob (PN template default).
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                operators_name: Some("OPS^ANNA".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A"]);
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                operators_name: Some("ops^anna".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert!(out.is_empty(), "case-sensitive by default");
        // ...and case-insensitive with the knob (same deviation as the study
        // PN keys).
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                operators_name: Some("ops^anna".into()),
                pn_case_insensitive: true,
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A"]);
        // Missing attribute never matches a set key.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                manufacturer: Some("*".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A", "B"]);
    }

    // Table 10.6.1-5: PerformedProcedureStepStartDate (0040,0244) and
    // ...StartTime (0040,0245) are SERIES matching keys - DA/TM RANGE matching
    // (PS3.4 C.2.2.2.5), the shared predicates, not SeriesDate/SeriesTime's
    // exact/LIKE semantics.
    #[test]
    fn pps_series_matching_keys_filter_series() {
        // Two series carry PPS date/time; a third carries none.
        fn with_pps(uid: &str, pps_date: Option<&str>, pps_time: Option<&str>) -> SeriesRecord {
            let mut s = series(uid, Some("1"), Some("Chest"), None);
            if let Some(d) = pps_date {
                // series_attrs stores DA PPS date as a UInt (mirrors SERIES_DATE).
                let n = d.parse::<u64>().expect("pps date numeric");
                s.attrs.push((tag::PPS_START_DATE, AttrValue::UInt(n)));
            }
            if let Some(t) = pps_time {
                s.attrs
                    .push((tag::PPS_START_TIME, AttrValue::Text(t.into())));
            }
            s.attrs.sort_by_key(|(t, _)| *t);
            s
        }
        let m = manifest_with(vec![
            with_pps("A", Some("20260802"), Some("104500")),
            with_pps("B", Some("20260802"), Some("113000")),
            with_pps("C", None, None),
        ]);
        // Exact date -> A and B.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                pps_start_date: Some("20260802".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A", "B"]);
        // Different date -> nothing (exact form is a single-day range).
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                pps_start_date: Some("20260803".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert!(out.is_empty());
        // Time range `104000-110000` -> A only (B is 113000).
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                pps_start_time: Some("104000-110000".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["A"]);
        // A filter that asks for a stored PPS on the C-series returns empty.
        let mut out = Vec::new();
        search_series(
            &m,
            &SeriesFilter {
                pps_start_date: Some("20260802".into()),
                pps_start_time: Some("110000-120000".into()),
                ..Default::default()
            },
            &mut out,
        );
        assert_eq!(uids(&out), vec!["B"]);
    }

    /// PS3.4 C.2.2.2.5: pps_start_date (DA) and pps_start_time (TM) are
    /// required series matching keys and must match the RANGE form inclusively
    /// with open ends - not the exact/LIKE comparison SeriesDate/SeriesTime keep.
    #[test]
    fn pps_start_date_and_time_range_matching() {
        fn with_pps(uid: &str, pps_date: Option<u64>, pps_time: Option<&str>) -> SeriesRecord {
            let mut s = series(uid, Some("1"), Some("Chest"), None);
            if let Some(d) = pps_date {
                s.attrs.push((tag::PPS_START_DATE, AttrValue::UInt(d)));
            }
            if let Some(t) = pps_time {
                s.attrs
                    .push((tag::PPS_START_TIME, AttrValue::Text(t.into())));
            }
            s.attrs.sort_by_key(|(t, _)| *t);
            s
        }
        let m = manifest_with(vec![
            with_pps("A", Some(20260815), Some("100000")),
            with_pps("B", Some(20260820), Some("110000")),
            with_pps("C", Some(20260901), Some("090000")),
            with_pps("D", None, None),
        ]);
        let search = |f: SeriesFilter| {
            let mut out = Vec::new();
            search_series(&m, &f, &mut out);
            uids(&out)
        };
        // DA closed range, inclusive: 20260815..20260820 in range.
        assert_eq!(
            search(SeriesFilter {
                pps_start_date: Some("20260801-20260831".into()),
                ..Default::default()
            }),
            vec!["A", "B"]
        );
        // DA open upper bound -> A and B.
        assert_eq!(
            search(SeriesFilter {
                pps_start_date: Some("20260815-".into()),
                ..Default::default()
            }),
            vec!["A", "B", "C"]
        );
        // DA open lower bound -> A only (B is after the upper end).
        assert_eq!(
            search(SeriesFilter {
                pps_start_date: Some("-20260815".into()),
                ..Default::default()
            }),
            vec!["A"]
        );
        // DA exact form still works.
        assert_eq!(
            search(SeriesFilter {
                pps_start_date: Some("20260820".into()),
                ..Default::default()
            }),
            vec!["B"]
        );
        // TM closed range, inclusive.
        assert_eq!(
            search(SeriesFilter {
                pps_start_time: Some("090000-110000".into()),
                ..Default::default()
            }),
            vec!["A", "B", "C"]
        );
        // TM open upper bound.
        assert_eq!(
            search(SeriesFilter {
                pps_start_time: Some("100000-".into()),
                ..Default::default()
            }),
            vec!["A", "B"]
        );
        // TM open lower bound.
        assert_eq!(
            search(SeriesFilter {
                pps_start_time: Some("-100000".into()),
                ..Default::default()
            }),
            vec!["A", "C"]
        );
        // TM exact form.
        assert_eq!(
            search(SeriesFilter {
                pps_start_time: Some("110000".into()),
                ..Default::default()
            }),
            vec!["B"]
        );
        // SeriesDate/SeriesTime are unchanged: exact DA / LIKE TM semantics.
        assert_eq!(
            search(SeriesFilter {
                series_date: Some("20260815-20260820".into()),
                ..Default::default()
            }),
            Vec::<&str>::new(),
            "series_date is NOT a range key - a hyphen literal matches nothing"
        );
    }

    #[test]
    fn instance_sop_uid_exact_filter() {
        let mut s = series("S", None, None, None);
        s.instances = vec![instance("A", None), instance("B", None)];
        let m = manifest_with(vec![s]);
        let mut out = Vec::new();
        let found = search_instances(
            &m,
            "S",
            &InstanceFilter {
                sop_instance_uid: Some("B".into()),
                ..Default::default()
            },
            &mut out,
            false,
        );
        assert!(found);
        assert_eq!(out.len(), 1);
        assert_eq!(text_attr(&out[0].attrs, tag::SOP_INSTANCE_UID), Some("B"));
    }

    // Table 10.6.1-5: SOPClassUID is a matching key.
    #[test]
    fn search_instances_sop_class_uid_filter() {
        let mut s = series("S", None, None, None);
        s.instances = vec![
            instance_full("A", "1.2.840.10008.5.1.4.1.1.2", None),
            instance_full("B", "1.2.840.10008.5.1.4.1.1.4", None),
        ];
        let m = manifest_with(vec![s]);
        let mut out = Vec::new();
        let found = search_instances(
            &m,
            "S",
            &InstanceFilter {
                sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.4".into()),
                ..Default::default()
            },
            &mut out,
            false,
        );
        assert!(found);
        assert_eq!(out.len(), 1);
        assert_eq!(text_attr(&out[0].attrs, tag::SOP_INSTANCE_UID), Some("B"));
    }

    // PS3.4 C.2.2.2.2: SOPClassUID supports List of UID Matching - each UID in
    // the list may generate a match, so a comma list must not be compared as
    // one string.
    #[test]
    fn search_instances_sop_class_uid_list_filter() {
        let mut s = series("S", None, None, None);
        s.instances = vec![instance_full("A", "1.2.840.10008.5.1.4.1.1.2", None)];
        let m = manifest_with(vec![s]);
        let mut out = Vec::new();
        let found = search_instances(
            &m,
            "S",
            &InstanceFilter {
                sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2,1.2.840.10008.5.1.4.1.1.4".into()),
                ..Default::default()
            },
            &mut out,
            false,
        );
        assert!(found);
        assert_eq!(out.len(), 1, "a CT instance matches the two-member list");
        assert_eq!(text_attr(&out[0].attrs, tag::SOP_INSTANCE_UID), Some("A"));
    }

    // Table 10.6.1-5: InstanceNumber is a matching key in BOTH stored
    // forms (UInt and Text) with the SeriesNumber Glob semantics (`*`/`?`).
    #[test]
    fn search_instances_instance_number_filter_both_forms_and_wildcard() {
        let mut s = series("S", None, None, None);
        s.instances = vec![
            instance_full("i-5", "C", Some(AttrValue::UInt(5))),
            instance_full("i-50", "C", Some(AttrValue::Text("50".into()))),
            instance_full("i-6", "C", Some(AttrValue::Text("6".into()))),
        ];
        let m = manifest_with(vec![s]);
        // Exact match against the UInt form.
        let mut out = Vec::new();
        search_instances(
            &m,
            "S",
            &InstanceFilter {
                instance_number: Some("5".into()),
                ..Default::default()
            },
            &mut out,
            false,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(text_attr(&out[0].attrs, tag::SOP_INSTANCE_UID), Some("i-5"));
        // Exact match against the Text form.
        let mut out = Vec::new();
        search_instances(
            &m,
            "S",
            &InstanceFilter {
                instance_number: Some("50".into()),
                ..Default::default()
            },
            &mut out,
            false,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            text_attr(&out[0].attrs, tag::SOP_INSTANCE_UID),
            Some("i-50")
        );
        // Prefix wildcard over the decimal render: both 5 and 50 match.
        let mut out = Vec::new();
        search_instances(
            &m,
            "S",
            &InstanceFilter {
                instance_number: Some("5*".into()),
                ..Default::default()
            },
            &mut out,
            false,
        );
        assert_eq!(out.len(), 2);
    }

    // Combined filters narrow down to exactly one instance.
    #[test]
    fn search_instances_combined_filters_narrow_to_one() {
        let mut s = series("S", None, None, None);
        s.instances = vec![
            instance_full("A", "CT-CLASS", Some(AttrValue::UInt(1))),
            instance_full("B", "CT-CLASS", Some(AttrValue::UInt(2))),
            instance_full("C", "MR-CLASS", Some(AttrValue::UInt(2))),
        ];
        let m = manifest_with(vec![s]);
        let mut out = Vec::new();
        let found = search_instances(
            &m,
            "S",
            &InstanceFilter {
                sop_class_uid: Some("CT-CLASS".into()),
                instance_number: Some("2".into()),
                sop_instance_uid: None,
            },
            &mut out,
            false,
        );
        assert!(found);
        assert_eq!(out.len(), 1);
        assert_eq!(text_attr(&out[0].attrs, tag::SOP_INSTANCE_UID), Some("B"));
    }

    // A filter with no matches yields an empty `out` while the series itself
    // exists (the `true` return stays: it reports series presence, not hits).
    #[test]
    fn search_instances_no_match_empty_but_series_present() {
        let mut s = series("S", None, None, None);
        s.instances = vec![instance_full("A", "CT-CLASS", Some(AttrValue::UInt(1)))];
        let m = manifest_with(vec![s]);
        let mut out = Vec::new();
        let found = search_instances(
            &m,
            "S",
            &InstanceFilter {
                sop_class_uid: Some("NOPE".into()),
                ..Default::default()
            },
            &mut out,
            false,
        );
        assert!(found);
        assert!(out.is_empty());
    }

    /// ap101 gate: `search_instances` with a COMPOSITE filter (all three
    /// keys set) over a populated series is steady-state allocation-free.
    /// The `out` buffer is warmed OUTSIDE the window (its first call may grow
    /// it); inside the window only borrowed-reference
    /// pushes, string comparisons and the sort run.
    #[test]
    fn search_instances_composite_filter_is_allocation_free() {
        let mut s = series("S", None, None, None);
        s.instances = vec![
            instance_full("A", "1.2.840.10008.5.1.4.1.1.2", Some(AttrValue::UInt(1))),
            instance_full(
                "B",
                "1.2.840.10008.5.1.4.1.1.2",
                Some(AttrValue::Text("2".into())),
            ),
            instance_full("C", "1.2.840.10008.5.1.4.1.1.4", Some(AttrValue::UInt(3))),
        ];
        let m = manifest_with(vec![s]);
        let filter = InstanceFilter {
            sop_instance_uid: Some("B".into()),
            sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".into()),
            instance_number: Some("2".into()),
        };
        let mut out: Vec<&InstanceRecord> = Vec::new();
        let _ = search_instances(&m, "S", &filter, &mut out, false); // warm the caller buffer

        ALLOC.set_enabled(false);
        ALLOC.reset();
        ALLOC.set_enabled(true);
        let before = ALLOC.snapshot();
        let found = search_instances(&m, "S", &filter, &mut out, false);
        let after = ALLOC.snapshot();
        ALLOC.set_enabled(false);
        assert!(
            before == after,
            "search_instances window allocated: {before:?} -> {after:?}"
        );
        assert!(found);
        assert_eq!(out.len(), 1);
        assert_eq!(text_attr(&out[0].attrs, tag::SOP_INSTANCE_UID), Some("B"));
    }

    #[test]
    fn instance_search_missing_series_returns_false() {
        let m = manifest_with(vec![series("S", None, None, None)]);
        let mut out = Vec::new();
        let found = search_instances(&m, "NOPE", &InstanceFilter::default(), &mut out, false);
        assert!(!found);
    }

    #[test]
    fn glob_matches_wildcards() {
        assert!(glob_match_bytes("Chest*", "Chest CT"));
        assert!(glob_match_bytes("*CT", "Chest CT"));
        assert!(glob_match_bytes("Chest?", "ChestC"));
        assert!(glob_match_bytes("*", "anything"));
        assert!(!glob_match_bytes("Chest", "Chest CT"));
        assert!(!glob_match_bytes("Chest*", "Head"));
        assert!(glob_match_bytes("", ""));
        assert!(glob_match_bytes("", ""));
    }

    /// `next_scalar` decodes ONE scalar from its own bytes.
    ///
    /// The step must not depend on the rest of the value: the wildcard walk
    /// re-reads the text from a `*` once per extra scalar it swallows, so a
    /// step that validated the whole remaining slice turned the pattern check
    /// into O(len^2) on a long Patient Name with an early wildcard. The
    /// assertion is on the step itself rather than on elapsed time, because a
    /// timing bound is machine-dependent and would pass a slow-but-quadratic
    /// implementation on a fast host.
    #[test]
    fn a_scalar_step_reads_only_its_own_bytes() {
        // A value whose tail is INVALID UTF-8 must still yield the valid lead
        // scalar: reading only the scalar's window is what makes that
        // possible, and it is also what keeps the step O(1).
        let mut bytes = b"A".to_vec();
        bytes.push(0xFF);
        bytes.extend(std::iter::repeat_n(b'\xD0', 4096));
        assert_eq!(next_scalar(&bytes, 0), Some(('A', 1)));

        // And a well-formed scalar in the middle of a long value is decoded
        // without regard to what precedes or follows it.
        let text = format!("{}Аx", "A".repeat(4096));
        assert_eq!(
            next_scalar(text.as_bytes(), 4096),
            Some(('А', 2)),
            "the Cyrillic scalar is decoded from its own two bytes"
        );
    }

    /// Multi-byte scalars are still decoded one at a time.
    #[test]
    fn a_multibyte_scalar_is_one_step_of_the_walk() {
        assert_eq!(next_scalar("АБ".as_bytes(), 0), Some(('А', 2)));
        assert_eq!(next_scalar("АБ".as_bytes(), 2), Some(('Б', 2)));
        // A lone continuation byte is malformed where it sits.
        assert_eq!(next_scalar(&[0x91, 0x92], 0), Some(('\u{91}', 1)));
        // A truncated sequence falls back to the lead byte.
        assert_eq!(next_scalar(&[0xD0], 0), Some(('\u{D0}', 1)));
    }

    /// A scalar step must not look at what follows the scalar.
    ///
    /// The decoded scalar and its width are the whole observable contract, and
    /// neither can depend on the tail. With the old `str::from_utf8(bytes[pos..])`
    /// probe this test still PASSES - both calls return the same value - but it
    /// takes milliseconds instead of microseconds, because every step validated
    /// the entire remaining value. Its value is that it makes the defect cheap
    /// to see: one megabyte of tail is either scanned on each step or not
    /// touched at all. The durable cost proof is the criterion bench
    /// (`benches/glob_worst_case_bench.rs`), not a wall-clock assertion here.
    #[test]
    fn a_scalar_step_does_not_scan_the_tail() {
        let short = b"A";
        let mut long = b"A".to_vec();
        long.extend(std::iter::repeat_n(b'\xD0', 1_000_000));
        assert_eq!(next_scalar(short, 0), next_scalar(&long, 0));
        assert_eq!(next_scalar(&long, 0), Some(('A', 1)));
    }

    #[test]
    fn non_numeric_number_sorts_last() {
        let m = manifest_with(vec![
            series("A", Some("abc"), None, None), // non-numeric -> last group
            series("B", Some("1"), None, None),
        ]);
        let mut out = Vec::new();
        search_series(&m, &SeriesFilter::default(), &mut out);
        assert_eq!(uids(&out), vec!["B", "A"]);
    }

    /// parity: a canonical 8-digit series date (stored as UInt) matches
    /// the SQLite TEXT `=` comparison string-wise.
    #[test]
    fn series_date_parity_with_sqlite_string_compare() {
        let attrs = vec![(tag::SERIES_DATE, AttrValue::UInt(20230515))];
        assert!(date_attr_equals(&attrs, tag::SERIES_DATE, "20230515"));
        assert!(!date_attr_equals(&attrs, tag::SERIES_DATE, "20230516"));
        // Leading-zero canonical render: 20315 stores as 8-digit 00020315.
        let leading = vec![(tag::SERIES_DATE, AttrValue::UInt(20315))];
        assert!(date_attr_equals(&leading, tag::SERIES_DATE, "00020315"));
        assert!(!date_attr_equals(&leading, tag::SERIES_DATE, "20315"));
    }

    /// A sequestered instance is invisible at series-scoped instance QIDO
    /// unless the caller asks for the expose view.
    #[test]
    fn search_instances_hides_rejected_by_default() {
        let mut s = series("1.2.840.1.1", Some("CT"), Some("Chest"), None);
        s.instances = vec![
            instance_with_flag("1.2.840.1.1.1", Some("1"), false),
            instance_with_flag("1.2.840.1.1.2", Some("2"), true),
        ];
        let m = manifest_with(vec![s]);

        let mut out = Vec::new();
        assert!(search_instances(
            &m,
            "1.2.840.1.1",
            &InstanceFilter::default(),
            &mut out,
            false
        ));
        assert_eq!(out.len(), 1, "only the visible instance");
        assert_eq!(
            text_attr(&out[0].attrs, tag::SOP_INSTANCE_UID),
            Some("1.2.840.1.1.1")
        );

        // The QA surface sees a Quality-rejected instance too: that is the
        // reason the surface exists.
        let mut all = Vec::new();
        assert!(search_instances(
            &m,
            "1.2.840.1.1",
            &InstanceFilter::default(),
            &mut all,
            true
        ));
        assert_eq!(all.len(), 2, "the QA surface lifts a Quality hide");
    }

    /// The expose flag does NOT mean "show every sequestered instance": it
    /// lifts the reasons that permit review and nothing else. This is the
    /// core-level counterpart of the served-surface test - if `hides` ever
    /// stops consulting the record, both reason classes become visible here.
    #[test]
    fn expose_lifts_quality_but_not_an_any_class_hide() {
        use crate::manifest::{NOT_REJECTED_REASON, QUALITY_REASON_CODE};

        let mut s = series("1.2.840.1.1", Some("CT"), Some("Chest"), None);
        s.instances = vec![
            instance_with_reason("1.2.840.1.1.1", 1, NOT_REJECTED_REASON),
            instance_with_reason("1.2.840.1.1.2", 2, QUALITY_REASON_CODE),
            instance_with_reason("1.2.840.1.1.3", 3, 113_037),
            instance_with_reason("1.2.840.1.1.4", 4, 113_038),
            instance_with_reason("1.2.840.1.1.5", 5, 113_039),
        ];
        let m = manifest_with(vec![s]);

        // Hidden by default: every sequestered instance, regardless of reason.
        let mut hidden = Vec::new();
        search_instances(
            &m,
            "1.2.840.1.1",
            &InstanceFilter::default(),
            &mut hidden,
            false,
        );
        assert_eq!(hidden.len(), 1, "only the visible instance");

        // The QA surface adds back exactly one: the Quality rejection.
        let mut exposed = Vec::new();
        search_instances(
            &m,
            "1.2.840.1.1",
            &InstanceFilter::default(),
            &mut exposed,
            true,
        );
        assert_eq!(exposed.len(), 2, "expose lifts Quality only");
        let uids: Vec<Option<&str>> = exposed
            .iter()
            .map(|i| text_attr(&i.attrs, tag::SOP_INSTANCE_UID))
            .collect();
        assert!(uids.contains(&Some("1.2.840.1.1.1")), "{uids:?}");
        assert!(uids.contains(&Some("1.2.840.1.1.2")), "{uids:?}");
        for forbidden in ["1.2.840.1.1.3", "1.2.840.1.1.4", "1.2.840.1.1.5"] {
            assert!(
                !uids.contains(&Some(forbidden)),
                "{forbidden} must stay hidden on every surface: {uids:?}"
            );
        }
    }

    /// The same rule holds on the constrained branch (a filter is present),
    /// which is a separate code path from the unfiltered one.
    #[test]
    fn search_instances_hides_rejected_with_filter_too() {
        let mut s = series("1.2.840.1.1", Some("CT"), Some("Chest"), None);
        s.instances = vec![
            instance_with_flag("1.2.840.1.1.1", Some("1"), false),
            instance_with_flag("1.2.840.1.1.2", Some("2"), true),
        ];
        let m = manifest_with(vec![s]);
        let filter = InstanceFilter {
            instance_number: Some("2".into()),
            ..Default::default()
        };

        let mut out = Vec::new();
        search_instances(&m, "1.2.840.1.1", &filter, &mut out, false);
        assert!(out.is_empty(), "the matching instance is sequestered");

        // The QA surface lifts a Quality hide on the constrained branch too;
        // the filter still applies.
        let mut all = Vec::new();
        search_instances(&m, "1.2.840.1.1", &filter, &mut all, true);
        assert_eq!(all.len(), 1, "expose returns it despite the filter");
    }

    /// P1: a Study IE key on the GLOBAL /instances route narrows the scan.
    ///
    /// PS3.18 10.6.1.2.1 makes those keys legal here. Accepting one without
    /// applying it answered `?PatientID=P1` with every patient's instances -
    /// strictly wider than the ask, and the client could not tell its filter
    /// never ran. The study gate runs on `StudyQido.attrs` through the same
    /// `QueryFilter` the study route uses, so both routes agree on what the key
    /// means.
    #[test]
    fn a_study_key_narrows_the_global_instance_scan() {
        let mk = |study_uid: &str, patient: &str| Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![
                    (tag::STUDY_INSTANCE_UID, AttrValue::Text(study_uid.into())),
                    (tag::PATIENT_ID, AttrValue::Text(patient.into())),
                ],
                ..Default::default()
            },
            series: vec![series(study_uid, Some("1"), None, None)],
        };
        let mut a = mk("1.2.840.9.1", "P1");
        a.series[0].instances = vec![instance_with_flag("1.2.840.9.1.1", Some("1"), false)];
        let mut b = mk("1.2.840.9.2", "P2");
        b.series[0].instances = vec![instance_with_flag("1.2.840.9.2.1", Some("1"), false)];
        let manifests = [a, b];

        // No study filter: both patients' instances come back.
        let mut all = Vec::new();
        search_all_instances(
            &manifests,
            &InstanceFilter::default(),
            &SeriesFilter::default(),
            None,
            &mut all,
            false,
        );
        assert_eq!(all.len(), 2, "an unfiltered global scan sees everything");

        // PatientID narrows it to the one patient.
        let filter = crate::query::filter::QueryFilter {
            patient_id: Some("P1".to_string()),
            ..Default::default()
        };
        let mut narrowed = Vec::new();
        search_all_instances(
            &manifests,
            &InstanceFilter::default(),
            &SeriesFilter::default(),
            Some(&filter),
            &mut narrowed,
            false,
        );
        assert_eq!(
            narrowed.len(),
            1,
            "?PatientID=P1 must not answer with P2's instances"
        );
        assert_eq!(
            text_attr(&narrowed[0].instance.attrs, tag::SOP_INSTANCE_UID),
            Some("1.2.840.9.1.1")
        );
    }

    /// ModalitiesInStudy narrows the GLOBAL instance scan.
    ///
    /// The global `/instances` route accepts Study IE keys (PS3.18 10.6.1.2.1).
    /// ModalitiesInStudy (0008,0061) is matched by its own set comparison
    /// against a dedicated field, not through the expected-attribute map, and
    /// the predicate used to be skipped entirely when that map was empty - so
    /// `?ModalitiesInStudy=CT` answered with every study's instances.
    #[test]
    fn modalities_in_study_narrows_the_global_instance_scan() {
        let mk = |study_uid: &str, modalities: &str| Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                // Deliberately NO (0008,0061) inside `attrs`: the value lives
                // in the dedicated field, which is what the bug ignored.
                attrs: vec![(tag::STUDY_INSTANCE_UID, AttrValue::Text(study_uid.into()))],
                modalities_in_study: Some(modalities.to_string()),
                ..Default::default()
            },
            series: vec![series(study_uid, Some("1"), None, None)],
        };
        let mut ct = mk("1.2.840.8.1", "CT");
        ct.series[0].instances = vec![instance_with_flag("1.2.840.8.1.1", Some("1"), false)];
        let mut mr = mk("1.2.840.8.2", "MR");
        mr.series[0].instances = vec![instance_with_flag("1.2.840.8.2.1", Some("1"), false)];
        let manifests = [ct, mr];

        let filter = crate::query::filter::QueryFilter {
            modalities_in_study: Some("CT".to_string()),
            ..Default::default()
        };
        let mut out = Vec::new();
        search_all_instances(
            &manifests,
            &InstanceFilter::default(),
            &SeriesFilter::default(),
            Some(&filter),
            &mut out,
            false,
        );
        assert_eq!(
            out.len(),
            1,
            "?ModalitiesInStudy=CT must not answer with the MR study's instances"
        );
        assert_eq!(
            text_attr(&out[0].instance.attrs, tag::SOP_INSTANCE_UID),
            Some("1.2.840.8.1.1")
        );
    }

    /// Global instance QIDO hides sequestered instances, and the study/series
    /// levels are NOT filtered (a rejected AI result must not hide its study).
    #[test]
    fn search_all_instances_hides_rejected_but_study_stays_visible() {
        let mut s = series("1.2.840.1.1", Some("CT"), Some("Chest"), None);
        s.instances = vec![
            instance_with_flag("1.2.840.1.1.1", Some("1"), false),
            instance_with_flag("1.2.840.1.1.2", Some("2"), true),
        ];
        let m = manifest_with(vec![s]);
        let manifests = [m.clone()];

        let mut out = Vec::new();
        search_all_instances(
            &manifests,
            &InstanceFilter::default(),
            &SeriesFilter::default(),
            None,
            &mut out,
            false,
        );
        assert_eq!(out.len(), 1, "global instance QIDO hides the rejected one");

        let mut all = Vec::new();
        search_all_instances(
            &manifests,
            &InstanceFilter::default(),
            &SeriesFilter::default(),
            None,
            &mut all,
            true,
        );
        assert_eq!(all.len(), 2, "expose shows both");

        // Study/series level: unaffected by the flag.
        let mut sout = Vec::new();
        search_series(&m, &SeriesFilter::default(), &mut sout);
        assert_eq!(sout.len(), 1, "the series itself stays visible");
    }

    /// H4: a UID list matches every member, at both collectors.
    ///
    /// A UID key is single-valued in Table 10.6.1-5, but its value may be a
    /// comma-separated list. Comparing the whole string against one stored UID
    /// made `sopInstanceUid=1.2.3,1.2.4` match nothing.
    #[test]
    fn a_uid_list_matches_every_member() {
        let inst_of = |uid: &str| InstanceRecord {
            attrs: vec![(tag::SOP_INSTANCE_UID, AttrValue::Text(uid.into()))],
            transfer_syntax: String::new(),
            storage_tier: TIER_HOT,
            rejection_reason: crate::manifest::NOT_REJECTED_REASON,
            blake3: [0u8; 32],
            file_size: 0,
            frames: Vec::new(),
        };
        let s = SeriesRecord {
            attrs: vec![(tag::SERIES_INSTANCE_UID, AttrValue::Text("1.2.3".into()))],
            instances: vec![inst_of("1.2.3.1"), inst_of("1.2.3.2"), inst_of("1.2.3.3")],
        };

        // Instance collector: both named instances match, the third does not.
        let inst = InstanceFilter {
            sop_instance_uid: Some("1.2.3.1,1.2.3.2".into()),
            ..Default::default()
        };
        let matched: Vec<_> = s
            .instances
            .iter()
            .filter(|i| inst.matches(&i.attrs))
            .collect();
        assert_eq!(matched.len(), 2, "both list members must match");

        // Series collector: membership on the series UID.
        let by_list = SeriesFilter {
            series_instance_uid: Some("9.9.9,1.2.3".into()),
            ..Default::default()
        };
        let by_other = SeriesFilter {
            series_instance_uid: Some("9.9.9,8.8.8".into()),
            ..Default::default()
        };
        assert!(by_list.matches(&s.attrs), "the stored UID is in the list");
        assert!(
            !by_other.matches(&s.attrs),
            "a list without the stored UID must not match"
        );
    }

    /// H6: `SeriesNumber` filters, it does not only sort.
    ///
    /// PS3.18 Table 10.6.1-5 lists SeriesNumber as a Series-level matching key.
    /// Without it `SeriesNumber=2` returned every series in the study.
    #[test]
    fn a_series_number_filters_the_series() {
        let m = manifest_with(vec![
            series("1.2.840.1.2.1", Some("1"), Some("First"), None),
            series("1.2.840.1.2.2", Some("2"), Some("Second"), None),
        ]);
        let filter = SeriesFilter {
            series_number: Some("2".into()),
            ..Default::default()
        };
        let mut out = Vec::new();
        search_series(&m, &filter, &mut out);
        assert_eq!(uids(&out), vec!["1.2.840.1.2.2"], "only series 2 matches");

        // An unmatched number yields nothing rather than everything.
        let none = SeriesFilter {
            series_number: Some("7".into()),
            ..Default::default()
        };
        search_series(&m, &none, &mut out);
        assert!(out.is_empty(), "no series carries number 7");
    }

    /// The global series scan threads each manifest's study UID into its rows
    /// so the renderer can inject (0020,000D) without a clone. Distinct
    /// studies must carry distinct borrowed UIDs across the merged result.
    #[test]
    fn search_all_series_threads_study_uid() {
        let mk = |study_uid: &str, series_uid: &str| Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![(tag::STUDY_INSTANCE_UID, AttrValue::Text(study_uid.into()))],
                ..Default::default()
            },
            series: vec![series(series_uid, Some("1"), None, None)],
        };
        let a = mk("1.2.840.7.1", "1.2.840.7.1.1");
        let b = mk("1.2.840.7.2", "1.2.840.7.2.1");
        // Unsorted input: the scan must sort, moving each study UID with its
        // series. 7.2.x is fed first; 7.1.x must win the global sort.
        let manifests = [b, a];

        let mut out: Vec<GlobalSeries<'_>> = Vec::new();
        search_all_series(&manifests, &SeriesFilter::default(), &mut out);
        assert_eq!(out.len(), 2, "one series per manifest");
        // Globally sorted by SeriesInstanceUID ASC: 7.1.1 then 7.2.1.
        assert_eq!(out[0].study_uid, "1.2.840.7.1");
        assert_eq!(out[1].study_uid, "1.2.840.7.2");
        assert_eq!(
            text_attr(&out[0].series.attrs, tag::SERIES_INSTANCE_UID),
            Some("1.2.840.7.1.1")
        );
        assert_eq!(
            text_attr(&out[1].series.attrs, tag::SERIES_INSTANCE_UID),
            Some("1.2.840.7.2.1")
        );
    }
}
