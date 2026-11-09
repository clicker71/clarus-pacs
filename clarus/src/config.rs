// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! nginx-style configuration via `clarus.conf`.
//!
//! One file. chmod 600. No CLI flags (except --version, --config, -t).
//! No env vars. KISS.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use crate::storage_tiers::{BackendSchedule, CronSchedule, MigrationSchedule};

// -- [btree] index tier config -----------------------------------------

/// B-tree manifest tier configuration.
///
/// The B-tree (manifests + idx + journal) is the primary persistence tier;
/// there is no SQLite backend (excised in). These paths place the
/// study manifests, the daily indexes, and the append-only journal.
#[derive(Debug, Clone)]
pub struct BtreeConfig {
    /// Root of the B-tree data tree (manifests + idx + audit). Default
    /// `./data` - the same base the storage_dir convention uses.
    pub root: String,
    /// Subdirectory for study manifests. Default `manifests/`.
    pub manifest_dir: String,
    /// Subdirectory for daily `.idx` files. Default `idx/`.
    pub idx_dir: String,
    /// Subdirectory for the append-only JSON audit trail.
    /// THE ONLY EVIDENCE OF DELETIONS - back up this directory. Default
    /// `audit/` under `btree.root`.
    pub audit_dir: String,
    /// Subdirectory for the access-audit journal (ATNA-lite, 2808). JSON
    /// lines, retention-pruned, sibling of `audit_dir`. Default `access/`
    /// under `btree.root`. The pruner has NO code path to `audit_dir`.
    pub access_dir: String,
    /// Number of recent immutable days whose `.idx` files are touched at
    /// startup (B-010). Default 7.
    ///
    /// This warms the OS page cache, not process RSS: the startup preload
    /// (main.rs) mmaps the recent immutable `.idx` files, touches them, and
    /// drops the mappings immediately; the kernel retains the pages. There is
    /// therefore no budgeted RAM cost to state here.
    pub preload_days: usize,
    /// 3008 orphan GC: interval in seconds between orphan-sidecar reclaim
    /// passes in the erase worker (background only). Default 3600 (1 h);
    /// 0 = disabled.
    pub orphan_gc_interval_sec: u64,
}

impl Default for BtreeConfig {
    fn default() -> Self {
        Self {
            root: "./data".to_string(),
            manifest_dir: "manifests".to_string(),
            idx_dir: "idx".to_string(),
            audit_dir: "audit".to_string(),
            access_dir: "access".to_string(),
            preload_days: 7,
            orphan_gc_interval_sec: 3600,
        }
    }
}

// -- [audit] access journal (2808) ----------------------------------

/// Access-audit journal knobs (bounded file, per-minute buckets,
/// sections 4 and 9a).
#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Days to keep access journal files (`0` = keep forever). Default 1095
    /// (3 years) - a PRODUCT choice covering the ISPDn inspection horizon,
    /// not a regulatory number; deployments align with their own policy.
    pub access_retention_days: u64,
    /// Minimum minutes between auth-visibility WARNs (design 9a).
    /// `0`, negative or non-numeric values are invalid: the parser WARNs
    /// and falls back to the default 60 (never fatal).
    pub auth_warn_interval_min: u64,
    /// Pruner dry-run: report ACCESS_ROTATE lines but delete nothing
    /// (first-deployment flag).
    pub access_prune_dry_run: bool,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            access_retention_days: 1095,
            auth_warn_interval_min: 60,
            access_prune_dry_run: false,
        }
    }
}

// -- [query] default_limit --------------------------------------------

/// [query] section configuration.
///
/// The single source-of-truth for the default QIDO result limit when the
/// client does not send an explicit `limit` parameter.
#[derive(Debug, Clone)]
pub struct QueryConfig {
    /// Default result limit for open-ended QIDO SEARCHES without an explicit
    /// `limit`. `0` = unlimited. Default 250 (preserves pre-behavior).
    ///
    /// Does NOT apply to scoped enumeration
    /// (`/studies/{s}/series`, `/studies/{s}/series/{se}/instances`,
    /// `/studies/{s}/instances`) - those are bounded by the path and honor
    /// only an explicit client limit (12.09 MicroDicom 250-cap root cause).
    pub default_limit: u64,
    /// PN wildcard matching case-sensitivity. `true` (default) = standard
    /// (PS3.4 wildcard matching is case-sensitive). `false` = case-insensitive
    /// PN glob (Orthanc-style convenience deviation).
    pub case_sensitive_pn: bool,
    /// Force Levenshtein-1 fuzzymatch on PN keys even when the client does
    /// not send `fuzzymatching=true` (deviation; Weasis never sends it).
    /// `false` (default) = standard.
    pub fuzzymatch_override: bool,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            default_limit: 250,
            case_sensitive_pn: true,
            fuzzymatch_override: false,
        }
    }
}

// -- [ups] UPS-RS search default limit (12.09) -----------------------

/// [ups] section configuration: UPS-RS search (`GET /dicomweb/workitems`)
/// plus the CAD-workitem origin-server capability.
///
/// Separate dial from `[query] default_limit` on purpose: UPS-RS is polled
/// by modalities every few minutes (high-frequency, small pages) while QIDO
/// is viewer-driven (low-frequency, huge sets) - one knob would trade QIDO
/// comfort against UPS DoS protection.
#[derive(Debug, Clone)]
pub struct UpsConfig {
    /// Default result limit for UPS-RS searches without an explicit `limit`.
    /// `0` = unlimited. Default 100.
    pub default_limit: u64,
    /// Clarus acts as the Task Producer: after a study is durably stored it
    /// mints ONE workitem per study (PS3.18 Section 11.1: "User agents and
    /// origin servers can create Workitems").
    ///
    /// The role is not ours alone - a modality, a RIS, an external
    /// orchestrator or Clarus itself may each be the Task Producer, and they
    /// are all standard and interchangeable. The Task Performer (clinfer)
    /// claims workitems regardless of who created them, which is why this
    /// defaults to `false`: the STOW path is then untouched, byte for byte.
    pub task_producer: bool,
    /// Attempts per outbound workitem push before the notification is dropped
    /// (PS3.18 11.10/11.11 subscriptions). Range 1..=5, default 3. One dial
    /// only: the backoff schedule and the request timeout are fixed constants
    /// in `push`, not knobs (the `[dimse] outbound_assoc_attempts` precedent).
    /// Durability comes from the subscriber's catch-up search, never from
    /// unbounded retries.
    pub push_attempts: u32,
    /// Days a workitem in a final state (COMPLETED or CANCELED) is kept before
    /// the retention sweep deletes it. `0` = keep forever (sweep off).
    ///
    /// Default 7. Range 0..=3650. The standard puts no duration on retention
    /// (PS3.18 section 11 defines no Delete transaction), so this is an SCP
    /// policy dial for the extension our DELETE already is. SCHEDULED and
    /// IN PROGRESS workitems are never swept: a stuck IN PROGRESS is the
    /// dead-claimant case, which the operator resolves through the same admin
    /// DELETE, not through retention.
    pub final_state_retention_days: u64,
    /// Hours a Deletion Lock keeps a final-state workitem alive.
    ///
    /// AIW-I 4.85.4.1.3: "If the Manager has internal logic to 'override'
    /// remaining deletion locks and delete instances that have reached a
    /// Final State anyway based on internal logic, it shall be capable of
    /// waiting at least 24 hours before such deletions. This capability may
    /// be configurable." This dial IS that configurability, and 24 is that
    /// floor - so the default honours the "at least 24 hours" wording while
    /// the upper bound lets an operator hold a lock far longer.
    ///
    /// `0` = Deletion Locks are not honoured: the sweep behaves exactly as
    /// it did before they existed. That is the same override the quote
    /// permits (the Manager deletes anyway), expressed as the switch that
    /// turns the wait off.
    ///
    /// Default 24. Range 0..=8760 (one year).
    pub deletion_lock_ttl: u64,
    /// The site's workitem code vocabulary, keyed by study Modality.
    ///
    /// A closed site vocabulary: Modality (0008,0060, type 1, read from the
    /// stored study header) -> one code value of [`Self::code_scheme`].
    /// AIW-I 50.4.1.2 makes the code set a deployment agreement rather than a
    /// standard one, so the values live here and not in the code - which is
    /// the AIW-I 4.80.4.1.2.1 requirement (the request states the scheduled
    /// workitem code) met with a table the deployment owns.
    ///
    /// Keys are compared upper-case; see [`Self::code_for_modality`].
    pub workitem_codes: Vec<(String, String)>,
    /// The code value used for a modality the table does not name, when the
    /// site has opted in.
    ///
    /// `None` is the shipped default and means "an unlisted modality mints
    /// NOTHING" - only a log line. That is the honest default: a generic
    /// workitem nobody claims is worse than no workitem, and an operator can
    /// act on the log. A site that wants the old always-mint behaviour names
    /// a code here (the reserved `fallback` key in `[ups.workitem_code]`).
    ///
    /// When it IS set, the routing decision stays where it belongs - in the
    /// performer's rules, which can match the fallback code.
    pub workitem_code_fallback: Option<String>,
    /// Modalities that are AI OUTPUTS, never task inputs.
    ///
    /// A study is minted from its whole modality SET (a performer stores its
    /// SR report back into the study it read), so the set routinely holds
    /// modalities that are products of the system rather than requests for
    /// work. Listing them here keeps a result-store from minting a junk
    /// workitem for its own report.
    ///
    /// Membership is checked BEFORE the fallback: a non-task modality never
    /// mints, even when `fallback` is configured. Compared upper-case, because
    /// a modality is a DICOM CS value.
    pub non_task_modalities: Vec<String>,
    /// The meaning string written beside every generated code.
    /// Also the meaning of the fallback, which has no row of its own.
    pub workitem_code_meaning: String,
    /// (0040,0001) Scheduled Station AE Title of the AUTOMATIC workitems.
    ///
    /// An automatic workitem is ADDRESSED: the performer's own query is
    /// station-filtered, so naming its AET here is what makes the performer
    /// claim the workitem without any local matching rule. `None` (the
    /// default, and what an absent or empty key means) writes NO station at
    /// all, which is byte-for-byte the wire shape this server produced before
    /// the catalog existed - so an existing deployment is unchanged.
    pub ai_station_aet: Option<String>,
    /// The code VALUES that are manual (declarative, NOT minted on STOW).
    ///
    /// A code listed here is not created by the STOW automaton: it stays
    /// declarative and is created lazily by the Task Requester (Weasis) through
    /// UPS Create (PS3.18 11.4). [`Self::catalog_for_modalities`] drops these
    /// values, which is what keeps the mint and the readiness paths in
    /// agreement. Every other code - including the fallback, which has no row
    /// of its own - is automatic and minted as before. Keyed by code value,
    /// compared case-sensitively because a Code Value is a machine key, not a
    /// modality.
    pub manual_codes: Vec<String>,
    /// CODE -> MEANING: the label of each catalog entry.
    ///
    /// This IS the dropdown label the viewer shows: the label is the
    /// CodeMeaning of the Scheduled Workitem Code Sequence, read by the
    /// viewer as-is. Keeping it here rather than in the viewer is what leaves
    /// the viewer without a local label table. A code with no row - and the
    /// fallback - takes the global [`Self::workitem_code_meaning`].
    pub code_meanings: Vec<(String, String)>,
    /// The Coding Scheme Designators the site accepts on the Scheduled
    /// Workitem Code Sequence, besides the private `99*` ones.
    ///
    /// PS3.3 8.2 makes every designator beginning with `99` private or local,
    /// so those are always accepted and never need listing; a PUBLIC scheme
    /// (`LN`, `SCT`, `DCM`, `UCUM`, ...) has to be declared, because the set of
    /// vocabularies a site actually uses is the site's own decision. Without
    /// the declaration a create carrying `SCT` would be refused on a typo
    /// rule, and with a wildcard it would be accepted on any string at all.
    ///
    /// Compared EXACTLY: the designator is a machine key (PS3.3 8.2), so the
    /// section is code-valued and not folded.
    pub coding_schemes: Vec<String>,
}

impl UpsConfig {
    /// The coding scheme designator every generated code is declared under.
    ///
    /// A constant, not a knob, and deliberately NOT derived from
    /// `[server] modifying_system`: that identity is per-INSTANCE (it carries
    /// the bound port), while a workitem code has to be the SAME on every
    /// instance of a deployment or a performer's rules would mean something
    /// different depending on which node stored the study. PS3.3 Table 8.8-1
    /// is how a locally-invented code declares its private scheme.
    pub const CODE_SCHEME: &'static str = "99CLARUS";
    /// Shipped default for `push_attempts`.
    pub const DEFAULT_PUSH_ATTEMPTS: u32 = 3;
    /// Upper bound of `push_attempts` (inclusive).
    pub const MAX_PUSH_ATTEMPTS: u32 = 5;
    /// Shipped default for `final_state_retention_days`.
    pub const DEFAULT_FINAL_STATE_RETENTION_DAYS: u64 = 7;
    /// Upper bound of `final_state_retention_days` (inclusive; 0 = keep forever).
    pub const MAX_FINAL_STATE_RETENTION_DAYS: u64 = 3650;
    /// Shipped default for `deletion_lock_ttl`: the 24-hour floor AIW-I
    /// 4.85.4.1.3 names for deleting past a remaining lock.
    pub const DEFAULT_DELETION_LOCK_TTL_HOURS: u64 = 24;
    /// Upper bound of `deletion_lock_ttl` (inclusive; 0 = locks ignored).
    pub const MAX_DELETION_LOCK_TTL_HOURS: u64 = 8760;
    /// The shipped default code table: the owner's demo vocabulary.
    ///
    /// DX -> CXR-AI, CT -> CT-ORGAN-SEG, everything else -> the
    /// fallback. A deployment overrides these in `clarus.conf`; the defaults
    /// exist so the capability works the moment it is switched on.
    pub const DEFAULT_WORKITEM_CODES: &'static [(&'static str, &'static str)] =
        &[("DX", "CXR-AI"), ("CT", "CT-ORGAN-SEG")];
    /// The shipped modality set that never mints.
    ///
    /// These are the standard's structured OUTPUT objects: a report (SR), a
    /// presentation state (PR), a segmentation (SEG), a key object (KO) and a
    /// radiation therapy structure set (RTSTRUCT). A study that contains only
    /// these is the product of work already done, so minting a task for it
    /// would ask a performer to analyse its own output.
    pub const DEFAULT_NON_TASK_MODALITIES: &'static [&'static str] =
        &["SR", "PR", "SEG", "KO", "RTSTRUCT"];
    /// The shipped fallback code value.
    ///
    /// Kept as a named value even though the shipped CONFIG has no fallback:
    /// `[ups.code_meaning]` examples and the tests name the generic code, and
    /// a site that turns the fallback on wants a predictable default string.
    pub const DEFAULT_WORKITEM_CODE_FALLBACK: &'static str = "GENERIC-AI";
    /// The shipped code meaning.
    pub const DEFAULT_WORKITEM_CODE_MEANING: &'static str = "AI inference";
    /// Upper bound of an AE Title (PS3.5 Table 6.2-1: AE, 16 chars).
    pub const MAX_AE_TITLE: usize = 16;

    /// The code value for one study Modality, or the fallback.
    ///
    /// Kept for the single-modality callers (the DIMSE-side paths that have no
    /// study set in scope). The mint and readiness paths use
    /// [`Self::catalog_for_modalities`], which takes the whole set.
    ///
    /// The comparison is case-insensitive and exact per key: a modality is a
    /// DICOM CS value, so `dx` and `DX` are the same modality, while `DXX`
    /// is a different (unlisted) one and takes the fallback.
    ///
    /// `None` - a study header with no Modality at all - also takes the
    /// fallback, when one is configured.
    #[must_use]
    pub fn code_for_modality(&self, modality: Option<&str>) -> Option<&str> {
        let Some(modality) = modality else {
            return self.workitem_code_fallback.as_deref();
        };
        self.workitem_codes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(modality.trim()))
            .map(|(_, value)| value.as_str())
            .or(self.workitem_code_fallback.as_deref())
    }

    /// Whether a modality is one of the AI OUTPUTS that never mint.
    ///
    /// Compared upper-case on both sides: the site writes a modality as a CS
    /// value, and the shipped defaults are upper-case.
    #[must_use]
    pub fn is_non_task_modality(&self, modality: &str) -> bool {
        let modality = modality.trim();
        self.non_task_modalities
            .iter()
            .any(|listed| listed.eq_ignore_ascii_case(modality))
    }

    /// Every catalog entry a STUDY produces, from its whole modality SET.
    ///
    /// This is THE selection rule, and it is deliberately one function rather
    /// than two: the mint path (which creates the workitems) and the readiness
    /// path (which promotes and regresses them) have to agree on exactly which
    /// codes a study yields, and two copies of that decision would drift into
    /// a study whose INR workitems are never promoted.
    ///
    /// `modalities` is the STUDY's distinct modality set, not one instance's.
    /// That is the whole point: a study a performer reads images from and
    /// writes a report into is `{DX, SR}`, and one modality cannot describe
    /// it. A single-modality study is simply a set of one.
    ///
    /// The rule, in order:
    ///
    /// 1. Drop every non-task modality ([`Self::is_non_task_modality`]): a
    ///    report is not a request for work, and it must not reach the fallback
    ///    either.
    /// 2. Collect the catalog rows of the remaining candidates, skipping the
    ///    PARKED (manual) codes ([`Self::is_manual_code`]): a manual code is
    ///    not minted on STOW at all, it is created lazily by the Task
    ///    Requester through UPS Create. The result is a SET of code values, so
    ///    a code reachable through two modalities (a rule naming `DX,MG`)
    ///    yields ONE entry - which is what makes the per-(study, code) UID the
    ///    idempotency key.
    /// 3. If no row matched and at least one candidate remains, the candidate
    ///    set is "unlisted input": mint the fallback IF the site configured
    ///    one, otherwise nothing. The fallback is automatic by construction
    ///    (it has no catalog row, so it cannot be listed in
    ///    `[ups.manual_codes]` either).
    /// 4. If no candidate remains at all, the study is all outputs: nothing.
    ///
    /// The filter lives HERE rather than at a caller, because both the mint
    /// (`handlers/cad_workitem.rs`) and the readiness paths go through this
    /// function: a filter somewhere below one of them would let the two
    /// disagree about which codes a study yields.
    ///
    /// The result is sorted by code value so the order does not depend on the
    /// table's line order or on the set's iteration order: the workitem UIDs
    /// are derived from (study, code) and nothing else, and a canonical order
    /// keeps any by-position output stable.
    #[must_use]
    pub fn catalog_for_modalities<I>(&self, modalities: I) -> Vec<(String, &str)>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        // (1) Candidates: the study's modalities minus the non-task outputs.
        //     The TRIGGER modality is kept beside each code, because the
        //     workitem has to state the modality its task is about.
        //
        // The trigger is an owned String: a study has a handful of modalities,
        // and owning it removes a borrow puzzle that would otherwise force the
        // caller to keep its iterator alive for the life of the result.
        let mut pairs: Vec<(String, &str)> = Vec::new();
        let mut has_candidate = false;
        for modality in modalities {
            let modality = modality.as_ref().trim();
            if modality.is_empty() || self.is_non_task_modality(modality) {
                continue;
            }
            has_candidate = true;
            // (2) Every row this candidate matches, by value, MINUS the parked
            //     (manual) codes: those are declarative, and the Task Requester
            //     creates them through UPS Create. A manual code therefore
            //     never reaches the mint, so no readiness debounce is armed for
            //     it either (there is nothing to promote).
            pairs.extend(
                self.workitem_codes
                    .iter()
                    .filter(|(key, _)| key.eq_ignore_ascii_case(modality))
                    .filter(|(_, value)| !self.is_manual_code(value))
                    .map(|(_, value)| (modality.to_string(), value.as_str())),
            );
        }
        // (3) An unlisted input takes the fallback only if the site opted in.
        //     A fallback has no catalog row, so it gets no trigger modality of
        //     its own - the record then carries none, which is the honest
        //     shape for a generic task that belongs to no particular modality.
        let opted_in = pairs.is_empty() && has_candidate;
        if let (true, Some(fallback)) = (opted_in, self.workitem_code_fallback.as_deref()) {
            pairs.push((String::new(), fallback));
        }
        // (4) Dedupe by code VALUE: one entry per code, however many modalities
        //     or table rows reached it. The UID is keyed on (study, code), so a
        //     duplicate here would be a second write of the same workitem.
        //
        //     The dedupe is by value alone, and stays that way: sorting by
        //     (modality, code) made `dedup_by` remove only ADJACENT equal codes,
        //     so the same code reached through two modalities with a different
        //     code between them survived and minted the workitem twice.
        //
        //     The order is (code, modality) so the tiebreak is DETERMINISTIC:
        //     sorting on the code alone left entries with equal codes in an
        //     order that depended on how the set was built, so which modality
        //     got recorded as a shared code's trigger varied between runs -
        //     and that modality is written into the workitem.
        pairs.sort_unstable_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(&b.0)));
        pairs.dedup_by(|a, b| a.1 == b.1);
        pairs
    }

    /// Every catalog entry a study of ONE modality produces.
    ///
    /// A convenience over [`Self::catalog_for_modalities`] for the callers that
    /// genuinely have a single modality (the DIMSE-side paths): it is exactly
    /// the set of one, so the two cannot disagree.
    #[must_use]
    pub fn catalog_for_modality(&self, modality: Option<&str>) -> Vec<&str> {
        match modality {
            Some(modality) => self
                .catalog_for_modalities(std::iter::once(modality))
                .into_iter()
                .map(|(_, code)| code)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Whether the site's table names this modality at all.
    ///
    /// Distinct from "which codes does it yield": an unnamed modality still
    /// yields the fallback, so the output looks the same either way and an
    /// operator cannot tell from the workitem alone that their table is
    /// missing a row. This predicate is what lets the mint log that case.
    ///
    /// The comparison is the same one [`Self::catalog_for_modality`] uses, so
    /// the two cannot disagree about what "named" means.
    #[must_use]
    pub fn names_modality(&self, modality: Option<&str>) -> bool {
        modality.is_some_and(|modality| {
            let modality = modality.trim();
            self.workitem_codes
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case(modality))
        })
    }

    /// Whether a code is one of the PARKED (manual) ones.
    ///
    /// The comparison is exact: a Code Value is a machine key agreed with the
    /// site, so `CXR-AI` and `cxr-ai` are different codes, not one code in two
    /// spellings. A code that is not listed - the fallback included - is
    /// automatic, which is the pre-catalog behaviour of every code.
    ///
    /// This is the predicate [`Self::catalog_for_modalities`] uses to KEEP a
    /// parked code out of the mint: a manual code is declarative, and it is
    /// created lazily by the Task Requester (UPS Create, PS3.18 11.4).
    #[must_use]
    pub fn is_manual_code(&self, code: &str) -> bool {
        self.manual_codes.iter().any(|listed| listed == code)
    }

    /// Whether a Coding Scheme Designator is one this server accepts.
    ///
    /// PS3.3 8.2: a designator beginning with `99` AND the single designator
    /// `L` are private or local, so both are accepted WITHOUT listing - the
    /// standard names `L` (HL7 local) in the same sentence as the `99` prefix.
    /// Everything else has to appear in `[ups.coding_schemes]`, exactly - the
    /// designator is a machine key, and a lookup that folded case would accept
    /// `ln` for `LN`.
    ///
    /// The test is a literal prefix/equality test, not `contains`: `X99Y` is
    /// not private, and a substring test would wave it through.
    #[must_use]
    pub fn allows_scheme(&self, scheme: &str) -> bool {
        scheme.starts_with("99")
            || scheme == "L"
            || self.coding_schemes.iter().any(|listed| listed == scheme)
    }

    /// The CodeMeaning written beside a code.
    ///
    /// This is the label the viewer's dropdown shows. A code with no row of
    /// its own falls back to the global meaning, which is also the fallback
    /// code's meaning - the fallback has no row by construction.
    #[must_use]
    pub fn meaning_for_code(&self, code: &str) -> &str {
        self.code_meanings
            .iter()
            .find(|(key, _)| key == code)
            .map_or(self.workitem_code_meaning.as_str(), |(_, value)| {
                value.as_str()
            })
    }
}

