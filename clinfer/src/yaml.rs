// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A YAML subset, parsed by hand.
//!
//! The configuration file is written by administrators and read once at
//! startup, so the parser accepts exactly the shapes the reference file
//! uses and rejects everything else with a line number. A full YAML
//! implementation would accept documents this program has no meaning for
//! (anchors, aliases, multiple documents, tags, flow sequences); accepting
//! them and then failing somewhere in the typed layer would report the
//! problem far from its cause.
//!
//! Supported:
//!
//! ```text
//! section:                     nested mapping, two-space steps
//!   key: value                 scalar: string, int, float, bool
//!   list:                      sequence, either at the key's own indent
//!     - entry                  with the dash taking the first column
//!     - key: 1                 an item that is itself a mapping
//!       key: 2
//!   inline: { a: 1, b: 2 }     flow mapping (and [a, b] for sequences)
//!   flow: [a, b]               flow sequence
//! # comment                    comments are stripped outside quotes
//! ```
//!
//! Rejected: tabs in indentation, anchors and aliases (`&name`, `*name`),
//! tags (`!!str`), document markers (`---`, `...`), block scalars
//! (`|`, `>`), duplicate keys, and any value that cannot be classified.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Maximum accepted nesting depth of the parser's own recursion.
///
/// The reference configuration nests four levels; the bound exists so that
/// a hand-edited file cannot reach the process stack limit through
/// recursion, and so that the failure names the parser rather than
/// arriving as a crash.
const MAX_DEPTH: usize = 16;

/// A parsed value.
///
/// Maps keep insertion order: rule order is load-bearing (the first
/// matching rule wins), and a diagnostics dump of the file reads in the
/// order the administrator wrote it.
#[derive(Debug, Clone, PartialEq)]
pub enum Yaml {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<Yaml>),
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    /// The map entry with this key, or `None`. Never reports a type
    /// error - the typed layer decides what an unexpected type means.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Self::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_list(&self) -> Option<&[Yaml]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }

    /// The keys, in file order. `None` for a scalar.
    #[must_use]
    pub fn keys(&self) -> Option<Vec<&str>> {
        match self {
            Self::Map(entries) => Some(entries.iter().map(|(k, _)| k.as_str()).collect()),
            _ => None,
        }
    }

    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Str(_) => "a string",
            Self::Int(_) => "an integer",
            Self::Float(_) => "a number",
            Self::Bool(_) => "a boolean",
            Self::List(_) => "a list",
            Self::Map(_) => "a mapping",
        }
    }

    /// How to write this value back into a message an administrator reads.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Str(s) => format!("\"{s}\""),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => format!("{f}"),
            Self::Bool(b) => b.to_string(),
            Self::List(_) => "a list".to_string(),
            Self::Map(_) => "a mapping".to_string(),
        }
    }
}

// -- Errors ------------------------------------------------------------

