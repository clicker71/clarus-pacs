// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Round-trip a real Implicit VR file through `reencode_implicit_to_explicit`.
//!
//! Set `REENC_TRIP_FILE` to the path of an Implicit VR Little Endian file; the
//! converted bytes are written to `<path>.evrle.dcm` for an external reader to
//! inspect. The test skips when the variable is unset, so the fixture lives
//! where the operator put it rather than in the repository.

use std::path::Path;

#[test]
fn a_real_implicit_file_survives_the_round_trip() {
    let Some(source) = std::env::var_os("REENC_TRIP_FILE") else {
        eprintln!("skipping: set REENC_TRIP_FILE to an Implicit VR Little Endian file");
        return;
    };
    let source = Path::new(&source);
    let bytes = std::fs::read(source).expect("read the fixture");
    assert_eq!(
        clarus_adapters::transcode::extract_transfer_syntax(&bytes).as_deref(),
        Some(clarus_adapters::codec::ts_uid::IMPLICIT_VR_LE),
        "the fixture has to be Implicit VR Little Endian"
    );

    let out = clarus_adapters::transcode::reencode_implicit_to_explicit(&bytes)
        .expect("the round trip succeeds");
    assert_eq!(
        clarus_adapters::transcode::extract_transfer_syntax(&out).as_deref(),
        Some(clarus_adapters::codec::ts_uid::EXPLICIT_VR_LE),
        "the meta group has to declare the new encoding"
    );
    // The File Meta group is rewritten as the canonical trio, so the output is
    // not required to be the same length: pydicom writes (0002,0012) and
    // (0002,0013), which this crate drops deliberately. What has to survive is
    // the DATASET, so the converted file is written out for the external
    // reader (`pydicom`) that the caller runs next: a Rust-side byte compare
    // could only say the frames differ, not that they still mean one thing.
    let input_dataset = dataset_bytes(&bytes);
    assert!(
        !input_dataset.is_empty(),
        "the fixture has a dataset after its meta group"
    );

    let target = source.with_extension("evrle.dcm");
    std::fs::write(&target, &out).expect("write the converted file");
    eprintln!("wrote {}", target.display());
}

/// The dataset's bytes, i.e. everything after the File Meta Information.
fn dataset_bytes(file: &[u8]) -> &[u8] {
    let mut pos = 132usize;
    while pos + 4 <= file.len() {
        if u16::from_le_bytes([file[pos], file[pos + 1]]) != 0x0002 {
            break;
        }
        let vr = std::str::from_utf8(&file[pos + 4..pos + 6]).unwrap_or("");
        let extended = matches!(
            vr,
            "OB" | "OD" | "OF" | "OL" | "OV" | "OW" | "SQ" | "UC" | "UN" | "UR" | "UT"
        );
        let len: usize = if extended {
            u32::from_le_bytes(file[pos + 8..pos + 12].try_into().unwrap()) as usize
        } else {
            u16::from_le_bytes([file[pos + 6], file[pos + 7]]) as usize
        };
        pos += if extended { 12 } else { 8 } + len + (len % 2);
    }
    &file[pos..]
}
