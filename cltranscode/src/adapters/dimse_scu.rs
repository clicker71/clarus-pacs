// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DIMSE C-STORE SCU adapter - forward DICOM to downstream DIMSE server.
//!
//! nginx-way: open TCP, associate, send C-STORE, read status, release.

#![deny(unsafe_code)]

use crate::adapters::pdu;
use crate::adapters::{AdapterError, OutputAdapter};
use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const KEEPALIVE_IDLE: Duration = Duration::from_secs(5);

/// Cached DIMSE connection - TCP stream + negotiated parameters.
/// Reused between sequential `send()` calls to avoid per-slice
/// TCP + DIMSE association overhead (~3ms per slice).
struct CachedConn {
    stream: TcpStream,
    sop_class: String,    // which SOP Class was negotiated
    ts_uid: String,       // which Transfer Syntax was negotiated
    last_used: Instant,   // idle eviction timer
    next_message_id: u16, // increments per C-STORE (PS3.7 Section C.1.2)
}

/// DIMSE C-STORE SCU - sends DICOM via DIMSE protocol.
pub struct DimseScu {
    called_ae: String,  // who we connect TO (e.g., CLBRIDGE)
    calling_ae: String, // who we ARE (e.g., CLTRANSCODE)
    host: String,
    port: u16,
    cached: Option<CachedConn>,
}

impl DimseScu {
    pub fn new(called_ae: String, calling_ae: String, host: String, port: u16) -> Self {
        Self {
            called_ae,
            calling_ae,
            host,
            port,
            cached: None,
        }
    }

    /// Send C-ECHO to verify downstream DIMSE connectivity.
    ///
    /// Opens a fresh TCP connection, associates with Verification SOP Class,
    /// sends C-ECHO-RQ, checks 0x0000 status, releases.
    /// Reuses the same PDU helpers as `send()` - no byte-level hand-roll.
    pub fn send_c_echo(&mut self) -> Result<(), AdapterError> {
        let addr = format!("{}:{}", self.host, self.port);

        let mut stream = TcpStream::connect(&addr).map_err(AdapterError::Io)?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(AdapterError::Io)?;

        // Step 1: A-ASSOCIATE-RQ with Verification SOP Class only
        let contexts = vec![pdu::PresentationContext {
            context_id: 1,
            result_reason: 0,
            abstract_syntax: "1.2.840.10008.1.1".into(), // Verification SOP Class
            transfer_syntaxes: vec!["1.2.840.10008.1.2".into()], // Implicit VR LE
        }];

        let rq = pdu::build_associate_rq(&self.called_ae, &self.calling_ae, &contexts);
        stream.write_all(&rq).map_err(AdapterError::Io)?;

        let (header, _ac_body) = pdu::read_pdu(&mut stream).map_err(AdapterError::Io)?;
        if header.pdu_type != pdu::PDU_AA_AC {
            return Err(AdapterError::Protocol("association rejected".into()));
        }

        // Step 2: C-ECHO-RQ (includes mandatory AffectedSOPClassUID)
        let cmd = build_c_echo_rq();
        let pdata = pdu::build_pdata(1, &cmd, true);
        stream.write_all(&pdata).map_err(AdapterError::Io)?;

        // Step 3: Read C-ECHO-RSP
        let (header, body) = pdu::read_pdu(&mut stream).map_err(AdapterError::Io)?;
        if header.pdu_type != pdu::PDU_PDATA {
            return Err(AdapterError::Protocol(format!(
                "expected P-DATA-TF for C-ECHO-RSP, got PDU type {:02X}",
                header.pdu_type
            )));
        }

        // Parse status from response - same as send() does for C-STORE-RSP.
        // parse_c_store_status scans the raw P-DATA-TF body for the (0000,0900)
        // Status tag; PDV headers are transparent to the tag scanner.
        let status = parse_c_store_status(&body);

        // Step 4: A-RELEASE-RQ
        let release = pdu::build_pdu(pdu::PDU_AR_RQ, &[0, 0, 0, 0]);
        stream.write_all(&release).map_err(AdapterError::Io)?;
        // Best-effort: don't fail if RELEASE-RP is missing

        if status == pdu::STATUS_SUCCESS {
            Ok(())
        } else {
            Err(AdapterError::Protocol(format!(
                "C-ECHO status {:04X}",
                status
            )))
        }
    }

