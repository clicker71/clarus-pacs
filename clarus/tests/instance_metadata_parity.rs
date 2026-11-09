// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Byte-goldens for the instance Metadata XML renderer (PS3.18 10.4.4).
//!
//! `clarus_server::handlers::instance_xml::json_metadata_to_xml` is the serde-
//! free XML path for the WADO `/metadata` endpoint. These tests freeze its
//! output as byte-exact goldens (2.7, variant A: the serde-backed
//! reference oracle `qido_xml::dicomweb_json_to_xml` was removed; the native
//! renderer's output - proven byte-identical to the oracle before removal -
//! is now the frozen contract) on the frozen WADO corpus fixture and on
//! richer hand-built cases (sequences, InlineBinary, escaping, floats,
//! negatives) that exercise the full controlled grammar.

#![forbid(unsafe_code)]

use clarus_server::handlers::instance_xml::json_metadata_to_xml;
use std::path::PathBuf;

/// Assert the native serde-free renderer's output is byte-exact with a
/// frozen golden (XML document as a string literal captured from the native
/// renderer at freeze time).
fn assert_golden(json_bytes: &[u8], expected_xml: &str) {
    let ours = json_metadata_to_xml(json_bytes)
        .expect("serde-free renderer must accept this controlled JSON");
    assert_eq!(
        String::from_utf8(ours).unwrap(),
        expected_xml,
        "instance XML renderer drifted from the frozen golden on input {}",
        String::from_utf8_lossy(json_bytes)
    );
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("wado")
}

const HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<NativeDicomModel xmlns=\"http://dicom.nema.org/PS3.19/models/NativeDICOM\">\n";
const TAIL: &str = "</NativeDicomModel>\n";

/// Frozen WADO corpus instance metadata (POST-injection: BulkDataURI on
/// PixelData, flat object, bare numbers, one PN).
#[test]
fn wado_corpus_fixture_golden() {
    let fixture =
        std::fs::read(fixtures_dir().join("wado_metadata.json")).expect("wado_metadata.json");
    let ours = json_metadata_to_xml(&fixture).expect("native render");
    // Byte-exact against the committed golden captured from the native
    // renderer at freeze time (2.7, variant A).
    let golden = std::fs::read(fixtures_dir().join("wado_metadata.xml")).expect("golden xml");
    assert_eq!(
        ours, golden,
        "native instance XML drifted from frozen wado_metadata.xml golden"
    );
}

/// Single PersonName with full 5-component decomposition.
#[test]
fn golden_pn_five_components() {
    assert_golden(
        br#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"DOE^JOHN^QUINCY^DR^JR"}]}}"#,
        &format!(
            "{HEAD}<DicomAttribute tag=\"00100010\" vr=\"PN\" keyword=\"\"><PersonName number=\"1\"><Alphabetic><FamilyName>DOE</FamilyName><GivenName>JOHN</GivenName><MiddleName>QUINCY</MiddleName><Prefix>DR</Prefix><Suffix>JR</Suffix></Alphabetic></PersonName></DicomAttribute>{TAIL}"
        ),
    );
}

/// Ideographic + Phonetic PersonName groups (multi-group PN).
///
/// PS3.19 A.1 puts all three component groups in the PersonName element; the
/// earlier golden froze the Alphabetic-only shape, which silently discarded
/// the two groups a CJK name carries.
#[test]
fn golden_pn_multi_group() {
    assert_golden(
        br#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"YAMADA^TARO","Ideographic":"\u5c71\u7530^\u592a\u90ce","Phonetic":"YAMADA^TAROU"}]}}"#,
        &format!("{HEAD}<DicomAttribute tag=\"00100010\" vr=\"PN\" keyword=\"\"><PersonName number=\"1\"><Alphabetic><FamilyName>YAMADA</FamilyName><GivenName>TARO</GivenName></Alphabetic><Ideographic><FamilyName>山田</FamilyName><GivenName>太郎</GivenName></Ideographic><Phonetic><FamilyName>YAMADA</FamilyName><GivenName>TAROU</GivenName></Phonetic></PersonName></DicomAttribute>{TAIL}"),
    );
}

/// Nested sequence (SQ) recursion.
#[test]
fn golden_sequence() {
    assert_golden(
        br#"{"00081198":{"vr":"SQ","Value":[{"00081150":{"vr":"UI","Value":["1.2.3"]},"00081155":{"vr":"UI","Value":["9.8.7"]}}]}}"#,
        &format!("{HEAD}<DicomAttribute tag=\"00081198\" vr=\"SQ\" keyword=\"\"><Item number=\"1\"><DicomAttribute tag=\"00081150\" vr=\"UI\" keyword=\"\"><Value number=\"1\">1.2.3</Value></DicomAttribute><DicomAttribute tag=\"00081155\" vr=\"UI\" keyword=\"\"><Value number=\"1\">9.8.7</Value></DicomAttribute></Item></DicomAttribute>{TAIL}"),
    );
}

