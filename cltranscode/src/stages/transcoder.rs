// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Transcoder pipeline stage - thin wrapper over `clarus_adapters::transcode`.
//!
//! nginx-way: check PixelData presence, try transcode, graceful degradation.

#![deny(unsafe_code)]

use crate::pipeline::{PipelineError, PipelineStage};
use clarus_adapters::codec::CodecRegistry;
use clarus_adapters::transcode::transcode_dicom;

/// Pipeline stage that transcodes DICOM pixel data to a target transfer syntax.
pub struct Transcoder {
    target_ts: String,
    registry: CodecRegistry,
    system_name: String,
}

impl Transcoder {
    pub fn new(target_ts: String, registry: CodecRegistry, system_name: String) -> Self {
        Self {
            target_ts,
            registry,
            system_name,
        }
    }
}

impl PipelineStage for Transcoder {
    fn process(&self, bytes: Vec<u8>) -> Result<Vec<u8>, PipelineError> {
        if !has_pixel_data(&bytes) {
            log::debug!("transcoder: no PixelData — pass-through");
            return Ok(bytes);
        }

        // DICOMweb path: has meta header -> transcode_dicom works
        if let Ok(result) = transcode_dicom(&bytes, &self.target_ts, &self.registry) {
            log::info!("transcoder: OK ({}B → {}B)", bytes.len(), result.len());
            let reason = format!("TS: {}", self.target_ts);
            let with_audit =
                crate::dicom_utils::append_audit_trail(&result, &reason, &self.system_name);
            log::info!(
                "transcoder: audit trail appended (system={}, reason={})",
                self.system_name,
                reason
            );
            return Ok(with_audit);
        }

        // DIMSE Explicit VR path (compressed source preserved by SCP):
        // data has meta-header but no DICM preamble -> prepend one,
        // then high-level transcode_dicom handles everything.
        let is_dimse = bytes.len() < 132 || &bytes[128..132] != b"DICM";
        if is_dimse && is_explicit_vr(&bytes) {
            let mut with_preamble = Vec::with_capacity(bytes.len() + 132);
            with_preamble.extend_from_slice(&[0u8; 128]);
            with_preamble.extend_from_slice(b"DICM");
            with_preamble.extend_from_slice(&bytes);
            if let Ok(result) = transcode_dicom(&with_preamble, &self.target_ts, &self.registry) {
                log::info!(
                    "transcoder: OK DIMSE Explicit VR ({}B → {}B)",
                    bytes.len(),
                    result.len()
                );
                let reason = format!("TS: {}", self.target_ts);
                let with_audit =
                    crate::dicom_utils::append_audit_trail(&result, &reason, &self.system_name);
                log::info!(
                    "transcoder: audit trail appended (system={}, reason={})",
                    self.system_name,
                    reason
                );
                return Ok(with_audit);
            }
            log::debug!("transcoder: DIMSE Explicit VR path failed, falling through");
        }

        // DIMSE path: if source TS == target TS, pass-through.
        // DIMSE data is always Implicit VR LE. No need to transcode if target matches.
        if is_dimse {
            use dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN;
            if IMPLICIT_VR_LITTLE_ENDIAN.uid() == self.target_ts {
                log::debug!("transcoder: DIMSE source==target (Implicit VR LE) — pass-through");
                return Ok(bytes);
            }
        }

        // DIMSE path: inline - decode, encode, replace PixelData in-place.
        match surgical_transcode(&bytes, &self.target_ts, &self.registry) {
            Ok(result) => {
                log::info!(
                    "transcoder: OK inline ({}B → {}B)",
                    bytes.len(),
                    result.len()
                );
                let reason = format!("TS: {}", self.target_ts);
                let with_audit =
                    crate::dicom_utils::append_audit_trail(&result, &reason, &self.system_name);
                log::info!(
                    "transcoder: audit trail appended (system={}, reason={})",
                    self.system_name,
                    reason
                );
                Ok(with_audit)
            }
            Err(e) => {
                log::warn!("transcoder: failed ({}). Sending as-is.", e);
                Ok(bytes)
            }
        }
    }
}

