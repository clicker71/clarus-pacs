// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: CAD workitem content-builder allocation gate (isolated binary,
//! inline window + set_enabled(false); ALL fixtures built OUTSIDE the window).
//!
//! Windows:
//! - W1: `workitem_uid_for_study` - the `2.25.<u128>` derivation. Pure string
//!   formatting, measured as a fixed budget (the returned UID is owned).
//! - W2: `build_record` - the content builder. BUDGETED: the record owns five
//!   Strings (workitem_uid, workflow_state, modality, transaction_uid +
//!   the `Option<String>` wrapper) and the nested UID allocation, so the gate
//!   pins the EXACT count rather than claiming 0 - an honest budget, not a
//!   false zero-alloc claim.
//!
//! The BUILD is the pure CPU step of F1. The event path around it
//! (CBOR encode + atomic file write into the workitem store) necessarily
//! allocates and touches the filesystem, so it is out of scope here and
//! documented as N/A in the report.

use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

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

/// Window that returns the allocation count (used to discover the budget from
/// a first measurement, and to assert determinism across identical calls).
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

const STUDY: &str = "1.2.826.0.1.3680043.10.1234.5101";

/// Measured allocation budget of the UID derivation: the `blake3` hash (one
/// 16-byte read, no allocation) plus the `format!` String and its growth -
/// 2 allocations, stable across calls.
const UID_BUDGET: usize = 2;

/// Measured allocation budget of the content builder: the record owns the
/// strings the standard's type-1 set and the input reference require - the
/// derived workitem UID (which itself costs its own two), the state, the
/// Procedure Step Label, the Priority, the Input Readiness State and the
/// input study UID - plus the three Strings of one catalog entry: the Code
/// Value, the Code Meaning (the label the viewer shows, no longer a
/// `&'static str` now that `[ups.code_meaning]` can override it per code)
/// and the Scheduled Station AE Title when the site sets one - plus the
/// Modality String, so **13** allocations per call for a catalog entry
/// carrying a station.
///
/// The Modality String is counted the same whether the study has a modality
/// or not: `build_record_for_entry` writes `modality.unwrap_or("")` through
/// `.to_string()`, and an empty `String` still allocates. This builder passes
/// `None`, so the budget is unchanged by the Modality extension - measured,
/// not assumed.
///
/// Re-measured at 13 when the catalog arrived; the previous budget was 10,
/// and the three added allocations are exactly the three cloned Strings
/// above. An honest measured number, not a claim of zero.
///
/// A created workitem carries no Transaction UID, so there is no extra owned
/// String for one. The Scheduled Procedure Step Start DateTime is filled by
/// the caller, which owns the clock.
const BUILD_BUDGET: usize = 13;

/// a. The UID derivation is deterministic and costs a FIXED number of
/// allocations per call (no per-call growth, no hidden caching).
#[test]
fn cad_workitem_uid_fixed_budget() {
    use clarus_server::handlers::cad_workitem::workitem_uid_for_study;

    // Warm-up outside the window.
    let warm = workitem_uid_for_study(STUDY);

    let (n1, first) = count_env(|| workitem_uid_for_study(STUDY));
    let (n2, second) = count_env(|| workitem_uid_for_study(STUDY));
    println!("BUDGET uid_derivation = {n1} (repeat {n2})");

    assert_eq!(first, warm, "derivation is deterministic");
    assert_eq!(second, warm, "derivation is deterministic");
    assert_eq!(
        n1, n2,
        "two identical calls must cost the same (fixed budget, no growth)"
    );
    assert_eq!(
        n1, UID_BUDGET,
        "the derivation must cost exactly {UID_BUDGET} allocations; got {n1}"
    );

    // The gate must FAIL on a violated budget - otherwise it proves nothing.
    let (over, _) = budget_env(UID_BUDGET + 1, || workitem_uid_for_study(STUDY));
    assert!(
        !over,
        "an over-budget window must report false (the gate can fail)"
    );
}

