// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Manifest record types and their CBOR encoding.
//!
//! This module is the single source of truth for the CBOR shape of a study
//! manifest. Encoding and decoding are symmetric and context-aware:
//! numeric key ranges are interpreted differently depending on the record
//! type being read (system header, study QIDO map, series, instance, frame).

use crate::cbor::{CborError, CborReader, CborToken, CborWriter};
use std::fmt;

// -- Top-level map keys --------------------------------------------------

/// The only manifest format version this codec reads or writes.
///
/// Both directions check it exactly. The decoder used to accept any non-zero
/// version, so a future version-2 manifest decoded as if it were version 1;
/// the encoder used to accept any non-zero version too, publishing a manifest
/// no reader in this build can interpret.
pub const MANIFEST_VERSION: u8 = 1;

pub const KEY_SYS: u64 = 1;
pub const KEY_QIDO_STUDY: u64 = 2;
pub const KEY_SERIES: u64 = 3;

// -- System header sub-keys ----------------------------------------------

pub const SYS_VERSION: u64 = 1;
pub const SYS_STORAGE_TIER: u64 = 2;
pub const SYS_TOTAL_PIXEL_BYTES: u64 = 3;
/// Unix epoch (seconds) when this study arrived (E-014 ingestion order).
pub const SYS_ARRIVED_AT: u64 = 4;

// -- Series map: instances array -----------------------------------------

/// System key inside a series map: the array of instance records.
pub const SERIES_INSTANCES: u64 = 4;

/// Series Modality (0008,0060) - a DICOM attribute tag read during the
/// shallow study decode to derive the ModalitiesInStudy aggregate.
const MODALITY_TAG: u64 = 0x0008_0060;

// -- Instance map: WADO-facing identity fields ---------------------------

/// System key inside an instance map: the storage tier of the stored file.
pub const INSTANCE_STORAGE_TIER: u64 = 2;
/// System key inside an instance map: the transfer syntax UID.
pub const INSTANCE_TRANSFER_SYNTAX: u64 = 5;
/// System key inside an instance map: the array of frame records.
pub const INSTANCE_FRAMES: u64 = 6;
/// System key inside an instance map: the full-file BLAKE3 hash (CAS address).
pub const INSTANCE_BLAKE3: u64 = 7;
/// System key inside an instance map: the stored `.dcm` size in bytes.
pub const INSTANCE_FILE_SIZE: u64 = 9;
/// System key inside an instance map: why the instance is sequestered (IHE
/// IOCM / AIRA rejection note). Additive key: a manifest written before this
/// key existed decodes to [`crate::manifest::NOT_REJECTED_REASON`], and that
/// value emits no key at all, so untouched manifests stay byte-identical.
///
/// The value is the FULL rejection reason code (113001 Quality, 113037
/// patient safety, 113038 incorrect MWL, 113039 retention expired), not a
/// two-value "may be exposed" flag: whether a hide can be lifted, and whether
/// a re-store must be refused, are different questions asked by different
/// callers, and a boolean cannot answer either one specifically.
pub const INSTANCE_REJECTION_REASON: u64 = 10;

/// Stored reason code of an instance that is not sequestered.
pub const NOT_REJECTED_REASON: u32 = 0;

/// The Quality rejection reason code (PS3.3 C.17.2; AIRA 57.1.1.1).
///
/// Duplicated here rather than imported: `clarus-core` does not depend on
/// `clarus-ports`, and adding that edge so one integer could be shared would
/// invert the layering for a constant. The value is a DICOM standard code, not
/// a project invention, so two declarations of it cannot drift.
pub const QUALITY_REASON_CODE: u32 = 113_001;

// -- Frame map sub-keys --------------------------------------------------

pub const FRAME_NUMBER: u64 = 1;
pub const FRAME_HASH: u64 = 2;
pub const FRAME_OFFSET: u64 = 3;
pub const FRAME_COMPRESSED_LEN: u64 = 4;

// -- Storage tiers (matches clarus-adapters::storage_tiers) --------------

pub const TIER_HOT: u8 = 1;
pub const TIER_COLD: u8 = 2;
pub const TIER_S3: u8 = 3;

/// A single DICOM attribute value in string/number form.
///
/// Only the scalar/text types used by the manifest subset are supported
/// (no floats, no tags, no negative ints).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValue {
    /// 32-bit DICOM tag value held as a number (date, count, etc.).
    UInt(u64),
    /// Free-form text value (UID, patient name, modality).
    Text(String),
    /// Encapsulated bytes (stored inline by some DICOM attributes).
    Bytes(Vec<u8>),
    /// A list of scalar values (e.g. multi-valued attributes).
    Array(Vec<AttrValue>),
}

/// System header - the fixed preamble of every manifest.
///
/// `#[repr(C)]` pins the field order (zero-padding
/// compile-time gate via `assert_no_padding!`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(C)]
pub struct SysHeader {
    /// Format version. Currently always `1`.
    pub version: u8,
    /// Storage tier of the study (hot/cold/s3).
    pub storage_tier: u8,
    /// Sum of compressed pixel bytes across all frames.
    pub total_pixel_bytes: u64,
    /// Unix epoch (seconds) of ingestion. Additive key: old manifests decode
    /// to `0`. Re-STOW refreshes it so the patient rises to the top.
    pub arrived_at: u64,
}

/// Study-level QIDO search attributes.
///
/// Stored as an ordered list of `(DICOM tag, value)` pairs. Order is
/// preserved on encode/decode so round-trips are exact.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StudyQido {
    /// Ordered study-level search keys (tag -> value).
    pub attrs: Vec<(u32, AttrValue)>,
    /// NumberOfStudyRelatedSeries (0020,1206) - RAM-only derived count,
    /// computed once when the manifest is decoded into memory, never
    /// persisted to the CBOR schema.
    pub series_count: u64,
    /// NumberOfStudyRelatedInstances (0020,1208) - RAM-only derived count,
    /// computed once when the manifest is decoded into memory, never
    /// persisted to the CBOR schema.
    pub instance_count: u64,
    /// ModalitiesInStudy (0008,0061) - RAM-only derived aggregate of the
    /// distinct series modalities, in first-occurrence order, comma-joined.
    /// Computed once when the manifest is decoded into memory; NEVER persisted
    /// to the CBOR schema (mirrors the SQLite-era `GROUP_CONCAT(DISTINCT
    /// modality)` shape, though the SQL ordering was not contractual - B-tree
    /// fixes first-occurrence order).
    pub modalities_in_study: Option<String>,
}

// ALLOC: This is the manifest construction layer (cold path, called once per
// STOW). String/Vec allocations are intentional - hotpath (cbor/parse.rs,
// cbor/encode.rs) is 0-alloc.

/// A single series within a study.
///
/// `#[repr(C)]` pins the field order (zero-padding
/// compile-time gate via `assert_no_padding!`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(C)]
pub struct SeriesRecord {
    /// Series DICOM attributes, kept in encoded order.
    pub attrs: Vec<(u32, AttrValue)>,
    /// Child instances, in STOW order.
    pub instances: Vec<InstanceRecord>,
}

/// A single instance (SOP) within a series.
///
/// The WADO-facing fields (`blake3`, `storage_tier`,
/// `transfer_syntax`, `file_size`) let the server resolve and serve the stored
/// `.dcm` strictly from the manifest, with no SQLite lookup.
///
/// `#[repr(C)]` pins the field order; the single 32-byte hash leads so it
/// occupies offsets 0..32 and never straddles a 64B cache line (AP101B-101: a
/// field split across two lines forces two line fills per access on the hot
/// QIDO/WADO read path).
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(C)]
pub struct InstanceRecord {
    /// Full-file BLAKE3 hash of the stored `.dcm` (the CAS address).
    pub blake3: [u8; 32],
    /// Instance DICOM attributes, kept in encoded order.
    pub attrs: Vec<(u32, AttrValue)>,
    /// Transfer syntax UID of the stored instance.
    pub transfer_syntax: String,
    /// Storage tier of the stored `.dcm` (hot/cold/s3).
    pub storage_tier: u8,
    /// Sequestered by an IOCM/AIRA rejection note: hidden from QIDO/WADO but
    /// kept. The value is the reason code, `0` meaning visible.
    ///
    /// Sits beside `storage_tier` on purpose. `u8` + `u16` fill four bytes of
    /// the 8-byte word before the `u64` below, so the struct keeps zero
    /// padding; appending the flag at the end would add a tail pad and break
    /// the `assert_no_padding!` layout gate. `blake3` still leads at offsets
    /// 0..32, so the cache-line invariant is untouched.
    ///
    /// `u32`, not a `u16` "because the codes are small": 113001 itself does
    /// not fit in 16 bits, and a narrower field would have to be scaled or
    /// truncated - both of which make the stored code stop being the DICOM
    /// code. The field still costs nothing: it occupies the tail of the word
    /// `storage_tier` opens, so the struct's size is unchanged.
    pub rejection_reason: u32,
    /// Stored `.dcm` size in bytes.
    pub file_size: u64,
    /// CAS frame-chunk table for this instance.
    pub frames: Vec<FrameRecord>,
}

impl InstanceRecord {
    /// Whether an IOCM/AIRA rejection note sequestered this instance.
    ///
    /// The reason code is the stored flag, so "rejected" and "why" can never
    /// disagree - a separate boolean would allow an instance marked rejected
    /// with no reason, which no caller could then decide to expose or refuse.
    #[inline]
    pub fn is_rejected(&self) -> bool {
        self.rejection_reason != NOT_REJECTED_REASON
    }

    /// Whether a review surface may retrieve this instance.
    ///
    /// True only for Quality (113001). The IOCM AnyClass reasons hide
    /// diagnostic images with no expose path at all (IOCM 4.66.4.2.3 /
    /// 4.66.4.3.3), so the answer comes from the persisted code rather than
    /// from whichever surface the client happened to use.
    #[inline]
    pub fn is_exposable(&self) -> bool {
        self.rejection_reason == QUALITY_REASON_CODE
    }
}

/// A single CAS frame chunk.
///
/// `#[repr(C)]` pins the field order (zero-padding
/// compile-time gate via `assert_no_padding!`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(C)]
pub struct FrameRecord {
    /// 1-based frame number within the instance.
    ///
    /// `u32`, not `u16`: a whole-slide instance routinely holds more than
    /// 65535 tiles (PS3.3 A.32.8 allows a total pixel matrix up to 2^32), and
    /// a narrower field forces the writer to DROP every frame above that,
    /// which silently degrades tile serving to a whole-blob read.
    pub number: u32,
    /// BLAKE3 hash of the compressed frame bytes (32 bytes).
    pub hash: [u8; 32],
    /// Byte offset of the frame within the CAS blob (`0` for a single blob).
    pub offset: u64,
    /// Compressed length of this frame in bytes.
    pub compressed_len: u64,
}

/// The complete study manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// System header.
    pub sys: SysHeader,
    /// Study-level QIDO search keys.
    pub study: StudyQido,
    /// Series, in STOW order.
    pub series: Vec<SeriesRecord>,
}

