// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A JSON subset, parsed by hand.
//!
//! What arrives over the wire is DICOMweb JSON: an object whose values are
//! `{"Value":[...],"vr":".."}` entries. Reading it needs a reader, not a
//! document model, so this parser produces values and the layers above
//! navigate them.
//!
//! Numbers are kept as written rather than converted, because the only
//! numeric fields clinfer reads are identifiers and they are always
//! quoted anyway; a `U64` that overflows is a malformed document, not a
//! value to round.

use std::fmt;

/// Maximum nesting depth accepted.
///
/// The parser recurses once per container, and the payloads here are
/// shallow (a workitem is two levels at most). The bound exists so a
/// hostile or corrupt body cannot reach the stack limit, and so the
/// failure is a parse error rather than a crash.
const MAX_DEPTH: usize = 64;

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// An integer that fits in `u64`. Larger values are a parse error.
    U64(u64),
    /// A number with a fraction or an exponent.
    F64(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The value of a top-level key, or `None`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Obj(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::U64(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// A number, whether it was written with a fraction or not.
    ///
    /// The parser keeps integers and reals in separate variants, so a
    /// document that says `12` and one that says `12.0` are different
    /// `Json`s while meaning the same number to a reader. Reading a numeric
    /// field through `as_u64` alone would therefore drop every whole-number
    /// value in a document that writes them without a fraction - including
    /// `0` and `1` in an orientation vector, where losing them is silent.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            // An integer beyond 2^53 cannot be represented exactly, and
            // returning an approximation would be worse than a refusal the
            // caller can see. Every geometry value is far below this bound,
            // so the branch costs nothing real.
            Self::U64(value) => {
                if *value <= (1u64 << 53) {
                    Some(*value as f64)
                } else {
                    None
                }
            }
            Self::F64(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Arr(items) => Some(items),
            _ => None,
        }
    }

    /// A string element of an array, which is the shape every
    /// `{"Value":[".."]}` entry takes.
    #[must_use]
    pub fn first_str(&self) -> Option<&str> {
        self.as_array()?.first()?.as_str()
    }

    /// The `Value[0]` string of a DICOMweb attribute entry:
    /// `{"00080060":{"Value":["CT"],"vr":"CS"}}`.
    #[must_use]
    pub fn attribute(&self, tag: &str) -> Option<&str> {
        self.get(tag)?.get("Value")?.first_str()
    }

    /// `Value[0].Alphabetic` of a DICOMweb PN entry, falling back to
    /// `Value[0]` when the entry carries a bare string.
    #[must_use]
    pub fn attribute_pn(&self, tag: &str) -> Option<&str> {
        let first = self.get(tag)?.get("Value")?.as_array()?.first()?;
        match first {
            Self::Obj(_) => first.get("Alphabetic")?.as_str(),
            Self::Str(text) => Some(text),
            _ => None,
        }
    }

    /// `Value[0]` of a DICOMweb sequence entry, as an object:
    /// `{"00404021":{"Value":[{...}],"vr":"SQ"}}`.
    ///
    /// The sequence item's own attributes are then read with `attribute` on
    /// the returned value, which is the same shape a top-level entry has.
    #[must_use]
    pub fn attribute_item(&self, tag: &str) -> Option<&Json> {
        self.get(tag)?.get("Value")?.as_array()?.first()
    }

    #[must_use]
    pub fn is_object(&self) -> bool {
        matches!(self, Self::Obj(_))
    }

    /// Every key/value pair of an object, or `None` for any other value.
    ///
    /// Used by readers that walk a whole document rather than addressing
    /// known tags - a coded sequence can hang under any tag, so the only
    /// way to find every CodeMeaning is to descend through the objects.
    #[must_use]
    pub fn object_entries(&self) -> Option<&[(String, Self)]> {
        match self {
            Self::Obj(entries) => Some(entries),
            _ => None,
        }
    }

    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "a boolean",
            Self::U64(_) | Self::F64(_) => "a number",
            Self::Str(_) => "a string",
            Self::Arr(_) => "an array",
            Self::Obj(_) => "an object",
        }
    }
}