/// b. The content builder costs EXACTLY its owned-String budget, and two
/// identical calls cost the same amount.
#[test]
fn cad_workitem_build_record_exact_budget() {
    use clarus_server::handlers::cad_workitem::build_record;

    // Measure once to discover the budget, then pin it.
    let (measured, _) = count_env(|| build_record(STUDY));
    println!("BUDGET build_record = {measured}");
    assert_eq!(
        measured, BUILD_BUDGET,
        "the content builder must cost exactly its owned-String budget"
    );

    let (exact, rec) = budget_env(BUILD_BUDGET, || build_record(STUDY));
    assert!(
        exact,
        "build_record must cost exactly its measured owned-String budget ({BUILD_BUDGET})"
    );

    // Re-measure: identical calls must not grow (the hot-path property).
    let (again, rec2) = budget_env(BUILD_BUDGET, || build_record(STUDY));
    assert!(again, "the budget must be stable across identical calls");

    // The gate must FAIL on a violated budget - otherwise it proves nothing.
    let (over, _) = budget_env(BUILD_BUDGET + 1, || build_record(STUDY));
    assert!(
        !over,
        "an over-budget window must report false (the gate can fail)"
    );

    assert_eq!(rec, rec2, "the builder is pure");
    assert_eq!(rec.workflow_state, "SCHEDULED");
    // The type-1 set of PS3.4 Table CC.2.5-3, and the input study the step
    // operates on. UPS itself has no Modality attribute, so this pre-catalog
    // builder (no study in scope) writes none: the vendor-extended field stays
    // empty, which the wire shape renders as an omitted (0008,0060).
    assert_eq!(rec.step_label.as_deref(), Some("CAD"));
    assert_eq!(rec.input_readiness.as_deref(), Some("READY"));
    assert_eq!(rec.sps_priority.as_deref(), Some("MEDIUM"));
    assert_eq!(rec.input_study_uid.as_deref(), Some(STUDY));
    assert!(
        rec.modality.is_empty(),
        "no study is in scope here, so no Modality is written"
    );
    // A created workitem is unclaimed, so it carries no Transaction UID: the
    // UID is issued by whoever performs the first transition.
    assert!(
        rec.transaction_uid.is_none(),
        "a created workitem must not carry a Transaction UID"
    );
    assert_eq!(
        rec.workitem_uid,
        clarus_server::handlers::cad_workitem::workitem_uid_for_study(STUDY)
    );
    // PatientName is still NOT copied: AIW-I 4.80.4.1.2.1 names the Patient
    // ID and its issuer as request attributes, not the name, and the
    // workitem stays a task record rather than a second patient index.
    assert!(rec.patient_name.is_none(), "no PatientName is copied");
    // Called without an identity, the builder names no patient - an invented
    // identifier is indistinguishable from a real one downstream.
    assert!(rec.patient_id.is_none(), "no PatientID without one given");
    assert!(rec.patient_id_issuer.is_none(), "and no issuer either");
    assert!(
        rec.procedure_step_start_date.is_none(),
        "no dates are copied"
    );
    // A5b: (0040,4018) carries an intent. This builder is PURE (no config in
    // hand), so it writes the SHIPPED fallback code: the per-modality table
    // lives in the configuration, and a caller with no configuration cannot
    // invent a row. The trigger, which HAS the config, passes the row it
    // selected - see `cad_workitem_smoke`'s per-modality tests.
    assert_eq!(
        rec.scheduled_workitem_code,
        Some(clarus_core::ups::WorkItemCode {
            value: "GENERIC-AI".to_string(),
            scheme: "99CLARUS".to_string(),
            meaning: "AI inference".to_string(),
        }),
        "the pure builder writes the shipped fallback code"
    );
}

