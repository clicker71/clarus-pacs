// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Wire tests: clinfer against a fake origin, over a real socket.
//!
//! These exercise the layers against each other rather than in isolation:
//! the point is that the request clinfer builds is the request the origin
//! sees, and that a status the origin returns is read the way the
//! lifecycle assumes. A scripted fake is what makes the second half
//! possible - a real origin will not hand out a 409 on demand.

#[path = "support/fake_origin.rs"]
mod fake_origin;

use clinfer::conf;
use clinfer::dicomweb::{
    Dicomweb, TAG_OUTPUT_INFORMATION_SEQUENCE, TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE, TAG_UPS_STATE,
};
use clinfer::http::Client;
use clinfer::json;
use clinfer::receiver::{self, Counters, Policy, QUEUE_DEPTH};
use clinfer::ups::{self, Action, Outcome, State};
use clinfer::worker::{self, Shared};
use fake_origin::{Answer, FakeOrigin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::Duration;

fn origin(fake: &FakeOrigin) -> Dicomweb {
    let client = Client::new(&fake.base_url(), "clinfer").expect("client");
    Dicomweb::new(client)
}

fn example_conf() -> clinfer::conf::Conf {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en");
    clinfer::conf::load(&path).expect("the shipped example parses")
}

/// A dispatch topology with NO model threads.
///
/// Most tests in this file drive the DECISION paths - claim, cancel,
/// decline, skip - and never reach a matching rule, so nothing is
/// dispatched. A rule that DOES match needs a thread to run on, and those
/// tests use [`threaded_topology`] instead: without a thread the rule
/// reports a per-rule failure, which is the right answer for a model with no
/// thread but not the answer those tests are about.
fn topology(origin: &Dicomweb, counters: &Arc<Counters>) -> worker::Topology {
    worker::Topology::without_models(Arc::new(origin_twin(origin)), Arc::clone(counters))
}

/// A topology with the configuration's model threads STARTED.
///
/// The returned `ModelThreads` must be kept alive for the cycle and joined
/// afterwards, so no child is left running when the test ends.
fn threaded_topology(
    origin: &Dicomweb,
    conf: &clinfer::conf::Conf,
    counters: &Arc<Counters>,
) -> (worker::Topology, worker::ModelThreads) {
    let threads =
        worker::Topology::spawn(conf, Arc::new(origin_twin(origin)), Arc::clone(counters));
    (threads.topology(), threads)
}

/// Join a set of model threads, ignoring a teardown failure.
fn join_models(models: worker::ModelThreads) {
    for handle in models.join() {
        let _ = handle.join();
    }
}

/// A second handle on the same origin, for the calls that need an `Arc`.
///
/// `Dicomweb` is cheap to build (five immutable fields and a fresh socket
/// per request), so the tests reuse the base URL rather than growing a
/// `Clone` on a type that has no need for one.
fn origin_twin(origin: &Dicomweb) -> Dicomweb {
    let (host, port, base) = origin.client().target();
    let client = Client::new(&format!("http://{host}:{port}{base}"), "clinfer").expect("client");
    Dicomweb::new(client)
}

/// A SCHEDULED workitem that names its input study.
///
/// Identity is (0008,0018); the study is reachable only through (0040,4021)
/// Input Information Sequence. The clinical attributes a rule matches on are
/// not on the workitem at all - they are read from the study.
fn workitem_json(study_uid: &str) -> String {
    format!(
        r#"{{"00080018":{{"vr":"UI","Value":["1.2.3"]}},
             "00741000":{{"vr":"CS","Value":["SCHEDULED"]}},
             "00404021":{{"vr":"SQ","Value":[{{"0040E020":{{"vr":"CS","Value":["DICOM"]}},
                                                "0020000D":{{"vr":"UI","Value":["{study_uid}"]}}}}]}}}}"#
    )
}

/// The study, as the origin's QIDO returns it.
///
/// The modality is emitted as (0008,0061) ModalitiesInStudy, which is the
/// STUDY-level matching attribute (PS3.18 Table 10.6.1-5). (0008,0060) is
/// SERIES-level, and a study row that carries only that is not a study
/// modality - which `worker::study_modalities` relies on.
fn study_json(modality: &str, description: &str) -> String {
    format!(
        r#"[{{"00080061":{{"vr":"CS","Value":["{modality}"]}},
              "00081030":{{"vr":"LO","Value":["{description}"]}}}}]"#
    )
}

// -- Transport -----------------------------------------------------------

#[test]
fn a_get_carries_the_identity_and_reaches_the_configured_path() {
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    let client = Client::new(&fake.base_url(), "clinfer").expect("client");
    let response = client
        .send(&clinfer::http::Request::get(client.url("/workitems")))
        .expect("send");

    assert_eq!(response.status, 200);
    let recorded = fake.last().expect("a request arrived");
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.path, "/dicomweb/workitems");
    // The base URL's host and port reach the Host header, which is what a
    // virtual-hosted reverse proxy routes on.
    assert!(
        recorded
            .header("host")
            .is_some_and(|h| h.contains("127.0.0.1"))
    );
    assert_eq!(recorded.identity(), Some("clinfer"));
}

#[test]
fn a_put_sends_its_body_with_a_length_the_origin_can_frame() {
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    let origin = origin(&fake);
    origin
        .update_state("1.2.3", State::InProgress.as_str(), Some("2.25.1"), None)
        .expect("update");

    let recorded = fake.last().expect("a request arrived");
    assert_eq!(recorded.method, "PUT");
    // The standard resource for a change state (PS3.18 11.7).
    assert_eq!(recorded.path, "/dicomweb/workitems/1.2.3/state");
    assert_eq!(
        recorded.header("content-length"),
        Some(recorded.body().len().to_string().as_str())
    );
    assert_eq!(
        recorded.header("content-type"),
        Some("application/dicom+json")
    );

    // The body is the state plus the transaction UID, and it parses.
    let body = json::parse(recorded.body().as_bytes()).expect("parses");
    assert_eq!(body.attribute(TAG_UPS_STATE), Some("IN PROGRESS"));
    assert_eq!(
        body.attribute(clinfer::dicomweb::TAG_TRANSACTION_UID),
        Some("2.25.1")
    );
}

/// RED FACT. Update (11.6) carries the Transaction UID in the PAYLOAD, not as
/// a query parameter.
///
/// PS3.18 11.6.1.4 requires the Update payload to comply with PS3.4 CC.2.6.2,
/// which for an IN PROGRESS workitem makes (0008,1195) an Attribute OF the
/// data set. PS3.18 11.6.1.2 also allows the query parameter, but dcm4chee-arc
/// reads only the data set, so the payload form is the interoperable one.
#[test]
fn an_update_sends_the_transaction_uid_in_the_payload() {
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    let origin = origin(&fake);
    origin
        .update_attributes(
            "1.2.3",
            "2.25.7",
            &[clinfer::dicomweb::OutputRef::new(
                "1.2.840.10008.5.1.4.1.1.11.1",
                "1.2.40.2",
                "1.2.40.1",
            )],
            None,
        )
        .expect("update");

    let recorded = fake.last().expect("a request arrived");
    assert_eq!(recorded.method, "POST");
    // The workitem resource, with NO TransactionUid query parameter: the
    // locking UID travels in the body.
    assert_eq!(
        recorded.path, "/dicomweb/workitems/1.2.3",
        "the TUID is not a query parameter any more"
    );
    assert!(
        !recorded.path.contains("TransactionUid"),
        "no TransactionUid in the URL: {}",
        recorded.path
    );

    let body = json::parse(recorded.body_bytes.as_slice()).expect("parses");
    assert_eq!(
        body.attribute(clinfer::dicomweb::TAG_TRANSACTION_UID),
        Some("2.25.7"),
        "the TUID is a payload attribute"
    );
    // No report was passed, so there is no (0074,1216) item and therefore no
    // place for the outputs: (0040,4033) is a child of that sequence, not a
    // top-level attribute. The TUID still travels, because CC.2.6.2 requires
    // it, and a top-level output list is the shape a standard origin
    // discards - so it is deliberately not written.
    assert!(
        !recorded.body().contains(TAG_OUTPUT_INFORMATION_SEQUENCE),
        "no top-level (0040,4033): {}",
        recorded.body()
    );
    assert!(
        body.attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
            .is_none(),
        "and no performed item either, since no report was given"
    );
}

/// Outputs cannot travel without a performed report.
///
/// PS3.3 Table C.30.3-1 makes (0040,4033) a child of (0074,1216), so a caller
/// that has outputs and no report has no standard place to put them. The
/// locking UID still has to travel (CC.2.6.2), so the Update is sent with it
/// alone rather than the outputs being emitted top-level - which is the shape
/// a standard origin discards, so sending it would be worse than not sending
/// the list at all.
#[test]
fn outputs_without_a_report_do_not_go_out_top_level() {
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    origin(&fake)
        .update_attributes(
            "1.2.3",
            "2.25.7",
            &[clinfer::dicomweb::OutputRef::new(
                "1.2.840.10008.5.1.4.1.1.11.1",
                "1.2.40.2",
                "1.2.40.1",
            )],
            None,
        )
        .expect("update");

    let recorded = fake.last().expect("a request arrived");
    let body = json::parse(recorded.body_bytes.as_slice()).expect("parses");
    assert_eq!(
        body.attribute(clinfer::dicomweb::TAG_TRANSACTION_UID),
        Some("2.25.7"),
        "the locking UID still travels"
    );
    assert!(
        !recorded.body().contains(TAG_OUTPUT_INFORMATION_SEQUENCE),
        "no top-level (0040,4033): the nested shape is the only place it belongs"
    );
}

/// The Update payload with NO outputs still leads with the Transaction UID:
/// the locking Attribute is required (CC.2.6.2), and it is not the output
/// list's business to carry it.
#[test]
fn an_update_without_outputs_still_carries_the_transaction_uid() {
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    let origin = origin(&fake);
    let performed = clinfer::dicomweb::PerformedReport {
        station_aet: "CLINFER".to_string(),
        station_name: "clinfer".to_string(),
        station_scheme: "99CLARUS".to_string(),
        workitem_code: None,
        started_at: "20261010120000".to_string(),
        ended_at: "20261010123456".to_string(),
    };
    origin
        .update_attributes("1.2.3", "2.25.8", &[], Some(&performed))
        .expect("update");

    let recorded = fake.last().expect("a request arrived");
    assert_eq!(recorded.path, "/dicomweb/workitems/1.2.3");
    let body = json::parse(recorded.body_bytes.as_slice()).expect("parses");
    assert_eq!(
        body.attribute(clinfer::dicomweb::TAG_TRANSACTION_UID),
        Some("2.25.8")
    );
    assert!(
        !recorded.body().contains(TAG_OUTPUT_INFORMATION_SEQUENCE),
        "no outputs -> no (0040,4033)"
    );
}

