// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Enhanced SR writer: numeric findings as a TID 1500 Measurement Report.
//!
//! A separate writer from [`crate::sr`] rather than a mode of it, because the
//! two documents share nothing but the Part-10 envelope and the code-sequence
//! primitive:
//!
//! - different SOP class: Enhanced SR `.88.22`, not Basic Text SR `.88.11`;
//! - different root template: TID 1500, not TID 2000;
//! - different value types: NUM, not TEXT. A number has no legal home in a
//!   Basic Text SR (NUM is absent from its value-type list, PS3.3
//!   A.35.1.3.1.1), so extending `sr` would either produce a non-conformant
//!   object or silently change the SOP class of a block every consumer keys
//!   on.
//!
//! The tree follows `analysis/2209_architect_tid1500_REPORT.md` section 3
//! exactly, one Measurement Group per finding:
//!
//! ```text
//! CONTAINER (126000, DCM, "Imaging Measurement Report")            TID 1500 R1
//! > HAS CONCEPT MOD CODE (121058, DCM, "Procedure reported")       TID 1500 R4 (optional)
//! > CONTAINS CONTAINER (126010, DCM, "Imaging Measurements")       TID 1500 R6
//! >> HAS CONCEPT MOD TID 4019: (111001 Algorithm Name),            TID 1500 R6b (once)
//!    (111003 Algorithm Version)
//! >> CONTAINS CONTAINER (125007, DCM, "Measurement Group")         TID 1500 R9 / TID 1501 R1
//! >>> CONTAINS IMAGE (121112, DCM, "Source of Measurement")        TID 1501 R10b
//! >>> CONTAINS NUM (122157, DCM, "Probability") 1 M                TID 1501 R10 / TID 300 R1
//!              UNITS = the unit the ABI carries
//! >>>> HAS CONCEPT MOD CODE (121071, DCM, "Finding")               TID 301 R2
//!              = (<pathology code supplied by the processor>)
//! >>>> HAS CONCEPT MOD CODE (370129005, SCT, "Measurement Method") TID 301 R3 (optional)
//! >>>> HAS CONCEPT MOD CODE (121401, DCM, "Derivation")            TID 301 R4 (optional)
//! >>> CONTAINS SCOORD (111030, DCM, "Image Region")       TID 1501 R10c (optional)
//!              (0040,DB73) -> the group's IMAGE item above
//!              (0070,0023) Graphic Type = POLYGON | POLYLINE
//!              (0070,0022) Graphic Data = 2 FL per point
//! >>>> SELECTED FROM IMAGE                                 TID 1501 R10d (mandatory)
//!              (0008,1199) Referenced SOP Sequence -> the instance
//! ```
//!
//! The SCOORD row is written only when the processor traced a region for
//! that measurement: a whole-image score has no region, and the row is
//! optional in TID 1501. The `SELECTED FROM IMAGE` child is NOT optional
//! when the row is present - PS3.3 C.18.6 makes it mandatory for every
//! SCOORD - and it is also the edge a Weasis reader follows to draw the
//! outline. When the row is present the points are the source frame's
//! PIXELS, carried exactly as the ABI supplied them; clinfer stamps the
//! DICOM geometry from the source instances rather than converting here.
//!
//! Deliberately absent (all optional in the template, and all unnecessary for
//! a whole-image score): the Image Library, the Derived Imaging Measurements
//! container, and the Qualitative Evaluations container.
//!
//! Observation context is satisfied by the DATASET attributes. Every row of
//! TID 1001 is "Required if ... not inherited" and defaults to (0040,A078)
//! Author Observer Sequence and its neighbours, so writing those means the
//! content tree needs no context items - and none are written.

use crate::abi::schema::{Code, Measurements, Region};
use crate::pr::{
    EXPLICIT_VR_LE, write_cs, write_da, write_ds, write_is, write_item_end, write_item_start,
    write_lo, write_pn, write_sequence_end, write_sequence_start, write_sh, write_tm, write_uc,
    write_ui,
};

/// SOP Class UID of an Enhanced SR: the class TID 1500 lives in.
pub const ENHANCED_SR_SOP_CLASS_UID: &str = "1.2.840.10008.5.1.4.1.1.88.22";

/// Root container, TID 1500 R1: (126000, DCM, "Imaging Measurement Report").
pub const CODE_ROOT: (&str, &str, &str) = ("126000", "DCM", "Imaging Measurement Report");

/// TID 1500 R4: (121058, DCM, "Procedure reported").
///
/// A different CODE VALUE from the (121066) the Basic Text SR writer uses for
/// the same meaning; the template row is what decides which one a document
/// must carry.
pub const CODE_PROCEDURE_REPORTED: (&str, &str, &str) = ("121058", "DCM", "Procedure reported");
/// The concept a site's own report title travels under.
///
/// The code VALUE is the title itself, in the deployment's scheme: a title is
/// a site's word, so it cannot borrow a standard code that means something
/// else. Only the concept (WHAT this is) is fixed.
pub const CODE_LOCAL_TITLE: (&str, &str, &str) = ("REPORTTITLE", "99CLARUS", "Report title");

/// TID 1500 R6: (126010, DCM, "Imaging Measurements").
pub const CODE_IMAGING_MEASUREMENTS: (&str, &str, &str) = ("126010", "DCM", "Imaging Measurements");

/// TID 1501 R1: (125007, DCM, "Measurement Group").
pub const CODE_MEASUREMENT_GROUP: (&str, &str, &str) = ("125007", "DCM", "Measurement Group");

/// TID 1501 R10b: (121112, DCM, "Source of Measurement").
pub const CODE_SOURCE_OF_MEASUREMENT: (&str, &str, &str) =
    ("121112", "DCM", "Source of Measurement");

