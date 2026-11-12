// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Pure-reqwest S3 client - drop-in replacement for rust-s3.
//!
//! Implements the `ObjectStore` trait from `s3.rs` using raw HTTP requests
//! with manual AWS SigV4 signing. No SDK, no XML library, no extra crates.
//! nginx-way: 290 lines, KISS.
//!
//! Enabled via `--features s3`.

use super::s3::{ObjectInfo, ObjectMeta, ObjectStore, S3Error};
use super::s3_http::s3_client;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// -- S3Client -----------------------------------------------------------

/// S3-compatible object store backed by `reqwest::blocking` + manual SigV4.
///
/// Uses `Arc<reqwest::blocking::Client>` for cheap cloning and testability.
/// In production, the client comes from the global `OnceLock` pool.
#[derive(Clone)]
pub struct S3Client {
    http: Arc<reqwest::blocking::Client>,
    endpoint: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    region: String,
    /// Object-key prefix inside the (possibly shared) bucket. Default `store/`.
    prefix: String,
}

impl S3Client {
    /// Create a new S3Client.
    ///
    /// Uses the global HTTP pool from `s3_http.rs` in production.
    /// In tests, caller can pass `Arc::new(Client::new())`.
    /// `prefix` is the object-key prefix (e.g. `store/`); pass `""` for none.
    pub fn new(
        endpoint: &str,
        access_key: &str,
        secret_key: &str,
        region: &str,
        bucket: &str,
        prefix: &str,
    ) -> Self {
        Self {
            http: Arc::new(s3_client().clone()),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            bucket: bucket.to_string(),
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
            region: region.to_string(),
            prefix: prefix.to_string(),
        }
    }

    /// Create an S3Client with a custom HTTP client (for testing).
    pub fn with_client(
        http: Arc<reqwest::blocking::Client>,
        endpoint: &str,
        access_key: &str,
        secret_key: &str,
        region: &str,
        bucket: &str,
        prefix: &str,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            bucket: bucket.to_string(),
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
            region: region.to_string(),
            prefix: prefix.to_string(),
        }
    }

    /// Build the full URL for an S3 object.
    fn object_url(&self, key: &str) -> String {
        format!("{}/{}/{}", self.endpoint, self.bucket, key)
    }

    /// Build URL with query parameters for ListObjectsV2.
    fn list_url(&self, prefix: &str, max_keys: Option<u32>, token: Option<&str>) -> String {
        list_url(&self.endpoint, &self.bucket, prefix, max_keys, token)
    }

    /// Sign and execute an HTTP request (blocking).
    /// `body` is owned so the PUT path can attach it zero-copy via
    /// `reqwest::blocking::Body::from` (diet 22.08: bytes `Bytes` unnecessary).
    /// `timeout` is chosen by the caller: object transfers use the wide
    /// `S3_XFER_TIMEOUT`, metadata calls (HEAD/LIST/DELETE) the short
    /// `S3_META_TIMEOUT` - a hung metadata call must not sit out the whole
    /// transfer ceiling while moving no bytes.
    fn request(
        &self,
        method: &str,
        url: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<reqwest::blocking::Response, S3Error> {
        // One clock read. Two `SystemTime::now()` calls can straddle a second
        // boundary, which signs a different `x-amz-date` than the one sent.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let amz_date = format_aws_date(now.as_secs());
        let payload_hash = hex_sha256(&body);

        let auth = sign_aws_v4_at(
            method,
            url,
            &body,
            &self.access_key,
            &self.secret_key,
            &self.region,
            &amz_date,
        );

        let req = self
            .http
            .request(method.parse().unwrap(), url)
            .timeout(timeout)
            .header("Authorization", &auth)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &amz_date)
            .header("Host", host_from_url(url));

        let req = if !body.is_empty() {
            req.header("Content-Type", "application/octet-stream")
                .body(reqwest::blocking::Body::from(body))
        } else {
            req
        };

        let resp = req.send().map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                S3Error::Connection(e.to_string())
            } else {
                S3Error::Request(e.to_string())
            }
        })?;

        Ok(resp)
    }

    /// Build the CAS-compatible S3 object key.
    ///
    /// Geometry is the canonical `cas_rel_path` (single source of truth for
    /// hot/cold/S3, always forward slashes) with the `[s3] prefix` prepended:
    /// `{prefix}{xx}/{yy}/{hash}.dcm`. The relative layout lives ONLY in
    /// `cas_rel_path`; here we add the prefix. Deliberately a `String` of
    /// forward slashes - never a `PathBuf` (a `\` in the key would break S3
    /// LIST/walk and disaster-recovery download from a Windows dev box).
    /// The prefix comes from `[s3] prefix` (default `store/`); pass `""` for none.
    pub fn s3_key(&self, blake3: &str) -> String {
        format!("{}{}", self.prefix, super::cas_rel_path(blake3))
    }

    /// Read object from S3 as a synchronous reader. Supports optional byte range.
    pub fn get_object_range_stream(
        &self,
        key: &str,
        range: Option<(u64, u64)>,
    ) -> Result<(std::io::Cursor<Vec<u8>>, u64), S3Error> {
        let (data, size) = if let Some((start, end)) = range {
            let ranged = self.get_object_range(key, start, end)?;
            let sz = ranged.len() as u64;
            (ranged, sz)
        } else {
            let full = self.get_object(key)?;
            let sz = full.len() as u64;
            (full, sz)
        };
        // `get_object`/`get_object_range` already return owned `Vec<u8>`, so
        // hand it straight to the Cursor (no extra copy; diet 22.08).
        Ok((std::io::Cursor::new(data), size))
    }
}

