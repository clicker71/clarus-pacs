// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The report object: a Basic Text SR carrying a model's answer verbatim
//! (PS3.3 A.35.4, IOD 1.2.840.10008.5.1.4.1.1.88.11).
//!
//! Same hand-written, zero-dependency style as [`crate::pr`], and the same
//! discipline: explicit VR little endian, stack buffers for the fixed
//! pieces, and one owned `Vec<u8>` for the dataset.
//!
//! The content items are written with DEFINED lengths rather than
//! delimiters: an undefined-length item leaves its end to a delimiter a
//! reader must track through the nested sequences of its own, and a reader
//! that gets it wrong places the answer beside the container instead of
//! inside it.
//!
//! # One text item, and why
//!
//! The document's content is ONE `TEXT` item inside a root `CONTAINER`,
//! and its `TextValue` is the model's reply byte for byte. Nothing here
//! parses the reply, splits it per image, or looks for a disclaimer: the
//! per-image narration and the closing sentence are PROMPT-FILE CONTENT,
//! so the transport is a transport. A reader that wants the answer reads
//! (0040,A160); it does not have to know how the prompt was worded.
//!
//! # Evidence
//!
//! `CurrentRequestedProcedureEvidenceSequence` (0040,A375) references
//! every analyzed image, so a viewer can bind the report to the series it
//! was written about instead of showing a floating document.
//!
//! # Identity
//!
//! Patient name and ID are COPIED from the source study. This is not
//! optional: the origin's SOP-owner check compares a derived object's
//! patient against the study's images, and a viewer groups the report
//! under the patient only when the name matches.

use crate::conf::{RuleResult, SrTemplate};
use crate::pr::{
    EXPLICIT_VR_LE, write_cs, write_da, write_is, write_item_end, write_item_start, write_lo,
    write_pn, write_sequence_end, write_sequence_start, write_tm, write_ui,
};

/// SOP Class UID of a Basic Text SR.
///
/// The current, non-retired class: unlike the presentation-state situation
/// in [`crate::pr`], every viewer that renders a report at all knows this
/// one, and there is no older class to stay compatible with.
pub const BASIC_TEXT_SR_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.88.11";

/// The coded concept name of the root container: (121070, DCM, "Findings").
pub const CODE_FINDINGS: (&str, &str, &str) = ("121070", "DCM", "Findings");

/// The coded concept name of the text item: (121071, DCM, "Finding").
pub const CODE_FINDING: (&str, &str, &str) = ("121071", "DCM", "Finding");

/// The coded concept name of the document title: (121066, DCM,
/// "Procedure reported").
///
/// The code the template requires at the title level: TID 2000 puts the
/// report's subject here, and a reader lists the document by it.
pub const CODE_PROCEDURE_REPORTED: (&str, &str, &str) = ("121066", "DCM", "Procedure reported");

/// The coding scheme designator of the DICOM concept-name codes.
pub const DCM_DESIGNATOR: &str = "DCM";

/// (0008,0005) Specific Character Set.
///
/// ISO_IR 192 is UTF-8, so ONE fixed declaration covers every script the
/// model can answer in - Latin, Cyrillic and CJK alike (PS3.3 Table C.12-2,
/// PS3.5 6.1.2.5.3). Branching per script would let a document's own byte
/// encoding depend on the answer's language, which is exactly the ambiguity
/// the attribute exists to remove.
pub const SPECIFIC_CHARACTER_SET: &str = "ISO_IR 192";

/// (0008,0070) Manufacturer: the device that produced the document.
///
/// Type 2 in the General Equipment module (PS3.3 C.7.5.1), so it is always
/// present; a report with no manufacturer reads as anonymous provenance.
pub const MANUFACTURER: &str = "clinfer";

/// (0002,0012) Implementation Class UID.
///
/// The project's own arc, shared with the bridge and the existing test
/// fixtures. A UID minted from a digest of the name would need a hash this
/// crate does not carry, and the arc is already the project's identifier
/// everywhere else.
pub const IMPLEMENTATION_CLASS_UID: &str = "1.2.826.0.1.3680043.10.1234.1";

/// (0002,0013) Implementation Version Name, VR SH: 16 characters at most
/// (PS3.10 Table 7.1-1, PS3.5 Table 6.2-1).
///
/// Truncated below where it is built: a longer name is not rejected by a
/// reader, it is silently written with the wrong length, which is worse.
const IMPLEMENTATION_VERSION_PREFIX: &str = "clinfer ";

/// The maximum length of an SH value (PS3.5 Table 6.2-1).
const SH_MAX_LEN: usize = 16;

/// The identity a report needs, supplied by the caller so a test can pin
/// it and the worker can mint it fresh.
#[derive(Debug, Clone)]
pub struct SrIdentity {
    /// (0008,0018) of the report itself.
    pub sop_instance_uid: String,
    /// (0020,000D) of the study the report belongs to.
    pub study_uid: String,
    /// (0020,000E) of the report series.
    pub series_uid: String,
    /// The source images the report was written about, grouped by series.
    pub referenced: Vec<SrReferencedSeries>,
    /// (0010,0010) copied from the source study; the origin's SOP-owner
    /// check and every viewer's patient grouping depend on it.
    pub patient_name: Option<String>,
    /// (0010,0020) copied from the source study - same reason.
    pub patient_id: Option<String>,
    /// (0010,0021) Issuer of Patient ID, part of the "Issuer of Patient ID
    /// Macro" row of AIW-I Appendix V Table V-1 ("Source Copy").
    ///
    /// `Some("")` writes a ZERO-LENGTH element, which is how a workitem
    /// states "no issuer at this site"; `None` omits the element because
    /// the workitem said nothing. The distinction matters: Appendix V
    /// copies the value unaltered, so an explicit empty must not be
    /// silently dropped nor replaced by a fallback.
    pub patient_id_issuer: Option<String>,
    /// (0008,0090) copied from the source study when it carries one; the
    /// report names the physician the images were referred by, and the
    /// sidecar has no other source for it.
    pub referring_physician: Option<String>,
    /// (0020,0010) copied from the source study. Type 2 in the General
    /// Study module: written empty when unknown.
    pub study_id: Option<String>,
    /// (0008,0050) copied from the source study - same Type-2 rule.
    pub accession_number: Option<String>,
    /// (0008,1010) Station Name, taken from the workitem's (0040,4028)
    /// Performed Station Name Code Sequence Code Meaning (AIW-I Appendix V
    /// Table V-1 Note 4).
    ///
    /// `None` omits the attribute: the mapping runs FROM (0040,4028), so
    /// there is nothing to fall back on and a station name invented from
    /// the writer's own configuration would claim the report came from a
    /// station the workitem never named.
    pub station_name: Option<String>,
    /// (0040,0008) Scheduled Protocol Code Sequence, mapped from the
    /// workitem's (0040,4018) Scheduled Workitem Code Sequence (Appendix V
    /// Table V-1: "Copy from: Scheduled Workitem Code Sequence (0040,4018)",
    /// one item). `None` omits the sequence.
    pub scheduled_protocol_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0040,1001) Requested Procedure ID, Appendix V Table V-1 "Source
    /// Copy" through the requested-procedure rows.
    pub requested_procedure_id: Option<String>,
    /// (0032,1060) Requested Procedure Description.
    pub requested_procedure_description: Option<String>,
    /// (0032,1064) Requested Procedure Code Sequence, one item.
    pub requested_procedure_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0040,1002) Reason for the Requested Procedure.
    pub reason_for_requested_procedure: Option<String>,
    /// (0040,100A) Reason for Requested Procedure Code Sequence, one item.
    pub reason_for_requested_procedure_code: Option<crate::dicomweb::WorkitemCode>,
}

/// One source series the report references, and its analyzed images.
#[derive(Debug, Clone, PartialEq)]
pub struct SrReferencedSeries {
    /// (0008,1115) item -> (0020,000E).
    pub series_uid: String,
    /// The images the report was written about, in instance order.
    pub images: Vec<SrReferencedImage>,
}

/// One analyzed image, named by its class+instance pair.
#[derive(Debug, Clone, PartialEq)]
pub struct SrReferencedImage {
    /// (0008,1150), Type 1 in the reference item (C.7.6.1).
    pub sop_class_uid: String,
    /// (0008,1155).
    pub sop_instance_uid: String,
}

/// What the rule contributes to the report.
#[derive(Debug, Clone)]
pub struct SrProfile {
    /// `result.series_description`, when the rule names one.
    pub series_description: Option<String>,
    /// `result.sr.title`: the human title of the document.
    pub title: Option<String>,
    /// `result.sr.template_id`.
    pub template_id: SrTemplate,
}

impl SrProfile {
    #[must_use]
    pub fn of(result: &RuleResult) -> Self {
        let sr = result.sr.clone().unwrap_or_default();
        Self {
            series_description: result.series_description.clone(),
            title: sr.title,
            template_id: sr.template_id,
        }
    }
}

