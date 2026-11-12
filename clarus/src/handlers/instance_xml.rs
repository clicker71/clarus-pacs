// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Instance Metadata XML (PS3.19) - serde-free JSON->XML converter for the
//! WADO `/metadata` endpoint (PS3.18 10.4.4, `Accept: application/dicom+xml`).
//!
//! # Why a second JSON->XML path?
//! The original converter (`dicomweb_json_to_xml`, formerly `qido_xml.rs`)
//! was the byte-parity oracle but lived on `serde_json::Value` and was removed
//! in the excision (2.7, serde-free). The metadata path must stay
//! serde-free (the bundle drops serde_json from the default binary), so this
//! module re-parses the *native renderer's* JSON bytes with a tiny hand-rolled
//! recursive-descent parser (~130 lines) and re-emits the DICOM XML model.
//!
//! # Controlled grammar (our own renderer)
//! The input is `inject_bulk_data_uris_native` output: a single JSON object
//! `{ "8hex": { "vr": "...", "Value": [...] | "BulkDataURI": "..." }, ... }`.
//! Strings are JSON-escaped; numbers are int or float (f64); bool/null occur.
//! This is a strict subset of JSON - the parser is deliberately minimal.
//!
//! # Byte-parity with the oracle
//! `serde_json::Value::Object` is a BTreeMap, so `dicomweb_json_to_xml` walks
//! attribute objects in SORTED key order. This module matches that: object
//! members are kept and emitted in sorted-tag order for the top-level object
//! and for SQ child objects.
//!
//! Numbers are preserved verbatim (the source token is never re-formatted).
//! This is byte-correct for the WADO corpus: the native renderer emits numeric
//! VRs via `zmij::format_finite` (FL/FD) or bare decimals (US/UL/SS/SL), and
//! DS/IS come through as quoted strings - all canonical shortest-round-trip
//! forms that serde's f64 Display reproduces exactly. Byte-parity is asserted
//! in `clarus-server/tests/instance_metadata_parity.rs` against the frozen
//! `wado_metadata.json` fixture and richer canonical forms.

#![forbid(unsafe_code)]

use std::fmt::Write;

/// The only error this module can produce: malformed JSON bytes.
#[derive(Debug)]
pub struct XmlError {
    msg: String,
}

impl XmlError {
    fn new(msg: impl Into<String>) -> Self {
        XmlError { msg: msg.into() }
    }
}

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "XML render: {}", self.msg)
    }
}

impl std::error::Error for XmlError {}

/// Lightweight JSON value - only what the metadata grammar needs.
enum J {
    Null,
    Bool(bool),
    /// Number preserved verbatim (raw token bytes, kept for byte-parity).
    Num(Vec<u8>),
    /// Decoded string bytes.
    Str(Vec<u8>),
    /// Array - order preserved.
    Arr(Vec<J>),
    /// Object - sorted by key to mirror serde_json's BTreeMap iteration.
    Obj(Vec<(Vec<u8>, J)>),
}

/// RFC 3339? No - DICOM XML namespace from PS3.19 Appendix A.1.
const DICOM_XML_NS: &str = "http://dicom.nema.org/PS3.19/models/NativeDICOM";