/// End-to-end: the ADAPTERS renderer's own nested-SQ output feeds the server's
/// XML converter. This closes the renderer -> XML loop: a nested SQ in a real
/// DICOM must render as nested `DicomAttribute` elements, not an empty
/// sequence. Uses `extract_full_metadata_bytes` (not hand-written JSON), so a
/// regression in the SQ walk fails here too.
#[test]
fn golden_renderer_output_xml_roundtrip() {
    use clarus_adapters::DicomRsParser;

    // A top-level SQ (0008,1198 ReferencedSOPSequence) with one item carrying
    // (0008,1150) UI and (0008,1155) UI - the canonical nested reference.
    let mut item = Vec::new();
    // (0008,1150) UI = "1.2.3"
    item.extend_from_slice(&0x0008u16.to_le_bytes());
    item.extend_from_slice(&0x1150u16.to_le_bytes());
    item.extend_from_slice(b"UI");
    item.extend_from_slice(&5u16.to_le_bytes());
    item.extend_from_slice(b"1.2.3");
    item.push(0x00); // odd -> pad
    // (0008,1155) UI = "9.8.7"
    item.extend_from_slice(&0x0008u16.to_le_bytes());
    item.extend_from_slice(&0x1155u16.to_le_bytes());
    item.extend_from_slice(b"UI");
    item.extend_from_slice(&5u16.to_le_bytes());
    item.extend_from_slice(b"9.8.7");
    item.push(0x00);

    let mut sq_value = Vec::new();
    sq_value.extend_from_slice(&0xFFFEu16.to_le_bytes()); // Item
    sq_value.extend_from_slice(&0xE000u16.to_le_bytes());
    sq_value.extend_from_slice(&(item.len() as u32).to_le_bytes());
    sq_value.extend_from_slice(&item);

    let mut dcm = Vec::new();
    dcm.extend(std::iter::repeat_n(0u8, 128));
    dcm.extend_from_slice(b"DICM");
    // meta: (0002,0010) UI Explicit VR LE (meta header always Uses 2 reserved
    // bytes + 4-byte length; odd value length padded with 0x00).
    dcm.extend_from_slice(&0x0002u16.to_le_bytes());
    dcm.extend_from_slice(&0x0010u16.to_le_bytes());
    dcm.extend_from_slice(b"UI");
    dcm.extend_from_slice(&[0u8, 0u8]);
    let ts = b"1.2.840.10008.1.2.1";
    dcm.extend_from_slice(&(ts.len() as u32).to_le_bytes());
    dcm.extend_from_slice(ts);
    if !ts.len().is_multiple_of(2) {
        dcm.push(0x00);
    }
    // (0010,0010) PN "SQ^TEST" (7 bytes, odd -> pad 0x00)
    dcm.extend_from_slice(&0x0010u16.to_le_bytes());
    dcm.extend_from_slice(&0x0010u16.to_le_bytes());
    dcm.extend_from_slice(b"PN");
    dcm.extend_from_slice(&7u16.to_le_bytes());
    dcm.extend_from_slice(b"SQ^TEST");
    dcm.push(0x00);
    // (0008,1198) SQ with the item stream (2 reserved bytes + 4-byte length).
    dcm.extend_from_slice(&0x0008u16.to_le_bytes());
    dcm.extend_from_slice(&0x1198u16.to_le_bytes());
    dcm.extend_from_slice(b"SQ");
    dcm.extend_from_slice(&[0u8, 0u8]);
    dcm.extend_from_slice(&(sq_value.len() as u32).to_le_bytes());
    dcm.extend_from_slice(&sq_value);

    let json = DicomRsParser
        .extract_full_metadata_bytes(&dcm)
        .expect("adapter renders nested SQ");
    let json_str = String::from_utf8_lossy(&json).into_owned();
    assert!(
        json_str.contains("\"00081198\":{\"Value\":[{"),
        "adapter must render the SQ item object: {json_str}"
    );

    let xml = json_metadata_to_xml(&json).expect("XML converter accepts the adapter output");
    let xml_str = String::from_utf8(xml).unwrap();
    // Nested DicomAttribute elements, with the leaf values inside.
    assert!(
        xml_str.contains("<DicomAttribute tag=\"00081198\" vr=\"SQ\" keyword=\"\"><Item number=\"1\"><DicomAttribute tag=\"00081150\""),
        "SQ must render nested DicomAttribute (not empty): {xml_str}"
    );
    assert!(
        xml_str.contains(">1.2.3</Value>") && xml_str.contains(">9.8.7</Value>"),
        "leaf values inside the nested SQ must render: {xml_str}"
    );
}

