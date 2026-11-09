// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Top-level DICOM transcoding: decode -> optional color-space conversion -> encode.
//!
//! ```text
//! DICOM bytes (source TS) -> decode -> raw pixels -> encode -> DICOM bytes (target TS)
//! ```
//!
//! # Pipeline
//!
//! 1. Parse source DICOM to extract metadata + pixel data
//! 2. Detect source transfer syntax
//! 3. Find source codec -> decode -> raw pixels (planar)
//! 4. Optional: convert color space if source & target photometric differ
//! 5. Find target codec -> encode -> compressed pixel bytes
//! 6. Rebuild DICOM dataset with new PixelData + updated meta-header

#![deny(unsafe_code)]

use crate::codec::{CodecError, CodecRegistry, DicomMeta};

/// Re-encode a DATASET from Implicit VR Little Endian to Explicit VR Little
/// Endian, without touching pixel data.
///
/// `transcode_dicom` is pixel-oriented: it decodes the image, applies any
/// colour conversion and re-encodes. An object with no PixelData at all (an
/// SR, a Presentation State, a Surface Segmentation) has no Rows/Columns for
/// the decoder to work from, so that pipeline cannot serve it - and WADO-RS
/// may not serve it as Implicit VR either (PS3.18 8.7.3: "The Implicit VR
/// Little Endian Transfer Syntax shall not be used with Web Services", a rule
/// the standard does not scope to images).
///
/// This is the dataset-level answer: walk the elements and re-emit each header
/// in Explicit VR, leaving every VALUE byte untouched. PixelData, when the
/// object happens to have one, is carried as an ordinary element because its
/// value needs no reinterpretation - the fail here is in the HEADERS, and only
/// the headers change.
///
/// `dicom_bytes` must carry Implicit VR LE; the caller checks the transfer
/// syntax from the manifest. The File Meta Information group is rewritten with
/// the target Transfer Syntax UID so the file describes itself correctly.
///
/// # Errors
/// - the file has no File Meta Information to locate the dataset from
/// - the dataset walk hits a truncated element
pub fn reencode_implicit_to_explicit(dicom_bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
    let data_start = if dicom_bytes.len() >= 132 && &dicom_bytes[128..132] == b"DICM" {
        132
    } else if dicom_bytes.len() >= 4 && &dicom_bytes[0..4] == b"DICM" {
        4
    } else {
        return Err(CodecError::InvalidMetadata(
            "No DICM prefix: not a DICOM file".into(),
        ));
    };
    let dataset_start = find_metaheader_end(dicom_bytes, data_start)?;

    let mut out: Vec<u8> = Vec::with_capacity(dicom_bytes.len());
    // The meta group is copied verbatim first so `ensure_file_meta` can find
    // its own elements and replace the transfer syntax UID in place.
    out.extend_from_slice(&dicom_bytes[..dataset_start]);
    if out.len() < 128 {
        return Err(CodecError::InvalidMetadata("Truncated preamble".into()));
    }
    out[128..132].copy_from_slice(b"DICM");
    copy_chunk_reencoded(
        &mut out,
        dicom_bytes,
        dataset_start,
        dicom_bytes.len(),
        true,
    )?;
    ensure_file_meta(&mut out, dicom_bytes, crate::codec::ts_uid::EXPLICIT_VR_LE);
    Ok(out)
}

/// Transcode a DICOM dataset from its original transfer syntax to a target one.
///
/// Returns the full DICOM dataset with updated PixelData and metadata.
///
/// # Arguments
/// - `dicom_bytes`: The original DICOM file bytes.
/// - `target_ts`: Target Transfer Syntax UID (e.g. "1.2.840.10008.1.2.5").
/// - `registry`: Codec registry for looking up source and target codecs.
///
/// # Errors
/// Returns `CodecError` if:
/// - Source or target transfer syntax is unsupported
/// - Decoding or encoding fails
/// - Required metadata is missing
pub fn transcode_dicom(
    dicom_bytes: &[u8],
    target_ts: &str,
    registry: &CodecRegistry,
) -> Result<Vec<u8>, CodecError> {
    // -- 1. Detect source transfer syntax --------------------------
    let source_ts = extract_transfer_syntax(dicom_bytes).ok_or_else(|| {
        CodecError::InvalidMetadata("Cannot detect transfer syntax from DICOM meta-header".into())
    })?;

    log::info!(
        "transcode: {} -> {} (input={}B)",
        source_ts,
        target_ts,
        dicom_bytes.len()
    );

    let source_is_be = source_ts.as_str() == crate::codec::ts_uid::EXPLICIT_VR_BE;
    let is_implicit = source_ts.as_str() == crate::codec::ts_uid::IMPLICIT_VR_LE;
    let palette_color = cs_value_equals(
        dicom_bytes,
        (0x0028, 0x0004),
        "PALETTE COLOR",
        source_is_be,
        is_implicit,
    );

    // If source == target, no transcoding needed - EXCEPT for PALETTE COLOR
    // sources, which the browser miniviewer cannot render: a palette is an
    // index-to-RGB map and the viewer paints raw samples. Falling through to
    // decode + palette-to-RGB keeps the demo colorful (01.09 owner request).
    if source_ts == target_ts && !palette_color {
        log::info!("transcode: source == target, no-op");
        return Ok(dicom_bytes.to_vec());
    }

    // -- 2. Get source and target codecs --------------------------
    let source_codec = registry.get(&source_ts).ok_or_else(|| {
        CodecError::UnsupportedTransferSyntax(format!(
            "Source transfer syntax '{}' is not supported for decoding",
            source_ts
        ))
    })?;

    let target_codec = registry.get(target_ts).ok_or_else(|| {
        CodecError::UnsupportedTransferSyntax(format!(
            "Target transfer syntax '{}' is not supported for encoding",
            target_ts
        ))
    })?;

    log::info!(
        "transcode: codecs {} -> {}",
        source_codec.name(),
        target_codec.name()
    );

    // -- 3. Decode source -> raw pixels ----------------------------
    let mut pixels = source_codec.decode(dicom_bytes)?;

    // NumberOfFrames (0028,0008) IS, default 1. Multi-frame pixel data
    // holds all frames concatenated (per plane, frame-major).
    let frames = number_of_frames(dicom_bytes, source_ts.as_str())?;

    // Validate decoded pixels (frame-aware size check)
    pixels.validate_frames(frames)?;

    // PALETTE COLOR: map sample values to RGB through the palette LUT
    // (PS3.3 C.7.9). Downstream consumers (browser miniviewer) cannot apply
    // palettes, so the conversion happens HERE, once, for every target TS.
    let mut palette_applied = false;
    if pixels.photometric_interpretation == "PALETTE COLOR" {
        apply_palette_color_lut(dicom_bytes, &mut pixels, source_is_be, is_implicit)?;
        palette_applied = true;
    }

    log::info!(
        "transcode: decode OK {}x{} spp={} bits={} pi={}",
        pixels.rows,
        pixels.columns,
        pixels.samples_per_pixel,
        pixels.bits_allocated,
        pixels.photometric_interpretation
    );

    // -- 4. Color space conversion (if needed) --------------------
    // Determine target photometric interpretation based on target TS
    let target_pi = photometric_for_ts(target_ts, &pixels.photometric_interpretation);

    if pixels.photometric_interpretation != target_pi {
        let converted = crate::codec::color_space::convert_color_space(
            &pixels.data,
            &pixels.photometric_interpretation,
            &target_pi,
            pixels.samples_per_pixel,
            pixels.bits_allocated,
        )?;
        let old_pi = pixels.photometric_interpretation.clone();
        pixels.data = converted;
        pixels.photometric_interpretation = target_pi;
        log::info!(
            "transcode: color_space {} -> {}",
            old_pi,
            pixels.photometric_interpretation
        );
    }

    // -- 4.5. Bit depth reduction 16->8 for targets that need it ---
    // JPEG Baseline: hardware 8-bit limit.
    // Explicit/Implicit VR LE: reduced for clarus-viewer.html compat.
    // PS3.3 C.7.6.3.1.1: BitsStored is how many bits actually carry data, and
    // it is NOT BitsAllocated. Rewriting a 12-bit CT as 16/15 told every
    // consumer the top four bits were meaningful; the values are unchanged,
    // only the declared width was wrong.
    let original_bits_stored: u8 = crate::codec::bits_stored_of(dicom_bytes).unwrap_or_else(|| {
        let allocated = pixels.bits_allocated;
        // A dataset without the element is 8-bit or fully populated by
        // convention; the reader below clamps to the allocated width.
        allocated
    });
    // Whether the samples were actually remapped. The window rewrite keys off
    // this, not off a comparison between two unrelated widths.
    let mut reduced_to_8bit = false;
    if pixels.bits_allocated > 8 && target_requires_8bit(target_ts) {
        if frames > 1 {
            return Err(CodecError::InvalidMetadata(
                "bit-depth reduction 16->8 is not supported for multi-frame images".into(),
            ));
        }
        pixels = crate::bit_depth::reduce_bit_depth_16to8(&pixels, dicom_bytes)?;
        reduced_to_8bit = true;
        log::info!("transcode: bit_depth reduced 16->8");
    }

    // -- 5. Build metadata for target encoder ---------------------
    // A reduced image is genuinely 8 bits now; otherwise the source width
    // stands. The reduction is the only case that may change it.
    let bits_stored = if pixels.bits_allocated < original_bits_stored {
        pixels.bits_allocated
    } else {
        original_bits_stored.min(pixels.bits_allocated)
    };
    let meta = DicomMeta {
        rows: pixels.rows,
        columns: pixels.columns,
        samples_per_pixel: pixels.samples_per_pixel,
        bits_allocated: pixels.bits_allocated,
        bits_stored,
        photometric_interpretation: pixels.photometric_interpretation.clone(),
        planar_configuration: None,
        // PixelRepresentation from the SOURCE dataset (JPEG2000 encode
        // needs it: signed=1 samples encoded as unsigned shift values).
        pixel_representation: crate::codec::pixel_representation_of(dicom_bytes),
        frames,
    };

    // -- 6. Encode pixels -> complete PixelData element ---------
    // nginx-way: codec builds its own PixelData element (native or encapsulated).
    // transcode_direct just inserts it - knows nothing about format.

    let target_is_be = target_ts == crate::codec::ts_uid::EXPLICIT_VR_BE;

    // -- 7. Assemble output DICOM ------------------------------
    // Architecture: two code paths, one if.
    // - transcode_direct: memcpy 129 untouched elements. Same-endianness only.
    // - update_dicom_metadata: element-by-element rebuild with byte-swap.
    //   Cross-endianness only (<0.01% of real-world DICOM traffic).

    let output = if source_is_be != target_is_be {
        // Compressed targets always build the encapsulated element through
        // `encode_pixeldata`, not only when there are several frames: a
        // single-frame colour JPEG-LS still yields one codestream per
        // COMPONENT, and `encode()` concatenates them into one blob that the
        // decoder rejects. Fragment boundaries are not recoverable from that
        // concatenation, so the codec has to supply them.
        let (raw_pixel_bytes, pixeldata_element) = if is_compressed_ts(target_ts) {
            let element = target_codec.encode_pixeldata(&pixels, &meta)?;
            let ratio = element.len() as f64 / dicom_bytes.len() as f64 * 100.0;
            log::info!(
                "transcode: encode OK {}B ({:.1}%) — cross-endianness rebuild (multi-frame element)",
                element.len(),
                ratio
            );
            (Vec::new(), Some(element))
        } else {
            let bytes = target_codec.encode(&pixels, &meta)?;
            let ratio = bytes.len() as f64 / dicom_bytes.len() as f64 * 100.0;
            log::info!(
                "transcode: encode OK {}B ({:.1}%) — cross-endianness rebuild",
                bytes.len(),
                ratio
            );
            (bytes, None)
        };
        update_dicom_metadata(
            dicom_bytes,
            &raw_pixel_bytes,
            pixeldata_element.as_deref(),
            target_ts,
            &pixels.photometric_interpretation,
            pixels.bits_allocated,
            bits_stored,
        )?
    } else {
        // Same-endianness: direct memcpy.
        let new_pixeldata_element = target_codec.encode_pixeldata(&pixels, &meta)?;
        let ratio = new_pixeldata_element.len() as f64 / dicom_bytes.len() as f64 * 100.0;
        log::info!(
            "transcode: encode OK {}B ({:.1}%)",
            new_pixeldata_element.len(),
            ratio
        );
        transcode_direct(
            dicom_bytes,
            &new_pixeldata_element,
            target_ts,
            &pixels.photometric_interpretation,
            pixels.bits_allocated,
            // The source's BitsStored describes how many bits actually carry
            // data; `bits_allocated` is only the container width. Declaring the
            // container width lost the 12-bit case, which the source had
            // described correctly. After a reduction the value is 8, which is
            // what `bits_stored` already resolves to.
            bits_stored,
            source_is_be,
            is_implicit,
            reduced_to_8bit,
        )?
    };

    // -- 8. Guarantee the required meta elements (field fix 30.08) -
    // DIMSE-style sources (cltranscode output) carry NO group 0002.
    // pynetdicom C-STORE refuses files missing MediaStorageSOPClassUID /
    // MediaStorageSOPInstanceUID, killing the bridge C-MOVE (0xB000).
    // Both assembly paths (memcpy and rebuild) run through this post-pass.
    let mut output = output;
    // Palette conversion turns 1-sample PALETTE COLOR into 3-sample RGB;
    // both assembly paths copied (0028,0002) SamplesPerPixel verbatim, and
    // a 3-sample image needs (0028,0006) Planar Configuration (=0,
    // interleaved - the native encoders emit interleaved).
    if palette_applied {
        let out_implicit = target_ts == crate::codec::ts_uid::IMPLICIT_VR_LE;
        patch_u16_element(
            &mut output,
            (0x0028, 0x0002),
            pixels.samples_per_pixel as u16,
            target_is_be,
            out_implicit,
        );
        ensure_planar_configuration(
            &mut output,
            pixels.samples_per_pixel as u16,
            target_is_be,
            out_implicit,
        );
    }
    ensure_file_meta(&mut output, dicom_bytes, target_ts);

    log::info!("transcode: OK -> {}B", output.len());

    Ok(output)
}

// ===========================================================================
// BE byte-swap helpers
// ===========================================================================

/// VRs whose values need byte-swapping when converting LE->BE.
fn is_multi_byte_vr(vr: &str) -> bool {
    matches!(
        vr,
        "US" | "SS" | "OW" | "AT"   // 2-byte units
            | "UL" | "SL" | "OL" | "FL"  // 4-byte
            | "UV" | "SV" // 8-byte integers (PS3.5 Table 6.2-1)
            | "FD" | "OD" // 8-byte
    )
}

/// Byte-swap a DICOM value from LE to BE in-place.
fn swap_value_be(value: &[u8], vr: &str) -> Vec<u8> {
    let mut out = value.to_vec();
    match vr {
        "US" | "SS" | "OW" | "AT" => {
            for chunk in out.as_chunks_mut::<2>().0 {
                chunk.swap(0, 1);
            }
        }
        "UL" | "SL" | "OL" | "FL" => {
            for chunk in out.as_chunks_mut::<4>().0 {
                chunk.swap(0, 3);
                chunk.swap(1, 2);
            }
        }
        "FD" | "OD" | "UV" | "SV" => {
            for chunk in out.as_chunks_mut::<8>().0 {
                chunk.swap(0, 7);
                chunk.swap(1, 6);
                chunk.swap(2, 5);
                chunk.swap(3, 4);
            }
        }
        _ => {}
    }
    out
}

/// DICOM PS3.5 Section 7.1: pad byte after odd-length value.
/// 0x20 for character strings, 0x00 for everything else.
fn pad_byte_for_vr(vr: &str) -> u8 {
    if matches!(
        vr,
        "CS" | "LO"
            | "SH"
            | "PN"
            | "AE"
            | "AS"
            | "DA"
            | "DS"
            | "DT"
            | "IS"
            | "LT"
            | "ST"
            | "TM"
            | "UC"
            | "UR"
            | "UT"
    ) {
        0x20
    } else {
        0x00
    }
}

/// Copy an element from source to output in Implicit VR LE format.
/// Handles both explicit and implicit VR sources.
#[allow(clippy::too_many_arguments)]
fn copy_element_to_implicit(
    out: &mut Vec<u8>,
    src: &[u8],
    pos: &mut usize,
    _group: u16,
    _elem: u16,
    elem_start: usize,
    source_is_implicit: bool,
    source_is_be: bool,
) -> Result<(), CodecError> {
    out.extend_from_slice(&src[elem_start..elem_start + 4]); // copy tag

    // The source VR decides whether a Big Endian value has to be swapped.
    // An Implicit VR source carries no VR at the element, so the only honest
    // answer is "unknown" and no swap is attempted: guessing a width would
    // corrupt every string of the right length.
    let mut vr_str = "UN";
    let (val_len, val_start) = if source_is_implicit {
        if *pos + 4 > src.len() {
            out.extend_from_slice(&src[elem_start..]);
            return Err(CodecError::InvalidMetadata(
                "Truncated at implicit len".into(),
            ));
        }
        let len = read_u32_syntax(src, *pos, source_is_be) as usize;
        *pos += 4;
        (len, *pos)
    } else {
        if *pos + 2 > src.len() {
            out.extend_from_slice(&src[elem_start..]);
            return Err(CodecError::InvalidMetadata("Truncated at VR".into()));
        }
        let vr = std::str::from_utf8(&src[*pos..*pos + 2]).unwrap_or("UN");
        vr_str = vr;
        *pos += 2;
        let is_extended = matches!(
            vr,
            "OB" | "OD"
                | "OF"
                | "OL"
                | "OV"
                | "OW"
                | "SQ"
                | "SV"
                | "UC"
                | "UN"
                | "UR"
                | "UT"
                | "UV"
        );
        let len = if is_extended {
            if *pos + 6 > src.len() {
                out.extend_from_slice(&src[elem_start..]);
                return Err(CodecError::InvalidMetadata(
                    "Truncated at extended len".into(),
                ));
            }
            *pos += 2; // reserved
            let l = read_u32_syntax(src, *pos, source_is_be) as usize;
            *pos += 4;
            l
        } else {
            let l = read_u16_syntax(src, *pos, source_is_be) as usize;
            *pos += 2;
            l
        };
        (len, *pos)
    };

    if val_len == 0xFFFF_FFFF {
        // Undefined length - copy verbatim.
        //
        // `*pos` now points INTO the item stream, where group FFFE carries NO
        // VR (PS3.5 7.5). `skip_element` expects an element header and would
        // read the item's own length as a VR, so the walk that knows the item
        // and delimiter structure is the one to use - with the SOURCE's own
        // syntax, because that is how the attributes inside the items are
        // encoded, and it need not match the output's.
        let mut skip_pos = *pos;
        if !skip_undefined_element(src, &mut skip_pos, source_is_implicit, source_is_be) {
            out.extend_from_slice(&src[elem_start..]);
            return Err(CodecError::InvalidMetadata(
                "Cannot skip undefined element".into(),
            ));
        }
        out.extend_from_slice(&src[elem_start..skip_pos]);
        *pos = skip_pos;
        return Ok(());
    }

    if val_start + val_len > src.len() {
        out.extend_from_slice(&src[elem_start..]);
        return Err(CodecError::InvalidMetadata("Truncated value".into()));
    }

    // Write implicit VR: 4-byte length + value
    let len_u32 = val_len as u32;
    out.extend_from_slice(&len_u32.to_le_bytes());
    // PS3.5 7.1: the transfer syntax fixes the byte order of every binary VR,
    // so a Big Endian source reaching this LE output must be swapped here.
    // Copying the value verbatim left `Rows`, `BitsAllocated`, rescale slopes
    // and the pixel data themselves byte-reversed - a file that opens but
    // shows garbage, which is the worst kind of wrong.
    if source_is_be {
        let swapped = swap_value_be(&src[val_start..val_start + val_len], vr_str);
        out.extend_from_slice(&swapped);
    } else {
        out.extend_from_slice(&src[val_start..val_start + val_len]);
    }
    *pos = val_start + val_len;
    if val_len % 2 != 0 {
        *pos += 1;
    }
    Ok(())
}

// ===========================================================================
// Direct transcode - nginx-way: memcpy untouched, replace only changed
// ===========================================================================

