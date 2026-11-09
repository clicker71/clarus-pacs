// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Scalar access on a parsed configuration document.
//!
//! Two rules run through everything here.
//!
//! Unknown keys are errors. A misspelled key would otherwise leave the
//! default in place and the administrator would be debugging behaviour
//! that does not correspond to the file on disk. Every mapping in the
//! configuration is closed: the typed layer walks it, and any key left
//! over is reported with its own name.
//!
//! A wrong type is an error too, for the same reason: `width: "640"` is a
//! string, and accepting it silently would make the parser look lenient
//! while the file stays wrong.

use crate::yaml::Yaml;
use std::fmt;
use std::path::{Path, PathBuf};

/// A configuration problem, reported with the location that caused it.
#[derive(Debug)]
pub struct ConfError {
    /// Where in the file, e.g. `rules[0].result.kind`.
    pub at: String,
    /// The file this came from, when the problem came from a file.
    pub path: Option<PathBuf>,
    pub message: String,
}

impl ConfError {
    pub fn new(at: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            at: at.into(),
            path: None,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn in_file(mut self, path: &Path) -> Self {
        self.path = Some(path.to_path_buf());
        self
    }

    /// `unknown key 'server.base_uri'`
    pub fn unknown_key(at: &str) -> Self {
        Self::new(at, "unknown key")
    }

    /// `expected an integer, found "640"`
    pub fn type_error(at: &str, expected: &str, found: &Yaml) -> Self {
        Self::new(
            at,
            format!("expected {expected}, found {}", found.describe()),
        )
    }
}

impl fmt::Display for ConfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}: {}", path.display(), self.at, self.message),
            None => write!(f, "{}: {}", self.at, self.message),
        }
    }
}

impl std::error::Error for ConfError {}

/// The error type of every configuration accessor.
pub type Result<T> = std::result::Result<T, ConfError>;

/// A mapping being read, with a record of which keys were claimed.
///
/// The record is what makes "unknown key is an error" true without a
/// hand-maintained allowlist that would drift away from the struct: a key
/// nobody asked for stays unclaimed, and [`Section::finish`] reports it.
///
/// Each accessor claims its key as it reads it, so the set of accepted
/// keys is derived from the code that consumes the values rather than
/// maintained alongside it.
pub struct Section<'a> {
    map: &'a Yaml,
    at: String,
    used: Vec<String>,
}

impl<'a> Section<'a> {
    /// Wrap a value known to be a mapping, naming it for messages.
    pub fn wrap(map: &'a Yaml, at: impl Into<String>) -> Result<Section<'a>> {
        let at = at.into();
        if !matches!(map, Yaml::Map(_)) {
            return Err(ConfError::type_error(&at, "a mapping", map));
        }
        Ok(Section {
            map,
            at,
            used: Vec::new(),
        })
    }

    /// Open a nested section by key; `None` when the key is absent.
    ///
    /// Two lifetimes rather than one: the child borrows the document, not
    /// the parent, so a parent can keep lending out sections after this
    /// call and both can be finished in either order.
    pub fn open<'b>(parent: &mut Section<'a>, key: &str) -> Result<Option<Section<'a>>>
    where
        'a: 'b,
    {
        let at = parent.child_at(key);
        let Some(value) = parent.value(key) else {
            return Ok(None);
        };
        Ok(Some(Section {
            map: value,
            at,
            used: Vec::new(),
        }))
    }

    #[must_use]
    pub fn at(&self) -> &str {
        &self.at
    }

    /// Path of a child key, for messages: `rules[0].result.kind`.
    #[must_use]
    pub fn child_at(&self, key: &str) -> String {
        if self.at.is_empty() {
            key.to_string()
        } else {
            format!("{}.{}", self.at, key)
        }
    }

