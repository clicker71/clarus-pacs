// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! gate: byte-for-byte SQLite <-> B-tree QIDO agreement under random input.
//!
//! Answering QIDO from the B-tree tier (CBOR
//! manifests + query engine) is byte-identical to answering it from SQLite.
//! The library is DB-free, so this test models the two tiers explicitly:
//!
//! - **SQLite reference** - an in-memory vector of study attribute sets (the
//!   `Study` rows SQLite would return), filtered by the same `QueryFilter`
//!   predicate logic the engine uses.
//! - **B-tree tier** - each study written as a CBOR manifest via the STOW
//!   producer, then read back through `StudyScanner`.
//!
//! For a random population and a random query, the property asserts that the
//! **set of matching study UIDs is identical** and that the **streamed JSON
//! bytes for each study are identical** between the two tiers. Filters that
//! produce nothing on either side must agree too.
//!
//! Deterministic empty-field generation (B-007) guarantees both tiers name the
//! same `StudyInstanceUID`, so the study sets are directly comparable.
//!
//! `proptest` runs `cases` (default 256; CI passes `--cases=50000`). The
//! full 50 000-case gate is set through the env var / harness flag.

use clarus_core::manifest::AttrValue;
use clarus_core::query::json::{write_instance_json, write_series_json, write_study_json};
use clarus_core::query::series::{InstanceFilter, SeriesFilter};
use clarus_core::query::tag;
use clarus_core::query::{QueryFilter, StudyScanner};
use clarus_core::stow::StudyManifestAttrs as Attrs;
use clarus_core::stow::{InstanceModel, SeriesModel, write_study_manifest};
use proptest::prelude::*;

/// Arbitrary study-level attributes with non-empty deterministic fields.
fn arb_study() -> impl Strategy<Value = Attrs> {
    (
        "[A-Za-z0-9]{2,10}",                   // patient_name
        "[A-Za-z0-9]{2,10}",                   // patient_id
        proptest::option::of("20[0-9]{6}"),    // study_date
        prop_oneof![Just("CT"), Just("MR")],   // modality
        proptest::option::of("[A-Z0-9]{1,6}"), // accession
    )
        .prop_map(|(name, pid, date, modality, acc)| Attrs {
            patient_name: Some(name),
            patient_id: Some(pid),
            patient_id_issuer: None,
            patient_birth_date: None,
            patient_sex: None,
            study_instance_uid: String::new(), // deterministically filled below
            study_date: date,
            study_time: None,
            study_description: None,
            accession_number: acc,
            referring_physician: None,
            performing_physician: None,
            name_of_reading_physicians: None,
            institution_name: None,
            requesting_physician: None,
            requested_procedure_description: None,
            responsible_person: None,
            modality: modality.to_string(),
            study_id: None,
            num_series: 0,
            num_instances: 0,
            modalities_in_study: None,
            other_patient_ids: None,
        })
}

/// Deterministically name a study's StudyInstanceUID from its raw fields
/// (mirrors B-007) so both tiers derive the same UID.
fn name_study(mut a: Attrs) -> Attrs {
    if a.study_instance_uid.is_empty() {
        let fallback_id = a.patient_id.clone().unwrap_or_default();
        a.study_instance_uid = clarus_core::stow::generate_study_instance_uid(
            a.patient_id.as_deref().unwrap_or(""),
            a.study_date.as_deref(),
            None, // study_time
            a.accession_number.as_deref(),
            &a.modality,
        )
        .unwrap_or_else(|_| format!("2.25.{}", fallback_id.len() as u64));
    }
    a
}

/// A random QIDO study-level query over the population's key space.
fn arb_filter() -> impl Strategy<Value = QueryFilter> {
    (
        proptest::option::of("[A-Za-z0-9]{1,6}"), // patient_name (prefix)
        proptest::option::of("[A-Za-z0-9]{1,6}"), // patient_id (exact-ish)
        proptest::option::of("20[0-9]{6}"),       // study_date_from
        proptest::option::of("20[0-9]{6}"),       // study_date_to
        proptest::option::of(
            prop_oneof![Just("CT"), Just("MR"), Just("US")].prop_map(|s: &str| s.to_string()),
        ), // modality
    )
        .prop_map(|(name, pid, from, to, modality)| QueryFilter {
            patient_name: name.map(|n| format!("{n}*")),
            patient_id: pid,
            study_date_from: from,
            study_date_to: to,
            modality,
            ..Default::default()
        })
}

