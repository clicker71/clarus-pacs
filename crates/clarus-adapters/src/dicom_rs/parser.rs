// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Streaming DICOM parser - manual, zero-unsafe implementation.
//!
//! Parses DICOM Part 10 files element-by-element from a byte slice
//! **without loading pixel data** into memory.
//!
//! # Supported Transfer Syntaxes
//! - 1.2.840.10008.1.2       - Implicit VR Little Endian (default)
//! - 1.2.840.10008.1.2.1     - Explicit VR Little Endian
//! - 1.2.840.10008.1.2.2     - Explicit VR Big Endian
//! - 1.2.840.10008.1.2.4.*   - JPEG variants (Lossless, 2000, LS, Baseline)
//! - 1.2.840.10008.1.2.5     - RLE Lossless
//!
//! Compressed transfer syntaxes are parsed as Explicit VR Little Endian
//! for metadata extraction only. Pixel data is never decoded by the
//! metadata parser - the original TS UID is preserved for WADO transcoding.
//!
//! # Multi-frame DICOM
//!
//! Multi-frame DICOM files (Enhanced CT/MR, ultrasound cine, XA
//! angiography) store multiple frames in a single PixelData element
//! (7FE0,0010) using an offset table. Clarus v1.0.0 stops parsing at
//! the first PixelData tag - metadata beyond this point (including
//! per-frame attributes) is NOT extracted. This does NOT affect
//! QIDO/WADO/STOW which operate on study/series/instance-level tags.
//! Full multi-frame support is tracked as `#P7-MULTIFRAME` (v1.1+).

#![forbid(unsafe_code)]

use std::collections::HashMap;

use clarus_ports::dicom::{DicomError, DicomParser, DicomTag, InstanceMeta};

use super::tags;

/// Maximum plausible size for any meta-header element value.
/// DICOM Part 10 Section 7.1: meta-header ends at tag (0002,FFFF).
/// No standard meta-header element exceeds a few hundred bytes.
const MAX_META_ELEMENT_SIZE: usize = 4096;

// ===========================================================================
// Public type
// ===========================================================================

/// Streaming DICOM parser - stateless, thread-safe.
#[derive(Debug, Clone, Copy, Default)]
pub struct DicomRsParser;

impl DicomParser for DicomRsParser {
    fn extract_metadata(&self, data: &[u8]) -> Result<InstanceMeta, DicomError> {
        let mut reader = DicomReader::new(data)?;
        let mut meta = InstanceMeta::default();
        // 3008: past the PixelData element the tail is OPTIONAL (our own
        // Updated files append the (0400,0561) embed there) - tail elements
        // are captured for sanitization but never feed the metadata fields,
        // and a malformed tail stops the walk instead of failing the parse.
        let mut past_pixels = false;

        // Nested KEY scalars, gathered inside the sequence branch below and
        // moved onto `meta` after the walk: the sequence has to be read while
        // the element offsets are in hand, and the borrow of `reader.data`
        // there rules out writing `meta` directly.
        let mut step_id: Option<String> = None;
        let mut procedure_id: Option<String> = None;
        let mut other_patient_ids: Vec<String> = Vec::new();

        // Rejection-note capture (IOCM / RAD-66). The Document Title lives in
        // the ROOT content item's Concept Name Code Sequence, which may appear
        // before or after the SOP Class element, so both are gathered here and
        // the decision waits for the end of the walk. A note is rare: these
        // stay empty (no allocation) for every ordinary instance.
        let mut rejection_title: Option<(String, String)> = None;
        let mut rejection_sops: Vec<(String, String)> = Vec::new();

        loop {
            let element_start = reader.pos; // tag start (before read_tag)
            let element = match reader.next_element() {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => {
                    if past_pixels {
                        break;
                    }
                    return Err(e);
                }
            };
            let element_end = reader.pos; // value end + trailing pad
            let tag = element.tag;

            // INV-001: never read the pixel value (next_element skipped it).
            if tag == tags::PIXEL_DATA {
                // The element's TAG offset, not its value: a caller that must
                // insert header attributes has to land before the tag itself,
                // and this is the only place the parser knows that position.
                // Taken from the walk rather than a byte search, so a value
                // that happens to look like the tag cannot fool it.
                meta.pixel_data_offset = Some(element_start as u64);
                past_pixels = true;
                continue;
            }

            // 3008 hidden-PHI capture (TOP-LEVEL only): record the raw extent
            // of each (0400,0550)/(0400,0561) and scan its value region for
            // identifying tags with non-empty values.
            if tag == tags::MODIFIED_ATTRIBUTES_SEQ || tag == tags::ORIGINAL_ATTRIBUTES_SEQ {
                meta.original_attrs_ranges
                    .push((element_start, element_end));
                if range_has_identifying_attrs(reader.data, element.value_start, element.value_end)
                {
                    meta.hidden_identifying_attrs = true;
                }
            }

            // Derived-object reference capture: a presentation state or a
            // segmentation says which series and images it annotates. The
            // walk starts at every top-level sequence rather than at the
            // three reference tags by name, because a segmentation keeps its
            // per-segment series reference one level deeper, inside its
            // segment sequence - naming only the reference tags would find
            // the top-level sources and silently miss that one.
            //
            // This runs while the file is open, so the deletion cascade can
            // later find the object by where it points without reading DICOM
            // bytes again. A malformed sequence is skipped rather than
            // failing the parse: a missing reference only means the object is
            // invisible to the cascade, while refusing the STOW would lose an
            // image over a field nobody serves.
            if element.is_sequence() {
                // Targeted nested scalars, read in the SAME pass but on a
                // separate walk from the UID collection: the two answer
                // different questions and the scalar read must not change what
                // the cascade sees.
                //
                // PS3.18 Table 10.6.1-5 makes two keys nested inside (0040,0275)
                // REQUIRED series matching attributes, so its first item is read
                // for (0040,0009) and (0040,1001). (0010,1002) is MULTI-VALUED:
                // every item's (0010,0020) is a value of the same key.
                match tag {
                    tags::REQUEST_ATTRIBUTES_SEQUENCE => {
                        let mut pairs = [
                            (tags::SCHEDULED_PROCEDURE_STEP_ID, step_id.take()),
                            (tags::REQUESTED_PROCEDURE_ID, procedure_id.take()),
                        ];
                        collect_scalars_in_first_item(
                            reader.data,
                            element.value_start,
                            element.value_end,
                            &mut pairs,
                            reader.syntax,
                        );
                        step_id = pairs[0].1.take();
                        procedure_id = pairs[1].1.take();
                    }
                    tags::OTHER_PATIENT_IDS_SEQUENCE => {
                        collect_scalars_in_all_items(
                            reader.data,
                            element.value_start,
                            element.value_end,
                            tags::PATIENT_ID,
                            &mut other_patient_ids,
                            reader.syntax,
                        );
                    }
                    tags::CONCEPT_NAME_CODE_SEQUENCE => {
                        // Document Title as a CODE (PS3.3 C.17.2): the ST string
                        // in (0042,0010) is the human-readable form and is not
                        // reliably populated, so the coded value is what the
                        // rejection detector matches.
                        if rejection_title.is_none() {
                            rejection_title = first_code_item(
                                reader.data,
                                element.value_start,
                                element.value_end,
                                reader.syntax,
                            );
                        }
                    }
                    tags::CURRENT_REQUESTED_PROCEDURE_EVIDENCE_SEQUENCE => {
                        // PS3.3 C.17.6.2 Table C.17-3: the ONLY sequence naming
                        // the instances a key object applies to.
                        collect_referenced_sops_in_range(
                            reader.data,
                            element.value_start,
                            element.value_end,
                            &mut rejection_sops,
                            reader.syntax,
                        );
                    }
                    _ => {}
                }
                collect_uids_in_range(
                    reader.data,
                    element.value_start,
                    element.value_end,
                    &mut meta.referenced_series_uids,
                    &mut meta.referenced_sop_instance_uids,
                    reader.syntax,
                );
                continue;
            }

            if past_pixels {
                continue; // tail: sanitization capture only, no metadata
            }

            // Capture SpecificCharacterSet BEFORE any text tag is decoded.
            // DICOM PS3.5 Section 6.1.1 guarantees this tag appears first in dataset.
            if tag == tags::SPECIFIC_CHARACTER_SET {
                let val = &reader.data[element.value_start..element.value_end];
                reader.charset = opt_str(val, None); // charset value is always ASCII
            }

            let val = &reader.data[element.value_start..element.value_end];
            let cs = reader.charset.as_deref();

            match tag {
                tags::PATIENT_NAME => {
                    let components = parse_pn_components(val, cs)?;
                    meta.patient_name = components.to_dicom_string();
                }
                tags::PATIENT_ID => {
                    meta.patient_id = opt_str(val, cs).unwrap_or_default();
                }
                tags::PATIENT_ID_ISSUER => {
                    // LO (0010,0021). An empty value reads as "no issuer
                    // declared", which is the same statement as an absent
                    // element and is stored as None for that reason.
                    meta.patient_id_issuer = opt_str(val, cs).filter(|s| !s.is_empty());
                }
                tags::PATIENT_BIRTH_DATE => {
                    meta.patient_birth_date = opt_str(val, cs);
                }
                tags::PATIENT_SEX => {
                    meta.patient_sex = opt_str(val, cs);
                }
                tags::RESPONSIBLE_PERSON => {
                    // PN (0010,2297); empty/absent value stored as None (the
                    // manifest omits empty optional text for search).
                    let components = parse_pn_components(val, cs)?;
                    let s = components.to_dicom_string();
                    meta.responsible_person = if s.is_empty() { None } else { Some(s) };
                }
                tags::STUDY_DATE => {
                    meta.study_date = opt_str(val, cs);
                }
                tags::STUDY_TIME => {
                    meta.study_time = opt_str(val, cs);
                }
                tags::STUDY_DESCRIPTION => {
                    meta.study_description = opt_str(val, cs);
                }
                tags::ACCESSION_NUMBER => {
                    meta.accession_number = opt_str(val, cs);
                }
                tags::REFERRING_PHYSICIAN => {
                    meta.referring_physician = opt_str(val, cs);
                }
                tags::INSTITUTION_NAME => {
                    meta.institution_name = opt_str(val, cs);
                }
                tags::MANUFACTURER => {
                    meta.manufacturer = opt_str(val, cs);
                }
                tags::PERFORMING_PHYSICIAN => {
                    meta.performing_physician = opt_str(val, cs);
                }
                tags::NAME_OF_READING_PHYSICIANS => {
                    meta.name_of_reading_physicians = opt_str(val, cs);
                }
                tags::INSTITUTIONAL_DEPARTMENT_NAME => {
                    meta.institutional_department_name = opt_str(val, cs);
                }
                tags::OPERATORS_NAME => {
                    meta.operators_name = opt_str(val, cs);
                }
                tags::STUDY_INSTANCE_UID => {
                    meta.study_instance_uid = required_str(val, cs, "StudyInstanceUID")?;
                }
                tags::STUDY_ID => {
                    meta.study_id = opt_str(val, cs);
                }
                tags::REQUESTING_PHYSICIAN => {
                    meta.requesting_physician = opt_str(val, cs);
                }
                tags::REQUESTED_PROCEDURE_DESCRIPTION => {
                    meta.requested_procedure_description = opt_str(val, cs);
                }
                tags::SERIES_INSTANCE_UID => {
                    meta.series_instance_uid = required_str(val, cs, "SeriesInstanceUID")?;
                }
                tags::SERIES_NUMBER => {
                    meta.series_number = opt_int(val, cs);
                }
                tags::SERIES_DESCRIPTION => {
                    meta.series_description = opt_str(val, cs);
                }
                tags::SERIES_DATE => {
                    meta.series_date = opt_str(val, cs);
                }
                tags::SERIES_TIME => {
                    meta.series_time = opt_str(val, cs);
                }
                tags::PPS_START_DATE => {
                    meta.pps_start_date = opt_str(val, cs);
                }
                tags::PPS_START_TIME => {
                    meta.pps_start_time = opt_str(val, cs);
                }
                tags::STATION_NAME => {
                    meta.station_name = opt_str(val, cs);
                }
                tags::MODALITY => {
                    meta.modality = required_str(val, cs, "Modality")?;
                }
                tags::BODY_PART_EXAMINED => {
                    meta.body_part_examined = opt_str(val, cs);
                }
                tags::PROTOCOL_NAME => {
                    meta.protocol_name = opt_str(val, cs);
                }
                tags::LATERALITY => {
                    meta.laterality = opt_str(val, cs);
                }
                tags::SOP_INSTANCE_UID => {
                    meta.sop_instance_uid = required_str(val, cs, "SOPInstanceUID")?;
                }
                tags::SOP_CLASS_UID => {
                    meta.sop_class_uid = opt_str(val, cs);
                }
                tags::INSTANCE_NUMBER => {
                    meta.instance_number = opt_int(val, cs);
                }
                tags::ROWS => {
                    meta.rows = opt_u16(val);
                }
                tags::COLUMNS => {
                    meta.cols = opt_u16(val);
                }
                tags::TRANSFER_SYNTAX_UID => {
                    // (0002,0010) - meta-header tag, always ASCII
                    meta.transfer_syntax_uid = opt_str(val, None);
                }
                tags::SPECIFIC_CHARACTER_SET => {
                    // Already captured above - do nothing
                }
                _ => {}
            }
        }

        verify_required(&meta)?;
        // G-S5: capture TransferSyntaxUID from the meta-header
        meta.transfer_syntax_uid = reader.transfer_syntax_uid.clone();
        // The nested key scalars gathered in the sequence branch. They are
        // moved here rather than written in place because the walk holds a
        // borrow of `reader.data` at that point.
        meta.scheduled_procedure_step_id = step_id;
        meta.requested_procedure_id = procedure_id;
        meta.other_patient_ids = other_patient_ids;

        // A rejection note is a Key Object Selection whose coded Document
        // Title is a recognised reason. Everything else - including a KOS
        // with an unknown title - leaves the field empty and is stored as an
        // ordinary document.
        let is_kos = meta.sop_class_uid.as_deref()
            == Some(clarus_ports::rejection::KEY_OBJECT_SELECTION_SOP_CLASS);
        if let Some((code_value, scheme)) = &rejection_title
            && is_kos
            && clarus_ports::rejection::reason_for(code_value, scheme).is_some()
        {
            meta.rejection_referenced_sops = rejection_sops;
            // The row was already matched above; a code that cannot be
            // expressed as the persisted integer leaves the field at
            // NOT_REJECTED, which reads as "visible" rather than as a
            // half-applied hide.
            meta.rejection_reason_code = clarus_ports::rejection::reason_code_to_u32(code_value)
                .unwrap_or(clarus_ports::rejection::NOT_REJECTED);
        }
        Ok(meta)
    }

    fn extract_search_tags(&self, data: &[u8]) -> Result<HashMap<DicomTag, String>, DicomError> {
        let mut reader = DicomReader::new(data)?;
        let mut map = HashMap::with_capacity(tags::QIDO_TAGS.len());

        while let Some(element) = reader.next_element()? {
            let tag = element.tag;

            if tag == tags::PIXEL_DATA {
                break;
            }

            // Capture charset
            if tag == tags::SPECIFIC_CHARACTER_SET {
                let val = &reader.data[element.value_start..element.value_end];
                reader.charset = opt_str(val, None);
            }

            if tags::QIDO_TAGS.contains(&tag) {
                let val = &reader.data[element.value_start..element.value_end];
                let cs = reader.charset.as_deref();
                if let Some(s) = opt_str(val, cs) {
                    map.insert(tag, s);
                }
            }
        }

        Ok(map)
    }
}

impl DicomRsParser {
    /// Extract the raw bytes for a specific DICOM tag.
    ///
    /// Scans the DICOM byte stream until the requested tag is found
    /// or PixelData is encountered. Returns the raw value bytes.
    ///
    /// Returns `None` if the tag is not found before PixelData or EOF.
    pub fn extract_tag_value(
        &self,
        data: &[u8],
        target_tag: DicomTag,
    ) -> Result<Option<Vec<u8>>, DicomError> {
        let mut reader = DicomReader::new(data)?;

        while let Some(element) = reader.next_element()? {
            if element.tag == tags::PIXEL_DATA {
                // For PixelData, extract it even though the normal parser stops.
                if target_tag == tags::PIXEL_DATA {
                    let val = &reader.data[element.value_start..element.value_end];
                    return Ok(Some(val.to_vec()));
                }
                break;
            }

            if element.tag == target_tag {
                let val = &reader.data[element.value_start..element.value_end];
                return Ok(Some(val.to_vec()));
            }

            // Capture charset for proper text decoding of subsequent tags.
            if element.tag == tags::SPECIFIC_CHARACTER_SET {
                let val = &reader.data[element.value_start..element.value_end];
                reader.charset = opt_str(val, None);
            }
        }

        Ok(None)
    }

    /// (C1 22.08) Return the (0002,0010) TransferSyntaxUID from the meta-header
    /// without full metadata validation. `None` means "no meta-header / not
    /// parseable" (e.g. DIMSE data with "DICM" prepended) - the caller treats
    /// that as compressed (conservative 406).
    pub fn transfer_syntax_uid(&self, data: &[u8]) -> Option<String> {
        DicomReader::new(data).ok()?.transfer_syntax_uid
    }
}

fn verify_required(meta: &InstanceMeta) -> Result<(), DicomError> {
    // PatientName (0010,0010) and PatientID (0010,0020) are DICOM Type 2 -
    // may be empty (anonymized studies). The server coerce logic generates
    // placeholders for empty values to keep SQLite indexes healthy.
    if meta.study_instance_uid.is_empty() {
        return Err(DicomError::MissingRequiredTag(
            tags::STUDY_INSTANCE_UID.0,
            tags::STUDY_INSTANCE_UID.1,
        ));
    }
    if meta.series_instance_uid.is_empty() {
        return Err(DicomError::MissingRequiredTag(
            tags::SERIES_INSTANCE_UID.0,
            tags::SERIES_INSTANCE_UID.1,
        ));
    }
    if meta.sop_instance_uid.is_empty() {
        return Err(DicomError::MissingRequiredTag(
            tags::SOP_INSTANCE_UID.0,
            tags::SOP_INSTANCE_UID.1,
        ));
    }
    if meta.modality.is_empty() {
        return Err(DicomError::MissingRequiredTag(
            tags::MODALITY.0,
            tags::MODALITY.1,
        ));
    }
    Ok(())
}

/// 3008 hidden-PHI detection inside a captured (0400,0550)/(0400,0561) value
/// region: true when any of the four identifying tags appears with a
/// non-empty value. Tags are always little-endian; each candidate is
/// interpreted under ALL plausible value-length encodings (explicit VR
/// short, explicit VR long, implicit; little- and big-endian), so no real
/// identifying value is missed. A false positive is harmless: the WHOLE
/// captured top-level range is excised regardless of where the hit is.
fn range_has_identifying_attrs(data: &[u8], start: usize, end: usize) -> bool {
    const IDENTIFYING: [DicomTag; 4] = [
        tags::PATIENT_NAME,
        tags::PATIENT_ID,
        tags::PATIENT_BIRTH_DATE,
        tags::PATIENT_SEX,
    ];
    let end = end.min(data.len());
    let mut i = start;
    while i + 8 <= end {
        let g = read_u16_le(data, i);
        let e = read_u16_le(data, i + 2);
        if IDENTIFYING.contains(&(g, e)) {
            // Implicit: 4-byte length right after the tag (LE / BE).
            if i + 8 <= end {
                let len = read_u32_le(data, i + 4) as usize;
                if i + 8 + len <= end && non_empty_value(data, i + 8, i + 8 + len) {
                    return true;
                }
                let len_be = read_u32_be(data, i + 4) as usize;
                if i + 8 + len_be <= end && non_empty_value(data, i + 8, i + 8 + len_be) {
                    return true;
                }
            }
            // Explicit VR short: 2-byte length (LE / BE).
            if i + 8 <= end {
                let len = read_u16_le(data, i + 6) as usize;
                if i + 8 + len <= end && non_empty_value(data, i + 8, i + 8 + len) {
                    return true;
                }
                let len_be = read_u16_be(data, i + 6) as usize;
                if i + 8 + len_be <= end && non_empty_value(data, i + 8, i + 8 + len_be) {
                    return true;
                }
            }
            // Explicit VR long: reserved + 4-byte length (LE / BE).
            if i + 12 <= end {
                let len = read_u32_le(data, i + 8) as usize;
                if i + 12 + len <= end && non_empty_value(data, i + 12, i + 12 + len) {
                    return true;
                }
                let len_be = read_u32_be(data, i + 8) as usize;
                if i + 12 + len_be <= end && non_empty_value(data, i + 12, i + 12 + len_be) {
                    return true;
                }
            }
        }
        i += 2; // tags are 2-aligned; the scan is cheap on small regions
    }
    false
}

/// A "non-empty" DICOM string value: at least one byte outside space/NUL.
fn non_empty_value(data: &[u8], start: usize, end: usize) -> bool {
    data[start..end].iter().any(|&b| b != 0x00 && b != 0x20)
}

/// Read named scalar elements out of the FIRST item of a sequence.
///
/// The counterpart of `collect_uids_in_range` for the nested KEY attributes:
/// PS3.18 Table 10.6.1-5 states `00400275.00400009` and `00400275.00401001`,
/// and both live in one item of Request Attributes Sequence, so the path is
/// resolved once and the two scalars come out together. Only the first item is
/// read, because the matching keys of that table are series-level singletons
/// and a second item would be a different performed step.
///
/// A scalar that cannot be read is left as `None`, which the callers treat as
/// "the instance does not state it" - the same tolerant rule the UID walk
/// follows, so a malformed sequence cannot fail a STOW.
fn collect_scalars_in_first_item(
    data: &[u8],
    mut pos: usize,
    end: usize,
    wanted: &mut [(DicomTag, Option<String>)],
    syntax: TransferSyntax,
) {
    // Descend to the first item; everything before it is item headers of
    // nested sequences the caller did not ask about.
    while pos + 8 <= end {
        let tag = (
            read_u16_syntax(data, pos, syntax),
            read_u16_syntax(data, pos + 2, syntax),
        );
        if tag == tags::ITEM {
            let length = read_u32_syntax(data, pos + 4, syntax);
            pos += 8;
            let item_end = if length == tags::UNDEFINED_LENGTH {
                undefined_extent(data, pos, end).unwrap_or(end)
            } else {
                pos.saturating_add(length as usize).min(end)
            };
            // Read the elements of this item; the first match wins, so a
            // repeated element cannot overwrite the value the key names.
            read_scalars_in_range(data, pos, item_end, wanted, syntax);
            return;
        }
        if tag == tags::ITEM_DELIMITATION || tag == tags::SEQUENCE_DELIMITATION {
            return;
        }
        // An element before the first item: step over its value.
        let Some(next) = element_extent_end(data, pos, end, syntax) else {
            return;
        };
        pos = next;
    }
}

/// Read one scalar element out of EVERY item of a sequence.
///
/// Other Patient IDs (0010,1002) is multi-valued: the key `00101002.00100020`
/// names the Path's value in ANY item, so every item that states one
/// contributes. Values are collected in file order, which is the order the
/// sequence states them.
fn collect_scalars_in_all_items(
    data: &[u8],
    mut pos: usize,
    end: usize,
    wanted: DicomTag,
    values: &mut Vec<String>,
    syntax: TransferSyntax,
) {
    while pos + 8 <= end {
        let tag = (
            read_u16_syntax(data, pos, syntax),
            read_u16_syntax(data, pos + 2, syntax),
        );
        if tag == tags::SEQUENCE_DELIMITATION {
            return;
        }
        if tag == tags::ITEM {
            let length = read_u32_syntax(data, pos + 4, syntax);
            pos += 8;
            let item_end = if length == tags::UNDEFINED_LENGTH {
                undefined_extent(data, pos, end).unwrap_or(end)
            } else {
                pos.saturating_add(length as usize).min(end)
            };
            let mut pairs = [(wanted, None)];
            read_scalars_in_range(data, pos, item_end, &mut pairs, syntax);
            if let Some(v) = pairs[0].1.take() {
                values.push(v);
            }
            pos = item_end;
            continue;
        }
        let Some(next) = element_extent_end(data, pos, end, syntax) else {
            return;
        };
        pos = next;
    }
}

/// Read the wanted scalar elements out of one dataset range.
///
/// Nested sequences are skipped rather than descended into: the callers ask
/// for elements that live directly in the item, and descending would let an
/// inner sequence's element of the same tag masquerade as the item's own.
fn read_scalars_in_range(
    data: &[u8],
    mut pos: usize,
    end: usize,
    wanted: &mut [(DicomTag, Option<String>)],
    syntax: TransferSyntax,
) {
    let implicit = syntax == TransferSyntax::ImplicitVrLE;
    while pos + 8 <= end {
        let tag = (
            read_u16_syntax(data, pos, syntax),
            read_u16_syntax(data, pos + 2, syntax),
        );
        if tag == tags::ITEM_DELIMITATION || tag == tags::SEQUENCE_DELIMITATION {
            return;
        }
        // PS3.5 6.2: Implicit VR carries NO VR field - the four bytes after
        // the tag are the length. Reading them as a VR took the low half of
        // that length as VR letters, so the walk landed in the middle of the
        // value and every nested scalar came back empty: QIDO returned
        // nothing for an Implicit VR instance that states its keys.
        let (length, value_start) = if implicit {
            (read_u32_syntax(data, pos + 4, syntax), pos + 8)
        } else {
            let vr = [data[pos + 4], data[pos + 5]];
            if is_extended_vr(vr) {
                if pos + 12 > end {
                    return;
                }
                (read_u32_syntax(data, pos + 8, syntax), pos + 12)
            } else {
                (u32::from(read_u16_syntax(data, pos + 6, syntax)), pos + 8)
            }
        };
        // A nested sequence's extent is skipped whole, whatever its length
        // form; only scalars at this level are candidates.
        let (value_end, next) = if length == tags::UNDEFINED_LENGTH {
            if value_start > end {
                return;
            }
            let sq_end = undefined_extent(data, value_start, end).unwrap_or(end);
            (value_start, sq_end)
        } else {
            let value_end = value_start.saturating_add(length as usize);
            if value_end > end {
                return;
            }
            // An odd-length value is followed by a pad byte, so the next
            // element starts one byte later than the value ends (PS3.5 7.1.1).
            // Stepping by the value length alone would read the pad as the
            // first byte of the next tag.
            let padded = value_end + (length as usize % 2);
            (value_end, padded.min(end))
        };
        for (wanted_tag, slot) in wanted.iter_mut() {
            if *wanted_tag == tag && slot.is_none() {
                // The nested scalars are SH and LO - single-byte VRs of
                // ASCII text - so the default charset is the right reading.
                *slot = opt_str(&data[value_start..value_end], None).filter(|s| !s.is_empty());
            }
        }
        pos = next;
    }
}

/// Read the first `(CodeValue, CodingSchemeDesignator)` pair out of a code
/// sequence such as Concept Name Code Sequence (0040,A043).
///
/// The root content item's concept name is a code sequence of one item; the
/// Document Title is that item's (0008,0100)/(0008,0102) pair. Only the first
/// item is read - a second concept would be a different qualification, and the
/// detector matches one title at a time. Either half may be absent, in which
/// case the pair is `None` and the note is treated as an unknown title rather
/// than guessed at.
fn first_code_item(
    data: &[u8],
    mut pos: usize,
    end: usize,
    syntax: TransferSyntax,
) -> Option<(String, String)> {
    while pos + 8 <= end {
        let tag = (
            read_u16_syntax(data, pos, syntax),
            read_u16_syntax(data, pos + 2, syntax),
        );
        if tag == tags::ITEM {
            let length = read_u32_syntax(data, pos + 4, syntax);
            pos += 8;
            let item_end = if length == tags::UNDEFINED_LENGTH {
                undefined_extent(data, pos, end).unwrap_or(end)
            } else {
                pos.saturating_add(length as usize).min(end)
            };
            let mut pair = [
                (tags::CODE_VALUE, None),
                (tags::CODING_SCHEME_DESIGNATOR, None),
            ];
            read_scalars_in_range(data, pos, item_end, &mut pair, syntax);
            return match (pair[0].1.take(), pair[1].1.take()) {
                (Some(value), Some(scheme)) => Some((value, scheme)),
                _ => None,
            };
        }
        if tag == tags::ITEM_DELIMITATION || tag == tags::SEQUENCE_DELIMITATION {
            return None;
        }
        pos = element_extent_end(data, pos, end, syntax)?;
    }
    None
}