impl Default for UpsConfig {
    fn default() -> Self {
        Self {
            default_limit: 100,
            task_producer: false,
            push_attempts: 3,
            final_state_retention_days: Self::DEFAULT_FINAL_STATE_RETENTION_DAYS,
            deletion_lock_ttl: Self::DEFAULT_DELETION_LOCK_TTL_HOURS,
            workitem_codes: Self::DEFAULT_WORKITEM_CODES
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
            workitem_code_fallback: None,
            non_task_modalities: {
                // Sorted, to match the parser: this is a SET, and having the
                // default and a parsed list disagree on order would make any
                // order-sensitive comparison of the two wrong for no reason.
                let mut m: Vec<String> = Self::DEFAULT_NON_TASK_MODALITIES
                    .iter()
                    .map(|m| (*m).to_string())
                    .collect();
                m.sort_unstable();
                m
            },
            workitem_code_meaning: Self::DEFAULT_WORKITEM_CODE_MEANING.to_string(),
            ai_station_aet: None,
            manual_codes: Vec::new(),
            code_meanings: Vec::new(),
            coding_schemes: Vec::new(),
        }
    }
}

// -- [workflow] MWL + MPPS records (PS3.18 Section 14/15) ------------

/// `[workflow]` section: the Modality Worklist (MWL, PS3.18 §14) and Modality
/// Performed Procedure Step (MPPS, PS3.18 §15) surfaces.
///
/// Both services name records that are mutable and short-lived - an MWL row
/// for a scheduled step, an MPPS row for a performed step - so they belong
/// nowhere near the immutable CAS. They live under one directory (with `mwl/`
/// and `mpps/` below it) and one retention dial sweeps them. The section sits
/// between `[ups]` and `[commitment]` because it is the same kind of dial:
/// a bounded, site-owned worklist surface.
#[derive(Debug, Clone)]
pub struct WorkflowConfig {
    /// Whether the MWL/MPPS surface answers at all. Default `false`.
    ///
    /// OFF by default, like `[ups] task_producer`: a deployment that has not
    /// opted in must not answer these two resources, and switching it on is a
    /// deliberate operator act. A disabled surface answers 404 (the resource
    /// is not there), not 405.
    pub enabled: bool,
    /// The store root. Empty means the default `{btree.root}/workflow`.
    ///
    /// Kept as written so a relative path resolves against the config file's
    /// parent, the same contract `[btree] root` follows; `resolved_dir` is how
    /// the handler reads it.
    pub dir: String,
    /// Days a record is kept before the sweep deletes it. `0` = keep forever.
    ///
    /// Default 7. Range 0..=3650. The same "0 disables" convention `[ups]
    /// final_state_retention_days` and `[commitment] result_retention_days`
    /// use. An MWL row and a FINAL MPPS row are swept; an `IN PROGRESS` MPPS
    /// row is not, because a step that never finished is a stuck performer the
    /// operator resolves, not a row retention may drop.
    pub retention_days: u64,
    /// The MWL search response bound when the client sends no `limit`.
    /// `0` = unlimited (still clamped by `mwl_max_responses`). Default 100.
    ///
    /// The same separate dial `[ups] default_limit` is, for the same reason:
    /// a worklist is polled per modality, and one knob would trade a viewer's
    /// comfort against a DoS bound.
    pub mwl_default_limit: u64,
    /// The HARD ceiling on an MWL search response. Range 1..=10_000.
    ///
    /// Default 200. A client `limit` above this is CLAMPED, and so is a
    /// default-derived one: this is the bound that stops one request from
    /// pulling a whole site's worklist in one answer.
    pub mwl_max_responses: u64,
    /// The `[btree] root` this config was loaded with, used to derive the
    /// default directory. Empty in a hand-built config, which then falls back
    /// to a bare relative `workflow`.
    #[doc(hidden)]
    pub btree_root_hint: String,
}

impl WorkflowConfig {
    /// Shipped default for `retention_days`.
    pub const DEFAULT_RETENTION_DAYS: u64 = 7;
    /// Upper bound of `retention_days` (inclusive; 0 = keep forever).
    pub const MAX_RETENTION_DAYS: u64 = 3650;
    /// Shipped default for `mwl_default_limit`.
    pub const DEFAULT_MWL_DEFAULT_LIMIT: u64 = 100;
    /// Shipped default for `mwl_max_responses`.
    pub const DEFAULT_MWL_MAX_RESPONSES: u64 = 200;
    /// Shipped lower bound for `mwl_max_responses`.
    pub const MIN_MWL_MAX_RESPONSES: u64 = 1;
    /// Upper bound of `mwl_max_responses` (inclusive).
    pub const MAX_MWL_MAX_RESPONSES: u64 = 10_000;

    /// The effective store root.
    ///
    /// An explicit, non-empty `dir` is taken verbatim (the loader resolves a
    /// relative one against the config file's parent). An absent or empty
    /// `dir` resolves to `{btree.root}/workflow`, which is the same directory
    /// the `[btree]` root already owns - so the store lands beside `manifests`
    /// and `idx` rather than in the process CWD.
    #[must_use]
    pub fn resolved_dir(&self) -> String {
        if !self.dir.trim().is_empty() {
            return self.dir.clone();
        }
        std::path::Path::new(&self.btree_root_hint)
            .join("workflow")
            .to_string_lossy()
            .to_string()
    }
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: String::new(),
            retention_days: Self::DEFAULT_RETENTION_DAYS,
            mwl_default_limit: Self::DEFAULT_MWL_DEFAULT_LIMIT,
            mwl_max_responses: Self::DEFAULT_MWL_MAX_RESPONSES,
            btree_root_hint: String::new(),
        }
    }
}

// -- [commitment] Storage Commitment result retention ---------------
///
/// The standard fixes NO duration for how long a Commit result must stay
/// available; 13.2 instead requires the origin to say so in its Conformance
/// Statement. That makes retention a product policy, and this is its dial.
#[derive(Debug, Clone)]
pub struct CommitmentConfig {
    /// Days a Storage Commitment result is kept before the sweep deletes it.
    /// `0` = keep forever (sweep off).
    ///
    /// Default 30. Range 0..=3650. The same "0 disables" convention the UPS
    /// deletion-lock TTL uses, so an operator has one rule to remember.
    /// After a result is swept, Check Commit Result answers 404, which
    /// Table 13.5.3-1 explicitly allows for a deleted result; no 410
    /// tombstone is kept.
    pub result_retention_days: u64,
}

impl CommitmentConfig {
    /// Default retention window, in days.
    pub const DEFAULT_RESULT_RETENTION_DAYS: u64 = 30;
    /// Upper bound, in days (ten years).
    pub const MAX_RESULT_RETENTION_DAYS: u64 = 3650;
}

impl Default for CommitmentConfig {
    fn default() -> Self {
        Self {
            result_retention_days: Self::DEFAULT_RESULT_RETENTION_DAYS,
        }
    }
}

// -- [transcode] bounded pool (2608) ---------------------------------

/// Bounded-transcode configuration (2608 writer task (c)).
///
/// Optional operator overrides for the global transcode/render permit pool.
/// Both keys are OPTIONAL: when absent the defaults are computed automatically
/// (the pool `N` from the machine's core count, the pixel budget from a fixed
/// deterministic guard). Dynamic inputs (cpu-load / mem%) are deliberately NOT
/// used - they would make 503s non-deterministic ("by the weather") and
/// untestable. The pair (N, B) already bounds BOTH CPU (N concurrent decodes)
/// and memory (N x frame size).
#[derive(Debug, Clone)]
pub struct TranscodeConfig {
    /// Global concurrent transcode/render permits (N). Shared by transcode
    /// AND render. Default: `clamp(available_parallelism()/4, 1..=32)`.
    /// Valid override range 1..=32.
    pub max_concurrent: u32,
    /// Max decoded pixel count (megapixels) per instance, checked BEFORE the
    /// decode buffer is allocated. Default 64 MP (a deterministic guard
    /// against decompression bombs; at 16bpp that is ~128 MB per frame, so
    /// worst-case transcode RSS = N x 128 MB). Valid override range 1..=1024.
    pub max_megapixels: u32,
}

impl TranscodeConfig {
    /// Compute the automatic default concurrency `N` = clamp(parallelism/4,
    /// 1..=32). Built only from `std::thread::available_parallelism()` (stable
    /// since 1.59, zero new deps), never from runtime load.
    pub fn default_concurrency() -> u32 {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        (cores / 4).clamp(1, 32) as u32
    }

    /// Parse the `[transcode]` section into effective values. Both keys are
    /// OPTIONAL overrides; an out-of-range override is a config error (not
    /// silently clamped) so a typo cannot hide a runaway pool or budget.
    /// Never reads runtime load - only the operator override or the static
    /// default, so `-t` prints deterministic effective values and 503s never
    /// depend on "the weather".
    pub fn parse(p: &ConfParser) -> Result<Self, String> {
        let max_concurrent = match p.get("transcode", "transcode_max_concurrent") {
            Some(raw) => {
                let v: u32 = raw.trim().parse().map_err(|_| {
                    format!(
                        "clarus.conf [transcode] transcode_max_concurrent: '{raw}' is not an integer"
                    )
                })?;
                if !(1..=32).contains(&v) {
                    return Err(format!(
                        "clarus.conf [transcode] transcode_max_concurrent: {v} is out of range [1..32]"
                    ));
                }
                v
            }
            None => Self::default_concurrency(),
        };
        let max_megapixels = match p.get("transcode", "transcode_max_megapixels") {
            Some(raw) => {
                let v: u32 = raw.trim().parse().map_err(|_| {
                    format!(
                        "clarus.conf [transcode] transcode_max_megapixels: '{raw}' is not an integer"
                    )
                })?;
                if !(1..=1024).contains(&v) {
                    return Err(format!(
                        "clarus.conf [transcode] transcode_max_megapixels: {v} is out of range [1..1024]"
                    ));
                }
                v
            }
            None => 64,
        };
        Ok(Self {
            max_concurrent,
            max_megapixels,
        })
    }
}

// -- HTTP/1.1 identity -------------------------------------------------

/// Trusted-proxy CIDR network. Parsed with our own decoder (no `ipnet`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CidrNet {
    /// IPv4 (addr, prefix_len, mask) as `(u32, u32)`.
    V4 { addr: u32, prefix: u32 },
    /// IPv6 (addr, prefix_len) as `(u128, u128)`.
    V6 { addr: u128, prefix: u32 },
}

impl CidrNet {
    /// Does `ip` fall inside this network?
    pub fn contains_v4(&self, ip: u32) -> bool {
        match *self {
            CidrNet::V4 { addr, prefix } => {
                let mask = if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                };
                ip & mask == addr & mask
            }
            CidrNet::V6 { .. } => false,
        }
    }

    /// Does `ip` fall inside this network?
    pub fn contains_v6(&self, ip: u128) -> bool {
        match *self {
            CidrNet::V6 { addr, prefix } => {
                let mask = if prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - prefix)
                };
                ip & mask == addr & mask
            }
            CidrNet::V4 { .. } => false,
        }
    }
}

/// Parse a dotted-quad IPv4 address into a `u32` (network byte order bits).
fn parse_ipv4(s: &str) -> Result<u32, String> {
    let mut parts = s.split('.');
    let mut out: u32 = 0;
    for byte in 0..4 {
        let part = parts.next().ok_or_else(|| format!("{s}: bad IPv4"))?;
        if part.is_empty() || part.len() > 3 {
            return Err(format!("{s}: bad IPv4 octet"));
        }
        let v: u32 = part.parse().map_err(|_| format!("{s}: bad IPv4"))?;
        if v > 255 {
            return Err(format!("{s}: IPv4 octet > 255"));
        }
        out |= v << (8 * (3 - byte));
    }
    if parts.next().is_some() {
        return Err(format!("{s}: too many IPv4 octets"));
    }
    Ok(out)
}

/// Parse a hex IPv6 address into a `u128` (RFC 4291 compressed form).
fn parse_ipv6(s: &str) -> Result<u128, String> {
    // Split on at most one "::" - the compressed zero run.
    fn parse_groups(pieces: &str, what: &str) -> Result<Vec<u16>, String> {
        let mut g = Vec::new();
        for piece in pieces.split(':').filter(|p| !p.is_empty()) {
            g.push(u16::from_str_radix(piece, 16).map_err(|_| format!("{what}: bad IPv6"))?);
        }
        Ok(g)
    }

    let groups: Vec<u16> = if let Some((left, right)) = s.split_once("::") {
        let left_groups = parse_groups(left, s)?;
        let right_groups = parse_groups(right, s)?;
        // The `::` run must absorb the remaining groups: gap = 8 - L - R.
        let left_len = left_groups.len();
        if left_len + right_groups.len() >= 8 {
            return Err(format!("{s}: too many IPv6 groups"));
        }
        let gap = 8 - left_len - right_groups.len();
        let mut all = left_groups;
        all.resize(left_len + gap, 0);
        all.extend(right_groups);
        all
    } else {
        let g = parse_groups(s, s)?;
        if g.len() != 8 {
            return Err(format!("{s}: bad IPv6 group count"));
        }
        g
    };

    let mut out: u128 = 0;
    for (i, g) in groups.iter().enumerate() {
        out |= (*g as u128) << (16 * (7 - i));
    }
    Ok(out)
}

/// Parse a CIDR string `addr[/prefix]` into a [`CidrNet`].
/// Default prefix: `/32` (v4), `/128` (v6). For IPv4 the prefix may also be
/// written as a dotted netmask (`/255.255.255.0`); non-contiguous masks are
/// rejected. IPv6 keeps the numeric prefix form.
pub fn parse_cidr(s: &str) -> Result<CidrNet, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty CIDR".into());
    }
    let (addr_str, prefix_str) = match s.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (s, None),
    };
    if addr_str.contains(':') {
        let addr = parse_ipv6(addr_str)?;
        let prefix = match prefix_str {
            Some(p) if p.contains('.') => {
                return Err(format!("{s}: dotted netmask is IPv4-only, use /prefix"));
            }
            Some(p) => p.parse::<u32>().map_err(|_| format!("{s}: bad prefix"))?,
            None => 128,
        };
        if prefix > 128 {
            return Err(format!("{s}: prefix > 128"));
        }
        Ok(CidrNet::V6 { addr, prefix })
    } else {
        let addr = parse_ipv4(addr_str)?;
        let prefix = match prefix_str {
            // Admins write masks in dotted form: `192.168.1.0/255.255.255.0`.
            // Convert a contiguous mask to its prefix length; reject masks
            // whose set bits are not a single leading run (255.0.255.0).
            Some(p) if p.contains('.') => {
                let mask = parse_ipv4(p)?;
                if mask.leading_ones() + mask.trailing_zeros() != 32 {
                    return Err(format!("{s}: non-contiguous netmask"));
                }
                mask.leading_ones()
            }
            Some(p) => p.parse::<u32>().map_err(|_| format!("{s}: bad prefix"))?,
            None => 32,
        };
        if prefix > 32 {
            return Err(format!("{s}: prefix > 32"));
        }
        Ok(CidrNet::V4 { addr, prefix })
    }
}

/// Parse a comma-separated CIDR list. Empty list is an error.
pub fn parse_trusted_proxies(s: &str) -> Result<Vec<CidrNet>, String> {
    let mut out = Vec::new();
    for item in s.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        out.push(parse_cidr(item)?);
    }
    if out.is_empty() {
        return Err("trusted_proxies: must contain at least one CIDR".into());
    }
    Ok(out)
}

/// Parse a comma-separated CIDR list where **empty is valid** (and is the
/// default). Used by `[server] admin_cidrs`: no value means "the local machine
/// only", which is the restrictive default, so an empty list must not be an
/// error the way an empty `trusted_proxies` is.
pub fn parse_cidr_list(s: &str) -> Result<Vec<CidrNet>, String> {
    let mut out = Vec::new();
    for item in s.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        out.push(parse_cidr(item)?);
    }
    Ok(out)
}

/// Parse an nginx-style size with `k`/`m` suffixes into a byte count.
///
/// - bare integer -> bytes (backward compatible)
/// - `k`/`K` -> x1024; `m`/`M` -> x1024^2
/// - fractions are NOT allowed (`1.5m` -> error)
/// - the result is validated against `[min, max]` after decoding
///
/// Returns a descriptive error (`expected size like "1m" or "1048576"`) on any
/// malformed or out-of-range input. This is the single parse-path for
/// `max_json_body` / `max_part_header` (the Section 6 size-suffix gate).
pub fn parse_size(s: &str, min: u64, max: u64) -> Result<u64, String> {
    let mul: u64 = match s.as_bytes().last() {
        Some(b'k') | Some(b'K') => 1024,
        Some(b'm') | Some(b'M') => 1024 * 1024,
        _ => 1,
    };
    let digits = if mul == 1 { s } else { &s[..s.len() - 1] };
    // Reject empty digits, negatives, trailing junk, and any non-digit form
    // (fractions like "1.5m" are excluded because '.' is not a digit).
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "expected size like \"1m\" or \"1048576\", got \"{s}\""
        ));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("expected size like \"1m\" or \"1048576\", got \"{s}\""))?;
    let bytes = n
        .checked_mul(mul)
        .ok_or_else(|| format!("expected size like \"1m\" or \"1048576\", got \"{s}\""))?;
    if !(min..=max).contains(&bytes) {
        return Err(format!(
            "size {s}: out of range [{min}..{max}] (got {bytes} bytes)"
        ));
    }
    Ok(bytes)
}

/// Render a byte count canonically with a `k`/`m` suffix where it divides
/// evenly, else as bare bytes (used by `-t` output).
pub fn size_canonical(bytes: u64) -> String {
    for (factor, suffix) in [(1024 * 1024, "m"), (1024, "k")] {
        if bytes >= factor && bytes.is_multiple_of(factor) {
            return format!("{}{suffix}", bytes / factor);
        }
    }
    bytes.to_string()
}

// -- nginx-style config parser (0 KB - std only) --------------------

/// Parsed clarus.conf - flat section-key-value map.
/// Sections are `[name]`, keys are `key = value`, comments are `#` or `;`.
#[derive(Debug)]
pub struct ConfParser {
    /// (section, key) -> value
    values: HashMap<(String, String), String>,
}

/// Known top-level sections. Anything else is a config error (typo guard).
const KNOWN_SECTIONS: &[&str] = &[
    "global",
    "server",
    "storage",
    "logging",
    "features",
    "s3",
    "storage_tiers",
    "btree",
    "recovery",
    "query",
    "ups",
    "ups.workitem_code",
    "ups.manual_codes",
    "ups.code_meaning",
    "ups.coding_schemes",
    "workflow",
    "transcode",
    "commitment",
    "audit",
];

/// Rename hints: (section, old_key, new_description).
/// Pre-production (v0.3.0-alpha) - hard error, no deprecation window.
const RENAME_HINTS: &[(&str, &str, &str)] = &[(
    "server",
    "request_timeout_secs",
    "header_timeout_secs (header read) / body_timeout_secs (body read)",
)];

/// Whether a section's KEYS are Code Values rather than configuration names.
///
/// Configuration names are case-insensitive, so the parser folds them. A Code
/// Value is not: PS3.16 code values are agreed strings, and `[ups.workitem_code]`
/// values are compared against them exactly. Folding those keys made a site's
/// own spelling unreachable, which is how a parked code could still be minted.
fn section_is_code_valued(section: &str) -> bool {
    matches!(
        section,
        "ups.workitem_code" | "ups.manual_codes" | "ups.code_meaning" | "ups.coding_schemes"
    )
}