#[derive(Debug)]
pub enum YamlError {
    /// The file could not be read.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The document is syntactically outside the accepted subset.
    Syntax { line: usize, message: String },
    /// The file parses, but not into the shape a config file must have.
    Shape { message: String },
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            Self::Syntax { line, message } => write!(f, "line {line}: {message}"),
            Self::Shape { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for YamlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

type Result<T> = std::result::Result<T, YamlError>;

// -- Line classification -----------------------------------------------

/// One source line, with its comment removed and its indentation measured.
struct Line {
    /// 1-based, for messages.
    number: usize,
    /// Indentation of the first token, in characters. Tabs are rejected,
    /// so this counts spaces.
    indent: usize,
    /// Content from the first non-space character to just before the
    /// comment, right-trimmed. May be empty.
    text: String,
}

impl Line {
    fn is_blank(&self) -> bool {
        self.text.is_empty()
    }

    /// True when the line opens a sequence item.
    fn is_item(&self) -> bool {
        self.text == "-" || self.text.starts_with("- ")
    }

    fn syntax(&self, message: impl Into<String>) -> YamlError {
        YamlError::Syntax {
            line: self.number,
            message: message.into(),
        }
    }
}

/// Split a source file into non-blank lines with comments removed.
///
/// Comments are stripped only outside quotes: `base_url: "http://x/#frag"`
/// keeps its fragment, and `identity_user: "a#b"` keeps its hash.
fn split_lines(source: &str) -> Result<Vec<Line>> {
    let mut out = Vec::new();
    for (index, raw) in source.lines().enumerate() {
        let number = index + 1;
        let bytes = raw.as_bytes();
        let mut indent = 0;
        while indent < bytes.len() && bytes[indent] == b' ' {
            indent += 1;
        }
        if bytes.get(indent) == Some(&b'\t') {
            return Err(YamlError::Syntax {
                line: number,
                message: "tab in indentation; use spaces".to_string(),
            });
        }
        let body = &raw[indent..];

        // Document markers and block scalars are outside the subset.
        if body == "---" || body == "..." {
            return Err(YamlError::Syntax {
                line: number,
                message: "document markers are not supported".to_string(),
            });
        }
        if let Some(key) = body.strip_suffix(':')
            && (key.ends_with('|') || key.ends_with('>'))
        {
            return Err(YamlError::Syntax {
                line: number,
                message: "block scalars are not supported".to_string(),
            });
        }
        if body.starts_with('&') || body.starts_with('*') {
            return Err(YamlError::Syntax {
                line: number,
                message: "anchors and aliases are not supported".to_string(),
            });
        }
        if body.starts_with("!!") {
            return Err(YamlError::Syntax {
                line: number,
                message: "explicit tags are not supported".to_string(),
            });
        }

        let text = strip_comment(body, number)?;
        out.push(Line {
            number,
            indent,
            text,
        });
    }
    Ok(out)
}

/// Remove a trailing `#` comment, ignoring hashes inside double quotes.
///
/// YAML 1.2 6.2.2: a comment starts at `#` only when it is the first character
/// of the line or follows whitespace. A `#` inside an unquoted value is part of
/// that value - a URL fragment (`.../study#viewer`) is the everyday case - and
/// cutting there silently truncated the value the operator wrote.
fn strip_comment(body: &str, number: usize) -> Result<String> {
    let mut out = String::with_capacity(body.len());
    let mut in_quotes = false;
    let mut previous_is_space = true; // the line start counts as one
    for c in body.chars() {
        if in_quotes {
            out.push(c);
            if c == '"' {
                in_quotes = false;
            }
        } else if c == '"' {
            in_quotes = true;
            out.push(c);
        } else if c == '#' && previous_is_space {
            break;
        } else {
            out.push(c);
        }
        if !in_quotes {
            previous_is_space = c == ' ' || c == '\t';
        }
    }
    if in_quotes {
        // A quote that never closes would silently swallow the rest of
        // the file as one long value.
        return Err(YamlError::Syntax {
            line: number,
            message: "unterminated quoted string".to_string(),
        });
    }
    Ok(out.trim_end().to_string())
}

// -- Document parser ---------------------------------------------------

/// Parse a document that must be a mapping at the top level.
pub fn parse_document(source: &str, path: &Path) -> Result<Yaml> {
    // YAML 1.2 5.2 requires a parser to accept a byte order mark at the
    // start of a stream: it is an encoding signature, not content, and
    // leaving it in makes the first key `\u{feff}server` rather than
    // `server`. Editors on Windows write it, so accepting it is the
    // difference between a file that opens and one that is refused for a
    // character the operator cannot see.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let lines = split_lines(source)?;
    let mut cursor = Cursor { lines, pos: 0 };
    // Drop leading blank lines so the root indent is the first real token's.
    cursor.skip_blank();
    let first = match cursor.peek() {
        Some(line) => line,
        None => {
            return Err(YamlError::Shape {
                message: format!("{}: the file is empty", path.display()),
            });
        }
    };
    if first.indent != 0 {
        return Err(YamlError::Syntax {
            line: first.number,
            message: "top-level keys must start in column 1".to_string(),
        });
    }
    let root = cursor.parse_map(0, 1)?;
    Ok(root)
}

/// The subset parser reads a document into values before types exist, so the
/// only keys it knows are the syntax ones.
struct Cursor {
    lines: Vec<Line>,
    pos: usize,
}

impl Cursor {
    fn peek(&self) -> Option<&Line> {
        self.lines.get(self.pos)
    }

    fn skip_blank(&mut self) {
        while self.peek().is_some_and(Line::is_blank) {
            self.pos += 1;
        }
    }

    /// Parse a mapping whose keys are indented `indent`. `depth` guards the
    /// recursion.
    fn parse_map(&mut self, indent: usize, depth: usize) -> Result<Yaml> {
        let mut entries: Vec<(String, Yaml)> = Vec::new();
        loop {
            self.skip_blank();
            let Some(line) = self.peek() else { break };
            if line.indent < indent {
                break;
            }
            if line.is_item() {
                // An item belongs to the list above this map.
                if line.indent == indent {
                    break;
                }
                return Err(line.syntax(format!(
                    "unexpected list item; expected a key indented {indent}"
                )));
            }
            if line.indent > indent {
                return Err(line.syntax(format!(
                    "unexpected indentation; this key belongs at indent {indent}"
                )));
            }
            let number = line.number;
            let text = line.text.clone();
            self.pos += 1;
            let (key, value) = self.parse_key(&text, number, indent, depth)?;
            if entries.iter().any(|(existing, _)| *existing == key) {
                return Err(YamlError::Syntax {
                    line: number,
                    message: format!("duplicate key '{key}'"),
                });
            }
            entries.push((key, value));
        }
        Ok(Yaml::Map(entries))
    }

    /// Parse `key:` or `key: rest` and whatever value follows it.
    fn parse_key(
        &mut self,
        text: &str,
        number: usize,
        indent: usize,
        depth: usize,
    ) -> Result<(String, Yaml)> {
        let fail = |message: &str| YamlError::Syntax {
            line: number,
            message: message.to_string(),
        };
        let colon = find_key_colon(text).ok_or_else(|| fail("expected 'key:'"))?;
        let key = unquote_key(text[..colon].trim(), number)?;
        if key.is_empty() {
            return Err(fail("empty key"));
        }
        let key = key.to_string();
        let rest = text[colon + 1..].trim().to_string();

        if !rest.is_empty() {
            let value = parse_scalar(&rest, number, depth)?;
            return Ok((key, value));
        }

        let value = self.parse_block(indent, depth, number)?;
        Ok((key, value))
    }

    /// Parse a value written below `key:` - either an indented block or an
    /// empty value.
    fn parse_block(&mut self, indent: usize, depth: usize, key_line: usize) -> Result<Yaml> {
        let fail = |message: String| YamlError::Syntax {
            line: key_line,
            message,
        };
        if depth >= MAX_DEPTH {
            return Err(fail(format!("nesting deeper than {MAX_DEPTH} levels")));
        }
        self.skip_blank();
        let Some(next) = self.peek() else {
            return Ok(Yaml::Map(Vec::new()));
        };
        if next.indent < indent {
            // The key stands alone at the end of a block.
            return Ok(Yaml::Map(Vec::new()));
        }
        if next.is_item() {
            let item_indent = next.indent;
            return self.parse_list(item_indent, depth + 1);
        }
        if next.indent == indent {
            // A sibling key at the same column: this key holds an empty
            // mapping, which is how a section with no content reads.
            return Ok(Yaml::Map(Vec::new()));
        }
        // A section header indents its children by one level. The step is
        // not fixed: administrators align list items under the key they
        // belong to, and those lines sit deeper than this mapping's
        // siblings. Any consistent deeper indent is accepted.
        let child_indent = next.indent;
        self.parse_map(child_indent, depth + 1)
    }

    fn parse_list(&mut self, indent: usize, depth: usize) -> Result<Yaml> {
        let mut items = Vec::new();
        loop {
            self.skip_blank();
            let Some(line) = self.peek() else { break };
            if line.indent != indent || !line.is_item() {
                if line.indent > indent {
                    return Err(line.syntax(format!(
                        "unexpected indentation; list items align at column {indent}"
                    )));
                }
                break;
            }
            let number = line.number;
            let text = line.text.clone();
            self.pos += 1;
            let body = text
                .strip_prefix("- ")
                .map(str::trim_start)
                .unwrap_or("")
                .to_string();
            if body.is_empty() {
                return Err(YamlError::Syntax {
                    line: number,
                    message: "list item is empty".to_string(),
                });
            }
            let item_indent = indent + 2;
            if find_key_colon(&body).is_some() {
                // An item that opens a mapping: its first key sits on the
                // dash line, its siblings align under that key.
                let (key, first_value) = self.parse_key(&body, number, item_indent, depth + 1)?;
                let mut entries = vec![(key, first_value)];
                // `parse_map` checks duplicates among the SIBLINGS it reads, but
                // the key on the dash line is not one of them - it was parsed
                // here. A list item that repeated it therefore produced a
                // mapping with the same key twice, and whichever copy the
                // consumer picked, the document said one thing and meant two.
                let dash_key = entries[0].0.clone();
                let following = self.parse_map(item_indent, depth + 1)?;
                if let Yaml::Map(rest) = following {
                    if let Some((dup, _)) = rest.iter().find(|(k, _)| *k == dash_key) {
                        return Err(YamlError::Syntax {
                            line: number,
                            message: format!("duplicate key '{dup}' in list item"),
                        });
                    }
                    entries.extend(rest);
                }
                items.push(Yaml::Map(entries));
            } else {
                items.push(parse_scalar(&body, number, depth)?);
            }
        }
        Ok(Yaml::List(items))
    }
}

/// Column of the `:` that separates a key from its value, or `None` when
/// the line is not a mapping entry.
///
/// A colon inside quotes is content (`"a:b"`), and a colon that is not
/// followed by a space or the end of the line is content too (`::1`,
/// `GPU.0`, `127.0.0.1:8124`), which is what keeps IPv6 peers and a
/// `host:port` listener from being mistaken for nested keys.
fn find_key_colon(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (index, c) in text.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == ':' {
                    let rest = &text[index + c.len_utf8()..];
                    if rest.is_empty() || rest.starts_with(' ') {
                        return Some(index);
                    }
                }
            }
        }
    }
    None
}