// -- series/instance parity helpers ----------------------------------
//
// The series/instance proptest models the SQLite tier as an in-memory
// reference over the same `SeriesModel`/`InstanceModel` population the B-tree
// manifest is built from. These helpers rebuild the canonical `(tag, value)`
// attribute lists (mirroring `stow::manifest::series_attrs` /
// `instance_attrs`) and the numeric sort key, so the reference is a faithful,
// DB-free oracle for the SQLite backend's filter/sort semantics.

/// Canonical series attribute vector from a model (mirrors `series_attrs`).
fn model_series_attrs(s: &SeriesModel) -> Vec<(u32, AttrValue)> {
    let mut v: Vec<(u32, AttrValue)> = Vec::with_capacity(9);
    if let Some(d) = s.series_date.as_deref()
        && !d.is_empty()
        && d.len() == 8
        && d.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = d.parse::<u64>()
    {
        v.push((tag::SERIES_DATE, AttrValue::UInt(n)));
    }
    if let Some(t) = s.series_time.as_deref()
        && !t.is_empty()
    {
        v.push((tag::SERIES_TIME, AttrValue::Text(t.to_string())));
    }
    if !s.modality.is_empty() {
        v.push((tag::MODALITY, AttrValue::Text(s.modality.clone())));
    }
    if let Some(n) = s.station_name.as_deref()
        && !n.is_empty()
    {
        v.push((tag::STATION_NAME, AttrValue::Text(n.to_string())));
    }
    if let Some(d) = s.series_description.as_deref()
        && !d.is_empty()
    {
        v.push((tag::SERIES_DESCRIPTION, AttrValue::Text(d.to_string())));
    }
    if let Some(b) = s.body_part_examined.as_deref()
        && !b.is_empty()
    {
        v.push((tag::BODY_PART_EXAMINED, AttrValue::Text(b.to_string())));
    }
    if !s.series_instance_uid.is_empty() {
        v.push((
            tag::SERIES_INSTANCE_UID,
            AttrValue::Text(s.series_instance_uid.clone()),
        ));
    }
    if let Some(n) = s.series_number.as_deref()
        && !n.is_empty()
    {
        v.push((tag::SERIES_NUMBER, AttrValue::Text(n.to_string())));
    }
    if let Some(l) = s.laterality.as_deref()
        && !l.is_empty()
    {
        v.push((tag::LATERALITY, AttrValue::Text(l.to_string())));
    }
    v
}

/// Canonical instance attribute vector from a model (mirrors `instance_attrs`).
fn model_instance_attrs(m: &InstanceModel) -> Vec<(u32, AttrValue)> {
    let mut v: Vec<(u32, AttrValue)> = Vec::with_capacity(3);
    if let Some(u) = m.sop_class_uid.as_deref()
        && !u.is_empty()
    {
        v.push((tag::SOP_CLASS_UID, AttrValue::Text(u.to_string())));
    }
    v.push((
        tag::SOP_INSTANCE_UID,
        AttrValue::Text(m.sop_instance_uid.clone()),
    ));
    if let Some(n) = m.instance_number.as_deref()
        && !n.is_empty()
    {
        v.push((tag::INSTANCE_NUMBER, AttrValue::Text(n.to_string())));
    }
    v
}

/// Series sort key `(present, numeric, uid)` mirroring `cmp_series`.
fn ref_series_key(attrs: &[(u32, AttrValue)]) -> (bool, u64, String) {
    let uid = text_of(attrs, tag::SERIES_INSTANCE_UID)
        .unwrap_or_default()
        .to_string();
    match number_of(attrs, tag::SERIES_NUMBER) {
        Some(n) => (true, n, uid),
        None => (false, 0, uid),
    }
}

/// Instance sort key `(present, numeric, uid)` mirroring `cmp_instance`.
fn ref_instance_key(attrs: &[(u32, AttrValue)]) -> (bool, u64, String) {
    let uid = text_of(attrs, tag::SOP_INSTANCE_UID)
        .unwrap_or_default()
        .to_string();
    match number_of(attrs, tag::INSTANCE_NUMBER) {
        Some(n) => (true, n, uid),
        None => (false, 0, uid),
    }
}