    // -- Keep-alive helpers ---------------------------------------

    /// Extract SOP Class UID + Transfer Syntax UID from DICOM bytes.
    fn extract_params(&self, bytes: &[u8]) -> (String, String) {
        let sop_class =
            extract_sop_class(bytes).unwrap_or_else(|| "1.2.840.10008.5.1.4.1.1.2".into());
        let ts_uid = extract_ts_uid(bytes).unwrap_or_else(|| "1.2.840.10008.1.2.1".into());
        (sop_class, ts_uid)
    }

    /// Open TCP + negotiate DIMSE association for the given SOP Class and TS.
    fn connect_and_associate(
        &self,
        sop_class: &str,
        ts_uid: &str,
    ) -> Result<CachedConn, AdapterError> {
        let addr = format!("{}:{}", self.host, self.port);
        log::info!("dimse_scu: connecting to {}", addr);

        let mut stream = TcpStream::connect(&addr).map_err(AdapterError::Io)?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .map_err(AdapterError::Io)?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(AdapterError::Io)?;

        let mut transfer_syntaxes = vec![ts_uid.to_string()];
        if ts_uid != "1.2.840.10008.1.2.1" {
            transfer_syntaxes.push("1.2.840.10008.1.2.1".into());
        }
        if ts_uid != "1.2.840.10008.1.2" {
            transfer_syntaxes.push("1.2.840.10008.1.2".into());
        }

        let contexts = vec![pdu::PresentationContext {
            context_id: 1,
            result_reason: 0,
            abstract_syntax: sop_class.to_string(),
            transfer_syntaxes,
        }];

        let rq = pdu::build_associate_rq(&self.called_ae, &self.calling_ae, &contexts);
        stream.write_all(&rq).map_err(AdapterError::Io)?;

        let (header, _body) = pdu::read_pdu(&mut stream).map_err(AdapterError::Io)?;
        match header.pdu_type {
            pdu::PDU_AA_AC => log::debug!("dimse_scu: association accepted"),
            pdu::PDU_AA_RJ => {
                return Err(AdapterError::Protocol("association rejected".into()));
            }
            other => {
                return Err(AdapterError::Protocol(format!(
                    "unexpected PDU type {:02X} during association",
                    other
                )));
            }
        }

        Ok(CachedConn {
            stream,
            sop_class: sop_class.to_string(),
            ts_uid: ts_uid.to_string(),
            last_used: Instant::now(),
            next_message_id: 1,
        })
    }

    /// Send C-STORE-RQ + dataset over an already-established association.
    fn send_cstore(cached: &mut CachedConn, dataset: &[u8]) -> Result<(), AdapterError> {
        let msg_id = cached.next_message_id;
        cached.next_message_id = cached.next_message_id.wrapping_add(1);

        let cmd = build_c_store_rq_with_id(dataset, msg_id);
        let pdata_cmd = pdu::build_pdata(1, &cmd, true);
        cached
            .stream
            .write_all(&pdata_cmd)
            .map_err(AdapterError::Io)?;

        let pdata_dataset = pdu::build_pdata(2, dataset, false);
        cached
            .stream
            .write_all(&pdata_dataset)
            .map_err(AdapterError::Io)?;

        let (header, body) = pdu::read_pdu(&mut cached.stream).map_err(AdapterError::Io)?;
        if header.pdu_type != pdu::PDU_PDATA {
            return Err(AdapterError::Protocol(
                "expected P-DATA-TF for C-STORE-RSP".into(),
            ));
        }

        let status = parse_c_store_status(&body);
        log::info!("dimse_scu: C-STORE status {:04X}", status);

        cached.last_used = Instant::now();

        match status {
            0x0000 => Ok(()),
            other => Err(AdapterError::Protocol(format!(
                "C-STORE failed with status {:04X}",
                other
            ))),
        }
    }

