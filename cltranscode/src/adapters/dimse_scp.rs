// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DIMSE C-STORE SCP adapter - receive DICOM from modalities.
//!
//! nginx-way: one thread per association, no thread pool.
//! Only C-STORE + C-ECHO. No C-FIND, C-MOVE, C-GET.

#![deny(unsafe_code)]

use crate::adapters::pdu;
use crate::adapters::{AdapterError, InputAdapter};
use crate::spool::Spool;
use clarus_adapters::codec::CodecRegistry;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const ASSOC_TIMEOUT: Duration = Duration::from_secs(30);

/// DIMSE C-STORE SCP adapter.
///
/// Listens on a TCP port, accepts associations, receives C-STORE datasets.
/// Writes directly to spool - no mpsc.
pub struct DimseScp {
    #[allow(dead_code)]
    ae_title: String,
    #[allow(dead_code)]
    port: u16,
    /// Keep handle to join threads on drop.
    _listener_thread: Option<thread::JoinHandle<()>>,
}

impl DimseScp {
    /// Create a new DIMSE SCP, spawning the listener thread.
    /// Writes received datasets directly to the spool.
    pub fn new(
        ae_title: String,
        port: u16,
        spool: Arc<Spool>,
        shutdown: Arc<AtomicBool>,
        registry: CodecRegistry,
    ) -> Result<Self, AdapterError> {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).map_err(AdapterError::Io)?;
        listener.set_nonblocking(true).map_err(AdapterError::Io)?;

        let ae = ae_title.clone();
        let handle = thread::spawn(move || {
            run_listener(listener, &ae, spool, shutdown, registry);
        });

        log::info!("dimse_scp: listening on port {} (AE: {})", port, ae_title);

        Ok(Self {
            ae_title,
            port,
            _listener_thread: Some(handle),
        })
    }
}

impl InputAdapter for DimseScp {
    fn recv(&mut self) -> Result<Vec<u8>, AdapterError> {
        unreachable!("dimse_scp: recv() is deprecated — SCP writes directly to spool");
    }
}

impl Drop for DimseScp {
    fn drop(&mut self) {
        log::info!("dimse_scp: shutting down port {}", self.port);
    }
}

/// Main listener loop: accept connections, spawn one thread per association.
fn run_listener(
    listener: TcpListener,
    ae_title: &str,
    spool: Arc<Spool>,
    shutdown: Arc<AtomicBool>,
    registry: CodecRegistry,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                log::info!("dimse_scp: connection from {}", addr);
                let ae = ae_title.to_string();
                let sp = Arc::clone(&spool);
                let reg = registry.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_association(stream, &ae, &sp, &reg) {
                        log::error!("dimse_scp: association error: {}", e);
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    || e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::ConnectionAborted =>
            {
                // Transient Windows socket errors under load - don't kill the listener.
                log::warn!("dimse_scp: transient accept error ({}), continuing", e);
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log::error!("dimse_scp: accept error: {}", e);
                break;
            }
        }
    }
}

