// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Generic CAD workitem creation after a durable STOW.
//!
//! An origin server MAY create workitems (PS3.18 Section 11.1: "User agents
//! and origin servers can create, retrieve, update, search for, and change
//! the state of Workitems"); "Computer Aided Detection" is the standard's own
//! example workitem type (11.1.1.1). This module is that capability and
//! nothing more specific: it knows a study UID and the UPS record shape, not
//! who consumes the workitem or why.
//!
//! # Contract
//! - ONE workitem per study, forever. The UID is derived deterministically
//!   from the StudyInstanceUID, so a re-STOW of the same study collides with
//!   the existing record and the duplicate check turns the second attempt
//!   into a no-op. Idempotency is the UID, not a lookup table.
//! - Fire-and-forget. A creation failure is logged and counted; it NEVER
//!   fails the STOW and never changes its response bytes.
//! - Strictly after the durable sync point. The caller invokes this after
//!   `sync_batch_manifests`, so a workitem never references a study whose
//!   manifest is not yet on stable storage.
//!
//! The record is CBOR-encoded into the SAME `workitems/` store as every other
//! workitem, so it is visible to the existing UPS-RS search, update and event
//! routes with no changes to them.

#![forbid(unsafe_code)]

use std::sync::atomic::Ordering;

use clarus_core::ups::{StoreError, WorkItemRecord, WorkItemStore};

use crate::state::AppState;
/// Root of the derived workitem UID (`2.25` = the ISO/ITU-T joint root used
/// by the UUID-derived identifiers elsewhere in the product).
const CAD_UID_ROOT: &str = "2.25.";

/// `ProcedureStepState` value for an offered (unclaimed) workitem.
pub const STATE_SCHEDULED: &str = "SCHEDULED";

/// `ProcedureStepLabel` for a CAD workitem.
pub const CAD_STEP_LABEL: &str = "CAD";

/// (0040,4018) Scheduled Workitem Code Sequence, as the private code this
/// deployment offers CAD work under.
///
/// The SCHEME is a constant; the VALUES are the deployment's, from
/// `[ups.workitem_code]` (see [`crate::config::UpsConfig::code_for_modality`]).
/// AIW-I 50.4.1.2 mandates no code set: the Task Requester, the Task Manager
/// and the Task Performer agree on codes per deployment, so the table is
/// configuration and only the scheme is ours. The scheme is deliberately not
/// derived from `[server] modifying_system`: that identity is per-INSTANCE (it
/// carries the bound port, so two servers on one host differ), while a
/// workitem code has to be the SAME on every instance of the deployment for a
/// performer's rules to mean anything.
///
/// The MEANING of a code is configuration too, but a caller that has no
/// configuration in hand (the allocation gate's pure builder) still has to
/// write something: it writes these defaults, which are the shipped values.
const CAD_CODE_MEANING: &str = "AI inference";
/// The code value such a caller writes: the shipped fallback.
const CAD_CODE_VALUE_FALLBACK: &str = "GENERIC-AI";

/// `ScheduledProcedureStepPriority` for a generated CAD workitem.
///
/// A study that has just been stored is offered at normal priority; the
/// attribute is type 1 at N-CREATE (PS3.4 Table CC.2.5-3), so a value has to
/// be chosen.
const CAD_PRIORITY: &str = "MEDIUM";

/// `InputReadinessState` for a workitem whose input is already stored.
///
/// A study is READY when nothing has been stored for it for the readiness
/// debounce period; that flip is what this module's caller does once the study
/// has settled (see `handlers/readiness.rs`).
pub const CAD_READINESS: &str = super::readiness::READINESS_READY;

/// Derive the deterministic workitem UID for a study.
///
/// `2.25.<decimal(first 16 bytes of blake3(StudyInstanceUID))>`. The digest is
/// read big-endian into a `u128`, so the decimal form is stable across
/// platforms and runs. Digits and dots only, well inside the PS3.5 64-char UID
/// bound (the decimal form of a `u128` is at most 39 digits, 4 + 39 = 43).
pub fn workitem_uid_for_study(study_instance_uid: &str) -> String {
    let digest = blake3::hash(study_instance_uid.as_bytes());
    let head: [u8; 16] = digest.as_bytes()[..16].try_into().expect("16 bytes");
    format!("{CAD_UID_ROOT}{}", u128::from_be_bytes(head))
}

