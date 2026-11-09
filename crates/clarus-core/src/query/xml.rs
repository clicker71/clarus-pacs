// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOM XML (PS3.19) emission for the B-tree QIDO tier - a typed mirror of
//! the streaming JSON writer (`crate::query::json`).
//!
//! The QIDO XML response body is a `<NativeDicomModel>` document whose
//! top-level `<DicomAttribute>` siblings stream straight from the integer
//! `(u32 tag, value)` manifest attributes. No `serde_json::Value` is
//! allocated on the study hotpath; the `DicomAttribute`/`Value`/`PersonName`
//! tag characters are appended directly into a caller-owned buffer.
//!
//! This writer reproduces the exact attribute set and
//! order of the JSON mirror (so a QIDO query answers identically in JSON and
//! XML), rendering each attribute in DICOM XML (PS3.19 Appendix A.1). PN
//! values are decomposed on `^` exactly as the legacy Value-based converter
//! did; VR comes from the shared `vr_for_tag` mapping.

use crate::manifest::{AttrValue, InstanceRecord, SeriesRecord, StudyQido};

/// The DICOM XML namespace for the NativeDICOM model (PS3.19).
const DICOM_XML_NS: &str = "http://dicom.nema.org/PS3.19/models/NativeDICOM";

/// Streaming byte sink that only writes into a fixed caller-owned buffer.
///
/// Mirrors the allocation-free `Sink` in `crate::query::json` - the ap101
/// design constraint that the study hotpath allocates nothing.
struct Sink<'a> {
    buf: &'a mut [u8],
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

/// Uppercase hex digits - DICOM XML tag keys are 8 uppercase hex digits
/// per PS3.19 (the same `GGGGEEEE` form DICOMweb JSON uses).
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Append a `u32` as 8 uppercase hex digits (DICOM tag `GGGGEEEE` form).
fn push_hex8(out: &mut Sink<'_>, v: u32) -> Result<(), StreamOverflow> {
    for shift in (0..8).rev() {
        out.push(HEX[((v >> (shift * 4)) & 0xf) as usize])?;
    }
    Ok(())
}

/// Append a `u64` in decimal (no leading zeros; `0` renders as "0").
fn push_dec(out: &mut Sink<'_>, mut v: u64) -> Result<(), StreamOverflow> {
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

// -- XML escaping ----------------------------------------------------------

/// Escape text content for XML element bodies (5 special chars).
fn esc_into(out: &mut Sink<'_>, s: &str) -> Result<(), StreamOverflow> {
    for &b in s.as_bytes() {
        match b {
            b'&' => out.extend(b"&amp;")?,
            b'<' => out.extend(b"&lt;")?,
            b'>' => out.extend(b"&gt;")?,
            b'"' => out.extend(b"&quot;")?,
            b'\'' => out.extend(b"&apos;")?,
            _ => out.push(b)?,
        }
    }
    Ok(())
}

// -- PersonName decomposition ----------------------------------------------

/// Write a DICOM PersonName (VR=PN) as an XML `PersonName` element.
///
/// Decomposes the raw `^`-delimited value into up to 5 components and emits
/// each non-empty component. Supports the three PN groups (Alphabetic before
/// the first `=`, Ideographic between `=`, Phonetic after) - identical to the
/// legacy Value-based converter.
fn write_person_name(out: &mut Sink<'_>, raw: &str) -> Result<(), StreamOverflow> {
    let groups: Vec<&str> = raw.split('=').collect();
    out.extend(b"<PersonName>")?;

    // Helper: write one name group.
    fn write_name_group(
        out: &mut Sink<'_>,
        group_name: &str,
        value: &str,
    ) -> Result<(), StreamOverflow> {
        let parts: Vec<&str> = value.split('^').collect();
        out.push(b'<')?;
        out.extend(group_name.as_bytes())?;
        out.push(b'>')?;

        let write_part = |out: &mut Sink<'_>, elem: &str, v: &str| -> Result<(), StreamOverflow> {
            out.push(b'<')?;
            out.extend(elem.as_bytes())?;
            out.push(b'>')?;
            esc_into(out, v)?;
            out.extend(b"</")?;
            out.extend(elem.as_bytes())?;
            out.push(b'>')?;
            Ok(())
        };

        if let Some(f) = parts.first()
            && !f.is_empty()
        {
            write_part(out, "FamilyName", f)?;
        }
        if let Some(g) = parts.get(1)
            && !g.is_empty()
        {
            write_part(out, "GivenName", g)?;
        }
        if let Some(m) = parts.get(2)
            && !m.is_empty()
        {
            write_part(out, "MiddleName", m)?;
        }
        if let Some(p) = parts.get(3)
            && !p.is_empty()
        {
            write_part(out, "Prefix", p)?;
        }
        if let Some(s) = parts.get(4)
            && !s.is_empty()
        {
            write_part(out, "Suffix", s)?;
        }

        out.extend(b"</")?;
        out.extend(group_name.as_bytes())?;
        out.push(b'>')?;
        Ok(())
    }

    if let Some(alphabetic) = groups.first()
        && !alphabetic.is_empty()
    {
        write_name_group(out, "Alphabetic", alphabetic)?;
    }
    if let Some(ideographic) = groups.get(1)
        && !ideographic.is_empty()
    {
        write_name_group(out, "Ideographic", ideographic)?;
    }
    if let Some(phonetic) = groups.get(2)
        && !phonetic.is_empty()
    {
        write_name_group(out, "Phonetic", phonetic)?;
    }

    out.extend(b"</PersonName>")?;
    Ok(())
}