// A quoted key IS its own value (YAML 1.2 7.3.1): `"00181030"` denotes the
// string `00181030`, exactly as the bare spelling does. Keys that reach the
// typed layer with their quotes attached would look like a different, and
// longer, key - which is how a quoted DICOM tag came to be rejected.
fn unquote_key(key: &str, line: usize) -> Result<&str> {
    let Some(inner) = key.strip_prefix('"') else {
        return Ok(key);
    };
    inner.strip_suffix('"').ok_or_else(|| YamlError::Syntax {
        line,
        message: "unterminated quoted key".to_string(),
    })
}

/// Parse a scalar or a flow collection.
fn parse_scalar(text: &str, line: usize, depth: usize) -> Result<Yaml> {
    let text = text.trim();
    if text.starts_with('{') {
        return parse_flow_map(text, line, depth);
    }
    if text.starts_with('[') {
        return parse_flow_list(text, line, depth);
    }
    if text.starts_with('&') || text.starts_with('*') {
        return Err(YamlError::Syntax {
            line,
            message: "anchors and aliases are not supported".to_string(),
        });
    }
    if text.starts_with('!') {
        return Err(YamlError::Syntax {
            line,
            message: "explicit tags are not supported".to_string(),
        });
    }
    if text.starts_with('|') || text.starts_with('>') {
        return Err(YamlError::Syntax {
            line,
            message: "block scalars are not supported".to_string(),
        });
    }

    let syntax = |message: &str| YamlError::Syntax {
        line,
        message: message.to_string(),
    };

    if let Some(inner) = text.strip_prefix('"') {
        let inner = inner
            .strip_suffix('"')
            .ok_or_else(|| syntax("unterminated quoted string"))?;
        return Ok(Yaml::Str(inner.to_string()));
    }
    if text.contains(['"', '\'', '{', '}', '[', ']']) {
        return Err(syntax(
            "unquoted value contains a quote or a flow delimiter",
        ));
    }

    Ok(match text {
        "true" => Yaml::Bool(true),
        "false" => Yaml::Bool(false),
        _ => match text.parse::<i64>() {
            Ok(value) => Yaml::Int(value),
            Err(_) => match text.parse::<f64>() {
                // Reject the non-finite spellings: the typed layer stores
                // thresholds and would accept them silently.
                Ok(value) if value.is_finite() => Yaml::Float(value),
                _ => Yaml::Str(text.to_string()),
            },
        },
    })
}