/// The UID of ONE catalog entry's workitem for one study.
///
/// `2.25.<decimal(first 16 bytes of blake3(StudyUID + "/" + code))>`, the same
/// construction as [`workitem_uid_for_study`] with the code value folded in.
/// The digest is read big-endian into a `u128`, so the decimal form is stable
/// across platforms and runs.
///
/// The `/` delimiter is what makes the input unambiguous: a study UID can
/// never contain `/` (PS3.5 Table 6.2-1 restricts UI to digits and dots), so
/// two different (study, code) pairs can never hash the same string.
///
/// Idempotency is the point: re-STOWing a study recomputes the same UIDs, and
/// the duplicate check turns the second attempt into a no-op - so a catalog
/// with N entries yields N stable workitems, not N per STOW. The CODE is part
/// of the key rather than the table's position, so re-ordering the table in
/// `clarus.conf` does not renumber anything.
#[must_use]
pub fn workitem_uid_for_study_code(study_instance_uid: &str, code_value: &str) -> String {
    let digest = blake3::hash(format!("{study_instance_uid}/{code_value}").as_bytes());
    let head: [u8; 16] = digest.as_bytes()[..16].try_into().expect("16 bytes");
    format!("{CAD_UID_ROOT}{}", u128::from_be_bytes(head))
}

/// Build the CAD workitem record for `study_instance_uid`.
///
/// Pure CPU: no filesystem, no clock, no allocation beyond the returned record
/// itself. Split out from [`create_for_study`] so the ap101 allocation gate can
/// measure exactly this step in isolation. The caller fills the timestamps.
///
/// The shape is the smallest honest study-scoped request: an offered
/// (SCHEDULED) step labelled `CAD`, whose input is the stored study. The type-1
/// attributes of PS3.4 Table CC.2.5-3 are all present, and the study the step
/// operates on is reachable through `input_study_uid`, which the wire shape
/// renders as (0040,4021) Input Information Sequence.
///
/// There is no Modality in the UPS IOD: PS3.4 Table CC.2.5-3 has no Modality
/// row, and the UPS IOD modules in PS3.3 C.30 have none either. The record
/// nonetheless carries the input study's modality, written by the caller that
/// knows it, because the DIMSE MWL service requires Modality as a Return key
/// Type 1 - see [`build_record_for_entry`].
///
/// The workitem carries no Transaction UID. PS3.18 11.6 issues that UID to
/// whoever performs the SCHEDULED -> IN PROGRESS transition, so minting one
/// here would make every claimant's own UID disagree with the stored one and
/// the optimistic lock would refuse a workitem nobody had touched.
pub fn build_record(study_instance_uid: &str) -> WorkItemRecord {
    build_record_with_identity(study_instance_uid, None, None)
}

/// The same record, with the patient identity the task names.
///
/// AIW-I 4.80.4.1.2.1 requires an Issuer of Patient ID for every Patient ID
/// the request carries, and 4.80.4.1.2.4 requires the AIW-I task fields
/// (Scheduled Workitem Code Sequence among them). `patient_id` comes from the
/// study the workitem operates on - the requester knows it because it has
/// just stored that study - and `issuer` is `(0010,0021)` when the stored
/// study carries one.
///
/// A study with no Patient ID still gets NO Patient ID on the workitem: an
/// invented identifier would be worse than an absent one, because a performer
/// downstream could not tell it apart from a real one. What the profile asks
/// for is the issuer of every ID that IS present.
///
/// `input_readiness` is a parameter rather than a constant because a workitem
/// created by a STOW is offered INCOMPLETE and promoted later (see
/// `handlers/readiness.rs`), while a caller that knows the study is whole may
/// offer it READY outright.
#[must_use]
pub fn build_record_with_identity(
    study_instance_uid: &str,
    patient_id: Option<&str>,
    issuer: Option<&str>,
) -> WorkItemRecord {
    build_record_readiness(
        study_instance_uid,
        patient_id,
        issuer,
        CAD_READINESS,
        CAD_CODE_VALUE_FALLBACK,
    )
}

