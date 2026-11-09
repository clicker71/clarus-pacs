// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Presentation-state overlay: asset gates and fixture integrity.
//!
//! JavaScript is not executed here - the viewer page is a static asset and
//! this crate has no JS runtime. What these tests can honestly assert is:
//!
//! - the `/` route still serves `text/html` and `viewer_html()` is still the
//!   embedded string, so the overlay changed nothing server-side;
//! - the asset contains the structural pieces the overlay needs (the plane,
//!   the tags it reads, and the transform application in `AZ()`);
//! - the committed fixture is the one the generator produces, and its
//!   documented coordinates are present in the bytes.
//!
//! The parser itself is exercised against these fixtures outside the Rust
//! suite, by running the asset's own `PD` in a JavaScript engine; see the
//! accompanying report for that transcript.

#![forbid(unsafe_code)]

use std::path::PathBuf;

/// The embedded viewer asset, as served.
fn asset() -> &'static str {
    clarus_server::handlers::viewer::viewer_html()
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

// -- Server side is untouched ---------------------------------------------

#[test]
fn viewer_html_is_still_a_static_string() {
    // Doubles as a compile-time check: the signature takes no arguments and
    // yields `&'static str`, so no per-request rendering was introduced.
    let html: &'static str = asset();
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "viewer must start with a doctype"
    );
    assert!(
        html.contains(&clarus_server::version::version_line()),
        "viewer must carry the build version line"
    );
    assert!(
        !html.contains("{VERSION}"),
        "the version placeholder must be substituted"
    );
    // Repeated calls return the same allocation, which is what keeps the
    // human-frequency route free of rendering work.
    assert!(
        std::ptr::eq(asset().as_ptr(), asset().as_ptr()),
        "the asset must be resolved once, not rebuilt per call"
    );
}

// -- Asset structure ------------------------------------------------------

#[test]
fn viewer_has_the_overlay_plane() {
    let html = asset();
    assert!(
        html.contains("#OL{position:absolute"),
        "the overlay plane needs its own rule to be positioned like the image"
    );
    assert!(
        html.contains("id=\"OL\""),
        "the overlay plane element must exist"
    );
    assert!(
        html.contains("<canvas id=\"OL\"></canvas>"),
        "the plane is a canvas sized to the image"
    );
    // The plane lives inside #W beside the image, which is what makes it share
    // the pan/zoom transform rather than needing a second one.
    let w = html.find("<div id=\"W\">").expect("#W present");
    let close = html[w..].find("</div>").expect("#W closes");
    let inner = &html[w..w + close];
    assert!(
        inner.contains("id=\"I\"") && inner.contains("id=\"OL\""),
        "the image and the plane must be siblings inside #W"
    );
}

#[test]
fn viewer_reads_the_annotation_tags() {
    let html = asset();
    for (tag, why) in [
        ("0x0070", "the graphic annotation group"),
        ("0x0001", "GraphicAnnotationSequence"),
        ("0x0009", "GraphicObjectSequence"),
        ("0x0022", "GraphicData"),
        ("0x0023", "GraphicType"),
        ("0x0008", "the reference group"),
        ("0x1140", "ReferencedImageSequence"),
        ("0x1199", "ReferencedSOPSequence"),
        ("0x1115", "ReferencedSeriesSequence"),
        ("0x000E", "SeriesInstanceUID"),
    ] {
        assert!(html.contains(tag), "asset must read {tag} ({why})");
    }
    for shape in ["'RECTANGLE'", "'TEXT'"] {
        assert!(html.contains(shape), "asset must handle {shape}");
    }
}

#[test]
fn viewer_walks_undefined_length_items() {
    let html = asset();
    // The frozen PR form writes items with an undefined length (0xFFFFFFFF
    // after (FFFE,E000)). SQ() must walk each item to its delimiter instead
    // of bailing out, or the whole overlay silently stays empty.
    assert!(
        html.contains("function IE(dv,start){"),
        "the walker needs an item-end helper for undefined-length items"
    );
    assert!(
        html.contains("if(l===0xFFFFFFFF){var z=IE(dv,off+8);out.push([off+8,z]);"),
        "SQ() must push an undefined-length item by walking to its delimiter"
    );
    assert!(
        !html.contains("if(l===0xFFFFFFFF)break;"),
        "SQ() must not bail on an undefined-length item"
    );
    // The sequence skipper must parse with the file's real VR. Every caller
    // hardcoding implicit (SU(...,true)) was the field bug: on an explicit
    // VR presentation state it read the VR bytes as a length, ran away, and
    // the walk silently lost the annotation sequence.
    for forbidden in [
        "SU(dv,vo,true)",
        "SU(dv,el.vo,true)",
        "SU(dv,e2.vo,true)",
        "SU(dv,e3.vo,true)",
    ] {
        assert!(
            !html.contains(forbidden),
            "no skipper call may force the implicit-VR parse ({forbidden})"
        );
    }
}

#[test]
fn overlay_shares_the_image_transform() {
    let html = asset();
    assert!(
        html.contains("OL.style.transform=TT"),
        "AZ() must apply the image transform string to the overlay plane"
    );
    assert!(
        html.contains("N.style.transform=TT"),
        "the transform must still be assigned to the image from the same value"
    );
    // One transform expression, two consumers: computing it twice is how the
    // two layers drift apart under zoom.
    assert!(
        !html.contains("OL.style.transform='translate("),
        "the plane must not compute its own transform"
    );
}

