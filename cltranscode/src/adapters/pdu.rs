// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! DIMSE PDU parser - parse exactly what's needed for C-STORE + C-ECHO.
//!
//! nginx-way: parse only the PDU fields needed. No universal DIMSE stack.
//!
//! Reference: DICOM PS3.8 Section 9.3 (DIMSE Upper Layer Protocol).

#![deny(unsafe_code)]

// -- PDU type constants -----------------------------------------------

pub const PDU_AA_RQ: u8 = 0x01; // A-ASSOCIATE-RQ
pub const PDU_AA_AC: u8 = 0x02; // A-ASSOCIATE-AC
pub const PDU_AA_RJ: u8 = 0x03; // A-ASSOCIATE-RJ
pub const PDU_PDATA: u8 = 0x04; // P-DATA-TF
pub const PDU_AR_RQ: u8 = 0x05; // A-RELEASE-RQ
pub const PDU_AR_RP: u8 = 0x06; // A-RELEASE-RP
pub const PDU_ABORT: u8 = 0x07; // A-ABORT

// -- DIMSE command constants ------------------------------------------

pub const C_STORE_RQ: u16 = 0x0001;
pub const C_STORE_RSP: u16 = 0x8001;
pub const C_GET_RQ: u16 = 0x0010;
pub const C_GET_RSP: u16 = 0x8010;
pub const C_MOVE_RQ: u16 = 0x0021;
pub const C_MOVE_RSP: u16 = 0x8021;
pub const C_ECHO_RQ: u16 = 0x0030;
pub const C_ECHO_RSP: u16 = 0x8030;
pub const N_EVENT_REPORT_RQ: u16 = 0x0100;
pub const N_EVENT_REPORT_RSP: u16 = 0x8100;
pub const N_ACTION_RQ: u16 = 0x0130;
pub const N_ACTION_RSP: u16 = 0x8130;

/// DIMSE status codes.
pub const STATUS_SUCCESS: u16 = 0x0000;
pub const STATUS_SOP_CLASS_NOT_SUPPORTED: u16 = 0x0122;
pub const STATUS_CANNOT_UNDERSTAND: u16 = 0xC000;
pub const STATUS_OUT_OF_RESOURCES: u16 = 0xA700;

// -- minimal PDU ------------------------------------------------------

/// A minimal parsed PDU header.
#[derive(Debug)]
pub struct PduHeader {
    pub pdu_type: u8,
    pub length: u32,
}

impl PduHeader {
    /// Parse a PDU header from the first 6 bytes of a buffer.
    /// Returns None if the buffer is too short.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 6 {
            return None;
        }
        let pdu_type = buf[0];
        // Reserved byte at buf[1], skip
        let length = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]);
        Some(Self { pdu_type, length })
    }

    /// Total PDU size = 6 (header) + length.
    pub fn total_size(&self) -> usize {
        6 + self.length as usize
    }
}

/// Build a PDU header as 6 bytes.
pub fn build_pdu(pdu_type: u8, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(6 + data.len());
    buf.push(pdu_type);
    buf.push(0x00); // reserved
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    buf
}

// -- A-ASSOCIATE builder/parser --------------------------------------

/// Presentation Context item for A-ASSOCIATE negotiation.
#[derive(Debug, Clone)]
pub struct PresentationContext {
    pub context_id: u8,
    pub result_reason: u8, // 0x00 = accepted, 0x03 = abstract syntax not supported
    pub abstract_syntax: String,
    pub transfer_syntaxes: Vec<String>,
}