// -- Attribute writer ------------------------------------------------------

/// Append one `<DicomAttribute>` element for a scalar value.
///
/// Mirrors the JSON writer's `write_attr`: `Text` renders as a `Value` body,
/// `UInt` as a bare number, `Bytes`/`Array` are omitted (not part of the
/// study/series/instance QIDO subset). PN-typed attributes use `PersonName`
/// decomposition; all other VRs use a `<Value number="N">` element.
fn write_attr_elem(
    out: &mut Sink<'_>,
    tag: u32,
    value: &AttrValue,
    vr: &str,
) -> Result<(), StreamOverflow> {
    out.extend(b"<DicomAttribute tag=\"")?;
    push_hex8(out, tag)?;
    out.extend(b"\" vr=\"")?;
    out.extend(vr.as_bytes())?;
    out.extend(b"\" keyword=\"\">")?;

    match value {
        AttrValue::Text(s) => {
            if vr == "PN" {
                write_person_name(out, s)?;
            } else {
                out.extend(b"<Value number=\"1\">")?;
                esc_into(out, s)?;
                out.extend(b"</Value>")?;
            }
        }
        AttrValue::UInt(u) => {
            out.extend(b"<Value number=\"1\">")?;
            push_dec(out, *u)?;
            out.extend(b"</Value>")?;
        }
        // Bytes (encapsulated) and Array (multi-valued) are not in the
        // study/series/instance subset - omitted, mirroring the JSON path.
        AttrValue::Bytes(_) | AttrValue::Array(_) => {}
    }

    out.extend(b"</DicomAttribute>")?;
    Ok(())
}

/// Append one `<DicomAttribute>` element for a comma-joined multi-value
/// string - one `<Value number="N">` per comma component, numbering from 1
/// (the DICOM XML mirror of the JSON `"Value":["CT","MR"]` array shape).
/// Components are NOT trimmed, mirroring `emit_written_csv` in the JSON
/// writer.
///
/// Zero allocation by construction: `split(',')` borrows the input string
/// and every byte lands in the caller-owned `Sink`.
fn write_attr_csv_elem(
    out: &mut Sink<'_>,
    tag: u32,
    vr: &str,
    csv: &str,
) -> Result<(), StreamOverflow> {
    out.extend(b"<DicomAttribute tag=\"")?;
    push_hex8(out, tag)?;
    out.extend(b"\" vr=\"")?;
    out.extend(vr.as_bytes())?;
    out.extend(b"\" keyword=\"\">")?;
    for (i, part) in csv.split(',').enumerate() {
        out.extend(b"<Value number=\"")?;
        push_dec(out, (i + 1) as u64)?;
        out.extend(b"\">")?;
        esc_into(out, part)?;
        out.extend(b"</Value>")?;
    }
    out.extend(b"</DicomAttribute>")?;
    Ok(())
}

// -- includefield filter ---------------------------------------------------

/// A fixed-capacity, allocation-free includefield tag set - identical to the
/// JSON mirror's `TagFilter`.
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
            // through the shared KEYWORD_TO_TAG table from the JSON mirror
            // (linear scan, zero allocation); unknown keywords and garbage
            // are skipped (the pre-existing skip semantics).
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
                crate::query::json::keyword_tag(field)
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

// -- Attribute accessors (shared with the JSON mirror) ---------------------

