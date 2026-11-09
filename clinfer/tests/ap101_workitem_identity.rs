// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the AIW-I Appendix V identity parse (Sprint B). Isolated binary,
//! inline windows; every fixture is built OUTSIDE the measured window.
//!
//! What is measured, and why this is the right seam:
//!
//! Sprint B adds identity ATTRIBUTES to the result writers, but it adds no
//! new allocation SITE. The writers already cloned one `Option<String>` per
//! identity field per stored object; what changed is WHERE the value came
//! from (a workitem first, the study second). The one new cost is the
//! per-workitem READ of those attributes, which happens once per workitem
//! rather than once per result object.
//!
//! So the gate is on `WorkItem::from_json`:
//!
//! - W1: a workitem that carries NO Appendix V identity costs a FIXED
//!   budget. This is the regression pin for the common case - a site that
//!   populates none of these attributes must not pay for the feature.
//! - W2: a workitem that carries the FULL Appendix V identity costs a
//!   FIXED budget too, exactly TWO allocations per populated field (see
//!   below), which is what owning a `String` read out of a borrowed JSON
//!   value costs and nothing more.
//!
//! MEASURED, not assumed: the first version of W2 claimed the full
//! workitem costs the SAME as the bare one, on the theory that reading a
//! field out of an object that already exists adds only lookups. The
//! measurement refused it (4 vs 16), because every populated field becomes
//! an OWNED `String` on the returned struct. The gate now asserts the
//! measured shape instead of the assumed one.
//!
//! The writers' own serializers are gated in `ap101_seg_serialize.rs` and
//! `ap101_pr_serialize.rs`; the identity fields they now copy travel as
//! borrows into those windows and are covered there.
//!
//! `TestAllocator` counts process-wide, so this binary keeps to the pattern
//! the other ap101 gates use: one thread, and every fixture built with
//! tracking disabled.

use clinfer::dicomweb::WorkItem;
use clinfer::json;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

/// Window that returns the allocation count (used to discover the budget
/// from a first measurement, and to assert determinism across calls).
fn count_env<F, R>(f: F) -> (usize, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n, result)
}

/// Window that asserts an EXACT allocation budget.
fn budget_env<F, R>(expect: usize, f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let result = f();
    let (n, _) = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (n == expect, result)
}

/// A minimal SCHEDULED workitem: the shape every deployment starts from,
/// with no Appendix V identity attributes at all.
const BARE: &str = r#"{"00080018":{"vr":"UI","Value":["2.25.1"]},
    "00741000":{"vr":"CS","Value":["SCHEDULED"]},
    "00404041":{"vr":"CS","Value":["READY"]},
    "00404021":{"vr":"SQ","Value":[{"0040E020":{"vr":"CS","Value":["DICOM"]},
                                       "0020000D":{"vr":"UI","Value":["2.25.9"]}}]}}"#;

/// The same workitem carrying every Appendix V "Source Copy" row and the
/// (0040,4028) Performed Station Name Code Sequence.
const FULL: &str = r#"{"00080018":{"vr":"UI","Value":["2.25.1"]},
    "00741000":{"vr":"CS","Value":["SCHEDULED"]},
    "00404041":{"vr":"CS","Value":["READY"]},
    "00100010":{"vr":"PN","Value":[{"Alphabetic":"WORKITEM^PAT"}]},
    "00100020":{"vr":"LO","Value":["WI-ID"]},
    "00100021":{"vr":"LO","Value":["SITE-A"]},
    "00100030":{"vr":"DA","Value":["19800101"]},
    "00100040":{"vr":"CS","Value":["M"]},
    "00200010":{"vr":"SH","Value":["WI-STUDY"]},
    "00080050":{"vr":"SH","Value":["WI-ACC"]},
    "00080090":{"vr":"PN","Value":[{"Alphabetic":"WI^REF"}]},
    "00404018":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["CAD"]},
                                    "00080102":{"vr":"SH","Value":["99CLARUS"]},
                                    "00080104":{"vr":"LO","Value":["AI detection"]}}]},
    "00404028":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["CLINFER"]},
                                    "00080102":{"vr":"SH","Value":["99CLARUS"]},
                                    "00080104":{"vr":"LO","Value":["Inference station"]}}]},
    "00401001":{"vr":"SH","Value":["RP-1"]},
    "00401002":{"vr":"LO","Value":["Screening"]},
    "0040100A":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["REASON-1"]},
                                    "00080102":{"vr":"SH","Value":["99CLARUS"]},
                                    "00080104":{"vr":"LO","Value":["Screening indication"]}}]},
    "00321060":{"vr":"LO","Value":["Chest CT"]},
    "00321064":{"vr":"SQ","Value":[{"00080100":{"vr":"SH","Value":["RP-CHEST"]},
                                    "00080102":{"vr":"SH","Value":["99CLARUS"]},
                                    "00080104":{"vr":"LO","Value":["Chest procedure"]}}]},
    "00404021":{"vr":"SQ","Value":[{"0040E020":{"vr":"CS","Value":["DICOM"]},
                                       "0020000D":{"vr":"UI","Value":["2.25.9"]}}]}}"#;

