// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOMweb JSON emission for a study manifest - the C-009 streaming core.
//!
//! The QIDO response body is a JSON array of study objects. The SQLite path
//! built that with `serde_json::Value`; the cut-over builds it by streaming
//! each study manifest's integer `(u32 tag, value)` attributes straight into a
//! caller-owned byte buffer. No `serde_json::Value` is allocated on the study
//! hotpath; the tuple characters `{},[],",:` are appended directly.
//!
//! Tags are translated from their `u32` integer form to the `GGGGEEEE` hex
//! form DICOMweb requires (`{ "00100010": { "vr": "PN", "Value": [...] } }`).
//! The `Value` rendering keeps numeric values unquoted and text quoted -
//! matching what a QIDO consumer expects - so the streamed bytes equal the
//! SQLite-baseline JSON for the same manifest.

use crate::manifest::{AttrValue, InstanceRecord, SeriesRecord, StudyQido};
use crate::query::tag;

/// The DICOM VR string assigned to an attribute based on its tag.
///
/// The manifest stores only the subset-relevant subset of DICOM tags; this
/// mapping covers the study-level search keys plus the series/instance tags
/// that are captured. Unknown tags fall back to `UN`.
pub fn vr_for_tag(tag: u32) -> &'static str {
    match tag {
        0x0010_0010 => "PN", // PatientName
        0x0010_0020 => "LO", // PatientID
        0x0010_0030 => "DA", // PatientBirthDate
        0x0010_0040 => "CS", // PatientSex
        0x0008_0016 => "UI", // SOPClassUID
        0x0008_0018 => "UI", // SOPInstanceUID
        0x0008_0020 => "DA", // StudyDate
        0x0008_0021 => "DA", // SeriesDate
        0x0008_0030 => "TM", // StudyTime
        0x0008_0031 => "TM", // SeriesTime
        0x0008_0050 => "SH", // AccessionNumber
        0x0008_0060 => "CS", // Modality
        0x0008_0061 => "CS", // ModalitiesInStudy
        0x0008_0070 => "LO", // Manufacturer
        0x0008_0080 => "LO", // InstitutionName
        0x0008_0090 => "PN", // ReferringPhysicianName
        0x0008_1010 => "SH", // StationName
        0x0008_1030 => "LO", // StudyDescription
        0x0008_103E => "LO", // SeriesDescription
        0x0008_1040 => "LO", // InstitutionalDepartmentName
        0x0008_1050 => "PN", // PerformingPhysicianName
        0x0008_1060 => "PN", // NameOfPhysiciansReadingStudy
        0x0008_1070 => "PN", // OperatorsName
        0x0018_0015 => "CS", // BodyPartExamined
        0x0018_1030 => "LO", // ProtocolName
        0x0020_000D => "UI", // StudyInstanceUID
        0x0020_000E => "UI", // SeriesInstanceUID
        0x0020_0010 => "SH", // StudyID
        0x0020_0011 => "IS", // SeriesNumber
        0x0020_0013 => "IS", // InstanceNumber
        0x0020_1206 => "IS", // NumberOfStudyRelatedSeries
        0x0020_1208 => "IS", // NumberOfStudyRelatedInstances
        0x0020_0060 => "CS", // Laterality
        0x0032_1032 => "PN", // RequestingPhysician
        0x0032_1060 => "LO", // RequestedProcedureDescription
        _ => "UN",
    }
}

/// A streaming byte sink that only writes into a fixed caller-owned buffer.
///
/// Because the sink owns no memory, the writer is allocation-free **by
/// construction** - this is the ap101 design constraint, not a post-hoc test.
struct Sink<'a> {
    buf: &'a mut [u8],
    /// Bytes written so far (always `<= buf.len()`).
    len: usize,
}

/// Result of a streaming write into an insufficiently-sized buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamOverflow;

impl<'a> Sink<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Sink { buf, len: 0 }
    }

    /// Append one byte. Returns `Err(Overflow)` if the buffer is full.
    fn push(&mut self, b: u8) -> Result<(), StreamOverflow> {
        if self.len >= self.buf.len() {
            return Err(StreamOverflow);
        }
        self.buf[self.len] = b;
        self.len += 1;
        Ok(())
    }

    /// Append a byte slice. Returns `Err(Overflow)` if it does not fit.
    fn extend(&mut self, s: &[u8]) -> Result<(), StreamOverflow> {
        if self.len + s.len() > self.buf.len() {
            return Err(StreamOverflow);
        }
        self.buf[self.len..self.len + s.len()].copy_from_slice(s);
        self.len += s.len();
        Ok(())
    }
}

/// PS3.18 mandatory response baseline per level (Table 10.6.1-2/3/4):
/// `includefield` ADDS requested tags, it never removes the mandatory ones.
/// 0109 field: OHIF asks `includefield=00081030,00080060,00080090` and then
/// fails with "study has no studyInstanceUid" when 0020000D is filtered out.
pub(crate) const MANDATORY_STUDY: [u32; 13] = [
    0x0008_0020, // StudyDate
    0x0008_0030, // StudyTime
    0x0008_0050, // AccessionNumber
    0x0008_0061, // ModalitiesInStudy
    0x0008_0090, // ReferringPhysicianName
    0x0010_0010, // PatientName
    0x0010_0020, // PatientID
    0x0010_0030, // PatientBirthDate
    0x0010_0040, // PatientSex
    0x0020_000D, // StudyInstanceUID
    0x0020_0010, // StudyID
    0x0020_1206, // NumberOfStudyRelatedSeries
    0x0020_1208, // NumberOfStudyRelatedInstances
];
pub(crate) const MANDATORY_SERIES: [u32; 4] = [
    0x0008_0060, // Modality
    0x0020_000E, // SeriesInstanceUID
    0x0020_0011, // SeriesNumber
    0x0020_1209, // NumberOfSeriesRelatedInstances
];
pub(crate) const MANDATORY_INSTANCE: [u32; 5] = [
    0x0008_0016, // SOPClassUID
    0x0008_0018, // SOPInstanceUID
    0x0020_000D, // StudyInstanceUID
    0x0020_000E, // SeriesInstanceUID
    0x0020_0013, // InstanceNumber
];

/// DICOM keyword -> tag table for the keyword form of `includefield`
/// (PS3.18 8.3.4.1 allows the DICOM keyword instead of the 8-hex tag).
/// Only the attributes the QIDO responses can actually emit are listed -
/// anything else is skipped by the filter parser (pre-existing semantics).
/// Shared by the XML mirror (like MANDATORY_*).
pub(crate) const KEYWORD_TO_TAG: &[(&str, u32)] = &[
    ("StudyDate", 0x0008_0020),
    ("StudyTime", 0x0008_0030),
    ("AccessionNumber", 0x0008_0050),
    ("Modality", 0x0008_0060),
    ("ModalitiesInStudy", 0x0008_0061),
    ("Manufacturer", 0x0008_0070),
    ("InstitutionName", 0x0008_0080),
    ("ReferringPhysicianName", 0x0008_0090),
    ("StudyDescription", 0x0008_1030),
    ("StationName", 0x0008_1010),
    ("InstitutionalDepartmentName", 0x0008_1040),
    ("PerformingPhysicianName", 0x0008_1050),
    ("NameOfPhysiciansReadingStudy", 0x0008_1060),
    ("OperatorsName", 0x0008_1070),
    ("PatientName", 0x0010_0010),
    ("PatientID", 0x0010_0020),
    ("PatientBirthDate", 0x0010_0030),
    ("PatientSex", 0x0010_0040),
    ("BodyPartExamined", 0x0018_0015),
    ("ProtocolName", 0x0018_1030),
    ("StudyInstanceUID", 0x0020_000D),
    ("StudyID", 0x0020_0010),
    ("RequestingPhysician", 0x0032_1032),
    ("RequestedProcedureDescription", 0x0032_1060),
    ("NumberOfStudyRelatedSeries", 0x0020_1206),
    ("NumberOfStudyRelatedInstances", 0x0020_1208),
    ("SeriesInstanceUID", 0x0020_000E),
    ("SeriesNumber", 0x0020_0011),
    ("SeriesDescription", 0x0008_103E),
    ("SeriesDate", 0x0008_0021),
    ("SeriesTime", 0x0008_0031),
    ("SOPClassUID", 0x0008_0016),
    ("SOPInstanceUID", 0x0008_0018),
    ("InstanceNumber", 0x0020_0013),
    ("NumberOfSeriesRelatedInstances", 0x0020_1209),
    ("RetrieveURL", 0x0008_1190),
    ("InstanceAvailability", 0x0008_0056),
];

/// Keyword-form lookup: linear scan of [`KEYWORD_TO_TAG`], case-insensitive,
/// zero allocation (no lowercase copy). `None` = unknown keyword -> the
/// caller skips the field.
pub(crate) fn keyword_tag(field: &str) -> Option<u32> {
    KEYWORD_TO_TAG
        .iter()
        .find(|(kw, _)| kw.eq_ignore_ascii_case(field))
        .map(|(_, t)| *t)
}

