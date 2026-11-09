// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Bit-depth reduction: 16-bit -> 8-bit for JPEG Baseline transcoding.
//!
//! JPEG Baseline (1.2.840.10008.1.2.4.50) only supports 8-bit pixels.
//! When transcoding 16-bit DICOM images (CT, MR, CR, etc.) to JPEG Baseline,
//! we must reduce the bit depth using VOI LUT (Window Center/Width) or
//! min-max normalization as a fallback.
//!
//! # Algorithm
//!
//! 1. Extract Window Center (0028,1050) / Window Width (0028,1051) from DICOM
//! 2. If present: apply linear VOI LUT windowing
//! 3. If absent: min-max normalization
//! 4. MONOCHROME1 images are inverted (white-on-black -> black-on-white)
//!
//! # References
//!
//! - DICOM PS3.3 Section C.11.2.1.2 (VOI LUT)
//! - DICOM PS3.3 Section C.7.6.3.1.2 (MONOCHROME1/2)

#![forbid(unsafe_code)]

use crate::codec::{CodecError, DecodedPixels};

/// DICOM tag: Window Center (0028,1050) - DS, multi-valued.
const TAG_WINDOW_CENTER: (u16, u16) = (0x0028, 0x1050);
/// DICOM tag: Window Width (0028,1051) - DS, multi-valued.
const TAG_WINDOW_WIDTH: (u16, u16) = (0x0028, 0x1051);
/// DICOM tag: Rescale Intercept (0028,1052) - DS, single value.
const TAG_RESCALE_INTERCEPT: (u16, u16) = (0x0028, 0x1052);
/// DICOM tag: Rescale Slope (0028,1053) - DS, single value.
const TAG_RESCALE_SLOPE: (u16, u16) = (0x0028, 0x1053);
/// DICOM tag: Pixel Representation (0028,0103) - US, 0 unsigned / 1 signed.
const TAG_PIXEL_REPRESENTATION: (u16, u16) = (0x0028, 0x0103);

/// Reduce 16-bit pixels to 8-bit using VOI LUT or min-max normalization.
///
/// # Arguments
/// - `pixels`: decoded pixel data (must have `bits_allocated > 8`)
/// - `dicom_bytes`: original DICOM for VOI LUT tag extraction
///
/// # Returns
/// New `DecodedPixels` with `bits_allocated = 8` and data reduced to 1 byte/sample.
///
/// # Panics
/// Never panics. Returns `CodecError` only if pixel data is malformed.
pub fn reduce_bit_depth_16to8(
    pixels: &DecodedPixels,
    dicom_bytes: &[u8],
) -> Result<DecodedPixels, CodecError> {
    if pixels.bits_allocated <= 8 {
        // Already 8-bit or less - nothing to do
        return Ok(pixels.clone());
    }

    let bytes_per_sample = (pixels.bits_allocated as usize) >> 3;
    if bytes_per_sample < 2 {
        return Err(CodecError::InvalidMetadata(format!(
            "bit_depth: unsupported bytes_per_sample={bytes_per_sample} for bits_allocated={}",
            pixels.bits_allocated
        )));
    }

    let pixel_count = (pixels.rows as usize) * (pixels.columns as usize);
    let num_components = pixels.samples_per_pixel as usize;
    let component_size = pixel_count * bytes_per_sample;

    // Extract VOI LUT parameters from original DICOM
    let voi = extract_window_params(dicom_bytes);
    // PS3.3 C.11.2.1.2: RescaleIntercept / RescaleSlope map the STORED value
    // to the modality value, and Window Center/Width are expressed in THOSE
    // units. Windowing the raw stored value against HU numbers put a signed CT
    // range entirely below the window: -1000 read as 64536 and every windowed
    // image came out almost binary.
    let intercept = extract_first_ds(dicom_bytes, TAG_RESCALE_INTERCEPT).unwrap_or(0.0);
    let slope = extract_first_ds(dicom_bytes, TAG_RESCALE_SLOPE).unwrap_or(1.0);
    // PS3.3 C.7.6.3.1.2: 1 = the samples are two's-complement signed.
    let signed = extract_first_us(dicom_bytes, TAG_PIXEL_REPRESENTATION).unwrap_or(0) == 1;
    let modality = ModalityValues {
        intercept,
        slope,
        signed,
    };

    // Process each component independently
    let mut out_data = Vec::with_capacity(pixel_count * num_components);
    let invert = pixels.photometric_interpretation == "MONOCHROME1";

    for comp in 0..num_components {
        let comp_start = comp * component_size;
        let comp_data = &pixels.data[comp_start..comp_start + component_size];

        let reduced = match voi {
            Some((center, width)) => {
                apply_voi_lut(comp_data, pixel_count, center, width, invert, &modality)
            }
            None => min_max_normalize(comp_data, pixel_count, invert, &modality),
        };

        out_data.extend_from_slice(&reduced);
    }

    Ok(DecodedPixels {
        data: out_data,
        rows: pixels.rows,
        columns: pixels.columns,
        samples_per_pixel: pixels.samples_per_pixel,
        bits_allocated: 8,
        // MONOCHROME1 was inverted above -> now MONOCHROME2
        photometric_interpretation: if invert {
            "MONOCHROME2".to_string()
        } else {
            pixels.photometric_interpretation.clone()
        },
    })
}

