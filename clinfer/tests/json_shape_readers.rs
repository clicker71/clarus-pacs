// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The single-dataset and search response shapes, through the READERS.
//!
//! PS3.18 F.2 fixes two top-level shapes. F.2.2 makes a single Data Set one
//! JSON object; F.2.1 makes multiple results a top-level array. In the field
//! the single-retrieve form diverges - Clarus answers the object, dcm4chee-arc
//! answers an array of one - so a reader has to accept both.
//!
//! These tests call `get_workitem`, `search_workitems` and
//! `search_workitems_for_station` against a fake origin that answers the shape
//! under test. They exist because the unit tests for `json::single_dataset`
//! prove the HELPER is right, not that any reader USES it: reverting the
//! unwrap inside `get_workitem` left those tests green, which is exactly the
//! failure mode a call-site test is for.

#[path = "support/fake_origin.rs"]
mod fake_origin;

use clinfer::dicomweb::Dicomweb;
use fake_origin::{Answer, FakeOrigin, Route};

/// One workitem as the two servers spell it: the same Data Set, wrapped or
/// not.
const WORKITEM_DATASET: &str = r#"{"00080018":{"vr":"UI","Value":["1.2.3"]},
    "00741000":{"vr":"CS","Value":["SCHEDULED"]},
    "00404021":{"vr":"SQ","Value":[{"0040E020":{"vr":"CS","Value":["DICOM"]},
                                    "0020000D":{"vr":"UI","Value":["9.9.9"]}}]},
    "00080060":{"vr":"CS","Value":["DX"]}}"#;

fn origin(fake: &FakeOrigin) -> Dicomweb {
    let client = clinfer::http::Client::new(&fake.base_url(), "clinfer").expect("client");
    clinfer::dicomweb::Dicomweb::new(client)
}

/// A fake that answers every `/workitems` path from one body.
///
/// The prefix is the full path the client asks for, `/dicomweb/workitems`:
/// the fake matches on `starts_with` against the request path, and the
/// base URL already carries the `/dicomweb` root.
fn origin_answering(body: &str) -> FakeOrigin {
    FakeOrigin::start_routed(vec![Route {
        starts_with: "/dicomweb/workitems".to_string(),
        answer: Answer::ok(body.to_string()),
    }])
}

// -- get_workitem: the single-dataset shapes (F.2.2 vs F.2.1) -------------

#[test]
fn get_workitem_reads_the_bare_object() {
    // The F.2.2 shape, which Clarus emits.
    let fake = origin_answering(WORKITEM_DATASET);
    let workitem = origin(&fake)
        .get_workitem("1.2.3")
        .expect("the bare object parses");
    assert_eq!(workitem.uid, "1.2.3");
    assert_eq!(workitem.state, "SCHEDULED");
    assert_eq!(workitem.study_instance_uid.as_deref(), Some("9.9.9"));
}

#[test]
fn get_workitem_reads_an_array_of_one() {
    // The dcm4chee-arc shape. Before the unwrap this returned Err("workitem
    // 1.2.3 carries no StudyInstanceUID") under an HTTP 200, because
    // `attribute("00080018")` on an array is None - which blocked the
    // dcm4chee leg of e2e-2.
    let body = format!("[{WORKITEM_DATASET}]");
    let fake = origin_answering(&body);
    let workitem = origin(&fake)
        .get_workitem("1.2.3")
        .expect("the array of one parses");
    assert_eq!(workitem.uid, "1.2.3");
    assert_eq!(workitem.state, "SCHEDULED");
    assert_eq!(
        workitem.study_instance_uid.as_deref(),
        Some("9.9.9"),
        "the study is the attribute the array form used to lose"
    );
}