/// Stream one study object (the 14 key attributes the QIDO response uses) as
/// DICOMweb JSON into `buf`, returning the number of bytes written.
///
/// Writes directly into the caller's fixed slice - no allocation, no
/// `serde_json::Value`. Returns [`StreamOverflow`] if the study does not fit;
/// the caller retries with a larger buffer (or streams per-study as today's
/// server does).
pub fn write_study_json(
    buf: &mut [u8],
    study: &StudyQido,
    includefield: Option<&str>,
    retrieve_base: Option<&str>,
) -> Result<usize, StreamOverflow> {
    // The QIDO contract: attributes are emitted as `"tag": {"vr": VR, "Value":[..]}`.
    // To keep the output stable regardless of manifest key order, we walk the
    // study attributes in DICOM group order via a fixed priority table. The
    // order mirrors the SQLite-era serde `BTreeMap` key sort (alphabetical by
    // 8-hex tag string) so the B-tree tier stays byte-identical - extended with
    // the derived counts (0020,1206 / 0020,1208) that the SQLite study renderer
    // also emits. SpecificCharacterSet (0008,0005) is ALWAYS emitted first (owner
    // see the `emit_scs` helper below the ORDER table.
    const ORDER: [u32; 20] = [
        0x0008_0020, // StudyDate
        0x0008_0030, // StudyTime
        0x0008_0050, // AccessionNumber
        0x0008_0060, // Modality
        0x0008_0061, // ModalitiesInStudy
        0x0008_0080, // InstitutionName (08.09 extension)
        0x0008_0090, // ReferringPhysicianName
        0x0008_1030, // StudyDescription
        0x0008_1050, // PerformingPhysicianName
        0x0008_1060, // NameOfPhysiciansReadingStudy
        0x0010_0010, // PatientName
        0x0010_0020, // PatientID
        0x0010_0030, // PatientBirthDate
        0x0010_0040, // PatientSex
        0x0020_000D, // StudyInstanceUID
        0x0020_0010, // StudyID
        0x0020_1206, // NumberOfStudyRelatedSeries (derived count)
        0x0020_1208, // NumberOfStudyRelatedInstances (derived count)
        0x0032_1032, // RequestingPhysician (08.09 extension)
        0x0032_1060, // RequestedProcedureDescription (08.09 extension)
    ];

    // `includefield` restricts which ORDER tags are emitted,
    // but the PS3.18 mandatory baseline is ALWAYS added (0109: OHIF).
    let filt = TagFilter::from_request(&MANDATORY_STUDY, includefield);
    let mut out = Sink::new(buf);
    out.push(b'{')?;
    let mut wrote = false;
    // SpecificCharacterSet (0008,0005) is always-on (owner 07.09, reverses
    // It is the lowest emitted tag, so it goes first, before the ORDER
    // walk - canonical ascending tag order in every well-formed row.
    emit_scs(&mut out, &mut wrote)?;
    for want in ORDER {
        // C attributes are emitted at their ascending-tag anchors,
        // unconditionally (bypassing tag_pass: they are always-on serving
        // attrs). InstanceAvailability (0008,0056) sorts after (0008,0050)
        // and before (0008,0060); RetrieveURL (0008,1190) sorts after the
        // whole (0008,xxxx) block (max element here 0008,1060) and before the
        // 0010 group PatientName (0010,0010).
        if want == 0x0008_0060 {
            emit_ia(&mut out, &mut wrote)?;
        } else if want == 0x0010_0010 {
            emit_ur(
                &mut out,
                &mut wrote,
                retrieve_base,
                "/dicomweb/studies/",
                study_uid(study),
            )?;
        }
        if !tag_pass(&filt, want) {
            continue;
        }
        // Derived study counts (RAM-only fields, never in attrs).
        if want == 0x0020_1206 {
            emit_written_num(&mut out, &mut wrote, 0x0020_1206, "IS", study.series_count)?;
            continue;
        }
        if want == 0x0020_1208 {
            emit_written_num(
                &mut out,
                &mut wrote,
                0x0020_1208,
                "IS",
                study.instance_count,
            )?;
            continue;
        }
        // ModalitiesInStudy (0008,0061) - derived aggregate from `StudyQido`,
        // comma-joined distinct modalities split WITHOUT trim (mirror of the
        // SQLite-era `GROUP_CONCAT(DISTINCT modality)` -> `split(',')`).
        // None/empty -> the tag is omitted (as `GROUP_CONCAT` over an empty
        // series set yields NULL in SQLite).
        if want == 0x0008_0061 {
            if let Some(csv) = study.modalities_in_study.as_deref()
                && !csv.is_empty()
            {
                emit_written_csv(&mut out, &mut wrote, 0x0008_0061, "CS", csv)?;
            }
            continue;
        }
        for (t, v) in &study.attrs {
            if *t != want {
                continue;
            }
            // Only the scalar/text forms the manifest stores are emitted;
            // array-valued (multi-valued) attributes are not part of the
            // study-level QIDO subset and are skipped.
            if matches!(v, AttrValue::Array(_)) {
                continue;
            }
            if wrote {
                out.push(b',')?;
            }
            wrote = true;
            write_attr(&mut out, *t, v, vr_for_tag(*t))?;
            break;
        }
    }
    out.push(b'}')?;
    Ok(out.len)
}

/// The row's StudyInstanceUID (0020,000D), if stored. Used for RetrieveURL.
fn study_uid(study: &StudyQido) -> Option<&str> {
    series_text(&study.attrs, tag::STUDY_INSTANCE_UID)
}

/// The row's SeriesInstanceUID (0020,000E), if stored. Used for RetrieveURL.
fn series_uid_of(series: &SeriesRecord) -> Option<&str> {
    series_text(&series.attrs, tag::SERIES_INSTANCE_UID)
}

/// The row's SOPInstanceUID (0008,0018), if stored. Used for RetrieveURL.
fn sop_uid_of(instance: &InstanceRecord) -> Option<&str> {
    series_text(&instance.attrs, tag::SOP_INSTANCE_UID)
}

/// Emit the always-on `SpecificCharacterSet` (0008,0005, CS "ISO_IR 192").
///
/// The QIDO writer always emits SpecificCharacterSet (0008,0005):
/// row ALWAYS announces UTF-8 via this tag at the study / series / patients
/// levels, so code-page-guessing clients (MicroDicom builds on DCMTK, whose
/// json2dcm converts UTF-8 to a guessed target charset when the tag is
/// absent) decode Cyrillic PatientName correctly. It is a serving attribute
/// like `InstanceAvailability`: emitted unconditionally, bypassing the
/// includefield `TagFilter`. (0008,0005) is the lowest tag in every emitted
/// dataset, so the caller emits it first (before StudyDate 0008,0020 /
/// SeriesDate 0008,0021), keeping canonical ascending tag order (PS3.19).
fn emit_scs(out: &mut Sink<'_>, wrote: &mut bool) -> Result<(), StreamOverflow> {
    emit_written_str(out, wrote, 0x0008_0005, "CS", "ISO_IR 192")
}

/// Emit the QIDO "C" attribute `InstanceAvailability` (0008,0056, CS "ONLINE").
/// It is ALWAYS emitted - the minimal serving set
/// is mandatory (PS3.18 10.6.3-3/4/5) and it is NOT an includefield-filtered
/// result attribute. The caller places this at its ascending-tag position
/// among the row's own attributes (0008,0056 sorts between (0008,0050) and
/// (0008,0060) for the study level and at the analogous points for
/// series/instance), so the emitted row stays in canonical ascending tag order
/// (PS3.19) across JSON and XML. `InstanceAvailability` and `RetrieveURL` are
/// emitted at DIFFERENT ascending anchors per level, hence two helpers.
fn emit_ia(out: &mut Sink<'_>, wrote: &mut bool) -> Result<(), StreamOverflow> {
    emit_written_str(out, wrote, 0x0008_0056, "CS", "ONLINE")
}

/// Emit the QIDO "C" attribute `RetrieveURL` (0008,1190, UR), conditional.
/// It is emitted only when `retrieve_base` is
/// present (the caller supplied a `Host`) AND the row carries the target UID;
/// the URL is `{retrieve_base}{path}{uid}`. It bypasses includefield
/// (`tag_pass`) - the serving attributes are not filterable result keys. The
/// caller places this at its ascending-tag position among the row's own
/// attributes (0008,1190 sorts after (0008,1060)/(0008,103E) and before the
/// 001x/002x groups, per level).
fn emit_ur(
    out: &mut Sink<'_>,
    wrote: &mut bool,
    retrieve_base: Option<&str>,
    path: &str,
    uid: Option<&str>,
) -> Result<(), StreamOverflow> {
    if let (Some(base), Some(uid)) = (retrieve_base, uid) {
        // Build the absolute URL on a fixed stack buffer (no heap): base is a
        // borrow (<=256) + path + UID; the row's UIDs are <=64 bytes so the
        // buffer never overflows a real Host/scheme.
        let mut url = [0u8; 320];
        let mut n = 0;
        n = copy_slice(&mut url, base.as_bytes(), n);
        n = copy_slice(&mut url, path.as_bytes(), n);
        n = copy_slice(&mut url, uid.as_bytes(), n);
        let s = std::str::from_utf8(&url[..n]).expect("URL is ASCII");
        emit_written_str(out, wrote, 0x0008_1190, "UR", s)?;
    }
    Ok(())
}