/// All known `(section, key)` keys that the strict parser accepts.
fn is_known_key(section: &str, key: &str) -> bool {
    const S: &[&str] = &[
        "host",
        "port",
        "header_timeout_secs",
        "body_timeout_secs",
        "keepalive_timeout_secs",
        "send_timeout_secs",
        "drain_timeout_secs",
        "max_connections",
        "max_json_body",
        "max_part_header",
        "trusted_proxies",
        "admin_cidrs",
        "expose_rejected_cidrs",
        "require_proxy_identity",
        "anyclass_enable",
        "hostname",
        "modifying_system",
    ];
    const STORAGE: &[&str] = &[
        "storage_dir",
        "max_instance_size_mb",
        "max_frame_bytes",
        "max_frames_per_instance",
        "max_batch_instances",
        "max_batch_bytes",
        "min_free_disk_mb",
        "min_free_disk_pct",
        "cold_path",
    ];
    const LOGGING: &[&str] = &["log_level", "log_dir"];
    const FEATURES: &[&str] = &["jpeg2000"];
    const S3: &[&str] = &[
        "endpoint",
        "bucket",
        "region",
        "tier",
        "prefix",
        "access_key",
        "secret_key",
    ];
    const S3_TIERS: &[&str] = &[
        "s3_schedule",
        "s3_min_age_days",
        "cold_path",
        "cold_min_age_days",
        "cold_schedule",
        "emergency_free_gb",
        "emergency_check_interval_mins",
    ];
    const BTREE: &[&str] = &[
        "root",
        "manifest_dir",
        "idx_dir",
        "audit_dir",
        "access_dir",
        "preload_days",
        "orphan_gc_interval_sec",
    ];
    const QUERY: &[&str] = &["default_limit", "case_sensitive_pn", "fuzzymatch_override"];
    const UPS: &[&str] = &[
        "default_limit",
        "task_producer",
        "push_attempts",
        "final_state_retention_days",
        "deletion_lock_ttl",
        "ai_station_aet",
        "non_task_modalities",
    ];
    /// Whether a key is one of the two reserved words of
    /// `[ups.workitem_code]`, compared case-insensitively.
    ///
    /// The section's keys are not folded by the parser (they are code values),
    /// so a site that writes `Fallback` or `MEANING` must still be understood
    /// as the reserved word: otherwise the row was validated as a modality,
    /// stored as one, and the option it was meant to set stayed unset - a
    /// silently disabled fallback, which is a behaviour change with no error.
    fn claims_reserved_workitem_code_key(key: &str) -> bool {
        key.eq_ignore_ascii_case("fallback") || key.eq_ignore_ascii_case("meaning")
    }

    /// `[ups.workitem_code]` is a MODALITY -> code table, so its keys are
    /// open-ended except for the two reserved words. A fixed allowlist cannot
    /// express it; instead every key is validated as a modality-shaped token
    /// so a typo in a KEY slips through no more easily than a fixed list.
    ///
    /// Keys arrive AS WRITTEN: `[ups.workitem_code]` is a code-valued table, so
    /// the parser does not fold its keys (`section_is_code_valued`). Both cases
    /// are therefore accepted here, and the modifier lookup compares
    /// case-insensitively because a modality is a DICOM CS value.
    ///
    /// Reserved: `fallback` (the unlisted-modality code, OPT-IN: absent means
    /// an unlisted modality mints nothing and only warns) and `meaning` (the
    /// label for codes without a `[ups.code_meaning]` row).
    fn is_workitem_code_key(key: &str) -> bool {
        // The reserved words are matched case-INSENSITIVELY. Keys in this
        // section arrive as written (it is a code-valued table), so `Fallback`
        // or `MEANING` reached the modality branch and was stored as a
        // modality token - the opt-in fallback was silently off and a
        // `Fallback` row became a bogus modality. Fold only for the reserved
        // lookup; a modality key keeps its spelling.
        if claims_reserved_workitem_code_key(key) {
            return true;
        }
        // section arrive as written (it is a code-valued table), so `Fallback`
        // or `MEANING` reached the modality branch below and was stored as a
        // modality token - the opt-in fallback was silently off and a
        // `Fallback` row became a bogus modality. Fold only for the reserved
        // lookup; a modality key keeps its spelling.
        if claims_reserved_workitem_code_key(key) {
            return true;
        }
        // A DICOM CS value: A-Z, 0-9 and underscore. Length 1..=16 is the CS
        // value bound (PS3.5 Table 6.2-1). Both cases are accepted because a
        // modality key is not folded any more: the site writes DX or dx and
        // the lookup compares case-insensitively.
        !key.is_empty()
            && key.len() <= 16
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }
    /// `[ups.manual_codes]` is a set of code VALUES, so its keys are the
    /// codes and its values are ignored: an operator writes one line per
    /// parked code. A fixed allowlist cannot express an open vocabulary, so
    /// each key is validated as a code value (SH: at most 16 printable ASCII
    /// chars) - the same rule [`UpsConfig`] applies to a value in
    ///`[ups.workitem_code]`.
    ///
    /// The keys arrive AS WRITTEN: `[ups.manual_codes]` is a code-valued table,
    /// so the parser does not fold them (`section_is_code_valued`) and the set
    /// keeps the spelling the site used.
    fn is_code_set_key(key: &str) -> bool {
        !key.is_empty() && key.len() <= 16 && key.bytes().all(|b| b.is_ascii_graphic())
    }
    /// `[ups.code_meaning]` is CODE -> MEANING, the label each catalog entry
    /// carries. Its keys are code values (validated as such) and its VALUES
    /// are free text, because a CodeMeaning is an LO (PS3.5 Table 6.2-1) and
    /// carries the label the viewer shows.
    fn is_code_meaning_key(key: &str) -> bool {
        is_code_set_key(key)
    }
    /// `[ups.coding_schemes]` lists the Coding Scheme Designators the site
    /// accepts on the Scheduled Workitem Code Sequence besides the private
    /// `99*` ones.
    ///
    /// The designator is an SH value (PS3.5 Table 6.2-1), so 1..=16 characters
    /// of A-Z/0-9/underscore. The set is matched EXACTLY - PS3.3 8.2 makes the
    /// designator a machine key, and `LN` is not `ln` - which is why the
    /// section is listed in `section_is_code_valued` and the keys are not
    /// folded.
    fn is_coding_scheme_key(key: &str) -> bool {
        !key.is_empty()
            && key.len() <= 16
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }
    const WORKFLOW: &[&str] = &[
        "enabled",
        "dir",
        "retention_days",
        "mwl_default_limit",
        "mwl_max_responses",
    ];
    const TRANSCODE: &[&str] = &["transcode_max_concurrent", "transcode_max_megapixels"];
    const COMMITMENT: &[&str] = &["result_retention_days"];
    const AUDIT: &[&str] = &[
        "access_retention_days",
        "auth_warn_interval_min",
        "access_prune_dry_run",
    ];
    match section {
        "server" => S.contains(&key),
        "storage" => STORAGE.contains(&key),
        "logging" => LOGGING.contains(&key),
        "features" => FEATURES.contains(&key),
        "s3" => S3.contains(&key),
        "storage_tiers" => S3_TIERS.contains(&key),
        "btree" => BTREE.contains(&key),
        "query" => QUERY.contains(&key),
        "ups" => UPS.contains(&key),
        "ups.workitem_code" => is_workitem_code_key(key),
        "ups.manual_codes" => is_code_set_key(key),
        "ups.code_meaning" => is_code_meaning_key(key),
        "ups.coding_schemes" => is_coding_scheme_key(key),
        "workflow" => WORKFLOW.contains(&key),
        "transcode" => TRANSCODE.contains(&key),
        "commitment" => COMMITMENT.contains(&key),
        "audit" => AUDIT.contains(&key),
        // [global] and [recovery]: no valid keys remain (recovery excised);
        // reject all keys in those sections.
        _ => false,
    }
}

impl ConfParser {
    /// Read and parse a clarus.conf file.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("Cannot read {}: {}", path.display(), e))?;
        Self::parse(&content)
    }

    /// Parse config content string with the strict allowlist + duplicate check.
    ///
    /// Unknown `(section, key)` pairs are rejected with the offending line
    /// number, and duplicate keys are an error (no silent last-wins).
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut values: HashMap<(String, String), String> = HashMap::new();
        let mut section = String::from("global");

        for (lineno, line) in input.lines().enumerate() {
            let line = line.trim();
            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            // Section header: [name]
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_lowercase();
                if section.is_empty() {
                    return Err(format!("Line {}: empty section name", lineno + 1));
                }
                if !KNOWN_SECTIONS.contains(&section.as_str()) {
                    return Err(format!(
                        "Line {}: unknown section [{}]",
                        lineno + 1,
                        section
                    ));
                }
                continue;
            }
            // Key = value
            if let Some(eq) = line.find('=') {
                // Most keys are configuration NAMES and are case-insensitive.
                // The code-valued tables are not: their keys are Code Values,
                // which are case-sensitive machine keys (PS3.16), and folding
                // them made a site's own spelling unreachable - `DX = bone-ai`
                // with `[ups.manual_codes] bone-ai` could never match.
                let key = if section_is_code_valued(&section) {
                    line[..eq].trim().to_string()
                } else {
                    line[..eq].trim().to_lowercase()
                };
                let value = line[eq + 1..].trim().to_string();
                // Strip inline comments (value # comment)
                let value = if let Some(hash) = value.find('#') {
                    value[..hash].trim().to_string()
                } else {
                    value
                };
                if key.is_empty() {
                    return Err(format!("Line {}: empty key", lineno + 1));
                }
                // Rename hints (no deprecation window - pre-production).
                if let Some(hint) = RENAME_HINTS
                    .iter()
                    .find(|(s, k, _)| *s == section.as_str() && *k == key.as_str())
                {
                    return Err(format!(
                        "Line {}: [{}] {} renamed to {}",
                        lineno + 1,
                        section,
                        key,
                        hint.2
                    ));
                }
                if !is_known_key(&section, &key) {
                    return Err(format!(
                        "Line {}: unknown key [{}] {}",
                        lineno + 1,
                        section,
                        key
                    ));
                }
                if values
                    .insert((section.clone(), key.clone()), value)
                    .is_some()
                {
                    return Err(format!(
                        "Line {}: duplicate key [{}] {}",
                        lineno + 1,
                        section,
                        key
                    ));
                }
            } else {
                return Err(format!(
                    "Line {}: expected 'key = value', got: {}",
                    lineno + 1,
                    line
                ));
            }
        }
        Ok(Self { values })
    }

    /// Get a value by section and key (case-insensitive).
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.values
            .get(&(section.to_lowercase(), key.to_lowercase()))
            .map(|v| v.as_str())
    }

    /// Every `(key, value)` pair of one section, in file order.
    ///
    /// For sections whose KEYS are data rather than a fixed vocabulary - the
    /// `[ups.workitem_code]` table is a modality -> code map, so its keys are
    /// whatever modalities the deployment lists and cannot be enumerated in
    /// advance. The section name is compared exactly (it is already
    /// lower-cased by the parser); the pairs come out sorted by key so the
    /// result does not depend on hash iteration order.
    #[must_use]
    pub fn section(&self, section: &str) -> Vec<(&str, &str)> {
        let wanted = section.to_lowercase();
        let mut pairs: Vec<(&str, &str)> = self
            .values
            .iter()
            .filter(|((name, _), _)| *name == wanted)
            .map(|((_, key), value)| (key.as_str(), value.as_str()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        pairs
    }

    /// Get a value or return a default.
    ///
    /// Intended for keys whose fallback is meaningful on its own: strings and
    /// booleans (`s3 region`, `server host`, `require_proxy_identity`). A
    /// bounded numeric key must NOT use this: an unparseable value becomes
    /// indistinguishable from an absent one, so a range check placed after it
    /// can never fire and `abc` silently turns into the default.
    pub fn get_or<T: FromStr>(&self, section: &str, key: &str, default: T) -> T {
        self.get(section, key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
}

/// Read a bounded `u64` key, failing loudly on garbage.
///
/// Absent -> `default`. Present but unparseable -> error naming the section
/// and key, so a typo cannot hide behind the default. Parsed but outside
/// `lo..=hi` -> error naming the accepted range. Follows the `[transcode]`
/// pattern; the wording is deliberately identical to it.
pub fn parse_bounded_u64(
    p: &ConfParser,
    section: &str,
    key: &str,
    default: u64,
    lo: u64,
    hi: u64,
) -> Result<u64, String> {
    match p.get(section, key) {
        Some(raw) => {
            let v: u64 = raw
                .trim()
                .parse()
                .map_err(|_| format!("clarus.conf [{section}] {key}: '{raw}' is not an integer"))?;
            if !(lo..=hi).contains(&v) {
                return Err(format!(
                    "clarus.conf [{section}] {key}: {v} is out of range [{lo}..{hi}]"
                ));
            }
            Ok(v)
        }
        None => Ok(default),
    }
}

/// `u32` counterpart of [`parse_bounded_u64`].
pub fn parse_bounded_u32(
    p: &ConfParser,
    section: &str,
    key: &str,
    default: u32,
    lo: u32,
    hi: u32,
) -> Result<u32, String> {
    match p.get(section, key) {
        Some(raw) => {
            let v: u32 = raw
                .trim()
                .parse()
                .map_err(|_| format!("clarus.conf [{section}] {key}: '{raw}' is not an integer"))?;
            if !(lo..=hi).contains(&v) {
                return Err(format!(
                    "clarus.conf [{section}] {key}: {v} is out of range [{lo}..{hi}]"
                ));
            }
            Ok(v)
        }
        None => Ok(default),
    }
}

/// `f64` counterpart of [`parse_bounded_u64`]; the wording is "is not a
/// number" because the value is not integral.
pub fn parse_bounded_f64(
    p: &ConfParser,
    section: &str,
    key: &str,
    default: f64,
    lo: f64,
    hi: f64,
) -> Result<f64, String> {
    match p.get(section, key) {
        Some(raw) => {
            let v: f64 = raw
                .trim()
                .parse()
                .map_err(|_| format!("clarus.conf [{section}] {key}: '{raw}' is not a number"))?;
            if !(lo..=hi).contains(&v) {
                return Err(format!(
                    "clarus.conf [{section}] {key}: {v} is out of range [{lo}..{hi}]"
                ));
            }
            Ok(v)
        }
        None => Ok(default),
    }
}

// -- S3 configuration (only when feature "s3" is enabled) ------------

/// S3-compatible object storage configuration.
#[cfg(feature = "s3")]
#[derive(Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub tier: String,
    pub access_key: String,
    pub secret_key: String,
    /// Object-key prefix inside the (possibly shared) bucket.
    /// Objects are stored as `{prefix}{xx}/{yy}/{blake3}.dcm`. Default `store/`.
    pub prefix: String,
}

#[cfg(feature = "s3")]
impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("tier", &self.tier)
            .field("prefix", &self.prefix)
            .field("access_key", &self.access_key)
            .field("secret_key", &"*** HIDDEN ***")
            .finish()
    }
}

// -- Application configuration --------------------------------------