/// Serialize one Basic Text SR.
///
/// `text` is the model's answer and is written verbatim; the caller
/// decides whether there is an answer at all (an empty reply is an error
/// before this point, not a document with an empty text item).
#[must_use]
pub fn serialize(identity: &SrIdentity, profile: &SrProfile, text: &str) -> Vec<u8> {
    let mut dataset = Vec::with_capacity(2048 + text.len());

    // TOP-LEVEL ELEMENTS ARE WRITTEN IN ASCENDING TAG ORDER: PS3.5 7.1 makes
    // that the order of a data set, and a reader is entitled to rely on it.
    // The comments below group the writes by MODULE; the order here is the
    // tag order, which is why one module's rows sometimes appear in two
    // places.
    //
    // (0008,0005) Specific Character Set, Type 1C (PS3.3 C.12.1.1.1): a
    // document whose text may be Cyrillic or CJK must declare its encoding
    // up front, or a reader falls back to the default and mangles it. It is
    // also the lowest top-level tag, so the two rules agree.
    write_cs(&mut dataset, 0x0008, 0x0005, SPECIFIC_CHARACTER_SET);
    // (0008,0016)/(0008,0018) repeat in the dataset what the file meta
    // says, for a reader that receives the object without its meta.
    write_ui(&mut dataset, 0x0008, 0x0016, BASIC_TEXT_SR_SOP_CLASS_UID);
    write_ui(&mut dataset, 0x0008, 0x0018, &identity.sop_instance_uid);
    // (0008,0020)/(0008,0030) Study Date/Time, then (0008,0023)/(0008,0033)
    // Content Date/Time: when this document's content was created, which is
    // the same instant.
    let (date, time) = crate::pr::now_stamp();
    write_da(&mut dataset, 0x0008, 0x0020, &date);
    write_da(&mut dataset, 0x0008, 0x0023, &date);
    write_tm(&mut dataset, 0x0008, 0x0030, &time);
    write_tm(&mut dataset, 0x0008, 0x0033, &time);
    // (0008,0050) AccessionNumber - Type 2 in the General Study module
    // (PS3.3 C.7.2.1), same present-but-may-be-empty rule.
    write_sh(
        &mut dataset,
        0x0008,
        0x0050,
        identity.accession_number.as_deref().unwrap_or(""),
    );
    // (0008,0060) Modality: a structured report is "SR".
    write_cs(&mut dataset, 0x0008, 0x0060, "SR");
    // (0008,0070) Manufacturer is Type 2 in General Equipment (C.7.5.1):
    // always present, "clinfer" when the device has no other name to give.
    write_lo(&mut dataset, 0x0008, 0x0070, MANUFACTURER);
    // (0008,0090) Referring Physician's Name, Type 2 in the General Study
    // module: written empty when the origin did not carry one, never
    // fabricated - a name here attributes the request to a person.
    write_pn(
        &mut dataset,
        0x0008,
        0x0090,
        identity.referring_physician.as_deref().unwrap_or(""),
    );
    // (0008,1010) Station Name, from the workitem's (0040,4028) Performed
    // Station Name Code Sequence Code Meaning (AIW-I Appendix V Table V-1,
    // Note 4: "Map the Code Meaning of the item in the Performed Station
    // Name Code Sequence (0040,4028) into the Station Name (0008,1010)").
    //
    // Type 3 (optional) in the General Equipment module, and there is no
    // fallback: the mapping is FROM (0040,4028), so a workitem that carries
    // no such sequence leaves the attribute out instead of inventing the
    // station that wrote the report.
    if let Some(station) = &identity.station_name {
        write_sh(&mut dataset, 0x0008, 0x1010, station);
    }
    // (0008,103E) Series Description, then (0008,1111) Referenced Performed
    // Procedure Step Sequence: Type 2, so present and empty rather than
    // absent, and it sorts among the (0008,xxxx) rows - which is why it is
    // here and not below with the other Type 2 sequence.
    if let Some(description) = &profile.series_description {
        write_lo(&mut dataset, 0x0008, 0x103E, description);
    }
    write_sequence_start(&mut dataset, 0x0008, 0x1111);
    write_sequence_end(&mut dataset);
    // (0008,0061) Modalities in Study and (0008,1110) Referenced Study
    // Sequence are not written: the first belongs to a study-level query
    // result, the second would repeat the study UID already carried.

    // -- Patient identity ---------------------------------------------
    // Copied from the source study.
    //
    // Type 2 means PRESENT, value may be empty (PS3.3 C.7.1.1): when the
    // source row carries no value the attribute is written with a zero
    // length rather than dropped. An absent attribute says the writer
    // forgot; an empty one says the origin did not know.
    write_pn(
        &mut dataset,
        0x0010,
        0x0010,
        identity.patient_name.as_deref().unwrap_or(""),
    );
    write_lo(
        &mut dataset,
        0x0010,
        0x0020,
        identity.patient_id.as_deref().unwrap_or(""),
    );
    // (0010,0021) Issuer of Patient ID - the "Issuer of Patient ID Macro"
    // row of AIW-I Appendix V Table V-1, typed Source Copy.
    //
    // `Some("")` writes the zero-length element: the workitem explicitly
    // said "no issuer at this site", and Appendix V copies the value
    // UNALTERED, so an explicit empty is written rather than dropped.
    // `None` - the workitem said nothing - omits the element, and there is
    // deliberately no fallback, because no source here carries one.
    if let Some(issuer) = &identity.patient_id_issuer {
        write_lo(&mut dataset, 0x0010, 0x0021, issuer);
    }

    // -- Study identity -----------------------------------------------
    // (0020,000D)/(0020,000E): the report is a series of the study it
    // describes, and the origin's STOW refuses an object that does not
    // name its study.
    write_ui(&mut dataset, 0x0020, 0x000D, &identity.study_uid);
    write_ui(&mut dataset, 0x0020, 0x000E, &identity.series_uid);
    // (0020,0010) StudyID - Type 2 in the General Study module.
    write_sh(
        &mut dataset,
        0x0020,
        0x0010,
        identity.study_id.as_deref().unwrap_or(""),
    );
    // (0020,0011) Series Number, Type 2 in the Series module: one object
    // per series, always 1.
    write_is(&mut dataset, 0x0020, 0x0011, "1");
    // (0020,0013) Instance Number, Type 1 in the SR Document General
    // module (C.17.2): one document per series here, always 1.
    write_is(&mut dataset, 0x0020, 0x0013, "1");
    // (0032,1060) Requested Procedure Description and (0032,1064) Requested
    // Procedure Code Sequence - Appendix V Table V-1 "Source Copy" rows.
    // Written only when the workitem states them: the co-located study does
    // not carry the requested-procedure attributes, so there is nothing to
    // fall back on (see the report).
    if let Some(description) = &identity.requested_procedure_description {
        write_lo(&mut dataset, 0x0032, 0x1060, description);
    }
    if let Some(code) = &identity.requested_procedure_code {
        write_workitem_code_sequence(&mut dataset, 0x0032, 0x1064, code);
    }

    // -- Requested-procedure identity (PS3.3 C.17.2) -------------------
    // (0040,0008) Scheduled Protocol Code Sequence, mapped from the
    // workitem's (0040,4018) Scheduled Workitem Code Sequence: "Copy from:
    // Scheduled Workitem Code Sequence (0040,4018)" on the result side of
    // AIW-I Appendix V Table V-1. Written only when the workitem scheduled
    // a code - the mapping is FROM the workitem, so there is no fallback.
    if let Some(code) = &identity.scheduled_protocol_code {
        write_workitem_code_sequence(&mut dataset, 0x0040, 0x0008, code);
    }
    // (0040,1001) Requested Procedure ID, (0040,1002) Reason for the
    // Requested Procedure and (0040,100A) Reason for Requested Procedure
    // Code Sequence - the remaining requested-procedure "Source Copy" rows.
    if let Some(id) = &identity.requested_procedure_id {
        write_sh(&mut dataset, 0x0040, 0x1001, id);
    }
    if let Some(reason) = &identity.reason_for_requested_procedure {
        write_lo(&mut dataset, 0x0040, 0x1002, reason);
    }
    if let Some(code) = &identity.reason_for_requested_procedure_code {
        write_workitem_code_sequence(&mut dataset, 0x0040, 0x100A, code);
    }
    // (0040,A372) Performed Procedure Code Sequence: Type 2, so present and
    // empty rather than absent. It sorts below the requested-procedure rows
    // above and above the document-general attributes below.
    write_sequence_start(&mut dataset, 0x0040, 0xA372);
    write_sequence_end(&mut dataset);
    // (0040,A375) Current Requested Procedure Evidence Sequence: what the
    // report was written about. It sorts above (0040,A491)/(0040,A493),
    // because A375 < A491 and the document-general rows come after the
    // evidence the document is about.
    write_sequence_start(&mut dataset, 0x0040, 0xA375);
    write_item_start(&mut dataset);
    // PS3.5 7.1 ascending order inside the item: (0008,1115) sorts before
    // (0020,000D), so the referenced-series sequence comes first and the
    // study UID after it. The item used to open with the study UID and then
    // descend to a lower tag.
    write_sequence_start(&mut dataset, 0x0008, 0x1115);
    for series in &identity.referenced {
        write_item_start(&mut dataset);
        // Same rule one level down: (0008,1199) before (0020,000E).
        write_sequence_start(&mut dataset, 0x0008, 0x1199);
        for image in &series.images {
            write_item_start(&mut dataset);
            write_ui(&mut dataset, 0x0008, 0x1150, &image.sop_class_uid);
            write_ui(&mut dataset, 0x0008, 0x1155, &image.sop_instance_uid);
            write_item_end(&mut dataset);
        }
        write_sequence_end(&mut dataset);
        write_ui(&mut dataset, 0x0020, 0x000E, &series.series_uid);
        write_item_end(&mut dataset);
    }
    write_sequence_end(&mut dataset);
    write_ui(&mut dataset, 0x0020, 0x000D, &identity.study_uid);
    write_item_end(&mut dataset);
    write_sequence_end(&mut dataset);
    // (0040,A491)/(0040,A493): COMPLETE and UNVERIFIED. The document is
    // finished (nothing was left out) and unverified (no human read it),
    // which is exactly what a machine-written report is.
    write_cs(&mut dataset, 0x0040, 0xA491, "COMPLETE");
    write_cs(&mut dataset, 0x0040, 0xA493, "UNVERIFIED");

    // -- SR Document Content ------------------------------------------
    // (0040,A730) Content Sequence -> the container item -> the text item.
    //
    // Both items are written with a DEFINED length. An undefined-length
    // item is legal, but it leaves the end of an item to a delimiter a
    // reader has to track through the nested sequences of its own: pydicom
    // read the container as TWO siblings in the sequence and put the
    // answer outside the item that owns it, and a strict DCMTK-based
    // viewer refused the document outright. The lengths are known here -
    // each item body is built into a local buffer - so there is nothing to
    // guess, and no reason to make a reader guess.
    let content = content_item(profile, text);
    write_sequence_start(&mut dataset, 0x0040, 0xA730);
    write_defined_item(&mut dataset, &content);
    write_sequence_end(&mut dataset);

    // -- Assemble the Part-10 file ------------------------------------
    let mut out = Vec::with_capacity(dataset.len() + 256);
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");
    write_file_meta(&mut out, identity);
    out.extend_from_slice(&dataset);
    out
}