/// Collect `(reference_sop_class_uid, referenced_sop_instance_uid)` pairs from
/// a Current Requested Procedure Evidence Sequence (0040,A375).
///
/// Shape per PS3.3 Table C.17-3: evidence item -> Referenced Series Sequence
/// (0008,1115) -> Referenced SOP Sequence (0008,1199) -> (0008,1150) /
/// (0008,1155). The walk is by tag rather than by nesting depth, so a vendor
/// that omits an intermediate level still yields its references instead of
/// being silently dropped.
///
/// A pair is pushed only when BOTH UIDs are present in the same Referenced SOP
/// item. The class travels with the instance because the marking step applies
/// the per-reason scope without re-reading the referenced object; a pair
/// missing its class could not be scope-checked and is skipped rather than
/// marked optimistically.
fn collect_referenced_sops_in_range(
    data: &[u8],
    mut pos: usize,
    end: usize,
    out: &mut Vec<(String, String)>,
    syntax: TransferSyntax,
) {
    let implicit = syntax == TransferSyntax::ImplicitVrLE;
    while pos + 8 <= end {
        let tag = (
            read_u16_syntax(data, pos, syntax),
            read_u16_syntax(data, pos + 2, syntax),
        );
        if tag == tags::SEQUENCE_DELIMITATION || tag == tags::ITEM_DELIMITATION {
            return;
        }
        // An item: descend into it and keep looking for Referenced SOP
        // Sequences (they sit two levels down).
        if tag == tags::ITEM {
            let length = read_u32_syntax(data, pos + 4, syntax);
            pos += 8;
            let item_end = if length == tags::UNDEFINED_LENGTH {
                undefined_extent(data, pos, end).unwrap_or(end)
            } else {
                pos.saturating_add(length as usize).min(end)
            };
            collect_referenced_sops_in_range(data, pos, item_end, out, syntax);
            pos = item_end;
            continue;
        }
        // The hierarchical macro nests one level between the evidence item and
        // the Referenced SOP Sequence: Referenced Series Sequence (0008,1115).
        // It has to be descended into like an item, otherwise the walker steps
        // over the series sequence and never sees the SOP items inside it.
        if tag == tags::REFERENCED_SERIES_SEQUENCE {
            // PS3.5 6.2: in Implicit VR there is no VR field at all, so the
            // 4-byte length sits directly after the tag.
            let (length, value_start) = if implicit {
                (read_u32_syntax(data, pos + 4, syntax), pos + 8)
            } else {
                let vr = [data[pos + 4], data[pos + 5]];
                if is_extended_vr(vr) {
                    if pos + 12 > end {
                        return;
                    }
                    (read_u32_syntax(data, pos + 8, syntax), pos + 12)
                } else {
                    (u32::from(read_u16_syntax(data, pos + 6, syntax)), pos + 8)
                }
            };
            let value_end = if length == tags::UNDEFINED_LENGTH {
                undefined_extent(data, value_start, end).unwrap_or(end)
            } else {
                value_start.saturating_add(length as usize).min(end)
            };
            collect_referenced_sops_in_range(data, value_start, value_end, out, syntax);
            pos = value_end;
            continue;
        }
        if tag == tags::REFERENCED_SOP_SEQUENCE {
            // Read the header the same way the generic walker does: an SQ is
            // an extended-VR element, so its 4-byte length sits AFTER the two
            // reserved bytes (pos + 8) and the value starts at pos + 12.
            // Reading the length at pos + 4 would take the reserved bytes.
            // Implicit VR has neither, so the length is at pos + 4.
            let (length, value_start) = if implicit {
                (read_u32_syntax(data, pos + 4, syntax), pos + 8)
            } else {
                let vr = [data[pos + 4], data[pos + 5]];
                if is_extended_vr(vr) {
                    if pos + 12 > end {
                        return;
                    }
                    (read_u32_syntax(data, pos + 8, syntax), pos + 12)
                } else {
                    (u32::from(read_u16_syntax(data, pos + 6, syntax)), pos + 8)
                }
            };
            let value_end = if length == tags::UNDEFINED_LENGTH {
                undefined_extent(data, value_start, end).unwrap_or(end)
            } else {
                value_start.saturating_add(length as usize).min(end)
            };
            collect_sop_items(data, value_start, value_end, out, syntax);
            pos = value_end;
            continue;
        }
        match element_extent_end(data, pos, end, syntax) {
            Some(next) => pos = next,
            None => return,
        }
    }
}

/// Read the (0008,1150)/(0008,1155) pairs out of the items of one Referenced
/// SOP Sequence value region.
fn collect_sop_items(
    data: &[u8],
    mut pos: usize,
    end: usize,
    out: &mut Vec<(String, String)>,
    syntax: TransferSyntax,
) {
    while pos + 8 <= end {
        let tag = (
            read_u16_syntax(data, pos, syntax),
            read_u16_syntax(data, pos + 2, syntax),
        );
        if tag == tags::SEQUENCE_DELIMITATION {
            return;
        }
        if tag == tags::ITEM {
            let length = read_u32_syntax(data, pos + 4, syntax);
            pos += 8;
            let item_end = if length == tags::UNDEFINED_LENGTH {
                undefined_extent(data, pos, end).unwrap_or(end)
            } else {
                pos.saturating_add(length as usize).min(end)
            };
            let mut pair = [
                (tags::REFERENCED_SOP_CLASS_UID, None),
                (tags::REFERENCED_SOP_INSTANCE_UID, None),
            ];
            read_scalars_in_range(data, pos, item_end, &mut pair, syntax);
            if let (Some(class_uid), Some(instance_uid)) = (pair[0].1.take(), pair[1].1.take()) {
                out.push((class_uid, instance_uid));
            }
            pos = item_end;
            continue;
        }
        match element_extent_end(data, pos, end, syntax) {
            Some(next) => pos = next,
            None => return,
        }
    }
}

/// The byte offset just past the element that starts at `pos`.
///
/// Used to step over an element the caller does not want without reading its
/// value: an ordinary element ends after its value, an undefined-length one
/// after its SequenceDelimitationItem.
///
/// `syntax` decides the header shape, exactly as [`read_scalars_in_range`]
/// does. Reading a VR from an Implicit VR element took the low half of the
/// length as VR letters, so a single unrelated element before the sequence
/// item desynchronised the walk and the wanted scalar came back empty.
fn element_extent_end(
    data: &[u8],
    pos: usize,
    end: usize,
    syntax: TransferSyntax,
) -> Option<usize> {
    if pos + 8 > end {
        return None;
    }
    // PS3.5 6.2: an Implicit VR element has no VR field at all.
    let (length, value_start) = if syntax == TransferSyntax::ImplicitVrLE {
        (read_u32_syntax(data, pos + 4, syntax), pos + 8)
    } else {
        let vr = [data[pos + 4], data[pos + 5]];
        if is_extended_vr(vr) {
            if pos + 12 > end {
                return None;
            }
            (read_u32_syntax(data, pos + 8, syntax), pos + 12)
        } else {
            (u32::from(read_u16_syntax(data, pos + 6, syntax)), pos + 8)
        }
    };
    if length == tags::UNDEFINED_LENGTH {
        let sq_end = undefined_extent(data, value_start, end)?;
        return Some(sq_end);
    }
    // Include the pad byte of an odd-length value (PS3.5 7.1.1), so the
    // caller lands on the next element rather than inside its header.
    let next = value_start
        .checked_add(length as usize)?
        .checked_add(length as usize % 2)?;
    (next <= end).then_some(next)
}

/// Maximum nesting depth for the UID walk. See [`collect_uids_within`].
const MAX_UID_WALK_DEPTH: usize = 64;

/// Collect the series and instance UIDs a sequence points at.
///
/// Used for the derived-object capture. A presentation state names what it
/// annotates in `ReferencedSeriesSequence` and `ReferencedImageSequence`; a
/// segmentation names its source images in `SourceImageSequence` and its
/// segments' series in the `ReferencedSeriesSequence` nested under each
/// segment. The shapes differ but the walk is the same, so one reader serves
/// all of them: the sequence is read as items, each item as elements, and any
/// element that is itself a sequence is descended into.
///
/// Nothing here can fail the parse. A reference that cannot be read leaves the
/// list short, which makes the object invisible to the deletion cascade;
/// refusing the instance would lose an image over a field nothing serves.
fn collect_uids_in_range(
    data: &[u8],
    pos: usize,
    end: usize,
    series_uids: &mut Vec<String>,
    instance_uids: &mut Vec<String>,
    syntax: TransferSyntax,
) {
    collect_uids_within(data, pos, end, series_uids, instance_uids, 0, syntax)
}

/// Bounded walk behind [`collect_uids_in_range`].
///
/// Recursion depth is bounded by [`MAX_UID_WALK_DEPTH`]: a crafted STOW can
/// nest undefined-length items until the stack runs out, and a 1 MiB payload
/// buys ~50k levels at 8 bytes each. Real objects nest a handful of levels
/// (source image sequence inside a segment inside a segmentation), so 64 is
/// past anything PS3.5/PS3.3 produces while keeping the walk's work bounded.
fn collect_uids_within(
    data: &[u8],
    mut pos: usize,
    end: usize,
    series_uids: &mut Vec<String>,
    instance_uids: &mut Vec<String>,
    depth: usize,
    syntax: TransferSyntax,
) {
    if depth > MAX_UID_WALK_DEPTH {
        return;
    }
    let implicit = syntax == TransferSyntax::ImplicitVrLE;
    while pos + 8 <= end {
        let tag = (
            read_u16_syntax(data, pos, syntax),
            read_u16_syntax(data, pos + 2, syntax),
        );

        // An item delimiter ends the enclosing item, and a sequence delimiter
        // ends the enclosing sequence. Neither carries a value.
        if tag == tags::ITEM_DELIMITATION || tag == tags::SEQUENCE_DELIMITATION {
            pos += 8;
            continue;
        }

        // An item header is tag + a 4-byte length with no VR. Its body is a
        // dataset, read by continuing the walk into it rather than skipping,
        // so elements inside the item are visited.
        if tag == tags::ITEM {
            let length = read_u32_le(data, pos + 4);
            pos += 8;
            if length == tags::UNDEFINED_LENGTH {
                // Undefined-length item: its body runs to its own
                // ItemDelimitationItem. Descending with the item's own
                // extent keeps the recursion depth bounded by the STRUCTURAL
                // nesting; descending with the parent's end instead would let
                // every nested undefined-length sequence inside re-walk the
                // whole remaining parent (20.09 field stack overflow on a
                // 432-frame SEG).
                let item_end = undefined_extent(data, pos, end).unwrap_or(end);
                collect_uids_within(
                    data,
                    pos,
                    item_end.saturating_sub(8),
                    series_uids,
                    instance_uids,
                    depth + 1,
                    syntax,
                );
                pos = item_end;
            } else {
                let item_end = pos.saturating_add(length as usize).min(end);
                collect_uids_within(
                    data,
                    pos,
                    item_end,
                    series_uids,
                    instance_uids,
                    depth + 1,
                    syntax,
                );
                pos = item_end;
            }
            continue;
        }

        // An ordinary element: read its header, then decide by VR whether it
        // is a value to keep or a sequence to descend into.
        //
        // PS3.5 6.2: an Implicit VR dataset has NO VR bytes at the element -
        // the two bytes a VR would occupy are the start of the 32-bit length.
        // Reading them as a VR put the walk two bytes out at every element and
        // desynchronised the whole descent, so sequence content was lost.
        let (vr_sq, extended) = if implicit {
            (false, false)
        } else {
            let vr = [data[pos + 4], data[pos + 5]];
            (&vr == b"SQ", is_extended_vr(vr))
        };
        let (length, value_start) = if implicit {
            if pos + 8 > end {
                return;
            }
            (read_u32_syntax(data, pos + 4, syntax), pos + 8)
        } else if extended {
            if pos + 12 > end {
                return;
            }
            (read_u32_syntax(data, pos + 8, syntax), pos + 12)
        } else {
            (u32::from(read_u16_syntax(data, pos + 6, syntax)), pos + 8)
        };
        if length == tags::UNDEFINED_LENGTH {
            // Undefined length: the extent runs to the element's own
            // SequenceDelimitationItem. Descend with that extent - not the
            // parent's - so the recursion depth tracks structural nesting
            // instead of re-walking the parent remainder per sequence.
            if value_start > end {
                return;
            }
            let sq_end = undefined_extent(data, value_start, end).unwrap_or(end);
            collect_uids_within(
                data,
                value_start,
                sq_end.saturating_sub(8),
                series_uids,
                instance_uids,
                depth + 1,
                syntax,
            );
            pos = sq_end;
            continue;
        }
        let value_end = value_start.saturating_add(length as usize);
        if value_end > end {
            return;
        }
        let value = &data[value_start..value_end];
        match tag {
            tags::SERIES_INSTANCE_UID => {
                if let Some(uid) = opt_str(value, None) {
                    push_unique(series_uids, uid);
                }
            }
            tags::REFERENCED_SOP_INSTANCE_UID => {
                if let Some(uid) = opt_str(value, None) {
                    push_unique(instance_uids, uid);
                }
            }
            _ => {
                // A nested sequence holds further references - a presentation
                // state's image list sits inside its referenced series, and a
                // segmentation's segment list sits inside its segment
                // sequence. Descending is what makes those reachable; reading
                // the sequence as an opaque value would record the outer
                // series and silently miss every image inside it.
                if vr_sq || implicit {
                    collect_uids_within(
                        data,
                        value_start,
                        value_end,
                        series_uids,
                        instance_uids,
                        depth + 1,
                        syntax,
                    );
                }
            }
        }
        // Odd lengths are padded to an even boundary (PS3.5 Section 7.1).
        pos = value_end + (length as usize % 2);
    }
}

/// Whether a VR is one of the extended-length forms that carry a 32-bit
/// length after two reserved bytes (PS3.5 Section 7.1.2).
fn is_extended_vr(vr: [u8; 2]) -> bool {
    matches!(
        &vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    )
}

/// Locate the end (byte position AFTER the delimiter) of the undefined-length
/// item or sequence whose content starts at `start`.
///
/// The walk mirrors [`collect_uids_in_range`]'s element grammar (explicit VR,
/// items, delimiters) and counts nesting so that inner delimiters unwind
/// without being mistaken for the enclosing one. Returns `None` when the
/// enclosing delimiter is not found before `end` (malformed input - the
/// caller falls back to the outer bound, the same as the previous behaviour).
fn undefined_extent(data: &[u8], mut pos: usize, end: usize) -> Option<usize> {
    let mut nesting: u32 = 0;
    while pos + 8 <= end {
        let tag = (read_u16_le(data, pos), read_u16_le(data, pos + 2));
        if tag == tags::ITEM {
            let length = read_u32_le(data, pos + 4);
            pos += 8;
            if length == tags::UNDEFINED_LENGTH {
                nesting += 1;
            } else {
                pos = pos.saturating_add(length as usize).min(end);
            }
            continue;
        }
        if tag == tags::ITEM_DELIMITATION || tag == tags::SEQUENCE_DELIMITATION {
            pos += 8;
            if nesting == 0 {
                return Some(pos);
            }
            nesting = nesting.saturating_sub(1);
            continue;
        }
        let vr = [data[pos + 4], data[pos + 5]];
        let extended = is_extended_vr(vr);
        let (length, value_start) = if extended {
            if pos + 12 > end {
                return None;
            }
            (read_u32_le(data, pos + 8), pos + 12)
        } else {
            (u32::from(read_u16_le(data, pos + 6)), pos + 8)
        };
        if length == tags::UNDEFINED_LENGTH {
            nesting += 1;
            pos = value_start;
            continue;
        }
        let value_end = value_start.saturating_add(length as usize);
        if value_end > end {
            return None;
        }
        pos = value_end + (length as usize % 2);
    }
    None
}

/// Append `uid` unless it is already present.
///
/// A segmentation may repeat the same series once per segment, and the
/// cascade only needs the set, so duplicates are dropped here rather than
/// left for a reader to notice.
fn push_unique(into: &mut Vec<String>, uid: String) {
    if !into.iter().any(|existing| existing == &uid) {
        into.push(uid);
    }
}

// ===========================================================================
// DICOM reader - stateful byte-level parser
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
enum TransferSyntax {
    ImplicitVrLE,
    ExplicitVrLE,
    ExplicitVrBE,
}

/// A parsed DICOM data element with byte-range value.
#[derive(Debug)]
struct ElementRange {
    tag: DicomTag,
    /// VR (Value Representation) - `Some` for explicit VR transfer syntaxes,
    /// `None` for implicit VR (caller must look up from data dictionary).
    /// Two ASCII bytes, zero heap allocations.
    vr: Option<[u8; 2]>,
    value_start: usize,
    value_end: usize,
}

impl ElementRange {
    /// Return VR as &str (e.g., "UI", "CS"). Returns "UN" if VR is missing.
    pub fn vr_str(&self) -> &str {
        match &self.vr {
            Some(bytes) => std::str::from_utf8(bytes).unwrap_or("UN"),
            None => "UN",
        }
    }

    /// Whether this element is a sequence.
    ///
    /// Explicit datasets say so in the VR. Implicit datasets have no VR on the
    /// wire, so it comes from the data dictionary (PS3.6) - without that
    /// lookup every walker gated on `vr == SQ` skipped EVERY implicit instance,
    /// which is most of a typical archive.
    fn is_sequence(&self) -> bool {
        match &self.vr {
            Some(bytes) => bytes == b"SQ",
            None => super::super::codec::vr_dict::vr_for_tag(self.tag.0, self.tag.1) == "SQ",
        }
    }
}

/// Stateful DICOM byte-stream reader.
///
/// Owns a reference to the raw bytes and tracks the current parse position.
/// Element values are byte ranges into the original data - no copies, no unsafe.
struct DicomReader<'a> {
    data: &'a [u8],
    pos: usize,
    syntax: TransferSyntax,
    charset: Option<String>,
    /// (0002,0010) TransferSyntaxUID extracted from meta-header.
    transfer_syntax_uid: Option<String>,
    /// 0310 F3: when set, `next_element` returns the PixelData element with a
    /// SYNTHETIC empty value range (value_start == value_end == position right
    /// after the value-length field) instead of validating that the declared
    /// value fits in `data`. This lets a BOUNDED header (a head slice that
    /// deliberately excludes the multi-GB PixelData value) be parsed: the
    /// caller breaks at PixelData and never needs its bytes. Off by default so
    /// every existing parse path is byte-for-byte unchanged.
    stop_at_pixel_data: bool,
    /// True for Explicit VR Big Endian: tags and lengths are read the other
    /// way round (PS3.5 7.1.3). The meta group is always Little Endian, so the
    /// flag is set only once the dataset transfer syntax is resolved.
    big_endian: bool,
}

impl<'a> DicomReader<'a> {
    fn new(data: &'a [u8]) -> Result<Self, DicomError> {
        if data.len() < 12 {
            return Err(DicomError::InvalidDicom(
                "Data too short to be DICOM".into(),
            ));
        }

        // Find DICM magic at offset 128 (standard Part 10 preamble)
        // or at offset 0 (files without preamble).
        let data_start = if data.len() >= 132 && &data[128..132] == b"DICM" {
            132
        } else if &data[0..4] == b"DICM" {
            4
        } else {
            return Err(DicomError::InvalidDicom("Missing DICM magic number".into()));
        };

        // Meta-header starts at the byte after DICM, always Explicit VR LE
        let mut reader = Self {
            data,
            pos: data_start,
            syntax: TransferSyntax::ExplicitVrLE,
            charset: None,
            transfer_syntax_uid: None,
            stop_at_pixel_data: false,
            big_endian: false,
        };

        let ts_uid = reader.parse_meta_header()?;
        reader.transfer_syntax_uid = ts_uid.clone();

        // When no meta-header (DIMSE data with "DICM" prepended), auto-detect
        // Explicit vs Implicit VR from the first dataset element's bytes.
        // DICOM PS3.10 Section 7.1 default is Implicit VR, but many modalities
        // (especially those sending secondary captures) use Explicit VR.
        if ts_uid.is_none() {
            reader.syntax = detect_transfer_syntax(data, data_start);
        } else {
            reader.syntax = resolve_transfer_syntax(ts_uid.as_deref())?;
        }
        // The meta group was read Little Endian (it always is); from here on
        // the byte order belongs to the dataset's transfer syntax.
        reader.big_endian = reader.syntax == TransferSyntax::ExplicitVrBE;
        Ok(reader)
    }

    /// Parse meta-header (group 0002) to find TransferSyntaxUID.
    fn parse_meta_header(&mut self) -> Result<Option<String>, DicomError> {
        let mut ts_uid: Option<String> = None;

        loop {
            if self.pos + 4 > self.data.len() {
                break;
            }

            let tag = self.read_tag();

            // Group no longer 0002 -> first dataset element, stop
            if tag.0 != 0x0002 {
                // Rewind: tag was read but not consumed
                self.pos -= 4;
                break;
            }

            let elem = self.read_meta_element()?;

            if tag == tags::TRANSFER_SYNTAX_UID {
                let val = &self.data[elem.value_start..elem.value_end];
                ts_uid = bytes_to_opt_string(val, None);
            }
        }

        Ok(ts_uid)
    }

    /// Read the next dataset element.
    ///
    /// A LOOP, not recursion. The two cases below both "skip and try again",
    /// and a crafted instance can consist of nothing but those: 8-byte
    /// delimiters repeat 130k times in a one-megabyte upload, which as
    /// recursion exhausted the stack and aborted the process (`panic =
    /// abort`). The read helpers therefore return `None` for a delimiter
    /// instead of calling back into this function, and the loop continues.
    fn next_element(&mut self) -> Result<Option<ElementRange>, DicomError> {
        loop {
            if self.pos + 4 > self.data.len() {
                return Ok(None);
            }

            let tag = self.read_tag();

            // 0310 F3: bounded-header mode - return PixelData with a synthetic
            // empty value range instead of validating its declared length
            // against `data`. The caller breaks here, so the (possibly
            // multi-GB) value is never needed and a head slice that excludes
            // it parses cleanly.
            if self.stop_at_pixel_data && tag == tags::PIXEL_DATA {
                let here = self.pos;
                return Ok(Some(ElementRange {
                    tag,
                    vr: None,
                    value_start: here,
                    value_end: here,
                }));
            }

            // DEV-009: DICOM PS3.5 Section 7.1 - skip reserved command groups
            if tag.0 == 0x0000 || tag.0 == 0x0004 {
                log::warn!(
                    "Skipping reserved DICOM group ({:04X},{:04X}) at offset {}",
                    tag.0,
                    tag.1,
                    self.pos - 4
                );
                // Read and discard the element to maintain correct position.
                let skipped = match self.syntax {
                    TransferSyntax::ImplicitVrLE => self.read_implicit_le(tag)?,
                    _ => self.read_explicit_vr(tag)?,
                };
                // A delimiter consumed inside the skipped element leaves the
                // cursor on the next real element; a plain element is the
                // answer itself.
                if let Some(element) = skipped {
                    return Ok(Some(element));
                }
                continue;
            }

            let element = match self.syntax {
                TransferSyntax::ImplicitVrLE => self.read_implicit_le(tag)?,
                _ => self.read_explicit_vr(tag)?,
            };
            // `None` means the helper consumed a delimiter: keep walking.
            match element {
                Some(element) => return Ok(Some(element)),
                None => continue,
            }
        }
    }

    // -- Helpers --------------------------------------------------------

    fn read_tag(&mut self) -> DicomTag {
        debug_assert!(
            self.pos + 4 <= self.data.len(),
            "read_tag() called past buffer end: pos={}, len={}",
            self.pos,
            self.data.len()
        );
        // Explicit VR Big Endian reverses the whole element: group, element and
        // both length fields. A little-endian tag read matches no tag at all on
        // that syntax, so every element fell through as unknown and the
        // metadata came out byte-swapped. (The META group is always Explicit VR
        // Little Endian - `big_endian` is false until the dataset syntax is
        // resolved, so the meta walk stays LE.)
        let (g, e) = if self.big_endian {
            (
                read_u16_be(self.data, self.pos),
                read_u16_be(self.data, self.pos + 2),
            )
        } else {
            (
                read_u16_le(self.data, self.pos),
                read_u16_le(self.data, self.pos + 2),
            )
        };
        self.pos += 4;
        (g, e)
    }

    /// Read a `u32` at `at` in this reader's byte order.
    ///
    /// Element lengths follow the dataset's transfer syntax, not the host's
    /// and not the meta group's: on Explicit VR Big Endian a little-endian
    /// read turns a 256-byte value into 65536 and the next element is read
    /// from wherever that lands.
    fn read_u32(&self, at: usize) -> u32 {
        if self.big_endian {
            read_u32_be(self.data, at)
        } else {
            read_u32_le(self.data, at)
        }
    }

    /// Advance `self.pos` past a value of `length` bytes, then skip the
    /// trailing pad byte if length is odd (DICOM PS3.5 Section 7.1).
    /// Returns `value_end` (exclusive, without pad).
    /// Bounds must be checked by the caller before invoking this method.
    fn set_pos_after_value(&mut self, vs: usize, length: usize) -> usize {
        let ve = vs + length;
        self.pos = ve;
        // DICOM PS3.5 Section 7.1: pad byte OUTSIDE Value Length for odd-length values.
        if !length.is_multiple_of(2) {
            self.pos += 1;
        }
        ve
    }

    fn read_meta_element(&mut self) -> Result<ElementRange, DicomError> {
        // Meta-header is always Explicit VR Little Endian.
        // Per DICOM Part 10, extended-length VRs (OB, OD, OF, OL, OW, SQ,
        // UC, UN, UR, UT) use reserved(2) + length(4). All other VRs use
        // length(2). However, some encoders (including the one that produced
        // our test files) emit the extended format for *all* meta-header VRs.
        // We detect this by reading the two bytes after VR: if they are
        // 0x0000 (common reserved value in meta-header) and the alternative
        // interpretation would land on a valid boundary, prefer extended.
        const EXTENDED_VR: &[&str] = &[
            "OB", "OD", "OF", "OL", "OV", "OW", "SQ", "SV", "UC", "UN", "UR", "UT", "UV",
        ];

        if self.pos + 4 > self.data.len() {
            return Err(DicomError::InvalidDicom(
                "Truncated meta-header: need VR+length".into(),
            ));
        }

        let vr = [self.data[self.pos], self.data[self.pos + 1]];
        let vr_str = std::str::from_utf8(&vr).map_err(|_| {
            DicomError::InvalidDicom(format!("Invalid VR in meta-header at {}", self.pos))
        })?;
        self.pos += 2;

        let (length, advance): (usize, usize) = if EXTENDED_VR.contains(&vr_str) {
            if self.pos + 6 > self.data.len() {
                return Err(DicomError::InvalidDicom(
                    "Truncated meta-header: extended VR".into(),
                ));
            }
            // The meta group is ALWAYS Explicit VR Little Endian (PS3.10 7.1),
            // so this length is little endian even on a Big Endian dataset.
            let len = read_u32_le(self.data, self.pos + 2) as usize;
            (len, 6)
        } else {
            // Standard VR: length is 2 bytes.
            let short_len = read_u16_le(self.data, self.pos) as usize;
            // Detect non-standard extended encoding: some encoders (including
            // the one that produced CR1.dcm) pad with two zero bytes before
            // a 4-byte length for *all* meta-header VRs, not just OB/OD/...
            let maybe_reserved = read_u16_le(self.data, self.pos) as usize;
            if maybe_reserved == 0 && self.pos + 6 <= self.data.len() {
                let alt_len = read_u32_le(self.data, self.pos + 2) as usize;
                // Heuristic: if the short length is 0 (meaningless for a
                // meta-header value) and the extended length is non-zero
                // and fits within data, prefer extended.
                if short_len == 0
                    && alt_len > 0
                    && alt_len <= MAX_META_ELEMENT_SIZE
                    && self.pos + 6 + alt_len <= self.data.len()
                {
                    (alt_len, 6)
                } else {
                    (short_len, 2)
                }
            } else {
                (short_len, 2)
            }
        };

        self.pos += advance;
        let vs = self.pos;
        let ve = vs + length;
        if ve > self.data.len() {
            return Err(DicomError::InvalidDicom(format!(
                "Meta-header value overflow: len={length}"
            )));
        }
        let ve = self.set_pos_after_value(vs, length);
        Ok(ElementRange {
            tag: (0, 0),
            vr: Some(vr),
            value_start: vs,
            value_end: ve,
        })
    }

