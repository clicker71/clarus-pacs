// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Pixel bytes and their geometry.
//!
//! The fetch contract asks the origin for explicit VR little endian, so
//! the pixel data arrives already in the form this module reads: a byte
//! range to borrow and a set of geometry attributes. Nothing here decodes
//! anything, and nothing here allocates - the buffer is borrowed from the
//! fetched instance and travels as a slice.
//!
//! Attributes are read straight from the instance header by tag rather
//! than through the parser's typed metadata, because the parser's
//! `InstanceMeta` carries Rows and Columns but not bits, samples or
//! photometric interpretation, and widening that struct for one consumer
//! would make it a different thing.

use std::fmt;

/// The failure shapes the pixel-geometry reader names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// One instance's header is missing something the caller needs.
    Instance(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Instance(message) => f.write_str(message),
        }
    }
}

// Geometry tags, PS3.3 C.7.6.3.
const TAG_SAMPLES_PER_PIXEL: (u16, u16) = (0x0028, 0x0002);
const TAG_PHOTOMETRIC_INTERPRETATION: (u16, u16) = (0x0028, 0x0004);
const TAG_ROWS: (u16, u16) = (0x0028, 0x0010);
const TAG_COLUMNS: (u16, u16) = (0x0028, 0x0011);
const TAG_BITS_ALLOCATED: (u16, u16) = (0x0028, 0x0100);
const TAG_BITS_STORED: (u16, u16) = (0x0028, 0x0101);
const TAG_PIXEL_REPRESENTATION: (u16, u16) = (0x0028, 0x0103);
const TAG_NUMBER_OF_FRAMES: (u16, u16) = (0x0028, 0x0008);
const TAG_SOP_CLASS_UID: (u16, u16) = (0x0008, 0x0016);
const TAG_MODALITY: (u16, u16) = (0x0008, 0x0060);
/// (0020,0052) Frame of Reference UID: the coordinate system the image
/// lives in. A segmentation has to share its source's (PS3.3 A.51.1), so
/// the frames path reads it here rather than making the encoder guess.
const TAG_FRAME_OF_REFERENCE_UID: (u16, u16) = (0x0020, 0x0052);
/// (0028,0030) Pixel Spacing. A DS pair with no binary form, so it is read
/// as text and parsed.
const TAG_PIXEL_SPACING: (u16, u16) = (0x0028, 0x0030);
/// (0020,0032) Image Position Patient: where the image sits in the frame
/// of reference, again a DS triple read as text.
const TAG_IMAGE_POSITION_PATIENT: (u16, u16) = (0x0020, 0x0032);
/// The Image Orientation Patient tag (0020,0037).
const TAG_IMAGE_ORIENTATION_PATIENT: (u16, u16) = (0x0020, 0x0037);
/// (0018,0088) Spacing Between Slices.
const TAG_SPACING_BETWEEN_SLICES: (u16, u16) = (0x0018, 0x0088);
/// (0002,0010) Transfer Syntax UID, from the file meta group.
const TAG_TRANSFER_SYNTAX_UID: (u16, u16) = (0x0002, 0x0010);

/// How a frame's samples are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleWidth {
    /// 8 bits per sample, unsigned.
    U8,
    /// 16 bits per sample.
    U16,
}

impl SampleWidth {
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
        }
    }
}

/// A frame's pixels, borrowed from the fetched instance.
///
/// The borrow is the point: the pixel stage hands the same bytes to resize
/// without a copy, which is what makes its zero-allocation gate honest
/// rather than aspirational.
#[derive(Debug, Clone, Copy)]
pub struct Pixels<'a> {
    bytes: &'a [u8],
    pub rows: u32,
    pub cols: u32,
    pub samples: u32,
    pub width: SampleWidth,
    pub bits_stored: u8,
    /// True for MONOCHROME1, where higher stored values are darker.
    pub inverted: bool,
    /// True when the samples are signed.
    pub signed: bool,
}

impl<'a> Pixels<'a> {
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The sample at a flat index, widened to `i32`.
    ///
    /// Returning a signed value in one signed type is what keeps the later
    /// stages from branching on the width: the window and the float
    /// conversion both work on `i32`, and the sign extension happens once,
    /// here, where the width is known.
    #[must_use]
    pub fn sample(&self, index: usize) -> Option<i32> {
        match self.width {
            SampleWidth::U8 => {
                let byte = *self.bytes.get(index)?;
                Some(if self.signed {
                    i32::from(byte as i8)
                } else {
                    i32::from(byte)
                })
            }
            SampleWidth::U16 => {
                let low = *self.bytes.get(index * 2)?;
                let high = *self.bytes.get(index * 2 + 1)?;
                let raw = u16::from_le_bytes([low, high]);
                Some(if self.signed {
                    i32::from(raw as i16)
                } else {
                    i32::from(raw)
                })
            }
        }
    }

    /// A grayscale frame's sample at `(row, column)`.
    #[must_use]
    pub fn gray(&self, row: u32, col: u32) -> Option<i32> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.sample((row * self.cols + col) as usize)
    }

    /// The number of samples the geometry implies.
    #[must_use]
    pub fn expected_samples(&self) -> usize {
        (self.rows as usize) * (self.cols as usize) * (self.samples as usize)
    }

    /// Whether the buffer holds at least the samples the geometry needs.
    ///
    /// A short buffer is a malformed instance, and catching it here means
    /// the resize stage never has to wonder whether a read is in range.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.len() >= self.expected_samples() * self.width.bytes()
    }
}

/// The shape of a decoded instance, before any pixels are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub rows: u32,
    pub cols: u32,
    pub samples: u32,
    pub bits_allocated: u16,
    pub bits_stored: u8,
    pub frames: u32,
    pub inverted: bool,
    pub signed: bool,
}

