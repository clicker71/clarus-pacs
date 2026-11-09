// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Capabilities endpoint handler.
//!
//! Implements PS3.18 Section 8.9 (Retrieve Capabilities Transaction). The
//! response is a Capabilities Description, which Annex H defines as a WADL
//! document: one top-level `application` element carrying one `resources`
//! element (whose `base` attribute is the service base URL) and a `resource`
//! element for every Target Resource the origin server implements, describing
//! its methods and child resources.
//!
//! The document is static: the resource tree is fixed by the route registry,
//! so it is emitted from a const skeleton with the base URL spliced in. Only
//! the subtree Clarus implements is described (Annex H: the resource(s) for
//! the Target Resource and each of its children), not the whole standard.
//!
//! # Invariants
//! - INV-001: `unsafe` forbidden. `unwrap()` forbidden in handlers.
//! - INV-002: All described resources MUST reflect the actual route registry.

#![forbid(unsafe_code)]

/// The canonical WADL media type (PS3.18 8.9.3.1.1 Content-Type; there is no
/// DICOM-specific WADL type in PS3.18 8.7).
pub const WADL_CONTENT_TYPE: &str = "application/vnd.sun.wadl+xml";

// -- Pure logic - no axum, no AppState -----------------------------------

/// Build the DICOMweb Capabilities Description as a WADL document
/// (PS3.18 8.9 + Annex H).
///
/// `base` is `{scheme}://{host}` (the same value the QIDO RetrieveURL is
/// built from); it is spliced into the `resources` `base` attribute, followed
/// by `/dicomweb`. When `base` is `None` (no usable Host) the attribute is
/// just `/dicomweb`, which is still a valid relative base.
///
/// # Returns
/// `(status, content_type, body)` - always `200` with the WADL media type.
pub fn build_capabilities(base: Option<&str>) -> (u16, &'static str, Vec<u8>) {
    const HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<application xmlns="http://wadl.dev.java.net/2009/02">
  <resources base=""#;
    const BODY: &str = r#"/dicomweb">
    <resource path="studies">
      <method name="GET" id="Search for Studies"/>
      <method name="POST" id="Store Instances"/>
      <resource path="{StudyInstance}">
        <method name="GET" id="Retrieve Study"/>
        <method name="POST" id="Store Study Instances"/>
        <resource path="metadata"><method name="GET" id="Retrieve Study Metadata"/></resource>
        <resource path="thumbnail"><method name="GET" id="Retrieve Study Thumbnail"/></resource>
        <resource path="bulkdata"><method name="GET" id="Retrieve Study Bulk Data"/></resource>
        <resource path="series">
          <method name="GET" id="Search for Series"/>
          <resource path="{SeriesInstance}">
            <method name="GET" id="Retrieve Series"/>
            <resource path="metadata"><method name="GET" id="Retrieve Series Metadata"/></resource>
            <resource path="thumbnail"><method name="GET" id="Retrieve Series Thumbnail"/></resource>
            <resource path="bulkdata"><method name="GET" id="Retrieve Series Bulk Data"/></resource>
            <resource path="instances">
              <method name="GET" id="Search for Instances"/>
              <resource path="{SOPInstance}">
                <method name="GET" id="Retrieve Instance"/>
                <resource path="metadata"><method name="GET" id="Retrieve Instance Metadata"/></resource>
                <resource path="rendered"><method name="GET" id="Retrieve Rendered Instance"/></resource>
                <resource path="thumbnail"><method name="GET" id="Retrieve Instance Thumbnail"/></resource>
                <resource path="bulkdata"><method name="GET" id="Retrieve Instance Bulk Data"/></resource>
                <resource path="frames">
                  <method name="GET" id="Retrieve Frames"/>
                  <resource path="{frame}"><method name="GET" id="Retrieve Frame"/></resource>
                </resource>
              </resource>
            </resource>
          </resource>
        </resource>
      </resource>
    </resource>
    <resource path="workitems">
      <method name="GET" id="Search for Workitems"/>
      <method name="POST" id="Create Workitem"/>
      <resource path="{workitem}">
        <method name="GET" id="Retrieve Workitem"/>
        <method name="POST" id="Update Workitem"/>
        <resource path="state"><method name="PUT" id="Change Workitem State"/></resource>
        <resource path="cancelrequest"><method name="POST" id="Request Cancellation"/></resource>
        <resource path="subscribers">
          <method name="POST" id="Create Subscription"/>
          <resource path="{subscriber}"><method name="DELETE" id="Delete Subscription"/></resource>
        </resource>
      </resource>
      <resource path="{subscriptionSOPClassUID}">
        <resource path="subscribers">
          <method name="POST" id="Create Subscription"/>
          <resource path="{subscriber}"><method name="DELETE" id="Delete Subscription"/></resource>
        </resource>
      </resource>
    </resource>
    <resource path="commitment-requests">
      <resource path="{TransactionUID}">
        <method name="POST" id="Commit"/>
        <method name="GET" id="Check Commit Result"/>
      </resource>
    </resource>
    <resource path="modality-scheduled-procedure-steps">
      <method name="GET" id="Search for Scheduled Procedure Steps"/>
    </resource>
    <resource path="modality-performed-procedure-steps">
      <resource path="{mppsUID}">
        <method name="POST" id="Create Performed Procedure Step"/>
        <method name="GET" id="Retrieve Performed Procedure Step"/>
      </resource>
    </resource>
  </resources>
</application>
"#;

    let mut body = Vec::with_capacity(HEAD.len() + BODY.len() + 64);
    body.extend_from_slice(HEAD.as_bytes());
    // The base comes from the request's Host header, which the client
    // controls. Splitting it raw into an XML attribute lets a header carrying
    // `"` or `<` close the attribute and inject elements into the document;
    // escaping is what keeps the WADL well-formed whatever the header says.
    if let Some(base) = base {
        push_xml_escaped(&mut body, base);
    }
    body.extend_from_slice(BODY.as_bytes());
    (200, WADL_CONTENT_TYPE, body)
}