/// Slice a full-object body down to the requested INCLUSIVE byte range
/// (`start..=end`) when the server answered 200 (Range ignored). Mirrors the
/// `InMemoryObjectStore` bounds semantics: out-of-bounds is an error, never a
/// truncated or empty "success".
fn slice_full_object(data: Vec<u8>, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
    let lo = start as usize;
    let hi = (end as usize).min(data.len().saturating_sub(1));
    if lo > hi || lo >= data.len() {
        return Err(S3Error::Request(format!(
            "range {lo}-{hi} out of bounds (len={})",
            data.len()
        )));
    }
    Ok(data[lo..=hi].to_vec())
}

/// Build a ListObjectsV2 URL.
///
/// Alphabetical by parameter name - the order SigV4 canonicalises to - and
/// every value percent-encoded. A prefix with a `/` or a space signed
/// differently from the one sent is a `SignatureDoesNotMatch` on every
/// listing, which is the scrub's only view of the bucket.
fn list_url(
    endpoint: &str,
    bucket: &str,
    prefix: &str,
    max_keys: Option<u32>,
    token: Option<&str>,
) -> String {
    let mut url = format!(
        "{}/{bucket}?list-type=2&prefix={}",
        endpoint,
        uri_encode(prefix, true)
    );
    if let Some(mk) = max_keys {
        url.push_str(&format!("&max-keys={mk}"));
    }
    if let Some(t) = token {
        // The continuation token is opaque and often contains `=`, `+` and
        // `/`; it must be encoded or the server reads a different token than
        // the one it issued.
        url.push_str(&format!("&continuation-token={}", uri_encode(t, true)));
    }
    url
}

// -- ObjectStore implementation -----------------------------------------

impl ObjectStore for S3Client {
    fn put_object(&self, key: &str, data: Vec<u8>, _content_type: &str) -> Result<String, S3Error> {
        let url = self.object_url(key);
        let etag = crate::hex_encode(&Sha256::digest(&data));

        let resp = self.request("PUT", &url, data, super::s3_http::S3_XFER_TIMEOUT)?;
        let status = resp.status().as_u16();

        if status != 200 {
            let body = resp.text().unwrap_or_default();
            return Err(S3Error::Request(format!(
                "PUT {key}: HTTP {status}: {body}"
            )));
        }

        Ok(etag)
    }