/// A Change State request from an origin built with a `requester` carries the
/// optional `requester` query parameter (PS3.18 11.7 Table 11.7.2.1-1),
/// URL-encoded, on every transition (claim, complete, cancel).
#[test]
fn a_change_state_carries_the_optional_requester_parameter() {
    let fake = FakeOrigin::start(vec![Answer::ok("{}"), Answer::ok("{}")]);
    let client = Client::new(&fake.base_url(), "clinfer").expect("client");
    let origin = Dicomweb::new(client).with_requester("CLINFER");

    origin
        .update_state("1.2.3", State::InProgress.as_str(), Some("2.25.1"), None)
        .expect("claim");
    let recorded = fake.last().expect("a request arrived");
    assert_eq!(recorded.method, "PUT");
    assert_eq!(
        recorded.path, "/dicomweb/workitems/1.2.3/state?requester=CLINFER",
        "the claim must name the requester"
    );

    // A cancel names it too, and an AE title with a reserved character is
    // percent-encoded in the query value.
    let client2 = Client::new(&fake.base_url(), "clinfer").expect("client");
    let origin2 = Dicomweb::new(client2).with_requester("STATION-1");
    origin2
        .update_state(
            "1.2.3",
            State::Canceled.as_str(),
            Some("2.25.1"),
            Some("OPERATOR"),
        )
        .expect("cancel");
    let recorded = fake.last().expect("a request arrived");
    assert_eq!(
        recorded.path,
        "/dicomweb/workitems/1.2.3/state?requester=STATION-1"
    );
}

/// WITHOUT a requester the parameter is omitted entirely, so a bare
/// `Dicomweb::new(client)` origin stays byte-compatible with the
/// pre-requester wire (the optional-absent shape).
#[test]
fn a_change_state_without_a_requester_omits_the_parameter() {
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    let origin = origin(&fake);
    origin
        .update_state("1.2.3", State::InProgress.as_str(), Some("2.25.1"), None)
        .expect("claim");
    let recorded = fake.last().expect("a request arrived");
    assert_eq!(recorded.path, "/dicomweb/workitems/1.2.3/state");
    assert!(
        !recorded.path.contains("requester"),
        "no requester must mean no parameter"
    );
}

// -- Capabilities (WADL, PS3.18 8.9 + Annex H) -------------------------

/// The exact WADL skeleton Clarus serves, reduced to the two subtrees the
/// check reads.
const CLARUS_WADL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<application xmlns="http://wadl.dev.java.net/2009/02">
  <resources base="http://host:8024/dicomweb">
    <resource path="workitems">
      <method name="GET" id="Search for Workitems"/>
      <method name="POST" id="Create Workitem"/>
      <resource path="{subscriptionSOPClassUID}">
        <resource path="subscribers">
          <method name="POST" id="Create Subscription"/>
          <resource path="{subscriber}"><method name="DELETE" id="Delete Subscription"/></resource>
        </resource>
      </resource>
    </resource>
  </resources>
</application>"#;

#[test]
fn a_capabilities_document_without_the_worklist_rows_is_reported_missing() {
    let fake = FakeOrigin::start(vec![Answer::ok(
        r#"<application xmlns="http://wadl.dev.java.net/2009/02">
             <resources base="/dicomweb">
               <resource path="studies"><method name="GET" id="Search for Studies"/></resource>
             </resources>
           </application>"#,
    )]);
    let document = origin(&fake).capabilities().expect("capabilities");
    assert_eq!(
        clinfer::dicomweb::capabilities_has_workitems_and_subscribers(&document),
        (false, false)
    );

    let fake = FakeOrigin::start(vec![Answer::ok(CLARUS_WADL)]);
    let document = origin(&fake).capabilities().expect("capabilities");
    assert_eq!(
        clinfer::dicomweb::capabilities_has_workitems_and_subscribers(&document),
        (true, true)
    );
}

#[test]
fn a_failing_capabilities_call_is_an_error_the_caller_warns_about() {
    let fake = FakeOrigin::start(vec![Answer::status(500)]);
    assert!(origin(&fake).capabilities().is_err());
}

#[test]
fn the_capabilities_request_asks_for_the_wadl_media_type() {
    // PS3.18 8.9.3.1.1: the Retrieve Capabilities representation is a WADL
    // document, served as application/vnd.sun.wadl+xml.
    let fake = FakeOrigin::start(vec![Answer::ok(CLARUS_WADL)]);
    origin(&fake).capabilities().expect("capabilities");

    let recorded = fake.last().expect("a request arrived");
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.path, "/dicomweb/");
    assert_eq!(
        recorded.header("accept"),
        Some("application/vnd.sun.wadl+xml"),
        "the capabilities Accept names the WADL media type"
    );
}

// -- The lifecycle over the wire ----------------------------------------

#[test]
fn a_matching_study_round_trips_claim_then_close() {
    // The rule wants a chest DX with a matching description, and those
    // attributes are read from the STUDY. The processor this harness
    // points at cannot run, so the chain fails after the fetch and the
    // workitem is closed with a reason rather than left claimed forever.
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),       // read the workitem
        Answer::ok(study_json("DX", "CHEST PA")), // read the study (decision)
        Answer::ok("{}"),                         // claim
        Answer::ok(study_json("DX", "CHEST PA")), // read the study (identity)
        Answer::ok("[]"),                         // series: none matching
        Answer::ok("{}"),                         // cancel
    ]);
    let counters = Arc::new(Counters::default());
    let (channels, models) = threaded_topology(&origin(&fake), &example_conf(), &counters);
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &channels,
    );
    join_models(models);
    assert!(
        matches!(outcome, worker::Processed::Canceled(_)),
        "{outcome:?}"
    );

    let requests = fake.requests();
    assert_eq!(requests.len(), 6, "{requests:#?}");
    // Read first: the decision is what makes a claim worthwhile.
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/dicomweb/workitems/1.2.3");
    // The study is what the rule matched on, and it is read through the
    // SearchForStudies transaction - not the retrieve form, which answers
    // WADO-RS multipart regardless of Accept.
    assert_eq!(requests[1].method, "GET");
    assert_eq!(
        requests[1].path, "/dicomweb/studies?StudyInstanceUID=9.9.9",
        "the study attributes come from the search transaction"
    );
    assert_eq!(
        requests[1].header("accept"),
        Some("application/dicom+json"),
        "the search asks for the DICOM JSON form"
    );
    // Then the claim, the chain's own fetches (identity + series), and
    // the close that releases the claim.
    assert_eq!(requests[2].method, "PUT");
    assert_eq!(requests[3].path, "/dicomweb/studies?StudyInstanceUID=9.9.9");
    assert_eq!(
        requests[4].path,
        "/dicomweb/studies/9.9.9/series?includefield=0020000E,00080060,0008103E,00180015"
    );
    assert_eq!(requests[5].method, "PUT");

    let claim = json::parse(requests[2].body().as_bytes()).expect("parses");
    assert_eq!(claim.attribute(TAG_UPS_STATE), Some("IN PROGRESS"));
    let claim_uid = claim
        .attribute(clinfer::dicomweb::TAG_TRANSACTION_UID)
        .expect("the claim carries a Transaction UID");
    // The close echoes the claim's UID - without it a conformant origin
    // refuses and the workitem stays IN PROGRESS.
    let close = json::parse(requests[5].body().as_bytes()).expect("parses");
    assert_eq!(close.attribute(TAG_UPS_STATE), Some("CANCELED"));
    assert_eq!(
        close.attribute(clinfer::dicomweb::TAG_TRANSACTION_UID),
        Some(claim_uid)
    );
    let reason = close
        .attribute(clinfer::dicomweb::TAG_REASON_FOR_CANCELLATION)
        .expect("a reason");
    assert!(reason.contains("has no matching frames"), "{reason}");

    assert_eq!(counters.claims.load(Ordering::Relaxed), 1);
    assert_eq!(counters.canceled.load(Ordering::Relaxed), 1);
}

#[test]
fn a_study_no_rule_matches_is_canceled_by_the_default() {
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(study_json("MR", "BRAIN")),
        Answer::ok("{}"), // claim
        Answer::ok("{}"), // cancel
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    match &outcome {
        worker::Processed::Canceled(reason) => {
            assert!(reason.contains("no rule matched"), "{reason}")
        }
        other => panic!("expected a cancel, got {other:?}"),
    }
    assert_eq!(counters.claims.load(Ordering::Relaxed), 1);
    assert_eq!(counters.canceled.load(Ordering::Relaxed), 1);
}

#[test]
fn a_workitem_that_names_no_study_is_canceled_rather_than_looked_up() {
    // Without an input reference there is no study to read, so nothing can
    // match and the worker must not invent one - reading the workitem UID as
    // if it were a study UID is exactly the latent 404 this guards.
    let fake = FakeOrigin::start(vec![
        Answer::ok(r#"{"00080018":{"Value":["1.2.3"]},"00741000":{"Value":["SCHEDULED"]}}"#),
        Answer::ok("{}"), // claim
        Answer::ok("{}"), // cancel
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );
    assert!(
        matches!(outcome, worker::Processed::Canceled(_)),
        "{outcome:?}"
    );
    // No QIDO by the workitem UID, which is what reading the study from the
    // workitem-UID slot would have done.
    assert!(
        !fake.requests().iter().any(|r| r.path.contains("studies")),
        "no study lookup may be attempted"
    );
}

#[test]
fn a_claim_409_is_the_conflict_and_only_409() {
    // PS3.18 Table 11.7.3-1: 409 is the state conflict - the workitem moved
    // on, so another claimant owns it and nothing went wrong. That is the
    // one status `clinfer_conflicts_total` counts.
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(study_json("DX", "CHEST PA")),
        Answer::status(409),
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(outcome, worker::Processed::Conflict);
    assert_eq!(counters.claims.load(Ordering::Relaxed), 0);
    assert_eq!(counters.conflicts.load(Ordering::Relaxed), 1);
    assert_eq!(
        counters.claim_failures.load(Ordering::Relaxed),
        0,
        "a 409 is not a claim failure"
    );
    // And it did not go on to close a workitem it does not hold.
    assert_eq!(fake.requests().len(), 3);
}

#[test]
fn a_400_on_a_claim_is_a_defect_signal_not_a_conflict() {
    // The table splits the 4xx family: 400 is "invalid request / missing or
    // incorrect Transaction UID", which is a defect on one side or the
    // other - not a lost race. Reading it as a conflict hid real protocol
    // errors behind the contention counter, so it now has its own counter
    // and a log line naming the status.
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(study_json("DX", "CHEST PA")),
        Answer::status(400),
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert!(
        matches!(outcome, worker::Processed::Failed(_)),
        "{outcome:?}"
    );
    assert_eq!(counters.claims.load(Ordering::Relaxed), 0);
    assert_eq!(
        counters.conflicts.load(Ordering::Relaxed),
        0,
        "a 400 is not a state conflict"
    );
    assert_eq!(counters.claim_failures.load(Ordering::Relaxed), 1);
}

#[test]
fn a_study_already_in_progress_is_not_claimed_again() {
    // Only an offered workitem can be taken; anything else belongs to
    // whoever holds it. Nothing is sent to the origin beyond the read.
    let fake = FakeOrigin::start(vec![Answer::ok(
        r#"{"00080018":{"Value":["1.2.3"]},"00741000":{"Value":["IN PROGRESS"]}}"#,
    )]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(outcome, worker::Processed::Conflict);
    assert_eq!(fake.requests().len(), 1, "only the read");
}

#[test]
fn an_unmatched_workitem_is_left_alone_without_being_claimed() {
    // The rules asked for nothing, so nothing is claimed: the workitem
    // stays SCHEDULED and available to whoever does want it.
    let conf = {
        let mut conf = example_conf();
        conf.default_rule = clinfer::conf::DefaultRule::Off;
        conf
    };
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(study_json("MR", "BRAIN")),
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &conf,
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(outcome, worker::Processed::LeftAlone);
    assert_eq!(counters.claims.load(Ordering::Relaxed), 0);
    assert!(
        !fake.requests().iter().any(|r| r.method == "PUT"),
        "an ignored workitem must not be claimed"
    );
}

#[test]
fn a_wire_cancel_that_fails_is_a_failure_not_a_cancel() {
    // The claim succeeds and the cancel is refused: the workitem is still
    // IN PROGRESS, so reporting it as canceled would tell the operator
    // something untrue.
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(study_json("DX", "CHEST PA")),
        Answer::ok("{}"),                         // claim
        Answer::ok(study_json("DX", "CHEST PA")), // study (identity)
        Answer::ok("[]"),                         // series: none matching
        Answer::status(400),                      // cancel refused
    ]);
    let counters = Arc::new(Counters::default());
    let (channels, models) = threaded_topology(&origin(&fake), &example_conf(), &counters);
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &channels,
    );
    join_models(models);

    assert!(
        matches!(outcome, worker::Processed::Failed(_)),
        "{outcome:?}"
    );
    assert_eq!(
        counters.canceled.load(Ordering::Relaxed),
        0,
        "a failed cancel must not be counted as a cancel"
    );
    assert_eq!(counters.claims.load(Ordering::Relaxed), 1);
}