// -- Parser state ---------------------------------------------------------

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), XmlError> {
        if self.bump() == Some(c) {
            Ok(())
        } else {
            Err(XmlError::new(format!(
                "expected '{}' at byte {}",
                c as char, self.pos
            )))
        }
    }

    /// Parse the whole document: exactly one JSON value, fully consumed.
    fn parse_document(&mut self) -> Result<J, XmlError> {
        let v = self.parse_value()?;
        self.skip_ws();
        if self.pos != self.src.len() {
            return Err(XmlError::new(format!("trailing bytes at {}", self.pos)));
        }
        Ok(v)
    }

    fn parse_value(&mut self) -> Result<J, XmlError> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(J::Str(self.parse_string()?)),
            Some(b't') => self.parse_keyword(b"true", J::Bool(true)),
            Some(b'f') => self.parse_keyword(b"false", J::Bool(false)),
            Some(b'n') => self.parse_keyword(b"null", J::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(XmlError::new(format!("unexpected byte 0x{:02x}", c))),
            None => Err(XmlError::new("unexpected end of input".to_string())),
        }
    }

    fn parse_keyword(&mut self, kw: &[u8], val: J) -> Result<J, XmlError> {
        if self.src.len() - self.pos < kw.len() || &self.src[self.pos..self.pos + kw.len()] != kw {
            return Err(XmlError::new("invalid literal".to_string()));
        }
        self.pos += kw.len();
        Ok(val)
    }

    fn parse_object(&mut self) -> Result<J, XmlError> {
        self.expect(b'{')?;
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(J::Obj(out));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let val = self.parse_value()?;
            out.push((key, val));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(XmlError::new("expected ',' or '}' in object".to_string())),
            }
        }
        // Sort by key to mirror serde_json's BTreeMap iteration order.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(J::Obj(out))
    }

    fn parse_array(&mut self) -> Result<J, XmlError> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(J::Arr(out));
        }
        loop {
            let val = self.parse_value()?;
            out.push(val);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(XmlError::new("expected ',' or ']' in array".to_string())),
            }
        }
        Ok(J::Arr(out))
    }

    /// Decode a JSON string (assumes the leading `"` is current => not yet
    /// consumed). Returns the decoded bytes.
    fn parse_string(&mut self) -> Result<Vec<u8>, XmlError> {
        self.expect(b'"')?;
        let mut out = Vec::with_capacity(16);
        loop {
            match self.bump() {
                None => return Err(XmlError::new("unterminated string".to_string())),
                Some(b'"') => break,
                Some(b'\\') => {
                    let e = self
                        .bump()
                        .ok_or_else(|| XmlError::new("bad escape".to_string()))?;
                    match e {
                        b'u' => {
                            let cp = self.parse_hex4()? as u32;
                            // Surrogate pair handling.
                            let cp = if (0xd800..=0xdbff).contains(&cp) {
                                if self.peek() == Some(b'\\')
                                    && self.src.get(self.pos + 1) == Some(&b'u')
                                {
                                    self.pos += 2;
                                    let lo = self.parse_hex4()? as u32;
                                    0x10000 + (((cp - 0xd800) << 10) | (lo - 0xdc00))
                                } else {
                                    cp
                                }
                            } else {
                                cp
                            };
                            let mut buf = [0u8; 4];
                            let s = std::char::from_u32(cp).unwrap_or('\u{FFFD}');
                            out.extend_from_slice(s.encode_utf8(&mut buf).as_bytes());
                        }
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        _ => return Err(XmlError::new("bad escape".to_string())),
                    }
                }
                Some(c) => out.push(c),
            }
        }
        Ok(out)
    }

    /// Parse a 4-digit `\uXXXX` escape. Assumes `\u` already consumed.
    fn parse_hex4(&mut self) -> Result<u16, XmlError> {
        let mut v = 0u16;
        for _ in 0..4 {
            let h = self
                .bump()
                .ok_or_else(|| XmlError::new("short \\u".to_string()))?;
            let d = match h {
                b'0'..=b'9' => h - b'0',
                b'a'..=b'f' => h - b'a' + 10,
                b'A'..=b'F' => h - b'A' + 10,
                _ => return Err(XmlError::new("bad \\u hex".to_string())),
            };
            v = v * 16 + d as u16;
        }
        Ok(v)
    }

    /// Parse a JSON number. The token is validated for shape and *preserved
    /// verbatim* into the DOM for later re-emission (byte-parity with the
    /// oracle, which re-prints the original f64 in the corpus).
    fn parse_number(&mut self) -> Result<J, XmlError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let int_start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            let frac_start = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == frac_start {
                return Err(XmlError::new("invalid number: trailing '.'".to_string()));
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            let exp_start = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == exp_start {
                return Err(XmlError::new("invalid number: bad exponent".to_string()));
            }
        }
        if self.pos == int_start {
            return Err(XmlError::new("invalid number".to_string()));
        }
        // Preserve the raw token (byte-parity with the oracle's f64 Display).
        Ok(J::Num(self.src[start..self.pos].to_vec()))
    }
}