/// The single dataset of a DICOM JSON payload (PS3.18 F.2).
///
/// F.2.2 fixes a single Data Set as one JSON object, and F.2.1 fixes multiple
/// results as a top-level array. In the field the single-retrieve forms
/// diverge: Clarus answers `{...}` and dcm4chee-arc answers `[{...}]`, so a
/// reader of a single-dataset response has to accept both or read one server's
/// reply as an empty one. That is the failure this exists to prevent -
/// `attribute("00080018")` on an array returns `None`, so a workitem would
/// read as carrying no StudyInstanceUID under an HTTP 200.
///
/// Anything else - an empty array, an array of several datasets, an array
/// whose only element is not an object, or a scalar - names no single dataset
/// and is `None`, so the caller refuses it rather than treating it as an empty
/// one.
#[must_use]
pub fn single_dataset(value: &Json) -> Option<&Json> {
    match value {
        Json::Obj(_) => Some(value),
        Json::Arr(items) if items.len() == 1 => match &items[0] {
            Json::Obj(_) => Some(&items[0]),
            _ => None,
        },
        _ => None,
    }
}

/// Where a parse stopped, for a message an operator can act on.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonError {
    /// Byte offset into the input.
    pub at: usize,
    pub message: String,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON at byte {}: {}", self.at, self.message)
    }
}

impl std::error::Error for JsonError {}

/// Parse a complete JSON document. Trailing content is an error: a body
/// that parses and then continues is a body clinfer does not understand.
pub fn parse(bytes: &[u8]) -> Result<Json, JsonError> {
    let text = std::str::from_utf8(bytes).map_err(|err| JsonError {
        at: err.valid_up_to(),
        message: "input is not valid UTF-8".to_string(),
    })?;
    let mut reader = Reader {
        bytes: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    reader.skip_ws();
    let value = reader.value()?;
    reader.skip_ws();
    if reader.pos != reader.bytes.len() {
        return Err(reader.fail("trailing content after the document"));
    }
    Ok(value)
}

/// Parse a document that must be an object.
pub fn parse_object(bytes: &[u8]) -> Result<Vec<(String, Json)>, JsonError> {
    match parse(bytes)? {
        Json::Obj(entries) => Ok(entries),
        other => Err(JsonError {
            at: 0,
            message: format!("expected an object, found {}", other.type_name()),
        }),
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Reader<'a> {
    fn fail(&self, message: impl Into<String>) -> JsonError {
        JsonError {
            at: self.pos,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// Consume `expected`, or fail naming it.
    fn eat(&mut self, expected: u8) -> Result<(), JsonError> {
        if self.peek() == Some(expected) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.fail(format!("expected '{}'", expected as char)))
        }
    }

    fn literal(&mut self, text: &str) -> Result<(), JsonError> {
        if self.bytes[self.pos..].starts_with(text.as_bytes()) {
            self.pos += text.len();
            Ok(())
        } else {
            Err(self.fail(format!("expected '{text}'")))
        }
    }

    fn value(&mut self) -> Result<Json, JsonError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.fail(format!("nested deeper than {MAX_DEPTH} levels")));
        }
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(other) => Err(self.fail(format!("unexpected byte 0x{other:02x}"))),
            None => Err(self.fail("unexpected end of input")),
        }
    }

    fn object(&mut self) -> Result<Json, JsonError> {
        self.eat(b'{')?;
        self.depth += 1;
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            let value = self.value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.fail("expected ',' or '}'")),
            }
        }
        self.depth -= 1;
        Ok(Json::Obj(entries))
    }

    fn array(&mut self) -> Result<Json, JsonError> {
        self.eat(b'[')?;
        self.depth += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.fail("expected ',' or ']'")),
            }
        }
        self.depth -= 1;
        Ok(Json::Arr(items))
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.fail("unterminated string"));
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(escape) = self.peek() else {
                        return Err(self.fail("unterminated escape"));
                    };
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        other => {
                            return Err(self.fail(format!("unknown escape '\\{}'", other as char)));
                        }
                    }
                }
                // Control characters must be escaped in a JSON string.
                0x00..=0x1F => {
                    return Err(self.fail(format!("unescaped control byte 0x{byte:02x}")));
                }
                _ => {
                    // Copy whole UTF-8 sequences: the input is already
                    // validated, so one byte through the char boundary is
                    // enough to advance.
                    let len = utf8_len(byte);
                    let end = self.pos + len;
                    if end > self.bytes.len() {
                        return Err(self.fail("truncated UTF-8 sequence"));
                    }
                    out.push_str(
                        std::str::from_utf8(&self.bytes[self.pos..end])
                            .map_err(|_| self.fail("invalid UTF-8 in string"))?,
                    );
                    self.pos = end;
                }
            }
        }
    }

    /// The four hex digits of a `\uXXXX` escape, including surrogate pairs.
    fn unicode_escape(&mut self) -> Result<char, JsonError> {
        let first = self.hex4()?;
        // A high surrogate must be followed by the `\u` of a low one;
        // anything else is an error rather than a silent truncation.
        if (0xD800..0xDC00).contains(&first) {
            if self.peek() != Some(b'\\') {
                return Err(self.fail("high surrogate not followed by a low surrogate"));
            }
            self.pos += 1;
            if self.peek() != Some(b'u') {
                return Err(self.fail("high surrogate not followed by a low surrogate"));
            }
            self.pos += 1;
            let second = self.hex4()?;
            if !(0xDC00..0xE000).contains(&second) {
                return Err(self.fail("high surrogate not followed by a low surrogate"));
            }
            let combined = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
            return char::from_u32(combined).ok_or_else(|| self.fail("invalid surrogate pair"));
        }
        if (0xDC00..0xE000).contains(&first) {
            return Err(self.fail("unpaired low surrogate"));
        }
        char::from_u32(first).ok_or_else(|| self.fail("invalid \\u escape"))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let end = self.pos + 4;
        if end > self.bytes.len() {
            return Err(self.fail("\\u escape is short"));
        }
        let digits = &self.bytes[self.pos..end];
        let mut value = 0u32;
        for &digit in digits {
            let nibble = (digit as char)
                .to_digit(16)
                .ok_or_else(|| self.fail("\\u escape has a non-hex digit"))?;
            value = value * 16 + nibble;
        }
        self.pos = end;
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Leading zeros are not valid JSON, and accepting them would make
        // this parser more lenient than every producer of it.
        if self.peek() == Some(b'0') {
            self.pos += 1;
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.fail("leading zero in a number"));
            }
        } else {
            let digits = self.digits()?;
            if digits == 0 {
                return Err(self.fail("expected a digit"));
            }
        }
        let mut fractional = false;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            fractional = true;
            if self.digits()? == 0 {
                return Err(self.fail("expected a digit after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            fractional = true;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits()? == 0 {
                return Err(self.fail("expected a digit in the exponent"));
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.fail("invalid UTF-8 in a number"))?;
        if let Some(value) = text.parse::<u64>().ok().filter(|_| !fractional) {
            return Ok(Json::U64(value));
        }
        match text.parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(Json::F64(value)),
            _ => Err(self.fail("number out of range")),
        }
    }

    fn digits(&mut self) -> Result<usize, JsonError> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        Ok(self.pos - start)
    }
}