/// Find the byte position of a DICOM tag (group, element) in `data`.
/// Returns the offset of the 4-byte tag, or None if not found.
fn find_dicom_tag(
    data: &[u8],
    target: (u16, u16),
    source_is_be: bool,
    is_implicit: bool,
) -> Option<usize> {
    let mut pos = if data.len() >= 132 && &data[128..132] == b"DICM" {
        132
    } else if data.len() >= 4 && &data[0..4] == b"DICM" {
        4
    } else {
        return None;
    };

    // Skip meta-header (always LE)
    while pos + 4 <= data.len() {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if g != 0x0002 {
            break;
        }
        pos += 4;
        if !skip_element(data, &mut pos, false, false) {
            return None;
        }
    }

    // Walk dataset
    while pos + 4 <= data.len() {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let e = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        if (g, e) == target {
            return Some(pos);
        }
        pos += 4;
        if !skip_element(data, &mut pos, is_implicit, source_is_be) {
            // Try undefined-length (sequence) skip
            if !skip_undefined_element(data, &mut pos, is_implicit, source_is_be) {
                return None;
            }
        }
    }
    None
}

/// Find the byte position where a DICOM element ends (past its value + pad).
/// Handles both defined-length and undefined-length (encapsulated) elements.
fn find_element_end(
    data: &[u8],
    tag_pos: usize,
    source_is_be: bool,
    is_implicit: bool,
) -> Result<usize, CodecError> {
    let mut pos = tag_pos + 4; // skip tag (4 bytes)

    if is_implicit {
        // Implicit VR LE: no VR, just 4-byte length
        if pos + 4 > data.len() {
            return Err(CodecError::InvalidMetadata("Truncated at length".into()));
        }
        let val_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if val_len == 0xFFFF_FFFF {
            return Err(CodecError::InvalidMetadata(
                "Undefined length in implicit VR".into(),
            ));
        }
        if pos + val_len > data.len() {
            return Err(CodecError::InvalidMetadata("Truncated value".into()));
        }
        pos += val_len;
        if !val_len.is_multiple_of(2) {
            pos += 1;
        }
        return Ok(pos);
    }

    // Explicit VR path
    if pos + 2 > data.len() {
        return Err(CodecError::InvalidMetadata("Truncated at VR".into()));
    }
    let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("UN");
    pos += 2;

    let is_extended = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT" | "UV"
    );

    let val_len: usize = if is_extended {
        pos += 2; // reserved
        let l = if source_is_be {
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize
        } else {
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize
        };
        pos += 4;
        l
    } else {
        let l = if source_is_be {
            u16::from_be_bytes([data[pos], data[pos + 1]]) as usize
        } else {
            u16::from_le_bytes([data[pos], data[pos + 1]]) as usize
        };
        pos += 2;
        l
    };

    if val_len == 0xFFFF_FFFF {
        // Undefined length - walk Items until SequenceDelimiter
        if !skip_undefined_element(data, &mut pos, false, source_is_be) {
            return Err(CodecError::InvalidMetadata(
                "Cannot skip undefined-length element".into(),
            ));
        }
    } else {
        if pos + val_len > data.len() {
            return Err(CodecError::InvalidMetadata("Truncated value".into()));
        }
        pos += val_len;
        if !val_len.is_multiple_of(2) {
            pos += 1; // pad byte
        }
    }

    Ok(pos)
}

/// Find meta-header end (first byte of dataset) by walking group 0002 elements.
fn find_metaheader_end(data: &[u8], start: usize) -> Result<usize, CodecError> {
    let mut pos = start;
    while pos + 4 <= data.len() {
        let g = read_u16_le(data, pos);
        if g != 0x0002 {
            return Ok(pos);
        }
        pos += 4;
        if !skip_element(data, &mut pos, false, false) {
            break;
        }
    }
    Ok(pos.min(data.len()))
}

/// 3008: the ONE canonical meta-group writer for every transcode path.
/// Emits exactly three elements in ASCENDING order - (0002,0002)
/// MediaStorageSOPClassUID, (0002,0003) MediaStorageSOPInstanceUID,
/// (0002,0010) TransferSyntaxUID LAST - each an Explicit VR LE UI with an
/// EVEN value length and the trailing NULL INCLUDED in the length
/// (PS3.5 sec 6.2). No external pad bytes in group 0002, ever; no
/// (0002,0000) group length (retired) and no (0002,0012)/(0002,0013)
/// (Type 3, dropped deliberately).
fn write_canonical_file_meta(
    out: &mut Vec<u8>,
    sop_class: Option<&[u8]>,
    sop_instance: Option<&[u8]>,
    ts: &[u8],
) {
    // Ascending order is guaranteed by construction. Unresolved UIDs are
    // OMITTED (degraded mode - same semantics as before 3008): a broken
    // source element is never copied as-is, and an empty UI is never
    // emitted.
    for (elem, value) in [
        (0x0002u16, sop_class),
        (0x0003u16, sop_instance),
        (0x0010u16, Some(ts)),
    ] {
        let Some(value) = value else {
            continue;
        };
        let padded_len = if value.len().is_multiple_of(2) {
            value.len()
        } else {
            value.len() + 1
        };
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(padded_len as u16).to_le_bytes());
        out.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            out.push(0x00); // NULL pad INCLUDED in the length
        }
    }
}