    fn get_object(&self, key: &str) -> Result<Vec<u8>, S3Error> {
        let url = self.object_url(key);
        let resp = self.request("GET", &url, Vec::new(), super::s3_http::S3_XFER_TIMEOUT)?;
        let status = resp.status().as_u16();

        if status == 404 {
            return Err(S3Error::NotFound(key.to_string()));
        }
        if status != 200 {
            let body = resp.text().unwrap_or_default();
            return Err(S3Error::Request(format!(
                "GET {key}: HTTP {status}: {body}"
            )));
        }

        // One copy at the boundary: reqwest hands us `bytes::Bytes`, S3-read is
        // a rare path, so `to_vec()` is documented as acceptable (diet 22.08).
        let data = resp.bytes().map_err(|e| S3Error::Request(e.to_string()))?;
        Ok(data.to_vec())
    }

    fn get_object_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        let url = self.object_url(key);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let amz_date = format_aws_date(now.as_secs());
        let payload_hash = hex_sha256(b"");
        let auth = sign_aws_v4_at(
            "GET",
            &url,
            b"",
            &self.access_key,
            &self.secret_key,
            &self.region,
            &amz_date,
        );

        let resp = self
            .http
            .get(&url)
            .timeout(super::s3_http::S3_XFER_TIMEOUT)
            .header("Authorization", &auth)
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &amz_date)
            .header("Host", host_from_url(&url))
            .header("Range", format!("bytes={start}-{end}"))
            .send()
            .map_err(|e| {
                if e.is_connect() || e.is_timeout() {
                    S3Error::Connection(e.to_string())
                } else {
                    S3Error::Request(e.to_string())
                }
            })?;

        let status = resp.status().as_u16();
        if status == 404 {
            return Err(S3Error::NotFound(key.to_string()));
        }
        if status != 206 && status != 200 {
            let body = resp.text().unwrap_or_default();
            return Err(S3Error::Request(format!(
                "GET range {key}: HTTP {status}: {body}"
            )));
        }

        let data = resp.bytes().map_err(|e| S3Error::Request(e.to_string()))?;
        let data = data.to_vec();
        if status == 200 {
            // The server answered 200: it ignored (or is not required to
            // honour) the Range header and returned the FULL object. Handing
            // that back as "the range" would overflow caller buffers sized
            // from the requested length, so slice the requested window
            // locally - same bounds semantics as `InMemoryObjectStore`.
            return slice_full_object(data, start, end);
        }
        Ok(data)
    }

    fn head_object(&self, key: &str) -> Result<ObjectMeta, S3Error> {
        let url = self.object_url(key);
        let resp = self.request("HEAD", &url, Vec::new(), super::s3_http::S3_META_TIMEOUT)?;
        let status = resp.status().as_u16();

        if status == 404 {
            return Err(S3Error::NotFound(key.to_string()));
        }
        if status != 200 {
            return Err(S3Error::Request(format!("HEAD {key}: HTTP {status}")));
        }

        let headers = resp.headers();
        let content_length = headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let etag = headers
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim_matches('"')
            .to_string();

        Ok(ObjectMeta {
            content_length,
            etag,
        })
    }

    fn delete_object(&self, key: &str) -> Result<(), S3Error> {
        let url = self.object_url(key);
        let resp = self.request("DELETE", &url, Vec::new(), super::s3_http::S3_META_TIMEOUT)?;
        let status = resp.status().as_u16();

        if status == 404 {
            return Err(S3Error::NotFound(key.to_string()));
        }
        if status != 204 && status != 200 {
            return Err(S3Error::Request(format!("DELETE {key}: HTTP {status}")));
        }

        Ok(())
    }

    fn list_objects(
        &self,
        prefix: &str,
        max_keys: Option<u32>,
    ) -> Result<Vec<ObjectInfo>, S3Error> {
        // ListObjectsV2 answers at most 1000 keys per call and reports the rest
        // through `IsTruncated`/`NextContinuationToken`. Reading one page and
        // stopping silently truncated every scrub and audit walk to the first
        // 1000 objects - a bucket of 100k instances looked like a bucket of
        // 1000 and the rest was never checked.
        let page_size = max_keys.unwrap_or(1000).clamp(1, 1000);
        let mut items: Vec<ObjectInfo> = Vec::new();
        let mut token: Option<String> = None;

        loop {
            let wanted = match max_keys {
                // A caller-supplied cap is a total, not a per-page size.
                Some(total) => (total as usize).saturating_sub(items.len()),
                None => page_size as usize,
            };
            if wanted == 0 {
                break;
            }

            let url = self.list_url(prefix, Some(wanted as u32), token.as_deref());
            let resp = self.request("GET", &url, Vec::new(), super::s3_http::S3_META_TIMEOUT)?;
            let status = resp.status().as_u16();

            if status != 200 {
                let body = resp.text().unwrap_or_default();
                return Err(S3Error::Request(format!(
                    "list prefix={prefix}: HTTP {status}: {body}"
                )));
            }

            let xml = resp.bytes().map_err(|e| S3Error::Request(e.to_string()))?;
            items.extend(parse_list_objects(&xml));

            match next_continuation_token(&xml) {
                Some(next) => token = Some(next),
                None => break,
            }
        }

        Ok(items)
    }
}