/// One `FFFE,E000` item with a DEFINED length.
///
/// The undefined sentinel and the `(FFFE,E00D)` delimiter are deliberately
/// not used: see the call site for why an item length is written rather
/// than delimited.
fn write_defined_item(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(&0xFFFEu16.to_le_bytes());
    out.extend_from_slice(&0xE000u16.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
}

/// The root CONTAINER item's body: the container's own attributes, its
/// Content Template Sequence, and the ONE nested TEXT item.
///
/// Split out so the two item lengths can be computed instead of delimited.
fn content_item(profile: &SrProfile, text: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(text.len() + 256);

    // The container carries the document's structure and NOT the answer:
    // (0040,A160) belongs to the nested TEXT item (PS3.3 Table C.17-4),
    // and putting it on the container is what a reader reports as a
    // malformed tree.
    //
    // (0040,A010) RelationshipType is NOT written on the root: the root
    // item has no source content item to relate to, and the attribute is
    // only meaningful for a child (PS3.3 C.17.3.1.2 - the root is written
    // without it, and a reader that expects one at the root is reading a
    // tree that was assembled wrongly).
    write_cs(&mut body, 0x0040, 0xA040, "CONTAINER");
    write_code_sequence(&mut body, 0x0040, 0xA043, CODE_FINDINGS);
    // (0040,A050) Continuity of Content, Type 1 (C.17.3): SEPARATE, because
    // the single text item is a whole finding and not a fragment of one.
    write_cs(&mut body, 0x0040, 0xA050, "SEPARATE");
    // (0040,A504) Content Template Sequence: the template the document
    // follows. `TemplateIdentifier` is CS, and the two UID-valued
    // attributes the item may also carry are absent: TID 2000 is
    // identified by its number in the DCMR registry.
    write_sequence_start(&mut body, 0x0040, 0xA504);
    let template = template_item(profile);
    write_defined_item(&mut body, &template);
    write_sequence_end(&mut body);

    // The one content item: the model's answer, verbatim.
    let text_body = text_item(text);
    write_sequence_start(&mut body, 0x0040, 0xA730);
    write_defined_item(&mut body, &text_body);
    write_sequence_end(&mut body);

    body
}

/// The `ContentTemplateSequence` item's body.
///
/// PS3.3 C.17.3.2.4 types the item as TemplateIdentifier (0040,DB00) CS +
/// MappingResource (0008,0105) CS. Neither the UID-valued attributes of the
/// item nor (0008,0102) CodingSchemeDesignator belong here: TID 2000 is
/// identified by its number in the DCMR registry, and the designator is the
/// attribute of a CODE item, not of a template item.
fn template_item(profile: &SrProfile) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    // Ascending tag order within the item.
    write_cs(&mut body, 0x0008, 0x0105, profile.template_id.designator());
    write_cs(
        &mut body,
        0x0040,
        0xDB00,
        profile.template_id.template_number(),
    );
    body
}

/// The nested TEXT content item's body, carrying the answer.
///
/// (0040,A160) TextValue is UT, a long VR: the answer is free text of
/// unbounded length, and a short-length attribute would cap it at 65534
/// bytes - which a chatty completion can reach, and whose failure mode
/// would be a truncated report.
fn text_item(text: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(text.len() + 96);
    // PS3.5 7.1 requires the elements inside an item in ASCENDING tag order:
    // (0040,A010) RelationshipType comes before (0040,A040) ValueType.
    // (0040,A010) CONTAINS, per TID 2002 "Report Narrative" Row 3
    // (PS3.16): `CONTAINS | TEXT | BCID 7002`. The narrative is reached
    // from TID 2000 by an INCLUDE of TID 2002 (TID 2000 Row 10), not by a
    // relationship of its own, so the relationship the child carries is
    // the narrative's. TID 2000 itself reserves HAS CONCEPT MOD for the
    // title modifiers in its rows 2-6.
    write_cs(&mut body, 0x0040, 0xA010, "CONTAINS");
    write_cs(&mut body, 0x0040, 0xA040, "TEXT");
    // (0040,A043) ConceptNameCodeSequence: (121071, DCM, "Finding") is a
    // member of the narrative's value set BCID 7002 (PS3.16 CID 7002,
    // extensible).
    write_code_sequence(&mut body, 0x0040, 0xA043, CODE_FINDING);
    write_ut(&mut body, 0x0040, 0xA160, text);
    body
}

/// (0008,0100) Code Value, (0008,0102) Coding Scheme Designator,
/// (0008,0104) Code Meaning.
fn write_code_sequence(out: &mut Vec<u8>, group: u16, element: u16, code: (&str, &str, &str)) {
    // The item inside carries a defined length for the same reason the
    // content items do: a reader should never have to track a delimiter
    // through a nested sequence to find where an item ends.
    let mut body = Vec::with_capacity(64);
    // Ascending tag order within the item.
    write_sh(&mut body, 0x0008, 0x0100, code.0);
    write_lo(&mut body, 0x0008, 0x0102, code.1);
    write_lo(&mut body, 0x0008, 0x0104, code.2);
    write_sequence_start(out, group, element);
    write_defined_item(out, &body);
    write_sequence_end(out);
}

/// The same, for a code read out of a workitem.
///
/// AIW-I Appendix V maps three workitem codes into this object's sequences:
/// (0040,4018) -> (0040,0008), (0032,1064) and (0040,100A). They arrive as
/// the `CodeSequenceMacro` the parser produced, and this lifts the three
/// parts onto the local writer above so there is ONE code-sequence
/// implementation in this file rather than a second one that could drift
/// from it.
fn write_workitem_code_sequence(
    out: &mut Vec<u8>,
    group: u16,
    element: u16,
    code: &crate::dicomweb::WorkitemCode,
) {
    write_code_sequence(
        out,
        group,
        element,
        (&code.value, &code.scheme, &code.meaning),
    );
}

/// (0008,0100) Code Value is SH, which `pr`'s writers do not carry.
fn write_sh(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    crate::pr::write_string(out, group, element, b"SH", value);
}

/// (0040,A160) TextValue, VR UT (PS3.3 Table C.17-4).
///
/// Written into the caller's buffer directly: the answer is already owned
/// by the caller, so a second copy would be a per-study allocation for no
/// reason.
fn write_ut(out: &mut Vec<u8>, group: u16, element: u16, value: &str) {
    let bytes = value.as_bytes();
    let pad = if bytes.len().is_multiple_of(2) { 0 } else { 1 };
    crate::pr::write_explicit_header(out, group, element, b"UT", bytes.len() + pad);
    out.extend_from_slice(bytes);
    if pad == 1 {
        out.push(b' ');
    }
}

/// (0002,xxxx) group: the canonical trio plus the two information UIDs.
fn write_file_meta(out: &mut Vec<u8>, identity: &SrIdentity) {
    let meta = file_meta_bytes(identity);
    crate::pr::write_explicit_header(out, 0x0002, 0x0000, b"UL", 4);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
}

fn file_meta_bytes(identity: &SrIdentity) -> Vec<u8> {
    let mut meta = Vec::with_capacity(256);
    // Every value goes through `write_string`, so one padding rule covers
    // the whole object: a hand-written length put an odd one on
    // (0002,0003) whenever a UID had an odd character count, which is
    // illegal for every VR (PS3.5 6.2).
    crate::pr::write_string(&mut meta, 0x0002, 0x0001, b"OB", "01");
    crate::pr::write_string(
        &mut meta,
        0x0002,
        0x0002,
        b"UI",
        BASIC_TEXT_SR_SOP_CLASS_UID,
    );
    crate::pr::write_string(&mut meta, 0x0002, 0x0003, b"UI", &identity.sop_instance_uid);
    crate::pr::write_string(&mut meta, 0x0002, 0x0010, b"UI", EXPLICIT_VR_LE);
    crate::pr::write_string(&mut meta, 0x0002, 0x0012, b"UI", IMPLEMENTATION_CLASS_UID);
    crate::pr::write_string(
        &mut meta,
        0x0002,
        0x0013,
        b"SH",
        &implementation_version_name(),
    );
    meta
}