/// Handle a single DIMSE association on one TCP stream.
///
/// Processes multiple C-STORE operations on a single association.
/// Sends C-STORE-RSP immediately after spooling each slice, then
/// continues waiting for the next C-STORE-RQ - standard DIMSE SCP
/// behavior (one association, many C-STORE's).
fn handle_association(
    mut stream: TcpStream,
    ae_title: &str,
    spool: &Arc<Spool>,
    registry: &CodecRegistry,
) -> Result<(), AdapterError> {
    stream.set_nonblocking(false).map_err(AdapterError::Io)?;
    stream
        .set_read_timeout(Some(ASSOC_TIMEOUT))
        .map_err(AdapterError::Io)?;

    // Step 1: Read A-ASSOCIATE-RQ
    let (header, body) = pdu::read_pdu(&mut stream).map_err(AdapterError::Io)?;
    if header.pdu_type != pdu::PDU_AA_RQ {
        return Err(AdapterError::Protocol(format!(
            "expected A-ASSOCIATE-RQ, got PDU type {:02X}",
            header.pdu_type
        )));
    }

    // Extract called/calling AE titles from A-ASSOCIATE-RQ body
    // PS3.8 Section 9.3.2: fixed header is 68 bytes:
    //   offset 4..20  = Called AE  (who we are)
    //   offset 20..36 = Calling AE (who connected to us)
    let called_ae = if body.len() >= 20 {
        String::from_utf8_lossy(&body[4..20])
            .trim_end_matches(' ')
            .to_string()
    } else {
        String::new()
    };
    let calling_ae = if body.len() >= 36 {
        String::from_utf8_lossy(&body[20..36])
            .trim_end_matches(' ')
            .to_string()
    } else {
        String::new()
    };

    log::info!(
        "dimse_scp: association from '{}' (called='{}')",
        calling_ae,
        called_ae
    );

    if called_ae != ae_title && !ae_title.is_empty() {
        // Wrong AE title - reject
        let rj = pdu::build_associate_rj(0x07); // called AE title not recognized
        stream.write_all(&rj).map_err(AdapterError::Io)?;
        return Err(AdapterError::Protocol(format!(
            "wrong AE title: '{}' (expected '{}')",
            called_ae, ae_title
        )));
    }

    // Step 2: Parse presentation contexts from A-ASSOCIATE-RQ and build AC
    // Echo the SCU's context IDs - PS3.8 requires odd IDs, SCP must reuse them.
    let contexts = parse_presentation_contexts(&body, registry);
    log::info!(
        "dimse_scp: parsed {} presentation contexts, sending AC",
        contexts.len()
    );

    // Build context_id -> accepted transfer syntax map for downstream use.
    let ctx_ts: std::collections::HashMap<u8, String> = contexts
        .iter()
        .filter_map(|c| {
            c.transfer_syntaxes
                .first()
                .map(|ts| (c.context_id, ts.clone()))
        })
        .collect();

    // Called AE in AC = us (ae_title), Calling AE in AC = them (calling_ae)
    let ac = pdu::build_associate_ac(ae_title, &calling_ae, &contexts);
    stream.write_all(&ac).map_err(AdapterError::Io)?;
    log::info!("dimse_scp: AC sent, waiting for P-DATA");

    // Step 3: Process P-DATA-TF (C-STORE-RQ commands + datasets)
    // PS3.8 Section 9.2: group 0x0000 = command, group 0x0002+ = data.

    let mut dataset_bytes: Option<Vec<u8>> = None;
    let mut c_store_cmd: Option<Vec<u8>> = None;
    let mut c_store_ctx: Option<u8> = None; // context_id for TS lookup

    loop {
        let (header, body) = match pdu::read_pdu(&mut stream) {
            Ok(h) => h,
            Err(ref e) => {
                // 500ms poll timeout with pending C-STORE -> dataset complete.
                // Process it and keep the association open for more slices.
                if is_recoverable_timeout(e) && c_store_cmd.is_some() && dataset_bytes.is_some() {
                    process_pending_c_store(
                        &mut stream,
                        &mut c_store_cmd,
                        &mut dataset_bytes,
                        &mut c_store_ctx,
                        &ctx_ts,
                        spool,
                    )?;
                    stream
                        .set_read_timeout(Some(ASSOC_TIMEOUT))
                        .map_err(AdapterError::Io)?;
                    continue;
                }
                log::debug!("dimse_scp: read complete ({})", e);
                break;
            }
        };

        match header.pdu_type {
            pdu::PDU_PDATA => {
                log::debug!(
                    "dimse_scp: P-DATA-TF body={} bytes, body[..10]={:02x?}",
                    body.len(),
                    &body[..body.len().min(10)]
                );
                // Parse PDV(s) from P-DATA-TF body
                let mut pos = 0;
                while pos + 4 <= body.len() {
                    let pdv_len = u32::from_be_bytes([
                        body[pos],
                        body[pos + 1],
                        body[pos + 2],
                        body[pos + 3],
                    ]) as usize;
                    pos += 4;
                    if pos + pdv_len > body.len() {
                        break;
                    }
                    let ctx_id = body[pos];
                    // pos+1 = MCH byte (Message Control Header)
                    let mch = body[pos + 1];
                    let pdv_data = &body[pos + 2..pos + pdv_len];
                    pos += pdv_len;

                    // PS3.8 Section 9.2: DIMSE commands are group 0x0000. Data is group 0x0002+.
                    let is_command =
                        pdv_data.len() >= 2 && pdv_data[0] == 0x00 && pdv_data[1] == 0x00;

                    if is_command {
                        // Command set - parse (0000,0100) CommandField
                        if let Some(cmd_field) = extract_dimse_command(pdv_data) {
                            match cmd_field {
                                pdu::C_ECHO_RQ => {
                                    log::debug!("dimse_scp: C-ECHO-RQ");
                                    let rsp = build_c_echo_rsp(ctx_id, pdv_data);
                                    let pdata = pdu::build_pdata(ctx_id, &rsp, true);
                                    stream.write_all(&pdata).map_err(AdapterError::Io)?;
                                }
                                pdu::C_STORE_RQ => {
                                    log::debug!("dimse_scp: C-STORE-RQ");
                                    // If a previous C-STORE is pending (pipelined by SCU),
                                    // process it before starting the new one.
                                    if c_store_cmd.is_some() && dataset_bytes.is_some() {
                                        process_pending_c_store(
                                            &mut stream,
                                            &mut c_store_cmd,
                                            &mut dataset_bytes,
                                            &mut c_store_ctx,
                                            &ctx_ts,
                                            spool,
                                        )?;
                                    }
                                    c_store_cmd = Some(pdv_data.to_vec());
                                    c_store_ctx = Some(ctx_id);
                                    // After C-STORE-RQ: short poll to detect end of fragments.
                                    // pynetdicom doesn't set last-fragment bit (mch=02 instead of 01).
                                    // 500ms poll: if no more PDVs arrive, we're done.
                                    stream
                                        .set_read_timeout(Some(Duration::from_millis(500)))
                                        .map_err(AdapterError::Io)?;
                                }
                                pdu::C_GET_RQ => {
                                    // C-GET not supported - respond with SOP Class not supported
                                    log::info!(
                                        "dimse_scp: C-GET-RQ — not implemented, sending 0x0122"
                                    );
                                    let rsp = build_dimse_failure_rsp(
                                        ctx_id,
                                        pdv_data,
                                        pdu::C_GET_RSP,
                                        pdu::STATUS_SOP_CLASS_NOT_SUPPORTED,
                                    );
                                    let pdata = pdu::build_pdata(ctx_id, &rsp, true);
                                    stream.write_all(&pdata).map_err(AdapterError::Io)?;
                                }
                                pdu::C_MOVE_RQ => {
                                    log::info!(
                                        "dimse_scp: C-MOVE-RQ — not implemented, sending 0x0122"
                                    );
                                    let rsp = build_dimse_failure_rsp(
                                        ctx_id,
                                        pdv_data,
                                        pdu::C_MOVE_RSP,
                                        pdu::STATUS_SOP_CLASS_NOT_SUPPORTED,
                                    );
                                    let pdata = pdu::build_pdata(ctx_id, &rsp, true);
                                    stream.write_all(&pdata).map_err(AdapterError::Io)?;
                                }
                                pdu::N_EVENT_REPORT_RQ => {
                                    log::info!(
                                        "dimse_scp: N-EVENT-REPORT-RQ — not implemented, sending 0x0122"
                                    );
                                    let rsp = build_dimse_failure_rsp(
                                        ctx_id,
                                        pdv_data,
                                        pdu::N_EVENT_REPORT_RSP,
                                        pdu::STATUS_SOP_CLASS_NOT_SUPPORTED,
                                    );
                                    let pdata = pdu::build_pdata(ctx_id, &rsp, true);
                                    stream.write_all(&pdata).map_err(AdapterError::Io)?;
                                }
                                pdu::N_ACTION_RQ => {
                                    log::info!(
                                        "dimse_scp: N-ACTION-RQ (Storage Commitment) — not implemented, sending 0x0122"
                                    );
                                    let rsp = build_dimse_failure_rsp(
                                        ctx_id,
                                        pdv_data,
                                        pdu::N_ACTION_RSP,
                                        pdu::STATUS_SOP_CLASS_NOT_SUPPORTED,
                                    );
                                    let pdata = pdu::build_pdata(ctx_id, &rsp, true);
                                    stream.write_all(&pdata).map_err(AdapterError::Io)?;
                                }
                                _ => {
                                    log::warn!("dimse_scp: unknown command {:04X}", cmd_field);
                                }
                            }
                        }
                    } else {
                        // Dataset PDV - accumulate all fragments.
                        // We don't check last-fragment bit (pynetdicom incompatibility).
                        // Instead, 500ms read timeout (set after C-STORE-RQ) signals end of data.
                        log::debug!("dimse_scp: dataset PDV ctx={} mch={:02x}", ctx_id, mch);
                        if dataset_bytes.is_none() {
                            dataset_bytes = Some(Vec::new());
                        }
                        if let Some(ref mut buf) = dataset_bytes {
                            buf.extend_from_slice(pdv_data);
                        }
                    }
                }
            }
            pdu::PDU_AR_RQ => {
                // A-RELEASE-RQ -> process any pending C-STORE, then send A-RELEASE-RP
                if c_store_cmd.is_some() && dataset_bytes.is_some() {
                    process_pending_c_store(
                        &mut stream,
                        &mut c_store_cmd,
                        &mut dataset_bytes,
                        &mut c_store_ctx,
                        &ctx_ts,
                        spool,
                    )?;
                }
                let rp = pdu::build_release_rp();
                stream.write_all(&rp).map_err(AdapterError::Io)?;
                break;
            }
            pdu::PDU_ABORT => {
                log::warn!("dimse_scp: association aborted");
                break;
            }
            _ => {
                log::warn!("dimse_scp: unexpected PDU type {:02X}", header.pdu_type);
                break;
            }
        }
    }

    Ok(())
}