    /// Skip an element with undefined length (0xFFFFFFFF).
    ///
    /// DICOM PS3.5 Section 7.5: Undefined length is valid for SQ and UN VRs.
    /// The element value is a sequence of Items terminated by
    /// SequenceDelimitationItem (FFFE,E0DD). Tracks nesting for
    /// nested undefined-length sequences/items.
    fn skip_undefined_element(&mut self, is_explicit: bool) -> Result<(), DicomError> {
        let mut nesting: u32 = 0;
        loop {
            if self.pos + 8 > self.data.len() {
                return Err(DicomError::InvalidDicom(
                    "Unexpected EOF while skipping undefined-length element".into(),
                ));
            }
            let tag = self.read_tag();
            match tag {
                (0xFFFE, 0xE000) => {
                    // Item start - length is always 4 bytes (even in explicit VR)
                    let item_len = read_u32_le(self.data, self.pos) as usize;
                    self.pos += 4;
                    if item_len == 0xFFFF_FFFF {
                        nesting += 1;
                    } else if self.pos + item_len > self.data.len() {
                        return Err(DicomError::InvalidDicom(
                            "Unexpected EOF inside sequence item".into(),
                        ));
                    } else {
                        self.pos += item_len;
                    }
                }
                (0xFFFE, 0xE00D) => {
                    // ItemDelimitationItem - end of an item, length is 0x00000000
                    self.pos += 4;
                    nesting = nesting.saturating_sub(1);
                }
                (0xFFFE, 0xE0DD) => {
                    // SequenceDelimitationItem - closes ONE undefined-length
                    // element. Nested undefined-length sequences and items
                    // each carry their own delimiter, so only the outermost
                    // one ends this skip; the inner ones just unwind `nesting`.
                    self.pos += 4;
                    if nesting == 0 {
                        return Ok(());
                    }
                    nesting -= 1;
                }
                _ => {
                    // Regular attribute inside an item - skip its value
                    if is_explicit {
                        if self.pos + 2 > self.data.len() {
                            return Err(DicomError::InvalidDicom(
                                "Unexpected EOF while skipping explicit attribute".into(),
                            ));
                        }
                        let vr = [self.data[self.pos], self.data[self.pos + 1]];
                        self.pos += 2;
                        let vr_str = std::str::from_utf8(&vr).map_err(|_| {
                            DicomError::InvalidDicom(
                                "Invalid VR in undefined-length element".into(),
                            )
                        })?;
                        let (attr_len, advance) = match vr_str {
                            "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN"
                            | "UR" | "UT" | "UV" => {
                                if self.pos + 6 > self.data.len() {
                                    return Err(DicomError::InvalidDicom(
                                        "Unexpected EOF in extended VR".into(),
                                    ));
                                }
                                let len = self.read_u32(self.pos + 2) as usize;
                                (len, 6)
                            }
                            _ => {
                                // Short-form lengths follow the dataset's byte
                                // order, unlike the tag.
                                let len = if self.big_endian {
                                    read_u16_be(self.data, self.pos) as usize
                                } else {
                                    read_u16_le(self.data, self.pos) as usize
                                };
                                (len, 2)
                            }
                        };
                        self.pos += advance;
                        if attr_len == 0xFFFF_FFFF {
                            nesting += 1;
                        } else if self.pos + attr_len > self.data.len() {
                            return Err(DicomError::InvalidDicom(
                                "Unexpected EOF while skipping attribute value".into(),
                            ));
                        } else {
                            self.pos += attr_len;
                            // PS3.5 Section 7.1: odd-length values carry a
                            // pad byte outside the value length; without it
                            // the walk desyncs on the next element.
                            self.pos += attr_len % 2;
                        }
                    } else {
                        // Implicit VR: 4-byte length follows the tag
                        let attr_len = read_u32_le(self.data, self.pos) as usize;
                        self.pos += 4;
                        if attr_len == 0xFFFF_FFFF {
                            nesting += 1;
                        } else if self.pos + attr_len > self.data.len() {
                            return Err(DicomError::InvalidDicom(
                                "Unexpected EOF while skipping attribute value".into(),
                            ));
                        } else {
                            self.pos += attr_len;
                            self.pos += attr_len % 2;
                        }
                    }
                }
            }
        }
    }

    fn read_explicit_vr(&mut self, tag: DicomTag) -> Result<Option<ElementRange>, DicomError> {
        // Delimiter tags (FFFE,E000), (FFFE,E00D), (FFFE,E0DD) have NO VR -
        // they always use 4-byte implicit length even in Explicit VR syntax.
        // PS3.5 Section 7.5: "Items and Delimiter Items shall be encoded using
        // Implicit VR Little Endian regardless of the Transfer Syntax."
        if tag.0 == 0xFFFE {
            if self.pos + 4 > self.data.len() {
                return Ok(None);
            }
            let length = read_u32_le(self.data, self.pos) as usize;
            self.pos += 4;

            if length == 0xFFFF_FFFF {
                // Undefined length: Item with nested content or delimiter.
                // (FFFE,E00D) and (FFFE,E0DD) never have undefined length,
                // but handle anyway for robustness. The value is consumed and
                // the caller's loop moves on - returning `None` rather than
                // calling back into `next_element` is what keeps a crafted
                // instance from recursing once per delimiter.
                self.skip_undefined_element(false)?;
                return Ok(None);
            }

            let vs = self.pos;
            let ve = vs + length;
            if ve > self.data.len() {
                return Err(DicomError::InvalidDicom(format!(
                    "Value overflow: ({:04X},{:04X}) len={length}",
                    tag.0, tag.1
                )));
            }
            let ve = self.set_pos_after_value(vs, length);

            // Item/Sequence Delimiters (FFFE,E00D) and (FFFE,E0DD)
            // carry no metadata - the caller's loop skips to the next real
            // element. Items (FFFE,E000) are returned as regular elements.
            if tag == (0xFFFE, 0xE00D) || tag == (0xFFFE, 0xE0DD) {
                return Ok(None);
            }

            return Ok(Some(ElementRange {
                tag,
                vr: None,
                value_start: vs,
                value_end: ve,
            }));
        }

        if self.pos + 2 > self.data.len() {
            return Ok(None);
        }

        let vr = [self.data[self.pos], self.data[self.pos + 1]];
        let vr_str = std::str::from_utf8(&vr).map_err(|_| {
            DicomError::InvalidDicom(format!("Invalid VR at {}: {vr:02X?}", self.pos))
        })?;
        self.pos += 2;

        let (length, extra) = match vr_str {
            "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT"
            | "UV" => {
                if self.pos + 6 > self.data.len() {
                    return Ok(None);
                }
                let len = read_u32_syntax(self.data, self.pos + 2, self.syntax) as usize;
                (len, 6)
            }
            _ => {
                // DICOM PS3.5 Section 7.1: unknown VR -> treat as UN (extended-length)
                let is_known = matches!(
                    vr_str,
                    "AE" | "AS"
                        | "AT"
                        | "CS"
                        | "DA"
                        | "DS"
                        | "DT"
                        | "FL"
                        | "FD"
                        | "IS"
                        | "LO"
                        | "LT"
                        | "PN"
                        | "SH"
                        | "SL"
                        | "SS"
                        | "ST"
                        | "TM"
                        | "UI"
                        | "UL"
                        | "US"
                );
                if !is_known {
                    log::warn!(
                        "Unknown VR '{}' at offset {} — skipping corrupted element \
                         (data is unrecoverable, instance is preserved)",
                        vr_str,
                        self.pos - 2
                    );
                    // VR bytes are garbage -> UN header (6 bytes) is also
                    // garbage -> element length is unreadable -> skip to EOF.
                    self.pos = self.data.len();
                    return Ok(None);
                } else {
                    let len = read_u16_syntax(self.data, self.pos, self.syntax) as usize;
                    (len, 2)
                }
            }
        };

        self.pos += extra;

        if length == 0xFFFF_FFFF {
            // PixelData (7FE0,0010): don't walk encapsulated items -
            // the metadata parser stops here anyway. Jumping into a
            // JPEG bitstream looking for FFFE items will inevitably
            // hit byte sequences that look like impossible lengths.
            if tag == tags::PIXEL_DATA {
                let vs = self.pos;
                self.pos = self.data.len();
                return Ok(Some(ElementRange {
                    tag,
                    vr: Some(vr),
                    value_start: vs,
                    value_end: self.data.len(),
                }));
            }
            // SQ / UN / other undefined-length: parse normally.
            let vs = self.pos;
            self.skip_undefined_element(true)?;
            let ve = self.pos;
            return Ok(Some(ElementRange {
                tag,
                vr: Some(vr),
                value_start: vs,
                value_end: ve,
            }));
        }

        let vs = self.pos;
        if vs + length > self.data.len() {
            // PixelData (7FE0,0010) may legitimately extend past the
            // metadata read buffer - clamp to buffer end so the caller
            // can detect it and stop parsing.
            if tag == tags::PIXEL_DATA {
                let clamped_ve = self.data.len();
                self.pos = clamped_ve;
                return Ok(Some(ElementRange {
                    tag,
                    vr: Some(vr),
                    value_start: vs,
                    value_end: clamped_ve,
                }));
            }
            return Err(DicomError::InvalidDicom(format!(
                "Value overflow: ({:04X},{:04X}) len={length}",
                tag.0, tag.1
            )));
        }
        let ve = self.set_pos_after_value(vs, length);

        Ok(Some(ElementRange {
            tag,
            vr: Some(vr),
            value_start: vs,
            value_end: ve,
        }))
    }

    fn read_implicit_le(&mut self, tag: DicomTag) -> Result<Option<ElementRange>, DicomError> {
        if self.pos + 4 > self.data.len() {
            return Ok(None);
        }
        let length = read_u32_le(self.data, self.pos) as usize;
        self.pos += 4;

        if length == 0xFFFF_FFFF {
            // PixelData (7FE0,0010): don't walk encapsulated items.
            // See twin comment in read_explicit_vr().
            if tag == tags::PIXEL_DATA {
                let vs = self.pos;
                self.pos = self.data.len();
                return Ok(Some(ElementRange {
                    tag,
                    vr: None,
                    value_start: vs,
                    value_end: self.data.len(),
                }));
            }
            // SQ / UN / other undefined-length: parse normally.
            let vs = self.pos;
            self.skip_undefined_element(false)?;
            let ve = self.pos;
            return Ok(Some(ElementRange {
                tag,
                vr: None,
                value_start: vs,
                value_end: ve,
            }));
        }

        let vs = self.pos;
        if vs + length > self.data.len() {
            // PixelData (7FE0,0010) may legitimately extend past the
            // metadata read buffer - clamp to buffer end so the caller
            // can detect it and stop parsing.
            if tag == tags::PIXEL_DATA {
                let clamped_ve = self.data.len();
                self.pos = clamped_ve;
                return Ok(Some(ElementRange {
                    tag,
                    vr: None,
                    value_start: vs,
                    value_end: clamped_ve,
                }));
            }
            return Err(DicomError::InvalidDicom(format!(
                "Value overflow: ({:04X},{:04X}) len={length}",
                tag.0, tag.1
            )));
        }
        let ve = self.set_pos_after_value(vs, length);

        // Skip sequence delimiters: the caller's loop moves on to the next
        // real element, so a buffer full of delimiters costs iterations
        // rather than stack frames.
        if tag == (0xFFFE, 0xE00D) || tag == (0xFFFE, 0xE0DD) {
            return Ok(None);
        }

        Ok(Some(ElementRange {
            tag,
            vr: None,
            value_start: vs,
            value_end: ve,
        }))
    }
}

// ===========================================================================
// Transfer syntax
// ===========================================================================

/// Auto-detect Explicit vs Implicit VR when no meta-header is present.
///
/// DIMSE data with "DICM" prepended has no (0002,0010) TransferSyntaxUID.
/// We look at the first element's potential VR bytes (offset +4 from data_start):
/// - If they form a known DICOM VR string -> Explicit VR (compressed or uncompressed).
/// - Otherwise -> Implicit VR (DICOM PS3.10 Section 7.1 default).
///
/// The VR bytes in Implicit VR would be the low bytes of the 4-byte length,
/// which for any realistic element length would be non-printable or invalid.
fn detect_transfer_syntax(data: &[u8], data_start: usize) -> TransferSyntax {
    const VALID_VRS: &[&str] = &[
        "AE", "AS", "AT", "CS", "DA", "DS", "DT", "FL", "FD", "IS", "LO", "LT", "OB", "OD", "OF",
        "OL", "OW", "PN", "SH", "SL", "SQ", "SS", "ST", "TM", "UC", "UI", "UL", "UN", "UR", "US",
        "UT",
    ];

    if data_start + 6 <= data.len() {
        let maybe_vr = &data[data_start + 4..data_start + 6];
        if let Ok(vr_str) = std::str::from_utf8(maybe_vr)
            && VALID_VRS.contains(&vr_str)
        {
            return TransferSyntax::ExplicitVrLE;
        }
    }

    TransferSyntax::ImplicitVrLE
}

fn resolve_transfer_syntax(uid: Option<&str>) -> Result<TransferSyntax, DicomError> {
    match uid {
        Some("1.2.840.10008.1.2") => Ok(TransferSyntax::ImplicitVrLE),
        Some("1.2.840.10008.1.2.1") => Ok(TransferSyntax::ExplicitVrLE),
        None => Ok(TransferSyntax::ImplicitVrLE), // DICOM Part 10 Section 7.1 default
        Some("1.2.840.10008.1.2.2") => Ok(TransferSyntax::ExplicitVrBE),
        Some("1.2.840.10008.1.2.1.99") => Err(DicomError::InvalidDicom(
            "Deflated transfer syntax (1.2.840.10008.1.2.1.99) is not supported \
             per DICOM PS3.10 §7.1. Use Explicit VR Little Endian (1.2.840.10008.1.2.1)."
                .into(),
        )),
        // ===================================================================
        // Compressed transfer syntaxes - metadata parsed as Explicit VR LE.
        // Non-pixel tags use standard Explicit VR Little Endian encoding.
        // Pixel data (7FE0,0010) is never decoded by the metadata parser -
        // it stops before the pixel data element. The original TS UID is
        // preserved in InstanceMeta::transfer_syntax_uid for WADO transcoding.
        // ===================================================================
        // JPEG Lossless (Process 14, Selection Value 1)
        Some("1.2.840.10008.1.2.4.70") => Ok(TransferSyntax::ExplicitVrLE),
        // JPEG Lossless Non-Hierarchical (Process 14)
        Some("1.2.840.10008.1.2.4.57") => Ok(TransferSyntax::ExplicitVrLE),
        // JPEG 2000 Lossless Only
        Some("1.2.840.10008.1.2.4.90") => Ok(TransferSyntax::ExplicitVrLE),
        // JPEG 2000 (Lossy)
        Some("1.2.840.10008.1.2.4.91") => Ok(TransferSyntax::ExplicitVrLE),
        // JPEG Baseline (Process 1) - lossy 8-bit
        Some("1.2.840.10008.1.2.4.50") => Ok(TransferSyntax::ExplicitVrLE),
        // JPEG Extended (Process 2 & 4) - lossy 12-bit
        Some("1.2.840.10008.1.2.4.51") => Ok(TransferSyntax::ExplicitVrLE),
        // JPEG-LS Lossless
        Some("1.2.840.10008.1.2.4.80") => Ok(TransferSyntax::ExplicitVrLE),
        // JPEG-LS Lossy (Near-Lossless)
        Some("1.2.840.10008.1.2.4.81") => Ok(TransferSyntax::ExplicitVrLE),
        // RLE Lossless
        Some("1.2.840.10008.1.2.5") => Ok(TransferSyntax::ExplicitVrLE),
        Some(other) => Err(DicomError::InvalidDicom(format!(
            "Unsupported transfer syntax: {other}"
        ))),
    }
}

// ===========================================================================
// Byte-level helpers
// ===========================================================================

fn read_u16_le(data: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([data[pos], data[pos + 1]])
}

fn read_u16_be(data: &[u8], pos: usize) -> u16 {
    u16::from_be_bytes([data[pos], data[pos + 1]])
}

fn read_u16_syntax(data: &[u8], pos: usize, syntax: TransferSyntax) -> u16 {
    match syntax {
        TransferSyntax::ExplicitVrBE => u16::from_be_bytes([data[pos], data[pos + 1]]),
        _ => u16::from_le_bytes([data[pos], data[pos + 1]]),
    }
}

fn read_u32_le(data: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
}

fn read_u32_be(data: &[u8], pos: usize) -> u32 {
    u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
}

fn read_u32_syntax(data: &[u8], pos: usize, syntax: TransferSyntax) -> u32 {
    match syntax {
        TransferSyntax::ExplicitVrBE => read_u32_be(data, pos),
        _ => read_u32_le(data, pos),
    }
}

// ===========================================================================
// Character set decoding (DICOM PS3.5 Section 6.1)
// ===========================================================================

/// ISO 8859-5 (Latin/Cyrillic) -> Unicode mapping for bytes 0xA0-0xFF.
/// Bytes 0x00-0x7F are ASCII (identical in UTF-8).
/// Bytes 0x80-0x9F are C1 control codes (rare in DICOM, map to U+FFFD).
///
/// Source: ISO/IEC 8859-5:1999, DICOM PS3.3 Table C.12-2 (ISO_IR 144).
#[rustfmt::skip]
static ISO_8859_5_TABLE: [char; 96] = [
    // 0xA0
    '\u{00A0}', '\u{0401}', '\u{0402}', '\u{0403}', '\u{0404}', '\u{0405}', '\u{0406}', '\u{0407}',
    '\u{0408}', '\u{0409}', '\u{040A}', '\u{040B}', '\u{040C}', '\u{00AD}', '\u{040E}', '\u{040F}',
    // 0xB0
    '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}', '\u{0416}', '\u{0417}',
    '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}', '\u{041F}',
    // 0xC0
    '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}',
    '\u{0428}', '\u{0429}', '\u{042A}', '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}',
    // 0xD0
    '\u{0430}', '\u{0431}', '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}',
    '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
    // 0xE0
    '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}', '\u{0447}',
    '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}', '\u{044E}', '\u{044F}',
    // 0xF0
    '\u{2116}', '\u{0451}', '\u{0452}', '\u{0453}', '\u{0454}', '\u{0455}', '\u{0456}', '\u{0457}',
    '\u{0458}', '\u{0459}', '\u{045A}', '\u{045B}', '\u{045C}', '\u{00A7}', '\u{045E}', '\u{045F}',
];

/// ISO 8859-2 (Latin-2 / Central European) -> Unicode mapping for bytes 0xA0-0xFF.
/// Covers: Polish, Czech, Slovak, Hungarian, Romanian, Croatian, Slovenian.
/// DICOM PS3.3 Table C.12-2: ISO_IR 101.
/// Source: ISO/IEC 8859-2:1999.
#[rustfmt::skip]
static ISO_8859_2_TABLE: [char; 96] = [
    // 0xA0
    '\u{00A0}', '\u{0104}', '\u{02D8}', '\u{0141}', '\u{00A4}', '\u{013D}', '\u{015A}', '\u{00A7}',
    '\u{00A8}', '\u{0160}', '\u{015E}', '\u{0164}', '\u{0179}', '\u{00AD}', '\u{017D}', '\u{017B}',
    // 0xB0
    '\u{00B0}', '\u{0105}', '\u{02DB}', '\u{0142}', '\u{00B4}', '\u{013E}', '\u{015B}', '\u{02C7}',
    '\u{00B8}', '\u{0161}', '\u{015F}', '\u{0165}', '\u{017A}', '\u{02DD}', '\u{017E}', '\u{017C}',
    // 0xC0
    '\u{0154}', '\u{00C1}', '\u{00C2}', '\u{0102}', '\u{00C4}', '\u{0139}', '\u{0106}', '\u{00C7}',
    '\u{010C}', '\u{00C9}', '\u{0118}', '\u{00CB}', '\u{011A}', '\u{00CD}', '\u{00CE}', '\u{010E}',
    // 0xD0
    '\u{0110}', '\u{0143}', '\u{0147}', '\u{00D3}', '\u{00D4}', '\u{0150}', '\u{00D6}', '\u{00D7}',
    '\u{0158}', '\u{016E}', '\u{00DA}', '\u{0170}', '\u{00DC}', '\u{00DD}', '\u{0162}', '\u{00DF}',
    // 0xE0
    '\u{0155}', '\u{00E1}', '\u{00E2}', '\u{0103}', '\u{00E4}', '\u{013A}', '\u{0107}', '\u{00E7}',
    '\u{010D}', '\u{00E9}', '\u{0119}', '\u{00EB}', '\u{011B}', '\u{00ED}', '\u{00EE}', '\u{010F}',
    // 0xF0
    '\u{0111}', '\u{0144}', '\u{0148}', '\u{00F3}', '\u{00F4}', '\u{0151}', '\u{00F6}', '\u{00F7}',
    '\u{0159}', '\u{016F}', '\u{00FA}', '\u{0171}', '\u{00FC}', '\u{00FD}', '\u{0163}', '\u{02D9}',
];

/// ISO 8859-3 (Latin-3 / South European) -> Unicode mapping for bytes 0xA0-0xFF.
/// Covers: Esperanto, Maltese, Turkish (partial). Rare in clinical practice.
/// DICOM PS3.3 Table C.12-2: ISO_IR 109.
/// Source: ISO/IEC 8859-3:1999.
#[rustfmt::skip]
static ISO_8859_3_TABLE: [char; 96] = [
    // 0xA0
    '\u{00A0}', '\u{0126}', '\u{02D8}', '\u{00A3}', '\u{00A4}', '\u{FFFD}', '\u{0124}', '\u{00A7}',
    '\u{00A8}', '\u{0130}', '\u{015E}', '\u{011E}', '\u{0134}', '\u{00AD}', '\u{FFFD}', '\u{017B}',
    // 0xB0
    '\u{00B0}', '\u{0127}', '\u{00B2}', '\u{00B3}', '\u{00B4}', '\u{00B5}', '\u{0125}', '\u{00B7}',
    '\u{00B8}', '\u{0131}', '\u{015F}', '\u{011F}', '\u{0135}', '\u{00BD}', '\u{FFFD}', '\u{017C}',
    // 0xC0
    '\u{00C0}', '\u{00C1}', '\u{00C2}', '\u{FFFD}', '\u{00C4}', '\u{010A}', '\u{0108}', '\u{00C7}',
    '\u{00C8}', '\u{00C9}', '\u{00CA}', '\u{00CB}', '\u{00CC}', '\u{00CD}', '\u{00CE}', '\u{00CF}',
    // 0xD0
    '\u{FFFD}', '\u{00D1}', '\u{00D2}', '\u{00D3}', '\u{00D4}', '\u{0120}', '\u{00D6}', '\u{00D7}',
    '\u{011C}', '\u{00D9}', '\u{00DA}', '\u{00DB}', '\u{00DC}', '\u{016C}', '\u{015C}', '\u{00DF}',
    // 0xE0
    '\u{00E0}', '\u{00E1}', '\u{00E2}', '\u{FFFD}', '\u{00E4}', '\u{010B}', '\u{0109}', '\u{00E7}',
    '\u{00E8}', '\u{00E9}', '\u{00EA}', '\u{00EB}', '\u{00EC}', '\u{00ED}', '\u{00EE}', '\u{00EF}',
    // 0xF0
    '\u{FFFD}', '\u{00F1}', '\u{00F2}', '\u{00F3}', '\u{00F4}', '\u{0121}', '\u{00F6}', '\u{00F7}',
    '\u{011D}', '\u{00F9}', '\u{00FA}', '\u{00FB}', '\u{00FC}', '\u{016D}', '\u{015D}', '\u{02D9}',
];

/// ISO 8859-4 (Latin-4 / North European) -> Unicode mapping for bytes 0xA0-0xFF.
/// Covers: Estonian, Latvian, Lithuanian, Greenlandic. Rare in clinical practice.
/// DICOM PS3.3 Table C.12-2: ISO_IR 110.
/// Source: ISO/IEC 8859-4:1998.
#[rustfmt::skip]
static ISO_8859_4_TABLE: [char; 96] = [
    // 0xA0
    '\u{00A0}', '\u{0104}', '\u{0138}', '\u{0156}', '\u{00A4}', '\u{0128}', '\u{013B}', '\u{00A7}',
    '\u{00A8}', '\u{0160}', '\u{0112}', '\u{0122}', '\u{0166}', '\u{00AD}', '\u{017D}', '\u{00AF}',
    // 0xB0
    '\u{00B0}', '\u{0105}', '\u{02DB}', '\u{0157}', '\u{00B4}', '\u{0129}', '\u{013C}', '\u{02C7}',
    '\u{00B8}', '\u{0161}', '\u{0113}', '\u{0123}', '\u{0167}', '\u{014A}', '\u{017E}', '\u{014B}',
    // 0xC0
    '\u{0100}', '\u{00C1}', '\u{00C2}', '\u{00C3}', '\u{00C4}', '\u{00C5}', '\u{00C6}', '\u{012E}',
    '\u{010C}', '\u{00C9}', '\u{0118}', '\u{00CB}', '\u{0116}', '\u{00CD}', '\u{00CE}', '\u{012A}',
    // 0xD0
    '\u{0110}', '\u{0145}', '\u{014C}', '\u{0136}', '\u{00D4}', '\u{00D5}', '\u{00D6}', '\u{00D7}',
    '\u{00D8}', '\u{0172}', '\u{00DA}', '\u{00DB}', '\u{00DC}', '\u{0168}', '\u{016A}', '\u{00DF}',
    // 0xE0
    '\u{0101}', '\u{00E1}', '\u{00E2}', '\u{00E3}', '\u{00E4}', '\u{00E5}', '\u{00E6}', '\u{012F}',
    '\u{010D}', '\u{00E9}', '\u{0119}', '\u{00EB}', '\u{0117}', '\u{00ED}', '\u{00EE}', '\u{012B}',
    // 0xF0
    '\u{0111}', '\u{0146}', '\u{014D}', '\u{0137}', '\u{00F4}', '\u{00F5}', '\u{00F6}', '\u{00F7}',
    '\u{00F8}', '\u{0173}', '\u{00FA}', '\u{00FB}', '\u{00FC}', '\u{0169}', '\u{016B}', '\u{02D9}',
];

/// ISO 8859-9 (Latin-5 / Turkish) -> Unicode mapping for bytes 0xA0-0xFF.
/// Identical to Latin-1 except for 6 Turkish-specific positions:
/// 0xD0 (Ğ), 0xDD (İ), 0xDE (Ş), 0xF0 (ğ), 0xFD (ı), 0xFE (ş).
/// DICOM PS3.3 Table C.12-2: ISO_IR 148.
/// Source: ISO/IEC 8859-9:1999.
#[rustfmt::skip]
static ISO_8859_9_TABLE: [char; 96] = [
    // 0xA0
    '\u{00A0}', '\u{00A1}', '\u{00A2}', '\u{00A3}', '\u{00A4}', '\u{00A5}', '\u{00A6}', '\u{00A7}',
    '\u{00A8}', '\u{00A9}', '\u{00AA}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{00AF}',
    // 0xB0
    '\u{00B0}', '\u{00B1}', '\u{00B2}', '\u{00B3}', '\u{00B4}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{00B8}', '\u{00B9}', '\u{00BA}', '\u{00BB}', '\u{00BC}', '\u{00BD}', '\u{00BE}', '\u{00BF}',
    // 0xC0
    '\u{00C0}', '\u{00C1}', '\u{00C2}', '\u{00C3}', '\u{00C4}', '\u{00C5}', '\u{00C6}', '\u{00C7}',
    '\u{00C8}', '\u{00C9}', '\u{00CA}', '\u{00CB}', '\u{00CC}', '\u{00CD}', '\u{00CE}', '\u{00CF}',
    // 0xD0
    '\u{011E}', '\u{00D1}', '\u{00D2}', '\u{00D3}', '\u{00D4}', '\u{00D5}', '\u{00D6}', '\u{00D7}',
    '\u{00D8}', '\u{00D9}', '\u{00DA}', '\u{00DB}', '\u{00DC}', '\u{0130}', '\u{015E}', '\u{00DF}',
    // 0xE0
    '\u{00E0}', '\u{00E1}', '\u{00E2}', '\u{00E3}', '\u{00E4}', '\u{00E5}', '\u{00E6}', '\u{00E7}',
    '\u{00E8}', '\u{00E9}', '\u{00EA}', '\u{00EB}', '\u{00EC}', '\u{00ED}', '\u{00EE}', '\u{00EF}',
    // 0xF0
    '\u{011F}', '\u{00F1}', '\u{00F2}', '\u{00F3}', '\u{00F4}', '\u{00F5}', '\u{00F6}', '\u{00F7}',
    '\u{00F8}', '\u{00F9}', '\u{00FA}', '\u{00FB}', '\u{00FC}', '\u{0131}', '\u{015F}', '\u{00FF}',
];

// CJK charset support (feature-gated, adds ~1 MB via encoding_rs)
#[cfg(feature = "cjk-charsets")]
use encoding_rs::{EUC_JP, EUC_KR, Encoding, GB18030, SHIFT_JIS};