fn copy_slice(buf: &mut [u8], src: &[u8], start: usize) -> usize {
    let n = src.len().min(buf.len() - start);
    buf[start..start + n].copy_from_slice(&src[..n]);
    start + n
}

/// A fixed-capacity, allocation-free includefield tag set.
///
/// `includefield` is a comma-separated list of DICOM attributes, each an
/// 8-hex-digit tag or a DICOM keyword (PS3.18 8.3.4.1). To keep
/// the QIDO writers zero-allocation, the set is parsed once into a fixed
/// stack array of numeric `u32` tags (same value the writer compares against),
/// capped at [`TagFilter::MAX_TAGS`]. `None` means "all tags pass" (the
/// no-filter default that the frozen goldens lock in).
struct TagFilter {
    tags: [u32; TagFilter::MAX_TAGS],
    len: usize,
}

impl TagFilter {
    /// 64 tags = a non-standard DoS bound (the ABNF `1#attribute` is
    /// unbounded). The PS3.18 mandatory baseline is inserted FIRST and can
    /// never be evicted by client tags; client tags fill the remainder.
    const MAX_TAGS: usize = 64;

    /// Build the includefield filter for one response level.
    ///
    /// `None` = no filter (emit everything). The PS3.18 mandatory baseline
    /// is inserted FIRST and can never be evicted by client tags; client
    /// tags fill the remaining capacity up to [`TagFilter::MAX_TAGS`] (a
    /// non-standard DoS bound - the ABNF `1#attribute` is unbounded).
    fn from_request(baseline: &[u32], includefield: Option<&str>) -> Option<TagFilter> {
        let s = includefield?;
        // `all` is a legal ABNF keyword for the WHOLE value (case-insensitive)
        // meaning "all available attributes" (PS3.18 8.3.4.3) - a null filter
        // admits everything.
        if s.trim().eq_ignore_ascii_case("all") {
            return None;
        }
        let mut f = TagFilter {
            tags: [0; TagFilter::MAX_TAGS],
            len: 0,
        };
        f.add_all(baseline);
        for field in s.split(',') {
            let field = field.trim();
            // The ABNF attribute form is an 8-hex-digit tag OR a DICOM
            // keyword (PS3.18 8.3.4.1). Keywords resolve case-insensitively
            // through the shared KEYWORD_TO_TAG table (linear scan, zero
            // allocation); unknown keywords and garbage are skipped (the
            // pre-existing skip semantics).
            let v = if field.len() == 8 && field.bytes().all(|c| c.is_ascii_hexdigit()) {
                let mut v: u32 = 0;
                for c in field.as_bytes() {
                    let d = match *c {
                        b'0'..=b'9' => c - b'0',
                        b'a'..=b'f' => c - b'a' + 10,
                        _ => c - b'A' + 10,
                    };
                    v = (v << 4) | d as u32;
                }
                Some(v)
            } else {
                keyword_tag(field)
            };
            if let Some(v) = v
                && v != 0
                && f.len < TagFilter::MAX_TAGS
            {
                f.tags[f.len] = v;
                f.len += 1;
            }
        }
        Some(f)
    }

    fn contains(&self, tag: u32) -> bool {
        self.tags[..self.len].contains(&tag)
    }

    /// Append mandatory baseline tags (deduplicated).
    fn add_all(&mut self, tags: &[u32]) {
        for t in tags {
            if self.len >= TagFilter::MAX_TAGS {
                break;
            }
            if self.tags[..self.len].contains(t) {
                continue;
            }
            self.tags[self.len] = *t;
            self.len += 1;
        }
    }
}

/// Whether `tag` passes the includefield filter. `None` (no filter) admits all.
fn tag_pass(filt: &Option<TagFilter>, tag: u32) -> bool {
    match filt {
        Some(f) => f.contains(tag),
        None => true,
    }
}

/// Stream a series record as a DICOMweb JSON object into `buf`, returning the
/// number of bytes written.
///
/// Emits the captured series attributes in ascending tag order, `SeriesNumber`
/// (0020,0011) as a JSON **number**, and `NumberOfSeriesRelatedInstances`
/// (0020,1209) as a JSON number taken from `series.instances.len()`.
/// `SpecificCharacterSet` (0008,0005) is ALWAYS emitted first (owner 07.09,
/// Zero allocation; caller-owned buffer.
/// `includefield` filters the emitted tags; `None` emits them all.
pub fn write_series_json(
    buf: &mut [u8],
    series: &SeriesRecord,
    includefield: Option<&str>,
    retrieve_base: Option<&str>,
    study_uid: Option<&str>,
) -> Result<usize, StreamOverflow> {
    let filt = TagFilter::from_request(&MANDATORY_SERIES, includefield);
    let mut out = Sink::new(buf);
    let mut wrote = false;
    out.push(b'{')?;
    // SpecificCharacterSet (0008,0005) is always-on (owner 07.09, reverses
    // It is the lowest emitted tag, so it goes first, before
    // SeriesDate (0008,0021) - canonical ascending tag order.
    emit_scs(&mut out, &mut wrote)?;

    // (0008,0021) SeriesDate - DA, canonical 8-digit string.
    if let Some(d) = series_attr(&series.attrs, tag::SERIES_DATE)
        && tag_pass(&filt, 0x0008_0021)
    {
        match d {
            AttrValue::UInt(u) => {
                let mut dbuf = [0u8; 8];
                push_date8(&mut dbuf, *u);
                emit_written_str(&mut out, &mut wrote, 0x0008_0021, "DA", bytes_to_str(&dbuf))?;
            }
            AttrValue::Text(s) => {
                emit_written_str(&mut out, &mut wrote, 0x0008_0021, "DA", s)?;
            }
            _ => {}
        }
    }
    // (0008,0031) SeriesTime - TM.
    if let Some(s) = series_text(&series.attrs, tag::SERIES_TIME)
        && tag_pass(&filt, 0x0008_0031)
    {
        emit_written_str(&mut out, &mut wrote, 0x0008_0031, "TM", s)?;
    }
    // C attribute InstanceAvailability (0008,0056) - always-on serving
    // attr, ascends after (0008,0031) and before (0008,0060).
    emit_ia(&mut out, &mut wrote)?;
    // (0008,0060) Modality - CS.
    if let Some(s) = series_text(&series.attrs, tag::MODALITY)
        && tag_pass(&filt, 0x0008_0060)
    {
        emit_written_str(&mut out, &mut wrote, 0x0008_0060, "CS", s)?;
    }
    // (0008,0070) Manufacturer - LO (08.09 extension, includefield-gated).
    if let Some(s) = series_text(&series.attrs, tag::MANUFACTURER)
        && tag_pass(&filt, 0x0008_0070)
    {
        emit_written_str(&mut out, &mut wrote, 0x0008_0070, "LO", s)?;
    }
    // (0008,1010) StationName - SH.
    if let Some(s) = series_text(&series.attrs, tag::STATION_NAME)
        && tag_pass(&filt, 0x0008_1010)
    {
        emit_written_str(&mut out, &mut wrote, 0x0008_1010, "SH", s)?;
    }
    // (0008,103E) SeriesDescription - LO.
    if let Some(s) = series_text(&series.attrs, tag::SERIES_DESCRIPTION)
        && tag_pass(&filt, 0x0008_103E)
    {
        emit_written_str(&mut out, &mut wrote, 0x0008_103E, "LO", s)?;
    }
    // (0008,1040) InstitutionalDepartmentName - LO (08.09 extension).
    if let Some(s) = series_text(&series.attrs, tag::INSTITUTIONAL_DEPARTMENT_NAME)
        && tag_pass(&filt, 0x0008_1040)
    {
        emit_written_str(&mut out, &mut wrote, 0x0008_1040, "LO", s)?;
    }
    // (0008,1070) OperatorsName - PN (08.09 extension). The object
    // `{"Alphabetic":"..."}` form (PS3.18 Annex F.2.1) - the same contract
    // the study-level PN keys use (Weasis reads the Alphabetic component).
    if let Some(s) = series_text(&series.attrs, tag::OPERATORS_NAME)
        && tag_pass(&filt, 0x0008_1070)
    {
        emit_written_pn(&mut out, &mut wrote, 0x0008_1070, s)?;
    }
    // C attribute RetrieveURL (0008,1190) - conditional serving attr,
    // ascends after the whole (0008,xxxx) block (0008,1190) and before the
    // (0018,0015) BodyPartExamined element.
    // 06.09 field incident: Weasis appends "/instances" to this URL and
    // queries the result verbatim. A series-root URL makes that query 404.
    // In study context the URL is therefore study-scoped; the global series
    // QIDO keeps the series-root form (served by the Weasis-compat
    // series-root instances route).
    if let Some(st) = study_uid {
        if let Some(se) = series_uid_of(series) {
            let mut u = [0u8; 192];
            let mut n = 0;
            n = copy_slice(&mut u, st.as_bytes(), n);
            n = copy_slice(&mut u, b"/series/", n);
            n = copy_slice(&mut u, se.as_bytes(), n);
            let s = std::str::from_utf8(&u[..n]).expect("URL is ASCII");
            emit_ur(
                &mut out,
                &mut wrote,
                retrieve_base,
                "/dicomweb/studies/",
                Some(s),
            )?;
        }
    } else {
        emit_ur(
            &mut out,
            &mut wrote,
            retrieve_base,
            "/dicomweb/series/",
            series_uid_of(series),
        )?;
    }
    // (0018,0015) BodyPartExamined - CS.
    if let Some(s) = series_text(&series.attrs, tag::BODY_PART_EXAMINED)
        && tag_pass(&filt, 0x0018_0015)
    {
        emit_written_str(&mut out, &mut wrote, 0x0018_0015, "CS", s)?;
    }
    // (0018,1030) ProtocolName - LO (08.09 extension, includefield-gated).
    if let Some(s) = series_text(&series.attrs, tag::PROTOCOL_NAME)
        && tag_pass(&filt, 0x0018_1030)
    {
        emit_written_str(&mut out, &mut wrote, 0x0018_1030, "LO", s)?;
    }
    // (0020,000E) SeriesInstanceUID - UI (always present).
    if let Some(s) = series_text(&series.attrs, tag::SERIES_INSTANCE_UID)
        && tag_pass(&filt, 0x0020_000E)
    {
        emit_written_str(&mut out, &mut wrote, 0x0020_000E, "UI", s)?;
    }
    // (0020,0011) SeriesNumber - IS as a JSON number (non-numeric omitted).
    if let Some(n) = series_number(&series.attrs, tag::SERIES_NUMBER)
        && tag_pass(&filt, 0x0020_0011)
    {
        emit_written_num(&mut out, &mut wrote, 0x0020_0011, "IS", n)?;
    }
    // (0020,0060) Laterality - CS.
    if let Some(s) = series_text(&series.attrs, tag::LATERALITY)
        && tag_pass(&filt, 0x0020_0060)
    {
        emit_written_str(&mut out, &mut wrote, 0x0020_0060, "CS", s)?;
    }
    // (0020,1209) NumberOfSeriesRelatedInstances - IS as a JSON number.
    if tag_pass(&filt, 0x0020_1209) {
        emit_written_num(
            &mut out,
            &mut wrote,
            0x0020_1209,
            "IS",
            series.instances.len() as u64,
        )?;
    }

    out.push(b'}')?;
    Ok(out.len)
}

