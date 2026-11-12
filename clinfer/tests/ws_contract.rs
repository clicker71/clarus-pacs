// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Declaration-level check that clinfer's RFC 6455 client and the server's
//! responder agree on the primitives they both implement.
//!
//! Duplicating SHA-1/base64/framing across two crates (clinfer cannot depend on
//! clarus-server) is a deliberate trade, and the risk it buys is DRIFT: the two
//! copies could be edited apart. This test pins the agreed values rather than
//! comparing source text, because the values are what the protocol requires and
//! what a mismatch would break.
//!
//! The handshake vector is the one RFC 6455 1.3 publishes, so both sides are
//! being checked against the standard rather than against each other.
//!
//! NOTE: this is a plain unit test inside clinfer (no cross-crate dependency).
//! The end-to-end agreement - clinfer's client completing a real handshake
//! against the server's responder - is covered by the live acceptance e2e
//! described in the sprint report, and cannot be a unit test here because
//! clinfer must not link the server crate.

#![deny(unsafe_code)]

use clinfer::ws;

/// The RFC 6455 1.3 example, byte for byte. Both implementations must produce
/// this value for this key; if either drifts, one of them is wrong.
const RFC6455_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const RFC6455_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

#[test]
fn the_accept_value_matches_the_published_vector() {
    assert_eq!(ws::accept_key(RFC6455_KEY), RFC6455_ACCEPT);
}

#[test]
fn the_guid_is_the_one_the_rfc_defines() {
    // A single wrong character here produces a wrong accept for EVERY key,
    // which is why it is asserted directly rather than only through the
    // composed value above.
    assert_eq!(ws::WS_GUID, b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
}

#[test]
fn the_version_is_13() {
    assert_eq!(ws::WS_VERSION, "13");
}

#[test]
fn sha1_matches_the_rfc3174_vectors_used_by_the_server_gate() {
    // The same four vectors the server module pins. Kept here so a change to
    // either copy that breaks SHA-1 is caught on both sides.
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    assert_eq!(
        hex(&ws::sha1(b"abc")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        hex(&ws::sha1(b"")),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
    assert_eq!(
        hex(&ws::sha1(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    assert_eq!(
        hex(&ws::sha1(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu")),
        "a49b2446a02c645bf419f995b67091253a04a259"
    );
}

#[test]
fn base64_matches_the_rfc4648_table_the_server_gate_uses() {
    for (input, expected) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(ws::base64_encode(input.as_bytes()), expected, "{input:?}");
    }
}

/// The masking asymmetry, stated as a test because it is the single most
/// likely place for a copy-paste mistake between the two modules: the server
/// sends UNMASKED frames, the client sends MASKED ones.
#[test]
fn the_client_masks_and_refuses_masked_server_frames() {
    let mask = [0x37, 0xFA, 0x21, 0x3D];
    let frame = ws::encode_masked(ws::opcode::TEXT, b"hi", mask);
    assert_eq!(frame[1] & 0x80, 0x80, "client frames set the mask bit");

    // A masked frame FROM the server is a violation (RFC 6455 5.1).
    assert_eq!(
        ws::decode_server_frame(&frame),
        Err(ws::FrameError::MaskedFromServer),
        "the client must refuse a masked server frame"
    );
}

/// A well-formed server frame round-trips through the client decoder.
#[test]
fn an_unmasked_server_frame_decodes() {
    let raw = [0x81u8, 0x02, b'h', b'i'];
    let (frame, used) = ws::decode_server_frame(&raw).expect("decodes");
    assert_eq!(used, 4);
    assert!(frame.fin);
    assert_eq!(frame.opcode, ws::opcode::TEXT);
    assert_eq!(frame.payload, b"hi");
}