fn parse_flow_map(text: &str, line: usize, depth: usize) -> Result<Yaml> {
    if depth >= MAX_DEPTH {
        return Err(YamlError::Syntax {
            line,
            message: format!("nesting deeper than {MAX_DEPTH} levels"),
        });
    }
    let inner = text
        .strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .ok_or_else(|| YamlError::Syntax {
            line,
            message: "unterminated flow mapping".to_string(),
        })?;
    let mut entries: Vec<(String, Yaml)> = Vec::new();
    for field in split_flow(inner) {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let colon = find_key_colon(field).ok_or_else(|| YamlError::Syntax {
            line,
            message: format!("flow mapping entry '{field}' has no ':'"),
        })?;
        let key = unquote_key(field[..colon].trim(), line)?;
        if key.is_empty() {
            return Err(YamlError::Syntax {
                line,
                message: "empty key in flow mapping".to_string(),
            });
        }
        let key = key.to_string();
        if entries.iter().any(|(existing, _)| *existing == key) {
            return Err(YamlError::Syntax {
                line,
                message: format!("duplicate key '{key}' in flow mapping"),
            });
        }
        let value = field[colon + 1..].trim();
        // A flow mapping value may itself be `[..]` or `{..}`.
        entries.push((key, parse_scalar(value, line, depth + 1)?));
    }
    Ok(Yaml::Map(entries))
}