/// Series comparator mirroring the B-tree `cmp_series`: numeric ASC, secondary
/// UID ASC, missing/non-numeric LAST (present sorts before missing).
fn ref_cmp_series(
    a_attrs: &[(u32, AttrValue)],
    b_attrs: &[(u32, AttrValue)],
) -> std::cmp::Ordering {
    let (ap, an, au) = ref_series_key(a_attrs);
    let (bp, bn, bu) = ref_series_key(b_attrs);
    bp.cmp(&ap)
        .then_with(|| an.cmp(&bn))
        .then_with(|| au.cmp(&bu))
}

/// Instance comparator mirroring the B-tree `cmp_instance`.
fn ref_cmp_instance(
    a_attrs: &[(u32, AttrValue)],
    b_attrs: &[(u32, AttrValue)],
) -> std::cmp::Ordering {
    let (ap, an, au) = ref_instance_key(a_attrs);
    let (bp, bn, bu) = ref_instance_key(b_attrs);
    bp.cmp(&ap)
        .then_with(|| an.cmp(&bn))
        .then_with(|| au.cmp(&bu))
}

/// First `Text` value of `tag`, or `None`.
fn text_of(attrs: &[(u32, AttrValue)], tag: u32) -> Option<&str> {
    attrs
        .iter()
        .find(|(t, _)| *t == tag)
        .and_then(|(_, v)| match v {
            AttrValue::Text(s) => Some(s.as_str()),
            _ => None,
        })
}

/// Numeric value of `tag` (parsed `Text`), or `None` when absent/non-numeric.
fn number_of(attrs: &[(u32, AttrValue)], tag: u32) -> Option<u64> {
    text_of(attrs, tag).and_then(|s| s.parse::<u64>().ok())
}

/// Random instances under a series (boundary-prone: non-numeric InstanceNumber,
/// leading-zero SOP UIDs, missing SOPClassUID).
fn arb_instance() -> impl Strategy<Value = InstanceModel> {
    (
        prop_oneof![
            "[0-9]{1,3}",            // numeric instance number
            Just("abc".to_string()), // non-numeric -> sorts last
            Just("".to_string()),    // empty -> absent (sorts last)
        ],
        "1[0-9]{6,8}", // SOP instance UID suffix
        proptest::option::of(prop_oneof![
            Just("1.2.840.10008.5.1.4.1.1.2".to_string()),
            Just("1.2.840.10008.5.1.4.1.1.4".to_string()),
        ]),
    )
        .prop_map(|(num, suf, cls)| InstanceModel {
            sop_instance_uid: format!("1.2.840.{suf}.{}", num.len()),
            sop_class_uid: cls,
            instance_number: Some(num),
            blake3: [0u8; 32],
            storage_tier: 1,
            transfer_syntax_uid: String::new(),
            file_size: 0,
            ..Default::default()
        })
}

/// Random series (boundary-prone: wildcards in Description/Time, non-numeric
/// SeriesNumber, leading-zero dates).
fn arb_series() -> impl Strategy<Value = SeriesModel> {
    (
        "[1-9][0-9]{5,9}", // series instance UID suffix
        prop_oneof![
            Just("CT".to_string()),
            Just("MR".to_string()),
            Just("US".to_string())
        ],
        prop_oneof!["[0-9]{1,3}", Just("abc".to_string()), Just("".to_string()),],
        proptest::option::of("0?20[0-9]{5}"), // leading-zero dates
        proptest::option::of(prop_oneof![
            Just("T2*".to_string()),
            Just("*Axial".to_string()),
            Just("Chest?".to_string()),
            Just("%_".to_string()), // literal %/_ (glob-escaped parity edge)
            "[A-Za-z0-9]{1,8}",
        ]),
        proptest::option::of("[0-9]{4,8}"), // series time
        proptest::collection::vec(arb_instance(), 0..4),
    )
        .prop_map(|(suf, mod_, num, date, desc, time, insts)| SeriesModel {
            series_instance_uid: format!("1.2.840.{suf}"),
            modality: mod_,
            series_number: Some(num),
            series_date: date,
            series_time: time,
            pps_start_date: None,
            pps_start_time: None,
            scheduled_procedure_step_id: None,
            requested_procedure_id: None,
            station_name: None,
            manufacturer: None,
            institutional_department_name: None,
            operators_name: None,
            series_description: desc,
            body_part_examined: None,
            protocol_name: None,
            laterality: None,
            instances: insts,
        })
}

