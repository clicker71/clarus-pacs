# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  DIMSE protocol mappings and helpers.

Defines QIDO-RS <-> DICOM tag mappings, C-FIND keyword conversion,
N-ACTION request/response builders, JSON-to-Dataset converters,
and shared DIMSE status <-> human-readable text (PS3.7 sec.C.4).
"""

from pydicom import Dataset
from pydicom.dataelem import DataElement
from pydicom.multival import MultiValue
from pydicom.sequence import Sequence
from pydicom.uid import (
    ExplicitVRLittleEndian,
    ImplicitVRLittleEndian,
    RLELossless,
)

# ---------------------------------------------------------------------------
# Shared by main.py and cmove.py - defined ONCE here so the bundled output
# cannot shadow one definition with another.
# 1. EVRLE FIRST: with 4.80 first, a native-only viewer whose preferred
#    transfer syntax is EVRLE rejected the (DX, 4.70) fallback association
#    and every image went through transcode; with EVRLE first it accepted
#    the fallback (field measurement: passthrough=5, transcoded=0,
#    refetched=0, about 1 s). Wire note: negotiation is acceptor-driven -
#    a standard DIMSE acceptor picks the first TS of ITS OWN supported list
#    that appears in the proposal (pynetdicom presentation.py
#    negotiate_as_acceptor), so a 4.80-capable viewer still negotiates 4.80
#    on the main association and stored 4.80 instances send as-is,
#    unaffected by the order. The proposal order matters only for
#    order-sensitive viewers.
#    Some viewers reject the (CT, 4.80) fallback in BOTH orders (a limit of
#    their JPEG-LS decoder) - the bridge refetches such instances, and the
#    per-job rejection cache (handlers/cmove.py) prevents repeated doomed
#    association attempts.
# 2. Trim: JPEGLossless (4.57) and JPEGBaseline8Bit (4.50) are never
#    stored and never produced by the server, and no viewer ever selected
#    them; a viewer that did would still get images via the per-(class, TS)
#    fallback association.
#    Keep 4.70 (the stored corpus uses it).
# ---------------------------------------------------------------------------
JPEGLS_LOSSLESS_TS = "1.2.840.10008.1.2.4.80"
# Canonical proposal order for C-MOVE outbound multi-context negotiation
# Uncompressed family first, then compressed in
# the DICOM transfer-syntax taxonomy order - RLE, JPEG lossless, JPEG-LS,
# JPEG lossy, JPEG 2000, MPEG. Deterministic output is the goal (the
# standard, PS3.8 7.1.1.13, is silent on proposer order). EVRLE and IVRLE
# are NEVER dropped during budget trimming (they are the refetch targets).
_CANONICAL_TS = [
    ExplicitVRLittleEndian,  # uncompressed
    ImplicitVRLittleEndian,  # uncompressed
    "1.2.840.10008.1.2.5",  # RLE Lossless
    "1.2.840.10008.1.2.4.57",  # JPEG Lossless (Process 14 SV1 non-hierarchical)
    "1.2.840.10008.1.2.4.70",  # JPEG Lossless SV1 first-order
    JPEGLS_LOSSLESS_TS,  # 4.80 JPEG-LS Lossless
    "1.2.840.10008.1.2.4.81",  # 4.81 JPEG-LS Near-Lossless
    "1.2.840.10008.1.2.4.50",  # 4.50 JPEG Baseline
    "1.2.840.10008.1.2.4.51",  # 4.51 JPEG Extended
    "1.2.840.10008.1.2.4.90",  # 4.90 JPEG 2000 Lossless
    "1.2.840.10008.1.2.4.91",  # 4.91 JPEG 2000
    "1.2.840.10008.1.2.4.92",  # 4.92 JPEG 2000 Part 2
]


def canonical_ts_sort_key(ts_uid):
    """Ordering key for transfer-syntax UIDs in canonical proposer order.

    Unknown UIDs sort to the END (kept, just after every known TS) so a
    stored TS we don't recognise still gets proposed after the standard
    family, never dropped silently. Deterministic (nginx-way).
    """
    try:
        return _CANONICAL_TS.index(ts_uid)
    except ValueError:
        return len(_CANONICAL_TS)


IMAGE_TS = [
    ExplicitVRLittleEndian,  # EVRLE first (31.08 owner experiment, accepted)
    JPEGLS_LOSSLESS_TS,  # 4.80 (T87) - still negotiated by 4.80-capable viewers (acceptor-driven)
    ImplicitVRLittleEndian,  # Weasis SCP needs this, but EVRLE preferred
    "1.2.840.10008.1.2.4.70",  # P14.1 (JPEG Lossless SV1) - stored ELDICOM corpus
]
NON_IMAGE_TS = [ExplicitVRLittleEndian, ImplicitVRLittleEndian]

# ---------------------------------------------------------------------------
# Incoming C-STORE: some viewers export LOCAL files as-is in their original
# TS. Offering only IMAGE_TS on the SCP side made such a sender get every
# context REJECTED and report "Total size 0 B" (bridge log: association
# accepted, released with no C-STORE; a probe against a live peer proved the
# rejects). The SCP side has NO fallback mechanism - a rejected context is a
# failed export. The bridge never decodes incoming pixels (store bytes ->
# STOW), so this list mirrors what the SERVER CAN INGEST AS-IS, and nothing
# else. It is deliberately BROADER than the transcode announceable set
# (IMAGE_TS): the negotiation decides what a sender may use, and a syntax the
# server accepts is a context the bridge must not refuse.
#
# The wider entries are not a claim that every one of them is common. They
# close the "sender offers only this" case, which used to be a silent failed
# export:
#   - HTJ2K (4.201/.202/.203) and JPEG 2000 Part 2 (4.92/.93): the server
#     writes header attributes into these instances and decodes them, so the
#     bridge would be refusing bytes its own archive handles.
#   - MPEG family (4.100-.105): the server does not decode video, exactly
#     like 4.51 today - stored and served raw, which is what a sender holding
#     a video object needs. This mirrors the "store+serve raw" case, not a
#     claim of analysis support.
#
# Order: uncompressed first - acceptor-driven negotiation picks the first
# supported match when a sender offers several TS per context - and the
# appended syntaxes keep their ascending UID order after the existing set.
# ---------------------------------------------------------------------------
IMAGE_TS_INCOMING = [
    ExplicitVRLittleEndian,  # native
    ImplicitVRLittleEndian,  # native
    JPEGLS_LOSSLESS_TS,  # 4.80 JPEG-LS Lossless (server CAPS)
    "1.2.840.10008.1.2.4.70",  # 4.70 JPEG Lossless SV1 (server CAPS)
    "1.2.840.10008.1.2.4.57",  # 4.57 JPEG Lossless P14 (server CAPS: JpegLosslessCodec)
    "1.2.840.10008.1.2.4.50",  # 4.50 JPEG Baseline (server CAPS; field US scanner)
    "1.2.840.10008.1.2.4.51",  # 4.51 JPEG Extended (server parses; store+serve raw, no transcode)
    RLELossless,  # 4.1.2.5 RLE (server CAPS; field US)
    "1.2.840.10008.1.2.4.81",  # 4.81 JPEG-LS Near-Lossless (server CAPS)
    "1.2.840.10008.1.2.4.90",  # 4.90 JPEG 2000 Lossless (server CAPS)
    "1.2.840.10008.1.2.4.91",  # 4.91 JPEG 2000 (server CAPS)
    # Appended 21.11 - see the "what the server can ingest as-is" note above.
    "1.2.840.10008.1.2.4.92",  # 4.92 JPEG 2000 Part 2 Multi-component Lossless
    "1.2.840.10008.1.2.4.93",  # 4.93 JPEG 2000 Part 2 Multi-component
    "1.2.840.10008.1.2.4.100",  # 4.100 MPEG2 Main Profile / Main Level
    "1.2.840.10008.1.2.4.101",  # 4.101 MPEG2 Main Profile / High Level
    "1.2.840.10008.1.2.4.102",  # 4.102 MPEG-4 AVC/H.264 High Profile / Level 4.1
    "1.2.840.10008.1.2.4.103",  # 4.103 MPEG-4 AVC/H.264 BD-compatible High / Level 4.1
    "1.2.840.10008.1.2.4.104",  # 4.104 MPEG-4 AVC/H.264 High / Level 4.2 For 2D
    "1.2.840.10008.1.2.4.105",  # 4.105 MPEG-4 AVC/H.264 High / Level 4.2 For 3D
    "1.2.840.10008.1.2.4.201",  # 4.201 HTJ2K Lossless (server decodes)
    "1.2.840.10008.1.2.4.202",  # 4.202 HTJ2K Lossless RPCL (server decodes)
    "1.2.840.10008.1.2.4.203",  # 4.203 HTJ2K Lossy (server decodes)
]


# ---------------------------------------------------------------------------
# Shared: DIMSE status -> human-readable text (PS3.7 sec.C.4)
# Used by C-STORE, C-MOVE, C-FIND, N-ACTION handlers.
# Defined ONCE here to avoid bundle duplicate-definition shadowing.
# ---------------------------------------------------------------------------
_DIMSE_STATUS_TEXT = {
    0x0000: "Success",
    0x0001: "Warning - SOP Class not supported",
    0x0116: "Warning - Attribute Value Out of Range",
    0x0118: "Class-Instance Conflict",
    0xA700: "Out of Resources",
    0xA701: "Out of Resources - Unable to calculate number of matches",
    0xA702: "Out of Resources - Unable to perform sub-operations",
    0xA801: "SOP Class Not Supported",
    0xA900: "Error - Identifier Does Not Match SOP Class",
    0xAA00: "Error - Nonexistent",
    0xB000: "Warning - Sub-operations Complete, One or More Failures",
    0xC000: "Processing Failure",
    0xC001: "Processing Failure - Cannot Understand",
    0xC002: "Processing Failure - Cannot Perform",
    0xFE00: "Cancel - Matching Terminated Due to Cancel Request",
}


def _dimse_status_name(code):
    """Return human-readable DIMSE status string for a status code."""
    if code is None:
        return "None"
    if not isinstance(code, int):
        try:
            code = int(code)
        except (TypeError, ValueError):
            return f"0x{code:04X}" if hasattr(code, "__int__") else str(code)
    text = _DIMSE_STATUS_TEXT.get(code)
    return f"0x{code:04X} ({text})" if text else f"0x{code:04X}"


# SOP Class UIDs for C-FIND dispatching
PATIENT_ROOT_UID = "1.2.840.10008.5.1.4.1.2.1.1"
STUDY_ROOT_UID = "1.2.840.10008.5.1.4.1.2.2.1"
MWL_UID = "1.2.840.10008.5.1.4.31"

# QIDO JSON key -> (DICOM tag group, element, VR, keyword)
#
# DELIBERATE omission: 0008,0005 SpecificCharacterSet. _json_map_to_dataset
# forces ISO_IR 192 (UTF-8) on the dataset BEFORE this map is applied, so that
# Cyrillic PatientName / StudyDescription survives (INC-004). A server study
# row that carries its own charset would REPLACE that value - the row is the
# server's opinion of the bytes it stored, not of the bytes this bridge emits -
# and a Latin-1 declaration makes a Cyrillic value unencodable on the wire.
# The series map below omits the same key for the same reason.
QIDO_KEY_TO_TAG = {
    "00080020": (0x0008, 0x0020, "DA", "StudyDate"),
    "00080030": (0x0008, 0x0030, "TM", "StudyTime"),
    "00080050": (0x0008, 0x0050, "SH", "AccessionNumber"),
    "00080060": (0x0008, 0x0060, "CS", "Modality"),
    "00080061": (0x0008, 0x0061, "CS", "ModalitiesInStudy"),
    "00080090": (0x0008, 0x0090, "PN", "ReferringPhysicianName"),
    "00081030": (0x0008, 0x1030, "LO", "StudyDescription"),
    "00100010": (0x0010, 0x0010, "PN", "PatientName"),
    "00100020": (0x0010, 0x0020, "LO", "PatientID"),
    "00100030": (0x0010, 0x0030, "DA", "PatientBirthDate"),
    "00100040": (0x0010, 0x0040, "CS", "PatientSex"),
    "0020000D": (0x0020, 0x000D, "UI", "StudyInstanceUID"),
    "00200010": (0x0020, 0x0010, "SH", "StudyID"),
    "00201200": (0x0020, 0x1200, "IS", "NumberOfPatientRelatedStudies"),
    "00201202": (0x0020, 0x1202, "IS", "NumberOfPatientRelatedSeries"),
    "00201204": (0x0020, 0x1204, "IS", "NumberOfPatientRelatedInstances"),
    "00201206": (0x0020, 0x1206, "IS", "NumberOfStudyRelatedSeries"),
    "00201208": (0x0020, 0x1208, "IS", "NumberOfStudyRelatedInstances"),
}

# C-FIND Study Root, SERIES level: the query keys the server accepts ONLY
# on the series level. Everything else mapped by C_FIND_TO_QIDO is a
# study-level key (the server rejects series keys on /studies with 400).
_SERIES_ONLY_QIDO_PARAMS = frozenset(
    {
        "SeriesInstanceUID",
        "SeriesNumber",
        "SeriesDate",
        "SeriesTime",
        "SeriesDescription",
    }
)

# C-FIND COMPOSITE OBJECT INSTANCE (IMAGE) level: the query keys the server
# accepts ONLY on its instance routes. `/studies/{s}/series/{se}` answers a
# request carrying any of them with 400 ("unsupported search keys"), so they
# must not travel with the series hop of an IMAGE-level walk.
_INSTANCE_ONLY_QIDO_PARAMS = frozenset(
    {
        "SOPInstanceUID",
        "SOPClassUID",
        "InstanceNumber",
    }
)

# Series-level QIDO JSON key -> (DICOM tag group, element, VR, keyword).
#
# Verified against the LIVE server 31.08 (GET /studies/{study}/series,
# fieldtest 8024) and the server's write_series_json render
# (crates/clarus-core/src/query/json.rs): series rows carry
# SeriesDate (0008,0021), SeriesTime (0008,0031), Modality, StationName
# (0008,1010), SeriesDescription (0008,103E), BodyPartExamined (0018,0015),
# SeriesInstanceUID (0020,000E), SeriesNumber (0020,0011), Laterality
# (0020,0060), NumberOfSeriesRelatedInstances (0020,1209).
#
# DELIBERATE omissions:
# - 0008,0005 SpecificCharacterSet: the series render always emits
#   "ISO_IR 100"; the converter forces ISO_IR 192 (UTF-8) first so
#   Cyrillic PatientName/StationName survives. Mapping it would let the
#   ISO_IR 100 overwrite the UTF-8 charset.
# - PatientName / PatientID / StudyInstanceUID: the server series render
#   does NOT emit them (confirmed live). They are injected from the study
#   row by _json_to_dataset_series.
QIDO_SERIES_KEY_TO_TAG = {
    "00080021": (0x0008, 0x0021, "DA", "SeriesDate"),
    "00080031": (0x0008, 0x0031, "TM", "SeriesTime"),
    "00080060": (0x0008, 0x0060, "CS", "Modality"),
    "00081010": (0x0008, 0x1010, "SH", "StationName"),
    "0008103E": (0x0008, 0x103E, "LO", "SeriesDescription"),
    "00180015": (0x0018, 0x0015, "CS", "BodyPartExamined"),
    "0020000E": (0x0020, 0x000E, "UI", "SeriesInstanceUID"),
    "00200011": (0x0020, 0x0011, "IS", "SeriesNumber"),
    "00200060": (0x0020, 0x0060, "CS", "Laterality"),
    "00201209": (0x0020, 0x1209, "IS", "NumberOfSeriesRelatedInstances"),
}

# Study-level identity attributes injected into a series C-FIND dataset
# from the owning study row (keyword -> tag). The server's series JSON
# carries none of them.
_STUDY_IDENTITY_ATTRS = {
    "PatientName": (0x0010, 0x0010),
    "PatientID": (0x0010, 0x0020),
    "StudyInstanceUID": (0x0020, 0x000D),
}

# COMPOSITE OBJECT INSTANCE (IMAGE) level, PS3.4 Table C.6-4
# (standard/ps3.4/image-level-C.6-4.json): Instance Number (0020,0013) is the
# level's Required key, SOP Instance UID (0008,0018) its Unique key, SOP Class
# UID (0008,0016) optional. The server's instance render carries exactly these
# three plus the Study/Series UIDs it injects from context and
# InstanceAvailability/RetrieveURL, so the map below needs no tag knowledge
# the series map does not already have.
QIDO_INSTANCE_KEY_TO_TAG = {
    "00080016": (0x0008, 0x0016, "UI", "SOPClassUID"),
    "00080018": (0x0008, 0x0018, "UI", "SOPInstanceUID"),
    "00200013": (0x0020, 0x0013, "IS", "InstanceNumber"),
}

# Attributes stored in the series record that an IMAGE row carries as well:
# PS3.4 Table C.6-4 leaves the other Series IE keys optional at this level, and
# a viewer that walked STUDY -> SERIES -> IMAGE expects the series context on
# the instance row. Only keys the server's series render supplies are listed.
_INSTANCE_SERIES_CONTEXT = frozenset(
    {
        "00080060",  # Modality
        "0008103E",  # SeriesDescription
        "00200011",  # SeriesNumber
    }
)


# C-FIND keyword -> QIDO query parameter.
#
# PS3.4 Study Root (C.6.2.1.2) REQUIRED matching keys: StudyDate, StudyTime,
# AccessionNumber, PatientName, PatientID, StudyInstanceUID, StudyID: all
# mapped. Optional keys the server filters on (PS3.18 8.3.3-1) are mapped too,
# incl. ModalitiesInStudy (0008,0061) - Weasis sends this, NOT Modality, in
# Study Root queries. Keys absent from the table are silently dropped, so a
# missing entry here = "match all" server-side (the field modality-filter bug).
C_FIND_TO_QIDO = {
    "PatientName": "PatientName",
    "PatientID": "PatientID",
    "PatientBirthDate": "PatientBirthDate",
    "PatientSex": "PatientSex",
    "StudyDate": "StudyDate",
    "StudyTime": "StudyTime",
    "AccessionNumber": "AccessionNumber",
    "StudyID": "StudyID",
    "StudyInstanceUID": "StudyInstanceUID",
    "Modality": "Modality",
    "ModalitiesInStudy": "ModalitiesInStudy",
    "StudyDescription": "StudyDescription",
    "ReferringPhysicianName": "ReferringPhysicianName",
    "PerformingPhysicianName": "PerformingPhysicianName",
    "NameOfPhysiciansReadingStudy": "NameOfPhysiciansReadingStudy",
    "SeriesInstanceUID": "SeriesInstanceUID",
    "SeriesNumber": "SeriesNumber",
    "SeriesDate": "SeriesDate",
    "SeriesTime": "SeriesTime",
    "SeriesDescription": "SeriesDescription",
    "SOPInstanceUID": "SOPInstanceUID",
    # PS3.4 Table C.6-4: the Composite Object Instance level's matching keys.
    # SOPClassUID is optional there and InstanceNumber is its Required key, so
    # an IMAGE-level request that names them must reach the server as a filter
    # rather than be dropped (= match all).
    "SOPClassUID": "SOPClassUID",
    "InstanceNumber": "InstanceNumber",
}


def _extract_request_keywords(ds):
    """Extract keyword set from a pydicom Dataset (excludes SQ elements)."""
    keywords = set()
    for elem in ds:
        if elem.VR != "SQ" and elem.keyword:
            keywords.add(elem.keyword)
    return keywords


def _iter_mwl_request_elements(ds):
    """Yield the C-FIND MWL identifier elements, top level and SPS item.

    A conformant MWL request carries its SPS matching and return keys INSIDE
    the single item of (0040,0100) Scheduled Procedure Step Sequence - that is
    where (0040,0001) ScheduledStationAETitle and (0040,0009) SPS ID live
    (PS3.4 Table K.6-1). A flat request that puts them at the top level is also
    accepted: older senders do it and the standard's sequence matching is
    satisfied either way.

    Top-level elements come first, so a value stated both ways resolves to the
    top-level one.
    """
    for elem in ds:
        if elem.VR != "SQ":
            yield elem
    for elem in ds:
        if elem.VR != "SQ" or elem.keyword != "ScheduledProcedureStepSequence":
            continue
        items = elem.value or []
        if not items:
            continue
        # Single Item only (Table K.6-1). A sender that breaks the rule gets
        # its first item read rather than an error: the response is one row per
        # SPS, and there is exactly one SPS to answer for.
        for item in items[:1]:
            for sub in item:
                if sub.VR != "SQ":
                    yield sub


def _extract_mwl_request_keywords(ds):
    """Request keywords for an MWL C-FIND: top level PLUS the (0040,0100) item.

    A modality asks for an Optional key by naming it in the identifier, and the
    SPS-level keys it may ask for live inside the sequence. Flat top-level
    keywords are kept too, so both request shapes work.
    """
    keywords = set()
    for elem in _iter_mwl_request_elements(ds):
        if elem.keyword:
            keywords.add(elem.keyword)
    return keywords


def _build_qido_params(ds):
    """Build QIDO-RS query parameters from a pydicom Dataset.

    Maps C-FIND keywords to QIDO query parameters.
    None and empty-after-strip values are skipped for ALL VRs (DICOMweb:
    absent parameter = match all). Field bug 31.08: an empty IS
    (SeriesNumber) decoded by pydicom to None was stringified to "None"
    and sent to the server, which answered 400.
    Non-empty values pass through as-is (server handles DICOM `*`/`?`
    wildcards).
    """
    params = {}
    for elem in ds:
        if elem.VR == "SQ":
            continue
        keyword = elem.keyword
        if keyword in C_FIND_TO_QIDO:
            if elem.value is None:
                continue
            # Multi-valued matching (a MultiValue) is a LIST of alternatives:
            # `str()` on one produces "['CT', 'MR']", which the server reads as
            # a single unmatched value and answers with zero matches. PS3.18
            # 10.6 defines the query syntax as comma-separated, so the
            # alternatives are joined that way.
            if isinstance(elem.value, (list, tuple, MultiValue)):
                val = ",".join(str(item).strip() for item in elem.value if item)
            else:
                val = str(elem.value).strip()
            # Skip empty values  -  DICOMweb convention: absent = match all
            if val:
                params[C_FIND_TO_QIDO[keyword]] = val
    return params


def _split_qido_params(ds):
    """Split a C-FIND identifier into (study_params, series_params).

    Server contract (qido_pure.rs foreign_keys, 31.08):
    - GET /studies rejects series-only keys with 400;
    - GET /studies/{study}/series accepts ONLY series keys + Modality
      (PatientName/PatientID/StudyInstanceUID are foreign there).
    Modality is legal at BOTH levels; it goes to the SERIES step only:
    the study-level Modality filter matches the study's first-series
    modality (documented server deviation) and could wrongly exclude a
    study whose later series carries the modality.
    """
    all_params = _build_qido_params(ds)
    study_params = {}
    series_params = {}
    for key, val in all_params.items():
        if key in _SERIES_ONLY_QIDO_PARAMS or key == "Modality":
            series_params[key] = val
        else:
            study_params[key] = val
    return study_params, series_params


def _split_instance_qido_params(ds):
    """Split an IMAGE-level C-FIND identifier into study/series/instance params.

    Returns `(study_params, series_params, instance_params)` for the three QIDO
    hops of an IMAGE-level query (PS3.4 C.4.1.3.1.1: match down the hierarchy
    from the top level to the level named by QueryRetrieveLevel).

    Three keys need care because the server scopes them to particular routes
    (qido_pure.rs `parse_and_validate`):

    - `Modality` is legal at the instance route but is a SERIES key in this
      query (a C-FIND IMAGE request states it to narrow the walk, and the
      series step applies it), so it stays on the series hop.
    - `SeriesInstanceUID` is a series key everywhere except the instance
      route, where it is foreign; it narrows the series hop and the instance
      rows are already scoped to that one series.
    - `SOPInstanceUID`, `SOPClassUID` and `InstanceNumber` are instance keys
      the series route rejects with 400, so they never travel with the series
      parameters.
    """
    all_params = _build_qido_params(ds)
    study_params = {}
    series_params = {}
    instance_params = {}
    for key, val in all_params.items():
        if key in _INSTANCE_ONLY_QIDO_PARAMS:
            instance_params[key] = val
        elif key in _SERIES_ONLY_QIDO_PARAMS or key == "Modality":
            series_params[key] = val
        else:
            study_params[key] = val
    return study_params, series_params, instance_params


# MWL (Modality Worklist) <-> UPS-RS workitem DICOMweb JSON keys.
#
# PS3.4 2026d Table K.6-1: the Modality Worklist Information Model is a FLAT,
# NON-OBJECT model - it has no SOP Common attributes, so SOP Instance UID
# (0008,0018) has no place in a response. The modality's identity for a
# Scheduled Procedure Step is (0040,0009) SPS ID plus (0040,1001) Requested
# Procedure ID, and the SPS attributes themselves live INSIDE the single item
# of (0040,0100) Scheduled Procedure Step Sequence:
#
#   "The Scheduled Procedure Step Sequence shall contain only a single Item."
#
# RETURN keys are emitted whenever the source has a value, whether the modality
# asked or not. OPTIONAL keys are emitted ONLY when the modality requested them
# (PS3.4 C.4.1.1.3.2), which is how a modality says "give me this key".
#
# Sources are the server's workitem render (handlers/ups/json.rs). The keys the
# server does not render simply produce nothing - an absent Return key, not a
# fabricated one. This is a documented VENDOR mapping: no normative DICOM or
# IHE mapping from UPS workitems to a DIMSE MWL response exists, so the derived
# identifiers below are the bridge's own, declared in
# dimse-bridge-conformance-statement.md.

# Top level. PatientName/PatientID are Return keys (Type 1) at identifier
# level; the rest are Optional and request-gated.
MWL_RETURN_TOP = {
    "00100010": (0x0010, 0x0010, "PN", "PatientName"),
    "00100020": (0x0010, 0x0020, "LO", "PatientID"),
}

MWL_OPTIONAL_TOP = {
    "00080050": (0x0008, 0x0050, "SH", "AccessionNumber"),
    "0020000D": (0x0020, 0x000D, "UI", "StudyInstanceUID"),
    "00321032": (0x0032, 0x1032, "PN", "RequestingPhysician"),
}

# The two Optional top-level keys whose UPS source moved into a sequence: the
# study and the accession are carried by (0040,A370) Referenced Request
# Sequence (PS3.4 Table CC.2.5-3). The bare top-level tag is a fallback for a
# server that still emits it (and the (0040,4021) Input Information Sequence
# names the study too), so a row is rendered whether the source is old or new.
MWL_OPTIONAL_TOP_SEQ = {
    "0020000D": ("0040A370", "0020000D"),
    "00080050": ("0040A370", "00080050"),
}

# The station source that moved into a sequence, resolved BEFORE the flat fill
# runs: UPS carries the station in (0040,4025) Scheduled Station Name Code
# Sequence, whose Code Value is the AE Title (PS3.4 Table CC.2.5-3; AIW-I
# 4.81.4.1.2.1 Table 4.81.4.1.2.1-3). The emitted MWL element stays
# (0040,0001) ScheduledStationAETitle, because that is the attribute Table
# K.6-1 defines for the DIMSE side - only the SOURCE changes.
MWL_STATION_SEQ = ("00404025", "00080100")

# Inside the single item of (0040,0100) Scheduled Procedure Step Sequence.
MWL_RETURN_SPS = {
    "00400001": (0x0040, 0x0001, "AE", "ScheduledStationAETitle"),
    "00400002": (0x0040, 0x0002, "DA", "ScheduledProcedureStepStartDate"),
    "00400003": (0x0040, 0x0003, "TM", "ScheduledProcedureStepStartTime"),
    # (0008,0060) Modality is a Return key Type 1 in MWL. UPS itself has no
    # Modality attribute (PS3.4 Table CC.2.5-3); the server renders the input
    # study's Modality as a documented vendor extension, and the bridge maps it
    # here. While a deployment's server does not render it, the key is simply
    # absent for every workitem - which is the same shape as any other absent
    # Return key, not a special case.
    "00080060": (0x0008, 0x0060, "CS", "Modality"),
}

MWL_OPTIONAL_SPS = {
    "00400010": (0x0040, 0x0010, "SH", "ScheduledStationName"),
}

# (0074,1200) ScheduledProcedureStepPriority is deliberately NOT here: it is an
# attribute of the UPS Scheduled Procedure Step Module (PS3.3 C.30.1) and has
# no row in Table K.6-1, so it must not appear in an MWL response.
#
# (0040,0006) ScheduledPerformingPhysicianName is a Return key Type 2 with no
# UPS source. Type 2 permits absence, so it is omitted rather than invented.

# SPS ID (0040,0009) / Requested Procedure ID (0040,1001) are derived, not
# mapped: see _mwl_sps_id / _mwl_rp_id.
MWL_SPS_ID_LEN = 16

# C-FIND MWL match keyword -> UPS-RS workitems query parameter.
# Only the keys the server's workitem search actually filters on (ups/types.rs
# parse_form_query / StoreFilter): station_aet, modality, patient_id,
# workitem_uid, workflow_state. Other MWL keys are NOT filterable server-side
# -> absent parameter = match all (DICOMweb convention).
MWL_TO_UPS_PARAM = {
    "ScheduledStationAETitle": "00404025.00080100",
    "Modality": "00080060",
    "PatientID": "00100020",
    "StudyInstanceUID": "0020000D",
    "ProcedureStepState": "00741000",
    "ScheduledProcedureStepStartDate": "00404005",
}

# PS3.4 Table K.6-1 (standard/ps3.4/mwl-k.6-1-matching-keys.json) makes
# Scheduled Procedure Step Start Time (0040,0003) a REQUIRED matching key of
# the SPS item, with the rule "Single Value or Range". Omitting it turned a
# 09:00-10:00 query into "match all": the bridge returned a 14:00 workitem
# alongside the 09:30 one. It has no separate UPS parameter - the origin filters
# on the combined workitem start instant (0040,4005) - so the time is folded
# into that value together with the date.
MWL_START_TIME_KEYWORD = "ScheduledProcedureStepStartTime"

# PS3.4 Table K.6-1 lists Scheduled Performing Physician's Name (0040,0006) as
# a matching key with the rule "Single Value or Wildcard". The UPS-RS workitem
# search exposes no parameter for it, so it cannot be pushed to the origin -
# but dropping it made the request match ALL workitems and a Jones procedure
# came back for a `Smith*` query. It is carried here and applied as a local
# wildcard filter over the response rows.
MWL_PHYSICIAN_KEYWORD = "ScheduledPerformingPhysicianName"


def _mwl_physician_matchers(ds):
    """The Scheduled Performing Physician patterns the request asked for.

    PS3.4 Table K.6-1 gives (0040,0006) the rule "Single Value or Wildcard",
    where the wildcard is `*` (PS3.4 C.2.2.2.4) and `?` matches one character.
    Each value is compiled once here, as a case-insensitive full match, and
    shared by every candidate row.

    Returns an empty tuple when the request states no physician, so the caller
    skips the filter entirely.
    """
    import re as _re

    patterns = []
    for elem in _iter_mwl_request_elements(ds):
        if elem.keyword != MWL_PHYSICIAN_KEYWORD:
            continue
        if elem.value is None:
            continue
        raw_values = (
            elem.value
            if isinstance(elem.value, (list, tuple, MultiValue))
            else [elem.value]
        )
        for raw in raw_values:
            pattern = str(raw or "").strip()
            if not pattern:
                continue
            # `*` is any run and `?` exactly one character (PS3.4 C.2.2.2.4).
            # Everything else is literal, so the value is quoted and only the
            # two wildcard characters are translated - a name containing a
            # regex metacharacter must not become a pattern.
            translated = "".join(
                ".*" if ch == "*" else "." if ch == "?" else _re.escape(ch)
                for ch in pattern
            )
            try:
                patterns.append(_re.compile(rf"\A{translated}\Z", _re.IGNORECASE))
            except _re.error:
                # An unparseable pattern cannot be honoured; keeping it would
                # reject every row for a request the origin already answered.
                continue
    return tuple(patterns)


def _mwl_physician_matches(stored, matchers):
    """Whether a stored physician name satisfies at least one request pattern.

    A row that carries no physician is KEPT: Type 2 permits an absent value,
    and absence is not proof that the name differs from the query. A request
    that states no physician (no matchers) keeps everything.
    """
    if not matchers:
        return True
    text = str(stored or "").strip()
    if not text:
        return True
    return any(m.match(text) for m in matchers)


def _mwl_start_datetime_bounds(ds):
    """The SPS start DATE+TIME window the request asks for, or None.

    PS3.4 Table K.6-1 defines (0040,0002) Start Date and (0040,0003) Start
    Time as separate matching keys, but they describe ONE interval: July 5-7
    at 10:00-18:00 includes a July 6 workitem at 22:00, because the time-of-day
    window applies inside the date range, not on top of it. Filtering each key
    independently dropped that workitem - the date query fetched it, then the
    time-of-day filter discarded it because 22:00 is outside 10:00-18:00.

    The combined window is expressed as an inclusive (low_dt, high_dt) pair of
    YYYYMMDDHHMMSS strings so a stored (0040,4005) instant compares by plain
    string ordering. A blank bound means unbounded on that side.

    Returns None when the request states no date/time window at all, so the
    caller skips local filtering entirely.
    """
    date_bounds = None
    time_bounds = None
    for elem in _iter_mwl_request_elements(ds):
        if elem.keyword == "ScheduledProcedureStepStartDate" and date_bounds is None:
            date_bounds = _mwl_date_bounds(elem.value)
        elif elem.keyword == MWL_START_TIME_KEYWORD and time_bounds is None:
            time_bounds = _mwl_time_bounds(elem.value)
    if date_bounds is None and time_bounds is None:
        return None

    d_lo, d_hi = date_bounds if date_bounds is not None else ("", "")
    t_lo, t_hi = time_bounds if time_bounds is not None else ("", "")

    if not d_lo and not d_hi:
        # TIME-ONLY request: the modality asked for a time of day on any
        # date, so only the HHMMSS component is comparable. Returning
        # date-prefixed bounds here would compare "2026..." against a bare
        # "090000" and reject every row.
        return ("T", t_lo, t_hi)

    # Each side is built from the SAME kind of bound: a single date with a
    # A date range with a time range is "EACH DAY in the range, WITHIN those
    # hours" - not one continuous span from the first evening to the last.
    # Concatenating the bounds (20260705210000..20260707230000) made a July 6
    # workitem at 10:00 match a 21:00-23:00 request, because 10:00 on July 6
    # lies between the two endpoints. The window is therefore carried as
    # three parts: the date span, and the time-of-day span applied inside it.
    if t_lo or t_hi:
        return ("DR", d_lo, d_hi, t_lo, t_hi)
    # Date only: the whole day, every day in the range.
    return ("D", d_lo, d_hi, "", "")


def _mwl_datetime_in_bounds(stored, bounds):
    """Whether a stored (0040,4005) instant lies inside a combined window.

    PS3.4 Table K.6-1 defines (0040,0002) Start Date and (0040,0003) Start
    Time as ONE interval: the date range selects the days, and the time range
    selects the hours WITHIN each of those days. July 5-7 at 21:00-23:00
    therefore includes a July 6 workitem at 22:00 and excludes one at 10:00,
    whereas comparing a single concatenated span accepted both.

    A TIME-ONLY request compares the HHMMSS component alone, because its
    bounds carry no date. A row without the attribute is kept: absence is not
    a proven mismatch.
    """
    if bounds is None:
        return True
    text = str(stored or "").strip()
    if len(text) < 8:
        return True
    # Pad the stored instant to the compared precision; a date-only stored
    # value compares as the start of that day.
    stamp = (text + "00000000000000")[:14]
    kind = bounds[0]
    if kind == "T":
        _, low, high = bounds
        hhmmss = stamp[8:14]
        if low and hhmmss < low:
            return False
        if high and hhmmss > high:
            return False
        return True
    _, d_lo, d_hi, t_lo, t_hi = bounds
    day = stamp[:8]
    # The DAY must be in the requested date range ...
    if d_lo and day < d_lo:
        return False
    if d_hi and day > d_hi:
        return False
    # ... and the TIME OF DAY inside that day must be in the time range.
    hhmmss = stamp[8:14]
    if t_lo and hhmmss < t_lo:
        return False
    if t_hi and hhmmss > t_hi:
        return False
    return True


def _mwl_time_bounds(value):
    """A (0040,0003) TM match value as an inclusive ``(low, high)`` HHMMSS pair.

    PS3.5 6.2 allows the short forms ``HH``, ``HHMM`` and ``HHMMSS``, and PS3.4
    Table K.6-1 gives this key the rule "Single Value or Range". Both are
    normalised to the six-digit form so a comparison against a stored time is
    plain string ordering:

      - ``0900``          -> ('090000', '090000')
      - ``090000-100000`` -> ('090000', '100000')
      - ``-100000``       -> ('', '100000')   (unbounded below)

    Returns None when the value cannot be read, so the caller can decide
    without guessing.
    """

    def _hm(part):
        part = part.strip()
        if not part:
            return None
        # PS3.5 6.2: TM is HH, HHMM or HHMMSS, each optionally followed by
        # `.FFFFFF` fractional seconds. The fraction was rejected outright,
        # so `093000.25-093000.75` - a legal range - yielded no bounds at all
        # and the request ran WITHOUT a time filter, returning a 14:00
        # workitem for a 09:30 request. The fraction is kept as its own
        # six-digit string so the comparison stays plain string ordering
        # against a stored instant that carries the same precision.
        #
        # The dot is split off FIRST: testing the whole component with
        # `isdigit()` rejected every fractional value before the split could
        # happen, which is the defect this branch exists to fix.
        whole, _, frac = part.partition(".")
        if not whole.isdigit() or len(whole) not in (2, 4, 6):
            return None
        if frac and not frac.isdigit():
            return None
        return (whole + "000000")[:6]

    text = str(value or "").strip()
    if not text:
        return None
    if "-" in text:
        low, _, high = text.partition("-")
        lo, hi = _hm(low), _hm(high)
        if lo is None and hi is None:
            return None
        return (lo or "", hi or "")
    single = _hm(text)
    if single is None:
        return None
    return (single, single)


def _mwl_date_bounds(value):
    """A (0040,0002) DA match value as an inclusive ``(low, high)`` YYYYMMDD pair.

    PS3.4 Table K.6-1 gives the key the rule "Single Value or Range", and
    PS3.5 6.2 allows the ``YYYY.MM.DD`` form. Both are normalised to the
    eight-digit dotted-free form so a comparison against a stored date is plain
    string ordering:

      - ``20260706``              -> ('20260706', '20260706')
      - ``20260705-20260707``     -> ('20260705', '20260707')
      - ``-20260707``             -> ('', '20260707')   (unbounded below)

    Returns None when the value cannot be read, so the caller decides without
    guessing.
    """

    def _day(part):
        part = part.strip().replace(".", "")
        if not part or not part.isdigit() or len(part) != 8:
            return None
        return part

    text = str(value or "").strip()
    if not text:
        return None
    if "-" in text:
        low, _, high = text.partition("-")
        lo, hi = _day(low), _day(high)
        if lo is None and hi is None:
            return None
        return (lo or "", hi or "")
    single = _day(text)
    if single is None:
        return None
    return (single, single)


def _mwl_time_in_bounds(stored, bounds):
    """Whether a stored (0040,4005) instant satisfies an SPS start-time range.

    The origin filters on the full instant; the modality expressed an interest
    in a TIME OF DAY. This applies that part of the matching locally, before
    the response limit, so a workitem outside the requested window never
    consumes a Pending response.
    """
    if bounds is None:
        return True
    low, high = bounds
    text = str(stored or "").strip()
    if len(text) < 8:
        return True
    # (0040,4005) DT: YYYYMMDDHHMMSS...; the time component starts at index 8.
    hhmmss = (text[8:14] + "000000")[:6]
    if low and hhmmss < low:
        return False
    if high and hhmmss > high:
        return False
    return True


def _mwl_date_to_ups_datetime(value):
    """Map a (0040,0002) DA match value onto the (0040,4005) DT grammar.

    MWL matches a DATE; UPS stores a DATETIME. The server compares the query
    value against the stored DT with string semantics inside an inclusive
    range (`<lo>-<hi>`), so:
      - `YYYYMMDD`        -> `YYYYMMDD000000-YYYYMMDD235959` (the whole day)
      - `YYYYMMDD-YYYYMMDD` -> `YYYYMMDD000000-YYYYMMDD235959`
      - a DT-shaped value is passed through untouched (some senders fill the
        DA key with the full instant).
    An unusable value is returned unchanged: a filter that cannot be built is
    not a reason to widen the search silently, and the server answers the
    unchanged form as it always has.
    """
    text = str(value or "").strip()
    if not text:
        return ""

    def _day(part):
        part = part.strip()
        return part if len(part) == 8 and part.isdigit() else None

    if "-" in text:
        low, _, high = text.partition("-")
        lo, hi = _day(low), _day(high)
        # Open-ended ranges keep their open end (the server treats an empty
        # bound as unbounded). The two bounds are built INDEPENDENTLY: emitting
        # the lower bound as a literal `000000` turned `-20261121` ("everything
        # up to that day") into `000000-20261121235959`, which silently
        # narrowed an unbounded window to one starting at midnight - the exact
        # opposite of what the modality asked.
        if lo is None and hi is None:
            return text
        lower = f"{lo}000000" if lo else ""
        # PS3.5 6.2: DT may carry fractional seconds. `{day}235959` is the
        # INSTANT 23:59:59, so a workitem scheduled at `{day}235959.500` sorts
        # AFTER it and a single-day query silently omitted the last 999 ms of
        # the day. The bound is the end of the final second, expressed with the
        # maximum fraction so every instant inside 23:59:59 compares below it.
        upper = f"{hi}235959.999999" if hi else ""
        return f"{lower}-{upper}"
    day = _day(text)
    if day is None:
        return text
    return f"{day}000000-{day}235959.999999"


def _mwl_start_time_bounds(ds):
    """The SPS start-time range the request asks for, or None.

    Read from the same two levels `_build_mwl_params` reads: the top level and
    the single item of (0040,0100). First statement wins, matching that
    function's rule so the two cannot disagree about which value applies.
    """
    for elem in _iter_mwl_request_elements(ds):
        if elem.keyword != MWL_START_TIME_KEYWORD:
            continue
        if elem.value is None:
            continue
        bounds = _mwl_time_bounds(elem.value)
        if bounds is not None:
            return bounds
        return None
    return None


def _build_mwl_params(ds):
    """Build UPS-RS workitems query params from a C-FIND MWL request Dataset.

    Reads the matching keys from BOTH the top level and the item of
    (0040,0100) Scheduled Procedure Step Sequence, because that is where a
    conformant MWL request puts them. A nested-only request used to yield an
    empty dict - i.e. match-all - so the modality's station filter silently
    returned every workitem on the server.

    Maps only the keys the server's workitem search filters on; empty values
    are skipped (absent = match all, the DICOMweb convention). Returns a dict
    (possibly empty).
    """
    params = {}
    for elem in _iter_mwl_request_elements(ds):
        keyword = elem.keyword
        if keyword not in MWL_TO_UPS_PARAM:
            continue
        param = MWL_TO_UPS_PARAM[keyword]
        if param in params:
            # Already stated at an earlier level (top level precedes the SPS
            # item). First statement wins, so the resolution does not depend on
            # element order inside the sequence.
            continue
        # PS3.18 10.6: the query syntax is comma-separated for a list of
        # alternatives, and `str()` on a MultiValue yields "['CT', 'MR']" -
        # which the server reads as one unmatched value. An element with no
        # value is skipped rather than sent as the text "None".
        if elem.value is None:
            continue
        if isinstance(elem.value, (list, tuple, MultiValue)):
            val = ",".join(str(item).strip() for item in elem.value if item)
        else:
            val = str(elem.value).strip()
        if val:
            if keyword == "ScheduledProcedureStepStartDate":
                val = _mwl_date_to_ups_datetime(val)
            params[param] = val
    return params


def _json_pn_to_dimse(raw):
    """A DICOMweb JSON Person Name back to its DIMSE `Alphabetic=Ideographic=Phonetic` form.

    PS3.18 F.2.2 gives each populated component group its OWN property. Reading
    only `Alphabetic` therefore dropped every non-alphabetic name: a PN whose
    answer carried only `Ideographic` became the empty string, so the modality
    received a nameless patient - and a PN with an ideographic part lost it.
    The three groups are rejoined with the `=` separator in the order the VR
    defines (PS3.5 6.2), preserving EMPTY leading groups so the modality can
    still tell which group a value belongs to.
    """
    if not isinstance(raw, dict):
        return str(raw)
    groups = [
        str(raw.get("Alphabetic", "") or ""),
        str(raw.get("Ideographic", "") or ""),
        str(raw.get("Phonetic", "") or ""),
    ]
    # Trailing empty groups are the default and carry no information, so only
    # the populated span up to the last non-empty group is emitted. An entirely
    # empty name stays an empty value rather than "==".
    while groups and not groups[-1]:
        groups.pop()
    return "=".join(groups)


def _safe_add_new(ds, group, elem, vr, val):
    """Add element to Dataset; fall back to direct assignment if tag exists (BR-014)."""
    try:
        ds.add_new((group, elem), vr, val)
    except ValueError:
        ds[(group, elem)] = DataElement((group, elem), vr, val)


def _json_add_vr(ds, group, elem, vr, raw_value):
    """Add one DICOMweb JSON value to `ds` under the element's VR (CMP-005).

    The single place a JSON value becomes a pydicom element, shared by the
    QIDO and MWL converters so the two cannot drift on VR handling. Returns
    True when the element was added.

    `raw_value` is the whole DICOMweb `Value` array (already checked non-empty
    by the caller). CS, UI and SH are value-multiplicity VRs (PS3.5 Table
    6.2-1): ModalitiesInStudy (0008,0061) legitimately carries several CS
    values, and taking `Value[0]` reported one modality for a study performed
    on three. Rendering the array with `str()` would be no better - the SCU
    would read "['CT', 'MR']" as a single value.

    A JSON `null` inside `Value` is an ABSENT value, not the text "None":
    `["CT", null]` stringified the null into five characters that then
    travelled to the SCU as a modality. Nulls are dropped before conversion;
    an array of nothing but nulls carries no value at all and is not added.
    """
    try:
        items = [v for v in raw_value if v is not None]
        if not items:
            return False
        if vr in ("CS", "UI", "SH") and len(items) > 1:
            val = [str(item) for item in items]
        else:
            raw_first = items[0]
            if vr == "PN":
                val = _json_pn_to_dimse(raw_first)
            elif vr == "IS":
                val = int(raw_first)
            elif vr == "TM":
                # Time: normalize the display "HH:MM:SS" form to the canonical
                # DICOM "HHMMSS.FFFFFF" (PS3.5 6.2). pydicom logs
                # "Invalid value for VR TM: '11:25:01'" when a colon form is
                # constructed (field noise on every C-FIND); stripping the
                # colons keeps the attribute valid and warning-free.
                val = str(raw_first).replace(":", "")
            else:
                # LO, DA - plain string.
                val = str(raw_first)
    except (TypeError, ValueError, IndexError):
        return False
    _safe_add_new(ds, group, elem, vr, val)
    return True


def _json_map_to_dataset(json_obj, keymap):
    """Convert a DICOMweb JSON object to a pydicom Dataset (shared core).

    Handles VR-aware parsing for PN/IS/CS/UI/LO/SH/DA/TM (CMP-005).

    The C-FIND response re-emits the 16 mapped attributes on every row;
    PS3.4 C.4.1.1.3.2 says the response shall not contain attributes that
    were not in the request or specified in that section - this is a
    deliberate, viewer-tolerated deviation (documented in the DIMSE bridge
    conformance statement).
    """
    ds = Dataset()
    # PS3.5 6.1.2: SpecificCharacterSet MUST be the first element.
    # ISO_IR 192 (UTF-8) is ALWAYS set, deliberately (documented in the DIMSE
    # bridge conformance statement): without it pydicom defaults to iso8859 /
    # Latin-1 and replaces Cyrillic PN/LO with '????' in RadiAnt/MicroDICOM.
    # The charset covers Cyrillic and all other scripts in the bridge rows.
    ds.SpecificCharacterSet = "ISO_IR 192"

    for json_key, value in json_obj.items():
        if json_key not in keymap:
            continue
        group, elem, vr, dicom_keyword = keymap[json_key]
        raw_value = value.get("Value", []) if isinstance(value, dict) else []
        if not raw_value:
            # Zero-length delta (documented in the DIMSE bridge conformance
            # statement): an attribute that is present but EMPTY is dropped,
            # i.e. the attribute is ABSENT from the row, NOT emitted as a
            # zero-length value. PS3.4 C.4.1.1.3.2 note 2 would require a
            # zero-length attribute; we deliberately do not do that (keeps
            # the rows minimal and is tolerated by all viewers).
            continue
        _json_add_vr(ds, group, elem, vr, raw_value)
    return ds


def _json_to_dataset(json_obj, request_keywords):
    """QIDO-RS study JSON -> pydicom Dataset (13/13 QIDO attributes)."""
    return _json_map_to_dataset(json_obj, QIDO_KEY_TO_TAG)


# PS3.4 C.6.1.1.2 Table C.6-1 (standard/ps3.4/patient-root-levels-C.6-1.json)
# fixes exactly these attributes at the Patient level of the Patient Root
# model. PatientName is the level's Type R key; the rest are optional, and
# QIDO_KEY_TO_TAG already maps all four, so no new tag knowledge is needed -
# the table only decides WHICH of the study row's attributes survive the fold.
PATIENT_LEVEL_KEYS = ("00100010", "00100020", "00100030", "00100040")


def _json_to_dataset_patient(json_obj):
    """QIDO-RS study JSON -> a Patient-level Dataset (PS3.4 Table C.6-1).

    Patient level is served from the study query: the origin's study row
    carries the patient attributes, so the fold is a projection, not a
    separate lookup. Keys outside Table C.6-1 are dropped - a Patient-level
    row that also carried StudyDate or StudyInstanceUID would tell the SCU
    the level is actually a study level with a relabelled (0008,0052).
    """
    return _json_map_to_dataset(
        json_obj, {k: QIDO_KEY_TO_TAG[k] for k in PATIENT_LEVEL_KEYS}
    )


def _json_first_value(json_obj, key):
    """First DICOMweb JSON Value entry for `key` ('' when absent).

    PN values arrive as {"Alphabetic": ...} objects; everything else as
    plain strings/numbers.

    A JSON null (or an empty list) means the attribute carries NO value, and
    returns None rather than the string "None": `str(None)` is a plausible
    looking token that then travels as a UID.
    """
    attr = json_obj.get(key) or {}
    values = attr.get("Value") or []
    if not values:
        return ""
    first = values[0]
    if first is None:
        return None
    if isinstance(first, dict):
        return str(first.get("Alphabetic", ""))
    return str(first)


def _json_seq_first_value(json_obj, seq_key, item_key):
    """First value of `item_key` inside the FIRST item of sequence `seq_key`.

    The UPS workitem render carries the study and the accession through
    (0040,A370) Referenced Request Sequence (PS3.4 Table CC.2.5-3), so a
    consumer that wants the input study reads it there. '' when the sequence,
    the item or the tag is absent.
    """
    seq = json_obj.get(seq_key) or {}
    items = seq.get("Value") or []
    if not items or not isinstance(items[0], dict):
        return ""
    return _json_first_value(items[0], item_key)


def _json_workitem_study_uid(json_obj):
    """The input study UID of a UPS workitem JSON, from either carriage.

    (0040,A370) Referenced Request Sequence is the standard carriage and is
    read first; the bare top-level (0020,000D) is the fallback for a server
    (or a stored row) that still emits it, and the (0040,4021) Input
    Information Sequence is the last resort. The three name one fact.
    """
    study = _json_seq_first_value(json_obj, "0040A370", "0020000D")
    if study:
        return study
    study = _json_first_value(json_obj, "0020000D")
    if study:
        return study
    return _json_seq_first_value(json_obj, "00404021", "0020000D")


def _json_to_dataset_series(json_obj, request_keywords, study_json=None):
    """QIDO-RS series JSON -> pydicom Dataset (31.08 series-level C-FIND).

    The series C-FIND dataset MUST carry series attributes (0020,000E,
    0020,0011, 0008,0060, ...) - viewers drop rows without them. The
    server's series render omits PatientName/PatientID/StudyInstanceUID
    (confirmed live 31.08), so when the owning study row (`study_json`)
    is given, those three are injected from it.
    """
    ds = _json_map_to_dataset(json_obj, QIDO_SERIES_KEY_TO_TAG)
    if study_json:
        study_ds = _json_map_to_dataset(study_json, QIDO_KEY_TO_TAG)
        for group, elem in _STUDY_IDENTITY_ATTRS.values():
            ident_elem = study_ds.get((group, elem))
            if ident_elem is not None:
                _safe_add_new(ds, group, elem, ident_elem.VR, ident_elem.value)
    return ds


def _json_to_dataset_instance(
    json_obj,
    request_keywords,
    study_uid,
    series_uid,
    study_json=None,
    series_json=None,
):
    """QIDO-RS instance JSON -> an IMAGE-level pydicom Dataset.

    PS3.4 Table C.6-4 / C.4.1.3.1.1 (standard/ps3.4/image-level-C.6-4.json):
    every response Identifier carries all Unique Keys of the levels above, so
    StudyInstanceUID and SeriesInstanceUID are injected from the walk's
    context, and PatientName/PatientID plus the series context (Modality,
    SeriesNumber, SeriesDescription) from the rows the walk already fetched.
    The server's instance render carries none of those.

    A row with NO SOPInstanceUID is not an IMAGE-level answer at all - it is
    the relabelled study/series row the old fall-through produced, which the
    SCU cannot use. The caller drops such a row before it is yielded; this
    converter cannot invent the UID.
    """
    ds = _json_map_to_dataset(json_obj, QIDO_INSTANCE_KEY_TO_TAG)
    # A missing UID on either axis would write a zero-length Unique Key, which
    # a SCU reads as a real (empty) UID: absent is the honest form.
    if study_uid:
        _safe_add_new(ds, 0x0020, 0x000D, "UI", study_uid)
    if series_uid:
        _safe_add_new(ds, 0x0020, 0x000E, "UI", series_uid)
    if study_json:
        study_ds = _json_map_to_dataset(
            study_json, {k: QIDO_KEY_TO_TAG[k] for k in PATIENT_LEVEL_KEYS}
        )
        for group, elem in _STUDY_IDENTITY_ATTRS.values():
            src = study_ds.get((group, elem))
            if src is not None:
                _safe_add_new(ds, group, elem, src.VR, src.value)
    if series_json:
        series_ds = _json_map_to_dataset(series_json, QIDO_SERIES_KEY_TO_TAG)
        for json_key in _INSTANCE_SERIES_CONTEXT:
            group, elem, _vr, _kw = QIDO_SERIES_KEY_TO_TAG[json_key]
            src = series_ds.get((group, elem))
            if src is not None:
                _safe_add_new(ds, group, elem, src.VR, src.value)
    return ds


def _mwl_tail(value):
    """An identifier that fits MWL_SPS_ID_LEN characters, or '' when absent.

    Both identifiers below live in VR SH (16 characters). A UPS workitem UID is
    `2.25.<decimal u128>` (up to 43 characters), so it cannot be carried
    verbatim and its tail is used instead. This is a DERIVATION into an
    identifier's own scoped namespace, not a truncation of a value the modality
    must round-trip: a modality only needs a stable handle to couple MPPS to
    the step, and SPS ID is scoped by the Requested Procedure (PS3.3 C.4.10).

    A value that ALREADY fits is returned unchanged. Returning '' for it made
    a short workitem UID yield no (0040,0009) at all - a Type 1 MWL key - even
    though the only reason to shorten anything is the VR's 16 characters.
    """
    text = str(value or "").strip()
    if len(text) <= MWL_SPS_ID_LEN:
        return text
    return text[-MWL_SPS_ID_LEN:]


def _mwl_sps_id(workitem_uid):
    """(0040,0009) Scheduled Procedure Step ID, derived from the workitem UID.

    The MWL model has no SOP Instance UID (PS3.4 Table K.6-1), so the workitem
    UID has no legal home; its tail is the honest, deterministic stand-in.
    """
    return _mwl_tail(workitem_uid)


def _mwl_rp_id(study_instance_uid):
    """(0040,1001) Requested Procedure ID, derived from the study UID.

    Empty when the workitem names no study: there is no Requested Procedure
    without one, so nothing is emitted.
    """
    return _mwl_tail(study_instance_uid)


def _mwl_fill_start_datetime(sps, json_obj):
    """Fill (0040,0002)/(0040,0003) from (0040,4005) when the pair is missing.

    Both are Return keys Type 1 in MWL, but the UPS workitem carries the single
    DT attribute (0040,4005) ScheduledProcedureStepStartDateTime instead. Split
    it: the first 8 characters are the date, the next 6 the time. Only fields
    that are still absent are filled, so an explicit DA/TM pair wins.
    """
    if (
        "ScheduledProcedureStepStartDate" in sps
        and "ScheduledProcedureStepStartTime" in sps
    ):
        return
    dt = _json_first_value(json_obj, "00404005")
    if not dt:
        return
    digits = str(dt).strip()
    if len(digits) < 8:
        return
    date_part = digits[:8]
    time_part = digits[8:14]
    if "ScheduledProcedureStepStartDate" not in sps and date_part.isdigit():
        _safe_add_new(sps, 0x0040, 0x0002, "DA", date_part)
    if (
        "ScheduledProcedureStepStartTime" not in sps
        and len(time_part) == 6
        and time_part.isdigit()
    ):
        _safe_add_new(sps, 0x0040, 0x0003, "TM", time_part)


def _json_to_dataset_mwl(json_obj, request_keywords):
    """UPS-RS workitem JSON -> pydicom MWL Dataset, in the standard's shape.

    PS3.4 Table K.6-1: one row per Scheduled Procedure Step, with
    PatientName/PatientID at identifier level and the SPS attributes inside the
    single item of (0040,0100) Scheduled Procedure Step Sequence.

    Emission rules:
      - Return keys (MWL_RETURN_TOP, MWL_RETURN_SPS) whenever the source has a
        value, whether the modality asked or not.
      - Optional keys (MWL_OPTIONAL_TOP, MWL_OPTIONAL_SPS) ONLY when the
        modality named them in the identifier (PS3.4 C.4.1.1.3.2).

    The two derived identifiers have no source to map from:
      - (0040,0009) SPS ID       <- the workitem UID's tail
      - (0040,1001) RP ID        <- the input study UID's tail
    See _mwl_sps_id / _mwl_rp_id for why the tail and not the whole value.
    """
    ds = Dataset()
    # PS3.5 6.1.2: SpecificCharacterSet MUST be the first element.
    # ISO_IR 192 (UTF-8) is ALWAYS set, deliberately (documented in the DIMSE
    # bridge conformance statement): without it pydicom defaults to iso8859 /
    # Latin-1 and replaces Cyrillic PN/LO with '????' in RadiAnt/MicroDICOM.
    ds.SpecificCharacterSet = "ISO_IR 192"

    sps = Dataset()

    def fill(target, keymap, source, always):
        """Copy mapped values from `source` into `target` under `keymap`."""
        for json_key, (group, elem, vr, keyword) in keymap.items():
            if not always and keyword not in request_keywords:
                continue
            value = source.get(json_key)
            if not isinstance(value, dict):
                continue
            raw_value = value.get("Value") or []
            if not raw_value:
                # Present but EMPTY is dropped, not emitted zero-length: an
                # attribute with no value is absent from the row (documented
                # deviation, PS3.4 C.4.1.1.3.2 note 2 would allow zero-length).
                continue
            _json_add_vr(target, group, elem, vr, raw_value)

    # Identifier level: the two Return keys, then the request-gated Optional
    # ones. The study and the accession live in the Referenced Request
    # Sequence on a current server, so their values are resolved through that
    # (with the bare tag and the Input Information Sequence as fallbacks)
    # before the flat fill runs.
    fill(ds, MWL_RETURN_TOP, json_obj, always=True)
    optional_source = dict(json_obj)
    for flat_key, (seq_key, item_key) in MWL_OPTIONAL_TOP_SEQ.items():
        # `0020000D` is resolved even when the flat tag is present: the bare
        # tag and the Referenced Request Sequence must name the SAME study as
        # the derived RP ID, and `_json_workitem_study_uid` is the one place
        # that decides which value wins. Skipping the resolution when the bare
        # tag exists let the two disagree - and an empty bare tag silently
        # dropped StudyInstanceUID while leaving the RP ID derived from it.
        if flat_key == "0020000D":
            value = _json_workitem_study_uid(json_obj)
        elif flat_key == "00080050":
            # AccessionNumber (0008,0050) is Type 2: a present-but-EMPTY flat
            # tag is the valid "not recorded" marker, not a value. The
            # accession's standard carriage is (0040,A370) Referenced Request
            # Sequence (PS3.4 Table CC.2.5-3), so it is resolved FIRST, with
            # the bare tag only as a fallback. The old guard skipped the
            # resolution on the bare tag's mere presence, so an empty flat tag
            # shadowed a sequence value and fill() dropped the attribute.
            value = _json_seq_first_value(
                json_obj, seq_key, item_key
            ) or _json_first_value(json_obj, flat_key)
        else:
            if flat_key in optional_source:
                continue
            value = _json_seq_first_value(
                json_obj, seq_key, item_key
            ) or _json_first_value(json_obj, flat_key)
        if value:
            optional_source[flat_key] = {"vr": "UI", "Value": [value]}
    fill(ds, MWL_OPTIONAL_TOP, optional_source, always=False)

    # SPS level: the Return keys, then the request-gated Optional ones. The
    # station is resolved through its sequence first, so the flat fill below
    # reads a value that came from (0040,4025) Code Value without the return
    # key map having to know about sequences.
    sps_source = dict(json_obj)
    station = _json_seq_first_value(json_obj, *MWL_STATION_SEQ)
    if station:
        sps_source["00400001"] = {"vr": "AE", "Value": [station]}
    fill(sps, MWL_RETURN_SPS, sps_source, always=True)
    fill(sps, MWL_OPTIONAL_SPS, json_obj, always=False)

    # (0040,0002)/(0040,0003) Start Date/Time are Return keys Type 1. A
    # workitem created by the 2709 UPS Update path carries only (0040,4005)
    # ScheduledProcedureStepStartDateTime (VR DT); split it into the DA/TM
    # pair rather than leaving two Return keys empty. With neither source, the
    # pair is honestly absent - a step with no start time cannot produce one.
    _mwl_fill_start_datetime(sps, json_obj)

    # (0040,0009) SPS ID: the handle the modality couples MPPS to.
    sps_id = _mwl_sps_id(_json_first_value(json_obj, "00080018"))
    if sps_id:
        _safe_add_new(sps, 0x0040, 0x0009, "SH", sps_id)

    # (0040,0007) SPS Description is Optional: the modality must ask for it.
    # Source is (0074,1204) Procedure Step Label, the human-readable label.
    if "ScheduledProcedureStepDescription" in request_keywords:
        label = _json_first_value(json_obj, "00741204")
        if label:
            _safe_add_new(sps, 0x0040, 0x0007, "LO", label)

    if len(sps) > 0:
        ds.ScheduledProcedureStepSequence = Sequence([sps])

    # (0040,1001) Requested Procedure ID is Optional, and derived from the
    # input study. No study -> no Requested Procedure, so nothing is emitted.
    # The study is read from the Referenced Request Sequence first (the
    # standard carriage), with the bare tag and the Input Information
    # Sequence as fallbacks.
    if "RequestedProcedureID" in request_keywords:
        rp_id = _mwl_rp_id(_json_workitem_study_uid(json_obj))
        if rp_id:
            _safe_add_new(ds, 0x0040, 0x1001, "SH", rp_id)

    return ds


# ---------------------------------------------------------------------------
# N-ACTION protocol helpers
# ---------------------------------------------------------------------------
def _parse_n_action_rq(event):
    """Parse N-ACTION-RQ dataset.

    Returns (transaction_uid, [(sop_class_uid, sop_instance_uid), ...]).
    Raises ValueError if TransactionUID (0008,1195) is missing.
    """
    # An N-ACTION-RQ carries its payload as the Action Information parameter
    # (PS3.7 9.3.2), which is `event.action_information`. `event.dataset` reads
    # the C-STORE Data Set parameter instead and RAISES for an N-ACTION, so it
    # is only a fallback for test doubles that populate it.
    ds = None
    try:
        ds = event.action_information
    except AttributeError:
        pass
    if ds is None:
        try:
            ds = event.dataset
        except AttributeError:
            ds = Dataset()
    tx_uid = getattr(ds, "TransactionUID", None)
    if not tx_uid or str(tx_uid).strip() == "":
        raise ValueError("N-ACTION-RQ: missing TransactionUID (0008,1195)")
    tx_uid = str(tx_uid)

    sop_list = []
    ref_seq = getattr(ds, "ReferencedSOPSequence", None)
    if ref_seq:
        for number, item in enumerate(ref_seq, start=1):
            sop_class = str(getattr(item, "ReferencedSOPClassUID", ""))
            sop_uid = str(getattr(item, "ReferencedSOPInstanceUID", ""))
            # PS3.4 J.3.2 makes both UIDs Type 1 in every item: an item that
            # lacks either one does not identify an instance, so there is
            # nothing the origin could be asked to commit.
            #
            # Silently skipping the item was the worst of the three options:
            # with only that item present the parser returned an empty list and
            # `handle_n_action` answered SUCCESS without registering any report,
            # and in a mixed request the malformed item vanished while the SCU
            # was told the whole set was accepted. Raising here sends the
            # request down the existing refusal path, which is honest about a
            # request the bridge cannot represent.
            if not sop_class.strip():
                raise ValueError(
                    "N-ACTION-RQ: Referenced SOP Sequence item "
                    f"{number} has no ReferencedSOPClassUID (0008,1150)"
                )
            if not sop_uid.strip():
                raise ValueError(
                    "N-ACTION-RQ: Referenced SOP Sequence item "
                    f"{number} has no ReferencedSOPInstanceUID (0008,1155)"
                )
            sop_list.append((sop_class, sop_uid))

    return tx_uid, sop_list


def _build_n_action_rsp(tx_uid, verified_sops, failed_sops):
    """Build N-ACTION-RSP pydicom Dataset.

    verified_sops: list of (sop_class_uid, sop_instance_uid)
    failed_sops:   list of (sop_class_uid, sop_instance_uid, failure_reason)
    """
    rsp = Dataset()
    rsp.TransactionUID = tx_uid

    if verified_sops:
        success_seq = []
        for sop_class, sop_uid in verified_sops:
            item = Dataset()
            item.ReferencedSOPClassUID = sop_class
            item.ReferencedSOPInstanceUID = sop_uid
            success_seq.append(item)
        rsp.ReferencedSOPSequence = success_seq

    if failed_sops:
        failed_seq = []
        for sop_class, sop_uid, reason in failed_sops:
            item = Dataset()
            item.ReferencedSOPClassUID = sop_class
            item.ReferencedSOPInstanceUID = sop_uid
            item.FailureReason = reason
            failed_seq.append(item)
        rsp.FailedSOPSequence = failed_seq

    return rsp