fn parse_flow_list(text: &str, line: usize, depth: usize) -> Result<Yaml> {
    if depth >= MAX_DEPTH {
        return Err(YamlError::Syntax {
            line,
            message: format!("nesting deeper than {MAX_DEPTH} levels"),
        });
    }
    let inner = text
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .ok_or_else(|| YamlError::Syntax {
            line,
            message: "unterminated flow sequence".to_string(),
        })?;
    let mut items = Vec::new();
    let mut rest = inner;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let (span, remainder) = split_first_flow(trimmed);
        items.push(parse_scalar(span, line, depth + 1)?);
        if remainder.is_none() {
            break;
        }
        rest = remainder.expect("checked");
    }
    Ok(Yaml::List(items))
}

/// Split a flow body on top-level commas.
fn split_flow(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let (span, remainder) = split_first_flow(rest);
        out.push(span.to_string());
        match remainder {
            Some(tail) => rest = tail,
            None => break,
        }
    }
    out
}

/// Take the first top-level comma-separated span; `None` when there is no
/// comma left. Quoted spans and nested flow collections are kept whole.
fn split_first_flow(text: &str) -> (&str, Option<&str>) {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (index, c) in text.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    // A closing bracket at depth 0 ends the collection
                    // this span belongs to.
                    if depth == 0 {
                        return (&text[..index], None);
                    }
                    depth -= 1;
                }
                ',' if depth == 0 => {
                    return (&text[..index], Some(&text[index + c.len_utf8()..]));
                }
                _ => {}
            },
        }
    }
    (text, None)
}

// -- Entry point -------------------------------------------------------

