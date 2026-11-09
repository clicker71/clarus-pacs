// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A fake WebSocket Notification Connection peer, for the channel tests.
//!
//! `fake_origin` answers one HTTP request per connection and closes, which is
//! exactly wrong for a notification channel: the channel is opened once and
//! then held while the server pushes Event Reports down it. This fake is the
//! small server that does that - it completes the RFC 6455 handshake and then
//! sends whatever frames the test queued, on demand.
//!
//! It is deliberately dumb: it does not parse the client's frames beyond what
//! it must to see the close, and it never masks (the server side must not).
//! Its one job is to be a peer the client's real handshake and frame decoder
//! have to agree with, so a regression in either shows up here.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The GUID the handshake digest appends (RFC 6455 4.2.2).
const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// A running fake channel peer.
pub struct FakeChannel {
    port: u16,
    /// The client's `Sec-WebSocket-Key` values, in arrival order. Lets a
    /// test prove the handshake reached this peer at all.
    keys: Arc<Mutex<Vec<String>>>,
    /// How many connections were accepted, including reconnects.
    connections: Arc<AtomicUsize>,
    /// Frames the peer should send once a client is attached.
    outbox: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl FakeChannel {
    /// Start the peer on an ephemeral port.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");

        let keys = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let outbox: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_keys = Arc::clone(&keys);
        let thread_connections = Arc::clone(&connections);
        let thread_outbox = Arc::clone(&outbox);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread_connections.fetch_add(1, Ordering::Relaxed);
                        handle_client(stream, &thread_keys, &thread_outbox, &thread_stop);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            port,
            keys,
            connections,
            outbox,
            stop,
            _thread: handle,
        }
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The handshake keys seen so far.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.keys.lock().expect("keys lock").clone()
    }

    /// How many connections were accepted.
    #[must_use]
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Queue a text frame to send to the next attached client.
    pub fn send_text(&self, payload: &str) {
        self.outbox
            .lock()
            .expect("outbox lock")
            .push(server_frame(0x1, payload.as_bytes()));
    }

    /// Queue a close frame.
    pub fn send_close(&self) {
        let mut body = Vec::with_capacity(2);
        body.extend_from_slice(&1000u16.to_be_bytes());
        self.outbox
            .lock()
            .expect("outbox lock")
            .push(server_frame(0x8, &body));
    }
}

impl Drop for FakeChannel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Serve one client: handshake, then push whatever is in the outbox.
fn handle_client(
    mut stream: TcpStream,
    keys: &Arc<Mutex<Vec<String>>>,
    outbox: &Arc<Mutex<Vec<Vec<u8>>>>,
    stop: &Arc<AtomicBool>,
) {
    // The listener is non-blocking so the accept loop can poll the stop
    // flag, and an accepted socket INHERITS that mode. The read timeouts set
    // below do nothing on a non-blocking socket, so the mode is restored
    // first. This harness happened to survive without the line because its
    // read loop ignores read errors, but the mode is still wrong and the
    // handshake read is the same race.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(head) = read_head(&mut stream) else {
        return;
    };

    let key = header_value(&head, "sec-websocket-key").unwrap_or_default();
    keys.lock().expect("keys lock").push(key.clone());

    let accept = accept_key(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(response.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    // Now hold the connection and drain frames the test queued. A short read
    // timeout keeps the loop responsive to the stop flag without needing a
    // second socket to wake it.
    //
    // The client's traffic is READ AND DISCARDED, deliberately. This peer has
    // nothing to say in response to a pong or a close, and it must not try to
    // interpret those bytes: a first version scanned every received byte for
    // a CLOSE opcode nibble, which a MASKED client frame can trigger by
    // chance - the mask and the payload are arbitrary bytes. That made the
    // peer hang up on a healthy connection, intermittently, which is exactly
    // the kind of flake this file exists to avoid.
    //
    // The connection now ends only when the client closes it (a zero-length
    // read) or the test stops the peer. A timeout is the EXPECTED result of
    // every read here, because the client has nothing to say after the
    // handshake; any other error is also ignored so a transient socket
    // condition cannot end a healthy session early. The write side is what
    // reports a genuinely dead peer.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut scratch = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        match stream.read(&mut scratch) {
            // A zero-length read is the client having closed its end.
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => {}
        }

        let pending: Vec<Vec<u8>> = {
            let mut queue = outbox.lock().expect("outbox lock");
            std::mem::take(&mut *queue)
        };
        for frame in pending {
            if stream.write_all(&frame).is_err() {
                return;
            }
            if stream.flush().is_err() {
                return;
            }
        }
    }
}

/// Read up to the end of the request head.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut raw = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&raw).into_owned());
        }
        if raw.len() > 8192 {
            return None;
        }
    }
}

/// The value of a header, case-insensitively.
fn header_value(head: &str, name: &str) -> Option<String> {
    for line in head.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Encode an unmasked server frame (RFC 6455 5.2).
fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode); // FIN + opcode, no RSV bits
    let len = payload.len();
    if len < 126 {
        frame.push(len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

// ---------------------------------------------------------------------------
// The handshake digest, duplicated so this fake does not depend on the code
// under test. A fake that reuses the client's `accept_key` would agree with
// the client even if BOTH were wrong.
// ---------------------------------------------------------------------------

/// SHA-1 (RFC 3174).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in padded.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// The `Sec-WebSocket-Accept` value for a key.
fn accept_key(key: &str) -> String {
    let mut input = Vec::with_capacity(key.len() + WS_GUID.len());
    input.extend_from_slice(key.as_bytes());
    input.extend_from_slice(WS_GUID);
    base64(&sha1(&input))
}

/// Standard base64 (RFC 4648), padded.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        out.push(TABLE[(b[0] >> 2) as usize] as char);
        out.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b[1] & 0x0F) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b[2] & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