/// Errors produced while encoding/decoding a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// Underlying CBOR codec failure.
    Cbor(CborError),
    /// A map key outside the expected range for the current record type.
    UnknownKey(u64),
    /// A value did not match the shape expected for its key.
    UnexpectedValue(&'static str),
    /// The DICOM tag in an attribute map is not a valid 32-bit value.
    InvalidTag(u64),
    /// A BLAKE3 hash had the wrong length (must be 32 bytes).
    InvalidHashLen(usize),
    /// The manifest envelope (top-level map) was malformed.
    MalformedEnvelope(&'static str),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Cbor(e) => write!(f, "manifest CBOR error: {e}"),
            ManifestError::UnknownKey(k) => write!(f, "unknown manifest key {k}"),
            ManifestError::UnexpectedValue(expected) => {
                write!(f, "unexpected manifest value, expected {expected}")
            }
            ManifestError::InvalidTag(t) => write!(f, "invalid DICOM tag 0x{t:08x}"),
            ManifestError::InvalidHashLen(n) => write!(f, "invalid BLAKE3 hash length {n}"),
            ManifestError::MalformedEnvelope(why) => write!(f, "malformed manifest: {why}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<CborError> for ManifestError {
    fn from(e: CborError) -> Self {
        ManifestError::Cbor(e)
    }
}

impl Manifest {
    /// Encode this manifest into `buf`, returning the number of bytes written.
    ///
    /// Falls back to [`ManifestError::Cbor(CborError::BufferFull)`] when `buf`
    /// is too small.
    pub fn encode_to<'a>(&self, writer: &mut CborWriter<'a>) -> Result<(), ManifestError> {
        // 3 top-level entries: sys, qido_study, series.
        writer.write_map_header(3)?;
        writer.write_u64(KEY_SYS)?;
        encode_sys(writer, &self.sys)?;
        writer.write_u64(KEY_QIDO_STUDY)?;
        encode_study(writer, &self.study)?;
        writer.write_u64(KEY_SERIES)?;
        writer.write_array_header(self.series.len() as u64)?;
        for series in &self.series {
            encode_series(writer, series)?;
        }
        Ok(())
    }

    /// Encode into a freshly allocated `Vec<u8>`.
    ///
    /// Allocation happens here, at the manifest boundary, not inside the
    /// CBOR codec. Use [`Manifest::encode_to`] with a fixed stack buffer when
    /// the hotpath matters.
    pub fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        let capacity = self.encoded_size_hint();
        let mut buf = vec![0u8; capacity];
        let written = {
            let mut writer = CborWriter::new(&mut buf);
            self.encode_to(&mut writer)?;
            writer.written()
        };
        buf.truncate(written);
        Ok(buf)
    }

    /// Parse a manifest from `buf` and return the decoded struct.
    ///
    /// The manifest is expected to occupy the whole slice; trailing bytes
    /// are rejected to catch truncated or appended data.
    pub fn decode(buf: &[u8]) -> Result<Manifest, ManifestError> {
        let mut reader = CborReader::new(buf);
        let manifest = decode_envelope(&mut reader, Depth::Full)?;
        if !reader.is_empty() {
            return Err(ManifestError::Cbor(CborError::TrailingData));
        }
        Ok(manifest)
    }

    /// Parse **only** the study-level QIDO attributes from a manifest,
    /// skipping all series, instance, and frame records.
    ///
    /// This is the QIDO hotpath: the CBOR decoder reads `KEY_SYS` and
    /// `KEY_QIDO_STUDY`, then calls [`CborReader::skip_value`] for
    /// `KEY_SERIES` - the recursive skip avoids parsing hundreds of
    /// instance and frame records that QIDO never inspects. For a
    /// 432-instance manifest this reduces CBOR-decode time from ~10 us
    /// to ~2 us.
    ///
    /// Trailing bytes are accepted (unlike [`Manifest::decode`]) because
    /// we deliberately leave them unread - the `skip_value` positions the
    /// reader past the series array but may leave the buffer non-empty.
    pub fn decode_study_only(buf: &[u8]) -> Result<Manifest, ManifestError> {
        let mut reader = CborReader::new(buf);
        decode_envelope(&mut reader, Depth::Study)
    }

    /// Parse the system header, study-level QIDO attributes, **and** every
    /// series record from a manifest, skipping the instance and frame records
    /// inside each series via [`CborReader::skip_value`].
    ///
    /// This is the series-QIDO shallow level: a caller that needs the
    /// series attributes but not the instance/frame payloads avoids decoding
    /// hundreds of instance and frame records. The returned
    /// [`SeriesRecord::instances`] vectors are empty (the instance arrays were
    /// skipped, not parsed); the series `attrs` are byte-identical to a full
    /// [`Manifest::decode`] of the same manifest.
    ///
    /// Like [`Manifest::decode_study_only`], trailing bytes are accepted.
    pub fn decode_to_series(buf: &[u8]) -> Result<Manifest, ManifestError> {
        let mut reader = CborReader::new(buf);
        decode_envelope(&mut reader, Depth::Series)
    }

    /// A conservative upper bound on the encoded size.
    ///
    /// Used to size the output buffer so `encode` never re-allocates.
    ///
    /// The bound must hold for EVERY field the encoders write, because an
    /// under-count makes `CborWriter` return `BufferFull` for a manifest the
    /// decoder would happily read - a valid study that cannot be stored.
    /// Counting attribute VALUES alone was exactly that under-count: it
    /// ignored the map keys, the array headers, and the per-record system
    /// keys, so a study of 100 five-byte-keyed `UInt(u64::MAX)` attributes
    /// estimated 1330 bytes for a payload needing 1400, and 100 EMPTY series
    /// contributed nothing while still costing a map header and an
    /// instances-array header each.
    ///
    /// The estimate is built from the same numbers the encoders use rather
    /// than from a running total that has to be re-audited every time a field
    /// is added: every map/array header is charged at the encoder's own
    /// maximum (`write_head` for a 64-bit length is 9 bytes) and every key at
    /// its maximum too, with a 25% margin on top.
    fn encoded_size_hint(&self) -> usize {
        // A CBOR length/argument header is at most 9 bytes (major type + the
        // 8-byte 64-bit form); charging every header the maximum is what makes
        // this a bound rather than an estimate.
        const HEAD: usize = 9;
        // Any key this codec writes - a system key or a 32-bit DICOM tag -
        // encodes in at most `HEAD` bytes.
        const KEY: usize = HEAD;

        // Envelope: map header + 3 keys, then each child's own header.
        let mut total = HEAD + 3 * KEY;

        // sys: map header (4 entries) + 4 key/value pairs.
        total += HEAD + 4 * (KEY + HEAD);

        // qido_study: map header + one key and one value per attribute.
        total += HEAD + self.study.attrs.len() * KEY;
        for (_, v) in &self.study.attrs {
            total += attr_encoded_size(v);
        }

        // series array header, then each series record.
        total += HEAD;
        for series in &self.series {
            // attrs + the instances system key.
            total += HEAD + (series.attrs.len() + 1) * KEY;
            for (_, v) in &series.attrs {
                total += attr_encoded_size(v);
            }
            // The instances array header, then each instance record.
            total += HEAD;
            for instance in &series.instances {
                // attrs + transfer syntax + tier + hash + file size + frames,
                // plus the rejection reason when the instance is sequestered.
                let system_keys = 5 + usize::from(instance.rejection_reason != NOT_REJECTED_REASON);
                total += HEAD + (instance.attrs.len() + system_keys) * KEY;
                for (_, v) in &instance.attrs {
                    total += attr_encoded_size(v);
                }
                // transfer syntax text, the 32-byte hash, and the scalar
                // values behind the remaining system keys.
                total += instance.transfer_syntax.len() + HEAD;
                total += HEAD + 32;
                total += 3 * HEAD;
                // Frames array header, then each 4-entry frame map.
                total += HEAD;
                total += instance.frames.len() * (HEAD + 4 * (KEY + HEAD + 32));
            }
        }
        total + total / 4
    }
}

/// Size of an attribute value in bytes once encoded, including its tag key.
///
/// Every branch must be a conservative upper bound on `encode_attr_value`
/// plus the 5-byte `write_u64(tag)` key the enclosing `encode_study` /
/// `encode_series` / `encode_instance` loops write for the attribute. An
/// over-count only wastes buffer bytes (the buffer is truncated to `written`);
/// an under-count makes `CborWriter` return `BufferFull` for a manifest the
/// decoder accepts, so the "conservative upper bound" claim in
/// [`Manifest::encoded_size_hint`] would be a lie.
fn attr_encoded_size(v: &AttrValue) -> usize {
    match v {
        // Tag key (5) + write_u64 value. A u32-range value encodes as a 1-byte
        // CBOR header + 4-byte argument = 5 bytes (cbor/encode.rs `write_head`
        // 0x1a form), so a UInt attribute is 10 bytes, not 9.
        AttrValue::UInt(_) => 10,
        AttrValue::Text(s) => s.len() + 9,
        AttrValue::Bytes(b) => b.len() + 9,
        // Tag key (5) + array header allowance (2) + each unkeyed item. The
        // per-item recursion adds a key per item, over-counting each, which is
        // safe - the items are scalars with no key of their own.
        AttrValue::Array(items) => 5 + 2 + items.iter().map(attr_encoded_size).sum::<usize>(),
    }
}

// -- Encoders ------------------------------------------------------------

fn encode_sys(w: &mut CborWriter<'_>, sys: &SysHeader) -> Result<(), ManifestError> {
    // Mirror the decoder's bounds (read_sys below): a value the decoder will
    // reject must fail HERE, before any bytes are written, so the publish path
    // never renames a poisoned manifest into place. The instance model's
    // `Default` leaves `storage_tier` at 0, and `build_manifest_full`
    // overwrites its TIER_HOT seed with that field - encoding it as-is would
    // put a tier-0 header on disk that every later read fails to decode.
    if sys.version != MANIFEST_VERSION {
        return Err(ManifestError::MalformedEnvelope("unsupported sys version"));
    }
    if !matches!(sys.storage_tier, TIER_HOT | TIER_COLD | TIER_S3) {
        return Err(ManifestError::MalformedEnvelope("unknown storage tier"));
    }
    w.write_map_header(4)?;
    w.write_u64(SYS_VERSION)?;
    w.write_u64(sys.version as u64)?;
    w.write_u64(SYS_STORAGE_TIER)?;
    w.write_u64(sys.storage_tier as u64)?;
    w.write_u64(SYS_TOTAL_PIXEL_BYTES)?;
    w.write_u64(sys.total_pixel_bytes)?;
    w.write_u64(SYS_ARRIVED_AT)?;
    w.write_u64(sys.arrived_at)?;
    Ok(())
}

fn encode_study(w: &mut CborWriter<'_>, study: &StudyQido) -> Result<(), ManifestError> {
    w.write_map_header(study.attrs.len() as u64)?;
    for (tag, val) in &study.attrs {
        ensure_tag(*tag)?;
        w.write_u64(*tag as u64)?;
        encode_attr_value(w, val)?;
    }
    Ok(())
}

fn encode_series(w: &mut CborWriter<'_>, series: &SeriesRecord) -> Result<(), ManifestError> {
    // attrs + the instances system key.
    w.write_map_header((series.attrs.len() + 1) as u64)?;
    for (tag, val) in &series.attrs {
        ensure_tag(*tag)?;
        w.write_u64(*tag as u64)?;
        encode_attr_value(w, val)?;
    }
    w.write_u64(SERIES_INSTANCES)?;
    w.write_array_header(series.instances.len() as u64)?;
    for instance in &series.instances {
        encode_instance(w, instance)?;
    }
    Ok(())
}

