// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The real derived instance, through the PixelData extractor.
//!
//! The instance is clinfer's own derived output (`derived_clahe.dcm`): an
//! explicit VR little endian Secondary Capture whose (0008,2112) Source Image
//! Sequence is written with an UNDEFINED length (PS3.5 7.5.1), followed by
//! (0028,xxxx) geometry and a PixelData OW of exactly 1048576 bytes.
//!
//! Two independent mistakes met on this file, and each alone was enough to
//! lose the payload:
//!
//! 1. The sequential walker added `12 + length` for the sentinel and ran off
//!    the end, returning None - so the parse fell through to the byte scan.
//! 2. The byte scan then refused the payload, because its ceiling was
//!    512*512*4 = 1048576 and the test was strict: a legitimate 1 MiB frame
//!    was rejected by its own maximum, and a 16-bit frame by a factor of two.
//!
//! The fixture is a byte prefix cut at the (7FE0,0010) header. The sequence,
//! both its delimiters and the geometry are the encoder's own bytes; only the
//! payload the last header promises is absent, which is what this crate is
//! asked to find.

#![forbid(unsafe_code)]

use clarus_adapters::codec::pixel_data::extract_pixel_data;
use std::path::PathBuf;

fn fixture() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/derived_clahe_pixeldata_prefix.dcm");
    std::fs::read(&path).expect("the derived fixture is present")
}

/// The instance's own PixelData length, read straight from the header at the
/// offset the walker has to reach.
const DECLARED_PIXEL_LEN: usize = 1_048_576;

#[test]
fn the_pixel_data_past_the_undefined_length_sequence_is_located() {
    // The fixture stops at the (7FE0,0010) header, so a located payload is
    // an empty slice at the value offset. Before the fix this was None: the
    // walker ran past the end and the byte scan refused 1048576 for being
    // its own ceiling.
    let bytes = fixture();
    assert_eq!(
        extract_pixel_data(&bytes),
        Some([].as_slice()),
        "the payload region is located, and the prefix holds none of it"
    );
}

#[test]
fn the_extracted_payload_is_everything_the_declared_length_covers() {
    // With the payload present, the declared length is what bounds the
    // slice - the removed constant must not reappear as a clamp, and the
    // value offset must be the header's own (948 = 936 + 12).
    let bytes = fixture();
    let mut complete = bytes.clone();
    complete.extend(std::iter::repeat_n(0x5Au8, DECLARED_PIXEL_LEN));
    let pixels = extract_pixel_data(&complete).expect("payload present");
    assert_eq!(pixels.len(), DECLARED_PIXEL_LEN);
    assert!(pixels.iter().all(|byte| *byte == 0x5A));
}