// -- Object accessors -----------------------------------------------------

/// Look up a member by key in an object (returns the value or None).
fn obj_get<'a>(obj: &'a [(Vec<u8>, J)], key: &[u8]) -> Option<&'a J> {
    // Members are sorted; a simple linear scan is bounded and clear.
    obj.iter()
        .find(|(k, _)| k.as_slice() == key)
        .map(|(_, v)| v)
}

fn obj_get_str<'a>(obj: &'a [(Vec<u8>, J)], key: &[u8]) -> Option<&'a [u8]> {
    match obj_get(obj, key) {
        Some(J::Str(s)) => Some(s),
        _ => None,
    }
}

fn obj_get_arr<'a>(obj: &'a [(Vec<u8>, J)], key: &[u8]) -> Option<&'a [J]> {
    match obj_get(obj, key) {
        Some(J::Arr(a)) => Some(a),
        _ => None,
    }
}

// -- XML escaping ---------------------------------------------------------

/// True when `c` cannot appear in an XML 1.0 document at all.
///
/// XML 1.0 section 2.2 production [2] `Char` admits only #x9, #xA, #xD,
/// [#x20-#xD7FF], [#xE000-#xFFFD] and [#x10000-#x10FFFF]. Section 4.1
/// production [66] applies the same set to character references, so escaping
/// such a character as `&#x1B;` is equally invalid - the only options are to
/// drop it or to invent a visible glyph.
///
/// PS3.5 6.1.3 allows exactly five C0 controls in a DICOM text value (LF, FF,
/// CR, ESC, TAB); of those, FF and ESC are legal DICOM but not representable in
/// XML, so they are dropped here too. Substituting U+FFFD would fabricate a
/// character the source never had and make the output indistinguishable from a
/// value that genuinely contains it.
fn is_xml_illegal(c: char) -> bool {
    matches!(c, '\u{0000}'..='\u{0008}' | '\u{000B}' | '\u{000C}' | '\u{000E}'..='\u{001F}')
}