/// Random series-level filter (all six keys, wildcard-prone).
fn arb_series_filter() -> impl Strategy<Value = SeriesFilter> {
    (
        proptest::option::of(prop_oneof!["1[0-9]{5,9}", "[0-9]",]), // series_instance_uid (exact; small index may match population)
        proptest::option::of(prop_oneof![
            Just("CT".to_string()),
            Just("MR".to_string()),
            Just("CT,MR".to_string()),
        ]), // modality (comma list)
        proptest::option::of("0?20[0-9]{5}"), // series_date (leading zeros + plain)
        proptest::option::of(prop_oneof![
            Just("*".to_string()),
            Just("?0?".to_string()),
            "[0-9]{2,6}",
            Just("%".to_string()),
            Just("_".to_string()),
        ]), // series_time (wildcards)
        proptest::option::of(prop_oneof![
            Just("*".to_string()),
            Just("Chest*".to_string()),
            Just("?axial".to_string()),
            "[A-Za-z0-9]*",
            Just("%".to_string()),
            Just("_".to_string()),
        ]), // series_description (wildcards)
    )
        .prop_map(|(uid, mod_, date, time, desc)| SeriesFilter {
            series_instance_uid: uid.map(|u| format!("1.2.840.{u}")),
            modality: mod_,
            series_date: date,
            series_time: time,
            series_description: desc,
            ..SeriesFilter::default()
        })
}

/// Random instance-level filter (SOPInstanceUID exact; the instance-level keys
/// SOPClassUID / InstanceNumber stay `None` here - the parity oracle covers
/// the matching itself).
fn arb_instance_filter() -> impl Strategy<Value = InstanceFilter> {
    proptest::option::of("1[0-9]{6,8}").prop_map(|u| InstanceFilter {
        sop_instance_uid: u.map(|x| format!("1.2.840.{x}.{}", 0)),
        sop_class_uid: None,
        instance_number: None,
    })
}

/// Build a `SeriesRecord` from a model (mirrors `stow::manifest::series_record`)
/// so the reference can render byte-identical JSON through the same writer.
fn model_to_series_record(s: &SeriesModel) -> clarus_core::manifest::SeriesRecord {
    clarus_core::manifest::SeriesRecord {
        attrs: model_series_attrs(s),
        instances: s.instances.iter().map(model_to_instance_record).collect(),
    }
}