// ===========================================================================
// VOI LUT extraction
// ===========================================================================

/// Extract Window Center and Window Width from raw DICOM bytes.
///
/// Returns `(center, width)` if both tags are present and width > 0.
fn extract_window_params(dicom_bytes: &[u8]) -> Option<(f64, f64)> {
    let center = extract_first_ds(dicom_bytes, TAG_WINDOW_CENTER)?;
    let width = extract_first_ds(dicom_bytes, TAG_WINDOW_WIDTH)?;
    if width <= 0.0 {
        return None;
    }
    Some((center, width))
}

/// Extract the first decimal string value from a DICOM tag.
///
/// Handles multi-valued DS (e.g., "40.0\60.0" -> 40.0).
/// Returns `None` if tag not found or value is unparseable.
fn extract_first_ds(data: &[u8], tag: (u16, u16)) -> Option<f64> {
    let raw = find_tag_value(data, tag)?;
    let s = String::from_utf8_lossy(&raw);
    // DS may have multiple values separated by backslash
    let first = s.split('\\').next().unwrap_or(&s).trim();
    // Some encodings pad with null bytes or spaces
    let cleaned = first.trim_end_matches(['\0', ' ']);
    cleaned.parse::<f64>().ok()
}

/// Extract the first `US` value from a DICOM tag.
///
/// `US` is binary little endian (PS3.5 Table 6.2-1), not text, so it cannot go
/// through the decimal-string reader.
fn extract_first_us(data: &[u8], tag: (u16, u16)) -> Option<u16> {
    let raw = find_tag_value(data, tag)?;
    if raw.len() < 2 {
        return None;
    }
    // PS3.5 7.1: the byte order follows the transfer syntax.
    Some(if is_big_endian(data) {
        u16::from_be_bytes([raw[0], raw[1]])
    } else {
        u16::from_le_bytes([raw[0], raw[1]])
    })
}

// ===========================================================================
// VOI LUT application
// ===========================================================================

/// The presentation transform between stored and modality values.
///
/// PS3.3 C.11.2.1.2: the stored value becomes a modality value through
/// `slope * stored + intercept`, and Window Center/Width are in modality
/// units. PS3.3 C.7.6.3.1.2 gives the signedness of the stored sample.
struct ModalityValues {
    intercept: f64,
    slope: f64,
    signed: bool,
}

impl ModalityValues {
    /// Stored sample -> modality value.
    fn value(&self, chunk: &[u8; 2]) -> f64 {
        let raw = if self.signed {
            // Two's complement: the high bit is the sign, so the same 16 bits
            // read as a negative number.
            i16::from_le_bytes(*chunk) as f64
        } else {
            u16::from_le_bytes(*chunk) as f64
        };
        self.slope * raw + self.intercept
    }
}

/// Apply linear VOI LUT windowing: map pixel values through Window Center/Width.
///
/// Formula: output = clamp((val - low) / (high - low), 0, 1) * 255
/// where low = center - width/2, high = center + width/2
fn apply_voi_lut(
    data: &[u8],
    pixel_count: usize,
    center: f64,
    width: f64,
    invert: bool,
    modality: &ModalityValues,
) -> Vec<u8> {
    let low = center - width / 2.0;
    let range = width;
    let mut out = Vec::with_capacity(pixel_count);

    for chunk in data.as_chunks::<2>().0 {
        let val = modality.value(chunk);
        let norm = (val - low) / range;
        let mapped = if invert {
            255.0 - norm * 255.0
        } else {
            norm * 255.0
        };
        out.push(mapped.clamp(0.0, 255.0) as u8);
    }

    out
}

// ===========================================================================
// Min-max normalization (fallback)
// ===========================================================================