    /// The value of `key`, claimed.
    fn value(&mut self, key: &str) -> Option<&'a Yaml> {
        let found = self.map.get(key);
        if found.is_some() {
            self.used.push(key.to_string());
        }
        found
    }

    /// Look at a key without claiming it, for parsers that read the whole
    /// value themselves and claim it afterwards.
    #[must_use]
    pub fn peek(&self, key: &str) -> Option<&'a Yaml> {
        self.map.get(key)
    }

    /// Take a subtree and claim its key. Use this instead of [`Section::open`]
    /// when the parent and the child are both alive afterwards: the returned
    /// value borrows the document, not the parent, so the parent can keep
    /// lending out siblings.
    pub fn take(&mut self, key: &str) -> Result<Option<&'a Yaml>> {
        let found = self.map.get(key);
        if let Some(value) = found {
            self.used.push(key.to_string());
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    /// Claim a key the caller read itself, for values with a grammar of
    /// their own (a tag map, whose keys are DICOM tags rather than names).
    pub fn claim(&mut self, key: &str) {
        self.used.push(key.to_string());
    }

    pub fn require_str(&mut self, key: &str) -> Result<&'a str> {
        let at = self.child_at(key);
        let value = self
            .value(key)
            .ok_or_else(|| ConfError::new(&at, "missing required key"))?;
        as_str(value, &at)
    }

    pub fn opt_str(&mut self, key: &str) -> Result<Option<&'a str>> {
        let at = self.child_at(key);
        match self.value(key) {
            Some(value) => Ok(Some(as_str(value, &at)?)),
            None => Ok(None),
        }
    }

    pub fn opt_bool(&mut self, key: &str) -> Result<Option<bool>> {
        let at = self.child_at(key);
        match self.value(key) {
            Some(Yaml::Bool(value)) => Ok(Some(*value)),
            Some(other) => Err(ConfError::type_error(&at, "a boolean", other)),
            None => Ok(None),
        }
    }

    /// A bounded integer. Out-of-range values name the bound, because
    /// "out of range" alone sends the administrator back to the docs.
    pub fn opt_bounded_int(&mut self, key: &str, low: i64, high: i64) -> Result<Option<i64>> {
        let at = self.child_at(key);
        match self.value(key) {
            Some(value) => {
                let parsed = as_int(value, &at)?;
                if parsed < low || parsed > high {
                    return Err(ConfError::new(
                        &at,
                        format!("{parsed} is outside {low}..{high}"),
                    ));
                }
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    /// An integer with no bound of its own.
    ///
    /// For keys whose range is not a constant: `engine.workers` is capped by
    /// the number of configured models, which is not known where the engine
    /// section is parsed. The caller applies its own rule and names it in
    /// the message, which is the reason this exists next to
    /// [`Self::opt_bounded_int`] rather than replacing it.
    pub fn opt_int(&mut self, key: &str) -> Result<Option<i64>> {
        let at = self.child_at(key);
        match self.value(key) {
            Some(value) => Ok(Some(as_int(value, &at)?)),
            None => Ok(None),
        }
    }

    /// A byte count, written either as a bare integer or with a `k`/`m`
    /// suffix (`"64m"`), matching the `<n>` unit convention configuration
    /// already uses for sizes.
    ///
    /// Both forms are accepted because both are useful: a bare integer is
    /// unambiguous and greppable, while `64m` is what an operator writes
    /// from the documentation. Fractions are refused (`"1.5m"`), because a
    /// half-megabyte is not a thing anyone needed and a silent rounding
    /// would be worse than the error.
    ///
    /// The value is returned UNSIGNED after the sign check: a negative byte
    /// count has no meaning, and saying so here keeps every caller from
    /// repeating the test.
    pub fn opt_size(&mut self, key: &str) -> Result<Option<u64>> {
        let at = self.child_at(key);
        let Some(value) = self.value(key) else {
            return Ok(None);
        };
        match value {
            Yaml::Int(number) => {
                let unsigned = u64::try_from(*number)
                    .map_err(|_| ConfError::new(&at, format!("{number} is not a byte count")))?;
                Ok(Some(unsigned))
            }
            Yaml::Str(text) => Ok(Some(parse_size(text, &at)?)),
            other => Err(ConfError::type_error(&at, "a byte count", other)),
        }
    }

    pub fn opt_float(&mut self, key: &str) -> Result<Option<f64>> {
        let at = self.child_at(key);
        match self.value(key) {
            Some(Yaml::Float(value)) => Ok(Some(*value)),
            Some(Yaml::Int(value)) => Ok(Some(*value as f64)),
            Some(other) => Err(ConfError::type_error(&at, "a number", other)),
            None => Ok(None),
        }
    }

    /// A list of strings. An absent key yields an empty vector.
    pub fn opt_str_list(&mut self, key: &str) -> Result<Vec<&'a str>> {
        let at = self.child_at(key);
        let Some(value) = self.value(key) else {
            return Ok(Vec::new());
        };
        str_items(value, &at)
    }

    /// A list of strings that must not be empty when the key is present.
    pub fn require_str_list(&mut self, key: &str) -> Result<Vec<&'a str>> {
        let at = self.child_at(key);
        let items = self.opt_str_list(key)?;
        if items.is_empty() {
            return Err(ConfError::new(&at, "the list is empty"));
        }
        Ok(items)
    }

    /// A list of mappings, each opened as its own section.
    pub fn opt_section_list(&mut self, key: &str) -> Result<Vec<Section<'a>>> {
        let at = self.child_at(key);
        let Some(value) = self.value(key) else {
            return Ok(Vec::new());
        };
        let Some(items) = value.as_list() else {
            return Err(ConfError::type_error(&at, "a list", value));
        };
        let mut out = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            out.push(Section::wrap(item, format!("{at}[{index}]"))?);
        }
        Ok(out)
    }

    /// A mapping keyed by an arbitrary name (a model id).
    pub fn opt_mapping(&mut self, key: &str) -> Result<Option<Section<'a>>> {
        let at = self.child_at(key);
        let Some(value) = self.value(key) else {
            return Ok(None);
        };
        Ok(Some(Section::wrap(value, at)?))
    }

    /// The entries of this mapping, in file order.
    #[must_use]
    pub fn entries(&self) -> Vec<(&'a str, &'a Yaml)> {
        match self.map {
            Yaml::Map(entries) => entries.iter().map(|(k, v)| (k.as_str(), v)).collect(),
            _ => Vec::new(),
        }
    }

    /// Report every key of this mapping that was never claimed.
    pub fn finish(self, what: &str) -> Result<()> {
        for (key, _) in self.entries() {
            if !self.used.iter().any(|used| used == key) {
                let named = if self.at.is_empty() {
                    key.to_string()
                } else {
                    format!("{}.{}", self.at, key)
                };
                return Err(ConfError::new(named, format!("unknown key in {what}")));
            }
        }
        Ok(())
    }
}

