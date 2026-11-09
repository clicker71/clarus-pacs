// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DICOMweb STOW-RS output adapter - forward DICOM to a DICOMweb server.
//!
//! nginx-way: minimal HTTP/1.1 client. No full HTTP stack.

#![deny(unsafe_code)]

use crate::adapters::{AdapterError, OutputAdapter};
use crate::auth::CloudAuth;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

/// DICOMweb STOW-RS output adapter.
pub struct DicomwebOutput {
    url: String,
    auth: CloudAuth,
    probed: bool,
}

impl DicomwebOutput {
    pub fn new(url: String, auth_mode: &str) -> Self {
        let auth = CloudAuth::from_config(auth_mode);
        Self {
            url,
            auth,
            probed: false,
        }
    }

    /// Quick GET to verify the server is alive before sending any data.
    /// Called once on first STOW. Fails fast - no retry loop.
    fn probe(&mut self) -> Result<(), AdapterError> {
        if self.probed {
            return Ok(());
        }
        let (host, port, path) = parse_url(&self.url)?;
        let probe_path = format!("{}/", path.trim_end_matches('/'));

        log::info!("dicomweb_output: probing {}:{} {}", host, port, probe_path);

        let mut stream =
            TcpStream::connect(format!("{}:{}", host, port)).map_err(AdapterError::Io)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(AdapterError::Io)?;

        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            probe_path, host
        );
        stream.write_all(req.as_bytes()).map_err(AdapterError::Io)?;

        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).map_err(AdapterError::Io)?;

        let status = parse_http_status(&resp);
        if status == 0 || status >= 500 {
            return Err(AdapterError::Protocol(format!(
                "server unreachable (GET {} → HTTP {}, check dicomweb_url in config)",
                probe_path, status
            )));
        }

        log::info!("dicomweb_output: probe OK (HTTP {})", status);
        self.probed = true;
        Ok(())
    }
}

impl OutputAdapter for DicomwebOutput {
    fn send(&mut self, bytes: &[u8]) -> Result<(), AdapterError> {
        // Pre-flight: verify server is alive before first STOW
        self.probe()?;

        let (host, port, path) = parse_url(&self.url)?;
        // DICOMweb convention: base URL /dicomweb, STOW-RS = POST {base}/studies
        let stow_path = format!("{}/studies", path.trim_end_matches('/'));

        log::info!("dicomweb_output: POST {}:{} {}", host, port, stow_path);

        let mut stream =
            TcpStream::connect(format!("{}:{}", host, port)).map_err(AdapterError::Io)?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(AdapterError::Io)?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(AdapterError::Io)?;

        // Build multipart/related body - match bridge format exactly.
        // multer expects RFC 2046 format: boundary must not appear in body.
        let boundary = format!(
            "BOUNDARY_{}{}",
            clarus_adapters::unique_id(),
            clarus_adapters::unique_id()
        );
        let body = build_multipart_body(bytes, &boundary);

        // Build HTTP request
        let auth_header = self.auth.sign("POST", &self.url, &body);
        let request = build_http_request("POST", &stow_path, &host, &body, &boundary, &auth_header);

        stream
            .write_all(request.as_bytes())
            .map_err(AdapterError::Io)?;
        stream.write_all(&body).map_err(AdapterError::Io)?;

        // Read response: Connection: close ensures server closes after response.
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .map_err(AdapterError::Io)?;

        let status = parse_http_status(&response);
        log::info!("dicomweb_output: HTTP {}", status);

        match status {
            200..=299 => Ok(()),
            429 => Err(AdapterError::Protocol(
                "STOW-RS: too many requests (429)".into(),
            )),
            code => {
                let resp_body = String::from_utf8_lossy(&response);
                let preview = &resp_body[..resp_body.len().min(300)];
                log::error!(
                    "dicomweb_output: STOW-RS HTTP {} — response: {}",
                    code,
                    preview
                );
                Err(AdapterError::Protocol(format!("STOW-RS: HTTP {}", code)))
            }
        }
    }
}

/// Parse URL into (host, port, path).
fn parse_url(url: &str) -> Result<(String, u16, String), AdapterError> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    let (host_and_port, path) = without_scheme
        .split_once('/')
        .unwrap_or((without_scheme, ""));

    let path = format!("/{}", path);

    let (host, port) = if let Some((h, p)) = host_and_port.split_once(':') {
        (h.to_string(), p.parse::<u16>().unwrap_or(443))
    } else {
        let default_port = if url.starts_with("https") { 443 } else { 80 };
        (host_and_port.to_string(), default_port)
    };

    Ok((host, port, path))
}

/// Build multipart/related body containing a single DICOM file.
fn build_multipart_body(dicom: &[u8], boundary: &str) -> Vec<u8> {
    let mut body = Vec::new();

    // Part headers
    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(b"Content-Type: application/dicom\r\n");
    body.extend_from_slice(b"\r\n");

    // DICOM data
    body.extend_from_slice(dicom);
    body.extend_from_slice(b"\r\n");

    // Closing boundary
    body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());

    body
}

/// Build HTTP request headers (without body).
fn build_http_request(
    method: &str,
    path: &str,
    host: &str,
    body: &[u8],
    boundary: &str,
    auth_header: &str,
) -> String {
    let mut req = String::new();
    req.push_str(&format!("{} {} HTTP/1.1\r\n", method, path));
    req.push_str(&format!("Host: {}\r\n", host));
    req.push_str(&format!(
        "Content-Type: multipart/related; type=\"application/dicom\"; boundary={}\r\n",
        boundary
    ));
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    req.push_str("Accept: application/dicom+json\r\n");
    req.push_str("Connection: close\r\n");
    if !auth_header.is_empty() {
        req.push_str(&format!("Authorization: {}\r\n", auth_header));
    }
    req.push_str("\r\n");
    req
}

/// Parse HTTP status code from response bytes.
fn parse_http_status(response: &[u8]) -> u16 {
    let resp_str = String::from_utf8_lossy(response);
    let first_line = resp_str.lines().next().unwrap_or("");
    // "HTTP/1.1 200 OK"
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_extracts_parts() {
        let (host, port, path) = parse_url("http://localhost:8042/dicomweb/studies").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 8042);
        assert_eq!(path, "/dicomweb/studies");

        let (host, port, path) = parse_url("https://pacs.example.com/dicomweb").unwrap();
        assert_eq!(host, "pacs.example.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/dicomweb");
    }

    #[test]
    fn build_multipart_contains_boundary() {
        let body = build_multipart_body(b"fake-dicom-data", "test-boundary");
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("--test-boundary"));
        assert!(s.contains("fake-dicom-data"));
        assert!(s.contains("Content-Type: application/dicom"));
    }

    #[test]
    fn parse_http_status_works() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse_http_status(resp), 200);

        let resp = b"HTTP/1.1 429 Too Many Requests\r\n\r\n";
        assert_eq!(parse_http_status(resp), 429);
    }

    #[test]
    fn build_http_request_includes_headers() {
        let req = build_http_request(
            "POST",
            "/dicomweb/studies",
            "localhost:8042",
            b"test-body",
            "boundary123",
            "",
        );
        assert!(req.contains("POST /dicomweb/studies HTTP/1.1"));
        assert!(req.contains("Host: localhost:8042"));
        assert!(req.contains("boundary=boundary123"));
        assert!(req.contains("Content-Length: 9"));
    }
}