/// The same record, deciding the Input Readiness State explicitly.
///
/// `code_meaning` is written beside the code the caller selected; the value
/// and the scheme come from `[ups.workitem_code]` and
/// [`crate::config::UpsConfig::CODE_SCHEME`].
#[must_use]
pub fn build_record_readiness(
    study_instance_uid: &str,
    patient_id: Option<&str>,
    issuer: Option<&str>,
    input_readiness: &str,
    code_value: &str,
) -> WorkItemRecord {
    build_record_for_entry(
        study_instance_uid,
        patient_id,
        issuer,
        input_readiness,
        &CatalogEntry {
            uid: workitem_uid_for_study(study_instance_uid),
            code_value: code_value.to_string(),
            meaning: CAD_CODE_MEANING.to_string(),
            station_aet: None,
            modality: String::new(),
        },
    )
}

/// One catalog entry's contribution to a workitem.
///
/// Everything that differs between two workitems of the SAME study lives
/// here: the UID (which folds in the code), the code value and its label, and
/// the station the workitem is addressed to. Keeping it a named struct means
/// the mint and the readiness paths pass the same four things around instead
/// of four positional arguments that a reader has to match up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The workitem UID for this (study, code) pair.
    pub uid: String,
    /// The Code Value of the scheduled workitem code.
    pub code_value: String,
    /// The CodeMeaning: the label the viewer's dropdown shows.
    pub meaning: String,
    /// (0040,0001) Scheduled Station AE Title, or `None` to write none.
    pub station_aet: Option<String>,
    /// The study modality that SELECTED this entry.
    ///
    /// Carried per entry rather than once per study because one study yields
    /// several entries and each is selected by a different modality: the DX
    /// row's workitem says `DX`, even when the study also holds SR. The DIMSE
    /// MWL service reads this as a Return key Type 1 (PS3.4 Table K.6-1), so
    /// it has to be the modality the task is ABOUT, not an arbitrary member of
    /// the study's set.
    pub modality: String,
}

/// The catalog entries a study yields, resolved from the site's config.
///
/// THE selection rule for both the mint and the readiness paths - see
/// [`crate::config::UpsConfig::catalog_for_modalities`], which is where the
/// decision itself lives so the two callers cannot drift apart.
///
/// `modalities` is the STUDY's distinct modality set, not one instance's: a
/// study a performer reads images from and writes a report into is `{DX, SR}`,
/// and no single modality describes it. A single-modality study is a set of one.
#[must_use]
pub fn catalog_entries_for_study<I>(
    ups: &crate::config::UpsConfig,
    study_instance_uid: &str,
    modalities: I,
) -> Vec<CatalogEntry>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    ups.catalog_for_modalities(modalities)
        .into_iter()
        .map(|(modality, code)| CatalogEntry {
            uid: workitem_uid_for_study_code(study_instance_uid, code),
            code_value: code.to_string(),
            meaning: ups.meaning_for_code(code).to_string(),
            // Every entry this function returns is AUTOMATIC: a parked (manual)
            // code never becomes an entry (`catalog_for_modalities` drops it),
            // so the address of an entry is always the AI station.
            station_aet: ups.ai_station_aet.as_deref().map(str::to_string),
            modality,
        })
        .collect()
}