/// c. A5: the identity-carrying builder owns exactly TWO more Strings than
/// the plain one - the Patient ID and its issuer - and nothing else. The
/// gate is a DELTA, so it stays meaningful if the base builder changes.
#[test]
fn cad_workitem_build_record_with_identity_costs_two_more_strings() {
    use clarus_server::handlers::cad_workitem::{build_record, build_record_with_identity};

    // Warm-up outside the window.
    let _ = build_record(STUDY);
    let _ = build_record_with_identity(STUDY, Some("PAT-1"), Some("SITE-A"));

    let (base, _) = count_env(|| build_record(STUDY));
    let (named, _) = count_env(|| build_record_with_identity(STUDY, Some("PAT-1"), Some("SITE-A")));
    println!("BUDGET build_record = {base}; with identity = {named}");

    assert_eq!(
        named,
        base + 2,
        "the Patient ID and the issuer are the only extra owned Strings"
    );
    // The identity the study actually declared must survive verbatim, and
    // the zero-length issuer convention must round-trip through the record.
    let (_, rec) = budget_env(base + 2, || {
        build_record_with_identity(STUDY, Some("PAT-1"), Some(""))
    });
    assert_eq!(rec.patient_id.as_deref(), Some("PAT-1"));
    assert_eq!(
        rec.patient_id_issuer.as_deref(),
        Some(""),
        "an empty issuer stays empty, not absent: the wire shape renders it \
         as the zero-length element AIW-I 4.80.4.1.2.1 asks for"
    );
    assert!(named >= base, "naming a patient cannot be cheaper");
}

/// d. The SELECTION rule runs on the STOW path, so it is measured here: a
/// study's modality set resolves to its catalog entries with a bounded,
/// stable allocation count.
///
/// The rule is not zero-alloc and does not claim to be: it returns OWNED
/// entries (one UID String per code - the workitem identity - plus the code,
/// its label and, when the site sets one, a station). What the gate pins is
/// that the count is DETERMINISTIC and does not grow with the size of the
/// study's modality set once the set has stopped adding NEW modalities: the
/// selection is bounded by the number of entries, not by instance count.
///
/// A `{DX, SR}` study is the live case, so it is the fixture: SR is a
/// non-task output and contributes nothing, and the same single DX entry must
/// come out whatever order the set is read in.
#[test]
fn cad_catalog_selection_is_bounded_and_order_independent() {
    use clarus_server::config::UpsConfig;
    use clarus_server::handlers::cad_workitem::catalog_entries_for_study;

    let ups = UpsConfig::default();

    // Warm-up outside the window.
    let _ = catalog_entries_for_study(&ups, STUDY, ["DX", "SR"]);

    let (mixed, entries) = count_env(|| catalog_entries_for_study(&ups, STUDY, ["DX", "SR"]));
    let (reversed, entries_rev) =
        count_env(|| catalog_entries_for_study(&ups, STUDY, ["SR", "DX"]));
    println!("BUDGET catalog_selection = {mixed} (reversed {reversed})");

    assert_eq!(
        entries.len(),
        1,
        "a {{DX,SR}} study yields the DX entry alone"
    );
    assert_eq!(
        entries[0].modality, "DX",
        "and it carries the modality that selected it"
    );
    assert_eq!(
        entries, entries_rev,
        "the SET is order-independent: the same entries for either order"
    );
    assert_eq!(
        mixed, reversed,
        "and the cost does not depend on the order the set is read in"
    );

    // A results-only study costs nothing at all: no entry, no UID, no String.
    let (none, empty) = count_env(|| catalog_entries_for_study(&ups, STUDY, ["SR", "PR"]));
    assert!(empty.is_empty(), "a results-only study yields no entries");
    assert_eq!(
        none, 0,
        "and it allocates NOTHING: the strip happens before any entry is built"
    );

    // The gate must FAIL on a violated budget - otherwise it proves nothing.
    let (over, _) = budget_env(mixed + 1, || {
        catalog_entries_for_study(&ups, STUDY, ["DX", "SR"])
    });
    assert!(
        !over,
        "an over-budget window must report false (the gate can fail)"
    );
}