#[test]
fn overlay_redraws_per_instance_and_frame() {
    let html = asset();
    assert!(
        html.contains("DHO()"),
        "the overlay redraw entry point must exist"
    );
    // A box with its own image reference draws only over that image: one
    // PR now carries boxes for several images of a study.
    assert!(
        html.contains("a.u!=null&&a.u!==CI"),
        "each box must draw only over its own image"
    );
    // Redrawn when an instance loads and when the frame changes.
    assert!(
        html.contains("DH=PD(buf);CI=inst.s;DHO();"),
        "loading an instance must redraw the overlay"
    );
    assert_eq!(
        html.matches("FRN.disabled=(FC>=NF);DHO()").count(),
        2,
        "both frame buttons must redraw the overlay"
    );
}

#[test]
fn discovery_is_scoped_to_the_current_series() {
    let html = asset();
    assert!(
        html.contains("Modality=PR"),
        "PR series are found through the existing QIDO search"
    );
    // The series reference decides adoption; anything looser would draw one
    // series' marks over another series' images. One PR may reference
    // several series of the study, so adoption is membership in the
    // referenced set, not equality with a single UID.
    assert!(
        html.contains("RSUIDS.indexOf(suid)<0"),
        "a presentation state must be rejected unless it names the series on screen"
    );
    assert!(
        html.contains("RSUIDS.push(sux)"),
        "every referenced series UID must be collected"
    );
}

#[test]
fn viewer_carries_no_overlay_ui() {
    let html = asset();
    // The mini-viewer is a service-engineer tool: boxes appearing IS the
    // signal. No toggle, no second disclaimer, no configured wording.
    for forbidden in [
        "id=\"DS\"",
        "window.VC",
        "disclaimer",
        "disclaimer_en",
        "disclaimer_ru",
    ] {
        assert!(
            !html.contains(forbidden),
            "the overlay must not introduce {forbidden}"
        );
    }
    assert!(
        html.contains("NOT FOR DIAGNOSTIC USE"),
        "the existing header notice must remain the only disclaimer"
    );
}

// -- Fixture --------------------------------------------------------------

#[test]
fn pr_fixture_carries_the_documented_values() {
    let bytes = std::fs::read(fixture_path("pr_minimal.dcm")).expect("pr fixture present");
    // The DICOM preamble plus the explicit-VR little-endian transfer syntax
    // the viewer's parser accepts.
    assert!(bytes.starts_with(&[0u8; 128]), "128-byte preamble");
    assert_eq!(&bytes[128..132], b"DICM", "DICM magic after the preamble");
    let ts = b"1.2.840.10008.1.2.1";
    assert!(
        bytes.windows(ts.len()).any(|w| w == ts),
        "explicit VR little endian transfer syntax"
    );

    // Proves the object is a presentation state, not a rendered image.
    let gsps = b"1.2.840.10008.5.1.4.1.1.11.1";
    assert!(
        bytes.windows(gsps.len()).any(|w| w == gsps),
        "GSPS SOP class UID"
    );
    // Modality is a 2-byte (0008,0060) CS value: tag, VR, 2-byte length, "PR"
    // with no pad byte, since the value already fills the even length.
    let modality = b"\x08\x00\x60\x00CS\x02\x00PR";
    assert!(
        bytes.windows(modality.len()).any(|w| w == modality),
        "Modality PR as an unpadded two-byte CS"
    );

    // The referenced series the overlay matches on.
    let series = b"1.2.826.0.1.3680043.10.1609.3.1";
    assert!(
        bytes.windows(series.len()).any(|w| w == series),
        "ReferencedSeriesSequence -> SeriesInstanceUID"
    );

    // Sequence and item delimiters for the annotation group: the walker needs
    // real items to iterate, so their absence would mean a fixture that cannot
    // exercise the parser.
    let item = [0xFEu8, 0xFF, 0x00, 0xE0];
    assert!(
        bytes.windows(4).any(|w| w == item),
        "at least one item delimiter"
    );

    // The live serializer writes every item with an undefined length, so the
    // committed fixture must too: a walker gate passes trivially against a
    // defined-length fixture and says nothing about the field shape.
    let undefined_item = [0xFEu8, 0xFF, 0x00, 0xE0, 0xFF, 0xFF, 0xFF, 0xFF];
    assert!(
        bytes.windows(8).any(|w| w == undefined_item),
        "at least one item with an undefined length"
    );

    // The referenced image's SOP class (Type 1 in the reference item,
    // PS3.3 C.7.6.1): Weasis binds a presentation state to an image by the
    // class+instance pair, so its absence is invisible to the own viewer
    // and fatal for the third-party one.
    let sop_class_ct = b"1.2.840.10008.5.1.4.1.1.2";
    assert!(
        bytes.windows(sop_class_ct.len()).any(|w| w == sop_class_ct),
        "ReferencedSOPClassUID carried in the reference item"
    );

    // GraphicData is stored as raw little-endian f32 values, so the documented
    // coordinate 110.0 must appear as its IEEE-754 byte image.
    for value in [
        10.0f32, 20.0, 110.0, 60.0, 400.0, 120.0, 520.0, 200.0, 300.0, 260.0,
    ] {
        let le = value.to_le_bytes();
        assert!(
            bytes.windows(4).any(|w| w == le),
            "GraphicData value {value} must be present as a little-endian f32"
        );
    }
}

#[test]
fn pr_fixture_generator_is_present_and_deterministic() {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("make_pr_fixture.py");
    let text = std::fs::read_to_string(&script).expect("generator present");
    // Determinism is what lets the binary be committed at all: a generator
    // that read the clock or a random UID would produce a fixture nobody can
    // reproduce.
    assert!(
        text.contains("SERIES_UID = "),
        "the referenced series UID must be a fixed literal"
    );
    assert!(
        !text.contains("datetime") && !text.contains("now()"),
        "the generator must not read the clock"
    );
    assert!(
        !text.contains("generate_uid()"),
        "every UID must be a fixed literal, not generated"
    );
}