/// Build one study's workitem for one catalog entry.
///
/// The body is [`build_record_readiness`]'s, with the catalog entry supplying
/// the four things that vary. Split out rather than folded in because the
/// ap101 allocation gate measures this step in isolation for a whole catalog:
/// the UID, the station and the label are the only inputs that differ between
/// two calls for the same study.
/// `modality` is the entry's TRIGGER modality: (0008,0060) of the study
/// modality that selected this catalog entry, or the empty string when the
/// entry is the fallback (a generic task belongs to no particular modality).
/// It is written into the record because the DIMSE MWL service requires
/// Modality as a Return key Type 1 (PS3.4 Table K.6-1), while the UPS IOD the
/// record otherwise follows defines no such attribute (Table CC.2.5-3). A
/// generic entry stays absent: the record holds an empty string and the wire
/// shape omits the element rather than inventing one.
#[must_use]
pub fn build_record_for_entry(
    study_instance_uid: &str,
    patient_id: Option<&str>,
    issuer: Option<&str>,
    input_readiness: &str,
    entry: &CatalogEntry,
) -> WorkItemRecord {
    WorkItemRecord {
        id: None,
        workitem_uid: entry.uid.clone(),
        workflow_state: STATE_SCHEDULED.to_string(),
        modality: entry.modality.clone(),
        station_aet: entry.station_aet.clone(),
        station_name: None,
        procedure_step_start_date: None,
        procedure_step_start_time: None,
        patient_name: None,
        patient_id: patient_id.map(str::to_string),
        // (0010,0021) is Type 1C in the request: it is required when the
        // Patient ID is present, and a zero-length value is the standard's
        // way of saying "present, but the site declares no issuer". The
        // record keeps `None` for that case and the wire shape renders the
        // zero-length element, so an absent issuer and an empty one are the
        // same statement instead of two different ones.
        patient_id_issuer: issuer.map(str::to_string),
        accession_number: None,
        requesting_physician: None,
        sps_priority: Some(CAD_PRIORITY.to_string()),
        transaction_uid: None,
        created_at: None,
        updated_at: None,
        step_label: Some(CAD_STEP_LABEL.to_string()),
        // (0074,1202) is Type 2/1 and the SCP fills it when the creator gives
        // none; the CAD path names its step, so the same value groups the
        // worklist.
        worklist_label: Some(CAD_STEP_LABEL.to_string()),
        // (0020,000D) at dataset root: this path DOES know the study the
        // outputs belong to - it is the study the catalog entry came from.
        output_study_uid: Some(study_instance_uid.to_string()),
        input_readiness: Some(input_readiness.to_string()),
        // Filled by the caller, which is the only side with a clock.
        sps_start_datetime: None,
        input_study_uid: Some(study_instance_uid.to_string()),
        reason_for_cancellation: None,
        cancellation_datetime: None,
        scheduled_workitem_code: Some(clarus_core::ups::WorkItemCode {
            value: entry.code_value.clone(),
            scheme: crate::config::UpsConfig::CODE_SCHEME.to_string(),
            meaning: entry.meaning.clone(),
        }),
        // The performed report and the output list are UPDATE-time
        // attributes (PS3.18 11.6): a freshly scheduled step has produced
        // nothing and reported nothing, so all three start absent.
        output_information: None,
        performed_workitem_code: None,
        performed_station_name_code: None,
        // The catalog workitem is derived from a stored study, which carries
        // no Referenced Request Sequence, so it names no requested procedure.
        requested_procedure_id: None,
        // No performer has reported yet, so neither datetime exists.
        performed_start_datetime: None,
        performed_end_datetime: None,
        // A minted workitem carries no client-supplied processing parameters
        // and no Referenced Request Sequence, so neither attribute exists:
        // the first is only ever what a Create or Update sent, and the reason
        // lives inside a sequence this path never writes.
        scheduled_processing_parameters: None,
        reason_for_requested_procedure: None,
        // No full Input Information Sequence nor performed-procedure sequence
        // is stored here: the minted workitem carries only the typed study UID
        // projection and no performer report, so both opaque fields are None.
        input_information: None,
        performed_procedure_information: None,
    }
}