/// 3008 UID resolution for the canonical meta, in order: the SOURCE meta
/// group (byte-level, explicit VR LE - the reader-side `len % 2` tolerance
/// for old files STAYS), else the source dataset (0008,0016)/(0008,0018)
/// via [`extract_top_level_ui`]. A broken element is never copied as-is.
#[allow(clippy::too_many_arguments)]
fn resolve_uid(
    original: &[u8],
    data_start: usize,
    mh_end: usize,
    meta_elem: u16,
    dataset_group: u16,
    dataset_elem: u16,
    source_is_implicit: bool,
    source_is_be: bool,
) -> Option<String> {
    let mut p = data_start;
    while p + 8 <= mh_end && p + 8 <= original.len() {
        let g = read_u16_le(original, p);
        if g != 0x0002 {
            break;
        }
        let e = read_u16_le(original, p + 2);
        let vr = &original[p + 4..p + 6];
        let long_vr = matches!(
            vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        let (vo, len) = if long_vr {
            if p + 12 > original.len() {
                break;
            }
            let len = u32::from_le_bytes(original[p + 8..p + 12].try_into().ok()?) as usize;
            (p + 12, len)
        } else {
            let len = u16::from_le_bytes(original[p + 6..p + 8].try_into().ok()?) as usize;
            (p + 8, len)
        };
        if e == meta_elem && vo.saturating_add(len) <= original.len() {
            let raw = &original[vo..vo + len];
            let s = String::from_utf8_lossy(raw)
                .trim_end_matches(['\0', ' '])
                .to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
        if vo.saturating_add(len) > original.len() {
            break;
        }
        p = vo + len + (len % 2); // reader tolerance for old files STAYS
    }
    // Dataset scan starts at the META END (meta is always LE; the dataset
    // byte order is what source_is_be says).
    extract_top_level_ui(
        original,
        mh_end,
        dataset_group,
        dataset_elem,
        source_is_implicit,
        source_is_be,
    )
}

/// Replace a US (2-byte) tag value in-place. Assumes same-length value.
fn replace_us_in_place(
    data: &mut [u8],
    target: (u16, u16),
    value: u16,
    is_implicit: bool,
) -> Result<(), CodecError> {
    let tag_pos = match find_dicom_tag(data, target, false, is_implicit) {
        Some(p) => p,
        None => {
            log::warn!(
                "direct: tag ({:04X},{:04X}) not found — skipping",
                target.0,
                target.1
            );
            return Ok(());
        }
    };
    // Tag layout: [g(2) e(2) VR("US",2) len(2,=2) val(2)]
    let val_pos = tag_pos + 8;
    if val_pos + 2 > data.len() {
        return Err(CodecError::InvalidMetadata("Tag too close to end".into()));
    }
    data[val_pos..val_pos + 2].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Replace a CS (string) tag value in-place. New value must be <= old length.
fn replace_cs_in_place(
    data: &mut [u8],
    target: (u16, u16),
    value: &str,
    is_implicit: bool,
) -> Result<(), CodecError> {
    let tag_pos = match find_dicom_tag(data, target, false, is_implicit) {
        Some(p) => p,
        None => {
            log::warn!(
                "direct: tag ({:04X},{:04X}) not found — skipping",
                target.0,
                target.1
            );
            return Ok(());
        }
    };
    // Read existing length
    let (old_len, val_pos) = if is_implicit {
        // Implicit VR: tag(4) + length(4) + value
        let l = u32::from_le_bytes([
            data[tag_pos + 4],
            data[tag_pos + 5],
            data[tag_pos + 6],
            data[tag_pos + 7],
        ]) as usize;
        (l, tag_pos + 8)
    } else {
        let len_field = tag_pos + 6;
        let l = u16::from_le_bytes([data[len_field], data[len_field + 1]]) as usize;
        (l, tag_pos + 8)
    };
    let new_bytes = value.as_bytes();
    if new_bytes.len() > old_len {
        return Err(CodecError::InvalidMetadata(format!(
            "New CS value '{}' ({}B) longer than old ({}B)",
            value,
            new_bytes.len(),
            old_len
        )));
    }
    // Write value, space-pad remainder
    data[val_pos..val_pos + new_bytes.len()].copy_from_slice(new_bytes);
    for b in &mut data[val_pos + new_bytes.len()..val_pos + old_len] {
        *b = 0x20;
    }
    Ok(())
}

/// Direct DICOM transcode: replace only PixelData + 5 metadata elements.
/// 129 of 134 dataset elements are memcpy'd untouched.
///
/// nginx-way: don't rebuild the response. Replace one element, send body as-is.
/// Does NOT know about native vs encapsulated - codec owns PixelData format.
#[allow(clippy::too_many_arguments)]
fn transcode_direct(
    original: &[u8],
    new_pixeldata_element: &[u8], // complete (7FE0,0010) from codec
    target_ts: &str,
    photometric_interpretation: &str,
    bits_allocated: u8,
    bits_stored: u8,
    source_is_be: bool,
    is_implicit: bool,
    reduced_to_8bit: bool,
) -> Result<Vec<u8>, CodecError> {
    // -- 1. Find structure boundaries ------------------------
    let data_start = if original.len() >= 132 && &original[128..132] == b"DICM" {
        132
    } else if original.len() >= 4 && &original[0..4] == b"DICM" {
        4
    } else {
        return Err(CodecError::InvalidMetadata("Not a valid DICOM file".into()));
    };

    let mh_end = find_metaheader_end(original, data_start)?;

    let pd_tag_pos = find_dicom_tag(original, (0x7FE0, 0x0010), source_is_be, is_implicit)
        .ok_or_else(|| CodecError::InvalidMetadata("PixelData (7FE0,0010) not found".into()))?;
    if pd_tag_pos < mh_end {
        return Err(CodecError::InvalidMetadata(
            "PixelData found inside meta-header".into(),
        ));
    }

    let pd_end = find_element_end(original, pd_tag_pos, source_is_be, is_implicit)?;

    // -- 2. Calculate delta ---------------------------------
    let delta = new_pixeldata_element.len() as isize - (pd_end - pd_tag_pos) as isize;

    // -- 3. Assemble output ---------------------------------
    let new_size = (original.len() as isize + delta) as usize;
    let mut out = Vec::with_capacity(new_size);

    // Preamble + DICM
    out.extend_from_slice(&original[..data_start]);
    // -- Meta: the canonical trio (3008) - never copy source meta as-is --
    let source_ts = extract_transfer_syntax(original);
    let source_is_implicit = source_ts.as_deref() == Some(crate::codec::ts_uid::IMPLICIT_VR_LE);
    let source_is_be = source_ts.as_deref() == Some(crate::codec::ts_uid::EXPLICIT_VR_BE);
    let sop_class = resolve_uid(
        original,
        data_start,
        mh_end,
        0x0002,
        0x0008,
        0x0016,
        source_is_implicit,
        source_is_be,
    );
    if sop_class.is_none() {
        log::warn!("direct: MediaStorageSOPClassUID unresolved - meta element omitted");
    }
    let sop_instance = resolve_uid(
        original,
        data_start,
        mh_end,
        0x0003,
        0x0008,
        0x0018,
        source_is_implicit,
        source_is_be,
    );
    if sop_instance.is_none() {
        log::warn!("direct: MediaStorageSOPInstanceUID unresolved - meta element omitted");
    }
    write_canonical_file_meta(
        &mut out,
        sop_class.as_deref().map(str::as_bytes),
        sop_instance.as_deref().map(str::as_bytes),
        target_ts.as_bytes(),
    );
    // Dataset before PixelData - re-encoded when the VR encoding flips,
    // otherwise the untouched memcpy stays (byte-identical to today).
    let out_is_implicit = target_ts == crate::codec::ts_uid::IMPLICIT_VR_LE;
    let reencode_vr = is_implicit != out_is_implicit;
    if reencode_vr {
        copy_chunk_reencoded(&mut out, original, mh_end, pd_tag_pos, is_implicit)?;
    } else {
        out.extend_from_slice(&original[mh_end..pd_tag_pos]);
    }
    // New PixelData element (from codec - native OW or encapsulated OB).
    // The codec always emits the native element in EXPLICIT OW; an implicit
    // target needs it re-wrapped as tag + u32 length + value.
    let pixel_element = if out_is_implicit {
        rewrap_pixeldata_implicit(new_pixeldata_element)?
    } else {
        new_pixeldata_element.to_vec()
    };
    out.extend_from_slice(&pixel_element);
    // Dataset after old PixelData - untouched memcpy (shifted by delta).
    // Guard: a malformed source whose PixelData end exceeds the file must
    // error out, not panic the process (01.09: caught by the palette gate
    // fixture with a missing PS3.5 7.1 pad byte).
    if pd_end > original.len() {
        return Err(CodecError::InvalidMetadata(
            "PixelData element extends past end of dataset".into(),
        ));
    }
    if reencode_vr {
        copy_chunk_reencoded(&mut out, original, pd_end, original.len(), is_implicit)?;
    } else {
        out.extend_from_slice(&original[pd_end..]);
    }

    // -- 4. Replace changed tags in-place -------------------
    // All before PixelData -> positions unchanged by delta.
    // All are US (2B) or CS (<=old length) -> in-place safe.
    // The output encoding is the TARGET's, so the patches follow it (after
    // a VR re-encode the source encoding is no longer the one on disk).
    let high_bit = bits_allocated.saturating_sub(1);
    replace_us_in_place(
        &mut out,
        (0x0028, 0x0100),
        bits_allocated as u16,
        out_is_implicit,
    )?;
    replace_us_in_place(
        &mut out,
        (0x0028, 0x0101),
        bits_stored as u16,
        out_is_implicit,
    )?;
    replace_us_in_place(&mut out, (0x0028, 0x0102), high_bit as u16, out_is_implicit)?;
    replace_cs_in_place(
        &mut out,
        (0x0028, 0x0004),
        photometric_interpretation,
        out_is_implicit,
    )?;

    // -- 4.5. Window Center/Width after a bit-depth reduction --
    // The reduction BAKES the window into the pixels: `reduce_bit_depth_16to8`
    // maps [center - width/2, center + width/2] onto 0..255. Scaling WC/WW by
    // new_max/old_max afterwards described a window in the ORIGINAL value
    // units that the new samples no longer use, and a viewer that re-applied
    // it produced a near-binary image. The output is full-range 8-bit now, so
    // the window is either full range or absent.
    // The condition is the REDUCTION itself. Comparing the output's
    // BitsAllocated with the source's BitsStored was a proxy: a 16-bit source
    // whose samples carry only 8 meaningful bits compared unequal and had its
    // window rewritten although no sample was remapped.
    if reduced_to_8bit {
        write_identity_window_8bit(&mut out, out_is_implicit)?;
    }

    log::info!(
        "direct: {}B -> {}B (delta={})",
        original.len(),
        out.len(),
        delta
    );

    Ok(out)
}

/// Re-wrap the codec's native PixelData element (explicit OW: tag + VR +
/// reserved + u32 length) into implicit VR form (tag + u32 length + value)
/// for an IVRLE target. The value bytes stay verbatim; an odd value keeps
/// its pad byte after the value, outside the length, per PS3.5 7.1.
fn rewrap_pixeldata_implicit(element: &[u8]) -> Result<Vec<u8>, CodecError> {
    if element.len() < 12 || &element[4..6] != b"OW" {
        return Err(CodecError::InvalidMetadata(
            "Native PixelData element is not explicit OW".into(),
        ));
    }
    let val_len = u32::from_le_bytes([element[8], element[9], element[10], element[11]]) as usize;
    if 12 + val_len > element.len() {
        return Err(CodecError::InvalidMetadata(
            "PixelData element truncated".into(),
        ));
    }
    let mut out = Vec::with_capacity(8 + val_len + 1);
    out.extend_from_slice(&element[..4]); // tag (7FE0,0010)
    out.extend_from_slice(&(val_len as u32).to_le_bytes());
    out.extend_from_slice(&element[12..12 + val_len]);
    if !val_len.is_multiple_of(2) {
        out.push(0x00); // pad byte, outside the declared length
    }
    Ok(out)
}

/// Rewrite Window Center/Width to describe the reduced 8-bit output.
///
/// PS3.3 C.11.2.1.2 describes the window in the units of the samples that are
/// actually stored. After a 16->8 reduction those are 0..255, so the honest
/// window is center 127.5, width 256 (the full range the mapping produced).
fn write_identity_window_8bit(data: &mut [u8], implicit: bool) -> Result<(), CodecError> {
    for (tag, value) in [((0x0028, 0x1050), "127.5"), ((0x0028, 0x1051), "256")] {
        // Argument order is (data, target, source_is_be, is_implicit): the
        // window elements live in the DATASET, whose byte order and encoding
        // are what the caller passes in. Swapping the two booleans told the
        // walker the dataset was big-endian explicit VR, so on an Implicit VR
        // dataset it read a length where a VR should be and never found the
        // tag.
        let Some(tag_pos) = find_dicom_tag(data, tag, false, implicit) else {
            continue;
        };
        let len_field = if implicit { tag_pos + 4 } else { tag_pos + 6 };
        if len_field + 2 > data.len() {
            continue;
        }
        let old_len = if implicit {
            u32::from_le_bytes([
                data[len_field],
                data[len_field + 1],
                data[len_field + 2],
                data[len_field + 3],
            ]) as usize
        } else {
            u16::from_le_bytes([data[len_field], data[len_field + 1]]) as usize
        };
        let val_start = if implicit {
            len_field + 4
        } else {
            len_field + 2
        };
        if val_start + old_len > data.len() || value.len() > old_len {
            // The replacement must fit the space the source reserved; a
            // shorter value is padded rather than shrinking the element.
            continue;
        }
        let bytes = value.as_bytes();
        data[val_start..val_start + bytes.len()].copy_from_slice(bytes);
        for byte in &mut data[val_start + bytes.len()..val_start + old_len] {
            *byte = 0x20;
        }
    }
    Ok(())
}

/// Scale WindowCenter (0028,1050) and WindowWidth (0028,1051) by ratio.
#[allow(dead_code)]
fn scale_window_center_width(data: &mut [u8], ratio: f64) -> Result<(), CodecError> {
    for (tag, name) in [
        ((0x0028, 0x1050), "WindowCenter"),
        ((0x0028, 0x1051), "WindowWidth"),
    ] {
        let tag_pos = match find_dicom_tag(data, tag, false, false) {
            Some(p) => p,
            None => continue,
        };
        let len_field = tag_pos + 6;
        let old_len = u16::from_le_bytes([data[len_field], data[len_field + 1]]) as usize;
        let val_start = tag_pos + 8;
        let old_val = std::str::from_utf8(&data[val_start..val_start + old_len])
            .map_err(|_| CodecError::InvalidMetadata(format!("{name}: non-UTF8 value")))?;
        let old_val = old_val.trim_end_matches('\0').trim_end();

        let new_val: String = old_val
            .split('\\')
            .filter_map(|s| {
                let s = s.trim();
                if s.is_empty() {
                    return None;
                }
                if let Ok(v) = s.parse::<f64>() {
                    let scaled = v * ratio;
                    Some(
                        format!("{:.4}", scaled)
                            .trim_end_matches('0')
                            .trim_end_matches('.')
                            .to_string(),
                    )
                } else {
                    Some(s.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join("\\");

        if new_val.is_empty() {
            continue;
        }
        let new_bytes = new_val.as_bytes();
        if new_bytes.len() > old_len {
            log::warn!(
                "direct: {name} scaled too long ({}B > {}B) — skip",
                new_bytes.len(),
                old_len
            );
            continue;
        }
        let old_val_owned = old_val.to_string();
        let new_val_owned = new_val.clone();
        let (left, right) = data.split_at_mut(val_start + new_bytes.len());
        left[val_start..].copy_from_slice(new_bytes);
        let pad_len = old_len.saturating_sub(new_bytes.len());
        if pad_len > 0 && right.len() >= pad_len {
            for b in &mut right[..pad_len] {
                *b = 0x20;
            }
        }
        log::info!("direct: {name} {} -> {}", old_val_owned, new_val_owned);
    }
    Ok(())
}

// ===========================================================================
// update_dicom_metadata - element-by-element rebuild for cross-endianness
// targets. Called from the transcode pipeline when source and target byte-order
// differ (byte-swap requires a full walk, the memcpy direct path cannot swap).
// ===========================================================================

/// Scan the SOURCE dataset (after the meta-header) top-level for one UI
/// element (SOP Class UID / SOP Instance UID) and return its trimmed value.
/// Used to synthesize the meta-header when the source was written without
/// one (DIMSE-style datasets, cltranscode output) - pynetdicom C-STORE
/// refuses files missing MediaStorageSOPClassUID/MediaStorageSOPInstanceUID.
fn extract_top_level_ui(
    original: &[u8],
    data_start: usize,
    group: u16,
    element: u16,
    source_is_implicit: bool,
    source_is_be: bool,
) -> Option<String> {
    let mut p = data_start;
    let mut guard = 0u32;
    while p + 8 <= original.len() && guard < 50_000 {
        guard += 1;
        let (g, e) = if source_is_be {
            (
                u16::from_be_bytes(original[p..p + 2].try_into().ok()?),
                u16::from_be_bytes(original[p + 2..p + 4].try_into().ok()?),
            )
        } else {
            (read_u16_le(original, p), read_u16_le(original, p + 2))
        };
        if (g, e) == (group, element) {
            let (vstart, vlen) = if source_is_implicit {
                let len = u32::from_le_bytes(original[p + 4..p + 8].try_into().ok()?) as usize;
                (p + 8, len)
            } else {
                let vr = &original[p + 4..p + 6];
                let long_vr = matches!(
                    vr,
                    b"OB"
                        | b"OD"
                        | b"OF"
                        | b"OL"
                        | b"OV"
                        | b"OW"
                        | b"SQ"
                        | b"UC"
                        | b"UN"
                        | b"UR"
                        | b"UT"
                );
                if long_vr {
                    if p + 12 > original.len() {
                        return None;
                    }
                    let len = if source_is_be {
                        u32::from_be_bytes(original[p + 8..p + 12].try_into().ok()?) as usize
                    } else {
                        u32::from_le_bytes(original[p + 8..p + 12].try_into().ok()?) as usize
                    };
                    (p + 12, len)
                } else {
                    let len = if source_is_be {
                        u16::from_be_bytes(original[p + 6..p + 8].try_into().ok()?) as usize
                    } else {
                        u16::from_le_bytes(original[p + 6..p + 8].try_into().ok()?) as usize
                    };
                    (p + 8, len)
                }
            };
            if vstart.saturating_add(vlen) > original.len() {
                return None;
            }
            let raw = &original[vstart..vstart + vlen];
            let s = String::from_utf8_lossy(raw)
                .trim_end_matches(['\0', ' '])
                .to_string();
            if s.is_empty() {
                return None;
            }
            return Some(s);
        }
        // skip_element expects pos at the VR (tag already consumed).
        p += 4;
        if !skip_element(original, &mut p, source_is_implicit, source_is_be) {
            break;
        }
    }
    None
}

fn ensure_file_meta(output: &mut Vec<u8>, source: &[u8], target_ts: &str) {
    let data_start = if source.len() >= 132 && &source[128..132] == b"DICM" {
        132
    } else if source.len() >= 4 && &source[0..4] == b"DICM" {
        4
    } else {
        return;
    };
    // Walk the OUTPUT meta group (always Explicit VR LE, PS3.5 sec 7.1).
    // The reader-side `len % 2` tolerance for old files STAYS.
    let mut pos = data_start;
    let mut meta_end = pos;
    while pos + 8 <= output.len() {
        let g = read_u16_le(output, pos);
        if g != 0x0002 {
            break;
        }
        let vr = std::str::from_utf8(&output[pos + 4..pos + 6]).unwrap_or("");
        let long_vr = matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        let len: usize = if long_vr {
            if pos + 12 > output.len() {
                break;
            }
            u32::from_le_bytes(output[pos + 8..pos + 12].try_into().unwrap()) as usize
        } else {
            read_u16_le(output, pos + 6) as usize
        };
        let vo = if long_vr { pos + 12 } else { pos + 8 };
        if vo.saturating_add(len) > output.len() {
            break;
        }
        pos = vo + len + (len % 2);
        meta_end = pos;
    }
    // 3008: VALIDATE-OR-REWRITE. Resolve the UIDs from the SOURCE (meta group
    // first, dataset fallback), build the canonical trio, and compare it
    // byte-for-byte with the existing output meta group.
    let source_ts = extract_transfer_syntax(source);
    let source_is_implicit = source_ts.as_deref() == Some(crate::codec::ts_uid::IMPLICIT_VR_LE);
    let source_is_be = source_ts.as_deref() == Some(crate::codec::ts_uid::EXPLICIT_VR_BE);
    let mh_end = find_metaheader_end(source, data_start).unwrap_or(meta_end);
    let sop_class = resolve_uid(
        source,
        data_start,
        mh_end,
        0x0002,
        0x0008,
        0x0016,
        source_is_implicit,
        source_is_be,
    );
    if sop_class.is_none() {
        log::warn!("transcode: MediaStorageSOPClassUID unresolved - meta element omitted");
    }
    let sop_instance = resolve_uid(
        source,
        data_start,
        mh_end,
        0x0003,
        0x0008,
        0x0018,
        source_is_implicit,
        source_is_be,
    );
    if sop_instance.is_none() {
        log::warn!("transcode: MediaStorageSOPInstanceUID unresolved - meta element omitted");
    }
    let mut canonical: Vec<u8> = Vec::new();
    write_canonical_file_meta(
        &mut canonical,
        sop_class.as_deref().map(str::as_bytes),
        sop_instance.as_deref().map(str::as_bytes),
        target_ts.as_bytes(),
    );
    let existing = &output[data_start..meta_end];
    if existing == canonical {
        return; // already canonical - no-op, byte-identical
    }
    // REPLACE the whole meta group with the canonical trio (never splice-
    // append after non-ascending or padded elements).
    output.splice(data_start..meta_end, canonical);
}

fn update_dicom_metadata(
    original: &[u8],
    new_pixel_data: &[u8],
    pixeldata_element: Option<&[u8]>, // complete (7FE0,0010) from the codec
    target_ts: &str,
    photometric_interpretation: &str,
    bits_allocated: u8,
    bits_stored: u8,
) -> Result<Vec<u8>, CodecError> {
    let source_ts = extract_transfer_syntax(original);
    let source_is_implicit = source_ts.as_deref() == Some("1.2.840.10008.1.2");
    let source_is_be = source_ts.as_deref() == Some(crate::codec::ts_uid::EXPLICIT_VR_BE);
    let target_is_be = target_ts == crate::codec::ts_uid::EXPLICIT_VR_BE;
    let target_is_implicit = target_ts == crate::codec::ts_uid::IMPLICIT_VR_LE;

    // Tags we replace (don't copy from source)
    let is_replaced = |g: u16, e: u16| -> bool {
        matches!(
            (g, e),
            (0x0028, 0x0100) // BitsAllocated
                | (0x0028, 0x0101) // BitsStored
                | (0x0028, 0x0102) // HighBit
                | (0x0028, 0x0004) // PhotometricInterpretation
                // WindowCenter (0028,1050) and WindowWidth (0028,1051)
                // are NOT removed - Weasis needs them for rendering.
                // Without VOI LUT, CR/DX images appear as black rectangle.
                // After transcode (P14.1->EVRLE), VOI values are still valid.
                | (0x7FE0, 0x0010) // PixelData
        )
    };

    // Find data start
    let data_start = if original.len() >= 132 && &original[128..132] == b"DICM" {
        132
    } else if original.len() >= 4 && &original[0..4] == b"DICM" {
        4
    } else {
        return Err(CodecError::InvalidMetadata("Not a valid DICOM file".into()));
    };

    let mut output = Vec::with_capacity(original.len());
    // Preamble + DICM
    output.extend_from_slice(&original[..data_start]);

    // -- 1. Meta: the canonical trio (3008) -------------------------
    // The source meta group is NEVER copied as-is: (0002,0012)/(0002,0013)
    // are Type 3 (optional) and are dropped deliberately; (0002,0000) is
    // retired; odd-length UI values get the NULL INSIDE the length.
    let mh_end =
        find_metaheader_end(original, data_start).unwrap_or(original.len().min(data_start + 8));
    let sop_class = resolve_uid(
        original,
        data_start,
        mh_end,
        0x0002,
        0x0008,
        0x0016,
        source_is_implicit,
        source_is_be,
    );
    if sop_class.is_none() {
        log::warn!("transcode: MediaStorageSOPClassUID unresolved - meta element omitted");
    }
    let sop_instance = resolve_uid(
        original,
        data_start,
        mh_end,
        0x0003,
        0x0008,
        0x0018,
        source_is_implicit,
        source_is_be,
    );
    if sop_instance.is_none() {
        log::warn!("transcode: MediaStorageSOPInstanceUID unresolved - meta element omitted");
    }
    write_canonical_file_meta(
        &mut output,
        sop_class.as_deref().map(str::as_bytes),
        sop_instance.as_deref().map(str::as_bytes),
        target_ts.as_bytes(),
    );
    let mut pos = mh_end;

    // -- 1b. Guarantee required meta elements (DIMSE-style sources) -
    // Shared post-pass: validate-or-rewrite of the OUTPUT meta group.
    // Idempotent - transcode_dicom runs it again on the final output.
    ensure_file_meta(&mut output, original, target_ts);

    // -- 2. Walk source dataset, copy non-replaced elements ----------

    // The replacement elements (BitsAllocated, BitsStored, HighBit,
    // PhotometricInterpretation) belong in tag order: they are (0028,0004) and
    // (0028,010x), so they precede everything from (0028,0106) onward. They
    // used to be appended AFTER the whole dataset, i.e. after PixelData, which
    // leaves a non-ascending element stream that strict readers reject and that
    // a reader stopping at PixelData never sees at all.
    let mut replacements_written = false;

    // Scan dataset, collecting non-replaced elements
    while pos + 4 <= original.len() {
        let g = group_of(original, pos, source_is_be);
        let e = elem_of(original, pos, source_is_be);
        if !replacements_written && (u32::from(g) << 16 | u32::from(e)) > REPLACEMENT_BOUNDARY {
            write_replacement_elements(
                &mut output,
                target_is_implicit,
                target_is_be,
                bits_allocated,
                bits_stored,
                photometric_interpretation,
            );
            replacements_written = true;
        }

        if g == 0xFFFE {
            // Sequence/item delimiters - copy raw bytes
            let start = pos;
            if !skip_element(original, &mut pos, source_is_implicit, source_is_be) {
                output.extend_from_slice(&original[start..]);
                break;
            }
            output.extend_from_slice(&original[start..pos]);
            continue;
        }

        if is_replaced(g, e) {
            // Skip - will write replacement later
            pos += 4; // skip tag, pos now at VR
            if (g, e) == (0x7FE0, 0x0010) && is_compressed_ts(source_ts.as_deref().unwrap_or("")) {
                // Compressed pixel data: skip VR+length header, then walk items.
                // Encapsulated PixelData layout:
                //   VR(2) + reserved(2) + undefined_len(4) + [Items...] + SeqDelim
                // Implicit VR layout: undefined_len(4) + [Items...] + SeqDelim
                if source_is_implicit {
                    pos += 4; // skip 4-byte length (0xFFFF_FFFF)
                } else {
                    pos += 8; // skip VR(2) + reserved(2) + length(4)
                }
                let mut skip_pos = pos;
                if skip_undefined_element(original, &mut skip_pos, source_is_implicit, source_is_be)
                {
                    pos = skip_pos;
                } else {
                    break; // can't skip -- stop
                }
            } else if !skip_element(original, &mut pos, source_is_implicit, source_is_be) {
                break;
            }
            continue;
        }

        // Copy element from source to output
        let elem_start = pos;
        pos += 4; // skip tag

        if target_is_implicit {
            // Convert to Implicit VR for output
            if let Err(err) = copy_element_to_implicit(
                &mut output,
                original,
                &mut pos,
                g,
                e,
                elem_start,
                source_is_implicit,
                source_is_be,
            ) {
                log::warn!(
                    "transcode rebuild: skipping truncated elem for implicit target ({:04X},{:04X}): {}",
                    g,
                    e,
                    err
                );
                break;
            }
        } else if source_is_implicit {
            // Implicit VR -> convert to Explicit VR
            if let Err(err) = copy_implicit_element_to_explicit(
                &mut output,
                original,
                &mut pos,
                g,
                e,
                elem_start,
                source_is_be,
                target_is_be,
            ) {
                log::warn!(
                    "transcode rebuild: skipping truncated implicit elem ({:04X},{:04X}): {}",
                    g,
                    e,
                    err
                );
                break;
            }
        } else {
            // Explicit VR -> copy with optional BE byte-swap
            if let Err(err) = copy_explicit_element(
                &mut output,
                original,
                &mut pos,
                elem_start,
                source_is_be,
                target_is_be,
            ) {
                log::warn!(
                    "transcode rebuild: skipping truncated explicit elem ({:04X},{:04X}): {}",
                    g,
                    e,
                    err
                );
                break;
            }
        }
    }

    // -- 3. Write any replacements the dataset never reached ----------
    // A source containing only low tags (or no dataset at all) still needs
    // them; the boundary check above covers the ordinary case.
    if !replacements_written {
        write_replacement_elements(
            &mut output,
            target_is_implicit,
            target_is_be,
            bits_allocated,
            bits_stored,
            photometric_interpretation,
        );
    }

    // -- 4. Write PixelData (7FE0,0010) ------------------------------
    // When the codec supplied a complete element (compressed multi-frame
    // targets: per-frame fragments + Basic Offset Table are codec-owned),
    // splice it verbatim. Otherwise wrap the raw encoded bytes as before.
    if let Some(element) = pixeldata_element {
        output.extend_from_slice(element);
        return Ok(output);
    }

    if target_is_be {
        output.extend_from_slice(&0x7FE0u16.to_be_bytes());
        output.extend_from_slice(&0x0010u16.to_be_bytes());
    } else {
        output.extend_from_slice(&0x7FE0u16.to_le_bytes());
        output.extend_from_slice(&0x0010u16.to_le_bytes());
    }

    if is_compressed_ts(target_ts) {
        // Encapsulated PixelData - always explicit VR (OB + undefined length)
        output.extend_from_slice(b"OB");
        output.extend_from_slice(&[0x00, 0x00]);
        output.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // BOT (empty)
        output.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        output.extend_from_slice(&0u32.to_le_bytes());
        // Fragment
        let mut frag = new_pixel_data.to_vec();
        while !frag.len().is_multiple_of(4) {
            frag.push(0x00);
        }
        output.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        output.extend_from_slice(&(frag.len() as u32).to_le_bytes());
        output.extend_from_slice(&frag);
        // Sequence delimiter
        output.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0]);
        output.extend_from_slice(&0u32.to_le_bytes());
    } else if target_is_implicit {
        // Implicit VR: tag(4) + length(4) + value
        let len = new_pixel_data.len() as u32;
        output.extend_from_slice(&len.to_le_bytes());
        output.extend_from_slice(new_pixel_data);
    } else {
        // Explicit VR: tag(4) + VR(2) + reserved(2) + length(4) + value
        output.extend_from_slice(b"OW");
        output.extend_from_slice(&[0x00, 0x00]);
        let len = new_pixel_data.len() as u32;
        if target_is_be {
            output.extend_from_slice(&len.to_be_bytes());
        } else {
            output.extend_from_slice(&len.to_le_bytes());
        }
        output.extend_from_slice(new_pixel_data);
    }

    Ok(output)
}

// ===========================================================================
// Element copy helpers (P37: extracted from old update_dicom_metadata)
// ===========================================================================

/// Tag that follows every replacement element. (0028,0102) is the highest of
/// them, so anything above it may be written after the replacements.
const REPLACEMENT_BOUNDARY: u32 = 0x0028_0102;

/// Write the replacement elements in the target encoding.
///
/// Kept as one function because the same four elements must be emitted at the
/// right position in the stream (see `REPLACEMENT_BOUNDARY`), and duplicating
/// the header logic per call site is how the two encodings drift apart.
#[allow(clippy::too_many_arguments)]
fn write_replacement_elements(
    out: &mut Vec<u8>,
    target_is_implicit: bool,
    target_is_be: bool,
    bits_allocated: u8,
    bits_stored: u8,
    photometric_interpretation: &str,
) {
    let high_bit = bits_allocated.saturating_sub(1);

    // Helper: write a US element in the target VR format
    let write_us_elem = |out: &mut Vec<u8>, elem: u16, val: u16| {
        // The tag follows the target byte order, like every other element in
        // the dataset; only the meta group is unconditionally little endian.
        if target_is_be {
            out.extend_from_slice(&0x0028u16.to_be_bytes());
            out.extend_from_slice(&elem.to_be_bytes());
        } else {
            out.extend_from_slice(&0x0028u16.to_le_bytes());
            out.extend_from_slice(&elem.to_le_bytes());
        }
        if target_is_implicit {
            // Implicit VR: no VR bytes, 4-byte length
            out.extend_from_slice(&2u32.to_le_bytes());
        } else {
            out.extend_from_slice(b"US");
            if target_is_be {
                out.extend_from_slice(&2u16.to_be_bytes());
            } else {
                out.extend_from_slice(&2u16.to_le_bytes());
            }
        }
        if target_is_be {
            out.extend_from_slice(&val.to_be_bytes());
        } else {
            out.extend_from_slice(&val.to_le_bytes());
        }
    };

    write_us_elem(out, 0x0100, bits_allocated as u16);
    write_us_elem(out, 0x0101, bits_stored as u16);
    write_us_elem(out, 0x0102, high_bit as u16);

    // PhotometricInterpretation (0028,0004) CS
    // DICOM PS3.5 Section 7.1.1: character string VRs MUST have even-length values.
    // If the value is odd, append a SPACE (0x20) and report even length.
    // Some parsers (pydicom, MicroDICOM) fail on odd-length CS + pad byte.
    let pi_raw = photometric_interpretation.as_bytes();
    let pi_padded: Vec<u8> = if pi_raw.len().is_multiple_of(2) {
        pi_raw.to_vec()
    } else {
        let mut v = pi_raw.to_vec();
        v.push(0x20); // SPACE pad -- included IN the value, not after it
        v
    };
    if target_is_be {
        out.extend_from_slice(&0x0028u16.to_be_bytes());
        out.extend_from_slice(&0x0004u16.to_be_bytes());
    } else {
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0004u16.to_le_bytes());
    }
    if target_is_implicit {
        out.extend_from_slice(&(pi_padded.len() as u32).to_le_bytes());
    } else {
        out.extend_from_slice(b"CS");
        if target_is_be {
            out.extend_from_slice(&(pi_padded.len() as u16).to_be_bytes());
        } else {
            out.extend_from_slice(&(pi_padded.len() as u16).to_le_bytes());
        }
    }
    out.extend_from_slice(&pi_padded);
}

/// Copy an Implicit VR LE element from source to Explicit VR LE output.
#[allow(clippy::too_many_arguments)]
fn copy_implicit_element_to_explicit(
    out: &mut Vec<u8>,
    src: &[u8],
    pos: &mut usize,
    group: u16,
    elem: u16,
    elem_start: usize,
    source_is_be: bool,
    target_is_be: bool,
) -> Result<(), CodecError> {
    if *pos + 4 > src.len() {
        out.extend_from_slice(&src[elem_start..]);
        return Err(CodecError::InvalidMetadata(
            "Truncated implicit element".into(),
        ));
    }
    let val_len = u32_from_slice(&src[*pos..*pos + 4]) as usize;
    *pos += 4;

    if val_len == 0xFFFF_FFFF {
        // Undefined length - copy verbatim
        let mut skip_pos = *pos;
        if !skip_undefined_element(src, &mut skip_pos, true, source_is_be) {
            out.extend_from_slice(&src[elem_start..]);
            return Err(CodecError::InvalidMetadata(
                "Cannot skip undefined element".into(),
            ));
        }
        out.extend_from_slice(&src[elem_start..skip_pos]);
        *pos = skip_pos;
        return Ok(());
    }

    if *pos + val_len > src.len() {
        out.extend_from_slice(&src[elem_start..]);
        return Err(CodecError::InvalidMetadata(
            "Truncated implicit value".into(),
        ));
    }
    let value = &src[*pos..*pos + val_len];
    *pos += val_len;
    // Odd-length implicit values carry a pad byte; the next element starts
    // past it (the explicit copier already skips the pad for its sources).
    if !val_len.is_multiple_of(2) {
        *pos += 1;
    }

    let vr = crate::codec::vr_dict::vr_for_tag(group, elem);
    // A sequence's items are datasets too, and they are encoded the SOURCE
    // way. Copying the value verbatim left implicit items inside an explicit
    // output (and the mirror case): the same hybrid encoding the chunk
    // re-encoder exists to prevent, one level down.
    if vr == "SQ"
        && let Some(rewritten) = reencode_sequence_items(value, true, target_is_be)
    {
        write_element_explicit(
            out,
            group,
            elem,
            vr,
            &rewritten,
            target_is_be,
            rewritten.len(),
        );
        return Ok(());
    }
    write_element_explicit(out, group, elem, vr, value, target_is_be, val_len);
    Ok(())
}

/// Re-encode a sequence value's items into the target VR encoding and byte
/// order.
///
/// Returns `None` when the value is not a plain, fully understood item stream.
/// A partially rewritten sequence is worse than an unrewritten one, so the
/// caller then copies the value verbatim.
fn reencode_sequence_items(
    items: &[u8],
    source_is_implicit: bool,
    target_is_be: bool,
) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(items.len());
    let mut pos = 0usize;
    while pos + 8 <= items.len() {
        let group = u16::from_le_bytes([items[pos], items[pos + 1]]);
        let element = u16::from_le_bytes([items[pos + 2], items[pos + 3]]);
        let raw_len = u32::from_le_bytes([
            items[pos + 4],
            items[pos + 5],
            items[pos + 6],
            items[pos + 7],
        ]);
        if group == 0xFFFE && element == 0xE0DD {
            out.extend_from_slice(&items[pos..pos + 8]);
            return Some(out);
        }
        if group != 0xFFFE || element != 0xE000 {
            return None;
        }
        if raw_len == 0xFFFF_FFFF {
            // Undefined-length item: its body runs to its ItemDelimitationItem.
            let body_start = pos + 8;
            let body_end = find_item_end(&items[body_start..])?;
            let rewritten = reencode_item_body(
                &items[body_start..body_start + body_end],
                source_is_implicit,
            )?;
            out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0xFF, 0xFF, 0xFF, 0xFF]);
            out.extend_from_slice(&rewritten);
            out.extend_from_slice(&[0xFE, 0xFF, 0x0D, 0xE0, 0, 0, 0, 0]);
            pos = body_start + body_end + 8;
            continue;
        }
        let len = raw_len as usize;
        if pos + 8 + len > items.len() {
            return None;
        }
        let rewritten = reencode_item_body(&items[pos + 8..pos + 8 + len], source_is_implicit)?;
        out.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        // Item headers are part of the element stream, so their length field
        // follows the target byte order like every other length.
        if target_is_be {
            out.extend_from_slice(&(rewritten.len() as u32).to_be_bytes());
        } else {
            out.extend_from_slice(&(rewritten.len() as u32).to_le_bytes());
        }
        out.extend_from_slice(&rewritten);
        pos += 8 + len;
        if !len.is_multiple_of(2) {
            pos += 1;
        }
    }
    Some(out)
}

/// Offset of the ItemDelimitationItem that closes the current item body.
fn find_item_end(data: &[u8]) -> Option<usize> {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let element = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        if group == 0xFFFE && element == 0xE00D {
            return Some(pos);
        }
        pos += 8;
    }
    None
}