fn encode_instance(w: &mut CborWriter<'_>, instance: &InstanceRecord) -> Result<(), ManifestError> {
    // attrs + transfer syntax + storage tier + blake3 + file size + frames
    // system keys, plus the rejection reason only when the instance is
    // sequestered.
    w.write_map_header(
        (instance.attrs.len() + 5 + usize::from(instance.rejection_reason != NOT_REJECTED_REASON))
            as u64,
    )?;
    for (tag, val) in &instance.attrs {
        ensure_tag(*tag)?;
        w.write_u64(*tag as u64)?;
        encode_attr_value(w, val)?;
    }
    w.write_u64(INSTANCE_STORAGE_TIER)?;
    w.write_u64(instance.storage_tier as u64)?;
    w.write_u64(INSTANCE_TRANSFER_SYNTAX)?;
    w.write_text(&instance.transfer_syntax)?;
    w.write_u64(INSTANCE_BLAKE3)?;
    w.write_bytes(&instance.blake3)?;
    w.write_u64(INSTANCE_FILE_SIZE)?;
    w.write_u64(instance.file_size)?;
    w.write_u64(INSTANCE_FRAMES)?;
    w.write_array_header(instance.frames.len() as u64)?;
    for frame in &instance.frames {
        encode_frame(w, frame)?;
    }
    if instance.rejection_reason != NOT_REJECTED_REASON {
        w.write_u64(INSTANCE_REJECTION_REASON)?;
        w.write_u64(u64::from(instance.rejection_reason))?;
    }
    Ok(())
}

fn encode_frame(w: &mut CborWriter<'_>, frame: &FrameRecord) -> Result<(), ManifestError> {
    w.write_map_header(4)?;
    w.write_u64(FRAME_NUMBER)?;
    // CBOR integer encoding is variable-length, so widening `number` from u16
    // to u32 changes NO bytes for a value that fits in a u16 - existing
    // manifests stay byte-identical.
    w.write_u64(u64::from(frame.number))?;
    w.write_u64(FRAME_HASH)?;
    w.write_bytes(&frame.hash)?;
    w.write_u64(FRAME_OFFSET)?;
    w.write_u64(frame.offset)?;
    w.write_u64(FRAME_COMPRESSED_LEN)?;
    w.write_u64(frame.compressed_len)?;
    Ok(())
}

fn encode_attr_value(w: &mut CborWriter<'_>, val: &AttrValue) -> Result<(), ManifestError> {
    match val {
        AttrValue::UInt(v) => w.write_u64(*v)?,
        AttrValue::Text(s) => w.write_text(s)?,
        AttrValue::Bytes(b) => w.write_bytes(b)?,
        AttrValue::Array(items) => {
            w.write_array_header(items.len() as u64)?;
            for item in items {
                encode_attr_value(w, item)?;
            }
        }
    }
    Ok(())
}

fn ensure_tag(tag: u32) -> Result<(), ManifestError> {
    // A DICOM tag is a 32-bit group/element pair; the smallest group a data
    // set can carry is 0002 (PS3.5 7.1 - group 0000 is command, 0002 is file
    // meta), so every real tag is >= 0x0002_0000. The manifest's own system
    // keys are small integers 1..=10 at the SAME nesting level as the
    // attributes, which is why a caller-supplied attribute key below that
    // floor would be written TWICE into one map: once from the attribute list
    // and once by the record's own system-key writer.
    //
    // CBOR permits a duplicate map key but says nothing about which value
    // wins (RFC 8949 5.6), and this decoder keeps the one it reads - so a
    // series attribute named `SERIES_INSTANCES` (key 4) would either be
    // discarded or be mistaken for a second instances array. Refusing it at
    // ENCODE time is the only place the ambiguity can be removed without
    // changing the wire format.
    if tag < 0x0002_0000 {
        return Err(ManifestError::MalformedEnvelope(
            "attribute key collides with a manifest system key",
        ));
    }
    Ok(())
}

// -- Decoders ------------------------------------------------------------

/// How deeply `decode_envelope` parses a manifest.
///
/// - [`Depth::Study`]: system header + study QIDO attrs; the whole series
///   array is skipped (study-level QIDO hotpath).
/// - [`Depth::Series`]: system header + study QIDO attrs + the series attrs;
///   each series' instance/frame records are skipped (series-QIDO shallow).
/// - [`Depth::Full`]: the complete manifest (series, instances, frames).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Depth {
    Study,
    Series,
    Full,
}

fn decode_envelope(r: &mut CborReader<'_>, depth: Depth) -> Result<Manifest, ManifestError> {
    let head = r.next()?;
    let entries = match head {
        CborToken::MapStart(n) => n,
        other => {
            return Err(ManifestError::MalformedEnvelope(CborToken::describe(
                &other,
            )));
        }
    };
    // Top-level map has exactly the 3 known entries (shallow or full).
    if entries != 3 {
        return Err(ManifestError::MalformedEnvelope("top-level map size != 3"));
    }

    let mut sys: Option<SysHeader> = None;
    let mut study: Option<StudyQido> = None;
    let mut series: Vec<SeriesRecord> = Vec::new();
    let mut seen_study = false;
    // The three envelope keys must each appear EXACTLY once. The entry count
    // alone does not prove that: a map of {SYS, QIDO_STUDY, QIDO_STUDY} also
    // has three entries, passes every per-key check, and yields a manifest
    // with no series at all. Tracking presence per key is what makes the
    // "== 3" count mean what it claims, in any key order.
    let mut seen_sys = false;
    let mut seen_series = false;
    // RAM-only derived instance count, accumulated across every decoded /
    // header-skipped series so 0020,1208 is consistent at any decode depth.
    let mut instance_acc: u64 = 0;
    // Study aggregates computed from the series array BEFORE the study map was
    // read, held here until it exists.
    //
    // CBOR maps reach this loop in whatever order the writer used, so
    // KEY_SERIES can legally precede KEY_QIDO_STUDY. Computing the counts and
    // writing them straight onto `study` silently dropped them whenever the
    // order was reversed - a manifest with one CT series reported 0 series, 0
    // instances and no modality at study depth while the full decode of the
    // same bytes reported 1, 1, "CT".
    let mut pending_section: Option<(u64, u64, String)> = None;

    for _ in 0..entries {
        let key = match r.next()? {
            CborToken::UInt(k) => k,
            _ => return Err(ManifestError::MalformedEnvelope("top-level key not uint")),
        };
        match key {
            KEY_SYS => {
                if seen_sys {
                    return Err(ManifestError::MalformedEnvelope("duplicate sys header"));
                }
                seen_sys = true;
                let sys_head = r.next()?;
                let n = match sys_head {
                    CborToken::MapStart(n) => n,
                    _ => return Err(ManifestError::UnexpectedValue("system map")),
                };
                sys = Some(read_sys(r, n)?);
            }
            KEY_QIDO_STUDY => {
                if seen_study {
                    return Err(ManifestError::MalformedEnvelope("duplicate study map"));
                }
                seen_study = true;
                let study_head = r.next()?;
                let n = match study_head {
                    CborToken::MapStart(n) => n,
                    _ => return Err(ManifestError::UnexpectedValue("study map")),
                };
                study = Some(read_study(r, n)?);
            }
            KEY_SERIES => {
                if seen_series {
                    return Err(ManifestError::MalformedEnvelope("duplicate series array"));
                }
                seen_series = true;
                if depth == Depth::Study {
                    // QIDO study hotpath: derive the series / instance counts
                    // and the ModalitiesInStudy aggregate from the array + map
                    // HEADERS only, without tokenizing element payloads
                    // (zero-alloc, no extra I/O - the same mmap is already
                    // decoded). The counts + aggregate are injected onto the
                    // in-memory `StudyQido` for /studies (0020,1206 / 0020,1208
                    // / 0008,0061); the CBOR schema is unchanged.
                    let arr_head = r.next()?;
                    let n_series = match arr_head {
                        CborToken::ArrayStart(n) => n,
                        _ => return Err(ManifestError::UnexpectedValue("series array")),
                    };
                    let mut n_instances: u64 = 0;
                    // One joined String per study (Mirror of how the SQLite-era
                    // `GROUP_CONCAT(DISTINCT modality)` aggregated series
                    // modalities; B-tree fixes first-occurrence order).
                    let mut modality_agg = String::new();
                    for _ in 0..n_series {
                        let mh = r.next()?;
                        let m = match mh {
                            CborToken::MapStart(m) => m,
                            _ => return Err(ManifestError::UnexpectedValue("series map")),
                        };
                        for _ in 0..m {
                            let key = expect_u64_key(r)?;
                            if key == SERIES_INSTANCES {
                                let ah = r.next()?;
                                let k = match ah {
                                    CborToken::ArrayStart(k) => k,
                                    _ => {
                                        return Err(ManifestError::UnexpectedValue(
                                            "instances array",
                                        ));
                                    }
                                };
                                n_instances = n_instances.saturating_add(k);
                                // `r.next()` above already consumed the
                                // `ArrayStart(k)` header; skip exactly `k`
                                // instance records (never tokenized).
                                for _ in 0..k {
                                    r.skip_value()?;
                                }
                            } else if key == MODALITY_TAG {
                                // Series Modality (0008,0060): read the text
                                // value (borrowed, zero-alloc) and aggregate
                                // distinct modalities in first-occurrence order.
                                //
                                // Anything that is NOT a text value must still
                                // be consumed WHOLE, or the reader parks on the
                                // modality's own bytes and the rest of the
                                // series map decodes as garbage. `next()` already
                                // consumed a scalar UInt/Bytes whole, so nothing
                                // remains; for ArrayStart/MapStart it consumed
                                // only the HEADER, so the elements must be
                                // skipped explicitly. The full-decode path
                                // (stamp_study_aggregates) drops a non-text
                                // modality the same way - it contributes nothing
                                // to ModalitiesInStudy.
                                match r.next()? {
                                    CborToken::Text(s) => {
                                        append_distinct_modality(&mut modality_agg, s)
                                    }
                                    // `next()` consumed the whole scalar - nothing remains.
                                    CborToken::UInt(_) | CborToken::Bytes(_) => {}
                                    // `next()` consumed only the header; skip exactly n elements.
                                    CborToken::ArrayStart(n) => {
                                        for _ in 0..n {
                                            r.skip_value()?;
                                        }
                                    }
                                    // MapStart cannot be produced by encode_attr_value, but
                                    // skip it correctly (2n items) rather than desyncing, for a
                                    // hostile/corrupt manifest.
                                    CborToken::MapStart(n) => {
                                        for _ in 0..n {
                                            r.skip_value()?; // key
                                            r.skip_value()?; // value
                                        }
                                    }
                                }
                            } else {
                                // Skip a series attribute value - not needed.
                                r.skip_value()?;
                            }
                        }
                    }
                    // Held until the study map exists (see `pending_section`).
                    pending_section = Some((n_series, n_instances, modality_agg));
                } else {
                    let arr_head = r.next()?;
                    let n = match arr_head {
                        CborToken::ArrayStart(n) => n,
                        _ => return Err(ManifestError::UnexpectedValue("series array")),
                    };
                    for _ in 0..n {
                        series.push(read_series(r, depth, &mut instance_acc)?);
                    }
                }
            }
            other => return Err(ManifestError::UnknownKey(other)),
        }
    }

    let sys = sys.ok_or(ManifestError::MalformedEnvelope("missing sys header"))?;
    if !seen_study {
        return Err(ManifestError::MalformedEnvelope("missing study keys"));
    }
    if !seen_series {
        return Err(ManifestError::MalformedEnvelope("missing series array"));
    }
    let mut study = study.unwrap_or_default();
    // Apply the study-depth aggregates NOW, after the loop, so the answer does
    // not depend on whether KEY_SERIES preceded KEY_QIDO_STUDY.
    if let Some((n_series, n_instances, modality_agg)) = pending_section {
        study.series_count = n_series;
        study.instance_count = n_instances;
        study.modalities_in_study = if modality_agg.is_empty() {
            None
        } else {
            Some(modality_agg)
        };
    }
    // Derived study counts + ModalitiesInStudy (RAM-only, never persisted to
    // CBOR). At Series/Full depth the series array is decoded, so series_count
    // comes from it, instance_count from the accumulation across each series'
    // instance arrays, and ModalitiesInStudy from aggregating the decoded
    // series modality attrs in first-occurrence order.
    if depth == Depth::Series || depth == Depth::Full {
        study.series_count = series.len() as u64;
        study.instance_count = instance_acc;
        let mut modality_agg = String::new();
        for s in &series {
            if let Some(AttrValue::Text(m)) = s
                .attrs
                .iter()
                .find(|(t, _)| *t as u64 == MODALITY_TAG)
                .map(|(_, v)| v)
            {
                append_distinct_modality(&mut modality_agg, m);
            }
        }
        study.modalities_in_study = if modality_agg.is_empty() {
            None
        } else {
            Some(modality_agg)
        };
    }
    Ok(Manifest { sys, study, series })
}