/// TID 1501 R10c: the concept name of the region row, EV (111030, DCM,
/// "Image Region").
///
/// The row's concept name is a template parameter, and an image region is
/// what the row IS - the finding it depicts is named by the NUM's own
/// modifier, and by the group's finding. Naming it here is what makes the
/// outline readable on its own, which matters because a reader that draws
/// the region does not necessarily show the number beside it.
pub const CODE_IMAGE_REGION: (&str, &str, &str) = ("111030", "DCM", "Image Region");

/// TID 301 R2: (121071, DCM, "Finding").
pub const CODE_FINDING: (&str, &str, &str) = ("121071", "DCM", "Finding");
pub const CODE_THRESHOLD: (&str, &str, &str) = ("THRESHOLD", "99CLARUS", "Operating threshold");

/// TID 4019 R1: (111001, DCM, "Algorithm Name").
pub const CODE_ALGORITHM_NAME: (&str, &str, &str) = ("111001", "DCM", "Algorithm Name");

/// TID 4019 R2: (111003, DCM, "Algorithm Version").
pub const CODE_ALGORITHM_VERSION: (&str, &str, &str) = ("111003", "DCM", "Algorithm Version");

/// (0040,A123) Person Role in Organization value marking a device observer:
/// (121088, DCM, "Device Observer").
pub const CODE_DEVICE_OBSERVER: (&str, &str, &str) = ("121088", "DCM", "Device Observer");

/// The UCUM unit of a probability in [0,1]: (1, UCUM, "no units").
///
/// Used when the ABI declares that unit. It is NOT a fallback: the writer
/// emits exactly the unit the block carries, and this constant exists only so
/// the (1) case has a code value to write.
pub const UNIT_NO_UNITS: (&str, &str, &str) = ("1", "UCUM", "no units");

/// (0008,0005) Specific Character Set.
pub const SPECIFIC_CHARACTER_SET: &str = crate::sr::SPECIFIC_CHARACTER_SET;

/// (0008,0070) Manufacturer.
pub const MANUFACTURER: &str = crate::sr::MANUFACTURER;

/// (0002,0012) Implementation Class UID.
pub const IMPLEMENTATION_CLASS_UID: &str = crate::sr::IMPLEMENTATION_CLASS_UID;

/// The identity a Measurement Report needs.
///
/// Separate from [`crate::sr::SrIdentity`] because the two documents
/// reference their sources differently: TID 1500 names one image per group
/// through a content item, so the analyzed images are a FLAT list rather than
/// the series-grouped structure the Basic Text SR carries.
#[derive(Debug, Clone)]
pub struct MeasurementsIdentity {
    /// (0008,0018) of the document itself.
    pub sop_instance_uid: String,
    /// (0020,000D) of the study the document belongs to.
    pub study_uid: String,
    /// (0020,000E) of the document series.
    pub series_uid: String,
    /// The analyzed images, in the order the frames were fetched. The FIRST
    /// one is what every Measurement Group points at: a whole-image score is
    /// about the image the model was given.
    pub images: Vec<MeasurementsImage>,
    /// (0010,0010) copied from the source study.
    pub patient_name: Option<String>,
    /// (0010,0020) copied from the source study.
    pub patient_id: Option<String>,
    /// (0010,0021) Issuer of Patient ID, part of the "Issuer of Patient ID
    /// Macro" row of AIW-I Appendix V Table V-1 ("Source Copy").
    ///
    /// `Some("")` writes a ZERO-LENGTH element (the workitem said "no issuer
    /// at this site"); `None` omits it (the workitem said nothing).
    pub patient_id_issuer: Option<String>,
    /// (0008,0090) copied from the source study.
    pub referring_physician: Option<String>,
    /// (0020,0010) copied from the source study.
    pub study_id: Option<String>,
    /// (0008,0050) copied from the source study.
    pub accession_number: Option<String>,
    /// (0008,1010) Station Name, mapped from the workitem's (0040,4028).
    pub station_name: Option<String>,
    /// (0040,0008) Scheduled Protocol Code Sequence, from (0040,4018).
    pub scheduled_protocol_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0040,1001) Requested Procedure ID.
    pub requested_procedure_id: Option<String>,
    /// (0032,1060) Requested Procedure Description.
    pub requested_procedure_description: Option<String>,
    /// (0032,1064) Requested Procedure Code Sequence.
    pub requested_procedure_code: Option<crate::dicomweb::WorkitemCode>,
    /// (0040,1002) Reason for the Requested Procedure.
    pub reason_for_requested_procedure: Option<String>,
    /// (0040,100A) Reason for Requested Procedure Code Sequence.
    pub reason_for_requested_procedure_code: Option<crate::dicomweb::WorkitemCode>,
    /// TID 4019 R1, (111001) Algorithm Name: the producer's model or script.
    ///
    /// Copied, never invented: `Producer.model` or `Producer.script` is the
    /// name the processor declared, and clinfer has no other source for it.
    pub algorithm_name: String,
    /// TID 4019 R2, (111003) Algorithm Version.
    pub algorithm_version: Option<String>,
    /// TID 4019 R2b, (122405) Algorithm Manufacturer.
    pub algorithm_manufacturer: Option<String>,
}

/// One analyzed image, named by its class+instance pair.
#[derive(Debug, Clone, PartialEq)]
pub struct MeasurementsImage {
    /// (0008,1150), Type 1 in the reference item (C.7.6.1).
    pub sop_class_uid: String,
    /// (0008,1155).
    pub sop_instance_uid: String,
    /// (0020,000E) of the series the image belongs to. The Evidence Sequence
    /// groups its reference items BY SERIES, and the processor's frames
    /// arrive flat, so the series is what carries an image back to the study
    /// it was fetched from.
    pub series_uid: String,
}