/// Re-encode one item body (a dataset) into explicit VR LE.
///
/// Nested sequences recurse through the same implicit copier, so every level
/// ends up in the target encoding rather than only the outermost.
fn reencode_item_body(body: &[u8], source_is_implicit: bool) -> Option<Vec<u8>> {
    if !source_is_implicit {
        // Explicit item bodies are already the target shape.
        return Some(body.to_vec());
    }
    let mut out: Vec<u8> = Vec::with_capacity(body.len());
    let mut pos = 0usize;
    while pos + 4 <= body.len() {
        let elem_start = pos;
        let group = u16::from_le_bytes([body[pos], body[pos + 1]]);
        let element = u16::from_le_bytes([body[pos + 2], body[pos + 3]]);
        pos += 4;
        if group == 0xFFFE {
            if !skip_element(body, &mut pos, true, false) {
                return None;
            }
            out.extend_from_slice(&body[elem_start..pos]);
            continue;
        }
        copy_implicit_element_to_explicit(
            &mut out, body, &mut pos, group, element, elem_start, false, false,
        )
        .ok()?;
    }
    Some(out)
}

/// Copy an Explicit VR element from source to output, with optional BE byte-swap.
fn copy_explicit_element(
    out: &mut Vec<u8>,
    src: &[u8],
    pos: &mut usize,
    elem_start: usize,
    source_is_be: bool,
    target_is_be: bool,
) -> Result<(), CodecError> {
    if *pos + 2 > src.len() {
        out.extend_from_slice(&src[elem_start..]);
        return Err(CodecError::InvalidMetadata("Truncated at VR".into()));
    }
    let vr = std::str::from_utf8(&src[*pos..*pos + 2]).unwrap_or("UN");
    *pos += 2;

    let is_extended = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT" | "UV"
    );

    let (val_len, len_bytes): (usize, usize) = if is_extended {
        if *pos + 6 > src.len() {
            out.extend_from_slice(&src[elem_start..]);
            return Err(CodecError::InvalidMetadata(
                "Truncated at extended len".into(),
            ));
        }
        *pos += 2; // reserved
        let l = read_u32_syntax(src, *pos, source_is_be) as usize;
        *pos += 4;
        (l, 6)
    } else {
        let l = read_u16_syntax(src, *pos, source_is_be) as usize;
        *pos += 2;
        (l, 2)
    };

    if val_len == 0xFFFF_FFFF {
        // Undefined length - copy verbatim.
        //
        // Same reason as `copy_element_to_implicit`: the cursor is inside the
        // item stream, where there is no VR to read. This function only ever
        // sees an explicit-VR source - it read a VR itself, above - so the
        // items it has to walk are explicit too.
        let mut skip_pos = *pos;
        if !skip_undefined_element(src, &mut skip_pos, false, source_is_be) {
            out.extend_from_slice(&src[elem_start..]);
            return Err(CodecError::InvalidMetadata(
                "Cannot skip undefined element".into(),
            ));
        }
        out.extend_from_slice(&src[elem_start..skip_pos]);
        *pos = skip_pos;
        return Ok(());
    }

    let val_start = elem_start + 4 + 2 + len_bytes;
    if val_start + val_len > src.len() {
        out.extend_from_slice(&src[elem_start..]);
        return Err(CodecError::InvalidMetadata(
            "Truncated explicit value".into(),
        ));
    }
    let value_bytes = &src[val_start..val_start + val_len];
    // Advance pos past value + pad
    *pos = val_start + val_len;
    if val_len % 2 != 0 {
        *pos += 1;
    }

    // Normalise multi-byte VR values from source endianness to LE before
    // passing to write_element_explicit, which handles target endianness.
    let value = if source_is_be && is_multi_byte_vr(vr) {
        swap_value_be(value_bytes, vr)
    } else {
        value_bytes.to_vec()
    };

    write_element_explicit(
        out,
        group_of(src, elem_start, source_is_be),
        elem_of(src, elem_start, source_is_be),
        vr,
        &value,
        target_is_be,
        val_len,
    );
    Ok(())
}

fn group_of(src: &[u8], pos: usize, be: bool) -> u16 {
    if be {
        u16::from_be_bytes([src[pos], src[pos + 1]])
    } else {
        u16::from_le_bytes([src[pos], src[pos + 1]])
    }
}
fn elem_of(src: &[u8], pos: usize, be: bool) -> u16 {
    if be {
        u16::from_be_bytes([src[pos + 2], src[pos + 3]])
    } else {
        u16::from_le_bytes([src[pos + 2], src[pos + 3]])
    }
}

/// Write a single DICOM element in Explicit VR LE (or BE for lengths/values if target_is_be).
fn write_element_explicit(
    out: &mut Vec<u8>,
    group: u16,
    elem: u16,
    vr: &str,
    value: &[u8],
    target_is_be: bool,
    val_len: usize,
) {
    // The tag follows the target byte order along with the length fields: an
    // Explicit VR Big Endian dataset reverses the whole element, so leaving the
    // tag little endian produced a file whose meta-header says BE and whose
    // elements no BE reader can address.
    if target_is_be {
        out.extend_from_slice(&group.to_be_bytes());
        out.extend_from_slice(&elem.to_be_bytes());
    } else {
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
    }
    out.extend_from_slice(vr.as_bytes());

    let is_extended = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT" | "UV"
    );

    if is_extended {
        out.extend_from_slice(&[0x00, 0x00]); // reserved
        if target_is_be {
            out.extend_from_slice(&(val_len as u32).to_be_bytes());
        } else {
            out.extend_from_slice(&(val_len as u32).to_le_bytes());
        }
    } else {
        if target_is_be {
            out.extend_from_slice(&(val_len as u16).to_be_bytes());
        } else {
            out.extend_from_slice(&(val_len as u16).to_le_bytes());
        }
    }

    if target_is_be && is_multi_byte_vr(vr) {
        let swapped = swap_value_be(value, vr);
        out.extend_from_slice(&swapped);
    } else {
        out.extend_from_slice(value);
    }

    if !val_len.is_multiple_of(2) {
        out.push(pad_byte_for_vr(vr));
    }
}

use crate::codec::skip_value;

/// Copy a dataset chunk `[start, end)` from `original` to `out`, re-encoding
/// element headers when the transfer syntax flips between implicit and
/// explicit VR. The direct path's verbatim memcpy is only legal when the
/// source and target encodings match; otherwise the untouched elements would
/// keep their old headers and contradict the meta-header's transfer syntax
/// (live field bug 17.09: a strict reader fails on the first dataset
/// element of such a hybrid).
///
/// `end` is an element boundary: the chunk always ends where an element
/// starts (PixelData's own element is spliced in by the caller).
fn copy_chunk_reencoded(
    out: &mut Vec<u8>,
    original: &[u8],
    start: usize,
    end: usize,
    source_is_implicit: bool,
) -> Result<(), CodecError> {
    let mut pos = start;
    while pos + 4 <= end {
        let g = read_u16_le(original, pos);
        let e = read_u16_le(original, pos + 2);
        let elem_start = pos;
        pos += 4; // past the tag
        if g == 0xFFFE {
            // Sequence/item delimiters are raw bytes in both encodings.
            let chunk_start = pos;
            if !skip_element(original, &mut pos, source_is_implicit, false) {
                out.extend_from_slice(&original[elem_start..]);
                return Err(CodecError::InvalidMetadata(
                    "Cannot skip delimiter element".into(),
                ));
            }
            out.extend_from_slice(&original[elem_start..pos]);
            debug_assert!(pos >= chunk_start);
            continue;
        }
        if source_is_implicit {
            copy_implicit_element_to_explicit(
                out, original, &mut pos, g, e, elem_start, false, false,
            )?;
        } else {
            copy_element_to_implicit(out, original, &mut pos, g, e, elem_start, false, false)?;
        }
        if pos <= elem_start {
            return Err(CodecError::InvalidMetadata(
                "Element walk did not advance".into(),
            ));
        }
    }
    if pos > end {
        return Err(CodecError::InvalidMetadata(
            "Element crosses chunk boundary".into(),
        ));
    }
    Ok(())
}

/// Skip over a DICOM element. Handles both Explicit VR (tag + VR + length + value)
/// and Implicit VR (tag + length + value). On success, `pos` points past the element.
/// Returns false if the data is truncated.
fn skip_element(data: &[u8], pos: &mut usize, implicit: bool, source_is_be: bool) -> bool {
    if *pos + 4 > data.len() {
        return false;
    }
    // pos points to the element tag (group + elem, 4 bytes already consumed by caller).
    // For explicit VR: VR(2) + length(2 or 6) + value
    // For implicit VR: length(4) + value

    if implicit {
        // Implicit VR: 4-byte length follows the tag
        if *pos + 4 > data.len() {
            return false;
        }
        let len = read_u32_syntax(data, *pos, source_is_be) as usize;
        *pos += 4;
        if len == 0xFFFF_FFFF {
            // Undefined length - can't reliably skip, just advance past it
            // For PixelData we handle it specially above
            return false;
        }
        if *pos + len > data.len() {
            return false;
        }
        skip_value(pos, len);
    } else {
        // Explicit VR
        if *pos + 2 > data.len() {
            return false;
        }
        let vr = std::str::from_utf8(&data[*pos..*pos + 2]).unwrap_or("");
        *pos += 2;
        let extended = matches!(
            vr,
            "OB" | "OD"
                | "OF"
                | "OL"
                | "OV"
                | "OW"
                | "SQ"
                | "SV"
                | "UC"
                | "UN"
                | "UR"
                | "UT"
                | "UV"
        );
        let len: usize = if extended {
            if *pos + 6 > data.len() {
                return false;
            }
            *pos += 2; // reserved
            let l = read_u32_syntax(data, *pos, source_is_be) as usize;
            *pos += 4;
            l
        } else {
            let l = read_u16_syntax(data, *pos, source_is_be) as usize;
            *pos += 2;
            l
        };
        if len == 0xFFFF_FFFF {
            return false; // undefined length
        }
        if *pos + len > data.len() {
            return false;
        }
        skip_value(pos, len);
    }
    true
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Extract Transfer Syntax UID from DICOM meta-header.
pub fn extract_transfer_syntax(data: &[u8]) -> Option<String> {
    // Find DICM magic
    let mut pos = if data.len() >= 132 && &data[128..132] == b"DICM" {
        132
    } else if data.len() >= 4 && &data[0..4] == b"DICM" {
        4
    } else {
        return None;
    };

    // Walk meta-header (group 0002), always Explicit VR LE
    while pos + 4 <= data.len() {
        let group = read_u16_le(data, pos);
        let elem = read_u16_le(data, pos + 2);
        pos += 4;

        if group != 0x0002 {
            break;
        }

        // Read VR
        if pos + 2 > data.len() {
            return None;
        }
        let vr = std::str::from_utf8(&data[pos..pos + 2]).ok()?;
        pos += 2;

        let extended = matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );

        let len: usize = if extended {
            if pos + 6 > data.len() {
                return None;
            }
            pos += 2; // reserved
            let l = u32_from_slice(&data[pos..pos + 4]) as usize;
            pos += 4;
            l
        } else {
            let l = read_u16_le(data, pos) as usize;
            pos += 2;
            l
        };

        if pos + len > data.len() {
            return None;
        }

        if (group, elem) == (0x0002, 0x0010) {
            let val = &data[pos..pos + len];
            let s = String::from_utf8_lossy(val);
            let t = s.trim_end_matches('\0');
            return Some(t.to_string());
        }

        pos += len;
    }

    None
}

/// Skip over an element with undefined length (0xFFFFFFFF).
///
/// Handles the sequence/item/delimiter structure for Implicit VR encoding.
/// On success, `pos` points past the SequenceDelimitationItem (FFFE,E0DD).
/// Returns false if the data is truncated.
fn skip_undefined_element(
    data: &[u8],
    pos: &mut usize,
    implicit: bool,
    source_is_be: bool,
) -> bool {
    let mut nesting: u32 = 0;
    loop {
        if *pos + 8 > data.len() {
            return false;
        }
        let tag_group = read_u16_syntax(data, *pos, source_is_be);
        let tag_elem = read_u16_syntax(data, *pos + 2, source_is_be);
        *pos += 4;
        match (tag_group, tag_elem) {
            (0xFFFE, 0xE000) => {
                // Item start - length is always 4 bytes (even in Implicit VR)
                let item_len = read_u32_syntax(data, *pos, source_is_be) as usize;
                *pos += 4;
                if item_len == 0xFFFF_FFFF {
                    nesting += 1;
                } else if *pos + item_len > data.len() {
                    return false;
                } else {
                    *pos += item_len;
                }
            }
            (0xFFFE, 0xE00D) => {
                // ItemDelimitationItem - end of an item, length is 0x00000000
                *pos += 4;
                nesting = nesting.saturating_sub(1);
            }
            (0xFFFE, 0xE0DD) => {
                // SequenceDelimitationItem - end of the undefined-length element
                *pos += 4;
                return true;
            }
            _ => {
                // Regular attribute inside an item - skip its value
                if implicit {
                    if *pos + 4 > data.len() {
                        return false;
                    }
                    let attr_len = read_u32_syntax(data, *pos, source_is_be) as usize;
                    *pos += 4;
                    if attr_len == 0xFFFF_FFFF {
                        nesting += 1;
                    } else if *pos + attr_len > data.len() {
                        return false;
                    } else {
                        *pos += attr_len;
                    }
                } else {
                    // Explicit VR inside item
                    if *pos + 2 > data.len() {
                        return false;
                    }
                    let vr = std::str::from_utf8(&data[*pos..*pos + 2]).unwrap_or("");
                    *pos += 2;
                    let extended = matches!(
                        vr,
                        "OB" | "OD"
                            | "OF"
                            | "OL"
                            | "OV"
                            | "OW"
                            | "SQ"
                            | "SV"
                            | "UC"
                            | "UN"
                            | "UR"
                            | "UT"
                            | "UV"
                    );
                    let attr_len: usize = if extended {
                        if *pos + 6 > data.len() {
                            return false;
                        }
                        *pos += 2;
                        let l = read_u32_syntax(data, *pos, source_is_be) as usize;
                        *pos += 4;
                        l
                    } else {
                        let l = read_u16_syntax(data, *pos, source_is_be) as usize;
                        *pos += 2;
                        l
                    };
                    if attr_len == 0xFFFF_FFFF {
                        nesting += 1;
                    } else if *pos + attr_len > data.len() {
                        return false;
                    } else {
                        *pos += attr_len;
                    }
                }
            }
        }
    }
}

fn photometric_for_ts(ts: &str, current_pi: &str) -> String {
    use crate::codec::ts_uid;

    match ts {
        ts_uid::JPEG_BASELINE => {
            // JPEG Baseline in DICOM typically uses YBR_FULL_422
            // If input is RGB, keep as RGB for simplicity in encoding
            if current_pi == "MONOCHROME1" || current_pi == "MONOCHROME2" {
                current_pi.to_string()
            } else {
                "RGB".to_string()
            }
        }
        ts_uid::RLE_LOSSLESS => {
            // RLE preserves the original photometric interpretation
            current_pi.to_string()
        }
        ts_uid::EXPLICIT_VR_LE => {
            // Explicit VR LE preserves the original photometric interpretation
            current_pi.to_string()
        }
        ts_uid::JPEG2000_LOSSLESS | ts_uid::JPEG2000_LOSSY => {
            // JPEG2000: monochrome stays monochrome, color -> RGB
            // openjpeg handles YCC<->RGB automatically
            if current_pi == "MONOCHROME1" || current_pi == "MONOCHROME2" {
                current_pi.to_string()
            } else {
                "RGB".to_string()
            }
        }
        ts_uid::JPEGLS_LOSSLESS | ts_uid::JPEGLS_NEAR_LOSSLESS => {
            // JPEG-LS (E2 fix 31.08): the encoder writes RGB components.
            // A YBR* tag on an RGB stream is a corrupt file, so color ->
            // RGB. Monochrome stays monochrome.
            if current_pi == "MONOCHROME1" || current_pi == "MONOCHROME2" {
                current_pi.to_string()
            } else {
                "RGB".to_string()
            }
        }
        _ => current_pi.to_string(),
    }
}

/// NumberOfFrames (0028,0008) IS, default 1.
///
/// Read from the source dataset respecting its transfer syntax
/// (Explicit/Implicit VR, endianness). Malformed/absent values -> 1.
fn number_of_frames(data: &[u8], source_ts: &str) -> Result<u32, CodecError> {
    let is_implicit = source_ts == crate::codec::ts_uid::IMPLICIT_VR_LE;
    let is_be = source_ts == crate::codec::ts_uid::EXPLICIT_VR_BE;
    let Some(tag_pos) = find_dicom_tag(data, (0x0028, 0x0008), is_be, is_implicit) else {
        return Ok(1);
    };
    let mut pos = tag_pos + 4;
    if is_implicit {
        if pos + 4 > data.len() {
            return Ok(1);
        }
        let len = read_u32_syntax(data, pos, is_be) as usize;
        pos += 4;
        if pos + len > data.len() {
            return Ok(1);
        }
        return Ok(parse_is_u32(&data[pos..pos + len]));
    }
    if pos + 2 > data.len() {
        return Ok(1);
    }
    let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("");
    pos += 2;
    let len: usize = if matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT" | "UV"
    ) {
        if pos + 6 > data.len() {
            return Ok(1);
        }
        pos += 2; // reserved
        let l = read_u32_syntax(data, pos, is_be) as usize;
        pos += 4;
        l
    } else {
        let l = read_u16_syntax(data, pos, is_be) as usize;
        pos += 2;
        l
    };
    if pos + len > data.len() {
        return Ok(1);
    }
    Ok(parse_is_u32(&data[pos..pos + len]))
}