/// The string items of a list value.
pub fn str_items<'a>(value: &'a Yaml, at: &str) -> Result<Vec<&'a str>> {
    let Some(items) = value.as_list() else {
        return Err(ConfError::type_error(at, "a list", value));
    };
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        out.push(as_str(item, &format!("{at}[{index}]"))?);
    }
    Ok(out)
}

fn as_str<'a>(value: &'a Yaml, at: &str) -> Result<&'a str> {
    match value {
        Yaml::Str(text) => Ok(text),
        other => Err(ConfError::type_error(at, "a string", other)),
    }
}

fn as_int(value: &Yaml, at: &str) -> Result<i64> {
    match value {
        Yaml::Int(number) => Ok(*number),
        other => Err(ConfError::type_error(at, "an integer", other)),
    }
}

/// Decode a `<n>` / `<n>k` / `<n>m` byte count.
///
/// The suffix convention is the one configuration already uses for sizes
/// (`max_json_body = 1m`): `k`/`K` is 1024, `m`/`M` is 1024 squared, and
/// anything else is a plain count. Fractions are refused rather than
/// rounded, and so is trailing junk - `"64mb"` is a typo, not a value.
fn parse_size(text: &str, at: &str) -> Result<u64> {
    let malformed = || {
        ConfError::new(
            at,
            format!("expected a byte count like \"64m\" or \"67108864\", got \"{text}\""),
        )
    };
    let multiplier: u64 = match text.as_bytes().last() {
        Some(b'k') | Some(b'K') => 1024,
        Some(b'm') | Some(b'M') => 1024 * 1024,
        _ => 1,
    };
    let digits = if multiplier == 1 {
        text
    } else {
        &text[..text.len() - 1]
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }
    let count: u64 = digits.parse().map_err(|_| malformed())?;
    count.checked_mul(multiplier).ok_or_else(malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml::parse_document;

    /// The document is leaked so a section can borrow it for the length of
    /// the test body; tests are short-lived processes.
    fn document(source: &str) -> &'static Yaml {
        Box::leak(Box::new(
            parse_document(source, Path::new("t")).expect("parses"),
        ))
    }

    #[test]
    fn an_unclaimed_key_is_reported_with_its_name() {
        let doc = document("server:\n  base_url: \"x\"\n  base_uri: \"y\"\n");
        let mut root = Section::wrap(doc, "").expect("mapping");
        root.require_str("server").expect_err("a string, not a map");
        let mut server = Section::open(&mut root, "server")
            .expect("opens")
            .expect("present");
        assert_eq!(server.require_str("base_url").expect("str"), "x");
        root.finish("the file").expect("root keys claimed");
        let err = server.finish("server").expect_err("unknown key");
        assert!(err.to_string().contains("server.base_uri"), "{err}");
        assert!(err.to_string().contains("unknown key"), "{err}");
    }

    #[test]
    fn a_wrong_type_names_what_was_found() {
        let doc = document("width: \"640\"\n");
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = root.opt_bounded_int("width", 1, 10).expect_err("type");
        assert!(err.to_string().contains("expected an integer"), "{err}");
        assert!(err.to_string().contains("\"640\""), "{err}");
    }

    #[test]
    fn a_bound_is_named_in_the_message() {
        let doc = document("streams: 9\n");
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = root
            .opt_bounded_int("streams", 1, 4)
            .expect_err("out of range");
        assert!(err.to_string().contains("outside 1..4"), "{err}");
    }

    #[test]
    fn a_missing_required_key_says_so() {
        let doc = document("other: 1\n");
        let mut root = Section::wrap(doc, "").expect("mapping");
        let err = root.require_str("base_url").expect_err("missing");
        assert!(err.to_string().contains("missing required key"), "{err}");
    }

    #[test]
    fn an_explicitly_claimed_key_passes_finish() {
        let doc = document("a: 1\n");
        let mut root = Section::wrap(doc, "").expect("mapping");
        assert!(root.entries().iter().any(|(key, _)| *key == "a"));
        root.claim("a");
        root.finish("root").expect("claimed");
    }
}