fn read_sys(r: &mut CborReader<'_>, n: u64) -> Result<SysHeader, ManifestError> {
    let mut version: Option<u8> = None;
    let mut tier: Option<u8> = None;
    let mut total: Option<u64> = None;
    // Additive key: absent on old manifests -> 0 (sorts last, never a panic).
    let mut arrived: u64 = 0;

    // The map start supplied the entry count; any of the known keys may
    // appear in any position. Verify the required ones after the loop.
    for _ in 0..n {
        let key = expect_u64_key(r)?;
        match key {
            // A narrowing `as u8` would turn 257 into 1, so an out-of-range
            // version or tier would decode as a LEGAL one - the exact case the
            // tier/version checks below exist to catch. Convert with `try_from`
            // so the range check happens before the value loses its high bits.
            SYS_VERSION => version = Some(narrow_u8(expect_u64_value(r)?, "sys version")?),
            SYS_STORAGE_TIER => tier = Some(narrow_u8(expect_u64_value(r)?, "sys storage tier")?),
            SYS_TOTAL_PIXEL_BYTES => total = Some(expect_u64_value(r)?),
            SYS_ARRIVED_AT => arrived = expect_u64_value(r)?,
            other => return Err(ManifestError::UnknownKey(other)),
        }
    }

    let version = version.ok_or(ManifestError::MalformedEnvelope("missing sys version"))?;
    let storage_tier = tier.ok_or(ManifestError::MalformedEnvelope("missing storage tier"))?;
    let total_pixel_bytes = total.ok_or(ManifestError::MalformedEnvelope(
        "missing total_pixel_bytes",
    ))?;

    // Version and tier are bounded; reject unknown values so a future format
    // never gets silently misread. The version check is `!=`, not `>=`: the
    // documented format is exactly version 1, and accepting "anything above 0"
    // would let a version-2 writer's manifest decode as a version-1 manifest
    // with whatever fields happen to line up.
    if version != MANIFEST_VERSION {
        return Err(ManifestError::MalformedEnvelope("unsupported sys version"));
    }
    if !matches!(storage_tier, TIER_HOT | TIER_COLD | TIER_S3) {
        return Err(ManifestError::MalformedEnvelope("unknown storage tier"));
    }

    Ok(SysHeader {
        version,
        storage_tier,
        total_pixel_bytes,
        arrived_at: arrived,
    })
}

fn read_study(r: &mut CborReader<'_>, n: u64) -> Result<StudyQido, ManifestError> {
    // The count comes from the file and can be anything a corrupt or hostile
    // CBOR header claims - `u64::MAX` would abort the process inside
    // `with_capacity`. Each entry needs at least two bytes (a key and a value),
    // so the reader's remaining length is the real ceiling; the Vec still grows
    // on demand, this only bounds the UP-FRONT allocation.
    let cap = bounded_capacity(n, r.remaining() / 2);
    let mut attrs = Vec::with_capacity(cap);
    for _ in 0..n {
        let key = expect_u64_key(r)?;
        let tag = u32::try_from(key).map_err(|_| ManifestError::InvalidTag(key))?;
        let value = read_attr_value(r)?;
        attrs.push((tag, value));
    }
    Ok(StudyQido {
        attrs,
        ..Default::default()
    })
}

/// A `with_capacity` hint that a corrupt length cannot turn into an abort.
///
/// `min(declared, bound)`: the declared count is what the file says, `bound` is
/// what the remaining bytes could actually hold. Anything larger is a lie, and
/// allocating on it is exactly the amplification a crafted record wants.
fn bounded_capacity(declared: u64, bound: usize) -> usize {
    let bound = bound as u64;
    declared.min(bound).min(usize::MAX as u64) as usize
}

/// Convert a decoded CBOR unsigned value to `u8`, rejecting anything that does
/// not fit.
///
/// A plain `as u8` truncates, so 257 decodes as 1 - a value the range checks
/// downstream then ACCEPT, because by the time they see it the evidence is
/// gone. `what` names the field for the error.
fn narrow_u8(v: u64, what: &'static str) -> Result<u8, ManifestError> {
    u8::try_from(v).map_err(|_| ManifestError::MalformedEnvelope(what))
}

/// Convert a decoded CBOR unsigned value to `u32`, rejecting anything wider.
fn narrow_u32(v: u64, what: &'static str) -> Result<u32, ManifestError> {
    u32::try_from(v).map_err(|_| ManifestError::MalformedEnvelope(what))
}

/// Append `m` to a comma-joined ModalitiesInStudy aggregate if not already
/// present (first-occurrence distinct order, mirroring the SQLite-era
/// `GROUP_CONCAT(DISTINCT modality)` value set; the SQL ordering was not
/// contractual - B-tree fixes first-occurrence order).
fn append_distinct_modality(agg: &mut String, m: &str) {
    if !agg.split(',').any(|x| x == m) {
        if !agg.is_empty() {
            agg.push(',');
        }
        agg.push_str(m);
    }
}

fn read_series(
    r: &mut CborReader<'_>,
    depth: Depth,
    instance_acc: &mut u64,
) -> Result<SeriesRecord, ManifestError> {
    let head = r.next()?;
    let n = match head {
        CborToken::MapStart(n) => n,
        _ => return Err(ManifestError::UnexpectedValue("series map")),
    };

    let mut attrs = Vec::new();
    let mut instances = Vec::new();
    let mut seen_instances = false;

    for _ in 0..n {
        let key = expect_u64_key(r)?;
        if key == SERIES_INSTANCES {
            seen_instances = true;
            if depth == Depth::Series {
                // Series-QIDO shallow: drop the instance/frame payloads but
                // still count them from the array header (RAM-only derived
                // count 0020,1208 stays consistent across decode depths).
                let arr = r.next()?;
                let count = match arr {
                    CborToken::ArrayStart(k) => k,
                    _ => return Err(ManifestError::UnexpectedValue("instances array")),
                };
                *instance_acc = instance_acc.saturating_add(count);
                // EVERY instance map, not one: `skip_value` consumes a single
                // item, so a series with two or more instances left the reader
                // mid-array and the enclosing map decoded as garbage. This is
                // the same loop `Depth::Study` uses for the same header.
                for _ in 0..count {
                    r.skip_value()?;
                }
                continue;
            }
            let arr = r.next()?;
            let count = match arr {
                CborToken::ArrayStart(n) => n,
                _ => return Err(ManifestError::UnexpectedValue("instances array")),
            };
            *instance_acc = instance_acc.saturating_add(count);
            for _ in 0..count {
                instances.push(read_instance(r)?);
            }
        } else {
            let tag = u32::try_from(key).map_err(|_| ManifestError::InvalidTag(key))?;
            let value = read_attr_value(r)?;
            attrs.push((tag, value));
        }
    }

    if depth != Depth::Series && !seen_instances {
        return Err(ManifestError::MalformedEnvelope("series missing instances"));
    }
    Ok(SeriesRecord { attrs, instances })
}

fn read_instance(r: &mut CborReader<'_>) -> Result<InstanceRecord, ManifestError> {
    let head = r.next()?;
    let n = match head {
        CborToken::MapStart(n) => n,
        _ => return Err(ManifestError::UnexpectedValue("instance map")),
    };

    let mut attrs = Vec::new();
    let mut transfer_syntax = String::new();
    let mut storage_tier: Option<u8> = None;
    let mut blake3: Option<[u8; 32]> = None;
    let mut file_size: Option<u64> = None;
    let mut frames = Vec::new();
    let mut rejection_reason: u32 = NOT_REJECTED_REASON;
    let mut seen_ts = false;
    let mut seen_frames = false;

    for _ in 0..n {
        let key = expect_u64_key(r)?;
        match key {
            INSTANCE_STORAGE_TIER => {
                storage_tier = Some(narrow_u8(expect_u64_value(r)?, "instance storage tier")?)
            }
            INSTANCE_TRANSFER_SYNTAX => {
                seen_ts = true;
                transfer_syntax = expect_text_value(r)?;
            }
            INSTANCE_BLAKE3 => blake3 = Some(expect_hash_value(r)?),
            INSTANCE_FILE_SIZE => file_size = Some(expect_u64_value(r)?),
            INSTANCE_FRAMES => {
                seen_frames = true;
                let arr = r.next()?;
                let count = match arr {
                    CborToken::ArrayStart(n) => n,
                    _ => return Err(ManifestError::UnexpectedValue("frames array")),
                };
                for _ in 0..count {
                    frames.push(read_frame(r)?);
                }
            }
            INSTANCE_REJECTION_REASON => {
                // The stored value is the reason code itself, so a reader that
                // does not know a particular code still preserves it. Anything
                // wider than u32 cannot have been written by a writer of this
                // format, which is worse than a failed decode.
                rejection_reason = match expect_u64_value(r)? {
                    v if v <= u64::from(u32::MAX) => v as u32,
                    _ => {
                        return Err(ManifestError::UnexpectedValue(
                            "instance rejection reason must fit in 32 bits",
                        ));
                    }
                };
            }
            _ => {
                let tag = u32::try_from(key).map_err(|_| ManifestError::InvalidTag(key))?;
                let value = read_attr_value(r)?;
                attrs.push((tag, value));
            }
        }
    }

    if !seen_ts {
        return Err(ManifestError::MalformedEnvelope(
            "instance missing transfer syntax",
        ));
    }
    if !seen_frames {
        return Err(ManifestError::MalformedEnvelope("instance missing frames"));
    }
    let storage_tier = storage_tier.ok_or(ManifestError::MalformedEnvelope(
        "instance missing storage tier",
    ))?;
    // The same tier vocabulary as the system header, enforced the same way.
    // Accepting tier 0 (the `Default`) or an out-of-range value would hand an
    // impossible tier to WADO lookup, which branches on it to choose the
    // storage backend.
    if !matches!(storage_tier, TIER_HOT | TIER_COLD | TIER_S3) {
        return Err(ManifestError::MalformedEnvelope("unknown storage tier"));
    }
    Ok(InstanceRecord {
        attrs,
        transfer_syntax,
        storage_tier,
        blake3: blake3.ok_or(ManifestError::MalformedEnvelope("instance missing blake3"))?,
        file_size: file_size.ok_or(ManifestError::MalformedEnvelope(
            "instance missing file size",
        ))?,
        frames,
        rejection_reason,
    })
}