/// Parse an IS value (ASCII integer, possibly '\0'/space padded).
fn parse_is_u32(raw: &[u8]) -> u32 {
    String::from_utf8_lossy(raw)
        .trim()
        .trim_end_matches('\0')
        .parse::<u32>()
        .unwrap_or(1)
}

// ===========================================================================
// PALETTE COLOR support (01.09, owner: color in the miniviewer demo)
// ===========================================================================

/// Return the VALUE bytes of the element at `tag_pos` (position of the tag
/// itself). Undefined-length values return None. Zero-allocation slice view.
fn element_value_bytes(
    data: &[u8],
    tag_pos: usize,
    source_is_be: bool,
    is_implicit: bool,
) -> Option<&[u8]> {
    let mut pos = tag_pos + 4;
    if is_implicit {
        if pos + 4 > data.len() {
            return None;
        }
        let len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if len == 0xFFFF_FFFF || pos + len > data.len() {
            return None;
        }
        return Some(&data[pos..pos + len]);
    }
    if pos + 2 > data.len() {
        return None;
    }
    let vr = std::str::from_utf8(&data[pos..pos + 2]).unwrap_or("");
    pos += 2;
    let extended = matches!(
        vr,
        "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "SV" | "UC" | "UN" | "UR" | "UT" | "UV"
    );
    let len = if extended {
        if pos + 6 > data.len() {
            return None;
        }
        pos += 2; // reserved
        let l = read_u32_syntax(data, pos, source_is_be) as usize;
        pos += 4;
        l
    } else {
        let l = read_u16_syntax(data, pos, source_is_be) as usize;
        pos += 2;
        l
    };
    if len == 0xFFFF_FFFF || pos + len > data.len() {
        return None;
    }
    Some(&data[pos..pos + len])
}

/// Compare a CS value to `want` WITHOUT allocating (keeps the no-op
/// passthrough path at its 2-allocation ap101 gate).
fn cs_value_equals(
    data: &[u8],
    tag: (u16, u16),
    want: &str,
    source_is_be: bool,
    is_implicit: bool,
) -> bool {
    let Some(pos) = find_dicom_tag(data, tag, source_is_be, is_implicit) else {
        return false;
    };
    let Some(v) = element_value_bytes(data, pos, source_is_be, is_implicit) else {
        return false;
    };
    let mut end = v.len();
    while end > 0 && (v[end - 1] == b' ' || v[end - 1] == 0) {
        end -= 1;
    }
    &v[..end] == want.as_bytes()
}

const TAG_PALETTE_DESCRIPTOR: (u16, u16) = (0x0028, 0x1101); // Red descriptor
const TAG_PALETTE_LUTS: [(u16, u16); 3] = [
    (0x0028, 0x1201), // Red Palette Color Lookup Table Data
    (0x0028, 0x1202), // Green
    (0x0028, 0x1203), // Blue
];

/// Rewrite a 2-byte (u16) element VALUE in place. No-op when the tag is
/// absent or the value is shorter than 2 bytes. Used for the post-transcode
/// SamplesPerPixel patch after palette conversion.
fn patch_u16_element(out: &mut [u8], tag: (u16, u16), value: u16, is_be: bool, is_implicit: bool) {
    let Some(pos) = find_dicom_tag(out, tag, is_be, is_implicit) else {
        return;
    };
    let mut p = pos + 4;
    if is_implicit {
        if p + 4 > out.len() {
            return;
        }
        let len = u32::from_le_bytes([out[p], out[p + 1], out[p + 2], out[p + 3]]) as usize;
        if len < 2 {
            return;
        }
        p += 4;
    } else {
        if p + 2 > out.len() {
            return;
        }
        let vr = std::str::from_utf8(&out[p..p + 2]).unwrap_or("");
        p += 2;
        let extended = matches!(
            vr,
            "OB" | "OD"
                | "OF"
                | "OL"
                | "OV"
                | "OW"
                | "SQ"
                | "SV"
                | "UC"
                | "UN"
                | "UR"
                | "UT"
                | "UV"
        );
        if extended {
            if p + 6 > out.len() {
                return;
            }
            p += 2; // reserved
            let len = if is_be {
                u32::from_be_bytes([out[p], out[p + 1], out[p + 2], out[p + 3]]) as usize
            } else {
                u32::from_le_bytes([out[p], out[p + 1], out[p + 2], out[p + 3]]) as usize
            };
            p += 4;
            if len < 2 {
                return;
            }
        } else {
            let len = if is_be {
                u16::from_be_bytes([out[p], out[p + 1]]) as usize
            } else {
                u16::from_le_bytes([out[p], out[p + 1]]) as usize
            };
            p += 2;
            if len < 2 {
                return;
            }
        }
    }
    let bytes = if is_be {
        value.to_be_bytes()
    } else {
        value.to_le_bytes()
    };
    out[p..p + 2].copy_from_slice(&bytes);
}

/// Ensure (0028,0006) Planar Configuration exists with value 0 for
/// multi-sample images: patch when present, insert after SamplesPerPixel
/// when absent (pydicom/dcm4che require the tag for spp > 1).
fn ensure_planar_configuration(out: &mut Vec<u8>, spp: u16, is_be: bool, is_implicit: bool) {
    if spp <= 1 {
        return;
    }
    if find_dicom_tag(out, (0x0028, 0x0006), is_be, is_implicit).is_some() {
        patch_u16_element(out, (0x0028, 0x0006), 0, is_be, is_implicit);
        return;
    }
    let Some(spp_pos) = find_dicom_tag(out, (0x0028, 0x0002), is_be, is_implicit) else {
        return;
    };
    let Ok(insert_at) = find_element_end(out, spp_pos, is_be, is_implicit) else {
        return;
    };
    let mut elem = Vec::with_capacity(12);
    if is_be {
        elem.extend_from_slice(&0x0028u16.to_be_bytes());
        elem.extend_from_slice(&0x0006u16.to_be_bytes());
    } else {
        elem.extend_from_slice(&0x0028u16.to_le_bytes());
        elem.extend_from_slice(&0x0006u16.to_le_bytes());
    }
    if is_implicit {
        elem.extend_from_slice(&2u32.to_le_bytes());
    } else {
        elem.extend_from_slice(b"US");
        elem.extend_from_slice(&2u16.to_le_bytes());
    }
    if is_be {
        elem.extend_from_slice(&0u16.to_be_bytes());
    } else {
        elem.extend_from_slice(&0u16.to_le_bytes());
    }
    out.splice(insert_at..insert_at, elem);
}

/// Read one palette LUT data element (0028,1201-1203) as u16 entries.
fn read_palette_lut(
    data: &[u8],
    tag: (u16, u16),
    source_is_be: bool,
    is_implicit: bool,
) -> Result<Vec<u16>, CodecError> {
    let pos = find_dicom_tag(data, tag, source_is_be, is_implicit).ok_or_else(|| {
        CodecError::InvalidMetadata(format!(
            "PALETTE COLOR: missing LUT tag {:04X},{:04X}",
            tag.0, tag.1
        ))
    })?;
    let val = element_value_bytes(data, pos, source_is_be, is_implicit).ok_or_else(|| {
        CodecError::InvalidMetadata(format!(
            "PALETTE COLOR: LUT tag {:04X},{:04X} has undefined length",
            tag.0, tag.1
        ))
    })?;
    if !val.len().is_multiple_of(2) {
        return Err(CodecError::InvalidMetadata(
            "PALETTE COLOR: odd LUT data length".into(),
        ));
    }
    let mut out = Vec::with_capacity(val.len() / 2);
    for chunk in val.as_chunks::<2>().0 {
        out.push(if source_is_be {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_le_bytes([chunk[0], chunk[1]])
        });
    }
    Ok(out)
}

/// Map palette-index samples to planar RGB (PS3.3 C.7.9).
///
/// `pixels` must be a single-sample (1 plane) 8/16-bit decode with
/// photometric PALETTE COLOR. On success it becomes 3-sample 8-bit RGB.
/// Errors (missing LUT, segmented palette, index out of range, entry > 255)
/// propagate - the transcode caller then serves original bytes instead of
/// producing wrong colors silently.
fn apply_palette_color_lut(
    data: &[u8],
    pixels: &mut crate::codec::DecodedPixels,
    source_is_be: bool,
    is_implicit: bool,
) -> Result<(), CodecError> {
    if pixels.samples_per_pixel != 1 {
        return Err(CodecError::InvalidMetadata(
            "PALETTE COLOR must be single-sample".into(),
        ));
    }
    let bytes_per_sample = (pixels.bits_allocated as usize) >> 3;
    if bytes_per_sample != 1 && bytes_per_sample != 2 {
        return Err(CodecError::InvalidMetadata(format!(
            "PALETTE COLOR: unsupported BitsAllocated {}",
            pixels.bits_allocated
        )));
    }

    // Descriptor (0028,1101): first value = number of entries, 0 = segmented
    // palette (data then lives in 0028,1221-1223, unsupported). PS3.3 C.7.9.
    let mut declared = 0usize;
    if let Some(pos) = find_dicom_tag(data, TAG_PALETTE_DESCRIPTOR, source_is_be, is_implicit)
        && let Some(v) = element_value_bytes(data, pos, source_is_be, is_implicit)
        && v.len() >= 4
    {
        let first = if source_is_be {
            u16::from_be_bytes([v[0], v[1]])
        } else {
            u16::from_le_bytes([v[0], v[1]])
        };
        if first == 0 {
            return Err(CodecError::InvalidMetadata(
                "PALETTE COLOR: segmented palette is not supported".into(),
            ));
        }
        declared = first as usize;
    }

    let lut_r = read_palette_lut(data, TAG_PALETTE_LUTS[0], source_is_be, is_implicit)?;
    let lut_g = read_palette_lut(data, TAG_PALETTE_LUTS[1], source_is_be, is_implicit)?;
    let lut_b = read_palette_lut(data, TAG_PALETTE_LUTS[2], source_is_be, is_implicit)?;
    let count = if declared > 0 { declared } else { lut_r.len() };
    if count == 0 || lut_r.len() < count || lut_g.len() < count || lut_b.len() < count {
        return Err(CodecError::InvalidMetadata(format!(
            "PALETTE COLOR: {} entries declared but LUTs have {}/{}/{}",
            count,
            lut_r.len(),
            lut_g.len(),
            lut_b.len()
        )));
    }
    // LUT entries are 16-bit; field truth (01.09 US-003): full-range values
    // stored as 8-bit*256 (e.g. 2 -> 512), so the 8-bit display value is
    // entry >> 8. This matches dcm4che/Weasis rendering.

    // ALL frames live in `pixels.data` (single channel, frame-major).
    // Output stays per-plane frame-major: R for all frames, then G, then B.
    if !pixels.data.len().is_multiple_of(bytes_per_sample) {
        return Err(CodecError::InvalidMetadata(
            "PALETTE COLOR: pixel data size not sample-aligned".into(),
        ));
    }
    let total = pixels.data.len() / bytes_per_sample;
    let mut rgb = vec![0u8; total * 3];
    for p in 0..total {
        let idx = if bytes_per_sample == 2 {
            u16::from_le_bytes([pixels.data[p * 2], pixels.data[p * 2 + 1]]) as usize
        } else {
            pixels.data[p] as usize
        };
        if idx >= count {
            return Err(CodecError::InvalidMetadata(format!(
                "PALETTE COLOR: sample index {} out of range ({})",
                idx, count
            )));
        }
        rgb[p] = (lut_r[idx] >> 8) as u8;
        rgb[total + p] = (lut_g[idx] >> 8) as u8;
        rgb[2 * total + p] = (lut_b[idx] >> 8) as u8;
    }

    pixels.data = rgb;
    pixels.samples_per_pixel = 3;
    pixels.bits_allocated = 8;
    pixels.photometric_interpretation = "RGB".to_string();
    Ok(())
}

/// Check if a transfer syntax uses compressed (encapsulated) pixel data.
fn is_compressed_ts(ts: &str) -> bool {
    use crate::codec::ts_uid;
    matches!(
        ts,
        ts_uid::RLE_LOSSLESS
            | ts_uid::JPEG_BASELINE
            | ts_uid::JPEG_LOSSLESS
            | ts_uid::JPEG_LOSSLESS_SV1
            | ts_uid::JPEGLS_LOSSLESS
            | ts_uid::JPEGLS_NEAR_LOSSLESS
            | ts_uid::JPEG2000_LOSSLESS
            | ts_uid::JPEG2000_LOSSY
    )
}

/// Check if a transfer syntax requires 8-bit pixel data.
/// JPEG Baseline requires 8-bit. Explicit/Implicit VR LE are reduced
/// to 8-bit for viewer compatibility (clarus-viewer.html only handles 8-bit).
fn target_requires_8bit(ts: &str) -> bool {
    use crate::codec::ts_uid;
    // JPEG Baseline: hardware 8-bit limit - mandatory.
    // EVRLE/IVRLE: support 16-bit natively - do NOT reduce.
    // 16->8 reduction causes posterization ("tissue paper effect")
    // visible in medical images with smooth tissue gradients.
    matches!(ts, ts_uid::JPEG_BASELINE)
}

fn read_u16_le(data: &[u8], pos: usize) -> u16 {
    read_u16_syntax(data, pos, false)
}