/// `[logging] log_level`: ceiling over the clog whitelist. ONE level for
/// BOTH sinks (the daily file and the console mirror) - nginx-style, one
/// `error_log` with one level. Values are case-insensitive and stored
/// canonical (lowercase). Ceiling order: error < warn < info < debug < trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Parse and canonicalize a `log_level` value (case-insensitive).
    /// Unknown values are a config error: the server refuses to start
    /// (nginx-way: a bad config is a refusal), `-t` included.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let canonical = raw.trim().to_ascii_lowercase();
        match canonical.as_str() {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(format!(
                "clarus.conf [logging] log_level: unsupported value \"{}\" (expected error|warn|info|debug|trace)",
                raw.trim()
            )),
        }
    }

    /// Canonical lowercase spelling (printed by `-t` and startup config).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// Convert to the clog severity used by the level ceiling.
    pub fn to_clog(self) -> crate::clog::Level {
        match self {
            Self::Error => crate::clog::Level::Error,
            Self::Warn => crate::clog::Level::Warn,
            Self::Info => crate::clog::Level::Info,
            Self::Debug => crate::clog::Level::Debug,
            Self::Trace => crate::clog::Level::Trace,
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Application configuration, loaded from clarus.conf.
/// No CLI flags (except --version, --config, -t).
/// No env vars. One file. chmod 600.
#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub storage_dir: String,
    /// 3008 (0400,0563) ModifyingSystem for update alibis. `None` = default
    /// `CLARUS_SERVER:{port}` (see [`Config::resolved_modifying_system`]).
    pub modifying_system: Option<String>,

    // -- [server] timeouts ----------------------------------------
    /// HTTP request timeout in seconds. 0 = disable.
    /// Default: 300 (5 minutes). Increase for slow networks / large studies.
    pub request_timeout_secs: u64,

    // -- [server] HTTP/1.1 -----------------------------------------
    /// Max seconds to read a request head before 408. 1..3600, 0 -> error.
    pub header_timeout_secs: u64,
    /// Max seconds to read a request body. 0..86400, 0 = disabled.
    pub body_timeout_secs: u64,
    /// Idle keep-alive seconds before close. 0..3600, 0 = no keep-alive.
    pub keepalive_timeout_secs: u64,
    /// Max seconds for one response write. 1..3600, 0 -> error.
    pub send_timeout_secs: u64,
    /// WADO drain ceiling during shutdown. 1..600, 0 -> error.
    pub drain_timeout_secs: u64,
    /// Concurrent connection cap. 1..4096, 0 -> error.
    pub max_connections: u32,
    /// Max JSON request body size in bytes (UPS create/update, admin ops).
    /// 1024..=67108864, 0 -> error. Default: 1048576 (1 MiB).
    pub max_json_body: u64,
    /// Max multipart part-header read cap in bytes (STOW).
    /// 1024..=67108864, 0 -> error. Default: 4194304 (4 MiB).
    pub max_part_header: u64,
    /// Trusted proxy networks (loopback by default). Identity headers only
    /// honored when `client_ip in  trusted_proxies`.
    pub trusted_proxies: Vec<CidrNet>,
    /// Networks allowed to reach the administrative surface (`/admin/v1/*` and
    /// `/dicomweb/patients`), on top of the local machine. **Empty is the
    /// default and means "the local machine only"**. Add a management subnet
    /// here to let an operator work from a browser without an SSH tunnel.
    pub admin_cidrs: Vec<CidrNet>,
    /// Networks allowed to use the Expose-Rejected surface
    /// (`?includeRejected` on the QIDO/WADO routes), on top of the local
    /// machine. **Empty is the default and means "the local machine only"**.
    ///
    /// Kept separate from `admin_cidrs` because the two answer different
    /// questions: admin access may modify configuration, whereas this one only
    /// lifts the sequestering filter on a read. A QA workstation that must
    /// review rejected instances therefore needs no administrative rights.
    ///
    /// Rejected instances are hidden from every ordinary search and retrieve,
    /// so widening this list is what makes them visible again - empty means the
    /// hiding is absolute for non-local clients.
    pub expose_rejected_cidrs: Vec<CidrNet>,
    /// Public hostname advertised in URLs. Non-empty, <= 253.
    pub hostname: String,
    /// Reject requests when a trusted proxy sent no identity headers.
    pub require_proxy_identity: bool,
    /// Whether an IOCM rejection note may hide DIAGNOSTIC images.
    ///
    /// The IOCM reasons `113037`/`113038`/`113039` have no class bound - they
    /// exist to hide the original CT/MR that a wrong-patient or
    /// wrong-worklist acquisition produced - and DICOM defines no un-reject
    /// for them (PS3.16 CID 7010, IHE RAD TF-2 4.66.4). Letting one through is
    /// therefore destructive and irreversible by any standard transaction,
    /// while a KOS is an ordinary object that any client able to reach STOW can
    /// write. It is thus an explicit, OFF-by-default opt-in rather than a
    /// consequence of some other setting.
    ///
    /// Deliberately separate from `require_proxy_identity`: that knob answers
    /// "can this deployment trust the identity headers it is given", which is
    /// a question about authentication. This one answers "does this deployment
    /// want a destructive IOCM feature at all", which no amount of correct
    /// authentication implies. Conflating the two made the feature switch on
    /// for every deployment that hardened its auth.
    pub anyclass_enable: bool,

    // -- [storage] extras ------------------------------------------
    /// Max DICOM instance size in megabytes. Default: 4096.
    ///
    /// Raised from 500 to 4096 for whole-slide microscopy: a WSI instance is
    /// routinely 1-4 GB (a gigapixel pyramid in one instance), and the old
    /// default rejected every one of them at STOW. The accepted MAXIMUM is
    /// unchanged (131072 = 128 GiB); this is only the default.
    pub max_instance_size_mb: u64,

    /// Max bytes of ONE frame a WADO `/frames/{N}` serve may buffer.
    /// Default: 67_108_864 (64 MiB), range [1_048_576 .. 1_073_741_824].
    ///
    /// `FrameRecord.compressed_len` in the manifest is an `item_len` read
    /// from the untrusted instance at STOW, so a crafted `item_len =
    /// 0xFFFFFFFF` stores a 4 GB length and one GET would otherwise request
    /// exactly that many bytes. `read_cas_range` refuses `len > max_frame_bytes`
    /// BEFORE the `vec!`, and the whole-blob fallback is gated on the stored
    /// file size, so an over-cap frame costs no allocation at all. A real WSI
    /// tile is a few hundred KiB; 64 MiB is two orders of magnitude of headroom.
    pub max_frame_bytes: u64,

    /// Max frames a SINGLE instance may declare/hold. Default: 1_000_000,
    /// range [1 .. 16_777_216] (the adapter's `MAX_FRAMES_CEILING`).
    ///
    /// F5 (0310): the manifest frame table costs 56 bytes per frame, so an
    /// instance that declares or actually holds an unbounded frame count turns
    /// a bounded upload into an amplified manifest (a 13 GB upload of 8-byte
    /// zero-length items -> ~91 GB table). STOW REJECTS such an instance (413)
    /// rather than storing it without a table, because an absent table reopens
    /// the whole-blob serve path (`read_to_end` of the instance). 1M frames x
    /// 56 B = 56 MiB max table per instance.
    pub max_frames_per_instance: u32,

    /// P1 group-commit memory guard: max accepted instances accumulated in
    /// one request's manifest batch before an honest mid-request flush.
    /// Default: 100 (matches the bridge outbox batch of 100 SOP per STOW-RS).
    pub max_batch_instances: u64,

    /// P1 group-commit memory guard: max accumulated instance bytes in one
    /// request's manifest batch before an honest mid-request flush.
    /// Default: 100 MB (matches the bridge outbox DRAM bound of ~96 MB).
    pub max_batch_bytes: u64,

    /// `[logging] log_level` ceiling over the INFO whitelist, shared by the
    /// file and the console mirror. Default: info.
    pub log_level: LogLevel,
    pub log_dir: String,
    #[cfg(feature = "s3")]
    pub s3: S3Config,
    /// Enable JPEG2000 codec (requires `--features jpeg2000` at build time).
    pub jpeg2000_enabled: bool,
    // -- BUG-003: disk full protection ------------------------------
    /// Minimum free disk space in MB before STOW rejects with HTTP 507.
    /// Default: 5000 (5 GB). Set to 0 to disable.
    pub min_free_disk_mb: u64,
    /// Minimum free disk percentage before STOW rejects with HTTP 507.
    /// Default: 5.0 (5%). Set to 0.0 to disable. Both thresholds are OR'd.
    pub min_free_disk_pct: f64,
    // -- FR-017b: Storage tier migration (cold + S3 + emergency) --
    /// Storage tier migration schedule: cold (NAS/HDD), S3 (cloud), emergency.
    /// Parsed from `[storage_tiers]` section.
    pub migration: MigrationSchedule,

    /// Path to cold storage directory (from `[storage_tiers] cold_path`).
    /// Used by main.rs to initialize the cold ContentAddressedStorage.
    pub cold_path: Option<String>,
    // -- [btree] shadow tier --------------------------------------
    /// B-tree/write-tier configuration.
    pub btree: BtreeConfig,
    // -- [query] QIDO limits --------------------------------------
    /// Default QIDO result limit when the client sends no explicit `limit`.
    /// `0` = unlimited. Default 250.
    pub query: QueryConfig,
    /// UPS-RS search defaults (separate dial from QIDO, 12.09).
    pub ups: UpsConfig,
    /// MWL + MPPS records (PS3.18 Sections 14 and 15).
    pub workflow: WorkflowConfig,
    /// Storage Commitment result retention (PS3.18 Section 13).
    pub commitment: CommitmentConfig,
    // -- [transcode] bounded pool (2608) --------------------------
    /// Bounded transcode/render concurrency + pixel budget. Optional operator
    /// overrides; defaults computed automatically (N from core count, budget
    /// a fixed 64 MP).
    pub transcode: TranscodeConfig,
    // == [audit] access journal (2808) ==================================================
    /// Access-journal retention + auth-visibility knobs (design sections
    /// 4 and 9a). The pruner only ever touches `btree.access_dir`.
    pub audit: AuditConfig,
}

// -- Fixed capacity gates (0610) ----------------------------------------
// The defaults below are TRUSTED bounds guarding allocation sizes; raising
// one is the guard-weakening edit that opens a DoS hole. Naming each as a
// module const and pinning it with `capacity_eq!` turns such an edit into a
// compile error. The values are unchanged - this only names them.

/// Default `[storage] max_frame_bytes` (64 MiB).
const DEFAULT_MAX_FRAME_BYTES: u64 = 67_108_864;
ferrite_core::capacity_eq!(DEFAULT_MAX_FRAME_BYTES, 67_108_864);

/// Default `[storage] max_frames_per_instance`.
const DEFAULT_MAX_FRAMES_PER_INSTANCE: u32 = 1_000_000;
ferrite_core::capacity_eq!(DEFAULT_MAX_FRAMES_PER_INSTANCE, 1_000_000);

/// Default `[transcode] transcode_max_megapixels` (64 MP).
const DEFAULT_MAX_MEGAPIXELS: u32 = 64;
ferrite_core::capacity_eq!(DEFAULT_MAX_MEGAPIXELS, 64);

impl Config {
    /// 3008: the (0400,0563) ModifyingSystem value embedded into update alibis.
    /// An explicit `[server] modifying_system` wins; the default is
    /// `CLARUS_SERVER:{port}` with the CONFIGURED port (in production the
    /// bound port - a listener always binds its configured port), so multiple
    /// instances on one host (8024, 8025, ...) become auto-distinct. Cluster
    /// nodes sharing a port MUST set an explicit per-node id.
    pub fn resolved_modifying_system(&self) -> String {
        match self.modifying_system.as_deref() {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => format!("CLARUS_SERVER:{}", self.port),
        }
    }

    /// Create a test configuration with all default values.
    /// Does NOT read any file - uses hardcoded development defaults.
    pub fn test_default() -> Self {
        #[cfg(feature = "s3")]
        let s3 = S3Config {
            endpoint: String::new(),
            bucket: String::new(),
            region: "us-east-1".into(),
            tier: "cold".into(),
            prefix: "store/".into(),
            access_key: String::new(),
            secret_key: String::new(),
        };

        // Every directory under the B-tree root is pointed into the system
        // temp directory, because `BtreeConfig::default()` names them
        // relatively (`audit`, `access`, ...) and a relative path resolves
        // against the process CWD - which for a `cargo test` run is the
        // source tree. `from_file` resolves these against the config file's
        // parent, so an absolute root here is the same contract, and the
        // audit journal and access journal no longer land in the checkout.
        // A caller that wants them elsewhere overrides the fields, exactly as
        // it already does for `manifest_dir` and `idx_dir`.
        let test_root = std::env::temp_dir().join("clarus-test-btree");
        let workflow_hint = test_root.to_string_lossy().to_string();
        let btree = BtreeConfig {
            root: test_root.to_string_lossy().to_string(),
            manifest_dir: test_root.join("manifests").to_string_lossy().to_string(),
            idx_dir: test_root.join("idx").to_string_lossy().to_string(),
            audit_dir: test_root.join("audit").to_string_lossy().to_string(),
            access_dir: test_root.join("access").to_string_lossy().to_string(),
            ..BtreeConfig::default()
        };

        Self {
            host: "127.0.0.1".into(),
            port: 8024,
            storage_dir: "./data".into(),
            modifying_system: None,
            max_instance_size_mb: 4096,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_frames_per_instance: DEFAULT_MAX_FRAMES_PER_INSTANCE,
            max_batch_instances: 100,
            max_batch_bytes: 100 * 1024 * 1024,
            request_timeout_secs: 300,
            header_timeout_secs: 30,
            body_timeout_secs: 60,
            keepalive_timeout_secs: 10,
            send_timeout_secs: 60,
            drain_timeout_secs: 30,
            max_connections: 512,
            max_json_body: 1_048_576,
            max_part_header: 4_194_304,
            trusted_proxies: vec![
                CidrNet::V4 {
                    addr: 0x7f00_0000,
                    prefix: 8,
                }, // 127.0.0.0/8
                CidrNet::V6 {
                    addr: 1,
                    prefix: 128,
                }, // ::1/128
            ],
            hostname: "localhost".into(),
            require_proxy_identity: false,
            anyclass_enable: false,
            admin_cidrs: Vec::new(),
            expose_rejected_cidrs: Vec::new(),
            log_level: LogLevel::Info,
            log_dir: std::env::temp_dir()
                .join("clarus-test-logs")
                .to_string_lossy()
                .to_string(),
            #[cfg(feature = "s3")]
            s3,
            jpeg2000_enabled: false,
            min_free_disk_mb: 5000,
            min_free_disk_pct: 5.0,
            cold_path: None,
            migration: MigrationSchedule::disabled(),
            btree,
            query: QueryConfig::default(),
            ups: UpsConfig::default(),
            workflow: WorkflowConfig {
                // The test default points the store at the same temp B-tree
                // root the other tiers use, so no test writes into the
                // checkout - the contract this fixture already keeps for
                // `manifest_dir` and `idx_dir`.
                btree_root_hint: workflow_hint,
                ..WorkflowConfig::default()
            },
            transcode: TranscodeConfig {
                max_concurrent: TranscodeConfig::default_concurrency(),
                max_megapixels: DEFAULT_MAX_MEGAPIXELS,
            },
            commitment: CommitmentConfig::default(),
            audit: AuditConfig::default(),
        }
    }

    /// Load configuration from a clarus.conf file.
    ///
    /// Relative paths (`storage_dir`) are resolved against the
    /// config file's parent directory - NOT the process CWD - so the whole
    /// `clarus/` folder can be copied to another machine and work immediately.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let p = ConfParser::from_file(path)?;
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

        /// Resolve a potentially-relative path against `base_dir`.
        /// If the path is already absolute, return it unchanged.
        fn resolve(base: &Path, raw: &str) -> String {
            let p = Path::new(raw);
            if p.is_absolute() {
                raw.to_string()
            } else {
                base.join(p).to_str().unwrap_or(raw).to_string()
            }
        }

        #[cfg(feature = "s3")]
        let s3 = S3Config {
            endpoint: p.get("s3", "endpoint").unwrap_or("").to_string(),
            bucket: p.get("s3", "bucket").unwrap_or("").to_string(),
            region: p.get_or("s3", "region", "us-east-1".to_string()),
            tier: p.get_or("s3", "tier", "cold".to_string()),
            prefix: p.get_or("s3", "prefix", "store/".to_string()),
            access_key: p.get("s3", "access_key").unwrap_or("").to_string(),
            secret_key: p.get("s3", "secret_key").unwrap_or("").to_string(),
        };

        let cfg = Self {
            host: p.get_or("server", "host", "127.0.0.1".to_string()),
            port: {
                // Parsed as u64 first, so a well-formed number that merely
                // exceeds the u16 range reports "out of range" rather than the
                // misleading "is not an integer" a direct u16 parse gives.
                let v: u64 = match p.get("server", "port") {
                    Some(raw) => raw.trim().parse().map_err(|_| {
                        format!("clarus.conf [server] port: '{raw}' is not an integer")
                    })?,
                    None => 8024u64,
                };
                if !(1..=65_535).contains(&v) {
                    return Err(format!(
                        "clarus.conf [server] port: {v} is out of range [1..65535]"
                    ));
                }
                v as u16
            },
            storage_dir: resolve(
                base_dir,
                &p.get_or("storage", "storage_dir", "./data".to_string()),
            ),

            // -- [storage] extras -------------------------------------
            // Default 4096 MB: a whole-slide microscopy instance is routinely
            // 1-4 GB, so the old 500 MB default rejected every WSI at STOW.
            // The accepted MAXIMUM is unchanged (131072 = 128 GiB).
            max_instance_size_mb: parse_bounded_u64(
                &p,
                "storage",
                "max_instance_size_mb",
                4096,
                1,
                131_072,
            )?,

            // -- WADO frame-range cap (alloc-DoS guard, 0310) -----------
            // One frame of ONE instance; a crafted manifest
            // `compressed_len` must not become an allocation. `parse_size`
            // gives the k/m suffixes other capacity knobs use.
            max_frame_bytes: parse_size(
                p.get("storage", "max_frame_bytes").unwrap_or("64m"),
                1_048_576,     // 1 MiB
                1_073_741_824, // 1 GiB
            )
            .map_err(|e| format!("clarus.conf [storage] max_frame_bytes: {e}"))?,

            // -- WADO frame-count cap (F5, 0310) ------------------------
            // Reject-at-STOW for an instance that declares/holds more frames
            // than this. Bounded to the adapter ceiling so the manifest table
            // can never exceed ~900 MiB even if an operator sets a large value.
            max_frames_per_instance: parse_bounded_u32(
                &p,
                "storage",
                "max_frames_per_instance",
                1_000_000,
                1,
                clarus_adapters::dicom_rs::frame::MAX_FRAMES_CEILING,
            )?,

            // -- P1 group-commit memory guards --------------------------
            max_batch_instances: parse_bounded_u64(
                &p,
                "storage",
                "max_batch_instances",
                100,
                1,
                10_000_000,
            )?,
            max_batch_bytes: parse_size(
                p.get("storage", "max_batch_bytes").unwrap_or("100m"),
                1024,
                10_737_418_240, // 1k..10GiB
            )
            .map_err(|e| format!("clarus.conf [storage] max_batch_bytes: {e}"))?,

            // -- BUG-003: disk full protection ------------------------
            min_free_disk_mb: parse_bounded_u64(
                &p,
                "storage",
                "min_free_disk_mb",
                5000,
                0,
                1_048_576,
            )?,
            min_free_disk_pct: parse_bounded_f64(
                &p,
                "storage",
                "min_free_disk_pct",
                5.0,
                0.0,
                100.0,
            )?,

            // `request_timeout_secs` is NOT parsed here: it is caught earlier
            // as a RENAME_HINT and rejected with a pointer to
            // `header_timeout_secs` / `body_timeout_secs`. The field survives
            // only for the split defaults and stays at the documented 300.
            request_timeout_secs: 300,

            // -- [server] HTTP/1.1 ---------------------------------
            header_timeout_secs: parse_bounded_u64(
                &p,
                "server",
                "header_timeout_secs",
                30,
                1,
                3600,
            )?,
            body_timeout_secs: parse_bounded_u64(&p, "server", "body_timeout_secs", 60, 0, 86_400)?,
            keepalive_timeout_secs: parse_bounded_u64(
                &p,
                "server",
                "keepalive_timeout_secs",
                10,
                0,
                3600,
            )?,
            send_timeout_secs: parse_bounded_u64(&p, "server", "send_timeout_secs", 60, 1, 3600)?,
            drain_timeout_secs: parse_bounded_u64(&p, "server", "drain_timeout_secs", 30, 1, 600)?,
            max_connections: parse_bounded_u32(&p, "server", "max_connections", 512, 1, 4096)?,
            max_json_body: parse_size(
                p.get("server", "max_json_body").unwrap_or("1m"),
                1024,
                67_108_864, // 1k..64m
            )
            .map_err(|e| format!("clarus.conf [server] max_json_body: {e}"))?,
            max_part_header: parse_size(
                p.get("server", "max_part_header").unwrap_or("4m"),
                1024,
                67_108_864, // 1k..64m
            )
            .map_err(|e| format!("clarus.conf [server] max_part_header: {e}"))?,
            trusted_proxies: {
                let raw = p
                    .get("server", "trusted_proxies")
                    .unwrap_or("127.0.0.0/8, ::1/128");
                parse_trusted_proxies(raw)
                    .map_err(|e| format!("clarus.conf [server] trusted_proxies: {e}"))?
            },
            hostname: {
                let v = p.get_or("server", "hostname", "localhost".to_string());
                if v.is_empty() || v.len() > 253 {
                    return Err(
                        "clarus.conf [server] hostname: must be non-empty and ≤ 253 chars"
                            .to_string(),
                    );
                }
                v
            },
            require_proxy_identity: p.get_or("server", "require_proxy_identity", false),
            anyclass_enable: p.get_or("server", "anyclass_enable", false),
            // Administrative surface networks. Absent = empty = local machine
            // only. A typo in one entry fails startup rather than silently
            // widening or narrowing access.
            admin_cidrs: parse_cidr_list(p.get("server", "admin_cidrs").unwrap_or(""))
                .map_err(|e| format!("clarus.conf [server] admin_cidrs: {e}"))?,
            // Expose-Rejected networks. Same empty-is-the-restrictive-default
            // rule as admin_cidrs: absent means non-local clients cannot ask
            // for rejected instances at all.
            expose_rejected_cidrs: parse_cidr_list(
                p.get("server", "expose_rejected_cidrs").unwrap_or(""),
            )
            .map_err(|e| format!("clarus.conf [server] expose_rejected_cidrs: {e}"))?,
            modifying_system: p.get("server", "modifying_system").map(|s| s.to_string()),

            // -- Storage tiers ---------------------------------------
            cold_path: p
                .get("storage_tiers", "cold_path")
                .map(|s| resolve(base_dir, s)),
            migration: parse_migration_schedule(&p, base_dir)?,

            // -- [btree] -----------------------------------------
            // The default sub-paths (`manifest_dir`, `idx_dir`, `audit_dir`)
            // are built from the RAW root value and then resolved exactly
            // ONCE against `base_dir` - same as `root` itself. Building them
            // from the already-resolved `root` (absolute) and resolving again
            // used to DOUBLE-join when `base_dir` is relative (config given
            // as a relative path), producing `tmp/tmp/./data/...`.
            btree: {
                let root_raw = p.get_or("btree", "root", "./data".to_string());
                let root = resolve(base_dir, &root_raw);
                let manifest_raw =
                    p.get_or("btree", "manifest_dir", format!("{root_raw}/manifests"));
                let manifest_dir = resolve(base_dir, &manifest_raw);
                let idx_raw = p.get_or("btree", "idx_dir", format!("{root_raw}/idx"));
                let idx_dir = resolve(base_dir, &idx_raw);
                let audit_raw = p.get_or("btree", "audit_dir", format!("{root_raw}/audit"));
                let audit_dir = resolve(base_dir, &audit_raw);
                let access_raw = p.get_or("btree", "access_dir", format!("{root_raw}/access"));
                let access_dir = resolve(base_dir, &access_raw);
                // Bounded rather than `get_or`: a typo in either key used to
                // fall back to the default silently, and a value of 0 for the
                // GC interval would spin the sweep continuously. The ranges are
                // wide enough that only a mistake can leave them.
                let preload_days: usize = parse_bounded_u64(&p, "btree", "preload_days", 7, 1, 3650)
                    .map_err(|e| format!("clarus.conf [btree] preload_days: {e}"))?
                    as usize;
                let orphan_gc_interval_sec =
                    parse_bounded_u64(&p, "btree", "orphan_gc_interval_sec", 3600, 1, 86_400)?;
                BtreeConfig {
                    root,
                    manifest_dir,
                    idx_dir,
                    audit_dir,
                    access_dir,
                    preload_days,
                    orphan_gc_interval_sec,
                }
            },

            // -- [query] default QIDO limit -------------------------
            query: {
                let default_limit: u64 = p.get_or("query", "default_limit", 250u64);
                let case_sensitive_pn: bool = p.get_or("query", "case_sensitive_pn", true);
                let fuzzymatch_override: bool = p.get_or("query", "fuzzymatch_override", false);
                QueryConfig {
                    default_limit,
                    case_sensitive_pn,
                    fuzzymatch_override,
                }
            },

            // -- [ups] UPS-RS search default limit + Task Producer (12.09) --
            ups: {
                let default_limit: u64 = p.get_or("ups", "default_limit", 100u64);
                let task_producer: bool = p.get_or("ups", "task_producer", false);
                // Validated, not clamped: a typo must not silently buy a
                // different retry budget than the operator wrote. Parsed
                // explicitly rather than through `get_or`, because `get_or`
                // treats an unparseable value as absent and would let
                // `push_attempts = many` fall back to the default unnoticed.
                let push_attempts: u32 = match p.get("ups", "push_attempts") {
                    Some(raw) => raw.parse().map_err(|_| {
                        format!(
                            "clarus.conf [ups] push_attempts: {raw:?} is not an integer in [1..{}]",
                            UpsConfig::MAX_PUSH_ATTEMPTS
                        )
                    })?,
                    None => UpsConfig::DEFAULT_PUSH_ATTEMPTS,
                };
                if !(1..=UpsConfig::MAX_PUSH_ATTEMPTS).contains(&push_attempts) {
                    return Err(format!(
                        "clarus.conf [ups] push_attempts: {push_attempts} is out of range [1..{}]",
                        UpsConfig::MAX_PUSH_ATTEMPTS
                    ));
                }
                // Same reasoning as push_attempts: validated, not clamped, and
                // 0 is a legal value meaning "keep final workitems forever".
                let final_state_retention_days: u64 =
                    match p.get("ups", "final_state_retention_days") {
                        Some(raw) => raw.parse().map_err(|_| {
                            format!(
                                "clarus.conf [ups] final_state_retention_days: {raw:?} is not \
                                 an integer in [0..{}]",
                                UpsConfig::MAX_FINAL_STATE_RETENTION_DAYS
                            )
                        })?,
                        None => UpsConfig::DEFAULT_FINAL_STATE_RETENTION_DAYS,
                    };
                if final_state_retention_days > UpsConfig::MAX_FINAL_STATE_RETENTION_DAYS {
                    return Err(format!(
                        "clarus.conf [ups] final_state_retention_days: {final_state_retention_days} \
                         is out of range [0..{}]",
                        UpsConfig::MAX_FINAL_STATE_RETENTION_DAYS
                    ));
                }
                // Same reasoning again: validated, not clamped, and 0 is a
                // legal value meaning "do not honour Deletion Locks".
                let deletion_lock_ttl: u64 = match p.get("ups", "deletion_lock_ttl") {
                    Some(raw) => raw.parse().map_err(|_| {
                        format!(
                            "clarus.conf [ups] deletion_lock_ttl: {raw:?} is not an integer \
                             in [0..{}]",
                            UpsConfig::MAX_DELETION_LOCK_TTL_HOURS
                        )
                    })?,
                    None => UpsConfig::DEFAULT_DELETION_LOCK_TTL_HOURS,
                };
                if deletion_lock_ttl > UpsConfig::MAX_DELETION_LOCK_TTL_HOURS {
                    return Err(format!(
                        "clarus.conf [ups] deletion_lock_ttl: {deletion_lock_ttl} is out of \
                         range [0..{}]",
                        UpsConfig::MAX_DELETION_LOCK_TTL_HOURS
                    ));
                }
                // The code table is a MODALITY -> Code Value(s) map. It is
                // read as a whole section because its keys are data; each key
                // is validated as a DICOM CS token at parse time (see
                // `is_workitem_code_key`), so a typo is refused with a line
                // number instead of silently becoming a modality that never
                // matches a study.
                //
                // The VALUE is a comma-separated list, because the catalog is
                // one entry per AI task and a modality can need several. A
                // repeated key cannot express that: the strict parser refuses
                // duplicate keys by design (no silent last-wins), so `DX` twice
                // is an error rather than two records. One key, one list.
                let mut workitem_codes: Vec<(String, String)> = Vec::new();
                let mut workitem_code_fallback: Option<String> = None;
                let mut workitem_code_meaning: Option<String> = None;
                // The modalities already SEEN as keys, upper-cased. Duplicate
                // detection is per KEY, not per (modality, code) pair: one key
                // legitimately carries several codes (`DX = A,B`), so the pair
                // table cannot tell "a second code" from "a second key".
                let mut seen_modalities: Vec<String> = Vec::new();
                for (key, value) in p.section("ups.workitem_code") {
                    // Reserved words first, case-insensitively: the parser
                    // keeps this section's keys as written, so `Fallback = X`
                    // must configure the fallback, not define a modality
                    // called Fallback.
                    //
                    // A SECOND spelling of the same reserved word is refused.
                    // The parser's duplicate check compares raw keys, and this
                    // section's keys are not folded, so `fallback` and
                    // `Fallback` both reach this loop; without this check the
                    // assignment below would run twice and the last one in the
                    // section's sorted order would silently win - a config the
                    // operator cannot reason about, in exactly the place where
                    // one of the two values is discarded.
                    if key.eq_ignore_ascii_case("fallback") {
                        if workitem_code_fallback.is_some() {
                            return Err(format!(
                                "clarus.conf [ups.workitem_code] {key}: duplicate reserved key \
                                 (fallback, case-insensitive)"
                            ));
                        }
                        workitem_code_fallback = Some(value.to_string());
                        continue;
                    }
                    if key.eq_ignore_ascii_case("meaning") {
                        if workitem_code_meaning.is_some() {
                            return Err(format!(
                                "clarus.conf [ups.workitem_code] {key}: duplicate reserved key \
                                 (meaning, case-insensitive)"
                            ));
                        }
                        workitem_code_meaning = Some(value.to_string());
                        continue;
                    }
                    let modality = key;
                    // Modality keys are upper-cased so the table is a SET:
                    // `dx` and `DX` are one modality, and a file listing both
                    // is a duplicate rather than two entries. The parser's own
                    // duplicate check compares raw keys, and this section's
                    // keys are not folded, so both reach this loop.
                    let modality_key = modality.to_ascii_uppercase();
                    if seen_modalities.iter().any(|have| have == &modality_key) {
                        return Err(format!(
                            "clarus.conf [ups.workitem_code] {modality}: duplicate modality \
                             (a modality appears at most once; list its codes comma-separated)"
                        ));
                    }
                    seen_modalities.push(modality_key.clone());
                    for value in value.split(',') {
                        let value = value.trim();
                        if value.is_empty() {
                            return Err(format!(
                                "clarus.conf [ups.workitem_code] {modality}: a code value \
                                 in the list is empty (`{value_raw}`)",
                                value_raw = value
                            ));
                        }
                        // A Code Value is an SH (PS3.5 Table 6.2-1), and it is
                        // a machine key: 16 characters, no free text. A
                        // Cyrillic or spaced value would encode but could never
                        // be matched by a rule written in the same file.
                        if value.len() > 16 || !value.bytes().all(|b| b.is_ascii_graphic()) {
                            return Err(format!(
                                "clarus.conf [ups.workitem_code] {modality}: {value:?} is \
                                 not a code value (SH: at most 16 printable ASCII chars)"
                            ));
                        }
                        // Modality keys were upper-cased above, so the table
                        // is a SET and `dx`/`DX` cannot both be listed. The
                        // CODE keeps its case - it is matched exactly,
                        // including by `[ups.manual_codes]`.
                        workitem_codes.push((modality_key.clone(), value.to_string()));
                    }
                }
                if workitem_codes.is_empty() && workitem_code_fallback.is_none() {
                    // No table at all: the shipped defaults, so switching the
                    // capability on works without also writing a table.
                    workitem_codes = UpsConfig::DEFAULT_WORKITEM_CODES
                        .iter()
                        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                        .collect();
                }
                // `fallback` stays as parsed: `None` means the site did not
                // opt in, and an unlisted modality then mints NOTHING. There is
                // no shipped default, deliberately - see the field's doc.
                let workitem_code_meaning = workitem_code_meaning
                    .unwrap_or_else(|| UpsConfig::DEFAULT_WORKITEM_CODE_MEANING.to_string());
                // The AI station is deliberately OPTIONAL and absent by
                // default: an empty or missing key writes no (0040,0001) at
                // all, which is exactly the wire shape this server produced
                // before the catalog, so switching the catalog on does not
                // change what an existing deployment sends.
                let ai_station_aet = match p.get("ups", "ai_station_aet") {
                    Some(raw) => {
                        let raw = raw.trim();
                        if raw.is_empty() {
                            None
                        } else if raw.len() > UpsConfig::MAX_AE_TITLE {
                            return Err(format!(
                                "clarus.conf [ups] ai_station_aet: {raw:?} is longer than {} \
                                 characters (AE, PS3.5 Table 6.2-1)",
                                UpsConfig::MAX_AE_TITLE
                            ));
                        } else {
                            Some(raw.to_string())
                        }
                    }
                    None => None,
                };
                // `non_task_modalities` is a CSV list of modalities that are
                // AI OUTPUTS. Absent -> the shipped list; present but EMPTY ->
                // an empty list, which is a deliberate choice ("mint for
                // everything not excluded otherwise") and therefore distinct
                // from absent. An empty ELEMENT inside a non-empty list is a
                // typo (`DX,,CT`) and is refused rather than silently dropped.
                let non_task_modalities: Vec<String> = match p.get("ups", "non_task_modalities") {
                    None => {
                        // The shipped set, sorted to match `Default` and the
                        // parsed branch below: one canonical order for a set.
                        let mut m: Vec<String> = UpsConfig::DEFAULT_NON_TASK_MODALITIES
                            .iter()
                            .map(|m| (*m).to_string())
                            .collect();
                        m.sort_unstable();
                        m
                    }
                    Some(raw) => {
                        let raw = raw.trim();
                        if raw.is_empty() {
                            Vec::new()
                        } else {
                            let mut out: Vec<String> = Vec::new();
                            for part in raw.split(',') {
                                let part = part.trim();
                                if part.is_empty() {
                                    return Err(
                                        "clarus.conf [ups] non_task_modalities: an entry is \
                                         empty (the list is comma-separated, e.g. SR,PR,SEG)"
                                            .to_string(),
                                    );
                                }
                                // Same rule as `[ups.workitem_code]` keys: a
                                // modality is a DICOM CS value, so a spaced or
                                // Cyrillic token could never match a study.
                                if part.len() > 16 || !part.bytes().all(|b| b.is_ascii_graphic()) {
                                    return Err(format!(
                                        "clarus.conf [ups] non_task_modalities: {part:?} is not \
                                         a modality (CS: at most 16 printable ASCII chars)"
                                    ));
                                }
                                out.push(part.to_ascii_uppercase());
                            }
                            out.sort_unstable();
                            out.dedup();
                            out
                        }
                    }
                };
                // `[ups.manual_codes]` is a SET: the keys are the parked code
                // values and the values carry no meaning.
                //
                // The keys are kept AS WRITTEN. A Code Value is a
                // case-sensitive machine key (PS3.16 code values are agreed
                // strings, not folded identifiers), and the workitem_code
                // values they are compared against keep their case too. Upper-
                // casing only this side made `DX = bone-ai` with
                // `[ups.manual_codes] bone-ai` a MISMATCH, so the parked code
                // was minted anyway - the exact outcome the park exists to
                // prevent.
                // Parked codes are DECLARATIVE: `catalog_for_modalities` drops
                // them, so the mint no longer creates a workitem for one.
                let mut manual_codes: Vec<String> = Vec::new();
                for (key, _value) in p.section("ups.manual_codes") {
                    if key.is_empty() {
                        return Err(
                            "clarus.conf [ups.manual_codes]: a code value is empty".to_string()
                        );
                    }
                    manual_codes.push(key.to_string());
                }
                manual_codes.sort_unstable();
                manual_codes.dedup();
                // `[ups.code_meaning]` is CODE -> MEANING: the label the
                // viewer's dropdown shows. A meaning is free text (LO), a code
                // value is a machine key, so only the KEY is validated.
                let mut code_meanings: Vec<(String, String)> = Vec::new();
                for (key, value) in p.section("ups.code_meaning") {
                    let value = value.trim();
                    if value.is_empty() {
                        return Err(format!(
                            "clarus.conf [ups.code_meaning] {key}: the meaning is empty; a \
                             catalog entry needs a label or the viewer shows a blank row"
                        ));
                    }
                    if value.len() > 64 {
                        return Err(format!(
                            "clarus.conf [ups.code_meaning] {key}: the meaning is longer than 64 \
                             characters (LO, PS3.5 Table 6.2-1)"
                        ));
                    }
                    // The KEY is stored AS WRITTEN. A Code Value is a
                    // case-sensitive machine key (PS3.16), and the parser
                    // already keeps this section's spelling; `meaning_for_code`
                    // compares the key with the catalog's code exactly. Upper-
                    // casing it here made `bone-ai = Bone age` unreachable by
                    // the code `bone-ai` - the row existed and could never be
                    // looked up.
                    code_meanings.push((key.to_string(), value.to_string()));
                }
                code_meanings.sort_by(|a, b| a.0.cmp(&b.0));
                code_meanings.dedup_by(|a, b| a.0 == b.0);
                // `[ups.coding_schemes]` is a SET: the keys are the Coding
                // Scheme Designators the site accepts besides the private
                // `99*` ones, and the values carry no meaning.
                //
                // The keys are kept AS WRITTEN. PS3.3 8.2 makes the designator
                // a machine key, so `LN` and `ln` are different identifiers and
                // folding would accept a typo.
                let mut coding_schemes: Vec<String> = Vec::new();
                for (key, _value) in p.section("ups.coding_schemes") {
                    if key.is_empty() {
                        return Err(
                            "clarus.conf [ups.coding_schemes]: a designator is empty".to_string()
                        );
                    }
                    coding_schemes.push(key.to_string());
                }
                coding_schemes.sort_unstable();
                coding_schemes.dedup();
                UpsConfig {
                    default_limit,
                    task_producer,
                    push_attempts,
                    final_state_retention_days,
                    deletion_lock_ttl,
                    workitem_codes,
                    workitem_code_fallback,
                    non_task_modalities,
                    workitem_code_meaning,
                    ai_station_aet,
                    manual_codes,
                    code_meanings,
                    coding_schemes,
                }
            },

            // -- [workflow] MWL + MPPS records (PS3.18 14/15) -------
            // Numerics are validated, not clamped (the [ups] precedent): a
            // typo must not silently buy a different bound than the operator
            // wrote, and `parse_bounded_u64` names the accepted range.
            workflow: {
                let enabled: bool = p.get_or("workflow", "enabled", false);
                // An explicit dir resolves against the config file's parent,
                // exactly as `[btree] root` does; absent or empty leaves the
                // default derivation to `resolved_dir`.
                let dir = match p.get("workflow", "dir") {
                    Some(raw) if !raw.trim().is_empty() => resolve(base_dir, raw.trim()),
                    _ => String::new(),
                };
                let retention_days = parse_bounded_u64(
                    &p,
                    "workflow",
                    "retention_days",
                    WorkflowConfig::DEFAULT_RETENTION_DAYS,
                    0,
                    WorkflowConfig::MAX_RETENTION_DAYS,
                )?;
                // 0 = unlimited is legal here (the [ups] default_limit rule);
                // the hard ceiling below is what actually bounds the answer.
                let mwl_default_limit = parse_bounded_u64(
                    &p,
                    "workflow",
                    "mwl_default_limit",
                    WorkflowConfig::DEFAULT_MWL_DEFAULT_LIMIT,
                    0,
                    u64::MAX,
                )?;
                let mwl_max_responses = parse_bounded_u64(
                    &p,
                    "workflow",
                    "mwl_max_responses",
                    WorkflowConfig::DEFAULT_MWL_MAX_RESPONSES,
                    WorkflowConfig::MIN_MWL_MAX_RESPONSES,
                    WorkflowConfig::MAX_MWL_MAX_RESPONSES,
                )?;
                WorkflowConfig {
                    enabled,
                    dir,
                    retention_days,
                    mwl_default_limit,
                    mwl_max_responses,
                    // The same resolution the `[btree]` block above applies to
                    // its root, repeated here because a struct-literal field
                    // cannot read a sibling field's value. One expression, so
                    // the two cannot drift into two different defaults.
                    btree_root_hint: resolve(
                        base_dir,
                        &p.get_or("btree", "root", "./data".to_string()),
                    ),
                }
            },

            // -- [transcode] bounded pool (2608) --------------------
            transcode: TranscodeConfig::parse(&p)?,

            // -- [commitment] Storage Commitment retention -----------
            // Same reasoning as the [ups] retention dial: validated, not
            // clamped, and 0 is legal and means "keep results forever".
            commitment: {
                let result_retention_days = match p.get("commitment", "result_retention_days") {
                    Some(raw) => raw.parse().map_err(|_| {
                        format!(
                            "clarus.conf [commitment] result_retention_days: {raw:?} is not \
                             an integer in [0..{}]",
                            CommitmentConfig::MAX_RESULT_RETENTION_DAYS
                        )
                    })?,
                    None => CommitmentConfig::DEFAULT_RESULT_RETENTION_DAYS,
                };
                if result_retention_days > CommitmentConfig::MAX_RESULT_RETENTION_DAYS {
                    return Err(format!(
                        "clarus.conf [commitment] result_retention_days: {result_retention_days} \
                         is out of range [0..{}]",
                        CommitmentConfig::MAX_RESULT_RETENTION_DAYS
                    ));
                }
                CommitmentConfig {
                    result_retention_days,
                }
            },

            // == [audit] access journal (2808) ==================
            audit: AuditConfig {
                access_retention_days: p.get_or("audit", "access_retention_days", 1095u64),
                // 0 / negative / non-numeric -> WARN + default 60 (owner
                // 28.08): a wrong knob must be visible but never fatal.
                //
                // An ABSENT key is not a wrong knob - most deployments never
                // set it, and warning at every boot trains an operator to
                // ignore WARN lines. Only a present-but-invalid value is a
                // configuration error worth a line.
                auth_warn_interval_min: {
                    let raw = p.get("audit", "auth_warn_interval_min");
                    match raw.and_then(|v| v.parse::<u64>().ok()) {
                        Some(n) if n >= 1 => n,
                        _ => {
                            if let Some(raw) = raw {
                                crate::clog_warn!(
                                    "clarus.conf [audit] auth_warn_interval_min: invalid value \"{raw}\" (expected minutes >= 1); using default 60",
                                );
                            }
                            60
                        }
                    }
                },
                access_prune_dry_run: p.get_or("audit", "access_prune_dry_run", false),
            },

            log_level: {
                let raw = p.get_or("logging", "log_level", "info".to_string());
                LogLevel::parse(&raw)?
            },
            log_dir: p.get_or("logging", "log_dir", "./logs".to_string()),
            #[cfg(feature = "s3")]
            s3,
            jpeg2000_enabled: p.get_or("features", "jpeg2000", false),
        };

        // Warn if [s3] section is present but s3 feature is not compiled in.
        #[cfg(not(feature = "s3"))]
        {
            let has_s3_config = p.get("s3", "endpoint").is_some()
                || p.get("s3", "bucket").is_some()
                || p.get("storage_tiers", "s3_schedule").is_some()
                || p.get("storage_tiers", "s3_min_age_days").is_some();
            if has_s3_config {
                crate::clog_warn!(
                    "S3 storage is configured ([s3] or [storage_tiers] s3_* in clarus.conf), \
                     but the 's3' feature is not compiled in. \
                     Rebuild with: cargo build --features s3"
                );
            }
        }

        // Warn if [transcode] keys are set but the feature is not compiled
        // in (same contract as S3: starts normally, keys inert).
        #[cfg(not(feature = "transcode"))]
        {
            let has_tc_config = p.get("transcode", "transcode_max_concurrent").is_some()
                || p.get("transcode", "transcode_max_megapixels").is_some();
            if has_tc_config {
                crate::clog_warn!(
                    "[transcode] is configured in clarus.conf, but the 'transcode' feature \
                     is not compiled in - transferSyntax requests will be served with \
                     original bytes. Rebuild with: cargo build --features transcode"
                );
            }
        }

        Ok(cfg)
    }
}