/// Process a pending C-STORE: normalize bytes, write to spool, send C-STORE-RSP.
/// Clears `cmd` and `bytes` on success - caller can reuse the Options for the next slice.
fn process_pending_c_store(
    stream: &mut TcpStream,
    cmd: &mut Option<Vec<u8>>,
    dataset_bytes: &mut Option<Vec<u8>>,
    ctx_id: &mut Option<u8>,
    ctx_ts: &std::collections::HashMap<u8, String>,
    spool: &Arc<Spool>,
) -> Result<(), AdapterError> {
    let c_store_cmd = cmd.take().unwrap();
    let c_store_ctx = ctx_id.take();
    let bytes = dataset_bytes.take().unwrap();

    let bytes = crate::dicom_utils::strip_group_0000(bytes);

    // Step 1: strip DICM file wrapper (preamble 128B + "DICM") if present.
    let bytes = if bytes.len() >= 132 && &bytes[128..132] == b"DICM" {
        bytes[132..].to_vec()
    } else if bytes.len() >= 4 && &bytes[..4] == b"DICM" {
        bytes[4..].to_vec()
    } else {
        bytes
    };

    // Step 3: normalize VR.
    // Explicit VR -> keep as-is + inject (0002,0010) for transcoder.
    // Implicit VR -> pass-through (already works for CT/uncompressed).
    // No PixelData -> pass-through (SR, PR, KO).
    let bytes = if crate::stages::transcoder::has_pixel_data(&bytes) && is_explicit_vr(&bytes) {
        let ts_uid = c_store_ctx
            .and_then(|id| ctx_ts.get(&id))
            .map(|s| s.as_str())
            .unwrap_or("1.2.840.10008.1.2.1");
        log::info!("dimse_scp: keeping Explicit VR, TS={}", ts_uid);
        inject_ts_into_explicit(bytes, ts_uid)
    } else {
        bytes // Implicit VR or no PixelData -- pass-through
    };
    let stem = spool.write_raw(&bytes).map_err(AdapterError::Io)?;
    log::info!("dimse_scp: spooled {}.dcm ({} bytes)", stem, bytes.len());

    // C-STORE-RSP 0x0000 immediately after spool
    let rsp = build_c_store_rsp(1, &c_store_cmd, pdu::STATUS_SUCCESS);
    let pdata = pdu::build_pdata(1, &rsp, true);
    match stream.write_all(&pdata) {
        Ok(()) => log::info!("dimse_scp: C-STORE-RSP sent (status=0x0000)"),
        Err(e) => log::warn!("dimse_scp: C-STORE-RSP failed (connection dead): {}", e),
    }
    Ok(())
}