fn u32_from_slice(bytes: &[u8]) -> u32 {
    if bytes.len() < 4 {
        0
    } else {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}

/// Read a u16 from `bytes` at `pos` respecting DICOM endianness.
/// `is_be` = true -> Big Endian (Explicit VR BE); false -> Little Endian.
fn read_u16_syntax(bytes: &[u8], pos: usize, is_be: bool) -> u16 {
    if pos + 2 > bytes.len() {
        return 0;
    }
    if is_be {
        u16::from_be_bytes([bytes[pos], bytes[pos + 1]])
    } else {
        u16::from_le_bytes([bytes[pos], bytes[pos + 1]])
    }
}

/// Read a u32 from `bytes` at `pos` respecting DICOM endianness.
/// `is_be` = true -> Big Endian (Explicit VR BE); false -> Little Endian.
fn read_u32_syntax(bytes: &[u8], pos: usize, is_be: bool) -> u32 {
    if pos + 4 > bytes.len() {
        return 0;
    }
    if is_be {
        u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
    } else {
        u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::ts_uid;

    #[test]
    fn test_is_compressed_ts() {
        assert!(is_compressed_ts(ts_uid::RLE_LOSSLESS));
        assert!(is_compressed_ts(ts_uid::JPEG_BASELINE));
        assert!(is_compressed_ts(ts_uid::JPEG2000_LOSSLESS));
        assert!(is_compressed_ts(ts_uid::JPEG2000_LOSSY));
        assert!(!is_compressed_ts(ts_uid::EXPLICIT_VR_LE));
        assert!(!is_compressed_ts(ts_uid::EXPLICIT_VR_BE));
    }

    #[test]
    fn test_photometric_for_ts() {
        assert_eq!(
            photometric_for_ts(ts_uid::EXPLICIT_VR_LE, "MONOCHROME2"),
            "MONOCHROME2"
        );
        assert_eq!(
            photometric_for_ts(ts_uid::JPEG_BASELINE, "MONOCHROME2"),
            "MONOCHROME2"
        );
        assert_eq!(photometric_for_ts(ts_uid::JPEG_BASELINE, "RGB"), "RGB");
        // JPEG2000: monochrome preserved
        assert_eq!(
            photometric_for_ts(ts_uid::JPEG2000_LOSSLESS, "MONOCHROME1"),
            "MONOCHROME1"
        );
        assert_eq!(
            photometric_for_ts(ts_uid::JPEG2000_LOSSY, "MONOCHROME2"),
            "MONOCHROME2"
        );
        // JPEG2000: color -> RGB
        assert_eq!(
            photometric_for_ts(ts_uid::JPEG2000_LOSSLESS, "YBR_FULL"),
            "RGB"
        );
        assert_eq!(
            photometric_for_ts(ts_uid::JPEG2000_LOSSY, "YBR_FULL_422"),
            "RGB"
        );
        // JPEG-LS (E2): monochrome preserved, color -> RGB
        assert_eq!(
            photometric_for_ts(ts_uid::JPEGLS_LOSSLESS, "MONOCHROME1"),
            "MONOCHROME1"
        );
        assert_eq!(
            photometric_for_ts(ts_uid::JPEGLS_LOSSLESS, "MONOCHROME2"),
            "MONOCHROME2"
        );
        assert_eq!(photometric_for_ts(ts_uid::JPEGLS_LOSSLESS, "RGB"), "RGB");
        assert_eq!(
            photometric_for_ts(ts_uid::JPEGLS_NEAR_LOSSLESS, "YBR_FULL_422"),
            "RGB"
        );
    }

    #[test]
    fn test_extract_transfer_syntax_explicit_vr_le() {
        // Build a minimal DICOM with Explicit VR LE
        let mut dicom = Vec::new();
        // Preamble (128 zero bytes)
        dicom.extend_from_slice(&[0u8; 128]);
        // DICM magic
        dicom.extend_from_slice(b"DICM");
        // Meta-header: TransferSyntaxUID (0002,0010) UI "1.2.840.10008.1.2.1"
        dicom.extend_from_slice(&[0x02, 0x00, 0x10, 0x00]); // tag
        dicom.extend_from_slice(b"UI"); // VR
        let ts = b"1.2.840.10008.1.2.1";
        dicom.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        dicom.extend_from_slice(ts);

        let extracted = extract_transfer_syntax(&dicom);
        assert_eq!(extracted.as_deref(), Some("1.2.840.10008.1.2.1"));
    }

    /// The demo-study fixture (test-images/clinfer_demo_implicit_vr.dcm):
    /// a de-identified 1024x1024 8-bit DX stored as Implicit VR Little
    /// Endian - the exact shape field scanners emit and the shape the live
    /// hybrid bug came from (17.09).
    #[cfg(feature = "transcode")]
    fn demo_implicit_fixture() -> Vec<u8> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-images/clinfer_demo_implicit_vr.dcm"
        );
        std::fs::read(path).expect("demo fixture must be in the repo")
    }

    /// Strict explicit-VR walker over a whole output: returns every dataset
    /// element as (tag, VR, length). Panics on any header that is not a valid
    /// explicit-VR pair, which is exactly what the hybrid output broke on.
    #[cfg(feature = "transcode")]
    fn strict_explicit_walk(data: &[u8]) -> Vec<((u16, u16), String, usize)> {
        let mut out = Vec::new();
        let mut pos = 132usize;
        while pos + 8 <= data.len() {
            let g = read_u16_le(data, pos);
            let e = read_u16_le(data, pos + 2);
            let vr = std::str::from_utf8(&data[pos + 4..pos + 6]).expect("VR bytes at {pos}");
            assert!(
                vr.chars().all(|c| c.is_ascii_uppercase()),
                "not an explicit VR header at {pos}: {vr:?}"
            );
            let extended = matches!(
                vr,
                "OB" | "OD"
                    | "OF"
                    | "OL"
                    | "OV"
                    | "OW"
                    | "SQ"
                    | "SV"
                    | "UC"
                    | "UN"
                    | "UR"
                    | "UT"
                    | "UV"
            );
            let (len, vo) = if extended {
                let l = u32_from_slice(&data[pos + 8..pos + 12]) as usize;
                (l, pos + 12)
            } else {
                let l = read_u16_le(data, pos + 6) as usize;
                (l, pos + 8)
            };
            if g == 0x0002 {
                pos = vo + len + (len % 2);
                continue;
            }
            out.push(((g, e), vr.to_string(), len));
            pos = vo + len + (len % 2);
            if (g, e) == (0x7FE0, 0x0010) {
                break;
            }
        }
        out
    }

    /// The direct path must re-encode element headers when the stored syntax
    /// is Implicit VR LE and the client asks for EVRLE: the output dataset
    /// must be walkable by a strict explicit reader (live 17.09 bug).
    #[test]
    #[cfg(feature = "transcode")]
    fn implicit_source_to_evrle_reencodes_dataset_headers() {
        let src = demo_implicit_fixture();
        let registry = crate::codec::CodecRegistry::default();
        let out = transcode_dicom(&src, ts_uid::EXPLICIT_VR_LE, &registry).expect("transcode");
        let walk = strict_explicit_walk(&out);
        let rows = walk
            .iter()
            .find(|((g, e), ..)| (*g, *e) == (0x0028, 0x0010))
            .expect("Rows walked");
        assert_eq!(rows.1, "US", "Rows must carry its VR");
        assert_eq!(rows.2, 2, "Rows value length");
        // The value itself must survive the re-encode: 1024 rows.
        let rows_pos = find_dicom_tag(&out, (0x0028, 0x0010), false, false).expect("Rows tag");
        assert_eq!(read_u16_le(&out, rows_pos + 8), 1024, "Rows value");
        let pi = walk
            .iter()
            .find(|((g, e), ..)| (*g, *e) == (0x0028, 0x0004))
            .expect("PhotometricInterpretation walked");
        assert_eq!(pi.1, "CS");
        let pd = walk
            .iter()
            .find(|((g, e), ..)| (*g, *e) == (0x7FE0, 0x0010))
            .expect("PixelData walked");
        assert_eq!(pd.1, "OW", "uncompressed PixelData is native OW");
    }

    /// The reverse flip (explicit source -> IVRLE target) must produce a
    /// strictly implicit dataset: tag + u32 length, no VR bytes anywhere.
    /// The explicit source is the EVRLE output of the sibling test.
    #[test]
    #[cfg(feature = "transcode")]
    fn explicit_source_to_ivrle_reencodes_dataset_headers() {
        let src = demo_implicit_fixture();
        let registry = crate::codec::CodecRegistry::default();
        let evrle =
            transcode_dicom(&src, ts_uid::EXPLICIT_VR_LE, &registry).expect("to EVRLE first");
        let out = transcode_dicom(&evrle, ts_uid::IMPLICIT_VR_LE, &registry).expect("transcode");
        // Walk the meta-header (explicit, always) to the dataset start.
        let mut pos = 132usize;
        while pos + 8 <= out.len() {
            let g = read_u16_le(&out, pos);
            if g != 0x0002 {
                break;
            }
            let vr = std::str::from_utf8(&out[pos + 4..pos + 6]).expect("meta VR");
            let extended = matches!(
                vr,
                "OB" | "OD"
                    | "OF"
                    | "OL"
                    | "OV"
                    | "OW"
                    | "SQ"
                    | "SV"
                    | "UC"
                    | "UN"
                    | "UR"
                    | "UT"
                    | "UV"
            );
            let len = if extended {
                u32_from_slice(&out[pos + 8..pos + 12]) as usize
            } else {
                read_u16_le(&out, pos + 6) as usize
            };
            let vo = pos + if extended { 12 } else { 8 };
            pos = vo + len + (len % 2);
        }
        let mut rows = None;
        let mut pd = None;
        while pos + 8 <= out.len() {
            let g = read_u16_le(&out, pos);
            let e = read_u16_le(&out, pos + 2);
            let len = u32_from_slice(&out[pos + 4..pos + 8]) as usize;
            if (g, e) == (0x0028, 0x0010) {
                rows = Some(&out[pos + 8..pos + 8 + len]);
            }
            if (g, e) == (0x7FE0, 0x0010) {
                pd = Some(len);
            }
            pos += 8 + len + (len % 2);
            if pd.is_some() {
                break;
            }
        }
        let rows = rows.expect("Rows walked in implicit form");
        assert_eq!(rows, &[0x00, 0x04], "Rows value 1024");
        assert_eq!(pd, Some(1024 * 1024), "PixelData implicit length");
    }

    /// Test transcode_dicom on real file and compute a BLAKE3 digest for
    /// comparison with the HTTP response.
    #[test]
    fn test_transcode_real_file_digest() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../data/instances/1d/1d27e1652de5f0eef5f90fff87936977e80f104391524bf19906994e9f16d782.dcm"
        );
        if !std::path::Path::new(path).exists() {
            eprintln!("Skipping: test file not in repo ({path})");
            return;
        }
        let original = std::fs::read(path).expect("test file must exist");

        let registry = crate::codec::CodecRegistry::default();
        let result = transcode_dicom(&original, crate::codec::ts_uid::RLE_LOSSLESS, &registry)
            .expect("transcode_dicom should succeed");

        // Compute BLAKE3 (CAS hash algorithm; sha2 removed from default build).
        let mut hasher = blake3::Hasher::new();
        hasher.update(&result);
        let hash = hasher.finalize();
        let hash_hex = hex::encode(&hash.as_bytes()[..8]);

        eprintln!(
            "Transcoded size: {} bytes, BLAKE3(first 8 bytes): {}",
            result.len(),
            hash_hex
        );
        // Save for external comparison
        let out_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test_rle_output.dcm");
        std::fs::write(out_path, &result).ok();
        eprintln!("Wrote to: {}", out_path);
    }

    /// Test the FULL transcode_dicom pipeline on the real CR file.
    #[test]
    fn test_transcode_real_file_to_rle() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../data/instances/1d/1d27e1652de5f0eef5f90fff87936977e80f104391524bf19906994e9f16d782.dcm"
        );
        if !std::path::Path::new(path).exists() {
            eprintln!("Skipping: test file not in repo ({path})");
            return;
        }
        let original = std::fs::read(path).expect("test file must exist");

        let registry = crate::codec::CodecRegistry::default();
        let result = transcode_dicom(&original, crate::codec::ts_uid::RLE_LOSSLESS, &registry)
            .expect("transcode_dicom should succeed");

        // Basic sanity: output must be larger than just preamble + DICM
        assert!(
            result.len() > 256,
            "Output too small: {} bytes",
            result.len()
        );

        // Must have DICM magic at 128
        assert_eq!(&result[128..132], b"DICM");

        // Walk dataset elements - verify Explicit VR
        let mut pos = 132;
        let mut meta_end = 0;
        while pos + 4 <= result.len() {
            let group = read_u16_le(&result, pos);
            if group != 0x0002 {
                meta_end = pos;
                break;
            }
            let _elem = read_u16_le(&result, pos + 2);
            pos += 4;
            let vr = std::str::from_utf8(&result[pos..pos + 2]).unwrap();
            pos += 2;
            let extended = matches!(
                vr,
                "OB" | "OD"
                    | "OF"
                    | "OL"
                    | "OV"
                    | "OW"
                    | "SQ"
                    | "SV"
                    | "UC"
                    | "UN"
                    | "UR"
                    | "UT"
                    | "UV"
            );
            let len: usize = if extended {
                pos += 2;
                let l = u32_from_slice(&result[pos..pos + 4]) as usize;
                pos += 4;
                l
            } else {
                let l = read_u16_le(&result, pos) as usize;
                pos += 2;
                l
            };
            pos += len + (len % 2);
        }

        assert!(meta_end > 132, "Meta-header must have at least one element");
        let first_vr = std::str::from_utf8(&result[meta_end + 4..meta_end + 6]).unwrap();
        assert!(
            first_vr.chars().all(|c| c.is_ascii_uppercase()),
            "First dataset element must have Explicit VR, got: {first_vr:?}"
        );
    }

    /// Test update_dicom_metadata on the real Implicit VR LE CR file.
    /// This is the file that pydicom can't read after transcoding.
    #[test]
    fn test_update_dicom_metadata_real_file() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../data/instances/1d/1d27e1652de5f0eef5f90fff87936977e80f104391524bf19906994e9f16d782.dcm"
        );
        if !std::path::Path::new(path).exists() {
            eprintln!("Skipping: test file not in repo ({path})");
            return;
        }
        let original = std::fs::read(path).expect("test file must exist");

        let new_ts = crate::codec::ts_uid::RLE_LOSSLESS;
        let new_pixels = b"FAKE_RLE_DATA";
        let result =
            update_dicom_metadata(&original, new_pixels, None, new_ts, "MONOCHROME2", 8, 8)
                .expect("update_dicom_metadata should succeed");

        // -- Parse meta-header --------------------------------------
        let mut pos = 132;
        let mut meta_end = 0;
        while pos + 4 <= result.len() {
            let group = read_u16_le(&result, pos);
            if group != 0x0002 {
                meta_end = pos;
                break;
            }
            let _elem = read_u16_le(&result, pos + 2);
            pos += 4;
            let vr = std::str::from_utf8(&result[pos..pos + 2]).unwrap();
            pos += 2;
            let extended = matches!(
                vr,
                "OB" | "OD"
                    | "OF"
                    | "OL"
                    | "OV"
                    | "OW"
                    | "SQ"
                    | "SV"
                    | "UC"
                    | "UN"
                    | "UR"
                    | "UT"
                    | "UV"
            );
            let len: usize = if extended {
                pos += 2;
                let l = u32_from_slice(&result[pos..pos + 4]) as usize;
                pos += 4;
                l
            } else {
                let l = read_u16_le(&result, pos) as usize;
                pos += 2;
                l
            };
            pos += len + (len % 2);
        }

        // The first element must be (0008,0005) SpecificCharacterSet
        let group = read_u16_le(&result, meta_end);
        let elem = read_u16_le(&result, meta_end + 2);
        assert_eq!(
            (group, elem),
            (0x0008, 0x0005),
            "First dataset element must be SpecificCharacterSet (0008,0005)"
        );

        // Check that it has Explicit VR (next 2 bytes after tag must be alphabetic)
        let maybe_vr = std::str::from_utf8(&result[meta_end + 4..meta_end + 6]).unwrap();
        assert!(
            maybe_vr.chars().all(|c| c.is_ascii_uppercase()),
            "Expected Explicit VR letters at first dataset element, got: {maybe_vr:?}"
        );

        // -- Walk ALL dataset elements, verify they parse as Explicit VR --
        let mut pos = meta_end;
        let mut elem_count = 0;
        let mut found_pixel_data = false;
        while pos + 6 <= result.len() && elem_count < 100 {
            let group = read_u16_le(&result, pos);
            let elem = read_u16_le(&result, pos + 2);
            pos += 4;
            let vr = std::str::from_utf8(&result[pos..pos + 2]).unwrap();
            pos += 2;

            // Check VR looks valid (2 uppercase ASCII letters)
            if !vr.chars().all(|c| c.is_ascii_uppercase()) {
                panic!(
                    "Element {}: ({:04X},{:04X}) at byte {} has invalid VR: {vr:?}. \
                     Bytes around: {:02x?}",
                    elem_count,
                    group,
                    elem,
                    pos - 6,
                    &result[pos - 6..(pos + 10).min(result.len())]
                );
            }

            let extended = matches!(
                vr,
                "OB" | "OD"
                    | "OF"
                    | "OL"
                    | "OV"
                    | "OW"
                    | "SQ"
                    | "SV"
                    | "UC"
                    | "UN"
                    | "UR"
                    | "UT"
                    | "UV"
            );
            let len: usize = if extended {
                pos += 2;
                let l = u32_from_slice(&result[pos..pos + 4]) as usize;
                pos += 4;
                l
            } else {
                let l = read_u16_le(&result, pos) as usize;
                pos += 2;
                l
            };

            if (group, elem) == (0x7FE0, 0x0010) {
                found_pixel_data = true;
            }

            pos += len + (len % 2);
            elem_count += 1;
        }

        assert!(found_pixel_data, "Must find PixelData element in output");
        assert!(
            elem_count > 5,
            "Must have at least 5 dataset elements, got {elem_count}"
        );
        eprintln!("Walked {elem_count} dataset elements successfully");
    }

    /// A DIMSE-style source with NO meta-header at all (cltranscode output)
    /// must come out of the rebuild WITH the three required meta elements -
    /// pynetdicom C-STORE refuses files missing MediaStorageSOPClassUID /
    /// MediaStorageSOPInstanceUID (field incident 30.08, C-MOVE 0xB000).
    #[test]
    fn test_update_dicom_metadata_synthesizes_missing_file_meta() {
        let mut original = Vec::new();
        original.extend_from_slice(&[0u8; 128]);
        original.extend_from_slice(b"DICM");
        // NO group 0002 - dataset starts immediately (Explicit VR LE).
        let sop_class = b"1.2.840.10008.5.1.4.1.1.2";
        original.extend_from_slice(&[0x08, 0x00, 0x16, 0x00]);
        original.extend_from_slice(b"UI");
        original.extend_from_slice(&(sop_class.len() as u16).to_le_bytes());
        original.extend_from_slice(sop_class);
        original.push(0x00); // odd-length UI pad (PS3.5 Section 7.1)
        let sop_inst = b"1.2.3.4.5.6.7.8.9.10";
        original.extend_from_slice(&[0x08, 0x00, 0x18, 0x00]);
        original.extend_from_slice(b"UI");
        original.extend_from_slice(&(sop_inst.len() as u16).to_le_bytes());
        original.extend_from_slice(sop_inst);
        original.extend_from_slice(&[0x28, 0x00, 0x10, 0x00]);
        original.extend_from_slice(b"US");
        original.extend_from_slice(&2u16.to_le_bytes());
        original.extend_from_slice(&4u16.to_le_bytes());
        original.extend_from_slice(&[0x28, 0x00, 0x11, 0x00]);
        original.extend_from_slice(b"US");
        original.extend_from_slice(&2u16.to_le_bytes());
        original.extend_from_slice(&4u16.to_le_bytes());
        original.extend_from_slice(&[0x28, 0x00, 0x04, 0x00]);
        original.extend_from_slice(b"CS");
        original.extend_from_slice(&11u16.to_le_bytes());
        original.extend_from_slice(b"MONOCHROME2");
        original.push(0x00); // odd-length CS pad (PS3.5 Section 7.1)
        original.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        original.extend_from_slice(b"OB");
        original.extend_from_slice(&[0, 0]);
        original.extend_from_slice(&4u32.to_le_bytes());
        original.extend_from_slice(&[0xAA; 4]);

        let new_ts = ts_uid::EXPLICIT_VR_LE;
        let result =
            update_dicom_metadata(&original, &[0xBB; 4], None, new_ts, "MONOCHROME2", 8, 8)
                .expect("rebuild must succeed on meta-less source");

        // Walk the OUTPUT meta group.
        let mut pos = 132usize;
        let mut got_0002 = None;
        let mut got_0003 = None;
        let mut got_0010 = None;
        while pos + 8 <= result.len() {
            let group = read_u16_le(&result, pos);
            if group != 0x0002 {
                break;
            }
            let elem = read_u16_le(&result, pos + 2);
            let vr = std::str::from_utf8(&result[pos + 4..pos + 6]).unwrap();
            let extended = matches!(
                vr,
                "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            );
            let len: usize = if extended {
                u32_from_slice(&result[pos + 8..pos + 12]) as usize
            } else {
                read_u16_le(&result, pos + 6) as usize
            };
            let vo = if extended { pos + 12 } else { pos + 8 };
            let v = String::from_utf8_lossy(&result[vo..vo + len])
                .trim_end_matches(['\0', ' '])
                .to_string();
            match elem {
                0x0002 => got_0002 = Some(v),
                0x0003 => got_0003 = Some(v),
                0x0010 => got_0010 = Some(v),
                _ => {}
            }
            pos = vo + len + (len % 2);
        }
        assert_eq!(got_0002.as_deref(), Some("1.2.840.10008.5.1.4.1.1.2"));
        assert_eq!(got_0003.as_deref(), Some("1.2.3.4.5.6.7.8.9.10"));
        assert_eq!(got_0010.as_deref(), Some(new_ts));
    }

    /// Same field incident (30.08, C-MOVE 0xB000), but through the DIRECT
    /// same-endianness path: transcode_direct memcpys the dataset and
    /// writes NO group 0002 at all for a meta-less source. The transcode
    /// post-pass must synthesize the three required meta elements there too.
    #[test]
    fn test_transcode_direct_postpass_synthesizes_missing_file_meta() {
        let mut original = Vec::new();
        original.extend_from_slice(&[0u8; 128]);
        original.extend_from_slice(b"DICM");
        // NO group 0002 - dataset starts immediately (Explicit VR LE).
        let sop_class = b"1.2.840.10008.5.1.4.1.1.2";
        original.extend_from_slice(&[0x08, 0x00, 0x16, 0x00]);
        original.extend_from_slice(b"UI");
        original.extend_from_slice(&(sop_class.len() as u16).to_le_bytes());
        original.extend_from_slice(sop_class);
        original.push(0x00); // odd-length UI pad (PS3.5 Section 7.1)
        let sop_inst = b"1.2.3.4.5.6.7.8.9.10";
        original.extend_from_slice(&[0x08, 0x00, 0x18, 0x00]);
        original.extend_from_slice(b"UI");
        original.extend_from_slice(&(sop_inst.len() as u16).to_le_bytes());
        original.extend_from_slice(sop_inst);
        original.extend_from_slice(&[0x28, 0x00, 0x10, 0x00]);
        original.extend_from_slice(b"US");
        original.extend_from_slice(&2u16.to_le_bytes());
        original.extend_from_slice(&4u16.to_le_bytes());
        original.extend_from_slice(&[0x28, 0x00, 0x11, 0x00]);
        original.extend_from_slice(b"US");
        original.extend_from_slice(&2u16.to_le_bytes());
        original.extend_from_slice(&4u16.to_le_bytes());
        original.extend_from_slice(&[0x28, 0x00, 0x04, 0x00]);
        original.extend_from_slice(b"CS");
        original.extend_from_slice(&11u16.to_le_bytes());
        original.extend_from_slice(b"MONOCHROME2");
        original.push(0x00); // odd-length CS pad (PS3.5 Section 7.1)
        original.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        original.extend_from_slice(b"OB");
        original.extend_from_slice(&[0, 0]);
        original.extend_from_slice(&4u32.to_le_bytes());
        original.extend_from_slice(&[0xAA; 4]);

        // Complete (7FE0,0010) OW element as produced by a target codec.
        let new_pixeldata_element = [
            &[0xE0u8, 0x7F, 0x10, 0x00][..],
            b"OW",
            &4u16.to_le_bytes(),
            &[0xBBu8; 4][..],
        ]
        .concat();

        let target_ts = ts_uid::EXPLICIT_VR_LE;
        let mut out = transcode_direct(
            &original,
            &new_pixeldata_element,
            target_ts,
            "MONOCHROME2",
            8,
            8,
            false, // source_is_be
            false, // is_implicit
            false, // no bit-depth reduction in this fixture
        )
        .expect("direct path must succeed on meta-less source");

        // Field fix: the transcode post-pass guarantees the meta elements.
        ensure_file_meta(&mut out, &original, target_ts);

        // Walk the OUTPUT meta group.
        let mut pos = 132usize;
        let mut got_0002 = None;
        let mut got_0003 = None;
        let mut got_0010 = None;
        while pos + 8 <= out.len() {
            let group = read_u16_le(&out, pos);
            if group != 0x0002 {
                break;
            }
            let elem = read_u16_le(&out, pos + 2);
            let vr = std::str::from_utf8(&out[pos + 4..pos + 6]).unwrap();
            let extended = matches!(
                vr,
                "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            );
            let len: usize = if extended {
                u32_from_slice(&out[pos + 8..pos + 12]) as usize
            } else {
                read_u16_le(&out, pos + 6) as usize
            };
            let vo = if extended { pos + 12 } else { pos + 8 };
            let v = String::from_utf8_lossy(&out[vo..vo + len])
                .trim_end_matches(['\0', ' '])
                .to_string();
            match elem {
                0x0002 => got_0002 = Some(v),
                0x0003 => got_0003 = Some(v),
                0x0010 => got_0010 = Some(v),
                _ => {}
            }
            pos = vo + len + (len % 2);
        }
        assert_eq!(got_0002.as_deref(), Some("1.2.840.10008.5.1.4.1.1.2"));
        assert_eq!(got_0003.as_deref(), Some("1.2.3.4.5.6.7.8.9.10"));
        assert_eq!(got_0010.as_deref(), Some(target_ts));

        // The dataset must still start right after the meta group.
        assert_eq!(read_u16_le(&out, pos), 0x0008);
    }

    /// Build a minimal Implicit VR LE DICOM and verify update_dicom_metadata
    /// converts it to Explicit VR LE with RLE pixel data.
    #[test]
    fn test_update_dicom_metadata_minimal_implicit() {
        // -- Build minimal Implicit VR LE DICOM ----------------------
        let mut original = Vec::new();
        // Preamble
        original.extend_from_slice(&[0u8; 128]);
        original.extend_from_slice(b"DICM");

        // Meta-header: (0002,0010) UI = "1.2.840.10008.1.2" (Implicit VR LE)
        original.extend_from_slice(&[0x02, 0x00, 0x10, 0x00]); // tag
        original.extend_from_slice(b"UI");
        let implicit_ts = b"1.2.840.10008.1.2";
        original.extend_from_slice(&(implicit_ts.len() as u16).to_le_bytes());
        original.extend_from_slice(implicit_ts);
        // DICOM PS3.5 Section 7.1: pad byte after odd-length value (17 bytes -> pad 0x00)
        original.push(0x00);

        // Dataset in Implicit VR LE:
        // (0008,0060) CS "CR"  - Modality
        let modality = b"CR";
        original.extend_from_slice(&[0x08, 0x00, 0x60, 0x00]); // tag
        original.extend_from_slice(&(modality.len() as u32).to_le_bytes()); // length (Implicit VR = 4 bytes)
        original.extend_from_slice(modality);

        // (0010,0010) PN "Test^Patient"
        let pn = b"Test^Patient";
        original.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]); // tag
        original.extend_from_slice(&(pn.len() as u32).to_le_bytes());
        original.extend_from_slice(pn);

        // (0028,0010) US = 256 (Rows)
        original.extend_from_slice(&[0x28, 0x00, 0x10, 0x00]); // tag
        original.extend_from_slice(&(2u32).to_le_bytes()); // length
        original.extend_from_slice(&256u16.to_le_bytes()); // value

        // (0028,0011) US = 256 (Columns)
        original.extend_from_slice(&[0x28, 0x00, 0x11, 0x00]); // tag
        original.extend_from_slice(&(2u32).to_le_bytes());
        original.extend_from_slice(&256u16.to_le_bytes());

        // (7FE0,0010) PixelData - 4 bytes of fake data
        let pixel_data: Vec<u8> = vec![0xAA; 4];
        original.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]); // tag
        original.extend_from_slice(&(pixel_data.len() as u32).to_le_bytes());
        original.extend_from_slice(&pixel_data);

        // -- Add a Sequence element (0008,1111) with one item -------
        // SQ value = Item(FFFE,E000) + item content (nested dataset in Implicit VR)
        let mut sq_value = Vec::new();
        // Item tag
        sq_value.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        // Item length (defined - the nested dataset size)
        let item_content = {
            let mut nested = Vec::new();
            // (0008,0104) LO "LEFT FOOT" - CodeMeaning inside sequence
            let code = b"LEFT FOOT";
            nested.extend_from_slice(&[0x08, 0x00, 0x04, 0x01]); // tag
            nested.extend_from_slice(&(code.len() as u32).to_le_bytes());
            nested.extend_from_slice(code);
            nested
        };
        sq_value.extend_from_slice(&(item_content.len() as u32).to_le_bytes());
        sq_value.extend_from_slice(&item_content);

        // SQ element: tag(4) + length(4) + value
        original.extend_from_slice(&[0x08, 0x00, 0x11, 0x11]); // tag (0008,1111)
        original.extend_from_slice(&(sq_value.len() as u32).to_le_bytes());
        original.extend_from_slice(&sq_value);

        // -- Run update_dicom_metadata ------------------------------
        let new_ts = ts_uid::RLE_LOSSLESS;
        let new_pixels = b"RLE_ENCODED_DATA";
        let result =
            update_dicom_metadata(&original, new_pixels, None, new_ts, "MONOCHROME2", 8, 8)
                .expect("update_dicom_metadata should succeed");

        // -- Verify output byte-by-byte ----------------------------
        // The output must be valid DICOM with Explicit VR LE dataset

        // 1. Check preamble + DICM
        assert_eq!(&result[..128], &[0u8; 128]);
        assert_eq!(&result[128..132], b"DICM");

        // 2. Parse meta-header to find TransferSyntaxUID
        let mut pos = 132;
        let mut found_ts = None;
        while pos + 4 <= result.len() {
            let group = read_u16_le(&result, pos);
            if group != 0x0002 {
                break;
            }
            let elem = read_u16_le(&result, pos + 2);
            pos += 4;
            let vr = std::str::from_utf8(&result[pos..pos + 2]).unwrap();
            pos += 2;
            let extended = matches!(
                vr,
                "OB" | "OD"
                    | "OF"
                    | "OL"
                    | "OV"
                    | "OW"
                    | "SQ"
                    | "SV"
                    | "UC"
                    | "UN"
                    | "UR"
                    | "UT"
                    | "UV"
            );
            let len: usize = if extended {
                pos += 2; // reserved
                let l = u32_from_slice(&result[pos..pos + 4]) as usize;
                pos += 4;
                l
            } else {
                let l = read_u16_le(&result, pos) as usize;
                pos += 2;
                l
            };
            if (group, elem) == (0x0002, 0x0010) {
                // Canonical UI: trailing NULL is INSIDE the length (3008).
                found_ts = Some(
                    String::from_utf8_lossy(&result[pos..pos + len])
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            // Pad to even
            pos += len + (len % 2);
        }

        // 3. Verify TS changed
        assert_eq!(
            found_ts.as_deref(),
            Some(new_ts),
            "TransferSyntaxUID must be changed to RLE"
        );

        // 4. Check first dataset element is Explicit VR LE
        // After meta-header, first element should be (0008,0060) CS "CR"
        assert!(
            pos + 6 <= result.len(),
            "Dataset should have at least one element"
        );
        let dset_group = read_u16_le(&result, pos);
        let dset_elem = read_u16_le(&result, pos + 2);
        let dset_vr_bytes = &result[pos + 4..pos + 6];
        let dset_vr = std::str::from_utf8(dset_vr_bytes).unwrap();

        assert_eq!(
            (dset_group, dset_elem),
            (0x0008, 0x0060),
            "First dataset element must be Modality (0008,0060)"
        );
        assert_eq!(
            dset_vr, "CS",
            "Dataset must use Explicit VR LE (CS for Modality)"
        );
    }

    // -- 3008 canonical file-meta tests (red-first) -----------------

    /// Independent reference emitter for the canonical trio (same PS3.5
    /// sec 6.2 spec as the production writer): ascending (0002,0002),
    /// (0002,0003), (0002,0010), Explicit VR LE UI, even length with the
    /// trailing NULL INSIDE the length, no external pad, no group length.
    fn ref_ui(out: &mut Vec<u8>, elem: u16, value: &[u8]) {
        let padded = if value.len().is_multiple_of(2) {
            value.len()
        } else {
            value.len() + 1
        };
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(padded as u16).to_le_bytes());
        out.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            out.push(0x00); // NULL pad INSIDE the length
        }
    }

    fn ref_trio(sop_class: &[u8], sop_instance: &[u8], ts: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        ref_ui(&mut out, 0x0002, sop_class);
        ref_ui(&mut out, 0x0003, sop_instance);
        ref_ui(&mut out, 0x0010, ts);
        out
    }

    /// Walk the OUTPUT meta group; return (elements as (tag,value,vo,len),
    /// dataset start offset). Reader-side tolerance: value skip is
    /// len + (len % 2).
    type MetaElem = (u16, Vec<u8>, usize, usize);
    fn walk_meta(data: &[u8]) -> (Vec<MetaElem>, usize) {
        let mut pos = 132usize;
        let mut elems = Vec::new();
        while pos + 8 <= data.len() {
            let g = read_u16_le(data, pos);
            if g != 0x0002 {
                break;
            }
            let e = read_u16_le(data, pos + 2);
            let vr = std::str::from_utf8(&data[pos + 4..pos + 6]).unwrap();
            let long_vr = matches!(
                vr,
                "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
            );
            let len: usize = if long_vr {
                u32_from_slice(&data[pos + 8..pos + 12]) as usize
            } else {
                read_u16_le(data, pos + 6) as usize
            };
            let vo = if long_vr { pos + 12 } else { pos + 8 };
            let v = data[vo..vo + len].to_vec();
            elems.push((e, v, vo, len));
            pos = vo + len + (len % 2);
        }
        (elems, pos)
    }

    /// Build an explicit VR LE dataset carrying (0008,0016)/(0008,0018),
    /// Rows, Columns, PhotometricInterpretation and a native OW PixelData.
    fn build_explicit_dataset(sop_class: &[u8], sop_instance: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        let push_ui = |out: &mut Vec<u8>, elem: u16, value: &[u8]| {
            let padded = if value.len().is_multiple_of(2) {
                value.len()
            } else {
                value.len() + 1
            };
            out.extend_from_slice(&0x0008u16.to_le_bytes());
            out.extend_from_slice(&elem.to_le_bytes());
            out.extend_from_slice(b"UI");
            out.extend_from_slice(&(padded as u16).to_le_bytes());
            out.extend_from_slice(value);
            if !value.len().is_multiple_of(2) {
                out.push(0x00);
            }
        };
        push_ui(&mut d, 0x0016, sop_class);
        push_ui(&mut d, 0x0018, sop_instance);
        d.extend_from_slice(&[0x28, 0x00, 0x10, 0x00]);
        d.extend_from_slice(b"US");
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&4u16.to_le_bytes());
        d.extend_from_slice(&[0x28, 0x00, 0x11, 0x00]);
        d.extend_from_slice(b"US");
        d.extend_from_slice(&2u16.to_le_bytes());
        d.extend_from_slice(&4u16.to_le_bytes());
        d.extend_from_slice(&[0x28, 0x00, 0x04, 0x00]);
        d.extend_from_slice(b"CS");
        d.extend_from_slice(&11u16.to_le_bytes());
        d.extend_from_slice(b"MONOCHROME2");
        d.push(0x00);
        d.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        d.extend_from_slice(b"OW");
        d.extend_from_slice(&[0u8; 2]);
        d.extend_from_slice(&4u32.to_le_bytes());
        d.extend_from_slice(&[0xAAu8; 4]);
        d
    }

    /// Wrap bytes in preamble + DICM (meta group may follow).
    fn with_preamble(body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0u8; 128]);
        v.extend_from_slice(b"DICM");
        v.extend_from_slice(body);
        v
    }

    /// A TS-ONLY source meta group (anon fixture shape) through the
    /// DIRECT path must come out with the canonical trio, ascending, and the
    /// dataset bytes untouched.
    #[test]
    fn test_direct_ts_only_source_emits_canonical_trio() {
        let sc = b"1.2.840.10008.5.1.4.1.1.2"; // 25 chars - odd
        let si = b"1.2.3.4.5.6.7.8.9.10"; // 20 chars - even
        let mut meta = Vec::new();
        ref_ui(&mut meta, 0x0010, ts_uid::EXPLICIT_VR_LE.as_bytes());
        let dataset = build_explicit_dataset(sc, si);
        let mut original = with_preamble(&meta);
        original.extend_from_slice(&dataset);

        let new_pixeldata_element = [
            &[0xE0u8, 0x7F, 0x10, 0x00][..],
            b"OW",
            &[0u8; 2],
            &4u32.to_le_bytes(),
            &[0xBBu8; 4][..],
        ]
        .concat();

        let out = transcode_direct(
            &original,
            &new_pixeldata_element,
            ts_uid::EXPLICIT_VR_LE,
            "MONOCHROME2",
            8,
            8,
            false,
            false,
            false, // no bit-depth reduction in this fixture
        )
        .expect("direct path must succeed");

        let expected = ref_trio(sc, si, ts_uid::EXPLICIT_VR_LE.as_bytes());
        let (elems, meta_end) = walk_meta(&out);
        assert_eq!(
            &out[132..meta_end],
            expected.as_slice(),
            "meta bytes differ"
        );
        assert_eq!(elems.len(), 3, "exactly three meta elements");
        let tags: Vec<u16> = elems.iter().map(|e| e.0).collect();
        assert_eq!(tags, vec![0x0002, 0x0003, 0x0010], "ascending order");
        // Dataset bytes untouched except the 4-byte pixel VALUE (same-length
        // replacement at dataset offset 114): compare everything around it.
        let ds_src = 132 + meta.len(); // dataset start in the source
        assert_eq!(out.len() - meta_end, original.len() - ds_src);
        assert_eq!(
            &out[meta_end..meta_end + 114],
            &original[ds_src..ds_src + 114]
        );
        assert_eq!(&out[meta_end + 118..], &original[ds_src + 118..]);
        assert_eq!(&out[meta_end + 114..meta_end + 118], &[0xBBu8; 4]);
    }

    /// The REBUILD path (update_dicom_metadata) emits the same
    /// canonical trio for a TS-only source whose dataset is Implicit VR LE
    /// (the real-world cross-endianness rebuild case: Implicit -> EVRLE).
    #[test]
    fn test_update_ts_only_source_emits_canonical_trio() {
        let sc = b"1.2.840.10008.5.1.4.1.1.2";
        let si = b"1.2.3.4.5.6.7.8.9.10";
        let mut meta = Vec::new();
        ref_ui(&mut meta, 0x0010, ts_uid::IMPLICIT_VR_LE.as_bytes());
        // Dataset in Implicit VR LE (matches the meta TS).
        let mut d = Vec::new();
        let push_ui = |out: &mut Vec<u8>, elem: u16, value: &[u8]| {
            let padded = if value.len().is_multiple_of(2) {
                value.len()
            } else {
                value.len() + 1
            };
            out.extend_from_slice(&0x0008u16.to_le_bytes());
            out.extend_from_slice(&elem.to_le_bytes());
            out.extend_from_slice(&(padded as u32).to_le_bytes());
            out.extend_from_slice(value);
            if !value.len().is_multiple_of(2) {
                out.push(0x00);
            }
        };
        push_ui(&mut d, 0x0016, sc);
        push_ui(&mut d, 0x0018, si);
        d.extend_from_slice(&[0x28, 0x00, 0x10, 0x00]);
        d.extend_from_slice(&2u32.to_le_bytes());
        d.extend_from_slice(&4u16.to_le_bytes());
        d.extend_from_slice(&[0x28, 0x00, 0x11, 0x00]);
        d.extend_from_slice(&2u32.to_le_bytes());
        d.extend_from_slice(&4u16.to_le_bytes());
        d.extend_from_slice(&[0x28, 0x00, 0x04, 0x00]);
        d.extend_from_slice(&12u32.to_le_bytes());
        d.extend_from_slice(b"MONOCHROME2");
        d.push(0x00);
        d.extend_from_slice(&[0xE0, 0x7F, 0x10, 0x00]);
        d.extend_from_slice(&4u32.to_le_bytes());
        d.extend_from_slice(&[0xAAu8; 4]);
        let mut original = with_preamble(&meta);
        original.extend_from_slice(&d);

        let out = update_dicom_metadata(
            &original,
            &[0xBBu8; 4],
            None,
            ts_uid::EXPLICIT_VR_LE,
            "MONOCHROME2",
            8,
            8,
        )
        .expect("rebuild must succeed");

        let expected = ref_trio(sc, si, ts_uid::EXPLICIT_VR_LE.as_bytes());
        let (elems, meta_end) = walk_meta(&out);
        assert_eq!(
            &out[132..meta_end],
            expected.as_slice(),
            "meta bytes differ"
        );
        let tags: Vec<u16> = elems.iter().map(|e| e.0).collect();
        assert_eq!(tags, vec![0x0002, 0x0003, 0x0010], "ascending order");
        // Dataset still starts with (0008,0016), now in Explicit VR LE.
        assert_eq!(read_u16_le(&out, meta_end), 0x0008);
        assert_eq!(read_u16_le(&out, meta_end + 2), 0x0016);
    }

    /// The UID resolver reads an Explicit VR Big-Endian dataset
    /// (tags and lengths swapped) - the rebuild path's source byte order.
    #[test]
    fn test_extract_top_level_ui_be_dataset() {
        let mut d = Vec::new();
        d.extend_from_slice(&[0x00, 0x08, 0x00, 0x16]); // (0008,0016) BE
        d.extend_from_slice(b"UI");
        d.extend_from_slice(&26u16.to_be_bytes());
        d.extend_from_slice(b"1.2.840.10008.5.1.4.1.1.2");
        d.push(0x00);
        let v = extract_top_level_ui(&d, 0, 0x0008, 0x0016, false, true);
        assert_eq!(v.as_deref(), Some("1.2.840.10008.5.1.4.1.1.2"));
        // Sanity: the same bytes read as LE must NOT resolve.
        let none = extract_top_level_ui(&d, 0, 0x0008, 0x0016, false, false);
        assert_eq!(none, None);
    }

    /// A meta-LESS source (cltranscode shape) resolves both UIDs
    /// from the dataset and emits the canonical trio - byte-exact.
    #[test]
    fn test_meta_less_source_emits_canonical_trio_from_dataset() {
        let sc = b"1.2.840.10008.5.1.4.1.1.2";
        let si = b"1.2.3.4.5.6.7.8.9.10";
        let original = with_preamble(&build_explicit_dataset(sc, si));

        let new_pixeldata_element = [
            &[0xE0u8, 0x7F, 0x10, 0x00][..],
            b"OW",
            &[0u8; 2],
            &4u32.to_le_bytes(),
            &[0xBBu8; 4][..],
        ]
        .concat();

        let out = transcode_direct(
            &original,
            &new_pixeldata_element,
            ts_uid::EXPLICIT_VR_LE,
            "MONOCHROME2",
            8,
            8,
            false,
            false,
            false, // no bit-depth reduction in this fixture
        )
        .expect("direct path must succeed");

        let expected = ref_trio(sc, si, ts_uid::EXPLICIT_VR_LE.as_bytes());
        let (elems, meta_end) = walk_meta(&out);
        assert_eq!(
            &out[132..meta_end],
            expected.as_slice(),
            "meta bytes differ"
        );
        assert_eq!(elems.len(), 3);
    }

    /// A source ALREADY carrying the canonical trio produces the
    /// byte-identical meta group (no churn, no reordering).
    #[test]
    fn test_full_canonical_source_meta_unchanged() {
        let sc = b"1.2.840.10008.5.1.4.1.1.2";
        let si = b"1.2.3.4.5.6.7.8.9.10";
        let meta = ref_trio(sc, si, ts_uid::EXPLICIT_VR_LE.as_bytes());
        let dataset = build_explicit_dataset(sc, si);
        let mut original = with_preamble(&meta);
        original.extend_from_slice(&dataset);

        let new_pixeldata_element = [
            &[0xE0u8, 0x7F, 0x10, 0x00][..],
            b"OW",
            &[0u8; 2],
            &4u32.to_le_bytes(),
            &[0xBBu8; 4][..],
        ]
        .concat();

        let out = transcode_direct(
            &original,
            &new_pixeldata_element,
            ts_uid::EXPLICIT_VR_LE,
            "MONOCHROME2",
            8,
            8,
            false,
            false,
            false, // no bit-depth reduction in this fixture
        )
        .expect("direct path must succeed");

        let (_, meta_end) = walk_meta(&out);
        assert_eq!(&out[132..meta_end], meta.as_slice(), "meta churned");
    }

    /// A 25-char (odd-length) SOP Class UID must be written with an
    /// EVEN length of 26 and the NULL INSIDE the value - the next element
    /// starts immediately after, with no external pad byte.
    #[test]
    fn test_odd_length_sop_class_uid_null_inside() {
        let sc = b"1.2.840.10008.5.1.4.1.1.2"; // 25 chars
        assert_eq!(sc.len(), 25);
        let si = b"1.2.3.4.5.6.7.8.9.10";
        let original = with_preamble(&build_explicit_dataset(sc, si));

        let new_pixeldata_element = [
            &[0xE0u8, 0x7F, 0x10, 0x00][..],
            b"OW",
            &[0u8; 2],
            &4u32.to_le_bytes(),
            &[0xBBu8; 4][..],
        ]
        .concat();

        let out = transcode_direct(
            &original,
            &new_pixeldata_element,
            ts_uid::EXPLICIT_VR_LE,
            "MONOCHROME2",
            8,
            8,
            false,
            false,
            false, // no bit-depth reduction in this fixture
        )
        .expect("direct path must succeed");

        let (elems, _) = walk_meta(&out);
        let (_, value, vo, len) = elems[0].clone();
        assert_eq!(value.len(), 26, "even length");
        assert_eq!(&out[vo + 25..vo + 26], &[0x00], "NULL inside the length");
        // (0002,0003) starts immediately after: no external pad byte.
        assert_eq!(&out[vo + len..vo + len + 2], &[0x02, 0x00]);
        assert_eq!(&out[vo + len + 2..vo + len + 4], &[0x03, 0x00]);
    }

    /// ensure_file_meta is idempotent - a second call is a byte
    /// no-op once the output carries the canonical trio.
    #[test]
    fn test_ensure_file_meta_double_call_byte_identical() {
        let sc = b"1.2.840.10008.5.1.4.1.1.2";
        let si = b"1.2.3.4.5.6.7.8.9.10";
        let original = with_preamble(&build_explicit_dataset(sc, si));
        let new_pixeldata_element = [
            &[0xE0u8, 0x7F, 0x10, 0x00][..],
            b"OW",
            &[0u8; 2],
            &4u32.to_le_bytes(),
            &[0xBBu8; 4][..],
        ]
        .concat();

        let mut out = transcode_direct(
            &original,
            &new_pixeldata_element,
            ts_uid::EXPLICIT_VR_LE,
            "MONOCHROME2",
            8,
            8,
            false,
            false,
            false, // no bit-depth reduction in this fixture
        )
        .expect("direct path must succeed");
        ensure_file_meta(&mut out, &original, ts_uid::EXPLICIT_VR_LE);
        let once = out.clone();
        ensure_file_meta(&mut out, &original, ts_uid::EXPLICIT_VR_LE);
        assert_eq!(out, once, "second ensure_file_meta must be a no-op");
    }

    // -- PALETTE COLOR tests (01.09) -------------------------------

    /// Build an explicit VR LE dataset with PI + palette tags (preamble +
    /// DICM, no meta group - the walker handles that).
    fn build_palette_dicom(
        pi: &str,
        lut_r: &[u16],
        lut_g: &[u16],
        lut_b: &[u16],
        descriptor: Option<&[u16]>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");

        // PhotometricInterpretation (0028,0004) CS
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0004u16.to_le_bytes());
        out.extend_from_slice(b"CS");
        let p = pi.as_bytes();
        out.extend_from_slice(&(p.len() as u16).to_le_bytes());
        out.extend_from_slice(p);
        if !p.len().is_multiple_of(2) {
            out.push(b' ');
        }

        // Descriptor (0028,1101) US
        if let Some(d) = descriptor {
            out.extend_from_slice(&0x0028u16.to_le_bytes());
            out.extend_from_slice(&0x1101u16.to_le_bytes());
            out.extend_from_slice(b"US");
            out.extend_from_slice(&((d.len() * 2) as u16).to_le_bytes());
            for v in d {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }

        // LUT data (0028,1201/1202/1203) OW
        for (elem, lut) in [(0x1201u16, lut_r), (0x1202, lut_g), (0x1203, lut_b)] {
            out.extend_from_slice(&0x0028u16.to_le_bytes());
            out.extend_from_slice(&elem.to_le_bytes());
            out.extend_from_slice(b"OW");
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&((lut.len() * 2) as u32).to_le_bytes());
            for v in lut {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn test_palette_8bit_to_rgb() {
        let dicom = build_palette_dicom(
            "PALETTE COLOR",
            &[10 << 8, 20 << 8, 30 << 8, 40 << 8],
            &[100 << 8, 110 << 8, 120 << 8, 130 << 8],
            &[200 << 8, 210 << 8, 220 << 8, 230 << 8],
            Some(&[4, 0]), // 4 entries, first mapped 0
        );
        let mut pixels = crate::codec::DecodedPixels {
            data: vec![0, 1, 2, 3],
            rows: 2,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "PALETTE COLOR".into(),
        };
        apply_palette_color_lut(&dicom, &mut pixels, false, false).unwrap();
        assert_eq!(pixels.samples_per_pixel, 3);
        assert_eq!(pixels.photometric_interpretation, "RGB");
        assert_eq!(
            pixels.data,
            vec![10, 20, 30, 40, 100, 110, 120, 130, 200, 210, 220, 230]
        );
    }

    #[test]
    fn test_palette_16bit_indices() {
        let dicom = build_palette_dicom(
            "PALETTE COLOR",
            &[10 << 8, 20 << 8],
            &[30 << 8, 40 << 8],
            &[50 << 8, 60 << 8],
            None, // no descriptor: count inferred from LUT length
        );
        let mut pixels = crate::codec::DecodedPixels {
            data: vec![0, 0, 1, 0], // two u16 LE indices: 0, 1
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "PALETTE COLOR".into(),
        };
        apply_palette_color_lut(&dicom, &mut pixels, false, false).unwrap();
        assert_eq!(pixels.data, vec![10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn test_palette_segmented_rejected() {
        let dicom = build_palette_dicom(
            "PALETTE COLOR",
            &[10],
            &[20],
            &[30],
            Some(&[0, 0]), // descriptor first value 0 = segmented palette
        );
        let mut pixels = crate::codec::DecodedPixels {
            data: vec![0],
            rows: 1,
            columns: 1,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "PALETTE COLOR".into(),
        };
        assert!(apply_palette_color_lut(&dicom, &mut pixels, false, false).is_err());
    }

    #[test]
    fn test_palette_missing_lut_rejected() {
        // PI says PALETTE COLOR but the LUT tags are absent.
        let dicom = build_palette_dicom("PALETTE COLOR", &[], &[], &[], None);
        let cut = dicom
            .windows(4)
            .position(|w| w == [0x28, 0x00, 0x01, 0x12])
            .expect("LUT tag present");
        let truncated = dicom[..cut].to_vec();
        let mut pixels = crate::codec::DecodedPixels {
            data: vec![0],
            rows: 1,
            columns: 1,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "PALETTE COLOR".into(),
        };
        assert!(apply_palette_color_lut(&truncated, &mut pixels, false, false).is_err());
    }

    #[test]
    fn test_palette_index_out_of_range_rejected() {
        let dicom = build_palette_dicom(
            "PALETTE COLOR",
            &[10],
            &[20],
            &[30],
            Some(&[1, 0]), // 1 entry
        );
        let mut pixels = crate::codec::DecodedPixels {
            data: vec![7], // index 7, LUT has 1 entry
            rows: 1,
            columns: 1,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "PALETTE COLOR".into(),
        };
        assert!(apply_palette_color_lut(&dicom, &mut pixels, false, false).is_err());
    }

    #[test]
    fn test_cs_value_equals_trimmed() {
        // CS value "MONOCHROME2 " (space pad) must compare true without alloc.
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0004u16.to_le_bytes());
        out.extend_from_slice(b"CS");
        let p = b"MONOCHROME2 ";
        out.extend_from_slice(&(p.len() as u16).to_le_bytes());
        out.extend_from_slice(p);
        assert!(cs_value_equals(
            &out,
            (0x0028, 0x0004),
            "MONOCHROME2",
            false,
            false
        ));
        assert!(!cs_value_equals(
            &out,
            (0x0028, 0x0004),
            "PALETTE COLOR",
            false,
            false
        ));
    }

    // -- Undefined-length elements inside a copy ------------------------------

    /// One undefined-length SQ holding one undefined-length item, as the
    /// derived encoder writes (0008,2112): sentinel length, (FFFE,E000) item
    /// with a sentinel length of its own, one attribute, then (FFFE,E00D)
    /// and (FFFE,E0DD).
    ///
    /// `item_implicit` chooses how the attribute INSIDE the item is spelled,
    /// because that is the flag the two copy functions have to pass on and
    /// the bug this pins was a hardcoded `false` regardless of the source.
    fn undefined_length_sq(item_implicit: bool) -> Vec<u8> {
        let mut sq = Vec::new();
        sq.extend_from_slice(&0x0008u16.to_le_bytes());
        sq.extend_from_slice(&0x2112u16.to_le_bytes());
        // `item_implicit` describes the WHOLE source dataset, so the sequence
        // header is spelled the same way its items are: implicit VR has no VR
        // field and a 4-byte length straight after the tag, explicit VR has
        // the VR, two reserved bytes and then the length.
        if item_implicit {
            sq.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        } else {
            sq.extend_from_slice(b"SQ");
            sq.extend_from_slice(&[0, 0]);
            sq.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        sq.extend_from_slice(&0xFFFEu16.to_le_bytes());
        sq.extend_from_slice(&0xE000u16.to_le_bytes());
        sq.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // One UI attribute, 27 characters, so the NUL pad brings it to 28.
        sq.extend_from_slice(&0x0008u16.to_le_bytes());
        sq.extend_from_slice(&0x1150u16.to_le_bytes());
        if item_implicit {
            sq.extend_from_slice(&28u32.to_le_bytes());
        } else {
            sq.extend_from_slice(b"UI");
            sq.extend_from_slice(&28u16.to_le_bytes());
        }
        sq.extend_from_slice(b"1.2.840.10008.5.1.4.1.1.1.1\0");
        sq.extend_from_slice(&0xFFFEu16.to_le_bytes());
        sq.extend_from_slice(&0xE00Du16.to_le_bytes());
        sq.extend_from_slice(&0u32.to_le_bytes());
        sq.extend_from_slice(&0xFFFEu16.to_le_bytes());
        sq.extend_from_slice(&0xE0DDu16.to_le_bytes());
        sq.extend_from_slice(&0u32.to_le_bytes());
        sq
    }

    /// A dataset of the sequence plus a trailing defined-length element, so
    /// the copy has something after it to get wrong.
    fn dataset_with_undefined_sq(item_implicit: bool) -> Vec<u8> {
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        // The 128-byte preamble plus "DICM" put the first element at 132, and
        // the tests below name that offset, so assert it rather than trust it.
        assert_eq!(out.len(), 132, "the dataset begins after the magic");
        out.extend_from_slice(&undefined_length_sq(item_implicit));
        // (0010,0010) PN, defined length, the element a misparse would eat.
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"PN");
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(b"TEST");
        // A last element after PN, so PN's value does not end exactly at the
        // buffer end: the copier refuses a value that runs past the buffer,
        // and a PN that is the final element would be refused for that
        // reason rather than for the sequence the test is about.
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(&0x0020u16.to_le_bytes());
        out.extend_from_slice(b"LO");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(b"ID");
        out
    }

    /// Where the sequence starts in [`dataset_with_undefined_sq`].
    const SQ_AT: usize = 132;

    /// `copy_element_to_implicit` must walk the item stream, not read a VR.
    ///
    /// The cursor it hands to the skip function is inside the item stream,
    /// where group FFFE carries NO VR (PS3.5 7.5). Reading one takes the
    /// item's own length as the VR, the walk desynchronises, and the copy
    /// gives up with "Cannot skip undefined element" - truncating the output
    /// at the sequence and losing every element after it.
    #[test]
    fn copy_to_implicit_walks_an_undefined_length_sequence() {
        let src = dataset_with_undefined_sq(false);
        let mut out = Vec::new();
        // The cursor points at the VR, which is what this function reads.
        let mut pos = SQ_AT + 4;
        copy_element_to_implicit(
            &mut out, &src, &mut pos, 0x0008, 0x2112, SQ_AT, false, false,
        )
        .expect("the sequence is copied, not abandoned");

        let expected_end = SQ_AT + undefined_length_sq(false).len();
        assert_eq!(pos, expected_end, "and the cursor lands past the delimiter");
        // The function writes the tag itself before appending the span, so
        // the output is the tag twice: the copy's own, then the verbatim
        // span that begins with the same four bytes.
        assert_eq!(
            &out[4..],
            &src[SQ_AT..expected_end],
            "the bytes are copied verbatim, delimiters and all"
        );
        assert_eq!(&out[..4], &src[SQ_AT..SQ_AT + 4], "and the tag is written");
    }

    /// The same walk, driven with the SOURCE marked implicit.
    ///
    /// The flag is not decoration: an implicit-VR source spells the attribute
    /// inside the item with a 4-byte length and no VR, so a walk that assumed
    /// explicit would read two bytes of that length as a VR and be two out at
    /// every nested attribute. This is the case the hardcoded `false` broke.
    #[test]
    fn copy_to_implicit_honours_the_source_syntax_inside_the_items() {
        let src = dataset_with_undefined_sq(true);
        let mut out = Vec::new();
        let mut pos = SQ_AT + 4;
        copy_element_to_implicit(&mut out, &src, &mut pos, 0x0008, 0x2112, SQ_AT, true, false)
            .expect("an implicit-VR source walks implicit items");

        let expected_end = SQ_AT + undefined_length_sq(true).len();
        assert_eq!(pos, expected_end);
        assert_eq!(&out[4..], &src[SQ_AT..expected_end]);
    }

    /// PS3.5 7.1: the transfer syntax fixes the byte order of every binary VR.
    /// A Big Endian source copied into Implicit VR LE must have its numeric
    /// values swapped; copying them verbatim produced a file that opens and
    /// shows garbage, because Rows, BitsAllocated and the pixels themselves
    /// were all reversed.
    #[test]
    fn copy_to_implicit_swaps_a_big_endian_value() {
        // (0028,0010) US Rows = 512, written BIG endian.
        let mut src = Vec::new();
        src.extend_from_slice(&[0x28, 0x00, 0x10, 0x00]); // tag, BE
        src.extend_from_slice(b"US");
        src.extend_from_slice(&2u16.to_be_bytes());
        src.extend_from_slice(&512u16.to_be_bytes()); // 0x02 0x00 in BE

        let mut out = Vec::new();
        let mut pos = 4;
        copy_element_to_implicit(&mut out, &src, &mut pos, 0x0028, 0x0010, 0, false, true)
            .expect("a BE source copies");

        // Implicit VR LE: tag (already LE), 4-byte length, swapped value.
        assert_eq!(&out[..4], &[0x28, 0x00, 0x10, 0x00]);
        assert_eq!(&out[4..8], &2u32.to_le_bytes());
        assert_eq!(&out[8..10], &512u16.to_le_bytes(), "value is LE now");

        // The little-endian control: the value must pass through untouched.
        let mut le = Vec::new();
        le.extend_from_slice(&[0x28, 0x00, 0x10, 0x00]);
        le.extend_from_slice(b"US");
        le.extend_from_slice(&2u16.to_le_bytes());
        le.extend_from_slice(&512u16.to_le_bytes());
        let mut out_le = Vec::new();
        let mut pos = 4;
        copy_element_to_implicit(&mut out_le, &le, &mut pos, 0x0028, 0x0010, 0, false, false)
            .expect("an LE source copies");
        assert_eq!(&out_le[8..10], &512u16.to_le_bytes());
    }

    /// `copy_explicit_element` is the second half of the same defect.
    #[test]
    fn copy_explicit_walks_an_undefined_length_sequence() {
        let src = dataset_with_undefined_sq(false);
        let mut out = Vec::new();
        let mut pos = SQ_AT + 4;
        copy_explicit_element(&mut out, &src, &mut pos, SQ_AT, false, false)
            .expect("the sequence is copied, not abandoned");

        let expected_end = SQ_AT + undefined_length_sq(false).len();
        assert_eq!(pos, expected_end);
        // `copy_explicit_element` copies from the element's start without
        // writing a tag of its own, so the span begins at the tag here.
        assert_eq!(out, src[SQ_AT..expected_end]);
    }

    /// The control: the element after the sequence is still reachable, which
    /// is what a desynchronised walk costs.
    #[test]
    fn the_element_after_an_undefined_length_sequence_survives_the_copy() {
        let src = dataset_with_undefined_sq(false);
        let seq_len = undefined_length_sq(false).len();
        let mut out = Vec::new();
        let mut pos = SQ_AT + 4;
        copy_explicit_element(&mut out, &src, &mut pos, SQ_AT, false, false).expect("the sequence");
        // Now the next element, from where the copy left the cursor. The
        // cursor is ON the tag, and the function wants `pos` at the VR with
        // `elem_start` at the tag, so the two differ by the tag's four bytes.
        assert_eq!(pos, SQ_AT + seq_len, "the cursor is on (0010,0010)");
        let elem_start = pos;
        let mut vr_pos = elem_start + 4;
        copy_explicit_element(&mut out, &src, &mut vr_pos, elem_start, false, false)
            .expect("the element after the sequence is copied");
        assert!(
            out.ends_with(b"TEST"),
            "the PN value follows the sequence in the output"
        );
    }

    // -- Implicit -> Explicit VR of a NON-IMAGE dataset ------------------

    /// A minimal DICOM file whose dataset is Implicit VR LE.
    ///
    /// `elements` are already-encoded implicit elements (tag, 4-byte length,
    /// value); the helper only supplies the preamble, the `DICM` magic and the
    /// File Meta group.
    fn implicit_file(elements: &[u8]) -> Vec<u8> {
        let mut file = vec![0u8; 128];
        file.extend_from_slice(b"DICM");
        // (0002,0002) MediaStorageSOPClassUID and (0002,0003) SOP Instance
        // UID name the object; (0002,0010) TransferSyntaxUID names the
        // encoding this file is in.
        for (tag, uid) in [
            (0x0002_0002u32, "1.2.840.10008.5.1.4.1.1.88.11"),
            (0x0002_0003, "1.2.826.0.1.3680043.10.9999.1"),
            (0x0002_0010, ts_uid::IMPLICIT_VR_LE),
        ] {
            file.extend_from_slice(&(tag >> 16).to_le_bytes()[..2]);
            file.extend_from_slice(&((tag & 0xFFFF) as u16).to_le_bytes());
            file.extend_from_slice(b"UI");
            // An odd-length UI value is padded with NULL, and the pad is part
            // of the value (PS3.5 6.2).
            let padded = if uid.len().is_multiple_of(2) {
                uid.len()
            } else {
                uid.len() + 1
            };
            file.extend_from_slice(&(padded as u16).to_le_bytes());
            file.extend_from_slice(uid.as_bytes());
            if !uid.len().is_multiple_of(2) {
                file.push(0);
            }
        }
        file.extend_from_slice(elements);
        file
    }

    /// One Implicit VR LE element: tag, 4-byte length, value (+ pad).
    fn implicit_element(group: u16, elem: u16, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + value.len() + 1);
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&elem.to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
        if !value.len().is_multiple_of(2) {
            out.push(0);
        }
        out
    }

    /// A non-image object (Surface Segmentation) with a nested sequence: the
    /// shape `transcode_dicom` cannot handle, because it has no Rows, no
    /// Columns and no PixelData for the decoder to work from.
    fn implicit_surface_dataset() -> Vec<u8> {
        let mut dataset = Vec::new();
        dataset.extend_from_slice(&implicit_element(
            0x0008,
            0x0016,
            b"1.2.840.10008.5.1.4.1.1.88.11",
        ));
        dataset.extend_from_slice(&implicit_element(
            0x0008,
            0x0018,
            b"1.2.826.0.1.3680043.10.9999.1",
        ));
        dataset.extend_from_slice(&implicit_element(0x0010, 0x0010, b"TEST^PATIENT "));
        // (0066,0001) Surface Count, IS "1"
        dataset.extend_from_slice(&implicit_element(0x0066, 0x0001, b"1 "));
        // (0066,0002) Surface Sequence, EXPLICIT length: one item holding
        // (0008,0016) SOP Class UID - a nested dataset, so the item's own
        // elements flip encoding too.
        let mut item = Vec::new();
        item.extend_from_slice(&implicit_element(
            0x0008,
            0x0016,
            b"1.2.840.10008.5.1.4.1.1.88.11",
        ));
        let mut sequence = Vec::new();
        sequence.extend_from_slice(&0xFFFEu16.to_le_bytes());
        sequence.extend_from_slice(&0xE000u16.to_le_bytes());
        sequence.extend_from_slice(&(item.len() as u32).to_le_bytes());
        sequence.extend_from_slice(&item);
        dataset.extend_from_slice(&implicit_element(0x0066, 0x0002, &sequence));
        implicit_file(&dataset)
    }

    /// The re-encoder turns an Implicit VR dataset into Explicit VR without a
    /// PixelData element, and the meta-header follows the bytes.
    #[test]
    fn a_non_image_implicit_dataset_becomes_explicit_vr() {
        let source = implicit_surface_dataset();
        assert_eq!(
            extract_transfer_syntax(&source).as_deref(),
            Some(ts_uid::IMPLICIT_VR_LE),
            "the fixture is Implicit VR to begin with"
        );

        let out = reencode_implicit_to_explicit(&source).expect("re-encoded");

        assert_eq!(
            extract_transfer_syntax(&out).as_deref(),
            Some(ts_uid::EXPLICIT_VR_LE),
            "the file meta has to declare what the bytes now are"
        );
        // A dataset element is now Explicit VR: (0008,0016) is followed by its
        // VR bytes, which an Implicit VR element can never carry.
        let dataset_start = find_metaheader_end(&out, 132).expect("meta end");
        assert_eq!(&out[dataset_start + 4..dataset_start + 6], b"UI");
        // The values are untouched: same patient name, same surface count.
        assert!(
            out.windows(12).any(|w| w == b"TEST^PATIENT"),
            "the PN value survives the header rewrite"
        );
    }

    /// The nested item is re-encoded too. A sequence left implicit inside an
    /// explicit dataset is the hybrid encoding a strict reader rejects on the
    /// element after the sequence.
    #[test]
    fn the_sequence_inside_a_non_image_dataset_is_reencoded() {
        let out = reencode_implicit_to_explicit(&implicit_surface_dataset()).expect("re-encoded");
        let at = out
            .windows(4)
            .position(|w| w == [0x66, 0x00, 0x02, 0x00])
            .expect("Surface Sequence tag");
        // (0066,0002) SQ header in the target encoding: tag(4) + VR(2) +
        // reserved(2) + length(4), and SQ is an extended-length VR.
        assert_eq!(&out[at + 4..at + 6], b"SQ", "the sequence itself is SQ");
        let item_start = at + 4 + 8;
        assert_eq!(
            &out[item_start..item_start + 4],
            &[0xFE, 0xFF, 0x00, 0xE0],
            "the item tag is byte-identical in both encodings"
        );
        // The item's own first element, past ItemTag(4) + ItemLength(4).
        let nested = item_start + 8;
        assert_eq!(
            &out[nested..nested + 4],
            &[0x08, 0x00, 0x16, 0x00],
            "the nested element starts at (0008,0016)"
        );
        assert_eq!(
            &out[nested + 4..nested + 6],
            b"UI",
            "the nested element carries its VR"
        );
    }

    /// A file with no `DICM` prefix is refused rather than guessed at.
    #[test]
    fn a_file_without_a_dicom_prefix_is_refused() {
        let error = reencode_implicit_to_explicit(b"not a dicom file").expect_err("refused");
        assert!(
            matches!(error, CodecError::InvalidMetadata(_)),
            "got {error:?}"
        );
    }
}