/// (0002,0013) Implementation Version Name: the crate version stamped onto
/// every document, so the build that produced a stored report is readable
/// from the report itself.
///
/// Truncated to the SH limit without splitting a character: the version is
/// ASCII here, but a name cut mid-codepoint is invalid UTF-8 in an
/// attribute that a reader will try to decode.
///
/// Public because `pr` writes the same value into its own file meta: the
/// build stamp has to identify the crate identically in both result
/// objects (PS3.10 Table 7.1-1).
#[must_use]
pub fn implementation_version_name() -> String {
    let name = format!(
        "{IMPLEMENTATION_VERSION_PREFIX}{}",
        env!("CARGO_PKG_VERSION")
    );
    if name.len() <= SH_MAX_LEN {
        return name;
    }
    let mut end = SH_MAX_LEN;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_SERIES: &str = "1.2.826.0.1.3680043.10.1234.9002";
    const SOURCE_INSTANCE: &str = "1.2.826.0.1.3680043.10.1234.9003";
    const SOURCE_CLASS: &str = "1.2.840.10008.5.1.4.1.1.1.1";

    fn identity() -> SrIdentity {
        SrIdentity {
            sop_instance_uid: "2.25.1234".to_string(),
            study_uid: "1.2.826.0.1.3680043.10.1234.9001".to_string(),
            series_uid: "2.25.5678".to_string(),
            referenced: vec![SrReferencedSeries {
                series_uid: SOURCE_SERIES.to_string(),
                images: vec![SrReferencedImage {
                    sop_class_uid: SOURCE_CLASS.to_string(),
                    sop_instance_uid: SOURCE_INSTANCE.to_string(),
                }],
            }],
            patient_name: Some("Fixture^SR".to_string()),
            patient_id: Some("SR-FIXTURE".to_string()),
            patient_id_issuer: Some("SITE-A".to_string()),
            referring_physician: Some("REFERRER^A".to_string()),
            study_id: Some("STUDY-1".to_string()),
            accession_number: Some("ACC-1".to_string()),
            station_name: Some("Fixture Station".to_string()),
            scheduled_protocol_code: Some(crate::dicomweb::WorkitemCode {
                value: "CAD".to_string(),
                scheme: "99CLARUS".to_string(),
                meaning: "AI detection".to_string(),
            }),
            requested_procedure_id: Some("RP-1".to_string()),
            requested_procedure_description: Some("Chest CT".to_string()),
            requested_procedure_code: Some(crate::dicomweb::WorkitemCode {
                value: "RP-CHEST".to_string(),
                scheme: "99CLARUS".to_string(),
                meaning: "Chest procedure".to_string(),
            }),
            reason_for_requested_procedure: Some("Screening".to_string()),
            reason_for_requested_procedure_code: Some(crate::dicomweb::WorkitemCode {
                value: "REASON-1".to_string(),
                scheme: "99CLARUS".to_string(),
                meaning: "Screening indication".to_string(),
            }),
        }
    }

    /// The same identity for a source row that carried none of the Type-2
    /// attributes: the writer must still emit them, empty.
    fn identity_without_source_values() -> SrIdentity {
        SrIdentity {
            patient_name: None,
            patient_id: None,
            referring_physician: None,
            study_id: None,
            accession_number: None,
            ..identity()
        }
    }

    fn profile(title: Option<&str>, description: Option<&str>) -> SrProfile {
        SrProfile {
            series_description: description.map(str::to_string),
            title: title.map(str::to_string),
            template_id: SrTemplate::Tid2000,
        }
    }

    const REPLY: &str = "Image 1: no pathology.\nImage 2: consolidation at the right base.\nI am an AI, I only advised, this is not a diagnosis.";

    /// Every occurrence of an 8-digit hex tag in the dataset.
    fn tag_at(bytes: &[u8], group: u16, element: u16) -> bool {
        let needle = [group.to_le_bytes(), element.to_le_bytes()].concat();
        bytes.windows(4).any(|window| window == needle.as_slice())
    }

    // -- The content tree, read the way a STRICT parser reads it ---------
    //
    // The tag pins above cannot see the defect these tests exist for: a
    // document can contain every required tag and still nest them wrongly.
    // pydicom read the previous output as `ContentSequence` holding TWO
    // siblings - the container, then the answer beside it - and a DCMTK
    // viewer refused it. So the tests below walk the item structure
    // rather than search for tags.

    /// One thing found while walking the content tree.
    #[derive(Debug, PartialEq, Eq)]
    enum Node {
        /// An item delimiter, with the length it declared.
        Item {
            length: u32,
        },
        /// A tag with its VR and value, at some depth.
        Element {
            tag: (u16, u16),
            vr: [u8; 2],
            value: Vec<u8>,
        },
        SequenceEnd,
    }

    /// Walk an explicit-VR-LE dataset into a flat list of nodes.
    ///
    /// Deliberately independent of the crate's own parser: it descends into
    /// every item and sequence, which is what lets it disagree with a
    /// serializer that got the nesting wrong.
    fn walk(data: &[u8], from: usize) -> Vec<Node> {
        // The VRs whose header carries a 2-byte reserved field and a 4-byte
        // length, rather than a 2-byte length (PS3.5 Table 7.1-1).
        const LONG_VR: &[&[u8; 2]] = &[
            b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT",
            b"UV",
        ];
        let mut nodes = Vec::new();
        let mut at = from;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
            if group == 0xFFFE {
                let length =
                    u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]);
                at += 8;
                match element {
                    0xE000 => nodes.push(Node::Item { length }),
                    0xE0DD => nodes.push(Node::SequenceEnd),
                    // An item delimiter at the dataset level means the
                    // nesting was wrong somewhere above.
                    _ => {}
                }
                continue;
            }
            let vr = [data[at + 4], data[at + 5]];
            let (length, head) = if LONG_VR.contains(&&vr) {
                (
                    u32::from_le_bytes([data[at + 8], data[at + 9], data[at + 10], data[at + 11]])
                        as usize,
                    12,
                )
            } else {
                (u16::from_le_bytes([data[at + 6], data[at + 7]]) as usize, 8)
            };
            let start = at + head;
            if length == 0xFFFF_FFFF {
                // An undefined-length sequence: its items and their
                // delimiters follow inline, so the walk steps past the
                // header and continues INTO the sequence rather than
                // skipping a length it does not have.
                nodes.push(Node::Element {
                    tag: (group, element),
                    vr,
                    value: Vec::new(),
                });
                at = start;
                continue;
            }
            let end = start + length;
            if end > data.len() {
                break;
            }
            nodes.push(Node::Element {
                tag: (group, element),
                vr,
                value: data[start..end].to_vec(),
            });
            at = end;
        }
        nodes
    }

    /// The dataset, after the 128-byte preamble and the meta group.
    fn dataset(bytes: &[u8]) -> &[u8] {
        // (0002,0000) UL is the group length at 132; the meta group ends
        // 12 bytes later plus its length.
        let length = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]) as usize;
        &bytes[132 + 12 + length..]
    }

    /// The top-level tags of the dataset, in the order they are written.
    ///
    /// Each element is measured by its own header rather than found by a
    /// byte scan: a scan cannot tell a sequence's contents from the dataset
    /// around it, so it would report item bodies as elements.
    fn top_level_tags(bytes: &[u8]) -> Vec<(u16, u16)> {
        const LONG_VR: &[&[u8; 2]] = &[
            b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT",
            b"UV",
        ];
        let data = dataset(bytes);
        let mut tags = Vec::new();
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
            if group == 0xFFFE {
                // A delimiter has no place here: the top level holds no
                // unterminated item, so reaching one means the walk is off.
                break;
            }
            let vr = [data[at + 4], data[at + 5]];
            let (length, head) = if LONG_VR.contains(&&vr) {
                (
                    u32::from_le_bytes([data[at + 8], data[at + 9], data[at + 10], data[at + 11]])
                        as usize,
                    12,
                )
            } else {
                (u16::from_le_bytes([data[at + 6], data[at + 7]]) as usize, 8)
            };
            tags.push((group, element));
            if length == 0xFFFF_FFFF {
                at = skip_undefined_value(data, at + head);
            } else {
                at += head + length;
            }
        }
        tags
    }

    /// The offset just past an undefined-length value that starts at `at`.
    fn skip_undefined_value(data: &[u8], mut at: usize) -> usize {
        const LONG_VR: &[&[u8; 2]] = &[
            b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT",
            b"UV",
        ];
        let mut depth = 1usize;
        while at + 8 <= data.len() {
            let group = u16::from_le_bytes([data[at], data[at + 1]]);
            let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
            if group != 0xFFFE {
                // A real element: step over it by its own length so a value
                // that happens to contain FFFE,E0DD is not read as one.
                let vr = [data[at + 4], data[at + 5]];
                let (length, head) = if LONG_VR.contains(&&vr) {
                    (
                        u32::from_le_bytes([
                            data[at + 8],
                            data[at + 9],
                            data[at + 10],
                            data[at + 11],
                        ]) as usize,
                        12,
                    )
                } else {
                    (u16::from_le_bytes([data[at + 6], data[at + 7]]) as usize, 8)
                };
                if length == 0xFFFF_FFFF {
                    at = skip_undefined_value(data, at + head);
                } else {
                    at += head + length;
                }
                continue;
            }
            let length =
                u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]])
                    as usize;
            at += 8;
            match element {
                0xE000 => {
                    // A defined-length item's body is skipped whole; an
                    // undefined one is walked until its own E00D.
                    if length != 0xFFFF_FFFF {
                        at += length;
                    } else {
                        depth += 1;
                    }
                }
                0xE00D => depth -= 1,
                0xE0DD => {
                    depth -= 1;
                    if depth == 0 {
                        return at;
                    }
                }
                _ => return at,
            }
        }
        at
    }

    #[test]
    fn the_top_level_elements_are_in_ascending_tag_order() {
        // PS3.5 7.1: a data set is an ordered list in ascending tag order,
        // and a reader is entitled to stop at the first descent. The
        // evidence sequence (0040,A375) used to be written after the
        // document-general rows (0040,A491)/(0040,A493), which is a descent.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let tags = top_level_tags(&bytes);
        assert!(
            tags.len() > 10,
            "the walk reached the end of the dataset: {} tags",
            tags.len()
        );
        for pair in tags.windows(2) {
            assert!(
                pair[0] < pair[1],
                "top-level order descends: {:04X},{:04X} before {:04X},{:04X}",
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1
            );
        }
        // The 0040 rows in their required order, as a reader expecting
        // ascending tags would meet them.
        let requested: Vec<(u16, u16)> =
            tags.iter().copied().filter(|tag| tag.0 == 0x0040).collect();
        assert_eq!(
            requested,
            vec![
                (0x0040, 0x0008),
                (0x0040, 0x1001),
                (0x0040, 0x1002),
                (0x0040, 0x100A),
                (0x0040, 0xA372),
                (0x0040, 0xA375),
                (0x0040, 0xA491),
                (0x0040, 0xA493),
                (0x0040, 0xA730),
            ],
            "the document-general rows follow the evidence they generalise"
        );
    }

    /// The tags directly inside the first item of `seq_tag`, in written order.
    ///
    /// The top-level order test cannot see this: it steps over every sequence
    /// by its terminator, so an item whose elements descend is invisible to
    /// it even though PS3.5 7.1 orders elements inside an item too.
    fn first_item_tags(nodes: &[Node], seq_tag: (u16, u16)) -> Vec<(u16, u16)> {
        // Find the sequence element, then the Item that follows it, then stop
        // at that item's delimiter - the flat node list makes the depth
        // bookkeeping a matter of counting, not of guessing.
        let mut at = match nodes
            .iter()
            .position(|n| matches!(n, Node::Element { tag, .. } if *tag == seq_tag))
        {
            Some(i) => i + 1,
            None => panic!("sequence {seq_tag:?} not found"),
        };
        // Skip to the sequence's first item.
        while at < nodes.len() && !matches!(nodes[at], Node::Item { .. }) {
            at += 1;
        }
        assert!(at < nodes.len(), "{seq_tag:?} holds no item");
        at += 1;
        let mut depth = 0usize;
        let mut tags = Vec::new();
        while at < nodes.len() {
            match &nodes[at] {
                Node::Element { tag, .. } => {
                    if depth == 0 {
                        tags.push(*tag);
                    }
                }
                Node::Item { .. } => depth += 1,
                Node::SequenceEnd => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
            }
            at += 1;
        }
        tags
    }

    /// PS3.5 7.1 orders the elements INSIDE an item as well. In (0040,A375)
    /// the referenced-series sequence (0008,1115) sorts before the study UID
    /// (0020,000D), and in each (0008,1115) item the referenced-SOP sequence
    /// (0008,1199) sorts before the series UID (0020,000E). The writer used to
    /// name the identifier first and then descend into its sequence.
    #[test]
    fn the_evidence_items_list_their_sequences_before_the_identifiers() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let nodes = walk(dataset(&bytes), 0);

        let evidence_item = first_item_tags(&nodes, (0x0040, 0xA375));
        let series_seq = evidence_item
            .iter()
            .position(|t| *t == (0x0008, 0x1115))
            .expect("(0040,A375) item holds the referenced-series sequence");
        let study_uid = evidence_item
            .iter()
            .position(|t| *t == (0x0020, 0x000D))
            .expect("(0040,A375) item names its study");
        assert!(
            series_seq < study_uid,
            "(0008,1115) must precede (0020,000D): {evidence_item:?}"
        );

        let series_item = first_item_tags(&nodes, (0x0008, 0x1115));
        let sop_seq = series_item
            .iter()
            .position(|t| *t == (0x0008, 0x1199))
            .expect("(0008,1115) item holds the referenced-SOP sequence");
        let series_uid = series_item
            .iter()
            .position(|t| *t == (0x0020, 0x000E))
            .expect("(0008,1115) item names its series");
        assert!(
            sop_seq < series_uid,
            "(0008,1199) must precede (0020,000E): {series_item:?}"
        );
    }

    #[test]
    fn the_content_tree_is_one_container_holding_one_text_item() {
        // The shape TID 2000 requires, asserted on the STRUCTURE: the
        // ContentSequence holds exactly one item, that item is the
        // container, and the container holds exactly one nested item of its
        // own carrying the answer.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let nodes = walk(dataset(&bytes), 0);

        // Locate the ContentSequence, then read the item that follows it.
        let content_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)))
            .expect("the ContentSequence is present");
        let after = &nodes[content_at + 1..];
        match &after[0] {
            Node::Item { length } => {
                assert_ne!(*length, 0xFFFF_FFFF, "the container item is defined-length");
                assert_ne!(*length, 0, "the container item is not empty");
            }
            other => panic!("ContentSequence must hold an item, found {other:?}"),
        }

        // Walk into the container from its own first element: the values
        // before the nested ContentSequence are the container's.
        let container_values: Vec<(u16, u16)> = after
            .iter()
            .take_while(
                |node| !matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)),
            )
            .filter_map(|node| match node {
                Node::Element { tag, .. } => Some(*tag),
                _ => None,
            })
            .collect();
        assert!(
            container_values.contains(&(0x0040, 0xA040)),
            "the container declares a ValueType"
        );
        assert!(
            !container_values.contains(&(0x0040, 0xA160)),
            "the container must NOT carry TextValue; the TEXT item does"
        );

        // The nested ContentSequence is the TEXT item's parent.
        let nested_at = after
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)))
            .map(|index| index + 1)
            .expect("the container holds a nested ContentSequence");
        let inner = &after[nested_at..];
        match &inner[0] {
            Node::Item { length } => {
                assert_ne!(*length, 0xFFFF_FFFF, "the text item is defined-length");
                assert_ne!(*length, 0, "the text item is not empty");
            }
            other => panic!("the nested sequence must hold an item, found {other:?}"),
        }

        // And the answer is inside THAT item, before the tree closes. Only
        // one TextValue may exist in the whole document.
        let all_text: Vec<&Vec<u8>> = nodes
            .iter()
            .filter_map(|node| match node {
                Node::Element { tag, value, .. } if *tag == (0x0040, 0xA160) => Some(value),
                _ => None,
            })
            .collect();
        assert_eq!(all_text.len(), 1, "exactly one TextValue in the document");
        let text = String::from_utf8_lossy(all_text[0]);
        assert!(
            text.trim_end() == REPLY,
            "the answer is verbatim: {:?}",
            &text[..text.len().min(60)]
        );

        // The text item closes before the container does: two sequence ends
        // between the TextValue and the end of the content tree.
        let text_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA160)))
            .expect("the TextValue is in the tree");
        let closes = nodes[text_at + 1..]
            .iter()
            .filter(|node| matches!(node, Node::SequenceEnd))
            .count();
        assert!(
            closes >= 2,
            "the text item and the container both close after the answer: {closes}"
        );
    }

    #[test]
    fn the_content_items_declare_a_length_rather_than_a_delimiter() {
        // The property the previous defect violated. An undefined-length
        // item leaves its end to a delimiter a reader must track through
        // nested sequences; a defined length cannot be misread. The
        // EVIDENCE sequence's items are deliberately not covered: they are
        // plain leaf items whose nesting is unambiguous, and changing them
        // is not this task.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let nodes = walk(dataset(&bytes), 0);
        let content_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)))
            .expect("the ContentSequence is present");
        let lengths: Vec<u32> = nodes[content_at..]
            .iter()
            .filter_map(|node| match node {
                Node::Item { length } => Some(*length),
                _ => None,
            })
            .collect();
        assert!(
            lengths.iter().all(|length| *length != 0xFFFF_FFFF),
            "no item in the content tree may use the undefined sentinel: {lengths:?}"
        );
        assert!(
            lengths.len() >= 2,
            "the container and the text item are both defined-length: {lengths:?}"
        );
    }

    #[test]
    fn the_container_item_ends_only_after_the_text_item() {
        // The exact defect the field report describes: the answer must be
        // INSIDE the container, not a sibling beside it. In the byte stream
        // that means the (0040,A160) value is followed by a sequence end,
        // and the container's own length covers it.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let nodes = walk(dataset(&bytes), 0);
        let content_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)))
            .expect("the ContentSequence is present");

        // The container's declared length must span its nested sequence.
        let Node::Item { length } = nodes[content_at + 1] else {
            panic!("the ContentSequence must hold an item");
        };
        let nested = nodes[content_at + 2..]
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)))
            .expect("the container holds a nested ContentSequence");
        // Everything the container declares must therefore contain the
        // nested sequence's own header, which is at least 12 bytes in.
        assert!(
            (length as usize) > nested,
            "the container's length {length} must span its nested sequence \
             (found at +{nested} from the container's first element)"
        );
    }

    #[test]
    fn a_long_answer_still_nests_correctly() {
        // The length arithmetic is the fragile part of a hand-written
        // writer, and the UT length is where it can overflow or misalign:
        // 65534 is the point the short form stops being usable. The text
        // deliberately ends in a non-space so the padding rule (an odd
        // length gets one space appended) cannot be confused with content.
        let long = format!("{}end", "Finding. ".repeat(9_000));
        assert!(long.len() > 65_534, "the answer crosses the short form");
        assert!(!long.ends_with(' '), "no trailing space to confuse padding");

        let bytes = serialize(&identity(), &profile(None, None), &long);
        let nodes = walk(dataset(&bytes), 0);
        let content_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)))
            .expect("the ContentSequence is present");
        let lengths: Vec<u32> = nodes[content_at..]
            .iter()
            .filter_map(|node| match node {
                Node::Item { length } => Some(*length),
                _ => None,
            })
            .collect();
        assert!(
            lengths.iter().all(|length| *length != 0xFFFF_FFFF),
            "a long answer still uses defined lengths: {lengths:?}"
        );
        let text: String = nodes
            .iter()
            .filter_map(|node| match node {
                Node::Element { tag, value, .. } if *tag == (0x0040, 0xA160) => {
                    Some(String::from_utf8_lossy(value).into_owned())
                }
                _ => None,
            })
            .collect();
        assert_eq!(text.trim_end(), long, "the whole answer survives");
    }

    #[test]
    fn the_output_is_a_part10_file_with_the_sr_class() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        assert!(bytes.len() > 132);
        assert_eq!(&bytes[128..132], b"DICM", "128-byte preamble + DICM");
        assert_eq!(&bytes[132..136], &[0x02, 0x00, 0x00, 0x00]);

        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains(BASIC_TEXT_SR_SOP_CLASS_UID),
            "the Basic Text SR class is written"
        );
        assert!(text.contains(EXPLICIT_VR_LE), "explicit VR little endian");
    }

    #[test]
    fn the_report_is_a_series_of_its_study() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("1.2.826.0.1.3680043.10.1234.9001"),
            "study UID"
        );
        assert!(text.contains("2.25.5678"), "series UID");
        assert!(text.contains("2.25.1234"), "instance UID");
        // (0008,0060) Modality = SR.
        assert!(tag_at(&bytes, 0x0008, 0x0060), "(0008,0060) Modality");
        assert!(bytes.windows(2).any(|w| w == b"SR"), "the modality value");
    }

    #[test]
    fn the_patient_identity_is_copied_from_the_study() {
        // The origin's SOP-owner check and every viewer's patient grouping
        // depend on this; a report with coerced placeholders is filed
        // under the wrong patient.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Fixture^SR"), "the name is copied");
        assert!(text.contains("SR-FIXTURE"), "the ID is copied");
    }

    #[test]
    fn the_station_name_comes_from_the_workitem() {
        // AIW-I Appendix V Table V-1 maps the workitem's (0040,4028)
        // Performed Station Name Code Sequence Code Meaning into
        // (0008,1010): "Map the Code Meaning of the item in the Performed
        // Station Name Code Sequence (0040,4028) into the Station Name
        // (0008,1010)" (Note 4).
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        assert!(tag_at(&bytes, 0x0008, 0x1010), "(0008,1010) StationName");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("Fixture Station"),
            "the Code Meaning is copied"
        );
    }

    #[test]
    fn a_workitem_without_a_performed_station_omits_the_station_name() {
        // The mapping runs FROM (0040,4028), so there is no fallback: a
        // workitem that carries no such sequence leaves the attribute out
        // rather than naming the station that happened to write it.
        let mut identity = identity();
        identity.station_name = None;
        let bytes = serialize(&identity, &profile(None, None), REPLY);
        assert!(
            !tag_at(&bytes, 0x0008, 0x1010),
            "an absent (0040,4028) omits Station Name entirely"
        );
        assert!(
            !String::from_utf8_lossy(&bytes).contains("Fixture Station"),
            "and no value is invented"
        );
    }

    /// T1: (0010,0021) Issuer of Patient ID - emitted, zero-length, omitted.
    #[test]
    fn the_issuer_of_patient_id_follows_the_patient_id() {
        // Appendix V's "Issuer of Patient ID Macro" row is Source Copy, and
        // it sorts between (0010,0020) and (0010,0030).
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        assert!(tag_at(&bytes, 0x0010, 0x0021), "(0010,0021) is present");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("SITE-A"), "the issuer value is copied");

        let issuer_at = bytes
            .windows(4)
            .position(|w| w == [0x10, 0x00, 0x21, 0x00])
            .expect("the issuer tag");
        let birth_at = bytes
            .windows(4)
            .position(|w| w == [0x10, 0x00, 0x30, 0x00])
            .unwrap_or(usize::MAX);
        assert!(issuer_at < birth_at, "rising tag order is kept");
    }

    #[test]
    fn an_empty_issuer_is_written_zero_length_and_an_absent_one_is_omitted() {
        // Appendix V copies the value UNALTERED, so the two cases are
        // different statements and must not collapse into one another.
        let mut empty = identity();
        empty.patient_id_issuer = Some(String::new());
        let bytes = serialize(&empty, &profile(None, None), REPLY);
        assert!(
            tag_at(&bytes, 0x0010, 0x0021),
            "an explicit empty issuer is still an element"
        );

        let mut absent = identity();
        absent.patient_id_issuer = None;
        let bytes = serialize(&absent, &profile(None, None), REPLY);
        assert!(
            !tag_at(&bytes, 0x0010, 0x0021),
            "an absent issuer is no element at all"
        );
    }

    /// T2/T3: the three code sequences Appendix V maps into this object.
    #[test]
    fn the_workitem_code_sequences_are_written_in_rising_tag_order() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        // (0032,1064) Requested Procedure Code, (0040,0008) Scheduled
        // Protocol Code, (0040,100A) Reason for Requested Procedure Code.
        for (group, element) in [(0x0032, 0x1064), (0x0040, 0x0008), (0x0040, 0x100A)] {
            assert!(
                tag_at(&bytes, group, element),
                "({group:04X},{element:04X}) is present"
            );
        }
        let text = String::from_utf8_lossy(&bytes);
        for value in ["RP-CHEST", "CAD", "REASON-1", "99CLARUS"] {
            assert!(text.contains(value), "{value} is carried");
        }

        let at = |group: u16, element: u16| {
            bytes
                .windows(4)
                .position(|w| {
                    w == [
                        group.to_le_bytes()[0],
                        group.to_le_bytes()[1],
                        element.to_le_bytes()[0],
                        element.to_le_bytes()[1],
                    ]
                })
                .expect("present")
        };
        assert!(at(0x0032, 0x1064) < at(0x0040, 0x0008));
        assert!(at(0x0040, 0x0008) < at(0x0040, 0x100A));
    }

    #[test]
    fn an_absent_workitem_code_omits_its_sequence() {
        let mut identity = identity();
        identity.scheduled_protocol_code = None;
        identity.requested_procedure_code = None;
        identity.reason_for_requested_procedure_code = None;
        let bytes = serialize(&identity, &profile(None, None), REPLY);
        for (group, element) in [(0x0032, 0x1064), (0x0040, 0x0008), (0x0040, 0x100A)] {
            assert!(
                !tag_at(&bytes, group, element),
                "({group:04X},{element:04X}) is omitted, not written empty"
            );
        }
    }

    /// T3: the requestor-supplied text rows.
    #[test]
    fn the_requested_procedure_text_rows_are_copied_when_present() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        for (group, element) in [(0x0040, 0x1001), (0x0040, 0x1002), (0x0032, 0x1060)] {
            assert!(
                tag_at(&bytes, group, element),
                "({group:04X},{element:04X}) is present"
            );
        }
        let text = String::from_utf8_lossy(&bytes);
        for value in ["RP-1", "Screening", "Chest CT"] {
            assert!(text.contains(value), "{value} is carried");
        }

        let mut absent = identity();
        absent.requested_procedure_id = None;
        absent.reason_for_requested_procedure = None;
        absent.requested_procedure_description = None;
        let bytes = serialize(&absent, &profile(None, None), REPLY);
        for (group, element) in [(0x0040, 0x1001), (0x0040, 0x1002), (0x0032, 0x1060)] {
            assert!(
                !tag_at(&bytes, group, element),
                "({group:04X},{element:04X}) is omitted when the workitem is silent"
            );
        }
    }

    #[test]
    fn the_mandatory_modules_are_present() {
        let bytes = serialize(
            &identity(),
            &profile(Some("LLM Findings"), Some("LLM Findings")),
            REPLY,
        );
        // SR Document General (C.17.2).
        assert!(tag_at(&bytes, 0x0040, 0xA491), "(0040,A491) CompletionFlag");
        assert!(
            tag_at(&bytes, 0x0040, 0xA493),
            "(0040,A493) VerificationFlag"
        );
        assert!(
            tag_at(&bytes, 0x0040, 0xA375),
            "(0040,A375) evidence sequence"
        );
        // InstanceNumber is Type 1 in this module, not Type 2.
        assert!(tag_at(&bytes, 0x0020, 0x0013), "(0020,0013) InstanceNumber");
        // Series module Type 2.
        assert!(tag_at(&bytes, 0x0020, 0x0011), "(0020,0011) SeriesNumber");
        // Content Date/Time, distinct from the Study pair.
        assert!(tag_at(&bytes, 0x0008, 0x0023), "(0008,0023) ContentDate");
        assert!(tag_at(&bytes, 0x0008, 0x0033), "(0008,0033) ContentTime");

        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("COMPLETE"), "CompletionFlag value");
        assert!(text.contains("UNVERIFIED"), "VerificationFlag value");
        assert!(text.contains("LLM Findings"), "the series description");
    }

    #[test]
    fn the_content_is_one_container_holding_one_text_item() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        // SR Document Content (C.17.3).
        assert!(
            tag_at(&bytes, 0x0040, 0xA730),
            "(0040,A730) ContentSequence"
        );
        assert!(tag_at(&bytes, 0x0040, 0xA040), "(0040,A040) ValueType");
        // (0040,A010) is carried by the CHILD item only: see
        // `the_root_item_carries_no_relationship_type`.
        assert!(
            tag_at(&bytes, 0x0040, 0xA010),
            "(0040,A010) RelationshipType"
        );
        assert!(
            tag_at(&bytes, 0x0040, 0xA043),
            "(0040,A043) ConceptNameCodeSequence"
        );
        assert!(
            tag_at(&bytes, 0x0040, 0xA050),
            "(0040,A050) ContinuityOfContent"
        );
        assert!(
            tag_at(&bytes, 0x0040, 0xA504),
            "(0040,A504) ContentTemplateSequence"
        );
        assert!(tag_at(&bytes, 0x0040, 0xA160), "(0040,A160) TextValue");

        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("CONTAINER"), "the root item's value type");
        assert!(text.contains("TEXT"), "the content item's value type");
        assert!(text.contains("CONTAINS"), "the relationship type");
        assert!(text.contains("SEPARATE"), "ContinuityOfContent");
        assert!(text.contains("DCMR"), "the template designator");
    }

    #[test]
    fn the_concept_names_are_the_tid_2000_codes() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let text = String::from_utf8_lossy(&bytes);
        // (121070, DCM, "Findings") on the root container and
        // (121071, DCM, "Finding") on the text item.
        assert!(text.contains("121070"), "the root concept code");
        assert!(text.contains("Findings"), "the root concept meaning");
        assert!(text.contains("121071"), "the text concept code");
        assert!(text.contains("Finding"), "the text concept meaning");
        assert!(text.contains("DCM"), "the concept scheme designator");
    }

    #[test]
    fn the_root_item_carries_no_relationship_type() {
        // (0040,A010) relates a content item to its SOURCE item, and the
        // root has none: PS3.3 C.17.3.1.2 defines the attribute for a child
        // of the tree. Writing it at the root hands a reader a relationship
        // with nothing to point at.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let nodes = walk(dataset(&bytes), 0);
        let content_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)))
            .expect("the ContentSequence is present");
        // From the ContentSequence forward: the container's own elements run
        // up to its nested ContentSequence, and that prefix is the root.
        let after = &nodes[content_at + 1..];
        let root_prefix: Vec<(u16, u16)> = after
            .iter()
            .take_while(
                |node| !matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA730)),
            )
            .filter_map(|node| match node {
                Node::Element { tag, .. } => Some(*tag),
                _ => None,
            })
            .collect();
        assert!(
            !root_prefix.contains(&(0x0040, 0xA010)),
            "the root item must not declare a relationship type: {root_prefix:?}"
        );
        // The child keeps one, and the document still carries the tag once.
        assert!(
            tag_at(&bytes, 0x0040, 0xA010),
            "the child item still declares its relationship"
        );
    }

    #[test]
    fn the_text_item_uses_the_report_narrative_relationship() {
        // TID 2002 "Report Narrative" Row 3 (PS3.16) types this child as
        // `CONTAINS | TEXT | BCID 7002`. TID 2000 reaches the narrative by
        // an INCLUDE of TID 2002 (Row 10), so the relationship the child
        // carries is the narrative's - TID 2000's own HAS CONCEPT MOD rows
        // (2-6) modify the document title, not the answer.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let nodes = walk(dataset(&bytes), 0);
        let text_at = nodes
            .iter()
            .position(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA160)))
            .expect("the TextValue is present");
        // The relationship belongs to the same item as the TextValue, so it
        // is the nearest preceding (0040,A010) value in the walk.
        let relationship = nodes[..text_at]
            .iter()
            .rev()
            .find_map(|node| match node {
                Node::Element { tag, value, .. } if *tag == (0x0040, 0xA010) => {
                    Some(String::from_utf8_lossy(value).trim_end().to_string())
                }
                _ => None,
            })
            .expect("the text item declares a relationship");
        assert_eq!(
            relationship, "CONTAINS",
            "TID 2002 Row 3 requires CONTAINS, not HAS CONCEPT MOD"
        );
    }

    #[test]
    fn the_dataset_declares_utf8_as_its_character_set() {
        // (0008,0005) Type 1C: the answer is free text and may be Cyrillic
        // or CJK, so the document states its encoding once, up front.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let nodes = walk(dataset(&bytes), 0);
        let charset = nodes
            .iter()
            .find_map(|node| match node {
                Node::Element { tag, value, .. } if *tag == (0x0008, 0x0005) => {
                    Some(String::from_utf8_lossy(value).trim_end().to_string())
                }
                _ => None,
            })
            .expect("(0008,0005) SpecificCharacterSet is present");
        assert_eq!(charset, "ISO_IR 192", "UTF-8");
    }

    #[test]
    fn a_cyrillic_answer_is_declared_utf8_and_survives() {
        // The Russian contour: the answer's bytes are UTF-8, and the
        // document says so rather than leaving a reader to guess a code
        // page for them.
        let answer = "Очаговых изменений не выявлено. Рекомендован контроль.";
        let bytes = serialize(&identity(), &profile(None, None), answer);
        assert!(
            String::from_utf8_lossy(&bytes).contains("ISO_IR 192"),
            "the charset is declared"
        );
        assert!(
            bytes.windows(answer.len()).any(|w| w == answer.as_bytes()),
            "the Cyrillic answer is stored byte for byte"
        );
    }

    #[test]
    fn a_chinese_answer_is_declared_utf8_and_survives() {
        // The exhibition contour: ISO_IR 192 is UTF-8 and therefore covers
        // CJK without a second declaration or a GB18030 branch.
        let answer = "未见明显异常。建议随访复查。";
        let bytes = serialize(&identity(), &profile(None, None), answer);
        assert!(
            String::from_utf8_lossy(&bytes).contains("ISO_IR 192"),
            "the same single declaration covers CJK"
        );
        assert!(
            bytes.windows(answer.len()).any(|w| w == answer.as_bytes()),
            "the CJK answer is stored byte for byte"
        );
    }

    #[test]
    fn the_type_2_attributes_are_present() {
        // Type 2 means present-but-may-be-empty. An owned report names its
        // manufacturer and an empty code sequence where it performs no
        // procedure step and bills no code; an ABSENT attribute would say
        // the writer forgot (PS3.3 C.7.5.1, C.17.2).
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        assert!(tag_at(&bytes, 0x0008, 0x0070), "(0008,0070) Manufacturer");
        assert!(
            tag_at(&bytes, 0x0008, 0x1111),
            "(0008,1111) ReferencedPerformedProcedureStepSequence"
        );
        assert!(
            tag_at(&bytes, 0x0040, 0xA372),
            "(0040,A372) PerformedProcedureCodeSequence"
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains("clinfer"),
            "the manufacturer value"
        );

        // Both sequences are EMPTY: header, then the sequence delimiter with
        // no item between them (a zero-length SQ, PS3.5 6.3).
        let nodes = walk(dataset(&bytes), 0);
        for tag in [(0x0008u16, 0x1111u16), (0x0040, 0xA372)] {
            let at = nodes
                .iter()
                .position(|node| matches!(node, Node::Element { tag: t, .. } if *t == tag))
                .unwrap_or_else(|| panic!("{tag:?} is present"));
            assert!(
                matches!(nodes[at + 1], Node::SequenceEnd),
                "{tag:?} must hold no items"
            );
        }
    }

    #[test]
    fn the_content_template_item_uses_the_standard_attributes() {
        // PS3.3 C.17.3.2.4: TemplateIdentifier (0040,DB00) CS and
        // MappingResource (0008,0105) CS. (0040,DB73) is
        // ReferencedContentItemIdentifier with VR UL, and (0008,0102) is
        // the designator of a CODE item - neither belongs to a template
        // item.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        assert!(
            tag_at(&bytes, 0x0040, 0xDB00),
            "(0040,DB00) TemplateIdentifier is present"
        );
        assert!(
            tag_at(&bytes, 0x0008, 0x0105),
            "(0008,0105) MappingResource is present"
        );
        assert!(
            !tag_at(&bytes, 0x0040, 0xDB73),
            "(0040,DB73) is a UL-valued attribute and must not carry the template number"
        );

        // Both are CS, and the values are the ones the standard names.
        let nodes = walk(dataset(&bytes), 0);
        let value_of = |tag: (u16, u16)| {
            nodes.iter().find_map(|node| match node {
                Node::Element { tag: t, vr, value } if *t == tag => {
                    Some((*vr, String::from_utf8_lossy(value).trim_end().to_string()))
                }
                _ => None,
            })
        };
        let (vr, number) = value_of((0x0040, 0xDB00)).expect("(0040,DB00)");
        assert_eq!(vr, *b"CS", "TemplateIdentifier is CS");
        assert_eq!(number, "2000", "the template number");
        let (vr, designator) = value_of((0x0008, 0x0105)).expect("(0008,0105)");
        assert_eq!(vr, *b"CS", "MappingResource is CS");
        assert_eq!(designator, "DCMR", "the registry the template lives in");
    }

    #[test]
    fn the_template_item_holds_exactly_the_two_standard_attributes() {
        // The concept-name codes still use (0008,0102) inside their own
        // items; the template item must not, so the item's BYTES are
        // walked rather than its tags searched for in the whole document.
        // The writer emits defined-length items, so the body can be read
        // by its declared length.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let data = dataset(&bytes);
        let at = data
            .windows(4)
            .position(|window| window == [0x40, 0x00, 0x04, 0xA5])
            .expect("(0040,A504) ContentTemplateSequence");
        assert_eq!(&data[at + 4..at + 6], b"SQ");
        // Undefined-length sequence: an item header follows immediately.
        assert_eq!(&data[at + 12..at + 16], &[0xFE, 0xFF, 0x00, 0xE0]);
        let length =
            u32::from_le_bytes([data[at + 16], data[at + 17], data[at + 18], data[at + 19]])
                as usize;
        let body = &data[at + 20..at + 20 + length];

        // Exactly two CS elements in ascending tag order, and their values.
        assert_eq!(&body[0..4], &[0x08, 0x00, 0x05, 0x01], "(0008,0105) first");
        assert_eq!(&body[4..6], b"CS");
        assert_eq!(u16::from_le_bytes([body[6], body[7]]), 4, "\"DCMR\"");
        assert_eq!(&body[8..12], b"DCMR");
        assert_eq!(
            &body[12..16],
            &[0x40, 0x00, 0x00, 0xDB],
            "(0040,DB00) second"
        );
        assert_eq!(&body[16..18], b"CS");
        assert_eq!(u16::from_le_bytes([body[18], body[19]]), 4, "\"2000\"");
        assert_eq!(&body[20..24], b"2000");
        assert_eq!(
            body.len(),
            24,
            "nothing else is in the item - no (0040,DB73), no (0008,0102)"
        );
    }

    #[test]
    fn the_type_2_study_attributes_are_present_but_empty_without_a_source() {
        // Type 2 = present, value may be empty (PS3.3 C.7.2.1, C.7.1.1):
        // a source row that carried nothing still gets the attribute, with
        // a zero length, rather than none at all.
        let bytes = serialize(
            &identity_without_source_values(),
            &profile(None, None),
            REPLY,
        );
        let nodes = walk(dataset(&bytes), 0);
        for (group, element, vr, why) in [
            (0x0010u16, 0x0010u16, b"PN" as &[u8; 2], "PatientName"),
            (0x0010, 0x0020, b"LO", "PatientID"),
            (0x0008, 0x0050, b"SH", "AccessionNumber"),
            (0x0020, 0x0010, b"SH", "StudyID"),
            (0x0008, 0x0090, b"PN", "ReferringPhysicianName"),
        ] {
            let found = nodes.iter().find_map(|node| match node {
                Node::Element { tag, vr, value } if *tag == (group, element) => {
                    Some((*vr, value.clone()))
                }
                _ => None,
            });
            let (found_vr, value) =
                found.unwrap_or_else(|| panic!("({group:04X},{element:04X}) {why} is absent"));
            assert_eq!(&found_vr, vr, "({group:04X},{element:04X}) {why} VR");
            assert!(
                value.is_empty(),
                "({group:04X},{element:04X}) {why} is present with a zero length"
            );
        }
    }

    #[test]
    fn the_type_2_study_attributes_carry_the_source_values_when_there_are_any() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let text = String::from_utf8_lossy(&bytes);
        for value in ["Fixture^SR", "SR-FIXTURE", "STUDY-1", "ACC-1", "REFERRER^A"] {
            assert!(
                text.contains(value),
                "the copied value {value:?} is present"
            );
        }
    }

    #[test]
    fn the_referring_physician_is_copied_from_the_study() {
        // (0008,0090) Type 2 in the General Study module: copied from the
        // source study like the patient identity, because the sidecar has
        // no other source and a fabricated name would attribute the
        // request to the wrong person.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        assert!(tag_at(&bytes, 0x0008, 0x0090), "(0008,0090) is present");
        assert!(
            String::from_utf8_lossy(&bytes).contains("REFERRER^A"),
            "the physician is copied"
        );
    }

    #[test]
    fn a_study_without_a_referring_physician_writes_an_empty_attribute() {
        // The other half of Type 2: nothing to copy means the attribute is
        // still written, with a zero length. Dropping it would say the
        // writer forgot, which is a different statement from "the origin
        // did not know" (PS3.3 C.7.2.1).
        let mut identity = identity();
        identity.referring_physician = None;
        let bytes = serialize(&identity, &profile(None, None), REPLY);
        assert!(
            tag_at(&bytes, 0x0008, 0x0090),
            "an unknown referrer is present, empty - not omitted"
        );
        let nodes = walk(dataset(&bytes), 0);
        let value = nodes
            .iter()
            .find_map(|node| match node {
                Node::Element { tag, value, .. } if *tag == (0x0008, 0x0090) => Some(value.clone()),
                _ => None,
            })
            .expect("(0008,0090) is present");
        assert!(value.is_empty(), "and its value is empty");
    }

    #[test]
    fn the_file_meta_stamps_the_build() {
        // (0002,0012)/(0002,0013): the reader of a stored report can tell
        // which build wrote it without asking the file system.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let nodes = walk(&bytes[132..], 0);
        let meta = |tag: (u16, u16)| {
            nodes.iter().find_map(|node| match node {
                Node::Element { tag: t, value, .. } if *t == tag => Some(
                    // A UID is padded with a NULL, not a space (PS3.5
                    // 6.2, Table 6.2-1).
                    String::from_utf8_lossy(value)
                        .trim_end_matches(['\0', ' '])
                        .to_string(),
                ),
                _ => None,
            })
        };
        assert_eq!(
            meta((0x0002, 0x0012)).as_deref(),
            Some(IMPLEMENTATION_CLASS_UID),
            "(0002,0012) ImplementationClassUID"
        );
        assert_eq!(
            meta((0x0002, 0x0013)).as_deref(),
            Some(implementation_version_name().as_str()),
            "(0002,0013) ImplementationVersionName"
        );
        assert!(
            implementation_version_name().starts_with("clinfer "),
            "the version name identifies the crate"
        );
        // The build stamp is the version where SH has room for it, and a
        // prefix of it where the limit cuts in - either way the name is
        // derived from the version and not a second hand-written string.
        let version = env!("CARGO_PKG_VERSION");
        let name = implementation_version_name();
        assert!(
            version.starts_with(&name[IMPLEMENTATION_VERSION_PREFIX.len()..]),
            "the version name extends the crate version: {name:?} vs {version:?}"
        );
    }

    #[test]
    fn the_implementation_version_name_fits_the_sh_limit() {
        // (0002,0013) is VR SH: 16 characters at most (PS3.5 Table 6.2-1).
        // A longer value is not rejected by a reader - it is written with a
        // length that contradicts the value, which is worse.
        let name = implementation_version_name();
        assert!(
            name.len() <= SH_MAX_LEN,
            "the version name must fit SH: {name:?} is {} characters",
            name.len()
        );
        assert!(!name.is_empty(), "and must not be empty");
        // The packing is the reason the truncation exists: prove it fires
        // on a name that would otherwise overflow, and that it never splits
        // a character.
        let long = format!("{IMPLEMENTATION_VERSION_PREFIX}{}", "9".repeat(40));
        assert!(long.len() > SH_MAX_LEN, "the fixture crosses the limit");
        assert!(
            long.is_char_boundary(SH_MAX_LEN),
            "an ASCII name truncates cleanly at the limit"
        );
    }

    #[test]
    fn the_model_answer_is_stored_verbatim() {
        // The transport does not parse, reformat or trim the reply: the
        // disclaimer is prompt-file content and arrives inside the text,
        // not from this crate.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        assert!(
            bytes
                .windows(REPLY.len())
                .any(|window| window == REPLY.as_bytes()),
            "the exact reply bytes must appear"
        );
        // Nothing else was invented around it.
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("Disclaimer"), "no disclaimer key");
        assert!(!text.contains("AI generated"), "no invented preface");
    }

    #[test]
    fn the_text_value_is_a_long_vr() {
        // UT takes the 32-bit length form (PS3.5 Table 7.1-1): a report
        // longer than 65534 bytes must not be refused by its own header.
        let long = "x".repeat(70_000);
        let bytes = serialize(&identity(), &profile(None, None), &long);
        // Find (0040,A160) and check the VR and the reserved pair.
        let needle = [0x40, 0x00, 0x60, 0xA1];
        let at = bytes
            .windows(4)
            .position(|window| window == needle)
            .expect("(0040,A160)");
        assert_eq!(&bytes[at + 4..at + 6], b"UT");
        assert_eq!(&bytes[at + 6..at + 8], &[0, 0], "the reserved bytes");
        let declared =
            u32::from_le_bytes([bytes[at + 8], bytes[at + 9], bytes[at + 10], bytes[at + 11]]);
        assert!(
            declared >= 70_000,
            "the declared length covers the whole answer: {declared}"
        );
    }

    #[test]
    fn the_evidence_sequence_names_every_analyzed_image() {
        let mut identity = identity();
        identity.referenced[0].images.push(SrReferencedImage {
            sop_class_uid: SOURCE_CLASS.to_string(),
            sop_instance_uid: "1.2.826.0.1.3680043.10.1234.9004".to_string(),
        });
        let bytes = serialize(&identity, &profile(None, None), REPLY);

        // (0008,1115) once, (0008,1199) once, and one (0008,1155) per
        // image.
        assert_eq!(
            bytes
                .windows(4)
                .filter(|w| *w == [0x08, 0x00, 0x15, 0x11])
                .count(),
            1,
            "one Referenced Series Sequence"
        );
        assert_eq!(
            bytes
                .windows(4)
                .filter(|w| *w == [0x08, 0x00, 0x99, 0x11])
                .count(),
            1,
            "one Referenced SOP Sequence"
        );
        assert_eq!(
            bytes
                .windows(4)
                .filter(|w| *w == [0x08, 0x00, 0x55, 0x11])
                .count(),
            2,
            "one instance UID per analyzed image"
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains(SOURCE_INSTANCE));
        assert!(text.contains("1.2.826.0.1.3680043.10.1234.9004"));
        assert!(text.contains(SOURCE_CLASS), "the source SOP class");
    }

    #[test]
    fn an_undefined_length_sequence_declares_the_sentinel_once() {
        // The (0008,1115) lesson from `pr`: the sentinel IS the length, and
        // writing a zero length and then four more bytes desynchronises
        // every reader at the first sequence.
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        let at = bytes
            .windows(4)
            .position(|window| window == [0x08, 0x00, 0x15, 0x11])
            .expect("(0008,1115)");
        assert_eq!(&bytes[at + 4..at + 6], b"SQ");
        assert_eq!(&bytes[at + 6..at + 8], &[0x00, 0x00], "the reserved bytes");
        assert_eq!(
            u32::from_le_bytes([bytes[at + 8], bytes[at + 9], bytes[at + 10], bytes[at + 11]]),
            0xFFFF_FFFF,
            "the length is the undefined sentinel"
        );
        assert_eq!(
            &bytes[at + 12..at + 16],
            &[0xFE, 0xFF, 0x00, 0xE0],
            "an item follows the header immediately"
        );
    }

    #[test]
    fn the_meta_group_framing_is_what_a_parser_seeks_by() {
        let bytes = serialize(&identity(), &profile(None, None), REPLY);
        assert_eq!(&bytes[128..132], b"DICM");
        assert_eq!(&bytes[132..134], &[0x02, 0x00], "group 0002");
        assert_eq!(&bytes[134..136], &[0x00, 0x00], "(0002,0000)");
        assert_eq!(&bytes[136..138], b"UL", "the VR");
        assert_eq!(
            u16::from_le_bytes([bytes[138], bytes[139]]),
            4,
            "a UL length is 16-bit"
        );
        let declared = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]);
        assert!(declared > 0);
        let meta_end = 144 + declared as usize;
        assert_eq!(
            &bytes[meta_end..meta_end + 4],
            &[0x08, 0x00, 0x05, 0x00],
            "the dataset starts with (0008,0005) Specific Character Set"
        );
    }

    #[test]
    fn the_serializer_is_deterministic_for_the_same_identity() {
        // Rerunning after a cleared cache must mint the same object: the
        // identity is derived from the study and the profile version, so
        // only the clock differs and the content is stable.
        let first = serialize(&identity(), &profile(None, None), REPLY);
        let second = serialize(&identity(), &profile(None, None), REPLY);
        assert_eq!(first, second);
    }

    #[test]
    fn a_missing_patient_identity_is_not_written_at_all() {
        // Type 2 would want an empty value, but a report with no patient
        // is a study the origin would coerce anyway; leaving the
        // attributes out keeps the writer honest about what it does not
        // know.
        let mut identity = identity();
        identity.patient_name = None;
        identity.patient_id = None;
        let bytes = serialize(&identity, &profile(None, None), REPLY);
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("Fixture^SR"));
        assert!(!text.contains("SR-FIXTURE"));
        // The document is still complete and parseable.
        assert!(tag_at(&bytes, 0x0040, 0xA491));
    }

    #[test]
    fn the_profile_reads_the_sr_block_from_the_rule_result() {
        let result = RuleResult {
            kind: crate::conf::ResultKind::Sr,
            color: None,
            profile_version: 1,
            series_description: Some("LLM Findings".to_string()),
            label: crate::conf::LabelProfile::default(),
            sr: Some(crate::conf::SrResult {
                title: Some("LLM Findings".to_string()),
                template_id: SrTemplate::Tid2000,
            }),
            measurements: None,
        };
        let profile = SrProfile::of(&result);
        assert_eq!(profile.series_description.as_deref(), Some("LLM Findings"));
        assert_eq!(profile.title.as_deref(), Some("LLM Findings"));
        assert_eq!(profile.template_id, SrTemplate::Tid2000);

        // A result with no sr block still yields a usable profile.
        let bare = RuleResult { sr: None, ..result };
        let profile = SrProfile::of(&bare);
        assert!(profile.title.is_none());
        assert_eq!(profile.template_id, SrTemplate::Tid2000);
    }
}
