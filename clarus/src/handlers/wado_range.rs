// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Range header parsing and 206 Partial Content response helpers for WADO-RS.
//!
//! Implements HTTP Range requests per RFC 7233:
//! - `bytes=0-1023` -> single range
//! - `bytes=1024-` -> suffix range (from offset to end)
//! - `bytes=-2048` -> last N bytes
//!
//! # Invariants
//! - INV-006: Correct `Content-Range: bytes start-end/total` header
//! - Range beyond file size -> 416 Range Not Satisfiable

#![forbid(unsafe_code)]

/// Maximum chunk size for streaming (64 KiB).
pub const WADO_CHUNK_SIZE: usize = 65536;

/// A parsed HTTP Range request for a single byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// Inclusive start byte offset.
    pub start: u64,
    /// Inclusive end byte offset.
    pub end: u64,
}

impl ByteRange {
    /// Length of the range in bytes.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    /// Returns true if the range is empty (len == 0).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Result of parsing a Range header against a known file size.
#[derive(Debug, Clone)]
pub enum RangeResult {
    /// No Range header present - serve full file (200 OK).
    FullFile,
    /// Valid range - serve partial content (206).
    Partial(ByteRange),
    /// Range is syntactically invalid (ignored, serve full file per RFC 7233).
    InvalidIgnored,
    /// Range is outside file bounds - 416 Range Not Satisfiable.
    NotSatisfiable,
}

/// Parse a `Range` header value string against a file size (pure, axum-free).
///
/// Only single byte ranges are supported. Multi-range requests return
/// `InvalidIgnored` (the server could also return 200 with the full file).
///
/// # RFC 7233 compliance
/// - `bytes=0-1023` -> bytes 0 through 1023 inclusive
/// - `bytes=1024-` -> bytes 1024 through end
/// - `bytes=-2048` -> last 2048 bytes
/// - Invalid range (start > end, start >= file_size) -> 416
pub fn parse_range_header_str(range: Option<&str>, file_size: u64) -> RangeResult {
    let Some(range_str) = range else {
        return RangeResult::FullFile;
    };

    // Only accept "bytes=" prefix.
    let spec = match range_str.strip_prefix("bytes=") {
        Some(s) => s.trim(),
        None => return RangeResult::InvalidIgnored,
    };

    // Reject multi-range requests (comma-separated).
    if spec.contains(',') {
        return RangeResult::InvalidIgnored;
    }

    // Parse: start-end, start-, or -suffix
    if let Some((start_str, end_str)) = spec.split_once('-') {
        let start_str = start_str.trim();
        let end_str = end_str.trim();

        if start_str.is_empty() {
            // Suffix range: bytes=-N -> last N bytes
            let suffix_len: u64 = match end_str.parse() {
                Ok(n) => n,
                Err(_) => return RangeResult::InvalidIgnored,
            };
            if suffix_len == 0 {
                return RangeResult::NotSatisfiable;
            }
            let start = file_size.saturating_sub(suffix_len);
            // end is file_size - 1, checked below for empty file
            if file_size == 0 {
                return RangeResult::NotSatisfiable;
            }
            let end = file_size.saturating_sub(1);
            return RangeResult::Partial(ByteRange { start, end });
        }

        let start: u64 = match start_str.parse() {
            Ok(n) => n,
            Err(_) => return RangeResult::InvalidIgnored,
        };

        if end_str.is_empty() {
            // Open-ended: bytes=N- -> from N to end
            if start >= file_size {
                return RangeResult::NotSatisfiable;
            }
            let end = file_size.saturating_sub(1);
            return RangeResult::Partial(ByteRange { start, end });
        }

        // Explicit range: bytes=N-M
        let end: u64 = match end_str.parse() {
            Ok(n) => n,
            Err(_) => return RangeResult::InvalidIgnored,
        };

        // INV: start must not exceed end
        if start > end {
            return RangeResult::NotSatisfiable;
        }

        // start must be within file
        if start >= file_size {
            return RangeResult::NotSatisfiable;
        }

        // Clamp end to file_size - 1
        let end = end.min(file_size.saturating_sub(1));

        RangeResult::Partial(ByteRange { start, end })
    } else {
        RangeResult::InvalidIgnored
    }
}

/// Build the `Content-Range` header value for a 206 Partial Content response.
pub fn content_range_header(range: ByteRange, total: u64) -> String {
    format!("bytes {}-{}/{}", range.start, range.end, total)
}

// -- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_range_header() {
        let result = parse_range_header_str(None, 1024);
        assert!(matches!(result, RangeResult::FullFile));
    }

    #[test]
    fn test_simple_range() {
        let result = parse_range_header_str(Some("bytes=0-1023"), 2048);
        assert!(matches!(
            result,
            RangeResult::Partial(ByteRange {
                start: 0,
                end: 1023
            })
        ));
    }

    #[test]
    fn test_open_ended_range() {
        let result = parse_range_header_str(Some("bytes=1024-"), 2048);
        assert!(matches!(
            result,
            RangeResult::Partial(ByteRange {
                start: 1024,
                end: 2047
            })
        ));
    }

    #[test]
    fn test_suffix_range() {
        let result = parse_range_header_str(Some("bytes=-512"), 2048);
        // Last 512 bytes: start = 2048-512 = 1536, end = 2047
        assert!(matches!(
            result,
            RangeResult::Partial(ByteRange {
                start: 1536,
                end: 2047
            })
        ));
    }

    #[test]
    fn test_suffix_range_exceeds_file() {
        let result = parse_range_header_str(Some("bytes=-9999"), 100);
        // suffix > file_size -> start = 0, end = 99
        assert!(matches!(
            result,
            RangeResult::Partial(ByteRange { start: 0, end: 99 })
        ));
    }

    #[test]
    fn test_range_ends_at_file_end() {
        let result = parse_range_header_str(Some("bytes=0-9999"), 100);
        // end clamped to 99
        assert!(matches!(
            result,
            RangeResult::Partial(ByteRange { start: 0, end: 99 })
        ));
    }

    #[test]
    fn test_range_start_beyond_file() {
        let result = parse_range_header_str(Some("bytes=2000-2999"), 100);
        assert!(matches!(result, RangeResult::NotSatisfiable));
    }

    #[test]
    fn test_range_start_gt_end() {
        let result = parse_range_header_str(Some("bytes=50-10"), 100);
        assert!(matches!(result, RangeResult::NotSatisfiable));
    }

    #[test]
    fn test_empty_file() {
        let result = parse_range_header_str(Some("bytes=0-1023"), 0);
        assert!(matches!(result, RangeResult::NotSatisfiable));
    }

    #[test]
    fn test_invalid_range_format() {
        let result = parse_range_header_str(Some("bytes=abc-xyz"), 100);
        assert!(matches!(result, RangeResult::InvalidIgnored));
    }

    #[test]
    fn test_content_range_header_value() {
        let range = ByteRange {
            start: 0,
            end: 1023,
        };
        let header = content_range_header(range, 2048);
        assert_eq!(header, "bytes 0-1023/2048");
    }

    #[test]
    fn test_suffix_range_zero() {
        let result = parse_range_header_str(Some("bytes=-0"), 100);
        assert!(matches!(result, RangeResult::NotSatisfiable));
    }
}