/// Build an A-ASSOCIATE-RQ PDU.
///
/// `called_ae` - the AE title we're connecting to.
/// `calling_ae` - our AE title.
/// `contexts` - list of presentation contexts we propose.
pub fn build_associate_rq(
    called_ae: &str,
    calling_ae: &str,
    contexts: &[PresentationContext],
) -> Vec<u8> {
    let mut data = Vec::new();

    // Protocol version (2 bytes): 0x0001
    data.extend_from_slice(&[0x00, 0x01]);
    // Reserved (2 bytes)
    data.extend_from_slice(&[0x00, 0x00]);

    // Called AE title (16 bytes, padded with spaces)
    data.extend_from_slice(&pad_ae(called_ae));
    // Calling AE title (16 bytes, padded with spaces)
    data.extend_from_slice(&pad_ae(calling_ae));
    // Reserved (32 bytes)
    data.extend_from_slice(&[0x00u8; 32]);

    // Variable items - Application Context
    // Item type 0x10, reserved 0x00, length
    let app_ctx = b"1.2.840.10008.3.1.1.1"; // DICOM Application Context
    data.push(0x10);
    data.push(0x00);
    data.extend_from_slice(&(app_ctx.len() as u16).to_be_bytes());
    data.extend_from_slice(app_ctx);

    // Presentation Context items
    for ctx in contexts {
        data.push(0x20); // item type
        data.push(0x00); // reserved
        // Length placeholder
        let len_pos = data.len();
        data.extend_from_slice(&0u16.to_be_bytes());

        data.push(ctx.context_id);
        data.push(0x00); // reserved (3 bytes)
        data.extend_from_slice(&[0x00, 0x00]);

        // Abstract syntax sub-item (0x30)
        let abs = ctx.abstract_syntax.as_bytes();
        let abs_len = abs.len() as u16;
        data.push(0x30);
        data.push(0x00);
        data.extend_from_slice(&abs_len.to_be_bytes());
        data.extend_from_slice(abs);

        // Transfer syntax sub-items (0x40)
        for ts in &ctx.transfer_syntaxes {
            let ts_bytes = ts.as_bytes();
            let ts_len = ts_bytes.len() as u16;
            data.push(0x40);
            data.push(0x00);
            data.extend_from_slice(&ts_len.to_be_bytes());
            data.extend_from_slice(ts_bytes);
        }

        // Patch length
        let item_len = (data.len() - len_pos - 2) as u16;
        data[len_pos..len_pos + 2].copy_from_slice(&item_len.to_be_bytes());
    }

    // User Information item (0x50)
    // Max PDU length sub-item (0x51)
    let max_pdu: u32 = 65536;
    data.push(0x50);
    data.push(0x00);
    let ui_len_pos = data.len();
    data.extend_from_slice(&0u16.to_be_bytes());

    data.push(0x51);
    data.push(0x00);
    data.extend_from_slice(&4u16.to_be_bytes());
    data.extend_from_slice(&max_pdu.to_be_bytes());

    let ui_len = (data.len() - ui_len_pos - 2) as u16;
    data[ui_len_pos..ui_len_pos + 2].copy_from_slice(&ui_len.to_be_bytes());

    build_pdu(PDU_AA_RQ, &data)
}

/// Build an A-ASSOCIATE-AC PDU.
pub fn build_associate_ac(
    called_ae: &str,
    calling_ae: &str,
    contexts: &[PresentationContext],
) -> Vec<u8> {
    let mut data = Vec::new();

    // Protocol version
    data.extend_from_slice(&[0x00, 0x01]);
    data.extend_from_slice(&[0x00, 0x00]); // reserved
    data.extend_from_slice(&pad_ae(called_ae));
    data.extend_from_slice(&pad_ae(calling_ae));
    data.extend_from_slice(&[0x00u8; 32]);

    // Application Context item
    let app_ctx = b"1.2.840.10008.3.1.1.1";
    data.push(0x10);
    data.push(0x00);
    data.extend_from_slice(&(app_ctx.len() as u16).to_be_bytes());
    data.extend_from_slice(app_ctx);

    // Presentation Context items (accepted/refused)
    for ctx in contexts {
        data.push(0x21); // item type for AC
        data.push(0x00);
        let len_pos = data.len();
        data.extend_from_slice(&0u16.to_be_bytes());

        data.push(ctx.context_id);
        data.push(0x00);
        data.extend_from_slice(&[ctx.result_reason, 0x00]);

        // Transfer syntax sub-item (0x40) - echo the first TS the SCU proposed
        if let Some(ts) = ctx.transfer_syntaxes.first() {
            log::debug!("dimse_scp: AC ctx {} accepted TS: {}", ctx.context_id, ts);
            data.push(0x40);
            data.push(0x00);
            data.extend_from_slice(&(ts.len() as u16).to_be_bytes());
            data.extend_from_slice(ts.as_bytes());
        }

        let item_len = (data.len() - len_pos - 2) as u16;
        data[len_pos..len_pos + 2].copy_from_slice(&item_len.to_be_bytes());
    }

    // User Information
    let max_pdu: u32 = 65536;
    data.push(0x50);
    data.push(0x00);
    let ui_len_pos = data.len();
    data.extend_from_slice(&0u16.to_be_bytes());
    data.push(0x51);
    data.push(0x00);
    data.extend_from_slice(&4u16.to_be_bytes());
    data.extend_from_slice(&max_pdu.to_be_bytes());
    let ui_len = (data.len() - ui_len_pos - 2) as u16;
    data[ui_len_pos..ui_len_pos + 2].copy_from_slice(&ui_len.to_be_bytes());

    build_pdu(PDU_AA_AC, &data)
}