/// Surgical transcode for DIMSE: decode -> encode -> replace PixelData in-place.
fn surgical_transcode(
    bytes: &[u8],
    target_ts: &str,
    registry: &CodecRegistry,
) -> Result<Vec<u8>, PipelineError> {
    use clarus_adapters::codec::DicomMeta;
    use dicom_object::InMemDicomObject;
    use dicom_object::Tag;
    use dicom_transfer_syntax_registry::entries::IMPLICIT_VR_LITTLE_ENDIAN;

    let ts = IMPLICIT_VR_LITTLE_ENDIAN.erased();
    let obj = InMemDicomObject::read_dataset_with_ts(bytes, &ts)
        .map_err(|e| PipelineError::Stage(format!("dicom parse: {}", e)))?;

    let rows = obj
        .get(Tag(0x0028, 0x0010))
        .and_then(|el| el.value().to_bytes().ok())
        .and_then(|b| {
            if b.len() >= 2 {
                Some(u16::from_le_bytes([b[0], b[1]]))
            } else {
                None
            }
        })
        .unwrap_or(0);
    let cols = obj
        .get(Tag(0x0028, 0x0011))
        .and_then(|el| el.value().to_bytes().ok())
        .and_then(|b| {
            if b.len() >= 2 {
                Some(u16::from_le_bytes([b[0], b[1]]))
            } else {
                None
            }
        })
        .unwrap_or(0);
    let spp = obj
        .get(Tag(0x0028, 0x0002))
        .and_then(|el| el.value().to_bytes().ok())
        .and_then(|b| {
            if b.len() >= 2 {
                Some(u16::from_le_bytes([b[0], b[1]]) as u8)
            } else {
                None
            }
        })
        .unwrap_or(1);
    let bits = obj
        .get(Tag(0x0028, 0x0100))
        .and_then(|el| el.value().to_bytes().ok())
        .and_then(|b| {
            if b.len() >= 2 {
                Some(u16::from_le_bytes([b[0], b[1]]) as u8)
            } else {
                None
            }
        })
        .unwrap_or(8);
    let pi = obj
        .get(Tag(0x0028, 0x0004))
        .and_then(|el| el.value().to_str().ok())
        .map(|s| s.trim_end_matches([' ', '\0']).to_string())
        .unwrap_or_else(|| "MONOCHROME2".into());
    // PixelRepresentation (0028,0103) from the source dataset: JPEG2000
    // encode must know signed vs unsigned (default 0 = unsigned).
    let pixel_representation = obj
        .get(Tag(0x0028, 0x0103))
        .and_then(|el| el.value().to_bytes().ok())
        .and_then(|b| {
            if b.len() >= 2 {
                Some(u16::from_le_bytes([b[0], b[1]]))
            } else {
                None
            }
        })
        .unwrap_or(0);

    // nginx-way: read TransferSyntaxUID from (0002,0010) preserved by SCP.
    // Fall back to Implicit VR LE for legacy DIMSE data without meta-header.
    let source_ts_uid = obj
        .get(Tag(0x0002, 0x0010))
        .and_then(|el| el.value().to_str().ok())
        .map(|s| s.trim_end_matches([' ', '\0']).to_string())
        .unwrap_or_else(|| IMPLICIT_VR_LITTLE_ENDIAN.uid().to_string());

    log::info!(
        "surgical_transcode: source TS={} target TS={}",
        source_ts_uid,
        target_ts
    );

    // If source == target - no transcoding needed, just inject TS UID.
    if source_ts_uid == target_ts {
        log::info!("surgical_transcode: source==target — pass-through");
        return Ok(inject_transfer_syntax_uid(bytes.to_vec(), target_ts));
    }

    let source_codec = registry
        .get(&source_ts_uid)
        .ok_or_else(|| PipelineError::Stage(format!("no source codec for {}", source_ts_uid)))?;

    // Two decode paths:
    // - Uncompressed (Implicit VR LE): wrap with fake DICM preamble,
    //   ImplicitVrLeCodec -> ExplicitVrLeCodec -> DicomRsParser handles Implicit VR.
    // - Compressed (P14, RLE, JPEG2000, JPEG-LS, JPEG Baseline):
    //   codecs expect Explicit VR - build minimal Explicit VR DICOM for them.
    let decoded = if source_ts_uid == IMPLICIT_VR_LITTLE_ENDIAN.uid() {
        let mut wrapped = Vec::with_capacity(bytes.len() + 136);
        wrapped.extend_from_slice(&[0u8; 128]);
        wrapped.extend_from_slice(b"DICM");
        wrapped.extend_from_slice(bytes);
        source_codec
            .decode(&wrapped)
            .map_err(|e| PipelineError::Stage(format!("decode: {}", e)))?
    } else {
        // Compressed DIMSE: extract encapsulated PixelData from Implicit VR,
        // rebuild as minimal Explicit VR DICOM for the compressed codec.
        decode_compressed_dimse(bytes, source_codec, rows, cols, spp, bits, &pi)?
    };

    let target_codec = registry
        .get(target_ts)
        .ok_or_else(|| PipelineError::Stage(format!("no codec for {}", target_ts)))?;
    let compressed = target_codec
        .encode(
            &decoded,
            &DicomMeta {
                rows,
                columns: cols,
                samples_per_pixel: spp,
                bits_allocated: bits,
                bits_stored: bits,
                photometric_interpretation: pi.clone(),
                planar_configuration: None,
                pixel_representation,
                // The surgical DIMSE path decodes a single frame (rows x cols
                // from the tags); multi-frame DIMSE sources are handled by
                // the server-side transcode_dicom pipeline, not here.
                frames: 1,
            },
        )
        .map_err(|e| PipelineError::Stage(format!("encode: {}", e)))?;

    // Output: Explicit VR DICOM for compressed targets (PS3.5 requires
    // encapsulated PixelData with undefined length -> Explicit VR only).
    // Implicit VR for uncompressed targets (original DIMSE behavior).
    let is_compressed_target = target_ts != IMPLICIT_VR_LITTLE_ENDIAN.uid()
        && target_ts != "1.2.840.10008.1.2.1"
        && target_ts != "1.2.840.10008.1.2.2";

    if is_compressed_target {
        // Build proper Explicit VR DICOM file with preamble.
        log::info!(
            "surgical_transcode: building Explicit VR output ({}B compressed pixels, target={})",
            compressed.len(),
            target_ts
        );
        build_explicit_output(bytes, &compressed, target_ts)
    } else {
        // Uncompressed target: Implicit VR (original behavior).
        let new_pd = clarus_adapters::codec::build_encapsulated_pixeldata(&compressed, true);
        let mut result = replace_pixeldata(bytes, &new_pd)?;
        result = inject_transfer_syntax_uid(result, target_ts);
        Ok(result)
    }
}

