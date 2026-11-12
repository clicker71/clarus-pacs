// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The derived instance, as the derived encoder wrote it, read back.
//!
//! A derived image is a Part-10 file whose (0008,2112) Source Image Sequence
//! is written with an UNDEFINED length (PS3.5 7.5.1): the encoder streams the
//! sequence and puts an explicit (FFFE,E0DD) delimiter after it, rather than
//! measuring an item to find a length it does not have while writing.
//!
//! That is legal, and it is what makes this instance the reader's own
//! counterexample: the geometry tags a second pass needs sit AFTER the
//! undefined-length sequence, so a reader that skips the sequence by a
//! length it does not have lands in the middle of it. The failure is
//! specific and was observed on a live run: `no Rows (0028,0010)` for an
//! instance whose Rows is 1024, which is what a walk 65535 bytes into an item
//! stream reports.
//!
//! The fixture is a byte prefix of the real instance produced by the
//! clahe -> upscale cycle, cut at the (7FE0,0010) header. Every element it
//! contains is complete, the sequence and its delimiters are untouched, and
//! the pixel payload the header promises is simply absent - nothing this
//! module reads lives there.

use std::path::PathBuf;

fn fixture() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/derived_undefined_length_sq.dcm");
    std::fs::read(&path).expect("the derived fixture is present")
}

/// The geometry the live instance declares, read past its own sequence.
///
/// 1024x1024 at BitsAllocated 8: the numbers the origin served and the
/// numbers pydicom reads from the same file, so the fixture's own claim and
/// the reader's answer can be compared instead of the reader's answer being
/// compared with itself.
#[test]
fn the_geometry_past_the_undefined_length_sequence_is_read() {
    let bytes = fixture();
    let geometry = clinfer::pixels::geometry(&bytes).expect("geometry past the sequence");
    assert_eq!(geometry.rows, 1024, "Rows (0028,0010)");
    assert_eq!(geometry.cols, 1024, "Columns (0028,0011)");
    assert_eq!(geometry.bits_allocated, 8, "BitsAllocated (0028,0100)");
    assert_eq!(geometry.bits_stored, 8, "BitsStored (0028,0101)");
    assert_eq!(geometry.samples, 1, "SamplesPerPixel (0028,0002)");
    assert!(!geometry.inverted, "MONOCHROME2, so not inverted");
    assert!(!geometry.signed, "PixelRepresentation 0, so unsigned");
}

/// The identity, which sits BEFORE the sequence: a walk that stopped at the
/// first element it could not skip would take these with it.
#[test]
fn the_identity_before_the_sequence_is_still_read() {
    use clinfer::pixels::{modality, sop_class_uid, transfer_syntax_uid};

    let bytes = fixture();
    assert_eq!(
        transfer_syntax_uid(&bytes),
        Some(&b"1.2.840.10008.1.2.1"[..]),
        "explicit VR little endian, from the file meta group"
    );
    assert_eq!(
        sop_class_uid(&bytes).as_deref(),
        Some("1.2.840.10008.5.1.4.1.1.7"),
        "Secondary Capture, the class the derived encoder writes, without \
         the NUL pad its odd length asked for (PS3.5 6.2)"
    );
    assert_eq!(
        modality(&bytes).as_deref(),
        Some("DX"),
        "carried from the source"
    );
}