#[cfg(feature = "cjk-charsets")]
fn is_cjk_charset(charset: &str) -> bool {
    matches!(
        charset,
        "ISO_IR 13" | "ISO 2022 IR 87" | "ISO 2022 IR 159" | "GB18030" | "ISO_IR 149"
    )
}

#[cfg(feature = "cjk-charsets")]
fn decode_cjk_string(bytes: &[u8], charset: &str) -> String {
    let encoding: &'static Encoding = match charset {
        "ISO_IR 13" => SHIFT_JIS,    // JIS X 0201 (katakana, subset of Shift_JIS)
        "ISO 2022 IR 87" => EUC_JP,  // JIS X 0208 (kanji, GR-region = EUC-JP layout)
        "ISO 2022 IR 159" => EUC_JP, // JIS X 0212 (suppl. kanji, EUC 3-byte 0x8F form, decode-only)
        "GB18030" => GB18030,
        "ISO_IR 149" => EUC_KR, // KS X 1001 (Korean)
        _ => unreachable!(),    // is_cjk_charset() gate
    };
    let (cow, _encoding_used, had_errors) = encoding.decode(bytes);
    if had_errors {
        log::warn!(
            "CJK decoding errors for charset '{}': some bytes replaced with U+FFFD",
            charset
        );
    }
    cow.into_owned()
}

#[cfg(all(test, feature = "cjk-charsets"))]
mod cjk_decode_tests {
    use super::{decode_cjk_string, is_cjk_charset};

    // Golden byte fixtures. Each was produced out-of-tree with Python's codecs
    // (`text.encode(codec)`), which keeps the bytes independent of the Rust
    // decoder under test - a fixture derived from the implementation would
    // agree with it by construction. The expected strings are written as
    // \u{...} escapes so this file stays ASCII.
    //
    // ap101: N/A - this decode runs on the STOW/metadata parse path (cold,
    // control plane), not on the QIDO/WADO read hot path.

    #[test]
    fn the_gate_accepts_exactly_the_five_supported_charsets() {
        for cs in [
            "ISO_IR 13",
            "ISO 2022 IR 87",
            "ISO 2022 IR 159",
            "GB18030",
            "ISO_IR 149",
        ] {
            assert!(is_cjk_charset(cs), "{cs} must be recognised as CJK");
        }
        // Case and spacing are part of the DICOM value, not a free-form label;
        // a near-miss must not silently take a CJK table.
        for cs in ["ISO_IR 14", "ISO_IR 87", "GB2312", "ISO 2022 IR 58", ""] {
            assert!(!is_cjk_charset(cs), "{cs} must not be treated as CJK");
        }
    }

    #[test]
    fn iso_ir_13_decodes_shift_jis_katakana() {
        // U+30A2 U+30A4 U+30A6 (katakana AIU), Shift_JIS 0x83 0x41 ...
        let bytes = [0x83, 0x41, 0x83, 0x43, 0x83, 0x45];
        assert_eq!(
            decode_cjk_string(&bytes, "ISO_IR 13"),
            "\u{30A2}\u{30A4}\u{30A6}"
        );
    }

    #[test]
    fn iso_ir_13_decodes_shift_jis_kanji() {
        // U+5C71 U+7530 (yamada), Shift_JIS 0x8E 0x52 0x93 0x63.
        let bytes = [0x8E, 0x52, 0x93, 0x63];
        assert_eq!(decode_cjk_string(&bytes, "ISO_IR 13"), "\u{5C71}\u{7530}");
    }

    #[test]
    fn iso_2022_ir_87_decodes_jis_x_0208() {
        // U+6771 U+4EAC (toukyou), JIS X 0208 in EUC-JP layout 0xC5 0xEC ...
        let bytes = [0xC5, 0xEC, 0xB5, 0xFE];
        assert_eq!(
            decode_cjk_string(&bytes, "ISO 2022 IR 87"),
            "\u{6771}\u{4EAC}"
        );
    }

    #[test]
    fn iso_2022_ir_159_decodes_jis_x_0212_three_byte_form() {
        // U+4E02 U+4E04 are JIS X 0212 supplementary kanji, which exist only in
        // the EUC-JP SS2/G2 three-byte form (lead byte 0x8F). This is the case
        // that separates IR 159 from IR 87's two-byte plane: a table that
        // dropped IR 159 would still pass the IR 87 test.
        let bytes = [0x8F, 0xB0, 0xA1, 0x8F, 0xB0, 0xA2];
        assert_eq!(
            decode_cjk_string(&bytes, "ISO 2022 IR 159"),
            "\u{4E02}\u{4E04}"
        );
    }

    #[test]
    fn gb18030_decodes_bmp_and_four_byte_plane() {
        // U+4E2D U+6587 (zhongwen), GB18030 two-byte.
        assert_eq!(
            decode_cjk_string(&[0xD6, 0xD0, 0xCE, 0xC4], "GB18030"),
            "\u{4E2D}\u{6587}"
        );
        // U+20000 (CJK Ext B), GB18030 four-byte 0x95 0x32 0x82 0x36. A
        // GB2312-only table would fail here, so this proves the wide plane.
        assert_eq!(
            decode_cjk_string(&[0x95, 0x32, 0x82, 0x36], "GB18030"),
            "\u{20000}"
        );
    }

    #[test]
    fn iso_ir_149_decodes_euc_kr() {
        // U+D55C U+AD6D (hanguk), EUC-KR 0xC7 0xD1 0xB1 0xB9.
        let bytes = [0xC7, 0xD1, 0xB1, 0xB9];
        assert_eq!(decode_cjk_string(&bytes, "ISO_IR 149"), "\u{D55C}\u{AD6D}");
    }

    #[test]
    fn a_truncated_multibyte_sequence_becomes_u_fffd() {
        // A lone lead byte cannot complete a Shift_JIS sequence. The decoder
        // must substitute U+FFFD rather than panic or drop the value: a
        // corrupt PN field should still yield a usable (if ugly) string.
        let out = decode_cjk_string(&[0x81], "ISO_IR 13");
        assert_eq!(out, "\u{FFFD}");
    }

    #[test]
    fn valid_prefix_survives_a_truncated_tail() {
        // 0x8E 0x52 is U+5C71; the trailing 0x81 is a truncated lead. The good
        // character must be preserved and only the bad byte replaced.
        let out = decode_cjk_string(&[0x8E, 0x52, 0x81], "ISO_IR 13");
        assert_eq!(out, "\u{5C71}\u{FFFD}");
    }

    #[test]
    fn ascii_passes_through_every_charset() {
        // A CJK charset must not disturb the ASCII range (PS3.5 6.1.2: the
        // default repertoire is invariant across the supported sets).
        for cs in [
            "ISO_IR 13",
            "ISO 2022 IR 87",
            "ISO 2022 IR 159",
            "GB18030",
            "ISO_IR 149",
        ] {
            assert_eq!(decode_cjk_string(b"DOE^JOHN", cs), "DOE^JOHN", "{cs}");
        }
    }

    #[test]
    fn a_jmn_iso2022_term_reaches_the_cjk_decoder() {
        // The regression: `starts_with("ISO 2022")` used to shadow every JMN
        // term, so this text decoded as Latin-1 (one byte per character) and
        // the kanji came out as mojibake. Routing through the full string
        // decoder is what the caller actually does.
        // EUC-JP 0xBB 0xB3 0xC5 0xC4 = U+5C71 U+7530 (yamada).
        let bytes = [0xBB, 0xB3, 0xC5, 0xC4];
        assert_eq!(
            super::decode_dicom_string(&bytes, Some("ISO 2022 IR 87")),
            "\u{5C71}\u{7530}"
        );
    }
}

/// Decode a DICOM string value from raw bytes to a Rust UTF-8 String.
///
/// # Parameters
/// - `bytes`: trimmed raw value bytes (without trailing spaces/nulls)
/// - `charset`: value of SpecificCharacterSet (0008,0005), e.g. "ISO_IR 100"
///
/// # Supported charsets
/// - `None` or `"ISO_IR 100"` -> Latin-1 (ISO 8859-1): default per DICOM PS3.5 Section 6.1.2
/// - `"ISO_IR 144"` -> Cyrillic (ISO 8859-5)
/// - `"ISO_IR 101"` -> Latin-2 / Central European (ISO 8859-2)
/// - `"ISO_IR 109"` -> Latin-3 / South European (ISO 8859-3)
/// - `"ISO_IR 110"` -> Latin-4 / North European (ISO 8859-4)
/// - `"ISO_IR 148"` -> Latin-5 / Turkish (ISO 8859-9)
/// - CJK charsets -> requires `cjk-charsets` feature (encoding_rs)
/// - ISO 2022 -> log warning, fallback to Latin-1 (tracked as #P7-ISO2022)
/// - Unknown charset -> log warning, fallback to Latin-1
fn decode_dicom_string(bytes: &[u8], charset: Option<&str>) -> String {
    // Select lookup table based on SpecificCharacterSet
    let table: Option<&[char; 96]> = match charset {
        Some("ISO_IR 192") => {
            // UTF-8 (DICOM PS3.5 Section 6.1.2.3): standard UTF-8 encoding.
            // Multi-byte sequences are native Rust strings - just validate.
            return String::from_utf8_lossy(bytes).into_owned();
        }
        Some("ISO_IR 144") => Some(&ISO_8859_5_TABLE), // Cyrillic
        Some("ISO_IR 101") => Some(&ISO_8859_2_TABLE), // Latin-2 (Central European)
        Some("ISO_IR 109") => Some(&ISO_8859_3_TABLE), // Latin-3 (South European)
        Some("ISO_IR 110") => Some(&ISO_8859_4_TABLE), // Latin-4 (North European)
        Some("ISO_IR 148") => Some(&ISO_8859_9_TABLE), // Latin-5 (Turkish)
        Some("ISO_IR 100") => None,                    // Latin-1 (default, explicit)
        // The CJK arms must come BEFORE the generic ISO 2022 arm: every JMN
        // term ("ISO 2022 IR 87", "ISO 2022 IR 159") starts with "ISO 2022",
        // so the prefix match below shadowed them and Japanese kanji decoded
        // as Latin-1 - one byte per character, every one of them wrong.
        #[cfg(feature = "cjk-charsets")]
        Some(cs) if is_cjk_charset(cs) => {
            return decode_cjk_string(bytes, cs);
        }
        Some(cs) if cs.starts_with("ISO 2022") => {
            // DICOM PS3.5 Section 6.1.2.4: complex multi-charset switching via
            // escape sequences is still not implemented for the remaining
            // terms (ISO 2022 IR 58, IR 149 with extensions, ...).
            log::warn!(
                "ISO 2022 charset '{}' is not supported. \
                 Decoding as Latin-1 (ISO_IR 100).",
                cs
            );
            None // fallback to Latin-1
        }
        Some(other) => {
            // Unrecognized charset - log once, fallback to Latin-1 per DICOM PS3.5 Section 6.1.2.
            // Latin-1 is the safe default: each byte maps directly to Unicode codepoint.
            log::warn!(
                "Unrecognized SpecificCharacterSet '{}' — will decode as Latin-1. \
                 Add to parser.rs if this is a standard DICOM charset.",
                other
            );
            None
        }
        None => None, // default: Latin-1 (ISO_IR 100)
    };

    match table {
        Some(lookup) => bytes
            .iter()
            .map(|&b| {
                if b < 0x80 {
                    b as char // ASCII
                } else if b >= 0xA0 {
                    lookup[(b - 0xA0) as usize]
                } else {
                    '\u{FFFD}' // C1 control -> replacement char
                }
            })
            .collect(),
        None => {
            // Latin-1 (ISO_IR 100): each byte maps to Unicode codepoint b.
            // This is the default per DICOM PS3.5 Section 6.1.2.
            bytes.iter().map(|&b| b as char).collect()
        }
    }
}

// ===========================================================================
// Value converters
// ===========================================================================

fn required_str(value: &[u8], charset: Option<&str>, label: &str) -> Result<String, DicomError> {
    let trimmed = trim_trailing(value);
    if trimmed.is_empty() {
        return Err(DicomError::ParseError(format!(
            "Empty required DICOM value: {label}"
        )));
    }
    Ok(decode_dicom_string(trimmed, charset))
}

fn opt_str(value: &[u8], charset: Option<&str>) -> Option<String> {
    let trimmed = trim_trailing(value);
    if trimmed.is_empty() {
        return None;
    }
    Some(decode_dicom_string(trimmed, charset))
}

fn bytes_to_opt_string(value: &[u8], charset: Option<&str>) -> Option<String> {
    opt_str(value, charset)
}

/// Parsed components of a DICOM PersonName (PS3.5 Section 6.2.1).
///
/// Extracted from the Alphabetic group (before the first `=`).
/// See `parse_pn_components()` for the top-level parse.
#[derive(Debug, Clone, Default)]
pub(crate) struct PnComponents {
    /// Family name (фамилия) - component 1
    pub family: String,
    /// Given name (имя) - component 2
    pub given: String,
    /// Middle name (отчество) - component 3
    pub middle: String,
    /// Prefix (префикс, e.g. "д-р") - component 4
    pub prefix: String,
    /// Suffix (суффикс, e.g. "мл.") - component 5
    pub suffix: String,
}

impl PnComponents {
    /// Build full Alphabetic DICOM string: "Family^Given^Middle^Prefix^Suffix".
    /// Trailing empty components are trimmed (no trailing `^`).
    pub(crate) fn to_dicom_string(&self) -> String {
        let parts: [&str; 5] = [
            &self.family,
            &self.given,
            &self.middle,
            &self.prefix,
            &self.suffix,
        ];
        // Find last non-empty component
        let last = parts
            .iter()
            .rposition(|s| !s.is_empty())
            .map_or(0, |i| i + 1);
        parts[..last].join("^")
    }
}

/// Parse a DICOM PersonName (PN) value into its components.
///
/// DICOM PS3.5 Section 6.2.1 defines PN as:
///   Alphabetic^Ideographic^Phonetic (3 groups via `=`)
///   Family^Given^Middle^Prefix^Suffix   (5 components via `^`)
///
/// This function extracts the Alphabetic group and splits it into
/// 5 components. Ideographic and Phonetic groups are discarded -
/// they require ISO 2022 escape-sequence decoding (v1.1).
///
/// # Errors
/// Returns `InvalidDicom` if the alphabetic component is empty.
fn parse_pn_components(raw: &[u8], charset: Option<&str>) -> Result<PnComponents, DicomError> {
    // PatientName is DICOM Type 2 - empty value is valid (anonymized studies).
    let full = match opt_str(raw, charset) {
        Some(s) => s,
        None => {
            return Ok(PnComponents {
                family: String::new(),
                given: String::new(),
                middle: String::new(),
                prefix: String::new(),
                suffix: String::new(),
            });
        }
    };

    // DICOM PS3.5 Section 6.2.1: take Alphabetic group (before first `=`)
    let alpha = full.split('=').next().unwrap_or(&full);

    // Split into 5 components by `^`
    let mut parts: Vec<&str> = alpha.split('^').collect();
    parts.resize(5, "");

    let family = parts[0].to_string();
    let given = parts[1].to_string();
    let middle = parts[2].to_string();
    let prefix = parts[3].to_string();
    let suffix = parts[4].to_string();

    // Type 2: empty name components are valid (anonymized studies).
    // The coercion logic in the server fills them per clarus.conf settings.

    Ok(PnComponents {
        family,
        given,
        middle,
        prefix,
        suffix,
    })
}

fn opt_int(value: &[u8], charset: Option<&str>) -> Option<i32> {
    bytes_to_opt_string(value, charset)?.parse::<i32>().ok()
}

/// Parse a raw binary u16 (US, SS, OW VRs).
///
/// DICOM US/SS/OW are binary - NOT ASCII strings. `opt_int` uses
/// `decode_dicom_string` -> `parse::<i32>()` which fails on raw bytes.
/// This function reads 2 bytes as little-endian u16 directly.
///
/// Returns `None` if the value has fewer than 2 bytes.
fn opt_u16(value: &[u8]) -> Option<u16> {
    if value.len() < 2 {
        return None;
    }
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn trim_trailing(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 {
        match bytes[end - 1] {
            b' ' | b'\0' | b'\r' | b'\n' | b'\t' => end -= 1,
            _ => break,
        }
    }
    &bytes[..end]
}

// ===========================================================================
// DICOM Data Dictionary - VR lookup for Implicit VR Transfer Syntax
// ===========================================================================

/// Look up the VR for a DICOM tag in implicit VR transfer syntax.
///
/// PS3.5 6.2: an Implicit VR dataset carries no VR at the element, so the VR
/// exists only in the dictionary (PS3.6 6) - a tag missing from it renders as
/// `UN` and loses its type, its JSON shape and its numeric conversion. The
/// hand-kept table below once covered a few dozen tags and left everything
/// else `UN`; the generated PS3.6 table now answers first.
fn lookup_vr(group: u16, element: u16) -> &'static str {
    // Group-length tags always have VR=UL (PS3.5 7.2, Table 7.2-1).
    if element == 0x0000 {
        return "UL";
    }
    // The generated PS3.6 table. A tag absent from it falls through to the
    // local overrides, then to UN.
    if let Some(vr) = crate::codec::vr_dict_std::standard_vr(group, element) {
        return vr;
    }
    lookup_vr_local(group, element)
}

/// Hand-kept VRs for the tags the renderer treats specially.
///
/// Kept after the dictionary so the explicit list still documents which tags
/// this renderer depends on; `standard_vr` already answers all of them.
fn lookup_vr_local(group: u16, element: u16) -> &'static str {
    match (group, element) {
        // -- Group 0002: Meta-header (always Explicit VR, included for completeness) -
        (0x0002, 0x0010) => "UI", // TransferSyntaxUID
        (0x0002, 0x0012) => "UI", // ImplementationClassUID
        (0x0002, 0x0013) => "SH", // ImplementationVersionName

        // -- Group 0008: Identifying --------------------------------------
        (0x0008, 0x0005) => "CS", // SpecificCharacterSet
        (0x0008, 0x0008) => "CS", // ImageType
        (0x0008, 0x0012) => "DA", // InstanceCreationDate
        (0x0008, 0x0013) => "TM", // InstanceCreationTime
        (0x0008, 0x0016) => "UI", // SOPClassUID
        (0x0008, 0x0018) => "UI", // SOPInstanceUID
        (0x0008, 0x0020) => "DA", // StudyDate
        (0x0008, 0x0021) => "DA", // SeriesDate
        (0x0008, 0x0022) => "DA", // AcquisitionDate
        (0x0008, 0x0023) => "DA", // ContentDate
        (0x0008, 0x0030) => "TM", // StudyTime
        (0x0008, 0x0031) => "TM", // SeriesTime
        (0x0008, 0x0032) => "TM", // AcquisitionTime
        (0x0008, 0x0033) => "TM", // ContentTime
        (0x0008, 0x0050) => "SH", // AccessionNumber
        (0x0008, 0x0060) => "CS", // Modality
        (0x0008, 0x0070) => "LO", // Manufacturer
        (0x0008, 0x0080) => "LO", // InstitutionName
        (0x0008, 0x0090) => "PN", // ReferringPhysicianName
        (0x0008, 0x1010) => "SH", // StationName
        (0x0008, 0x1030) => "LO", // StudyDescription
        (0x0008, 0x103E) => "LO", // SeriesDescription
        (0x0008, 0x1090) => "LO", // ManufacturerModelName

        // -- Group 0010: Patient ------------------------------------------
        (0x0010, 0x0010) => "PN", // PatientName
        (0x0010, 0x0020) => "LO", // PatientID
        (0x0010, 0x0030) => "DA", // PatientBirthDate
        (0x0010, 0x0040) => "CS", // PatientSex
        (0x0010, 0x1010) => "AS", // PatientAge
        (0x0010, 0x1020) => "DS", // PatientSize
        (0x0010, 0x1030) => "DS", // PatientWeight

        // -- Group 0018: Acquisition --------------------------------------
        (0x0018, 0x0010) => "LO", // ContrastBolusAgent
        (0x0018, 0x0015) => "CS", // BodyPartExamined
        (0x0018, 0x0022) => "CS", // ScanOptions
        (0x0018, 0x0050) => "DS", // SliceThickness
        (0x0018, 0x0060) => "DS", // KVP
        (0x0018, 0x0088) => "DS", // SpacingBetweenSlices
        (0x0018, 0x1030) => "LO", // ProtocolName
        (0x0018, 0x5100) => "CS", // PatientPosition

        // -- Group 0020: Image --------------------------------------------
        (0x0020, 0x000D) => "UI", // StudyInstanceUID
        (0x0020, 0x000E) => "UI", // SeriesInstanceUID
        (0x0020, 0x0010) => "SH", // StudyID
        (0x0020, 0x0011) => "IS", // SeriesNumber
        (0x0020, 0x0013) => "IS", // InstanceNumber
        (0x0020, 0x0020) => "CS", // PatientOrientation
        (0x0020, 0x0032) => "DS", // ImagePositionPatient
        (0x0020, 0x0037) => "DS", // ImageOrientationPatient
        (0x0020, 0x0052) => "UI", // FrameOfReferenceUID
        (0x0020, 0x0060) => "CS", // Laterality
        (0x0020, 0x1041) => "DS", // SliceLocation
        (0x0020, 0x1206) => "IS", // NumberOfStudyRelatedSeries
        (0x0020, 0x1208) => "IS", // NumberOfStudyRelatedInstances

        // -- Group 0028: Image Presentation -------------------------------
        (0x0028, 0x0002) => "US", // SamplesPerPixel
        (0x0028, 0x0004) => "CS", // PhotometricInterpretation
        (0x0028, 0x0006) => "US", // PlanarConfiguration
        (0x0028, 0x0008) => "IS", // NumberOfFrames
        (0x0028, 0x0010) => "US", // Rows
        (0x0028, 0x0011) => "US", // Columns
        (0x0028, 0x0030) => "DS", // PixelSpacing
        (0x0028, 0x0100) => "US", // BitsAllocated
        (0x0028, 0x0101) => "US", // BitsStored
        (0x0028, 0x0102) => "US", // HighBit
        (0x0028, 0x0103) => "US", // PixelRepresentation
        (0x0028, 0x0106) => "US", // SmallestImagePixelValue
        (0x0028, 0x0107) => "US", // LargestImagePixelValue
        (0x0028, 0x1050) => "DS", // WindowCenter
        (0x0028, 0x1051) => "DS", // WindowWidth
        (0x0028, 0x1052) => "DS", // RescaleIntercept
        (0x0028, 0x1053) => "DS", // RescaleSlope
        (0x0028, 0x1054) => "LO", // RescaleType
        (0x0028, 0x2110) => "CS", // LossyImageCompression
        (0x0028, 0x2112) => "DS", // LossyImageCompressionRatio

        // -- Group 0032: Study -------------------------------------------
        (0x0032, 0x000A) => "CS", // StudyStatusID
        (0x0032, 0x1032) => "PN", // RequestingPhysician
        (0x0032, 0x1060) => "LO", // RequestedProcedureDescription

        // -- Group 0040: Procedure ----------------------------------------
        (0x0040, 0x0002) => "DA", // ScheduledProcedureStepStartDate
        (0x0040, 0x0003) => "TM", // ScheduledProcedureStepStartTime
        (0x0040, 0x0004) => "DA", // ScheduledProcedureStepEndDate
        (0x0040, 0x0005) => "TM", // ScheduledProcedureStepEndTime
        (0x0040, 0x0253) => "SH", // PerformedProcedureStepID
        (0x0040, 0x0254) => "LO", // PerformedProcedureStepDescription
        (0x0040, 0x1001) => "SH", // RequestedProcedureID

        // -- Group 7FE0: Pixel Data ---------------------------------------
        (0x7FE0, 0x0010) => "OW", // PixelData

        _ => "UN",
    }
}

// ===========================================================================
// DICOMweb JSON extraction - extract_full_metadata()
// ===========================================================================

/// String VRs: value is a string array.
const STRING_VRS: &[&str] = &[
    "AE", "AS", "CS", "DA", "DS", "DT", "IS", "LO", "LT", "SH", "ST", "TM", "UC", "UI", "UR", "UT",
];

/// Numeric VRs: value is a number array.
const NUMERIC_VRS: &[&str] = &["FL", "FD", "SL", "SS", "UL", "US"];

/// Binary VRs: value is a single inline-base64 string under `InlineBinary`.
const BINARY_VRS: &[&str] = &["OB", "OD", "OF", "OL", "OV", "OW", "UN"];

impl DicomRsParser {
    // P2 (deferred - v0.4): extract_metadata() monolithic match -> split into
    // parse_patient_module/study/series/equipment sub-parsers.
    // Parser WORKS. Deferring: too risky before field deployment.

    /// serde-free DICOMweb metadata renderer.
    ///
    /// Writes the DICOMweb JSON directly (no `serde_json::Value`, no `serde`
    /// at all). The output is byte-identical to the serde-era renderer for the
    /// same input - proven by the frozen capture goldens over the full corpus
    /// (Commit A). This is the renderer used by WADO: the instance `.dcm` is
    /// read from CAS and this produces the `/metadata` body inline, without the
    /// SQLite `metadata_blob`.
    ///
    /// Key order mirrors the serde `Map` (BTreeMap): tags ascending by hex
    /// string (= numeric, fixed-width). Each tag object emits `"Value"` before
    /// `"vr"` (alphabetical), values converted per-VR (PN decomposition,
    /// base64 binary, charset-aware strings, fixed-width numerics).
    pub fn extract_full_metadata_bytes(&self, data: &[u8]) -> Result<Vec<u8>, DicomError> {
        self.extract_full_metadata_bytes_inner(data, false)
    }

    /// 0310 F3: render instance metadata from a BOUNDED header.
    ///
    /// Identical output to [`extract_full_metadata_bytes`] for any slice that
    /// contains every element before PixelData plus the PixelData tag+length
    /// (a bounded head). It activates the reader's `stop_at_pixel_data` mode so
    /// the (possibly multi-GB) PixelData value need not be in `data`: the
    /// element is returned with an empty value range and the walk breaks, so
    /// the metadata render never touches a pixel byte.
    ///
    /// The WADO metadata route reads only `max_part_header` bytes of the CAS
    /// blob and calls this, so an instance of any size costs a bounded read.
    pub fn extract_full_metadata_bytes_bounded(&self, head: &[u8]) -> Result<Vec<u8>, DicomError> {
        self.extract_full_metadata_bytes_inner(head, true)
    }

    fn extract_full_metadata_bytes_inner(
        &self,
        data: &[u8],
        bounded: bool,
    ) -> Result<Vec<u8>, DicomError> {
        let mut reader = DicomReader::new(data)?;
        reader.stop_at_pixel_data = bounded;
        let ts_uid = reader.transfer_syntax_uid.clone();
        let syntax = reader.syntax;
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();

        while let Some(element) = reader.next_element()? {
            let tag = element.tag;

            // Stop at PixelData (same as the serde renderer).
            if tag == tags::PIXEL_DATA {
                break;
            }

            // Capture SpecificCharacterSet for text decoding.
            if tag == tags::SPECIFIC_CHARACTER_SET {
                let val = &reader.data[element.value_start..element.value_end];
                reader.charset = opt_str(val, None);
            }

            let val = &reader.data[element.value_start..element.value_end];
            let cs = reader.charset.as_deref();

            let vr = match element.vr {
                Some(_) => element.vr_str(),
                None => lookup_vr(tag.0, tag.1),
            };

            let hex_key = format!("{:04X}{:04X}", tag.0, tag.1);
            if entries.iter().any(|(k, _)| *k == hex_key) {
                continue;
            }

            let mut value = Vec::new();
            if !Self::write_element_json_native(&mut value, val, vr, cs, syntax, 0)? {
                // Zero-length scalar value: omit the attribute entirely.
                // Emitting `"Value":[]` makes dicom-microscopy-viewer produce
                // an empty array, which crashes viewers that expect a string
                // or number (e.g. Slim's date/name parsers).
                continue;
            }
            let mut tagged = Vec::with_capacity(value.len() + 8);
            tagged.push(b'"');
            tagged.extend_from_slice(hex_key.as_bytes());
            tagged.extend_from_slice(b"\":");
            tagged.extend_from_slice(&value);
            entries.push((hex_key, tagged));
        }

        // Inject TransferSyntaxUID (0002,0010) if not already present.
        if let Some(ts) = ts_uid
            && !entries.iter().any(|(k, _)| k == "00020010")
        {
            let mut v = Vec::with_capacity(ts.len() + 32);
            v.extend_from_slice(b"{\"");
            v.extend_from_slice(b"Value");
            v.extend_from_slice(b"\":[");
            push_json_str_native(&mut v, &ts);
            v.extend_from_slice(b"],\"vr\":\"UI\"}");
            let mut tagged = Vec::new();
            tagged.extend_from_slice(br#""00020010":"#);
            tagged.extend_from_slice(&v);
            entries.push(("00020010".to_string(), tagged));
        }

        // Sort by hex key (BTreeMap order: fixed-width hex -> numeric asc).
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = Vec::with_capacity(entries.len() * 24);
        out.push(b'{');
        for (i, (_, tagged)) in entries.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(tagged);
        }
        out.push(b'}');
        Ok(out)
    }