/// Read the geometry attributes out of an instance header.
///
/// Only the tags present are used; the ones with a single legal value are
/// defaulted rather than required, because a device that omits
/// SamplesPerPixel means one sample per pixel.
pub fn geometry(data: &[u8]) -> Result<Geometry, Error> {
    let rows =
        number(data, TAG_ROWS).ok_or_else(|| Error::Instance("no Rows (0028,0010)".to_string()))?;
    let cols = number(data, TAG_COLUMNS)
        .ok_or_else(|| Error::Instance("no Columns (0028,0011)".to_string()))?;
    if rows == 0 || cols == 0 {
        return Err(Error::Instance("Rows or Columns is zero".to_string()));
    }
    let bits_allocated = number(data, TAG_BITS_ALLOCATED)
        .ok_or_else(|| Error::Instance("no BitsAllocated (0028,0100)".to_string()))?;
    let bits_allocated = u16::try_from(bits_allocated).map_err(|_| {
        Error::Instance(format!(
            "BitsAllocated {bits_allocated} does not fit 16 bits"
        ))
    })?;
    let bits_stored = match number(data, TAG_BITS_STORED) {
        Some(value) => u8::try_from(value)
            .map_err(|_| Error::Instance(format!("BitsStored {value} does not fit 8 bits")))?,
        // Absent is not the same as wrong. A device that omits (0028,0101)
        // means the usual value for the allocation size; one that states a
        // value this reader cannot parse has stated something it is unsafe to
        // guess at, because the guess silently changes how every pixel of the
        // instance is interpreted.
        None if find_tag(data, TAG_BITS_STORED).is_some() => {
            return Err(Error::Instance(
                "malformed BitsStored (0028,0101)".to_string(),
            ));
        }
        None => bits_allocated.min(16) as u8,
    };

    let samples = number(data, TAG_SAMPLES_PER_PIXEL).unwrap_or(1);
    let samples = u32::try_from(samples)
        .map_err(|_| Error::Instance(format!("SamplesPerPixel {samples} is out of range")))?;
    let frames = number(data, TAG_NUMBER_OF_FRAMES).unwrap_or(1);
    let frames = u32::try_from(frames)
        .map_err(|_| Error::Instance(format!("NumberOfFrames {frames} is out of range")))?;

    let photometric = text(data, TAG_PHOTOMETRIC_INTERPRETATION).unwrap_or_default();
    // MONOCHROME1 inverts the display convention: the higher a stored
    // value, the darker the pixel. A model trained on MONOCHROME2 data
    // sees the negative of the image unless this is corrected.
    let inverted = photometric.trim().eq_ignore_ascii_case("MONOCHROME1");
    // (0028,0103) tells the reader whether the stored values are two's
    // complement. Deferring to unsigned when the attribute is PRESENT but
    // unparseable turns a signed instance into an unsigned one, and every
    // negative value then reads as a large positive one - a defect that
    // produces numbers rather than an error, so nothing downstream notices.
    let signed = match number(data, TAG_PIXEL_REPRESENTATION) {
        Some(value) => value == 1,
        None if find_tag(data, TAG_PIXEL_REPRESENTATION).is_some() => {
            return Err(Error::Instance(
                "malformed PixelRepresentation (0028,0103)".to_string(),
            ));
        }
        // Absent: PS3.3 C.7.6.3 gives the attribute a default of 0.
        None => false,
    };

    Ok(Geometry {
        rows: u32::try_from(rows)
            .map_err(|_| Error::Instance(format!("Rows {rows} is out of range")))?,
        cols: u32::try_from(cols)
            .map_err(|_| Error::Instance(format!("Columns {cols} is out of range")))?,
        samples,
        bits_allocated,
        bits_stored,
        frames,
        inverted,
        signed,
    })
}

/// The instance's SOP Class UID (0008,0016).
///
/// The result object's (0008,1150) Referenced SOP Class UID needs it:
/// Type 1 in the reference item (PS3.3 C.7.6.1), carried by the frozen
/// Python reference, and what a viewer like Weasis binds the annotation
/// to an image by.
pub fn sop_class_uid(data: &[u8]) -> Option<String> {
    uid_text(data, TAG_SOP_CLASS_UID)
}

/// (0028,0004) Photometric Interpretation, as the instance states it.
///
/// A derived image copies this from its source: a processor that returns
/// pixels of the same shape produces an image of the same kind, and
/// writing a different value would tell a viewer to interpret the output
/// under a convention it was not produced in.
///
/// `None` means the attribute is absent, which is legal for a single
/// MONOCHROME2 image; the caller decides what to do about it rather than
/// this function guessing on its behalf.
#[must_use]
pub fn photometric_interpretation(data: &[u8]) -> Option<String> {
    text(data, TAG_PHOTOMETRIC_INTERPRETATION)
}

/// (0008,0060) Modality, as the instance states it.
///
/// A derived image carries the modality of the image it replaces: the
/// processor changed the pixels, not the kind of examination they came
/// from, and a derived CT image that declared itself `OT` would drop out
/// of every modality-filtered series list.
#[must_use]
pub fn modality(data: &[u8]) -> Option<String> {
    text(data, TAG_MODALITY)
}

/// The instance's Frame of Reference UID (0020,0052), when it declares one.
///
/// Optional by design: a single-frame DX image commonly carries none, and
/// a segmentation of it then carries none either - writing an invented
/// value would claim the masks share a coordinate system with nothing.
#[must_use]
pub fn frame_of_reference_uid(data: &[u8]) -> Option<String> {
    uid_text(data, TAG_FRAME_OF_REFERENCE_UID)
}

/// The instance's Pixel Spacing (0028,0030), when it declares two usable
/// numbers: `[row, column]`, the order the standard defines.
///
/// A segmentation copies this so a reader can MEASURE a mask instead of
/// only counting its pixels, and a value with a non-finite or unparsable
/// component is treated as absent rather than propagated: a spacing of
/// `NaN` would travel into `Pixel Measures` as text no reader can use.
#[must_use]
pub fn pixel_spacing(data: &[u8]) -> Option<[f64; 2]> {
    let value = text(data, TAG_PIXEL_SPACING)?;
    let mut parts = value.split('\\');
    let row = parts.next()?.trim().parse::<f64>().ok()?;
    let column = parts.next()?.trim().parse::<f64>().ok()?;
    if !row.is_finite() || !column.is_finite() {
        return None;
    }
    Some([row, column])
}

/// The instance's Image Position Patient (0020,0032), when it declares
/// three finite numbers.
///
/// This is what places a frame in the frame of reference, so a mask frame
/// copies its source's position rather than being left unplaced.
#[must_use]
pub fn image_position_patient(data: &[u8]) -> Option<[f64; 3]> {
    let value = text(data, TAG_IMAGE_POSITION_PATIENT)?;
    let mut parts = value.split('\\');
    let mut position = [0f64; 3];
    for component in &mut position {
        *component = parts.next()?.trim().parse::<f64>().ok()?;
        if !component.is_finite() {
            return None;
        }
    }
    Some(position)
}