/// Stream an instance record as a DICOMweb JSON object into `buf`, returning
/// the number of bytes written.
///
/// Byte-identical to the frozen SQLite-era `instance_to_dicomweb_json`: SOP
/// Class/Instance UIDs, the injected Study/Series Instance UIDs from context,
/// and `InstanceNumber` (0020,0013) as a JSON **number**. Zero allocation.
pub fn write_instance_json(
    buf: &mut [u8],
    instance: &InstanceRecord,
    study_uid: &str,
    series_uid: &str,
    includefield: Option<&str>,
    retrieve_base: Option<&str>,
) -> Result<usize, StreamOverflow> {
    let filt = TagFilter::from_request(&MANDATORY_INSTANCE, includefield);
    let mut out = Sink::new(buf);
    let mut wrote = false;
    out.push(b'{')?;

    // (0008,0016) SOPClassUID - UI, if present.
    if let Some(s) = instance_text(&instance.attrs, tag::SOP_CLASS_UID)
        && tag_pass(&filt, 0x0008_0016)
    {
        emit_written_str(&mut out, &mut wrote, 0x0008_0016, "UI", s)?;
    }
    // (0008,0018) SOPInstanceUID - UI (always present).
    if let Some(s) = instance_text(&instance.attrs, tag::SOP_INSTANCE_UID)
        && tag_pass(&filt, 0x0008_0018)
    {
        emit_written_str(&mut out, &mut wrote, 0x0008_0018, "UI", s)?;
    }
    // C attributes: InstanceAvailability (0008,0056) always, then
    // RetrieveURL (0008,1190) conditional. Both live in the (0008,xxxx)
    // group, ascending after SOPInstanceUID (0008,0018) and before the
    // (0020,000D) StudyInstanceUID block.
    emit_ia(&mut out, &mut wrote)?;
    if let (Some(base), Some(sop)) = (retrieve_base, sop_uid_of(instance)) {
        // {base}/dicomweb/studies/{study}/series/{series}/instances/{sop}
        let mut url = [0u8; 320];
        let mut n = 0;
        n = copy_slice(&mut url, base.as_bytes(), n);
        n = copy_slice(&mut url, b"/dicomweb/studies/", n);
        n = copy_slice(&mut url, study_uid.as_bytes(), n);
        n = copy_slice(&mut url, b"/series/", n);
        n = copy_slice(&mut url, series_uid.as_bytes(), n);
        n = copy_slice(&mut url, b"/instances/", n);
        n = copy_slice(&mut url, sop.as_bytes(), n);
        let s = std::str::from_utf8(&url[..n]).expect("URL is ASCII");
        emit_written_str(&mut out, &mut wrote, 0x0008_1190, "UR", s)?;
    }
    // (0020,000D) StudyInstanceUID - UI, injected from context (OHIF needs it).
    if !study_uid.is_empty() && tag_pass(&filt, 0x0020_000D) {
        emit_written_str(&mut out, &mut wrote, 0x0020_000D, "UI", study_uid)?;
    }
    // (0020,000E) SeriesInstanceUID - UI, injected from context.
    if !series_uid.is_empty() && tag_pass(&filt, 0x0020_000E) {
        emit_written_str(&mut out, &mut wrote, 0x0020_000E, "UI", series_uid)?;
    }
    // (0020,0013) InstanceNumber - IS as a JSON number (non-numeric omitted).
    if let Some(n) = instance_number(&instance.attrs, tag::INSTANCE_NUMBER)
        && tag_pass(&filt, 0x0020_0013)
    {
        emit_written_num(&mut out, &mut wrote, 0x0020_0013, "IS", n)?;
    }

    out.push(b'}')?;
    Ok(out.len)
}

/// A patient-list row ready for the native QIDO-RS `/patients` render.
///
/// Mirrors the fields the SQLite-era serde renderer emitted (same byte-shape),
/// owned so the writer can borrow them; the sink itself stays zero-alloc.
#[derive(Debug, Clone, PartialEq)]
pub struct PatientQido {
    /// (0010,0020) PatientID.
    pub patient_uid: String,
    /// (0010,0010) PatientName.
    pub patient_name: String,
    /// Most recent ingestion epoch (seconds); `null` when unknown.
    pub last_arrival: Option<u64>,
    /// Number of studies for this patient.
    pub study_count: i64,
    /// Internal patient id.
    pub patient_id: i64,
}

/// Stream one patient of the QIDO-RS `/patients` list into `buf`.
///
/// Byte-exact with the frozen serde-era render: outer keys in alphabetical
/// order (`00100010`, `00100020`, `arrived_at`, `patient_id`, `study_count`),
/// each DICOM-attribute object emitting `"Value"` before `"vr"` (serde
/// `BTreeMap` order, 0x56 `Value` < 0x76 `vr`). PN uses the
/// `{"Alphabetic":...}` object form; `arrived_at: None` renders `null`.
pub fn write_patient_json(buf: &mut [u8], p: &PatientQido) -> Result<usize, StreamOverflow> {
    let mut out = Sink::new(buf);
    out.push(b'{')?;
    // SpecificCharacterSet (0008,0005) - always-on (owner 07.09, reverses
    // the patients row carries PatientName too. (0008,0005) is the
    // lowest tag, so it is the first outer key (before 0010,0010) to keep
    // ascending DICOM tag order among the DICOM-attribute keys.
    out.extend(b"\"00080005\":{\"Value\":[\"ISO_IR 192\"],\"vr\":\"CS\"},")?;
    // (0010,0010) PatientName - PN, Alphabetic object form.
    out.extend(b"\"00100010\":{\"Value\":[{\"Alphabetic\":")?;
    write_json_string(&mut out, &p.patient_name)?;
    out.extend(b"}],\"vr\":\"PN\"},")?;
    // (0010,0020) PatientID - LO.
    out.extend(b"\"00100020\":{\"Value\":[")?;
    write_json_string(&mut out, &p.patient_uid)?;
    out.extend(b"],\"vr\":\"LO\"},")?;
    // arrived_at - number or null.
    out.extend(b"\"arrived_at\":")?;
    match p.last_arrival {
        Some(u) => push_dec(&mut out, u)?,
        None => out.extend(b"null")?,
    }
    out.extend(b",\"patient_id\":")?;
    push_dec_i64(&mut out, p.patient_id)?;
    out.extend(b",\"study_count\":")?;
    push_dec_i64(&mut out, p.study_count)?;
    out.push(b'}')?;
    Ok(out.len)
}

// -- Helpers for the byte-exact series/instance writers --------------------