/// What the rule contributes to the document.
#[derive(Debug, Clone)]
pub struct MeasurementsProfile {
    /// `result.series_description`, when the rule names one.
    pub series_description: Option<String>,
    /// `result.measurements.title`: the human title of the document.
    pub title: Option<String>,
    /// `result.measurements.template_id`.
    pub template_id: crate::conf::SrTemplate,
}

impl MeasurementsProfile {
    #[must_use]
    pub fn of(result: &crate::conf::RuleResult) -> Self {
        let block = result.measurements.clone().unwrap_or_default();
        Self {
            series_description: result.series_description.clone(),
            title: block.title,
            template_id: block.template_id,
        }
    }
}

/// Serialize one Enhanced SR Measurement Report.
///
/// `measurements` is the validated ABI block: the unit every value carries and
/// one group per finding. Values are written VERBATIM - clinfer applies no
/// threshold, decides nothing, and never invents a number.
#[must_use]
pub fn serialize(
    identity: &MeasurementsIdentity,
    profile: &MeasurementsProfile,
    measurements: &Measurements,
) -> Vec<u8> {
    let mut dataset = Vec::with_capacity(4096);

    // -- What this object is ------------------------------------------
    // PS3.5 7.1: the Data Set is an ordered list in ascending tag order.
    // The attributes below are grouped by module in the standard but must be
    // WRITTEN in tag order, so each group is emitted where its tags sort.
    write_cs(&mut dataset, 0x0008, 0x0005, SPECIFIC_CHARACTER_SET);
    write_ui(&mut dataset, 0x0008, 0x0016, ENHANCED_SR_SOP_CLASS_UID);
    write_ui(&mut dataset, 0x0008, 0x0018, &identity.sop_instance_uid);
    // (0008,0020)/(0008,0030) Study Date/Time and (0008,0023)/(0008,0033)
    // Content Date/Time: this document is created now, so both pairs carry
    // the creation instant.
    let (date, time) = crate::pr::now_stamp();
    write_da(&mut dataset, 0x0008, 0x0020, &date);
    write_da(&mut dataset, 0x0008, 0x0023, &date);
    write_tm(&mut dataset, 0x0008, 0x0030, &time);
    write_tm(&mut dataset, 0x0008, 0x0033, &time);
    write_sh(
        &mut dataset,
        0x0008,
        0x0050,
        identity.accession_number.as_deref().unwrap_or(""),
    );
    write_cs(&mut dataset, 0x0008, 0x0060, "SR");
    write_lo(&mut dataset, 0x0008, 0x0070, MANUFACTURER);
    write_pn(
        &mut dataset,
        0x0008,
        0x0090,
        identity.referring_physician.as_deref().unwrap_or(""),
    );
    if let Some(station) = &identity.station_name {
        write_sh(&mut dataset, 0x0008, 0x1010, station);
    }
    if let Some(description) = &profile.series_description {
        write_lo(&mut dataset, 0x0008, 0x103E, description);
    }
    // (0008,1111) Referenced Performed Procedure Step Sequence is Type 2, so
    // it is always present and legitimately empty when this document was not
    // produced by a performed step. It sorts inside group 0008.
    write_sequence_start(&mut dataset, 0x0008, 0x1111);
    write_sequence_end(&mut dataset);
    // Patient identity, Type 2: present, value may be empty.
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
    if let Some(issuer) = &identity.patient_id_issuer {
        write_lo(&mut dataset, 0x0010, 0x0021, issuer);
    }
    write_ui(&mut dataset, 0x0020, 0x000D, &identity.study_uid);
    write_ui(&mut dataset, 0x0020, 0x000E, &identity.series_uid);
    write_sh(
        &mut dataset,
        0x0020,
        0x0010,
        identity.study_id.as_deref().unwrap_or(""),
    );
    // (0020,0011)/(0020,0013): one document per series, so both are 1.
    write_is(&mut dataset, 0x0020, 0x0011, "1");
    write_is(&mut dataset, 0x0020, 0x0013, "1");
    if let Some(description) = &identity.requested_procedure_description {
        write_lo(&mut dataset, 0x0032, 0x1060, description);
    }
    if let Some(code) = &identity.requested_procedure_code {
        write_workitem_code_sequence(&mut dataset, 0x0032, 0x1064, code);
    }

    // -- Requested-procedure identity (AIW-I Appendix V, Source Copy) --
    if let Some(code) = &identity.scheduled_protocol_code {
        write_workitem_code_sequence(&mut dataset, 0x0040, 0x0008, code);
    }
    if let Some(id) = &identity.requested_procedure_id {
        write_sh(&mut dataset, 0x0040, 0x1001, id);
    }
    if let Some(reason) = &identity.reason_for_requested_procedure {
        write_lo(&mut dataset, 0x0040, 0x1002, reason);
    }
    if let Some(code) = &identity.reason_for_requested_procedure_code {
        write_workitem_code_sequence(&mut dataset, 0x0040, 0x100A, code);
    }

    // -- Observer context, at the DATASET level -----------------------
    // This is what lets the content tree carry no context items: TID 1001's
    // rows are all "Required if ... not inherited" and inherit from these.
    write_sequence_start(&mut dataset, 0x0040, 0xA078);
    write_item_start(&mut dataset);
    // The item's two Type-1C attributes (PS3.3 C.17.8.5). A DEVICE entry
    // carries no person name, so (0040,A075) is written empty rather than
    // filled with a lie; (0040,A088) names the organization that owns it.
    write_pn(&mut dataset, 0x0040, 0xA075, "");
    write_lo(&mut dataset, 0x0040, 0xA088, MANUFACTURER);
    write_code_sequence(&mut dataset, 0x0040, 0xA123, CODE_DEVICE_OBSERVER);
    write_item_end(&mut dataset);
    write_sequence_end(&mut dataset);

    // -- Type 2 sequences with nothing to reference -------------------
    write_sequence_start(&mut dataset, 0x0040, 0xA372);
    write_sequence_end(&mut dataset);

    // -- SR Document General ------------------------------------------
    // (0040,A375) Current Requested Procedure Evidence Sequence sorts before
    // (0040,A491) Completion Flag, so the evidence block is written first.
    write_evidence(&mut dataset, identity);
    write_cs(&mut dataset, 0x0040, 0xA491, "COMPLETE");
    write_cs(&mut dataset, 0x0040, 0xA493, "UNVERIFIED");

    // -- SR Document Content ------------------------------------------
    let content = content_item(identity, profile, measurements);
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

/// (0040,A375) Current Requested Procedure Evidence Sequence.
fn write_evidence(out: &mut Vec<u8>, identity: &MeasurementsIdentity) {
    write_sequence_start(out, 0x0040, 0xA375);
    write_item_start(out);
    write_ui(out, 0x0020, 0x000D, &identity.study_uid);
    write_sequence_start(out, 0x0008, 0x1115);
    for (series_uid, images) in group_images_by_series(&identity.images) {
        write_item_start(out);
        write_ui(out, 0x0020, 0x000E, series_uid);
        write_sequence_start(out, 0x0008, 0x1199);
        for image in images {
            write_item_start(out);
            write_ui(out, 0x0008, 0x1150, &image.sop_class_uid);
            write_ui(out, 0x0008, 0x1155, &image.sop_instance_uid);
            write_item_end(out);
        }
        write_sequence_end(out);
        write_item_end(out);
    }
    write_sequence_end(out);
    write_item_end(out);
    write_sequence_end(out);
}

/// Group the analyzed images by series, preserving first-seen order.
fn group_images_by_series(images: &[MeasurementsImage]) -> Vec<(&str, Vec<&MeasurementsImage>)> {
    let mut out: Vec<(&str, Vec<&MeasurementsImage>)> = Vec::new();
    for image in images {
        match out.iter_mut().find(|slot| slot.0 == image.series_uid) {
            Some(slot) => slot.1.push(image),
            None => out.push((image.series_uid.as_str(), vec![image])),
        }
    }
    out
}

/// The root CONTAINER's body: the document title, the heading container with
/// its algorithm context, and one Measurement Group per finding.
fn content_item(
    identity: &MeasurementsIdentity,
    profile: &MeasurementsProfile,
    measurements: &Measurements,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(1024);
    // (0040,A040)/(0040,A043): the root carries no RelationshipType - it has
    // no source content item to relate to (PS3.3 C.17.3.1.2).
    write_cs(&mut body, 0x0040, 0xA040, "CONTAINER");
    write_code_sequence(&mut body, 0x0040, 0xA043, CODE_ROOT);
    // (0040,A050) Continuity of Content, Type 1: SEPARATE, because each
    // finding is a whole measurement and not a fragment of one.
    write_cs(&mut body, 0x0040, 0xA050, "SEPARATE");
    // (0040,A504) Content Template Sequence: the template this document
    // follows, from the rule that produced it.
    write_template(&mut body, profile);

    write_sequence_start(&mut body, 0x0040, 0xA730);

    // TID 1500 R4: the procedure the report is about, when the rule titles
    // it. A CODE item, so it carries (0040,A168) rather than a text value.
    //
    // The concept is the LOCAL title, not DCM 121058. That code means
    // "Procedure reported" - a label for the whole report - and hanging it on
    // a site's own title says the site's string is the standard's procedure
    // name, which is a claim about a vocabulary it is not part of. The title
    // travels under the deployment's own scheme instead.
    //
    // It PRECEDES the heading in this sequence, so it decides the item
    // number the heading lands on - which is what a region's (0040,DB73)
    // has to name. Counted here rather than assumed, because the row is
    // conditional in exactly the same way the algorithm version row is.
    let mut rows_before_heading = 0usize;
    if let Some(title) = &profile.title {
        let item = title_item(
            "HAS CONCEPT MOD",
            CODE_LOCAL_TITLE,
            (title.as_str(), "99CLARUS", title.as_str()),
        );
        write_defined_item(&mut body, &item);
        rows_before_heading += 1;
    }

    // The heading container, with TID 4019 once per report.
    let heading = heading_container(identity, measurements, rows_before_heading + 1);
    write_defined_item(&mut body, &heading);

    write_sequence_end(&mut body);
    body
}

/// The (126010) heading container's body, including TID 4019 and every
/// Measurement Group.
///
/// `item_number` is which child of the root this container is, 1-based. The
/// caller counts it, because the title row before it is conditional and a
/// region's identifier has to name this container rather than whatever
/// happens to come first.
fn heading_container(
    identity: &MeasurementsIdentity,
    measurements: &Measurements,
    item_number: usize,
) -> Vec<u8> {
    let source = identity.images.first();

    let mut body = Vec::with_capacity(1024);
    write_cs(&mut body, 0x0040, 0xA040, "CONTAINER");
    write_cs(&mut body, 0x0040, 0xA010, "CONTAINS");
    write_code_sequence(&mut body, 0x0040, 0xA043, CODE_IMAGING_MEASUREMENTS);
    write_cs(&mut body, 0x0040, 0xA050, "SEPARATE");

    write_sequence_start(&mut body, 0x0040, 0xA730);

    // TID 1500 R6b: the algorithm, once for the whole report. Written as a
    // TEXT item under HAS CONCEPT MOD, per TID 4019 R1/R2.
    //
    // These rows PRECEDE the groups in this sequence, so they decide the
    // item number each group lands on - which is what a SCOORD's
    // (0040,DB73) has to name. The count is measured here rather than
    // assumed, because the version row is conditional.
    let mut rows_before_groups = 0usize;
    let name = text_item(
        "HAS CONCEPT MOD",
        CODE_ALGORITHM_NAME,
        &identity.algorithm_name,
    );
    write_defined_item(&mut body, &name);
    rows_before_groups += 1;
    if let Some(version) = &identity.algorithm_version {
        let item = text_item("HAS CONCEPT MOD", CODE_ALGORITHM_VERSION, version);
        write_defined_item(&mut body, &item);
        rows_before_groups += 1;
    }

    // One Measurement Group per finding.
    for (position, group) in measurements.groups.iter().enumerate() {
        // (0040,DB73) Reference Content Item Identifier: the 1-based item
        // path from the ROOT content item down to the item it names
        // (PS3.3 C.18.6.1.1). A SCOORD inside this group names this
        // group's IMAGE item, so the path is
        //   [root_child, rows_before_groups + position + 1, 1]
        // - `root_child` is the heading container's own item number under
        // the root, the heading's child at that index is this group, and
        // the group's first child is the IMAGE. Both numbers are built here
        // from what was actually written, which is what keeps the path true
        // when the title row or the conditional version row appears or
        // disappears.
        let identifier = ContentPath {
            root_child: item_number,
            group: rows_before_groups + position + 1,
            image: 1,
        };
        let item = measurement_group(source, &measurements.unit, group, identifier);
        write_defined_item(&mut body, &item);
    }

    write_sequence_end(&mut body);
    body
}

/// Where a content item sits in the tree, as the item numbers a
/// (0040,DB73) Reference Content Item Identifier carries.
///
/// Each field is 1-based and counts the item at that DEPTH under its
/// parent. Naming three levels is enough for the one reference this writer
/// makes - a SCOORD pointing at its group's IMAGE - and keeping the shape
/// explicit means the numbers are not three loose integers at the call
/// site.
#[derive(Debug, Clone, Copy)]
struct ContentPath {
    /// Which child of the root this region's GROUP hangs under, 1-based.
    ///
    /// It is the heading container's own item number, which is 1 when the
    /// report carries no title and 2 when it does: the TID 1500 R4 title
    /// row precedes the heading.
    root_child: usize,
    /// Which child of the heading (1 + the TID 4019 rows + the group).
    group: usize,
    /// Which child of the group (1 = the IMAGE item).
    ///
    /// A SCOORD names this item both on itself and on its `SELECTED FROM
    /// IMAGE` child; see [`scord_item`] for why the copy exists.
    image: usize,
}

impl ContentPath {
    /// The identifier as (0040,DB73) stores it: UL values, **highest level
    /// first**.
    ///
    /// PS3.3 C.18.6.1.1 defines the value as the item numbers read from the
    /// root DOWN, and the order is not cosmetic: a reader resolves the path
    /// by descending its values in order, so reversing them names a
    /// different item - or no item at all.
    fn as_uls(self) -> [u32; 3] {
        [
            u32::try_from(self.root_child).unwrap_or(1),
            u32::try_from(self.group).unwrap_or(1),
            u32::try_from(self.image).unwrap_or(1),
        ]
    }
}

/// One (125007, DCM, "Measurement Group") container: the source image and
/// every NUM for one finding.
///
/// The group's children are ordered IMAGE first, so that the item number a
/// region's identifier names (child 1) is fixed regardless of how many
/// measurements the group carries.
fn measurement_group(
    source: Option<&MeasurementsImage>,
    unit: &Code,
    group: &crate::abi::schema::MeasurementGroup,
    identifier: ContentPath,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(512);
    write_cs(&mut body, 0x0040, 0xA040, "CONTAINER");
    write_cs(&mut body, 0x0040, 0xA010, "CONTAINS");
    write_code_sequence(&mut body, 0x0040, 0xA043, CODE_MEASUREMENT_GROUP);
    write_cs(&mut body, 0x0040, 0xA050, "SEPARATE");

    write_sequence_start(&mut body, 0x0040, 0xA730);

    // TID 1501 R10b: the analyzed image. One per group, and every group
    // points at the same image because a whole-image score is about it.
    if let Some(image) = source {
        let item = image_item("CONTAINS", &image.sop_class_uid, &image.sop_instance_uid);
        write_defined_item(&mut body, &item);
    }

    // TID 1501 R10 / TID 300 R1: one NUM per measurement, with the finding
    // as a HAS CONCEPT MOD child of the NUM (TID 301 R2). When the processor
    // reported the class's own operating threshold, it travels as its own
    // NUM under 99CLARUS - the threshold is the model's data, not ours.
    //
    // A measurement that carries a region gets a SCOORD item BESIDE its
    // NUM, not instead of it: TID 1500's Measurement Group requirement for
    // a measurement is the NUM, and a region without a number is a shape
    // with nothing to say. That is why the SCOORD is emitted here, after
    // the NUM it belongs to, rather than as a replacement. The region is
    // TID 1501 R10c/R10d - a SCOORD carrying a SELECTED FROM IMAGE child.
    for measurement in &group.measurements {
        let concept = measurement
            .concept
            .clone()
            .unwrap_or_else(crate::abi::schema::default_measurement_concept);
        let item = num_item(
            unit,
            (&concept.code, &concept.scheme, &concept.meaning),
            measurement.value,
            &group.finding,
        );
        write_defined_item(&mut body, &item);
        if let Some(threshold) = measurement.threshold {
            let item = num_item(unit, CODE_THRESHOLD, threshold, &group.finding);
            write_defined_item(&mut body, &item);
        }
        if let Some(region) = &measurement.region {
            let item = scord_item(region, identifier, source);
            write_defined_item(&mut body, &item);
        }
    }

    write_sequence_end(&mut body);
    body
}

/// One SCOORD item: a traced region of the source frame.
///
/// Shape is TID 1501 R10c/10d, over PS3.3 C.18.6:
///
/// ```text
/// CONTAINS SCOORD                                       TID 1501 R10c
///   (0040,DB73) Referenced Content Item Identifier         -> the IMAGE
///   (0070,0023) Graphic Type   = POLYGON | POLYLINE
///   (0070,0022) Graphic Data   = 2 FL per point
///   SELECTED FROM IMAGE                                 TID 1501 R10d
///     (0040,DB73) Referenced Content Item Identifier       -> that IMAGE
///     (0008,1199) Referenced SOP Sequence                  -> the instance
/// ```
///
/// The relationship split is TID 1501's: the item's OWN (0040,A010) is
/// `CONTAINS` - the group contains the region (R10c) - and the image is a
/// CHILD under `SELECTED FROM` (R10d). PS3.3 C.18.6 makes that second
/// relationship MANDATORY: "An SCOORD Content Item shall always be the
/// Source Content Item of one or more SELECTED FROM Relationships with
/// IMAGE Target Content Items". It is also the edge a Weasis reader
/// follows.
///
/// The identifier is written ON THE SCOORD ONLY, not on its child:
///
/// - PS3.3 C.18.6.1.1 defines (0040,DB73) as naming the IMAGE the
///   identifier is on, and a SCOORD that carries one is self-describing to
///   a reader that walks the tree. The path is `[root_child, group, 1]`,
///   the conformant form starting at the root's children.
/// - Weasis 4.7.x does NOT read the SCOORD's own identifier: SRReader's
///   "SCOORD" branch builds a graphic only inside a loop over the item's
///   CHILDREN, and it keys its image map by ITS OWN node levels, which
///   include the root ("1.1.G.1"). A conformant identifier ("1.G.1") can
///   never equal one of those keys, so a child that CARRIED the identifier
///   would be registered as a reference-less graphics entry: the outline
///   would attach to an entry that can never open, and the click would
///   die silently. WITHOUT the identifier the child falls back to being
///   keyed by the SCOORD's own node level with the child's Referenced SOP
///   Sequence - the entry the click opens and the layer draws from.
///
/// The child therefore carries (0008,1199) only; the mandatory part is the
/// SELECTED FROM relationship and the IMAGE target, not the identifier.
///
/// The points are written exactly as the ABI carried them - source PIXELS,
/// `[column, row]` - with no conversion to patient coordinates. clinfer
/// stamps the DICOM geometry from the source instances, so a conversion
/// here would be a second, unverifiable copy of the same transformation.
fn scord_item(
    region: &Region,
    identifier: ContentPath,
    source: Option<&MeasurementsImage>,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(256 + region.points.len() * 8);
    write_cs(&mut body, 0x0040, 0xA040, "SCOORD");
    // (0040,A010) names the relationship in which THIS item is the target:
    // the group CONTAINS the region (TID 1501 R10c). The SELECTED FROM
    // relationship the region has with its image is carried ON THE CHILD
    // below - the attribute on a child names the child's own source.
    write_cs(&mut body, 0x0040, 0xA010, "CONTAINS");
    // (0040,A043) Concept Name Code Sequence: TID 1501 R10c names this row
    // by a parameter, and an image region is what the row IS. The finding
    // the region depicts is already named by the group's NUM, so repeating
    // it here would say it twice and name the shape instead of its meaning.
    write_code_sequence(&mut body, 0x0040, 0xA043, CODE_IMAGE_REGION);
    write_referenced_item_identifier(&mut body, identifier);
    write_cs(&mut body, 0x0070, 0x0023, "POLYLINE");
    write_graphic_data(&mut body, &region.points);

    // TID 1501 R10d: the image this region was selected from, as a child.
    //
    // The child is a CONTENT ITEM, so it travels inside (0040,A730)
    // ContentSequence like every other one. Writing the item delimiter
    // directly into this body - as this writer first did - puts a bare
    // (FFFE,E000) where a reader expects an element tag, and a strict
    // parser reports an unknown VR of NONE and abandons the dataset.
    if let Some(image) = source {
        let item = selected_from_image(&image.sop_class_uid, &image.sop_instance_uid);
        write_sequence_start(&mut body, 0x0040, 0xA730);
        write_defined_item(&mut body, &item);
        write_sequence_end(&mut body);
    }
    body
}

/// The `SELECTED FROM IMAGE` child of a SCOORD item (TID 1501 R10d).
///
/// It carries the image reference ONLY - no (0040,DB73). See
/// [`scord_item`] for why the identifier must stay on the SCOORD: a child
/// identifier can never match the node levels Weasis keys its image map
/// by, and its presence is what turns the clickable link into a dead one.
fn selected_from_image(class_uid: &str, instance_uid: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(224);
    write_cs(&mut body, 0x0040, 0xA040, "IMAGE");
    write_cs(&mut body, 0x0040, 0xA010, "SELECTED FROM");
    write_code_sequence(&mut body, 0x0040, 0xA043, CODE_SOURCE_OF_MEASUREMENT);
    // (0008,1199) Referenced SOP Sequence, one item.
    write_sequence_start(&mut body, 0x0008, 0x1199);
    write_item_start(&mut body);
    write_ui(&mut body, 0x0008, 0x1150, class_uid);
    write_ui(&mut body, 0x0008, 0x1155, instance_uid);
    write_item_end(&mut body);
    write_sequence_end(&mut body);
    body
}

/// (0040,DB73) Referenced Content Item Identifier: a path of UL values.
///
/// The path is written HIGHEST LEVEL FIRST, which is the order PS3.3
/// C.18.6.1.1 defines and the order a reader descends. The element is VR UL
/// with one value per level, so the header's length is `4 * levels`.
fn write_referenced_item_identifier(out: &mut Vec<u8>, identifier: ContentPath) {
    let levels = identifier.as_uls();
    crate::pr::write_explicit_header(out, 0x0040, 0xDB73, b"UL", levels.len() * 4);
    for level in levels {
        out.extend_from_slice(&level.to_le_bytes());
    }
}

/// (0070,0022) Graphic Data: FL, two values (`column`, `row`) per point.
///
/// The FL values are written as `f32`, because that is the VR's width: a
/// point kept as `f64` would make the element twice its declared size and
/// desynchronise every reader after it. A pixel coordinate far beyond
/// `f32`'s exact integer range (2^24) would lose precision, which is
/// unreachable for a frame and is not silently repaired here.
fn write_graphic_data(out: &mut Vec<u8>, points: &[[f64; 2]]) {
    crate::pr::write_explicit_header(out, 0x0070, 0x0022, b"FL", points.len() * 8);
    for point in points {
        out.extend_from_slice(&(point[0] as f32).to_le_bytes());
        out.extend_from_slice(&(point[1] as f32).to_le_bytes());
    }
}

/// One NUM item: the value, its unit, and the finding it is about.
fn num_item(unit: &Code, concept: (&str, &str, &str), value: f64, finding: &Code) -> Vec<u8> {
    let mut body = Vec::with_capacity(384);
    write_cs(&mut body, 0x0040, 0xA040, "NUM");
    write_cs(&mut body, 0x0040, 0xA010, "CONTAINS");
    write_code_sequence(&mut body, 0x0040, 0xA043, (concept.0, concept.1, concept.2));
    // (0040,A300) MeasuredValueSequence: a NUM's value and its unit live
    // TOGETHER inside this one sequence item (PS3.3 C.18.1, TID 300 R1
    // "NUM $Measurement ... UNITS = $Units"). The value goes into
    // (0040,A30A) Numeric Value (VR DS for a decimal); (0040,A160) Text
    // Value is VR UT and belongs to TEXT items, not to a NUM's measured
    // value. Writing (0040,A160) and (0040,08EA) directly on the NUM
    // item - as this writer first did - produces an object pydicom reads
    // with no MeasuredValueSequence at all, so the number is not where
    // any reader looks for it.
    write_sequence_start(&mut body, 0x0040, 0xA300);
    let mut measured = Vec::with_capacity(160);
    // Ascending tag order within the item: (0040,08EA) before (0040,A30A).
    write_code_sequence(
        &mut measured,
        0x0040,
        0x08EA,
        (&unit.code, &unit.scheme, &unit.meaning),
    );
    write_ds(&mut measured, 0x0040, 0xA30A, &format_ds(value));
    write_defined_item(&mut body, &measured);
    write_sequence_end(&mut body);

    write_sequence_start(&mut body, 0x0040, 0xA730);
    // TID 301 R2: the finding, as a post-coordinated modifier of the NUM.
    let modifier = code_item(
        "HAS CONCEPT MOD",
        CODE_FINDING,
        (&finding.code, &finding.scheme, &finding.meaning),
    );
    write_defined_item(&mut body, &modifier);
    write_sequence_end(&mut body);
    body
}

/// A CODE content item's body.
fn code_item(
    relationship: &str,
    concept: (&str, &str, &str),
    value: (&str, &str, &str),
) -> Vec<u8> {
    let mut body = Vec::with_capacity(192);
    write_cs(&mut body, 0x0040, 0xA040, "CODE");
    write_cs(&mut body, 0x0040, 0xA010, relationship);
    write_code_sequence(&mut body, 0x0040, 0xA043, concept);
    write_code_sequence(&mut body, 0x0040, 0xA168, value);
    body
}

/// A CODE content item whose VALUE carries free text, so its code value goes
/// into (0008,0119) Long Code Value instead of (0008,0100) Code Value.
///
/// PS3.3 Table 8.8-1: (0008,0100) is VR SH, capped at 16 characters, and it
/// holds the identifier a vocabulary defines - not a deployment's free-text
/// title. A longer title has to live in (0008,0119) (VR UC, no short limit) or
/// in (0008,0104) Code Meaning, or it is either truncated on the wire or
/// written with a length its VR cannot declare. The concept row keeps the
/// ordinary shape, because its code IS a short identifier.
fn title_item(
    relationship: &str,
    concept: (&str, &str, &str),
    value: (&str, &str, &str),
) -> Vec<u8> {
    let mut body = Vec::with_capacity(256 + value.0.len());
    write_cs(&mut body, 0x0040, 0xA040, "CODE");
    write_cs(&mut body, 0x0040, 0xA010, relationship);
    write_code_sequence(&mut body, 0x0040, 0xA043, concept);
    write_long_code_sequence(&mut body, 0x0040, 0xA168, value);
    body
}

/// A code sequence whose (0008,0100) slot holds a long value, written to the
/// designated alternate element (0008,0119) instead.
fn write_long_code_sequence(out: &mut Vec<u8>, group: u16, element: u16, code: (&str, &str, &str)) {
    let mut body = Vec::with_capacity(96);
    // Ascending tag order within the item, as everywhere else.
    write_uc(&mut body, 0x0008, 0x0119, code.0);
    write_lo(&mut body, 0x0008, 0x0102, code.1);
    write_lo(&mut body, 0x0008, 0x0104, code.2);
    write_sequence_start(out, group, element);
    write_defined_item(out, &body);
    write_sequence_end(out);
}

/// A TEXT content item's body, used for the TID 4019 algorithm rows.
fn text_item(relationship: &str, concept: (&str, &str, &str), value: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(160 + value.len());
    write_cs(&mut body, 0x0040, 0xA040, "TEXT");
    write_cs(&mut body, 0x0040, 0xA010, relationship);
    write_code_sequence(&mut body, 0x0040, 0xA043, concept);
    // (0040,A160) is UT for a TEXT item.
    write_ut(&mut body, value);
    body
}

/// An IMAGE content item's body: the analyzed image reference.
///
/// The reference travels in (0008,1199) Referenced SOP Sequence. That is
/// the sequence Weasis reads for IMAGE items: SRReader.getReferencedImage
/// looks only at ReferencedSOPSequence, and with (0008,1140) Referenced
/// Image Sequence the item renders as a bare label with nothing to click.
fn image_item(relationship: &str, class_uid: &str, instance_uid: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(192);
    write_cs(&mut body, 0x0040, 0xA040, "IMAGE");
    write_cs(&mut body, 0x0040, 0xA010, relationship);
    write_code_sequence(&mut body, 0x0040, 0xA043, CODE_SOURCE_OF_MEASUREMENT);
    // (0008,1199) Referenced SOP Sequence, one item.
    write_sequence_start(&mut body, 0x0008, 0x1199);
    write_item_start(&mut body);
    write_ui(&mut body, 0x0008, 0x1150, class_uid);
    write_ui(&mut body, 0x0008, 0x1155, instance_uid);
    write_item_end(&mut body);
    write_sequence_end(&mut body);
    body
}

/// A TEXT item's (0040,A160) value: VR UT, a long VR.
fn write_ut(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    let pad = usize::from(!bytes.len().is_multiple_of(2));
    crate::pr::write_explicit_header(out, 0x0040, 0xA160, b"UT", bytes.len() + pad);
    out.extend_from_slice(bytes);
    if pad == 1 {
        out.push(b' ');
    }
}

/// (0040,A504) ContentTemplateSequence, from the rule's template id.
fn write_template(out: &mut Vec<u8>, profile: &MeasurementsProfile) {
    write_sequence_start(out, 0x0040, 0xA504);
    let mut item = Vec::with_capacity(32);
    write_cs(&mut item, 0x0008, 0x0105, profile.template_id.designator());
    write_cs(
        &mut item,
        0x0040,
        0xDB00,
        profile.template_id.template_number(),
    );
    write_defined_item(out, &item);
    write_sequence_end(out);
}

/// A NUM value as a DS string.
///
/// DS is a Decimal String of at most sixteen characters including a sign,
/// a decimal point and an exponent (PS3.5 Table 6.2-1), so the rendering
/// cannot be fixed at six decimals: a sigmoid output of 1e-9 would round
/// to `0.000000`, and 1e300 would spend 300 characters the VR does not
/// have. `pr`'s renderer picks the shortest legal form, which keeps every
/// value in the element and keeps a probability's small tail readable.
fn format_ds(value: f64) -> String {
    crate::pr::ds_text(value, 6, 6)
}

/// One `FFFE,E000` item with a DEFINED length.
///
/// Items are defined-length while the sequences around them are
/// undefined-length with delimiters. That mixture is legal (PS3.5 7.5.1/7.5.2)
/// and is what a writer that streams sequences but builds items locally
/// produces: `pr`'s code-sequence primitive writes the sentinel, and each
/// item body here is a buffer whose length is known. pydicom reads the
/// result exactly as intended, which is the check that matters.
fn write_defined_item(out: &mut Vec<u8>, body: &[u8]) {
    out.extend_from_slice(&0xFFFEu16.to_le_bytes());
    out.extend_from_slice(&0xE000u16.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
}

/// (0008,0100)/(0008,0102)/(0008,0104) as one item of a code sequence.
fn write_code_sequence(out: &mut Vec<u8>, group: u16, element: u16, code: (&str, &str, &str)) {
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

/// (0002,xxxx) group.
fn write_file_meta(out: &mut Vec<u8>, identity: &MeasurementsIdentity) {
    let meta = file_meta_bytes(identity);
    crate::pr::write_explicit_header(out, 0x0002, 0x0000, b"UL", 4);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
}

fn file_meta_bytes(identity: &MeasurementsIdentity) -> Vec<u8> {
    let mut meta = Vec::with_capacity(256);
    crate::pr::write_string(&mut meta, 0x0002, 0x0001, b"OB", "01");
    crate::pr::write_string(&mut meta, 0x0002, 0x0002, b"UI", ENHANCED_SR_SOP_CLASS_UID);
    crate::pr::write_string(&mut meta, 0x0002, 0x0003, b"UI", &identity.sop_instance_uid);
    crate::pr::write_string(&mut meta, 0x0002, 0x0010, b"UI", EXPLICIT_VR_LE);
    crate::pr::write_string(&mut meta, 0x0002, 0x0012, b"UI", IMPLEMENTATION_CLASS_UID);
    crate::pr::write_string(
        &mut meta,
        0x0002,
        0x0013,
        b"SH",
        &crate::sr::implementation_version_name(),
    );
    meta
}