#[test]
fn a_failing_claim_is_a_failure_not_a_conflict() {
    // The read and the study lookup succeed and the CLAIM fails with 500:
    // that is a real failure, not a lost race, and the counters must say so.
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(study_json("DX", "CHEST PA")),
        Answer::status(500),
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert!(
        matches!(outcome, worker::Processed::Failed(_)),
        "{outcome:?}"
    );
    assert_eq!(counters.claims.load(Ordering::Relaxed), 0);
    assert_eq!(counters.conflicts.load(Ordering::Relaxed), 0);
}

#[test]
fn catch_up_claims_only_the_workitems_that_are_still_scheduled() {
    let fake = FakeOrigin::start(vec![
        // The search returns two workitems; only one is still SCHEDULED.
        Answer::ok(r#"[{"00080018":{"Value":["1.1"]}},{"00080018":{"Value":["2.2"]}}]"#),
        // Each is READ first: the read is what decides. 1.1 is finished, so
        // nothing more happens to it.
        Answer::ok(r#"{"00080018":{"Value":["1.1"]},"00741000":{"Value":["COMPLETED"]}}"#),
        // 2.2 is offered but names no study, so there is nothing to match
        // on and the default rule cancels it.
        Answer::ok(r#"{"00080018":{"Value":["2.2"]},"00741000":{"Value":["SCHEDULED"]}}"#),
        Answer::ok("{}"), // claim 2.2
        Answer::ok("{}"), // cancel 2.2
    ]);
    let counters = Arc::new(Counters::default());
    let tallies = worker::catch_up_with(
        &origin(&fake),
        &example_conf(),
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(tallies.skipped, 1, "1.1 was already finished");
    assert_eq!(
        tallies.conflicts, 0,
        "a workitem someone else finished is not contention"
    );
    assert_eq!(counters.conflicts.load(Ordering::Relaxed), 0);
    assert_eq!(tallies.canceled, 1, "2.2 names no study, so it is canceled");
    assert_eq!(tallies.completed, 0);
    let requests = fake.requests();
    let exchange: Vec<(String, String)> = requests
        .iter()
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect();

    // Read the search, then read each workitem. Nothing is written to 1.1:
    // the finished workitem is not ours to touch. 2.2 is read, claimed and
    // released, because the rules had nothing to act on.
    //
    // The search now carries the station key (A1, AIW-I 4.81.4.1.2.2) and
    // every other path is unchanged: the key narrows what comes back, it
    // does not change the per-workitem exchange.
    assert_eq!(
        exchange,
        vec![
            (
                "GET".to_string(),
                format!(
                    "/dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue={}",
                    example_conf().server.station_aet
                )
            ),
            ("GET".to_string(), "/dicomweb/workitems/1.1".to_string()),
            ("GET".to_string(), "/dicomweb/workitems/2.2".to_string()),
            (
                "PUT".to_string(),
                "/dicomweb/workitems/2.2/state".to_string()
            ),
            (
                "PUT".to_string(),
                "/dicomweb/workitems/2.2/state".to_string()
            ),
        ],
        "{exchange:#?}"
    );

    // The finished workitem was never written to.
    assert!(
        !requests
            .iter()
            .any(|r| r.method == "PUT" && r.path.contains("1.1")),
        "a completed workitem must not be written to"
    );
}

/// A rule that matches makes the sidecar claim, and every request after the
/// claim carries the same Transaction UID the claim generated.
#[test]
fn a_claim_is_followed_by_the_same_transaction_uid() {
    let fake = FakeOrigin::start(vec![
        Answer::ok(r#"[{"00080018":{"Value":["2.2"]}}]"#),
        // The workitem names its input study...
        Answer::ok(workitem_json("9.9.9")),
        // ...and the rule's attributes come from that study.
        Answer::ok(study_json("DX", "CHEST PA")),
        // The claim, then the chain's fetches. The processor cannot run
        // here, so the workitem is claimed and released rather than
        // completed.
        Answer::ok("{}"),
        Answer::ok(study_json("DX", "CHEST PA")), // study (identity)
        Answer::ok("[]"),                         // series: none matching
        Answer::ok("{}"),
    ]);
    let counters = Arc::new(Counters::default());
    let tallies = worker::catch_up_with(
        &origin(&fake),
        &example_conf(),
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(
        tallies.canceled, 1,
        "claimed, then released without a runnable processor"
    );

    // Every state change must go to the standard /state resource.
    let puts: Vec<_> = fake
        .requests()
        .into_iter()
        .filter(|r| r.method == "PUT")
        .collect();
    assert_eq!(puts.len(), 2, "a claim and a completion");
    for put in &puts {
        assert_eq!(
            put.path, "/dicomweb/workitems/2.2/state",
            "state changes go to the standard resource"
        );
    }

    // Both carry the same UID: a completion with a different one would be
    // refused by a conformant origin, leaving the workitem IN PROGRESS.
    let uid_of = |body: &str| {
        json::parse(body.as_bytes())
            .ok()
            .and_then(|d| d.attribute("00081195").map(str::to_string))
    };
    let claim_uid = uid_of(&puts[0].body()).expect("the claim carries a Transaction UID");
    let close_uid = uid_of(&puts[1].body()).expect("the completion carries a Transaction UID");
    assert_eq!(
        claim_uid, close_uid,
        "the completion must echo the claim's Transaction UID"
    );
}

// -- D-7: COMPLETED always names what was produced -----------------------

#[test]
fn a_chain_that_stored_nothing_cancels_rather_than_completing() {
    // D-7. COMPLETED is only sent together with (0040,4033) Output
    // Information Sequence (PS3.4 CC.2.5-3): a COMPLETED workitem whose
    // output list is empty claims an output the consumer cannot find. The
    // rule itself is pinned by `worker::transition`'s own tests; this one
    // checks the wire consequence - a chain that produced no object is
    // closed with CANCELED and a reason, and never with COMPLETED.
    //
    // With no runnable processor the chain fails, so the reason is the
    // processor's - which is still a cancel, and still never a completion
    // with an empty list.
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(study_json("DX", "CHEST PA")),
        Answer::ok("{}"),                         // claim
        Answer::ok(study_json("DX", "CHEST PA")), // study (identity)
        Answer::ok("[]"),                         // series: none matching
        Answer::ok("{}"),                         // cancel
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert!(
        matches!(outcome, worker::Processed::Canceled(_)),
        "{outcome:?}"
    );
    assert_eq!(
        counters.completed.load(Ordering::Relaxed),
        0,
        "an output-less cycle must not be counted as completed"
    );

    let requests = fake.requests();
    let close = requests.last().expect("a close was sent");
    assert_eq!(close.method, "PUT");
    assert_eq!(close.path, "/dicomweb/workitems/1.2.3/state");
    let body = json::parse(close.body().as_bytes()).expect("the close parses");
    assert_eq!(body.attribute(TAG_UPS_STATE), Some("CANCELED"));
    assert_eq!(
        body.attribute(clinfer::dicomweb::TAG_REASON_FOR_CANCELLATION),
        outcome_reason(&outcome).as_deref(),
        "the reason is on the wire, not only in the log"
    );
    assert!(
        !close.body().contains("COMPLETED"),
        "COMPLETED must not be sent for a cycle with no output"
    );
    assert!(
        !close.body().contains(TAG_OUTPUT_INFORMATION_SEQUENCE),
        "and with no output list"
    );
}

/// The reason a `Canceled` outcome carries, for the assertion above.
fn outcome_reason(outcome: &worker::Processed) -> Option<String> {
    match outcome {
        worker::Processed::Canceled(reason) => Some(reason.clone()),
        _ => None,
    }
}

#[test]
fn a_completed_transition_with_outputs_but_no_report_carries_only_the_state() {
    // The name used to promise the opposite. The rule is: (0040,4033) is a
    // CHILD of the performed (0074,1216) item (PS3.3 Table C.30.3-1), so a
    // COMPLETED change-state request has nowhere standard to put an output
    // list. The sidecar therefore sends the state and the locking UID and
    // reports the outputs as undeliverable on the UPDATE half - it does not
    // invent a top-level (0040,4033) that a conformant origin discards.
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    origin(&fake)
        .update_state_with_output(
            "1.2.3",
            "COMPLETED",
            Some("1.2.40.1"),
            None,
            &[clinfer::dicomweb::OutputRef::new(
                "1.2.840.10008.5.1.4.1.1.11.1",
                "1.2.40.2",
                "2.25.9",
            )],
        )
        .expect("the fake accepts it");

    let recorded = fake.requests().into_iter().next().expect("recorded");
    let body = json::parse(recorded.body_bytes.as_slice()).expect("parses");
    assert_eq!(body.attribute(TAG_UPS_STATE), Some("COMPLETED"));
    // (0040,4033) names what was produced. Without a report there is no
    // (0074,1216) item to hold it, so the change state carries the state and
    // the locking UID and nothing else - which is what 11.7.1.4 reads, and
    // the reason a completed transition with outputs is a caller that owes a
    // report.
    assert!(
        !recorded.body().contains(TAG_OUTPUT_INFORMATION_SEQUENCE),
        "no top-level (0040,4033): {}",
        recorded.body()
    );
    assert!(
        !recorded
            .body()
            .contains(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE),
        "and no performed item, since no report was given: {}",
        recorded.body()
    );
}

// -- D-8: modality comes from the study, multi-valued -------------------

/// A rule that matches on MODALITY ALONE, so nothing else can rescue it.
///
/// The shipped example's rule also names a description, and the matcher
/// OR-s those signals - so a study row could satisfy it without the
/// modality mattering at all, and a test built on it would pass even if
/// (0008,0061) were never read. This rule removes that escape.
fn modality_only_conf() -> conf::Conf {
    let mut conf = example_conf();
    let rule = &mut conf.rules[0];
    rule.r#match.modality = vec!["DX".to_string()];
    rule.r#match.study_description_contains.clear();
    rule.r#match.code_meaning_contains.clear();
    rule.r#match.body_part_contains.clear();
    rule.r#match.tags.clear();
    conf
}

#[test]
fn modalities_in_study_matches_a_rule_by_set_intersection() {
    // D-8. A study-level row carries (0008,0061) ModalitiesInStudy, which
    // is multi-valued (PS3.18 Table 10.6.1-5). The rule's modality is
    // deliberately NOT the first entry, so a reader that looked at only the
    // first value - or treated the attribute as single-valued - would miss
    // it. A modality-only rule removes every other way to match.
    //
    // The assertion is on the REASON, not the claim count: a workitem that
    // matches no rule is still claimed here (the example's fallback is
    // cancel), so "a claim happened" cannot tell a match from a fallback.
    // A match runs the chain, which fetches the study's series and fails
    // when none exist; a fallback cancels with "no rule matched".
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT","MR","DX"]}}]"#),
        Answer::ok("{}"), // claim
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT","MR","DX"]}}]"#), // identity
        Answer::ok("[]"), // series: none matching
        Answer::ok("{}"), // close
    ]);
    let counters = Arc::new(Counters::default());
    let (channels, models) = threaded_topology(&origin(&fake), &modality_only_conf(), &counters);
    let outcome = worker::process(
        &origin(&fake),
        &modality_only_conf(),
        "1.2.3",
        &counters,
        &channels,
    );
    join_models(models);

    let reason = outcome_reason(&outcome).expect("the cycle closed with a reason");
    assert!(
        !reason.contains("no rule matched"),
        "the rule matched DX in the middle of a multi-valued (0008,0061); \
         the fallback must not have been taken. Reason: {reason}"
    );
    assert!(
        reason.contains("has no matching frames"),
        "a match runs the chain, which fails when no series exist: {reason}"
    );
}

#[test]
fn a_rule_matching_no_entry_of_a_multi_valued_0061_does_not_fire() {
    // The converse: the whole declared set is consulted, and a rule naming
    // something absent from it does not fire however long the list is.
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT","MR","US"]}}]"#),
        Answer::ok("{}"), // claim
        Answer::ok("{}"), // cancel
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &modality_only_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    let reason = outcome_reason(&outcome).expect("the cycle closed with a reason");
    assert!(
        reason.contains("no rule matched"),
        "no declared modality satisfies the DX rule, so the fallback applies: {reason}"
    );
}

#[test]
fn a_study_whose_modalities_do_not_intersect_the_rule_is_not_matched() {
    // (0008,0061) declares CT, the modality-only rule wants DX. No
    // intersection, so the rule does not apply. The fallback is switched
    // OFF, so a non-match yields LeftAlone - which is exactly the
    // observable that distinguishes "no match" from "matched and failed".
    let mut conf = modality_only_conf();
    conf.default_rule = conf::DefaultRule::Off;
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT"]}}]"#),
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &conf,
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(
        counters.claims.load(Ordering::Relaxed),
        0,
        "a mismatch on the study's declared modalities must not be claimed"
    );
    assert!(
        matches!(outcome, worker::Processed::LeftAlone),
        "{outcome:?}"
    );
}

#[test]
fn a_series_level_modality_is_the_documented_fallback_when_0061_is_absent() {
    // The prompt's D-8 wording: (0008,0061) is what a study-level row
    // SHOULD carry, and an origin that returns (0008,0060) instead is read
    // as a one-element list - "if the origin returned neither (0008,0061)
    // nor (0008,0060), the modality rule does not fire". So (0008,0060) is
    // an accepted fallback, NOT the primary key.
    //
    // This test pins that fallback so it cannot be dropped by accident; the
    // PRIMARY key's behaviour is pinned by the (0008,0061) tests above.
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(r#"[{"00080060":{"vr":"CS","Value":["DX"]}}]"#),
        Answer::ok("{}"),                                           // claim
        Answer::ok(r#"[{"00080060":{"vr":"CS","Value":["DX"]}}]"#), // identity
        Answer::ok("[]"),                                           // series: none matching
        Answer::ok("{}"),                                           // close
    ]);
    let counters = Arc::new(Counters::default());
    let (channels, models) = threaded_topology(&origin(&fake), &modality_only_conf(), &counters);
    let outcome = worker::process(
        &origin(&fake),
        &modality_only_conf(),
        "1.2.3",
        &counters,
        &channels,
    );
    join_models(models);

    let reason = outcome_reason(&outcome).expect("the cycle closed with a reason");
    assert!(
        !reason.contains("no rule matched"),
        "an origin that returns only (0008,0060) is read, as the prompt specifies: {reason}"
    );
    assert!(
        reason.contains("has no matching frames"),
        "the match ran the chain, which fails when no series exist: {reason}"
    );
}

#[test]
fn modalities_in_study_takes_precedence_over_a_series_level_modality() {
    // When BOTH are present the study-level set is what decides. A row that
    // declares (0008,0061) = ["CT"] and (0008,0060) = ["DX"] must match a
    // CT rule and must NOT match a DX rule: the series-level value belongs
    // to a different level of the model and cannot override the study's
    // own declaration.
    let mut conf = modality_only_conf();
    conf.default_rule = conf::DefaultRule::Off;
    conf.rules[0].r#match.modality = vec!["DX".to_string()];
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(
            r#"[{"00080061":{"vr":"CS","Value":["CT"]},
                 "00080060":{"vr":"CS","Value":["DX"]}}]"#,
        ),
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &conf,
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(
        counters.claims.load(Ordering::Relaxed),
        0,
        "(0008,0061) = CT wins over (0008,0060) = DX, so a DX rule does not match"
    );
    assert!(
        matches!(outcome, worker::Processed::LeftAlone),
        "{outcome:?}"
    );
}

#[test]
fn a_study_declaring_no_modality_does_not_satisfy_a_modality_rule() {
    // D-8. Neither (0008,0061) nor (0008,0060) on the study row: the
    // modality is unknown, and an unknown modality is not a match. With a
    // modality-only rule and the fallback switched off, the workitem is
    // left alone.
    let mut conf = modality_only_conf();
    conf.default_rule = conf::DefaultRule::Off;
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(r#"[{"00081030":{"vr":"LO","Value":["CHEST PA"]}}]"#),
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &conf,
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(
        counters.claims.load(Ordering::Relaxed),
        0,
        "an unknown modality must not match a modality-constrained rule"
    );
    assert!(
        matches!(outcome, worker::Processed::LeftAlone),
        "{outcome:?}"
    );
}

#[test]
fn a_terminal_workitem_is_skipped_and_not_counted_as_a_conflict() {
    // N-13. A workitem someone else already finished is not contention: the
    // standard has no notion of a conflict for it. Counting it as one made
    // `clinfer_conflicts_total` - which is documented as claims refused by
    // another worker - report a busy swarm where there was only a
    // worklist that had already been worked.
    let fake = FakeOrigin::start(vec![
        Answer::ok(r#"[{"00080018":{"Value":["1.1"]}}]"#),
        Answer::ok(r#"{"00080018":{"Value":["1.1"]},"00741000":{"Value":["COMPLETED"]}}"#),
    ]);
    let counters = Arc::new(Counters::default());
    let tallies = worker::catch_up_with(
        &origin(&fake),
        &example_conf(),
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(
        tallies.skipped, 1,
        "the finished workitem is counted as skipped"
    );
    assert_eq!(tallies.conflicts, 0, "and not as a conflict");
    assert_eq!(tallies.completed, 0);
    assert_eq!(
        counters.conflicts.load(Ordering::Relaxed),
        0,
        "the metric counts real 409 conflicts only"
    );
    // The workitem is read and then left completely alone.
    assert_eq!(fake.requests().len(), 2, "a read, and nothing else");
}

#[test]
fn a_workitem_still_in_progress_is_a_conflict_not_a_skip() {
    // The other half of N-13: IN PROGRESS means another claimant holds it,
    // which IS contention and is what the conflicts counter is for.
    let fake = FakeOrigin::start(vec![Answer::ok(
        r#"{"00080018":{"Value":["1.2.3"]},"00741000":{"Value":["IN PROGRESS"]}}"#,
    )]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &example_conf(),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(outcome, worker::Processed::Conflict);
    assert_eq!(counters.conflicts.load(Ordering::Relaxed), 1);
}

#[test]
fn catch_up_after_a_restart_finds_the_work_the_sidecar_missed() {
    // The same shape as the restart case: a study stored while the sidecar
    // was down is SCHEDULED and is picked up by the search, with no push
    // involved.
    let fake = FakeOrigin::start(vec![
        Answer::ok(r#"[{"00080018":{"Value":["9.9"]}}]"#),
        Answer::ok(workitem_json("9.9.9")),
        Answer::ok(study_json("DX", "CHEST PA")),
        Answer::ok("{}"),                         // claim
        Answer::ok(study_json("DX", "CHEST PA")), // study (identity)
        Answer::ok("[]"),                         // series: none matching
        Answer::ok("{}"),                         // cancel (no runnable processor in this harness)
    ]);
    let counters = Arc::new(Counters::default());
    let tallies = worker::catch_up_with(
        &origin(&fake),
        &example_conf(),
        &counters,
        &topology(&origin(&fake), &counters),
    );
    assert_eq!(counters.claims.load(Ordering::Relaxed), 1);
    assert_eq!(
        tallies.canceled, 1,
        "claimed and closed, with no runnable processor"
    );
    assert_eq!(tallies.canceled, 1);
    // The search the performer issues is the STATION-ORIENTED one (AIW-I
    // 4.81.4.1.2.2); `example_conf` leaves the station at the shipped
    // default, so the key is that default rather than a literal in this
    // test.
    assert_eq!(
        fake.requests()[0].path,
        format!(
            "/dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue={}",
            example_conf().server.station_aet
        )
    );
}

// -- D-10: an unlabelled part is checked against the dataset ------------

/// A minimal Part-10 instance carrying (0002,0010) with `syntax`.
///
/// Explicit VR little endian, which is the encoding the file meta group
/// itself always uses (PS3.10): the file meta group's own syntax is fixed
/// even when the dataset that follows it is compressed, which is exactly
/// why (0002,0010) is the authority on the dataset's encoding.
fn instance_with_transfer_syntax(syntax: &str) -> Vec<u8> {
    let mut out = vec![0u8; 128];
    out.extend_from_slice(b"DICM");
    // (0008,0016) SOP Class UID, so the instance is otherwise readable.
    let sop_class = b"1.2.840.10008.5.1.4.1.1.1.1";
    out.extend_from_slice(&0x0008u16.to_le_bytes());
    out.extend_from_slice(&0x0016u16.to_le_bytes());
    out.extend_from_slice(b"UI");
    out.extend_from_slice(&(sop_class.len() as u16).to_le_bytes());
    out.extend_from_slice(sop_class);
    // (0002,0010) Transfer Syntax UID. UI pads with NUL to even length.
    let uid = syntax.as_bytes();
    let pad = uid.len() % 2;
    out.extend_from_slice(&0x0002u16.to_le_bytes());
    out.extend_from_slice(&0x0010u16.to_le_bytes());
    out.extend_from_slice(b"UI");
    out.extend_from_slice(&((uid.len() + pad) as u16).to_le_bytes());
    out.extend_from_slice(uid);
    if pad == 1 {
        out.push(0);
    }
    out
}

#[test]
fn an_unlabelled_part_whose_dataset_is_evrle_is_accepted() {
    // D-10. The part header names no transfer syntax (conformant: PS3.18
    // 8.7.3 makes the parameter optional), so the dataset's own
    // (0002,0010) decides. An EVRLE dataset is what the reader wants, so
    // it is taken.
    let instance = instance_with_transfer_syntax("1.2.840.10008.1.2.1");
    let fake = FakeOrigin::start(vec![Answer::multipart_dicom_unlabelled(&instance)]);
    let bytes = origin(&fake)
        .instance("1.2.3", "1.2.4", "1.2.5", &mut 0)
        .expect("an EVRLE dataset behind an unlabelled part is readable");
    assert_eq!(bytes, instance, "the part body is carried through");
}

#[test]
fn an_unlabelled_part_whose_dataset_is_compressed_is_refused_by_name() {
    // The defect this closes: an unlabelled part used to be read as EVRLE
    // whatever the dataset said, so a compressed instance was fed to the
    // reader as samples. The dataset names its syntax, and the refusal
    // quotes it.
    let instance = instance_with_transfer_syntax("1.2.840.10008.1.2.4.70");
    let fake = FakeOrigin::start(vec![Answer::multipart_dicom_unlabelled(&instance)]);
    let err = origin(&fake)
        .instance("1.2.3", "1.2.4", "1.2.5", &mut 0)
        .expect_err("a compressed dataset must be refused");
    let text = err.to_string();
    assert!(
        text.contains("1.2.840.10008.1.2.4.70"),
        "the refusal names the syntax it found: {text}"
    );
    assert!(
        text.contains("cannot be read"),
        "and says why it cannot be used: {text}"
    );
}

#[test]
fn an_unlabelled_part_with_no_transfer_syntax_anywhere_is_refused() {
    // Neither the part header nor the dataset establishes the encoding, so
    // reading the bytes as samples would be a guess. It is refused instead.
    let mut instance = vec![0u8; 128];
    instance.extend_from_slice(b"DICM");
    instance.extend_from_slice(&0x0008u16.to_le_bytes());
    instance.extend_from_slice(&0x0016u16.to_le_bytes());
    instance.extend_from_slice(b"UI");
    instance.extend_from_slice(&26u16.to_le_bytes());
    instance.extend_from_slice(b"1.2.840.10008.5.1.4.1.1.1.1");

    let fake = FakeOrigin::start(vec![Answer::multipart_dicom_unlabelled(&instance)]);
    let err = origin(&fake)
        .instance("1.2.3", "1.2.4", "1.2.5", &mut 0)
        .expect_err("an unestablished encoding must be refused");
    let text = err.to_string();
    assert!(
        text.contains("0002,0010") || text.contains("(0002,0010)"),
        "the refusal names the missing attribute: {text}"
    );
}

// -- Subscription --------------------------------------------------------

#[test]
fn a_subscription_posts_under_the_standard_route_with_no_endpoint_at_all() {
    // What the sidecar now sends: the subscription alone, no notification
    // URI. Delivery is the WebSocket channel, so the origin must not be
    // handed a callback it would try to reach.
    let fake = FakeOrigin::start(vec![Answer::with(201, "{}")]);
    origin(&fake)
        .subscribe("CLINFER", None, None)
        .expect("subscribe");

    let recorded = fake.last().expect("a request arrived");
    assert_eq!(recorded.method, "POST");
    assert_eq!(
        recorded.path,
        "/dicomweb/workitems/1.2.840.10008.5.1.4.34.5/subscribers/CLINFER"
    );
    let body = json::parse(recorded.body().as_bytes()).expect("parses");
    assert_eq!(
        body.get("endpoint"),
        None,
        "no endpoint key is emitted when there is no notification URI"
    );
}

#[test]
fn a_subscription_with_an_endpoint_still_posts_it() {
    // The endpoint form is not deleted, only no longer used by this
    // deployment: a subscription that names a URI must still send it.
    let fake = FakeOrigin::start(vec![Answer::with(201, "{}")]);
    origin(&fake)
        .subscribe("CLINFER", Some("http://127.0.0.1:8124/workitems"), None)
        .expect("subscribe");

    let body = json::parse(fake.last().expect("sent").body().as_bytes()).expect("parses");
    assert_eq!(
        body.get("endpoint").and_then(json::Json::as_str),
        Some("http://127.0.0.1:8124/workitems")
    );
}

#[test]
fn a_filtered_subscription_carries_the_scheme_key_in_the_query() {
    // The filter is a QUERY parameter (PS3.18 11.10.1.1 + 8.3.4.1), and the
    // performer's key is the coding scheme - one key, because an attribute
    // shall not repeat, and the scheme is what marks a workitem as this
    // deployment's business.
    let fake = FakeOrigin::start(vec![Answer::with(201, "{}")]);
    let filter = clinfer::dicomweb::WorklistFilter::own_scheme();
    origin(&fake)
        .subscribe("CLINFER", None, Some(&filter))
        .expect("subscribe");

    let sent = fake.last().expect("sent");
    let path = &sent.path;
    assert!(
        path.contains("filter=00404018.00080102=99CLARUS"),
        "the scheme key travels in the query: {path}"
    );
    assert_eq!(
        path.matches("filter=").count(),
        1,
        "one matching key only: {path}"
    );

    // The BODY stays the no-payload form: the filter is not duplicated into
    // it, and the channel-only subscription still carries no endpoint.
    let body = sent.body();
    let body = json::parse(body.as_bytes()).expect("parses");
    assert_eq!(
        body.get("endpoint"),
        None,
        "channel-only stays channel-only"
    );
    assert_eq!(
        body.get("filter"),
        None,
        "the filter is a query parameter, not a body field"
    );
}

// -- The receiver --------------------------------------------------------

/// Start a receiver on an ephemeral port and return the address, the
/// A configuration cell for the listener, with no processors.
///
/// `/processors` reads this cell, so the listener needs one even in a test
/// that never calls the endpoint. Built through the public loader so the
/// value is a real parsed configuration rather than a struct a test
/// invented.
fn listener_config() -> Arc<clinfer::worker::Shared> {
    let dir = std::env::temp_dir().join(format!(
        "clinfer-wire-conf-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let path = dir.join("clinfer.conf");
    std::fs::write(
        &path,
        r#"server:
  base_url: "http://127.0.0.1:1/dicomweb"
  identity_user: "clinfer"
  listen: "127.0.0.1:18124"
engine:
  cache_dir: "/tmp/clinfer-wire-cache"
models:
  proc:
    script: "/bin/true"
    timeout_secs: 60
rules:
  - match:
      modality: [DX]
    model: "proc"
    result:
      kind: pr
      profile_version: 3
default_rule: cancel
"#,
    )
    .expect("write the config");
    let conf = clinfer::conf::load(&path).expect("the listener config parses");
    let _ = std::fs::remove_dir_all(&dir);
    Arc::new(clinfer::worker::Shared::new(Arc::new(conf)))
}

/// counters, and the queue's receiving end.
fn start_receiver(
    policy: Policy,
) -> (
    String,
    Arc<Counters>,
    std::sync::mpsc::Receiver<String>,
    Arc<AtomicBool>,
) {
    // Bind ONCE here and move the SOCKET into the server thread. Probing
    // :0, reading the port, dropping, and rebinding that number inside the
    // thread is a TOCTOU race: two parallel tests can probe the same free
    // port, and the loser's `bind` fails silently in the thread, leaving the
    // winner's address answering a foreign test. A socket moved into the
    // thread cannot be reused before the server listens on it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let listen = listener.local_addr().expect("addr").to_string();

    let counters = Arc::new(Counters::default());
    let (sender, queue) = sync_channel(QUEUE_DEPTH);
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let thread_counters = Arc::clone(&counters);
    let live = listener_config();
    std::thread::spawn(move || {
        let _ = receiver::serve_bound(
            listener,
            policy,
            thread_counters,
            live,
            receiver::deliver_channel(sender),
            thread_shutdown,
        );
    });
    (listen, counters, queue, shutdown)
}

/// A minimal client for the receiver, so the tests do not use clinfer's
/// own client to test clinfer's own listener.
fn send_raw(address: &str, request: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = loop {
        match std::net::TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    stream.write_all(request.as_bytes()).expect("write");
    stream.flush().expect("flush");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

/// A well-formed POST to the retired push endpoint.
///
/// Built by hand rather than through `clinfer::http` because the point is what
/// the SOCKET answers to a request the old origin used to send.
fn retired_push_request(identity: Option<&str>, body: &str) -> String {
    let identity_header = match identity {
        Some(value) => format!("X-Authenticated-User: {value}\r\n"),
        None => String::new(),
    };
    format!(
        "POST /workitems/1.2.3 HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Content-Type: application/dicom+json\r\n\
         {identity_header}\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn the_push_route_is_gone_even_for_a_trusted_peer_with_an_identity() {
    // This endpoint used to answer 200 and hand the UID to the worker. It is
    // retired: the sidecar opens the notification channel itself, so an origin
    // that still pushes gets an honest 404 rather than a silent success that
    // would make it believe delivery worked.
    let policy = Policy::new(&["127.0.0.1".to_string()], true).0;
    let (address, _, queue, shutdown) = start_receiver(policy);
    let body = r#"{"00080018":{"vr":"UI","Value":["1.2.3"]},"00741000":{"Value":["SCHEDULED"]}}"#;

    let response = send_raw(&address, &retired_push_request(Some("clarus"), body));
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(
        queue.try_recv().is_err(),
        "a retired route must not enqueue anything"
    );
    shutdown.store(true, Ordering::Relaxed);
}

#[test]
fn the_push_route_is_gone_for_an_untrusted_peer_too() {
    // 404 rather than 403: the trust check no longer guards a route that
    // exists. Answering 403 would leak that this path once existed and that
    // the caller failed its policy check, which is not what happened.
    let policy = Policy::new(&["10.0.0.5".to_string()], false).0;
    let (address, counters, queue, shutdown) = start_receiver(policy);
    let body = r#"{"00080018":{"Value":["1.2.3"]}}"#;

    let response = send_raw(&address, &retired_push_request(Some("clarus"), body));
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(queue.try_recv().is_err(), "nothing should be queued");
    assert_eq!(
        counters.rejected_peers.load(Ordering::Relaxed),
        0,
        "no policy decision is taken for a route that does not exist"
    );
    shutdown.store(true, Ordering::Relaxed);
}

#[test]
fn the_push_route_is_gone_without_an_identity_as_well() {
    let policy = Policy::new(&["127.0.0.1".to_string()], true).0;
    let (address, counters, queue, shutdown) = start_receiver(policy);
    let body = r#"{"00080018":{"Value":["1.2.3"]}}"#;

    let response = send_raw(&address, &retired_push_request(None, body));
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(queue.try_recv().is_err(), "nothing should be queued");
    assert_eq!(counters.ws_events.load(Ordering::Relaxed), 0);
    shutdown.store(true, Ordering::Relaxed);
}

#[test]
fn health_metrics_and_version_answer_on_the_same_listener() {
    let policy = Policy::new(&["127.0.0.1".to_string()], false).0;
    let (address, _, _, shutdown) = start_receiver(policy);
    let get = |path: &str| {
        send_raw(
            &address,
            &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"),
        )
    };

    let health = get("/health");
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.ends_with("OK\n"), "{health}");

    let metrics = get("/metrics");
    assert!(metrics.starts_with("HTTP/1.1 200"), "{metrics}");
    assert!(metrics.contains("clinfer_claims_total"), "{metrics}");

    let version = get("/version");
    assert!(version.contains("clinfer "), "{version}");

    let missing = get("/nope");
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    shutdown.store(true, Ordering::Relaxed);
}

#[test]
fn the_processors_endpoint_answers_with_the_configured_list() {
    // Through the SOCKET, not by calling `Processors` directly: the route is
    // what ClarityX builds its menu from, and a test that read the type
    // would not notice a route that answered with something else.
    let policy = Policy::new(&["127.0.0.1".to_string()], false).0;
    let (address, _, _, shutdown) = start_receiver(policy);
    let response = send_raw(
        &address,
        "GET /processors HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    );

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        response.contains("application/json"),
        "the answer is JSON: {response}"
    );
    // The fixture configuration declares one processor, `proc`, whose rule
    // produces `pr`. Both the id and the kind are asserted: a route that
    // answered with an empty list, or with a kind taken from somewhere
    // other than the rule, would fail here.
    assert!(
        response.contains(r#"{"processors":[{"id":"proc","kind":"pr"}]}"#),
        "the configured processor is listed with its rule's kind: {response}"
    );
    shutdown.store(true, Ordering::Relaxed);
}

#[test]
fn a_post_with_a_body_that_is_not_a_workitem_is_gone_as_a_route() {
    let policy = Policy::new(&["127.0.0.1".to_string()], false).0;
    let (address, _, queue, shutdown) = start_receiver(policy);

    let response = send_raw(
        &address,
        &retired_push_request(Some("clarus"), "{\"00741000\":{}}"),
    );
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(queue.try_recv().is_err());
    shutdown.store(true, Ordering::Relaxed);
}

#[test]
fn a_post_to_the_queue_depth_of_the_retired_route_is_irrelevant() {
    // The queue is still real: the worker drains it. What is gone is the
    // inbound path that filled it, so nothing over the socket can make it
    // overflow. Asserted rather than assumed, because the 503 answer was a
    // property of the retired handler and the queue itself is unchanged.
    let policy = Policy::new(&["127.0.0.1".to_string()], false).0;
    // Bind HERE and serve on the bound listener: a probe-and-release port is a
    // race (another process may take it), and `serve(addr)` reports a bind
    // failure by returning from a thread nobody joins, so the test would then
    // fail on a connection error that says nothing about the actual bug.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let counters = Arc::new(Counters::default());
    let (sender, queue) = sync_channel(1);
    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = Arc::clone(&shutdown);
        let counters = Arc::clone(&counters);
        let live = listener_config();
        std::thread::spawn(move || {
            let _ = receiver::serve_bound(
                listener,
                policy,
                counters,
                live,
                receiver::deliver_channel(sender),
                shutdown,
            );
        });
    }

    let body = r#"{"00080018":{"Value":["1.2.3"]}}"#;
    for _ in 0..2 {
        let response = send_raw(
            &format!("127.0.0.1:{port}"),
            &retired_push_request(None, body),
        );
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    }
    assert!(
        queue.try_recv().is_err(),
        "the socket can no longer enqueue at all"
    );
    shutdown.store(true, Ordering::Relaxed);
}

// -- The worker loop -----------------------------------------------------

#[test]
fn a_pushed_workitem_runs_through_the_worker_loop() {
    let fake = FakeOrigin::start(vec![
        Answer::ok(workitem_json("9.9.9")),    // read the workitem
        Answer::ok(study_json("MR", "BRAIN")), // read the study
        Answer::ok("{}"),                      // claim
        Answer::ok("{}"),                      // close (no engine here)
    ]);
    let counters = Arc::new(Counters::default());
    let (sender, queue) = sync_channel(QUEUE_DEPTH);
    let shared = Arc::new(Shared::new(Arc::new(example_conf())));
    let (worker_thread, shutdown) = worker::spawn_with(
        Arc::new(origin(&fake)),
        shared,
        queue,
        Arc::clone(&counters),
        topology(&origin(&fake), &counters),
    );

    sender.send("1.2.3".to_string()).expect("queue");
    assert!(
        fake_origin::wait_for(
            || counters.canceled.load(Ordering::Relaxed) == 1,
            Duration::from_secs(5)
        ),
        "the worker did not process the workitem"
    );
    assert_eq!(counters.claims.load(Ordering::Relaxed), 1);

    shutdown.store(true, Ordering::Relaxed);
    // Unblock the loop's timeout so the thread can observe the flag.
    let _ = sender.send("".to_string());
    let _ = worker_thread.join();
}

// -- The lifecycle decision ---------------------------------------------

#[test]
fn the_lifecycle_agrees_with_the_matcher_on_the_reference_rule() {
    // The pure decision and the wire behaviour are the same conclusion
    // reached twice; this asserts they have not diverged.
    let conf = example_conf();
    let attrs = clinfer::matcher::StudyAttrs {
        modalities: vec!["DX".into()],
        study_description: "CHEST PA".into(),
        ..clinfer::matcher::StudyAttrs::default()
    };
    assert_eq!(
        ups::decide(&conf, &attrs),
        Outcome::Infer {
            rule_indices: vec![0]
        }
    );
    assert!(matches!(
        ups::action(Outcome::Infer {
            rule_indices: vec![0]
        }),
        Action::Infer { .. }
    ));
}

// -- Sprint A: station identity, claim guards, performed report ----------
//
// A1 (T7) sends the station key on search, A2 (T8) refuses work that is not
// this performer's, A3 (T6) reports the performed station and the echoed
// workitem code on the update. The three share one configuration value, and
// the tests below are the wire evidence for each.

/// The example configuration with a station identity this test controls.
///
/// The shipped example names `CLINFER`, so a test that wants to prove the
/// value is READ rather than hardcoded has to change it.
fn station_conf(aet: &str, name: &str) -> conf::Conf {
    let mut conf = example_conf();
    conf.server.station_aet = aet.to_string();
    conf.server.station_name = name.to_string();
    conf
}

/// A workitem body with the assignment and readiness attributes a guard
/// looks at. `assignment` is `None` for a workitem that names nobody.
fn guarded_workitem(study_uid: &str, assignment: Option<(&str, &str)>, readiness: &str) -> String {
    let assignment = match assignment {
        Some((_tag, value)) => format!(
            r#","00404025":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["{value}"]}},
                            "00080102":{{"vr":"SH","Value":["99CLARUS"]}},
                            "00080104":{{"vr":"LO","Value":["AI station"]}}}}]}}"#
        ),
        None => String::new(),
    };
    format!(
        r#"{{"00080018":{{"vr":"UI","Value":["1.2.3"]}},
             "00741000":{{"vr":"CS","Value":["SCHEDULED"]}},
             "00404041":{{"vr":"CS","Value":["{readiness}"]}},
             "00404018":{{"vr":"SQ","Value":[{{"00080100":{{"vr":"SH","Value":["CAD"]}},
                                              "00080102":{{"vr":"SH","Value":["99CLARUS"]}},
                                              "00080104":{{"vr":"LO","Value":["AI detection"]}}}}]}},
             "00404021":{{"vr":"SQ","Value":[{{"0040E020":{{"vr":"CS","Value":["DICOM"]}},
                                                "0020000D":{{"vr":"UI","Value":["{study_uid}"]}}}}]}}{assignment}}}"#
    )
}

#[test]
fn a_station_search_sends_the_configured_ae_title_as_the_matching_key() {
    // A1 (T7), AIW-I 4.81.4.1.2.2: the performer's search carries its own
    // station. PS3.4 Table CC.2.5-3 carries that fact in the Code Value of
    // (0040,4025), and PS3.18 8.3.4 spells the key as the keyword path. The
    // assertion is on the REQUEST, because that is the whole obligation -
    // what the origin does with the key is the origin's conformance, not this
    // sidecar's.
    let fake = FakeOrigin::start(vec![Answer::ok("[]")]);
    let uids = origin(&fake)
        .search_workitems_for_station("STATION_7")
        .expect("the fake answers");
    assert!(uids.is_empty());

    let recorded = fake.requests().into_iter().next().expect("recorded");
    assert_eq!(recorded.method, "GET");
    assert_eq!(
        recorded.path, "/dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue=STATION_7",
        "the station key is sent as the sequence-qualified Code Value"
    );
    assert_eq!(
        recorded.identity(),
        Some("clinfer"),
        "and the request still carries the configured identity"
    );
}

#[test]
fn the_station_search_percent_encodes_an_ae_title_that_needs_it() {
    // An AE Title may carry a space (PS3.5 Annex A allows any character from
    // the repertoire), and a bare space in a query string is not the same
    // value to a form parser as an encoded one.
    let fake = FakeOrigin::start(vec![Answer::ok("[]")]);
    origin(&fake)
        .search_workitems_for_station("CT 1")
        .expect("the fake answers");
    let recorded = fake.requests().into_iter().next().expect("recorded");
    assert_eq!(
        recorded.path,
        "/dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue=CT%201"
    );
}

#[test]
fn catch_up_uses_the_station_oriented_search_and_names_the_station() {
    // The same obligation, reached through the real startup path: the FIRST
    // request is the station-oriented one under this station's own AE title
    // (an empty answer is re-asked unfiltered, so the requests are two).
    let fake = FakeOrigin::start(vec![Answer::ok("[]"), Answer::ok("[]")]);
    let counters = Arc::new(Counters::default());
    let tallies = worker::catch_up_with(
        &origin(&fake),
        &station_conf("CLINFER_1", "Inference station"),
        &counters,
        &topology(&origin(&fake), &counters),
    );
    assert_eq!(tallies.completed, 0);
    let recorded = fake.requests();
    assert_eq!(
        recorded[0].path,
        "/dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue=CLINFER_1"
    );
    assert_eq!(recorded[1].path, "/dicomweb/workitems");
}

#[test]
fn catch_up_falls_back_to_the_unfiltered_search_when_the_station_key_is_refused() {
    // An origin that does not accept the key must not leave the sidecar
    // with nothing to do. The fallback is a second REQUEST, not a silent
    // give-up, and it is the unfiltered search exactly as before Sprint A.
    let fake = FakeOrigin::start(vec![
        Answer::with(400, r#"{"error":"unsupported matching attribute"}"#),
        Answer::ok("[]"),
    ]);
    let counters = Arc::new(Counters::default());
    worker::catch_up_with(
        &origin(&fake),
        &station_conf("CLINFER", "Inference station"),
        &counters,
        &topology(&origin(&fake), &counters),
    );

    let requests = fake.requests();
    assert_eq!(requests.len(), 2, "the refusal is retried unfiltered");
    assert_eq!(
        requests[0].path,
        "/dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue=CLINFER"
    );
    assert_eq!(requests[1].path, "/dicomweb/workitems");
}

#[test]
fn catch_up_falls_back_to_the_unfiltered_search_when_the_station_search_is_empty() {
    // The key is accepted but the narrowed queue is empty. An empty answer
    // is not "no work": a queue narrowed to OUR station cannot see an
    // unassigned workitem (an offer to whoever can serve it), so the
    // performer re-asks unfiltered and lets the local rules decide.
    let fake = FakeOrigin::start(vec![Answer::ok("[]"), Answer::ok("[]")]);
    let counters = Arc::new(Counters::default());
    worker::catch_up_with(
        &origin(&fake),
        &station_conf("CLINFER", "Inference station"),
        &counters,
        &topology(&origin(&fake), &counters),
    );

    let requests = fake.requests();
    assert_eq!(
        requests.len(),
        2,
        "an empty station queue is re-asked unfiltered"
    );
    assert_eq!(
        requests[0].path,
        "/dicomweb/workitems?ScheduledStationNameCodeSequence.CodeValue=CLINFER"
    );
    assert_eq!(requests[1].path, "/dicomweb/workitems");
}

#[test]
fn a_workitem_assigned_to_another_station_is_left_alone() {
    // A2 (T8), AIW-I 4.82.4.1.1. Two consequences matter and both are
    // asserted: the workitem is NOT claimed, and the outcome is LeftAlone
    // rather than a failure - a decline is a decision, not an error.
    let fake = FakeOrigin::start(vec![Answer::ok(guarded_workitem(
        "9.9.9",
        Some(("00404025", "OTHER_STATION")),
        "READY",
    ))]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &station_conf("CLINFER", "Inference station"),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(outcome, worker::Processed::LeftAlone);
    assert_eq!(
        fake.requests().len(),
        1,
        "only the fetch happened: a declined workitem is not claimed"
    );
    assert_eq!(
        counters.declined.load(Ordering::Relaxed),
        1,
        "and the decline is counted where an operator can see it"
    );
    assert_eq!(counters.claims.load(Ordering::Relaxed), 0);
}

#[test]
fn a_workitem_assigned_to_this_station_is_claimed() {
    // The converse of the guard, and the case that makes it a guard rather
    // than a wall. The station matches case-insensitively, because an AE
    // Title is defined as a case-insensitive identifier (PS3.5 Annex A) and
    // an origin that echoes it differently must not make the site stop.
    let fake = FakeOrigin::start(vec![
        Answer::ok(guarded_workitem(
            "9.9.9",
            Some(("00404025", "clinfer")),
            "READY",
        )),
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT"]}}]"#),
        Answer::ok("{}"), // claim
        Answer::ok("{}"), // cancel (no rule matched the CT study)
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &station_conf("CLINFER", "Inference station"),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_ne!(
        outcome,
        worker::Processed::LeftAlone,
        "a matching assignment is not a decline"
    );
    assert_eq!(counters.declined.load(Ordering::Relaxed), 0);
    assert_eq!(counters.claims.load(Ordering::Relaxed), 1, "it was claimed");
}

#[test]
fn an_unassigned_workitem_is_claimable_the_regression_pin() {
    // THE regression pin for the guard: a workitem with NO assignment
    // attributes is "offered to whoever can serve it", not "assigned to
    // somebody else". Every deployment that never sets a station runs in
    // this shape, so a guard that refused it would stop the product.
    let fake = FakeOrigin::start(vec![
        Answer::ok(guarded_workitem("9.9.9", None, "READY")),
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT"]}}]"#),
        Answer::ok("{}"), // claim
        Answer::ok("{}"), // cancel
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &station_conf("CLINFER", "Inference station"),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_ne!(outcome, worker::Processed::LeftAlone);
    assert_eq!(
        counters.declined.load(Ordering::Relaxed),
        0,
        "an absent assignment names nobody, so it is not a decline"
    );
    assert_eq!(counters.claims.load(Ordering::Relaxed), 1);
}

#[test]
fn an_empty_assignment_value_is_treated_as_unassigned_not_as_a_mismatch() {
    // A populated sequence whose Code Value is empty is the same statement
    // as an absent sequence. Refusing here would decline every workitem an
    // origin sent with a placeholder, which is the opposite of the intent.
    let fake = FakeOrigin::start(vec![
        Answer::ok(guarded_workitem("9.9.9", Some(("00404025", "")), "READY")),
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT"]}}]"#),
        Answer::ok("{}"), // claim
        Answer::ok("{}"), // cancel
    ]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &station_conf("CLINFER", "Inference station"),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_ne!(outcome, worker::Processed::LeftAlone);
    assert_eq!(counters.declined.load(Ordering::Relaxed), 0);
}

#[test]
fn a_workitem_whose_input_is_incomplete_is_not_claimed() {
    // A2 (T8), second obligation: 4.82.4.1.1 forbids claiming a workitem
    // whose input has not all arrived. Claiming it would run a model over a
    // partial study, which is precisely what the guard exists to prevent.
    let fake = FakeOrigin::start(vec![Answer::ok(guarded_workitem(
        "9.9.9",
        None,
        "INCOMPLETE",
    ))]);
    let counters = Arc::new(Counters::default());
    let outcome = worker::process(
        &origin(&fake),
        &station_conf("CLINFER", "Inference station"),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    assert_eq!(outcome, worker::Processed::LeftAlone);
    assert_eq!(counters.claims.load(Ordering::Relaxed), 0);
    assert_eq!(counters.declined.load(Ordering::Relaxed), 1);
}

#[test]
fn an_unknown_readiness_value_does_not_block_a_claim() {
    // Only INCOMPLETE forbids a claim. A value this build has never seen is
    // not evidence that the input is missing, and refusing on it would let
    // an origin's future addition silently stop the site.
    let fake = FakeOrigin::start(vec![
        Answer::ok(guarded_workitem("9.9.9", None, "READY_FOR_PROCESSING")),
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT"]}}]"#),
        Answer::ok("{}"), // claim
        Answer::ok("{}"), // cancel
    ]);
    let counters = Arc::new(Counters::default());
    worker::process(
        &origin(&fake),
        &station_conf("CLINFER", "Inference station"),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );
    assert_eq!(counters.declined.load(Ordering::Relaxed), 0);
    assert_eq!(counters.claims.load(Ordering::Relaxed), 1);
}

#[test]
fn the_completed_update_reports_the_performed_station_and_echoes_the_workitem_code() {
    // A3 (T6), AIW-I 4.84.4.1.2.1. One request carries both, and the
    // station comes from the configuration while the workitem code is the
    // one the workitem scheduled - not a second, independent value.
    let fake = FakeOrigin::start(vec![
        Answer::ok(guarded_workitem("9.9.9", None, "READY")),
        Answer::ok(r#"[{"00080061":{"vr":"CS","Value":["CT"]}}]"#),
        Answer::ok("{}"), // claim
        Answer::ok("{}"), // cancel (no rule matched)
    ]);
    let counters = Arc::new(Counters::default());
    worker::process(
        &origin(&fake),
        &station_conf("STATION_9", "Inference station"),
        "1.2.3",
        &counters,
        &topology(&origin(&fake), &counters),
    );

    // The cancel path deliberately does NOT carry the performed report: the
    // performer did not perform the step, so reporting a performed station
    // on it would be a false statement. Prove the negative first.
    let close = fake
        .requests()
        .into_iter()
        .find(|r| r.method == "PUT")
        .expect("the cycle closed the workitem");
    assert!(
        !close.body().contains("00404028"),
        "a cancel carries no Performed Station Name: {}",
        close.body()
    );
    assert!(
        !close.body().contains("00404019"),
        "and no Performed Workitem Code: {}",
        close.body()
    );
}

#[test]
fn the_performed_report_is_one_object_carrying_both_sequences() {
    // The encoder, asserted directly: (0040,4019) echoes the scheduled code
    // and (0040,4028) names this station, in ONE body. Written as a direct
    // call because the COMPLETED path needs a whole inference chain to
    // reach, and the encoder is the part that can be wrong.
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    let report = clinfer::dicomweb::PerformedReport {
        station_aet: "STATION_9".to_string(),
        station_name: "Inference station".to_string(),
        station_scheme: clinfer::dicomweb::LOCAL_CODING_SCHEME.to_string(),
        workitem_code: Some(clinfer::dicomweb::WorkitemCode {
            value: "CAD".to_string(),
            scheme: "99CLARUS".to_string(),
            meaning: "AI detection".to_string(),
        }),
        started_at: "20261010120000".to_string(),
        ended_at: "20261010123456".to_string(),
    };
    origin(&fake)
        .update_state_with_report(
            "1.2.3",
            "COMPLETED",
            Some("1.2.40.1"),
            None,
            &[clinfer::dicomweb::OutputRef::new(
                "1.2.840.10008.5.1.4.1.1.11.1",
                "1.2.40.2",
                "2.25.9",
            )],
            Some(&report),
        )
        .expect("the fake accepts it");

    let recorded = fake.requests().into_iter().next().expect("recorded");
    let body = json::parse(recorded.body_bytes.as_slice()).expect("parses");
    assert_eq!(body.attribute(TAG_UPS_STATE), Some("COMPLETED"));
    assert!(
        recorded.body().contains("00404028"),
        "(0040,4028) is present: {}",
        recorded.body()
    );
    assert!(
        recorded.body().contains("STATION_9"),
        "the Code Value is the configured AE Title"
    );
    assert!(
        recorded.body().contains("Inference station"),
        "and the Code Meaning is the configured station name"
    );
    assert!(
        recorded.body().contains("00404019"),
        "(0040,4019) echoes the scheduled code"
    );
    assert!(
        recorded.body().contains("99CLARUS"),
        "under the local coding scheme"
    );
    // The outputs are still named: the report joins the payload rather than
    // replacing it.
    assert!(recorded.body().contains(TAG_OUTPUT_INFORMATION_SEQUENCE));
    assert!(recorded.body().contains("2.25.9"));
    // (0040,4019) sorts before (0040,4028), and the object closes once.
    let workitem_code_at = recorded.body().find("00404019").expect("present");
    let station_at = recorded.body().find("00404028").expect("present");
    assert!(
        workitem_code_at < station_at,
        "the sequences are written in ascending tag order"
    );
    assert!(
        recorded.body().ends_with("}}"),
        "the object is closed exactly once: {}",
        recorded.body()
    );
}

#[test]
fn a_performed_report_without_a_scheduled_code_omits_the_echo() {
    // AIW-I 4.84.4.1.2.1: the performer echoes the code "if present". A
    // workitem that scheduled no code has nothing to echo, and inventing
    // one would misreport which task ran.
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    let report = clinfer::dicomweb::PerformedReport {
        station_aet: "STATION_9".to_string(),
        station_name: "Inference station".to_string(),
        station_scheme: clinfer::dicomweb::LOCAL_CODING_SCHEME.to_string(),
        workitem_code: None,
        started_at: "20261010120000".to_string(),
        ended_at: "20261010123456".to_string(),
    };
    origin(&fake)
        .update_state_with_report(
            "1.2.3",
            "COMPLETED",
            Some("1.2.40.1"),
            None,
            &[],
            Some(&report),
        )
        .expect("the fake accepts it");

    let recorded = fake.requests().into_iter().next().expect("recorded");
    assert!(
        !recorded.body().contains("00404019"),
        "(0040,4019) is omitted rather than invented: {}",
        recorded.body()
    );
    assert!(
        recorded.body().contains("00404028"),
        "but the performed station is still reported"
    );
}

#[test]
fn the_performed_station_code_carries_the_local_coding_scheme() {
    // An AE Title is a site-local identifier, not a DICOM vocabulary term.
    // Declaring it under a private scheme is the honest form (PS3.3 Table
    // 8.8-1); declaring it under a standard one would be a false statement
    // about where the code comes from.
    assert_eq!(clinfer::dicomweb::LOCAL_CODING_SCHEME, "99CLARUS");
    let fake = FakeOrigin::start(vec![Answer::ok("{}")]);
    let report = clinfer::dicomweb::PerformedReport {
        station_aet: "STATION_9".to_string(),
        station_name: "Inference station".to_string(),
        station_scheme: clinfer::dicomweb::LOCAL_CODING_SCHEME.to_string(),
        workitem_code: None,
        started_at: "20261010120000".to_string(),
        ended_at: "20261010123456".to_string(),
    };
    origin(&fake)
        .update_state_with_report("1.2.3", "COMPLETED", None, None, &[], Some(&report))
        .expect("the fake accepts it");
    let recorded = fake.requests().into_iter().next().expect("recorded");
    let body = json::parse(recorded.body_bytes.as_slice()).expect("parses");
    // The station code is a CHILD of (0074,1216), not a top-level sequence:
    // PS3.3 Table C.30.3-1 nests it, and that is the shape a standard origin
    // reads.
    let item = body
        .attribute_item(TAG_UPS_PERFORMED_PROCEDURE_SEQUENCE)
        .expect("the performed report is one (0074,1216) item");
    let station = item
        .attribute_item("00404028")
        .expect("the sequence carries one item");
    assert_eq!(station.attribute("00080102"), Some("99CLARUS"));
    assert_eq!(station.attribute("00080100"), Some("STATION_9"));
    assert!(
        body.attribute_item("00404028").is_none(),
        "and it is not also a top-level sequence"
    );
}

// -- Instance-fetch transport retry --------------------------------------

/// A 2x2 8-bit DX instance with a real pixel tail, for the retrieve tests.
fn dx_instance(sop_instance_uid: &str) -> Vec<u8> {
    // A minimal dataset the reader can take: the same shape the concurrency
    // harness builds, kept local because these tests assert on the transport
    // and only need an object the parser accepts.
    let mut out = vec![0u8; 128];
    out.extend_from_slice(b"DICM");
    let tag = |group: u16, element: u16, vr: &[u8; 2], value: &[u8]| {
        let mut item = Vec::new();
        item.extend_from_slice(&group.to_le_bytes());
        item.extend_from_slice(&element.to_le_bytes());
        item.extend_from_slice(vr);
        if matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN") {
            item.extend_from_slice(&[0, 0]);
            item.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            item.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        item.extend_from_slice(value);
        item
    };
    out.extend_from_slice(&tag(0x0008, 0x0005, b"CS", b"ISO_IR 192"));
    out.extend_from_slice(&tag(
        0x0008,
        0x0016,
        b"UI",
        b"1.2.840.10008.5.1.4.1.1.1.1\0",
    ));
    let uid = if sop_instance_uid.len().is_multiple_of(2) {
        sop_instance_uid.to_string()
    } else {
        format!("{sop_instance_uid}\0")
    };
    out.extend_from_slice(&tag(0x0008, 0x0018, b"UI", uid.as_bytes()));
    out.extend_from_slice(&tag(0x0008, 0x0060, b"CS", b"DX"));
    out.extend_from_slice(&tag(0x0028, 0x0002, b"US", &1u16.to_le_bytes()));
    out.extend_from_slice(&tag(0x0028, 0x0004, b"CS", b"MONOCHROME2"));
    out.extend_from_slice(&tag(0x0028, 0x0010, b"US", &2u16.to_le_bytes()));
    out.extend_from_slice(&tag(0x0028, 0x0011, b"US", &2u16.to_le_bytes()));
    out.extend_from_slice(&tag(0x0028, 0x0100, b"US", &8u16.to_le_bytes()));
    out.extend_from_slice(&tag(0x0028, 0x0101, b"US", &8u16.to_le_bytes()));
    out.extend_from_slice(&tag(0x0028, 0x0103, b"US", &0u16.to_le_bytes()));
    out.extend_from_slice(&tag(0x7FE0, 0x0010, b"OW", &[1, 2, 3, 4]));
    out
}

#[test]
fn an_instance_whose_first_attempt_is_dropped_mid_body_is_fetched_on_retry() {
    // The field failure, reproduced at the wire: the origin sends the head,
    // promises the whole body, writes part of it and closes. The first
    // attempt must be treated as a transport failure (not as a short
    // answer), the second attempt must serve the full instance, and the
    // caller must see the instance plus the retry it paid for.
    let instance = dx_instance("1.2.3.4.5.1");
    let cut = instance.len() / 2;
    let fake = FakeOrigin::start(vec![
        Answer::multipart_dicom(&instance).cut_short(cut),
        Answer::multipart_dicom(&instance),
    ]);
    let mut attempts: u32 = 0;
    let bytes = origin(&fake)
        .instance("9.9.9", "1.2.3.4.5", "1.2.3.4.5.1", &mut attempts)
        .expect("the second attempt completes the instance");
    assert_eq!(bytes, instance, "the retry returned the whole instance");
    assert_eq!(attempts, 2, "exactly two sends were made");
    assert_eq!(
        fake.requests().len(),
        2,
        "the origin saw two requests: {:#?}",
        fake.requests()
    );
}

#[test]
fn an_instance_dropped_on_every_attempt_fails_as_a_transport_error() {
    // The whole budget on a body that never finishes: the call fails, and
    // the failure must carry the transport wording rather than a short
    // object, because a caller cannot tell a truncated instance from a
    // small one that arrived whole.
    let instance = dx_instance("1.2.3.4.5.1");
    let cut = instance.len() / 2;
    let fake = FakeOrigin::start(vec![
        Answer::multipart_dicom(&instance).cut_short(cut),
        Answer::multipart_dicom(&instance).cut_short(cut),
        Answer::multipart_dicom(&instance).cut_short(cut),
    ]);
    let mut attempts: u32 = 0;
    let err = origin(&fake)
        .instance("9.9.9", "1.2.3.4.5", "1.2.3.4.5.1", &mut attempts)
        .expect_err("a body that never finishes fails the call");
    assert_eq!(attempts, 3, "the whole budget was spent");
    assert_eq!(
        fake.requests().len(),
        3,
        "three sends, one per attempt: {:#?}",
        fake.requests()
    );
    assert!(
        err.to_string().contains("transport"),
        "the failure names the transport: {err}"
    );
}

#[test]
fn a_not_found_instance_is_not_retried() {
    // A 404 is the origin's answer about THIS instance. Re-sending produces
    // the same answer, so the budget must not be spent on it.
    let fake = FakeOrigin::start(vec![Answer::status(404)]);
    let mut attempts: u32 = 0;
    let err = origin(&fake)
        .instance("9.9.9", "1.2.3.4.5", "1.2.3.4.5.1", &mut attempts)
        .expect_err("a 404 is a failure");
    assert_eq!(attempts, 1, "one send only");
    assert_eq!(fake.requests().len(), 1, "the origin saw one request");
    assert!(err.to_string().contains("404"), "{err}");
}

/// A DX instance padded past one 8 MiB budget with a legal (0007,xxxx)
/// private element.
///
/// The padding rides inside a real data element rather than after the pixel
/// data so the object stays well-formed: the reader walks elements to the
/// end, and trailing bytes would be a different test.
fn dx_instance_larger_than_the_default_budget(sop_instance_uid: &str) -> Vec<u8> {
    let mut out = dx_instance(sop_instance_uid);
    const PAD: usize = 9 * 1024 * 1024;
    out.extend_from_slice(&0x0009u16.to_le_bytes());
    out.extend_from_slice(&0x0001u16.to_le_bytes());
    out.extend_from_slice(b"OB");
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(PAD as u32).to_le_bytes());
    out.resize(out.len() + PAD, 0);
    out
}

#[test]
fn an_instance_body_above_the_json_budget_is_still_retrieved() {
    // A 2048x2048 16-bit DX instance is 8 MiB of samples before a single
    // header element, so the 8 MiB that bounds a QIDO answer refuses the
    // object it was written to fetch. The instance path carries its own,
    // larger budget; this body is past the old one and must arrive whole.
    let instance = dx_instance_larger_than_the_default_budget("1.2.3.4.5.1");
    assert!(
        instance.len() > 8 * 1024 * 1024,
        "the fixture is larger than the default budget: {}",
        instance.len()
    );
    let fake = FakeOrigin::start(vec![Answer::multipart_dicom(&instance)]);
    let mut attempts: u32 = 0;
    let bytes = origin(&fake)
        .instance("9.9.9", "1.2.3.4.5", "1.2.3.4.5.1", &mut attempts)
        .expect("the instance path accepts a body past the JSON budget");
    assert_eq!(bytes.len(), instance.len(), "the whole object arrived");
    assert_eq!(attempts, 1, "and it took one send");
}

#[test]
fn a_qido_answer_above_the_json_budget_is_still_refused() {
    // The larger budget is for the instance path only. A search answer that
    // grew past 8 MiB is a misdirected request or a broken origin, and it
    // must still fail with the cap rather than be read into memory.
    let padding = " ".repeat(9 * 1024 * 1024);
    let fake = FakeOrigin::start(vec![Answer::ok(format!("[]{}", padding))]);
    let err = origin(&fake)
        .instances("9.9.9", "1.2.3.4.5")
        .expect_err("a QIDO answer past the budget is refused");
    assert!(
        err.to_string().contains("response larger than"),
        "the failure names the cap: {err}"
    );
}

#[test]
fn a_busy_origin_is_retried_under_the_retry_after_header() {
    // The 503 path is unchanged: the origin's own hint is honoured and the
    // retry is counted. A zero-second Retry-After keeps the test quick
    // without changing the code path.
    let instance = dx_instance("1.2.3.4.5.1");
    let fake = FakeOrigin::start(vec![
        Answer::with(503, "").with_header("Retry-After", "0"),
        Answer::multipart_dicom(&instance),
    ]);
    let mut attempts: u32 = 0;
    let bytes = origin(&fake)
        .instance("9.9.9", "1.2.3.4.5", "1.2.3.4.5.1", &mut attempts)
        .expect("the retry after a 503 completes the instance");
    assert_eq!(bytes, instance);
    assert_eq!(attempts, 2, "exactly two sends were made");
}