    /// Compose one DICOMweb tag value `{"Value":[...],"vr":"VR"}` natively,
    /// mirroring [`convert_element_to_dicomweb_json`] byte-for-byte.
    ///
    /// Each branch builds the `Value` payload into `v`, then wraps it with the
    /// uniform `{"Value":<v>,"vr":"VR"}` framing via [`write_tag_value_object_native`].
    fn write_element_json_native(
        out: &mut Vec<u8>,
        raw: &[u8],
        vr: &str,
        charset: Option<&str>,
        syntax: TransferSyntax,
        depth: usize,
    ) -> Result<bool, DicomError> {
        let mut v = Vec::new();
        match vr {
            "PN" => {
                let trimmed = trim_trailing(raw);
                if trimmed.is_empty() {
                    // Zero-length scalar: omit the attribute (see caller).
                    return Ok(false);
                }
                let decoded = decode_dicom_string(trimmed, charset);
                let groups = split_pn_groups(&decoded);
                let mut obj = Vec::new();
                obj.extend_from_slice(br#"{"Alphabetic":"#);
                push_json_str_native(&mut obj, &groups.alphabetic);
                if let Some(ideo) = groups.ideographic {
                    obj.extend_from_slice(br#","Ideographic":"#);
                    push_json_str_native(&mut obj, &ideo);
                }
                if let Some(phone) = groups.phonetic {
                    obj.extend_from_slice(br#","Phonetic":"#);
                    push_json_str_native(&mut obj, &phone);
                }
                obj.push(b'}');
                v.extend_from_slice(b"\"Value\":[");
                v.extend_from_slice(&obj);
                v.push(b']');
                write_tag_value_object_native(out, vr, &v);
            }
            "AT" => {
                let tags = decode_at_values_count(raw);
                if tags.is_empty() {
                    // An empty collection is not a value. Emitting
                    // `"Value":[]` produced an attribute a consumer must
                    // special-case, and the Slim decoders choke on the shape
                    // rather than skipping it (PS3.18 F.2.2: a zero-length
                    // value carries no `Value`).
                    return Ok(false);
                }
                v.extend_from_slice(b"\"Value\":[");
                for (i, t) in tags.iter().enumerate() {
                    if i > 0 {
                        v.push(b',');
                    }
                    v.push(b'"');
                    v.extend_from_slice(t.as_bytes());
                    v.push(b'"');
                }
                v.push(b']');
                write_tag_value_object_native(out, vr, &v);
            }
            vr if STRING_VRS.contains(&vr) => {
                let trimmed = trim_trailing(raw);
                if trimmed.is_empty() {
                    // Zero-length scalar: omit the attribute (see caller).
                    return Ok(false);
                }
                v.extend_from_slice(b"\"Value\":[");
                let decoded = decode_dicom_string(trimmed, charset);
                let mut first = true;
                for p in decoded.split('\\') {
                    if !first {
                        v.push(b',');
                    }
                    first = false;
                    push_json_str_native(&mut v, p);
                }
                v.push(b']');
                write_tag_value_object_native(out, vr, &v);
            }
            vr if NUMERIC_VRS.contains(&vr) => {
                let numbers = decode_numeric_values_raw(raw, vr, syntax);
                if numbers.is_empty() {
                    // Same rule as the AT branch: a zero-length or short
                    // value has no numbers, and `"Value":[]` is not a shape a
                    // consumer can treat as absent.
                    return Ok(false);
                }
                v.extend_from_slice(b"\"Value\":[");
                for (i, n) in numbers.iter().enumerate() {
                    if i > 0 {
                        v.push(b',');
                    }
                    n.write_to(&mut v);
                }
                v.push(b']');
                write_tag_value_object_native(out, vr, &v);
            }
            vr if BINARY_VRS.contains(&vr) => {
                // PS3.18 F.2.2: a binary VR carries a SINGLE base64 string
                // under `InlineBinary`, not a `Value` array. Slim
                // (dicom-microscopy-viewer) decodes binary base64 only from
                // `InlineBinary`.
                let b64 = base64_encode(raw);
                v.extend_from_slice(b"\"InlineBinary\":");
                push_json_str_native(&mut v, &b64);
                write_tag_value_object_native(out, vr, &v);
            }
            "SQ" => {
                // Nested sequence: render each item as a JSON object, so the
                // value is an array of objects (PS3.18 8.7.3.2). An empty
                // sequence stays an empty array. Beyond the depth cap the
                // sequence renders empty rather than recursing without bound.
                v.extend_from_slice(b"\"Value\":[");
                if depth < MAX_SQ_DEPTH {
                    write_sq_items_json_native(&mut v, raw, syntax, charset, depth + 1)?;
                }
                v.push(b']');
                write_tag_value_object_native(out, vr, &v);
            }
            _ => {
                // Unknown VR: render as a binary value, i.e. a single inline
                // base64 string under `InlineBinary` (same shape as the
                // known binary VRs above).
                let b64 = base64_encode(raw);
                v.extend_from_slice(b"\"InlineBinary\":");
                push_json_str_native(&mut v, &b64);
                write_tag_value_object_native(out, vr, &v);
            }
        }
        Ok(true)
    }
}

/// Maximum nested-sequence depth rendered into WADO-RS metadata JSON.
///
/// Real DICOM object nesting is at most about 4 levels; 8 leaves headroom for
/// unusual objects while bounding the recursion on a hostile or malformed
/// file (unbounded recursion is a crash class - a deeply nested synthetic
/// DICOM once aborted a worker). Beyond the cap a sequence renders as `[]` and
/// the rest of the object still renders.
const MAX_SQ_DEPTH: usize = 8;

/// Render the items of an SQ value as a DICOMweb-JSON array of objects.
///
/// `raw` is the SQ value: a stream of `(FFFE,E000)` items, each carrying a
/// dataset, terminated by an item/sequence delimitation for undefined-length
/// forms. The item grammar mirrors [`collect_uids_in_range`]: items are
/// descended into rather than skipped, delimiters are consumed, odd value
/// lengths advance by their pad byte, and a nested SQ recurses.
///
/// VR handling branches on `syntax`: explicit VR carries 2 VR bytes plus a
/// 2- or 4-byte length; implicit VR has no VR bytes (4-byte length) and the VR
/// is looked up from the tag. Big-endian is not handled here (LE item walk).
fn write_sq_items_json_native(
    out: &mut Vec<u8>,
    raw: &[u8],
    syntax: TransferSyntax,
    charset: Option<&str>,
    depth: usize,
) -> Result<(), DicomError> {
    let data = raw;
    let end = data.len();
    let mut pos = 0usize;
    let mut first_item = true;

    while pos + 8 <= end {
        let tag = (read_u16_le(data, pos), read_u16_le(data, pos + 2));

        // An item delimiter ends the enclosing item and a sequence delimiter
        // ends the enclosing sequence; neither carries a value.
        if tag == tags::ITEM_DELIMITATION || tag == tags::SEQUENCE_DELIMITATION {
            pos += 8;
            continue;
        }

        // Item header: tag + 4-byte length, no VR. Its body is a dataset, read
        // by continuing into it rather than skipping.
        if tag == tags::ITEM {
            let length = read_u32_le(data, pos + 4);
            pos += 8;
            let item_end = if length == tags::UNDEFINED_LENGTH {
                undefined_extent(data, pos, end).unwrap_or(end)
            } else {
                pos.saturating_add(length as usize).min(end)
            };
            // The item's dataset body excludes its own delimitation item (the
            // 8 bytes `undefined_extent` consumed); a defined-length item is
            // exactly its body.
            let body_end = if length == tags::UNDEFINED_LENGTH {
                item_end.saturating_sub(8)
            } else {
                item_end
            };
            if !first_item {
                out.push(b',');
            }
            first_item = false;
            out.push(b'{');
            write_sq_dataset_json_native(out, data, pos, body_end, syntax, charset, depth)?;
            out.push(b'}');
            pos = item_end;
            continue;
        }

        // Anything else outside an item is malformed for an SQ; stop.
        break;
    }
    Ok(())
}

/// Render one item's dataset body (`raw[from..to]`) as the members of one JSON
/// object: `"TAG":{...}` pairs, comma-separated, no surrounding braces.
fn write_sq_dataset_json_native(
    out: &mut Vec<u8>,
    data: &[u8],
    mut pos: usize,
    end: usize,
    syntax: TransferSyntax,
    charset: Option<&str>,
    depth: usize,
) -> Result<(), DicomError> {
    let mut first = true;
    while pos + 8 <= end {
        let tag = (read_u16_le(data, pos), read_u16_le(data, pos + 2));

        // Defensive: an inner delimiter ends this item's dataset early.
        if tag == tags::ITEM_DELIMITATION || tag == tags::SEQUENCE_DELIMITATION {
            break;
        }
        if tag == tags::ITEM {
            // A bare item header inside a dataset is malformed; stop.
            break;
        }

        // Explicit VR carries 2 VR bytes; implicit VR carries none (the VR is
        // looked up from the tag). `vr_bytes` is hoisted so its `&str` view
        // lives as long as this iteration's use of `vr`.
        let mut vr_bytes = [0u8; 2];
        let (vr_raw, length, value_start) = match syntax {
            TransferSyntax::ImplicitVrLE => {
                if pos + 8 > end {
                    return Ok(());
                }
                (None, read_u32_le(data, pos + 4), pos + 8)
            }
            _ => {
                vr_bytes = [data[pos + 4], data[pos + 5]];
                if is_extended_vr(vr_bytes) {
                    if pos + 12 > end {
                        return Ok(());
                    }
                    (Some(vr_bytes), read_u32_le(data, pos + 8), pos + 12)
                } else {
                    (
                        Some(vr_bytes),
                        u32::from(read_u16_le(data, pos + 6)),
                        pos + 8,
                    )
                }
            }
        };
        let vr: &str = if vr_raw.is_some() {
            std::str::from_utf8(&vr_bytes).unwrap_or("UN")
        } else {
            lookup_vr(tag.0, tag.1)
        };

        let (value_end, next_pos) = if length == tags::UNDEFINED_LENGTH {
            if value_start > end {
                return Ok(());
            }
            let sq_end = undefined_extent(data, value_start, end).unwrap_or(end);
            (sq_end.saturating_sub(8), sq_end)
        } else {
            let ve = value_start.saturating_add(length as usize);
            if ve > end {
                return Ok(());
            }
            // Odd lengths are padded to an even boundary (PS3.5 7.1).
            (ve, ve + (length as usize % 2))
        };

        // Emit `"TAG":<value-object>`; the value object carries its own braces.
        // Zero-length scalar values are omitted, mirroring the top-level
        // renderer (extract_full_metadata_bytes).
        let mut value_json = Vec::new();
        if !DicomRsParser::write_element_json_native(
            &mut value_json,
            &data[value_start..value_end],
            vr,
            charset,
            syntax,
            depth + 1,
        )? {
            pos = next_pos;
            continue;
        }
        if !first {
            out.push(b',');
        }
        first = false;
        let mut key = [0u8; 8];
        write_hex_key(&mut key, tag.0, tag.1);
        out.push(b'"');
        out.extend_from_slice(&key);
        out.extend_from_slice(b"\":");
        out.extend_from_slice(&value_json);

        pos = next_pos;
    }
    Ok(())
}

/// Write the 8 uppercase hex characters of a tag into `key` (no allocation).
fn write_hex_key(key: &mut [u8; 8], group: u16, element: u16) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let full = ((group as u32) << 16) | element as u32;
    for (i, slot) in key.iter_mut().enumerate() {
        let shift = (7 - i) * 4;
        *slot = HEX[((full >> shift) & 0xF) as usize];
    }
}

// -- native DICOMweb-JSON byte helpers (serde-free) ---------
//
// Mirrors the serde-era `convert_element_to_dicomweb_json` byte-for-byte but
// writes directly. String escaping is byte-identical to the server
// `json_common::push_json_str` (Family A), so the adapters renderer and the
// server writers agree exactly.

/// Append `s` as a JSON string (serde_json-exact escaping, Family A).
fn push_json_str_native(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x00..=0x1F => {
                out.extend_from_slice(b"\\u00");
                const LHEX: &[u8; 16] = b"0123456789abcdef";
                out.push(LHEX[(b >> 4) as usize]);
                out.push(LHEX[(b & 0xf) as usize]);
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

/// A serde-free JSON scalar for numeric VRs. Writes exactly like
/// `serde_json::Number` (integral stays bare; float uses ryu positional with
/// a `.0` kept for whole floats).
enum NativeJsonNum {
    U64(u64),
    I64(i64),
    F64(f64),
}

impl NativeJsonNum {
    fn write_to(&self, out: &mut Vec<u8>) {
        match self {
            NativeJsonNum::U64(v) => push_u64_native(out, *v),
            NativeJsonNum::I64(v) => {
                if *v < 0 {
                    out.push(b'-');
                    // `unsigned_abs` yields the magnitude (2^63 for i64::MIN).
                    push_u64_native(out, (*v).unsigned_abs());
                } else {
                    push_u64_native(out, *v as u64);
                }
            }
            NativeJsonNum::F64(v) => push_f64_native(out, *v),
        }
    }
}

/// Append a u64 in decimal (no leading zeros).
fn push_u64_native(out: &mut Vec<u8>, mut v: u64) {
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
    out.extend_from_slice(&buf[i..]);
}

/// Append `v` as a JSON number, byte-exact with serde_json's own float
/// serializer. serde_json delegates to `zmij::Buffer::format_finite` (the
/// shortest-round-trip formatter, successor of ryu), so we do the same to
/// guarantee byte-identity - covering integral `.0`, scientific `1e+30`,
/// subnormals, etc. (see the `metadata_bytes_native_matches_serde` capture
/// test which runs against the real `tests/**/*.dcm` corpus).
fn push_f64_native(out: &mut Vec<u8>, v: f64) {
    let mut buf = zmij::Buffer::new();
    out.extend_from_slice(buf.format_finite(v).as_bytes());
}

/// Base64-encode `data` per RFC 4648 (standard alphabet `A-Za-z0-9+/`,
/// `=` padding). Local encoder replaces the `base64` crate (diet 22.08:
/// -1 from the default production graph; only encode is needed for the
/// DICOMweb JSON renderer of binary/unknown-VR values).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut chunk = data.chunks(3);
    for c in &mut chunk {
        let b0 = c[0];
        let b1 = *c.get(1).unwrap_or(&0);
        let b2 = *c.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        match c.len() {
            1 => {
                out.push('=');
                out.push('=');
            }
            2 => {
                out.push(ALPHABET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
                out.push('=');
            }
            _ => {
                out.push(ALPHABET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
                out.push(ALPHABET[(b2 & 0x3F) as usize] as char);
            }
        }
    }
    out
}

/// Compose the outer `{"Value":[...],"vr":"VR"}` framings.
fn write_tag_value_object_native(out: &mut Vec<u8>, vr: &str, value: &[u8]) {
    out.extend_from_slice(b"{");
    out.extend_from_slice(value);
    out.extend_from_slice(b",\"vr\":\"");
    out.extend_from_slice(vr.as_bytes());
    out.extend_from_slice(b"\"}");
}

/// Decode numeric DICOM values (FL/FD/SL/SS/UL/US) into serde-free scalars,
/// mirroring `decode_numeric_values` (fixed-width LE, multi-valued).
fn decode_numeric_values_raw(bytes: &[u8], vr: &str, syntax: TransferSyntax) -> Vec<NativeJsonNum> {
    let width: usize = match vr {
        "US" | "SS" => 2,
        "UL" | "SL" | "FL" => 4,
        "FD" => 8,
        _ => return vec![],
    };
    if bytes.len() < width {
        return vec![];
    }
    // PS3.5 7.1: the transfer syntax fixes the byte order of every binary VR.
    // Explicit VR Big Endian stores them reversed, so a fixed `from_le_bytes`
    // rendered Rows, BitsAllocated and every rescale value byte-swapped - the
    // numbers were present but meaningless.
    let be = syntax == TransferSyntax::ExplicitVrBE;
    let u16_at = |s: &[u8]| {
        if be {
            u16::from_be_bytes([s[0], s[1]])
        } else {
            u16::from_le_bytes([s[0], s[1]])
        }
    };
    let u32_at = |s: &[u8]| {
        if be {
            u32::from_be_bytes([s[0], s[1], s[2], s[3]])
        } else {
            u32::from_le_bytes([s[0], s[1], s[2], s[3]])
        }
    };
    let count = bytes.len() / width;
    let mut values = Vec::with_capacity(count);
    for i in 0..count {
        let slice = &bytes[i * width..i * width + width];
        match vr {
            "US" => values.push(NativeJsonNum::U64(u16_at(slice) as u64)),
            "SS" => values.push(NativeJsonNum::I64(u16_at(slice) as i16 as i64)),
            "UL" => values.push(NativeJsonNum::U64(u32_at(slice) as u64)),
            "SL" => values.push(NativeJsonNum::I64(u32_at(slice) as i32 as i64)),
            "FL" => {
                // Mirror serde's Number::from_f64: NaN/+-Inf -> None -> the
                // value is SKIPPED (renders "Value":[]). zmij guards
                // internal exponents with assert!, so non-finite must never
                // reach push_f64_native (panic="abort" in release).
                let v = f32::from_bits(u32_at(slice)) as f64;
                if v.is_finite() {
                    values.push(NativeJsonNum::F64(v));
                }
            }
            "FD" => {
                let bits = if be {
                    u64::from_be_bytes([
                        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6],
                        slice[7],
                    ])
                } else {
                    u64::from_le_bytes([
                        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6],
                        slice[7],
                    ])
                };
                let v = f64::from_bits(bits);
                if v.is_finite() {
                    values.push(NativeJsonNum::F64(v));
                }
            }
            _ => {}
        }
    }
    values
}

/// Decode AT values as the serde-era quoted `"GGGG,EEEE"` strings.
fn decode_at_values_count(bytes: &[u8]) -> Vec<String> {
    if bytes.len() < 4 {
        return vec![];
    }
    let count = bytes.len() / 4;
    let mut values = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * 4;
        let group = u16::from_le_bytes([bytes[start], bytes[start + 1]]);
        let elem = u16::from_le_bytes([bytes[start + 2], bytes[start + 3]]);
        values.push(format!("{:04X},{:04X}", group, elem));
    }
    values
}

/// Decomposed DICOM PersonName (PN) value into its 3 component groups.
/// Per PS3.5 Section 6.2.1.2, PN components are separated by '^' within groups
/// and by '=' between groups: Alphabetic=Ideographic=Phonetic.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PnGroups {
    /// Alphabetic group (always present).
    pub alphabetic: String,
    /// Ideographic group (CJK characters, optional).
    pub ideographic: Option<String>,
    /// Phonetic group (kana/hiragana, optional).
    pub phonetic: Option<String>,
}

/// Split a DICOM PN value into its 3 component groups.
///
/// The input is a decoded DICOM PN string with groups separated by '=':
/// `Alphabetic=Ideographic=Phonetic`.
///
/// # Examples
///
/// ```ignore
/// let groups = split_pn_groups("SMITH^JOHN");
/// assert_eq!(groups.alphabetic, "SMITH^JOHN");
/// assert!(groups.ideographic.is_none());
///
/// let groups = split_pn_groups("ИВАНОВ^ИВАН=イワノフ^イワン=IVANOV^IVAN");
/// assert_eq!(groups.alphabetic, "ИВАНОВ^ИВАН");
/// assert_eq!(groups.ideographic.unwrap(), "イワノフ^イワン");
/// assert_eq!(groups.phonetic.unwrap(), "IVANOV^IVAN");
/// ```
pub fn split_pn_groups(value: &str) -> PnGroups {
    let mut groups = value.split('=');
    PnGroups {
        alphabetic: groups.next().unwrap_or("").to_string(),
        ideographic: groups.next().filter(|s| !s.is_empty()).map(String::from),
        phonetic: groups.next().filter(|s| !s.is_empty()).map(String::from),
    }
}

// removed `build_pn_json` (returned a `serde_json::Value`; its only
// production caller `json_common::tag_person_name` was itself dead and removed).
// The alive DICOMweb JSON renderers emit PersonName natively (Alphabetic always,
// via `push_pn_object`/the core writers) and are covered by the byte goldens.
// `split_pn_groups` remains (used by the native renderers).

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // -- Synthetic DICOM builder ----------------------------------------

    /// Build a minimal valid DICOM Part 10 byte sequence.
    ///
    /// Creates: 128-byte preamble + "DICM" + meta-header (TransferSyntaxUID)
    /// + dataset with specified tags -> values.
    struct DicomBuilder {
        buf: Vec<u8>,
    }

    impl DicomBuilder {
        fn new() -> Self {
            let mut buf = Vec::new();
            // 128-byte preamble (all zeros)
            buf.extend(std::iter::repeat_n(0u8, 128));
            // DICM magic
            buf.extend_from_slice(b"DICM");
            Self { buf }
        }

        /// Add the meta-header with TransferSyntaxUID (0002,0010).
        /// Meta-header elements ALWAYS use 4-byte length with 2 reserved bytes.
        fn meta_header(mut self, transfer_syntax_uid: &str) -> Self {
            let value = transfer_syntax_uid.as_bytes();
            // Tag: (0002,0010)
            self.buf.extend_from_slice(&0x0002_u16.to_le_bytes());
            self.buf.extend_from_slice(&0x0010_u16.to_le_bytes());
            // VR: "UI"
            self.buf.extend_from_slice(b"UI");
            // Reserved: 2 bytes
            self.buf.extend_from_slice(&[0u8, 0u8]);
            // Length: 4 bytes
            self.buf
                .extend_from_slice(&(value.len() as u32).to_le_bytes());
            // Value
            self.buf.extend_from_slice(value);
            // DICOM PS3.5 Section 7.1: pad byte after odd-length value
            if !value.len().is_multiple_of(2) {
                self.buf.push(0x00); // UI -> 0x00 pad
            }
            self
        }

        /// Add a DICOM data element (explicit VR little-endian) to the dataset.
        fn add_element(mut self, group: u16, elem: u16, vr: &[u8; 2], value: &[u8]) -> Self {
            self.add_explicit_element(group, elem, vr, value);
            self
        }

        /// Add a DICOM data element in Implicit VR LE format (no VR, 4-byte length).
        fn add_implicit_element(mut self, group: u16, elem: u16, value: &[u8]) -> Self {
            self.buf.extend_from_slice(&group.to_le_bytes());
            self.buf.extend_from_slice(&elem.to_le_bytes());
            self.buf
                .extend_from_slice(&(value.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(value);
            // DICOM PS3.5 Section 7.1: pad byte after odd-length value
            if !value.len().is_multiple_of(2) {
                self.buf.push(0x00);
            }
            self
        }

        fn add_explicit_element(&mut self, group: u16, elem: u16, vr: &[u8; 2], value: &[u8]) {
            // Tag: group (LE), element (LE)
            self.buf.extend_from_slice(&group.to_le_bytes());
            self.buf.extend_from_slice(&elem.to_le_bytes());
            // VR
            self.buf.extend_from_slice(vr);

            let vr_str = std::str::from_utf8(vr).unwrap_or("");
            let is_long = matches!(
                vr_str,
                "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            );

            if is_long {
                self.buf.extend_from_slice(&[0u8, 0u8]); // reserved
                self.buf
                    .extend_from_slice(&(value.len() as u32).to_le_bytes());
            } else {
                self.buf
                    .extend_from_slice(&(value.len() as u16).to_le_bytes());
            }
            self.buf.extend_from_slice(value);
            // DICOM PS3.5 Section 7.1: pad byte OUTSIDE Value Length for odd-length values
            if !value.len().is_multiple_of(2) {
                let pad = if vr_str == "CS"
                    || vr_str == "LO"
                    || vr_str == "SH"
                    || vr_str == "PN"
                    || vr_str == "AE"
                    || vr_str == "AS"
                    || vr_str == "DA"
                    || vr_str == "DS"
                    || vr_str == "DT"
                    || vr_str == "IS"
                    || vr_str == "LT"
                    || vr_str == "ST"
                    || vr_str == "TM"
                    || vr_str == "UC"
                    || vr_str == "UR"
                    || vr_str == "UT"
                {
                    0x20
                } else {
                    0x00
                };
                self.buf.push(pad);
            }
        }

        fn build(self) -> Vec<u8> {
            self.buf
        }
    }

    /// Create a minimal, valid DICOM file with the given tag values.
    fn make_minimal_dicom(patient_name: &str, modality: &str) -> Vec<u8> {
        DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1") // Explicit VR LE
            .add_element(0x0010, 0x0010, b"PN", patient_name.as_bytes()) // PatientName
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001") // PatientID
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.113619.2.55.3.1.2.1") // StudyInstanceUID
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1") // SeriesInstanceUID
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1.1") // SOPInstanceUID
            .add_element(0x0008, 0x0060, b"CS", modality.as_bytes()) // Modality
            .add_element(0x0008, 0x0020, b"DA", b"20200101") // StudyDate
            .add_element(0x0010, 0x0030, b"DA", b"19700101") // PatientBirthDate
            .add_element(0x0010, 0x0040, b"CS", b"M") // PatientSex
            .add_element(0x0020, 0x0011, b"IS", b"1") // SeriesNumber
            .build()
    }

    /// Read a committed fixture from the server test corpus.
    ///
    /// The derived-object fixtures are generated by
    /// `clarus/tests/make_derived_fixtures.py` and read here
    /// rather than rebuilt in Rust: they are the bytes a real producer wrote,
    /// so the capture is exercised against realistic item framing rather than
    /// against a shape invented to match the reader.
    fn derived_fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("clarus")
            .join("tests")
            .join("fixtures")
            .join(name);
        std::fs::read(&path)
            .unwrap_or_else(|e| panic!("fixture {} is unreadable: {e}", path.display()))
    }

    #[test]
    fn a_presentation_states_references_are_captured() {
        let meta = DicomRsParser
            .extract_metadata(&derived_fixture("derived_pr.dcm"))
            .expect("the PR fixture parses");
        assert_eq!(
            meta.sop_class_uid.as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.11.1")
        );
        // (0008,1115) -> SeriesInstanceUID, and (0008,1140) -> the image.
        assert_eq!(
            meta.referenced_series_uids,
            vec!["1.2.826.0.1.3680043.10.1709.1.1".to_string()]
        );
        assert_eq!(
            meta.referenced_sop_instance_uids,
            vec!["1.2.826.0.1.3680043.10.1709.1.1.1".to_string()]
        );
    }

    #[test]
    fn a_segmentations_references_are_captured_from_both_shapes() {
        let meta = DicomRsParser
            .extract_metadata(&derived_fixture("derived_seg.dcm"))
            .expect("the SEG fixture parses");
        assert_eq!(
            meta.sop_class_uid.as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.66.4")
        );
        // (0008,2112) SourceImageSequence -> the source image...
        assert_eq!(
            meta.referenced_sop_instance_uids,
            vec!["1.2.826.0.1.3680043.10.1709.1.1.1".to_string()]
        );
        // ...and the per-segment (0008,1115) -> the series.
        assert_eq!(
            meta.referenced_series_uids,
            vec!["1.2.826.0.1.3680043.10.1709.1.1".to_string()]
        );
    }

    #[test]
    fn an_ordinary_image_captures_no_references() {
        // The negative case the capture must not regress: a plain image has no
        // reference sequences, so both lists stay empty and the deletion
        // cascade never considers it.
        let meta = DicomRsParser
            .extract_metadata(&make_minimal_dicom("TEST^PATIENT", "CT"))
            .expect("the minimal file parses");
        assert!(meta.referenced_series_uids.is_empty());
        assert!(meta.referenced_sop_instance_uids.is_empty());
    }

    #[test]
    fn a_reference_is_recorded_once() {
        // A segmentation may name the same series per segment; the cascade
        // wants the set, so a repeat must not accumulate.
        let mut series = Vec::new();
        let mut instances = Vec::new();
        push_unique(&mut series, "1.2.3".to_string());
        push_unique(&mut series, "1.2.3".to_string());
        push_unique(&mut series, "4.5".to_string());
        push_unique(&mut instances, "6.7".to_string());
        push_unique(&mut instances, "6.7".to_string());
        assert_eq!(series, vec!["1.2.3".to_string(), "4.5".to_string()]);
        assert_eq!(instances, vec!["6.7".to_string()]);
    }

    // -- Derived-reference walk depth regression (20.09 field) ----------

    /// Append an undefined-length SQ header, an undefined-length item header,
    /// or an item/sequence delimitation item to the byte buffer.
    fn sq_undef(buf: &mut Vec<u8>, group: u16, elem: u16) {
        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&elem.to_le_bytes());
        buf.extend_from_slice(b"SQ");
        buf.extend_from_slice(&[0u8, 0u8]);
        buf.extend_from_slice(&tags::UNDEFINED_LENGTH.to_le_bytes());
    }