fn read_frame(r: &mut CborReader<'_>) -> Result<FrameRecord, ManifestError> {
    let head = r.next()?;
    let n = match head {
        CborToken::MapStart(n) => n,
        _ => return Err(ManifestError::UnexpectedValue("frame map")),
    };

    let mut number: Option<u32> = None;
    let mut hash: Option<[u8; 32]> = None;
    let mut offset: Option<u64> = None;
    let mut compressed_len: Option<u64> = None;

    for _ in 0..n {
        let key = expect_u64_key(r)?;
        match key {
            // `as u32` would turn 4_294_967_297 into frame 1, aliasing two
            // different frames onto one number in a table that lookups walk
            // linearly. Convert with `try_from` so the overflow is a decode
            // error, not a silently different frame.
            FRAME_NUMBER => number = Some(narrow_u32(expect_u64_value(r)?, "frame number")?),
            FRAME_HASH => {
                hash = Some(expect_hash_value(r)?);
            }
            FRAME_OFFSET => offset = Some(expect_u64_value(r)?),
            FRAME_COMPRESSED_LEN => compressed_len = Some(expect_u64_value(r)?),
            other => return Err(ManifestError::UnknownKey(other)),
        }
    }

    // The frame table is ONE-BASED: frame 0 does not exist, and a reader that
    // asked for frame 0 is already outside the contract. Storing it here would
    // make the table satisfy that lookup instead of failing it.
    let number = number.ok_or(ManifestError::MalformedEnvelope("frame missing number"))?;
    if number == 0 {
        return Err(ManifestError::MalformedEnvelope(
            "frame number is one-based and cannot be 0",
        ));
    }
    Ok(FrameRecord {
        number,
        hash: hash.ok_or(ManifestError::MalformedEnvelope("frame missing hash"))?,
        offset: offset.ok_or(ManifestError::MalformedEnvelope("frame missing offset"))?,
        compressed_len: compressed_len.ok_or(ManifestError::MalformedEnvelope(
            "frame missing compressed_len",
        ))?,
    })
}

// -- Shared decode helpers -----------------------------------------------

fn expect_u64_key(r: &mut CborReader<'_>) -> Result<u64, ManifestError> {
    match r.next()? {
        CborToken::UInt(k) => Ok(k),
        _ => Err(ManifestError::MalformedEnvelope("map key not uint")),
    }
}

fn expect_u64_value(r: &mut CborReader<'_>) -> Result<u64, ManifestError> {
    match r.next()? {
        CborToken::UInt(v) => Ok(v),
        _ => Err(ManifestError::UnexpectedValue("uint value")),
    }
}

fn expect_text_value(r: &mut CborReader<'_>) -> Result<String, ManifestError> {
    match r.next()? {
        CborToken::Text(s) => Ok(s.to_owned()),
        _ => Err(ManifestError::UnexpectedValue("text value")),
    }
}

fn expect_hash_value(r: &mut CborReader<'_>) -> Result<[u8; 32], ManifestError> {
    match r.next()? {
        CborToken::Bytes(b) => {
            let arr: [u8; 32] = b
                .try_into()
                .map_err(|_| ManifestError::InvalidHashLen(b.len()))?;
            Ok(arr)
        }
        _ => Err(ManifestError::UnexpectedValue("byte hash")),
    }
}

fn read_attr_value(r: &mut CborReader<'_>) -> Result<AttrValue, ManifestError> {
    read_attr_value_at(r, 0)
}

/// [`read_attr_value`] with a nesting counter.
///
/// A nested array attribute is legal CBOR at any depth, so an unbounded
/// recursion on a crafted manifest overflows the stack and aborts the process.
/// [`crate::cbor::MAX_NESTING_DEPTH`] is the same bound `skip_value` uses.
fn read_attr_value_at(r: &mut CborReader<'_>, depth: usize) -> Result<AttrValue, ManifestError> {
    if depth > crate::cbor::MAX_NESTING_DEPTH {
        return Err(ManifestError::UnexpectedValue("attribute nesting too deep"));
    }
    match r.next()? {
        CborToken::UInt(v) => Ok(AttrValue::UInt(v)),
        CborToken::Text(s) => Ok(AttrValue::Text(s.to_owned())),
        CborToken::Bytes(b) => Ok(AttrValue::Bytes(b.to_vec())),
        CborToken::ArrayStart(n) => {
            let mut items = Vec::with_capacity(bounded_capacity(n, r.remaining()));
            for _ in 0..n {
                items.push(read_attr_value_at(r, depth + 1)?);
            }
            Ok(AttrValue::Array(items))
        }
        other => Err(ManifestError::UnexpectedValue(CborToken::describe(&other))),
    }
}

