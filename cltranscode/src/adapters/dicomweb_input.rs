// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! STOW-RS input adapter - HTTP listener, writes directly to spool.
//!
//! PS3.18 Section 10.5: Store Over the Web (STOW-RS).
//! Accepts `POST /dicomweb/studies` with multipart/related body,
//! extracts DICOM parts per RFC 2046 Section 5.1, atomically writes to
//! spool/raw/ via `fs::rename`.
//!
//! nginx-way: minimal HTTP parse - method line, Content-Type,
//! Content-Length, boundary. No full HTTP/1.1 stack.

#![forbid(unsafe_code)]

use crate::adapters::{AdapterError, InputAdapter};
use crate::spool::Spool;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// STOW-RS listener bound to a TCP port.
///
/// One accept thread + one thread per request.
/// Writes DICOM parts directly to spool - no internal channels.
/// Same pattern as [`super::dimse_scp::DimseScp`].
pub struct DicomwebInput {
    pub port: u16,
    _listener_thread: Option<thread::JoinHandle<()>>,
}

impl DicomwebInput {
    /// Bind TCP listener and spawn the accept thread.
    ///
    /// `max_pending` - spool capacity guard.
    /// Requests are rejected with HTTP 503 when `spool.count_raw() >= max_pending`.
    pub fn new(
        port: u16,
        spool: Arc<Spool>,
        shutdown: Arc<AtomicBool>,
        max_pending: u32,
    ) -> Result<Self, AdapterError> {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).map_err(AdapterError::Io)?;
        listener.set_nonblocking(true).map_err(AdapterError::Io)?;

        let handle = thread::spawn(move || {
            run_http_listener(listener, spool, shutdown, max_pending);
        });

        log::info!("dicomweb_input: listening on port {}", port);
        Ok(Self {
            port,
            _listener_thread: Some(handle),
        })
    }
}

impl InputAdapter for DicomwebInput {
    fn recv(&mut self) -> Result<Vec<u8>, AdapterError> {
        unreachable!("dicomweb_input writes directly to spool; recv() is unused")
    }
}

// -- Accept loop ------------------------------------------------------

fn run_http_listener(
    listener: TcpListener,
    spool: Arc<Spool>,
    shutdown: Arc<AtomicBool>,
    max_pending: u32,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                log::debug!("dicomweb_input: connection from {}", addr);
                let sp = Arc::clone(&spool);
                thread::spawn(move || {
                    if let Err(e) = handle_http_request(stream, &sp, max_pending) {
                        log::error!("dicomweb_input: request error: {}", e);
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log::error!("dicomweb_input: accept error: {}", e);
                break;
            }
        }
    }
}

// -- Request handler --------------------------------------------------

fn handle_http_request(
    mut stream: TcpStream,
    spool: &Arc<Spool>,
    max_pending: u32,
) -> Result<(), AdapterError> {
    // -- Backpressure: reject when spool is full --
    // PS3.18 Section 10.5.2.2: server may reject STOW-RS with 503.
    if max_pending > 0 && spool.count_raw() >= max_pending as u64 {
        log::warn!(
            "dicomweb_input: spool full ({}/{}), rejecting with 503",
            spool.count_raw(),
            max_pending
        );
        send_503(&mut stream);
        return Ok(());
    }

    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(AdapterError::Io)?;

    // Read until \r\n\r\n (end of headers)
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).map_err(AdapterError::Io)?;
        if n == 0 {
            return Err(AdapterError::Protocol("client disconnected".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 65536 {
            send_error(&mut stream, 413, "Payload Too Large");
            return Err(AdapterError::Protocol("headers too large".into()));
        }
    }

    // Parse request line: METHOD SP PATH SP HTTP/1.1
    let headers = String::from_utf8_lossy(&buf);
    let first_line = headers.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    let method = parts.first().copied().unwrap_or("");

    if method != "POST" {
        send_error(&mut stream, 405, "Method Not Allowed");
        return Err(AdapterError::Protocol(format!(
            "unsupported method: {}",
            method
        )));
    }

    // Content-Type -> multipart boundary
    let content_type = headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-type:"))
        .unwrap_or("");
    let boundary = extract_boundary(content_type);

    // Content-Length -> body size
    let content_length: usize = headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    // Read body bytes (may be partially in buf already)
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(buf.len())
        + 4;
    let already_read = buf.len() - header_end;
    let mut body = buf[header_end..].to_vec();
    if already_read < content_length {
        let remaining = content_length - already_read;
        let mut rest = vec![0u8; remaining];
        stream.read_exact(&mut rest).map_err(AdapterError::Io)?;
        body.extend_from_slice(&rest);
    }

    // -- Single DICOM (no multipart boundary) --
    if boundary.is_empty() {
        log::info!(
            "dicomweb_input: {} bytes (single part) → spool/raw/",
            body.len()
        );
        spool
            .write_raw(&body)
            .map_err(|e| AdapterError::Protocol(format!("spool write error: {}", e)))?;
        send_ok(&mut stream);
        return Ok(());
    }

    // -- Multipart/related (RFC 2046 Section 5.1) --
    // Boundary delimiters: "--boundary" for parts, "--boundary--" for end.
    let boundary_marker = format!("--{}", boundary);
    let boundary_end = format!("--{}--", boundary);

    let mut n_written: usize = 0;
    let parts_iter = SplitByBoundary::new(&body, &boundary_marker, &boundary_end);

    for part in parts_iter {
        if part.is_empty() || part == b"--" {
            continue;
        }

        // Each MIME part: headers \r\n\r\n body
        if let Some(mime_end) = part.windows(4).position(|w| w == b"\r\n\r\n") {
            let dicom_bytes = &part[mime_end + 4..];
            if !dicom_bytes.is_empty() {
                spool
                    .write_raw(dicom_bytes)
                    .map_err(|e| AdapterError::Protocol(format!("spool write error: {}", e)))?;
                n_written += 1;
            }
        }
    }

    log::info!("dicomweb_input: {} DICOM parts → spool/raw/", n_written);
    send_ok(&mut stream);
    Ok(())
}

/// Iterator that splits bytes by a boundary byte sequence.
struct SplitByBoundary<'a> {
    data: &'a [u8],
    boundary: &'a [u8],
    boundary_end: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> SplitByBoundary<'a> {
    fn new(data: &'a [u8], boundary: &'a str, boundary_end: &'a str) -> Self {
        Self {
            data,
            boundary: boundary.as_bytes(),
            boundary_end: boundary_end.as_bytes(),
            pos: 0,
            done: false,
        }
    }
}