/// The instance's Image Orientation Patient (0020,0037), when it declares
/// six finite direction cosines: row direction then column direction.
///
/// A segmentation copies this into the shared functional group's Plane
/// Orientation (Patient) so a reader can ROTATE the mask onto the source's
/// image plane; without it the mask has a position but no rotation, and a
/// reader cannot align the bit plane with the pixels it covers.
#[must_use]
pub fn image_orientation_patient(data: &[u8]) -> Option<[f64; 6]> {
    let value = text(data, TAG_IMAGE_ORIENTATION_PATIENT)?;
    let mut parts = value.split('\\');
    let mut orientation = [0f64; 6];
    for component in &mut orientation {
        *component = parts.next()?.trim().parse::<f64>().ok()?;
        if !component.is_finite() {
            return None;
        }
    }
    Some(orientation)
}

/// The instance's Spacing Between Slices (0018,0088), when it declares a
/// usable number.
#[must_use]
pub fn spacing_between_slices(data: &[u8]) -> Option<f64> {
    let value = text(data, TAG_SPACING_BETWEEN_SLICES)?
        .trim()
        .parse::<f64>()
        .ok()?;
    if value.is_finite() { Some(value) } else { None }
}

/// (0002,0010) Transfer Syntax UID, borrowed from the instance.
///
/// This is what names how the rest of the dataset is encoded, and it is
/// the only way to tell an uncompressed instance from a compressed one
/// when the transport did not say. The value is returned as a borrowed
/// slice rather than a `String`: the caller is reading it to compare it
/// against a known UID, so owning a copy would be an allocation on a path
/// that only needs to look at bytes it already holds.
///
/// `None` means the attribute is absent or unreadable, which is itself a
/// fact the caller acts on - an instance whose encoding cannot be
/// established must not be read as if it were uncompressed.
#[must_use]
pub fn transfer_syntax_uid(data: &[u8]) -> Option<&[u8]> {
    let offset = find_tag(data, TAG_TRANSFER_SYNTAX_UID)?;
    let (_, value_offset, length) = element_header(data, offset)?;
    // UI is a short VR and carries no sentinel, so an undefined length here
    // would mean the header was not a UI header; refusing is the honest
    // answer, because a transfer syntax borrowed from a guess is worse than
    // an absent one the caller has to handle.
    let Length::Defined(length) = length else {
        return None;
    };
    let bytes = data.get(value_offset..value_offset.checked_add(length)?)?;
    // UI pads with NUL (PS3.5 6.2): trim the padding and any stray space,
    // which is what some writers emit instead.
    let end = bytes
        .iter()
        .position(|byte| *byte == 0 || *byte == b' ')
        .unwrap_or(bytes.len());
    let trimmed = &bytes[..end];
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Borrow the first frame's pixel bytes out of an instance.
///
/// The whole pixel element is a borrow; a multi-frame instance is refused
/// rather than silently taking frame zero, because a study that should
/// have been one image and is not is a fact the caller needs to know.
pub fn frame<'a>(data: &'a [u8], geometry: &Geometry) -> Result<Pixels<'a>, Error> {
    if geometry.frames > 1 {
        return Err(Error::Instance(format!(
            "{} frames; this engine reads single-frame images",
            geometry.frames
        )));
    }
    if geometry.samples != 1 {
        return Err(Error::Instance(format!(
            "{} samples per pixel; this engine reads monochrome images",
            geometry.samples
        )));
    }
    let width = match geometry.bits_allocated {
        8 => SampleWidth::U8,
        16 => SampleWidth::U16,
        other => {
            return Err(Error::Instance(format!(
                "BitsAllocated {other}; this engine reads 8 or 16 bit samples"
            )));
        }
    };

    let bytes = clarus_adapters::codec::pixel_data::extract_pixel_data(data)
        .ok_or_else(|| Error::Instance("no PixelData (7FE0,0010)".to_string()))?;

    let pixels = Pixels {
        bytes,
        rows: geometry.rows,
        cols: geometry.cols,
        samples: geometry.samples,
        width,
        bits_stored: geometry.bits_stored,
        inverted: geometry.inverted,
        signed: geometry.signed,
    };
    if !pixels.is_complete() {
        return Err(Error::Instance(format!(
            "pixel data is {} bytes; the geometry needs {}",
            pixels.len(),
            pixels.expected_samples() * width.bytes()
        )));
    }
    Ok(pixels)
}

/// Read one integer attribute.
///
/// The VR decides how the value is encoded, not its length. An integer
/// attribute in the field is US, SS, UL or SL (binary) or IS (text, often
/// padded with a trailing space); dispatching on length alone read the text
/// form as binary - `"1 "` is two bytes, and two bytes of `0x31 0x20` are
/// the number 8241, so a padded IS silently became a coordinate nobody
/// entered (PS3.5 6.2, Table 6.2-1: the VR is the encoding, and IS is a
/// character string).
///
/// A binary VR whose length is not the one its VR prescribes is not read as
/// either form: the value is malformed, and guessing would place a landmark
/// in the wrong place.
fn number(data: &[u8], tag: (u16, u16)) -> Option<i64> {
    let offset = find_tag(data, tag)?;
    let (vr, value_offset, length) = element_header(data, offset)?;
    let Length::Defined(length) = length else {
        return None;
    };
    match vr.as_str() {
        // Unsigned/signed short: two bytes little endian.
        "US" => {
            if length != 2 {
                return None;
            }
            let low = *data.get(value_offset)?;
            let high = *data.get(value_offset + 1)?;
            Some(i64::from(u16::from_le_bytes([low, high])))
        }
        "SS" => {
            if length != 2 {
                return None;
            }
            let low = *data.get(value_offset)?;
            let high = *data.get(value_offset + 1)?;
            Some(i64::from(i16::from_le_bytes([low, high])))
        }
        // Unsigned/signed long: four bytes little endian.
        "UL" => {
            if length != 4 {
                return None;
            }
            let bytes = data.get(value_offset..value_offset + 4)?;
            let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            Some(i64::from(raw))
        }
        "SL" => {
            if length != 4 {
                return None;
            }
            let bytes = data.get(value_offset..value_offset + 4)?;
            let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            Some(i64::from(raw))
        }
        // IS and every other VR: the value is text.
        _ => {
            let bytes = data.get(value_offset..value_offset.checked_add(length)?)?;
            let text = String::from_utf8_lossy(bytes);
            text.trim().parse::<i64>().ok()
        }
    }
}

