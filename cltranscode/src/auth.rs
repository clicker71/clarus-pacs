// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Cloud authentication - AWS SIGV4 + GCP OAuth2 for DICOMweb output.
//!
//! nginx-way: only signature for HTTP `Authorization` header. No SDK.

#![deny(unsafe_code)]

use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Supported cloud authentication methods.
pub enum CloudAuth {
    None,
    AwsV4 {
        access_key: String,
        secret_key: String,
        region: String,
    },
    GcpOAuth2 {
        token: String,
    },
}

impl CloudAuth {
    /// Create from configuration mode string.
    /// Reads credentials from environment variables.
    pub fn from_config(mode: &str) -> Self {
        match mode.to_lowercase().as_str() {
            "aws" | "awsv4" => {
                let access_key = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default();
                let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
                let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into());
                if access_key.is_empty() || secret_key.is_empty() {
                    log::warn!("cloud_auth: AWS credentials not set — falling back to None");
                    Self::None
                } else {
                    Self::AwsV4 {
                        access_key,
                        secret_key,
                        region,
                    }
                }
            }
            "gcp" | "google" => {
                let creds_path =
                    std::env::var("GOOGLE_APPLICATION_CREDENTIALS").unwrap_or_default();
                if creds_path.is_empty() {
                    log::warn!(
                        "cloud_auth: GOOGLE_APPLICATION_CREDENTIALS not set — falling back to None"
                    );
                    Self::None
                } else {
                    // Read service account JSON and extract token
                    // For now, use a placeholder - full OAuth2 requires JWT signing.
                    // nginx-way: GCP health check endpoint only needs Bearer token from metadata server.
                    let token = Self::fetch_gcp_token(&creds_path).unwrap_or_default();
                    Self::GcpOAuth2 { token }
                }
            }
            _ => Self::None,
        }
    }

    /// Sign an HTTP request. Returns the value for the `Authorization` header.
    pub fn sign(&self, method: &str, url: &str, body: &[u8]) -> String {
        match self {
            Self::None => String::new(),
            Self::AwsV4 {
                access_key,
                secret_key,
                region,
            } => sign_aws_v4(method, url, body, access_key, secret_key, region),
            Self::GcpOAuth2 { token } => {
                format!("Bearer {}", token)
            }
        }
    }

    fn fetch_gcp_token(_creds_path: &str) -> Option<String> {
        // nginx-way: try GCP metadata server for default service account token.
        // Falls back to empty - DICOMweb server will return 401, outbox retries.
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let mut stream = TcpStream::connect("169.254.169.254:80").ok()?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok()?;
        let req = "GET /computeMetadata/v1/instance/service-accounts/default/token HTTP/1.1\r\n\
                    Host: metadata.google.internal\r\n\
                    Metadata-Flavor: Google\r\n\
                    Connection: close\r\n\r\n";
        stream.write_all(req.as_bytes()).ok()?;
        let mut resp = String::new();
        stream.read_to_string(&mut resp).ok()?;
        // Extract access_token from JSON response
        resp.lines()
            .find(|l| l.contains("access_token"))
            .and_then(|l| l.split('"').nth(3))
            .map(|s| s.to_string())
    }
}

// -- AWS Signature V4 -------------------------------------------------

/// Create AWS SIGV4 Authorization header value.
///
/// Reference: https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html
fn sign_aws_v4(
    method: &str,
    url: &str,
    body: &[u8],
    access_key: &str,
    secret_key: &str,
    region: &str,
) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let amz_date = format_aws_date(now.as_secs());
    let date_stamp = &amz_date[..8];

    // Parse URL
    let (host, uri, query) = parse_url_parts(url);

    let service = "s3";

    // Task 1: Canonical Request
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

    // Task 2: String to Sign
    let algorithm = "AWS4-HMAC-SHA256";
    let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
    let string_to_sign = format!(
        "{}\n{}\n{}\n{}",
        algorithm, amz_date, credential_scope, canonical_request_hash
    );

    // Task 3: Signature
    let signature = hmac_sha256_str(
        &hmac_sha256_str(
            &hmac_sha256_str(
                &hmac_sha256_str(
                    &hmac_sha256_str(&format!("AWS4{}", secret_key), date_stamp),
                    region,
                ),
                service,
            ),
            "aws4_request",
        ),
        &string_to_sign,
    );

    format!(
        "{} Credential={}/{}, SignedHeaders={}, Signature={}",
        algorithm, access_key, credential_scope, signed_headers, signature
    )
}

