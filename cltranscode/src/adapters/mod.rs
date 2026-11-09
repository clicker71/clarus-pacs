// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Adapter layer - input/output abstractions and implementations.
//!
//! Re-exports traits from the old `adapters.rs` (now moved here)
//! plus all concrete adapters.

#![deny(unsafe_code)]

pub mod dicomweb_input;
pub mod dicomweb_output;
pub mod dimse_scp;
pub mod dimse_scu;
pub mod pdu;

pub use dicomweb_input::DicomwebInput;
pub use dicomweb_output::DicomwebOutput;
pub use dimse_scp::DimseScp;
pub use dimse_scu::DimseScu;

// -- Traits (moved from old adapters.rs) ------------------------------

use std::fmt;

/// Errors that can occur during adapter operations (send/receive).
#[derive(Debug)]
pub enum AdapterError {
    /// Wrapped I/O error.
    Io(std::io::Error),
    /// Protocol-level error (e.g. malformed DICOM message).
    Protocol(String),
    /// Operation timed out.
    Timeout,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "adapter I/O error: {err}"),
            Self::Protocol(msg) => write!(f, "adapter protocol error: {msg}"),
            Self::Timeout => write!(f, "adapter timeout"),
        }
    }
}

impl std::error::Error for AdapterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AdapterError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Abstraction for receiving data from an input source.
pub trait InputAdapter {
    /// Receive the next chunk of raw bytes from the input source.
    fn recv(&mut self) -> Result<Vec<u8>, AdapterError>;
}

/// Abstraction for sending data to an output destination.
pub trait OutputAdapter: Send {
    /// Send raw bytes to the output destination.
    fn send(&mut self, bytes: &[u8]) -> Result<(), AdapterError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_error_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err = AdapterError::Io(io_err);
        assert!(err.to_string().contains("adapter I/O error: refused"));

        let err = AdapterError::Protocol("bad pdv".into());
        assert_eq!(err.to_string(), "adapter protocol error: bad pdv");

        let err = AdapterError::Timeout;
        assert_eq!(err.to_string(), "adapter timeout");
    }

    #[test]
    fn adapter_error_from_io() {
        let io_err = std::io::Error::other("oops");
        let adapter_err: AdapterError = io_err.into();
        assert!(matches!(adapter_err, AdapterError::Io(_)));
    }
}