    fn item_undef(buf: &mut Vec<u8>) {
        buf.extend_from_slice(&tags::ITEM.0.to_le_bytes());
        buf.extend_from_slice(&tags::ITEM.1.to_le_bytes());
        buf.extend_from_slice(&tags::UNDEFINED_LENGTH.to_le_bytes());
    }

    fn item_delim(buf: &mut Vec<u8>) {
        buf.extend_from_slice(&tags::ITEM_DELIMITATION.0.to_le_bytes());
        buf.extend_from_slice(&tags::ITEM_DELIMITATION.1.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
    }

    fn seq_delim(buf: &mut Vec<u8>) {
        buf.extend_from_slice(&tags::SEQUENCE_DELIMITATION.0.to_le_bytes());
        buf.extend_from_slice(&tags::SEQUENCE_DELIMITATION.1.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
    }

    // -- Nested-SQ metadata rendering (WADO-RS /metadata) ---------------

    /// Append a defined-length item header carrying `body` (already padded).
    fn item_defined(buf: &mut Vec<u8>, body: &[u8]) {
        buf.extend_from_slice(&tags::ITEM.0.to_le_bytes());
        buf.extend_from_slice(&tags::ITEM.1.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(body);
    }

    /// Append a defined-length explicit-VR element to a raw byte buffer.
    fn explicit_element(buf: &mut Vec<u8>, group: u16, elem: u16, vr: &[u8; 2], value: &[u8]) {
        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&elem.to_le_bytes());
        buf.extend_from_slice(vr);
        let vr_str = std::str::from_utf8(vr).unwrap_or("");
        let is_long = matches!(
            vr_str,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        if is_long {
            buf.extend_from_slice(&[0u8, 0u8]);
            buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            buf.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        buf.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            buf.push(0x00);
        }
    }

    /// Build a nested-SQ DICOM: a top-level SQ (0040,A730 ContentSequence)
    /// whose single item holds a CodeSequence SQ (0008,0100) with one item
    /// carrying a CS value and an OB value.
    fn nested_sq_dicom() -> Vec<u8> {
        // Inner CodeSequence content: one item with (0008,0100) CS + OB.
        let mut inner_item = Vec::new();
        explicit_element(&mut inner_item, 0x0008, 0x0100, b"SH", b"CODE1");
        explicit_element(
            &mut inner_item,
            0x0008,
            0x0102,
            b"OB",
            &[0xDE, 0xAD, 0xBE, 0xEF],
        );

        let mut code_seq = Vec::new();
        item_defined(&mut code_seq, &inner_item);

        let mut content_item = Vec::new();
        explicit_element(&mut content_item, 0x0040, 0xA160, b"UT", b"nested text");
        // Nested CodeSequence (0008,0100) as its own SQ element.
        explicit_element(&mut content_item, 0x0008, 0x0100, b"SQ", &code_seq);

        let mut content_seq = Vec::new();
        item_defined(&mut content_seq, &content_item);

        let mut d = DicomBuilder::new().meta_header("1.2.840.10008.1.2.1");
        d = d.add_element(0x0010, 0x0010, b"PN", b"SQ^TEST");
        d = d.add_element(0x0020, 0x000D, b"UI", b"1.2.3");
        d = d.add_element(0x0040, 0xA730, b"SQ", &content_seq);
        d.build()
    }

    #[test]
    fn nested_sq_renders_recursively() {
        let data = nested_sq_dicom();
        let bytes = DicomRsParser
            .extract_full_metadata_bytes(&data)
            .expect("nested SQ renders");
        let s = String::from_utf8_lossy(&bytes).into_owned();

        // Top-level SQ is an array with one object, not an empty array.
        assert!(
            s.contains("\"0040A730\":{\"Value\":[{"),
            "top-level SQ must render one item object: {s}"
        );
        // The inner CodeSequence SQ nested inside the item also renders.
        assert!(
            s.contains("\"00080100\":{\"Value\":[{"),
            "nested CodeSequence must render recursively: {s}"
        );
        // The leaf SH value inside the inner item is present.
        assert!(
            s.contains("\"CODE1\""),
            "leaf value inside nested item must render: {s}"
        );
        // The OB value inside the nested item is inline base64 (0xDEADBEEF).
        assert!(
            s.contains("\"3q2+7w==\""),
            "binary inside SQ must be inline base64: {s}"
        );
    }

    #[test]
    fn empty_sq_stays_empty_array() {
        let mut d = DicomBuilder::new().meta_header("1.2.840.10008.1.2.1");
        d = d.add_element(0x0010, 0x0010, b"PN", b"SQ^EMPTY");
        d = d.add_element(0x0040, 0xA730, b"SQ", &[]);
        let bytes = DicomRsParser
            .extract_full_metadata_bytes(&d.build())
            .expect("empty SQ renders");
        let s = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            s.contains("\"0040A730\":{\"Value\":[],\"vr\":\"SQ\"}"),
            "empty SQ must stay an empty array: {s}"
        );
    }

    #[test]
    fn undefined_length_sq_and_item_render() {
        let mut d = DicomBuilder::new().meta_header("1.2.840.10008.1.2.1");
        d = d.add_element(0x0010, 0x0010, b"PN", b"SQ^UNDEF");
        // Undefined-length SQ (0040,A730) with one undefined-length item.
        sq_undef(&mut d.buf, 0x0040, 0xA730);
        item_undef(&mut d.buf);
        d.add_explicit_element(0x0040, 0xA160, b"UT", b"undef text");
        item_delim(&mut d.buf);
        seq_delim(&mut d.buf);
        let bytes = DicomRsParser
            .extract_full_metadata_bytes(&d.build())
            .expect("undefined-length SQ renders");
        let s = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            s.contains("\"0040A730\":{\"Value\":[{"),
            "undefined-length SQ must render its item: {s}"
        );
        assert!(
            s.contains("undef text"),
            "undefined-length item's value must render: {s}"
        );
    }

    #[test]
    fn depth_cap_renders_inner_sq_empty_without_panic() {
        // Build 16 levels of undefined-length SQ nesting. Beyond MAX_SQ_DEPTH
        // (8) the deeper sequences render as `[]`, and the renderer must not
        // panic or recurse without bound.
        const LEVELS: usize = 16;
        let mut d = DicomBuilder::new().meta_header("1.2.840.10008.1.2.1");
        d = d.add_element(0x0010, 0x0010, b"PN", b"SQ^DEEP");
        // Open LEVELS nested SQs, each with an item, then close them.
        for i in 0..LEVELS {
            sq_undef(&mut d.buf, 0x0040, 0xA730 + i as u16);
            item_undef(&mut d.buf);
        }
        d.add_explicit_element(0x0040, 0xA160, b"UT", b"deep leaf");
        for _ in 0..LEVELS {
            item_delim(&mut d.buf);
            seq_delim(&mut d.buf);
        }
        let bytes = DicomRsParser
            .extract_full_metadata_bytes(&d.build())
            .expect("deep nesting renders without panic");
        let s = String::from_utf8_lossy(&bytes).into_owned();
        // The outermost SQ rendered an item object (the walk started).
        assert!(
            s.contains("\"0040A730\":{\"Value\":[{"),
            "outermost SQ must render: {s}"
        );
        // At least one inner level past the cap rendered empty.
        assert!(
            s.contains("\"Value\":[],\"vr\":\"SQ\""),
            "a level past the depth cap must render empty: {s}"
        );
    }

    #[test]
    fn deeply_nested_undefined_sequences_walk_in_bounded_depth() {
        // Mirrors the 20.09 field shape that overflowed the 512 KiB worker
        // stack: a top-level reference sequence (undefined length) whose
        // items are undefined-length and each carry several nested
        // undefined-length sequences (a per-frame functional group). The old
        // walk recursed once per nested sequence AND re-walked the whole
        // parent remainder from each one - about 2.6k live frames on a
        // 432-frame object - so the process aborted with STATUS_STACK_OVERFLOW.
        // The fixed walk's depth tracks structural nesting only.
        let mut data = DicomBuilder::new().meta_header("1.2.840.10008.1.2.1");
        data = data.add_element(0x0010, 0x0010, b"PN", b"SEG^CRASH");
        data = data.add_element(0x0010, 0x0020, b"LO", b"PID-1");
        data = data.add_element(0x0020, 0x000D, b"UI", b"1.2.3");
        data = data.add_element(0x0020, 0x000E, b"UI", b"1.2.3.4");
        data = data.add_element(0x0008, 0x0018, b"UI", b"1.2.3.4.5");
        data = data.add_element(0x0008, 0x0060, b"CS", b"SEG");
        data = data.add_element(0x0008, 0x0020, b"DA", b"20200101");
        data = data.add_element(0x0010, 0x0030, b"DA", b"19700101");
        data = data.add_element(0x0010, 0x0040, b"CS", b"F");
        data = data.add_element(0x0020, 0x0011, b"IS", b"1");

        const ITEMS: usize = 432;
        const NESTED_SQS: usize = 6;

        sq_undef(&mut data.buf, 0x0008, 0x1115); // ReferencedSeriesSequence
        for i in 0..ITEMS {
            item_undef(&mut data.buf);
            // Referenced SOP Instance UID - the capture target.
            let uid = format!("1.2.3.4.5.{i}");
            data.add_explicit_element(0x0008, 0x1155, b"UI", uid.as_bytes());
            // The per-frame nested sequences that drove the old walk deep.
            for j in 0..NESTED_SQS {
                sq_undef(&mut data.buf, 0x0040, 0xA730 + j as u16);
                item_undef(&mut data.buf);
                let inner = format!("2.3.4.{i}.{j}");
                data.add_explicit_element(0x0008, 0x1150, b"UI", inner.as_bytes());
                item_delim(&mut data.buf);
                seq_delim(&mut data.buf);
            }
            item_delim(&mut data.buf);
        }
        seq_delim(&mut data.buf);

        let meta = DicomRsParser
            .extract_metadata(&data.buf)
            .expect("the synthetic SEG-shaped file parses");
        assert_eq!(meta.referenced_sop_instance_uids.len(), ITEMS);
        assert_eq!(
            meta.referenced_sop_instance_uids
                .first()
                .map(String::as_str),
            Some("1.2.3.4.5.0")
        );
        assert_eq!(
            meta.referenced_sop_instance_uids.last().map(String::as_str),
            Some(format!("1.2.3.4.5.{}", ITEMS - 1).as_str())
        );
    }

    // -- Valid DICOM tests ----------------------------------------------

    #[test]
    fn test_cr_modality() {
        let data = make_minimal_dicom("DOE^JOHN", "CR");
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.modality, "CR");
    }

    #[test]
    fn test_required_tags_present() {
        let data = make_minimal_dicom("DOE^JOHN", "CT");
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert!(!meta.patient_name.is_empty());
        assert!(!meta.patient_id.is_empty());
        assert!(!meta.study_instance_uid.is_empty());
        assert!(!meta.series_instance_uid.is_empty());
        assert!(!meta.sop_instance_uid.is_empty());
        assert!(!meta.modality.is_empty());
    }

    #[test]
    fn test_all_modalities_parse() {
        for m in &["CR", "DX", "MG", "CT", "MR", "US", "NM"] {
            let data = make_minimal_dicom("TEST^USER", m);
            let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
            assert_eq!(meta.modality, *m, "modality mismatch for {m}");
        }
    }

    #[test]
    fn test_uids_extracted() {
        let data = make_minimal_dicom("DOE^JOHN", "CR");
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.study_instance_uid, "1.2.840.113619.2.55.3.1.2.1");
        assert_eq!(meta.series_instance_uid, "1.2.840.113619.2.55.3.1.2.1.1");
        assert_eq!(meta.sop_instance_uid, "1.2.840.113619.2.55.3.1.2.1.1.1");
    }

    /// PS3.5 7.5: undefined-length sequences are legal, nested included.
    /// Each (FFFE,E0DD) closes exactly one level; the walker must not stop
    /// at the first one - that was the bug that made a real GSPS PR with
    /// nested undefined sequences read as `Unexpected EOF while skipping
    /// attribute value` and the server answer STOW with 400.
    #[test]
    fn nested_undefined_length_sequences_walk_to_the_trailing_element() {
        let mut data = make_minimal_dicom("DOE^JOHN", "PR");
        let sq = |data: &mut Vec<u8>, g: u16, e: u16| {
            data.extend_from_slice(&g.to_le_bytes());
            data.extend_from_slice(&e.to_le_bytes());
            data.extend_from_slice(b"SQ");
            data.extend_from_slice(&[0u8, 0u8]);
            data.extend_from_slice(&tags::UNDEFINED_LENGTH.to_le_bytes());
        };
        let item = |data: &mut Vec<u8>| {
            data.extend_from_slice(&0xFFFEu16.to_le_bytes());
            data.extend_from_slice(&0xE000u16.to_le_bytes());
            data.extend_from_slice(&tags::UNDEFINED_LENGTH.to_le_bytes());
        };
        let item_end = |data: &mut Vec<u8>| {
            data.extend_from_slice(&0xFFFEu16.to_le_bytes());
            data.extend_from_slice(&0xE00Du16.to_le_bytes());
            data.extend_from_slice(&0u32.to_le_bytes());
        };
        let sq_end = |data: &mut Vec<u8>| {
            data.extend_from_slice(&0xFFFEu16.to_le_bytes());
            data.extend_from_slice(&0xE0DDu16.to_le_bytes());
            data.extend_from_slice(&0u32.to_le_bytes());
        };
        // (0008,1115) SQ undefined -> item undefined -> (0008,1140) SQ
        // undefined -> item undefined -> (0008,1155) UI, then four delimiters.
        sq(&mut data, 0x0008, 0x1115);
        item(&mut data);
        sq(&mut data, 0x0008, 0x1140);
        item(&mut data);
        let value = b"1.2.3";
        data.extend_from_slice(&0x0008u16.to_le_bytes());
        data.extend_from_slice(&0x1155u16.to_le_bytes());
        data.extend_from_slice(b"UI");
        data.extend_from_slice(&(value.len() as u16).to_le_bytes());
        data.extend_from_slice(value);
        data.extend_from_slice(&[0u8]); // PS3.5 7.1 pad after odd-length value
        item_end(&mut data);
        sq_end(&mut data);
        item_end(&mut data);
        sq_end(&mut data);
        // A trailing element the walk must reach exactly.
        data.extend_from_slice(&0x0008u16.to_le_bytes());
        data.extend_from_slice(&0x1030u16.to_le_bytes());
        data.extend_from_slice(b"LO");
        data.extend_from_slice(&8u16.to_le_bytes());
        data.extend_from_slice(b"TRAILING");

        let meta = DicomRsParser
            .extract_metadata(&data)
            .expect("nested undefined SQ parses");
        assert_eq!(
            meta.study_description.as_deref(),
            Some("TRAILING"),
            "the walk must resume after the OUTER delimiter"
        );
    }