/// Min-max normalization: scale 16-bit values to 0..255.
///
/// Finds min and max in the data, then linearly maps to [0, 255].
/// If all values equal -> output mid-gray (128).
/// If `invert` -> white becomes black (MONOCHROME1).
fn min_max_normalize(
    data: &[u8],
    pixel_count: usize,
    invert: bool,
    modality: &ModalityValues,
) -> Vec<u8> {
    // Find min/max
    let mut min_val = f64::INFINITY;
    let mut max_val = f64::NEG_INFINITY;
    for chunk in data.as_chunks::<2>().0 {
        let val = modality.value(chunk);
        min_val = min_val.min(val);
        max_val = max_val.max(val);
    }

    // Uniform image
    if min_val == max_val {
        return vec![128u8; pixel_count];
    }

    let range = max_val - min_val;
    let mut out = Vec::with_capacity(pixel_count);

    for chunk in data.as_chunks::<2>().0 {
        let val = modality.value(chunk);
        let norm = (val - min_val) / range;
        let mapped = if invert {
            255.0 - norm * 255.0
        } else {
            norm * 255.0
        };
        out.push(mapped.clamp(0.0, 255.0) as u8);
    }

    out
}

// ===========================================================================
// Low-level DICOM tag scanning
// ===========================================================================

/// The element grammar in force for this instance.
///
/// PS3.5 6.2 and 7.1: a dataset is either Explicit VR (with the VR spelled at
/// every element) and either Little or Big Endian; an Implicit VR dataset has
/// no VR at the element at all. The reader below once assumed Explicit VR LE
/// unconditionally, which mis-read every element of the other two forms.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grammar {
    ImplicitVrLe,
    ExplicitVrLe,
    ExplicitVrBe,
}

impl Grammar {
    fn implicit(self) -> bool {
        self == Grammar::ImplicitVrLe
    }
    fn big_endian(self) -> bool {
        self == Grammar::ExplicitVrBe
    }
    fn u16(self, data: &[u8], pos: usize) -> u16 {
        if self.big_endian() {
            u16::from_be_bytes([data[pos], data[pos + 1]])
        } else {
            u16::from_le_bytes([data[pos], data[pos + 1]])
        }
    }
    fn u32(self, data: &[u8], pos: usize) -> u32 {
        if self.big_endian() {
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
        } else {
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
        }
    }
}

/// Transfer syntax UID (0002,0010) of a Part 10 file, as a `Grammar`.
///
/// The meta group is always Explicit VR Little Endian (PS3.10 7.1), so it is
/// read before the dataset's own grammar is known.
fn grammar_of(data: &[u8]) -> Grammar {
    let mut pos = if data.len() >= 132 && &data[128..132] == b"DICM" {
        132
    } else {
        return Grammar::ExplicitVrLe;
    };
    while pos + 8 <= data.len() {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if g != 0x0002 {
            break;
        }
        let e = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        let vr = [data[pos + 4], data[pos + 5]];
        let (len, vstart) = if is_extended_vr(vr) {
            if pos + 12 > data.len() {
                break;
            }
            (
                u32::from_le_bytes([data[pos + 8], data[pos + 9], data[pos + 10], data[pos + 11]])
                    as usize,
                pos + 12,
            )
        } else {
            (
                u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as usize,
                pos + 8,
            )
        };
        if (g, e) == (0x0002, 0x0010) && vstart + len <= data.len() {
            let ts = String::from_utf8_lossy(&data[vstart..vstart + len]);
            let ts = ts.trim().trim_end_matches('\0');
            return match ts {
                "1.2.840.10008.1.2" => Grammar::ImplicitVrLe,
                "1.2.840.10008.1.2.2" => Grammar::ExplicitVrBe,
                // Every other syntax in PS3.6 keeps the Explicit VR Little
                // Endian element grammar; only the pixel stream differs.
                _ => Grammar::ExplicitVrLe,
            };
        }
        pos = vstart.saturating_add(len + (len % 2));
    }
    Grammar::ExplicitVrLe
}

/// True when this file's values are stored Big Endian.
fn is_big_endian(data: &[u8]) -> bool {
    grammar_of(data).big_endian()
}