// -- Migration schedule parser --------------------------------------------

/// Parse `[storage_tiers]` section into a `MigrationSchedule`.
///
/// # Validation
/// - Cron expressions must be valid 5-field format.
/// - `min_age_days` must be in [1, 36500].
/// - `emergency_check_interval_mins` must be in [1, 60].
/// - At least one backend must be configured if emergency is enabled.
fn parse_migration_schedule(p: &ConfParser, _base_dir: &Path) -> Result<MigrationSchedule, String> {
    // -- Emergency migration --------------------------------------
    // Bounded parse, not `.parse().ok().unwrap_or(default)`: a typo in the
    // config silently became the default and the operator never learned that
    // the value they wrote was not the value in force.
    let emergency_free_gb =
        parse_bounded_u32(p, "storage_tiers", "emergency_free_gb", 0, 0, u32::MAX)?;

    let emergency_check_interval_mins = parse_bounded_u64(
        p,
        "storage_tiers",
        "emergency_check_interval_mins",
        5,
        1,
        60,
    )?;
    let emergency_check_secs = emergency_check_interval_mins * 60;

    // -- Cold (NAS/HDD) backend -----------------------------------
    let cold_path_raw = p.get("storage_tiers", "cold_path");

    let cold = if cold_path_raw.is_some() {
        let cold_min_age_days =
            parse_bounded_u32(p, "storage_tiers", "cold_min_age_days", 90, 1, 36500)?;

        let cold_schedule =
            parse_optional_cron(p.get("storage_tiers", "cold_schedule"), "cold_schedule")?;

        Some(BackendSchedule {
            schedule: cold_schedule,
            min_age_days: cold_min_age_days,
        })
    } else {
        None
    };

    // -- S3 backend -----------------------------------------------
    #[cfg(feature = "s3")]
    let s3 = {
        let s3_configured = p.get("s3", "endpoint").is_some() && p.get("s3", "bucket").is_some();

        if s3_configured {
            let s3_min_age_days =
                parse_bounded_u32(p, "storage_tiers", "s3_min_age_days", 180, 1, 36500)?;

            let s3_schedule =
                parse_optional_cron(p.get("storage_tiers", "s3_schedule"), "s3_schedule")?;

            Some(BackendSchedule {
                schedule: s3_schedule,
                min_age_days: s3_min_age_days,
            })
        } else {
            None
        }
    };
    #[cfg(not(feature = "s3"))]
    let s3: Option<BackendSchedule> = {
        // Warn if user has S3 config but forgot --features s3.
        let s3_has_endpoint = p.get("s3", "endpoint").is_some();
        let s3_has_schedule = p.get("storage_tiers", "s3_schedule").is_some();
        let s3_has_min_age = p.get("storage_tiers", "s3_min_age_days").is_some();
        if s3_has_endpoint || s3_has_schedule || s3_has_min_age {
            crate::clog_warn!(
                "S3 storage is configured in clarus.conf, but the 's3' feature is not compiled in. \
                 Rebuild with: cargo build --features s3"
            );
        }
        None
    };

    // -- Cross-validation -----------------------------------------
    if emergency_free_gb > 0 && cold.is_none() && s3.is_none() {
        return Err(
            "[storage_tiers] emergency_free_gb is set, but no cold_path or S3 is configured. \
             Emergency migration needs at least one target."
                .to_string(),
        );
    }

    Ok(MigrationSchedule {
        emergency_free_gb,
        emergency_check_secs,
        cold,
        #[cfg(feature = "s3")]
        s3,
    })
}

/// Parse an optional cron field. Returns `Ok(None)` if the value is missing or empty.
/// Returns an error if the value is present but invalid.
fn parse_optional_cron(
    raw: Option<&str>,
    field_name: &str,
) -> Result<Option<CronSchedule>, String> {
    match raw {
        Some(val) if !val.is_empty() => CronSchedule::parse(val)
            .map(Some)
            .map_err(|e| format!("[storage_tiers] {}: {}", field_name, e)),
        _ => Ok(None),
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut ds = f.debug_struct("Config");
        ds.field("host", &self.host)
            .field("port", &self.port)
            .field("storage_dir", &self.storage_dir)
            .field("log_level", &self.log_level)
            .field("jpeg2000_enabled", &self.jpeg2000_enabled)
            .field("max_instance_size_mb", &self.max_instance_size_mb)
            .field("max_frame_bytes", &self.max_frame_bytes)
            .field("max_frames_per_instance", &self.max_frames_per_instance)
            .field("max_batch_instances", &self.max_batch_instances)
            .field("max_batch_bytes", &self.max_batch_bytes);
        ds.field("cold_path", &self.cold_path);
        ds.field("migration", &self.migration);
        #[cfg(feature = "s3")]
        ds.field("s3", &self.s3);
        ds.field("header_timeout_secs", &self.header_timeout_secs)
            .field("body_timeout_secs", &self.body_timeout_secs)
            .field("keepalive_timeout_secs", &self.keepalive_timeout_secs)
            .field("send_timeout_secs", &self.send_timeout_secs)
            .field("drain_timeout_secs", &self.drain_timeout_secs)
            .field("max_connections", &self.max_connections)
            .field("hostname", &self.hostname)
            .field("require_proxy_identity", &self.require_proxy_identity)
            .field("anyclass_enable", &self.anyclass_enable)
            .field("anyclass_enable", &self.anyclass_enable);
        ds.finish()
    }
}

// -- File permission verification (chmod 600) -----------------------

/// Verify that the config file is not readable by group or others.
/// On Unix: check mode bits; if insecure, best-effort self-fix to 600 with
/// a WARN (a hard fail for a fixable condition was
/// worse UX than a warning), and only fail when chmod itself fails (e.g.
/// read-only mount). On Windows: warn only (NTFS ACL model, no chmod
/// equivalent - the asymmetry is intentional).
pub fn verify_config_permissions(path: &Path) -> Result<(), String> {
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("Cannot stat {}: {}", path.display(), e))?;
        let mode = meta.permissions().mode();
        // Check that group and other have no read/write/execute.
        if mode & 0o077 != 0 {
            match std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                Ok(()) => crate::clog_warn!(
                    "SECURITY: {} had insecure permissions {:o}; fixed to 600.",
                    path.display(),
                    mode & 0o777
                ),
                Err(e) => {
                    return Err(format!(
                        "SECURITY: {} has insecure permissions {:o} and chmod 600 failed: {e}. \
                         Run: chmod 600 {}",
                        path.display(),
                        mode & 0o777,
                        path.display()
                    ));
                }
            }
        } else {
            crate::clog_info!("Config file permissions OK: {:o}", mode & 0o777);
        }
    }
    #[cfg(not(target_family = "unix"))]
    {
        let _ = path;
        crate::clog_warn!(
            "Config file permission check skipped on this platform. \
             Ensure {} is readable only by the service account.",
            path.display()
        );
    }
    Ok(())
}

