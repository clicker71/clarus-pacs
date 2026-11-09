// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Clarus ports layer - trait definitions (abstractions).
//!
//! This crate defines the interfaces that the adapters implement
//! and the server consumes.

#![forbid(unsafe_code)]

pub mod dicom;
pub mod models;
pub mod rejection;
pub mod stow_response;

// -- StorageTier enum ----------------------------------------------------

/// Where a DICOM file physically lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageTier {
    Hot,
    Cold,
    S3,
}

impl std::str::FromStr for StorageTier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hot" => Ok(Self::Hot),
            "cold" => Ok(Self::Cold),
            "s3" => Ok(Self::S3),
            other => Err(format!("Unknown storage_tier: {other}")),
        }
    }
}

impl std::fmt::Display for StorageTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hot => write!(f, "hot"),
            Self::Cold => write!(f, "cold"),
            Self::S3 => write!(f, "s3"),
        }
    }
}