/// Find the raw value bytes of a DICOM tag by scanning the binary dataset.
///
/// Handles the element grammar of the instance (Implicit VR, Explicit VR LE,
/// Explicit VR BE) rather than assuming one.
/// Returns the raw value bytes (without VR/length header).
fn find_tag_value(data: &[u8], target: (u16, u16)) -> Option<Vec<u8>> {
    let grammar = grammar_of(data);
    let mut pos = dicom_data_start(data)?;
    let end = data.len();

    while pos + 4 <= end {
        let g = grammar.u16(data, pos);
        let e = grammar.u16(data, pos + 2);
        pos += 4;

        if (g, e) == target {
            // Found - extract value
            let (len, vstart) = element_header(data, grammar, pos)?;
            if len == 0xFFFF_FFFF {
                return Some(Vec::new()); // undefined length
            }
            if vstart + len > end {
                return None;
            }
            return Some(data[vstart..vstart + len].to_vec());
        }

        // Skip non-target elements
        if g == 0xFFFE {
            if pos + 4 > end {
                return None;
            }
            let len = grammar.u32(data, pos) as usize;
            pos += 4;
            if len == 0xFFFF_FFFF {
                continue;
            }
            skip_value(&mut pos, len);
            continue;
        }

        if !skip_element(data, &mut pos, grammar) {
            return None;
        }
    }

    None
}

/// Length and value start of the element whose value header begins at `pos`.
fn element_header(data: &[u8], grammar: Grammar, pos: usize) -> Option<(usize, usize)> {
    let end = data.len();
    if grammar.implicit() {
        // PS3.5 6.2: no VR, a 4-byte length directly after the tag.
        if pos + 4 > end {
            return None;
        }
        return Some((grammar.u32(data, pos) as usize, pos + 4));
    }
    if pos + 2 > end {
        return None;
    }
    let vr_bytes = [data[pos], data[pos + 1]];
    let vr = std::str::from_utf8(&vr_bytes).unwrap_or("  ");
    if is_extended_vr(vr_bytes) {
        if pos + 8 > end {
            return None;
        }
        // Two reserved bytes then a 32-bit length (PS3.5 7.1.2).
        return Some((grammar.u32(data, pos + 4) as usize, pos + 8));
    }
    if pos + 4 > end {
        return None;
    }
    let _ = vr;
    Some((grammar.u16(data, pos + 2) as usize, pos + 4))
}