/// Escape text content: all 5 chars. Iterates CHARACTERS (not bytes) so
/// non-ASCII UTF-8 (Cyrillic, CJK, ...) round-trips intact - the old byte loop
/// did `b as char`, which corrupted any byte >= 0x80 into mojibake. ASCII
/// behaviour is byte-identical, so goldens are unchanged.
fn esc(s: &[u8]) -> String {
    let mut out = String::new();
    for c in String::from_utf8_lossy(s).chars() {
        if is_xml_illegal(c) {
            continue;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape attribute text: `&`, `<`, `"` only. Same UTF-8-safe character
/// iteration as [`esc`] (attribute values can also carry non-ASCII text),
/// and the same C0 removal - XML attribute values are governed by the same
/// character production as text nodes.
fn esc_attr(s: &[u8]) -> String {
    let mut out = String::new();
    for c in String::from_utf8_lossy(s).chars() {
        if is_xml_illegal(c) {
            continue;
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

// -- PersonName -----------------------------------------------------------

/// The component groups of a raw `A=B\C` person name, without the wrapping
/// `<PersonName>` - used where the element must carry a `number`.
fn write_name_groups_from_raw(xml: &mut String, raw: &[u8]) {
    let groups: Vec<&[u8]> = raw.split(|&b| b == b'=').collect();
    for (name, value) in [
        ("Alphabetic", groups.first()),
        ("Ideographic", groups.get(1)),
        ("Phonetic", groups.get(2)),
    ] {
        let Some(value) = value.filter(|v| !v.is_empty()) else {
            continue;
        };
        write_name_group_element(xml, name, value);
    }
}

/// One component group of a person name: `<Alphabetic>` and its parts.
fn write_name_group_element(xml: &mut String, group_name: &str, value: &[u8]) {
    let parts: Vec<&[u8]> = value.split(|&b| b == b'^').collect();
    xml.push('<');
    xml.push_str(group_name);
    xml.push('>');
    for (name, part) in [
        ("FamilyName", parts.first()),
        ("GivenName", parts.get(1)),
        ("MiddleName", parts.get(2)),
        ("Prefix", parts.get(3)),
        ("Suffix", parts.get(4)),
    ] {
        let Some(part) = part.filter(|p| !p.is_empty()) else {
            continue;
        };
        xml.push('<');
        xml.push_str(name);
        xml.push('>');
        xml.push_str(&esc(part));
        xml.push_str("</");
        xml.push_str(name);
        xml.push('>');
    }
    xml.push_str("</");
    xml.push_str(group_name);
    xml.push('>');
}

/// A DICOM JSON PersonName object (PS3.18 F.2.2) as `<PersonName number="N">`.
///
/// The three component groups are separate JSON keys, so they are read
/// separately - a value that carries only Ideographic must still be written.
fn write_person_name_obj(xml: &mut String, members: &[(Vec<u8>, J)], number: usize) {
    let _ = write!(xml, "<PersonName number=\"{number}\">");
    for (key, group_name) in [
        (b"Alphabetic".as_slice(), "Alphabetic"),
        (b"Ideographic".as_slice(), "Ideographic"),
        (b"Phonetic".as_slice(), "Phonetic"),
    ] {
        if let Some(raw) = obj_get_str(members, key)
            && !raw.is_empty()
        {
            write_name_group_element(xml, group_name, raw);
        }
    }
    xml.push_str("</PersonName>");
}

// -- Core emit ------------------------------------------------------------

/// Render one scalar `Value` array item - mirrors the frozen metadata oracle's
/// scalar path (see `instance_metadata_parity.rs`; qido_xml.rs was removed in
/// the excision).
fn write_scalar(xml: &mut String, v: &J, index: usize) {
    let number = index + 1;
    let _ = write!(xml, "<Value number=\"{number}\">");
    match v {
        J::Bool(b) => {
            let _ = write!(xml, "{b}");
        }
        J::Num(raw) => {
            let s = String::from_utf8_lossy(raw);
            xml.push_str(&s);
        }
        J::Str(s) => {
            xml.push_str(&esc(s));
        }
        J::Null | J::Arr(_) | J::Obj(_) => {
            // The frozen oracle serialised these via `item.to_string()`; the
            // metadata grammar never puts them in a scalar slot, so emit empty.
        }
    }
    let _ = write!(xml, "</Value>");
}

/// Emit one `<DicomAttribute ...>` element - byte-contract frozen in
/// `instance_metadata_parity.rs` (qido_xml.rs gone post 2.7).
fn write_dicom_attribute(xml: &mut String, tag: &[u8], keyword: &str, info: &J) {
    let (vr, bulk, inline, value) = match info {
        J::Obj(members) => {
            let vr = obj_get_str(members, b"vr").unwrap_or(b"UN");
            let bulk = obj_get_str(members, b"BulkDataURI");
            let inline = obj_get_str(members, b"InlineBinary");
            let value = obj_get_arr(members, b"Value");
            (vr, bulk, inline, value)
        }
        _ => (b"UN".as_slice(), None, None, None),
    };

    let _ = write!(
        xml,
        "<DicomAttribute tag=\"{}\" vr=\"{}\" keyword=\"{}\">",
        String::from_utf8_lossy(tag),
        String::from_utf8_lossy(vr),
        keyword
    );

    // PGN 1: BulkDataURI --------------------------------------------
    if let Some(uri) = bulk {
        let _ = write!(xml, "<BulkData uri=\"{}\"/>", esc_attr(uri));
        xml.push_str("</DicomAttribute>");
        return;
    }

    // PGN 2: InlineBinary --------------------------------------------
    if let Some(b64) = inline {
        let _ = write!(xml, "<InlineBinary>{}</InlineBinary>", esc(b64));
        xml.push_str("</DicomAttribute>");
        return;
    }

    // PGN 3: PersonName (VR=PN) --------------------------------------
    //
    // PS3.19 A.1: a PersonName carries the three component groups from the
    // DICOM JSON object, and every person-name value is numbered. Reading only
    // `Alphabetic` dropped Ideographic and Phonetic (the common case for CJK
    // names) and a value that has ONLY Ideographic wrote no PersonName at all.
    if vr == b"PN" {
        if let Some(values) = value {
            for (i, item) in values.iter().enumerate() {
                match item {
                    J::Obj(m) => write_person_name_obj(xml, m, i + 1),
                    J::Str(s) => {
                        let _ = write!(xml, "<PersonName number=\"{}\">", i + 1);
                        write_name_groups_from_raw(xml, s);
                        xml.push_str("</PersonName>");
                    }
                    _ => {}
                }
            }
        }
        xml.push_str("</DicomAttribute>");
        return;
    }

    // PGN 4: Sequence (VR=SQ) ----------------------------------------
    //
    // PS3.19 A.1 puts each item in its own `<Item number="N">`. Writing the
    // child attributes straight into the parent DicomAttribute merged every
    // item of a multi-item sequence into one flat list, which a reader cannot
    // split back into items.
    if vr == b"SQ" {
        if let Some(values) = value {
            for (i, item) in values.iter().enumerate() {
                if let J::Obj(members) = item {
                    let _ = write!(xml, "<Item number=\"{}\">", i + 1);
                    for (child_tag, child_info) in members {
                        write_dicom_attribute(xml, child_tag, "", child_info);
                    }
                    xml.push_str("</Item>");
                }
            }
        }
        xml.push_str("</DicomAttribute>");
        return;
    }

    // PGN 5: Scalar or multi-valued ----------------------------------
    if let Some(values) = value {
        for (i, item) in values.iter().enumerate() {
            write_scalar(xml, item, i);
        }
    }

    xml.push_str("</DicomAttribute>");
}

/// Emit all attributes (top-level object) - byte-contract frozen in
/// `instance_metadata_parity.rs` (qido_xml.rs gone post 2.7).
fn write_dicom_attributes(xml: &mut String, obj: &J) {
    if let J::Obj(members) = obj {
        for (tag, info) in members {
            write_dicom_attribute(xml, tag, "", info);
        }
    }
}

// -- Public API -----------------------------------------------------------

/// Convert the DICOMweb JSON bytes (native renderer output) -> DICOM XML.
///
/// This is the instance-metadata counterpart to the QIDO `dicomweb_json_to_xml`
/// oracle: same PS3.19 model, same byte layout, but driven by the serde-free
/// mini-parser over our own renderer's bytes.
///
/// # Errors
/// Returns `Err` on malformed JSON (broken grammar, truncated input).
pub fn json_metadata_to_xml(json_bytes: &[u8]) -> Result<Vec<u8>, XmlError> {
    let mut parser = Parser {
        src: json_bytes,
        pos: 0,
    };
    let v = parser.parse_document()?;

    let mut xml = String::with_capacity(json_bytes.len() + 512);
    let _ = writeln!(xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = writeln!(xml, r#"<NativeDicomModel xmlns="{DICOM_XML_NS}">"#);
    write_dicom_attributes(&mut xml, &v);
    let _ = writeln!(xml, "</NativeDicomModel>");
    Ok(xml.into_bytes())
}

/// Render ONE stored DICOM JSON attribute element - the `{"vr","Value"}`
/// info object a UPS (0074,1210) is carried as - into a single
/// `<DicomAttribute>` element for `tag`.
///
/// The UPS workitem XML writer stores Scheduled Processing Parameters as an
/// opaque JSON info object (the value subtree survives verbatim, only the
/// outer wrapper is re-emitted). To render that element into the XML document
/// it is re-parsed by the same serde-free parser that serves `/metadata` and
/// walked by [`write_dicom_attribute`], so an arbitrary Content Item subtree
/// (nested SQ, PN, multi-value DS/IS, BulkDataURI, InlineBinary) round-trips
/// without the typed writer modelling every Content Item a site may invent.
///
/// The top level is the INFO object itself, not the tag-keyed object the
/// document path reads - which is why this function takes `tag` explicitly.
///
/// # Errors
/// Returns `Err` on malformed JSON. A parse failure IS possible even for a
/// stored subtree written by this process: this parser has no depth cap,
/// while the request-body parser (`json_common`) refuses at `MAX_JSON_DEPTH`,
/// so a stored subtree the server accepted can still fail here. Callers must
/// therefore fall back to their typed projection instead of assuming success.
pub fn json_info_to_xml_element(
    tag: &str,
    keyword: &str,
    json_bytes: &[u8],
) -> Result<String, XmlError> {
    let mut parser = Parser {
        src: json_bytes,
        pos: 0,
    };
    let info = parser.parse_document()?;

    let mut xml = String::with_capacity(json_bytes.len() + 64);
    write_dicom_attribute(&mut xml, tag.as_bytes(), keyword, &info);
    Ok(xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_xml(json: &str) -> String {
        let bytes = json_metadata_to_xml(json.as_bytes()).expect("parse ok");
        String::from_utf8(bytes).unwrap()
    }

    // -- Empty object ------------------------------------------------

    #[test]
    fn empty_object() {
        let xml = to_xml("{}");
        assert!(xml.contains("<NativeDicomModel"));
        assert!(xml.contains("</NativeDicomModel>"));
        assert!(!xml.contains("<DicomAttribute"));
    }

    // -- PN decomposition --------------------------------------------

    #[test]
    fn pn_decomposition() {
        let xml = to_xml(r#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"DOE^JOHN"}]}}"#);
        // PS3.19 A.1 requires the value number on PersonName.
        assert!(xml.contains("<PersonName number=\"1\">"), "{xml}");
        assert!(xml.contains("<FamilyName>DOE</FamilyName>"));
        assert!(xml.contains("<GivenName>JOHN</GivenName>"));
    }

    /// PS3.19 A.1 puts each sequence item in its own `<Item number="N">`: two
    /// items written into one DicomAttribute cannot be told apart again.
    #[test]
    fn sequence_items_are_wrapped_and_numbered() {
        let xml = to_xml(
            r#"{"00081115":{"vr":"SQ","Value":[
                {"0020000E":{"vr":"UI","Value":["1.2.3"]}},
                {"0020000E":{"vr":"UI","Value":["4.5.6"]}}]}}"#,
        );
        assert!(xml.contains("<Item number=\"1\">"), "{xml}");
        assert!(xml.contains("<Item number=\"2\">"), "{xml}");
        assert_eq!(xml.matches("</Item>").count(), 2, "{xml}");
    }

    /// A PersonName that carries only Ideographic still reaches the document,
    /// and the group is written where PS3.19 puts it.
    #[test]
    fn an_ideographic_only_person_name_is_written() {
        let xml = to_xml(r#"{"00100010":{"vr":"PN","Value":[{"Ideographic":"山田^太郎"}]}}"#);
        assert!(xml.contains("<PersonName number=\"1\">"), "{xml}");
        assert!(
            xml.contains("<Ideographic><FamilyName>山田</FamilyName>"),
            "{xml}"
        );
        assert!(!xml.contains("<Alphabetic>"), "{xml}");
    }

    // -- Multi-value DS ----------------------------------------------

    #[test]
    fn multi_value_ds() {
        let xml = to_xml(r#"{"00281030":{"vr":"DS","Value":[1.5,2.5,-3]}}"#);
        assert!(xml.contains(r#"tag="00281030" vr="DS""#));
        assert!(xml.contains("<Value number=\"1\">1.5</Value>"));
        assert!(xml.contains("<Value number=\"2\">2.5</Value>"));
        assert!(xml.contains("<Value number=\"3\">-3</Value>"));
    }

    // -- SQ recursion ------------------------------------------------

    #[test]
    fn sq_recursion() {
        let xml = to_xml(
            r#"{"00081198":{"vr":"SQ","Value":[{"00081150":{"vr":"UI","Value":["1.2.3"]}}]}}"#,
        );
        assert!(xml.contains(r#"tag="00081198" vr="SQ""#));
        assert!(xml.contains(r#"tag="00081150""#));
        assert!(xml.contains("1.2.3"));
    }

    // -- Escaping ----------------------------------------------------

    #[test]
    fn escaping() {
        let xml = to_xml(r#"{"00081030":{"vr":"LO","Value":["a<&>\"'"]}}"#);
        assert!(xml.contains("&lt;"));
        assert!(xml.contains("&amp;"));
        assert!(xml.contains("&gt;"));
        assert!(xml.contains("&quot;"));
        assert!(xml.contains("&apos;"));
    }

    // -- Escaping: non-ASCII UTF-8 must round-trip, not mojibake -------

    #[test]
    fn escaping_cyrillic_utf8() {
        // Cyrillic survives byte->char->UTF-8 intact (no `b as char` mojibake).
        let name = "Иванов^Иван";
        let xml = to_xml(&format!(
            r#"{{"00081030":{{"vr":"LO","Value":["{}"]}}}}"#,
            name
        ));
        // The exact UTF-8 string is present in the escaped output.
        assert!(xml.contains(name), "Cyrillic must round-trip: {xml}");
        // And it is NOT the byte-latin mangled form (each UTF-8 byte shown as
        // a separate Latin-1 character), which the assertion below detects.
        let mangled: String = name.as_bytes().iter().map(|&b| b as char).collect();
        assert!(
            !xml.contains(&mangled),
            "must not emit byte-as-char mojibake: {xml}"
        );

        // PN: the name is decomposed into name groups (^ is the group
        // separator), so the '^'-joined string is NOT contiguous. Assert the
        // Cyrillic components round-trip into their groups.
        let xml2 = to_xml(&format!(
            r#"{{"00080010":{{"vr":"PN","Value":["{}"]}}}}"#,
            name
        ));
        assert!(
            xml2.contains("Иванов"),
            "PN FamilyName must round-trip: {xml2}"
        );
        assert!(
            xml2.contains("Иван"),
            "PN GivenName must round-trip: {xml2}"
        );
        // And neither part is the byte-as-char mojibake.
        let fam: String = "Иванов".as_bytes().iter().map(|&b| b as char).collect();
        assert!(
            !xml2.contains(&fam),
            "must not emit mojibake FamilyName: {xml2}"
        );
    }

    // -- BulkDataURI -------------------------------------------------

    #[test]
    fn bulkdata() {
        let xml = to_xml(r#"{"7FE00010":{"BulkDataURI":"/dicomweb/studies/1/2","vr":"OW"}}"#);
        assert!(xml.contains("<BulkData uri=\"/dicomweb/studies/1/2\"/>"));
        assert!(xml.contains("</DicomAttribute>"));
    }

    // -- InlineBinary ------------------------------------------------

    #[test]
    fn inline_binary() {
        let xml = to_xml(r#"{"00080008":{"vr":"OB","InlineBinary":"AQID"}}"#);
        assert!(xml.contains("<InlineBinary>AQID</InlineBinary>"));
    }

    // -- Numbers: int, float, negative -------------------------------

    #[test]
    fn numbers() {
        let xml = to_xml(
            r#"{"00100020":{"vr":"LO","Value":["x"]},"00080020":{"vr":"DA","Value":["2024"]}}"#,
        );
        assert!(xml.contains(r#"tag="00080020""#));
        assert!(xml.contains(r#"tag="00100020""#));
    }

    // -- Broken grammar -> Err ---------------------------------------

    #[test]
    fn broken_grammar() {
        assert!(json_metadata_to_xml(b"{").is_err());
        assert!(json_metadata_to_xml(b"[1,").is_err());
        assert!(json_metadata_to_xml(b"not json").is_err());
        assert!(json_metadata_to_xml(b"{\"x\":}").is_err());
    }

    // -- Sorted top-level order (mirrors BTreeMap) -------------------

    #[test]
    fn sorted_order() {
        // Elements listed unsorted in source; emitted sorted by tag.
        let xml = to_xml(
            r#"{"0020000D":{"vr":"UI","Value":["1"]},"00080020":{"vr":"DA","Value":["d"]}}"#,
        );
        let i800 = xml.find(r#"tag="00080020""#).unwrap();
        let i2000 = xml.find(r#"tag="0020000D""#).unwrap();
        assert!(
            i800 < i2000,
            "top-level attrs must be emitted in sorted tag order"
        );
    }
}