/// Read and parse a configuration file.
pub fn parse_file(path: &Path) -> Result<Yaml> {
    let source = fs::read_to_string(path).map_err(|source| YamlError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_document(&source, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<Yaml> {
        parse_document(source, Path::new("test.conf"))
    }

    fn syntax_error(source: &str) -> YamlError {
        parse(source).expect_err("expected a parse error")
    }

    #[test]
    fn scalars_are_typed() {
        let doc = parse("a: text\nb: 12\nc: -3\nd: 1.5\ne: true\nf: false\ng: \"12\"\nh: 1e3\n")
            .expect("parses");
        assert_eq!(doc.get("a"), Some(&Yaml::Str("text".into())));
        assert_eq!(doc.get("b"), Some(&Yaml::Int(12)));
        assert_eq!(doc.get("c"), Some(&Yaml::Int(-3)));
        assert_eq!(doc.get("d"), Some(&Yaml::Float(1.5)));
        assert_eq!(doc.get("e"), Some(&Yaml::Bool(true)));
        assert_eq!(doc.get("f"), Some(&Yaml::Bool(false)));
        assert_eq!(doc.get("g"), Some(&Yaml::Str("12".into())));
        assert_eq!(doc.get("h"), Some(&Yaml::Float(1000.0)));
    }

    #[test]
    fn a_colon_that_is_not_a_separator_stays_in_the_value() {
        let doc = parse("listen: 127.0.0.1:8124\nbase: \"http://x/#f\"\n").expect("parses");
        assert_eq!(doc.get("listen"), Some(&Yaml::Str("127.0.0.1:8124".into())));
        assert_eq!(doc.get("base"), Some(&Yaml::Str("http://x/#f".into())));
    }

    #[test]
    fn nested_maps_take_the_children_indent() {
        let doc = parse("server:\n  base_url: \"x\"\n  port: 1\nengine:\n  cache_dir: \"./c\"\n")
            .expect("parses");
        let server = doc.get("server").expect("server");
        assert_eq!(server.get("base_url"), Some(&Yaml::Str("x".into())));
        assert_eq!(server.get("port"), Some(&Yaml::Int(1)));
        assert_eq!(
            doc.get("engine").and_then(|e| e.get("cache_dir")),
            Some(&Yaml::Str("./c".into()))
        );
    }

    #[test]
    fn lists_of_scalars_and_of_maps() {
        let doc = parse(
            "peers:\n  - \"127.0.0.1\"\n  - \"::1\"\ndevices:\n  - id: \"GPU.0\"\n    streams: 2\n  - id: \"GPU.1\"\n    streams: 1\n",
        )
        .expect("parses");
        assert_eq!(
            doc.get("peers").and_then(Yaml::as_list),
            Some([Yaml::Str("127.0.0.1".into()), Yaml::Str("::1".into())].as_slice())
        );
        let devices = doc.get("devices").and_then(Yaml::as_list).expect("list");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[1].get("id"), Some(&Yaml::Str("GPU.1".into())));
        assert_eq!(devices[1].get("streams"), Some(&Yaml::Int(1)));
    }

    #[test]
    fn an_item_body_may_hold_several_lines() {
        let doc = parse(
            "rules:\n  - match:\n      modality: [DX]\n    model: \"m\"\n    result:\n      kind: pr\n",
        )
        .expect("parses");
        let rules = doc.get("rules").and_then(Yaml::as_list).expect("list");
        let rule = &rules[0];
        assert_eq!(
            rule.get("match").and_then(|m| m.get("modality")),
            Some(&Yaml::List(vec![Yaml::Str("DX".into())]))
        );
        assert_eq!(rule.get("model"), Some(&Yaml::Str("m".into())));
        assert_eq!(
            rule.get("result").and_then(|r| r.get("kind")),
            Some(&Yaml::Str("pr".into()))
        );
    }

    #[test]
    fn flow_collections_nest() {
        let doc = parse(
            "resize: { mode: letterbox, pad_value: 0 }\nlist: [a, b]\nparts: [{ a: 1 }, { b: 2 }]\n",
        )
        .expect("parses");
        let resize = doc.get("resize").expect("resize");
        assert_eq!(resize.get("mode"), Some(&Yaml::Str("letterbox".into())));
        assert_eq!(resize.get("pad_value"), Some(&Yaml::Int(0)));
        assert_eq!(
            doc.get("list").and_then(Yaml::as_list),
            Some([Yaml::Str("a".into()), Yaml::Str("b".into())].as_slice())
        );
        let parts = doc.get("parts").and_then(Yaml::as_list).expect("list");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].get("a"), Some(&Yaml::Int(1)));
    }

    #[test]
    fn comments_are_stripped_outside_quotes_only() {
        let doc = parse("# leading\na: 1 # trailing\nb: \"x # y\"\n").expect("parses");
        assert_eq!(doc.get("a"), Some(&Yaml::Int(1)));
        assert_eq!(doc.get("b"), Some(&Yaml::Str("x # y".into())));
    }

    #[test]
    fn a_single_quote_is_not_a_string_delimiter() {
        // Only double quotes delimit scalars, so an apostrophe inside an
        // unquoted value is an error rather than a silent second style.
        let err = syntax_error("a: it's here\n");
        assert!(err.to_string().contains("unquoted value"), "{err}");
    }

    #[test]
    fn a_backslash_is_an_ordinary_character() {
        // Nothing escapes in this subset, so a Windows path passes
        // through untouched rather than being half-decoded.
        let doc = parse("a: \"C:\\\\models\"\n").expect("parses");
        assert_eq!(doc.get("a"), Some(&Yaml::Str("C:\\\\models".into())));
    }

    #[test]
    fn an_empty_item_is_rejected() {
        let err = syntax_error("list:\n  -\n    a: 1\n");
        assert!(err.to_string().contains("list item is empty"), "{err}");
    }

    #[test]
    fn an_empty_section_is_an_empty_mapping() {
        let doc = parse("server:\nother: 1\n").expect("parses");
        assert_eq!(doc.get("server"), Some(&Yaml::Map(Vec::new())));
        assert_eq!(doc.get("other"), Some(&Yaml::Int(1)));
    }

    #[test]
    fn duplicate_keys_are_rejected() {
        let err = syntax_error("a: 1\na: 2\n");
        assert!(err.to_string().contains("duplicate key 'a'"), "{err}");
    }

    /// A repeated key inside a LIST ITEM's mapping. The dash-line key is parsed
    /// by the list reader, so the map reader never saw it and accepted the
    /// repeat - the item then held the same key twice and which copy won
    /// depended on the consumer.
    #[test]
    fn a_duplicate_key_inside_a_list_item_is_rejected() {
        let err = syntax_error("models:\n  - name: a\n    kind: x\n    name: b\n");
        assert!(err.to_string().contains("duplicate key 'name'"), "{err}");
        let err = syntax_error("models:\n  - name: a\n    name: a\n");
        assert!(err.to_string().contains("duplicate key 'name'"), "{err}");
        // Distinct keys in one item still parse.
        assert!(parse("models:\n  - name: a\n    kind: x\n").is_ok());
    }

    #[test]
    fn indentation_must_be_consistent() {
        let err = syntax_error("a:\n  b: 1\n   c: 2\n");
        assert!(err.to_string().contains("unexpected indentation"), "{err}");
    }

    #[test]
    fn tabs_are_rejected() {
        let err = syntax_error("a:\n\tb: 1\n");
        assert!(err.to_string().contains("tab in indentation"), "{err}");
    }

    #[test]
    fn unsupported_yaml_surfaces_is_rejected_with_a_reason() {
        for (source, needle) in [
            ("a: &anchor 1\n", "anchors and aliases"),
            ("a: *alias\n", "anchors and aliases"),
            ("a: !!str 1\n", "explicit tags"),
            ("---\na: 1\n", "document markers"),
            ("a: |\n  text\n", "block scalars"),
            ("a: \"unterminated\n", "unterminated quoted string"),
            ("a: {b: 1\n", "unterminated flow mapping"),
            ("a: [1, 2\n", "unterminated flow sequence"),
        ] {
            let err = syntax_error(source);
            assert!(
                err.to_string().contains(needle),
                "for {source:?} expected {needle:?}, got {err}"
            );
        }
    }

    #[test]
    fn a_line_without_a_colon_is_not_a_key() {
        let err = syntax_error("a: 1\nnonsense\n");
        assert!(err.to_string().contains("expected 'key:'"), "{err}");
    }

    #[test]
    fn a_quoted_key_is_its_own_value() {
        // YAML 1.2 7.3.1: `"00181030"` denotes the string `00181030`, the
        // same key the bare spelling produces. The typed layer would reject
        // the quoted spelling if the quotes survived into the key, because
        // it is not an eight-character hex tag any more.
        let bare = parse("a: { 00181030: [\"*chest*\"] }\n").expect("bare parses");
        let quoted = parse("a: { \"00181030\": [\"*chest*\"] }\n").expect("quoted parses");
        assert_eq!(bare, quoted, "both spellings denote one key");
        assert!(
            bare.get("a")
                .and_then(|inner| inner.get("00181030"))
                .is_some(),
            "the key is the tag, without quotes"
        );
    }

    #[test]
    fn a_quoted_key_works_at_block_indentation_too() {
        // The block form reaches `parse_key` by a different route than the
        // flow form reaches `parse_flow_map`, so both are pinned.
        let doc = parse("outer:\n  \"00181030\": [\"*chest*\"]\n").expect("parses");
        assert!(
            doc.get("outer")
                .and_then(|inner| inner.get("00181030"))
                .is_some(),
            "the quotes are removed in the block form as well"
        );
    }

    #[test]
    fn an_unterminated_quoted_key_is_refused() {
        // An odd number of quotes is caught while the line is still being
        // read, which is earlier than the key layer: the same check that
        // protects a quoted value protects a quoted key.
        let err = syntax_error("a: { \"00181030: [1] }\n");
        assert!(
            err.to_string().contains("unterminated quoted string"),
            "{err}"
        );
        // A key that opens and closes is fine; one that only opens is not,
        // and `unquote_key` is the layer that would otherwise let it
        // through as the literal string `\u{22}00181030`.
        let err = parse("a: { \"00181030: 1, b: 2 }\n").expect_err("refused");
        assert!(
            err.to_string().contains("unterminated quoted string")
                || err.to_string().contains("unterminated quoted key"),
            "{err}"
        );
    }

    #[test]
    fn a_leading_bom_is_not_part_of_the_first_key() {
        // YAML 1.2 5.2: a BOM at the start of a stream is an encoding
        // signature and must be accepted. Leaving it in makes the first key
        // `\u{feff}server`, which no typed lookup matches - and the operator
        // cannot see the character that broke their file.
        let doc = parse("\u{feff}server: 1\nnext: 2\n").expect("a BOM is accepted");
        assert_eq!(
            doc.get("server"),
            Some(&Yaml::Int(1)),
            "the key is 'server'"
        );
        assert_eq!(doc.get("next"), Some(&Yaml::Int(2)));
    }

    #[test]
    fn a_bom_inside_the_file_is_still_ordinary_text() {
        // Only the leading signature is stripped: a BOM elsewhere is a
        // zero-width character in a value, and rewriting it would change
        // data the operator typed.
        let doc = parse("a: \"x\u{feff}y\"\n").expect("parses");
        assert_eq!(doc.get("a"), Some(&Yaml::Str("x\u{feff}y".into())));
    }

    #[test]
    fn the_first_key_must_start_in_column_one() {
        let err = syntax_error("  a: 1\n");
        assert!(err.to_string().contains("column 1"), "{err}");
    }

    #[test]
    fn an_empty_document_is_reported_as_such() {
        let err = parse("").expect_err("empty");
        assert!(err.to_string().contains("file is empty"), "{err}");
    }

    #[test]
    fn keys_keep_file_order() {
        let doc = parse("z: 1\na: 2\n").expect("parses");
        assert_eq!(doc.keys(), Some(vec!["z", "a"]));
    }
}