/// Check if an I/O error is a recoverable timeout (WouldBlock/TimedOut).
fn is_recoverable_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Extract DIMSE CommandField (0000,0100) from raw command bytes.
fn extract_dimse_command(data: &[u8]) -> Option<u16> {
    // Command elements are Implicit VR LE (group 0000)
    // Tag (0000,0100) = bytes: 00 00 00 01 (LE: group=0x0000, elem=0x0100)
    let tag = &[0x00u8, 0x00, 0x00, 0x01];
    data.windows(4).position(|w| w == tag).and_then(|pos| {
        if pos + 10 <= data.len() {
            // Implicit VR: 4 tag + 4 length + value (need at least 2 for value)
            let len =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            if pos + 8 + len <= data.len() && len >= 2 {
                Some(u16::from_le_bytes([data[pos + 8], data[pos + 9]]))
            } else {
                None
            }
        } else {
            None
        }
    })
}

/// Build a C-STORE-RSP command dataset.
fn build_c_store_rsp(_ctx_id: u8, _rq_data: &[u8], status: u16) -> Vec<u8> {
    let mut cmd = Vec::new();
    // Implicit VR LE command elements
    // (0000,0100) CommandField = C_STORE_RSP
    write_implicit_element(&mut cmd, 0x0000, 0x0100, &pdu::C_STORE_RSP.to_le_bytes());
    // (0000,0900) Status
    write_implicit_element(&mut cmd, 0x0000, 0x0900, &status.to_le_bytes());
    // (0000,0120) MessageIDBeingRespondedTo - copy from request if possible
    let msg_id = extract_message_id(_rq_data).unwrap_or(1);
    write_implicit_element(&mut cmd, 0x0000, 0x0120, &msg_id.to_le_bytes());
    // (0000,0800) CommandDataSetType = 0x0101 (no dataset)
    write_implicit_element(&mut cmd, 0x0000, 0x0800, &0x0101u16.to_le_bytes());
    cmd
}