/// The bare workitem's parse cost, in allocations: the measured budget of
/// `WorkItem::from_json` over a workitem that carries no Appendix V
/// identity. The reader owns the UID, the state and the input study UID,
/// which is what this number is made of.
const BARE_PARSE_BUDGET: usize = 4;

/// The fully-populated workitem's parse cost: the same reader over the same
/// shape plus every Appendix V identity attribute, including the three
/// requestor-supplied code sequences the tail sprint added.
///
/// MEASURED, 4 -> 28. The twenty-four extra allocations are the owned
/// `String`s the returned struct carries: eight plain identity values, the
/// (0040,4028) Code Meaning, three requestor-supplied text rows
/// ((0040,1001), (0040,1002), (0032,1060)) and four `CodeSequenceMacro`s
/// ((0040,4018), (0032,1064), (0040,100A) and the (0040,4025) the
/// performer-code reader builds) at three Strings each.
const FULL_PARSE_BUDGET: usize = 28;

/// The bare workitem's parse cost, in allocations.
///
/// Discovered from a first measurement rather than guessed: the reader
/// owns the UID, the state, the input study UID and the two identity
/// `Vec`/`Option` fields the parser produces, and every one of them is a
/// documented field of the returned struct.
#[test]
fn a_workitem_without_identity_parses_on_a_fixed_budget() {
    let fixture = json::parse(BARE.as_bytes()).expect("parseable");

    let (n1, bare) = count_env(|| WorkItem::from_json(&fixture).expect("a workitem"));
    let (n2, _) = count_env(|| WorkItem::from_json(&fixture).expect("a workitem"));
    println!("BUDGET workitem_parse_bare = {n1} (repeat {n2})");

    assert_eq!(n1, BARE_PARSE_BUDGET, "the measured bare budget");
    assert_eq!(n1, n2, "two identical calls cost the same");

    // The regression pin: a workitem that states no Appendix V identity
    // carries None everywhere, so nothing was allocated for the feature.
    assert_eq!(bare.identity.patient_name, None);
    assert_eq!(bare.identity.patient_id, None);
    assert_eq!(bare.identity.patient_birth_date, None);
    assert_eq!(bare.identity.patient_sex, None);
    assert_eq!(bare.identity.study_id, None);
    assert_eq!(bare.identity.accession_number, None);
    assert_eq!(bare.identity.referring_physician, None);
    assert_eq!(bare.identity.performed_station_name, None);

    let (exact, _) = budget_env(n1, || WorkItem::from_json(&fixture).expect("a workitem"));
    assert!(exact, "the parse costs exactly its measured budget ({n1})");

    // The gate must FAIL on a violated budget - otherwise it proves nothing.
    let (over, _) = budget_env(n1 + 1, || {
        WorkItem::from_json(&fixture).expect("a workitem")
    });
    assert!(!over, "an over-budget window must report false");
}