impl<'a> CborToken<'a> {
    /// Short human-readable name of the token kind (for error messages).
    fn describe(tok: &CborToken<'_>) -> &'static str {
        match tok {
            CborToken::UInt(_) => "uint",
            CborToken::Text(_) => "text",
            CborToken::Bytes(_) => "bytes",
            CborToken::ArrayStart(_) => "array",
            CborToken::MapStart(_) => "map",
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::manifest::ManifestError;

    // -- AP101B layout discipline --------------------
    //
    // Zero-hidden-padding gate for the *real mixed* manifest structs - i.e.
    // structs with heterogeneous field types, NOT `[u8; N]` single-field
    // wrappers where the assertion is vacuous. Each `assert_no_padding!`
    // recomputes the `#[repr(C)]`-pinned layout from the declared field list
    // and panics (at compile time, inside a `const`) if it diverges from
    // `size_of` - i.e. if a field is added/removed/reordered without updating
    // this gate, or hidden/tail padding crept in. `#[repr(C)]` (on the pinned
    // structs) makes the gate a true layout contract. Hosted here so the
    // `ferrite-core` dev-dependency (also used by journal.rs) is in scope and
    // the default dependency tree stays free of new deps.
    ferrite_core::assert_no_padding!(
        SysHeader,
        version: u8,
        storage_tier: u8,
        total_pixel_bytes: u64,
        arrived_at: u64,
    );
    ferrite_core::assert_no_padding!(
        InstanceRecord,
        blake3: [u8; 32],
        attrs: Vec<(u32, AttrValue)>,
        transfer_syntax: String,
        storage_tier: u8,
        rejection_reason: u32,
        file_size: u64,
        frames: Vec<FrameRecord>,
    );
    ferrite_core::assert_no_padding!(
        SeriesRecord,
        attrs: Vec<(u32, AttrValue)>,
        instances: Vec<InstanceRecord>,
    );
    ferrite_core::assert_no_padding!(
        FrameRecord,
        number: u32,
        hash: [u8; 32],
        offset: u64,
        compressed_len: u64,
    );

    /// A fully populated manifest exercising every record type.
    pub fn sample_manifest() -> Manifest {
        let mut frame_hash = [0u8; 32];
        frame_hash[0] = 0xAB;
        frame_hash[31] = 0xEF;

        Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 2_000_000,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![
                    (0x0008_0020, AttrValue::UInt(20260802)),        // StudyDate
                    (0x0008_0060, AttrValue::Text("CT".into())),     // Modality
                    (0x0010_0010, AttrValue::Text("ivanov".into())), // PatientName
                    (0x0020_000d, AttrValue::Text("1.2.840.1.1".into())), // StudyInstanceUID
                ],
                // One series (CT), one instance - matched by the decoded
                // manifest (counts + ModalitiesInStudy).
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
                    attrs: vec![
                        (0x0008_0018, AttrValue::Text("1.2.840.1.3".into())), // SOPInstanceUID
                        (0x0028_0010, AttrValue::UInt(512)),                  // Rows
                        (0x0028_0011, AttrValue::UInt(512)),                  // Columns
                    ],
                    transfer_syntax: "1.2.840.10008.1.2.1".into(),
                    storage_tier: TIER_HOT,
                    rejection_reason: crate::manifest::NOT_REJECTED_REASON,
                    blake3: frame_hash,
                    file_size: 2_000_000,
                    frames: vec![
                        FrameRecord {
                            number: 1,
                            hash: frame_hash,
                            offset: 0,
                            compressed_len: 1_059_968,
                        },
                        FrameRecord {
                            number: 2,
                            hash: frame_hash,
                            offset: 1_059_968,
                            compressed_len: 940_032,
                        },
                    ],
                }],
            }],
        }
    }

    fn roundtrip_eq(m: &Manifest) {
        let bytes = m.encode().expect("encode");
        let decoded = Manifest::decode(&bytes).expect("decode");
        assert_eq!(&decoded, m, "roundtrip mismatch");
    }

    #[test]
    fn roundtrip_full_manifest() {
        roundtrip_eq(&sample_manifest());
    }

    /// The encoder must refuse the same values the decoder rejects, BEFORE
    /// any byte is produced, so the publish path never renames a poisoned
    /// manifest into place. Revert the encode-side check and this test reds
    /// (encode would return Ok for both).
    #[test]
    fn encode_refuses_unknown_tier_and_zero_version() {
        let mut tier_zero = sample_manifest();
        tier_zero.sys.storage_tier = 0;
        match tier_zero.encode() {
            Err(ManifestError::MalformedEnvelope(why)) => {
                assert_eq!(why, "unknown storage tier");
            }
            other => panic!("tier 0 must be refused at encode, got {other:?}"),
        }

        let mut version_zero = sample_manifest();
        version_zero.sys.version = 0;
        match version_zero.encode() {
            Err(ManifestError::MalformedEnvelope(why)) => {
                assert_eq!(why, "unsupported sys version");
            }
            other => panic!("version 0 must be refused at encode, got {other:?}"),
        }

        // A FUTURE version is refused too: this build cannot interpret what a
        // version-2 writer meant, so publishing one would hand every reader a
        // manifest it has to guess at. The check is `!=`, not `> 0`.
        let mut version_future = sample_manifest();
        version_future.sys.version = 2;
        match version_future.encode() {
            Err(ManifestError::MalformedEnvelope(why)) => {
                assert_eq!(why, "unsupported sys version");
            }
            other => panic!("version 2 must be refused at encode, got {other:?}"),
        }
    }

    /// A frame number above `u16::MAX` must survive the CBOR round trip.
    ///
    /// This is the whole point of widening `FrameRecord.number`: a whole-slide
    /// instance holds far more than 65535 tiles, and a truncating decode would
    /// silently mis-address every tile past the 16-bit boundary.
    #[test]
    fn roundtrip_frame_number_above_u16_max() {
        let mut m = sample_manifest();
        let mut b3 = [0u8; 32];
        b3[0] = 0xAB;
        let big = u32::from(u16::MAX) + 1234; // 66769
        m.series[0].instances[0].frames = vec![
            FrameRecord {
                number: 1,
                hash: b3,
                offset: 0,
                compressed_len: 100,
            },
            FrameRecord {
                number: big,
                hash: b3,
                offset: 100,
                compressed_len: 200,
            },
        ];

        let bytes = m.encode().expect("encode");
        let decoded = Manifest::decode(&bytes).expect("decode");
        let frames = &decoded.series[0].instances[0].frames;
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[1].number, big,
            "a >u16::MAX frame number must survive"
        );
        // And the whole manifest compares equal - no other field drifted.
        assert_eq!(decoded, m, "roundtrip mismatch");
    }

    /// Widening `number` must not change the encoded bytes of a SMALL number:
    /// CBOR integers are variable-length, so the u16 range encodes identically.
    /// This is what keeps existing manifests readable and golden-stable.
    #[test]
    fn widening_number_leaves_small_number_bytes_identical() {
        // Encode one frame record into a fixed buffer, exactly as the manifest
        // encoder does, so the assertion is on the frame's own bytes.
        fn frame_bytes(number: u32) -> Vec<u8> {
            let mut buf = [0u8; 128];
            let written = {
                let mut w = CborWriter::new(&mut buf);
                encode_frame(
                    &mut w,
                    &FrameRecord {
                        number,
                        hash: [0x11; 32],
                        offset: 4096,
                        compressed_len: 512,
                    },
                )
                .expect("encode frame");
                w.written()
            };
            buf[..written].to_vec()
        }

        // 1 encodes as the single CBOR byte 0x01 right after its key, exactly
        // as it did with a u16 field: CBOR does not record a Rust width.
        let one = frame_bytes(1);
        assert!(
            one.contains(&0x01),
            "small values keep their compact CBOR encoding"
        );
        // The largest u16 value is still the 2-byte-integer form, so a
        // u16-era manifest and a u32-era manifest are byte-identical.
        assert_eq!(frame_bytes(65535), frame_bytes(u32::from(u16::MAX)));
        // Past the old ceiling the encoding necessarily grows, which is what
        // makes the widened range observable at all.
        assert_ne!(frame_bytes(65535), frame_bytes(u32::from(u16::MAX) + 1));
        // Round-trip the interesting values through the frame decoder.
        for n in [1u32, 255, 65535, u32::from(u16::MAX) + 1, 4_000_000] {
            let bytes = frame_bytes(n);
            let mut r = CborReader::new(&bytes);
            let decoded = read_frame(&mut r).expect("decode frame");
            assert_eq!(decoded.number, n, "round-trip of {n}");
        }
    }

    /// the series-level shallow decode yields series `attrs`
    /// byte-identical to a full `decode`, with `instances` skipped (empty).
    #[test]
    fn decode_to_series_matches_full_decode_series() {
        let bytes = sample_manifest().encode().expect("encode");
        let full = Manifest::decode(&bytes).expect("full decode");
        let shallow = Manifest::decode_to_series(&bytes).expect("shallow decode");

        // Same number of series; attrs byte-identical.
        assert_eq!(full.series.len(), shallow.series.len());
        for (fs, ss) in full.series.iter().zip(shallow.series.iter()) {
            assert_eq!(fs.attrs, ss.attrs, "series attrs must be byte-identical");
        }
        // Instances were skipped, not decoded.
        assert!(shallow.series.iter().all(|s| s.instances.is_empty()));
        // The shallow read still yields the study attrs.
        assert_eq!(shallow.study, full.study);
        assert_eq!(shallow.sys, full.sys);
    }

    /// a manifest with multiple series (incl. instance/frame payloads)
    /// shallow-decodes to the same series attrs as full decode.
    #[test]
    fn decode_to_series_skips_instance_and_frame_payloads() {
        let mut m = sample_manifest();
        // Add a second series with an instance carrying frames.
        let mut b3 = [0u8; 32];
        b3[0] = 0x77;
        m.series.push(SeriesRecord {
            attrs: vec![
                (0x0020_000e, AttrValue::Text("1.2.840.1.9".into())),
                (0x0020_0011, AttrValue::Text("2".into())),
            ],
            instances: vec![InstanceRecord {
                attrs: vec![(0x0008_0018, AttrValue::Text("1.2.840.1.10".into()))],
                transfer_syntax: "1.2.840.10008.1.2.1".into(),
                storage_tier: TIER_HOT,
                rejection_reason: crate::manifest::NOT_REJECTED_REASON,
                blake3: b3,
                file_size: 42,
                frames: vec![FrameRecord {
                    number: 1,
                    hash: b3,
                    offset: 0,
                    compressed_len: 42,
                }],
            }],
        });
        let bytes = m.encode().expect("encode");
        let full = Manifest::decode(&bytes).expect("full decode");
        let shallow = Manifest::decode_to_series(&bytes).expect("shallow decode");
        assert_eq!(full.series.len(), 2);
        assert_eq!(shallow.series.len(), 2);
        for (fs, ss) in full.series.iter().zip(shallow.series.iter()) {
            assert_eq!(fs.attrs, ss.attrs);
        }
        assert!(
            shallow
                .series
                .iter()
                .all(|s| s.instances.is_empty() && s.instances.is_empty())
        );
    }

    /// A manifest whose instances carry no rejection reason encodes exactly as
    /// it did before the key existed: the key is emitted only when a reason is
    /// recorded, so the byte stream of an untouched manifest is unchanged.
    #[test]
    fn rejection_reason_absent_keeps_manifest_byte_identical() {
        let m = sample_manifest();
        assert!(
            !m.series[0].instances[0].is_rejected(),
            "fixture must start visible"
        );
        let base = m.encode().expect("encode");

        // Setting a reason is what changes the bytes - there is no separate
        // flag that could be set without one.
        let mut flagged = sample_manifest();
        flagged.series[0].instances[0].rejection_reason = crate::manifest::QUALITY_REASON_CODE;
        assert_ne!(
            flagged.encode().expect("encode"),
            base,
            "a recorded reason must change the bytes"
        );

        let mut cleared = sample_manifest();
        cleared.series[0].instances[0].rejection_reason = crate::manifest::NOT_REJECTED_REASON;
        assert_eq!(
            cleared.encode().expect("encode"),
            base,
            "no reason must not change the bytes"
        );
    }

    /// The reason survives an encode/decode round trip for every shipped code,
    /// so the expose decision (Quality only) is made from durable data.
    #[test]
    fn rejection_reason_round_trips() {
        for reason in [
            crate::manifest::NOT_REJECTED_REASON,
            crate::manifest::QUALITY_REASON_CODE,
            113_037,
            113_038,
            113_039,
        ] {
            let mut m = sample_manifest();
            m.series[0].instances[0].rejection_reason = reason;
            let bytes = m.encode().expect("encode");
            let decoded = Manifest::decode(&bytes).expect("decode");
            assert_eq!(
                decoded.series[0].instances[0].rejection_reason, reason,
                "reason {reason} must round-trip"
            );
        }
    }

    /// A manifest written before the key existed decodes as visible.
    #[test]
    fn rejection_reason_defaults_to_not_rejected_for_old_manifests() {
        let bytes = sample_manifest().encode().expect("encode");
        let decoded = Manifest::decode(&bytes).expect("decode");
        assert_eq!(
            decoded.series[0].instances[0].rejection_reason,
            crate::manifest::NOT_REJECTED_REASON,
            "an old manifest has no key and must decode to visible"
        );
        assert!(!decoded.series[0].instances[0].is_rejected());
    }

    /// Only a Quality hide may be lifted by a review surface. The IOCM
    /// AnyClass reasons hide diagnostic images with no expose path, so this
    /// must be decided from the stored code alone.
    #[test]
    fn only_a_quality_reason_is_exposable() {
        let mut m = sample_manifest();
        for (reason, exposable) in [
            (crate::manifest::NOT_REJECTED_REASON, false),
            (crate::manifest::QUALITY_REASON_CODE, true),
            (113_037, false),
            (113_038, false),
            (113_039, false),
        ] {
            m.series[0].instances[0].rejection_reason = reason;
            assert_eq!(
                m.series[0].instances[0].is_exposable(),
                exposable,
                "reason {reason}"
            );
        }
    }

    #[test]
    fn roundtrip_sys_only() {
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 42,
                arrived_at: 0,
            },
            study: StudyQido::default(),
            series: Vec::new(),
        };
        roundtrip_eq(&m);
    }

    #[test]
    fn roundtrip_study_only() {
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![(0x0008_0060, AttrValue::Text("XA".into()))],
                ..Default::default()
            },
            series: Vec::new(),
        };
        roundtrip_eq(&m);
    }

    /// the shallow decode derives ModalitiesInStudy as
    /// the distinct series modalities in first-occurrence order, comma-joined.
    #[test]
    fn decode_derives_mixed_modalities() {
        let mk_series = |uid: &str, modality: &str| SeriesRecord {
            attrs: vec![
                (0x0020_000e, AttrValue::Text(uid.into())),
                (0x0008_0060, AttrValue::Text(modality.into())),
            ],
            instances: vec![],
        };
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![(0x0020_000d, AttrValue::Text("1.2.840.9.1".into()))],
                ..Default::default()
            },
            series: vec![
                mk_series("1.2.840.9.1.1", "CT"),
                mk_series("1.2.840.9.1.2", "MR"),
                // A duplicate modality is merged (not repeated).
                mk_series("1.2.840.9.1.3", "CT"),
            ],
        };
        let bytes = m.encode().expect("encode");
        // Full decode derives the aggregate from the decoded series.
        let full = Manifest::decode(&bytes).expect("decode");
        assert_eq!(
            full.study.modalities_in_study.as_deref(),
            Some("CT,MR"),
            "distinct first-occurrence aggregation"
        );
        assert_eq!(full.study.series_count, 3);
        // Shallow decode derives it from the header walk - identical.
        let shallow = Manifest::decode_study_only(&bytes).expect("decode study only");
        assert_eq!(shallow.study.modalities_in_study.as_deref(), Some("CT,MR"));
        assert_eq!(shallow.study.series_count, 3);

        // A study with no series -> None (like GROUP_CONCAT over empty set).
        let empty = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido::default(),
            series: vec![],
        };
        let ebytes = empty.encode().expect("encode");
        assert_eq!(
            Manifest::decode(&ebytes).unwrap().study.modalities_in_study,
            None
        );
        assert_eq!(
            Manifest::decode_study_only(&ebytes)
                .unwrap()
                .study
                .modalities_in_study,
            None
        );
    }

    /// A non-text series Modality is consumed WHOLE by the study-only decode,
    /// so the series-map walk stays aligned and the non-text value contributes
    /// nothing to ModalitiesInStudy (matching the full-decode aggregation).
    ///
    /// Our encoder always writes Modality as Text, so this arm is defensive;
    /// before the fix `skip_value` double-consumed a scalar (reading the next
    /// map key as a value) or skipped only one array element, desyncing the
    /// walk into a decode error or wrong counts.
    #[test]
    fn decode_study_only_consumes_non_text_modality_whole() {
        let build = |modality: AttrValue| -> Manifest {
            Manifest {
                sys: SysHeader {
                    version: 1,
                    storage_tier: TIER_HOT,
                    total_pixel_bytes: 0,
                    arrived_at: 0,
                },
                study: StudyQido::default(),
                series: vec![SeriesRecord {
                    attrs: vec![
                        (0x0020_000e, AttrValue::Text("1.2.840.9.7".into())),
                        (0x0008_0060, modality),
                    ],
                    instances: vec![],
                }],
            }
        };

        // Scalar UInt: `next()` consumes it whole, nothing must be skipped.
        let uint = build(AttrValue::UInt(42));
        let bytes = uint.encode().expect("encode uint modality");
        let decoded = Manifest::decode_study_only(&bytes).expect("uint decodes");
        assert_eq!(decoded.study.series_count, 1);
        assert_eq!(decoded.study.instance_count, 0);
        assert_eq!(
            decoded.study.modalities_in_study, None,
            "a non-text modality contributes nothing to the aggregate"
        );

        // Array: `next()` consumed only the header, every element is skipped.
        let array = build(AttrValue::Array(vec![
            AttrValue::Text("CT".into()),
            AttrValue::Text("MR".into()),
        ]));
        let bytes = array.encode().expect("encode array modality");
        let decoded = Manifest::decode_study_only(&bytes).expect("array decodes");
        assert_eq!(decoded.study.series_count, 1);
        assert_eq!(decoded.study.instance_count, 0);
        assert_eq!(decoded.study.modalities_in_study, None);
    }

    #[test]
    fn roundtrip_series_with_empty_attrs() {
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_COLD,
                total_pixel_bytes: 100,
                arrived_at: 0,
            },
            study: StudyQido {
                // One series (empty attrs), zero instances - the decoded
                // manifest derives these counts from the series array.
                series_count: 1,
                instance_count: 0,
                ..Default::default()
            },
            series: vec![SeriesRecord {
                attrs: Vec::new(),
                instances: Vec::new(),
            }],
        };
        roundtrip_eq(&m);
    }

    #[test]
    fn roundtrip_attr_array_value() {
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![(
                    0x0008_0008, // ImageType - multi-valued
                    AttrValue::Array(vec![
                        AttrValue::Text("ORIGINAL".into()),
                        AttrValue::Text("PRIMARY".into()),
                    ]),
                )],
                ..Default::default()
            },
            series: Vec::new(),
        };
        roundtrip_eq(&m);
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let bytes = sample_manifest().encode().expect("encode");
        let mut padded = bytes.clone();
        padded.push(0x00);
        assert_eq!(
            Manifest::decode(&padded).unwrap_err(),
            ManifestError::Cbor(CborError::TrailingData)
        );
    }

    #[test]
    fn decode_rejects_unknown_top_level_key() {
        let mut buf = [0u8; 512];
        let mut w = CborWriter::new(&mut buf);
        // Exactly 3 top-level entries, one of which is an unknown key.
        w.write_map_header(3).unwrap();
        w.write_u64(KEY_SYS).unwrap();
        w.write_map_header(3).unwrap();
        w.write_u64(SYS_VERSION).unwrap();
        w.write_u64(1).unwrap();
        w.write_u64(SYS_STORAGE_TIER).unwrap();
        w.write_u64(TIER_HOT as u64).unwrap();
        w.write_u64(SYS_TOTAL_PIXEL_BYTES).unwrap();
        w.write_u64(0).unwrap();
        w.write_u64(KEY_QIDO_STUDY).unwrap();
        w.write_map_header(0).unwrap();
        w.write_u64(99).unwrap(); // unknown top-level key
        w.write_u64(0).unwrap();
        let bytes = w.as_slice().to_vec();
        assert_eq!(
            Manifest::decode(&bytes).unwrap_err(),
            ManifestError::UnknownKey(99)
        );
    }

    /// `decode_to_series` must skip EVERY instance map, not just the first.
    ///
    /// `skip_value` consumes a single item, so a series with two instances left
    /// the reader parked inside the array and the enclosing map decoded as
    /// garbage - a series-QIDO answer for a multi-instance series was simply
    /// lost. The single-instance fixture in the round-trip test never reached
    /// that shape.
    #[test]
    fn decode_to_series_handles_multi_instance_and_empty_series() {
        let mut m = sample_manifest();
        // One series, TWO instances (the case the single skip missed).
        let inst = m.series[0].instances[0].clone();
        m.series[0].instances.push(inst.clone());

        let bytes = m.encode().expect("encode");
        let series = Manifest::decode_to_series(&bytes).expect("series depth decode");
        assert_eq!(series.series.len(), 1, "the series survives the walk");
        assert_eq!(
            series.series[0].attrs.len(),
            m.series[0].attrs.len(),
            "and so do its attributes"
        );
        assert!(
            series.series[0].instances.is_empty(),
            "the instance payloads are skipped, not decoded"
        );

        // Zero instances: the loop runs zero times and must not consume the
        // next key.
        let mut empty = sample_manifest();
        empty.series[0].instances.clear();
        let bytes = empty.encode().expect("encode");
        let series = Manifest::decode_to_series(&bytes).expect("empty series decodes");
        assert_eq!(series.series.len(), 1);
        assert!(series.series[0].instances.is_empty());
    }

    #[test]
    fn decode_rejects_missing_sys() {
        let mut buf = [0u8; 64];
        let mut w = CborWriter::new(&mut buf);
        w.write_map_header(2).unwrap();
        w.write_u64(KEY_QIDO_STUDY).unwrap();
        w.write_map_header(0).unwrap();
        w.write_u64(KEY_SERIES).unwrap();
        w.write_array_header(0).unwrap();
        let bytes = w.as_slice().to_vec();
        assert!(matches!(
            Manifest::decode(&bytes).unwrap_err(),
            ManifestError::MalformedEnvelope(_)
        ));
    }

    #[test]
    fn decode_rejects_bad_hash_length() {
        let mut buf = [0u8; 256];
        let mut w = CborWriter::new(&mut buf);
        w.write_map_header(3).unwrap();
        w.write_u64(KEY_SYS).unwrap();
        w.write_map_header(3).unwrap();
        w.write_u64(SYS_VERSION).unwrap();
        w.write_u64(1).unwrap();
        w.write_u64(SYS_STORAGE_TIER).unwrap();
        w.write_u64(TIER_HOT as u64).unwrap();
        w.write_u64(SYS_TOTAL_PIXEL_BYTES).unwrap();
        w.write_u64(0).unwrap();
        w.write_u64(KEY_QIDO_STUDY).unwrap();
        w.write_map_header(0).unwrap();
        w.write_u64(KEY_SERIES).unwrap();
        w.write_array_header(1).unwrap();
        // single series with a single instance with one frame of wrong hash len
        w.write_map_header(1).unwrap(); // series: only instances key
        w.write_u64(SERIES_INSTANCES).unwrap();
        w.write_array_header(1).unwrap();
        w.write_map_header(2).unwrap(); // instance: ts + frames
        w.write_u64(INSTANCE_TRANSFER_SYNTAX).unwrap();
        w.write_text("1.2.840.10008.1.2.1").unwrap();
        w.write_u64(INSTANCE_FRAMES).unwrap();
        w.write_array_header(1).unwrap();
        w.write_map_header(4).unwrap();
        w.write_u64(FRAME_NUMBER).unwrap();
        w.write_u64(1).unwrap();
        w.write_u64(FRAME_HASH).unwrap();
        w.write_bytes(&[1, 2, 3]).unwrap(); // wrong length -> hash 3 bytes
        w.write_u64(FRAME_OFFSET).unwrap();
        w.write_u64(0).unwrap();
        w.write_u64(FRAME_COMPRESSED_LEN).unwrap();
        w.write_u64(10).unwrap();
        let bytes = w.as_slice().to_vec();
        assert_eq!(
            Manifest::decode(&bytes).unwrap_err(),
            ManifestError::InvalidHashLen(3)
        );
    }

    /// 100 study attrs of empty arrays must encode and round-trip.
    ///
    /// The pre-fix bound counted an Array attribute as 2 bytes (the array
    /// header only, no tag key), so `encoded_size_hint` returned ~330 against
    /// an actual ~687 and `Manifest::encode` failed with `BufferFull` for a
    /// manifest the decoder accepts. The bound is part of the public record
    /// format, so it must not lie - "conservative upper bound" has to mean
    /// "no valid manifest can overflow it".
    #[test]
    fn encode_succeeds_for_one_hundred_empty_array_study_attrs() {
        let mut m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido::default(),
            series: Vec::new(),
        };
        // Real DICOM tags live in groups 0002..FFFF and encode as the 5-byte
        // `write_u64` form (0x1a + u32). Small tags (0..99) would encode in
        // 1-2 bytes and never exercise the missing-key under-count.
        for i in 0u32..100 {
            m.study
                .attrs
                .push((0x0008_0000 + i, AttrValue::Array(Vec::new())));
        }
        let bytes = m.encode().expect("100 empty-array study attrs must encode");
        let decoded = Manifest::decode(&bytes).expect("decode");
        assert_eq!(decoded, m, "round-trip mismatch");
    }

    /// A study attr holding the largest u32-range UInt must round-trip.
    ///
    /// The pre-fix bound counted a UInt as 9 bytes against an actual 10
    /// (tag key 5 + value header 1 + argument 4), so the 25% margin silently
    /// absorbed the missing byte. `u32::MAX` exercises the full 5-byte value
    /// form, the case the old constant under-counted.
    #[test]
    fn encode_round_trips_uint_max_study_attr() {
        let m = Manifest {
            sys: SysHeader {
                version: 1,
                storage_tier: TIER_HOT,
                total_pixel_bytes: 0,
                arrived_at: 0,
            },
            study: StudyQido {
                attrs: vec![(0x0008_0020, AttrValue::UInt(u64::from(u32::MAX)))],
                ..Default::default()
            },
            series: Vec::new(),
        };
        let bytes = m.encode().expect("encode");
        let decoded = Manifest::decode(&bytes).expect("decode");
        assert_eq!(decoded, m, "round-trip mismatch");
    }

    /// Encode a hand-built CBOR envelope through `fill`.
    ///
    /// The writer borrows its buffer, so a test that needs raw control over the
    /// wire bytes (duplicate keys, out-of-range system values) cannot go
    /// through `Manifest::encode`. This closes the borrow and hands back the
    /// bytes that were written.
    fn envelope_bytes(fill: impl FnOnce(&mut CborWriter<'_>)) -> Vec<u8> {
        let mut buf = vec![0u8; 8192];
        let written = {
            let mut w = CborWriter::new(&mut buf);
            fill(&mut w);
            w.written()
        };
        buf.truncate(written);
        buf
    }

    /// Write a minimal system map holding `version` and `tier`.
    ///
    /// The `KEY_SYS` key itself is the caller's, so the envelope tests can put
    /// the three entries in any order.
    fn write_sys(w: &mut CborWriter<'_>, version: u64, tier: u64) {
        w.write_map_header(4).expect("sys");
        w.write_u64(SYS_VERSION).expect("k");
        w.write_u64(version).expect("v");
        w.write_u64(SYS_STORAGE_TIER).expect("k");
        w.write_u64(tier).expect("v");
        w.write_u64(SYS_TOTAL_PIXEL_BYTES).expect("k");
        w.write_u64(0).expect("v");
        w.write_u64(SYS_ARRIVED_AT).expect("k");
        w.write_u64(0).expect("v");
    }

    /// Write a one-attribute study map.
    fn write_empty_study(w: &mut CborWriter<'_>) {
        w.write_u64(KEY_QIDO_STUDY).expect("k");
        w.write_map_header(0).expect("study");
    }

    /// The size bound must cover the MAP KEYS and the per-record system keys,
    /// not just the attribute values.
    ///
    /// With 100 study attributes carrying five-byte keys and `UInt(u64::MAX)`
    /// values the old bound estimated 1330 bytes against 1400 needed, and 100
    /// EMPTY series contributed nothing at all while still costing a map
    /// header and an instances-array header each. Both are valid manifests
    /// that the old hint rejected with `BufferFull` - a study the decoder
    /// would happily read and the writer refused to store.
    ///
    /// Reverting to the value-only estimate reds this test on the encode
    /// itself, before any round-trip assertion runs.
    #[test]
    fn encode_covers_keys_and_empty_series_in_the_size_bound() {
        let mut m = sample_manifest();
        m.study.attrs.clear();
        // Five-byte keys (real DICOM tags) with the widest UInt argument.
        for i in 0u32..100 {
            m.study
                .attrs
                .push((0x0008_0000 + i, AttrValue::UInt(u64::MAX)));
        }
        // 100 empty series: no attribute and no instance to contribute to the
        // old estimate, yet each still writes a map header and an array
        // header. Each carries a real UID so the round-trip is meaningful.
        m.series = (0u32..100)
            .map(|i| SeriesRecord {
                attrs: vec![(0x0020_000e, AttrValue::Text(format!("1.2.840.1.{i}")))],
                instances: Vec::new(),
            })
            .collect();

        let bytes = m.encode().expect("encode");
        let decoded = Manifest::decode(&bytes).expect("decode");
        assert_eq!(decoded.series.len(), 100);
        assert_eq!(decoded.study.attrs.len(), 100);
    }

    /// Each of the three envelope keys must be present EXACTLY once.
    ///
    /// Counting entries cannot prove that: a map of {SYS, QIDO_STUDY,
    /// QIDO_STUDY} has the required three entries and passes every per-key
    /// check, yielding a manifest with no series at all - a study that QIDO
    /// lists and WADO can never serve an instance from.
    #[test]
    fn a_duplicated_envelope_key_is_refused() {
        let bytes = envelope_bytes(|w| {
            w.write_map_header(3).expect("map");
            w.write_u64(KEY_SYS).expect("key");
            write_sys(w, 1, u64::from(TIER_HOT));
            // qido_study, TWICE
            for _ in 0..2 {
                write_empty_study(w);
            }
        });

        match Manifest::decode(&bytes) {
            Err(ManifestError::MalformedEnvelope(why)) => {
                assert_eq!(why, "duplicate study map");
            }
            other => panic!("a duplicated study map must be refused, got {other:?}"),
        }
    }

    /// A manifest with no series array at all must be refused.
    ///
    /// The three-entry count was satisfied by three keys of which one was
    /// missing and another repeated; nothing checked that `KEY_SERIES` was
    /// among them.
    #[test]
    fn a_missing_series_envelope_key_is_refused() {
        let bytes = envelope_bytes(|w| {
            w.write_map_header(3).expect("map");
            w.write_u64(KEY_SYS).expect("key");
            write_sys(w, 1, u64::from(TIER_HOT));
            // Two study maps, no KEY_SERIES anywhere.
            for _ in 0..2 {
                write_empty_study(w);
            }
        });
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ManifestError::MalformedEnvelope(_))
        ));
    }

    /// Study-depth aggregates must not depend on the CBOR key ORDER.
    ///
    /// CBOR maps have no defined order, so a writer may emit KEY_SERIES before
    /// KEY_QIDO_STUDY. The aggregates were computed straight onto `study`, so
    /// with that order they were discarded (`study` was still `None`) and a
    /// manifest holding one CT series reported 0 series, 0 instances and no
    /// modality - while the same bytes at full depth reported 1, 1, "CT".
    #[test]
    fn study_aggregates_survive_a_series_first_envelope() {
        let m = sample_manifest();
        let bytes = envelope_bytes(|w| {
            w.write_map_header(3).expect("map");
            w.write_u64(KEY_SERIES).expect("key");
            w.write_array_header(1).expect("series array");
            encode_series(w, &m.series[0]).expect("series");
            w.write_u64(KEY_SYS).expect("key");
            write_sys(w, u64::from(m.sys.version), u64::from(m.sys.storage_tier));
            w.write_u64(KEY_QIDO_STUDY).expect("key");
            encode_study(w, &m.study).expect("study");
        });

        let study = Manifest::decode_study_only(&bytes).expect("study decode");
        assert_eq!(study.study.series_count, 1, "series count must survive");
        assert_eq!(study.study.instance_count, 1, "instance count must survive");
        assert_eq!(
            study.study.modalities_in_study.as_deref(),
            Some("CT"),
            "ModalitiesInStudy must survive a series-first envelope"
        );
    }

    /// An attribute key that collides with a manifest system key is refused
    /// at ENCODE time.
    ///
    /// `SERIES_INSTANCES` is 4, which a caller-supplied series attribute could
    /// claim. The encoder would then write key 4 twice - CBOR permits that
    /// (RFC 8949 5.6) but says nothing about which value wins, so a text
    /// attribute would either be discarded or be mistaken for a second
    /// instances array.
    #[test]
    fn a_system_key_used_as_an_attribute_key_is_refused() {
        let mut m = sample_manifest();
        m.series[0]
            .attrs
            .push((SERIES_INSTANCES as u32, AttrValue::Text("hijack".into())));
        match m.encode() {
            Err(ManifestError::MalformedEnvelope(why)) => {
                assert_eq!(why, "attribute key collides with a manifest system key");
            }
            other => panic!("a colliding attribute key must be refused, got {other:?}"),
        }

        // The study map has the same hazard against its own keys.
        let mut m2 = sample_manifest();
        m2.study.attrs.push((1, AttrValue::Text("hijack".into())));
        assert!(matches!(
            m2.encode(),
            Err(ManifestError::MalformedEnvelope(
                "attribute key collides with a manifest system key"
            ))
        ));
    }

    /// A frame number must be one-based and fit in 32 bits.
    ///
    /// `as u32` turned 4_294_967_297 into frame 1, aliasing two frames onto
    /// one number in a table that lookups walk linearly; frame 0 is outside
    /// the one-based contract and would satisfy a lookup that should miss.
    #[test]
    fn a_frame_number_outside_the_one_based_u32_range_is_refused() {
        let m = sample_manifest();
        let inst = &m.series[0].instances[0];
        for bad in [0u64, u64::from(u32::MAX) + 2] {
            let bytes = envelope_bytes(|w| {
                w.write_map_header(3).expect("map");
                w.write_u64(KEY_SYS).expect("key");
                write_sys(w, u64::from(m.sys.version), u64::from(m.sys.storage_tier));
                w.write_u64(KEY_QIDO_STUDY).expect("key");
                encode_study(w, &m.study).expect("study");
                w.write_u64(KEY_SERIES).expect("k");
                w.write_array_header(1).expect("arr");
                // One series carrying one instance with one frame whose number
                // is `bad` - hand-written, because `FrameRecord::number` is a
                // u32 and cannot hold the out-of-range value.
                w.write_map_header(2).expect("series map");
                w.write_u64(SERIES_INSTANCES).expect("k");
                w.write_array_header(1).expect("instances");
                w.write_map_header(5).expect("instance map");
                w.write_u64(INSTANCE_STORAGE_TIER).expect("k");
                w.write_u64(u64::from(inst.storage_tier)).expect("v");
                w.write_u64(INSTANCE_TRANSFER_SYNTAX).expect("k");
                w.write_text(&inst.transfer_syntax).expect("v");
                w.write_u64(INSTANCE_BLAKE3).expect("k");
                w.write_bytes(&inst.blake3).expect("v");
                w.write_u64(INSTANCE_FILE_SIZE).expect("k");
                w.write_u64(inst.file_size).expect("v");
                w.write_u64(INSTANCE_FRAMES).expect("k");
                w.write_array_header(1).expect("frames");
                w.write_map_header(4).expect("frame map");
                w.write_u64(FRAME_NUMBER).expect("k");
                w.write_u64(bad).expect("v");
                w.write_u64(FRAME_HASH).expect("k");
                w.write_bytes(&inst.frames[0].hash).expect("v");
                w.write_u64(FRAME_OFFSET).expect("k");
                w.write_u64(inst.frames[0].offset).expect("v");
                w.write_u64(FRAME_COMPRESSED_LEN).expect("k");
                w.write_u64(inst.frames[0].compressed_len).expect("v");
                w.write_u64(0x0020_000e).expect("series attr key");
                w.write_text("1.2.840.1.2").expect("v");
            });

            assert!(
                matches!(
                    Manifest::decode(&bytes),
                    Err(ManifestError::MalformedEnvelope(_))
                ),
                "frame number {bad} must be refused"
            );
        }
    }

    /// An out-of-range version or tier must not narrow into a legal one.
    ///
    /// `257 as u8` is 1, and 1 is a valid tier and a valid version - so a
    /// truncating decode reported a manifest this build can read.
    #[test]
    fn an_out_of_range_version_or_tier_is_refused_not_truncated() {
        let bad_tier = envelope_bytes(|w| {
            w.write_map_header(3).expect("map");
            w.write_u64(KEY_SYS).expect("key");
            write_sys(w, 1, 257);
            write_empty_study(w);
            w.write_u64(KEY_SERIES).expect("k");
            w.write_array_header(0).expect("series");
        });
        assert!(
            matches!(
                Manifest::decode(&bad_tier),
                Err(ManifestError::MalformedEnvelope("sys storage tier"))
            ),
            "a 257 tier must not narrow to TIER_HOT"
        );

        let bad_version = envelope_bytes(|w| {
            w.write_map_header(3).expect("map");
            w.write_u64(KEY_SYS).expect("key");
            write_sys(w, 257, u64::from(TIER_HOT));
            write_empty_study(w);
            w.write_u64(KEY_SERIES).expect("k");
            w.write_array_header(0).expect("series");
        });
        assert!(
            matches!(
                Manifest::decode(&bad_version),
                Err(ManifestError::MalformedEnvelope("sys version"))
            ),
            "a 257 version must not narrow to version 1"
        );
    }

    /// An instance tier must be one of the three known tiers.
    ///
    /// The instance decoder only checked that the key was PRESENT, so tier 0
    /// (the `Default`) and any out-of-range byte reached WADO lookup, which
    /// branches on the tier to pick a storage backend.
    #[test]
    fn an_invalid_instance_tier_is_refused() {
        for bad in [0u64, 4, 257] {
            let bytes = envelope_bytes(|w| {
                w.write_map_header(3).expect("map");
                w.write_u64(KEY_SYS).expect("key");
                write_sys(w, 1, u64::from(TIER_HOT));
                write_empty_study(w);
                w.write_u64(KEY_SERIES).expect("k");
                w.write_array_header(1).expect("series");
                w.write_map_header(2).expect("series map");
                w.write_u64(SERIES_INSTANCES).expect("k");
                w.write_array_header(1).expect("instances");
                w.write_map_header(5).expect("instance map");
                w.write_u64(INSTANCE_STORAGE_TIER).expect("k");
                w.write_u64(bad).expect("v");
                w.write_u64(INSTANCE_TRANSFER_SYNTAX).expect("k");
                w.write_text("1.2.840.10008.1.2.1").expect("v");
                w.write_u64(INSTANCE_BLAKE3).expect("k");
                w.write_bytes(&[0u8; 32]).expect("v");
                w.write_u64(INSTANCE_FILE_SIZE).expect("k");
                w.write_u64(64).expect("v");
                w.write_u64(INSTANCE_FRAMES).expect("k");
                w.write_array_header(0).expect("frames");
                w.write_u64(0x0020_000e).expect("series attr key");
                w.write_text("1.2.840.1.2").expect("v");
            });
            assert!(
                matches!(
                    Manifest::decode(&bytes),
                    Err(ManifestError::MalformedEnvelope(_))
                ),
                "instance tier {bad} must be refused"
            );
        }
    }
}