/// Build an `InstanceRecord` from a model (mirrors `instance_record`).
fn model_to_instance_record(m: &InstanceModel) -> clarus_core::manifest::InstanceRecord {
    clarus_core::manifest::InstanceRecord {
        attrs: model_instance_attrs(m),
        transfer_syntax: String::new(),
        storage_tier: 1,
        rejection_reason: 0,
        blake3: [0u8; 32],
        file_size: 0,
        frames: Vec::new(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        // File-I/O bound (a tempdir per case); the full 50 000-case sweep runs
        // in CI via `--cases=50000`. Locally keep it small.
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// SQLite and B-tree must answer QIDO identically (set + byte-for-byte JSON).
    #[test]
    fn sqlite_and_btree_agree(
        pop in proptest::collection::vec(arb_study(), 0..6),
        filter in arb_filter(),
    ) {
        let mut pop: Vec<Attrs> = pop.into_iter().map(name_study).collect();
        // -- B-tree tier: write each study as a CBOR manifest --
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&manifests).unwrap();
        for a in &mut pop {
            write_study_manifest(&manifests, a).unwrap();
        }
        // Lazy recovery (C-008) builds the modality accelerator from manifests
        // at startup, exactly as the server does before serving QIDO. The dir
        // always exists (created above), so an empty population yields a 0
        // report and both tiers agree on the empty result.
        clarus_core::recovery::reconcile_idx(&manifests, &idx).unwrap();
        let scanner = StudyScanner::new(manifests, idx);
        let (btree, _) = scanner.search(&filter).unwrap();

        // -- SQLite reference: filter the in-memory population --
        // The B-tree tier (scan.rs::search) sorts /studies by
        // (study_date ASC, study_uid ASC): numeric 8-digit YYYYMMDD, missing
        // date last (u64::MAX), UID tiebreak. The reference here must sort
        // identically or a same-date study pair (or different-dated studies)
        // diverges - a real client-visible divergence the comparison must catch.
        let mut ref_keys: Vec<(u64, String)> = pop
            .iter()
            .filter(|a| {
                // Rebuild the study attrs list and apply the same predicate.
                let attrs = clarus_core::stow::manifest::study_attrs(a);
                filter.matches(&attrs)
            })
            .map(|a| {
                let date_key = a
                    .study_date
                    .as_deref()
                    .and_then(|d| d.parse::<u64>().ok())
                    .unwrap_or(u64::MAX);
                (date_key, a.study_instance_uid.clone())
            })
            .collect();
        // Sort by (date, uid): `(u64, String)` sorts lexicographically on the
        // tuple, which is exactly (date ASC, uid ASC).
        ref_keys.sort();
        // Keep only the UIDs (in the sorted order) for the set comparison.
        let ref_uids: Vec<String> = ref_keys.into_iter().map(|(_, uid)| uid).collect();

        // -- Compare the matched study UID sets --
        let btree_uids: Vec<String> = btree.iter().map(|r| r.study_uid.clone()).collect();
        prop_assert_eq!(&btree_uids, &ref_uids, "B-tree and SQLite question UID sets differ");

        // -- Byte-for-byte JSON for a matched subset --
        // Both tiers stream from the same attrs through the same writer, so any
        // matched study must stream identical JSON. Compare every matched study.
        // A generous fixed buffer is reused; a study that overflows it is a test
        // configuration failure, not a data mismatch.
        let mut buf_ref = [0u8; 4096];
        let mut buf_btree = [0u8; 4096];
        for r in &btree {
            let a = pop.iter().find(|a| a.study_instance_uid == r.study_uid).unwrap();
            let ref_attrs = clarus_core::stow::manifest::study_attrs(a);
            let ref_qido = clarus_core::manifest::StudyQido {
                attrs: ref_attrs,
                ..Default::default()
            };
            let n_ref = write_study_json(&mut buf_ref, &ref_qido, None, None).expect("fits");
            let n_btree = write_study_json(&mut buf_btree, &r.qido, None, None).expect("fits");

            prop_assert_eq!(
                &buf_btree[..n_btree],
                &buf_ref[..n_ref],
                "streamed JSON differs between SQLite reference and B-tree"
            );
        }
    }

    /// series + instance QIDO must agree between the B-tree search path
    /// and the in-memory SQLite-parity reference: same match set, same sort
    /// order, byte-for-byte JSON. Runs the full decode -> filter -> sort ->
    /// write pipeline in memory (no disk), so a 10 000+ case sweep is fast.
    #[test]
    fn sqlite_and_btree_agree_series_instances(
        series in proptest::collection::vec(arb_series(), 0..6),
        sfilter in arb_series_filter(),
        ifilter in arb_instance_filter(),
    ) {
        let attrs = Attrs {
            patient_name: Some("PROP".to_string()),
            patient_id: Some("P".to_string()),
            study_instance_uid: "1.2.840.111.1".to_string(),
            modality: "CT".to_string(),
            ..Default::default()
        };
        // -- Disambiguate: give every series a distinct SeriesInstanceUID so
        // the two tiers zip unambiguously even when the generated suffix
        // collides (a duplicate UID is valid DICOM but would alias the 1:1
        // series-pairing this property intends to compare). --
        let mut series: Vec<SeriesModel> = series;
        for (i, s) in series.iter_mut().enumerate() {
            s.series_instance_uid = format!("1.2.840.{i}");
        }
        // -- B-tree tier: build the in-memory manifest (same encoding the
        // manifest writer / recovery path would commit to disk) --
        let manifest = clarus_core::stow::build_manifest_full(&attrs, &series);

        // -- Reference tier: filter + sort the in-memory models --
        let mut ref_series: Vec<&SeriesModel> = series
            .iter()
            .filter(|s| sfilter.matches(&model_series_attrs(s)))
            .collect();
        ref_series.sort_by(|a, b| {
            ref_cmp_series(&model_series_attrs(a), &model_series_attrs(b))
        });

        // -- B-tree series search --
        let mut btree: Vec<&clarus_core::manifest::SeriesRecord> = Vec::new();
        clarus_core::query::series::search_series(&manifest, &sfilter, &mut btree);

        // Match set + order must agree.
        let btree_uids: Vec<String> = btree
            .iter()
            .map(|r| {
                text_of(&r.attrs, tag::SERIES_INSTANCE_UID)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let ref_uids: Vec<String> = ref_series
            .iter()
            .map(|s| s.series_instance_uid.clone())
            .collect();
        prop_assert_eq!(&btree_uids, &ref_uids, "series match sets differ");

        // Byte-for-byte JSON for each matched series.
        let mut buf_b = [0u8; 4096];
        let mut buf_r = [0u8; 4096];
        for (b, r) in btree.iter().zip(ref_series.iter()) {
            let nb = write_series_json(&mut buf_b, b, None, None, None).expect("fits");
            let rr = model_to_series_record(r);
            let nr = write_series_json(&mut buf_r, &rr, None, None, None).expect("fits");
            prop_assert_eq!(
                &buf_b[..nb],
                &buf_r[..nr],
                "series JSON differs between B-tree and reference"
            );
        }

        // -- Instances: compare the first matched series (if any) --
        if let Some(ref_s) = ref_series.first() {
            let series_uid = &ref_s.series_instance_uid;
            // Reference: filter + sort instance models under that series.
            let mut ref_insts: Vec<&InstanceModel> = ref_s
                .instances
                .iter()
                .filter(|m| ifilter.matches(&model_instance_attrs(m)))
                .collect();
            ref_insts.sort_by(|a, b| {
                ref_cmp_instance(&model_instance_attrs(a), &model_instance_attrs(b))
            });

            // B-tree: search instances under that series.
            let mut btree_insts: Vec<&clarus_core::manifest::InstanceRecord> = Vec::new();
            let present =
                clarus_core::query::series::search_instances(
                    &manifest,
                    series_uid,
                    &ifilter,
                    &mut btree_insts,
                    false,
                );
            prop_assert!(present, "matched series must exist in the B-tree manifest");

            let b_uid: Vec<String> = btree_insts
                .iter()
                .map(|i| {
                    text_of(&i.attrs, tag::SOP_INSTANCE_UID)
                        .unwrap_or_default()
                        .to_string()
                })
                .collect();
            let r_uid: Vec<String> = ref_insts
                .iter()
                .map(|m| m.sop_instance_uid.clone())
                .collect();
            prop_assert_eq!(&b_uid, &r_uid, "instance match sets differ");

            for (bi, ri) in btree_insts.iter().zip(ref_insts.iter()) {
                let nb = write_instance_json(&mut buf_b, bi, "1.2.840.111.1", series_uid, None, None)
                    .expect("fits");
                let rr = model_to_instance_record(ri);
                let nr =
                    write_instance_json(&mut buf_r, &rr, "1.2.840.111.1", series_uid, None, None)
                        .expect("fits");
                prop_assert_eq!(
                    &buf_b[..nb],
                    &buf_r[..nr],
                    "instance JSON differs between B-tree and reference"
                );
            }
        }
    }

    /// the RAM `StudyQidoIndex` cold path must be byte-identical to the
    /// manifest-scan reference: same matched UID set, same order, byte-for-byte
    /// `write_study_json`. Builds one manifest population, then answers a random
    /// filter through both a scan-only scanner and an index-attached scanner.
    #[test]
    fn index_path_matches_scan_path(
        pop in proptest::collection::vec(arb_study(), 0..6),
        filter in arb_filter(),
    ) {
        let mut pop: Vec<Attrs> = pop.into_iter().map(name_study).collect();
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        let idx = dir.path().join("idx");
        std::fs::create_dir_all(&manifests).unwrap();
        for a in &mut pop {
            write_study_manifest(&manifests, a).unwrap();
        }
        clarus_core::recovery::reconcile_idx(&manifests, &idx).unwrap();

        // Scan path: no index attached (the reference / lazy fallback).
        let scan = StudyScanner::new(manifests.clone(), idx.clone());
        let (scan_res, _) = scan.search(&filter).expect("scan-path search");

        // Index path: attach a RAM index built from the same manifests.
        let mut with_index = scan.clone();
        with_index.attach_index(clarus_core::query::StudyQidoIndex::build_from_dir(&manifests));
        let (index_res, _) = with_index.search(&filter).expect("index-path search");

        // Same matched UID set + order.
        let scan_uids: Vec<String> = scan_res.iter().map(|r| r.study_uid.clone()).collect();
        let index_uids: Vec<String> = index_res.iter().map(|r| r.study_uid.clone()).collect();
        prop_assert_eq!(&scan_uids, &index_uids, "index and scan UID sets differ");

        // Byte-for-byte JSON for every matched study.
        let mut buf_s = [0u8; 4096];
        let mut buf_i = [0u8; 4096];
        for (s, i) in scan_res.iter().zip(index_res.iter()) {
            let ns = write_study_json(&mut buf_s, &s.qido, None, None).expect("fits");
            let ni = write_study_json(&mut buf_i, &i.qido, None, None).expect("fits");
            prop_assert_eq!(
                &buf_i[..ni],
                &buf_s[..ns],
                "streamed JSON differs between index and scan paths"
            );
        }
    }
}
