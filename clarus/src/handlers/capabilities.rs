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

use crate::handlers::workflow_response::HTTP_OK;

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
      <method name="GET"><doc title="Search for Studies"/></method>
      <method name="POST"><doc title="Store Instances"/></method>
      <resource path="{StudyInstance}">
        <method name="GET"><doc title="Retrieve Study"/></method>
        <method name="POST"><doc title="Store Study Instances"/></method>
        <resource path="metadata"><method name="GET"><doc title="Retrieve Study Metadata"/></method></resource>
        <resource path="thumbnail"><method name="GET"><doc title="Retrieve Study Thumbnail"/></method></resource>
        <resource path="bulkdata"><method name="GET"><doc title="Retrieve Study Bulk Data"/></method></resource>
        <resource path="series">
          <method name="GET"><doc title="Search for Series"/></method>
          <resource path="{SeriesInstance}">
            <method name="GET"><doc title="Retrieve Series"/></method>
            <resource path="metadata"><method name="GET"><doc title="Retrieve Series Metadata"/></method></resource>
            <resource path="thumbnail"><method name="GET"><doc title="Retrieve Series Thumbnail"/></method></resource>
            <resource path="bulkdata"><method name="GET"><doc title="Retrieve Series Bulk Data"/></method></resource>
            <resource path="instances">
              <method name="GET"><doc title="Search for Instances"/></method>
              <resource path="{SOPInstance}">
                <method name="GET"><doc title="Retrieve Instance"/></method>
                <resource path="metadata"><method name="GET"><doc title="Retrieve Instance Metadata"/></method></resource>
                <resource path="rendered"><method name="GET"><doc title="Retrieve Rendered Instance"/></method></resource>
                <resource path="thumbnail"><method name="GET"><doc title="Retrieve Instance Thumbnail"/></method></resource>
                <resource path="bulkdata"><method name="GET"><doc title="Retrieve Instance Bulk Data"/></method></resource>
                <resource path="frames">
                  <method name="GET"><doc title="Retrieve Frames"/></method>
                  <resource path="{frame}">
                    <method name="GET"><doc title="Retrieve Frame"/></method>
                    <resource path="rendered"><method name="GET"><doc title="Retrieve Rendered Frame"/></method></resource>
                  </resource>
                </resource>
              </resource>
            </resource>
          </resource>
        </resource>
      </resource>
    </resource>
    <resource path="workitems">
      <method name="GET"><doc title="Search for Workitems"/></method>
      <method name="POST"><doc title="Create Workitem"/></method>
      <resource path="{workitem}">
        <method name="GET"><doc title="Retrieve Workitem"/></method>
        <method name="POST"><doc title="Update Workitem"/></method>
        <resource path="state"><method name="PUT"><doc title="Change Workitem State"/></method></resource>
        <resource path="cancelrequest"><method name="POST"><doc title="Request Cancellation"/></method></resource>
        <resource path="subscribers">
          <method name="POST"><doc title="Create Subscription"/></method>
          <resource path="{subscriber}"><method name="DELETE"><doc title="Delete Subscription"/></method></resource>
        </resource>
      </resource>
      <resource path="{subscriptionSOPClassUID}">
        <resource path="subscribers">
          <method name="POST"><doc title="Create Subscription"/></method>
          <resource path="{subscriber}"><method name="DELETE"><doc title="Delete Subscription"/></method></resource>
        </resource>
      </resource>
    </resource>
    <resource path="commitment-requests">
      <resource path="{TransactionUID}">
        <method name="POST"><doc title="Commit"/></method>
        <method name="GET"><doc title="Check Commit Result"/></method>
      </resource>
    </resource>
    <resource path="modality-scheduled-procedure-steps">
      <method name="GET"><doc title="Search for Scheduled Procedure Steps"/></method>
    </resource>
    <resource path="modality-performed-procedure-steps">
      <resource path="{mppsUID}">
        <method name="POST"><doc title="Create Performed Procedure Step"/></method>
        <method name="GET"><doc title="Retrieve Performed Procedure Step"/></method>
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
    (HTTP_OK, WADL_CONTENT_TYPE, body)
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

    /// WADL (Annex H) does not allow `id` on a locally embedded `method`
    /// element. Every method carries its label in a `doc` child instead, and
    /// the HTTP method stays in `name`, so the document validates.
    #[test]
    fn no_method_carries_an_id_attribute() {
        let (_, _, body) = build_capabilities(Some("http://host:8024"));
        let s = String::from_utf8(body).expect("wadl is utf-8");
        assert!(
            !s.contains("<method name=\"GET\" id=") && !s.contains("<method name=\"POST\" id="),
            "a local method element must not carry an id: {s}"
        );
        assert!(
            s.contains(r#"<method name="GET"><doc title="Search for Studies"/></method>"#),
            "the label moved into a doc child: {s}"
        );
        // Every method element names its HTTP method and has a closing tag.
        assert_eq!(
            s.matches("<method ").count(),
            s.matches("</method>").count(),
            "every method element is closed: {s}"
        );
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
    /// `/rendered` is implemented at the INSTANCE level AND at the FRAME level
    /// (one frame of a multi-frame instance), but not at the study/series
    /// level (those would need a montage encoder), and `/pixeldata` is not
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
            !s.contains(r#"<doc title="Retrieve Rendered Study"/>"#),
            "study-level rendered is not implemented and must not be advertised"
        );
        assert!(
            !s.contains(r#"<doc title="Retrieve Rendered Series"/>"#),
            "series-level rendered is not implemented and must not be advertised"
        );
        assert!(
            s.contains(r#"<doc title="Retrieve Rendered Instance"/>"#),
            "instance-level rendered IS implemented and must be advertised"
        );
        assert!(
            s.contains(r#"<doc title="Retrieve Rendered Frame"/>"#),
            "frame-level rendered IS implemented and must be advertised"
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