/// Read one text attribute.
fn text(data: &[u8], tag: (u16, u16)) -> Option<String> {
    let offset = find_tag(data, tag)?;
    let (_, value_offset, length) = element_header(data, offset)?;
    let Length::Defined(length) = length else {
        return None;
    };
    let bytes = data.get(value_offset..value_offset.checked_add(length)?)?;
    Some(String::from_utf8_lossy(bytes).trim().to_string())
}

/// Read a UID attribute, stripping the padding its VR prescribes.
///
/// UI is the one string VR padded with NUL, every other pads with SPACE
/// (PS3.5 6.2), and `str::trim` removes whitespace but not NUL. Reading a
/// UID through [`text`] therefore returns an odd-length UID with its pad
/// byte attached - `"1.2.840.10008.5.1.4.1.1.7\0"` - which compares unequal
/// to the same UID written out, so an identity check against a constant
/// silently never matches.
///
/// The general `text` reader keeps its space trimming: for CS, LO, PN and
/// the rest that is exactly right, and widening it to NUL would strip a
/// character those VRs are entitled to carry.
fn uid_text(data: &[u8], tag: (u16, u16)) -> Option<String> {
    let offset = find_tag(data, tag)?;
    let (_, value_offset, length) = element_header(data, offset)?;
    let Length::Defined(length) = length else {
        return None;
    };
    let bytes = data.get(value_offset..value_offset.checked_add(length)?)?;
    // Trim the pad first, then space: a writer that pads a UID with SPACE is
    // non-conformant but occurs, and a second pad byte is not the UID.
    let end = bytes
        .iter()
        .position(|byte| *byte == 0 || *byte == b' ')
        .unwrap_or(bytes.len());
    let trimmed = &bytes[..end];
    // A zero-length value is an absent one here rather than an empty UID: a
    // UID of no characters identifies nothing, and the caller acts on the
    // same absence either way - which is what `transfer_syntax_uid` does
    // with the identical rule.
    if trimmed.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(trimmed).to_string())
    }
}

/// The byte offset of an element's header, or `None`.
///
/// The scan is top-level and flat, but it still has to SKIP correctly past
/// an element it does not want, and one spelling of an element carries no
/// length to skip by: an undefined length (`FFFFFFFF`, PS3.5 7.5.1) leaves
/// the end of the element to a delimiter further on. The pre-1993 encoding
/// rule for those is "read until the delimiter" rather than "read a length
/// and jump", so the walk steps past the header and continues INTO the
/// content, where the delimiters are the only ones that end it.
fn find_tag(data: &[u8], tag: (u16, u16)) -> Option<usize> {
    let mut offset = 132; // past the preamble and the "DICM" magic
    while offset + 8 <= data.len() {
        let group = u16::from_le_bytes([data[offset], data[offset + 1]]);
        let element = u16::from_le_bytes([data[offset + 2], data[offset + 3]]);
        if group == 0xFFFE {
            // A delimiter is not an attribute and has no VR: its "length" is
            // the four bytes after the tag (PS3.5 7.5). Every one of them
            // ends a structure whose start was already skipped, so the walk
            // consumes the eight-byte header and reads on.
            offset += 8;
            continue;
        }
        let (_, value_offset, length) = element_header(data, offset)?;
        if (group, element) == tag {
            return Some(offset);
        }
        // A sequence is not walked for its own sake: the attributes this
        // module reads are all top-level, and skipping a nested structure by
        // its declared length is cheaper than descending into one. An
        // undefined length has no such number, and its items do: the walk
        // enters the item stream and leaves it at (FFFE,E0DD).
        let advance = match length {
            Length::Defined(bytes) => value_offset + bytes - offset,
            Length::Undefined => value_offset - offset,
        };
        if advance == 0 {
            return None;
        }
        offset += advance;
    }
    None
}

/// An element's length, as PS3.5 7.5.1 defines it.
///
/// The sentinel is not an extreme length but the ABSENCE of one, and
/// treating it as a number is how a reader ends up 65535 bytes into a
/// structure it thinks it has skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Length {
    Defined(usize),
    Undefined,
}

impl Length {
    /// The undefined-length sentinel, `FFFFFFFF` (PS3.5 7.5.1).
    const SENTINEL: usize = 0xFFFF_FFFF;
}