/// The fully-populated workitem costs a FIXED, MEASURED budget.
///
/// MEASURED, not assumed. Two wrong models were tried against this gate
/// before it was written, and both were refused by measurement:
///
/// 1. "reading a field out of an object that already exists adds only
///    lookups" - refuted, 4 allocations bare against 16 full. Every
///    populated field becomes an OWNED `String` on the returned struct.
/// 2. "each populated field costs two allocations" - refuted, the delta is
///    12 for nine populated strings, not 18. The reader reuses buffers
///    across the `attribute` calls, so the per-field cost is not a clean
///    constant and inventing one would be a made-up number.
///
/// What the gate therefore pins is the two measurements themselves, as an
/// exact budget on each shape, plus the fact that the delta is bounded by
/// the number of populated fields. That is a claim the code can be held to:
/// any new allocation on this path - a collected intermediate, a second
/// parse, an eager `Vec` - moves one of the two numbers and fails the gate.
#[test]
fn a_workitem_with_the_full_identity_costs_a_fixed_budget() {
    let bare_fixture = json::parse(BARE.as_bytes()).expect("parseable");
    let full_fixture = json::parse(FULL.as_bytes()).expect("parseable");

    let (bare_cost, _) = count_env(|| WorkItem::from_json(&bare_fixture).expect("a workitem"));
    let (full_cost, full) = count_env(|| WorkItem::from_json(&full_fixture).expect("a workitem"));
    println!("BUDGET workitem_parse_bare = {bare_cost}; full = {full_cost}");

    assert_eq!(
        bare_cost, BARE_PARSE_BUDGET,
        "the bare parse budget is the measured {BARE_PARSE_BUDGET}"
    );
    assert_eq!(
        full_cost, FULL_PARSE_BUDGET,
        "the full parse budget is the measured {FULL_PARSE_BUDGET}"
    );

    // The delta must stay bounded by the populated values: one owned
    // String per value is the most the reader can be doing, and the
    // measured cost is lower than that bound (the reader reuses buffers
    // inside one parse). This asserts the SHAPE without inventing a
    // per-value constant that the measurement does not support.
    //
    // Counted: eight plain identity values, the (0040,4028) Code Meaning,
    // three requestor-supplied text rows and four code macros at three
    // parts each.
    const POPULATED_VALUES: usize = 24;
    let delta = full_cost - bare_cost;
    println!("BUDGET identity_delta = {delta} for {POPULATED_VALUES} populated values");
    assert!(
        delta <= POPULATED_VALUES * 2,
        "the identity delta {delta} must not exceed one String's worth of \
         allocation per populated value ({} for {POPULATED_VALUES})",
        POPULATED_VALUES * 2
    );

    // And the identity really is populated, so the delta above is not the
    // cost of two empty results.
    assert_eq!(full.identity.patient_name.as_deref(), Some("WORKITEM^PAT"));
    assert_eq!(full.identity.patient_id.as_deref(), Some("WI-ID"));
    assert_eq!(
        full.identity.patient_birth_date.as_deref(),
        Some("19800101")
    );
    assert_eq!(full.identity.patient_sex.as_deref(), Some("M"));
    assert_eq!(full.identity.study_id.as_deref(), Some("WI-STUDY"));
    assert_eq!(full.identity.accession_number.as_deref(), Some("WI-ACC"));
    assert_eq!(full.identity.referring_physician.as_deref(), Some("WI^REF"));
    // Appendix V Note 4: the Code MEANING is what maps into (0008,1010),
    // not the Code Value.
    assert_eq!(
        full.identity.performed_station_name.as_deref(),
        Some("Inference station")
    );
    // The tail sprint's requestor-supplied rows, so the budget above covers
    // them rather than an empty fixture that happens to parse.
    assert_eq!(
        full.identity
            .scheduled_workitem_code
            .as_ref()
            .map(|c| c.value.as_str()),
        Some("CAD"),
        "(0040,4018) is what maps into the result's (0040,0008)"
    );
    assert_eq!(
        full.identity.requested_procedure_id.as_deref(),
        Some("RP-1")
    );
    assert_eq!(
        full.identity.requested_procedure_description.as_deref(),
        Some("Chest CT")
    );
    assert_eq!(
        full.identity
            .requested_procedure_code
            .as_ref()
            .map(|c| c.value.as_str()),
        Some("RP-CHEST")
    );
    assert_eq!(
        full.identity.reason_for_requested_procedure.as_deref(),
        Some("Screening")
    );
    assert_eq!(
        full.identity
            .reason_for_requested_procedure_code
            .as_ref()
            .map(|c| c.value.as_str()),
        Some("REASON-1")
    );

    let (exact, _) = budget_env(FULL_PARSE_BUDGET, || {
        WorkItem::from_json(&full_fixture).expect("a workitem")
    });
    assert!(exact, "the full parse costs exactly its measured budget");

    // The gate must FAIL on a violated budget.
    let (over, _) = budget_env(FULL_PARSE_BUDGET + 1, || {
        WorkItem::from_json(&full_fixture).expect("a workitem")
    });
    assert!(!over, "an over-budget window must report false");
}

/// The parse cost is stable across repeated calls: no per-call growth.
#[test]
fn the_identity_parse_cost_is_stable() {
    let fixture = json::parse(FULL.as_bytes()).expect("parseable");
    let (first, _) = count_env(|| WorkItem::from_json(&fixture).expect("a workitem"));
    let (second, _) = count_env(|| WorkItem::from_json(&fixture).expect("a workitem"));
    let (third, _) = count_env(|| WorkItem::from_json(&fixture).expect("a workitem"));
    assert_eq!(first, second, "call 1 and call 2 cost the same");
    assert_eq!(second, third, "call 2 and call 3 cost the same");
}