    /// Prepare dataset for DIMSE: strip DICM preamble, handle group 0x0002.
    fn prepare_dataset(bytes: &[u8], ts_uid: &str) -> Vec<u8> {
        let full_dataset = if bytes.len() >= 132 && &bytes[128..132] == b"DICM" {
            log::debug!(
                "dimse_scu: stripping DICM preamble ({}B → {}B)",
                bytes.len(),
                bytes.len() - 132
            );
            bytes[132..].to_vec()
        } else {
            bytes.to_vec()
        };

        if ts_uid == "1.2.840.10008.1.2" {
            skip_group_0002(&full_dataset).to_vec()
        } else {
            full_dataset
        }
    }
}

impl OutputAdapter for DimseScu {
    fn send(&mut self, bytes: &[u8]) -> Result<(), AdapterError> {
        let (sop_class, ts_uid) = self.extract_params(bytes);

        // -- Keep-alive: try to reuse cached connection --
        if let Some(ref mut cached) = self.cached {
            let params_match = cached.sop_class == sop_class && cached.ts_uid == ts_uid;
            let fresh = cached.last_used.elapsed() < KEEPALIVE_IDLE;

            if params_match && fresh {
                log::debug!(
                    "dimse_scu: reusing cached connection (sop={}, ts={})",
                    sop_class,
                    ts_uid
                );
                let dataset = Self::prepare_dataset(bytes, &ts_uid);
                match Self::send_cstore(cached, &dataset) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        log::warn!("dimse_scu: cached connection failed ({}), reconnecting", e);
                        self.cached = None;
                    }
                }
            } else {
                if !params_match {
                    log::debug!(
                        "dimse_scu: SOP/TS mismatch (cached: {}/{}, new: {}/{}), re-associating",
                        cached.sop_class,
                        cached.ts_uid,
                        sop_class,
                        ts_uid
                    );
                } else {
                    log::debug!(
                        "dimse_scu: cached connection expired ({}s idle), releasing",
                        cached.last_used.elapsed().as_secs()
                    );
                }
                // Best-effort A-RELEASE - connection is dead or stale anyway
                let release = pdu::build_pdu(pdu::PDU_AR_RQ, &[0x00, 0x00, 0x00, 0x00]);
                let _ = cached.stream.write_all(&release);
                self.cached = None;
            }
        }

        // -- Fresh connection --
        let mut cached = self.connect_and_associate(&sop_class, &ts_uid)?;
        let dataset = Self::prepare_dataset(bytes, &ts_uid);
        Self::send_cstore(&mut cached, &dataset)?;

        // Keep association open for next call - no A-RELEASE.
        self.cached = Some(cached);
        Ok(())
    }
}

/// Skip Explicit VR group 0x0002 elements at the start of data.
/// Returns a slice starting at the first non-0002 element.
/// DIMSE C-STORE must not include File Meta Information (PS3.7).
fn skip_group_0002(data: &[u8]) -> &[u8] {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if group != 0x0002 {
            break;
        }
        pos += 4; // skip tag
        if pos + 2 > data.len() {
            break;
        }
        let is_ob = matches!(
            &data[pos..pos + 2],
            b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        pos += 2; // skip VR
        if is_ob {
            if pos + 6 > data.len() {
                break;
            }
            pos += 2; // reserved
            let vlen = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            pos += 4 + vlen;
        } else {
            let vlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2 + vlen;
        }
    }
    &data[pos..]
}

/// Extract TransferSyntaxUID from DICOM data.
/// For data with DICM preamble: reads (0002,0010) from Explicit VR meta-header.
/// For DIMSE data without preamble: checks if first 4 bytes look like group 0002
/// (SCP now injects meta-header for all Explicit VR data). Falls back to None.
fn extract_ts_uid(data: &[u8]) -> Option<String> {
    // Determine start offset: DICM preamble -> 132, or group 0002 at position 0.
    let start = if data.len() >= 140 && &data[128..132] == b"DICM" {
        132
    } else if data.len() >= 4 {
        let group = u16::from_le_bytes([data[0], data[1]]);
        if group == 0x0002 {
            log::debug!("dimse_scu: no DICM preamble, but group 0002 at offset 0 (SCP-injected)");
            0
        } else {
            log::debug!(
                "dimse_scu: no DICM preamble, data[..4]={:02x?}",
                &data[..data.len().min(4)]
            );
            return None;
        }
    } else {
        return None;
    };

    log::debug!("dimse_scu: scanning meta-header from offset {}", start);
    let mut pos = start;
    while pos + 8 <= data.len() && pos < start + 512 {
        let group = u16::from_le_bytes([data[pos], data[pos + 1]]);
        if group != 0x0002 {
            break;
        }
        let elem = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        pos += 4;
        if pos + 2 > data.len() {
            break;
        }
        let is_ob = matches!(
            &data[pos..pos + 2],
            b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        );
        pos += 2;
        let vlen: usize = if is_ob {
            if pos + 6 > data.len() {
                break;
            }
            pos += 2;
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
            let ts = String::from_utf8_lossy(val)
                .trim_end_matches([' ', '\0'])
                .to_string();
            log::debug!("dimse_scu: found TS={} in meta-header", ts);
            return Some(ts);
        }
        pos += vlen;
    }
    log::debug!("dimse_scu: (0002,0010) not found in meta-header");
    None
}