fn series_attr(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&AttrValue> {
    attrs.iter().find(|(t, _)| *t == tag).map(|(_, v)| v)
}

fn series_text(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&str> {
    match series_attr(attrs, tag) {
        Some(AttrValue::Text(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn series_number(attrs: &[(u32, AttrValue)], tag: u32) -> Option<u64> {
    match series_attr(attrs, tag) {
        Some(AttrValue::Text(s)) => s.parse::<u64>().ok(),
        Some(AttrValue::UInt(u)) => Some(*u),
        _ => None,
    }
}

fn instance_text(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&str> {
    series_text(attrs, tag)
}

// -- Public API: document body assembly ------------------------------------

/// Append the XML declaration and the `<NativeDicomModel>` opening root.
///
/// Every `<DicomAttribute>` written afterwards is a top-level sibling under
/// this root until [`write_xml_doc_end`] is called - the standard shape for a
/// QIDO list response (multiple objects share one root, per PS3.19 A.1).
pub fn write_xml_doc_start(out: &mut Vec<u8>) {
    out.extend_from_slice(br#"<?xml version="1.0" encoding="UTF-8"?>"#);
    out.extend_from_slice(b"\n<NativeDicomModel xmlns=\"");
    out.extend_from_slice(DICOM_XML_NS.as_bytes());
    out.extend_from_slice(b"\">\n");
}

/// Append the closing `</NativeDicomModel>` root.
pub fn write_xml_doc_end(out: &mut Vec<u8>) {
    out.extend_from_slice(b"\n</NativeDicomModel>\n");
}

fn copy_slice(buf: &mut [u8], src: &[u8], start: usize) -> usize {
    let n = src.len().min(buf.len() - start);
    buf[start..start + n].copy_from_slice(&src[..n]);
    start + n
}

/// Emit the QIDO "C" attribute `InstanceAvailability` (0008,0056, CS "ONLINE")
/// as a sibling `<DicomAttribute>`. It is ALWAYS
/// emitted (minimal serving set, PS3.18 10.6.3-3/4/5), not includefield
/// filtered. The caller places it at its ascending-tag anchor so the dataset
/// stays in canonical ascending tag order (PS3.19). Split from `RetrieveURL`
/// because the two C attrs sort at DIFFERENT anchors per level.
fn emit_xml_ia(out: &mut Sink<'_>) -> Result<(), StreamOverflow> {
    write_attr_elem(out, 0x0008_0056, &AttrValue::Text("ONLINE".into()), "CS")
}

/// Emit the always-on `SpecificCharacterSet` (0008,0005, CS "ISO_IR 192") as a
/// sibling `<DicomAttribute>`. The QIDO writer
/// dataset ALWAYS announces UTF-8 at the study / series / patients levels, so
/// code-page-guessing clients decode Cyrillic PatientName correctly. Bypasses
/// includefield (serving attr). (0008,0005) is the lowest tag in every emitted
/// dataset, so the caller emits it as the first sibling - canonical ascending
/// tag order (PS3.19). Mirrors the JSON writer's `emit_scs`.
fn emit_xml_scs(out: &mut Sink<'_>) -> Result<(), StreamOverflow> {
    write_attr_elem(
        out,
        0x0008_0005,
        &AttrValue::Text("ISO_IR 192".into()),
        "CS",
    )
}

/// Emit the QIDO "C" attribute `RetrieveURL` (0008,1190, UR) as a sibling
/// `<DicomAttribute>`. It is emitted only when
/// `retrieve_base` is present (caller supplied a `Host`) AND the row carries
/// the target UID (`url` = `{retrieve_base}{path}{uid}`); bypasses
/// includefield. Mirrors the JSON writer's `emit_ur`; element placed at its
/// ascending-tag anchor per level. The XML writers are not on the ap101
/// zero-alloc stream path (they already allocate per-text attribute via
/// `to_string`), so the small `String` here is consistent.
fn emit_xml_ur(
    out: &mut Sink<'_>,
    retrieve_base: Option<&str>,
    path: &str,
    uid: Option<&str>,
) -> Result<(), StreamOverflow> {
    if let (Some(base), Some(uid)) = (retrieve_base, uid) {
        let mut url = [0u8; 320];
        let mut n = 0;
        n = copy_slice(&mut url, base.as_bytes(), n);
        n = copy_slice(&mut url, path.as_bytes(), n);
        n = copy_slice(&mut url, uid.as_bytes(), n);
        let s = std::str::from_utf8(&url[..n]).expect("URL is ASCII");
        write_attr_elem(out, 0x0008_1190, &AttrValue::Text(s.to_string()), "UR")?;
    }
    Ok(())
}

/// Write one study's XML attributes into `buf`, returning bytes written.
///
/// Emits exactly the attributes the JSON mirror's `write_study_json` would
/// (the same ORDER table, the same includefield filter, the same skip of
/// array/bytes values), each as a `<DicomAttribute>` element. PN values are
/// decomposed. `buf` must be large enough for one study; a manifest larger
/// than the caller's scratch surfaces as [`StreamOverflow`].
pub fn write_study_attrs(
    buf: &mut [u8],
    study: &StudyQido,
    includefield: Option<&str>,
    retrieve_base: Option<&str>,
) -> Result<usize, StreamOverflow> {
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

    let filt = TagFilter::from_request(&crate::query::json::MANDATORY_STUDY, includefield);
    let mut out = Sink::new(buf);
    // SpecificCharacterSet (0008,0005) is always-on (owner 07.09, reverses
    // Lowest emitted tag -> first sibling, before the ORDER walk.
    emit_xml_scs(&mut out)?;
    for want in ORDER {
        // C attributes are emitted at their ascending-tag anchors,
        // unconditionally (they are always-on serving attrs, not filterable
        // result keys). InstanceAvailability (0008,0056) sorts after
        // (0008,0050) and before (0008,0060); RetrieveURL (0008,1190) sorts
        // after the whole (0008,xxxx) block (max here 0008,1060) and before
        // the 0010 group PatientName (0010,0010).
        if want == 0x0008_0060 {
            emit_xml_ia(&mut out)?;
        } else if want == 0x0010_0010 {
            let study_uid = series_text(&study.attrs, crate::query::tag::STUDY_INSTANCE_UID);
            emit_xml_ur(&mut out, retrieve_base, "/dicomweb/studies/", study_uid)?;
        }
        if !tag_pass(&filt, want) {
            continue;
        }
        // Derived study counts (RAM-only fields, never in attrs) - the
        // mandatory R attributes of PS3.18 Table 10.6.3-3 (2026c),
        // mirroring the JSON writer's derived branches.
        if want == 0x0020_1206 {
            write_attr_elem(
                &mut out,
                0x0020_1206,
                &AttrValue::UInt(study.series_count),
                "IS",
            )?;
            continue;
        }
        if want == 0x0020_1208 {
            write_attr_elem(
                &mut out,
                0x0020_1208,
                &AttrValue::UInt(study.instance_count),
                "IS",
            )?;
            continue;
        }
        // ModalitiesInStudy (0008,0061) - derived aggregate from `StudyQido`
        // (RAM-only; never stored in attrs). None/empty -> the tag is
        // omitted (as `GROUP_CONCAT` over an empty series set yields NULL).
        if want == 0x0008_0061 {
            if let Some(csv) = study.modalities_in_study.as_deref()
                && !csv.is_empty()
            {
                write_attr_csv_elem(&mut out, 0x0008_0061, "CS", csv)?;
            }
            continue;
        }
        for (t, v) in &study.attrs {
            if *t != want {
                continue;
            }
            if matches!(v, AttrValue::Array(_)) {
                continue;
            }
            write_attr_elem(&mut out, *t, v, crate::query::json::vr_for_tag(*t))?;
            break;
        }
    }
    Ok(out.len)
}

/// Write one patient's two DICOM-tag attributes of the QIDO-RS `/patients`
/// list into `buf` as `<DicomAttribute>` siblings (no document wrapper),
/// returning bytes written.
///
/// Byte-exact with the legacy `dicomweb_json_to_xml` per-patient fragment:
/// the two DICOM-tag attributes (0010,0010 PN / 0010,0020 LO) back-to-back.
/// The non-DICOM patient keys (`arrived_at`, `study_count`, `patient_id`)
/// are not DICOM attributes and are dropped, matching the legacy converter.
/// The caller wraps the siblings in a `<NativeDicomModel>` document.
pub fn write_patient_xml(
    buf: &mut [u8],
    p: &crate::query::json::PatientQido,
) -> Result<usize, StreamOverflow> {
    let mut out = Sink::new(buf);
    // SpecificCharacterSet (0008,0005) is always-on (owner 07.09, reverses
    // the patient row carries PatientName too. (0008,0005) is the first
    // sibling (before 0010,0010) to keep canonical ascending tag order.
    emit_xml_scs(&mut out)?;
    // (0010,0010) PatientName - PN, PersonName decomposition.
    out.extend(b"<DicomAttribute tag=\"00100010\" vr=\"PN\" keyword=\"\">")?;
    write_person_name(&mut out, &p.patient_name)?;
    out.extend(b"</DicomAttribute>")?;
    // (0010,0020) PatientID - LO, single Value.
    out.extend(b"<DicomAttribute tag=\"00100020\" vr=\"LO\" keyword=\"\">")?;
    out.extend(b"<Value number=\"1\">")?;
    esc_into(&mut out, &p.patient_uid)?;
    out.extend(b"</Value></DicomAttribute>")?;
    Ok(out.len)
}

/// Write one patient's `<DicomAttribute>` siblings into a caller Vec.
///
/// Convenience wrapper over [`write_patient_xml`] that grows `out` as needed
/// (the patient list response owns its buffer).
///
/// A patient whose render does not fit the stack buffer grows an owned one and
/// retries rather than disappearing. The old `.ok()` swallowed
/// [`StreamOverflow`], and the caller had no way to notice: one patient with a
/// long name or a long identifier was silently MISSING from the XML patient
/// list, which reads as "not in the archive". Growing the buffer costs one
/// allocation for that row and is visible, which is the honest trade.
pub fn write_patient_xml_into(out: &mut Vec<u8>, p: &crate::query::json::PatientQido) {
    let mut buf = [0u8; 512];
    if let Ok(n) = write_patient_xml(&mut buf, p) {
        out.extend_from_slice(&buf[..n]);
        return;
    }
    let mut grown = vec![0u8; 8192];
    if let Ok(n) = write_patient_xml(&mut grown, p) {
        out.extend_from_slice(&grown[..n]);
    }
}

/// Write one series' XML attributes into `buf`, returning bytes written.
///
/// Mirrors `write_series_json`: the captured series attributes in ascending tag
/// order, `SeriesNumber` (0020,0011) as a number, and
/// `NumberOfSeriesRelatedInstances` (0020,1209) from `series.instances.len()`.
/// `SpecificCharacterSet` (0008,0005) is ALWAYS emitted first (owner 07.09,
///
pub fn write_series_attrs(
    buf: &mut [u8],
    series: &SeriesRecord,
    includefield: Option<&str>,
    retrieve_base: Option<&str>,
    study_uid: Option<&str>,
) -> Result<usize, StreamOverflow> {
    let filt = TagFilter::from_request(&crate::query::json::MANDATORY_SERIES, includefield);
    let mut out = Sink::new(buf);
    // SpecificCharacterSet (0008,0005) is always-on (owner 07.09, reverses
    // first sibling, before SeriesDate (0008,0021).
    emit_xml_scs(&mut out)?;

    // (0008,0021) SeriesDate - DA, canonical 8-digit string.
    if let Some(d) = series_attr(&series.attrs, crate::query::tag::SERIES_DATE)
        && tag_pass(&filt, 0x0008_0021)
    {
        write_attr_elem(&mut out, 0x0008_0021, d, "DA")?;
    }
    // (0008,0031) SeriesTime - TM.
    if let Some(s) = series_text(&series.attrs, crate::query::tag::SERIES_TIME)
        && tag_pass(&filt, 0x0008_0031)
    {
        write_attr_elem(&mut out, 0x0008_0031, &AttrValue::Text(s.to_string()), "TM")?;
    }
    // C attribute InstanceAvailability (0008,0056) - always-on serving
    // attr, ascends after (0008,0031) and before (0008,0060).
    emit_xml_ia(&mut out)?;
    // (0008,0060) Modality - CS.
    if let Some(s) = series_text(&series.attrs, crate::query::tag::MODALITY)
        && tag_pass(&filt, 0x0008_0060)
    {
        write_attr_elem(&mut out, 0x0008_0060, &AttrValue::Text(s.to_string()), "CS")?;
    }
    // (0008,0070) Manufacturer - LO (08.09 extension, includefield-gated).
    if let Some(s) = series_text(&series.attrs, crate::query::tag::MANUFACTURER)
        && tag_pass(&filt, 0x0008_0070)
    {
        write_attr_elem(&mut out, 0x0008_0070, &AttrValue::Text(s.to_string()), "LO")?;
    }
    // (0008,1010) StationName - SH.
    if let Some(s) = series_text(&series.attrs, crate::query::tag::STATION_NAME)
        && tag_pass(&filt, 0x0008_1010)
    {
        write_attr_elem(&mut out, 0x0008_1010, &AttrValue::Text(s.to_string()), "SH")?;
    }
    // (0008,103E) SeriesDescription - LO.
    if let Some(s) = series_text(&series.attrs, crate::query::tag::SERIES_DESCRIPTION)
        && tag_pass(&filt, 0x0008_103E)
    {
        write_attr_elem(&mut out, 0x0008_103E, &AttrValue::Text(s.to_string()), "LO")?;
    }
    // (0008,1040) InstitutionalDepartmentName - LO (08.09 extension).
    if let Some(s) = series_text(
        &series.attrs,
        crate::query::tag::INSTITUTIONAL_DEPARTMENT_NAME,
    ) && tag_pass(&filt, 0x0008_1040)
    {
        write_attr_elem(&mut out, 0x0008_1040, &AttrValue::Text(s.to_string()), "LO")?;
    }
    // (0008,1070) OperatorsName - PN (08.09 extension). PersonName
    // decomposition via write_attr_elem, mirroring the study PN keys.
    if let Some(s) = series_text(&series.attrs, crate::query::tag::OPERATORS_NAME)
        && tag_pass(&filt, 0x0008_1070)
    {
        write_attr_elem(&mut out, 0x0008_1070, &AttrValue::Text(s.to_string()), "PN")?;
    }
    // C attribute RetrieveURL (0008,1190) - conditional serving attr,
    // ascends after the whole (0008,xxxx) block and before (0018,0015).
    // 06.09 field incident: Weasis appends "/instances" to this URL and
    // queries it. In study context the URL is study-scoped; the global
    // series QIDO keeps the series-root form (Weasis-compat route serves
    // it). The XML writers allocate per attribute already, so the combined
    // UID String is consistent with the path.
    let series_uid = series_text(&series.attrs, crate::query::tag::SERIES_INSTANCE_UID);
    if let Some(st) = study_uid {
        if let Some(se) = series_uid {
            let uid = format!("{st}/series/{se}");
            emit_xml_ur(&mut out, retrieve_base, "/dicomweb/studies/", Some(&uid))?;
        }
    } else {
        emit_xml_ur(&mut out, retrieve_base, "/dicomweb/series/", series_uid)?;
    }
    // (0018,0015) BodyPartExamined - CS.
    if let Some(s) = series_text(&series.attrs, crate::query::tag::BODY_PART_EXAMINED)
        && tag_pass(&filt, 0x0018_0015)
    {
        write_attr_elem(&mut out, 0x0018_0015, &AttrValue::Text(s.to_string()), "CS")?;
    }
    // (0018,1030) ProtocolName - LO (08.09 extension, includefield-gated).
    if let Some(s) = series_text(&series.attrs, crate::query::tag::PROTOCOL_NAME)
        && tag_pass(&filt, 0x0018_1030)
    {
        write_attr_elem(&mut out, 0x0018_1030, &AttrValue::Text(s.to_string()), "LO")?;
    }
    // (0020,000E) SeriesInstanceUID - UI (always present).
    if let Some(s) = series_text(&series.attrs, crate::query::tag::SERIES_INSTANCE_UID)
        && tag_pass(&filt, 0x0020_000E)
    {
        write_attr_elem(&mut out, 0x0020_000E, &AttrValue::Text(s.to_string()), "UI")?;
    }
    // (0020,0011) SeriesNumber - IS.
    if let Some(n) = series_number(&series.attrs, crate::query::tag::SERIES_NUMBER)
        && tag_pass(&filt, 0x0020_0011)
    {
        write_attr_elem(&mut out, 0x0020_0011, &AttrValue::UInt(n), "IS")?;
    }
    // (0020,0060) Laterality - CS.
    if let Some(s) = series_text(&series.attrs, crate::query::tag::LATERALITY)
        && tag_pass(&filt, 0x0020_0060)
    {
        write_attr_elem(&mut out, 0x0020_0060, &AttrValue::Text(s.to_string()), "CS")?;
    }
    // (0020,1209) NumberOfSeriesRelatedInstances - IS.
    if tag_pass(&filt, 0x0020_1209) {
        write_attr_elem(
            &mut out,
            0x0020_1209,
            &AttrValue::UInt(series.instances.len() as u64),
            "IS",
        )?;
    }

    Ok(out.len)
}

/// Write one instance's XML attributes into `buf`, returning bytes written.
///
/// Mirrors `write_instance_json`: SOP Class/Instance UIDs, the injected
/// Study/Series Instance UIDs from context, and `InstanceNumber` (0020,0013).
pub fn write_instance_attrs(
    buf: &mut [u8],
    instance: &InstanceRecord,
    study_uid: &str,
    series_uid: &str,
    includefield: Option<&str>,
    retrieve_base: Option<&str>,
) -> Result<usize, StreamOverflow> {
    let filt = TagFilter::from_request(&crate::query::json::MANDATORY_INSTANCE, includefield);
    let mut out = Sink::new(buf);

    // (0008,0016) SOPClassUID - UI, if present.
    if let Some(s) = instance_text(&instance.attrs, crate::query::tag::SOP_CLASS_UID)
        && tag_pass(&filt, 0x0008_0016)
    {
        write_attr_elem(&mut out, 0x0008_0016, &AttrValue::Text(s.to_string()), "UI")?;
    }
    // (0008,0018) SOPInstanceUID - UI (always present).
    if let Some(s) = instance_text(&instance.attrs, crate::query::tag::SOP_INSTANCE_UID)
        && tag_pass(&filt, 0x0008_0018)
    {
        write_attr_elem(&mut out, 0x0008_0018, &AttrValue::Text(s.to_string()), "UI")?;
    }
    // C attributes: InstanceAvailability (0008,0056) always, then
    // RetrieveURL (0008,1190) conditional with the full
    // study/series/instance path. Both live in the (0008,xxxx) group,
    // ascending after SOPInstanceUID (0008,0018) and before (0020,000D).
    emit_xml_ia(&mut out)?;
    if let (Some(base), Some(sop)) = (
        retrieve_base,
        instance_text(&instance.attrs, crate::query::tag::SOP_INSTANCE_UID),
    ) && !study_uid.is_empty()
        && !series_uid.is_empty()
    {
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
        write_attr_elem(&mut out, 0x0008_1190, &AttrValue::Text(s.to_string()), "UR")?;
    }
    // (0020,000D) StudyInstanceUID - UI, injected from context.
    if !study_uid.is_empty() && tag_pass(&filt, 0x0020_000D) {
        write_attr_elem(
            &mut out,
            0x0020_000D,
            &AttrValue::Text(study_uid.to_string()),
            "UI",
        )?;
    }
    // (0020,000E) SeriesInstanceUID - UI, injected from context.
    if !series_uid.is_empty() && tag_pass(&filt, 0x0020_000E) {
        write_attr_elem(
            &mut out,
            0x0020_000E,
            &AttrValue::Text(series_uid.to_string()),
            "UI",
        )?;
    }
    // (0020,0013) InstanceNumber - IS.
    if let Some(n) = series_number(&instance.attrs, crate::query::tag::INSTANCE_NUMBER)
        && tag_pass(&filt, 0x0020_0013)
    {
        write_attr_elem(&mut out, 0x0020_0013, &AttrValue::UInt(n), "IS")?;
    }

    Ok(out.len)
}

// -- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn study(attrs: Vec<(u32, AttrValue)>) -> StudyQido {
        StudyQido {
            attrs,
            ..Default::default()
        }
    }

    #[test]
    fn study_attrs_render_dicomattribute() {
        let s = study(vec![
            (0x0008_0020, AttrValue::UInt(20260802)),
            (0x0008_0060, AttrValue::Text("CT".into())),
            (0x0010_0010, AttrValue::Text("ivanov^ivan".into())),
            (0x0020_000D, AttrValue::Text("1.2.840.1.1".into())),
        ]);
        let mut buf = [0u8; 1024];
        let n = write_study_attrs(&mut buf, &s, None, None).expect("fits");
        let got = String::from_utf8(buf[..n].to_vec()).unwrap();
        assert!(got.contains(
            "<DicomAttribute tag=\"00080020\" vr=\"DA\" keyword=\"\"><Value number=\"1\">20260802</Value></DicomAttribute>"
        ));
        assert!(got.contains("vr=\"CS\""));
        // PN is decomposed.
        assert!(got.contains(
            "<PersonName><Alphabetic><FamilyName>ivanov</FamilyName><GivenName>ivan</GivenName></Alphabetic></PersonName>"
        ));
    }

    #[test]
    fn study_attrs_emit_derived_counts_and_modalities() {
        let mut s = study(vec![(0x0008_0020, AttrValue::UInt(20260802))]);
        s.series_count = 2;
        s.instance_count = 42;
        s.modalities_in_study = Some("CT,MR".into());
        let mut buf = [0u8; 1024];
        let n = write_study_attrs(&mut buf, &s, None, None).expect("fits");
        let got = String::from_utf8(buf[..n].to_vec()).unwrap();
        // Derived counts render as IS elements.
        assert!(got.contains(
            "<DicomAttribute tag=\"00201206\" vr=\"IS\" keyword=\"\"><Value number=\"1\">2</Value></DicomAttribute>"
        ));
        assert!(got.contains(
            "<DicomAttribute tag=\"00201208\" vr=\"IS\" keyword=\"\"><Value number=\"1\">42</Value></DicomAttribute>"
        ));
        // ModalitiesInStudy renders one Value per comma component, in order.
        assert!(got.contains(
            "<DicomAttribute tag=\"00080061\" vr=\"CS\" keyword=\"\"><Value number=\"1\">CT</Value><Value number=\"2\">MR</Value></DicomAttribute>"
        ));
        // ORDER: 00080061 stays in its group-8 slot; the counts come after
        // StudyID and before SeriesInstanceUID (the json.rs:177-178 slot).
        let pos_m = got.find("tag=\"00080061\"").expect("00080061 present");
        let pos_1206 = got.find("tag=\"00201206\"").expect("00201206 present");
        let pos_1208 = got.find("tag=\"00201208\"").expect("00201208 present");
        assert!(pos_m < pos_1206 && pos_1206 < pos_1208);
    }

    #[test]
    fn study_attrs_always_emits_charset_no_series_uid() {
        let s = study(vec![(0x0020_000D, AttrValue::Text("1.2.840.1.1".into()))]);
        let mut buf = [0u8; 512];
        let n = write_study_attrs(&mut buf, &s, None, None).expect("fits");
        let got = String::from_utf8(buf[..n].to_vec()).unwrap();
        // (0008,0005) SpecificCharacterSet is ALWAYS emitted first as the
        // study writer's lowest tag.
        assert!(
            got.contains(
                "<DicomAttribute tag=\"00080005\" vr=\"CS\" keyword=\"\"><Value number=\"1\">ISO_IR 192</Value></DicomAttribute>"
            ),
            "charset must be emitted first: {got}"
        );
        // (0020,000E) SeriesInstanceUID is a study-level ORDER leftover
        // that never matches study attrs.
        assert!(!got.contains("0020000E"));
    }

    #[test]
    fn study_attrs_filtered_keeps_derived_and_requested() {
        let mut s = study(vec![(0x0008_1030, AttrValue::Text("Chest CT".into()))]);
        s.series_count = 3;
        s.instance_count = 7;
        s.modalities_in_study = Some("CT".into());
        let mut buf = [0u8; 1024];
        let n = write_study_attrs(&mut buf, &s, Some("00081030"), None).expect("fits");
        let got = String::from_utf8(buf[..n].to_vec()).unwrap();
        // The mandatory baseline survives includefield - the derived trio
        // is always emitted.
        assert!(got.contains("tag=\"00201206\""));
        assert!(got.contains("tag=\"00201208\""));
        assert!(got.contains("tag=\"00080061\""));
        // The requested non-mandatory tag is emitted alongside the baseline.
        assert!(got.contains("tag=\"00081030\""));
        assert!(got.contains("Chest CT"));
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
        let baseline = crate::query::json::MANDATORY_STUDY;
        let f = TagFilter::from_request(&baseline, Some(&sixty)).expect("filter");
        for t in &baseline {
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
        let baseline = crate::query::json::MANDATORY_STUDY;
        assert!(TagFilter::from_request(&baseline, Some("all")).is_none());
        assert!(TagFilter::from_request(&baseline, Some("ALL")).is_none());
        assert!(TagFilter::from_request(&baseline, Some(" all ")).is_none());
    }

    // The standard defines `all` only as the WHOLE value; in a mixed token
    // list the `all` token is ignored as a non-hex field and the rest still
    // filters.
    #[test]
    fn filter_mixed_all_token_is_skipped() {
        let baseline = crate::query::json::MANDATORY_STUDY;
        let f = TagFilter::from_request(&baseline, Some("all,00081030")).expect("filter");
        assert!(f.contains(0x0008_1030), "requested tag must be present");
        assert!(f.contains(0x0020_000D), "baseline must be present");
    }

    // keyword-form includefield (mirror of the JSON tests). The ABNF
    // attribute is an 8-hex-digit tag or a DICOM keyword; keywords resolve
    // case-insensitively via the shared KEYWORD_TO_TAG table.
    #[test]
    fn filter_keyword_form_resolves_to_tag() {
        let baseline = crate::query::json::MANDATORY_SERIES;
        // PatientName is NOT in the series baseline - a hit proves the
        // keyword resolved to 00100010.
        let f = TagFilter::from_request(&baseline, Some("PatientName")).expect("filter");
        assert!(
            f.contains(0x0010_0010),
            "PatientName must resolve to 00100010"
        );
        let f2 = TagFilter::from_request(&baseline, Some("patientname")).expect("filter");
        assert!(
            f2.contains(0x0010_0010),
            "keyword lookup is case-insensitive"
        );
        // An 8-character keyword is not 8 hex digits - the old len==8 gate
        // would have dropped it silently.
        let study_baseline = crate::query::json::MANDATORY_STUDY;
        let f3 = TagFilter::from_request(&study_baseline, Some("Modality")).expect("filter");
        assert!(f3.contains(0x0008_0060), "Modality (8 chars) must resolve");
        // Unknown keyword -> skipped; the baseline survives.
        let f4 = TagFilter::from_request(&baseline, Some("NoSuchKeyword")).expect("filter");
        assert!(
            f4.contains(0x0020_000E),
            "baseline must survive unknown keyword"
        );
        assert!(!f4.contains(0x0010_0010));
    }

    // Hex and keyword forms mix in a single includefield value.
    #[test]
    fn filter_hex_keyword_mix() {
        let baseline = crate::query::json::MANDATORY_SERIES;
        let f = TagFilter::from_request(&baseline, Some("00200011,SeriesDate")).expect("filter");
        assert!(f.contains(0x0020_0011), "hex tag present");
        assert!(f.contains(0x0008_0021), "keyword tag present");
    }

    // Writer level: `all` and `None` produce byte-identical study output.
    #[test]
    fn write_study_attrs_all_keyword_matches_none() {
        let s = study(vec![
            (0x0008_0020, AttrValue::UInt(20260802)),
            (0x0008_0060, AttrValue::Text("CT".into())),
            (0x0010_0010, AttrValue::Text("ivanov".into())),
            (0x0020_000D, AttrValue::Text("1.2.840.1.1".into())),
        ]);
        let mut buf_a = [0u8; 1024];
        let mut buf_b = [0u8; 1024];
        let a = write_study_attrs(&mut buf_a, &s, Some("all"), None).expect("fits");
        let b = write_study_attrs(&mut buf_b, &s, None, None).expect("fits");
        assert_eq!(&buf_a[..a], &buf_b[..b]);
    }

    #[test]
    fn patient_xml_matches_legacy_render() {
        use crate::query::json::PatientQido;
        let p = PatientQido {
            patient_id: 7,
            patient_uid: "PT-UID-9".into(),
            patient_name: "SMIRNOV^IVAN".into(),
            last_arrival: Some(500),
            study_count: 2,
        };
        let mut buf = [0u8; 1024];
        let n = write_patient_xml(&mut buf, &p).expect("fits");
        // Patient XML now leads with the always-on SpecificCharacterSet
        // (0008,0005) sibling, then the two DICOM-tag attributes
        // back-to-back; non-DICOM keys dropped.
        assert_eq!(
            &buf[..n],
            b"<DicomAttribute tag=\"00080005\" vr=\"CS\" keyword=\"\"><Value number=\"1\">ISO_IR 192</Value></DicomAttribute><DicomAttribute tag=\"00100010\" vr=\"PN\" keyword=\"\"><PersonName><Alphabetic><FamilyName>SMIRNOV</FamilyName><GivenName>IVAN</GivenName></Alphabetic></PersonName></DicomAttribute><DicomAttribute tag=\"00100020\" vr=\"LO\" keyword=\"\"><Value number=\"1\">PT-UID-9</Value></DicomAttribute>"
        );
    }

    #[test]
    fn xml_escaping() {
        let s = study(vec![(0x0010_0010, AttrValue::Text("a&b<c>d\"e'".into()))]);
        let mut buf = [0u8; 1024];
        let n = write_study_attrs(&mut buf, &s, None, None).expect("fits");
        let got = String::from_utf8(buf[..n].to_vec()).unwrap();
        assert!(got.contains("a&amp;b&lt;c&gt;d&quot;e&apos;"));
    }

    #[test]
    fn underflow_reports_overflow() {
        let s = study(vec![(0x0010_0010, AttrValue::Text("ivanov".into()))]);
        let mut tiny = [0u8; 8];
        assert_eq!(
            write_study_attrs(&mut tiny, &s, None, None),
            Err(StreamOverflow)
        );
    }

    #[test]
    fn doc_wraps_native_dicom_model() {
        let mut body = Vec::new();
        write_xml_doc_start(&mut body);
        let s = study(vec![(0x0020_000D, AttrValue::Text("1.2.840.1.1".into()))]);
        let mut buf = [0u8; 512];
        let n = write_study_attrs(&mut buf, &s, None, None).expect("fits");
        body.extend_from_slice(&buf[..n]);
        write_xml_doc_end(&mut body);
        let got = String::from_utf8(body).unwrap();
        assert!(got.starts_with(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<NativeDicomModel xmlns=\"http://dicom.nema.org/PS3.19/models/NativeDICOM\">\n"
        ));
        assert!(got.ends_with("\n</NativeDicomModel>\n"));
    }
}
