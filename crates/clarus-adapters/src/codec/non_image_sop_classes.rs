// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! SOP Class UIDs that carry no Pixel Data - skip transcoding.
//!
//! These classes have no pixel data (SR, RT structures, Presentation State,
//! Encapsulated documents, Surface Segmentation). Transcoding them is pointless
//! and causes errors (pydicom/image crate can't find PixelData tag).
//!
//! Segmentation Storage (66.4) is NOT one of them: PS3.3 C.8.20.2 makes
//! Samples per Pixel, Photometric Interpretation, Pixel Representation,
//! Bits Allocated and Bits Stored Type 1, and the IOD carries Pixel Data, so a
//! SEG instance is as much an image as a CT instance
//! (`standard/ps3.3/segmentation-image-module-C.8.20.2.json`). Its
//! REPRESENTATION can still be untranscodable - a BINARY segmentation stores
//! one bit per pixel and no compressed syntax carries that - but that is a
//! question about the representation and the target, not about the class.
//!
//! See PS3.6 Table A.1-1, PS3.4 Annex B.5.
//!
//! # Maintenance
//! To add a new non-image SOP class: copy the UID from `sop_classes.yaml`.
//! One per line. Keep sorted alphabetically within each group.

/// Returns `true` if the SOP Class UID represents an image SOP class
/// that CAN be transcoded (has pixel data).
///
/// Returns `false` for SR, RT structures, Presentation State,
/// Encapsulated documents, Segmentation, and other non-image SOPs.
pub fn is_image_sop_class(uid: &str) -> bool {
    !matches!(
        uid,
        // -- Structured Reports --------------------------
        "1.2.840.10008.5.1.4.1.1.88.11"  // Basic Text SR
        | "1.2.840.10008.5.1.4.1.1.88.22"  // Enhanced SR
        | "1.2.840.10008.5.1.4.1.1.88.33"  // Comprehensive SR
        | "1.2.840.10008.5.1.4.1.1.88.34"  // Comprehensive 3D SR
        | "1.2.840.10008.5.1.4.1.1.88.40"  // Procedure Log
        | "1.2.840.10008.5.1.4.1.1.88.50"  // Mammography CAD SR
        | "1.2.840.10008.5.1.4.1.1.88.59"  // Key Object Selection
        | "1.2.840.10008.5.1.4.1.1.88.65"  // Chest CAD SR
        | "1.2.840.10008.5.1.4.1.1.88.67"  // X-Ray Radiation Dose SR
        | "1.2.840.10008.5.1.4.1.1.88.68"  // Radiopharm. Radiation Dose SR
        | "1.2.840.10008.5.1.4.1.1.88.69"  // Colon CAD SR
        | "1.2.840.10008.5.1.4.1.1.88.70"  // Implantation Plan SR
        // -- RT (non-image) -----------------------------
        | "1.2.840.10008.5.1.4.1.1.481.3"  // RT Structure Set
        | "1.2.840.10008.5.1.4.1.1.481.4"  // RT Beams Treatment Record
        | "1.2.840.10008.5.1.4.1.1.481.5"  // RT Plan
        | "1.2.840.10008.5.1.4.1.1.481.6"  // RT Brachy Treatment Record
        | "1.2.840.10008.5.1.4.1.1.481.7"  // RT Treatment Summary Record
        | "1.2.840.10008.5.1.4.1.1.481.8"  // RT Ion Plan
        | "1.2.840.10008.5.1.4.1.1.481.9"  // RT Ion Beams Treatment Record
        // -- Presentation State -------------------------
        | "1.2.840.10008.5.1.4.1.1.11.1"   // Grayscale Softcopy Presentation State
        | "1.2.840.10008.5.1.4.1.1.11.2"   // Color Softcopy Presentation State
        | "1.2.840.10008.5.1.4.1.1.11.3"   // Pseudo-Color Softcopy Presentation State
        | "1.2.840.10008.5.1.4.1.1.11.4"   // Blending Softcopy Presentation State
        | "1.2.840.10008.5.1.4.1.1.11.5"   // XA/XRF Grayscale Softcopy Presentation State
        // -- Encapsulated ------------------------------
        | "1.2.840.10008.5.1.4.1.1.104.1"  // Encapsulated PDF
        | "1.2.840.10008.5.1.4.1.1.104.2"  // Encapsulated CDA
        | "1.2.840.10008.5.1.4.1.1.104.3"  // Encapsulated STL
        | "1.2.840.10008.5.1.4.1.1.104.4"  // Encapsulated OBJ
        // PS3.3 A.85: the 3D Manufacturing IODs encapsulate a document in
        // Encapsulated Document (0042,0011) and have no Image Pixel Module, so
        // Encapsulated MTL (104.5) belongs here beside STL and OBJ. It carries
        // a material library, not pixels.
        | "1.2.840.10008.5.1.4.1.1.104.5"  // Encapsulated MTL
        // -- Spatial Registration & Surface Segmentation -------
        // Surface Segmentation (66.5) is a polygon mesh with no Segmentation
        // Image Module and no Pixel Data (PS3.3 A.57,
        // `standard/ps3.3/surface-segmentation-a57.json`).
        // Segmentation Storage (66.4) is NOT listed: it carries Pixel Data
        // like any other image (PS3.3 C.8.20.2).
        | "1.2.840.10008.5.1.4.1.1.66.1"   // Spatial Registration
        | "1.2.840.10008.5.1.4.1.1.66.5" // Surface Segmentation
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_sop_classes() {
        // Common image SOP classes
        assert!(is_image_sop_class("1.2.840.10008.5.1.4.1.1.2")); // CT Image Storage
        assert!(is_image_sop_class("1.2.840.10008.5.1.4.1.1.1.1")); // DX (CR)
        assert!(is_image_sop_class("1.2.840.10008.5.1.4.1.1.4")); // MR Image Storage
        // RT Image and RT Dose ARE images (have PixelData)
        assert!(is_image_sop_class("1.2.840.10008.5.1.4.1.1.481.1")); // RT Image
        assert!(is_image_sop_class("1.2.840.10008.5.1.4.1.1.481.2")); // RT Dose
    }

    #[test]
    fn test_non_image_sop_classes() {
        assert!(!is_image_sop_class("1.2.840.10008.5.1.4.1.1.88.11")); // Basic Text SR
        assert!(!is_image_sop_class("1.2.840.10008.5.1.4.1.1.11.1")); // GSPS
        assert!(!is_image_sop_class("1.2.840.10008.5.1.4.1.1.481.3")); // RT Structure Set
        assert!(!is_image_sop_class("1.2.840.10008.5.1.4.1.1.104.1")); // Encapsulated PDF
        assert!(!is_image_sop_class("1.2.840.10008.5.1.4.1.1.66.5")); // Surface Segmentation
        assert!(!is_image_sop_class("1.2.840.10008.5.1.4.1.1.66.1")); // Spatial Registration
    }

    /// PS3.3 A.85 makes the 3D Manufacturing IODs an Encapsulated Document
    /// without an Image Pixel Module, so the whole 104.x family is non-image -
    /// including MTL, which carries a material library and no pixels.
    #[test]
    fn every_encapsulated_document_class_is_non_image() {
        for uid in [
            "1.2.840.10008.5.1.4.1.1.104.1", // PDF
            "1.2.840.10008.5.1.4.1.1.104.2", // CDA
            "1.2.840.10008.5.1.4.1.1.104.3", // STL
            "1.2.840.10008.5.1.4.1.1.104.4", // OBJ
            "1.2.840.10008.5.1.4.1.1.104.5", // MTL
        ] {
            assert!(!is_image_sop_class(uid), "{uid} must be non-image");
        }
        // Segmentation Storage stays an image: it carries Pixel Data like any
        // other image (PS3.3 C.8.20.2), and classifying it as non-image would
        // make its pixels unreachable.
        assert!(
            is_image_sop_class("1.2.840.10008.5.1.4.1.1.66.4"),
            "Segmentation"
        );
    }

    #[test]
    fn segmentation_storage_is_an_image_class() {
        // PS3.3 C.8.20.2: the Segmentation Image Module makes the pixel
        // attributes Type 1 and the IOD carries Pixel Data, so a SEG instance
        // must reach the transcoding path. Classifying it as pixel-free made
        // every segmentation skip transcoding and stay in its stored syntax.
        assert!(is_image_sop_class("1.2.840.10008.5.1.4.1.1.66.4"));
        // Its neighbour differs exactly on this question.
        assert!(!is_image_sop_class("1.2.840.10008.5.1.4.1.1.66.5"));
    }

    #[test]
    fn test_unknown_uid_is_image() {
        // Unknown/novel UIDs default to "image" (safe: transcode attempt fails gracefully)
        assert!(is_image_sop_class("1.2.840.10008.5.1.4.1.1.99999"));
        assert!(is_image_sop_class(""));
    }
}