/// Post-STOW entry point: one workitem per DISTINCT study in the batch.
///
/// Called by both STOW paths AFTER `sync_batch_manifests`, so the returned
/// studies are already durable. When `[ups] task_producer` is off (the
/// default) this is a single boolean test and the request path does no work at
/// all - this process is then not the Task Producer, and some other standard
/// actor (a modality, a RIS, an external orchestrator) is.
///
/// The workitem is offered INCOMPLETE and NOT announced: the study is almost
/// always still being stored at this point (per-instance ingest is legal and
/// unbounded), and announcing now is what let a live performer claim a study
/// after 2 of 53 instances. `handlers/readiness.rs` promotes it to READY and
/// publishes the offer once the study has been quiet for the debounce period.
///
/// This applies to EVERY study, not only multi-instance ones. A single-instance
/// study waits for the same quiet period even though it is complete on
/// arrival: a branch that skipped the wait for it would be a second path with
/// its own timing assumptions, and "partial equals complete" is exactly the
/// coincidence the debounce exists to stop relying on.
pub fn after_durable_stow(state: &AppState, batch: &super::stow_pure::ManifestBatch) {
    if !state.config.ups.task_producer {
        return;
    }
    for study_uid in batch.study_uids() {
        // The patient identity of the task comes from the study the batch has
        // just made durable (AIW-I 4.80.4.1.2.1 requires the request to state
        // the Patient ID and its issuer). It is read here rather than in the
        // request path so the workitem carries what was actually committed.
        let identity = batch.patient_identity(study_uid);
        // The INTENT of the task: the code is chosen from the site's table by
        // the STUDY's modality SET, not by one instance's modality (AIW-I
        // 4.80.4.1.2.1: the request states the Scheduled Workitem Code
        // Sequence). A study a performer writes a report into is `{DX, SR}`,
        // and the images' task has to be offered even when the report was
        // stored first.
        //
        // A candidate the table does not name takes the configured fallback -
        // but only when the site opted in; and a modality that is an AI OUTPUT
        // never mints at all. Both cases are LOGGED, because an operator has
        // to be able to see why a stored study produced no offer rather than
        // discover it from the worklist.
        let modalities = batch.modalities(study_uid);
        let entries = catalog_entries_for_study(
            &state.config.ups,
            study_uid,
            modalities.iter().map(String::as_str),
        );
        if entries.is_empty() {
            log_empty_selection(state, study_uid, &modalities);
        }
        create_for_study_with_identity(state, study_uid, identity, &entries);
    }
}

/// Say WHY a study produced no workitem, in the operator's terms.
///
/// Two different situations produce an empty selection, and an operator can
/// only act on the one they are actually in: a study that is all outputs is
/// expected (nothing to do), while a study with an unlisted input is a table
/// that is missing a row. A single "no workitem" line would hide that.
fn log_empty_selection(state: &AppState, study_uid: &str, modalities: &[String]) {
    let ups = &state.config.ups;
    // A study whose every modality is an AI output: the report came back into
    // the study it came from. This is the designed case, not a gap.
    if modalities.iter().all(|m| ups.is_non_task_modality(m)) {
        crate::clog_info!(
            study_instance_uid = %study_uid,
            modalities = %modalities.join(","),
            "CAD workitem: modality is a non-task output, no workitem"
        );
        return;
    }
    // An unlisted input with no fallback configured: the site has not said
    // what to do, and the honest answer is nothing plus this line.
    if ups.workitem_code_fallback.is_none() {
        crate::clog_warn!(
            study_instance_uid = %study_uid,
            modalities = %modalities.join(","),
            "CAD workitem: unlisted modality, no workitem (fallback not set)"
        );
    }
}