/// The first attribute value for `tag`, if any (any attrs layer).
fn series_attr(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&AttrValue> {
    attrs.iter().find(|(t, _)| *t == tag).map(|(_, v)| v)
}

/// The text value of `tag`, if it is a `Text`.
fn series_text(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&str> {
    match series_attr(attrs, tag) {
        Some(AttrValue::Text(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// The numeric value of `tag` (a non-negative integer), parsed from a `Text`
/// or read from a `UInt`. `None` when absent, empty, or non-numeric.
fn series_number(attrs: &[(u32, AttrValue)], tag: u32) -> Option<u64> {
    match series_attr(attrs, tag) {
        Some(AttrValue::Text(s)) => s.parse::<u64>().ok(),
        Some(AttrValue::UInt(u)) => Some(*u),
        _ => None,
    }
}

/// The text value of `tag` for an instance, if it is a `Text`.
fn instance_text(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&str> {
    series_text(attrs, tag)
}

/// The numeric value of `tag` for an instance (see [`series_number`]).
fn instance_number(attrs: &[(u32, AttrValue)], tag: u32) -> Option<u64> {
    series_number(attrs, tag)
}

/// Emit `"TAG":{"Value":["s"],"vr":"VR"}` with a leading comma when `wrote`.
///
/// The `"Value"` key precedes `"vr"` to match the frozen golden bytes: the
/// SQLite-era renderers build a `serde_json::Value::Object` (a `BTreeMap`),
/// whose keys sort alphabetically - `"Value"` (uppercase, 0x56) before `"vr"`
/// (0x76).
fn emit_written_str(
    out: &mut Sink<'_>,
    wrote: &mut bool,
    tag: u32,
    vr: &'static str,
    s: &str,
) -> Result<(), StreamOverflow> {
    if *wrote {
        out.push(b',')?;
    }
    *wrote = true;
    out.push(b'"')?;
    push_hex8(out, tag)?;
    out.extend(b"\":{\"Value\":[")?;
    write_json_string(out, s)?;
    out.extend(b"],\"vr\":\"")?;
    out.extend(vr.as_bytes())?;
    out.extend(b"\"}")?;
    Ok(())
}

/// Emit `"TAG":{"Value":[{"Alphabetic":"s"}],"vr":"PN"}` with a
/// leading comma when `wrote` - the study-level PN JSON contract
/// (PS3.18 Annex F.2.1, `write_attr` object form), borrowed-str form so the
/// series writer stays allocation-free.
fn emit_written_pn(
    out: &mut Sink<'_>,
    wrote: &mut bool,
    tag: u32,
    s: &str,
) -> Result<(), StreamOverflow> {
    if *wrote {
        out.push(b',')?;
    }
    *wrote = true;
    out.push(b'"')?;
    push_hex8(out, tag)?;
    out.extend(b"\":{\"Value\":[{\"Alphabetic\":")?;
    write_json_string(out, s)?;
    out.extend(b"}],\"vr\":\"PN\"}")?;
    Ok(())
}

/// Emit `"TAG":{"Value":[n],"vr":"VR"}` with a leading comma when `wrote`.
fn emit_written_num(
    out: &mut Sink<'_>,
    wrote: &mut bool,
    tag: u32,
    vr: &'static str,
    n: u64,
) -> Result<(), StreamOverflow> {
    if *wrote {
        out.push(b',')?;
    }
    *wrote = true;
    out.push(b'"')?;
    push_hex8(out, tag)?;
    out.extend(b"\":{\"Value\":[")?;
    push_dec(out, n)?;
    out.extend(b"],\"vr\":\"")?;
    out.extend(vr.as_bytes())?;
    out.extend(b"\"}")?;
    Ok(())
}

/// Emit `"TAG":{"Value":["a","b"],"vr":"VR"}` for a comma-joined multi-value
/// string, with a leading comma when `wrote`. Each segment is split on `,`
/// WITHOUT trimming (mirror of the SQLite-era `GROUP_CONCAT(DISTINCT ...)`
/// then `split(',')` render) and emitted as a separate quoted JSON string.
fn emit_written_csv(
    out: &mut Sink<'_>,
    wrote: &mut bool,
    tag: u32,
    vr: &'static str,
    csv: &str,
) -> Result<(), StreamOverflow> {
    if *wrote {
        out.push(b',')?;
    }
    *wrote = true;
    out.push(b'"')?;
    push_hex8(out, tag)?;
    out.extend(b"\":{\"Value\":[")?;
    let mut first = true;
    for part in csv.split(',') {
        if !first {
            out.push(b',')?;
        }
        first = false;
        write_json_string(out, part)?;
    }
    out.extend(b"],\"vr\":\"")?;
    out.extend(vr.as_bytes())?;
    out.extend(b"\"}")?;
    Ok(())
}

/// Write a `YYYYMMDD` integer as zero-padded 8 decimal ASCII digits into
/// `buf` (allocation-free).
fn push_date8(buf: &mut [u8; 8], v: u64) {
    for b in buf.iter_mut() {
        *b = b'0';
    }
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
}

/// Write a `HHMMSS` integer as zero-padded 6 decimal ASCII digits into
/// `buf` (allocation-free).
fn push_hms6(buf: &mut [u8; 6], v: u64) {
    for b in buf.iter_mut() {
        *b = b'0';
    }
    let mut rem = v;
    let mut i = 6;
    while i > 0 {
        i -= 1;
        buf[i] = b'0' + (rem % 10) as u8;
        rem /= 10;
        if rem == 0 {
            break;
        }
    }
}

/// Interpret an ASCII-digit byte slice as a `&str` (always ASCII digits, so
/// valid UTF-8).
fn bytes_to_str(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap_or("")
}

/// Append one `"tag": {"Value": [...], "vr": VR}` attribute.
///
/// The tag is rendered as 8 uppercase hex digits directly into the sink; no
/// formatting machinery, no allocation. The `"Value"` key precedes `"vr"`
/// (serde `BTreeMap` order: `Value` 0x56 is lexicographically before `vr`
/// 0x76), which is the frozen SQLite golden contract and matches the study
/// writer's `emit_written_str` / `emit_written_num` helpers. PN values use the
/// object form `{"Alphabetic":"..."}` (PS3.18 Annex F.2.1), the SQLite-era
/// contract Weasis parses; a bare string breaks its patient tree (Weasis QIDO
/// regression, fieldtest 18.08).
fn write_attr(
    out: &mut Sink<'_>,
    tag: u32,
    value: &AttrValue,
    vr: &'static str,
) -> Result<(), StreamOverflow> {
    out.push(b'"')?;
    push_hex8(out, tag)?;
    out.extend(b"\":{\"Value\":[")?;
    match value {
        AttrValue::Text(s) => {
            if vr == "PN" {
                // DICOMweb PN JSON is an object with component keys, not a
                // plain string. Weasis reads the Alphabetic component; the
                // SQLite-era writer emitted exactly `{"Alphabetic":"..."}`
                // (whole string, `^` components kept) - stay byte-compatible.
                out.extend(b"{\"Alphabetic\":")?;
                write_json_string(out, s)?;
                out.push(b'}')?;
            } else {
                write_json_string(out, s)?;
            }
        }
        AttrValue::UInt(u) => {
            // DA/TM must be JSON strings (PS3.18 F.2.1); a numeric token
            // breaks dcm4che's JSONReader, which Weasis uses for QIDO-RS
            // (Weasis fieldtest 18.08). DA: canonical 8-digit YYYYMMDD;
            // TM: 6-digit HHMMSS. Same shape write_series_json emits.
            if vr == "DA" {
                let mut dbuf = [0u8; 8];
                push_date8(&mut dbuf, *u);
                write_json_string(out, bytes_to_str(&dbuf))?;
            } else if vr == "TM" {
                let mut tbuf = [0u8; 6];
                push_hms6(&mut tbuf, *u);
                write_json_string(out, bytes_to_str(&tbuf))?;
            } else {
                push_dec(out, *u)?;
            }
        }
        // Encapsulated bytes are never inlined per the bulkdata rule and are
        // not part of the study-level QIDO subset - omitted (empty array).
        AttrValue::Bytes(_) | AttrValue::Array(_) => {}
    }
    out.push(b']')?;
    out.extend(b",\"vr\":\"")?;
    out.extend(vr.as_bytes())?;
    out.extend(b"\"}")?;
    Ok(())
}

/// Append `s` as a JSON string literal (escaping quotes, backslashes, control
/// chars). UTF-8 bytes pass through verbatim.
fn write_json_string(out: &mut Sink<'_>, s: &str) -> Result<(), StreamOverflow> {
    out.push(b'"')?;
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend(b"\\\"")?,
            b'\\' => out.extend(b"\\\\")?,
            0x08 => out.extend(b"\\b")?,
            0x0C => out.extend(b"\\f")?,
            b'\n' => out.extend(b"\\n")?,
            b'\r' => out.extend(b"\\r")?,
            b'\t' => out.extend(b"\\t")?,
            0x00..=0x1F => {
                out.extend(b"\\u00")?;
                push_hex2(out, b)?;
            }
            _ => out.push(b)?,
        }
    }
    out.push(b'"')?;
    Ok(())
}

/// Append a `u32` as 8 uppercase hex digits (DICOM tag `GGGGEEEE` form).
fn push_hex8(out: &mut Sink<'_>, v: u32) -> Result<(), StreamOverflow> {
    for shift in (0..8).rev() {
        out.push(HEX[((v >> (shift * 4)) & 0xf) as usize])?;
    }
    Ok(())
}

/// Append a byte as 2 uppercase hex digits.
fn push_hex2(out: &mut Sink<'_>, v: u8) -> Result<(), StreamOverflow> {
    out.push(HEX[((v >> 4) & 0xf) as usize])?;
    out.push(HEX[(v & 0xf) as usize])?;
    Ok(())
}

/// Append a `u64` in decimal (no leading zeros; `0` renders as "0").
fn push_dec(out: &mut Sink<'_>, mut v: u64) -> Result<(), StreamOverflow> {
    // Format into a stack buffer then copy out - no heap, no formatting crate.
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend(&buf[i..])
}

/// Append an `i64` in decimal, with a leading `-` for negatives (`serde_json`
/// shape). Non-negative values are byte-identical to `push_dec` of the
/// magnitude.
fn push_dec_i64(out: &mut Sink<'_>, v: i64) -> Result<(), StreamOverflow> {
    if v < 0 {
        out.push(b'-')?;
        push_dec(out, v.unsigned_abs())
    } else {
        push_dec(out, v as u64)
    }
}

/// Uppercase hex digits - DICOMweb JSON tag keys are 8 uppercase hex digits
/// per PS3.18 Section F.2; a lowercase key (e.g. `0020000d`) breaks consumers that
/// look tags up by the standard form.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::AttrValue;

    fn qido(attrs: Vec<(u32, AttrValue)>) -> StudyQido {
        StudyQido {
            attrs,
            ..Default::default()
        }
    }

    #[test]
    fn streams_a_simple_study() {
        let s = qido(vec![
            (0x0008_0020, AttrValue::UInt(20260802)),
            (0x0008_0030, AttrValue::UInt(103000)),
            (0x0008_0060, AttrValue::Text("CT".into())),
            (0x0008_0090, AttrValue::Text("petrov^p".into())),
            (0x0010_0010, AttrValue::Text("ivanov".into())),
            (0x0010_0030, AttrValue::UInt(19870409)),
            (0x0020_000D, AttrValue::Text("1.2.840.1.1".into())),
        ]);
        let mut out = [0u8; 512];
        let n = write_study_json(&mut out, &s, None, None).expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        // DA/TM must be strings (PS3.18 F.2.1): dcm4che JSONReader (Weasis)
        // rejects numeric tokens for string VRs. Attr objects use the golden
        // `{"Value":[...],"vr":"VR"}` key order (serde BTreeMap).
        assert!(got.contains("\"00080020\":{\"Value\":[\"20260802\"],\"vr\":\"DA\"}"));
        assert!(got.contains("\"00080030\":{\"Value\":[\"103000\"],\"vr\":\"TM\"}"));
        assert!(got.contains("\"00100030\":{\"Value\":[\"19870409\"],\"vr\":\"DA\"}"));
        assert!(got.contains("\"00080060\":{\"Value\":[\"CT\"],\"vr\":\"CS\"}"));
        // PN must be the object form (PS3.18 F.2.1): Weasis parses
        // `{"Alphabetic":...}` and drops patients on a bare string.
        assert!(
            got.contains("\"00080090\":{\"Value\":[{\"Alphabetic\":\"petrov^p\"}],\"vr\":\"PN\"}")
        );
        assert!(
            got.contains("\"00100010\":{\"Value\":[{\"Alphabetic\":\"ivanov\"}],\"vr\":\"PN\"}")
        );
        assert!(got.contains("\"0020000D\":{\"Value\":[\"1.2.840.1.1\"],\"vr\":\"UI\"}"));
        assert!(got.starts_with('{') && got.ends_with('}'));
    }

    #[test]
    fn emits_performing_and_reading_physician_as_pn_object() {
        let s = qido(vec![
            (0x0008_1050, AttrValue::Text("Perf^Doc".into())),
            (0x0008_1060, AttrValue::Text("Read^Doc".into())),
        ]);
        let mut out = [0u8; 512];
        let n = write_study_json(&mut out, &s, None, None).expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        assert!(
            got.contains("\"00081050\":{\"Value\":[{\"Alphabetic\":\"Perf^Doc\"}],\"vr\":\"PN\"}")
        );
        assert!(
            got.contains("\"00081060\":{\"Value\":[{\"Alphabetic\":\"Read^Doc\"}],\"vr\":\"PN\"}")
        );
    }

    // byte-parity of the native patient writer with the frozen
    // serde-era render (captured 19.08 from the live qido_patients path).
    fn patient_some() -> PatientQido {
        PatientQido {
            patient_id: 7,
            patient_uid: "PT-UID-9".into(),
            patient_name: "SMIRNOV^IVAN".into(),
            last_arrival: Some(500),
            study_count: 2,
        }
    }

    fn patient_none() -> PatientQido {
        PatientQido {
            patient_id: 42,
            patient_uid: "U".into(),
            patient_name: "Na\"me\\^X".into(),
            last_arrival: None,
            study_count: 0,
        }
    }

    #[test]
    fn write_patient_json_matches_serde_render() {
        let mut out = [0u8; 512];
        let n = write_patient_json(&mut out, &patient_some()).expect("fits");
        assert_eq!(
            &out[..n],
            b"{\"00080005\":{\"Value\":[\"ISO_IR 192\"],\"vr\":\"CS\"},\"00100010\":{\"Value\":[{\"Alphabetic\":\"SMIRNOV^IVAN\"}],\"vr\":\"PN\"},\"00100020\":{\"Value\":[\"PT-UID-9\"],\"vr\":\"LO\"},\"arrived_at\":500,\"patient_id\":7,\"study_count\":2}"
        );
        let n2 = write_patient_json(&mut out, &patient_none()).expect("fits");
        assert_eq!(
            &out[..n2],
            b"{\"00080005\":{\"Value\":[\"ISO_IR 192\"],\"vr\":\"CS\"},\"00100010\":{\"Value\":[{\"Alphabetic\":\"Na\\\"me\\\\^X\"}],\"vr\":\"PN\"},\"00100020\":{\"Value\":[\"U\"],\"vr\":\"LO\"},\"arrived_at\":null,\"patient_id\":42,\"study_count\":0}"
        );
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        let s = qido(vec![(0x0010_0010, AttrValue::Text("a\"b\\c".into()))]);
        let mut out = [0u8; 256];
        let n = write_study_json(&mut out, &s, None, None).expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        assert!(got.contains("\\\""));
        assert!(got.contains("\\\\"));
    }

    #[test]
    fn empty_study_is_empty_object() {
        // A study without text attributes still emits always-on attrs: first
        // SpecificCharacterSet (0008,0005), then InstanceAvailability
        // (0008,0056) ascending before the derived count tags (0020,1206 /
        // 0020,1208) as 0 - matching the SQLite `COUNT(*)` semantics for a
        // study with no series/instances.
        let s = qido(vec![]);
        let mut out = [0u8; 256];
        let n = write_study_json(&mut out, &s, None, None).expect("fits");
        assert_eq!(
            &out[..n],
            b"{\"00080005\":{\"Value\":[\"ISO_IR 192\"],\"vr\":\"CS\"},\"00080056\":{\"Value\":[\"ONLINE\"],\"vr\":\"CS\"},\"00201206\":{\"Value\":[0],\"vr\":\"IS\"},\"00201208\":{\"Value\":[0],\"vr\":\"IS\"}}"
        );
    }

    #[test]
    fn arrays_are_skipped() {
        // Array-valued attributes (multi-valued) are not part of the study-level
        // QIDO subset; they must be silently omitted, not crash the writer. SCS
        // (0008,0005) and IA (0008,0056) stay always-on and ascend before the
        // derived counts.
        let s = qido(vec![(
            0x9999_9999,
            AttrValue::Array(vec![AttrValue::Text("x".into())]),
        )]);
        let mut out = [0u8; 256];
        let n = write_study_json(&mut out, &s, None, None).expect("fits");
        assert_eq!(
            &out[..n],
            b"{\"00080005\":{\"Value\":[\"ISO_IR 192\"],\"vr\":\"CS\"},\"00080056\":{\"Value\":[\"ONLINE\"],\"vr\":\"CS\"},\"00201206\":{\"Value\":[0],\"vr\":\"IS\"},\"00201208\":{\"Value\":[0],\"vr\":\"IS\"}}"
        );
    }

    // SpecificCharacterSet (0008,0005) is ALWAYS
    // emitted = "ISO_IR 192" at the study level, filtered (`all`) or not, even
    // for a Cyrillic PatientName. The value is deterministic on the writer's
    // always-on rule, not gated on non-ASCII presence.
    #[test]
    fn study_json_always_emits_charset() {
        // Cyrillic PatientName.
        let cyr = qido(vec![(0x0010_0010, AttrValue::Text("ИВАНОВ^ИВАН".into()))]);
        let mut out = [0u8; 256];
        let n = write_study_json(&mut out, &cyr, None, None).expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        assert!(
            got.contains("\"00080005\":{\"Value\":[\"ISO_IR 192\"],\"vr\":\"CS\"}"),
            "charset must be emitted first: {got}"
        );
        // unfiltered vs include-all are equivalent here; present under both.
        let n2 = write_study_json(&mut out, &cyr, Some("all"), None).expect("fits");
        let got2 = String::from_utf8(out[..n2].to_vec()).unwrap();
        assert!(
            got2.contains("\"00080005\":{\"Value\":[\"ISO_IR 192\"],\"vr\":\"CS\"}"),
            "charset must be emitted under include-all: {got2}"
        );
    }

    // ModalitiesInStudy (0008,0061) is emitted from the
    // comma-joined aggregate (split WITHOUT trim, mirroring SQLite), honors
    // includefield, and is omitted when None/empty.
    #[test]
    fn study_modalities_in_study_emit() {
        // Mixed modalities -> {"Value":["CT","MR"],"vr":"CS"}.
        let s = qido(vec![(0x0020_000D, AttrValue::Text("1.2.840.1.1".into()))]);
        let mut s = s;
        s.modalities_in_study = Some("CT,MR".into());
        let mut out = [0u8; 256];
        let n = write_study_json(&mut out, &s, None, None).expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        assert!(
            got.contains("\"00080061\":{\"Value\":[\"CT\",\"MR\"],\"vr\":\"CS\"}"),
            "modalities emit: {got}"
        );

        // None -> omitted (like SQLite GROUP_CONCAT NULL over empty series).
        let s2 = qido(vec![]);
        let mut out2 = [0u8; 256];
        let n2 = write_study_json(&mut out2, &s2, None, None).expect("fits");
        let got2 = String::from_utf8(out2[..n2].to_vec()).unwrap();
        assert!(
            !got2.contains("00080061"),
            "empty aggregate must be omitted: {got2}"
        );

        // PS3.18 mandatory baseline: ModalitiesInStudy is emitted even when
        // includefield does not list it (0109: OHIF).
        let mut out3 = [0u8; 256];
        let n3 = write_study_json(&mut out3, &s, Some("0020000D"), None).expect("fits");
        let got3 = String::from_utf8(out3[..n3].to_vec()).unwrap();
        assert!(
            got3.contains("\"00080061\""),
            "modalities are mandatory in the study response: {got3}"
        );
        // And included when requested explicitly.
        let mut out4 = [0u8; 256];
        let n4 = write_study_json(&mut out4, &s, Some("00080061"), None).expect("fits");
        let got4 = String::from_utf8(out4[..n4].to_vec()).unwrap();
        assert!(
            got4.contains("\"00080061\":{\"Value\":[\"CT\",\"MR\"],\"vr\":\"CS\"}"),
            "modalities must be emitted on explicit includefield: {got4}"
        );
    }

    // the derived study counts (0020,1206 / 0020,1208)
    // are emitted as unquoted IS values from the RAM count fields, and only
    // when includefield admits them.
    #[test]
    fn study_counts_emit_derived_series_and_instances() {
        let s = qido(vec![(0x0020_000D, AttrValue::Text("1.2.840.1.1".into()))]);
        // 1 series / 2 instances -> {"Value":[1],"vr":"IS"} / {"Value":[2]...}.
        let mut s = s;
        s.series_count = 1;
        s.instance_count = 2;
        let mut out = [0u8; 256];
        let n = write_study_json(&mut out, &s, None, None).expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        assert!(
            got.contains("\"00201206\":{\"Value\":[1],\"vr\":\"IS\"}"),
            "series count: {got}"
        );
        assert!(
            got.contains("\"00201208\":{\"Value\":[2],\"vr\":\"IS\"}"),
            "instance count: {got}"
        );

        // 2 series / 5 instances.
        let s2 = qido(vec![]);
        let mut s2 = s2;
        s2.series_count = 2;
        s2.instance_count = 5;
        let mut out2 = [0u8; 256];
        let n2 = write_study_json(&mut out2, &s2, None, None).expect("fits");
        let got2 = String::from_utf8(out2[..n2].to_vec()).unwrap();
        assert!(got2.contains("\"00201206\":{\"Value\":[2],\"vr\":\"IS\"}"));
        assert!(got2.contains("\"00201208\":{\"Value\":[5],\"vr\":\"IS\"}"));

        // PS3.18 mandatory baseline: the derived counts are emitted even
        // when includefield does not list them (0109: OHIF).
        let mut out3 = [0u8; 256];
        let n3 = write_study_json(&mut out3, &s, Some("0020000D"), None).expect("fits");
        let got3 = String::from_utf8(out3[..n3].to_vec()).unwrap();
        assert!(got3.contains("00201206"), "counts are mandatory: {got3}");
        assert!(got3.contains("00201208"), "counts are mandatory: {got3}");
    }

    // A client sending more tags than the filter capacity must not
    // evict the mandatory baseline. The baseline is inserted FIRST now (it
    // was appended AFTER user tags pre-fix, so 33+ tags silently dropped
    // the whole baseline including 0020,000D - the OHIF loss mode).
    #[test]
    fn filter_baseline_survives_overflow() {
        // 60 distinct valid tags outside the baseline value range.
        let sixty: String = (0..60)
            .map(|i| format!("{:08X}", 0x1000_0001u32 + i))
            .collect::<Vec<_>>()
            .join(",");
        let f = TagFilter::from_request(&MANDATORY_STUDY, Some(&sixty)).expect("filter");
        for t in &MANDATORY_STUDY {
            assert!(f.contains(*t), "baseline {t:08X} must survive overflow");
        }
        // Remaining capacity after the 13-tag baseline is 64 - 13 = 51.
        for i in 0..51 {
            assert!(f.contains(0x1000_0001 + i), "user tag {i} must be admitted");
        }
        for i in 51..60 {
            assert!(!f.contains(0x1000_0001 + i), "user tag {i} must be dropped");
        }
    }

    // `includefield=all` is the ABNF keyword for "all available
    // attributes" (PS3.18 8.3.4.3), case-insensitive; a null filter admits
    // everything.
    #[test]
    fn filter_all_keyword_is_none() {
        assert!(TagFilter::from_request(&MANDATORY_STUDY, Some("all")).is_none());
        assert!(TagFilter::from_request(&MANDATORY_STUDY, Some("ALL")).is_none());
        assert!(TagFilter::from_request(&MANDATORY_STUDY, Some(" all ")).is_none());
    }

    // The standard defines `all` only as the WHOLE value; in a mixed token
    // list the `all` token is ignored as a non-hex field and the rest still
    // filters.
    #[test]
    fn filter_mixed_all_token_is_skipped() {
        let f = TagFilter::from_request(&MANDATORY_STUDY, Some("all,00081030")).expect("filter");
        assert!(f.contains(0x0008_1030), "requested tag must be present");
        assert!(f.contains(0x0020_000D), "baseline must be present");
    }

    // keyword-form includefield. The ABNF attribute is an 8-hex-digit
    // tag or a DICOM keyword (PS3.18 8.3.4.1); keywords resolve
    // case-insensitively via KEYWORD_TO_TAG (zero-alloc linear scan).
    #[test]
    fn filter_keyword_form_resolves_to_tag() {
        // PatientName is NOT in the series baseline, so a hit proves the
        // keyword resolved to 00100010.
        let f = TagFilter::from_request(&MANDATORY_SERIES, Some("PatientName")).expect("filter");
        assert!(
            f.contains(0x0010_0010),
            "PatientName must resolve to 00100010"
        );
        let f2 = TagFilter::from_request(&MANDATORY_SERIES, Some("patientname")).expect("filter");
        assert!(
            f2.contains(0x0010_0010),
            "keyword lookup is case-insensitive"
        );
        // An 8-character keyword is not 8 hex digits - the old len==8 gate
        // would have dropped it silently.
        let f3 = TagFilter::from_request(&MANDATORY_STUDY, Some("Modality")).expect("filter");
        assert!(f3.contains(0x0008_0060), "Modality (8 chars) must resolve");
        // Unknown keyword -> skipped; the baseline survives.
        let f4 = TagFilter::from_request(&MANDATORY_SERIES, Some("NoSuchKeyword")).expect("filter");
        assert!(
            f4.contains(0x0020_000E),
            "baseline must survive unknown keyword"
        );
        assert!(!f4.contains(0x0010_0010));
    }

    // Hex and keyword forms mix in a single includefield value.
    #[test]
    fn filter_hex_keyword_mix() {
        let f = TagFilter::from_request(&MANDATORY_SERIES, Some("00200011,SeriesDate"))
            .expect("filter");
        assert!(f.contains(0x0020_0011), "hex tag present");
        assert!(f.contains(0x0008_0021), "keyword tag present");
    }

    // Writer level: `all` and `None` produce byte-identical study output.
    #[test]
    fn write_study_json_all_keyword_matches_none() {
        let s = qido(vec![
            (0x0008_0020, AttrValue::UInt(20260802)),
            (0x0008_0060, AttrValue::Text("CT".into())),
            (0x0010_0010, AttrValue::Text("ivanov".into())),
            (0x0020_000D, AttrValue::Text("1.2.840.1.1".into())),
        ]);
        let mut buf_a = [0u8; 1024];
        let mut buf_b = [0u8; 1024];
        let a = write_study_json(&mut buf_a, &s, Some("all"), None).expect("fits");
        let b = write_study_json(&mut buf_b, &s, None, None).expect("fits");
        assert_eq!(&buf_a[..a], &buf_b[..b]);
    }

    #[test]
    fn undersized_buffer_reports_overflow() {
        let s = qido(vec![(0x0010_0010, AttrValue::Text("ivanov".into()))]);
        let mut tiny = [0u8; 4];
        assert_eq!(
            write_study_json(&mut tiny, &s, None, None),
            Err(StreamOverflow)
        );
    }

    #[test]
    fn series_json_roundtrips_captured_attrs() {
        let series = crate::manifest::SeriesRecord {
            attrs: vec![
                (0x0008_0021, AttrValue::UInt(20260802)), // SeriesDate
                (0x0008_103E, AttrValue::Text("Chest CT".into())), // SeriesDescription
                (0x0020_000E, AttrValue::Text("1.2.840.1.2".into())), // SeriesInstanceUID
                (0x0020_0011, AttrValue::Text("3".into())), // SeriesNumber
            ],
            instances: Vec::new(),
        };
        let mut out = [0u8; 512];
        let n = write_series_json(&mut out, &series, None, None, None).expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        // SCS (0008,0005) is always-on and is the first emitted series tag.
        assert!(
            got.contains("\"00080005\":{\"Value\":[\"ISO_IR 192\"],\"vr\":\"CS\"}"),
            "charset must be emitted first: {got}"
        );
        assert!(got.contains("\"00080021\":{\"Value\":[\"20260802\"],\"vr\":\"DA\"}"));
        assert!(got.contains("\"0008103E\":{\"Value\":[\"Chest CT\"],\"vr\":\"LO\"}"));
        assert!(got.contains("\"0020000E\":{\"Value\":[\"1.2.840.1.2\"],\"vr\":\"UI\"}"));
        assert!(got.contains("\"00200011\":{\"Value\":[3],\"vr\":\"IS\"}"));
        assert!(got.contains("\"00201209\":{\"Value\":[0],\"vr\":\"IS\"}"));
        assert!(got.starts_with('{') && got.ends_with('}'));
    }

    #[test]
    fn instance_json_roundtrips_captured_attrs() {
        let instance = crate::manifest::InstanceRecord {
            attrs: vec![
                (
                    0x0008_0016,
                    AttrValue::Text("1.2.840.10008.5.1.4.1.1.2".into()),
                ),
                (0x0008_0018, AttrValue::Text("1.2.840.1.3".into())),
                (0x0020_0013, AttrValue::Text("7".into())),
            ],
            transfer_syntax: String::new(),
            storage_tier: 1,
            rejection_reason: crate::manifest::NOT_REJECTED_REASON,
            blake3: [0u8; 32],
            file_size: 0,
            frames: Vec::new(),
        };
        let mut out = [0u8; 512];
        // Inject the study/series UIDs from the request context (OHIF needs
        // them) - matches the frozen SQLite renderer.
        let n = write_instance_json(
            &mut out,
            &instance,
            "1.2.840.1.1",
            "1.2.840.1.2",
            None,
            None,
        )
        .expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        assert!(
            got.contains("\"00080016\":{\"Value\":[\"1.2.840.10008.5.1.4.1.1.2\"],\"vr\":\"UI\"}")
        );
        assert!(got.contains("\"00080018\":{\"Value\":[\"1.2.840.1.3\"],\"vr\":\"UI\"}"));
        assert!(got.contains("\"0020000D\":{\"Value\":[\"1.2.840.1.1\"],\"vr\":\"UI\"}"));
        assert!(got.contains("\"0020000E\":{\"Value\":[\"1.2.840.1.2\"],\"vr\":\"UI\"}"));
        assert!(got.contains("\"00200013\":{\"Value\":[7],\"vr\":\"IS\"}"));
        assert!(got.starts_with('{') && got.ends_with('}'));
    }

    #[test]
    fn series_json_skips_array_and_bytes() {
        let series = crate::manifest::SeriesRecord {
            attrs: vec![
                (0x0008_0021, AttrValue::UInt(20260802)),
                (
                    0x8888_8888,
                    AttrValue::Array(vec![AttrValue::Text("x".into())]),
                ),
                (0x7777_7777, AttrValue::Bytes(vec![1, 2, 3])),
            ],
            instances: Vec::new(),
        };
        let mut out = [0u8; 256];
        let n = write_series_json(&mut out, &series, None, None, None).expect("fits");
        let got = String::from_utf8(out[..n].to_vec()).unwrap();
        assert!(got.contains("\"00080021\""));
        assert!(!got.contains("88888888"), "array attr must be omitted");
        assert!(!got.contains("77777777"), "bytes attr must be omitted");
    }

    #[test]
    fn series_json_undersized_reports_overflow() {
        let series = crate::manifest::SeriesRecord {
            attrs: vec![(0x0020_000E, AttrValue::Text("1.2.840.1.2".into()))],
            instances: Vec::new(),
        };
        let mut tiny = [0u8; 4];
        assert_eq!(
            write_series_json(&mut tiny, &series, None, None, None),
            Err(StreamOverflow)
        );
    }

    // With a retrieve_base the writer emits RetrieveURL (0008,1190, UR)
    // = {base}/dicomweb/... alongside InstanceAvailability (0008,0056 ONLINE);
    // without a base only InstanceAvailability is present (no URL). Pinned for
    // the study / series / instance levels (the HTTP shower is exercised by
    // the smoke fixtures, which carry a deterministic Host).
    #[test]
    fn retrieve_url_emitted_with_base_and_ia_always() {
        // Study
        let study = qido(vec![(0x0020_000D, AttrValue::Text("1.2.840.1.1".into()))]);
        let mut out = [0u8; 1024];
        let n = write_study_json(&mut out, &study, None, Some("http://qido.test")).expect("fits");
        let s = String::from_utf8(out[..n].to_vec()).unwrap();
        assert!(s.contains("\"00080056\":{\"Value\":[\"ONLINE\"],\"vr\":\"CS\"}"));
        assert!(s.contains(
            "\"00081190\":{\"Value\":[\"http://qido.test/dicomweb/studies/1.2.840.1.1\"],\"vr\":\"UR\"}"
        ));
        // Same study, no base -> no RetrieveURL, IA still present.
        let n0 = write_study_json(&mut out, &study, None, None).expect("fits");
        let s0 = String::from_utf8(out[..n0].to_vec()).unwrap();
        assert!(s0.contains("\"00080056\":{\"Value\":[\"ONLINE\"],\"vr\":\"CS\"}"));
        assert!(!s0.contains("00081190"), "no base -> no RetrieveURL: {s0}");

        // Series
        let series = crate::manifest::SeriesRecord {
            attrs: vec![(0x0020_000E, AttrValue::Text("1.2.840.1.2".into()))],
            instances: Vec::new(),
        };
        let mut sbuf = [0u8; 1024];
        let ns = write_series_json(&mut sbuf, &series, None, Some("http://qido.test"), None)
            .expect("fits");
        let ss = String::from_utf8(sbuf[..ns].to_vec()).unwrap();
        assert!(ss.contains("\"00080056\":{\"Value\":[\"ONLINE\"],\"vr\":\"CS\"}"));
        assert!(ss.contains(
            "\"00081190\":{\"Value\":[\"http://qido.test/dicomweb/series/1.2.840.1.2\"],\"vr\":\"UR\"}"
        ));
        // 06.09 Weasis incident: in study context the URL is study-scoped so
        // a client appending "/instances" reaches a real route.
        let ns2 = write_series_json(
            &mut sbuf,
            &series,
            None,
            Some("http://qido.test"),
            Some("1.2.840.1.1"),
        )
        .expect("fits");
        let ss2 = String::from_utf8(sbuf[..ns2].to_vec()).unwrap();
        assert!(ss2.contains(
            "\"00081190\":{\"Value\":[\"http://qido.test/dicomweb/studies/1.2.840.1.1/series/1.2.840.1.2\"],\"vr\":\"UR\"}"
        ));

        // Instance: full {base}/dicomweb/studies/{study}/series/{s}/instances/{sop}
        let instance = crate::manifest::InstanceRecord {
            attrs: vec![(tag::SOP_INSTANCE_UID, AttrValue::Text("1.2.840.1.3".into()))],
            transfer_syntax: String::new(),
            storage_tier: 1,
            rejection_reason: crate::manifest::NOT_REJECTED_REASON,
            blake3: [0u8; 32],
            file_size: 0,
            frames: Vec::new(),
        };
        let mut ibuf = [0u8; 1024];
        let ni = write_instance_json(
            &mut ibuf,
            &instance,
            "1.2.840.1.1",
            "1.2.840.1.2",
            None,
            Some("http://qido.test"),
        )
        .expect("fits");
        let si = String::from_utf8(ibuf[..ni].to_vec()).unwrap();
        assert!(si.contains("\"00080056\":{\"Value\":[\"ONLINE\"],\"vr\":\"CS\"}"));
        assert!(si.contains(
            "\"00081190\":{\"Value\":[\"http://qido.test/dicomweb/studies/1.2.840.1.1/series/1.2.840.1.2/instances/1.2.840.1.3\"],\"vr\":\"UR\"}"
        ));
    }
}