/// Build a C-ECHO-RSP command dataset.
fn build_c_echo_rsp(_ctx_id: u8, _rq_data: &[u8]) -> Vec<u8> {
    let mut cmd = Vec::new();
    write_implicit_element(&mut cmd, 0x0000, 0x0100, &pdu::C_ECHO_RSP.to_le_bytes());
    write_implicit_element(&mut cmd, 0x0000, 0x0900, &pdu::STATUS_SUCCESS.to_le_bytes());
    let msg_id = extract_message_id(_rq_data).unwrap_or(1);
    write_implicit_element(&mut cmd, 0x0000, 0x0120, &msg_id.to_le_bytes());
    write_implicit_element(&mut cmd, 0x0000, 0x0800, &0x0101u16.to_le_bytes());
    cmd
}

/// Build a failure response for unsupported DIMSE commands (C-GET, C-MOVE, N-EVENT, N-ACTION).
fn build_dimse_failure_rsp(
    _ctx_id: u8,
    _rq_data: &[u8],
    command_field: u16,
    status: u16,
) -> Vec<u8> {
    let mut cmd = Vec::new();
    // (0000,0100) CommandField = response command
    write_implicit_element(&mut cmd, 0x0000, 0x0100, &command_field.to_le_bytes());
    // (0000,0900) Status = failure code (0x0122 or 0xA700)
    write_implicit_element(&mut cmd, 0x0000, 0x0900, &status.to_le_bytes());
    // (0000,0120) MessageIDBeingRespondedTo
    let msg_id = extract_message_id(_rq_data).unwrap_or(1);
    write_implicit_element(&mut cmd, 0x0000, 0x0120, &msg_id.to_le_bytes());
    // (0000,0800) CommandDataSetType = 0x0101 (no dataset)
    write_implicit_element(&mut cmd, 0x0000, 0x0800, &0x0101u16.to_le_bytes());
    cmd
}

fn extract_message_id(data: &[u8]) -> Option<u16> {
    // (0000,0110) MessageID: LE bytes = 00 00 10 01
    let tag = &[0x00u8, 0x00, 0x10, 0x01];
    data.windows(4).position(|w| w == tag).and_then(|pos| {
        if pos + 10 <= data.len() {
            let len =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            if pos + 8 + len <= data.len() && len >= 2 {
                Some(u16::from_le_bytes([data[pos + 8], data[pos + 9]]))
            } else {
                None
            }
        } else {
            None
        }
    })
}

fn write_implicit_element(buf: &mut Vec<u8>, group: u16, element: u16, value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    buf.extend_from_slice(value);
}

/// Prepend (0002,0010) TransferSyntaxUID in Explicit VR LE format
/// to a dataset that lacks a meta-header (DIMSE C-STORE data).
fn inject_ts_into_explicit(mut data: Vec<u8>, ts_uid: &str) -> Vec<u8> {
    let ts_bytes = ts_uid.as_bytes();
    // DICOM UI: must be even-length, null-padded
    let ts_padded = if ts_bytes.len().is_multiple_of(2) {
        ts_bytes.len()
    } else {
        ts_bytes.len() + 1
    };
    let mut out = Vec::with_capacity(data.len() + 8 + ts_padded);
    // (0002,0010) UI - Explicit VR LE
    out.extend_from_slice(&0x0002u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"UI");
    out.extend_from_slice(&(ts_padded as u16).to_le_bytes());
    out.extend_from_slice(ts_bytes);
    if !ts_bytes.len().is_multiple_of(2) {
        out.push(0x00);
    }
    out.append(&mut data);
    out
}