/// Append `s` with the five XML predefined entities escaped.
///
/// A WADL `base` is an ATTRIBUTE value, so the quote characters matter as much
/// as the angle brackets: an unescaped `"` ends the value early.
fn push_xml_escaped(out: &mut Vec<u8>, s: &str) {
    for b in s.bytes() {
        match b {
            b'&' => out.extend_from_slice(b"&amp;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'"' => out.extend_from_slice(b"&quot;"),
            b'\'' => out.extend_from_slice(b"&apos;"),
            _ => out.push(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Capabilities Description is a WADL document (Annex H): it opens
    /// with the XML declaration and an `<application>` element carrying a
    /// `<resources base>` and a `<resource>` for a real Target Resource.
    #[test]
    fn capabilities_is_wadl() {
        let (status, ct, body) = build_capabilities(Some("http://host:8024"));
        assert_eq!(status, 200);
        assert_eq!(ct, WADL_CONTENT_TYPE);
        let s = String::from_utf8(body).expect("wadl is utf-8");
        assert!(s.contains(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
        assert!(s.contains("<application"));
        assert!(s.contains(r#"<resources base="http://host:8024/dicomweb">"#));
        assert!(s.contains("<resource"));
        assert!(s.contains("modality-scheduled-procedure-steps"));
    }

    /// With no base the `base` attribute is the relative `/dicomweb`, so the
    /// document is still well-formed WADL.
    #[test]
    fn capabilities_without_base_is_relative() {
        let (_, _, body) = build_capabilities(None);
        let s = String::from_utf8(body).expect("wadl is utf-8");
        assert!(s.contains(r#"<resources base="/dicomweb">"#));
    }

    /// INV-002: every advertised resource must reflect the route registry.
    /// `/rendered` is implemented at the INSTANCE level only (study/series
    /// rendering would need a montage encoder), and `/pixeldata` is not
    /// implemented at all (`/frames` is the pixel access).
    ///
    /// A workitem DOES have a `/subscribers` child: PS3.18 11.10.1.1
    /// Table 11.10.1-1 lists the Workitem Subscription template and requires
    /// the origin server to support it, so both subscription shapes are
    /// advertised - the SOP-Class-scoped one and the workitem-scoped one.
    #[test]
    fn capabilities_omits_unimplemented_resources() {
        let (_, _, body) = build_capabilities(Some("http://host:8024"));
        let s = String::from_utf8(body).expect("wadl is utf-8");
        assert!(
            !s.contains(r#"id="Retrieve Rendered Study""#),
            "study-level rendered is not implemented and must not be advertised"
        );
        assert!(
            !s.contains(r#"id="Retrieve Rendered Series""#),
            "series-level rendered is not implemented and must not be advertised"
        );
        assert!(
            s.contains(r#"id="Retrieve Rendered Instance""#),
            "instance-level rendered IS implemented and must be advertised"
        );
        assert!(
            !s.contains(r#"path="pixeldata""#),
            "pixeldata is not implemented and must not be advertised"
        );
        assert!(
            s.contains(r#"<resource path="{subscriptionSOPClassUID}">"#),
            "the Worklist/Filtered Worklist subscription resource must be advertised"
        );
        // PS3.18 11.10.1.1 Table 11.10.1-1 requires the Workitem Subscription
        // template too. It nests under `{workitem}`, so assert on the exact
        // nesting rather than on a bare `subscribers` substring - the
        // SOP-Class form would satisfy that.
        assert!(
            s.contains(r#"<resource path="subscribers">"#),
            "a subscribers child must be advertised"
        );
        let workitem_block = s
            .split(r#"<resource path="{workitem}">"#)
            .nth(1)
            .expect("workitem resource block");
        let workitem_block = workitem_block
            .split(r#"<resource path="{subscriptionSOPClassUID}">"#)
            .next()
            .expect("workitem block ends before the SOP Class one");
        assert!(
            workitem_block.contains(r#"<resource path="subscribers">"#),
            "the Workitem Subscription template must nest under the workitem: {workitem_block}"
        );
    }
}