/// Split an element header into `(vr, value offset, length)`.
///
/// The size of the length field is the VR's, not the caller's: a VR absent
/// from the long list has a plain 16-bit length with no reserved field, and
/// reading a 32-bit length where a 16-bit one belongs puts the value offset
/// four bytes off (PS3.5 Table 7.1-1, and its complement 7.1-2).
fn element_header(data: &[u8], offset: usize) -> Option<(String, usize, Length)> {
    let vr = String::from_utf8_lossy(data.get(offset + 4..offset + 6)?).into_owned();
    // The VRs with a 32-bit length use a two-byte reserved field first.
    let long = matches!(
        vr.as_str(),
        "OB" | "OW" | "OF" | "SQ" | "UT" | "UN" | "OD" | "OL" | "UC" | "UR" | "OV" | "SV" | "UV"
    );
    if long {
        let bytes = data.get(offset + 8..offset + 12)?;
        let length = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        // A long VR's length field is the only span that can reach the
        // sentinel: a 16-bit length cannot express it, and 65535 is a real
        // value length there.
        let length = match usize::try_from(length) {
            Ok(bytes) if bytes == Length::SENTINEL => Length::Undefined,
            Ok(bytes) => Length::Defined(bytes),
            Err(_) => return None,
        };
        Some((vr, offset + 12, length))
    } else {
        let bytes = data.get(offset + 6..offset + 8)?;
        let length = u16::from_le_bytes([bytes[0], bytes[1]]);
        Some((vr, offset + 8, Length::Defined(usize::from(length))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal explicit VR little endian instance with the given
    /// attributes, so the reader is exercised on bytes rather than on a
    /// struct a test constructed.
    pub(crate) fn instance(attrs: &[(&str, u16, u16, Vec<u8>)], pixels: Option<&[u8]>) -> Vec<u8> {
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        for (vr, group, element, value) in attrs {
            out.extend_from_slice(&group.to_le_bytes());
            out.extend_from_slice(&element.to_le_bytes());
            let long = matches!(
                *vr,
                "OB" | "OW" | "OF" | "SQ" | "UT" | "UN" | "OD" | "OL" | "UC" | "UR"
            );
            out.extend_from_slice(vr.as_bytes());
            if long {
                out.extend_from_slice(&[0, 0]);
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            } else {
                // A short-VR header carries a 16-bit length, so a fixture
                // value beyond it would be written with a wrapped length -
                // the same defect the serializer's own header writer
                // refuses. A test that silently built such a file would
                // pin behaviour on bytes no writer should produce.
                assert!(
                    value.len() <= crate::pr::SHORT_VR_MAX,
                    "fixture value of {} bytes does not fit a short VR",
                    value.len()
                );
                out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            }
            out.extend_from_slice(value);
        }
        if let Some(pixels) = pixels {
            out.extend_from_slice(&0x7FE0u16.to_le_bytes());
            out.extend_from_slice(&0x0010u16.to_le_bytes());
            out.extend_from_slice(b"OW");
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
            out.extend_from_slice(pixels);
        }
        out
    }

    fn us(value: u16) -> Vec<u8> {
        value.to_le_bytes().to_vec()
    }

    /// One undefined-length sequence holding a single item, as the derived
    /// encoder writes (0008,2112): an `SQ` header whose length is the
    /// sentinel, then an item that is itself undefined-length, then the
    /// item's own elements, then the two delimiters that close it.
    ///
    /// Written out byte by byte rather than through the serializer, because
    /// the reader is the subject: a fixture built by the code under test
    /// would agree with it by construction.
    fn undefined_length_sequence(
        group: u16,
        element: u16,
        item_attrs: &[(&str, u16, u16, Vec<u8>)],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&element.to_le_bytes());
        out.extend_from_slice(b"SQ");
        out.extend_from_slice(&[0, 0]); // reserved, then the 32-bit length
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // PS3.5 7.5.1
        // (FFFE,E000) item with an undefined length of its own.
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0xE000u16.to_le_bytes());
        out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        for (vr, item_group, item_element, value) in item_attrs {
            out.extend_from_slice(&item_group.to_le_bytes());
            out.extend_from_slice(&item_element.to_le_bytes());
            out.extend_from_slice(vr.as_bytes());
            let long = matches!(*vr, "OB" | "OW" | "OF" | "SQ" | "UT" | "UN");
            if long {
                out.extend_from_slice(&[0, 0]);
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            } else {
                out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            }
            out.extend_from_slice(value);
        }
        // (FFFE,E00D) item delimiter, then (FFFE,E0DD) sequence delimiter.
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0xE00Du16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0xE0DDu16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    /// A derived instance, byte for byte as `derived.rs` writes one: the
    /// undefined-length (0008,2112) Source Image Sequence sits between the
    /// instance identity and the pixel geometry, so the geometry tags the
    /// reader wants come AFTER the sequence it has to skip.
    fn derived_shaped_instance(rows: u16, cols: u16) -> Vec<u8> {
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        // The identity, in the tag order `derived.rs` writes it. The UID is
        // padded to an even length with NUL, the rule `write_string`
        // applies: an odd length would make every later offset odd and the
        // fixture would stop resembling a real file.
        for (vr, group, element, value) in [
            ("UI", 0x0008u16, 0x0016u16, "1.2.840.10008.5.1.4.1.1.7"),
            ("CS", 0x0008, 0x0060, "DX"),
        ] {
            let mut bytes = value.as_bytes().to_vec();
            if !bytes.len().is_multiple_of(2) {
                bytes.push(b'\0');
            }
            out.extend_from_slice(&group.to_le_bytes());
            out.extend_from_slice(&element.to_le_bytes());
            out.extend_from_slice(vr.as_bytes());
            out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(&bytes);
        }
        out.extend_from_slice(&undefined_length_sequence(
            0x0008,
            0x2112,
            &[
                ("UI", 0x0008, 0x1150, b"1.2.840.10008.5.1.4.1.1.1".to_vec()),
                ("UI", 0x0008, 0x1155, b"1.2.3.4.9".to_vec()),
            ],
        ));
        // The geometry the reader is looking for, written AFTER the
        // sequence - if the skip lands anywhere but the delimiter, these
        // four bytes are never reached.
        for (vr, group, element, value) in geometry_attrs(rows, cols, 16) {
            out.extend_from_slice(&group.to_le_bytes());
            out.extend_from_slice(&element.to_le_bytes());
            out.extend_from_slice(vr.as_bytes());
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.extend_from_slice(&value);
        }
        out
    }

    #[test]
    fn geometry_is_read_past_an_undefined_length_sequence() {
        // The defect this pins: the sentinel clamped to 65535 put the walk
        // 65535 bytes into the file, from where it read item contents as
        // top-level elements and reported "no Rows" for an instance whose
        // Rows is right there. clinfer's own derived output carries an
        // undefined-length (0008,2112), so this is the second pass over a
        // study the first pass wrote.
        let data = derived_shaped_instance(1024, 1024);
        let g = geometry(&data).expect("geometry past the undefined-length sequence");
        assert_eq!(g.rows, 1024);
        assert_eq!(g.cols, 1024);
        assert_eq!(g.bits_allocated, 16);
        assert_eq!(g.samples, 1);
    }

    #[test]
    fn the_tags_after_an_undefined_length_sequence_are_all_reachable() {
        // Not only Rows: every reader that walks the dataset has to get past
        // the same sequence, and a walk that resynchronises by luck would
        // leave some of them behind.
        let data = derived_shaped_instance(512, 640);
        // The identity, before the sequence. The UID is returned WITHOUT the
        // NUL pad the writer added to make its odd length even (PS3.5 6.2):
        // the reader strips the padding, so the value is the UID itself and
        // compares equal to the same UID spelled out anywhere else.
        assert_eq!(
            sop_class_uid(&data).as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.7"),
            "the identity before the sequence"
        );
        assert_eq!(
            modality(&data).as_deref(),
            Some("DX"),
            "also before the sequence"
        );
        assert_eq!(
            pixel_spacing(&data),
            None,
            "absent, and absent is not a miss"
        );
        assert_eq!(
            number(&data, TAG_ROWS),
            Some(512),
            "the Rows after the sequence"
        );
        assert_eq!(
            number(&data, TAG_COLUMNS),
            Some(640),
            "the Columns after the sequence"
        );
        assert_eq!(
            number(&data, TAG_BITS_STORED),
            Some(12),
            "BitsStored after the sequence"
        );
    }

    /// An instance declaring the tags whose padding rules differ: a UID of
    /// odd length that must be padded with NUL, and a CS of odd length that
    /// must be padded with SPACE (PS3.5 6.2).
    fn instance_with_padded_strings() -> Vec<u8> {
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        // 27 characters, so odd, so one NUL pad -> 28 bytes.
        let uid = b"1.2.840.10008.5.1.4.1.1.7";
        assert!(!uid.len().is_multiple_of(2), "the fixture UID must be odd");
        let mut padded_uid = uid.to_vec();
        padded_uid.push(0);
        // 5 characters, so odd, so one SPACE pad -> 6 bytes.
        let mut padded_cs = b"DERIV".to_vec();
        padded_cs.push(b' ');
        for (vr, group, element, value) in [
            ("UI", 0x0008u16, 0x0016u16, padded_uid.as_slice()),
            ("CS", 0x0008, 0x0008, padded_cs.as_slice()),
        ] {
            out.extend_from_slice(&group.to_le_bytes());
            out.extend_from_slice(&element.to_le_bytes());
            out.extend_from_slice(vr.as_bytes());
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.extend_from_slice(value);
        }
        out
    }

    #[test]
    fn a_uid_is_returned_without_the_nul_pad_its_odd_length_asked_for() {
        // The latent mine this closes: `text` trims whitespace and not NUL,
        // so an odd-length UID came back with its pad byte attached and
        // compared unequal to the same UID written out. Nothing compares a
        // UID against a constant today, which is the only reason this had
        // not surfaced.
        let data = instance_with_padded_strings();
        assert_eq!(
            sop_class_uid(&data).as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.7"),
            "the pad byte is the writer's, not part of the UID"
        );
        assert_eq!(
            sop_class_uid(&data).as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.7"),
            "and it compares equal to the same UID spelled out"
        );
    }

    #[test]
    fn a_uid_reader_strips_a_space_pad_too() {
        // A writer that pads a UID with SPACE is non-conformant, and it
        // occurs. The UID is what precedes the pad, whichever byte it is.
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        let mut value = b"1.2.840.10008.5.1.4.1.1.7".to_vec();
        value.push(b' ');
        out.extend_from_slice(&0x0008u16.to_le_bytes());
        out.extend_from_slice(&0x0016u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.extend_from_slice(&value);
        assert_eq!(
            sop_class_uid(&out).as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.7")
        );
    }

    #[test]
    fn the_general_text_reader_keeps_its_space_trim() {
        // The two readers exist because the rules differ: CS, LO and PN are
        // SPACE-padded (PS3.5 6.2), so `text` must go on trimming SPACE, and
        // it is deliberately NOT given the UID reader's NUL rule.
        let data = instance_with_padded_strings();
        assert_eq!(
            text(&data, (0x0008, 0x0008)).as_deref(),
            Some("DERIV"),
            "the SPACE pad is trimmed"
        );
    }

    #[test]
    fn an_empty_uid_reads_as_absent_rather_than_as_an_empty_string() {
        // A UID of no characters identifies nothing, so it is reported the
        // same way an absent one is - which is what every caller already
        // does with `None`.
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        out.extend_from_slice(&0x0008u16.to_le_bytes());
        out.extend_from_slice(&0x0016u16.to_le_bytes());
        out.extend_from_slice(b"UI");
        out.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(sop_class_uid(&out), None);
    }

    #[test]
    fn a_defined_length_sequence_is_still_skipped_by_its_length() {
        // The fix must not turn every sequence into a walk: a defined length
        // is still the cheaper skip, and a walk that descends into a defined
        // sequence would be reading a length the writer already promised.
        // The item here says its value is 8 bytes, and the sequence says its
        // value is the whole item, so the walk must land exactly on the
        // element that follows - a byte either way and Rows is missed.
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        out.extend_from_slice(&0x0008u16.to_le_bytes());
        out.extend_from_slice(&0x2112u16.to_le_bytes());
        out.extend_from_slice(b"SQ");
        out.extend_from_slice(&[0, 0]);
        // The sequence's own value: an 8-byte item header plus 8 bytes of
        // item value = 16, so the element ends 16 bytes after the value
        // begins and never needs a delimiter.
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&0xFFFEu16.to_le_bytes());
        out.extend_from_slice(&0xE000u16.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(b"1.2.3.4\0");
        for (vr, group, element, value) in geometry_attrs(3, 9, 16) {
            out.extend_from_slice(&group.to_le_bytes());
            out.extend_from_slice(&element.to_le_bytes());
            out.extend_from_slice(vr.as_bytes());
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
            out.extend_from_slice(&value);
        }
        assert_eq!(number(&out, TAG_ROWS), Some(3));
        assert_eq!(number(&out, TAG_COLUMNS), Some(9));
    }

    /// A short-VR element and its neighbour, so a reader that gets the
    /// length width wrong for one of them is caught.
    #[test]
    fn the_length_width_follows_the_vr_and_not_the_element() {
        // UL is a SHORT VR (PS3.5 Table 7.1-2 lists it), unlike OB/OW/UN:
        // reading four length bytes where two belong puts the value offset
        // four bytes off, and the next element is read from the middle of a
        // value.
        let mut out = vec![0u8; 128];
        out.extend_from_slice(b"DICM");
        // (0002,0000) UL, a short VR whose value happens to be four bytes.
        out.extend_from_slice(&0x0002u16.to_le_bytes());
        out.extend_from_slice(&0x0000u16.to_le_bytes());
        out.extend_from_slice(b"UL");
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        // Then the attribute the reader wants.
        out.extend_from_slice(&0x0028u16.to_le_bytes());
        out.extend_from_slice(&0x0010u16.to_le_bytes());
        out.extend_from_slice(b"US");
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&7u16.to_le_bytes());
        assert_eq!(number(&out, TAG_ROWS), Some(7));
    }

    fn geometry_attrs(rows: u16, cols: u16, bits: u16) -> Vec<(&'static str, u16, u16, Vec<u8>)> {
        vec![
            ("US", 0x0028, 0x0010, us(rows)),
            ("US", 0x0028, 0x0011, us(cols)),
            ("US", 0x0028, 0x0100, us(bits)),
            ("US", 0x0028, 0x0101, us(12)),
            ("US", 0x0028, 0x0002, us(1)),
            ("CS", 0x0028, 0x0004, b"MONOCHROME2".to_vec()),
        ]
    }

    #[test]
    fn geometry_is_read_from_the_instance() {
        let data = instance(&geometry_attrs(4, 5, 16), None);
        let g = geometry(&data).expect("geometry");
        assert_eq!(g.rows, 4);
        assert_eq!(g.cols, 5);
        assert_eq!(g.samples, 1);
        assert_eq!(g.bits_allocated, 16);
        assert_eq!(g.bits_stored, 12);
        assert_eq!(g.frames, 1);
        assert!(!g.inverted);
        assert!(!g.signed);
    }

    /// A (0028,0103) that is PRESENT but not readable as a US is not the same
    /// as an absent one: defaulting it to unsigned reads every negative value
    /// as a large positive one, and the instance decodes into plausible
    /// numbers that mean the wrong thing. A four-byte US is exactly that
    /// shape - the attribute is there, its length is not US's.
    #[test]
    fn a_malformed_pixel_representation_is_an_error_not_unsigned() {
        let mut attrs = geometry_attrs(4, 5, 16);
        attrs.push(("US", 0x0028, 0x0103, vec![1, 0, 0, 0]));
        let data = instance(&attrs, None);

        let message = match geometry(&data) {
            Err(Error::Instance(message)) => message,
            Ok(g) => panic!(
                "a present-but-unreadable PixelRepresentation must not be defaulted, got signed={}",
                g.signed
            ),
        };
        assert!(
            message.contains("PixelRepresentation"),
            "the error names the attribute: {message}"
        );
    }

    /// The same rule for (0028,0101): a value that does not fit the reader's
    /// type is a stated-but-unreadable attribute, not a missing one.
    #[test]
    fn a_malformed_bits_stored_is_an_error_not_the_allocated_default() {
        let mut attrs = geometry_attrs(4, 5, 16);
        // Replace the well-formed BitsStored with a value over 255.
        attrs.retain(|(_, g, e, _)| !(*g == 0x0028 && *e == 0x0101));
        attrs.push(("US", 0x0028, 0x0101, us(300)));
        let data = instance(&attrs, None);

        let message = match geometry(&data) {
            Err(Error::Instance(message)) => message,
            Ok(g) => panic!(
                "a BitsStored of 300 must not fall back to BitsAllocated, got {}",
                g.bits_stored
            ),
        };
        assert!(
            message.contains("BitsStored"),
            "the error names the attribute: {message}"
        );
    }

    /// The default is still applied when the attribute is genuinely ABSENT -
    /// the fix distinguishes the two cases rather than refusing both.
    #[test]
    fn an_absent_bits_stored_and_pixel_representation_keep_their_defaults() {
        let mut attrs = geometry_attrs(4, 5, 16);
        attrs.retain(|(_, g, e, _)| !(*g == 0x0028 && (*e == 0x0101 || *e == 0x0103)));
        let data = instance(&attrs, None);

        let g = geometry(&data).expect("omitted attributes are legal");
        assert_eq!(g.bits_stored, 16, "absent BitsStored follows BitsAllocated");
        assert!(!g.signed, "absent PixelRepresentation means unsigned");
    }

    #[test]
    fn the_sop_class_uid_is_read_from_the_instance() {
        let mut attrs = geometry_attrs(2, 2, 8);
        attrs.push((
            "UI",
            0x0008,
            0x0016,
            b"1.2.840.10008.5.1.4.1.1.1.1".to_vec(),
        ));
        let data = instance(&attrs, None);
        assert_eq!(
            sop_class_uid(&data).as_deref(),
            Some("1.2.840.10008.5.1.4.1.1.1.1"),
            "the result object's (0008,1150) value"
        );
    }

    #[test]
    fn monochrome1_is_recorded_as_inverted() {
        let mut attrs = geometry_attrs(2, 2, 8);
        attrs[5] = ("CS", 0x0028, 0x0004, b"MONOCHROME1".to_vec());
        let data = instance(&attrs, None);
        assert!(geometry(&data).expect("geometry").inverted);
    }

    #[test]
    fn a_missing_geometry_attribute_is_named() {
        let attrs: Vec<_> = geometry_attrs(4, 5, 16)
            .into_iter()
            .filter(|(_, _, element, _)| *element != 0x0011)
            .collect();
        let data = instance(&attrs, None);
        let err = geometry(&data).expect_err("no columns");
        assert!(err.to_string().contains("Columns"), "{err}");
    }

    #[test]
    fn pixels_are_borrowed_from_the_instance() {
        // Four 16-bit samples.
        let samples: Vec<u8> = [10u16, 20, 30, 40]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let data = instance(&geometry_attrs(2, 2, 16), Some(&samples));
        let g = geometry(&data).expect("geometry");
        let pixels = frame(&data, &g).expect("pixels");
        assert_eq!(pixels.len(), 8);
        assert_eq!(pixels.gray(0, 0), Some(10));
        assert_eq!(pixels.gray(0, 1), Some(20));
        assert_eq!(pixels.gray(1, 0), Some(30));
        assert_eq!(pixels.gray(1, 1), Some(40));
        // Out of range reads are refused rather than wrapping.
        assert_eq!(pixels.gray(2, 0), None);
        assert_eq!(pixels.gray(0, 2), None);
    }

    #[test]
    fn eight_bit_samples_read_as_unsigned_bytes() {
        let samples = vec![0u8, 127, 200, 255];
        let data = instance(&geometry_attrs(2, 2, 8), Some(&samples));
        let g = geometry(&data).expect("geometry");
        let pixels = frame(&data, &g).expect("pixels");
        assert_eq!(pixels.width, SampleWidth::U8);
        assert_eq!(pixels.gray(0, 0), Some(0));
        assert_eq!(pixels.gray(1, 1), Some(255));
    }

    #[test]
    fn a_signed_instance_sign_extends() {
        let samples: Vec<u8> = [-1i16, -32768, 0, 32767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut attrs = geometry_attrs(2, 2, 16);
        attrs.push(("US", 0x0028, 0x0103, us(1)));
        let data = instance(&attrs, Some(&samples));
        let g = geometry(&data).expect("geometry");
        assert!(g.signed);
        let pixels = frame(&data, &g).expect("pixels");
        assert_eq!(pixels.gray(0, 0), Some(-1));
        assert_eq!(pixels.gray(0, 1), Some(-32768));
        assert_eq!(pixels.gray(1, 1), Some(32767));
    }

    #[test]
    fn a_multiframe_instance_is_refused_rather_than_truncated() {
        let mut attrs = geometry_attrs(2, 2, 16);
        attrs.push(("IS", 0x0028, 0x0008, b"4".to_vec()));
        let samples = vec![0u8; 32];
        let data = instance(&attrs, Some(&samples));
        let g = geometry(&data).expect("geometry");
        assert_eq!(g.frames, 4);
        let err = frame(&data, &g).expect_err("multiframe");
        assert!(err.to_string().contains("4 frames"), "{err}");
    }

    #[test]
    fn a_colour_instance_is_refused() {
        let mut attrs = geometry_attrs(2, 2, 8);
        attrs[4] = ("US", 0x0028, 0x0002, us(3));
        let samples = vec![0u8; 12];
        let data = instance(&attrs, Some(&samples));
        let g = geometry(&data).expect("geometry");
        let err = frame(&data, &g).expect_err("colour");
        assert!(err.to_string().contains("3 samples"), "{err}");
    }

    #[test]
    fn a_short_pixel_buffer_is_refused_with_both_sizes() {
        // The geometry says four 16-bit samples; only three are present.
        let data = instance(&geometry_attrs(2, 2, 16), Some(&[0u8; 6]));
        let g = geometry(&data).expect("geometry");
        let err = frame(&data, &g).expect_err("short");
        let text = err.to_string();
        assert!(text.contains("6 bytes"), "{text}");
        assert!(text.contains("needs 8"), "{text}");
    }

    #[test]
    fn a_missing_pixel_element_is_named() {
        let data = instance(&geometry_attrs(2, 2, 16), None);
        let g = geometry(&data).expect("geometry");
        let err = frame(&data, &g).expect_err("no pixels");
        assert!(err.to_string().contains("PixelData"), "{err}");
    }

    #[test]
    fn an_unsupported_bit_depth_is_refused_by_name() {
        let data = instance(&geometry_attrs(2, 2, 32), Some(&[0u8; 32]));
        let g = geometry(&data).expect("geometry");
        let err = frame(&data, &g).expect_err("32 bit");
        assert!(err.to_string().contains("BitsAllocated 32"), "{err}");
    }

    #[test]
    fn zero_dimensions_are_refused() {
        let data = instance(&geometry_attrs(0, 5, 16), Some(&[0u8; 4]));
        let err = geometry(&data).expect_err("zero rows");
        assert!(err.to_string().contains("zero"), "{err}");
    }

    /// (0002,0010) is read from bytes already in hand, borrowed not owned.
    #[test]
    fn the_transfer_syntax_is_borrowed_from_the_file_meta() {
        let mut attrs = geometry_attrs(2, 2, 8);
        attrs.insert(
            0,
            ("UI", 0x0002, 0x0010, b"1.2.840.10008.1.2.4.70".to_vec()),
        );
        let data = instance(&attrs, None);
        assert_eq!(
            transfer_syntax_uid(&data),
            Some(&b"1.2.840.10008.1.2.4.70"[..]),
            "the UID is returned as a slice of the instance"
        );
    }

    #[test]
    fn a_padded_transfer_syntax_reads_without_its_padding() {
        // UI pads with NUL to an even length (PS3.5 6.2); a writer that
        // pads with a space instead still reads.
        for (spelling, expected) in [
            (b"1.2.840.10008.1.2.1\0".to_vec(), "1.2.840.10008.1.2.1"),
            (
                b"1.2.840.10008.1.2.4.70 ".to_vec(),
                "1.2.840.10008.1.2.4.70",
            ),
        ] {
            let mut attrs = geometry_attrs(2, 2, 8);
            attrs.insert(0, ("UI", 0x0002, 0x0010, spelling));
            let data = instance(&attrs, None);
            let uid = transfer_syntax_uid(&data).expect("readable");
            assert_eq!(
                String::from_utf8_lossy(uid),
                expected,
                "the padding is trimmed and the UID itself is intact"
            );
        }
    }

    #[test]
    fn a_missing_transfer_syntax_is_reported_as_absent() {
        // The absence is a fact the caller acts on: an instance whose
        // encoding cannot be established must not be read as uncompressed.
        let data = instance(&geometry_attrs(2, 2, 8), None);
        assert_eq!(transfer_syntax_uid(&data), None);
    }

    /// The landmark coordinates, which is what `number` is for.
    const TAG_X: (u16, u16) = (0x0070, 0x0022);

    #[test]
    fn a_padded_is_value_is_read_as_the_number_it_says() {
        // The defect: `"1 "` is two bytes, and reading two bytes as binary
        // gives 0x2031 = 8241. A landmark at x=1 must not become x=8241.
        let data = instance(&[("IS", 0x0070, 0x0022, b"1 ".to_vec())], None);
        assert_eq!(number(&data, TAG_X), Some(1), "a padded IS is text");
    }

    #[test]
    fn is_text_is_parsed_not_decoded() {
        let data = instance(&[("IS", 0x0070, 0x0022, b"12".to_vec())], None);
        assert_eq!(number(&data, TAG_X), Some(12));
        let negative = instance(&[("IS", 0x0070, 0x0022, b"-7".to_vec())], None);
        assert_eq!(number(&negative, TAG_X), Some(-7));
    }

    #[test]
    fn a_us_value_is_read_as_binary() {
        let data = instance(&[("US", 0x0070, 0x0022, us(5))], None);
        assert_eq!(number(&data, TAG_X), Some(5));
        // Two bytes whose text reading would differ from their binary one:
        // 0x0031 is 49, and as text it is "1" then a NUL.
        let tricky = instance(&[("US", 0x0070, 0x0022, us(49))], None);
        assert_eq!(number(&tricky, TAG_X), Some(49));
    }

    #[test]
    fn a_ss_value_is_read_as_signed() {
        let data = instance(
            &[("SS", 0x0070, 0x0022, (-2i16).to_le_bytes().to_vec())],
            None,
        );
        assert_eq!(number(&data, TAG_X), Some(-2));
    }

    #[test]
    fn a_ul_value_is_read_as_four_bytes() {
        let data = instance(
            &[("UL", 0x0070, 0x0022, 70_000u32.to_le_bytes().to_vec())],
            None,
        );
        assert_eq!(number(&data, TAG_X), Some(70_000));
    }

    #[test]
    fn a_binary_vr_with_the_wrong_length_is_refused() {
        // A US carrying four bytes is malformed. Reading it either way
        // would place the landmark at a number nobody wrote.
        let data = instance(&[("US", 0x0070, 0x0022, vec![1, 0, 0, 0])], None);
        assert_eq!(number(&data, TAG_X), None);
    }
}