fn parse_url_parts(url: &str) -> (String, String, String) {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (host_and_port, path_and_query) = without_scheme
        .split_once('/')
        .unwrap_or((without_scheme, ""));
    let (host, _port) = host_and_port
        .split_once(':')
        .unwrap_or((host_and_port, "443"));
    let uri = format!("/{}", path_and_query.split('?').next().unwrap_or(""));
    let query = path_and_query.split('?').nth(1).unwrap_or("");
    (host.to_string(), uri, query.to_string())
}

fn format_aws_date(unix_secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = unix_to_ymdhms(unix_secs);
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", y, m, d, hh, mm, ss)
}

fn hex_sha256(data: &[u8]) -> String {
    clarus_adapters::hex_encode(&Sha256::digest(data))
}

/// HMAC-SHA256 (hex-encoded output).
/// nginx-way: manual HMAC implementation, no `hmac` crate.
fn hmac_sha256_str(key: &str, msg: &str) -> String {
    // RFC 2104 HMAC using SHA-256
    let block_size = 64usize;
    let mut key_bytes = key.as_bytes().to_vec();
    if key_bytes.len() > block_size {
        let mut hk = Sha256::new();
        hk.update(&key_bytes);
        key_bytes = hk.finalize().to_vec();
    }
    key_bytes.resize(block_size, 0x00);

    let o_key_pad: Vec<u8> = key_bytes.iter().map(|b| b ^ 0x5c).collect();
    let i_key_pad: Vec<u8> = key_bytes.iter().map(|b| b ^ 0x36).collect();

    let mut inner = Sha256::new();
    inner.update(&i_key_pad);
    inner.update(msg.as_bytes());
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&o_key_pad);
    outer.update(inner_hash);
    clarus_adapters::hex_encode(&outer.finalize())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_auth_returns_empty_signature() {
        let auth = CloudAuth::None;
        assert_eq!(auth.sign("POST", "http://localhost/test", b""), "");
    }

    #[test]
    fn aws_v4_signature_format() {
        let auth = CloudAuth::AwsV4 {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            region: "us-east-1".into(),
        };
        let sig = auth.sign("GET", "https://s3.amazonaws.com/test.txt", b"");
        assert!(sig.starts_with("AWS4-HMAC-SHA256 "));
        assert!(sig.contains("Credential=AKIAIOSFODNN7EXAMPLE/"));
        assert!(sig.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert!(sig.contains("Signature="));
    }

    #[test]
    fn gcp_oauth2_bearer_format() {
        let auth = CloudAuth::GcpOAuth2 {
            token: "ya29.test".into(),
        };
        assert_eq!(
            auth.sign("POST", "https://storage.googleapis.com/", b""),
            "Bearer ya29.test"
        );
    }

    #[test]
    fn parse_url_extracts_parts() {
        let (host, uri, query) =
            parse_url_parts("https://s3.us-east-1.amazonaws.com/bucket/key?acl");
        assert_eq!(host, "s3.us-east-1.amazonaws.com");
        assert_eq!(uri, "/bucket/key");
        assert_eq!(query, "acl");

        let (host, uri, query) = parse_url_parts("http://localhost:8042/dicomweb/studies");
        assert_eq!(host, "localhost");
        assert_eq!(uri, "/dicomweb/studies");
        assert_eq!(query, "");
    }
}