/// Write a READY workitem back to INCOMPLETE because its study grew.
///
/// Called on the re-STOW path, where the workitem already exists and a new
/// instance has just become durable. The readiness debounce (`arm`) only moves
/// a deadline; it does not revise what the record says, so without this a
/// study declared READY stays READY while instances keep arriving - and a
/// performer woken by the READY announcement would claim an input the sender
/// has since extended.
///
/// Deliberately SILENT: nothing is published on this flip. An INCOMPLETE
/// regression is not an offer, and the performer's own guard (AIW-I
/// 4.82.4.1.1) already refuses a claim while the input is INCOMPLETE - which
/// is the standard's arrangement, not a gap. The later promotion publishes
/// through the one existing path, exactly as it does for a fresh study.
///
/// A workitem that is not SCHEDULED is left ALONE, the same rule `promote`
/// follows: it has been claimed, cancelled or completed, and rewriting its
/// readiness would change what that claim was about after the fact.
///
/// `pub` rather than private so the ap101 allocation gate in
/// `tests/ap101_readiness.rs` can call it: an integration test is a separate
/// crate and cannot reach a `pub(crate)` item. That is the same visibility the
/// other gate-measured builders in this module carry
/// ([`workitem_uid_for_study`], [`build_record`]).
pub fn regress_readiness_on_new_instances(
    store: &WorkItemStore,
    existing: clarus_core::ups::WorkItemRecord,
) {
    if existing.workflow_state != STATE_SCHEDULED {
        return;
    }
    if existing.input_readiness.as_deref() != Some(super::readiness::READINESS_READY) {
        // Already INCOMPLETE (the ordinary mid-upload case) or unset: nothing
        // to revise, and re-writing the same value would only churn the
        // record's `updated_at`.
        return;
    }

    let workitem_uid = existing.workitem_uid.clone();
    let mut record = existing;
    record.input_readiness = Some(super::readiness::READINESS_INCOMPLETE.to_string());
    record.updated_at = Some(super::ups_pure::now_stamp());
    // `None` for the expected transaction UID, the same shape `promote` uses:
    // this is a readiness revision, not a state transition, and the workitem
    // is still SCHEDULED, which is what no-performer-held means.
    if let Err(error) = store.cas_update(&record, None) {
        crate::clog_warn!(
            workitem_uid = %workitem_uid,
            error = %error,
            "CAD workitem: cannot write the INCOMPLETE regression after a re-STOW"
        );
    } else {
        crate::clog_info!(
            workitem_uid = %workitem_uid,
            "CAD workitem input is INCOMPLETE again; the study received more instances"
        );
    }
}

/// Create the CAD workitem for one study, fire-and-forget.
///
/// Returns `true` when the study now has a workitem (freshly created OR
/// already present - both are success from the caller's point of view) and
/// `false` on a store failure.
pub fn create_for_study(state: &AppState, study_instance_uid: &str) -> bool {
    // No modality is known on this path, so the catalog resolves to nothing
    // unless the site configured an explicit code for this caller. The empty
    // entry list means no workitem - which is the honest answer for a path
    // that has not read the study, and the same shape as an unlisted modality
    // with no fallback.
    let entries = catalog_entries_for_study(
        &state.config.ups,
        study_instance_uid,
        std::iter::empty::<&str>(),
    );
    create_for_study_with_identity(state, study_instance_uid, None, &entries)
}

/// The same creation, naming the patient the task is about.
///
/// `identity` is `(Patient ID, Issuer of Patient ID)` as the stored study
/// declares them; `None` means the requester does not know one, and the
/// workitem then carries no Patient ID - which is the honest shape, because
/// an invented identifier is indistinguishable from a real one downstream.
///
/// Returns `true` when EVERY catalog entry now has a workitem (freshly
/// created OR already present - both are success from the caller's point of
/// view) and `false` if any of them failed to reach the store.
pub fn create_for_study_with_identity(
    state: &AppState,
    study_instance_uid: &str,
    identity: Option<(&str, Option<&str>)>,
    entries: &[CatalogEntry],
) -> bool {
    let store: WorkItemStore = super::ups_pure::workitem_store(state);
    let (patient_id, issuer) = match identity {
        Some((id, issuer)) => (Some(id), issuer),
        None => (None, None),
    };

    let mut all_ok = true;
    for entry in entries {
        // One workitem per catalog entry, each with its own UID: a procedure
        // that needs three AI tasks is three Offers (see
        // `config::UpsConfig::catalog_for_modalities`). They share the study,
        // the patient identity and the readiness arm below, and differ in the
        // code, its label, its trigger modality and the station they are
        // addressed to.
        if !create_one(&store, study_instance_uid, patient_id, issuer, entry) {
            all_ok = false;
        }
    }
    // ARM ONCE PER STUDY, after the entries: the debounce is a property of the
    // study (how long it has been quiet), not of any one workitem, so arming
    // inside the loop would only repeat the same write.
    if !entries.is_empty() {
        state
            .readiness
            .arm(study_instance_uid, std::time::Instant::now());
    }
    all_ok
}