/// Extract SOP Class UID (0008,0016) from DICOM dataset.
fn extract_sop_class(data: &[u8]) -> Option<String> {
    let obj = dicom_object::FileDicomObject::from_reader(data).ok()?;
    obj.get(dicom_object::Tag(0x0008, 0x0016))?
        .value()
        .to_str()
        .ok()
        .map(|cow| cow.into_owned())
}

/// Build C-STORE-RQ DIMSE command.
#[allow(dead_code)]
fn build_c_store_rq(dataset: &[u8]) -> Vec<u8> {
    build_c_store_rq_with_id(dataset, 1)
}

/// Build C-STORE-RQ with explicit Message ID (for keep-alive reuse).
fn build_c_store_rq_with_id(dataset: &[u8], message_id: u16) -> Vec<u8> {
    let sop_class =
        extract_sop_class(dataset).unwrap_or_else(|| "1.2.840.10008.5.1.4.1.1.2".into());
    let sop_instance =
        extract_sop_instance(dataset).unwrap_or_else(|| "1.2.840.10008.5.1.4.1.1.2.1".into());

    let mut cmd = Vec::new();
    write_implicit_element(&mut cmd, 0x0000, 0x0100, &pdu::C_STORE_RQ.to_le_bytes());
    write_implicit_element(&mut cmd, 0x0000, 0x0110, &message_id.to_le_bytes());
    write_implicit_element(&mut cmd, 0x0000, 0x0700, &0u16.to_le_bytes()); // Priority
    write_implicit_element(&mut cmd, 0x0000, 0x0800, &0u16.to_le_bytes()); // CommandDataSetType
    write_implicit_str(&mut cmd, 0x0000, 0x0002, &sop_class); // AffectedSOPClassUID
    write_implicit_str(&mut cmd, 0x0000, 0x1000, &sop_instance); // AffectedSOPInstanceUID
    cmd
}

/// Build C-ECHO-RQ DIMSE command (no dataset).
///
/// Includes mandatory AffectedSOPClassUID (Verification) - the missing field
/// that made the hand-rolled C-ECHO in health.rs fail with an
/// "unexpected C-ECHO service message" error.
fn build_c_echo_rq() -> Vec<u8> {
    let mut cmd = Vec::new();
    write_implicit_element(&mut cmd, 0x0000, 0x0100, &pdu::C_ECHO_RQ.to_le_bytes()); // CommandField
    write_implicit_element(&mut cmd, 0x0000, 0x0110, &1u16.to_le_bytes()); // MessageID
    write_implicit_str(&mut cmd, 0x0000, 0x0002, "1.2.840.10008.1.1"); // AffectedSOPClassUID = Verification
    write_implicit_element(&mut cmd, 0x0000, 0x0800, &0x0101u16.to_le_bytes()); // DataSetType = no dataset
    cmd
}

fn extract_sop_instance(data: &[u8]) -> Option<String> {
    let obj = dicom_object::FileDicomObject::from_reader(data).ok()?;
    obj.get(dicom_object::Tag(0x0008, 0x0018))?
        .value()
        .to_str()
        .ok()
        .map(|cow| cow.into_owned())
}