/// VRs whose header carries two reserved bytes and a 32-bit length.
fn is_extended_vr(vr_bytes: [u8; 2]) -> bool {
    matches!(
        &vr_bytes,
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

/// Find the start of DICOM dataset (after preamble + meta-header).
fn dicom_data_start(data: &[u8]) -> Option<usize> {
    if data.len() < 132 {
        return None;
    }
    let mut pos = if &data[128..132] == b"DICM" { 132 } else { 0 };
    // Skip meta-header (group 0002)
    let end = data.len();
    // PS3.10 7.1: the File Meta Information group is ALWAYS Explicit VR
    // Little Endian, whatever the dataset's own syntax is.
    while pos + 4 <= end {
        let g = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if g != 0x0002 {
            break;
        }
        pos += 4;
        if !skip_element(data, &mut pos, Grammar::ExplicitVrLe) {
            return None;
        }
    }
    Some(pos)
}

/// Skip a DICOM data element, advancing `pos` past its value.
fn skip_element(data: &[u8], pos: &mut usize, grammar: Grammar) -> bool {
    match element_header(data, grammar, *pos) {
        Some((len, vstart)) => {
            // An undefined length is not a skip distance: the value is a
            // sequence whose extent is not known here. Stop rather than walk
            // into it with a four-million-byte leap.
            if len == 0xFFFF_FFFF {
                return false;
            }
            *pos = vstart;
            // `skip_value` already steps over the odd-length pad byte
            // (PS3.5 7.1), so adding it here would overshoot by one.
            skip_value(pos, len);
            *pos <= data.len()
        }
        None => false,
    }
}

use crate::codec::skip_value;

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- DICOM builder helper ------------------------------------------

    /// Build a minimal 16-bit monochrome DICOM with optional VOI LUT tags.
    fn build_test_dicom(
        pixels_16: &[u16],
        w: u16,
        h: u16,
        window: Option<(f64, f64)>,
        photometric: &str,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        // Preamble + DICM
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");

        // Meta-header: TransferSyntaxUID (0002,0010) = Explicit VR LE
        let ts = b"1.2.840.10008.1.2.1";
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(ts.len() as u16).to_le_bytes());
        out.extend_from_slice(ts);
        // DICOM PS3.5 Section 7.1: pad byte for odd-length values
        if !ts.len().is_multiple_of(2) {
            out.push(0x00);
        }

        // Dataset tags
        // Rows (0028,0010)
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"US");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());

        // Columns (0028,0011)
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0011u16.to_le_bytes());
        out.extend_from_slice(b"US");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());

        // SamplesPerPixel (0028,0002)
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(b"US");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());

        // BitsAllocated (0028,0100)
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0100u16.to_le_bytes());
        out.extend_from_slice(b"US");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());

        // PhotometricInterpretation (0028,0004)
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0004u16.to_le_bytes());
        out.extend_from_slice(b"CS");
        let pi_bytes = photometric.as_bytes();
        out.extend_from_slice(&(pi_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(pi_bytes);
        if !pi_bytes.len().is_multiple_of(2) {
            out.push(b' ');
        }

        // Optional: Window Center (0028,1050)
        if let Some((center, _width)) = window {
            let center_str = format!("{center}");
            out.extend_from_slice(&0x0028u16.to_le_bytes());
            out.extend_from_slice(&0x1050u16.to_le_bytes());
            out.extend_from_slice(b"DS");
            let cb = center_str.as_bytes();
            out.extend_from_slice(&(cb.len() as u16).to_le_bytes());
            out.extend_from_slice(cb);
            if cb.len() % 2 != 0 {
                out.push(b' ');
            }

            let width_str = format!("{_width}");
            out.extend_from_slice(&0x0028u16.to_le_bytes());
            out.extend_from_slice(&0x1051u16.to_le_bytes());
            out.extend_from_slice(b"DS");
            let wb = width_str.as_bytes();
            out.extend_from_slice(&(wb.len() as u16).to_le_bytes());
            out.extend_from_slice(wb);
            if wb.len() % 2 != 0 {
                out.push(b' ');
            }
        }

        // PixelData (7FE0,0010) - raw LE (not encapsulated)
        let pixel_bytes: Vec<u8> = pixels_16.iter().flat_map(|v| v.to_le_bytes()).collect();
        out.extend_from_slice(&0x7FE0u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"OW");
        out.extend_from_slice(&[0u8, 0]); // reserved
        out.extend_from_slice(&(pixel_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&pixel_bytes);

        out
    }

    // -- Unit tests ----------------------------------------------------

    #[test]
    fn reduce_min_max_monochrome2() {
        // Simple gradient: 0, 100, 1000, 10000 -> mapped to 0..255
        let pixels_16 = vec![0u16, 100, 1000, 10000];
        let w = 2u16;
        let h = 2u16;
        let dicom = build_test_dicom(&pixels_16, w, h, None, "MONOCHROME2");

        let decoded = DecodedPixels {
            data: pixels_16.iter().flat_map(|v| v.to_le_bytes()).collect(),
            rows: h,
            columns: w,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME2".into(),
        };

        let result = reduce_bit_depth_16to8(&decoded, &dicom).unwrap();
        assert_eq!(result.bits_allocated, 8);
        assert_eq!(result.data.len(), 4);

        // Min = 0 -> 0, Max = 10000 -> 255
        assert_eq!(result.data[0], 0); // min
        assert!(result.data[1] < result.data[2]);
        assert!(result.data[2] < result.data[3]);
        assert_eq!(result.data[3], 255); // max
    }

    #[test]
    fn reduce_min_max_monochrome1_inverts() {
        let pixels_16 = vec![0u16, 10000];
        let dicom = build_test_dicom(&pixels_16, 2, 1, None, "MONOCHROME1");

        let decoded = DecodedPixels {
            data: pixels_16.iter().flat_map(|v| v.to_le_bytes()).collect(),
            rows: 1,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME1".into(),
        };

        let result = reduce_bit_depth_16to8(&decoded, &dicom).unwrap();
        // MONOCHROME1: min value = white (255), max value = black (0)
        assert_eq!(result.data[0], 255); // min -> white
        assert_eq!(result.data[1], 0); // max -> black
    }

    #[test]
    fn reduce_with_voi_lut() {
        // Create gradient 0..4095 (typical 12-bit CT stored as 16-bit)
        let w = 64u16;
        let h = 1u16;
        let pc = (w as usize) * (h as usize);
        let pixels_16: Vec<u16> = (0..pc).map(|i| i as u16 * 64).collect(); // 0..4032

        let dicom = build_test_dicom(
            &pixels_16,
            w,
            h,
            Some((2000.0, 1500.0)), // center=2000, width=1500 -> low=1250, high=2750
            "MONOCHROME2",
        );

        let decoded = DecodedPixels {
            data: pixels_16.iter().flat_map(|v| v.to_le_bytes()).collect(),
            rows: h,
            columns: w,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME2".into(),
        };

        let result = reduce_bit_depth_16to8(&decoded, &dicom).unwrap();
        assert_eq!(result.bits_allocated, 8);
        assert_eq!(result.data.len(), pc);

        // Values below low (1250) should be clamped to 0
        let idx_low = 1250 / 64; // index ~ 19
        assert_eq!(result.data[idx_low - 1], 0);

        // Values in the window should be > 0 and < 255
        let idx_mid = ((1250 + 2750) / 2 / 64) as usize;
        let mid_val = result.data[idx_mid];
        assert!(mid_val > 100 && mid_val < 155, "mid_val={mid_val}");

        // Values above high (2750) should be 255
        let idx_high = (2750 / 64 + 1).min(pc - 1);
        assert_eq!(result.data[idx_high], 255);
    }

    #[test]
    fn reduce_uniform_image_mid_gray() {
        let pixels_16 = vec![42u16; 16];
        let dicom = build_test_dicom(&pixels_16, 4, 4, None, "MONOCHROME2");

        let decoded = DecodedPixels {
            data: pixels_16.iter().flat_map(|v| v.to_le_bytes()).collect(),
            rows: 4,
            columns: 4,
            samples_per_pixel: 1,
            bits_allocated: 16,
            photometric_interpretation: "MONOCHROME2".into(),
        };

        let result = reduce_bit_depth_16to8(&decoded, &dicom).unwrap();
        // Uniform image -> all pixels = 128 (mid-gray)
        assert_eq!(result.data, vec![128u8; 16]);
    }

    #[test]
    fn reduce_already_8bit_noop() {
        let data = vec![10u8, 20, 30, 40];
        let dicom = build_test_dicom(&[10, 20, 30, 40], 2, 2, None, "MONOCHROME2");

        let decoded = DecodedPixels {
            data: data.clone(),
            rows: 2,
            columns: 2,
            samples_per_pixel: 1,
            bits_allocated: 8,
            photometric_interpretation: "MONOCHROME2".into(),
        };

        let result = reduce_bit_depth_16to8(&decoded, &dicom).unwrap();
        assert_eq!(result.data, data); // unchanged
    }

    #[test]
    fn extract_window_params_success() {
        let dicom = build_test_dicom(&[100u16, 200], 2, 1, Some((40.0, 400.0)), "MONOCHROME2");
        let (center, width) = extract_window_params(&dicom).unwrap();
        assert_eq!(center, 40.0);
        assert_eq!(width, 400.0);
    }

    #[test]
    fn extract_window_params_missing() {
        let dicom = build_test_dicom(&[100u16, 200], 2, 1, None, "MONOCHROME2");
        assert!(extract_window_params(&dicom).is_none());
    }

    /// PS3.5 6.2 / 7.1: the element grammar is Implicit VR or Explicit VR and
    /// the byte order follows the transfer syntax. The reader assumed Explicit
    /// VR Little Endian, so an Implicit VR file walked two bytes out at every
    /// element and an Explicit VR Big Endian file read its numbers reversed -
    /// Window Center/Width either went missing or came back meaningless.
    ///
    /// Driven by the real fixtures, whose transfer syntax is in the file, so
    /// the test exercises the same detection the production path uses.
    #[test]
    fn every_grammar_reads_the_same_numbers() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/dicoms/edge");
        let read = |name: &str| std::fs::read(format!("{dir}/{name}")).expect("fixture");

        let be = read("gen_explicit_vr_be.dcm");
        let implicit = read("gen_implicit_vr_le.dcm");

        // Each fixture declares its own syntax, and the reader must agree.
        assert!(grammar_of(&be).big_endian(), "BE fixture detected as BE");
        assert!(!grammar_of(&implicit).big_endian());
        assert!(
            grammar_of(&implicit).implicit(),
            "implicit fixture detected as implicit"
        );

        // Rows is US in both files and physically 64. Reading the BE bytes as
        // LE yields 16384, which is exactly the corruption this pins down.
        assert_eq!(extract_first_us(&be, (0x0028, 0x0010)), Some(64), "BE Rows");
        assert_eq!(
            extract_first_us(&implicit, (0x0028, 0x0010)),
            Some(64),
            "implicit Rows"
        );
        assert_eq!(extract_first_us(&be, (0x0028, 0x0100)), Some(16), "BE Bits");
        assert_eq!(
            extract_first_us(&implicit, (0x0028, 0x0100)),
            Some(16),
            "implicit Bits"
        );
    }
}