/// BulkDataURI on a non-pixel tag.
#[test]
fn golden_bulkdata() {
    assert_golden(
        br#"{"7FE00010":{"BulkDataURI":"/dicomweb/studies/1/series/2/instances/3/bulkdata/7FE00010","vr":"OB"}}"#,
        &format!("{HEAD}<DicomAttribute tag=\"7FE00010\" vr=\"OB\" keyword=\"\"><BulkData uri=\"/dicomweb/studies/1/series/2/instances/3/bulkdata/7FE00010\"/></DicomAttribute>{TAIL}"),
    );
}

/// InlineBinary.
#[test]
fn golden_inline_binary() {
    assert_golden(
        br#"{"00080008":{"vr":"OB","InlineBinary":"AQIDBA=="}}"#,
        &format!(
            "{HEAD}<DicomAttribute tag=\"00080008\" vr=\"OB\" keyword=\"\"><InlineBinary>AQIDBA==</InlineBinary></DicomAttribute>{TAIL}"
        ),
    );
}

/// XML escaping of all five special characters in text.
#[test]
fn golden_escaping() {
    assert_golden(
        br#"{"00081030":{"vr":"LO","Value":["a & b < c > d \" e ' f"]}}"#,
        &format!(
            "{HEAD}<DicomAttribute tag=\"00081030\" vr=\"LO\" keyword=\"\"><Value number=\"1\">a &amp; b &lt; c &gt; d &quot; e &apos; f</Value></DicomAttribute>{TAIL}"
        ),
    );
}

/// XML escaping in attribute values (`>` and `'` unescaped in attrs).
#[test]
fn golden_escaping_attr() {
    assert_golden(
        br#"{"7FE00010":{"BulkDataURI":"a&b<c>d\"e'f","vr":"OW"}}"#,
        &format!(
            "{HEAD}<DicomAttribute tag=\"7FE00010\" vr=\"OW\" keyword=\"\"><BulkData uri=\"a&amp;b&lt;c>d&quot;e'f\"/></DicomAttribute>{TAIL}"
        ),
    );
}

/// Numeric values in the renderer's canonical forms: bare ints (US/UL/SS/SL),
/// integral floats kept with `.0`, decimals, negatives, and FL/FD scientific
/// notation (`zmij` shortest-round-trip - byte-identical to serde's f64).
#[test]
fn golden_numbers() {
    assert_golden(
        br#"{"00280030":{"vr":"FD","Value":[2.0]},"00280031":{"vr":"FD","Value":[1e+30]},"00280032":{"vr":"FD","Value":[5e-324]},"00280033":{"vr":"FL","Value":[1.5]},"00280034":{"vr":"FL","Value":[10000000000.0]},"00280035":{"vr":"FD","Value":[-0.5]}}"#,
        &format!("{HEAD}<DicomAttribute tag=\"00280030\" vr=\"FD\" keyword=\"\"><Value number=\"1\">2.0</Value></DicomAttribute><DicomAttribute tag=\"00280031\" vr=\"FD\" keyword=\"\"><Value number=\"1\">1e+30</Value></DicomAttribute><DicomAttribute tag=\"00280032\" vr=\"FD\" keyword=\"\"><Value number=\"1\">5e-324</Value></DicomAttribute><DicomAttribute tag=\"00280033\" vr=\"FL\" keyword=\"\"><Value number=\"1\">1.5</Value></DicomAttribute><DicomAttribute tag=\"00280034\" vr=\"FL\" keyword=\"\"><Value number=\"1\">10000000000.0</Value></DicomAttribute><DicomAttribute tag=\"00280035\" vr=\"FD\" keyword=\"\"><Value number=\"1\">-0.5</Value></DicomAttribute>{TAIL}"),
    );
}

/// Multi-valued attribute (e.g. multiple IS/DS values).
#[test]
fn golden_multi_value() {
    assert_golden(
        br#"{"00181060":{"vr":"DS","Value":[1.5,2.5,3.5]}}"#,
        &format!(
            "{HEAD}<DicomAttribute tag=\"00181060\" vr=\"DS\" keyword=\"\"><Value number=\"1\">1.5</Value><Value number=\"2\">2.5</Value><Value number=\"3\">3.5</Value></DicomAttribute>{TAIL}"
        ),
    );
}

/// Bool scalar values (last-resort grammar members).
#[test]
fn golden_bool() {
    assert_golden(
        br#"{"00080008":{"vr":"CS","Value":[true]}}"#,
        &format!(
            "{HEAD}<DicomAttribute tag=\"00080008\" vr=\"CS\" keyword=\"\"><Value number=\"1\">true</Value></DicomAttribute>{TAIL}"
        ),
    );
}