// -- AWS Signature V4 ---------------------------------------------------
// AWS Signature Version 4, hand-rolled - no SDK.

/// Sign against an explicit `x-amz-date`.
///
/// The date is threaded in rather than read here so the header the request
/// sends and the timestamp inside the string-to-sign cannot straddle a second
/// boundary (AWS rejects the result with `SignatureDoesNotMatch`).
#[allow(clippy::too_many_arguments)]
fn sign_aws_v4_at(
    method: &str,
    url: &str,
    body: &[u8],
    access_key: &str,
    secret_key: &str,
    region: &str,
    amz_date: &str,
) -> String {
    let date_stamp = &amz_date[..8];

    let (host, uri, query) = parse_url_parts(url);
    let service = "s3";

    let payload_hash = hex_sha256(body);
    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, amz_date
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, uri, query, canonical_headers, signed_headers, payload_hash
    );
    let canonical_request_hash = hex_sha256(canonical_request.as_bytes());

    let algorithm = "AWS4-HMAC-SHA256";
    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign =
        format!("{algorithm}\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");

    // PS3.18 does not cover this - it is the AWS SigV4 signing-key chain
    // (Signature Version 4, "Deriving the Signing Key"). Every step feeds the
    // RAW 32-byte HMAC output into the next as the key; only the final
    // signature is hex. Hex-encoding the intermediate steps makes every step
    // after the first sign with the wrong 64-character key, and AWS rejects
    // every request.
    let date_key = hmac_sha256(
        format!("AWS4{secret_key}").as_bytes(),
        date_stamp.as_bytes(),
    );
    let region_key = hmac_sha256(&date_key, region.as_bytes());
    let service_key = hmac_sha256(&region_key, service.as_bytes());
    let signing_key = hmac_sha256(&service_key, b"aws4_request");
    let signature = crate::hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    format!(
        "{algorithm} Credential={access_key}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

fn hex_sha256(data: &[u8]) -> String {
    crate::hex_encode(&Sha256::digest(data))
}

fn format_aws_date(unix_secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = unix_to_ymdhms(unix_secs);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

fn host_from_url(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

fn parse_url_parts(url: &str) -> (String, String, String) {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (host_and_port, path_and_query) = without_scheme
        .split_once('/')
        .unwrap_or((without_scheme, ""));
    // The port stays attached: SigV4 signs the Host header, and the Host
    // header carries the port when it is not the scheme default. Dropping it
    // here made every MinIO/Ceph endpoint on a nonstandard port fail with
    // `SignatureDoesNotMatch`.
    let uri = format!("/{}", path_and_query.split('?').next().unwrap_or(""));
    let query = canonical_query(path_and_query.split('?').nth(1).unwrap_or(""));
    (host_and_port.to_string(), uri, query)
}

/// Ordinal-sorted, percent-encoded query string (SigV4 canonical form).
///
/// The canonical request sorts parameters by name and encodes both names and
/// values. The input is the query exactly as it goes on the wire, so it is
/// already percent-encoded: decoding first would encode it a SECOND time
/// (`store%2F01%2F` -> `store%252F01%252F`) and sign a prefix the server never
/// sees, which is a `SignatureDoesNotMatch` on every prefixed listing.
fn canonical_query(raw: &str) -> String {
    let mut pairs: Vec<(String, String)> = raw
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (uri_encode(k, true), v.to_string()),
            None => (uri_encode(pair, true), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode per SigV4: unreserved characters pass through, everything
/// else becomes `%XX` with upper-case hex.
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// HMAC-SHA256 over raw bytes, returning the raw 32-byte digest.
///
/// Raw bytes, not hex: the SigV4 signing key is a chain of HMACs where each
/// output is the next call's KEY, and a 64-character hex string is not the key
/// AWS derives.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let block_size = 64usize;
    let mut key_bytes = if key.len() > block_size {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    key_bytes.resize(block_size, 0x00);

    let mut inner = Sha256::new();
    inner.update(key_bytes.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(msg);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(key_bytes.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner_hash);
    let out = outer.finalize();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    digest
}

fn unix_to_ymdhms(ts: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (ts / 86400) as i64;
    let rem = ts % 86400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    let (y, m, d) = days_to_ymd(days);
    (y, m, d, hh as u32, mm as u32, ss as u32)
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

// -- XML parser for ListObjectsV2 ---------------------------------------

/// Parse S3 ListObjectsV2 XML response.
///
/// Manual byte-scanning parser - no `quick-xml` dependency.
/// We only need three tags: `<Key>`, `<Size>`, `<LastModified>`.
fn parse_list_objects(xml: &[u8]) -> Vec<ObjectInfo> {
    let mut items = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = xml[pos..].windows(5).position(|w| w == b"<Key>") {
        let abs = pos + rel;
        let key = extract_text(xml, abs, b"<Key>", b"</Key>");
        let size: u64 = extract_text(xml, abs, b"<Size>", b"</Size>")
            .parse()
            .unwrap_or(0);
        let last_modified = extract_text(xml, abs, b"<LastModified>", b"</LastModified>");
        items.push(ObjectInfo {
            key,
            size,
            last_modified,
        });
        pos = abs + 1;
    }
    items
}

/// Extract `<NextContinuationToken>` from a ListObjectsV2 page.
///
/// Only present when `<IsTruncated>true</IsTruncated>`; its presence IS the
/// "there is another page" signal, so the flag needs no separate parse.
fn next_continuation_token(xml: &[u8]) -> Option<String> {
    let token = extract_text(
        xml,
        0,
        b"<NextContinuationToken>",
        b"</NextContinuationToken>",
    );
    if token.is_empty() { None } else { Some(token) }
}

/// Extract text between `<open>` and `</close>` tags.
fn extract_text(xml: &[u8], search_from: usize, open: &[u8], close: &[u8]) -> String {
    let start = match xml[search_from..]
        .windows(open.len())
        .position(|w| w == open)
    {
        Some(p) => search_from + p + open.len(),
        None => return String::new(),
    };
    let end = match xml[start..].windows(close.len()).position(|w| w == close) {
        Some(p) => start + p,
        None => return String::new(),
    };
    String::from_utf8_lossy(&xml[start..end]).into_owned()
}

// -- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_list_objects_empty() {
        let xml = b"<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></ListBucketResult>";
        let items = parse_list_objects(xml);
        assert!(items.is_empty());
    }

    #[test]
    fn test_parse_list_objects_multiple() {
        let xml = br##"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
            <Contents>
                <Key>store/00/abc123.dcm</Key>
                <Size>1048576</Size>
                <LastModified>2026-01-15T10:30:00.000Z</LastModified>
            </Contents>
            <Contents>
                <Key>store/ff/def456.dcm</Key>
                <Size>512</Size>
                <LastModified>2026-03-20T14:15:00.000Z</LastModified>
            </Contents>
            <Contents>
                <Key>store/aa/xyz789.dcm</Key>
                <Size>2097152</Size>
                <LastModified>2026-06-01T08:00:00.000Z</LastModified>
            </Contents>
        </ListBucketResult>"##;

        let items = parse_list_objects(xml);
        assert_eq!(items.len(), 3);

        assert_eq!(items[0].key, "store/00/abc123.dcm");
        assert_eq!(items[0].size, 1048576);
        assert_eq!(items[0].last_modified, "2026-01-15T10:30:00.000Z");

        assert_eq!(items[1].key, "store/ff/def456.dcm");
        assert_eq!(items[1].size, 512);

        assert_eq!(items[2].key, "store/aa/xyz789.dcm");
        assert_eq!(items[2].size, 2097152);
    }

    #[test]
    fn test_parse_list_objects_with_common_prefixes() {
        let xml = br##"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
            <CommonPrefixes>
                <Prefix>store/00/</Prefix>
            </CommonPrefixes>
            <Contents>
                <Key>store/01/test.dcm</Key>
                <Size>100</Size>
                <LastModified>2026-01-01T00:00:00.000Z</LastModified>
            </Contents>
        </ListBucketResult>"##;

        let items = parse_list_objects(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "store/01/test.dcm");
    }

    #[test]
    fn test_extract_text() {
        let xml = b"<Key>hello/world.dcm</Key>";
        let result = extract_text(xml, 0, b"<Key>", b"</Key>");
        assert_eq!(result, "hello/world.dcm");
    }

    /// A 200 (Range ignored) answer must still honour the requested window:
    /// the caller sized its buffer from the requested length, so the full body
    /// must be sliced locally, not handed back whole.
    #[test]
    fn a_full_body_response_is_sliced_to_the_range() {
        let data: Vec<u8> = (0..10u8).collect();
        assert_eq!(slice_full_object(data, 2, 5).unwrap(), vec![2, 3, 4, 5]);
    }

    /// Out-of-bounds is an error, never a truncated or empty "success".
    #[test]
    fn a_range_beyond_the_full_body_is_rejected() {
        let data: Vec<u8> = (0..5u8).collect();
        assert!(slice_full_object(data, 5, 9).is_err());
    }

    /// The published AWS SigV4 example ("Example: GET Object", the
    /// Signature Version 4 test suite): the signing key must be the chain of
    /// RAW HMAC digests, and hex-encoding an intermediate step changes the
    /// signature. Expected values are the AWS specification's own, taken from
    /// its documented chain for secret key
    /// `wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY`.
    #[test]
    fn signing_key_matches_the_aws_test_vector() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let date_key = hmac_sha256(format!("AWS4{secret}").as_bytes(), b"20150830");
        assert_eq!(
            crate::hex_encode(&date_key),
            "0138c7a6cbd60aa727b2f653a522567439dfb9f3e72b21f9b25941a42f04a7cd"
        );

        let region_key = hmac_sha256(&date_key, b"us-east-1");
        assert_eq!(
            crate::hex_encode(&region_key),
            "f33d5808504bf34812e5fade63308b424b244c59189be2a591dd2282c7cb563f"
        );

        let service_key = hmac_sha256(&region_key, b"s3");
        assert_eq!(
            crate::hex_encode(&service_key),
            "027fad2062f9f5a88972922cfc18150ea0474338d18d32d5a5c0105842379f69"
        );

        let signing_key = hmac_sha256(&service_key, b"aws4_request");
        assert_eq!(
            crate::hex_encode(&signing_key),
            "32f78051dcde24c552811d654f4a769112bb834b03975cdd6b1fd7d16248c269"
        );
    }

    /// The signature end to end, so a break anywhere in the canonical request
    /// (URI, query, headers, payload hash) or in the key chain is caught. Input
    /// is the AWS test suite's GET Object string-to-sign.
    #[test]
    fn signature_matches_the_aws_test_vector() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let date_key = hmac_sha256(format!("AWS4{secret}").as_bytes(), b"20150830");
        let region_key = hmac_sha256(&date_key, b"us-east-1");
        let service_key = hmac_sha256(&region_key, b"s3");
        let signing_key = hmac_sha256(&service_key, b"aws4_request");
        let string_to_sign = "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/s3/aws4_request\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            crate::hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes())),
            "d1a40c9f909615e6a80ca17fad06b15487d711a69f429d1bbeacc0a17ea8b0c8"
        );
    }

    /// A hex-encoded intermediate key is the bug: the chain above would feed a
    /// 64-byte string into the next HMAC instead of 32 raw bytes.
    #[test]
    fn hex_encoding_an_intermediate_step_changes_the_key() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let raw = hmac_sha256(format!("AWS4{secret}").as_bytes(), b"20150830");
        let hex_string = crate::hex_encode(&raw);
        let from_hex_string = hmac_sha256(hex_string.as_bytes(), b"us-east-1");
        let from_raw = hmac_sha256(&raw, b"us-east-1");
        assert_ne!(from_hex_string, from_raw);
    }

    #[test]
    fn canonical_query_sorts_and_keeps_the_wire_encoding() {
        // Build order is max-keys, list-type, prefix; the canonical form is
        // sorted. The values arrive already encoded from `list_url` and must
        // NOT be encoded again.
        let q = canonical_query("max-keys=10&list-type=2&prefix=store%2F00%2F");
        assert_eq!(q, "list-type=2&max-keys=10&prefix=store%2F00%2F");
    }

    #[test]
    fn a_nonstandard_port_stays_in_the_signed_host() {
        let (host, _, _) = parse_url_parts("http://127.0.0.1:9000/bucket/key");
        assert_eq!(host, "127.0.0.1:9000");
        assert_eq!(host_from_url("http://127.0.0.1:9000/bucket/key"), host);
    }

    #[test]
    fn a_continuation_token_is_encoded() {
        let url = list_url("http://h", "b", "store/", None, Some("1/abc+def="));
        assert!(
            url.ends_with("continuation-token=1%2Fabc%2Bdef%3D"),
            "{url}"
        );
        assert!(url.contains("prefix=store%2F"), "{url}");
    }

    /// The prefix is encoded once on the wire and that encoding IS the signed
    /// one. Re-encoding it in the canonical request (`%2F` -> `%252F`) signs a
    /// prefix the server never sees, so a listing under `store/` answered
    /// `SignatureDoesNotMatch` and the tier was unusable.
    #[test]
    fn the_sent_and_signed_prefix_agree() {
        let url = list_url("http://h", "b", "store/01/", Some(10), None);
        let sent_query = url.split_once('?').expect("query").1;
        let (_, _, signed) = parse_url_parts(&url);
        assert!(url.contains("prefix=store%2F01%2F"), "{url}");
        assert_eq!(signed, "list-type=2&max-keys=10&prefix=store%2F01%2F");
        assert!(!signed.contains("%252F"), "double-encoded: {signed}");
        assert!(sent_query.contains("prefix=store%2F01%2F"), "{sent_query}");
    }
}
