// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Study-level CBOR manifest builder - the STOW write side.
//!
//! For B-tree to answer QIDO, every accepted study must land a CBOR manifest
//! in `manifests/{study_instance_uid}.manifest` (the query engine's source of
//! truth). This module builds that manifest from the study-level fields the
//! STOW pipeline already extracts (the `InstanceMeta` subset), writes it
//! atomically (tmp + rename), and verifies the round-trip.
//!
//! Only the study-level QIDO attributes are populated here. Series, instances
//! and frame chunks are written by the full-hierarchy manifest path (its
//! recovery manifest); study-level QIDO, which is what the engine's
//! `QueryFilter` matches, needs only this level. The `series` array is left
//! empty deliberately - the query engine never descends into it for study
//! search.

use crate::manifest::{
    AttrValue, FrameRecord, InstanceRecord, Manifest, SeriesRecord, StudyQido, SysHeader, TIER_HOT,
};
use crate::query::tag;
use std::io;
use std::path::{Path, PathBuf};

/// The study-level DICOM attributes carried by a STOW-accepted study.
///
/// Mirrors the fields of the server's `InstanceMeta` that are study search
/// keys (PS3.18 Table 8.3.3-1). Optional text fields are stored only when
/// present; empty values are omitted so a missing search key never matches a
/// stray empty string.
#[derive(Debug, Clone, Default)]
pub struct StudyManifestAttrs {
    pub patient_name: Option<String>,
    pub patient_id: Option<String>,
    /// IssuerOfPatientID (0010,0021) - LO.
    ///
    /// Captured so the STOW-triggered workitem can state the issuer AIW-I
    /// 4.80.4.1.2.1 requires for every Patient ID it carries. Sorts between
    /// PatientID and PatientBirthDate.
    pub patient_id_issuer: Option<String>,
    pub patient_birth_date: Option<String>,
    pub patient_sex: Option<String>,
    pub study_instance_uid: String,
    pub study_date: Option<String>,
    pub study_time: Option<String>,
    pub study_description: Option<String>,
    pub accession_number: Option<String>,
    pub referring_physician: Option<String>,
    pub performing_physician: Option<String>,
    pub name_of_reading_physicians: Option<String>,
    /// InstitutionName (0008,0080) - LO. 08.09 capture + study search key.
    pub institution_name: Option<String>,
    /// RequestingPhysician (0032,1032) - PN. 08.09 capture + study key.
    pub requesting_physician: Option<String>,
    /// RequestedProcedureDescription (0032,1060) - LO. 08.09 capture + key.
    pub requested_procedure_description: Option<String>,
    /// ResponsiblePerson (0010,2297) - PN veterinary search key (FR-vet stage 1).
    pub responsible_person: Option<String>,
    pub modality: String,
    pub study_id: Option<String>,
    /// NumberOfStudyRelatedSeries (0020,1206) - the series count, carried to
    /// the RAM study index at STOW-commit time. RAM-only, never persisted.
    pub num_series: u64,
    /// NumberOfStudyRelatedInstances (0020,1208) - the instance count, carried
    /// to the RAM study index at STOW-commit time. RAM-only, never persisted.
    pub num_instances: u64,
    /// ModalitiesInStudy (0008,0061) - distinct series modalities in
    /// first-occurrence order, comma-joined. Carried to the RAM study index at
    /// STOW-commit time. RAM-only, never persisted.
    pub modalities_in_study: Option<String>,
    /// OtherPatientIDsSequence (0010,1002) > PatientID (0010,0020) -
    /// comma-joined, in sequence order.
    ///
    /// MULTI-VALUED, mirroring `modalities_in_study`: the sequence lists every
    /// other identifier the patient is known by, and PS3.18 reaches any of
    /// them through the path key `00101002.00100020`. Stored comma-joined
    /// because the key asks whether ANY listed identifier matches, which is
    /// the set comparison those two fields share.
    pub other_patient_ids: Option<String>,
}