    #[test]
    fn test_optional_tags_extracted() {
        let data = make_minimal_dicom("DOE^JOHN", "CT");
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.study_date.as_deref(), Some("20200101"));
        assert_eq!(meta.patient_birth_date.as_deref(), Some("19700101"));
        assert_eq!(meta.patient_sex.as_deref(), Some("M"));
        assert_eq!(meta.series_number, Some(1));
    }

    /// 08.09 Stage B: the seven newly captured attributes are extracted from
    /// a fixture carrying all of them (parser side of the capture chain).
    #[test]
    fn test_stage_b_capture_tags_extracted() {
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"DOE^JOHN")
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001")
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.113619.2.55.3.1.2.2")
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.113619.2.55.3.1.2.2.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.113619.2.55.3.1.2.2.1.1")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .add_element(0x0008, 0x0070, b"LO", b"ACME SCANNERS") // Manufacturer
            .add_element(0x0008, 0x0080, b"LO", b"CITY HOSPITAL") // InstitutionName
            .add_element(0x0008, 0x1040, b"LO", b"RADIOLOGY") // InstitutionalDepartmentName
            .add_element(0x0008, 0x1070, b"PN", b"OPS^ANNA") // OperatorsName
            .add_element(0x0018, 0x1030, b"LO", b"CHEST ROUTINE") // ProtocolName
            .add_element(0x0032, 0x1032, b"PN", b"REQ^PAUL") // RequestingPhysician
            .add_element(0x0032, 0x1060, b"LO", b"CT CHEST WO") // RequestedProcedureDescription
            .build();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.manufacturer.as_deref(), Some("ACME SCANNERS"));
        assert_eq!(meta.institution_name.as_deref(), Some("CITY HOSPITAL"));
        assert_eq!(
            meta.institutional_department_name.as_deref(),
            Some("RADIOLOGY")
        );
        assert_eq!(meta.operators_name.as_deref(), Some("OPS^ANNA"));
        assert_eq!(meta.protocol_name.as_deref(), Some("CHEST ROUTINE"));
        assert_eq!(meta.requesting_physician.as_deref(), Some("REQ^PAUL"));
        assert_eq!(
            meta.requested_procedure_description.as_deref(),
            Some("CT CHEST WO")
        );
    }

    /// StudyID (0020,0010) is captured from DICOM into
    /// `InstanceMeta.study_id` when present, and stays absent otherwise.
    #[test]
    fn test_study_id_extracted() {
        // With StudyID present.
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"DOE^JOHN")
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001")
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.113619.2.55.3.1.2.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1.1")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .add_element(0x0020, 0x0010, b"SH", b"S100")
            .build();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.study_id.as_deref(), Some("S100"));

        // Without StudyID -> None.
        let data2 = make_minimal_dicom("DOE^JOHN", "CT");
        let meta2 = DicomRsParser.extract_metadata(&data2).expect("valid DICOM");
        assert_eq!(meta2.study_id, None);
    }

    /// FR-vet stage 1: ResponsiblePerson (0010,2297) - a PN tag - is captured
    /// into `InstanceMeta.responsible_person` when present, parsed like
    /// PatientName (trailing empty components trimmed), and stays absent when
    /// the element is not carried.
    #[test]
    fn test_responsible_person_extracted() {
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"SMITH^JOHN") // PatientName
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001")
            .add_element(0x0010, 0x2297, b"PN", b"IVANOV^IVAN^") // ResponsiblePerson
            .add_element(0x0020, 0x000D, b"UI", b"1.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2")
            .add_element(0x0008, 0x0018, b"UI", b"1.3")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .build();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        // Trailing empty component trimmed (to_dicom_string drops trailing ^).
        assert_eq!(meta.responsible_person.as_deref(), Some("IVANOV^IVAN"));

        // Absent element -> None.
        let data2 = make_minimal_dicom("DOE^JOHN", "CT");
        let meta2 = DicomRsParser.extract_metadata(&data2).expect("valid DICOM");
        assert_eq!(meta2.responsible_person, None);
    }

    /// Build a DICOM whose dataset carries the two nested sequences the QIDO
    /// series/study matching keys reach into: RequestAttributesSequence
    /// (0040,0275) with one item holding (0040,0009) and (0040,1001), and
    /// OtherPatientIDsSequence (0010,1002) with two items holding (0010,0020).
    fn nested_key_dicom() -> Vec<u8> {
        let mut ras_item = Vec::new();
        explicit_element(&mut ras_item, 0x0040, 0x0009, b"SH", b"SPS-7");
        explicit_element(&mut ras_item, 0x0040, 0x1001, b"SH", b"RP-7");
        let mut ras_seq = Vec::new();
        item_defined(&mut ras_seq, &ras_item);

        let mut id_item_a = Vec::new();
        explicit_element(&mut id_item_a, 0x0010, 0x0020, b"LO", b"OTHER-A");
        let mut id_item_b = Vec::new();
        explicit_element(&mut id_item_b, 0x0010, 0x0020, b"LO", b"OTHER-B");
        let mut ids_seq = Vec::new();
        item_defined(&mut ids_seq, &id_item_a);
        item_defined(&mut ids_seq, &id_item_b);

        DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"SMITH^JOHN")
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001")
            .add_element(0x0010, 0x1002, b"SQ", &ids_seq)
            .add_element(0x0020, 0x000D, b"UI", b"1.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2")
            .add_element(0x0008, 0x0018, b"UI", b"1.3")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .add_element(0x0040, 0x0275, b"SQ", &ras_seq)
            .build()
    }

    /// PS3.18 Table 10.6.1-5 makes `00400275.00400009` and
    /// `00400275.00401001` REQUIRED series matching attributes, so both nested
    /// scalars are captured from the sequence's first item - a key a search
    /// accepts has to match a value the store holds.
    #[test]
    fn test_request_attributes_sequence_nested_scalars_extracted() {
        let data = nested_key_dicom();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.scheduled_procedure_step_id.as_deref(), Some("SPS-7"));
        assert_eq!(meta.requested_procedure_id.as_deref(), Some("RP-7"));

        // An instance without the sequence states neither value.
        let plain = make_minimal_dicom("DOE^JOHN", "CT");
        let plain_meta = DicomRsParser.extract_metadata(&plain).expect("valid DICOM");
        assert_eq!(plain_meta.scheduled_procedure_step_id, None);
        assert_eq!(plain_meta.requested_procedure_id, None);
    }

    /// `00101002.00100020` is MULTI-VALUED: every item of OtherPatientIDsSequence
    /// contributes, so all of them are collected in file order.
    #[test]
    fn test_other_patient_ids_sequence_collects_every_item() {
        let data = nested_key_dicom();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.other_patient_ids, vec!["OTHER-A", "OTHER-B"]);

        let plain = make_minimal_dicom("DOE^JOHN", "CT");
        let plain_meta = DicomRsParser.extract_metadata(&plain).expect("valid DICOM");
        assert!(plain_meta.other_patient_ids.is_empty());
    }

    #[test]
    fn test_performing_and_reading_physician_extracted() {
        // synthetic DICOM carrying both new physician tags.
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"SMITH")
            .add_element(0x0010, 0x0020, b"LO", b"ID-1")
            .add_element(0x0020, 0x000D, b"UI", b"1.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2")
            .add_element(0x0008, 0x0018, b"UI", b"1.3")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .add_element(0x0008, 0x1050, b"PN", b"PERF^DOC")
            .add_element(0x0008, 0x1060, b"PN", b"READ^DOC")
            .build();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.performing_physician.as_deref(), Some("PERF^DOC"));
        assert_eq!(meta.name_of_reading_physicians.as_deref(), Some("READ^DOC"));
    }

    /// extract_metadata captures PerformedProcedureStepStartDate
    /// (0040,0244, DA) and PerformedProcedureStepStartTime (0040,0245, TM)
    /// into InstanceMeta, mirroring SERIES_DATE/SERIES_TIME.
    #[test]
    fn test_pps_start_extracted() {
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"SMITH")
            .add_element(0x0010, 0x0020, b"LO", b"ID-1")
            .add_element(0x0020, 0x000D, b"UI", b"1.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2")
            .add_element(0x0008, 0x0018, b"UI", b"1.3")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .add_element(0x0040, 0x0244, b"DA", b"20260802")
            .add_element(0x0040, 0x0245, b"TM", b"104500.250")
            .build();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.pps_start_date.as_deref(), Some("20260802"));
        assert_eq!(meta.pps_start_time.as_deref(), Some("104500.250"));

        // Absent tags stay None.
        let data2 = make_minimal_dicom("DOE^JOHN", "CT");
        let meta2 = DicomRsParser.extract_metadata(&data2).expect("valid DICOM");
        assert_eq!(meta2.pps_start_date, None);
        assert_eq!(meta2.pps_start_time, None);
    }

    #[test]
    fn test_dicom_no_preamble() {
        // DICM at offset 0 (no 128-byte preamble)
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"SMITH")
            .add_element(0x0010, 0x0020, b"LO", b"ID-1")
            .add_element(0x0020, 0x000D, b"UI", b"1.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2")
            .add_element(0x0008, 0x0018, b"UI", b"1.3")
            .add_element(0x0008, 0x0060, b"CS", b"US")
            .build();
        // Remove preamble - DICM now at offset 0
        let no_preamble = &data[128..]; // skip 128 zero bytes
        let meta = DicomRsParser
            .extract_metadata(no_preamble)
            .expect("valid DICOM");
        assert_eq!(meta.modality, "US");
    }

    #[test]
    fn test_implicit_vr() {
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2") // Implicit VR LE
            // For implicit VR, we need implicit-format elements (no VR in header)
            .build();
        // The implicit VR file has just metadata header, no dataset elements
        // Should succeed (empty required tags -> MissingRequiredTag error)
        let result = DicomRsParser.extract_metadata(&data);
        assert!(result.is_err());
    }

    // -- Implicit VR LE extract_tag_value -------------------------------

    #[test]
    fn test_extract_tag_value_implicit_vr() {
        // Build an Implicit VR LE DICOM with known tag values
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2") // Implicit VR LE
            .add_implicit_element(0x0028, 0x0010, &512u16.to_le_bytes()) // Rows = 512
            .add_implicit_element(0x0028, 0x0011, &512u16.to_le_bytes()) // Columns = 512
            .add_implicit_element(0x0028, 0x0002, &1u16.to_le_bytes()) // SamplesPerPixel = 1
            .add_implicit_element(0x0028, 0x0100, &16u16.to_le_bytes()) // BitsAllocated = 16
            .add_implicit_element(0x0028, 0x0101, &12u16.to_le_bytes()) // BitsStored = 12
            .add_implicit_element(0x0028, 0x0004, b"MONOCHROME2") // PhotometricInterpretation
            .build();

        // Extract BitsAllocated (0028,0100)
        let val = DicomRsParser
            .extract_tag_value(&data, (0x0028, 0x0100))
            .expect("extract_tag_value should succeed")
            .expect("BitsAllocated should be found");
        let bits_allocated = u16::from_le_bytes([val[0], val[1]]);
        assert_eq!(
            bits_allocated, 16,
            "BitsAllocated should be 16, got {bits_allocated}"
        );

        // Extract Rows (0028,0010)
        let val = DicomRsParser
            .extract_tag_value(&data, (0x0028, 0x0010))
            .expect("extract_tag_value should succeed")
            .expect("Rows should be found");
        let rows = u16::from_le_bytes([val[0], val[1]]);
        assert_eq!(rows, 512, "Rows should be 512");

        // Extract PhotometricInterpretation (0028,0004)
        let val = DicomRsParser
            .extract_tag_value(&data, (0x0028, 0x0004))
            .expect("extract_tag_value should succeed")
            .expect("PhotometricInterpretation should be found");
        let pi = String::from_utf8_lossy(&val)
            .trim_end_matches([' ', '\0'])
            .to_string();
        assert_eq!(pi, "MONOCHROME2");
    }

    // -- Missing required tags -> error ----------------------------------

    #[test]
    fn test_missing_modality() {
        // Build a DICOM without the Modality tag
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"SMITH")
            .add_element(0x0010, 0x0020, b"LO", b"ID-1")
            .add_element(0x0020, 0x000D, b"UI", b"1.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2")
            .add_element(0x0008, 0x0018, b"UI", b"1.3")
            // NOTE: no Modality (0008,0060)
            .build();
        let result = DicomRsParser.extract_metadata(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_patient_name() {
        // PatientName is DICOM Type 2 - may be empty (anonymized studies).
        // The parser should accept it and return an empty string.
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0020, b"LO", b"ID-1")
            .add_element(0x0020, 0x000D, b"UI", b"1.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2")
            .add_element(0x0008, 0x0018, b"UI", b"1.3")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .build();
        let result = DicomRsParser.extract_metadata(&data);
        assert!(
            result.is_ok(),
            "PatientName Type 2: empty should be accepted"
        );
        let meta = result.unwrap();
        assert!(meta.patient_name.is_empty());
    }

    // -- Image geometry: Rows/Columns via US (binary u16) --------------

    #[test]
    fn extract_metadata_parses_rows_columns_from_us_explicit_vr() {
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1") // Explicit VR LE
            .add_element(0x0010, 0x0010, b"PN", b"DOE^JOHN")
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001")
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.113619.2.55.3.1.2.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1.1")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .add_element(0x0028, 0x0010, b"US", &776u16.to_le_bytes()) // Rows
            .add_element(0x0028, 0x0011, b"US", &776u16.to_le_bytes()) // Columns
            .build();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.rows, Some(776), "Rows should be 776 (US binary)");
        assert_eq!(meta.cols, Some(776), "Columns should be 776 (US binary)");
    }

    #[test]
    fn extract_metadata_parses_rows_columns_from_us_implicit_vr() {
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2") // Implicit VR LE
            .add_implicit_element(0x0010, 0x0010, b"DOE^JOHN")
            .add_implicit_element(0x0010, 0x0020, b"PAT-001")
            .add_implicit_element(0x0020, 0x000D, b"1.2.840.113619.2.55.3.1.2.1")
            .add_implicit_element(0x0020, 0x000E, b"1.2.840.113619.2.55.3.1.2.1.1")
            .add_implicit_element(0x0008, 0x0018, b"1.2.840.113619.2.55.3.1.2.1.1.1")
            .add_implicit_element(0x0008, 0x0060, b"CT")
            .add_implicit_element(0x0028, 0x0010, &512u16.to_le_bytes()) // Rows
            .add_implicit_element(0x0028, 0x0011, &512u16.to_le_bytes()) // Columns
            .build();
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.rows, Some(512), "Rows should be 512 (Implicit VR US)");
        assert_eq!(
            meta.cols,
            Some(512),
            "Columns should be 512 (Implicit VR US)"
        );
    }

    // -- Corrupt / non-DICOM -> error, no panic -------------------------

    #[test]
    fn test_corrupt_returns_error() {
        let mut data = vec![0u8; 132];
        data[128..132].copy_from_slice(b"DICM");
        assert!(DicomRsParser.extract_metadata(&data).is_err());
    }

    #[test]
    fn test_random_bytes_error() {
        assert!(DicomRsParser.extract_metadata(b"not a dicom file").is_err());
    }

    #[test]
    fn test_empty_error() {
        assert!(DicomRsParser.extract_metadata(&[]).is_err());
    }

    #[test]
    fn test_short_error() {
        assert!(DicomRsParser.extract_metadata(&[0u8; 10]).is_err());
    }

    #[test]
    fn test_pn_no_trailing_hats_in_parse() {
        let data = make_minimal_dicom("SMITH^JOHN^^^", "CT");
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert_eq!(meta.patient_name, "SMITH^JOHN");
        assert!(!meta.patient_name.ends_with('^'));
    }

    // -- PnComponents (DEV-013) -----------------------------------------

    #[test]
    fn test_pn_components_full() {
        let pn = PnComponents {
            family: "Смирнов".into(),
            given: "Иван".into(),
            middle: "Петрович".into(),
            prefix: "д-р".into(),
            suffix: "мл.".into(),
        };
        assert_eq!(pn.to_dicom_string(), "Смирнов^Иван^Петрович^д-р^мл.");
    }

    #[test]
    fn test_pn_components_trailing_empty() {
        let pn = PnComponents {
            family: "SMITH".into(),
            given: "JOHN".into(),
            ..Default::default()
        };
        assert_eq!(pn.to_dicom_string(), "SMITH^JOHN");
    }

    #[test]
    fn test_pn_components_family_only() {
        let pn = PnComponents {
            family: "SMITH".into(),
            ..Default::default()
        };
        assert_eq!(pn.to_dicom_string(), "SMITH");
        assert!(!pn.to_dicom_string().ends_with('^'));
    }

    #[test]
    fn test_pn_components_smith_john() {
        let pn = parse_pn_components(b"SMITH^JOHN^^^", None).unwrap();
        assert_eq!(pn.family, "SMITH");
        assert_eq!(pn.given, "JOHN");
        assert_eq!(pn.middle, "");
        assert_eq!(pn.prefix, "");
        assert_eq!(pn.suffix, "");
        assert_eq!(pn.to_dicom_string(), "SMITH^JOHN");
    }

    #[test]
    fn test_pn_five_components() {
        let pn = parse_pn_components(b"DOE^JANE^MARIE^DR^JR", None).unwrap();
        assert_eq!(pn.family, "DOE");
        assert_eq!(pn.given, "JANE");
        assert_eq!(pn.middle, "MARIE");
        assert_eq!(pn.prefix, "DR");
        assert_eq!(pn.suffix, "JR");
        assert_eq!(pn.to_dicom_string(), "DOE^JANE^MARIE^DR^JR");
    }

    // -- Search tags ----------------------------------------------------
    #[test]
    fn test_search_tags() {
        let data = make_minimal_dicom("DOE^JANE", "CR");
        let tags = DicomRsParser.extract_search_tags(&data).unwrap();
        assert!(!tags.is_empty());
        assert!(tags.contains_key(&(0x0010, 0x0010))); // PatientName
        assert_eq!(tags.get(&(0x0008, 0x0060)).map(|s| s.as_str()), Some("CR"));
        assert_eq!(
            tags.get(&(0x0010, 0x0020)).map(|s| s.as_str()),
            Some("PAT-001")
        );
    }

    // -- Pixel data invariant -------------------------------------------

    #[test]
    fn test_pixel_data_skipped() {
        let data = make_minimal_dicom("SMITH", "CT");
        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        assert!(!meta.sop_instance_uid.is_empty());
    }

    #[test]
    fn test_pixel_data_stops_parser() {
        // Build DICOM with PixelData but NO value tags after it
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"TEST")
            .add_element(0x0010, 0x0020, b"LO", b"ID")
            .add_element(0x0020, 0x000D, b"UI", b"1.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2")
            .add_element(0x0008, 0x0018, b"UI", b"1.3")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            // PixelData - parser should stop here
            .add_element(0x7FE0, 0x0010, b"OB", b"fake_pixel_data")
            // This tag is AFTER PixelData - should NOT be parsed
            .add_element(0x0008, 0x1030, b"LO", b"SHOULD_NOT_APPEAR")
            .build();

        let meta = DicomRsParser.extract_metadata(&data).expect("valid DICOM");
        // StudyDescription should be None because parser stopped at PixelData
        assert!(
            meta.study_description.is_none(),
            "StudyDescription should be None (tag after PixelData was skipped)"
        );
    }

    // -- trim_trailing --------------------------------------------------

    #[test]
    fn test_trim_spaces() {
        assert_eq!(trim_trailing(b"HELLO   "), b"HELLO");
    }

    #[test]
    fn test_trim_nulls() {
        assert_eq!(trim_trailing(b"VALUE\0\0"), b"VALUE");
    }

    #[test]
    fn test_trim_mixed() {
        assert_eq!(trim_trailing(b"DATA \0\r\n"), b"DATA");
    }

    #[test]
    fn test_trim_all() {
        assert!(trim_trailing(b"   \0").is_empty());
    }

    // -- Cyrillic (ISO_IR 144) support ----------------------------------

    #[test]
    fn test_cyrillic_patient_name_iso_ir_144() {
        // "Басилмо" in ISO 8859-5 bytes
        let cyrillic_bytes = [0xB1, 0xD0, 0xE1, 0xD8, 0xDB, 0xDC, 0xDE];
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0008, 0x0005, b"CS", b"ISO_IR 144") // первым в dataset
            .add_element(0x0010, 0x0010, b"PN", &cyrillic_bytes)
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001")
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.113619.2.55.3.1.2.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1.1")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .build();
        let meta = DicomRsParser
            .extract_metadata(&data)
            .expect("valid Cyrillic DICOM");
        assert_eq!(meta.patient_name, "Басилмо");
    }

    #[test]
    fn test_latin1_default_when_no_charset() {
        // Without SpecificCharacterSet, bytes should be treated as Latin-1.
        // 0xE9 = é in Latin-1, 0xF4 = ô
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            // NO SpecificCharacterSet -> defaults to ISO_IR 100 (Latin-1)
            .add_element(
                0x0010,
                0x0010,
                b"PN",
                &[
                    0x44, 0x75, 0x70, 0x6F, 0x6E, 0x74, 0x5E, 0x4A, 0xE9, 0x72, 0xF4, 0x6D, 0x65,
                ],
            )
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001")
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.113619.2.55.3.1.2.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1.1")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .build();
        let meta = DicomRsParser
            .extract_metadata(&data)
            .expect("valid Latin-1 DICOM");
        // Dupont^Jérôme - é = U+00E9, ô = U+00F4
        assert_eq!(meta.patient_name, "Dupont^Jérôme");
    }

    #[test]
    fn test_cyrillic_study_description() {
        let cyrillic_bytes = [0xB3, 0xC0, 0xC3, 0xB4, 0xBD, 0xB0, 0xCF]; // "ГРУДНАЯ" (chest)
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0008, 0x0005, b"CS", b"ISO_IR 144")
            .add_element(0x0010, 0x0010, b"PN", b"TEST^PATIENT")
            .add_element(0x0010, 0x0020, b"LO", b"PAT-001")
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.113619.2.55.3.1.2.1")
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.113619.2.55.3.1.2.1.1.1")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .add_element(0x0008, 0x1030, b"LO", &cyrillic_bytes) // StudyDescription
            .build();
        let meta = DicomRsParser
            .extract_metadata(&data)
            .expect("valid Cyrillic DICOM");
        assert_eq!(meta.study_description.as_deref(), Some("ГРУДНАЯ"));
    }

    // -- extract_full_metadata tests (FAIL-002) ------------------------

    #[test]
    fn test_extract_full_metadata_basic() {
        // DICOM with PatientName + Modality -> JSON with keys "00100010", "00080060".
        // Assert on the deterministic native renderer bytes (serde-free).
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"SMITH^JOHN")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .build();
        let json_bytes = DicomRsParser
            .extract_full_metadata_bytes(&data)
            .expect("valid DICOM");
        let s = String::from_utf8_lossy(&json_bytes);
        assert!(
            s.contains("\"00100010\""),
            "Should contain PatientName: {s}"
        );
        assert!(s.contains("\"00080060\""), "Should contain Modality: {s}");
        // Verify structure: {"Alphabetic":"SMITH^JOHN"} with "vr":"PN".
        assert!(s.contains("\"Alphabetic\":\"SMITH^JOHN\""), "PN value: {s}");
        assert!(s.contains("\"vr\":\"PN\""), "PN vr: {s}");
    }

    // -- native (serde-free) renderer byte-parity ----------

    /// Workspace-relative directory holding the committed real DICOM corpus
    /// (`tests/**/*.dcm`, 104 files at capture time).
    const REAL_CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests");

    /// Directory holding the frozen serde-era metadata goldens for the real
    /// corpus (Commit A). Filename of each = the DICOM's relative path with
    /// `.dcm` -> `.json`.
    const GOLDENS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/goldens");

    /// Assert two byte slices are identical (helper for readable failures).
    fn assert_byte_eq(a: &[u8], b: &[u8], ctx: &str) {
        assert_eq!(
            String::from_utf8_lossy(a),
            String::from_utf8_lossy(b),
            "{ctx}"
        );
    }

    /// The native renderer output must be byte-identical to the frozen
    /// serde-era goldens. Over a deliberately synthetic corpus covering the
    /// VR / charset / padding variants the WADO `/metadata` path must keep
    /// stable (capture 19.08): negative SS/SL, FL/FD (integral `.0`, `1e30`,
    /// subnormal), ISO_IR 100 + ISO_IR 144 charsets, padded string VR and
    /// non-finite FL/FD. Each item's expected bytes are embedded via
    /// `include_bytes!` (Commit A).

    #[test]
    fn metadata_bytes_native_matches_synthetic_corpus() {
        // FL/FD values exercising scientific, integral `.0`, subnormal, negative.
        let fd_sci = 1e30f64;
        let fd_sub = 5e-324f64;
        let fl_base = 1.5f32;
        let fl_big = 1e10f32;

        let corpus: Vec<Vec<u8>> = vec![
            // Standard instance (PatientName/Modality/Study/Series/SOP + pixel).
            DicomBuilder::new()
                .meta_header("1.2.840.10008.1.2.1")
                .add_element(0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2")
                .add_element(0x0008, 0x0018, b"UI", b"1.2.826.0.1.3680043.10.1234.6201")
                .add_element(0x0010, 0x0010, b"PN", b"WADO^SMOKE")
                .add_element(0x0010, 0x0020, b"LO", b"WADOPAT01")
                .add_element(0x0020, 0x000D, b"UI", b"1.2.826.0.1.3680043.10.1234.6001")
                .add_element(0x0020, 0x000E, b"UI", b"1.2.826.0.1.3680043.10.1234.6101")
                .add_element(0x0008, 0x0060, b"CS", b"CT")
                .add_element(0x0028, 0x0002, b"US", &1u16.to_le_bytes())
                .add_element(0x0028, 0x0004, b"CS", b"MONOCHROME2")
                .add_element(0x0028, 0x0010, b"US", &8u16.to_le_bytes())
                .add_element(0x0028, 0x0011, b"US", &8u16.to_le_bytes())
                .add_element(0x7FE0, 0x0010, b"OB", &[0x5a_u8; 64])
                .build(),
            // Cyrillic PN + multi-valued DS + AT + binary (UN).
            DicomBuilder::new()
                .meta_header("1.2.840.10008.1.2.1")
                .add_element(
                    0x0010,
                    0x0010,
                    b"PN",
                    b"\xd0\x98\xd0\x92\xd0\x90\xd0\x9d\xd0\x9e\xd0\x92^^\xcc\x82",
                )
                .add_element(0x0028, 0x0030, b"DS", b"0.5\\0.5\\0.25")
                .add_element(0x0008, 0x0008, b"AT", &[0x08u8, 0x00, 0x60, 0x00])
                .add_element(0x0040, 0xA010, b"UN", &[0xDEu8, 0xAD, 0xBE, 0xEF])
                .add_element(0x7FE0, 0x0010, b"OB", &[0x00u8; 16])
                .build(),
            // Negative SS/SL numerics + unsigned + empty PN.
            DicomBuilder::new()
                .meta_header("1.2.840.10008.1.2.1")
                .add_element(0x0010, 0x0010, b"PN", b"") // empty PN
                .add_element(0x0028, 0x0106, b"SS", &(-100i16).to_le_bytes())
                .add_element(0x0028, 0x0107, b"SL", &(-2_147_483_648i32).to_le_bytes())
                .add_element(0x0028, 0x0108, b"SS", &(-1i16).to_le_bytes())
                .add_element(0x7FE0, 0x0010, b"OB", &[0xFFu8; 32])
                .build(),
            // FL/FD: integral `.0`, scientific, subnormal, negative, multi-value.
            DicomBuilder::new()
                .meta_header("1.2.840.10008.1.2.1")
                .add_element(0x0028, 0x0030, b"FD", &2.0f64.to_le_bytes())
                .add_element(0x0028, 0x0031, b"FD", &fd_sci.to_le_bytes())
                .add_element(0x0028, 0x0032, b"FD", &fd_sub.to_le_bytes())
                .add_element(0x0028, 0x0033, b"FL", &fl_base.to_le_bytes())
                .add_element(0x0028, 0x0034, b"FL", &fl_big.to_le_bytes())
                .add_element(0x0028, 0x0035, b"FD", &(-0.5f64).to_le_bytes())
                .add_element(0x7000, 0x0010, b"OW", &[0x11u8, 0x22, 0x33, 0x44])
                .add_element(0x7FE0, 0x0010, b"OB", &[0x00u8; 16])
                .build(),
            // SpecificCharacterSet ISO_IR 100 (Latin-1) for Cyrillic PN.
            DicomBuilder::new()
                .meta_header("1.2.840.10008.1.2.1")
                .add_element(0x0008, 0x0005, b"CS", b"ISO_IR 100")
                .add_element(0x0010, 0x0010, b"PN", b"\xc8\xc2\xc0\xcd\xce\xc2")
                .add_element(0x7FE0, 0x0010, b"OB", &[0x00u8; 8])
                .build(),
            // SpecificCharacterSet ISO_IR 144 (Cyrillic) PN + padded string VR.
            DicomBuilder::new()
                .meta_header("1.2.840.10008.1.2.1")
                .add_element(0x0008, 0x0005, b"CS", b"ISO_IR 144")
                .add_element(0x0010, 0x0010, b"PN", b"\x18\x18\x20\x41\x42\x20") // Cyrillic + trailing space pad
                .add_element(0x0008, 0x0060, b"CS", b"CT ") // space-padded string VR
                .add_element(0x0010, 0x1001, b"SH", b"Name ") // space-padded SH
                .add_element(0x7FE0, 0x0010, b"OB", &[0x00u8; 8])
                .build(),
            // Non-finite FL/FD: serde Number::from_f64 -> None -> value SKIPPED,
            // so each renders "Value":[] (empty). Native must mirror (skip) and
            // must NOT reach zmij::format_finite (panic in release).
            DicomBuilder::new()
                .meta_header("1.2.840.10008.1.2.1")
                .add_element(0x0028, 0x0030, b"FD", &f64::NAN.to_le_bytes())
                .add_element(0x0028, 0x0031, b"FD", &f64::INFINITY.to_le_bytes())
                .add_element(0x0028, 0x0032, b"FL", &f32::NEG_INFINITY.to_le_bytes())
                .add_element(0x7FE0, 0x0010, b"OB", &[0x00u8; 8])
                .build(),
        ];

        // Each corpus item's expected metadata is a frozen serde-era golden
        // (Commit A), embedded at compile time (inline fixture). The native
        // renderer must stay byte-identical to these; no live serde needed.
        let expected: [&[u8]; 7] = [
            include_bytes!("../../tests/goldens/synthetic/synth_0.json"),
            include_bytes!("../../tests/goldens/synthetic/synth_1.json"),
            include_bytes!("../../tests/goldens/synthetic/synth_2.json"),
            include_bytes!("../../tests/goldens/synthetic/synth_3.json"),
            include_bytes!("../../tests/goldens/synthetic/synth_4.json"),
            include_bytes!("../../tests/goldens/synthetic/synth_5.json"),
            include_bytes!("../../tests/goldens/synthetic/synth_6.json"),
        ];

        for (i, dcm) in corpus.iter().enumerate() {
            let native = DicomRsParser
                .extract_full_metadata_bytes(dcm)
                .expect("native render");
            // One-shot regeneration (binary VRs now render as `InlineBinary`):
            // the synthetic goldens embed these bytes, so refreshing them is a
            // deliberate act with this env set, like the real corpus.
            if std::env::var_os("CLARUS_FIXTURE_REGEN").is_some() {
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests")
                    .join("goldens")
                    .join("synthetic")
                    .join(format!("synth_{i}.json"));
                std::fs::write(&path, &native)
                    .unwrap_or_else(|e| panic!("regen write {}: {e}", path.display()));
                continue;
            }
            assert_byte_eq(&native, expected[i], &format!("synthetic[{i}]"));
        }
    }

    /// Every real committed DICOM in `tests/**/*.dcm` must render
    /// byte-identically to its committed golden. Real files carry FD spacing,
    /// SS/SL, charsets and multi-frame that synthetic fixtures cannot reliably
    /// reproduce - this is the true empirical gate.
    ///
    /// Goldens live under `crates/clarus-adapters/tests/goldens/`, one `.json`
    /// per DICOM (filename = the relative path with the `.dcm` extension
    /// replaced by `.json`, e.g. `dicoms/real/CT_pydicom.dcm.json`). They were
    /// captured from the serde-era renderer during the Commit-A freeze (native
    /// was asserted equal to serde at that time). A DICOM with no golden is one
    /// the serde-era renderer failed to parse - the native renderer must ERROR
    /// on it too. This test is serde-free: it only compares against goldens.
    fn real_golden_path(dcm_abs: &std::path::Path) -> std::path::PathBuf {
        let root = std::path::Path::new(REAL_CORPUS_DIR);
        let rel = dcm_abs.strip_prefix(root).unwrap_or(dcm_abs);
        let rel_json = rel.with_extension("json");
        std::path::Path::new(GOLDENS_DIR).join(rel_json)
    }

    #[test]
    fn metadata_bytes_native_matches_real_corpus() {
        let root = std::path::Path::new(REAL_CORPUS_DIR);
        assert!(
            root.is_dir(),
            "real corpus dir missing: {}",
            REAL_CORPUS_DIR
        );

        let mut dcm_files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().map(|e| e == "dcm").unwrap_or(false) {
                    dcm_files.push(path);
                }
            }
        }
        dcm_files.sort();
        assert!(
            !dcm_files.is_empty(),
            "no .dcm files found under {REAL_CORPUS_DIR}"
        );

        for path in &dcm_files {
            let data = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => panic!("corpus file unreadable: {}: {e}", path.display()),
            };
            let ctx = format!("real: {}", path.display());
            let golden_path = real_golden_path(path);
            if golden_path.is_file() {
                let native = DicomRsParser
                    .extract_full_metadata_bytes(&data)
                    .expect("native render (golden exists)");
                // One-shot regeneration: the recursive SQ renderer changes the
                // bytes for any object with a non-empty sequence, so the
                // serde-era goldens (which recorded every SQ as an empty
                // array) are refreshed deliberately with this env set. The
                // test itself never regenerates on a normal run.
                if std::env::var_os("CLARUS_FIXTURE_REGEN").is_some() {
                    std::fs::write(&golden_path, &native)
                        .unwrap_or_else(|e| panic!("regen write {}: {e}", golden_path.display()));
                    continue;
                }
                let golden = std::fs::read(&golden_path)
                    .unwrap_or_else(|e| panic!("read golden {}: {e}", golden_path.display()));
                assert_byte_eq(&native, &golden, &format!("{ctx} (vs golden)"));
            } else {
                // No golden: the serde-era freeze recorded that this file was
                // unparseable. The native renderer must error on it too.
                assert!(
                    DicomRsParser.extract_full_metadata_bytes(&data).is_err(),
                    "{ctx}: expected error (serde-era freeze: no golden)"
                );
            }
        }
    }

    #[test]
    fn test_full_metadata_pixel_data_excluded() {
        // Ensure 7FE00010 is absent from full metadata output (serde-free: the
        // native renderer stops at PixelData, so it never appears).
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"TEST")
            // PixelData (7FE0,0010) - parser must stop before it
            .add_element(0x7FE0, 0x0010, b"OB", &[0u8; 100])
            .build();
        let json_bytes = DicomRsParser
            .extract_full_metadata_bytes(&data)
            .expect("valid DICOM");
        let s = String::from_utf8_lossy(&json_bytes);
        assert!(
            !s.contains("7FE00010"),
            "PixelData must be excluded from full metadata: {s}"
        );
        // PatientName should still be present (before PixelData)
        assert!(s.contains("00100010"), "PatientName present: {s}");
    }

    #[test]
    fn test_convert_string_element_pn() {
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"SMITH^JOHN")
            .build();
        let json_bytes = DicomRsParser
            .extract_full_metadata_bytes(&data)
            .expect("valid DICOM");
        let s = String::from_utf8_lossy(&json_bytes);
        assert!(s.contains("\"vr\":\"PN\""), "PN vr: {s}");
        assert!(s.contains("\"Alphabetic\":\"SMITH^JOHN\""), "PN value: {s}");
    }

    #[test]
    fn test_convert_numeric_element_us() {
        // Rows (0028,0010) = 512 as US
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0028, 0x0010, b"US", &512u16.to_le_bytes())
            .build();
        let json_bytes = DicomRsParser
            .extract_full_metadata_bytes(&data)
            .expect("valid DICOM");
        let s = String::from_utf8_lossy(&json_bytes);
        assert!(
            s.contains("\"00280010\":{\"Value\":[512],\"vr\":\"US\"}"),
            "{s}"
        );
    }

    #[test]
    fn test_convert_numeric_element_multi_valued() {
        // DS with two values: PixelSpacing = "0.5\0.5"
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0028, 0x0030, b"DS", b"0.5\\0.5")
            .build();
        let json_bytes = DicomRsParser
            .extract_full_metadata_bytes(&data)
            .expect("valid DICOM");
        let s = String::from_utf8_lossy(&json_bytes);
        // DS values are stored as strings in DICOMweb JSON
        assert!(
            s.contains("\"00280030\":{\"Value\":[\"0.5\",\"0.5\"],\"vr\":\"DS\"}"),
            "{s}"
        );
    }

    #[test]
    fn test_convert_at_element() {
        // ImageType (0008,0008) AT with value pointing to (0008,0060)
        let data = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0008, 0x0008, b"AT", &[0x08u8, 0x00, 0x60, 0x00])
            .build();
        let json_bytes = DicomRsParser
            .extract_full_metadata_bytes(&data)
            .expect("valid DICOM");
        let s = String::from_utf8_lossy(&json_bytes);
        // AT: group,elem pair formatted as "XXXX,XXXX"
        assert!(
            s.contains("\"00080008\":{\"Value\":[\"0008,0060\"],\"vr\":\"AT\"}"),
            "{s}"
        );
    }

    #[test]
    fn test_lookup_vr_known_tag() {
        assert_eq!(lookup_vr(0x0010, 0x0010), "PN"); // PatientName
        assert_eq!(lookup_vr(0x0028, 0x0010), "US"); // Rows
        assert_eq!(lookup_vr(0x0028, 0x0011), "US"); // Columns
    }

    #[test]
    fn test_lookup_vr_unknown_tag() {
        assert_eq!(lookup_vr(0x9999, 0x9999), "UN"); // Unknown -> UN
    }

    /// PS3.6 6: an Implicit VR dataset carries no VR, so the dictionary is the
    /// only source. The hand-kept table answered a few dozen tags and left the
    /// rest `UN`, which lost their type, their JSON shape and their numeric
    /// conversion.
    #[test]
    fn lookup_vr_answers_from_the_standard_dictionary() {
        // (0008,1050) Performing Physician's Name is PN and is not in the
        // hand-kept list; (0020,0032) Image Position (Patient) is DS.
        assert_eq!(lookup_vr(0x0008, 0x1050), "PN");
        assert_eq!(lookup_vr(0x0020, 0x0032), "DS");
        // A group-length element is UL whatever the group (PS3.5 7.2).
        assert_eq!(lookup_vr(0x0028, 0x0000), "UL");
        // An unknown tag still has no invented VR.
        assert_eq!(lookup_vr(0xFFFF, 0xEEEE), "UN");
    }

    /// PS3.5 7.1: an Explicit VR Big Endian source stores every binary VR
    /// byte-reversed. A fixed `from_le_bytes` rendered the numbers swapped -
    /// present, but meaningless.
    #[test]
    fn numeric_values_follow_the_transfer_syntax_byte_order() {
        let values = [64u16, 512];
        let le: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let be: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes()).collect();

        let from_le = decode_numeric_values_raw(&le, "US", TransferSyntax::ExplicitVrLE);
        let from_be = decode_numeric_values_raw(&be, "US", TransferSyntax::ExplicitVrBE);
        assert_eq!(from_le.len(), 2);
        assert_eq!(from_be.len(), 2);

        fn render(n: &NativeJsonNum) -> String {
            let mut out = Vec::new();
            n.write_to(&mut out);
            String::from_utf8(out).expect("ascii digits")
        }
        // The decisive check: the same numbers read either way, so only the
        // syntax decides.
        for (a, b) in from_le.iter().zip(from_be.iter()) {
            assert_eq!(render(a), render(b));
        }
        assert_eq!(render(&from_be[0]), "64");
        // Reading the BE bytes as LE is the bug this pins down.
        let wrong = decode_numeric_values_raw(&be, "US", TransferSyntax::ExplicitVrLE);
        assert_eq!(render(&wrong[0]), "16384");
    }

    #[test]
    fn test_base64_encode_rfc4648_vectors() {
        // RFC 4648 Section 10 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    // -- 3008 hidden-PHI capture KATs -----------------------------------
    //
    // Reference generator (pydicom) for each fixture shape:
    //   ds = pydicom.Dataset(); ds.PatientName = ""; ds.PatientID = "";
    //   ds.StudyInstanceUID = ...; ds.SeriesInstanceUID = ...;
    //   ds.SOPInstanceUID = ...; ds.Modality = "CT"; ds.Rows = 1;
    //   ds.Columns = 1; ds.PixelData = b"\x00\x01"
    //   ds.OriginalAttributesSequence = [pydicom.Dataset()]  # (0400,0561)
    //   ds.OriginalAttributesSequence[0].PatientName = "HIDDEN^NAME"
    //   ds.save_as(path, write_like_original=True)
    // The offsets asserted below are the builder-tracked element extents, so
    // they are independent of the parser's own range computation.

    /// Explicit VR LE: top-level (0400,0550) SQ UNDEFINED length with
    /// multiple items and a NESTED (0400,0561) (the conformant shape), plus a
    /// stray top-level 0561 BEFORE pixels and one AFTER pixels (our own
    /// Updated-file shape).
    #[test]
    fn test_hidden_phi_ranges_explicit_le() {
        let mut b = DicomBuilder::new().meta_header("1.2.840.10008.1.2.1");
        b = b.add_element(0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.2");
        b = b.add_element(0x0010, 0x0010, b"PN", b"");
        b = b.add_element(0x0010, 0x0020, b"LO", b"");
        b = b.add_element(0x0020, 0x000D, b"UI", b"1.2.3.3101");
        b = b.add_element(0x0020, 0x000E, b"UI", b"1.2.3.3101.1");
        b = b.add_element(0x0008, 0x0018, b"UI", b"1.2.3.3101.1.1");
        b = b.add_element(0x0008, 0x0060, b"CS", b"CT");
        b = b.add_element(0x0028, 0x0010, b"US", &1u16.to_le_bytes());
        b = b.add_element(0x0028, 0x0011, b"US", &1u16.to_le_bytes());

        // Hidden 1: (0400,0550) SQ UNDEFINED length, two items, item 1 nests
        // (0400,0561) with PN HIDDEN^NAME.
        let s1 = b.buf.len();
        let nested_0561 = el_le(
            0x0400,
            0x0561,
            b"SQ",
            &sq_item(&el_le(0x0010, 0x0010, b"PN", b"HIDDEN^NAME")),
        );
        let item2 = sq_item(&el_le(0x0010, 0x0020, b"LO", b"SECRET-ID"));
        let mut v1 = Vec::new();
        v1.extend_from_slice(&sq_item(&nested_0561));
        v1.extend_from_slice(&item2);
        v1.extend_from_slice(&0xFFFE_u16.to_le_bytes());
        v1.extend_from_slice(&0xE0DD_u16.to_le_bytes());
        v1.extend_from_slice(&0u32.to_le_bytes());
        // Append the SQ element manually: undefined length (0xFFFFFFFF).
        b.buf.extend_from_slice(&0x0400_u16.to_le_bytes());
        b.buf.extend_from_slice(&0x0550_u16.to_le_bytes());
        b.buf.extend_from_slice(b"SQ");
        b.buf.extend_from_slice(&[0u8, 0u8]);
        b.buf.extend_from_slice(&u32::MAX.to_le_bytes());
        b.buf.extend_from_slice(&v1);
        let e1 = b.buf.len();

        // Hidden 2: stray top-level (0400,0561) BEFORE pixels.
        let s2 = b.buf.len();
        b.add_explicit_element(
            0x0400,
            0x0561,
            b"SQ",
            &sq_item(&el_le(0x0010, 0x0010, b"PN", b"HIDDEN2^NAME")),
        );
        let e2 = b.buf.len();

        // Pixel data (kept by the sanitizer).
        b = b.add_element(0x7FE0, 0x0010, b"OB", &[0x00, 0x01]);

        // Hidden 3: (0400,0561) AFTER pixels (our own Updated-file shape).
        let s3 = b.buf.len();
        b.add_explicit_element(
            0x0400,
            0x0561,
            b"SQ",
            &sq_item(&el_le(0x0010, 0x0010, b"PN", b"HIDDEN3^NAME")),
        );
        let e3 = b.buf.len();

        let data = b.build();
        let meta = DicomRsParser.extract_metadata(&data).expect("parse");
        assert!(
            meta.hidden_identifying_attrs,
            "identifying values detected inside the sequences"
        );
        assert_eq!(
            meta.original_attrs_ranges,
            vec![(s1, e1), (s2, e2), (s3, e3)],
            "captured ranges are the builder-tracked element extents"
        );
        assert_eq!(meta.patient_name, "", "patient fields stay empty");
    }

    /// Implicit VR LE: a top-level 0561 with a nested non-empty PatientID.
    #[test]
    fn test_hidden_phi_ranges_implicit_le() {
        let mut b = DicomBuilder::new().meta_header("1.2.840.10008.1.2");
        b = b.add_implicit_element(0x0008, 0x0016, b"1.2.840.10008.5.1.4.1.1.2");
        b = b.add_implicit_element(0x0010, 0x0010, b"");
        b = b.add_implicit_element(0x0010, 0x0020, b"");
        b = b.add_implicit_element(0x0020, 0x000D, b"1.2.3.3102");
        b = b.add_implicit_element(0x0020, 0x000E, b"1.2.3.3102.1");
        b = b.add_implicit_element(0x0008, 0x0018, b"1.2.3.3102.1.1");
        b = b.add_implicit_element(0x0008, 0x0060, b"CT");
        b = b.add_implicit_element(0x0028, 0x0010, &1u16.to_le_bytes());
        b = b.add_implicit_element(0x0028, 0x0011, &1u16.to_le_bytes());

        let s1 = b.buf.len();
        let v1 = sq_item_implicit(&el_implicit(0x0010, 0x0020, b"HIDDEN-PATIENT-ID"));
        b = b.add_implicit_element(0x0400, 0x0561, &v1);
        let e1 = b.buf.len();

        let data = b.build();
        let meta = DicomRsParser.extract_metadata(&data).expect("parse");
        assert!(meta.hidden_identifying_attrs);
        assert_eq!(meta.original_attrs_ranges, vec![(s1, e1)]);
    }

    /// A dataset whose 0550/0561 carry NO identifying values is NOT flagged
    /// (the sanitizer leaves such bytes as-is).
    #[test]
    fn test_hidden_phi_not_flagged_for_benign_sequence() {
        let mut b = DicomBuilder::new().meta_header("1.2.840.10008.1.2.1");
        b = b.add_element(0x0010, 0x0010, b"PN", b"SMITH^JOHN");
        b = b.add_element(0x0010, 0x0020, b"LO", b"PAT-001");
        b = b.add_element(0x0020, 0x000D, b"UI", b"1.2.3.3103");
        b = b.add_element(0x0020, 0x000E, b"UI", b"1.2.3.3103.1");
        b = b.add_element(0x0008, 0x0018, b"UI", b"1.2.3.3103.1.1");
        b = b.add_element(0x0008, 0x0060, b"CS", b"CT");
        let benign = sq_item(&el_le(0x0008, 0x0020, b"DA", b"20200101"));
        b.add_explicit_element(0x0400, 0x0561, b"SQ", &benign);
        let data = b.build();
        let meta = DicomRsParser.extract_metadata(&data).expect("parse");
        assert!(
            !meta.hidden_identifying_attrs,
            "no identifying values inside: not flagged"
        );
        assert_eq!(meta.original_attrs_ranges.len(), 1, "range still captured");
    }

    /// Explicit VR LE helpers: element + item byte encoders.
    fn el_le(group: u16, elem: u16, vr: &[u8; 2], value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(vr);
        let long = matches!(
            std::str::from_utf8(vr).unwrap_or(""),
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        if long {
            out.extend_from_slice(&[0u8, 0u8]);
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        out.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            out.push(0x00);
        }
        out
    }

    fn el_implicit(group: u16, elem: u16, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            out.push(0x00);
        }
        out
    }

    /// (FFFE,E000) item with a 4-byte little-endian length (explicit LE).
    fn sq_item(inner: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xFFFE_u16.to_le_bytes());
        out.extend_from_slice(&0xE000_u16.to_le_bytes());
        out.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        out.extend_from_slice(inner);
        out
    }

    /// (FFFE,E000) item for implicit LE datasets (same 4-byte LE length).
    fn sq_item_implicit(inner: &[u8]) -> Vec<u8> {
        sq_item(inner)
    }

    // -- 0310 F3: bounded-header metadata parity -----------------------

    /// A head slice that ends inside a huge declared PixelData value must
    /// render byte-identically to the full-blob render. This is the F3
    /// contract: the metadata route reads only `max_part_header` bytes, yet the
    /// output is unchanged because the renderer stops at PixelData.
    #[test]
    fn bounded_metadata_equals_full_metadata_on_huge_pixeldata() {
        let parser = DicomRsParser;

        // A dataset with metadata tags followed by a PixelData element whose
        // DECLARED length is far larger than the head we will pass (simulating
        // a multi-GB instance). We build only the head: the declared length
        // says 100 MiB, but we store just 64 bytes of it.
        let mut file = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0008, 0x0060, b"CS", b"SM")
            .add_element(0x0028, 0x0010, b"US", &256u16.to_le_bytes())
            .add_element(0x0028, 0x0011, b"US", &256u16.to_le_bytes())
            .add_element(0x0028, 0x0008, b"IS", b"1000")
            .build();
        // PixelData header with a 100 MiB declared value, then only 64 bytes.
        file.extend_from_slice(&0x7FE0_u16.to_le_bytes());
        file.extend_from_slice(&0x0010_u16.to_le_bytes());
        file.extend_from_slice(b"OW");
        file.extend_from_slice(&[0u8, 0u8]);
        file.extend_from_slice(&(100 * 1024 * 1024u32).to_le_bytes());
        file.extend_from_slice(&[0xABu8; 64]); // the "beginning" of the value

        // Full render: this slice is SHORTER than the declared PixelData, so
        // the strict reader would reject it. The bounded reader accepts it.
        let bounded = parser
            .extract_full_metadata_bytes_bounded(&file)
            .expect("bounded render must accept a head that cuts the PixelData value");

        // The bounded output must be a valid JSON object containing the
        // metadata tags and NOT the PixelData value.
        let s = String::from_utf8_lossy(&bounded);
        assert!(s.starts_with('{') && s.ends_with('}'), "valid JSON object");
        assert!(s.contains("\"00080060\""), "Modality present");
        assert!(s.contains("\"00280010\""), "Rows present");
        assert!(
            !s.contains("7FE00010"),
            "PixelData is NOT rendered inline (the renderer stops at it)"
        );

        // Parity against a full render on a COMPLETE file (same metadata, a
        // small real PixelData). The metadata JSON (pre-injection) must match
        // the bounded render on the head, since the extra PixelData bytes never
        // affect the output.
        let mut complete = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0008, 0x0060, b"CS", b"SM")
            .add_element(0x0028, 0x0010, b"US", &256u16.to_le_bytes())
            .add_element(0x0028, 0x0011, b"US", &256u16.to_le_bytes())
            .add_element(0x0028, 0x0008, b"IS", b"1000")
            .build();
        complete.extend_from_slice(&0x7FE0_u16.to_le_bytes());
        complete.extend_from_slice(&0x0010_u16.to_le_bytes());
        complete.extend_from_slice(b"OW");
        complete.extend_from_slice(&[0u8, 0u8]);
        complete.extend_from_slice(&8u32.to_le_bytes()); // small real value
        complete.extend_from_slice(&[0xABu8; 8]);

        let full = parser
            .extract_full_metadata_bytes(&complete)
            .expect("full render");
        assert_eq!(
            bounded, full,
            "bounded head render must equal the full-blob render byte-for-byte"
        );
    }

    /// The bounded reader must not change the NON-PixelData parse: on a file
    /// WITHOUT PixelData the bounded and full renders are identical.
    #[test]
    fn bounded_metadata_equals_full_metadata_without_pixeldata() {
        let parser = DicomRsParser;
        let file = make_minimal_dicom("BOUNDED^HEAD", "CT");
        let full = parser.extract_full_metadata_bytes(&file).expect("full");
        let bounded = parser
            .extract_full_metadata_bytes_bounded(&file)
            .expect("bounded");
        assert_eq!(bounded, full);
    }

    // -- IOCM rejection notes (Stage 2 T2.1) --------------------------------

    /// Build a Key Object Selection whose root content item's concept name is
    /// the given `(code_value, scheme)` pair, referencing one SOP per entry of
    /// `refs` as `(sop_class_uid, sop_instance_uid)`.
    ///
    /// Shape per PS3.3 Table C.17-3:
    /// (0040,A375) evidence -> item -> (0008,1115) referenced series -> item ->
    /// (0008,1199) referenced SOP -> item -> (0008,1150) + (0008,1155).
    fn kos_dicom(code_value: &str, scheme: &str, refs: &[(&str, &str)]) -> Vec<u8> {
        // Innermost first: the per-SOP items.
        let mut sop_items = Vec::new();
        for (class_uid, instance_uid) in refs {
            let mut item_body = Vec::new();
            item_body.extend_from_slice(&0x0008u16.to_le_bytes());
            item_body.extend_from_slice(&0x1150u16.to_le_bytes());
            item_body.extend_from_slice(b"UI");
            item_body.extend_from_slice(&(class_uid.len() as u16).to_le_bytes());
            item_body.extend_from_slice(class_uid.as_bytes());
            if !class_uid.len().is_multiple_of(2) {
                item_body.push(0);
            }
            item_body.extend_from_slice(&0x0008u16.to_le_bytes());
            item_body.extend_from_slice(&0x1155u16.to_le_bytes());
            item_body.extend_from_slice(b"UI");
            item_body.extend_from_slice(&(instance_uid.len() as u16).to_le_bytes());
            item_body.extend_from_slice(instance_uid.as_bytes());
            if !instance_uid.len().is_multiple_of(2) {
                item_body.push(0);
            }
            // (FFFE,E000) item with an explicit length.
            sop_items.extend_from_slice(&0xFFFEu16.to_le_bytes());
            sop_items.extend_from_slice(&0xE000u16.to_le_bytes());
            sop_items.extend_from_slice(&(item_body.len() as u32).to_le_bytes());
            sop_items.extend_from_slice(&item_body);
        }

        // (0008,1199) Referenced SOP Sequence, explicit VR SQ.
        let mut series_item_body = Vec::new();
        series_item_body.extend_from_slice(&0x0008u16.to_le_bytes());
        series_item_body.extend_from_slice(&0x1199u16.to_le_bytes());
        series_item_body.extend_from_slice(b"SQ");
        series_item_body.extend_from_slice(&[0u8, 0u8]);
        series_item_body.extend_from_slice(&(sop_items.len() as u32).to_le_bytes());
        series_item_body.extend_from_slice(&sop_items);

        // Series item wrapping it.
        let mut series_items = Vec::new();
        series_items.extend_from_slice(&0xFFFEu16.to_le_bytes());
        series_items.extend_from_slice(&0xE000u16.to_le_bytes());
        series_items.extend_from_slice(&(series_item_body.len() as u32).to_le_bytes());
        series_items.extend_from_slice(&series_item_body);

        // (0008,1115) Referenced Series Sequence.
        let mut evidence_item_body = Vec::new();
        evidence_item_body.extend_from_slice(&0x0008u16.to_le_bytes());
        evidence_item_body.extend_from_slice(&0x1115u16.to_le_bytes());
        evidence_item_body.extend_from_slice(b"SQ");
        evidence_item_body.extend_from_slice(&[0u8, 0u8]);
        evidence_item_body.extend_from_slice(&(series_items.len() as u32).to_le_bytes());
        evidence_item_body.extend_from_slice(&series_items);
        // The study UID that owns the series (0020,000D), as the macro states.
        // Appended BEFORE any padding decision so the whole SQ value stays
        // even: `add_element` writes an explicit length and then pads outside
        // it, and an odd SQ body here would shift every following element.
        evidence_item_body.extend_from_slice(&0x0020u16.to_le_bytes());
        evidence_item_body.extend_from_slice(&0x000Du16.to_le_bytes());
        evidence_item_body.extend_from_slice(b"UI");
        let study = b"1.2.840.10008.9.9";
        evidence_item_body.extend_from_slice(&(study.len() as u16).to_le_bytes());
        evidence_item_body.extend_from_slice(study);
        if !evidence_item_body.len().is_multiple_of(2) {
            evidence_item_body.push(0);
        }

        let mut evidence_items = Vec::new();
        evidence_items.extend_from_slice(&0xFFFEu16.to_le_bytes());
        evidence_items.extend_from_slice(&0xE000u16.to_le_bytes());
        evidence_items.extend_from_slice(&(evidence_item_body.len() as u32).to_le_bytes());
        evidence_items.extend_from_slice(&evidence_item_body);

        // Concept Name Code Sequence (0040,A043): one item with the title code.
        let mut code_item_body = Vec::new();
        code_item_body.extend_from_slice(&0x0008u16.to_le_bytes());
        code_item_body.extend_from_slice(&0x0100u16.to_le_bytes());
        code_item_body.extend_from_slice(b"SH");
        code_item_body.extend_from_slice(&(code_value.len() as u16).to_le_bytes());
        code_item_body.extend_from_slice(code_value.as_bytes());
        if !code_value.len().is_multiple_of(2) {
            code_item_body.push(0x20);
        }
        code_item_body.extend_from_slice(&0x0008u16.to_le_bytes());
        code_item_body.extend_from_slice(&0x0102u16.to_le_bytes());
        code_item_body.extend_from_slice(b"SH");
        code_item_body.extend_from_slice(&(scheme.len() as u16).to_le_bytes());
        code_item_body.extend_from_slice(scheme.as_bytes());

        let mut concept_items = Vec::new();
        concept_items.extend_from_slice(&0xFFFEu16.to_le_bytes());
        concept_items.extend_from_slice(&0xE000u16.to_le_bytes());
        concept_items.extend_from_slice(&(code_item_body.len() as u32).to_le_bytes());
        concept_items.extend_from_slice(&code_item_body);

        DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"QA^NOTE")
            .add_element(0x0010, 0x0020, b"LO", b"PAT-KOS")
            .add_element(0x0020, 0x000D, b"UI", study)
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.10008.9.9.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.10008.9.9.1.1")
            .add_element(0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.88.59")
            .add_element(0x0008, 0x0060, b"CS", b"SR")
            .add_element(0x0040, 0xA043, b"SQ", &concept_items)
            .add_element(0x0040, 0xA375, b"SQ", &evidence_items)
            .build()
    }

    /// A Quality rejection note yields its referenced SOPs, class UID included
    /// so the caller can apply the per-reason scope without re-reading them.
    #[test]
    fn a_quality_rejection_note_yields_its_references() {
        let refs = [
            ("1.2.840.10008.5.1.4.1.1.88.33", "1.2.3.4.5.1"),
            ("1.2.840.10008.5.1.4.1.1.2", "1.2.3.4.5.2"),
        ];
        let file = kos_dicom("113001", "DCM", &refs);
        let meta = DicomRsParser
            .extract_metadata(&file)
            .expect("the synthetic KOS parses");
        assert_eq!(
            meta.sop_class_uid.as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.88.59"),
            "fixture must be a key object selection"
        );
        assert_eq!(
            meta.rejection_referenced_sops,
            vec![
                (
                    "1.2.840.10008.5.1.4.1.1.88.33".to_string(),
                    "1.2.3.4.5.1".to_string()
                ),
                (
                    "1.2.840.10008.5.1.4.1.1.2".to_string(),
                    "1.2.3.4.5.2".to_string()
                ),
            ]
        );
    }

    /// A KOS with a title that is not a known reason is stored as an ordinary
    /// document: no references leave the parser, so nothing can be hidden.
    #[test]
    fn a_kos_with_an_unknown_title_yields_no_references() {
        let refs = [("1.2.840.10008.5.1.4.1.1.88.33", "1.2.3.4.5.1")];
        let meta = DicomRsParser
            .extract_metadata(&kos_dicom("121066", "DCM", &refs))
            .expect("the synthetic KOS parses");
        assert!(
            meta.rejection_referenced_sops.is_empty(),
            "an unrecognised title must not sequester anything"
        );
    }

    /// The same number under a different coding scheme is a different concept.
    #[test]
    fn a_title_under_another_scheme_yields_no_references() {
        let refs = [("1.2.840.10008.5.1.4.1.1.88.33", "1.2.3.4.5.1")];
        let meta = DicomRsParser
            .extract_metadata(&kos_dicom("113001", "99IHE", &refs))
            .expect("the synthetic KOS parses");
        assert!(meta.rejection_referenced_sops.is_empty());
    }

    /// An ordinary image that happens to carry a reference sequence is not a
    /// rejection note - the SOP class gate keeps the walk inert.
    #[test]
    fn a_non_kos_with_references_yields_no_rejection_references() {
        let mut sop_items = Vec::new();
        sop_items.extend_from_slice(&0xFFFEu16.to_le_bytes());
        sop_items.extend_from_slice(&0xE000u16.to_le_bytes());
        sop_items.extend_from_slice(&0u32.to_le_bytes());
        let file = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"IMG^ONE")
            .add_element(0x0010, 0x0020, b"LO", b"PAT-1")
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.10008.9.9")
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.10008.9.9.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.10008.9.9.1.1")
            // Enhanced SR, NOT the key object class.
            .add_element(0x0008, 0x0016, b"UI", b"1.2.840.10008.5.1.4.1.1.88.22")
            .add_element(0x0008, 0x0060, b"CS", b"SR")
            .add_element(0x0040, 0xA375, b"SQ", &sop_items)
            .build();
        let meta = DicomRsParser
            .extract_metadata(&file)
            .expect("the synthetic SR parses");
        assert!(meta.rejection_referenced_sops.is_empty());
    }

    /// An Implicit VR instance still yields its nested KEY scalars.
    ///
    /// PS3.18 Table 10.6.1-5 makes `00400275.00400009` and
    /// `00400275.00401001` REQUIRED series matching attributes. PS3.5 6.2
    /// gives an Implicit VR element no VR field, so a walker that read
    /// `pos+4..pos+6` as a VR consumed the low half of the length and landed
    /// inside the value: the nested scalars came back empty and QIDO matched
    /// nothing for those instances.
    #[test]
    fn an_implicit_instance_yields_its_nested_request_attributes() {
        // Request Attributes Sequence, one item, written IMPLICIT VR LE.
        let mut item = Vec::new();
        // (0040,0009) ScheduledProcedureStepID SH
        item.extend_from_slice(&0x0040u16.to_le_bytes());
        item.extend_from_slice(&0x0009u16.to_le_bytes());
        item.extend_from_slice(&4u32.to_le_bytes());
        item.extend_from_slice(b"SPS1");
        // (0040,1001) RequestedProcedureID SH
        item.extend_from_slice(&0x0040u16.to_le_bytes());
        item.extend_from_slice(&0x1001u16.to_le_bytes());
        item.extend_from_slice(&4u32.to_le_bytes());
        item.extend_from_slice(b"RP01");

        let mut file = DicomBuilder::new().meta_header("1.2.840.10008.1.2").build();
        // The meta group is Explicit VR LE; everything below it is Implicit VR
        // LE, so every element is tag + 4-byte length + value.
        let mut push = |g: u16, e: u16, v: &[u8]| {
            file.extend_from_slice(&g.to_le_bytes());
            file.extend_from_slice(&e.to_le_bytes());
            file.extend_from_slice(&(v.len() as u32).to_le_bytes());
            file.extend_from_slice(v);
            if !v.len().is_multiple_of(2) {
                file.push(0);
            }
        };
        push(0x0010, 0x0010, b"IMPL^NESTED");
        push(0x0010, 0x0020, b"PAT-9");
        push(0x0020, 0x000D, b"1.2.840.10008.9.11");
        push(0x0020, 0x000E, b"1.2.840.10008.9.11.1");
        push(0x0008, 0x0018, b"1.2.840.10008.9.11.1.1");
        push(0x0008, 0x0060, b"CT");
        // (0040,0275) SQ, defined length, one item.
        file.extend_from_slice(&0x0040u16.to_le_bytes());
        file.extend_from_slice(&0x0275u16.to_le_bytes());
        let sq_value_len = 8 + item.len();
        file.extend_from_slice(&(sq_value_len as u32).to_le_bytes());
        file.extend_from_slice(&0xFFFEu16.to_le_bytes());
        file.extend_from_slice(&0xE000u16.to_le_bytes());
        file.extend_from_slice(&(item.len() as u32).to_le_bytes());
        file.extend_from_slice(&item);

        let meta = DicomRsParser
            .extract_metadata(&file)
            .expect("an implicit instance parses");
        assert_eq!(
            meta.scheduled_procedure_step_id.as_deref(),
            Some("SPS1"),
            "the nested step id is read in implicit VR"
        );
        assert_eq!(meta.requested_procedure_id.as_deref(), Some("RP01"));
    }

    /// An unrelated element BEFORE the sequence item must not end the walk.
    ///
    /// The step-over helper read a VR from every element it passed, which an
    /// Implicit VR element (PS3.5 6.2) does not carry: the low half of its
    /// 4-byte length became "VR letters" and the walk landed inside the value,
    /// so a single plain LO ahead of the sequence swallowed the nested scalars
    /// the sequence holds.
    #[test]
    fn an_unrelated_implicit_element_before_the_sequence_does_not_end_the_walk() {
        let mut item = Vec::new();
        item.extend_from_slice(&0x0040u16.to_le_bytes());
        item.extend_from_slice(&0x0009u16.to_le_bytes());
        item.extend_from_slice(&4u32.to_le_bytes());
        item.extend_from_slice(b"SPS7");

        let mut file = DicomBuilder::new().meta_header("1.2.840.10008.1.2").build();
        let mut push = |g: u16, e: u16, v: &[u8]| {
            file.extend_from_slice(&g.to_le_bytes());
            file.extend_from_slice(&e.to_le_bytes());
            file.extend_from_slice(&(v.len() as u32).to_le_bytes());
            file.extend_from_slice(v);
            if !v.len().is_multiple_of(2) {
                file.push(0);
            }
        };
        push(0x0010, 0x0010, b"IMPL^SKIP");
        push(0x0010, 0x0020, b"PAT-10");
        push(0x0020, 0x000D, b"1.2.840.10008.9.21");
        push(0x0020, 0x000E, b"1.2.840.10008.9.21.1");
        push(0x0008, 0x0018, b"1.2.840.10008.9.21.1.1");
        push(0x0008, 0x0060, b"CT");
        // (0040,0275) Request Attributes Sequence, DEFINED length, whose value
        // holds a plain LO before its item - the element the walk has to step
        // over. Implicit VR gives it no VR field, so a step-over that reads
        // one consumes the low half of the length as VR letters.
        let mut sq_value = Vec::new();
        sq_value.extend_from_slice(&0x0008u16.to_le_bytes());
        sq_value.extend_from_slice(&0x1030u16.to_le_bytes());
        sq_value.extend_from_slice(&(b"ROUTINE CHEST ".len() as u32).to_le_bytes());
        sq_value.extend_from_slice(b"ROUTINE CHEST ");
        sq_value.extend_from_slice(&0xFFFEu16.to_le_bytes());
        sq_value.extend_from_slice(&0xE000u16.to_le_bytes());
        sq_value.extend_from_slice(&(item.len() as u32).to_le_bytes());
        sq_value.extend_from_slice(&item);
        file.extend_from_slice(&0x0040u16.to_le_bytes());
        file.extend_from_slice(&0x0275u16.to_le_bytes());
        file.extend_from_slice(&(sq_value.len() as u32).to_le_bytes());
        file.extend_from_slice(&sq_value);

        let meta = DicomRsParser
            .extract_metadata(&file)
            .expect("an implicit instance parses");
        assert_eq!(
            meta.scheduled_procedure_step_id.as_deref(),
            Some("SPS7"),
            "the walk survives the unrelated element ahead of the sequence"
        );
    }

    /// A stream of delimiters must not consume the stack.    ///
    /// PS3.5 Section 7.5 lets a Sequence Delimiter (FFFE,E0DD) appear without
    /// a matching opener, and nothing bounds how many an upload may contain:
    /// 8 bytes each, so a 1 MiB part holds ~130k of them. The walk has to stay
    /// a loop - one stack frame per delimiter aborts the process under
    /// `panic = abort`, which is a remote kill on a single crafted request.
    #[test]
    fn a_stream_of_delimiters_does_not_exhaust_the_stack() {
        const DELIMITERS: usize = 200_000;
        let mut body = Vec::with_capacity(DELIMITERS * 8);
        for _ in 0..DELIMITERS {
            body.extend_from_slice(&0xFFFEu16.to_le_bytes());
            body.extend_from_slice(&0xE00Du16.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes());
        }
        let file = DicomBuilder::new()
            .meta_header("1.2.840.10008.1.2.1")
            .add_element(0x0010, 0x0010, b"PN", b"DELIM^TEST")
            .add_element(0x0010, 0x0020, b"LO", b"PAT-1")
            .add_element(0x0020, 0x000D, b"UI", b"1.2.840.10008.9.7")
            .add_element(0x0020, 0x000E, b"UI", b"1.2.840.10008.9.7.1")
            .add_element(0x0008, 0x0018, b"UI", b"1.2.840.10008.9.7.1.1")
            .add_element(0x0008, 0x0060, b"CS", b"CT")
            .build();
        let mut data = file.clone();
        data.extend_from_slice(&body);

        // Element 0x00100010 precedes the delimiter run, so a walk that dies
        // on the tenth delimiter would never reach this assertion.
        let meta = DicomRsParser
            .extract_metadata(&data)
            .expect("the delimiter run is traversed, not recursed");
        assert_eq!(meta.patient_id, "PAT-1");
    }
}
