// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! HTTP status classification.
//!
//! Replaces the `http` crate's `StatusCode` dependency in the server's metrics
//! path with a plain `u16` status-code -> class-bucket table. Per the owner
//! rule (15.08), a cross-crate primitive lives as its own module in core, not
//! embedded in a caller.

/// Class bucket index for the 2xx range (metrics histogram slot 0).
pub const STATUS_2XX: usize = 0;
/// Class bucket index for the 4xx range (metrics histogram slot 1).
pub const STATUS_4XX: usize = 1;
/// Class bucket index for the 5xx range (metrics histogram slot 2).
pub const STATUS_5XX: usize = 2;
/// Number of status-class buckets.
pub const NUM_STATUS_CLASSES: usize = 3;

/// Map a raw HTTP status code to its class bucket.
///
/// 2xx -> [`STATUS_2XX`], 4xx -> [`STATUS_4XX`], 5xx -> [`STATUS_5XX`]; 1xx and
/// 3xx are folded into the 2xx bucket (the metrics histogram only splits the
/// success/client-error/server-error families).
#[inline]
pub fn status_class(code: u16) -> usize {
    match code / 100 {
        2 => STATUS_2XX,
        4 => STATUS_4XX,
        5 => STATUS_5XX,
        _ => STATUS_2XX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_families() {
        assert_eq!(status_class(200), STATUS_2XX);
        assert_eq!(status_class(204), STATUS_2XX);
        assert_eq!(status_class(404), STATUS_4XX);
        assert_eq!(status_class(401), STATUS_4XX);
        assert_eq!(status_class(500), STATUS_5XX);
        // Informational/redirect fall into the 2xx bucket.
        assert_eq!(status_class(100), STATUS_2XX);
        assert_eq!(status_class(304), STATUS_2XX);
    }
}