/// Build an A-ASSOCIATE-RJ PDU.
pub fn build_associate_rj(reason: u8) -> Vec<u8> {
    // Result (1), Source (1), Reason/Diag (1)
    let data = vec![0x01, 0x01, reason, 0x00]; // result=rejected-permanent, source=service-user
    build_pdu(PDU_AA_RJ, &data)
}

/// Build an A-RELEASE-RP PDU.
pub fn build_release_rp() -> Vec<u8> {
    build_pdu(PDU_AR_RP, &[0x00, 0x00, 0x00, 0x00])
}

/// Build an A-ABORT PDU.
pub fn build_abort() -> Vec<u8> {
    let data = vec![0x00, 0x00]; // source=service-user, reason=not-specified
    build_pdu(PDU_ABORT, &data)
}

// -- P-DATA-TF --------------------------------------------------------

/// Build a P-DATA-TF PDU containing a single PDV with the given presentation context ID
/// and data. `is_command` = true for DIMSE command, false for dataset.
pub fn build_pdata(context_id: u8, data: &[u8], is_command: bool) -> Vec<u8> {
    let mut pdv = Vec::new();
    // PDV length = ctx_id(1) + MCH(1) + data(N) = N + 2
    let pdv_len = (data.len() + 2) as u32;
    pdv.extend_from_slice(&pdv_len.to_be_bytes());
    // Presentation Context ID byte
    pdv.push(context_id);
    // Message Control Header byte: bit 0 = last fragment, bit 1 = command
    let mch: u8 = 0b0000_0010 | if is_command { 0b0000_0001 } else { 0 };
    pdv.push(mch);
    // Presentation Data Value
    pdv.extend_from_slice(data);
    build_pdu(PDU_PDATA, &pdv)
}

// -- Helpers ----------------------------------------------------------

fn pad_ae(ae: &str) -> [u8; 16] {
    let mut buf = [b' '; 16];
    let bytes = ae.as_bytes();
    let len = bytes.len().min(16);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

/// Read exactly `n` bytes from a TCP stream.
pub fn read_exact(stream: &mut dyn std::io::Read, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read a full PDU from a stream: header (6 bytes) + body.
pub fn read_pdu(stream: &mut dyn std::io::Read) -> std::io::Result<(PduHeader, Vec<u8>)> {
    let header_bytes = read_exact(stream, 6)?;
    let header = PduHeader::parse(&header_bytes)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "short PDU header"))?;
    let body = read_exact(stream, header.length as usize)?;
    Ok((header, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdu_header_parse() {
        let h = PduHeader::parse(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x10]).unwrap();
        assert_eq!(h.pdu_type, PDU_AA_RQ);
        assert_eq!(h.length, 16);
        assert_eq!(h.total_size(), 22);
    }

    #[test]
    fn build_release_rp_correct_type() {
        let rp = build_release_rp();
        assert_eq!(rp[0], PDU_AR_RP);
    }

    #[test]
    fn build_associate_rq_contains_aes() {
        let ctx = PresentationContext {
            context_id: 1,
            result_reason: 0,
            abstract_syntax: "1.2.840.10008.5.1.4.1.1.2".into(),
            transfer_syntaxes: vec!["1.2.840.10008.1.2.1".into()],
        };
        let rq = build_associate_rq("STORESCP", "CLTRANSCODE", &[ctx]);
        assert_eq!(rq[0], PDU_AA_RQ);
        let body = String::from_utf8_lossy(&rq[6..]);
        assert!(body.contains("STORESCP"));
        assert!(body.contains("CLTRANSCODE"));
    }

    #[test]
    fn pad_ae_works() {
        let padded = pad_ae("MYAE");
        assert_eq!(&padded[..4], b"MYAE");
        assert_eq!(&padded[4..], b"            "); // spaces
    }
}