/// Build a proper Explicit VR DICOM file with preamble for compressed output.
fn build_explicit_output(
    original_bytes: &[u8],
    compressed_pixels: &[u8],
    target_ts: &str,
) -> Result<Vec<u8>, PipelineError> {
    // Build the PixelData element in Explicit VR encapsulated format.
    let pd_element = clarus_adapters::codec::build_encapsulated_pixeldata(compressed_pixels, false);

    // Walk original Implicit VR dataset, collect all non-PixelData elements,
    // and build an Explicit VR DICOM file with preamble.
    let mut out = Vec::with_capacity(original_bytes.len() + 256);
    // Preamble
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");

    // Meta-header: (0002,0010) TransferSyntaxUID
    let ts_bytes = target_ts.as_bytes();
    let ts_padded = if ts_bytes.len().is_multiple_of(2) {
        ts_bytes.len()
    } else {
        ts_bytes.len() + 1
    };
    out.extend_from_slice(&0x0002u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"UI");
    out.extend_from_slice(&(ts_padded as u16).to_le_bytes());
    out.extend_from_slice(ts_bytes);
    if !ts_bytes.len().is_multiple_of(2) {
        out.push(0x00);
    }

    // Walk original Implicit VR elements, copy to Explicit VR.
    // Use vr_dict for VR lookup.
    let mut pos = 0usize;
    while pos + 8 <= original_bytes.len() {
        let group = u16::from_le_bytes([original_bytes[pos], original_bytes[pos + 1]]);
        let elem = u16::from_le_bytes([original_bytes[pos + 2], original_bytes[pos + 3]]);
        let vlen = u32::from_le_bytes([
            original_bytes[pos + 4],
            original_bytes[pos + 5],
            original_bytes[pos + 6],
            original_bytes[pos + 7],
        ]) as usize;

        if (group, elem) == (0x7FE0, 0x0010) {
            // Replace with new PixelData
            out.extend_from_slice(&pd_element);
            // Skip old PixelData (handle defined/undefined)
            if vlen == 0xFFFF_FFFF {
                // Walk Items to find end (reuse find_pixeldata_end_implicit)
                pos = find_pixeldata_end_implicit(original_bytes, pos)?;
            } else {
                pos += 8 + vlen;
            }
        } else if group == 0x0002 {
            // Skip meta-header elements (already written above)
            pos += 8 + vlen;
        } else {
            // Copy element, converting Implicit VR -> Explicit VR
            let vr = clarus_adapters::codec::vr_dict::vr_for_tag(group, elem);
            let val_start = pos + 8;
            let val_end = val_start + vlen;
            if val_end > original_bytes.len() {
                break;
            }

            out.extend_from_slice(&original_bytes[pos..pos + 4]); // tag
            out.extend_from_slice(vr.as_bytes());
            let is_ob = matches!(
                vr,
                "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            );
            if is_ob {
                out.extend_from_slice(&[0x00, 0x00]); // reserved
                out.extend_from_slice(&(vlen as u32).to_le_bytes());
            } else {
                out.extend_from_slice(&(vlen as u16).to_le_bytes());
            }
            out.extend_from_slice(&original_bytes[val_start..val_end]);
            pos = val_end;
        }
    }

    Ok(out)
}

/// Prepend (0002,0010) TransferSyntaxUID to DICOM bytes.
/// Written as Implicit VR LE: tag(4) + len(4) + value(padded to even).
fn inject_transfer_syntax_uid(mut data: Vec<u8>, ts_uid: &str) -> Vec<u8> {
    let mut val = ts_uid.as_bytes().to_vec();
    if val.len().is_multiple_of(2) {
        val.push(0x00); // DICOM UI must be even-length, null-padded
    }
    let mut out = Vec::with_capacity(data.len() + 8 + val.len());
    // Tag (0002,0010)
    out.extend_from_slice(&0x0002u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    // Length (4 bytes)
    out.extend_from_slice(&(val.len() as u32).to_le_bytes());
    out.extend_from_slice(&val);
    out.append(&mut data);
    out
}

/// Decode compressed DIMSE data by building a minimal Explicit VR DICOM file
/// that the compressed codec can parse (all compressed codecs expect Explicit VR).
///
/// nginx-way: one conversion at the boundary serves ALL compressed codecs -
/// P14, RLE, JPEG2000, JPEG-LS, JPEG Baseline.
fn decode_compressed_dimse(
    bytes: &[u8],
    source_codec: &dyn clarus_adapters::codec::DicomCodec,
    rows: u16,
    cols: u16,
    spp: u8,
    bits: u8,
    pi: &str,
) -> Result<clarus_adapters::codec::DecodedPixels, PipelineError> {
    // 1. Extract encapsulated PixelData from Implicit VR DIMSE data.
    //    In Implicit VR: tag(4) + undefined_len(4, 0xFFFFFFFF) + Items...
    //    We need the Items bytes (including BOT, fragment, SequenceDelimiter)
    //    to embed into an Explicit VR wrapper.
    let pd_items = extract_pixeldata_items_implicit(bytes)?;

    // 2. Build minimal Explicit VR DICOM file.
    //    [128x00] [DICM] [(0002,0010) UI] [metadata US/CS] [(7FE0,0010) OB undefined {pd_items}]
    let minimal = build_minimal_explicit_dicom(
        source_codec.transfer_syntax_uid(),
        rows,
        cols,
        spp,
        bits,
        pi,
        &pd_items,
    );

    // 3. Decode using the correct compressed codec.
    source_codec
        .decode(&minimal)
        .map_err(|e| PipelineError::Stage(format!("decode: {}", e)))
}

/// Find the end position of an encapsulated PixelData element in Implicit VR
/// (or Explicit VR with undefined length - the Items structure is identical).
///
/// Walks Items from `tag_pos + 8` (past tag + undefined-length marker)
/// until SequenceDelimiter (FFFE,E0DD). Returns the byte offset of the
/// byte AFTER the SequenceDelimiter.
fn find_pixeldata_end_implicit(data: &[u8], tag_pos: usize) -> Result<usize, PipelineError> {
    // Past tag(4) + undefined length marker(4) - same for Implicit VR (4B len)
    // and Explicit VR (VR2+reserved2+len4=8B). Caller handles the offset.
    let after_len = if data.len() >= tag_pos + 12 {
        // Check if Explicit VR: tag(4) + VR(2) + reserved(2) + len(4) = 12 bytes header
        let vr_slot = &data[tag_pos + 4..tag_pos + 6];
        if vr_slot == b"OB"
            || vr_slot == b"OW"
            || vr_slot == b"OD"
            || vr_slot == b"OF"
            || vr_slot == b"OL"
            || vr_slot == b"SQ"
            || vr_slot == b"UC"
            || vr_slot == b"UN"
            || vr_slot == b"UR"
            || vr_slot == b"UT"
        {
            tag_pos + 12
        } else {
            tag_pos + 8
        }
    } else {
        tag_pos + 8
    };

    let mut cursor = after_len;
    loop {
        if cursor + 8 > data.len() {
            return Err(PipelineError::Stage(
                "truncated in encapsulated PixelData".into(),
            ));
        }
        let tag = (
            u16::from_le_bytes([data[cursor], data[cursor + 1]]),
            u16::from_le_bytes([data[cursor + 2], data[cursor + 3]]),
        );
        let item_len = u32::from_le_bytes([
            data[cursor + 4],
            data[cursor + 5],
            data[cursor + 6],
            data[cursor + 7],
        ]) as usize;

        match tag {
            (0xFFFE, 0xE000) => {
                cursor += 8;
                if item_len == 0xFFFF_FFFF {
                    cursor = skip_until_item_delimiter(data, cursor)?;
                } else {
                    cursor += item_len;
                }
            }
            (0xFFFE, 0xE0DD) => {
                return Ok(cursor + 8); // past delimiter
            }
            _ => {
                return Err(PipelineError::Stage(format!(
                    "unexpected tag in PixelData: ({:04X},{:04X})",
                    tag.0, tag.1
                )));
            }
        }
    }
}

/// Extract the Items portion of an encapsulated PixelData from Implicit VR bytes.
///
/// Implicit VR encapsulated PixelData layout:
///   tag (7FE0,0010) [4B] + undefined length (0xFFFFFFFF) [4B] +
///   BOT Item (FFFE,E000, len=0) [8B] +
///   Fragment Item (FFFE,E000, len=N) [8B] + N bytes +
///   SequenceDelimiter (FFFE,E0DD, len=0) [8B]
///
/// Returns the bytes starting from BOT through SequenceDelimiter (inclusive).
fn extract_pixeldata_items_implicit(data: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let tag_pat = &[0xE0u8, 0x7F, 0x10, 0x00];
    let pos = data
        .windows(4)
        .position(|w| w == tag_pat)
        .ok_or_else(|| PipelineError::Stage("PixelData not found in Implicit VR".into()))?;

    // In Implicit VR: tag(4) + length(4)
    if pos + 8 > data.len() {
        return Err(PipelineError::Stage("truncated at PixelData length".into()));
    }
    let len_raw = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
    if len_raw != 0xFFFF_FFFF {
        return Err(PipelineError::Stage(format!(
            "expected undefined length (0xFFFFFFFF), got 0x{len_raw:08X}"
        )));
    }

    // Walk Items from pos+8 until SequenceDelimiter (FFFE,E0DD).
    let mut cursor = pos + 8;
    loop {
        if cursor + 8 > data.len() {
            return Err(PipelineError::Stage(
                "truncated in encapsulated PixelData".into(),
            ));
        }
        let tag = (
            u16::from_le_bytes([data[cursor], data[cursor + 1]]),
            u16::from_le_bytes([data[cursor + 2], data[cursor + 3]]),
        );
        let item_len = u32::from_le_bytes([
            data[cursor + 4],
            data[cursor + 5],
            data[cursor + 6],
            data[cursor + 7],
        ]) as usize;

        match tag {
            (0xFFFE, 0xE000) => {
                // Item - skip over it
                cursor += 8;
                if item_len == 0xFFFF_FFFF {
                    // Nested undefined length - skip until ItemDelimiter
                    cursor = skip_until_item_delimiter(data, cursor)?;
                } else {
                    cursor += item_len;
                }
            }
            (0xFFFE, 0xE0DD) => {
                // SequenceDelimiter - end of encapsulated data
                cursor += 8; // include delimiter in output
                return Ok(data[pos + 8..cursor].to_vec());
            }
            _ => {
                return Err(PipelineError::Stage(format!(
                    "unexpected tag ({:04X},{:04X}) in encapsulated PixelData",
                    tag.0, tag.1
                )));
            }
        }
    }
}

/// Skip until (FFFE,E00D) ItemDelimiter with matching nesting.
fn skip_until_item_delimiter(data: &[u8], mut cursor: usize) -> Result<usize, PipelineError> {
    let mut depth: u32 = 1;
    loop {
        if cursor + 8 > data.len() {
            return Err(PipelineError::Stage("truncated in nested sequence".into()));
        }
        let tag = (
            u16::from_le_bytes([data[cursor], data[cursor + 1]]),
            u16::from_le_bytes([data[cursor + 2], data[cursor + 3]]),
        );
        let len = u32::from_le_bytes([
            data[cursor + 4],
            data[cursor + 5],
            data[cursor + 6],
            data[cursor + 7],
        ]) as usize;

        match tag {
            (0xFFFE, 0xE000) => {
                cursor += 8;
                if len == 0xFFFF_FFFF {
                    depth += 1;
                } else {
                    cursor += len;
                }
            }
            (0xFFFE, 0xE00D) => {
                cursor += 8; // len is always 0x00000000
                depth -= 1;
                if depth == 0 {
                    return Ok(cursor);
                }
            }
            (0xFFFE, 0xE0DD) => {
                cursor += 8;
                depth -= 1;
                if depth == 0 {
                    return Ok(cursor);
                }
            }
            _ => {
                return Err(PipelineError::Stage(format!(
                    "unexpected tag in nested seq: ({:04X},{:04X})",
                    tag.0, tag.1
                )));
            }
        }
    }
}

/// Build a minimal Explicit VR LE DICOM file sufficient for compressed codec decode.
///
/// Layout:
///   [128x00] [DICM]
///   (0002,0010) UI {source_ts_uid}
///   (0028,0010) US {rows}
///   (0028,0011) US {cols}
///   (0028,0002) US {spp}
///   (0028,0100) US {bits}
///   (0028,0101) US {bits}
///   (0028,0004) CS {pi}
///   (7FE0,0010) OB 0xFFFFFFFF {pd_items}
fn build_minimal_explicit_dicom(
    source_ts_uid: &str,
    rows: u16,
    cols: u16,
    spp: u8,
    bits: u8,
    pi: &str,
    pd_items: &[u8],
) -> Vec<u8> {
    /// Write an Explicit VR element: tag(4) + VR(2) + len(2) + value.
    fn write_us(out: &mut Vec<u8>, elem: u16, val: u16) {
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(b"US");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }

    fn write_cs(out: &mut Vec<u8>, elem: u16, val: &str) {
        let v = val.as_bytes();
        // DICOM PS3.5: CS values must be even-length, pad with SPACE (0x20).
        let pad = !v.len().is_multiple_of(2);
        let padded_len = v.len() + pad as usize;
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(b"CS");
        out.extend_from_slice(&(padded_len as u16).to_le_bytes());
        out.extend_from_slice(v);
        if pad {
            out.push(0x20);
        }
    }

    // Precompute total size for single allocation.
    let ts_bytes = source_ts_uid.as_bytes();
    let ts_padded = if ts_bytes.len().is_multiple_of(2) {
        ts_bytes.len()
    } else {
        ts_bytes.len() + 1
    };
    // Meta-header: tag(4) + VR"UI"(2) + len(2) + value(ts_padded)
    let mh_size = 8 + ts_padded;
    // 5 US elements (tag+VR+len+val = 4+2+2+2 = 10B each) + 1 CS + PixelData header
    let ds_meta = 5 * 10 + 8 + pi.len() + if !pi.len().is_multiple_of(2) { 1 } else { 0 };
    // PixelData: tag(4) + VR"OB"(2) + reserved(2) + undefined_len(4) + items
    let pd_hdr = 12;
    let total = 128 + 4 + mh_size + ds_meta + pd_hdr + pd_items.len();
    let mut out = Vec::with_capacity(total);

    // Preamble
    out.extend_from_slice(&[0u8; 128]);
    out.extend_from_slice(b"DICM");

    // Meta-header: (0002,0010) TransferSyntaxUID
    out.extend_from_slice(&0x0002u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"UI");
    out.extend_from_slice(&(ts_padded as u16).to_le_bytes());
    out.extend_from_slice(ts_bytes);
    if !ts_bytes.len().is_multiple_of(2) {
        out.push(0x00); // NULL pad for UI
    }

    // Dataset: image metadata
    write_us(&mut out, 0x0010, rows);
    write_us(&mut out, 0x0011, cols);
    write_us(&mut out, 0x0002, spp as u16);
    write_us(&mut out, 0x0100, bits as u16);
    write_us(&mut out, 0x0101, bits as u16);
    write_cs(&mut out, 0x0004, pi);

    // PixelData: Explicit VR encapsulated
    out.extend_from_slice(&0x7FE0u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"OB");
    out.extend_from_slice(&[0x00, 0x00]); // reserved
    out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    out.extend_from_slice(pd_items);

    out
}

/// Find PixelData (7FE0,0010) in raw DICOM bytes and replace it.
/// Handles both Explicit VR and Implicit VR (DIMSE C-STORE data).
#[allow(unused_assignments)]
fn replace_pixeldata(original: &[u8], new_pixeldata: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let tag = &[0xE0u8, 0x7F, 0x10, 0x00];
    let pos = original
        .windows(4)
        .position(|w| w == tag)
        .ok_or_else(|| PipelineError::Stage("PixelData not found".into()))?;

    // Detect Implicit VR (DIMSE data has no DICM magic at offset 128)
    let is_implicit = original.len() < 132 || &original[128..132] != b"DICM";

    let old_end = if is_implicit {
        // Implicit VR: tag(4) + length(4) + value
        let l = u32::from_le_bytes([
            original[pos + 4],
            original[pos + 5],
            original[pos + 6],
            original[pos + 7],
        ]);
        if l == 0xFFFF_FFFF {
            // Undefined length - walk Items to find true end.
            find_pixeldata_end_implicit(original, pos)?
        } else {
            pos + 8 + l as usize
        }
    } else {
        let mut cursor = pos + 4;
        let vr = std::str::from_utf8(&original[cursor..cursor + 2]).unwrap_or("");
        cursor += 2;
        let is_ob = matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        let (hdr_size, l) = if is_ob {
            cursor += 2;
            let l = u32::from_le_bytes([
                original[cursor],
                original[cursor + 1],
                original[cursor + 2],
                original[cursor + 3],
            ]);
            cursor += 4;
            (12, l)
        } else {
            let l = u16::from_le_bytes([original[cursor], original[cursor + 1]]) as u32;
            cursor += 2;
            (8, l)
        };
        if l == 0xFFFF_FFFF {
            // Undefined length (encapsulated) - walk Items to find true end.
            find_pixeldata_end_implicit(original, pos)?
        } else {
            pos + hdr_size + l as usize
        }
    };

    let old_pd_len = old_end - pos;
    let new_capacity = original.len() - old_pd_len + new_pixeldata.len();
    let mut out = Vec::with_capacity(new_capacity);
    out.extend_from_slice(&original[..pos]);
    out.extend_from_slice(new_pixeldata);
    if old_end < original.len() {
        out.extend_from_slice(&original[old_end..]);
    }
    Ok(out)
}

pub(crate) fn has_pixel_data(data: &[u8]) -> bool {
    let tag = [0xE0u8, 0x7F, 0x10, 0x00];
    data.windows(4).any(|w| w == tag)
}

/// Check if data starts with valid Explicit VR (e.g. "UI", "CS", "OB").
/// Looks at the VR bytes of the first element after any meta-header.
pub(crate) fn is_explicit_vr(data: &[u8]) -> bool {
    if data.len() < 6 {
        return false;
    }
    // If data starts with group 0x0002 (meta-header), skip to first dataset element.
    let mut pos = 0usize;
    while pos + 6 <= data.len() && pos < 512 {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if group != 0x0002 {
            break;
        }
        // Skip this meta-header element (Explicit VR LE)
        pos += 4; // tag
        if pos + 2 > data.len() {
            return false;
        }
        let is_ob = matches!(
            &data[pos..pos + 2],
            b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        pos += 2; // VR
        if is_ob {
            if pos + 6 > data.len() {
                return false;
            }
            pos += 2; // reserved
            let vlen = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4 + vlen;
        } else {
            let vlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2 + vlen;
        }
    }
    // Now check VR of the first non-meta element
    if pos + 6 > data.len() {
        return false;
    }
    let vr = &data[pos + 4..pos + 6];
    matches!(
        std::str::from_utf8(vr).ok(),
        Some(
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
                | "OB"
                | "OD"
                | "OF"
                | "OL"
                | "OW"
                | "PN"
                | "SH"
                | "SL"
                | "SQ"
                | "SS"
                | "ST"
                | "TM"
                | "UC"
                | "UI"
                | "UL"
                | "UN"
                | "UR"
                | "US"
                | "UT"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pixeldata_passes_through() {
        let data = build_dicom(false);
        let registry = CodecRegistry::new();
        let t = Transcoder::new(
            "1.2.840.10008.1.2.4.70".into(),
            registry,
            "CLTRANSCODE".into(),
        );
        let result = t.process(data.clone()).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn bad_bytes_pass_through() {
        let bad = vec![0x00, 0x01];
        let registry = CodecRegistry::new();
        let t = Transcoder::new(
            "1.2.840.10008.1.2.4.70".into(),
            registry,
            "CLTRANSCODE".into(),
        );
        let result = t.process(bad.clone()).unwrap();
        assert_eq!(result, bad);
    }

    // -- is_explicit_vr tests --------------------------------------

    #[test]
    fn is_explicit_vr_detects_valid_vr() {
        // Minimal Explicit VR: (0010,0010) PN "Test"
        let mut data = Vec::new();
        data.extend_from_slice(&0x0010u16.to_le_bytes()); // group
        data.extend_from_slice(&0x0010u16.to_le_bytes()); // elem
        data.extend_from_slice(b"PN"); // VR
        data.extend_from_slice(&4u16.to_le_bytes()); // len
        data.extend_from_slice(b"Test"); // value
        assert!(is_explicit_vr(&data));
    }

    #[test]
    fn is_explicit_vr_rejects_implicit() {
        // Implicit VR: (0010,0010) len=4 value="Test"
        let mut data = Vec::new();
        data.extend_from_slice(&0x0010u16.to_le_bytes()); // group
        data.extend_from_slice(&0x0010u16.to_le_bytes()); // elem
        data.extend_from_slice(&4u32.to_le_bytes()); // len (4 bytes, Implicit VR)
        data.extend_from_slice(b"Test"); // value
        assert!(!is_explicit_vr(&data));
    }

    #[test]
    fn is_explicit_vr_skips_meta_header() {
        // Meta-header (0002,0010) UI + dataset (0028,0010) US
        let mut data = Vec::new();
        // Meta element: (0002,0010) UI "1.2.840.10008.1.2.1"
        data.extend_from_slice(&0x0002u16.to_le_bytes());
        data.extend_from_slice(&0x0010u16.to_le_bytes());
        data.extend_from_slice(b"UI");
        let ts = b"1.2.840.10008.1.2.1\0";
        data.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        data.extend_from_slice(ts);
        // Dataset: (0028,0010) US 512
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0010u16.to_le_bytes());
        data.extend_from_slice(b"US");
        data.extend_from_slice(&512u16.to_le_bytes());
        assert!(is_explicit_vr(&data));
    }

    #[test]
    fn is_explicit_vr_empty_returns_false() {
        assert!(!is_explicit_vr(&[]));
        assert!(!is_explicit_vr(&[0u8; 5]));
    }

    // -- Explicit VR DIMSE path tests ------------------------------

    /// Build Explicit VR DIMSE data (no preamble) - mimics what SCP produces.
    fn build_dimse_explicit(ts_uid: &str, rows: u16, cols: u16) -> Vec<u8> {
        let mut d = Vec::new();
        // Meta-header: (0002,0010) UI
        d.extend_from_slice(&0x0002u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"UI");
        let ts_bytes = if ts_uid.len().is_multiple_of(2) {
            let mut v = ts_uid.as_bytes().to_vec();
            v.push(0x00);
            v
        } else {
            ts_uid.as_bytes().to_vec()
        };
        d.extend_from_slice(&(ts_bytes.len() as u16).to_le_bytes());
        d.extend_from_slice(&ts_bytes);
        // Dataset: metadata
        fn wr_us(buf: &mut Vec<u8>, elem: u16, val: u16) {
            buf.extend_from_slice(&0x0028u16.to_le_bytes());
            buf.extend_from_slice(&elem.to_le_bytes());
            buf.extend_from_slice(b"US");
            buf.extend_from_slice(&2u16.to_le_bytes());
            buf.extend_from_slice(&val.to_le_bytes());
        }
        wr_us(&mut d, 0x0010, rows);
        wr_us(&mut d, 0x0011, cols);
        wr_us(&mut d, 0x0002, 1); // spp=1
        wr_us(&mut d, 0x0100, 8); // bits=8
        wr_us(&mut d, 0x0101, 8);
        // PhotometricInterpretation CS
        d.extend_from_slice(&0x0028u16.to_le_bytes());
        d.extend_from_slice(&0x0004u16.to_le_bytes());
        d.extend_from_slice(b"CS");
        let pi = b"MONOCHROME2 ";
        d.extend_from_slice(&(pi.len() as u16).to_le_bytes());
        d.extend_from_slice(pi);
        // PixelData OW with raw pixels (2x2=4 pixels, 8-bit = 4 bytes)
        let pixels = vec![0x10, 0x20, 0x30, 0x40u8];
        d.extend_from_slice(&0x7FE0u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"OW");
        d.extend_from_slice(&[0x00, 0x00]); // reserved
        d.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        d.extend_from_slice(&pixels);
        d
    }

    #[test]
    fn dimse_explicit_vr_path_triggers() {
        // Explicit VR DIMSE data (uncompressed, Implicit VR LE target -> pass-through)
        let data = build_dimse_explicit("1.2.840.10008.1.2.1", 2, 2);
        assert!(has_pixel_data(&data));
        assert!(is_explicit_vr(&data));
        // No DICM preamble -> is_dimse
        assert!(data.len() < 132 || &data[128..132] != b"DICM");

        let registry = CodecRegistry::default();
        // Target = Implicit VR LE -> early pass-through (before surgical_transcode)
        let t = Transcoder::new("1.2.840.10008.1.2".into(), registry, "CLTRANSCODE".into());
        let result = t.process(data.clone()).unwrap();
        // Should pass through unchanged (DIMSE Implicit VR LE target matches)
        assert_eq!(result.len(), data.len());
    }

    #[test]
    fn dimse_explicit_vr_to_explicit_vr_transcodes() {
        // Explicit VR DIMSE -> Explicit VR LE target (same-endianness transcode)
        let data = build_dimse_explicit("1.2.840.10008.1.2.1", 2, 2);
        let registry = CodecRegistry::default();
        // Target = Explicit VR LE
        let t = Transcoder::new("1.2.840.10008.1.2.1".into(), registry, "CLTRANSCODE".into());
        let result = t.process(data.clone()).unwrap();
        // transcode_dicom path: source==target -> no-op
        // Output should have DICM preamble
        assert!(result.len() >= 132);
        assert_eq!(&result[128..132], b"DICM");
    }

    #[test]
    fn dimse_implicit_vr_still_works() {
        // Implicit VR DIMSE data (uncompressed) -> existing surgical_transcode path
        let mut data = Vec::new();
        // (0028,0010) Implicit VR: tag + 4B len + value
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0010u16.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes()); // rows=2
        // (0028,0011)
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0011u16.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes()); // cols=2
        // (0028,0002)
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0002u16.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes()); // spp=1
        // (0028,0100)
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0100u16.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&8u16.to_le_bytes()); // bits=8
        // (0028,0101)
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0101u16.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&8u16.to_le_bytes());
        // (0028,0004) CS "MONOCHROME2"
        data.extend_from_slice(&0x0028u16.to_le_bytes());
        data.extend_from_slice(&0x0004u16.to_le_bytes());
        let pi = b"MONOCHROME2 ";
        data.extend_from_slice(&(pi.len() as u32).to_le_bytes());
        data.extend_from_slice(pi);
        // (7FE0,0010) OW implicit: tag + len(4) + pixels
        let pixels = vec![0x10, 0x20, 0x30, 0x40u8];
        data.extend_from_slice(&0x7FE0u16.to_le_bytes());
        data.extend_from_slice(&0x0010u16.to_le_bytes());
        data.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        data.extend_from_slice(&pixels);

        assert!(!is_explicit_vr(&data)); // Implicit VR
        assert!(has_pixel_data(&data));

        let registry = CodecRegistry::default();
        // Target = Implicit VR LE -> pass-through
        let t = Transcoder::new("1.2.840.10008.1.2".into(), registry, "CLTRANSCODE".into());
        let result = t.process(data.clone()).unwrap();
        assert_eq!(result.len(), data.len()); // unchanged
    }

    fn build_dicom(with_pixeldata: bool) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&[0u8; 128]);
        d.extend_from_slice(b"DICM");
        fn wr_short(buf: &mut Vec<u8>, g: u16, e: u16, vr: &[u8; 2], val: &[u8]) {
            buf.extend_from_slice(&g.to_le_bytes());
            buf.extend_from_slice(&e.to_le_bytes());
            buf.extend_from_slice(vr);
            buf.extend_from_slice(&(val.len() as u16).to_le_bytes());
            buf.extend_from_slice(val);
        }
        fn wr_long(buf: &mut Vec<u8>, g: u16, e: u16, vr: &[u8; 2], val: &[u8]) {
            buf.extend_from_slice(&g.to_le_bytes());
            buf.extend_from_slice(&e.to_le_bytes());
            buf.extend_from_slice(vr);
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&(val.len() as u32).to_le_bytes());
            buf.extend_from_slice(val);
        }
        wr_long(&mut d, 0x0002, 0x0001, b"OB", &[0x00, 0x01]);
        wr_short(&mut d, 0x0002, 0x0010, b"UI", b"1.2.840.10008.1.2.1\0");
        wr_short(
            &mut d,
            0x0008,
            0x0016,
            b"UI",
            b"1.2.840.10008.5.1.4.1.1.2\0",
        );
        if with_pixeldata {
            wr_long(&mut d, 0x7FE0, 0x0010, b"OW", &[0xAA; 4]);
        }
        d
    }
}