#[test]
fn get_workitem_refuses_a_dataset_it_cannot_identify() {
    // Correctness must not be traded for tolerance: a non-empty object
    // without (0008,0018) is still refused, with the same message and the
    // same status.
    let fake = origin_answering(r#"{"00741000":{"vr":"CS","Value":["SCHEDULED"]}}"#);
    let error = origin(&fake)
        .get_workitem("1.2.3")
        .expect_err("no SOP Instance UID is not a workitem");
    assert!(
        error.to_string().contains("carries no StudyInstanceUID"),
        "the message the caller already handled: {error}"
    );
}

// -- search_workitems: a non-array is a protocol violation (option A) -----

#[test]
fn search_workitems_reads_an_array_and_an_empty_array() {
    // Both conformant answers keep working: an array of one, and the empty
    // array a site with no scheduled workitems returns.
    let one = r#"[{"00080018":{"vr":"UI","Value":["2.25.1"]}}]"#;
    let fake = origin_answering(one);
    assert_eq!(
        origin(&fake).search_workitems().expect("an array"),
        vec!["2.25.1".to_string()]
    );

    let empty = FakeOrigin::start_routed(vec![Route {
        starts_with: "/dicomweb/workitems".to_string(),
        answer: Answer::ok("[]"),
    }]);
    assert_eq!(
        origin(&empty).search_workitems().expect("an empty array"),
        Vec::<String>::new(),
        "an empty array is a real answer, not an error"
    );
}

#[test]
fn search_workitems_fails_loudly_on_a_non_array() {
    // Owner decision A: a search answers a top-level array (F.2.1), so an
    // object is a protocol violation. `unwrap_or(&[])` turned it into "no
    // workitems", which for the catch-up path means the study is silently
    // never processed.
    let fake = origin_answering(WORKITEM_DATASET);
    let error = origin(&fake)
        .search_workitems()
        .expect_err("an object is not a search answer");
    assert!(
        error.to_string().contains("non-array"),
        "the reason must name the shape: {error}"
    );
}

#[test]
fn search_workitems_for_station_reads_an_array_and_refuses_an_object() {
    let one = r#"[{"00080018":{"vr":"UI","Value":["2.25.7"]}}]"#;
    let fake = origin_answering(one);
    assert_eq!(
        origin(&fake)
            .search_workitems_for_station("CLINFER")
            .expect("an array"),
        vec!["2.25.7".to_string()]
    );

    let object = origin_answering(WORKITEM_DATASET);
    let error = origin(&object)
        .search_workitems_for_station("CLINFER")
        .expect_err("an object is refused here too");
    assert!(error.to_string().contains("non-array"), "{error}");
}

// -- inspect: a non-array stops the tree rather than emptying it ----------

#[test]
fn inspect_refuses_a_non_array_series_answer() {
    // The inspection renders a QIDO tree, so a non-array at the series level
    // used to render as "the study has no series" - a wrong answer with no
    // signal. It now stops with the reason, which is what the exit-non-zero
    // path turns into a message.
    let fake = FakeOrigin::start_routed(vec![
        // Most specific first: the harness takes the FIRST matching prefix,
        // and /dicomweb/studies also prefixes the series path.
        Route {
            starts_with: "/dicomweb/studies/1.2.3/series".to_string(),
            answer: Answer::ok(WORKITEM_DATASET),
        },
        Route {
            starts_with: "/dicomweb/studies".to_string(),
            answer: Answer::ok(r#"[{"0020000D":{"vr":"UI","Value":["1.2.3"]}}]"#),
        },
    ]);
    let error = clinfer::inspect::inspect(&origin(&fake), "1.2.3")
        .expect_err("an object is not a series list");
    assert!(error.to_string().contains("non-array"), "{error}");
}

#[test]
fn inspect_renders_an_empty_series_answer() {
    // The other side of the boundary: an EMPTY array is a real answer and
    // still renders, so the command keeps working on a study with no series.
    let fake = FakeOrigin::start_routed(vec![
        // Most specific first, for the same reason.
        Route {
            starts_with: "/dicomweb/studies/1.2.3/series".to_string(),
            answer: Answer::ok("[]"),
        },
        Route {
            starts_with: "/dicomweb/studies".to_string(),
            answer: Answer::ok(r#"[{"0020000D":{"vr":"UI","Value":["1.2.3"]}}]"#),
        },
    ]);
    let rendered =
        clinfer::inspect::inspect(&origin(&fake), "1.2.3").expect("an empty series list renders");
    assert!(rendered.contains("study 1.2.3"), "{rendered}");
    assert!(!rendered.contains("series "), "no series lines: {rendered}");
}