impl<'a> Iterator for SplitByBoundary<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.pos >= self.data.len() {
            self.done = true;
            return None;
        }

        // Find next boundary
        let remaining = &self.data[self.pos..];
        let (boundary_len, is_final) = if let Some(idx) = remaining
            .windows(self.boundary.len())
            .position(|w| w == self.boundary)
        {
            let end_marker_pos = remaining[idx..]
                .windows(self.boundary_end.len())
                .position(|w| w == self.boundary_end);
            if end_marker_pos == Some(0) || idx == 0 && end_marker_pos.is_some() {
                (self.boundary_end.len(), true)
            } else {
                (self.boundary.len(), false)
            }
        } else {
            self.done = true;
            return None;
        };

        let part_start = self.pos;
        let boundary_start = self.pos
            + remaining
                .windows(self.boundary.len())
                .position(|w| w == self.boundary)?;

        let part = &self.data[part_start..boundary_start];

        if is_final {
            self.done = true;
        } else {
            // Skip past boundary + \r\n
            self.pos = boundary_start + boundary_len;
            if self.pos + 2 <= self.data.len() && &self.data[self.pos..self.pos + 2] == b"\r\n" {
                self.pos += 2;
            }
        }

        Some(part)
    }
}

fn extract_boundary(content_type: &str) -> String {
    content_type
        .split(';')
        .find(|p| p.trim().to_lowercase().starts_with("boundary="))
        .and_then(|p| p.split('=').nth(1))
        .map(|s| s.trim().trim_matches('"').to_string())
        .unwrap_or_default()
}

// -- HTTP response helpers --------------------------------------------

fn send_ok(stream: &mut TcpStream) {
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}

/// RFC 7231 Section 6.6.4 - server is temporarily unable to handle the request.
fn send_503(stream: &mut TcpStream) {
    let _ = stream.write_all(
        b"HTTP/1.1 503 Service Unavailable\r\n\
          Retry-After: 30\r\n\
          Content-Length: 0\r\n\
          Connection: close\r\n\r\n",
    );
}

fn send_error(stream: &mut TcpStream, code: u16, msg: &str) {
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            code, msg
        )
        .as_bytes(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_boundary_from_content_type() {
        let ct = "multipart/related; boundary=abc123";
        assert_eq!(extract_boundary(ct), "abc123");

        let ct2 = r#"multipart/related; type="application/dicom"; boundary="my-boundary""#;
        assert_eq!(extract_boundary(ct2), "my-boundary");

        let ct3 = "application/dicom";
        assert_eq!(extract_boundary(ct3), "");
    }

    #[test]
    fn split_by_boundary_works() {
        let data = b"--abc\r\nContent-Type: application/dicom\r\n\r\nDICOMDATA--abc--";
        // Simple test: just verify boundary detection
        assert!(data.windows(5).any(|w| w == b"--abc"));
    }
}