/// Create one catalog entry's workitem, or re-arm it when it already exists.
///
/// Split from the loop so the per-entry branch - duplicate versus fresh - is
/// read once rather than inferred from the loop body.
fn create_one(
    store: &WorkItemStore,
    study_instance_uid: &str,
    patient_id: Option<&str>,
    issuer: Option<&str>,
    entry: &CatalogEntry,
) -> bool {
    let workitem_uid = entry.uid.as_str();

    // A duplicate is the EXPECTED outcome of a re-STOW, not an error: the UID
    // is deterministic, so "already present" is idempotency working as
    // designed. It never counts as a failure.
    //
    // It is also not an opportunity to enrich the stored record with the
    // identity: rewriting a workitem a performer may already hold would change
    // what a claim is about after the fact.
    match store.find_by_uid(workitem_uid) {
        Ok(Some(existing)) => {
            // A study that was ALREADY READY goes back to INCOMPLETE first: a
            // new instance means the study is no longer what the performer was
            // offered, and a record that stayed READY would let a claim act on
            // an input the sender has just extended. `arm` alone does not do
            // this - it only moves the deadline - so the flip is written here.
            // It applies to EVERY entry of the study, because the new instance
            // extended the input all of them operate on.
            regress_readiness_on_new_instances(store, existing);
            return true;
        }
        Ok(None) => {}
        Err(e) => {
            record_failure(&e, workitem_uid, "lookup");
            return false;
        }
    }

    let stamp = super::ups_pure::now_stamp();
    let dt_stamp = super::ups_pure::now_dt();
    let mut record = build_record_for_entry(
        study_instance_uid,
        patient_id,
        issuer,
        super::readiness::READINESS_INCOMPLETE,
        entry,
    );
    // (0040,4005) Scheduled Procedure Step Start DateTime is VR DT, whose form
    // is `YYYYMMDDHHMMSS` with no separators (PS3.5 6.2). Joining the store's
    // human-readable stamp with a `T` produced ISO 8601, which is a different
    // syntax and is not a legal DT. The step is offered now.
    record.sps_start_datetime = Some(dt_stamp);
    record.created_at = Some(stamp.clone());
    record.updated_at = Some(stamp);

    // Event publishing mirrors the HTTP create path: the SCHEDULED Event
    // Report reaches both the SSE fan-out and any open WebSocket Notification
    // Connection of a subscribed requester (RAD-87/RAD-109), in addition to
    // the POST push below. One publish site, one serialisation.
    //
    // NOT HERE, though. A workitem created by a STOW is offered INCOMPLETE and
    // announced only when the readiness debounce promotes it (see
    // `handlers/readiness.rs`): publishing now is what used to wake a
    // performer on the FIRST instance of a study and let it claim the two or
    // three slices that happened to be stored. Publishing the READY flip keeps
    // the envelope identical - same sink, same shape - and makes it the single
    // announcement a generated workitem ever has.
    match store.insert(&record) {
        Ok(()) => {
            crate::clog_info!(
                workitem_uid = %workitem_uid,
                code = %entry.code_value,
                station = %entry.station_aet.as_deref().unwrap_or("<none>"),
                "CAD workitem created INCOMPLETE for stored study"
            );
            true
        }
        // A concurrent request may have won the race between the lookup and
        // the insert. That is still a created workitem, not a failure.
        Err(StoreError::CasConflict(_)) => true,
        Err(e) => {
            record_failure(&e, workitem_uid, "create");
            false
        }
    }
}

/// Count and log one creation failure. Identifiers only - the workitem UID and
/// the error; never a patient field.
fn record_failure(error: &StoreError, workitem_uid: &str, stage: &'static str) {
    crate::metrics::CAD_WORKITEM_FAILURES_TOTAL.fetch_add(1, Ordering::Relaxed);
    crate::clog_warn!(
        error = %error,
        workitem_uid = %workitem_uid,
        stage = %stage,
        "CAD workitem creation failed (study already stored, client unaffected)"
    );
}