/// Parse C-STORE status from P-DATA-TF response body.
fn parse_c_store_status(body: &[u8]) -> u16 {
    // Look for (0000,0900) Status in the command PDV
    let tag = [0x00u8, 0x00, 0x00, 0x09];
    body.windows(4)
        .position(|w| w == tag)
        .and_then(|pos| {
            // Need at least: 4 (tag) + 4 (length) + 2 (status US) = 10 bytes
            if pos + 10 <= body.len() {
                let len = u32::from_le_bytes([
                    body[pos + 4],
                    body[pos + 5],
                    body[pos + 6],
                    body[pos + 7],
                ]) as usize;
                if pos + 8 + len <= body.len() && len >= 2 {
                    Some(u16::from_le_bytes([body[pos + 8], body[pos + 9]]))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .unwrap_or(0xFFFF)
}

fn write_implicit_element(buf: &mut Vec<u8>, group: u16, element: u16, value: &[u8]) {
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    buf.extend_from_slice(value);
}

fn write_implicit_str(buf: &mut Vec<u8>, group: u16, element: u16, s: &str) {
    let mut val = s.as_bytes().to_vec();
    if !val.len().is_multiple_of(2) {
        val.push(0x00);
    }
    write_implicit_element(buf, group, element, &val);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_sop_class_works() {
        let dcm = build_minimal_dicom();
        let uid = extract_sop_class(&dcm);
        assert_eq!(uid.as_deref(), Some("1.2.840.10008.5.1.4.1.1.2"));
    }

    #[test]
    fn extract_sop_instance_works() {
        let dcm = build_minimal_dicom();
        let uid = extract_sop_instance(&dcm);
        assert_eq!(uid.as_deref(), Some("1.2.840.10008.5.1.4.1.1.2.999"));
    }

    fn build_minimal_dicom() -> Vec<u8> {
        // Build a valid minimal DICOM using dicom-object to ensure parseability.
        use dicom_core::VR;
        use dicom_object::InMemDicomObject;
        let mut obj = InMemDicomObject::new_empty();
        obj.put_str(
            dicom_object::Tag(0x0008, 0x0016),
            VR::UI,
            "1.2.840.10008.5.1.4.1.1.2",
        );
        obj.put_str(
            dicom_object::Tag(0x0008, 0x0018),
            VR::UI,
            "1.2.840.10008.5.1.4.1.1.2.999",
        );
        // Wrap in FileDicomObject with meta to produce a valid DICOM file
        use dicom_object::meta::FileMetaTableBuilder;
        let file_obj = obj
            .with_meta(
                FileMetaTableBuilder::new()
                    .transfer_syntax("1.2.840.10008.1.2.1")
                    .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.2"),
            )
            .expect("valid meta");
        let mut out = Vec::new();
        file_obj.write_all(&mut out).expect("write_all");
        out
    }

    #[test]
    fn extract_params_returns_defaults_for_minimal_dicom() {
        let scu = DimseScu::new("CALLED".into(), "CALLING".into(), "127.0.0.1".into(), 8104);
        let dcm = build_minimal_dicom();
        let (sop, ts) = scu.extract_params(&dcm);
        assert_eq!(sop, "1.2.840.10008.5.1.4.1.1.2");
        assert!(ts.contains("1.2.840.10008.1.2"));
    }

    #[test]
    fn build_c_store_rq_with_id_increments_message_id() {
        let dcm = build_minimal_dicom();
        let cmd1 = build_c_store_rq_with_id(&dcm, 1);
        let cmd2 = build_c_store_rq_with_id(&dcm, 42);
        let msg_id_1 = extract_message_id_test(&cmd1);
        let msg_id_2 = extract_message_id_test(&cmd2);
        assert_eq!(msg_id_1, Some(1));
        assert_eq!(msg_id_2, Some(42));
    }

    fn extract_message_id_test(cmd: &[u8]) -> Option<u16> {
        let tag: [u8; 4] = [0x00, 0x00, 0x10, 0x01]; // group 0000, element 0110
        cmd.windows(4).position(|w| w == tag).and_then(|pos| {
            if pos + 10 <= cmd.len() {
                Some(u16::from_le_bytes([cmd[pos + 8], cmd[pos + 9]]))
            } else {
                None
            }
        })
    }

    #[test]
    fn next_message_id_wraps_at_u16_max() {
        let mut id: u16 = u16::MAX;
        id = id.wrapping_add(1);
        assert_eq!(id, 0);
    }
}