// -- Tests ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ConfParser tests -----------------------------------------

    #[test]
    fn test_parse_basic() {
        let input = r#"
[server]
host = 127.0.0.1
port = 8024

[storage]
storage_dir = /var/clarus/data
"#;
        let p = ConfParser::parse(input).expect("parse should succeed");
        assert_eq!(p.get("server", "host"), Some("127.0.0.1"));
        assert_eq!(p.get("server", "port"), Some("8024"));
        assert_eq!(p.get("storage", "storage_dir"), Some("/var/clarus/data"));
    }

    #[test]
    fn test_auth_warn_interval_validation() {
        let cfg = |line: &str| {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("clarus.conf");
            std::fs::write(
                &path,
                format!("[server]\nhost = 127.0.0.1\nport = 8024\n[audit]\n{line}\n"),
            )
            .expect("write");
            Config::from_file(&path).expect("config")
        };
        assert_eq!(
            cfg("auth_warn_interval_min = 0")
                .audit
                .auth_warn_interval_min,
            60
        );
        assert_eq!(
            cfg("auth_warn_interval_min = -1")
                .audit
                .auth_warn_interval_min,
            60
        );
        assert_eq!(
            cfg("auth_warn_interval_min = abc")
                .audit
                .auth_warn_interval_min,
            60
        );
        assert_eq!(
            cfg("auth_warn_interval_min = 30")
                .audit
                .auth_warn_interval_min,
            30
        );
    }

    #[test]
    fn test_parse_unknown_key_rejected() {
        let input = "[server]\nhost = 127.0.0.1\nbogus_key = 1\n";
        let err = ConfParser::parse(input).unwrap_err();
        assert!(err.contains("unknown key"), "got: {err}");
    }

    #[test]
    fn test_parse_unknown_section_rejected() {
        let input = "[bogus]\nfoo = 1\n";
        let err = ConfParser::parse(input).unwrap_err();
        assert!(err.contains("unknown section"), "got: {err}");
    }

    #[test]
    fn test_parse_duplicate_key_rejected() {
        let input = "[server]\nhost = 127.0.0.1\nhost = 0.0.0.0\n";
        let err = ConfParser::parse(input).unwrap_err();
        assert!(err.contains("duplicate key"), "got: {err}");
    }

    #[test]
    fn test_parse_rename_hint() {
        let input = "[server]\nrequest_timeout_secs = 30\n";
        let err = ConfParser::parse(input).unwrap_err();
        assert!(err.contains("renamed to"), "got: {err}");
        assert!(err.contains("header_timeout_secs"));
    }

    #[test]
    fn admin_cidrs_empty_is_default_and_not_an_error() {
        let load = |extra: &str| {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("clarus.conf");
            std::fs::write(
                &path,
                format!("[server]\nhost = 127.0.0.1\nport = 8024\n{extra}"),
            )
            .expect("write");
            Config::from_file(&path)
        };
        // No key at all -> empty list (local machine only).
        let config = load("").expect("load without admin_cidrs");
        assert!(config.admin_cidrs.is_empty());
        // Explicit empty value is accepted too. An empty trusted_proxies is a
        // hard error; admin_cidrs deliberately is not, because empty is the
        // restrictive default.
        let config = load("admin_cidrs = \n").expect("empty value");
        assert!(config.admin_cidrs.is_empty());
        // A real list parses and decides by membership.
        let config = load("admin_cidrs = 192.168.50.0/24, fd00::/8\n").expect("load list");
        assert_eq!(config.admin_cidrs.len(), 2);
        assert!(config.admin_cidrs[0].contains_v4(u32::from_be_bytes([192, 168, 50, 9])));
        assert!(!config.admin_cidrs[0].contains_v4(u32::from_be_bytes([192, 168, 51, 9])));
    }

    #[test]
    fn admin_cidrs_typo_fails_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clarus.conf");
        std::fs::write(
            &path,
            "[server]\nhost = 127.0.0.1\nport = 8024\nadmin_cidrs = 192.168.50.0/99\n",
        )
        .expect("write");
        let err = Config::from_file(&path).expect_err("prefix > 32 must be rejected");
        assert!(err.contains("admin_cidrs"), "got: {err}");
    }

    #[test]
    fn expose_rejected_cidrs_empty_is_default_and_not_an_error() {
        let load = |extra: &str| {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("clarus.conf");
            std::fs::write(
                &path,
                format!("[server]\nhost = 127.0.0.1\nport = 8024\n{extra}"),
            )
            .expect("write");
            Config::from_file(&path)
        };
        // Absent -> empty, i.e. no remote client may ask for rejected
        // instances. This is the restrictive default and must not be an error.
        let config = load("").expect("load without expose_rejected_cidrs");
        assert!(config.expose_rejected_cidrs.is_empty());
        let config = load("expose_rejected_cidrs = \n").expect("empty value");
        assert!(config.expose_rejected_cidrs.is_empty());
        let config = load("expose_rejected_cidrs = 192.168.60.0/24\n").expect("load list");
        assert_eq!(config.expose_rejected_cidrs.len(), 1);
        assert!(config.expose_rejected_cidrs[0].contains_v4(u32::from_be_bytes([192, 168, 60, 7])));
        assert!(
            !config.expose_rejected_cidrs[0].contains_v4(u32::from_be_bytes([192, 168, 61, 7]))
        );
    }

    #[test]
    fn expose_rejected_cidrs_is_independent_of_admin_cidrs() {
        // The two surfaces answer different questions, so setting one must
        // never imply the other: an operator who grants QA read access to
        // rejected instances must not thereby grant them configuration rights.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clarus.conf");
        std::fs::write(
            &path,
            "[server]\nhost = 127.0.0.1\nport = 8024\nexpose_rejected_cidrs = 10.1.0.0/16\n",
        )
        .expect("write");
        let config = Config::from_file(&path).expect("load");
        assert_eq!(config.expose_rejected_cidrs.len(), 1);
        assert!(config.admin_cidrs.is_empty());
    }

    #[test]
    fn expose_rejected_cidrs_typo_fails_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clarus.conf");
        std::fs::write(
            &path,
            "[server]\nhost = 127.0.0.1\nport = 8024\nexpose_rejected_cidrs = 10.1.0.0/33\n",
        )
        .expect("write");
        let err = Config::from_file(&path).expect_err("prefix > 32 must be rejected");
        assert!(err.contains("expose_rejected_cidrs"), "got: {err}");
    }

    #[test]
    fn test_parse_cidr_list() {
        let nets = parse_trusted_proxies("127.0.0.0/8, 10.0.0.0/8, ::1/128").expect("ok");
        assert_eq!(nets.len(), 3);
        assert!(!nets.first().unwrap().contains_v4(10u32.to_be()));
        assert!(
            nets.first()
                .unwrap()
                .contains_v4(u32::from_be_bytes([127, 1, 2, 3]))
        );
        assert!(
            nets.get(1)
                .unwrap()
                .contains_v4(u32::from_be_bytes([10, 0, 0, 1]))
        );
    }

    #[test]
    fn test_parse_trusted_proxies_empty() {
        let err = parse_trusted_proxies("").unwrap_err();
        assert!(err.contains("at least one CIDR"));
    }

    #[test]
    fn test_parse_cidr_dotted_netmask() {
        let net = parse_cidr("192.168.1.0/255.255.255.0").expect("dotted netmask");
        assert_eq!(
            net,
            CidrNet::V4 {
                addr: parse_ipv4("192.168.1.0").unwrap(),
                prefix: 24
            }
        );
        assert!(net.contains_v4(u32::from_be_bytes([192, 168, 1, 7])));
        assert!(!net.contains_v4(u32::from_be_bytes([192, 168, 2, 7])));

        assert_eq!(
            parse_cidr("10.0.0.0/255.0.0.0").unwrap(),
            CidrNet::V4 {
                addr: parse_ipv4("10.0.0.0").unwrap(),
                prefix: 8
            }
        );
        assert_eq!(
            parse_cidr("0.0.0.0/0.0.0.0").unwrap(),
            CidrNet::V4 { addr: 0, prefix: 0 }
        );
        assert_eq!(
            parse_cidr("1.2.3.4/255.255.255.255").unwrap(),
            CidrNet::V4 {
                addr: parse_ipv4("1.2.3.4").unwrap(),
                prefix: 32
            }
        );

        assert!(
            parse_cidr("10.0.0.0/255.0.255.0")
                .unwrap_err()
                .contains("non-contiguous")
        );
        assert!(
            parse_cidr("::1/255.255.255.0")
                .unwrap_err()
                .contains("IPv4-only")
        );
        assert!(parse_cidr("10.0.0.0/999.0.0.0").is_err());
    }

    #[test]
    fn test_parse_comments() {
        let input = r#"
# This is a comment
[server]
host = 127.0.0.1   # inline comment
; another comment style
port = 8024
"#;
        let p = ConfParser::parse(input).expect("parse should succeed");
        assert_eq!(p.get("server", "host"), Some("127.0.0.1"));
        assert_eq!(p.get("server", "port"), Some("8024"));
    }

    #[test]
    fn test_parse_defaults() {
        let input = "[server]\nhost = 0.0.0.0\n";
        let p = ConfParser::parse(input).expect("parse should succeed");
        // get_or with default
        let port: u16 = p.get_or("server", "port", 8024u16);
        assert_eq!(port, 8024);
        let missing: String = p.get_or("server", "nonexistent_key", "fallback".to_string());
        assert_eq!(missing, "fallback");
    }

    /// The two E-013 `[server]` limits parse into `Config` and default to the
    /// documented values when omitted; 0 is rejected (range error).
    #[test]
    fn test_config_max_json_body_and_max_part_header() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n{body}\n[storage]\nstorage_dir = ./data\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Custom values parse.
        let c =
            parse_and_load("max_json_body = 2048\nmax_part_header = 8192").expect("custom parse");
        assert_eq!(c.max_json_body, 2048);
        assert_eq!(c.max_part_header, 8192);

        // Defaults when omitted.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(c.max_json_body, 1_048_576);
        assert_eq!(c.max_part_header, 4_194_304);

        // 0 -> error (range [1024..67108864]).
        let err = parse_and_load("max_json_body = 0").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
        let err = parse_and_load("max_part_header = 0").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");

        // Below the lower bound -> error.
        let err = parse_and_load("max_json_body = 1023").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    /// 0310 F2: `[storage] max_frame_bytes` parses with `k`/`m` suffixes
    /// (default 64 MiB), bare integers work, and both range ends are
    /// enforced - the WADO frame-range cap is the alloc-DoS bound, so a
    /// bogus value must fail startup, never silently fall back.
    #[test]
    fn test_config_max_frame_bytes() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!("[storage]\nstorage_dir = ./data\n{body}\n[server]\nhost = 127.0.0.1\nport = 8024\n"),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Default when omitted: 64 MiB.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(c.max_frame_bytes, 67_108_864);

        // Suffix and bare-integer forms agree.
        let c = parse_and_load("max_frame_bytes = 64m").expect("suffix parse");
        assert_eq!(c.max_frame_bytes, 67_108_864);
        let c = parse_and_load("max_frame_bytes = 67108864").expect("bare parse");
        assert_eq!(c.max_frame_bytes, 67_108_864);

        // Both bounds are accepted (1 MiB .. 1 GiB).
        let c = parse_and_load("max_frame_bytes = 1m").expect("lower bound");
        assert_eq!(c.max_frame_bytes, 1_048_576);
        let c = parse_and_load("max_frame_bytes = 1024m").expect("upper bound");
        assert_eq!(c.max_frame_bytes, 1_073_741_824);

        // Out of range -> error naming the key (below min, above max).
        let err = parse_and_load("max_frame_bytes = 1023").unwrap_err();
        assert!(
            err.contains("max_frame_bytes") && err.contains("out of range"),
            "got: {err}"
        );
        let err = parse_and_load("max_frame_bytes = 1025m").unwrap_err();
        assert!(
            err.contains("max_frame_bytes") && err.contains("out of range"),
            "got: {err}"
        );
    }

    /// Stage 2 (F5): `[storage] max_frames_per_instance` parses (absent ->
    /// 1_000_000, explicit honored, both bounds accepted, ceiling enforced).
    #[test]
    fn test_config_max_frames_per_instance() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!("[storage]\nstorage_dir = ./data\n{body}\n[server]\nhost = 127.0.0.1\nport = 8024\n"),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Default when omitted: 1_000_000.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(c.max_frames_per_instance, 1_000_000);

        // Explicit value honored.
        let c = parse_and_load("max_frames_per_instance = 250000").expect("explicit parse");
        assert_eq!(c.max_frames_per_instance, 250_000);

        // Both bounds accepted.
        let c = parse_and_load("max_frames_per_instance = 1").expect("lower bound");
        assert_eq!(c.max_frames_per_instance, 1);
        let c = parse_and_load("max_frames_per_instance = 16777216").expect("ceiling");
        assert_eq!(c.max_frames_per_instance, 16_777_216);

        // Below min / above the ceiling -> error naming the key.
        let err = parse_and_load("max_frames_per_instance = 0").unwrap_err();
        assert!(
            err.contains("max_frames_per_instance") && err.contains("out of range"),
            "got: {err}"
        );
        let err = parse_and_load("max_frames_per_instance = 16777217").unwrap_err();
        assert!(
            err.contains("max_frames_per_instance") && err.contains("out of range"),
            "got: {err}"
        );

        // Non-integer -> error.
        let err = parse_and_load("max_frames_per_instance = abc").unwrap_err();
        assert!(
            err.contains("max_frames_per_instance") && err.contains("is not an integer"),
            "got: {err}"
        );
    }

    /// P2 Stage A: `max_batch_bytes` parses human-readable `k`/`m` suffixes
    /// via `parse_size` (default 100 MiB, same range and default as before),
    /// bare integers stay backward compatible, out-of-range is rejected.
    #[test]
    fn test_config_max_batch_bytes_suffix() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!("[storage]\nstorage_dir = ./data\n{body}\n[server]\nhost = 127.0.0.1\nport = 8024\n"),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // "100m" -> 100 MiB (the documented default value).
        let c = parse_and_load("max_batch_bytes = 100m").expect("suffix parse");
        assert_eq!(c.max_batch_bytes, 100 * 1024 * 1024);

        // Bare integer stays backward compatible.
        let c = parse_and_load("max_batch_bytes = 104857600").expect("bare parse");
        assert_eq!(c.max_batch_bytes, 104857600);

        // Default when omitted: 100 MiB.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(c.max_batch_bytes, 100 * 1024 * 1024);

        // Out of range -> error (below min, above max).
        let err = parse_and_load("max_batch_bytes = 1023").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
        // 20000m = 20 971 520 000 > 10 737 418 240 (10 GiB cap).
        let err = parse_and_load("max_batch_bytes = 20000m").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    /// `[query] default_limit` parses (absent -> 250,
    /// 0 -> unlimited, explicit value honored). `0` here is VALID (means
    /// unlimited), unlike the [server] size limits where 0 is an error.
    #[test]
    fn test_config_query_default_limit() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Absent -> default 250.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(c.query.default_limit, 250);

        // 0 = unlimited (valid, no range error).
        let c = parse_and_load("[query]\ndefault_limit = 0").expect("zero parse");
        assert_eq!(c.query.default_limit, 0);

        // Explicit value.
        let c = parse_and_load("[query]\ndefault_limit = 1000").expect("custom parse");
        assert_eq!(c.query.default_limit, 1000);

        // Unknown key in [query] -> rejected (allowlist).
        let err = parse_and_load("[query]\nbogus_key = 1").unwrap_err();
        assert!(err.contains("unknown"), "got: {err}");
    }

    /// Helper for the `[ups]` tests: load a config with the given body.
    fn load_ups(body: &str) -> Result<Config, String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let conf = dir.path().join("clarus.conf");
        std::fs::write(
            &conf,
            format!(
                "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
            ),
        )
        .expect("write conf");
        Config::from_file(&conf)
    }

    /// The shipped code table: DX and CT are named, and an unlisted or absent
    /// modality yields NOTHING because the shipped config has no fallback -
    /// the fallback is opt-in. The lookup is case-insensitive because a
    /// modality is a DICOM CS value.
    #[test]
    fn test_config_ups_workitem_code_defaults() {
        let c = load_ups("").expect("default parse");
        assert_eq!(
            c.ups.code_for_modality(Some("DX")),
            Some("CXR-AI"),
            "the shipped DX code"
        );
        assert_eq!(
            c.ups.code_for_modality(Some("dx")),
            Some("CXR-AI"),
            "CS: any case"
        );
        assert_eq!(
            c.ups.code_for_modality(Some(" DX ")),
            Some("CXR-AI"),
            "trimmed"
        );
        assert_eq!(c.ups.code_for_modality(Some("CT")), Some("CT-ORGAN-SEG"));
        // Not in the table, and no fallback configured: no code at all.
        assert_eq!(c.ups.code_for_modality(Some("US")), None);
        assert_eq!(c.ups.code_for_modality(Some("DXX")), None);
        // No modality at all: the same answer, because there is no fallback.
        assert_eq!(c.ups.code_for_modality(None), None);
        assert_eq!(
            UpsConfig::CODE_SCHEME,
            "99CLARUS",
            "the scheme is a constant"
        );
    }

    /// A site table replaces the shipped one; `fallback` and `meaning` are
    /// the reserved keys, and naming `fallback` is what turns an unlisted
    /// modality from "no workitem" into "the generic code".
    #[test]
    fn test_config_ups_workitem_code_site_table() {
        let c = load_ups(
            "[ups]\ntask_producer = true\n\
             [ups.workitem_code]\nDX = CXR-AI\nMG = MAMMO-AI\nfallback = UNKNOWN-AI\n\
             meaning = site AI\n",
        )
        .expect("site table parses");

        assert_eq!(c.ups.code_for_modality(Some("DX")), Some("CXR-AI"));
        assert_eq!(c.ups.code_for_modality(Some("mg")), Some("MAMMO-AI"));
        assert_eq!(c.ups.code_for_modality(Some("CT")), Some("UNKNOWN-AI"));
        assert_eq!(c.ups.code_for_modality(None), Some("UNKNOWN-AI"));
        assert_eq!(c.ups.workitem_code_fallback.as_deref(), Some("UNKNOWN-AI"));
        assert_eq!(c.ups.workitem_code_meaning, "site AI");
        // The table is exactly what was listed - no merged-in defaults.
        assert_eq!(c.ups.workitem_codes.len(), 2);
        assert!(c.ups.task_producer);
    }

    /// W26: the reserved words are reserved in ANY case.
    ///
    /// `[ups.workitem_code]` is a code-valued table, so its keys are not folded
    /// by the parser. `Fallback = GENERIC-AI` therefore passed modality
    /// validation, was stored as a modality called FALLBACK, and the option it
    /// was meant to set stayed unset - the fallback was silently OFF, which is
    /// a behaviour change with no error to notice.
    #[test]
    fn test_config_ups_workitem_code_reserved_words_are_case_insensitive() {
        let c = load_ups(
            "[ups]\ntask_producer = true\n\
             [ups.workitem_code]\nDX = CXR-AI\nFallback = GENERIC-AI\nMEANING = site AI\n",
        )
        .expect("a mixed-case reserved key parses");

        assert_eq!(
            c.ups.workitem_code_fallback.as_deref(),
            Some("GENERIC-AI"),
            "Fallback configures the fallback rather than a modality"
        );
        assert_eq!(c.ups.workitem_code_meaning, "site AI");
        // And it did NOT become a modality row.
        assert_eq!(c.ups.workitem_codes.len(), 1, "only DX is a modality");
        assert_eq!(
            c.ups.code_for_modality(Some("FALLBACK")),
            Some("GENERIC-AI")
        );
    }

    /// W26: one modality listed twice in different cases is a duplicate.
    ///
    /// Both `DX` and `dx` fold to the same modality, so a file carrying both
    /// described one table entry twice. Last-wins would be a silent overwrite;
    /// the strict parser refuses duplicates everywhere else, so it refuses
    /// this one too.
    #[test]
    fn test_config_ups_workitem_code_duplicate_modality_is_refused() {
        let err = load_ups(
            "[ups]\ntask_producer = true\n\
             [ups.workitem_code]\nDX = CXR-AI\ndx = BONE-AI\n",
        )
        .expect_err("a modality listed twice is refused");
        assert!(
            err.contains("duplicate modality"),
            "the error names the problem: {err}"
        );
    }

    /// W27: a `[ups.code_meaning]` key is a Code Value and keeps its case.
    ///
    /// Code Values are case-sensitive machine keys (PS3.16), and the parser
    /// keeps this section's spelling. Upper-casing the key at store time while
    /// `meaning_for_code` compared it exactly made a lower-case row
    /// unreachable: the label was configured, validated, stored - and could
    /// never be looked up.
    #[test]
    fn test_config_ups_code_meaning_key_keeps_its_spelling() {
        let c = load_ups(
            "[ups]\ntask_producer = true\n\
             [ups.workitem_code]\nDX = bone-ai\n\
             [ups.code_meaning]\nbone-ai = Bone age\n",
        )
        .expect("a lower-case code value parses");

        assert_eq!(
            c.ups.meaning_for_code("bone-ai"),
            "Bone age",
            "the row is found by the code exactly as the site wrote it"
        );
        // Stored as written, not folded.
        assert_eq!(
            c.ups.code_meanings,
            vec![("bone-ai".to_string(), "Bone age".to_string())]
        );
        // And the global meaning is still the fallback for an unknown code.
        assert_eq!(
            c.ups.meaning_for_code("other-ai"),
            UpsConfig::DEFAULT_WORKITEM_CODE_MEANING
        );
    }

    /// W29: one reserved word spelled two ways is a duplicate, not a rerun.
    ///
    /// `fallback` and `Fallback` are the same key once the reserved words are
    /// matched case-insensitively, but the parser's duplicate check compares
    /// raw keys and this section's keys are not folded. Both therefore reached
    /// the loader, which assigned twice and let the last one in the section's
    /// sorted order win - a silently discarded configuration value.
    #[test]
    fn test_config_ups_workitem_code_duplicate_reserved_key_is_refused() {
        let err = load_ups(
            "[ups]\ntask_producer = true\n\
             [ups.workitem_code]\nDX = CXR-AI\nfallback = A\nFallback = B\n",
        )
        .expect_err("the same reserved word twice is refused");
        assert!(
            err.contains("duplicate reserved key"),
            "the error names the problem: {err}"
        );

        // The same rule for `meaning`, and neither spelling alone is enough
        // to trigger it - the control case below must still parse.
        let err = load_ups(
            "[ups]\ntask_producer = true\n\
             [ups.workitem_code]\nmeaning = a\nMEANING = b\n",
        )
        .expect_err("the same reserved word twice is refused");
        assert!(err.contains("duplicate reserved key"), "got: {err}");

        let ok = load_ups(
            "[ups]\ntask_producer = true\n\
             [ups.workitem_code]\nDX = CXR-AI\nFallback = A\nMeaning = b\n",
        )
        .expect("one spelling of each reserved word parses");
        assert_eq!(ok.ups.workitem_code_fallback.as_deref(), Some("A"));
        assert_eq!(ok.ups.workitem_code_meaning, "b");
    }

    /// The shipped `non_task_modalities` list, and that it is a SET.
    #[test]
    fn test_config_ups_non_task_modalities_defaults() {
        let c = load_ups("").expect("default parse");
        assert_eq!(
            c.ups.non_task_modalities,
            vec!["KO", "PR", "RTSTRUCT", "SEG", "SR"],
            "the shipped list, sorted (a set)"
        );
        for m in ["SR", "PR", "SEG", "KO", "RTSTRUCT"] {
            assert!(c.ups.is_non_task_modality(m), "{m} is an AI output");
        }
        assert!(!c.ups.is_non_task_modality("DX"), "DX is an input");
        assert!(
            c.ups.is_non_task_modality("sr"),
            "a modality is a CS value: case-insensitive"
        );
    }

    /// `non_task_modalities` parses: CSV, upper-cased, deduped and sorted; an
    /// EMPTY value is an empty list (a deliberate choice, distinct from
    /// absent, which takes the shipped list).
    #[test]
    fn test_config_ups_non_task_modalities_parsing() {
        let c = load_ups("[ups]\nnon_task_modalities = sr, pr ,SR\n").expect("parses");
        assert_eq!(
            c.ups.non_task_modalities,
            vec!["PR", "SR"],
            "upper-cased, deduped, sorted"
        );

        let c = load_ups("[ups]\nnon_task_modalities =\n").expect("empty parses");
        assert!(
            c.ups.non_task_modalities.is_empty(),
            "an empty value means NOTHING is excluded, not the shipped list"
        );

        let c = load_ups("[ups]\nnon_task_modalities = CT\n").expect("parses");
        assert_eq!(
            c.ups.non_task_modalities,
            vec!["CT"],
            "a site list REPLACES the shipped one"
        );
    }

    /// A malformed `non_task_modalities` is refused at startup.
    #[test]
    fn test_config_ups_non_task_modalities_refusals() {
        // An empty element inside a non-empty list is a typo.
        let err = load_ups("[ups]\nnon_task_modalities = SR,,PR\n").unwrap_err();
        assert!(
            err.contains("non_task_modalities") && err.contains("empty"),
            "got: {err}"
        );
        // A token that cannot be a modality (CS: printable ASCII, <= 16).
        let err = load_ups("[ups]\nnon_task_modalities = SR,МОДАЛЬНОСТЬ\n").unwrap_err();
        assert!(err.contains("non_task_modalities"), "got: {err}");
    }

    /// A malformed key or value is refused at startup with the offending
    /// name, rather than becoming a modality that silently never matches.
    #[test]
    fn test_config_ups_workitem_code_refusals() {
        // A key that is not a CS-shaped modality token. The parser reads the
        // key up to the first space, so a two-word key arrives as one token
        // that the shape check refuses.
        let err = load_ups("[ups.workitem_code]\ndxcode-thing = CXR-AI\n").unwrap_err();
        assert!(err.contains("unknown"), "got: {err}");

        // An empty code value.
        let err = load_ups("[ups.workitem_code]\nDX = \n").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");

        // A value that is not an SH-shaped machine key (over 16 chars).
        let err = load_ups("[ups.workitem_code]\nDX = A-VERY-LONG-CODE-VALUE-HERE\n").unwrap_err();
        assert!(err.contains("not a code value"), "got: {err}");

        // A key in [ups] itself is still allowlist-checked.
        let err = load_ups("[ups]\nnot_a_key = 1\n").unwrap_err();
        assert!(err.contains("unknown"), "got: {err}");
    }

    /// The catalog: EVERY code of a modality, not just the first, in
    /// canonical (sorted) order so the table's line order cannot change the
    /// result. A modality the table does not name yields exactly ONE entry,
    /// the fallback - the pre-catalog behaviour.
    ///
    /// The LIST form is what makes several codes per modality expressible:
    /// the strict parser refuses a repeated key, so `DX` twice is an error
    /// rather than two records (`test_config_ups_catalog_refusals` pins
    /// that), and one key carrying a list is the shape that fits.
    #[test]
    fn test_config_ups_catalog_lists_every_matching_record() {
        let c = load_ups(
            "[ups.workitem_code]\nDX = CXR-AI, BONE-AI\nCT = CT-SEG\n\
             fallback = GENERIC-AI\n",
        )
        .expect("a two-code DX list parses");
        assert_eq!(
            c.ups.catalog_for_modality(Some("DX")),
            vec!["BONE-AI", "CXR-AI"],
            "every listed code, sorted (BONE-AI before CXR-AI)"
        );
        assert_eq!(
            c.ups.catalog_for_modality(Some("dx")),
            vec!["BONE-AI", "CXR-AI"],
            "the same set for any case of the modality"
        );
        assert_eq!(
            c.ups.catalog_for_modality(Some("CT")),
            vec!["CT-SEG"],
            "a single code is still a one-entry catalog"
        );

        // An UNLISTED modality: exactly one entry, the fallback - the site
        // named one in this fixture.
        assert_eq!(c.ups.catalog_for_modality(Some("US")), vec!["GENERIC-AI"]);
        // NO modality at all is not a candidate: there is nothing to route to
        // the fallback, so the catalog is empty.
        assert_eq!(c.ups.catalog_for_modality(None), Vec::<&str>::new());

        // `code_for_modality` keeps its first-match behaviour for the callers
        // that still want one code (the HTTP create path).
        assert_eq!(c.ups.code_for_modality(Some("DX")), Some("CXR-AI"));
    }

    /// The shipped defaults: no section at all still yields a workable
    /// catalog for a NAMED modality, and nothing changes for an existing
    /// deployment.
    ///
    /// The shipped config has no fallback, so an unlisted modality yields an
    /// EMPTY catalog - that is the opt-in change, and it is deliberate: a
    /// generic workitem nobody claims is worse than no workitem plus a log.
    #[test]
    fn test_config_ups_catalog_defaults_leave_the_wire_unchanged() {
        let c = load_ups("").expect("default parse");
        assert_eq!(
            c.ups.catalog_for_modality(Some("DX")),
            vec!["CXR-AI"],
            "the shipped table names DX once"
        );
        assert_eq!(
            c.ups.catalog_for_modality(Some("US")),
            Vec::<&str>::new(),
            "no shipped fallback: an unlisted modality mints nothing"
        );
        assert!(
            c.ups.ai_station_aet.is_none(),
            "no AI station by default: (0040,0001) is not written, which is \
             the wire shape this server had before the catalog"
        );
        assert!(
            c.ups.manual_codes.is_empty(),
            "nothing is manual until a site says so"
        );
    }

    /// The automatic/declarative split, the station routing and the per-code
    /// labels.
    ///
    /// Every entry `catalog_for_modalities` returns is AUTOMATIC and is
    /// addressed to `ai_station_aet`; a parked code never becomes an entry at
    /// all (see [`test_config_ups_manual_code_is_not_minted`]).
    #[test]
    fn test_config_ups_station_and_meanings() {
        let c = load_ups(
            "[ups]\nai_station_aet = AISTATION\n\
             [ups.workitem_code]\nDX = CXR-AI, BONE-AI\n[ups.manual_codes]\n\
             BONE-AI = parked\n[ups.code_meaning]\nCXR-AI = Chest X-ray AI\n\
             BONE-AI = Bone age\n",
        )
        .expect("the catalog parses");

        assert_eq!(c.ups.ai_station_aet.as_deref(), Some("AISTATION"));

        // The station is the AI one for every MINTED entry: BONE-AI is parked,
        // so it is not minted, and the only entry DX yields is CXR-AI.
        assert_eq!(
            c.ups.catalog_for_modality(Some("DX")),
            vec!["CXR-AI"],
            "the parked BONE-AI is not among the minted codes"
        );
        assert!(
            !c.ups.is_manual_code("CXR-AI"),
            "an unlisted code is automatic"
        );
        assert!(c.ups.is_manual_code("BONE-AI"), "a listed code is manual");
        assert!(
            !c.ups.is_manual_code("GENERIC-AI"),
            "the fallback has no row, so it is automatic"
        );

        // Labels: the per-code row wins, the global meaning is the fallback.
        assert_eq!(c.ups.meaning_for_code("CXR-AI"), "Chest X-ray AI");
        assert_eq!(c.ups.meaning_for_code("BONE-AI"), "Bone age");
        assert_eq!(
            c.ups.meaning_for_code("GENERIC-AI"),
            UpsConfig::DEFAULT_WORKITEM_CODE_MEANING,
            "a code with no row takes the global meaning"
        );
    }

    /// RED FACT. Two modalities reaching the SAME code mint ONE workitem.
    ///
    /// The UID is keyed on (study, code), so a code that survives the dedupe
    /// twice is a second write of the same workitem. Sorting by
    /// (modality, code) before `dedup_by` removed only adjacent duplicates, so
    /// the same code under two modalities with a different code between them
    /// in that order survived the dedupe.
    #[test]
    fn test_config_ups_one_workitem_per_code_across_modalities() {
        let c = load_ups("[ups.workitem_code]\nDX = AAA, ZZZ\nCT = ZZZ, BBB\n")
            .expect("the catalog parses");

        let entries = c.ups.catalog_for_modalities(["DX", "CT"]);
        let codes: Vec<&str> = entries.iter().map(|(_, code)| *code).collect();
        assert_eq!(
            codes,
            vec!["AAA", "BBB", "ZZZ"],
            "each code appears exactly once, whatever modality reached it"
        );
        // The canonical order is (code, modality): the code is the primary key
        // of the output and the modality only breaks ties, so a shared code
        // always carries the SAME trigger modality regardless of how the
        // input set was built.
        let zzz = entries
            .iter()
            .find(|(_, code)| *code == "ZZZ")
            .expect("ZZZ is present");
        assert_eq!(
            zzz.0, "CT",
            "a shared code's trigger modality is deterministic (CT < DX)"
        );
    }

    /// `[ups.coding_schemes]` is empty by default and accepts only private
    /// `99*` designators.
    #[test]
    fn test_config_ups_coding_schemes_default_is_private_only() {
        let c = load_ups("").expect("parses");
        assert!(
            c.ups.coding_schemes.is_empty(),
            "no scheme is declared by default"
        );
        // PS3.3 8.2: a designator beginning with 99 is private or local, and
        // any site may declare such schemes without listing them. The single
        // designator L is the other half of the same sentence.
        assert!(c.ups.allows_scheme("99CLARUS"));
        assert!(c.ups.allows_scheme("99123"));
        assert!(
            c.ups.allows_scheme("L"),
            "the HL7 L designator is private/local"
        );
        // A public scheme must be declared.
        assert!(!c.ups.allows_scheme("LN"));
        assert!(!c.ups.allows_scheme("SCT"));
    }

    /// A declared site list is parsed, sorted and de-duplicated, and the
    /// lookup is an exact match.
    #[test]
    fn test_config_ups_coding_schemes_are_parsed_and_exact() {
        let c = load_ups("[ups.coding_schemes]\nSCT = snomed\nLN = loinc\n").expect("parses");
        assert_eq!(
            c.ups.coding_schemes,
            vec!["LN".to_string(), "SCT".to_string()],
            "sorted and kept as written"
        );
        assert!(c.ups.allows_scheme("LN"));
        assert!(c.ups.allows_scheme("L"), "L stays implicit");
        assert!(c.ups.allows_scheme("SCT"));
        assert!(c.ups.allows_scheme("99CLARUS"), "99 stays implicit");
        assert!(
            !c.ups.allows_scheme("ln"),
            "a designator is a machine key, not folded"
        );
        assert!(
            !c.ups.allows_scheme("X99Y"),
            "the private test is a PREFIX, not a substring"
        );
    }

    /// A duplicate designator is refused rather than silently last-wins.
    #[test]
    fn test_config_ups_coding_schemes_duplicate_is_refused() {
        let err = load_ups("[ups.coding_schemes]\nLN = a\nLN = b\n")
            .expect_err("a duplicate key must be refused");
        assert!(
            err.contains("duplicate") || err.contains("LN"),
            "the refusal names the key: {err}"
        );
    }

    /// A key that is not a legal SH designator is refused.
    #[test]
    fn test_config_ups_coding_schemes_bad_key_is_refused() {
        let err = load_ups("[ups.coding_schemes]\nL-N = hyphen\n")
            .expect_err("a designator is SH: A-Z, 0-9, underscore only");
        assert!(
            err.contains("L-N") || err.contains("coding_schemes"),
            "{err}"
        );
        let err = load_ups(&format!(
            "[ups.coding_schemes]\n{} = long\n",
            "A".repeat(17)
        ))
        .expect_err("a designator is at most 16 characters");
        assert!(
            err.contains("coding_schemes") || err.contains("AAAA"),
            "{err}"
        );
    }

    /// The manual set is matched exactly: a Code Value is a machine key, so
    /// case is significant, unlike a modality.
    #[test]
    fn test_config_ups_manual_codes_are_matched_exactly() {
        let c = load_ups("[ups.manual_codes]\nBONE-AI = parked\n").expect("parses");
        assert!(c.ups.is_manual_code("BONE-AI"));
        assert!(
            !c.ups.is_manual_code("bone-ai"),
            "a Code Value is not case-folded: it is a site-agreed key"
        );
        assert_eq!(c.ups.manual_codes, vec!["BONE-AI".to_string()]);
    }

    /// A parked code is parked when the catalog writes it the SAME WAY.
    ///
    /// Both sides are Code Values and neither is folded: the parser used to
    /// lower-case every key, so `[ups.workitem_code] DX = BONE-AI` stored
    /// `bone-ai` while `[ups.manual_codes] BONE-AI` was upper-cased on the
    /// other side. The two could never meet, and a "parked" code was minted
    /// anyway. With both sides kept as written, the site's own spelling parks.
    #[test]
    fn test_config_ups_parked_code_matches_the_catalog_spelling() {
        let c =
            load_ups("[ups.workitem_code]\nDX = BONE-AI\n[ups.manual_codes]\nBONE-AI = parked\n")
                .expect("parses");
        assert!(c.ups.is_manual_code("BONE-AI"), "the code value as written");
        assert!(
            c.ups.catalog_for_modality(Some("DX")).is_empty(),
            "a parked code is never minted"
        );
        // The site's spelling must SURVIVE the parse. Folding every key made
        // `[ups.workitem_code] DX = BONE-AI` store `bone-ai`, so a rule, a
        // viewer label or an operator search written as the site wrote it could
        // never address the code that was actually minted.
        assert_eq!(
            c.ups.workitem_codes,
            vec![("DX".to_string(), "BONE-AI".to_string())],
            "the Code Value keeps the case the site wrote"
        );

        // A DIFFERENT spelling is a different Code Value, so it is not parked -
        // that is the documented machine-key rule, and it is why the parser
        // must not fold either side.
        let c =
            load_ups("[ups.workitem_code]\nDX = bone-ai\n[ups.manual_codes]\nBONE-AI = parked\n")
                .expect("parses");
        assert!(
            !c.ups.is_manual_code("bone-ai"),
            "case is significant: the catalog says bone-ai, the park says BONE-AI"
        );
        assert_eq!(
            c.ups.catalog_for_modality(Some("DX")),
            vec!["bone-ai"],
            "so the catalog's own spelling is still minted"
        );
    }

    /// The RED FACT: the park list must keep the case the site wrote.
    ///
    /// The parser folded every key to lower-case and then upper-cased the
    /// manual set, so a site that wrote `[ups.manual_codes] cxr-ai` had that
    /// entry stored as `CXR-AI` - a spelling the site never used. Whether the
    /// parked code was then matched depended on the catalogue's case by luck,
    /// not on the config.
    #[test]
    fn test_config_ups_parked_code_keeps_the_site_spelling() {
        let c = load_ups("[ups.workitem_code]\nDX = CXR-AI\n[ups.manual_codes]\ncxr-ai = parked\n")
            .expect("parses");
        assert_eq!(
            c.ups.manual_codes,
            vec!["cxr-ai".to_string()],
            "the park set stores the Code Value as the site wrote it"
        );
        assert_eq!(
            c.ups.workitem_codes,
            vec![("DX".to_string(), "CXR-AI".to_string())],
            "and the catalog value keeps its own case"
        );
    }

    /// An ABSENT `auth_warn_interval_min` is the shipped default, not a
    /// configuration error: defaulting silently is what keeps a clean boot free
    /// of WARN lines that train an operator to ignore them.
    #[test]
    fn test_config_audit_absent_warn_interval_is_silent() {
        let p = ConfParser::parse("[audit]\naccess_retention_days = 30\n").expect("parses");
        let raw = p.get("audit", "auth_warn_interval_min");
        assert!(raw.is_none(), "the key is absent");

        // A PRESENT but invalid value must still be seen as invalid - the
        // silence above is about absence, not about ignoring mistakes.
        let p = ConfParser::parse("[audit]\nauth_warn_interval_min = 0\n").expect("parses");
        assert_eq!(
            p.get("audit", "auth_warn_interval_min"),
            Some("0"),
            "an invalid value is present, so the loader warns about it"
        );
    }

    /// RED FACT. A manual (parked) code is NOT minted: `catalog_for_modalities`
    /// drops it, so the STOW automaton produces no workitem for it.
    ///
    /// Before this the catalog returned every code the modality table named,
    /// and a parked code was minted SCHEDULED and parked on the manual station.
    /// That pre-minted workitem is non-terminal, so a Task Requester's "Run"
    /// (UPS Create, PS3.18 11.4) sees an existing non-terminal workitem with
    /// that code and does nothing - forever. A manual code is DECLARATIVE: it
    /// is created lazily by the Task Requester instead, and the mint must leave
    /// it alone.
    #[test]
    fn test_config_ups_manual_code_is_not_minted() {
        let c = load_ups(
            "[ups.workitem_code]\nDX = CXR-AI, BONE-AI, REVIEW-ONLY\n\
             [ups.manual_codes]\nREVIEW-ONLY = parked\nREVIEW-ONLY2 = parked\n",
        )
        .expect("the catalog parses with a parked code");

        // The manual code is dropped; the two automatic ones remain.
        assert_eq!(
            c.ups.catalog_for_modality(Some("DX")),
            vec!["BONE-AI", "CXR-AI"],
            "the parked REVIEW-ONLY is NOT among the minted codes"
        );
        assert!(
            c.ups.is_manual_code("REVIEW-ONLY"),
            "and it is still recognised as a manual code"
        );

        // A modality whose ONLY row is parked yields NOTHING - not an empty
        // entry, and not a fallback (the fallback is opt-in and unset here).
        let only_manual = load_ups(
            "[ups.workitem_code]\nMG = REVIEW-ONLY\n\
             [ups.manual_codes]\nREVIEW-ONLY = parked\n",
        )
        .expect("parses");
        assert_eq!(
            only_manual.ups.catalog_for_modality(Some("MG")),
            Vec::<&str>::new(),
            "a modality whose only code is parked mints nothing"
        );

        // The fallback is automatic by construction: it has no catalog row, so
        // it can never be listed in `[ups.manual_codes]` and is unaffected.
        let with_fallback = load_ups(
            "[ups.workitem_code]\nfallback = GENERIC-AI\n[ups.manual_codes]\nBONE-AI = parked\n",
        )
        .expect("parses");
        assert_eq!(
            with_fallback.ups.catalog_for_modality(Some("US")),
            vec!["GENERIC-AI"],
            "the fallback still mints for an unlisted modality"
        );
    }

    /// Refusals: a bad AE Title, an empty label, and an unknown section are
    /// all rejected at startup with the offending name.
    #[test]
    fn test_config_ups_catalog_refusals() {
        // An AE Title is 16 characters at most (PS3.5 Table 6.2-1).
        let err = load_ups("[ups]\nai_station_aet = A-STATION-NAME-LONGER-THAN-16\n").unwrap_err();
        assert!(err.contains("longer than 16"), "got: {err}");

        // An empty AI station is NOT an error: it is the documented way to
        // write no station at all, which is the pre-catalog wire shape.
        let c = load_ups("[ups]\nai_station_aet = \n").expect("empty AI station is legal");
        assert!(c.ups.ai_station_aet.is_none());

        // The removed manual-station key is no longer a known key: the
        // allowlist refuses it rather than silently ignoring it.
        let err = load_ups("[ups]\nmanual_station_aet = PARKED\n").unwrap_err();
        assert!(err.contains("unknown"), "got: {err}");

        // An empty meaning.
        let err = load_ups("[ups.code_meaning]\nCXR-AI = \n").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");

        // A meaning longer than LO (64 chars).
        let long = "x".repeat(65);
        let err = load_ups(&format!("[ups.code_meaning]\nCXR-AI = {long}\n")).unwrap_err();
        assert!(err.contains("longer than 64"), "got: {err}");

        // The new sections are allowlist-checked as sections, so a typo in a
        // section name is refused rather than silently ignored.
        let err = load_ups("[ups.manual_code]\nBONE-AI = parked\n").unwrap_err();
        assert!(err.contains("unknown"), "got: {err}");

        // A REPEATED modality key is refused (the strict no-last-wins rule),
        // which is exactly why the catalog uses a LIST: two codes for one
        // modality are one key with two values, never two keys.
        let err = load_ups("[ups.workitem_code]\nDX = CXR-AI\nDX = BONE-AI\n").unwrap_err();
        assert!(err.contains("duplicate"), "got: {err}");

        // An empty element inside the list is refused by name.
        let err = load_ups("[ups.workitem_code]\nDX = CXR-AI,,BONE-AI\n").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    /// Every shipped default code fits the SH bound (PS3.5 Table 6.2-1,
    /// 16 characters): a default must never out-run what a hand-written
    /// value is allowed to say. The previous shipped CT code was 21
    /// characters and bypassed the check because defaults are applied
    /// programmatically; it was shortened by owner decision 27.09
    /// (analysis/2609_writer_ups_catalog_REPORT.md section 4).
    #[test]
    fn test_config_ups_shipped_default_codes_fit_the_sh_bound() {
        let longest = UpsConfig::DEFAULT_WORKITEM_CODES
            .iter()
            .map(|(_, code)| code.len())
            .max()
            .expect("the shipped table is not empty");
        assert!(
            longest <= 16,
            "a shipped default longer than SH would bypass the parser bound"
        );
        // A hand-written copy of a shipped value is accepted: the parser and
        // the defaults no longer disagree about the same string.
        for (_, code) in UpsConfig::DEFAULT_WORKITEM_CODES {
            let body = format!("[ups.workitem_code]\nDX = {code}\n");
            assert!(load_ups(&body).is_ok(), "shipped code {code} refused");
        }
        // And the bound is still real for a longer value.
        let err = load_ups("[ups.workitem_code]\nCT = CT-organ-segmentation\n").unwrap_err();
        assert!(err.contains("not a code value"), "got: {err}");
    }

    /// `[ups] default_limit` parses (absent -> 100, 0 -> unlimited,
    /// explicit honored). Separate dial from [query] (12.09).
    #[test]
    fn test_config_ups_default_limit() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Absent -> default 100.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(c.ups.default_limit, 100);

        // 0 = unlimited.
        let c = parse_and_load("[ups]\ndefault_limit = 0").expect("zero parse");
        assert_eq!(c.ups.default_limit, 0);

        // Explicit value.
        let c = parse_and_load("[ups]\ndefault_limit = 500").expect("custom parse");
        assert_eq!(c.ups.default_limit, 500);

        // Unknown key in [ups] -> rejected (allowlist).
        let err = parse_and_load("[ups]\nbogus_key = 1").unwrap_err();
        assert!(err.contains("unknown"), "got: {err}");
    }

    /// `[ups] final_state_retention_days` parses, defaults to 7 when absent,
    /// accepts `0` (= keep forever) and is refused outside `0..=3650`.
    #[test]
    fn test_config_ups_final_state_retention_days() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Absent -> the shipped default.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(
            c.ups.final_state_retention_days,
            UpsConfig::DEFAULT_FINAL_STATE_RETENTION_DAYS
        );
        assert_eq!(c.ups.final_state_retention_days, 7);

        // 0 = keep final workitems forever.
        let c = parse_and_load("[ups]\nfinal_state_retention_days = 0").expect("zero parse");
        assert_eq!(c.ups.final_state_retention_days, 0);

        // The upper bound is accepted, one past it is not.
        let c = parse_and_load("[ups]\nfinal_state_retention_days = 3650").expect("max parse");
        assert_eq!(c.ups.final_state_retention_days, 3650);

        let err = parse_and_load("[ups]\nfinal_state_retention_days = 3651").unwrap_err();
        assert!(
            err.contains("final_state_retention_days"),
            "the key must be named: {err}"
        );

        // A negative value cannot parse as u64 -> refused by the same path.
        let err = parse_and_load("[ups]\nfinal_state_retention_days = -1").unwrap_err();
        assert!(
            err.contains("final_state_retention_days"),
            "the key must be named: {err}"
        );
    }

    /// `[ups] task_producer` parses (absent -> false, explicit true/false
    /// honored). Off by default: the STOW path is untouched.
    #[test]
    fn test_config_ups_task_producer() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Absent -> false (the shipped default creates no workitems).
        let c = parse_and_load("").expect("default parse");
        assert!(!c.ups.task_producer);
        assert_eq!(c.ups.default_limit, 100, "the existing key is unaffected");

        // Explicit on.
        let c = parse_and_load("[ups]\ntask_producer = true").expect("on parse");
        assert!(c.ups.task_producer);

        // Explicit off.
        let c = parse_and_load("[ups]\ntask_producer = false").expect("off parse");
        assert!(!c.ups.task_producer);

        // Both [ups] keys coexist in one section.
        let c = parse_and_load("[ups]\ndefault_limit = 5\ntask_producer = true")
            .expect("combined parse");
        assert_eq!(c.ups.default_limit, 5);
        assert!(c.ups.task_producer);
    }

    /// `[ups] push_attempts`: default 3, explicit in-range honored, out-of-range
    /// is a CONFIG ERROR (not a silent clamp) so a typo cannot buy a different
    /// retry budget than the operator wrote.
    #[test]
    fn test_config_ups_push_attempts() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Absent -> 3.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(c.ups.push_attempts, 3);

        // Range boundaries are accepted.
        assert_eq!(
            parse_and_load("[ups]\npush_attempts = 1")
                .expect("lower bound")
                .ups
                .push_attempts,
            1
        );
        assert_eq!(
            parse_and_load("[ups]\npush_attempts = 5")
                .expect("upper bound")
                .ups
                .push_attempts,
            5
        );

        // Outside the range: refused, and the message names the key and range.
        let err = parse_and_load("[ups]\npush_attempts = 0").unwrap_err();
        assert!(
            err.contains("push_attempts") && err.contains("[1..5]"),
            "{err}"
        );
        let err = parse_and_load("[ups]\npush_attempts = 6").unwrap_err();
        assert!(
            err.contains("push_attempts") && err.contains("[1..5]"),
            "{err}"
        );

        // Non-numeric is also a config error, not a silent default.
        let err = parse_and_load("[ups]\npush_attempts = many").unwrap_err();
        assert!(err.contains("push_attempts"), "{err}");
    }

    /// 2608: `[transcode]` keys are OPTIONAL overrides; defaults computed
    /// automatically; invalid values (out of range / non-integer) are config
    /// errors so `-t` prints deterministic effective values and a typo cannot
    /// hide a runaway pool or budget.
    #[test]
    fn test_config_transcode_bounded_pool() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Absent -> automatic default N = clamp(cores/4, 1..=32), budget 64.
        let c = parse_and_load("").expect("default parse");
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        assert_eq!(c.transcode.max_concurrent, (cores / 4).clamp(1, 32) as u32);
        assert_eq!(c.transcode.max_megapixels, 64);

        // Explicit valid overrides are honored.
        let c = parse_and_load(
            "[transcode]\ntranscode_max_concurrent = 8\ntranscode_max_megapixels = 128",
        )
        .expect("custom parse");
        assert_eq!(c.transcode.max_concurrent, 8);
        assert_eq!(c.transcode.max_megapixels, 128);

        // Boundary values are valid (1 and 32; 1 and 1024).
        let c = parse_and_load(
            "[transcode]\ntranscode_max_concurrent = 1\ntranscode_max_megapixels = 1",
        )
        .expect("lower bounds");
        assert_eq!(c.transcode.max_concurrent, 1);
        assert_eq!(c.transcode.max_megapixels, 1);
        let c = parse_and_load(
            "[transcode]\ntranscode_max_concurrent = 32\ntranscode_max_megapixels = 1024",
        )
        .expect("upper bounds");
        assert_eq!(c.transcode.max_concurrent, 32);
        assert_eq!(c.transcode.max_megapixels, 1024);

        // Out-of-range -> config error (not silently clamped).
        let err = parse_and_load("[transcode]\ntranscode_max_concurrent = 33").unwrap_err();
        assert!(
            err.contains("transcode_max_concurrent") && err.contains("out of range"),
            "got: {err}"
        );
        let err = parse_and_load("[transcode]\ntranscode_max_concurrent = 0").unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
        let err = parse_and_load("[transcode]\ntranscode_max_megapixels = 2048").unwrap_err();
        assert!(err.contains("transcode_max_megapixels"), "got: {err}");

        // Non-integer -> config error.
        let err = parse_and_load("[transcode]\ntranscode_max_concurrent = lots").unwrap_err();
        assert!(err.contains("not an integer"), "got: {err}");

        // Unknown key in [transcode] -> rejected (allowlist).
        let err = parse_and_load("[transcode]\nbogus_key = 1").unwrap_err();
        assert!(err.contains("unknown"), "got: {err}");
    }

    /// Every bounded numeric key rejects garbage loudly instead of silently
    /// falling back to its default.
    ///
    /// `get_or` collapses "present but unparseable" into "absent", so a range
    /// check written after it could never fire and `abc` became the default.
    /// These cases pin the fail-fast behaviour for each hardened key: absent
    /// still yields the documented default, garbage names the section and key,
    /// and an out-of-range number names the accepted range.
    #[test]
    fn test_config_bounded_numeric_keys_fail_fast() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!("[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Absent keys keep their documented defaults (behaviour unchanged).
        let c = parse_and_load("").expect("defaults");
        assert_eq!(c.port, 8024);
        assert_eq!(c.max_connections, 512);
        assert_eq!(c.header_timeout_secs, 30);
        assert_eq!(c.body_timeout_secs, 60);
        assert_eq!(c.keepalive_timeout_secs, 10);
        assert_eq!(c.send_timeout_secs, 60);
        assert_eq!(c.drain_timeout_secs, 30);
        assert_eq!(c.max_instance_size_mb, 4096);
        assert_eq!(c.max_batch_instances, 100);
        assert_eq!(c.min_free_disk_mb, 5000);
        assert_eq!(c.min_free_disk_pct, 5.0);

        // (section, key, body) for the u64/u32 keys.
        // `port` is omitted here: the preamble below already sets it, and a
        // second `[server] port` line is a duplicate-key error, which would
        // mask the parse error under test.
        // `request_timeout_secs` is omitted because it is a RENAME_HINT: the
        // rename check runs before parsing, so that line can never reach this
        // code and always yields "renamed to".
        let int_cases: &[(&str, &str, &str)] = &[
            (
                "server",
                "max_connections",
                "[server]\nmax_connections = abc",
            ),
            (
                "server",
                "header_timeout_secs",
                "[server]\nheader_timeout_secs = abc",
            ),
            (
                "server",
                "body_timeout_secs",
                "[server]\nbody_timeout_secs = abc",
            ),
            (
                "server",
                "keepalive_timeout_secs",
                "[server]\nkeepalive_timeout_secs = abc",
            ),
            (
                "server",
                "send_timeout_secs",
                "[server]\nsend_timeout_secs = abc",
            ),
            (
                "server",
                "drain_timeout_secs",
                "[server]\ndrain_timeout_secs = abc",
            ),
            (
                "storage",
                "max_instance_size_mb",
                "[storage]\nmax_instance_size_mb = abc",
            ),
            (
                "storage",
                "max_batch_instances",
                "[storage]\nmax_batch_instances = abc",
            ),
            (
                "storage",
                "min_free_disk_mb",
                "[storage]\nmin_free_disk_mb = abc",
            ),
        ];
        for (section, key, body) in int_cases {
            let err = parse_and_load(body).unwrap_err();
            assert!(
                err.contains(section) && err.contains(key) && err.contains("is not an integer"),
                "unparseable [{section}] {key}: got {err}"
            );
        }

        // [storage] min_free_disk_pct is the one f64 key: different wording.
        let err = parse_and_load("[storage]\nmin_free_disk_pct = abc").unwrap_err();
        assert!(
            err.contains("storage") && err.contains("min_free_disk_pct"),
            "got: {err}"
        );
        assert!(err.contains("is not a number"), "got: {err}");

        // Out-of-range values name the accepted range and the offending value.
        let range_cases: &[(&str, &str, &str)] = &[
            ("server", "max_connections", "[server]\nmax_connections = 0"),
            (
                "server",
                "max_connections",
                "[server]\nmax_connections = 4097",
            ),
            (
                "server",
                "header_timeout_secs",
                "[server]\nheader_timeout_secs = 0",
            ),
            (
                "server",
                "drain_timeout_secs",
                "[server]\ndrain_timeout_secs = 0",
            ),
            (
                "server",
                "drain_timeout_secs",
                "[server]\ndrain_timeout_secs = 601",
            ),
            (
                "storage",
                "max_instance_size_mb",
                "[storage]\nmax_instance_size_mb = 0",
            ),
            (
                "storage",
                "min_free_disk_pct",
                "[storage]\nmin_free_disk_pct = 101",
            ),
        ];
        for (section, key, body) in range_cases {
            let err = parse_and_load(body).unwrap_err();
            assert!(
                err.contains(section) && err.contains(key) && err.contains("out of range"),
                "out-of-range [{section}] {key}: got {err}"
            );
        }

        // Boundary values stay accepted.
        let c = parse_and_load(
            "[server]\nmax_connections = 1\n[storage]\nmax_instance_size_mb = 1\nmin_free_disk_mb = 0\nmin_free_disk_pct = 0",
        )
        .expect("lower bounds");
        assert_eq!(c.max_connections, 1);
        assert_eq!(c.max_instance_size_mb, 1);
        assert_eq!(c.min_free_disk_mb, 0);
        assert_eq!(c.min_free_disk_pct, 0.0);
        let c = parse_and_load(
            "[server]\nmax_connections = 4096\n[storage]\nmax_instance_size_mb = 131072\nmin_free_disk_pct = 100",
        )
        .expect("upper bounds");
        assert_eq!(c.max_connections, 4096);
        assert_eq!(c.max_instance_size_mb, 131_072);
        assert_eq!(c.min_free_disk_pct, 100.0);

        // Timeouts that allow 0 ("disabled") keep allowing it.
        let c = parse_and_load("[server]\nbody_timeout_secs = 0\nkeepalive_timeout_secs = 0")
            .expect("zero means disabled for these two");
        assert_eq!(c.body_timeout_secs, 0);
        assert_eq!(c.keepalive_timeout_secs, 0);

        // `[server] port` needs its own harness: the shared preamble sets it,
        // so a second line would be a duplicate-key error rather than the
        // parse/range error under test.
        let port_only = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(&conf, format!("[server]\nhost = 127.0.0.1\n{body}\n"))
                .expect("write conf");
            Config::from_file(&conf)
        };
        let err = port_only("port = abc").unwrap_err();
        assert!(
            err.contains("server") && err.contains("port") && err.contains("is not an integer"),
            "got: {err}"
        );
        let err = port_only("port = 0").unwrap_err();
        assert!(
            err.contains("server") && err.contains("port") && err.contains("out of range"),
            "got: {err}"
        );
        let err = port_only("port = 70000").unwrap_err();
        assert!(
            err.contains("server") && err.contains("port") && err.contains("out of range"),
            "got: {err}"
        );
        assert_eq!(port_only("port = 1").expect("lower bound").port, 1);
        assert_eq!(port_only("port = 65535").expect("upper bound").port, 65_535);
        assert_eq!(port_only("").expect("absent keeps the default").port, 8024);
    }

    /// `[logging] log_level` accepts error|warn|info|debug|trace
    /// (case-insensitive, canonical lowercase, default info). Anything else
    /// is a parse error: the server refuses to start, and `-t` fails too
    /// because validation lives in `Config::from_file`.
    #[test]
    fn test_config_log_level_validation() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // All five valid values, including mixed case -> canonical lowercase.
        for (raw, canonical) in [
            ("error", LogLevel::Error),
            ("warn", LogLevel::Warn),
            ("info", LogLevel::Info),
            ("debug", LogLevel::Debug),
            ("trace", LogLevel::Trace),
            ("Error", LogLevel::Error),
            ("WARN", LogLevel::Warn),
            ("InFo", LogLevel::Info),
            ("DEBUG", LogLevel::Debug),
            ("TrAcE", LogLevel::Trace),
        ] {
            let cfg = parse_and_load(&format!("[logging]\nlog_level = {raw}\n"))
                .unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(cfg.log_level, canonical, "{raw} canonicalizes");
        }

        // Absent -> default info.
        let c = parse_and_load("").expect("default parse");
        assert_eq!(c.log_level, LogLevel::Info);

        // Garbage -> error naming the section, key, value and the allowed set.
        let err = parse_and_load("[logging]\nlog_level = banana\n").unwrap_err();
        assert!(err.contains("[logging] log_level"), "got: {err}");
        assert!(err.contains("banana"), "got: {err}");
        assert!(err.contains("error|warn|info|debug|trace"), "got: {err}");
    }

    /// PN search deviations: defaults are standard-conformant (case-sensitive
    /// glob, no forced fuzzymatch); explicit values are honored.
    #[test]
    fn test_config_query_pn_deviations() {
        let parse_and_load = |body: &str| -> Result<Config, String> {
            let dir = tempfile::tempdir().expect("tempdir");
            let conf = dir.path().join("clarus.conf");
            std::fs::write(
                &conf,
                format!(
                    "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
                ),
            )
            .expect("write conf");
            Config::from_file(&conf)
        };

        // Absent -> standard: case-sensitive PN, no override.
        let c = parse_and_load("").expect("default parse");
        assert!(c.query.case_sensitive_pn);
        assert!(!c.query.fuzzymatch_override);

        // Explicit deviations.
        let c = parse_and_load("[query]\ncase_sensitive_pn = false\nfuzzymatch_override = true")
            .expect("deviation parse");
        assert!(!c.query.case_sensitive_pn);
        assert!(c.query.fuzzymatch_override);
    }

    #[test]
    fn test_parse_size() {
        // Suffixes: k/m (any case), exact multiples.
        assert_eq!(parse_size("1m", 1024, 67_108_864), Ok(1_048_576));
        assert_eq!(parse_size("4m", 1024, 67_108_864), Ok(4_194_304));
        assert_eq!(parse_size("1M", 1024, 67_108_864), Ok(1_048_576));
        assert_eq!(parse_size("2k", 1024, 67_108_864), Ok(2048));
        assert_eq!(parse_size("2K", 1024, 67_108_864), Ok(2048));
        // Bare bytes - backward compatible.
        assert_eq!(parse_size("1048576", 1024, 67_108_864), Ok(1_048_576));
        assert_eq!(parse_size("4096", 1024, 67_108_864), Ok(4096));
        // Canonical renderer.
        assert_eq!(size_canonical(1_048_576), "1m");
        assert_eq!(size_canonical(4_194_304), "4m");
        assert_eq!(size_canonical(4096), "4k");
        assert_eq!(size_canonical(4097), "4097");
        // 0 -> out of range error.
        let err = parse_size("0", 1024, 67_108_864).unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
        // Fraction -> error.
        let err = parse_size("1.5m", 1024, 67_108_864).unwrap_err();
        assert!(err.contains("expected size"), "got: {err}");
        // Range limits honoured.
        assert!(parse_size("1k", 1024, 67_108_864).is_ok());
        assert!(parse_size("64m", 1024, 67_108_864).is_ok()); // at the top cap
        assert!(parse_size("65m", 1024, 67_108_864).is_err()); // above 64m cap
        assert!(parse_size("127", 1024, 67_108_864).is_err()); // below min
    }

    #[test]
    fn test_parse_case_insensitive() {
        let input = "[SERVER]\nHOST = 127.0.0.1\nPort = 8024\n";
        let p = ConfParser::parse(input).expect("parse should succeed");
        assert_eq!(p.get("server", "host"), Some("127.0.0.1"));
        assert_eq!(p.get("Server", "Port"), Some("8024"));
    }

    #[test]
    fn test_parse_error_on_bad_line() {
        let input = "[server]\nhost = ok\nthis is bad\n";
        let err = ConfParser::parse(input).unwrap_err();
        assert!(err.contains("expected 'key = value'"));
    }

    #[test]
    fn test_parse_empty_section_name() {
        let input = "[]\nhost = 127.0.0.1\n";
        let err = ConfParser::parse(input).unwrap_err();
        assert!(err.contains("empty section name"));
    }

    #[test]
    fn test_parse_empty_key() {
        let input = "[server]\n= value\n";
        let err = ConfParser::parse(input).unwrap_err();
        assert!(err.contains("empty key"));
    }

    #[test]
    fn test_parse_empty_value() {
        let input = "[server]\nhost =\n";
        let p = ConfParser::parse(input).expect("parse should succeed");
        assert_eq!(p.get("server", "host"), Some(""));
    }

    // -- Config tests --------------------------------------------

    #[test]
    fn test_config_defaults() {
        let input = "[server]\nhost = 0.0.0.0\n";
        let p = ConfParser::parse(input).expect("parse should succeed");
        let host: String = p.get_or("server", "host", "127.0.0.1".to_string());
        assert_eq!(host, "0.0.0.0");
        let port: u16 = p.get_or("server", "port", 8024u16);
        assert_eq!(port, 8024);
    }

    #[test]
    fn test_debug_hides_secrets() {
        // Security: verify S3 secret key is hidden in Debug output.
        // Auth secrets are no longer stored - auth is handled by nginx.
        let config = Config {
            host: "localhost".into(),
            port: 8024,
            storage_dir: "./data".into(),
            modifying_system: None,
            log_level: LogLevel::Info,
            log_dir: "./logs".into(),
            max_instance_size_mb: 4096,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_frames_per_instance: DEFAULT_MAX_FRAMES_PER_INSTANCE,
            max_batch_instances: 100,
            max_batch_bytes: 100 * 1024 * 1024,
            jpeg2000_enabled: false,
            min_free_disk_mb: 5000,
            min_free_disk_pct: 5.0,
            cold_path: None,
            migration: MigrationSchedule::disabled(),
            btree: BtreeConfig::default(),
            request_timeout_secs: 300,
            header_timeout_secs: 30,
            body_timeout_secs: 60,
            keepalive_timeout_secs: 10,
            send_timeout_secs: 60,
            drain_timeout_secs: 30,
            max_connections: 512,
            max_json_body: 1_048_576,
            max_part_header: 4_194_304,
            trusted_proxies: vec![CidrNet::V4 {
                addr: 0x7f00_0000,
                prefix: 8,
            }],
            hostname: "localhost".into(),
            require_proxy_identity: false,
            anyclass_enable: false,
            admin_cidrs: Vec::new(),
            expose_rejected_cidrs: Vec::new(),
            #[cfg(feature = "s3")]
            s3: S3Config {
                endpoint: "http://localhost:9000".into(),
                bucket: "test".into(),
                region: "us-east-1".into(),
                tier: "cold".into(),
                prefix: "store/".into(),
                access_key: "AKID123".into(),
                secret_key: "s3_super_secret".into(),
            },
            query: QueryConfig::default(),
            ups: UpsConfig::default(),
            workflow: WorkflowConfig::default(),
            transcode: TranscodeConfig {
                max_concurrent: TranscodeConfig::default_concurrency(),
                max_megapixels: DEFAULT_MAX_MEGAPIXELS,
            },
            commitment: CommitmentConfig::default(),
            audit: AuditConfig::default(),
        };
        let debug_str = format!("{:?}", config);
        // S3 secret key must be hidden (access key is intentionally visible)
        assert!(!debug_str.contains("s3_super_secret"));
    }

    #[test]
    fn test_config_bool_parsing() {
        let input = "[features]\njpeg2000 = false\n";
        let p = ConfParser::parse(input).expect("parse should succeed");
        assert!(!p.get_or::<bool>("features", "jpeg2000", true));
    }

    /// `anyclass_enable` is the consent switch for hiding DIAGNOSTIC images.
    /// Getting it wrong in either direction is a clinical failure - defaulting
    /// to on hides images a deployment never agreed to hide, defaulting a typo
    /// to off silently disables a feature an operator believes is running - so
    /// the absent case and both explicit values are pinned here.
    #[test]
    fn test_config_anyclass_enable_defaults_off_and_roundtrips() {
        // Absent -> false. This is the safety-critical direction: an existing
        // config that never mentions the key must NOT start hiding images.
        let base = Config::test_default();
        assert!(
            !base.anyclass_enable,
            "the default must be off so an unchanged config cannot hide images"
        );

        for (raw, expected) in [("true", true), ("false", false)] {
            let dir =
                std::env::temp_dir().join(format!("clarus_anyclass_{}_{raw}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join("clarus.conf");
            std::fs::write(
                &path,
                format!("[server]\nanyclass_enable = {raw}\nport = 8024\n"),
            )
            .expect("write conf");
            let cfg = Config::from_file(&path).expect("parse conf");
            assert_eq!(cfg.anyclass_enable, expected, "anyclass_enable = {raw}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn btree_paths_not_doubled_with_relative_config_path() {
        // Regression for the `tmp/tmp/./data/...` doubling: when the config is
        // given by a RELATIVE path, `base_dir` is relative and every btree
        // sub-path must resolve against it exactly ONCE. The old code built the
        // defaults from the already-resolved (relative) root and then resolved
        // again, producing `base/base/./data/...`.
        let cwd = std::env::current_dir().expect("current_dir");
        let sub = cwd.join("w1508_cfg_sub");
        std::fs::create_dir_all(&sub).expect("create subdir");
        let cfg_path = sub.join("clarus.conf");
        std::fs::write(&cfg_path, "[server]\nhost = 127.0.0.1\nport = 8024\n")
            .expect("write config");

        // A RELATIVE config path makes `base_dir` relative -> repro the bug.
        let rel = std::path::Path::new("w1508_cfg_sub/clarus.conf");
        let cfg = Config::from_file(rel).expect("load config");

        let base = "w1508_cfg_sub";
        for p in [
            &cfg.btree.root,
            &cfg.btree.manifest_dir,
            &cfg.btree.idx_dir,
            &cfg.btree.audit_dir,
        ] {
            let normalized = p.replace('\\', "/");
            let count = normalized.matches(base).count();
            assert_eq!(
                count, 1,
                "btree path '{p}' must reference base '{base}' exactly once (got {count})"
            );
        }

        let _ = std::fs::remove_dir_all(&sub);
    }

    // -- 3008 (0400,0563) modifying_system resolution -------------------

    #[test]
    fn modifying_system_default_resolves_to_clarus_server_port() {
        let mut cfg = Config::test_default();
        cfg.port = 8024;
        assert_eq!(cfg.resolved_modifying_system(), "CLARUS_SERVER:8024");
        cfg.port = 9999;
        assert_eq!(cfg.resolved_modifying_system(), "CLARUS_SERVER:9999");
    }

    #[test]
    fn modifying_system_explicit_value_roundtrips() {
        let mut cfg = Config::test_default();
        cfg.modifying_system = Some("NODE-7".to_string());
        assert_eq!(cfg.resolved_modifying_system(), "NODE-7");
        cfg.modifying_system = Some("  ".to_string());
        assert_eq!(
            cfg.resolved_modifying_system(),
            "CLARUS_SERVER:8024",
            "a blank value falls back to the default"
        );
    }

    // -- [commitment] Storage Commitment result retention ----------------

    /// Helper for the `[commitment]` tests: load a config with the given body.
    fn load_commitment(body: &str) -> Result<Config, String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let conf = dir.path().join("clarus.conf");
        std::fs::write(
            &conf,
            format!(
                "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
            ),
        )
        .expect("write conf");
        Config::from_file(&conf)
    }

    /// `[commitment] result_retention_days` parses, defaults to 30 when
    /// absent, treats 0 as "keep forever", and is range-checked rather than
    /// silently clamped.
    #[test]
    fn test_config_commitment_result_retention_days() {
        let c = load_commitment("").expect("absent section");
        assert_eq!(
            c.commitment.result_retention_days,
            CommitmentConfig::DEFAULT_RESULT_RETENTION_DAYS
        );

        let c = load_commitment("[commitment]\nresult_retention_days = 0").expect("zero parse");
        assert_eq!(
            c.commitment.result_retention_days, 0,
            "0 means keep forever"
        );

        let c = load_commitment("[commitment]\nresult_retention_days = 90").expect("custom parse");
        assert_eq!(c.commitment.result_retention_days, 90);

        let c = load_commitment("[commitment]\nresult_retention_days = 3650").expect("max parse");
        assert_eq!(c.commitment.result_retention_days, 3650);

        let err = load_commitment("[commitment]\nresult_retention_days = 3651").unwrap_err();
        assert!(err.contains("out of range"), "error: {err}");

        let err = load_commitment("[commitment]\nresult_retention_days = -1").unwrap_err();
        assert!(err.contains("is not"), "error: {err}");

        let err = load_commitment("[commitment]\nresult_retention_days = many").unwrap_err();
        assert!(err.contains("is not"), "error: {err}");
    }

    /// An unknown key inside `[commitment]` is refused (allowlist guard).
    #[test]
    fn test_config_commitment_unknown_key_is_refused() {
        let err = load_commitment("[commitment]\nnot_a_key = 1").unwrap_err();
        assert!(err.contains("not_a_key"), "error: {err}");
    }

    /// `[commitment]` is a known section, so a typo in its NAME is a
    /// section-level error rather than a silently ignored block.
    #[test]
    fn test_config_commitment_section_is_known() {
        let c = load_commitment("[commitment]\nresult_retention_days = 7").expect("section known");
        assert_eq!(c.commitment.result_retention_days, 7);

        let err = load_commitment("[commitmnet]\nresult_retention_days = 7").unwrap_err();
        assert!(err.contains("commitmnet"), "error: {err}");
    }

    // -- [workflow] MWL + MPPS (PS3.18 14/15) --------------------------

    /// Helper for the `[workflow]` tests: load a config with the given body.
    fn load_workflow(body: &str) -> Result<Config, String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let conf = dir.path().join("clarus.conf");
        std::fs::write(
            &conf,
            format!(
                "[server]\nhost = 127.0.0.1\nport = 8024\n[storage]\nstorage_dir = ./data\n{body}\n"
            ),
        )
        .expect("write conf");
        Config::from_file(&conf)
    }

    /// An absent `[workflow]` section yields the shipped defaults: the surface
    /// is off, retention 7 days, a 100-row default page bounded by 200.
    #[test]
    fn test_config_workflow_defaults() {
        let c = load_workflow("").expect("absent section");
        assert!(!c.workflow.enabled, "the surface is off by default");
        assert_eq!(
            c.workflow.retention_days,
            WorkflowConfig::DEFAULT_RETENTION_DAYS
        );
        assert_eq!(
            c.workflow.mwl_default_limit,
            WorkflowConfig::DEFAULT_MWL_DEFAULT_LIMIT
        );
        assert_eq!(
            c.workflow.mwl_max_responses,
            WorkflowConfig::DEFAULT_MWL_MAX_RESPONSES
        );
        // No dir -> the default lands beside the B-tree root, never the CWD.
        assert!(
            c.workflow.resolved_dir().ends_with("workflow"),
            "default dir: {}",
            c.workflow.resolved_dir()
        );
    }

    /// Every dial parses; 0 is legal for retention (keep forever) and for the
    /// default limit (unlimited).
    #[test]
    fn test_config_workflow_parses_each_dial() {
        let c = load_workflow(
            "[workflow]\nenabled = true\nretention_days = 30\n\
             mwl_default_limit = 0\nmwl_max_responses = 500\n",
        )
        .expect("parse");
        assert!(c.workflow.enabled);
        assert_eq!(c.workflow.retention_days, 30);
        assert_eq!(c.workflow.mwl_default_limit, 0);
        assert_eq!(c.workflow.mwl_max_responses, 500);

        let c = load_workflow("[workflow]\nretention_days = 0").expect("zero retention");
        assert_eq!(c.workflow.retention_days, 0);
    }

    /// An explicit `dir` is taken and resolved; the derived default is not.
    #[test]
    fn test_config_workflow_explicit_dir_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conf = dir.path().join("clarus.conf");
        std::fs::write(
            &conf,
            "[server]\nhost = 127.0.0.1\nport = 8024\n\
             [workflow]\nenabled = true\ndir = ./wf-store\n",
        )
        .expect("write conf");
        let c = Config::from_file(&conf).expect("parse");
        assert!(
            c.workflow.resolved_dir().ends_with("wf-store"),
            "an explicit dir is used verbatim: {}",
            c.workflow.resolved_dir()
        );
        assert!(
            std::path::Path::new(&c.workflow.resolved_dir()).is_absolute(),
            "a relative dir resolves against the config file's parent"
        );
    }

    /// Range-checked, not clamped: a value outside the accepted range refuses.
    #[test]
    fn test_config_workflow_refusals() {
        let err = load_workflow("[workflow]\nretention_days = 3651").unwrap_err();
        assert!(err.contains("out of range"), "error: {err}");

        let err = load_workflow("[workflow]\nmwl_max_responses = 0").unwrap_err();
        assert!(err.contains("out of range"), "error: {err}");

        let err = load_workflow("[workflow]\nmwl_max_responses = 10001").unwrap_err();
        assert!(err.contains("out of range"), "error: {err}");

        let err = load_workflow("[workflow]\nretention_days = many").unwrap_err();
        assert!(err.contains("is not"), "error: {err}");

        // An unknown key inside a known section is refused (allowlist guard).
        let err = load_workflow("[workflow]\nnot_a_key = 1").unwrap_err();
        assert!(err.contains("not_a_key"), "error: {err}");

        // A typo in the SECTION name is a section-level error.
        let err = load_workflow("[workfow]\nenabled = true").unwrap_err();
        assert!(err.contains("workfow"), "error: {err}");
    }
}