/// Build the `(tag, AttrValue)` study-level attribute list in DICOM tag order.
///
/// Values are normalized to the forms the query filter compares: dates are
/// kept as `UInt` (they are stored and matched as `YYYYMMDD`), everything else
/// as `Text`. This matches how the SQLite baseline stored and compared them,
/// so filtered results agree byte-for-byte.
pub fn study_attrs(a: &StudyManifestAttrs) -> Vec<(u32, AttrValue)> {
    let mut v: Vec<(u32, AttrValue)> = Vec::with_capacity(13);
    // DICOM tags are emitted in ascending group/element order so the manifest
    // satisfies the binary-search contract the decoder relies on.
    if let Some(d) = a.study_date.as_deref() {
        push_date(&mut v, tag::STUDY_DATE, d);
    }
    if let Some(s) = a.study_time.as_deref()
        && !s.is_empty()
    {
        v.push((tag::STUDY_TIME, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.accession_number.as_deref()
        && !s.is_empty()
    {
        v.push((tag::ACCESSION_NUMBER, AttrValue::Text(s.to_string())));
    }
    if !a.modality.is_empty() {
        v.push((tag::MODALITY, AttrValue::Text(a.modality.clone())));
    }
    // 08.09: InstitutionName (0008,0080) sorts between Modality (0008,0060)
    // and ReferringPhysician (0008,0090) - the ascending-tag contract.
    if let Some(s) = a.institution_name.as_deref()
        && !s.is_empty()
    {
        v.push((tag::INSTITUTION_NAME, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.referring_physician.as_deref()
        && !s.is_empty()
    {
        v.push((tag::REFERRING_PHYSICIAN, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.study_description.as_deref()
        && !s.is_empty()
    {
        v.push((tag::STUDY_DESCRIPTION, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.performing_physician.as_deref()
        && !s.is_empty()
    {
        v.push((tag::PERFORMING_PHYSICIAN, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.name_of_reading_physicians.as_deref()
        && !s.is_empty()
    {
        v.push((
            tag::NAME_OF_READING_PHYSICIANS,
            AttrValue::Text(s.to_string()),
        ));
    }
    if let Some(s) = a.patient_name.as_deref()
        && !s.is_empty()
    {
        v.push((tag::PATIENT_NAME, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.patient_id.as_deref()
        && !s.is_empty()
    {
        v.push((tag::PATIENT_ID, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.patient_id_issuer.as_deref()
        && !s.is_empty()
    {
        v.push((tag::PATIENT_ID_ISSUER, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.patient_birth_date.as_deref() {
        push_date(&mut v, tag::PATIENT_BIRTH_DATE, s);
    }
    if let Some(s) = a.patient_sex.as_deref()
        && !s.is_empty()
    {
        v.push((tag::PATIENT_SEX, AttrValue::Text(s.to_string())));
    }
    // OtherPatientIDsSequence (0010,1002) sorts after PatientSex (0010,0040)
    // and before ResponsiblePerson (0010,2297), keeping the ascending-tag
    // contract. It is stored as one comma-joined scalar, the same shape
    // ModalitiesInStudy uses, because the key's semantics is set intersection.
    if let Some(s) = a.other_patient_ids.as_deref()
        && !s.is_empty()
    {
        v.push((tag::OTHER_PATIENT_IDS, AttrValue::Text(s.to_string())));
    }
    // FR-vet stage 1: ResponsiblePerson (0010,2297) is a PN attribute of the
    // 0010,22xx block. It sorts AFTER PatientSex (0010,0040) and BEFORE
    // StudyInstanceUID (0020,000D), preserving the ascending-tag invariant
    // (binary-search contract of the decoder).
    if let Some(s) = a.responsible_person.as_deref()
        && !s.is_empty()
    {
        v.push((tag::RESPONSIBLE_PERSON, AttrValue::Text(s.to_string())));
    }
    if !a.study_instance_uid.is_empty() {
        v.push((
            tag::STUDY_INSTANCE_UID,
            AttrValue::Text(a.study_instance_uid.clone()),
        ));
    }
    if let Some(s) = a.study_id.as_deref()
        && !s.is_empty()
    {
        v.push((tag::STUDY_ID, AttrValue::Text(s.to_string())));
    }
    // 08.09: the 0032 pair sorts after the whole 0020 group (ascending-tag
    // contract): RequestingPhysician (0032,1032) then
    // RequestedProcedureDescription (0032,1060).
    if let Some(s) = a.requesting_physician.as_deref()
        && !s.is_empty()
    {
        v.push((tag::REQUESTING_PHYSICIAN, AttrValue::Text(s.to_string())));
    }
    if let Some(s) = a.requested_procedure_description.as_deref()
        && !s.is_empty()
    {
        v.push((
            tag::REQUESTED_PROCEDURE_DESCRIPTION,
            AttrValue::Text(s.to_string()),
        ));
    }
    v
}

/// Restamp an incoming [`StudyManifestAttrs`] from the MERGED manifest's
/// study record. Used by the STOW commit path so the RAM study index always
/// learns the disk truth: an incoming derived object (PR/SEG) carries no
/// patient identity, and feeding its coerced placeholders to the scanner
/// re-attributed a live study to `AUTOPAT_` on the 17.09 clinfer rehearsal.
pub fn restamp_from_manifest(a: &mut StudyManifestAttrs, m: &Manifest) {
    // Steady-state discipline: another instance of the SAME study with
    // identical identity must allocate NOTHING here. Text fields compare as
    // borrowed strings and date fields render into a stack buffer, so only
    // a real divergence (a derived object whose placeholders differ from the
    // archived truth) clones.
    let find = |t: u32| m.study.attrs.iter().find(|(x, _)| *x == t).map(|(_, v)| v);
    let same = |t: u32, want: Option<&str>| match find(t) {
        Some(AttrValue::Text(s)) => want == Some(s.as_str()),
        Some(AttrValue::UInt(n)) => {
            let mut buf = [0u8; 24];
            let mut v = *n;
            let mut i = buf.len();
            loop {
                i -= 1;
                buf[i] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            want == Some(std::str::from_utf8(&buf[i..]).unwrap_or(""))
        }
        _ => false,
    };
    let set = |slot: &mut Option<String>, t: u32| {
        if !same(t, slot.as_deref()) {
            *slot = match find(t) {
                Some(AttrValue::Text(s)) => Some(s.clone()),
                Some(AttrValue::UInt(n)) => Some(n.to_string()),
                _ => None,
            };
        }
    };
    set(&mut a.patient_name, tag::PATIENT_NAME);
    set(&mut a.patient_id, tag::PATIENT_ID);
    set(&mut a.patient_id_issuer, tag::PATIENT_ID_ISSUER);
    set(&mut a.patient_birth_date, tag::PATIENT_BIRTH_DATE);
    set(&mut a.patient_sex, tag::PATIENT_SEX);
    set(&mut a.study_date, tag::STUDY_DATE);
    set(&mut a.study_time, tag::STUDY_TIME);
    set(&mut a.study_description, tag::STUDY_DESCRIPTION);
    set(&mut a.accession_number, tag::ACCESSION_NUMBER);
    set(&mut a.referring_physician, tag::REFERRING_PHYSICIAN);
    set(&mut a.performing_physician, tag::PERFORMING_PHYSICIAN);
    set(
        &mut a.name_of_reading_physicians,
        tag::NAME_OF_READING_PHYSICIANS,
    );
    set(&mut a.institution_name, tag::INSTITUTION_NAME);
    set(&mut a.requesting_physician, tag::REQUESTING_PHYSICIAN);
    set(
        &mut a.requested_procedure_description,
        tag::REQUESTED_PROCEDURE_DESCRIPTION,
    );
    set(&mut a.responsible_person, tag::RESPONSIBLE_PERSON);
    set(&mut a.study_id, tag::STUDY_ID);
    if !same(tag::MODALITY, Some(a.modality.as_str())) {
        a.modality = match find(tag::MODALITY) {
            Some(AttrValue::Text(s)) => s.clone(),
            _ => String::new(),
        };
    }
    if !same(tag::STUDY_INSTANCE_UID, Some(a.study_instance_uid.as_str())) {
        a.study_instance_uid = match find(tag::STUDY_INSTANCE_UID) {
            Some(AttrValue::Text(s)) => s.clone(),
            _ => String::new(),
        };
    }
}

/// Push a `YYYYMMDD` attribute as a numeric `UInt` when it is 8 ASCII digits.
fn push_date(v: &mut Vec<(u32, AttrValue)>, t: u32, s: &str) {
    if s.len() == 8
        && s.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = s.parse::<u64>()
    {
        v.push((t, AttrValue::UInt(n)));
    }
}

/// The WADO-facing data of a single instance, carried by a STOW accept.
///
/// This is what lets WADO resolve and serve an instance purely from the
/// manifest (no SQLite): the CAS address (BLAKE3), the storage tier, the
/// transfer syntax, and the file size.
///
/// Field order is a cache-line layout: blake3 lands at 96 and
/// ends exactly at 128; the multi-set (4 x 24B + 32B + 8B + 1B) provably
/// cannot reach ZERO straddles (brute force over all 5040 permutations:
/// minimum = 1), so this order achieves the minimum - instance_number@48
/// is the single BUDGETED straddle, pinned by the ap101 cacheline gate.
/// Semantics-free - all construction is by field name and the model is
/// converted to attr lists by field name.
#[derive(Debug, Clone, Default)]
pub struct InstanceModel {
    /// SOP Instance UID (0008,0018) - the WADO lookup key.
    pub sop_instance_uid: String,
    /// SOP Class UID (0008,0016) - the instance's service class.
    pub sop_class_uid: Option<String>,
    /// Instance Number (0020,0013) - the instance sort key, kept as
    /// text for display parity with the existing series_number.
    pub instance_number: Option<String>,
    /// Transfer syntax UID of the stored instance (4th: fills the 72..96
    /// slot so blake3 starts at 96).
    pub transfer_syntax_uid: String,
    /// Full-file BLAKE3 hash - the CAS address.
    pub blake3: [u8; 32],
    /// Stored `.dcm` size in bytes.
    pub file_size: u64,
    /// Storage tier of the stored file.
    pub storage_tier: u8,
    /// Referenced Series Instance UIDs, captured at STOW for a derived object
    /// (a presentation state or a segmentation). Empty for an ordinary image.
    ///
    /// Captured rather than parsed on demand: at STOW the referenced objects
    /// have just been stored, so the references resolve, and the deletion
    /// path can then read where an object points without reading DICOM bytes.
    pub referenced_series_uids: Vec<String>,
    /// Referenced SOP Instance UIDs, captured at STOW under the same rule.
    pub referenced_sop_instance_uids: Vec<String>,
    /// Frame-chunk table: one entry per frame, each carrying the frame's
    /// byte range inside the stored `.dcm`. Filled at STOW (and re-filled on
    /// rebuild) so `/frames/{N}` serves a bounded byte range instead of
    /// buffering the whole instance. Empty means "serve the whole blob" -
    /// the pre-existing path, kept for single-frame instances and for rows
    /// written before the table existed.
    pub frames: Vec<FrameRecord>,
}

/// A series to embed in the manifest, with its instances.
#[derive(Debug, Clone, Default)]
pub struct SeriesModel {
    /// Series Instance UID (0020,000E).
    pub series_instance_uid: String,
    /// Modality (0008,0060).
    pub modality: String,
    /// Series Number (0020,0011), kept as text for sorting / display.
    pub series_number: Option<String>,
    /// Series Date (0008,0021), `YYYYMMDD`.
    pub series_date: Option<String>,
    /// Series Time (0008,0031).
    pub series_time: Option<String>,
    /// Performed Procedure Step Start Date (0040,0244), `YYYYMMDD`.
    pub pps_start_date: Option<String>,
    /// Performed Procedure Step Start Time (0040,0245).
    pub pps_start_time: Option<String>,
    /// RequestAttributesSequence (0040,0275) > ScheduledProcedureStepID
    /// (0040,0009) - SH.
    ///
    /// PS3.18 Table 10.6.1-5 lists `00400275.00400009` as a REQUIRED series
    /// matching attribute, so the value has to be stored for the key to be a
    /// real filter rather than an accepted no-op.
    pub scheduled_procedure_step_id: Option<String>,
    /// RequestAttributesSequence (0040,0275) > RequestedProcedureID
    /// (0040,1001) - SH. The second REQUIRED nested key of the same table,
    /// read from the same sequence item.
    pub requested_procedure_id: Option<String>,
    /// Station Name (0008,1010).
    pub station_name: Option<String>,
    /// Manufacturer (0008,0070). 08.09 capture + series search key.
    pub manufacturer: Option<String>,
    /// InstitutionalDepartmentName (0008,1040). 08.09 capture + series key.
    pub institutional_department_name: Option<String>,
    /// OperatorsName (0008,1070). 08.09 capture + series search key.
    pub operators_name: Option<String>,
    /// Series Description (0008,103E).
    pub series_description: Option<String>,
    /// Body Part Examined (0018,0015).
    pub body_part_examined: Option<String>,
    /// ProtocolName (0018,1030). 08.09 capture + series search key.
    pub protocol_name: Option<String>,
    /// Laterality (0020,0060).
    pub laterality: Option<String>,
    /// Child instances, in STOW order.
    pub instances: Vec<InstanceModel>,
}

/// Build the instance DICOM attribute list in ascending tag order.
///
/// SOP Instance UID is always present (the WADO lookup key); SOP Class UID
/// and Instance Number are captured when known. Ascending order
/// preserves the manifest decoder's binary-search contract.
fn instance_attrs(m: &InstanceModel) -> Vec<(u32, AttrValue)> {
    let mut attrs: Vec<(u32, AttrValue)> = Vec::with_capacity(5);
    if let Some(u) = m.sop_class_uid.as_deref()
        && !u.is_empty()
    {
        attrs.push((tag::SOP_CLASS_UID, AttrValue::Text(u.to_string())));
    }
    attrs.push((
        tag::SOP_INSTANCE_UID,
        AttrValue::Text(m.sop_instance_uid.clone()),
    ));
    if let Some(n) = m.instance_number.as_deref()
        && !n.is_empty()
    {
        attrs.push((tag::INSTANCE_NUMBER, AttrValue::Text(n.to_string())));
    }
    // The captured references go last. Their keys are above every DICOM tag,
    // so they sort after the entries above and the ascending contract holds
    // without a sort pass.
    if !m.referenced_series_uids.is_empty() {
        attrs.push((
            tag::REFERENCED_SERIES_UIDS,
            AttrValue::Array(
                m.referenced_series_uids
                    .iter()
                    .map(|uid| AttrValue::Text(uid.clone()))
                    .collect(),
            ),
        ));
    }
    if !m.referenced_sop_instance_uids.is_empty() {
        attrs.push((
            tag::REFERENCED_SOP_INSTANCE_UIDS,
            AttrValue::Array(
                m.referenced_sop_instance_uids
                    .iter()
                    .map(|uid| AttrValue::Text(uid.clone()))
                    .collect(),
            ),
        ));
    }
    attrs
}

/// Build an instance record from the STOW-extracted model.
fn instance_record(m: &InstanceModel) -> InstanceRecord {
    InstanceRecord {
        attrs: instance_attrs(m),
        transfer_syntax: m.transfer_syntax_uid.clone(),
        storage_tier: m.storage_tier,
        blake3: m.blake3,
        file_size: m.file_size,
        // Frame-chunk table as computed at STOW (empty = serve the whole
        // blob, the pre-existing path). The BLAKE3 per-frame hash is left
        // zero: nothing reads it yet, and hashing every tile at STOW would
        // add a full pass over a multi-GB instance for no consumer.
        frames: m.frames.clone(),
        // A freshly stored instance is visible. Only an IOCM/AIRA rejection
        // note (a later KOS STOW) ever records a reason.
        rejection_reason: crate::manifest::NOT_REJECTED_REASON,
    }
}

/// Build the series DICOM attribute list in ascending tag order.
///
/// Only non-empty values are emitted (a missing search key never matches a
/// stray empty string). Ascending order preserves the decoder's
/// binary-search contract.
fn series_attrs(s: &SeriesModel) -> Vec<(u32, AttrValue)> {
    let mut attrs: Vec<(u32, AttrValue)> = Vec::with_capacity(11);
    if let Some(d) = s.series_date.as_deref()
        && !d.is_empty()
    {
        push_date(&mut attrs, tag::SERIES_DATE, d);
    }
    if let Some(t) = s.series_time.as_deref()
        && !t.is_empty()
    {
        attrs.push((tag::SERIES_TIME, AttrValue::Text(t.to_string())));
    }
    if !s.modality.is_empty() {
        attrs.push((tag::MODALITY, AttrValue::Text(s.modality.clone())));
    }
    // 08.09 ascending-tag contract: Manufacturer (0008,0070) sorts between
    // Modality (0008,0060) and StationName (0008,1010).
    if let Some(m) = s.manufacturer.as_deref()
        && !m.is_empty()
    {
        attrs.push((tag::MANUFACTURER, AttrValue::Text(m.to_string())));
    }
    if let Some(name) = s.station_name.as_deref()
        && !name.is_empty()
    {
        attrs.push((tag::STATION_NAME, AttrValue::Text(name.to_string())));
    }
    if let Some(d) = s.series_description.as_deref()
        && !d.is_empty()
    {
        attrs.push((tag::SERIES_DESCRIPTION, AttrValue::Text(d.to_string())));
    }
    // 08.09 ascending-tag contract: InstitutionalDepartmentName (0008,1040)
    // sorts between SeriesDescription (0008,103E) and OperatorsName
    // (0008,1070), which precedes BodyPartExamined (0018,0015).
    if let Some(dn) = s.institutional_department_name.as_deref()
        && !dn.is_empty()
    {
        attrs.push((
            tag::INSTITUTIONAL_DEPARTMENT_NAME,
            AttrValue::Text(dn.to_string()),
        ));
    }
    // 08.09 ascending-tag contract: OperatorsName (0008,1070) is the highest
    // 0008-group captured series tag - AFTER InstitutionalDepartmentName
    // (0008,1040) and BEFORE BodyPartExamined (0018,0015).
    if let Some(o) = s.operators_name.as_deref()
        && !o.is_empty()
    {
        attrs.push((tag::OPERATORS_NAME, AttrValue::Text(o.to_string())));
    }
    if let Some(b) = s.body_part_examined.as_deref()
        && !b.is_empty()
    {
        attrs.push((tag::BODY_PART_EXAMINED, AttrValue::Text(b.to_string())));
    }
    // 08.09 ascending-tag contract: ProtocolName (0018,1030) sorts between
    // BodyPartExamined (0018,0015) and SeriesInstanceUID (0020,000E).
    if let Some(p) = s.protocol_name.as_deref()
        && !p.is_empty()
    {
        attrs.push((tag::PROTOCOL_NAME, AttrValue::Text(p.to_string())));
    }
    if !s.series_instance_uid.is_empty() {
        attrs.push((
            tag::SERIES_INSTANCE_UID,
            AttrValue::Text(s.series_instance_uid.clone()),
        ));
    }
    if let Some(n) = s.series_number.as_deref()
        && !n.is_empty()
    {
        attrs.push((tag::SERIES_NUMBER, AttrValue::Text(n.to_string())));
    }
    if let Some(l) = s.laterality.as_deref()
        && !l.is_empty()
    {
        attrs.push((tag::LATERALITY, AttrValue::Text(l.to_string())));
    }
    // The (0040,0275) nested scalars are emitted under their OWN tags
    // (0040,0009, 0040,1001), not under the sequence tag: the manifest holds
    // scalars, and the query layer rebuilds the path from the key the client
    // sends. Ascending order puts (0040,0009) before the PPS pair
    // (0040,0244/0040,0245) and (0040,1001) after them, which is the
    // binary-search contract of the decoder.
    if let Some(v) = s.scheduled_procedure_step_id.as_deref()
        && !v.is_empty()
    {
        attrs.push((
            tag::SCHEDULED_PROCEDURE_STEP_ID,
            AttrValue::Text(v.to_string()),
        ));
    }
    if let Some(d) = s.pps_start_date.as_deref()
        && !d.is_empty()
    {
        push_date(&mut attrs, tag::PPS_START_DATE, d);
    }
    if let Some(t) = s.pps_start_time.as_deref()
        && !t.is_empty()
    {
        attrs.push((tag::PPS_START_TIME, AttrValue::Text(t.to_string())));
    }
    if let Some(v) = s.requested_procedure_id.as_deref()
        && !v.is_empty()
    {
        attrs.push((tag::REQUESTED_PROCEDURE_ID, AttrValue::Text(v.to_string())));
    }
    attrs
}

/// Build a series record with its instance records.
fn series_record(s: &SeriesModel) -> SeriesRecord {
    SeriesRecord {
        attrs: series_attrs(s),
        instances: s.instances.iter().map(instance_record).collect(),
    }
}

/// Merge new STOW instances into an existing manifest's series tree.
///
/// - Existing series (matched by SeriesInstanceUID): new instances are
///   appended; duplicates (same SOPInstanceUID) are silently skipped so
///   repeated STOWs of the same instance are idempotent.
/// - New series: the whole series record is appended.
/// - `sys.total_pixel_bytes` is recomputed from all instances after the merge
///   so the header stays accurate without per-call delta tracking.
fn merge_series_into(manifest: &mut Manifest, new_series: &[SeriesModel]) {
    for new_s in new_series {
        // Find an existing series with the same SeriesInstanceUID.
        let existing = manifest.series.iter_mut().find(|s| {
            s.attrs.iter().any(|(t, v)| {
                *t == tag::SERIES_INSTANCE_UID
                    && matches!(v, AttrValue::Text(uid) if uid == &new_s.series_instance_uid)
            })
        });

        if let Some(existing_s) = existing {
            // Series already present - merge instances, skip duplicates.
            for new_inst in &new_s.instances {
                let is_dup = existing_s.instances.iter().any(|i| {
                    i.attrs.iter().any(|(t, v)| {
                        *t == tag::SOP_INSTANCE_UID
                            && matches!(v, AttrValue::Text(uid) if uid == &new_inst.sop_instance_uid)
                    })
                });
                if !is_dup {
                    existing_s.instances.push(instance_record(new_inst));
                }
            }
        } else {
            // New series - append the whole record.
            manifest.series.push(series_record(new_s));
        }
    }

    // Recompute the total-byte counter from all instances.
    manifest.sys.total_pixel_bytes = manifest
        .series
        .iter()
        .flat_map(|s| &s.instances)
        .map(|i| i.file_size)
        .sum();
}

/// Update-merge new STOW instances into an existing manifest's series tree
/// (2808 update-in-place): an instance whose SOPInstanceUID already exists is
/// REPLACED (new blake3 / file_size / transfer syntax / attrs) instead of
/// skipped - the archive re-points to the replacement blob. Series-level attrs
/// are enriched in place (missing capture tags added, existing values kept);
/// unknown series/instances are appended exactly like [`merge_series_into`].
fn replace_series_into(manifest: &mut Manifest, new_series: &[SeriesModel]) {
    for new_s in new_series {
        let existing = manifest.series.iter_mut().find(|s| {
            s.attrs.iter().any(|(t, v)| {
                *t == tag::SERIES_INSTANCE_UID
                    && matches!(v, AttrValue::Text(uid) if uid == &new_s.series_instance_uid)
            })
        });

        if let Some(existing_s) = existing {
            merge_attrs(&mut existing_s.attrs, series_attrs(new_s));
            for new_inst in &new_s.instances {
                let existing_i = existing_s.instances.iter_mut().find(|i| {
                    i.attrs.iter().any(|(t, v)| {
                        *t == tag::SOP_INSTANCE_UID
                            && matches!(v, AttrValue::Text(uid) if uid == &new_inst.sop_instance_uid)
                    })
                });
                match existing_i {
                    Some(slot) => *slot = instance_record(new_inst),
                    None => existing_s.instances.push(instance_record(new_inst)),
                }
            }
        } else {
            manifest.series.push(series_record(new_s));
        }
    }

    manifest.sys.total_pixel_bytes = manifest
        .series
        .iter()
        .flat_map(|s| &s.instances)
        .map(|i| i.file_size)
        .sum();
}

/// Rebuild the series/instance capture attributes of a study manifest from
/// freshly extracted models, leaving the system header and study-level QIDO
/// attributes untouched.
///
/// An existing series (matched by SeriesInstanceUID) and instance (matched by
/// SOPInstanceUID) are **enriched in place**: any capture tag missing from the
/// old record is added, existing values are kept. Series or instances the old
/// manifest does not have are appended. This is the backward-compat
/// enrichment path for historical manifests written before the capture
/// expansion: decoding alone is safe (attributes are additive and sort last),
/// and rewriting here only ever fills missing tags.
///
/// The system header - critically `arrived_at` - is preserved verbatim, so a
/// historical rebuild never collapses the "recent patients" ordering
/// (E-014). The `total_pixel_bytes` counter is likewise left as the old
/// manifest reported it; this function is enrichment-only, not a byte recount.
///
/// Reusable by design: (`clarus check` + cas->manifests rebuild) drives
/// this with models re-derived from the stored DICOM and calls this one public
/// function rather than duplicating the capture mapping.
pub fn rebuild_series_attrs(manifest: &mut Manifest, series: &[SeriesModel]) {
    for new_s in series {
        let fresh_series_attrs = series_attrs(new_s);
        // Find an existing series with the same SeriesInstanceUID.
        let existing = manifest.series.iter_mut().find(|s| {
            s.attrs.iter().any(|(t, v)| {
                *t == tag::SERIES_INSTANCE_UID
                    && matches!(v, AttrValue::Text(uid) if uid == &new_s.series_instance_uid)
            })
        });

        if let Some(existing_s) = existing {
            merge_attrs(&mut existing_s.attrs, fresh_series_attrs);
            for new_inst in &new_s.instances {
                let fresh_inst_attrs = instance_attrs(new_inst);
                // Find the matching instance by its SOP Instance UID.
                let existing_i = existing_s.instances.iter_mut().find(|i| {
                    i.attrs.iter().any(|(t, v)| {
                        *t == tag::SOP_INSTANCE_UID
                            && matches!(v, AttrValue::Text(uid) if uid == &new_inst.sop_instance_uid)
                    })
                });
                match existing_i {
                    Some(i) => merge_attrs(&mut i.attrs, fresh_inst_attrs),
                    None => existing_s.instances.push(instance_record(new_inst)),
                }
            }
        } else {
            // New series - append the whole record.
            manifest.series.push(series_record(new_s));
        }
    }
}

/// Merge `fresh` attribute pairs into `target`, adding only tags that are not
/// already present (existing values win). Both are sorted by tag, so the
/// merge is a linear two-pointer union preserving ascending order.
fn merge_attrs(target: &mut Vec<(u32, AttrValue)>, fresh: Vec<(u32, AttrValue)>) {
    let mut out: Vec<(u32, AttrValue)> = Vec::with_capacity(target.len() + fresh.len());
    let mut t = 0usize;
    let mut f = 0usize;
    while t < target.len() && f < fresh.len() {
        match target[t].0.cmp(&fresh[f].0) {
            std::cmp::Ordering::Less => {
                out.push(target[t].clone());
                t += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(fresh[f].clone());
                f += 1;
            }
            std::cmp::Ordering::Equal => {
                // Existing value wins; the fresh capture tag is a no-op.
                out.push(target[t].clone());
                t += 1;
                f += 1;
            }
        }
    }
    out.extend(target[t..].iter().cloned());
    out.extend(fresh[f..].iter().cloned());
    *target = out;
}

/// Unix epoch (seconds) now - the E-014 ingestion timestamp.
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a full study manifest from study attrs plus the series/instance tree.
///
/// The `sys` header reports the first instance's storage tier and the sum of
/// all instance file sizes, so WADO and recovery know where fresh data lives
/// without descending into the series array.
pub fn build_manifest_full(a: &StudyManifestAttrs, series: &[SeriesModel]) -> Manifest {
    let mut storage_tier = TIER_HOT;
    let mut total_bytes = 0u64;
    for s in series {
        for inst in &s.instances {
            storage_tier = inst.storage_tier;
            total_bytes += inst.file_size;
        }
    }
    Manifest {
        sys: SysHeader {
            version: 1,
            storage_tier,
            total_pixel_bytes: total_bytes,
            arrived_at: now_epoch(),
        },
        study: StudyQido {
            attrs: study_attrs(a),
            // Derived counts + ModalitiesInStudy (RAM-only, never persisted to
            // CBOR) - set from the series/instance tree this build carries so
            // in-memory consumers (and the merge stamp in
            // `write_study_manifest_full`) see them.
            series_count: series.len() as u64,
            instance_count: series
                .iter()
                .map(|s| s.instances.len() as u64)
                .fold(0u64, |acc, n| acc.saturating_add(n)),
            modalities_in_study: {
                let mut agg = String::new();
                for s in series {
                    if s.modality.is_empty() {
                        continue;
                    }
                    if !agg.split(',').any(|x| x == s.modality) {
                        if !agg.is_empty() {
                            agg.push(',');
                        }
                        agg.push_str(&s.modality);
                    }
                }
                if agg.is_empty() { None } else { Some(agg) }
            },
        },
        series: series.iter().map(series_record).collect(),
    }
}

/// Build a study-level-only CBOR `Manifest` (no series) for attribute matches.
///
/// Kept for callers that only need study QIDO; the instance-populated path is
/// [`build_manifest_full`].
pub fn build_manifest(a: &StudyManifestAttrs) -> Manifest {
    build_manifest_full(a, &[])
}

/// The target path `manifests/{study_uid}.manifest`.
pub fn manifest_path(manifests_dir: &Path, study_uid: &str) -> PathBuf {
    manifests_dir.join(format!("{study_uid}.manifest"))
}

/// The tombstone path `manifests/{study_uid}.del`.
///
/// A study delete atomically renames `{study_uid}.manifest` here. The
/// full manifest bytes are preserved, so the erase-worker can read the
/// instance list back out of it without any separate journal. Readers filter
/// by the `.manifest` suffix, so a tombstone is invisible to QIDO/WADO without
/// touching any of the five read paths.
pub fn tombstone_path(manifests_dir: &Path, study_uid: &str) -> PathBuf {
    manifests_dir.join(format!("{study_uid}.del"))
}

/// The orphan sidecar path `manifests/{study_uid}.orphans` (2808 P1).
///
/// Every update-in-place appends the REPLACED blob's `hex tier` as a line
/// here (no hot-path deletion); the erase worker drains it when the study
/// tombstone is lifted, reclaiming old blobs the update orphaned. The
/// `.orphans` suffix is invisible to every `.manifest`-suffixed reader.
pub fn orphan_sidecar_path(manifests_dir: &Path, study_uid: &str) -> PathBuf {
    manifests_dir.join(format!("{study_uid}.orphans"))
}

/// Index the PersonName search keys of an accepted study into the n-gram `.idx`.
///
/// Called by a STOW writer right after it commits the study manifest: the
/// `PatientName` n-grams are appended under `idx/ngrams/` and the
/// `ReferringPhysicianName` n-grams under `idx/physician/`, so a later
/// fuzzymatch QIDO can name its candidate set without a full manifest scan.
///
/// This is best-effort and fire-and-forget: a failed index write never rolls
/// back the manifest (the `.idx` is an accelerator, not a source of truth -
/// a missing n-gram only widens the next query back to a full scan).
///
/// `day` is the `YYYYMMDD` bucket for the append, normally the study's date or
/// today. Returns the number of `.idx` rows appended (0 when the study has no
/// name to index).
pub fn index_study_names(
    idx_dir: &Path,
    a: &StudyManifestAttrs,
    study_uid: &str,
    day: &str,
) -> usize {
    let mut appended = 0;
    if let Some(name) = a.patient_name.as_deref()
        && !name.is_empty()
    {
        appended += append_ngrams(
            idx_dir,
            crate::idx::ngram::NameKind::Patient,
            name,
            study_uid,
            day,
        );
    }
    if let Some(phys) = a.referring_physician.as_deref()
        && !phys.is_empty()
    {
        appended += append_ngrams(
            idx_dir,
            crate::idx::ngram::NameKind::Physician,
            phys,
            study_uid,
            day,
        );
    }
    appended
}

/// Append every n-gram of one name to its kind's `.idx`, returning rows written.
fn append_ngrams(
    idx_dir: &Path,
    kind: crate::idx::ngram::NameKind,
    name: &str,
    study_uid: &str,
    day: &str,
) -> usize {
    let mut written = 0;
    // Fuzzy path only: normalize PN separators to spaces so the n-gram keys
    // line up with the (space-separated) fuzzymatch query grams. Exact search
    // and wildcards do not read this accelerator.
    let normalized = crate::idx::ngram::fuzzy_normalize(name);
    for gram in crate::idx::ngram::name_ngrams(&normalized) {
        if crate::idx::ngram::append(idx_dir, kind, &gram, study_uid, day).is_ok() {
            written += 1;
        }
    }
    written
}

/// Atomically write a study manifest with the full series/instance tree.
///
/// Before writing, the exclusive manifest-write lease is acquired (the
/// in-process mutex) so parallel STOW threads of this process never race on
/// the same manifest. Existing instances from a prior STOW are preserved
/// through a read-merge step: the current manifest is decoded, new instances
/// are merged in (duplicates skipped), and the result is committed via atomic
/// `tmp+rename`. A round-trip guard verifies the commit.
pub fn write_study_manifest_full(
    manifests_dir: &Path,
    a: &mut StudyManifestAttrs,
    series: &[SeriesModel],
) -> Result<PathBuf, StudyManifestError> {
    std::fs::create_dir_all(manifests_dir).map_err(StudyManifestError::Io)?;
    let path = manifest_path(manifests_dir, &a.study_instance_uid);

    // Acquire the exclusive manifest-write lease (the in-process Mutex).
    let _lease = super::coordinator::acquire();

    // resurrection-guard: if the study is tombstoned (`.del` present),
    // a fresh manifest would resurrect a deleted study. Refuse until the
    // erase-worker physically removes the files and lifts the tombstone. The
    // lease is held, so this never races a concurrent tombstone rename (the
    // admin delete takes the same mutex around its rename).
    if tombstone_path(manifests_dir, &a.study_instance_uid).exists() {
        return Err(StudyManifestError::StudyDeleted);
    }

    // B3 cache discipline: bump the slot version and take the cached entry
    // BEFORE the disk mutation. The taken Arc is ignored -
    // the merge base stays a disk read.
    let (cache_v, _taken) = crate::manifest_cache::begin_write(&a.study_instance_uid);

    // Read-merge: preserve instances already committed by earlier STOWs.
    let mut manifest = if path.exists() {
        let mut m = crate::manifest::read_manifest(&path)
            .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
        merge_series_into(&mut m, series);
        // The RAM study index must learn the MERGED truth (the disk
        // manifest), not the incoming batch attrs: a derived object (PR)
        // carries no patient identity, and its coerced placeholders must
        // never re-attribute an existing study.
        restamp_from_manifest(a, &m);
        m
    } else {
        build_manifest_full(a, series)
    };

    // Stamp the final merged series / instance counts back onto the caller's
    // attrs so `on_study_committed` can feed them to the RAM study index
    // (0020,1206 / 0020,1208). The counts are RAM-only - they are never
    // persisted into the CBOR manifest (the schema is unchanged).
    stamp_derived_counts(&manifest, a);

    // E-014: stamp the arrival time on every commit - a fresh STOW and a
    // re-STOW of the same study both push the patient to the top.
    manifest.sys.arrived_at = now_epoch();

    // Encode once; the atomic write takes the exact encoded bytes.
    let bytes = manifest
        .encode()
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest::write_bytes_atomic(&path, &bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    // Round-trip guard (A9): decode the exact in-memory bytes handed to the
    // atomic write instead of re-opening the just-written file - same
    // guarantee (a manifest we cannot decode is not a valid commit), half
    // the decode cost.
    crate::manifest::read_manifest_at(&bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    // B3: install the post-commit entry ONLY after the A9 guard passed.
    // encoded_len = the exact encoded bytes just committed (== file length).
    crate::manifest_cache::end_write(
        &a.study_instance_uid,
        cache_v,
        std::sync::Arc::new(manifest),
        bytes.len() as u64,
        manifests_dir,
    );
    Ok(path)
}

/// Atomically update a study manifest in REPLACE mode (2808 update-in-place).
///
/// Mirrors [`write_study_manifest_full`] (lease + tombstone guard + atomic
/// tmp+rename + round-trip guard), with two differences:
/// - matching SOPInstanceUIDs are REPLACED (new blake3 / size / transfer
///   syntax) instead of skipped;
/// - the study-level QIDO attrs are RE-STAMPED from `a` (the corrected
///   values) and `arrived_at` bumps so the corrected study rises to the top
///   of "recent patients" (E-014).
///
/// A missing manifest (degraded study) degrades to a full build exactly like
/// a fresh study.
pub fn write_study_manifest_update(
    manifests_dir: &Path,
    a: &mut StudyManifestAttrs,
    series: &[SeriesModel],
) -> Result<PathBuf, StudyManifestError> {
    std::fs::create_dir_all(manifests_dir).map_err(StudyManifestError::Io)?;
    let path = manifest_path(manifests_dir, &a.study_instance_uid);

    let _lease = super::coordinator::acquire();

    if tombstone_path(manifests_dir, &a.study_instance_uid).exists() {
        return Err(StudyManifestError::StudyDeleted);
    }

    // B3 cache discipline: bump + take BEFORE the disk mutation.
    let (cache_v, _taken) = crate::manifest_cache::begin_write(&a.study_instance_uid);

    let mut manifest = if path.exists() {
        let mut m = crate::manifest::read_manifest(&path)
            .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
        replace_series_into(&mut m, series);
        // RE-STAMP the study QIDO attrs from the corrected incoming values.
        m.study.attrs = study_attrs(a);
        m
    } else {
        build_manifest_full(a, series)
    };

    stamp_derived_counts(&manifest, a);
    manifest.sys.arrived_at = now_epoch();

    // A9: encode once, write the exact bytes, then verify by decoding the
    // in-memory buffer (same guard as the full-write path).
    let bytes = manifest
        .encode()
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest::write_bytes_atomic(&path, &bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest::read_manifest_at(&bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    // B3: install the post-commit entry ONLY after the A9 guard passed.
    crate::manifest_cache::end_write(
        &a.study_instance_uid,
        cache_v,
        std::sync::Arc::new(manifest),
        bytes.len() as u64,
        manifests_dir,
    );
    Ok(path)
}

/// Mark instances of `study_uid` as rejected (sequestered).
///
/// Mirrors [`write_study_manifest_update`]: coordinator lease -> tombstone
/// guard -> `begin_write` -> read, modify, encode -> atomic write -> A9
/// round-trip -> `end_write`. The cache invariant "entry present => entry ==
/// disk" therefore holds across the mutation, exactly as it does for a STOW.
///
/// Idempotent by design: an instance that is already rejected keeps the reason
/// it already carries and the encoding is unchanged, so a repeated rejection
/// note costs one read and one write and cannot flip anything back - nor can
/// it overwrite a Quality hide with an IOCM one. An SOP UID that is not in this
/// study is ignored - the caller resolves ownership first, and a note that
/// names an instance of another study must not disturb this manifest.
///
/// Returns the number of instances whose flag CHANGED (0 means "nothing to do"
/// and the manifest is not rewritten at all).
pub fn mark_rejected(
    manifests_dir: &Path,
    study_uid: &str,
    sop_uids: &[&str],
    reason: u32,
) -> Result<usize, StudyManifestError> {
    if sop_uids.is_empty() {
        return Ok(0);
    }
    let path = manifest_path(manifests_dir, study_uid);
    if !path.exists() {
        // No manifest: nothing to sequester. Not an error - a rejection note
        // may arrive before the study it names, and re-arrival is the
        // operator's problem, not a reason to fail the note's STOW.
        return Ok(0);
    }

    let _lease = super::coordinator::acquire();

    if tombstone_path(manifests_dir, study_uid).exists() {
        return Err(StudyManifestError::StudyDeleted);
    }

    // B3 cache discipline: bump + take BEFORE the disk mutation.
    let (cache_v, _taken) = crate::manifest_cache::begin_write(study_uid);

    let mut manifest = crate::manifest::read_manifest(&path)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;

    let mut changed = 0usize;
    for series in manifest.series.iter_mut() {
        for instance in series.instances.iter_mut() {
            if instance.is_rejected() {
                continue;
            }
            let matches = instance.attrs.iter().any(|(tag, value)| {
                *tag == crate::query::tag::SOP_INSTANCE_UID
                    && matches!(value, crate::manifest::AttrValue::Text(uid) if sop_uids.contains(&uid.as_str()))
            });
            if matches {
                instance.rejection_reason = reason;
                changed += 1;
            }
        }
    }

    if changed == 0 {
        // Nothing to write. `end_write` still has to install the entry that
        // `begin_write` took, so the cache does not lose a valid manifest.
        let bytes = manifest
            .encode()
            .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
        crate::manifest_cache::end_write(
            study_uid,
            cache_v,
            std::sync::Arc::new(manifest),
            bytes.len() as u64,
            manifests_dir,
        );
        return Ok(0);
    }

    // A9: encode once, write those bytes, then verify by decoding them.
    let bytes = manifest
        .encode()
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest::write_bytes_atomic(&path, &bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest::read_manifest_at(&bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest_cache::end_write(
        study_uid,
        cache_v,
        std::sync::Arc::new(manifest),
        bytes.len() as u64,
        manifests_dir,
    );
    Ok(changed)
}

/// The rejection reason already recorded for `sop_uid` in the study's
/// manifest, or [`NOT_REJECTED_REASON`] when it is absent or visible.
///
/// Used by the STOW path BEFORE accepting a part: IOCM 4.66.4.2.3.1 and
/// 4.66.4.3.3 say a receiver "shall not accept subsequent occurrence of
/// instances that have been hidden" under patient-safety or incorrect-MWL
/// rejections. Answering that question needs the persisted code, not a
/// boolean, because a Quality hide is explicitly re-storable (a corrected AI
/// result replaces the rejected one).
///
/// Read-only and best-effort: a missing manifest or an unreadable one reports
/// "not rejected", which accepts the part. Refusing a store because a manifest
/// could not be read would turn an index problem into a data-loss problem.
pub fn hidden_reason_of(manifests_dir: &Path, study_uid: &str, sop_uid: &str) -> u32 {
    let path = manifest_path(manifests_dir, study_uid);
    if !path.exists() {
        return crate::manifest::NOT_REJECTED_REASON;
    }
    // Prefer the cache: a just-committed manifest in this same request is
    // already there, and re-decoding it from disk per part would add a full
    // manifest parse to every instance of a large STOW.
    let manifest = match crate::manifest_cache::get_or_load(manifests_dir, study_uid) {
        Ok(m) => m,
        Err(_) => return crate::manifest::NOT_REJECTED_REASON,
    };
    for series in manifest.series.iter() {
        for instance in series.instances.iter() {
            let matches = instance.attrs.iter().any(|(tag, value)| {
                *tag == crate::query::tag::SOP_INSTANCE_UID
                    && matches!(value, crate::manifest::AttrValue::Text(uid) if uid == sop_uid)
            });
            if matches {
                return instance.rejection_reason;
            }
        }
    }
    crate::manifest::NOT_REJECTED_REASON
}

/// Clear the rejection reason of `sop_uids` in the study's manifest.
///
/// The mirror image of [`mark_rejected`], with the same durability sequence
/// (coordinator lease -> tombstone guard -> `begin_write` -> read, modify,
/// encode -> atomic write -> A9 round-trip -> `end_write`) and the same
/// idempotence: an instance that is already visible contributes nothing and
/// the encoding is unchanged.
///
/// Restore is an administrative extension, not a DICOM/IHE transaction: there
/// is no standard un-reject, so the only record that this happened is the
/// audit row the caller appends BEFORE calling this function. That ordering is
/// deliberate - the trail must survive a crash between the append and the
/// mutation, never the other way round.
///
/// Clearing the reason removes CBOR key 10 entirely, so a restored instance
/// re-encodes byte-identically to one that was never rejected. That property
/// is what makes "hidden, then restored" indistinguishable from "never
/// hidden" in the stored data, which is the honest outcome: the KOS note in
/// CAS remains the historical record of the rejection itself.
///
/// Returns the number of instances whose reason CHANGED (0 means "nothing to
/// do" and the manifest is not rewritten).
pub fn mark_restored(
    manifests_dir: &Path,
    study_uid: &str,
    sop_uids: &[&str],
) -> Result<usize, StudyManifestError> {
    if sop_uids.is_empty() {
        return Ok(0);
    }
    let path = manifest_path(manifests_dir, study_uid);
    if !path.exists() {
        return Ok(0);
    }

    let _lease = super::coordinator::acquire();

    if tombstone_path(manifests_dir, study_uid).exists() {
        return Err(StudyManifestError::StudyDeleted);
    }

    // B3 cache discipline: bump + take BEFORE the disk mutation.
    let (cache_v, _taken) = crate::manifest_cache::begin_write(study_uid);

    let mut manifest = crate::manifest::read_manifest(&path)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;

    let mut changed = 0usize;
    for series in manifest.series.iter_mut() {
        for instance in series.instances.iter_mut() {
            if !instance.is_rejected() {
                continue;
            }
            let matches = instance.attrs.iter().any(|(tag, value)| {
                *tag == crate::query::tag::SOP_INSTANCE_UID
                    && matches!(value, crate::manifest::AttrValue::Text(uid) if sop_uids.contains(&uid.as_str()))
            });
            if matches {
                instance.rejection_reason = crate::manifest::NOT_REJECTED_REASON;
                changed += 1;
            }
        }
    }

    if changed == 0 {
        // Nothing to write. `end_write` still has to install the entry that
        // `begin_write` took, so the cache does not lose a valid manifest.
        let bytes = manifest
            .encode()
            .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
        crate::manifest_cache::end_write(
            study_uid,
            cache_v,
            std::sync::Arc::new(manifest),
            bytes.len() as u64,
            manifests_dir,
        );
        return Ok(0);
    }

    let bytes = manifest
        .encode()
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest::write_bytes_atomic(&path, &bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest::read_manifest_at(&bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest_cache::end_write(
        study_uid,
        cache_v,
        std::sync::Arc::new(manifest),
        bytes.len() as u64,
        manifests_dir,
    );
    Ok(changed)
}

/// Atomically write a study manifest in REBUILD mode (0809 enrichment fix).
/// Mirrors [`write_study_manifest_full`] (lease + tombstone guard + atomic
/// tmp+rename + A9 round-trip guard + B3 cache discipline) with ONE
/// difference: when the manifest already exists it is ENRICHED, never
/// re-stamped:
/// - study attrs: [`merge_attrs`] adds capture tags missing from the old
///   record, existing values win;
/// - series/instance attrs: [`rebuild_series_attrs`] (the enrichment)
///   adds missing capture tags and missing instances;
/// - `sys.arrived_at` and `sys.total_pixel_bytes` are preserved VERBATIM -
///   a historical rebuild must not collapse the E-014 recent-patient order
///   and must not fake a pixel-byte recount.
///
/// A missing manifest degrades to a full build exactly like a fresh study
/// (first-arrival `arrived_at` is set by [`build_manifest_full`]).
pub fn write_study_manifest_rebuild(
    manifests_dir: &Path,
    a: &mut StudyManifestAttrs,
    series: &[SeriesModel],
) -> Result<PathBuf, StudyManifestError> {
    std::fs::create_dir_all(manifests_dir).map_err(StudyManifestError::Io)?;
    let path = manifest_path(manifests_dir, &a.study_instance_uid);

    // Acquire the exclusive manifest-write lease (the in-process Mutex).
    let _lease = super::coordinator::acquire();

    // resurrection-guard, same as the full-write path.
    if tombstone_path(manifests_dir, &a.study_instance_uid).exists() {
        return Err(StudyManifestError::StudyDeleted);
    }

    // B3 cache discipline: bump + take BEFORE the disk mutation.
    let (cache_v, _taken) = crate::manifest_cache::begin_write(&a.study_instance_uid);

    let manifest = if path.exists() {
        let mut m = crate::manifest::read_manifest(&path)
            .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
        // ENRICHMENT-ONLY: add missing study-level capture tags (existing
        // values win - merge_attrs semantics), then enrich the series and
        // instance records in place.
        merge_attrs(&mut m.study.attrs, study_attrs(a));
        rebuild_series_attrs(&mut m, series);
        // sys.arrived_at / total_pixel_bytes are deliberately NOT touched:
        // the historical arrival order and the stored byte accounting stay
        // exactly what the previous commit recorded.
        m
    } else {
        build_manifest_full(a, series)
    };

    // RAM-only derived counts for `on_study_committed` (never persisted).
    stamp_derived_counts(&manifest, a);

    // Encode once; the atomic write takes the exact encoded bytes.
    let bytes = manifest
        .encode()
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    crate::manifest::write_bytes_atomic(&path, &bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    // Round-trip guard (A9): decode the exact in-memory bytes.
    crate::manifest::read_manifest_at(&bytes)
        .map_err(|e| StudyManifestError::Manifest(e.to_string()))?;
    // B3: install the post-commit entry ONLY after the A9 guard passed.
    crate::manifest_cache::end_write(
        &a.study_instance_uid,
        cache_v,
        std::sync::Arc::new(manifest),
        bytes.len() as u64,
        manifests_dir,
    );
    Ok(path)
}

/// Stamp the RAM-only derived counts (series/instance counts,
/// ModalitiesInStudy) from a merged manifest back onto the caller's attrs.
fn stamp_derived_counts(manifest: &Manifest, a: &mut StudyManifestAttrs) {
    a.num_series = manifest.series.len() as u64;
    a.num_instances = manifest
        .series
        .iter()
        .map(|s| s.instances.len() as u64)
        .fold(0u64, |acc, n| acc.saturating_add(n));
    // ModalitiesInStudy (0008,0061): distinct series modalities in
    // first-occurrence order, comma-joined. RAM-only; never persisted.
    a.modalities_in_study = {
        let mut agg = String::new();
        for s in &manifest.series {
            for (t, v) in &s.attrs {
                if *t == tag::MODALITY
                    && let AttrValue::Text(m) = v
                    && !agg.split(',').any(|x| x == m)
                {
                    if !agg.is_empty() {
                        agg.push(',');
                    }
                    agg.push_str(m);
                }
            }
        }
        if agg.is_empty() { None } else { Some(agg) }
    };
}

/// Atomically write a study-level-only manifest (no instance data).
///
/// Accepted for callers that do not yet feed instance records; the WADO-capable
/// write is [`write_study_manifest_full`].
pub fn write_study_manifest(
    manifests_dir: &Path,
    a: &mut StudyManifestAttrs,
) -> Result<PathBuf, StudyManifestError> {
    write_study_manifest_full(manifests_dir, a, &[])
}

/// Errors produced by [`write_study_manifest`].
#[derive(Debug)]
pub enum StudyManifestError {
    /// Filesystem error creating the directory or writing the file.
    Io(io::Error),
    /// The manifest could not be encoded, written, or decoded back.
    Manifest(String),
    /// The study is tombstoned (`.del` exists) - re-ingest after erase.
    StudyDeleted,
}

impl std::fmt::Display for StudyManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StudyManifestError::Io(e) => write!(f, "study manifest I/O error: {e}"),
            StudyManifestError::Manifest(e) => write!(f, "study manifest encode error: {e}"),
            StudyManifestError::StudyDeleted => {
                write!(f, "study is deleted; re-ingest after erase completes")
            }
        }
    }
}

impl std::error::Error for StudyManifestError {}

impl From<io::Error> for StudyManifestError {
    fn from(e: io::Error) -> Self {
        StudyManifestError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::read_manifest;
    use crate::query::QueryFilter;

    fn attrs() -> StudyManifestAttrs {
        StudyManifestAttrs {
            patient_name: Some("ivanov".into()),
            patient_id: Some("PAT12".into()),
            study_date: Some("20260802".into()),
            modality: "CT".into(),
            study_instance_uid: "1.2.840.1.1".into(),
            accession_number: Some("A100".into()),
            ..Default::default()
        }
    }

    #[test]
    fn builds_study_attrs_in_tag_order() {
        let a = attrs();
        let list = study_attrs(&a);
        // Tag order must be ascending for the manifest's binary-search contract.
        for w in list.windows(2) {
            assert!(w[0].0 < w[1].0, "tags must be ascending");
        }
        // Represents the study UID, date (numeric), name, id, modality, accession.
        assert_eq!(list.len(), 6);
    }

    #[test]
    fn study_attrs_includes_performing_and_reading_physicians() {
        let mut a = attrs();
        a.performing_physician = Some("Perf^Doc".into());
        a.name_of_reading_physicians = Some("Read^Doc".into());
        a.study_description = Some("Chest".into());
        let list = study_attrs(&a);

        let perf = list
            .iter()
            .find(|(t, _)| *t == tag::PERFORMING_PHYSICIAN)
            .expect("performing physician emitted");
        assert!(matches!(perf.1, AttrValue::Text(ref s) if s == "Perf^Doc"));

        let read = list
            .iter()
            .find(|(t, _)| *t == tag::NAME_OF_READING_PHYSICIANS)
            .expect("name of reading physicians emitted");
        assert!(matches!(read.1, AttrValue::Text(ref s) if s == "Read^Doc"));

        // Strict ascending order (binary-search contract).
        let pos_perf = list
            .iter()
            .position(|(t, _)| *t == tag::PERFORMING_PHYSICIAN)
            .unwrap();
        let pos_study_desc = list
            .iter()
            .position(|(t, _)| *t == tag::STUDY_DESCRIPTION)
            .unwrap();
        let pos_read = list
            .iter()
            .position(|(t, _)| *t == tag::NAME_OF_READING_PHYSICIANS)
            .unwrap();
        let pos_patient = list
            .iter()
            .position(|(t, _)| *t == tag::PATIENT_NAME)
            .unwrap();
        assert!(pos_study_desc < pos_perf && pos_perf < pos_read && pos_read < pos_patient);
        for w in list.windows(2) {
            assert!(w[0].0 < w[1].0, "tags must be ascending");
        }
    }

    #[test]
    fn responsible_person_attrs_sorts_between_sex_and_study_uid() {
        // FR-vet stage 1: ResponsiblePerson (0010,2297) must be emitted AFTER
        // PatientSex (0010,0040) and BEFORE StudyInstanceUID (0020,000D) so the
        // ascending-tag (binary-search) invariant holds.
        let mut a = attrs();
        a.patient_sex = Some("O".into());
        a.patient_birth_date = Some("20200101".into());
        a.responsible_person = Some("Иванов^Иван".into());
        let list = study_attrs(&a);

        let pos_sex = list
            .iter()
            .position(|(t, _)| *t == tag::PATIENT_SEX)
            .expect("patient sex emitted");
        let pos_birth = list
            .iter()
            .position(|(t, _)| *t == tag::PATIENT_BIRTH_DATE)
            .expect("birth date emitted");
        let pos_resp = list
            .iter()
            .position(|(t, _)| *t == tag::RESPONSIBLE_PERSON)
            .expect("responsible person emitted");
        let pos_study = list
            .iter()
            .position(|(t, _)| *t == tag::STUDY_INSTANCE_UID)
            .expect("study uid emitted");

        // 0010,0030 < 0010,0040 < 0010,2297 < 0020,000D
        assert!(pos_birth < pos_sex && pos_sex < pos_resp && pos_resp < pos_study);
        // The stored text is retrievable.
        assert!(matches!(
            list[pos_resp].1,
            AttrValue::Text(ref s) if s == "Иванов^Иван"
        ));
        // Ascending-order invariant holds across the whole list.
        for w in list.windows(2) {
            assert!(w[0].0 < w[1].0, "tags must be ascending");
        }
    }

    #[test]
    fn empty_responsible_person_field_is_omitted() {
        let mut a = attrs();
        a.responsible_person = Some("".into());
        let list = study_attrs(&a);
        assert!(!list.iter().any(|(t, _)| *t == tag::RESPONSIBLE_PERSON));
    }

    #[test]
    fn empty_physician_fields_are_omitted() {
        let mut a = attrs();
        a.performing_physician = Some("".into());
        a.name_of_reading_physicians = Some("".into());
        let list = study_attrs(&a);
        assert!(!list.iter().any(|(t, _)| *t == tag::PERFORMING_PHYSICIAN));
        assert!(
            !list
                .iter()
                .any(|(t, _)| *t == tag::NAME_OF_READING_PHYSICIANS)
        );
    }

    /// 08.09 Stage B: the seven captured attributes land at their
    /// ascending-tag anchors in the study/series attr lists, and empty
    /// values are omitted (the existing capture discipline).
    #[test]
    fn stage_b_capture_attrs_in_tag_order() {
        // Study side: InstitutionName between Modality and ReferringPhysician;
        // the 0032 pair at the end, after StudyID.
        let mut a = attrs();
        a.institution_name = Some("City Hospital".into());
        a.requesting_physician = Some("REQ^PAUL".into());
        a.requested_procedure_description = Some("CT CHEST WO".into());
        a.study_id = Some("S1".into());
        a.referring_physician = Some("REF^DOC".into());
        let list = study_attrs(&a);
        for w in list.windows(2) {
            assert!(w[0].0 < w[1].0, "tags must be ascending");
        }
        let pos = |t: u32| list.iter().position(|(tag, _)| *tag == t).unwrap();
        assert!(pos(tag::INSTITUTION_NAME) < pos(tag::REFERRING_PHYSICIAN));
        assert!(pos(tag::STUDY_ID) < pos(tag::REQUESTING_PHYSICIAN));
        assert!(pos(tag::REQUESTING_PHYSICIAN) < pos(tag::REQUESTED_PROCEDURE_DESCRIPTION));
        // Empty values are never captured.
        let mut b = attrs();
        b.institution_name = Some(String::new());
        assert!(
            !study_attrs(&b)
                .iter()
                .any(|(t, _)| *t == tag::INSTITUTION_NAME)
        );

        // Series side: the four captured attributes.
        let sm = SeriesModel {
            series_instance_uid: "1.2.840.1.2".into(),
            modality: "CT".into(),
            series_number: Some("1".into()),
            manufacturer: Some("ACME".into()),
            institutional_department_name: Some("RAD".into()),
            operators_name: Some("OPS^ANNA".into()),
            protocol_name: Some("CHEST_RTN".into()),
            station_name: Some("CT1".into()),
            series_description: Some("Chest".into()),
            body_part_examined: Some("CHEST".into()),
            ..Default::default()
        };
        let s = series_attrs(&sm);
        for w in s.windows(2) {
            assert!(w[0].0 < w[1].0, "tags must be ascending");
        }
        let pos = |t: u32| s.iter().position(|(tag, _)| *tag == t).unwrap();
        assert!(pos(tag::MANUFACTURER) < pos(tag::STATION_NAME));
        assert!(pos(tag::STATION_NAME) < pos(tag::SERIES_DESCRIPTION));
        assert!(pos(tag::SERIES_DESCRIPTION) < pos(tag::INSTITUTIONAL_DEPARTMENT_NAME));
        assert!(pos(tag::INSTITUTIONAL_DEPARTMENT_NAME) < pos(tag::OPERATORS_NAME));
        assert!(pos(tag::BODY_PART_EXAMINED) < pos(tag::PROTOCOL_NAME));
        assert!(pos(tag::PROTOCOL_NAME) < pos(tag::SERIES_INSTANCE_UID));
        // Empty values are never captured.
        let empty = SeriesModel {
            manufacturer: Some(String::new()),
            ..sm
        };
        assert!(
            !series_attrs(&empty)
                .iter()
                .any(|(t, _)| *t == tag::MANUFACTURER)
        );
    }

    #[test]
    fn writes_and_reads_back_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let path = write_study_manifest(&manifests, &mut a).expect("write");

        assert!(path.is_file());
        let decoded = read_manifest(&path).expect("read");
        // The study UID round-trips through the CBOR encode/decode boundary.
        let uid = decoded
            .study
            .attrs
            .iter()
            .find(|(t, _)| *t == tag::STUDY_INSTANCE_UID)
            .and_then(|(_, v)| match v {
                AttrValue::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("study uid present");
        assert_eq!(uid, "1.2.840.1.1");
    }

    #[test]
    fn query_engine_matches_written_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        write_study_manifest(&manifests, &mut a).expect("write");

        let decoded = read_manifest(&manifests.join("1.2.840.1.1.manifest")).expect("read");
        // A PatientName glob and modality comma-list must both match.
        let f = QueryFilter {
            patient_name: Some("iva*".into()),
            modality: Some("CT,MR".into()),
            ..Default::default()
        };
        assert!(crate::query::match_study_attrs(&f, &decoded));
    }

    #[test]
    fn manifest_roundtrip_preserves_responsible_person_and_filters() {
        // FR-vet stage 1: the (0010,2297) attribute survives the
        // write -> read (CBOR) boundary that BOTH the STOW path and the rebuild
        // path share (write_study_manifest_full), and a ResponsiblePerson
        // QueryFilter matches the reconstructed manifest.
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        a.patient_sex = Some("O".into());
        a.responsible_person = Some("IVANOV^IVAN".into());
        write_study_manifest(&manifests, &mut a).expect("write");

        let decoded = read_manifest(&manifests.join("1.2.840.1.1.manifest")).expect("read");
        // The stored attrs keep ascending order and the vet tag between sex/date.
        let mut tags = decoded.study.attrs.iter().map(|(t, _)| *t);
        assert!(
            (|| {
                let mut prev = 0u32;
                for t in tags.by_ref() {
                    if t <= prev {
                        return false;
                    }
                    prev = t;
                }
                true
            })(),
            "manifest attrs must be strictly ascending"
        );
        assert!(
            decoded
                .study
                .attrs
                .iter()
                .any(|(t, v)| *t == tag::RESPONSIBLE_PERSON
                    && matches!(v, AttrValue::Text(s) if s == "IVANOV^IVAN")),
            "responsible person must round-trip"
        );

        // Exact and glob ResponsiblePerson filters match the rebuilt manifest.
        let exact = QueryFilter {
            responsible_person: Some("IVANOV^IVAN".into()),
            ..Default::default()
        };
        assert!(crate::query::match_study_attrs(&exact, &decoded));

        let glob = QueryFilter {
            responsible_person: Some("IVANO*".into()),
            ..Default::default()
        };
        assert!(crate::query::match_study_attrs(&glob, &decoded));

        // Absent or different responsible person must not match.
        let other = QueryFilter {
            responsible_person: Some("PETROV".into()),
            ..Default::default()
        };
        assert!(!crate::query::match_study_attrs(&other, &decoded));
    }

    #[test]
    fn empty_optional_fields_are_omitted() {
        let mut a = attrs();
        a.patient_sex = None;
        a.accession_number = Some("".into()); // empty -> omitted
        let list = study_attrs(&a);
        assert!(!list.iter().any(|(t, _)| *t == tag::ACCESSION_NUMBER));
        assert!(!list.iter().any(|(t, _)| *t == tag::PATIENT_SEX));
    }

    /// A two-series / two-instance tree mirroring one STOW-accepted study.
    fn full_series() -> Vec<SeriesModel> {
        let mut b3_a = [0u8; 32];
        b3_a[0] = 0x11;
        let mut b3_b = [0u8; 32];
        b3_b[0] = 0x22;
        vec![
            SeriesModel {
                series_instance_uid: "1.2.840.1.2".into(),
                modality: "CT".into(),
                series_number: Some("1".into()),
                series_date: Some("20260802".into()),
                series_time: Some("103000".into()),
                station_name: Some("CT1".into()),
                manufacturer: None,
                institutional_department_name: None,
                operators_name: None,
                series_description: Some("Chest".into()),
                body_part_examined: Some("CHEST".into()),
                protocol_name: None,
                laterality: Some("L".into()),
                pps_start_date: None,
                pps_start_time: None,
                scheduled_procedure_step_id: None,
                requested_procedure_id: None,
                instances: vec![InstanceModel {
                    sop_instance_uid: "1.2.840.1.3".into(),
                    sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".into()),
                    instance_number: Some("1".into()),
                    blake3: b3_a,
                    storage_tier: TIER_HOT,
                    transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                    file_size: 2_000_000,
                    ..Default::default()
                }],
            },
            SeriesModel {
                series_instance_uid: "1.2.840.1.4".into(),
                modality: "CT".into(),
                series_number: Some("2".into()),
                series_date: Some("20260802".into()),
                series_time: Some("103100".into()),
                scheduled_procedure_step_id: None,
                requested_procedure_id: None,
                station_name: Some("CT1".into()),
                manufacturer: None,
                institutional_department_name: None,
                operators_name: None,
                series_description: Some("Chest".into()),
                body_part_examined: Some("CHEST".into()),
                protocol_name: None,
                laterality: Some("R".into()),
                pps_start_date: None,
                pps_start_time: None,
                instances: vec![InstanceModel {
                    sop_instance_uid: "1.2.840.1.5".into(),
                    sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".into()),
                    instance_number: Some("1".into()),
                    blake3: b3_b,
                    storage_tier: TIER_HOT,
                    transfer_syntax_uid: "1.2.840.10008.1.2.4.80".into(),
                    file_size: 1_500_000,
                    ..Default::default()
                }],
            },
        ]
    }

    #[test]
    fn full_manifest_roundtrips_series_and_instances() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let series = full_series();
        let path = write_study_manifest_full(&manifests, &mut a, &series).expect("write");

        let decoded = read_manifest(&path).expect("read");
        // Two series, each with exactly one instance.
        assert_eq!(decoded.series.len(), 2);
        assert_eq!(decoded.series[0].instances.len(), 1);
        assert_eq!(decoded.series[1].instances.len(), 1);

        // sys header reflects the tier + summed file sizes from instances.
        assert_eq!(decoded.sys.storage_tier, TIER_HOT);
        assert_eq!(decoded.sys.total_pixel_bytes, 3_500_000);
    }

    /// A series carrying PerformedProcedureStepStartDate/Time persists the
    /// tags through a full STOW write, and a QIDO `SeriesFilter` on those tags
    /// finds the series from the decoded on-disk manifest.
    #[test]
    fn pps_capture_persists_and_qido_finds_series() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let sm = SeriesModel {
            series_instance_uid: "1.2.840.pps1".into(),
            modality: "CT".into(),
            series_number: Some("7".into()),
            series_date: Some("20260802".into()),
            series_time: Some("103000".into()),
            pps_start_date: Some("20260802".into()),
            pps_start_time: Some("111500".into()),
            scheduled_procedure_step_id: Some("SPS-7".into()),
            requested_procedure_id: Some("RP-7".into()),
            station_name: Some("CT1".into()),
            manufacturer: None,
            institutional_department_name: None,
            operators_name: None,
            series_description: Some("Chest".into()),
            body_part_examined: Some("CHEST".into()),
            protocol_name: None,
            laterality: Some("L".into()),
            instances: vec![InstanceModel {
                sop_instance_uid: "1.2.840.pps.inst".into(),
                sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".into()),
                instance_number: Some("1".into()),
                blake3: [0u8; 32],
                storage_tier: TIER_HOT,
                transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                file_size: 512,
                ..Default::default()
            }],
        };
        let path = write_study_manifest_full(&manifests, &mut a, &[sm]).expect("write");

        let decoded = read_manifest(&path).expect("read");
        // The PPS capture tags are on-disk (date as UInt like SERIES_DATE).
        let s0 = &decoded.series[0];
        let mut pps_date: Option<u64> = None;
        let mut pps_time: Option<&str> = None;
        for (t, v) in &s0.attrs {
            if *t == tag::PPS_START_DATE {
                if let AttrValue::UInt(n) = v {
                    pps_date = Some(*n);
                }
            } else if let (tag::PPS_START_TIME, AttrValue::Text(s)) = (*t, v) {
                pps_time = Some(s.as_str());
            }
        }
        assert_eq!(pps_date, Some(20260802));
        assert_eq!(pps_time, Some("111500"));

        // A QIDO series filter on the PPS tags resolves the series.
        let filter = crate::query::series::SeriesFilter {
            pps_start_date: Some("20260802".into()),
            pps_start_time: Some("11*".into()),
            ..Default::default()
        };
        let mut out: Vec<&crate::manifest::SeriesRecord> = Vec::new();
        crate::query::series::search_series(&decoded, &filter, &mut out);
        assert_eq!(out.len(), 1);
        let mut uid: Option<&str> = None;
        for (t, v) in &out[0].attrs {
            if let (tag::SERIES_INSTANCE_UID, AttrValue::Text(s)) = (*t, v) {
                uid = Some(s.as_str());
            }
        }
        assert_eq!(uid, Some("1.2.840.pps1"));
    }

    /// `write_study_manifest_full` stamps the final merged
    /// series / instance counts AND the ModalitiesInStudy aggregate back onto
    /// the caller's attrs so `on_study_committed` -> upsert can serve correct
    /// /studies values from the RAM index (no transient empty aggregate).
    #[test]
    fn manifest_write_stamps_modalities_and_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let series = full_series(); // two CT series, two instances
        write_study_manifest_full(&manifests, &mut a, &series).expect("write");

        assert_eq!(a.num_series, 2);
        assert_eq!(a.num_instances, 2);
        assert_eq!(a.modalities_in_study.as_deref(), Some("CT"));

        // The written manifest's study derives the same aggregate on read.
        let decoded = read_manifest(&manifests.join("1.2.840.1.1.manifest")).expect("read");
        assert_eq!(decoded.study.modalities_in_study.as_deref(), Some("CT"));
        assert_eq!(decoded.study.series_count, 2);
        assert_eq!(decoded.study.instance_count, 2);
    }

    /// 0809 enrichment fix: `write_study_manifest_rebuild` adds missing
    /// capture tags to an existing manifest, keeps existing values, and
    /// preserves `sys.arrived_at` / `total_pixel_bytes` verbatim.
    #[test]
    fn rebuild_write_enriches_existing_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let text = |attrs: &[(u32, AttrValue)], t: u32| -> Option<String> {
            attrs
                .iter()
                .find(|(tag, _)| *tag == t)
                .and_then(|(_, v)| match v {
                    AttrValue::Text(s) => Some(s.clone()),
                    _ => None,
                })
        };

        // OLD capture shape: none of the 08.09 fields; stale StationName.
        let mut old = attrs();
        let old_series = [SeriesModel {
            series_instance_uid: "1.2.840.1.2".into(),
            modality: "CT".into(),
            series_number: Some("1".into()),
            station_name: Some("OLDSTATION".into()),
            series_description: Some("Chest".into()),
            instances: vec![InstanceModel {
                sop_instance_uid: "1.2.840.1.3".into(),
                blake3: [0u8; 32],
                storage_tier: TIER_HOT,
                transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                file_size: 1234,
                ..Default::default()
            }],
            ..Default::default()
        }];
        write_study_manifest_full(&manifests, &mut old, &old_series).expect("old write");
        let path = manifest_path(&manifests, &old.study_instance_uid);
        let before = read_manifest(&path).expect("read old");
        let arrived_before = before.sys.arrived_at;
        let bytes_before = before.sys.total_pixel_bytes;
        assert_eq!(bytes_before, 1234);

        // Rebuild with the enriched capture set and conflicting values.
        let mut fresh = attrs();
        fresh.institution_name = Some("City Hospital".into());
        fresh.requesting_physician = Some("REQ^PAUL".into());
        fresh.patient_name = Some("petrov".into()); // must NOT overwrite "ivanov"
        let fresh_series = [SeriesModel {
            series_instance_uid: "1.2.840.1.2".into(),
            modality: "CT".into(),
            series_number: Some("1".into()),
            manufacturer: Some("ACME".into()),
            institutional_department_name: Some("RAD".into()),
            operators_name: Some("OPS^ANNA".into()),
            protocol_name: Some("CHEST_PROTO".into()),
            station_name: Some("CT2".into()), // must NOT overwrite OLDSTATION
            series_description: Some("Chest".into()),
            instances: vec![InstanceModel {
                sop_instance_uid: "1.2.840.1.3".into(),
                blake3: [0u8; 32],
                storage_tier: TIER_HOT,
                transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                file_size: 9999, // must NOT overwrite the stored size
                ..Default::default()
            }],
            ..Default::default()
        }];
        write_study_manifest_rebuild(&manifests, &mut fresh, &fresh_series).expect("rebuild write");

        let after = read_manifest(&path).expect("read enriched");
        // Study level: missing tags added, existing values win.
        assert_eq!(
            text(&after.study.attrs, tag::INSTITUTION_NAME).as_deref(),
            Some("City Hospital")
        );
        assert_eq!(
            text(&after.study.attrs, tag::PATIENT_NAME).as_deref(),
            Some("ivanov")
        );
        // Series level: enriched; the stale StationName survives.
        let s0 = &after.series[0];
        assert_eq!(text(&s0.attrs, tag::MANUFACTURER).as_deref(), Some("ACME"));
        assert_eq!(
            text(&s0.attrs, tag::STATION_NAME).as_deref(),
            Some("OLDSTATION")
        );
        // Ascending-tag contract holds in every enriched list.
        for w in after.study.attrs.windows(2) {
            assert!(w[0].0 < w[1].0, "study tags must be ascending");
        }
        for w in s0.attrs.windows(2) {
            assert!(w[0].0 < w[1].0, "series tags must be ascending");
        }
        // sys + instance accounting preserved verbatim.
        assert_eq!(after.sys.arrived_at, arrived_before);
        assert_eq!(after.sys.total_pixel_bytes, bytes_before);
        assert_eq!(after.series[0].instances[0].file_size, 1234);
    }

    /// 0809 enrichment fix: `write_study_manifest_rebuild` builds a fresh
    /// manifest when one is missing and refuses a tombstoned study exactly
    /// like the other writers.
    #[test]
    fn rebuild_write_guards_tombstone_and_builds_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let series = full_series(); // two series, 2_000_000 + 1_500_000 bytes

        // Missing manifest -> full build, round-trips (A9 guard passed).
        let path = write_study_manifest_rebuild(&manifests, &mut a, &series).expect("fresh");
        let decoded = read_manifest(&path).expect("roundtrip");
        assert_eq!(decoded.sys.total_pixel_bytes, 3_500_000);
        assert_eq!(decoded.series.len(), 2);

        // Tombstone guard: a `.del` present refuses the write.
        std::fs::write(tombstone_path(&manifests, &a.study_instance_uid), b"").expect("tombstone");
        let mut b = attrs();
        let err = write_study_manifest_rebuild(&manifests, &mut b, &series).unwrap_err();
        assert!(matches!(err, StudyManifestError::StudyDeleted));
    }

    #[test]
    fn full_manifest_find_instance_returns_wado_data() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let series = full_series();
        write_study_manifest_full(&manifests, &mut a, &series).expect("write");

        let decoded = read_manifest(&manifests.join("1.2.840.1.1.manifest")).expect("read");
        // WADO resolves the instance by SOP UID and gets its pure-manifest data.
        let inst = decoded
            .find_instance("1.2.840.1.5")
            .expect("instance present");
        assert_eq!(inst.transfer_syntax, "1.2.840.10008.1.2.4.80");
        assert_eq!(inst.storage_tier, TIER_HOT);
        assert_eq!(inst.blake3[0], 0x22);
        assert_eq!(inst.file_size, 1_500_000);
        // No such instance -> None.
        assert!(decoded.find_instance("9.9.9.9").is_none());
    }

    /// capture: series/instance attrs land in the manifest and survive a
    /// CBOR round-trip, in ascending tag order.
    #[test]
    fn captures_series_and_instance_attributes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let series = full_series();
        let path = write_study_manifest_full(&manifests, &mut a, &series).expect("write");
        let decoded = read_manifest(&path).expect("read");

        let s0 = &decoded.series[0];
        let tags: Vec<u32> = s0.attrs.iter().map(|(t, _)| *t).collect();
        // Ascending tag order preserves the decoder's binary-search contract.
        for w in tags.windows(2) {
            assert!(w[0] < w[1], "series tags must be ascending");
        }
        fn text_attr(attrs: &[(u32, crate::manifest::AttrValue)], t: u32) -> Option<&str> {
            attrs
                .iter()
                .find(|(tag, _)| *tag == t)
                .and_then(|(_, v)| match v {
                    crate::manifest::AttrValue::Text(s) => Some(s.as_str()),
                    _ => None,
                })
        }
        fn uint_attr(attrs: &[(u32, crate::manifest::AttrValue)], t: u32) -> Option<u64> {
            attrs
                .iter()
                .find(|(tag, _)| *tag == t)
                .and_then(|(_, v)| match v {
                    crate::manifest::AttrValue::UInt(n) => Some(*n),
                    _ => None,
                })
        }

        // Series capture tags.
        assert_eq!(uint_attr(&s0.attrs, tag::SERIES_DATE), Some(20260802));
        assert_eq!(text_attr(&s0.attrs, tag::SERIES_TIME), Some("103000"));
        assert_eq!(text_attr(&s0.attrs, tag::STATION_NAME), Some("CT1"));
        assert_eq!(text_attr(&s0.attrs, tag::SERIES_DESCRIPTION), Some("Chest"));
        assert_eq!(text_attr(&s0.attrs, tag::BODY_PART_EXAMINED), Some("CHEST"));
        assert_eq!(text_attr(&s0.attrs, tag::LATERALITY), Some("L"));

        // Instance capture tags.
        let i0 = &s0.instances[0];
        assert_eq!(
            text_attr(&i0.attrs, tag::SOP_CLASS_UID),
            Some("1.2.840.10008.5.1.4.1.1.2")
        );
        assert_eq!(text_attr(&i0.attrs, tag::INSTANCE_NUMBER), Some("1"));
    }

    /// capture: absent optional series/instance attrs are omitted, never
    /// encoded as empty strings.
    #[test]
    fn omits_absent_series_instance_attrs() {
        let mut s = SeriesModel {
            series_instance_uid: "1.2.840.1.2".into(),
            modality: "CT".into(),
            series_number: Some("1".into()),
            instances: vec![InstanceModel {
                sop_instance_uid: "1.2.840.1.3".into(),
                blake3: [0u8; 32],
                storage_tier: TIER_HOT,
                transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                file_size: 100,
                ..Default::default()
            }],
            ..Default::default()
        };
        // Specifically absent (None) and explicitly empty (Some("")).
        s.series_description = Some("".into());
        let sr = series_record(&s);
        let ir = &sr.instances[0];
        let has = |attrs: &[(u32, crate::manifest::AttrValue)], t: u32| {
            attrs.iter().any(|(tag, _)| *tag == t)
        };
        assert!(!has(&sr.attrs, tag::SERIES_DESCRIPTION));
        assert!(!has(&sr.attrs, tag::SERIES_DATE));
        assert!(!has(&sr.attrs, tag::STATION_NAME));
        assert!(!has(&ir.attrs, tag::SOP_CLASS_UID));
        assert!(!has(&ir.attrs, tag::INSTANCE_NUMBER));
        // The identity tags are still present.
        assert!(has(&sr.attrs, tag::SERIES_INSTANCE_UID));
        assert!(has(&ir.attrs, tag::SOP_INSTANCE_UID));
    }

    /// backward compatibility: a manifest captured before the expansion
    /// (no extra series/instance tags) decodes and re-encodes unchanged.
    #[test]
    fn old_manifest_without_new_attrs_decodes() {
        // A minimal, pre-series carry only identity attrs,
        // instances only SOP Instance UID + the system keys.
        let old = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 1234,
                arrived_at: 987_654_321,
            },
            study: StudyQido {
                attrs: vec![(0x0020_000d, AttrValue::Text("1.2.840.1.1".into()))],
                // One series (CT), one instance - matched by the decoded manifest.
                series_count: 1,
                instance_count: 1,
                modalities_in_study: Some("CT".into()),
            },
            series: vec![SeriesRecord {
                attrs: vec![
                    (0x0020_000e, AttrValue::Text("1.2.840.1.2".into())),
                    (0x0008_0060, AttrValue::Text("CT".into())),
                ],
                instances: vec![InstanceRecord {
                    attrs: vec![(0x0008_0018, AttrValue::Text("1.2.840.1.3".into()))],
                    transfer_syntax: "1.2.840.10008.1.2.1".into(),
                    storage_tier: TIER_HOT,
                    rejection_reason: crate::manifest::NOT_REJECTED_REASON,
                    blake3: [0x11; 32],
                    file_size: 1234,
                    frames: Vec::new(),
                }],
            }],
        };

        // Encode -> decode the "old" bytes exactly as it would have been
        // stored on disk; the decoder must not reject the missing tags.
        let bytes = old.encode().expect("encode old");
        let decoded = Manifest::decode(&bytes).expect("old manifest must decode");
        assert_eq!(decoded, old);
        // The new capture keys are absent (they sort last by design, but are
        // not fabricated during a plain decode).
        let series_attrs = &decoded.series[0].attrs;
        assert!(
            !series_attrs
                .iter()
                .any(|(t, _)| *t == tag::SERIES_DATE || *t == tag::STATION_NAME)
        );
        assert!(
            !decoded.series[0].instances[0]
                .attrs
                .iter()
                .any(|(t, _)| *t == tag::SOP_CLASS_UID || *t == tag::INSTANCE_NUMBER)
        );
    }

    /// rebuild: enriching a historical manifest preserves `arrived_at`
    /// (E-014 "recent patients" ordering) and fills the new capture tags where
    /// derivable, without duplicating identity attrs.
    #[test]
    fn rebuild_fills_capture_attrs_and_keeps_arrived_at() {
        // A historical manifest missing the expanded capture tags.
        let old = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 1234,
                arrived_at: 987_654_321,
            },
            study: StudyQido {
                attrs: vec![(0x0020_000d, AttrValue::Text("1.2.840.1.1".into()))],
                // One series (CT), one instance - matched by the decoded manifest.
                series_count: 1,
                instance_count: 1,
                modalities_in_study: Some("CT".into()),
            },
            series: vec![SeriesRecord {
                attrs: vec![
                    (0x0020_000e, AttrValue::Text("1.2.840.1.2".into())),
                    (0x0008_0060, AttrValue::Text("CT".into())),
                ],
                instances: vec![InstanceRecord {
                    attrs: vec![(0x0008_0018, AttrValue::Text("1.2.840.1.3".into()))],
                    transfer_syntax: "1.2.840.10008.1.2.1".into(),
                    storage_tier: TIER_HOT,
                    rejection_reason: crate::manifest::NOT_REJECTED_REASON,
                    blake3: [0x11; 32],
                    file_size: 1234,
                    frames: Vec::new(),
                }],
            }],
        };

        // Decode to the on-disk form, as the offline tool would.
        let bytes = old.encode().expect("encode old");
        let mut decoded = Manifest::decode(&bytes).expect("decode old");

        // Freshly extracted models re-derive the expanded capture attributes.
        let fresh = vec![SeriesModel {
            series_instance_uid: "1.2.840.1.2".into(),
            modality: "CT".into(),
            series_number: Some("1".into()),
            series_date: Some("20260802".into()),
            series_time: Some("103000".into()),
            scheduled_procedure_step_id: None,
            requested_procedure_id: None,
            station_name: Some("CT1".into()),
            manufacturer: None,
            institutional_department_name: None,
            operators_name: None,
            series_description: Some("Chest".into()),
            body_part_examined: Some("CHEST".into()),
            protocol_name: None,
            laterality: Some("L".into()),
            pps_start_date: Some("20260802".into()),
            pps_start_time: Some("104500".into()),
            instances: vec![InstanceModel {
                sop_instance_uid: "1.2.840.1.3".into(),
                sop_class_uid: Some("1.2.840.10008.5.1.4.1.1.2".into()),
                instance_number: Some("1".into()),
                blake3: [0x11; 32],
                storage_tier: TIER_HOT,
                transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                file_size: 1234,
                ..Default::default()
            }],
        }];

        rebuild_series_attrs(&mut decoded, &fresh);

        // `arrived_at` must survive the rebuild - the "recent patients" list
        // ordering (E-014) is unchanged.
        assert_eq!(decoded.sys.arrived_at, 987_654_321);
        assert_eq!(decoded.sys.total_pixel_bytes, 1234);
        // Study-level QIDO attrs are untouched.
        assert_eq!(decoded.study, old.study);

        // The expanded series tags are now present.
        let s0 = &decoded.series[0];
        fn text_attr(attrs: &[(u32, crate::manifest::AttrValue)], t: u32) -> Option<&str> {
            attrs
                .iter()
                .find(|(tag, _)| *tag == t)
                .and_then(|(_, v)| match v {
                    crate::manifest::AttrValue::Text(s) => Some(s.as_str()),
                    _ => None,
                })
        }
        fn uint_attr(attrs: &[(u32, crate::manifest::AttrValue)], t: u32) -> Option<u64> {
            attrs
                .iter()
                .find(|(tag, _)| *tag == t)
                .and_then(|(_, v)| match v {
                    crate::manifest::AttrValue::UInt(n) => Some(*n),
                    _ => None,
                })
        }
        assert_eq!(uint_attr(&s0.attrs, tag::SERIES_DATE), Some(20260802));
        assert_eq!(text_attr(&s0.attrs, tag::STATION_NAME), Some("CT1"));
        assert_eq!(text_attr(&s0.attrs, tag::SERIES_DESCRIPTION), Some("Chest"));
        assert_eq!(text_attr(&s0.attrs, tag::BODY_PART_EXAMINED), Some("CHEST"));
        assert_eq!(text_attr(&s0.attrs, tag::LATERALITY), Some("L"));
        // The PPS date/time capture tags survive a rebuild round-trip.
        assert_eq!(uint_attr(&s0.attrs, tag::PPS_START_DATE), Some(20260802));
        assert_eq!(text_attr(&s0.attrs, tag::PPS_START_TIME), Some("104500"));

        let i0 = &s0.instances[0];
        assert_eq!(
            text_attr(&i0.attrs, tag::SOP_CLASS_UID),
            Some("1.2.840.10008.5.1.4.1.1.2")
        );
        assert_eq!(text_attr(&i0.attrs, tag::INSTANCE_NUMBER), Some("1"));

        // Identity attrs are not duplicated by the merge (still one each).
        assert_eq!(
            s0.attrs
                .iter()
                .filter(|(t, _)| *t == tag::SERIES_INSTANCE_UID)
                .count(),
            1
        );
        assert_eq!(
            i0.attrs
                .iter()
                .filter(|(t, _)| *t == tag::SOP_INSTANCE_UID)
                .count(),
            1
        );
        // The rebuilt manifest is a valid, round-trippable manifest.
        let re = decoded.encode().expect("encode rebuilt");
        let _ = Manifest::decode(&re).expect("rebuilt must decode");
    }

    /// A second STOW for the same study accumulates instances instead of
    /// overwriting.
    #[test]
    fn second_stow_accumulates_instances() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();

        // First STOW: one instance.
        let mut b3_a = [0u8; 32];
        b3_a[0] = 0x11;
        let first = vec![SeriesModel {
            series_instance_uid: "1.2.840.1.2".into(),
            modality: "CT".into(),
            series_number: Some("1".into()),
            instances: vec![InstanceModel {
                sop_instance_uid: "1.2.840.1.3".into(),
                blake3: b3_a,
                storage_tier: TIER_HOT,
                transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                file_size: 1_000_000,
                ..Default::default()
            }],
            ..Default::default()
        }];
        write_study_manifest_full(&manifests, &mut a, &first).expect("first STOW");

        // Second STOW: another instance in the same series.
        let mut b3_b = [0u8; 32];
        b3_b[0] = 0x22;
        let second = vec![SeriesModel {
            series_instance_uid: "1.2.840.1.2".into(),
            modality: "CT".into(),
            series_number: Some("1".into()),
            instances: vec![InstanceModel {
                sop_instance_uid: "1.2.840.1.4".into(),
                blake3: b3_b,
                storage_tier: TIER_HOT,
                transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                file_size: 2_000_000,
                ..Default::default()
            }],
            ..Default::default()
        }];
        write_study_manifest_full(&manifests, &mut a, &second).expect("second STOW");

        let decoded = read_manifest(&manifests.join("1.2.840.1.1.manifest")).expect("read");

        // One series, but now two instances.
        assert_eq!(decoded.series.len(), 1);
        assert_eq!(decoded.series[0].instances.len(), 2);
        // Both instances are present.
        assert!(decoded.find_instance("1.2.840.1.3").is_some());
        assert!(decoded.find_instance("1.2.840.1.4").is_some());
        // Total bytes = sum of both.
        assert_eq!(decoded.sys.total_pixel_bytes, 3_000_000);
    }

    /// Repeating the same SOPInstanceUID is idempotent - no duplicates.
    #[test]
    fn duplicate_sop_instance_uid_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();

        let mut b3 = [0u8; 32];
        b3[0] = 0xAA;
        let series = vec![SeriesModel {
            series_instance_uid: "1.2.840.1.2".into(),
            modality: "CT".into(),
            series_number: Some("1".into()),
            instances: vec![InstanceModel {
                sop_instance_uid: "1.2.840.1.99".into(),
                blake3: b3,
                storage_tier: TIER_HOT,
                transfer_syntax_uid: "1.2.840.10008.1.2.1".into(),
                file_size: 500_000,
                ..Default::default()
            }],
            ..Default::default()
        }];

        // Same payload twice.
        write_study_manifest_full(&manifests, &mut a, &series).expect("first");
        write_study_manifest_full(&manifests, &mut a, &series).expect("second");

        let decoded = read_manifest(&manifests.join("1.2.840.1.1.manifest")).expect("read");
        // Still exactly one instance.
        assert_eq!(decoded.series.len(), 1);
        assert_eq!(decoded.series[0].instances.len(), 1);
        assert_eq!(decoded.sys.total_pixel_bytes, 500_000);
    }

    #[test]
    fn index_study_names_writes_patient_and_physician_grams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx = dir.path().join("idx");
        let a = StudyManifestAttrs {
            patient_name: Some("Иванов".into()),
            referring_physician: Some("Смирнов".into()),
            study_instance_uid: "1.2.840.1.1".into(),
            ..Default::default()
        };

        let written = index_study_names(&idx, &a, "1.2.840.1.1", "20260811");
        // "Иванов" -> 4 trigrams, "Смирнов" -> 5 trigrams.
        assert_eq!(written, 9);

        // Patient grams land under ngrams/, physician under physician/.
        let first_patient_gram = &crate::idx::ngram::name_ngrams("Иванов")[0];
        let patient_rows = crate::idx::ngram::read_study_uids(
            &idx,
            crate::idx::ngram::NameKind::Patient,
            first_patient_gram,
            "20260811",
        )
        .expect("read patient");
        assert_eq!(patient_rows, vec!["1.2.840.1.1"]);

        let first_phys_gram = &crate::idx::ngram::name_ngrams("Смирнов")[0];
        let phys_rows = crate::idx::ngram::read_study_uids(
            &idx,
            crate::idx::ngram::NameKind::Physician,
            first_phys_gram,
            "20260811",
        )
        .expect("read phys");
        assert_eq!(phys_rows, vec!["1.2.840.1.1"]);
    }

    #[test]
    fn index_study_names_skips_absent_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx = dir.path().join("idx");
        let a = StudyManifestAttrs {
            patient_name: None,
            referring_physician: None,
            study_instance_uid: "1.2.3".into(),
            ..Default::default()
        };
        assert_eq!(index_study_names(&idx, &a, "1.2.3", "20260811"), 0);
    }

    /// StudyID flows STOW->manifest -> study attrs -> QIDO
    /// JSON. A manifest written with `study_id` reads back with (0020,0010)
    /// in the study attrs and `write_study_json` emits it.
    #[test]
    fn study_id_roundtrips_manifest_to_qido() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut a = StudyManifestAttrs {
            patient_name: Some("ivanov".into()),
            patient_id: Some("PAT12".into()),
            modality: "CT".into(),
            study_instance_uid: "1.2.840.1.1".into(),
            study_id: Some("S100".into()),
            ..Default::default()
        };
        // STOW: write the study manifest (counts stamped = RAM-only).
        write_study_manifest_full(dir.path(), &mut a, &[]).expect("write manifest");
        assert_eq!(a.num_series, 0);
        assert_eq!(a.num_instances, 0);

        // Manifest -> study attrs: 0020,0010 present.
        let m = crate::manifest::read_manifest_study_only(&dir.path().join("1.2.840.1.1.manifest"))
            .expect("read manifest");
        let study_attr = m
            .study
            .attrs
            .iter()
            .find(|(t, _)| *t == crate::query::tag::STUDY_ID)
            .expect("00200010 present in study attrs");
        let sid = match &study_attr.1 {
            crate::manifest::AttrValue::Text(t) => t.as_str(),
            _ => panic!("StudyID attribute is not text"),
        };
        assert_eq!(sid, "S100", "StudyID value mismatch");

        // QIDO: write_study_json emits 00200010.
        let mut buf = [0u8; 512];
        let n = crate::query::json::write_study_json(&mut buf, &m.study, None, None).expect("fits");
        let got = String::from_utf8(buf[..n].to_vec()).unwrap();
        assert!(
            got.contains("\"00200010\":{\"Value\":[\"S100\"],\"vr\":\"SH\"}"),
            "QIDO must emit StudyID: {got}"
        );
    }

    // -- rejection marking (Stage 2 T2.2) -----------------------------------

    #[test]
    fn mark_rejected_flags_only_the_named_instances() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let path =
            write_study_manifest_full(&manifests, &mut a, &full_series()).expect("write study");

        // full_series has 1.2.840.1.3 in one series and 1.2.840.1.5 in the other.
        let changed = mark_rejected(
            &manifests,
            &a.study_instance_uid,
            &["1.2.840.1.5"],
            crate::manifest::QUALITY_REASON_CODE,
        )
        .expect("mark");
        assert_eq!(changed, 1, "exactly one instance changes state");

        let decoded = read_manifest(&path).expect("read");
        let flags: Vec<bool> = decoded
            .series
            .iter()
            .flat_map(|s| s.instances.iter().map(|i| i.is_rejected()))
            .collect();
        assert_eq!(
            flags,
            vec![false, true],
            "only the named instance is hidden"
        );
    }

    #[test]
    fn mark_rejected_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        write_study_manifest_full(&manifests, &mut a, &full_series()).expect("write study");

        let first = mark_rejected(
            &manifests,
            &a.study_instance_uid,
            &["1.2.840.1.3"],
            crate::manifest::QUALITY_REASON_CODE,
        )
        .expect("mark");
        assert_eq!(first, 1);
        // A second identical note changes nothing and must not fail.
        let second = mark_rejected(
            &manifests,
            &a.study_instance_uid,
            &["1.2.840.1.3"],
            crate::manifest::QUALITY_REASON_CODE,
        )
        .expect("mark again");
        assert_eq!(second, 0, "an already-rejected instance is a no-op");
    }

    #[test]
    fn mark_rejected_ignores_unknown_and_foreign_sop_uids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let path =
            write_study_manifest_full(&manifests, &mut a, &full_series()).expect("write study");
        let before = std::fs::read(&path).expect("read bytes");

        // A note naming an instance of another study must not touch this one.
        let changed = mark_rejected(
            &manifests,
            &a.study_instance_uid,
            &["9.9.9.9"],
            crate::manifest::QUALITY_REASON_CODE,
        )
        .expect("mark");
        assert_eq!(changed, 0);
        let after = std::fs::read(&path).expect("read bytes");
        assert_eq!(before, after, "no flag changed, so the file is untouched");
    }

    #[test]
    fn mark_rejected_refuses_a_tombstoned_study() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        write_study_manifest_full(&manifests, &mut a, &full_series()).expect("write study");
        // Drop the tombstone marker next to the manifest.
        let del = tombstone_path(&manifests, &a.study_instance_uid);
        std::fs::write(&del, b"").expect("tombstone");

        let err = mark_rejected(
            &manifests,
            &a.study_instance_uid,
            &["1.2.840.1.3"],
            crate::manifest::QUALITY_REASON_CODE,
        )
        .expect_err("a tombstoned study must refuse the mutation");
        assert!(matches!(err, StudyManifestError::StudyDeleted));
    }

    /// Restore clears the reason, and the re-encoded manifest is byte-for-byte
    /// what it would have been had the instance never been rejected. That is
    /// the strongest available statement of "restored": not merely visible,
    /// but indistinguishable in stored form from never-hidden, while the KOS
    /// note in CAS remains the historical record.
    #[test]
    fn mark_restored_returns_the_manifest_to_its_never_rejected_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let path =
            write_study_manifest_full(&manifests, &mut a, &full_series()).expect("write study");
        let never_rejected = std::fs::read(&path).expect("read bytes");

        // 1.2.840.1.3 lives in full_series; hide it with a real reason.
        let hidden = mark_rejected(&manifests, &a.study_instance_uid, &["1.2.840.1.3"], 113_038)
            .expect("mark");
        assert_eq!(hidden, 1);
        assert_ne!(
            std::fs::read(&path).expect("read bytes"),
            never_rejected,
            "a recorded reason must change the stored bytes"
        );

        let restored =
            mark_restored(&manifests, &a.study_instance_uid, &["1.2.840.1.3"]).expect("restore");
        assert_eq!(restored, 1, "exactly one instance changes state");
        assert_eq!(
            std::fs::read(&path).expect("read bytes"),
            never_rejected,
            "a restored instance re-encodes identically to a never-rejected one"
        );
    }

    #[test]
    fn mark_restored_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        write_study_manifest_full(&manifests, &mut a, &full_series()).expect("write study");
        mark_rejected(
            &manifests,
            &a.study_instance_uid,
            &["1.2.840.1.3"],
            crate::manifest::QUALITY_REASON_CODE,
        )
        .expect("mark");

        assert_eq!(
            mark_restored(&manifests, &a.study_instance_uid, &["1.2.840.1.3"]).expect("first"),
            1
        );
        // Restoring an already-visible instance must not fail and must not
        // claim a state change - that is what keeps the metric honest.
        assert_eq!(
            mark_restored(&manifests, &a.study_instance_uid, &["1.2.840.1.3"]).expect("second"),
            0
        );
    }

    /// Restore is scoped to the instances named, exactly like mark_rejected.
    #[test]
    fn mark_restored_leaves_other_hidden_instances_hidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        let mut a = attrs();
        let path =
            write_study_manifest_full(&manifests, &mut a, &full_series()).expect("write study");
        mark_rejected(
            &manifests,
            &a.study_instance_uid,
            &["1.2.840.1.3", "1.2.840.1.5"],
            113_037,
        )
        .expect("mark");

        let restored =
            mark_restored(&manifests, &a.study_instance_uid, &["1.2.840.1.3"]).expect("restore");
        assert_eq!(restored, 1);

        let flags: Vec<bool> = read_manifest(&path)
            .expect("read")
            .series
            .iter()
            .flat_map(|s| s.instances.iter().map(|i| i.is_rejected()))
            .collect();
        assert_eq!(
            flags,
            vec![false, true],
            "only the named instance is restored; the other stays hidden"
        );
    }

    #[test]
    fn mark_restored_with_no_manifest_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        assert_eq!(
            mark_restored(&manifests, "1.2.840.10008.9.9", &["1.2.3"]).expect("restore"),
            0
        );
    }

    #[test]
    fn mark_rejected_with_no_manifest_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifests = dir.path().join("manifests");
        std::fs::create_dir_all(&manifests).expect("mkdir");
        // A note may arrive before the study it names.
        let changed = mark_rejected(
            &manifests,
            "1.2.840.10008.9.9",
            &["1.2.3"],
            crate::manifest::QUALITY_REASON_CODE,
        )
        .expect("mark");
        assert_eq!(changed, 0);
    }
}