/// The byte length of a UTF-8 sequence from its first byte.
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Json {
        parse(text.as_bytes()).expect("parses")
    }

    fn err(text: &str) -> JsonError {
        parse(text.as_bytes()).expect_err("should fail")
    }

    #[test]
    fn scalars_are_typed() {
        assert_eq!(ok("null"), Json::Null);
        assert_eq!(ok("true"), Json::Bool(true));
        assert_eq!(ok("false"), Json::Bool(false));
        assert_eq!(ok("0"), Json::U64(0));
        assert_eq!(ok("42"), Json::U64(42));
        assert_eq!(ok("-7"), Json::F64(-7.0));
        assert_eq!(ok("1.5"), Json::F64(1.5));
        assert_eq!(ok("1e3"), Json::F64(1000.0));
        assert_eq!(ok("2.5E-2"), Json::F64(0.025));
        assert_eq!(ok("\"\""), Json::Str(String::new()));
        assert_eq!(ok("\"x\""), Json::Str("x".into()));
    }

    #[test]
    fn as_f64_reads_a_number_written_either_way() {
        // The parser separates integers from reals, so `12` and `12.0` are
        // different `Json`s meaning the same number. A numeric field read
        // through one variant only would silently drop the other - and in
        // an orientation vector `1` written without a fraction is the
        // common case.
        assert_eq!(ok("12").as_f64(), Some(12.0));
        assert_eq!(ok("12.0").as_f64(), Some(12.0));
        assert_eq!(ok("-7").as_f64(), Some(-7.0));
        assert_eq!(ok("0.5").as_f64(), Some(0.5));
        assert_eq!(ok("1e3").as_f64(), Some(1000.0));
        // Not a number is not a number: no coercion from text or null.
        assert_eq!(ok("\"12\"").as_f64(), None);
        assert_eq!(ok("null").as_f64(), None);
        assert_eq!(ok("true").as_f64(), None);
    }

    #[test]
    fn as_f64_refuses_an_integer_beyond_exact_representation() {
        // Beyond 2^53 a float cannot hold every integer, so returning an
        // approximation would be a silent loss of precision. A refusal the
        // caller can see is the honest answer; no geometry value is near
        // this bound.
        assert_eq!(ok("9007199254740992").as_f64(), Some(9007199254740992.0));
        assert_eq!(ok("9007199254740993").as_f64(), None);
    }

    #[test]
    fn a_dicomweb_attribute_entry_reads_as_a_value() {
        let doc = ok(r#"{"00080060":{"Value":["CT"],"vr":"CS"}}"#);
        assert_eq!(doc.attribute("00080060"), Some("CT"));
        assert_eq!(doc.attribute("00080061"), None);
    }

    #[test]
    fn a_person_name_entry_reads_its_alphabetic_form() {
        let doc = ok(r#"{"00100010":{"Value":[{"Alphabetic":"SMIRNOV^IVAN"}],"vr":"PN"}}"#);
        assert_eq!(doc.attribute_pn("00100010"), Some("SMIRNOV^IVAN"));
        // A producer that sends a bare string still reads.
        let doc = ok(r#"{"00100010":{"Value":["PLAIN"],"vr":"PN"}}"#);
        assert_eq!(doc.attribute_pn("00100010"), Some("PLAIN"));
    }

    #[test]
    fn nested_containers_and_empty_ones() {
        assert_eq!(ok("{}"), Json::Obj(Vec::new()));
        assert_eq!(ok("[]"), Json::Arr(Vec::new()));
        let doc = ok(r#"{"a":[{"b":[1,2]}],"c":{}}"#);
        let inner = doc.get("a").expect("a");
        assert_eq!(inner.as_array().expect("array").len(), 1);
        assert!(doc.get("c").expect("c").is_object());
    }

    #[test]
    fn duplicate_keys_keep_the_first_as_get_does() {
        // JSON allows duplicate names; `get` returns the first, which is
        // what every producer here sends anyway.
        let doc = ok(r#"{"a":1,"a":2}"#);
        assert_eq!(doc.get("a"), Some(&Json::U64(1)));
    }

    #[test]
    fn escapes_decode() {
        assert_eq!(ok(r#""a\"b""#), Json::Str("a\"b".into()));
        assert_eq!(ok(r#""a\\b""#), Json::Str("a\\b".into()));
        assert_eq!(ok(r#""a\/b""#), Json::Str("a/b".into()));
        assert_eq!(ok(r#""\b\f\n\r\t""#), Json::Str("\u{8}\u{c}\n\r\t".into()));
        assert_eq!(ok(r#""\u0041""#), Json::Str("A".into()));
        assert_eq!(ok(r#""\u00e9""#), Json::Str("é".into()));
        // A surrogate pair is one character, not two replacement glyphs.
        assert_eq!(ok(r#""\ud83d\ude00""#), Json::Str("😀".into()));
    }

    #[test]
    fn utf8_passes_through_verbatim() {
        let doc = ok(r#"{"00100010":{"Value":[{"Alphabetic":"ПЕТРОВ^ИВАН"}]}}"#);
        assert_eq!(doc.attribute_pn("00100010"), Some("ПЕТРОВ^ИВАН"));
    }

    #[test]
    fn malformed_documents_are_refused_with_a_position() {
        for (text, needle) in [
            ("{", "expected '\"'"),
            ("}", "unexpected byte"),
            ("{\"a\"}", "expected ':'"),
            ("{\"a\":1", "expected ',' or '}'"),
            ("[1,", "unexpected end"),
            ("\"unterminated", "unterminated string"),
            ("'single'", "unexpected byte"),
            ("tru", "expected 'true'"),
            ("01", "leading zero"),
            ("1.", "expected a digit after '.'"),
            ("1e", "expected a digit in the exponent"),
            ("{\"a\":1} extra", "trailing content"),
            (r#""\q""#, "unknown escape"),
            (r#""\u12""#, "\\u escape is short"),
            (r#""\ud800""#, "not followed by a low surrogate"),
            (r#""\udc00""#, "unpaired low surrogate"),
        ] {
            let error = err(text);
            assert!(
                error.message.contains(needle),
                "for {text:?} expected {needle:?}, got {}",
                error.message
            );
        }
    }

    #[test]
    fn a_control_byte_inside_a_string_is_refused() {
        let error = err("\"a\nb\"");
        assert!(error.message.contains("control byte"), "{}", error.message);
    }

    #[test]
    fn non_utf8_input_is_refused() {
        let error = parse(&[b'"', 0xFF, 0xFE, b'"']).expect_err("bad utf8");
        assert!(error.message.contains("UTF-8"), "{}", error.message);
    }

    #[test]
    fn deep_nesting_is_refused_rather_than_overflowing() {
        let text = format!(
            "{}1{}",
            "[".repeat(MAX_DEPTH + 1),
            "]".repeat(MAX_DEPTH + 1)
        );
        let error = parse(text.as_bytes()).expect_err("too deep");
        assert!(error.message.contains("nested deeper"), "{}", error.message);
    }

    #[test]
    fn a_document_must_be_an_object_when_the_caller_says_so() {
        assert!(parse_object(b"[1]").is_err());
        assert!(parse_object(br#"{"a":1}"#).is_ok());
    }

    #[test]
    fn a_large_integer_becomes_a_float_rather_than_failing() {
        assert_eq!(ok("18446744073709551616"), Json::F64(1.8446744073709552e19));
    }

    // -- single_dataset (PS3.18 F.2) --------------------------------------

    #[test]
    fn a_bare_object_is_its_own_single_dataset() {
        // F.2.2: a single Data Set is one JSON object, which is the form
        // Clarus emits.
        let value = ok(r#"{"00080018":{"Value":["1.2.3"]}}"#);
        assert_eq!(
            single_dataset(&value).and_then(|d| d.attribute("00080018")),
            Some("1.2.3")
        );
    }

    #[test]
    fn an_array_of_one_object_is_that_object() {
        // F.2.1: the field form dcm4chee-arc emits for a single retrieve.
        // Reading it as an array is the failure this prevents - the
        // attribute lookup would return None and the workitem would read as
        // carrying no StudyInstanceUID.
        let value = ok(r#"[{"00080018":{"Value":["1.2.3"]}}]"#);
        assert_eq!(
            single_dataset(&value).and_then(|d| d.attribute("00080018")),
            Some("1.2.3")
        );
    }

    #[test]
    fn both_spellings_of_one_dataset_are_indistinguishable() {
        // The point of the helper: a reader must not be able to tell which
        // server answered. Comparing the unwrapped values is how that is
        // pinned, rather than comparing each against a literal.
        let object = ok(r#"{"00080018":{"Value":["1.2.3"]}}"#);
        let array = ok(r#"[{"00080018":{"Value":["1.2.3"]}}]"#);
        assert_eq!(single_dataset(&object), single_dataset(&array));
    }

    #[test]
    fn nothing_else_names_a_single_dataset() {
        // Every refusal case at once: an empty array (no result), an array
        // of two (a search answer, not one dataset), an array of one
        // non-object (not a dataset), and the scalars.
        for (document, why) in [
            ("[]", "an empty array has no dataset"),
            (
                r#"[{"00080018":{"Value":["1.1"]}},{"00080018":{"Value":["1.2"]}}]"#,
                "two datasets are a search answer",
            ),
            ("[1]", "an array of a scalar is not a dataset"),
            ("[\"text\"]", "an array of a string is not a dataset"),
            (
                "{}",
                "an EMPTY object is a dataset, so this one asserts the other way",
            ),
            ("null", "null is not a dataset"),
            ("true", "a boolean is not a dataset"),
            ("1", "a number is not a dataset"),
            ("\"text\"", "a string is not a dataset"),
        ] {
            let value = ok(document);
            if document == "{}" {
                // An object with no attributes is still an object: an empty
                // dataset is legal and the caller decides whether its
                // attributes suffice.
                assert!(single_dataset(&value).is_some(), "{why}");
                continue;
            }
            assert!(single_dataset(&value).is_none(), "{why}");
        }
    }
}