/// Check if data has a PixelData element with undefined length (0xFFFFFFFF).
/// This is the universal detector for compressed DICOM data - works on
/// dataset elements directly, no meta-header needed (DIMSE C-STORE data
/// does not include group 0x0002).
#[allow(dead_code)]
fn has_undefined_length_pixeldata(data: &[u8]) -> bool {
    let tag = [0xE0u8, 0x7F, 0x10, 0x00];
    data.windows(4)
        .position(|w| w == tag)
        .and_then(|pos| {
            // Need at least tag(4) + VR(2) + reserved(2) + length(4) = 12 bytes
            if pos + 12 > data.len() {
                return None;
            }
            // Check VR is OB/OW (pixel data VRs)
            let vr = &data[pos + 4..pos + 6];
            if vr != b"OB" && vr != b"OW" {
                return None;
            }
            // Check length is undefined
            let len =
                u32::from_le_bytes([data[pos + 8], data[pos + 9], data[pos + 10], data[pos + 11]]);
            Some(len == 0xFFFF_FFFF)
        })
        .unwrap_or(false)
}

/// Extract TransferSyntaxUID (0002,0010) from Explicit VR meta-header bytes.
/// Scans the first 1024 bytes for the tag - meta-header is small and always
/// Explicit VR LE per DICOM PS3.10.
#[allow(dead_code)]
fn extract_ts_from_metaheader(data: &[u8]) -> String {
    let mut pos = 0usize;
    while pos + 8 <= data.len() && pos < 1024 {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if group != 0x0002 {
            break; // end of meta-header
        }
        let elem = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        pos += 4; // skip tag

        if pos + 2 > data.len() {
            break;
        }
        let vr = &data[pos..pos + 2];
        pos += 2;

        let is_ob = matches!(
            vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        let vlen: usize = if is_ob {
            if pos + 6 > data.len() {
                break;
            }
            pos += 2; // reserved
            let l = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4;
            l
        } else {
            let l = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            l
        };

        if pos + vlen > data.len() {
            break;
        }

        if (group, elem) == (0x0002, 0x0010) {
            let val = &data[pos..pos + vlen];
            return String::from_utf8_lossy(val)
                .trim_end_matches([' ', '\0'])
                .to_string();
        }

        pos += vlen;
    }
    // Fallback: assume uncompressed
    "1.2.840.10008.1.2".to_string()
}

/// Check if a TransferSyntaxUID represents compressed (encapsulated) pixel data.
/// Uncompressed TS: Implicit VR LE, Explicit VR LE, Explicit VR BE, Deflated EVRLE.
#[allow(dead_code)]
fn is_compressed_ts_uid(uid: &str) -> bool {
    !matches!(
        uid,
        "1.2.840.10008.1.2"      // Implicit VR LE
        | "1.2.840.10008.1.2.1"  // Explicit VR LE
        | "1.2.840.10008.1.2.2"  // Explicit VR BE
        | "1.2.840.10008.1.2.1.99" // Deflated Explicit VR LE
    )
}

/// Check if data starts with valid Explicit VR (e.g. "UI", "CS", "OB").
#[allow(dead_code)]
fn is_explicit_vr(data: &[u8]) -> bool {
    if data.len() < 6 {
        return false;
    }
    // First 4 bytes = tag, next 2 = VR
    let vr = &data[4..6];
    matches!(
        std::str::from_utf8(vr).ok(),
        Some(
            "AE" | "AS"
                | "AT"
                | "CS"
                | "DA"
                | "DS"
                | "DT"
                | "FL"
                | "FD"
                | "IS"
                | "LO"
                | "LT"
                | "OB"
                | "OD"
                | "OF"
                | "OL"
                | "OW"
                | "PN"
                | "SH"
                | "SL"
                | "SQ"
                | "SS"
                | "ST"
                | "TM"
                | "UC"
                | "UI"
                | "UL"
                | "UN"
                | "UR"
                | "US"
                | "UT"
        )
    )
}

/// Convert Explicit VR dataset to Implicit VR LE.
/// Walks elements, copies tag(4)+len(4)+value - drops VR+reserved bytes.
/// Handles encapsulated PixelData (undefined length 0xFFFFFFFF) by walking
/// Items until SequenceDelimiter - same logic as transcoder.rs.
#[allow(dead_code)]
fn convert_explicit_to_implicit(data: Vec<u8>) -> Result<Vec<u8>, AdapterError> {
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let _elem = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        // Skip group 0000/0004 (DIMSE commands, retired - shouldn't be here)
        if group == 0x0000 || group == 0x0004 {
            // Implicit VR: tag(4)+len(4)
            if pos + 8 > data.len() {
                break;
            }
            let vlen =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            pos += 8 + vlen;
            continue;
        }
        // Special: FFFE items/sequences have no VR - copy as-is (tag+len+value)
        if group == 0xFFFE {
            if pos + 8 > data.len() {
                break;
            }
            let vlen =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            let end = pos + 8 + vlen;
            if end > data.len() {
                break;
            }
            out.extend_from_slice(&data[pos..end]);
            pos = end;
            continue;
        }
        if pos + 6 > data.len() {
            break;
        }
        let is_ob = matches!(
            &data[pos + 4..pos + 6],
            b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        let (hdr, vlen) = if is_ob {
            if pos + 12 > data.len() {
                break;
            }
            (
                12,
                u32::from_le_bytes([data[pos + 8], data[pos + 9], data[pos + 10], data[pos + 11]])
                    as usize,
            )
        } else {
            if pos + 8 > data.len() {
                break;
            }
            (
                8,
                u16::from_le_bytes([data[pos + 6], data[pos + 7]]) as usize,
            )
        };
        // Write Implicit VR: tag(4) + len(4) + value
        out.extend_from_slice(&data[pos..pos + 4]);
        let val_start = pos + hdr;

        // Encapsulated PixelData or SQ with undefined length (0xFFFFFFFF):
        // copy tag + undefined length + rest of data as-is. Don't try to
        // walk Items - regular elements may appear inside SQ Items.
        // The transcoder handles encapsulated structure correctly.
        if vlen == 0xFFFF_FFFF {
            out.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            out.extend_from_slice(&data[val_start..]);
            return Ok(out);
        }

        out.extend_from_slice(&(vlen as u32).to_le_bytes());
        let val_end = val_start + vlen;
        if val_end > data.len() {
            break;
        }
        out.extend_from_slice(&data[val_start..val_end]);
        pos = val_end;
    }
    Ok(out)
}

/// Parse presentation contexts from an A-ASSOCIATE-RQ body.
///
/// Scans for Presentation Context items (type 0x20) and extracts
/// context_id + abstract syntax. Returns contexts ready for
/// `build_associate_ac` - we accept everything with Explicit VR LE.
fn parse_presentation_contexts(
    body: &[u8],
    registry: &CodecRegistry,
) -> Vec<pdu::PresentationContext> {
    let mut contexts = Vec::new();
    let mut pos = 68; // skip fixed header (protocol, AEs, reserved)

    while pos + 4 <= body.len() {
        let item_type = body[pos];
        pos += 1;
        let _reserved = body[pos];
        pos += 1;
        let item_len = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
        pos += 2;

        if pos + item_len > body.len() {
            break;
        }

        if item_type == 0x20 {
            // Presentation Context (RQ)
            if item_len >= 4 {
                let ctx_id = body[pos]; // PS3.8: odd number 1..255
                let _reserved2 = &body[pos + 1..pos + 4];
                // Scan sub-items for abstract syntax (0x30)
                let mut sub_pos = pos + 4;
                let sub_end = pos + item_len;
                let mut abstract_syntax = String::new();
                let mut scu_transfer_syntaxes: Vec<String> = Vec::new();
                while sub_pos + 4 <= sub_end {
                    let sub_type = body[sub_pos];
                    sub_pos += 1;
                    let _sub_reserved = body[sub_pos];
                    sub_pos += 1;
                    let sub_len = u16::from_be_bytes([body[sub_pos], body[sub_pos + 1]]) as usize;
                    sub_pos += 2;
                    if sub_type == 0x30 && sub_pos + sub_len <= sub_end {
                        abstract_syntax =
                            String::from_utf8_lossy(&body[sub_pos..sub_pos + sub_len]).to_string();
                    } else if sub_type == 0x40 && sub_pos + sub_len <= sub_end {
                        let ts =
                            String::from_utf8_lossy(&body[sub_pos..sub_pos + sub_len]).to_string();
                        scu_transfer_syntaxes.push(ts);
                    }
                    sub_pos += sub_len;
                }
                // Filter TS against codec registry: keep only supported ones
                let supported_ts: Vec<String> = scu_transfer_syntaxes
                    .into_iter()
                    .filter(|ts| registry.supports(ts))
                    .collect();
                // Fallback: if no supported TS, default to Implicit VR LE (always available)
                let final_ts = if supported_ts.is_empty() {
                    vec!["1.2.840.10008.1.2".to_string()]
                } else {
                    supported_ts
                };
                if ctx_id % 2 == 1 {
                    log::debug!(
                        "dimse_scp: ctx {}: abs={} accepted {} TS",
                        ctx_id,
                        abstract_syntax,
                        final_ts.len()
                    );
                    contexts.push(pdu::PresentationContext {
                        context_id: ctx_id,
                        result_reason: 0x00,
                        abstract_syntax,
                        transfer_syntaxes: final_ts,
                    });
                }
            }
        }
        pos += item_len;
    }
    contexts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_dimse_command_finds_c_echo() {
        let mut cmd = Vec::new();
        write_implicit_element(&mut cmd, 0x0000, 0x0100, &pdu::C_ECHO_RQ.to_le_bytes());
        write_implicit_element(&mut cmd, 0x0000, 0x0110, &1u16.to_le_bytes());
        assert_eq!(extract_dimse_command(&cmd), Some(pdu::C_ECHO_RQ));
    }

    #[test]
    fn extract_dimse_command_finds_c_store() {
        let mut cmd = Vec::new();
        write_implicit_element(&mut cmd, 0x0000, 0x0100, &pdu::C_STORE_RQ.to_le_bytes());
        assert_eq!(extract_dimse_command(&cmd), Some(pdu::C_STORE_RQ));
    }

    #[test]
    fn extract_message_id_works() {
        let mut cmd = Vec::new();
        write_implicit_element(&mut cmd, 0x0000, 0x0110, &42u16.to_le_bytes());
        assert_eq!(extract_message_id(&cmd), Some(42));
    }

    // -- extract_ts_from_metaheader tests --------------------------

    #[test]
    fn extract_ts_p14() {
        let data = build_meta_header(b"1.2.840.10008.1.2.4.70\0");
        assert_eq!(extract_ts_from_metaheader(&data), "1.2.840.10008.1.2.4.70");
    }

    #[test]
    fn extract_ts_implicit_vr_le() {
        let data = build_meta_header(b"1.2.840.10008.1.2\0");
        assert_eq!(extract_ts_from_metaheader(&data), "1.2.840.10008.1.2");
    }

    #[test]
    fn extract_ts_jpegls() {
        let data = build_meta_header(b"1.2.840.10008.1.2.4.80\0");
        assert_eq!(extract_ts_from_metaheader(&data), "1.2.840.10008.1.2.4.80");
    }

    #[test]
    fn extract_ts_no_meta_header_falls_back() {
        // Data without meta-header -> returns default Implicit VR LE
        let data = vec![0x00u8; 20];
        assert_eq!(extract_ts_from_metaheader(&data), "1.2.840.10008.1.2");
    }

    fn build_meta_header(ts_value: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        // (0002,0010) UI
        d.extend_from_slice(&0x0002u16.to_le_bytes());
        d.extend_from_slice(&0x0010u16.to_le_bytes());
        d.extend_from_slice(b"UI");
        d.extend_from_slice(&(ts_value.len() as u16).to_le_bytes());
        d.extend_from_slice(ts_value);
        d
    }

    // -- is_compressed_ts_uid tests ---------------------------------

    #[test]
    fn uncompressed_ts_are_not_compressed() {
        assert!(!is_compressed_ts_uid("1.2.840.10008.1.2")); // Implicit VR LE
        assert!(!is_compressed_ts_uid("1.2.840.10008.1.2.1")); // Explicit VR LE
        assert!(!is_compressed_ts_uid("1.2.840.10008.1.2.2")); // Explicit VR BE
    }

    #[test]
    fn compressed_ts_are_compressed() {
        assert!(is_compressed_ts_uid("1.2.840.10008.1.2.4.50")); // JPEG Baseline
        assert!(is_compressed_ts_uid("1.2.840.10008.1.2.4.57")); // JPEG Lossless P14
        assert!(is_compressed_ts_uid("1.2.840.10008.1.2.4.70")); // JPEG Lossless SV1
        assert!(is_compressed_ts_uid("1.2.840.10008.1.2.4.80")); // JPEG-LS Lossless
        assert!(is_compressed_ts_uid("1.2.840.10008.1.2.4.81")); // JPEG-LS Near-Lossless
        assert!(is_compressed_ts_uid("1.2.840.10008.1.2.4.90")); // JPEG2000 Lossless
        assert!(is_compressed_ts_uid("1.2.840.10008.1.2.4.91")); // JPEG2000 Lossy
        assert!(is_compressed_ts_uid("1.2.840.10008.1.2.5")); // RLE Lossless
    }
}
