// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! TID 1500 writer tests: the content tree, read the way a STRICT parser
//! reads it.
//
//! The walker here is deliberately independent of the crate's own parser and
//! of the `sr` tests' walker: it descends into every item and sequence so it
//! can disagree with a serializer that nested them wrongly. A tag-pinning
//! test cannot see such a defect - a document can carry every required tag
//! and still put them in the wrong item.

use clinfer::abi::schema::{
    Code, Measurement, MeasurementGroup, Measurements, Region, default_measurement_concept,
};
use clinfer::conf::SrTemplate;
use clinfer::measurements_writer::{
    MeasurementsIdentity, MeasurementsImage, MeasurementsProfile, serialize,
};

/// One thing found while walking the dataset.
#[derive(Debug, PartialEq, Eq)]
enum Node {
    /// An item delimiter, with the length it declared. `body` is where the
    /// item's content begins, which lets a test descend into a specific
    /// item without re-deriving offsets from the node sizes.
    Item {
        length: u32,
        body: usize,
    },
    /// A tag with its VR and value.
    Element {
        tag: (u16, u16),
        vr: [u8; 2],
        value: Vec<u8>,
    },
    SequenceEnd,
}

/// Walk an explicit-VR-LE dataset into a flat list of nodes.
fn walk(data: &[u8], from: usize) -> Vec<Node> {
    const LONG_VR: &[&[u8; 2]] = &[
        b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT", b"UV",
    ];
    let mut nodes = Vec::new();
    let mut at = from;
    while at + 8 <= data.len() {
        let group = u16::from_le_bytes([data[at], data[at + 1]]);
        let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
        if group == 0xFFFE {
            let length =
                u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]);
            at += 8;
            match element {
                0xE000 => nodes.push(Node::Item { length, body: at }),
                0xE0DD => nodes.push(Node::SequenceEnd),
                _ => {}
            }
            continue;
        }
        let vr = [data[at + 4], data[at + 5]];
        let (length, head) = if LONG_VR.contains(&&vr) {
            (
                u32::from_le_bytes([data[at + 8], data[at + 9], data[at + 10], data[at + 11]])
                    as usize,
                12,
            )
        } else {
            (u16::from_le_bytes([data[at + 6], data[at + 7]]) as usize, 8)
        };
        let start = at + head;
        if length == 0xFFFF_FFFF {
            nodes.push(Node::Element {
                tag: (group, element),
                vr,
                value: Vec::new(),
            });
            at = start;
            continue;
        }
        let end = start + length;
        if end > data.len() {
            break;
        }
        nodes.push(Node::Element {
            tag: (group, element),
            vr,
            value: data[start..end].to_vec(),
        });
        at = end;
    }
    nodes
}

/// The dataset, after the 128-byte preamble and the meta group.
fn dataset(bytes: &[u8]) -> &[u8] {
    let length = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]) as usize;
    &bytes[132 + 12 + length..]
}

fn code(value: &str, scheme: &str, meaning: &str) -> Code {
    Code {
        code: value.to_string(),
        scheme: scheme.to_string(),
        meaning: meaning.to_string(),
    }
}

fn identity() -> MeasurementsIdentity {
    MeasurementsIdentity {
        sop_instance_uid: "1.2.826.0.1.3680043.10.1234.7001".to_string(),
        study_uid: "1.2.826.0.1.3680043.10.1234.7002".to_string(),
        series_uid: "1.2.826.0.1.3680043.10.1234.7003".to_string(),
        images: vec![MeasurementsImage {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.1".to_string(),
            sop_instance_uid: "1.2.826.0.1.3680043.10.1234.7004".to_string(),
            series_uid: "1.2.826.0.1.3680043.10.1234.7005".to_string(),
        }],
        patient_name: Some("TID^ONE".to_string()),
        patient_id: Some("PT-1500".to_string()),
        patient_id_issuer: None,
        referring_physician: Some("REF^DOC".to_string()),
        study_id: Some("ST-1".to_string()),
        accession_number: Some("ACC-1".to_string()),
        station_name: Some("CLINFER".to_string()),
        scheduled_protocol_code: None,
        requested_procedure_id: None,
        requested_procedure_description: None,
        requested_procedure_code: None,
        reason_for_requested_procedure: None,
        reason_for_requested_procedure_code: None,
        algorithm_name: "densenet121-res224-all".to_string(),
        algorithm_version: Some("1.5.4".to_string()),
        algorithm_manufacturer: None,
    }
}

fn profile() -> MeasurementsProfile {
    MeasurementsProfile {
        series_description: Some("Measurements".to_string()),
        title: Some("Chest classifier".to_string()),
        template_id: SrTemplate::Tid1500,
    }
}

/// A block with `n` findings, one measurement each.
fn block(n: usize) -> Measurements {
    let groups = (0..n)
        .map(|i| MeasurementGroup {
            finding: code(&format!("11{i:04}"), "DCM", &format!("Finding {i}")),
            measurements: vec![Measurement {
                concept: None,
                value: 0.5 + (i as f64) / 100.0,
                threshold: None,
                region: None,
            }],
        })
        .collect();
    Measurements {
        unit: code("1", "UCUM", "no units"),
        groups,
    }
}

/// A closed outline region: the points are the ring, first point repeated
/// last by the caller.
fn outline(points: &[[f64; 2]]) -> Region {
    Region {
        points: points.to_vec(),
    }
}

/// A block with `n` findings, the FIRST of which carries `region`.
fn block_with_region(n: usize, region: Option<Region>) -> Measurements {
    let mut data = block(n);
    data.groups[0].measurements[0].region = region;
    data
}

/// The values of an element's text, trimmed of DICOM padding.
///
/// Both paddings are stripped: a string VR pads with a trailing SPACE, and
/// a UI pads with a trailing NUL. Trimming only spaces leaves an odd-length
/// UID's trailing NUL in the value, which makes an equality check against
/// the unpadded UID fail for a document that is in fact correct.
fn text(node: &Node) -> String {
    let Node::Element { value, .. } = node else {
        panic!("not an element");
    };
    String::from_utf8_lossy(value)
        .trim_end_matches(['\0', ' '])
        .to_string()
}

/// Whether the walk saw an element with this tag and value.
fn has(nodes: &[Node], tag: (u16, u16), value: &str) -> bool {
    nodes.iter().any(|node| match node {
        Node::Element { tag: t, .. } if *t == tag => text(node) == value,
        _ => false,
    })
}

/// Whether the walk saw this tag at all, whatever its value.
fn has_tag(nodes: &[Node], tag: (u16, u16)) -> bool {
    nodes
        .iter()
        .any(|node| matches!(node, Node::Element { tag: t, .. } if *t == tag))
}

/// The (0040,A010) value ON the item whose own (0040,A040) is `type_` -
/// the relationship the item is the TARGET of, not one its children carry.
///
/// The walk is linear, so the first element after an item delimiter is the
/// item's own Value Type and the second is its own Relationship Type.
fn own_relationship(nodes: &[Node], type_: &str) -> Option<String> {
    for (position, node) in nodes.iter().enumerate() {
        if !matches!(node, Node::Item { .. }) {
            continue;
        }
        let Some(Node::Element {
            tag: (0x0040, 0xA040),
            ..
        }) = nodes.get(position + 1)
        else {
            continue;
        };
        if text(nodes.get(position + 1).expect("the value type node")) != type_ {
            continue;
        }
        let Some(Node::Element {
            tag: (0x0040, 0xA010),
            ..
        }) = nodes.get(position + 2)
        else {
            return None;
        };
        return Some(text(
            nodes.get(position + 2).expect("the relationship node"),
        ));
    }
    None
}

/// The UL values of an element, as u32.
fn uls(node: &Node) -> Vec<u32> {
    let Node::Element { value, .. } = node else {
        panic!("not an element");
    };
    value
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// The FL values of an element, as f32.
fn fls(node: &Node) -> Vec<f32> {
    let Node::Element { value, .. } = node else {
        panic!("not an element");
    };
    value
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// The first node carrying this tag.
fn node_with(nodes: &[Node], tag: (u16, u16)) -> Option<&Node> {
    nodes.iter().find(|node| match node {
        Node::Element { tag: t, .. } => *t == tag,
        _ => false,
    })
}

/// Resolve a (0040,DB73) path against the content tree and return what is
/// inside the item it names.
///
/// The identifier is a list of 1-based item numbers read from the root
/// down, so this descends the way a reader does: into the ContentSequence,
/// then the Nth item of it, then into THAT item's ContentSequence, and so
/// on. Doing it this way is the point - it proves the identifier names a
/// real item at every level, which a test that only checked the numbers
/// could not.
///
/// The ordinals are counted at ONE DEPTH. `walk` lists items of every depth
/// it reaches, so counting its `Item` nodes directly would let a path with
/// a level too many resolve against a nested item that happens to sit at
/// the same ordinal. Each step therefore bounds itself to the children of
/// the item just chosen, and a path that asks for a level the item does not
/// have is refused rather than satisfied from below.
fn resolve_path(dataset: &[u8], levels: &[u32]) -> Vec<Node> {
    // The identifier starts at the ROOT ITEM'S CHILDREN (PS3.3 C.18.6.1.1:
    // the first value is the item number of the content item under the root
    // CONTAINER, not the number of the root itself). The dataset-level
    // ContentSequence holds exactly the root item, so its body is the level
    // the first ordinal indexes.
    let root = walk(dataset, root_content_sequence(dataset))
        .into_iter()
        .find_map(|node| match node {
            Node::Item { body, .. } => Some(body),
            _ => None,
        })
        .expect("the dataset-level ContentSequence holds the root CONTAINER item");
    let mut container =
        sequence_value(dataset, root).expect("the root CONTAINER has its own ContentSequence");

    for (depth, level) in levels.iter().enumerate() {
        let index = (*level as usize) - 1;
        let items = child_items(dataset, container);
        let body = *items.get(index).unwrap_or_else(|| {
            panic!(
                "no item {level} at depth {depth}; the identifier names a missing item \
                 (this level holds {})",
                items.len()
            )
        });
        let current = walk(dataset, body);
        if depth + 1 == levels.len() {
            return current;
        }
        container = sequence_value(dataset, body).unwrap_or_else(|| {
            panic!(
                "item {level} at depth {depth} has no ContentSequence, so the path \
                 cannot descend to depth {}",
                depth + 1
            )
        });
    }
    Vec::new()
}

/// The item bodies that are DIRECT children of the sequence at `from`.
///
/// Walks the sequence's own delimiters rather than reusing `walk`: `walk`
/// returns everything it passes, including the items of nested sequences,
/// and this is the one caller that must not see them. A defined-length item
/// is skipped whole; an undefined-length one is skipped to its Item
/// Delimitation Item, so a nested sequence inside it cannot contribute a
/// child here.
fn child_items(dataset: &[u8], from: usize) -> Vec<usize> {
    let mut items = Vec::new();
    let mut at = from;
    while at + 8 <= dataset.len() {
        let group = u16::from_le_bytes([dataset[at], dataset[at + 1]]);
        let element = u16::from_le_bytes([dataset[at + 2], dataset[at + 3]]);
        let length = u32::from_le_bytes([
            dataset[at + 4],
            dataset[at + 5],
            dataset[at + 6],
            dataset[at + 7],
        ]);
        if group != 0xFFFE {
            // An ordinary element of this level: skip its value, which may
            // itself be an undefined-length sequence.
            at = skip_element(dataset, at);
            continue;
        }
        match element {
            0xE000 => {
                items.push(at + 8);
                at = if length == 0xFFFF_FFFF {
                    skip_undefined_item(dataset, at + 8)
                } else {
                    at + 8 + length as usize
                };
            }
            // The sequence this level belongs to ended.
            0xE0DD => return items,
            _ => at += 8,
        }
    }
    items
}

/// The byte just past one element whose value may itself be undefined.
fn skip_element(dataset: &[u8], at: usize) -> usize {
    const LONG_VR: &[&[u8; 2]] = &[
        b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT", b"UV",
    ];
    if at + 8 > dataset.len() {
        return dataset.len();
    }
    let vr = [dataset[at + 4], dataset[at + 5]];
    let (length, head) = if LONG_VR.contains(&&vr) {
        (
            u32::from_le_bytes([
                dataset[at + 8],
                dataset[at + 9],
                dataset[at + 10],
                dataset[at + 11],
            ]) as usize,
            12,
        )
    } else {
        (
            u16::from_le_bytes([dataset[at + 6], dataset[at + 7]]) as usize,
            8,
        )
    };
    let start = at + head;
    if length == 0xFFFF_FFFF {
        if vr == *b"SQ" {
            return skip_undefined_sequence(dataset, start);
        }
        return start;
    }
    start + length
}

/// The byte just past the `FFFE,E00D` that closes an undefined-length item.
fn skip_undefined_item(dataset: &[u8], from: usize) -> usize {
    let mut at = from;
    while at + 8 <= dataset.len() {
        let group = u16::from_le_bytes([dataset[at], dataset[at + 1]]);
        let element = u16::from_le_bytes([dataset[at + 2], dataset[at + 3]]);
        let length = u32::from_le_bytes([
            dataset[at + 4],
            dataset[at + 5],
            dataset[at + 6],
            dataset[at + 7],
        ]);
        if group == 0xFFFE {
            match element {
                0xE00D => return at + 8,
                0xE000 => {
                    at = if length == 0xFFFF_FFFF {
                        skip_undefined_item(dataset, at + 8)
                    } else {
                        at + 8 + length as usize
                    };
                }
                _ => at += 8,
            }
            continue;
        }
        at = skip_element(dataset, at);
    }
    at
}

/// The byte just past the `FFFE,E0DD` that closes an undefined-length
/// sequence.
fn skip_undefined_sequence(dataset: &[u8], from: usize) -> usize {
    let mut at = from;
    while at + 8 <= dataset.len() {
        let group = u16::from_le_bytes([dataset[at], dataset[at + 1]]);
        let element = u16::from_le_bytes([dataset[at + 2], dataset[at + 3]]);
        let length = u32::from_le_bytes([
            dataset[at + 4],
            dataset[at + 5],
            dataset[at + 6],
            dataset[at + 7],
        ]);
        if group == 0xFFFE {
            match element {
                0xE0DD => return at + 8,
                0xE000 => {
                    at = if length == 0xFFFF_FFFF {
                        skip_undefined_item(dataset, at + 8)
                    } else {
                        at + 8 + length as usize
                    };
                }
                _ => at += 8,
            }
            continue;
        }
        at = skip_element(dataset, at);
    }
    at
}

/// The byte offset of a `(0040,A730)` ContentSequence value inside the item
/// whose body starts at `from`, or `None` when the item has none.
fn sequence_value(dataset: &[u8], from: usize) -> Option<usize> {
    let needle = [0x40u8, 0x00, 0x30, 0xA7];
    // Only the item's OWN level: a ContentSequence of a nested item must not
    // be mistaken for this one.
    let mut at = from;
    while at + 8 <= dataset.len() {
        let group = u16::from_le_bytes([dataset[at], dataset[at + 1]]);
        let element = u16::from_le_bytes([dataset[at + 2], dataset[at + 3]]);
        if group == 0xFFFE {
            let length = u32::from_le_bytes([
                dataset[at + 4],
                dataset[at + 5],
                dataset[at + 6],
                dataset[at + 7],
            ]);
            match element {
                0xE00D => return None,
                0xE000 => {
                    at = if length == 0xFFFF_FFFF {
                        skip_undefined_item(dataset, at + 8)
                    } else {
                        at + 8 + length as usize
                    };
                    continue;
                }
                _ => {
                    at += 8;
                    continue;
                }
            }
        }
        if [
            dataset[at],
            dataset[at + 1],
            dataset[at + 2],
            dataset[at + 3],
        ] == needle
        {
            // SQ is a long VR: tag(4) + VR(2) + reserved(2) + length(4).
            return Some(at + 12);
        }
        at = skip_element(dataset, at);
    }
    None
}

/// The byte offset of the root content item's ContentSequence value.
///
/// Found by locating the dataset-level `(0040,A730)` header and skipping
/// its 12-byte long-VR header, rather than by summing node sizes: the
/// header layout is fixed and known, while a size sum depends on the
/// walker's notion of every VR being right.
fn root_content_sequence(dataset: &[u8]) -> usize {
    let needle = [0x40u8, 0x00, 0x30, 0xA7];
    let at = dataset
        .windows(4)
        .position(|window| window == needle)
        .expect("the dataset has a (0040,A730) ContentSequence");
    // SQ is a long VR: tag(4) + VR(2) + reserved(2) + length(4).
    at + 12
}

// -- The tree ------------------------------------------------------------

#[test]
fn the_root_container_carries_the_tid_1500_title() {
    let bytes = serialize(&identity(), &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    // (0040,A040) CONTAINER, (0040,A050) SEPARATE, and the root concept name.
    assert!(has(&nodes, (0x0040, 0xA040), "CONTAINER"));
    assert!(has(&nodes, (0x0040, 0xA050), "SEPARATE"));
    assert!(has(&nodes, (0x0008, 0x0100), "126000"));
    assert!(has(&nodes, (0x0008, 0x0104), "Imaging Measurement Report"));
}

#[test]
fn the_root_item_carries_no_relationship_type() {
    // (0040,A010) is meaningful only for a CHILD item (PS3.3 C.17.3.1.2).
    // The root has no source content item to relate to, and a reader that
    // expects one at the root is reading a tree assembled wrongly.
    //
    // The root's body opens with (0040,A040) CONTAINER and (0040,A043) the
    // root concept, and reaches (0040,A010) only when it descends into its
    // FIRST child. That child is the title item (TID 1500 R4), a HAS
    // CONCEPT MOD of the root - which is exactly what a modifier of the
    // root looks like, and the correct first relationship in the document.
    let bytes = serialize(&identity(), &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    let relationships: Vec<String> = nodes
        .iter()
        .filter_map(|node| match node {
            Node::Element {
                tag: (0x0040, 0xA010),
                ..
            } => Some(text(node)),
            _ => None,
        })
        .collect();
    assert!(
        relationships
            .iter()
            .all(|r| r == "CONTAINS" || r == "HAS CONCEPT MOD"),
        "every relationship is one TID 1500/301 allows: {relationships:?}"
    );
    // One for the title item, one for the heading container, one for the
    // algorithm name, one for the algorithm version, then the group, its
    // image and its NUM, and finally the NUM's finding modifier. The root
    // contributes none - that is the property under test, and the exact
    // count is what pins it: a root that declared a relationship would add
    // a ninth.
    assert_eq!(
        relationships.len(),
        8,
        "the root contributes no relationship: {relationships:?}"
    );
}

#[test]
fn the_findings_are_probabilities_in_the_abi_unit() {
    let bytes = serialize(&identity(), &profile(), &block(2));
    let nodes = walk(dataset(&bytes), 0);

    // TID 1501 R1: one Measurement Group container per finding.
    let groups = nodes
        .iter()
        .filter(|node| {
            matches!(
                node,
                Node::Element {
                    tag: (0x0008, 0x0100),
                    ..
                }
            ) && text(node) == "125007"
        })
        .count();
    assert_eq!(groups, 2, "one Measurement Group per finding");

    // TID 1501 R10 / TID 300 R1: one NUM per measurement, concept 122157.
    assert!(has(&nodes, (0x0008, 0x0100), "122157"));
    assert!(has(&nodes, (0x0008, 0x0104), "Probability"));

    // TID 301 R2: the finding, as a HAS CONCEPT MOD of the NUM.
    assert!(has(&nodes, (0x0040, 0xA010), "HAS CONCEPT MOD"));
    assert!(has(&nodes, (0x0008, 0x0100), "110000"));
    assert!(has(&nodes, (0x0008, 0x0100), "110001"));
}

#[test]
fn the_unit_travels_from_the_abi_block_verbatim() {
    // The unit is not guessed: a 0..100 model's block must reach the wire
    // as %, not as the (1) the default probability would carry. The unit
    // lives INSIDE (0040,A300) MeasuredValueSequence together with the
    // value (PS3.3 C.18.1), so it is the same sequence the value is in.
    let mut with_percent = block(1);
    with_percent.unit = code("%", "UCUM", "Percent");
    let bytes = serialize(&identity(), &profile(), &with_percent);
    let nodes = walk(dataset(&bytes), 0);
    assert!(
        has_tag(&nodes, (0x0040, 0xA300)),
        "the NUM carries a measured value sequence"
    );
    assert!(has(&nodes, (0x0008, 0x0100), "%"));
    assert!(has(&nodes, (0x0008, 0x0104), "Percent"));
    assert!(
        !has(&nodes, (0x0008, 0x0100), "1"),
        "the writer must not fall back to the (1) unit"
    );
}

#[test]
fn the_numeric_value_lives_inside_the_measured_value_sequence() {
    // The defect this pins: a value written directly on the NUM item is
    // invisible to a reader, which looks for (0040,A300) and finds nothing.
    //
    // The flat walk cannot answer this, because (0040,A30A) shares its VR
    // with other DS values elsewhere in the document. So the question is
    // asked of the bytes, tracking nesting depth so that the NESTED unit
    // code sequence does not end the span early: the unit is itself a
    // sequence with its own delimiter, so a plain "first delimiter wins"
    // scan stops inside the unit instead of at the end of the measured
    // value sequence.
    let bytes = serialize(&identity(), &profile(), &block(1));
    let dataset_bytes = dataset(&bytes);
    let header = [0x40u8, 0x00, 0x00, 0xA3];
    let at = dataset_bytes
        .windows(4)
        .position(|w| w == header)
        .expect("(0040,A300) is written");
    let sentinel = [0xFFu8, 0xFF, 0xFF, 0xFF];
    assert_eq!(
        &dataset_bytes[at + 8..at + 12],
        &sentinel,
        "the measured value sequence is undefined-length"
    );

    // Find the delimiter that closes THIS sequence: walk forward counting
    // nested sequence headers against delimiters.
    let sequence_marker = |w: &[u8]| w == sentinel;
    let delimiter = [0xFEu8, 0xFF, 0xDD, 0xE0];
    let mut depth = 1usize;
    let mut cursor = at + 12;
    let mut end = None;
    while cursor + 4 <= dataset_bytes.len() {
        let window = &dataset_bytes[cursor..cursor + 4];
        if window == delimiter {
            depth -= 1;
            if depth == 0 {
                end = Some(cursor);
                break;
            }
            cursor += 8;
            continue;
        }
        // A nested SQ header ends in the sentinel at +8.
        if sequence_marker(&dataset_bytes[cursor + 4..cursor + 8]) {
            depth += 1;
        }
        cursor += 1;
    }
    let end = end.expect("the sequence is closed by its own delimiter");
    let body = &dataset_bytes[at..end];
    // (0040,A30A) Numeric Value, VR DS: the tag every reader looks for.
    let value = [0x40u8, 0x00, 0x0A, 0xA3];
    let unit = [0x40u8, 0x00, 0xEA, 0x08];
    assert!(
        body.windows(4).any(|w| w == value),
        "the value must be INSIDE (0040,A300), not beside the sequence"
    );
    assert!(
        body.windows(4).any(|w| w == unit),
        "the unit must be inside the same sequence as the value (PS3.3 C.18.1)"
    );
}

#[test]
fn a_measurement_without_a_concept_takes_the_block_default() {
    let bytes = serialize(&identity(), &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    let default = default_measurement_concept();
    assert!(has(&nodes, (0x0008, 0x0100), &default.code));
    assert!(has(&nodes, (0x0008, 0x0104), &default.meaning));
}

#[test]
fn an_explicit_concept_overrides_the_default() {
    let mut custom = block(1);
    custom.groups[0].measurements[0].concept = Some(code("111047", "DCM", "Probability of cancer"));
    let bytes = serialize(&identity(), &profile(), &custom);
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(&nodes, (0x0008, 0x0100), "111047"));
    assert!(has(&nodes, (0x0008, 0x0104), "Probability of cancer"));
}

#[test]
fn the_analyzed_image_is_named_once_per_group() {
    let bytes = serialize(&identity(), &profile(), &block(2));
    let nodes = walk(dataset(&bytes), 0);
    // TID 1501 R10b: (121112, DCM, "Source of Measurement").
    let images = nodes
        .iter()
        .filter(|node| {
            matches!(
                node,
                Node::Element {
                    tag: (0x0008, 0x0100),
                    ..
                }
            ) && text(node) == "121112"
        })
        .count();
    assert_eq!(images, 2, "one source image per Measurement Group");
    assert!(has(
        &nodes,
        (0x0008, 0x1155),
        "1.2.826.0.1.3680043.10.1234.7004"
    ));
}

#[test]
fn the_operating_threshold_travels_as_its_own_num() {
    let mut measurements = block(1);
    measurements.groups[0].measurements[0].threshold = Some(0.05);
    let bytes = serialize(&identity(), &profile(), &measurements);
    let nodes = walk(dataset(&bytes), 0);
    // The model's own per-class operating point is its data: it travels as
    // a second NUM under a 99CLARUS concept, not as our judgement. 0.05
    // states itself in four characters, so the renderer writes it exactly
    // and the trailing zeros the fixed form would pad with are not added.
    assert!(has(&nodes, (0x0008, 0x0100), "THRESHOLD"));
    assert!(has(&nodes, (0x0008, 0x0104), "Operating threshold"));
    assert!(has(&nodes, (0x0040, 0xA30A), "0.05"));
}

#[test]
fn a_small_measurement_keeps_its_magnitude_inside_the_ds_limit() {
    // DS is at most sixteen characters (PS3.5 Table 6.2-1). A sigmoid tail
    // of 1e-9 under six fixed decimals would print "0.000000" - a different
    // number - and a large value would print digits the VR cannot hold. The
    // renderer must keep both legal and faithful.
    let mut measurements = block(1);
    measurements.groups[0].measurements[0].value = 1e-9;
    let bytes = serialize(&identity(), &profile(), &measurements);
    let nodes = walk(dataset(&bytes), 0);
    let values: Vec<String> = nodes
        .iter()
        .filter(|node| {
            matches!(
                node,
                Node::Element {
                    tag: (0x0040, 0xA30A),
                    ..
                }
            )
        })
        .map(text)
        .collect();
    assert_eq!(values.len(), 1, "one NUM per measurement");
    let rendered = &values[0];
    assert!(
        rendered
            .parse::<f64>()
            .is_ok_and(|parsed| (parsed - 1e-9).abs() < 1e-12),
        "a tiny probability must survive the DS rendering, got {rendered:?}"
    );
    assert!(rendered.len() <= 16, "DS is at most 16 chars: {rendered:?}");
}

#[test]
fn the_algorithm_identity_is_written_once() {
    let bytes = serialize(&identity(), &profile(), &block(3));
    let nodes = walk(dataset(&bytes), 0);
    // TID 1500 R6b / TID 4019 R1-R2: one pair for the whole report, not one
    // per finding.
    let names = nodes
        .iter()
        .filter(|node| {
            matches!(
                node,
                Node::Element {
                    tag: (0x0008, 0x0100),
                    ..
                }
            ) && text(node) == "111001"
        })
        .count();
    assert_eq!(names, 1, "the algorithm is named once per report");
    assert!(has(&nodes, (0x0008, 0x0104), "Algorithm Name"));
    assert!(has(&nodes, (0x0008, 0x0104), "Algorithm Version"));
}

#[test]
fn a_missing_algorithm_version_omits_the_row() {
    let mut bare = identity();
    bare.algorithm_version = None;
    let bytes = serialize(&bare, &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(&nodes, (0x0008, 0x0100), "111001"));
    assert!(
        !has(&nodes, (0x0008, 0x0100), "111003"),
        "an absent version must not be written as an empty row"
    );
}

// -- What the document declares ------------------------------------------

#[test]
fn the_output_is_a_part10_file_with_the_enhanced_sr_class() {
    let bytes = serialize(&identity(), &profile(), &block(1));
    assert_eq!(&bytes[128..132], b"DICM");
    // The meta group length is the UL at 140, and its Value ends at
    // 132 + 12 + length. The three UIDs in it are UI, so each is padded to
    // an even length with a NUL - the search must cover the padded form.
    let meta_len = u32::from_le_bytes([bytes[140], bytes[141], bytes[142], bytes[143]]) as usize;
    let meta = &bytes[132..132 + 12 + meta_len];
    let meta_text = String::from_utf8_lossy(meta);
    assert!(
        meta_text.contains("1.2.840.10008.5.1.4.1.1.88.22"),
        "the file meta must declare the Enhanced SR class"
    );
    assert!(
        !meta_text.contains("1.2.840.10008.5.1.4.1.1.88.11"),
        "the Enhanced SR writer must not emit the Basic Text SR class"
    );
    // The dataset repeats the class, for a reader that receives the object
    // without its meta. Searched with the needle's own length rather than a
    // hand-counted window: the UID is 29 characters ("...10008.5.1.4.1.1.88.22").
    let dataset_bytes = dataset(&bytes);
    let needle = b"1.2.840.10008.5.1.4.1.1.88.22";
    assert!(
        dataset_bytes
            .windows(needle.len())
            .any(|w| w == needle.as_slice()),
        "the dataset must repeat the Enhanced SR class"
    );
}

#[test]
fn the_dataset_declares_utf8_as_its_character_set() {
    let bytes = serialize(&identity(), &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(&nodes, (0x0008, 0x0005), "ISO_IR 192"));
}

#[test]
fn a_cyrillic_finding_survives_as_utf8() {
    let mut cyrillic = block(1);
    cyrillic.groups[0].finding = code("110000", "DCM", "Очаговое изменение");
    let bytes = serialize(&identity(), &cyrillic_profile(), &cyrillic);
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(&nodes, (0x0008, 0x0104), "Очаговое изменение"));
}

fn cyrillic_profile() -> MeasurementsProfile {
    MeasurementsProfile {
        series_description: Some("Измерения".to_string()),
        title: Some("Классификатор".to_string()),
        template_id: SrTemplate::Tid1500,
    }
}

#[test]
fn the_completion_flags_say_complete_and_unverified() {
    let bytes = serialize(&identity(), &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(&nodes, (0x0040, 0xA491), "COMPLETE"));
    assert!(has(&nodes, (0x0040, 0xA493), "UNVERIFIED"));
}

#[test]
fn the_content_template_names_tid_1500() {
    let bytes = serialize(&identity(), &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(&nodes, (0x0040, 0xDB00), "1500"));
    assert!(has(&nodes, (0x0008, 0x0105), "DCMR"));
}

#[test]
fn the_observer_context_lives_at_the_dataset_level() {
    // TID 1001's rows are all "Required if ... not inherited" and default to
    // these attributes, which is what lets the content tree carry no context
    // items. The test pins the attribute, not the absence - the absence is
    // what the tree tests above cover.
    let bytes = serialize(&identity(), &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(&nodes, (0x0008, 0x0070), "clinfer"));
    assert!(has(&nodes, (0x0008, 0x0060), "SR"));
}

#[test]
fn the_evidence_sequence_names_the_analyzed_image() {
    let bytes = serialize(&identity(), &profile(), &block(1));
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(
        &nodes,
        (0x0020, 0x000D),
        "1.2.826.0.1.3680043.10.1234.7002"
    ));
    assert!(has(
        &nodes,
        (0x0020, 0x000E),
        "1.2.826.0.1.3680043.10.1234.7005"
    ));
    assert!(has(
        &nodes,
        (0x0008, 0x1155),
        "1.2.826.0.1.3680043.10.1234.7004"
    ));
}

#[test]
fn the_serializer_is_deterministic_apart_from_the_timestamps() {
    let a = serialize(&identity(), &profile(), &block(2));
    let b = serialize(&identity(), &profile(), &block(2));
    assert_eq!(
        a.len(),
        b.len(),
        "the same input serializes to the same size"
    );
    // (0008,0020) StudyDate onward is stable; only the two stamp pairs can
    // differ, and they are written at known offsets. Comparing the tail
    // proves the tree itself carries no ordering that varies run to run.
    let a_tree = &dataset(&a)[40..];
    let b_tree = &dataset(&b)[40..];
    assert_eq!(a_tree, b_tree, "the content tree is run-independent");
}

#[test]
fn an_empty_value_list_is_never_written() {
    // The schema refuses an empty group list, but the writer must also not
    // produce a ContentSequence with nothing under the heading: a reader
    // would show an empty document rather than no document.
    let bytes = serialize(
        &identity(),
        &profile(),
        &Measurements {
            unit: code("1", "UCUM", "no units"),
            groups: vec![],
        },
    );
    let nodes = walk(dataset(&bytes), 0);
    assert!(
        !has(&nodes, (0x0008, 0x0100), "125007"),
        "no finding means no Measurement Group"
    );
    // The heading itself is still a valid, empty container.
    assert!(has(&nodes, (0x0008, 0x0100), "126010"));
}

// -- SCOORD regions ------------------------------------------------------

#[test]
fn a_measurement_without_a_region_writes_no_scord_item() {
    let bytes = serialize(&identity(), &profile(), &block(2));
    let nodes = walk(dataset(&bytes), 0);
    assert!(
        !has(&nodes, (0x0040, 0xA040), "SCOORD"),
        "a whole-image score has no region, so no SCOORD row appears"
    );
    assert!(
        !has_tag(&nodes, (0x0040, 0xDB73)),
        "no region means no content item identifier either"
    );
    // (0070,0022) must not appear: nothing to draw.
    assert!(!has_tag(&nodes, (0x0070, 0x0022)));
}

#[test]
fn an_identifier_with_a_level_too_many_does_not_resolve() {
    // PS3.3 C.18.6.1.1 makes the identifier a path of ITEM NUMBERS, so its
    // LENGTH is part of what it names. A resolver that collects items from
    // every depth in one list cannot tell a four-number path from the
    // three-number one that prefixes it: the extra number finds an item in
    // the nested list and the path resolves to a node on the wrong level.
    //
    // The assertion is therefore that the path is REFUSED. A resolver
    // without depth awareness resolves it happily and hands back the SCOORD
    // or one of its children - an answer a reader would act on.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];
    let bytes = serialize(
        &identity(),
        &profile(),
        &block_with_region(1, Some(outline(&points))),
    );
    let dataset = dataset(&bytes);
    let nodes = walk(dataset, 0);

    let mut levels = uls(node_with(&nodes, (0x0040, 0xDB73)).expect("an identifier"));
    assert_eq!(levels.len(), 3, "the written path is three levels deep");
    levels.push(1);

    let refused = std::panic::catch_unwind(|| resolve_path(dataset, &levels));
    assert!(
        refused.is_err(),
        "a path with one level too many must not resolve: {:?}",
        refused.ok().map(|nodes| nodes.len())
    );
}

#[test]
fn a_region_is_written_as_a_scord_item_beside_its_num() {
    let points = [[10.0, 20.0], [30.0, 20.0], [30.0, 40.0]];
    let bytes = serialize(
        &identity(),
        &profile(),
        &block_with_region(1, Some(outline(&points))),
    );
    let nodes = walk(dataset(&bytes), 0);

    // The item exists and is a SCOORD whose OWN relationship is CONTAINS:
    // (0040,A010) is the relationship in which THIS item is the target, and
    // the group contains the region (TID 1501 R10c). SELECTED FROM is the
    // child IMAGE's relationship with the SCOORD, pinned by its own test
    // below. The check reads the SCOORD item itself, not any item in the
    // tree, so a stray SELECTED FROM elsewhere cannot satisfy it.
    assert!(has(&nodes, (0x0040, 0xA040), "SCOORD"));
    assert_eq!(
        own_relationship(&nodes, "SCOORD").as_deref(),
        Some("CONTAINS")
    );
    assert!(has(&nodes, (0x0070, 0x0023), "POLYLINE"));
    // And POLYGON appears nowhere: it is not a legal (0070,0023) value
    // (PS3.3 C.18.6.1.2), so a reader must never be handed one.
    assert!(
        !has(&nodes, (0x0070, 0x0023), "POLYGON"),
        "the closed outline is a POLYLINE, never a POLYGON"
    );

    // The NUM is still there: TID 1500 requires it, and a region is added
    // beside it rather than replacing it.
    assert!(has(&nodes, (0x0040, 0xA040), "NUM"));

    // Graphic Data is exactly 2 FL values per point.
    let graphic = node_with(&nodes, (0x0070, 0x0022)).expect("Graphic Data is written");
    let Node::Element { vr, .. } = graphic else {
        panic!("not an element");
    };
    assert_eq!(*vr, *b"FL", "Graphic Data is FL");
    let values = fls(graphic);
    assert_eq!(
        values.len(),
        points.len() * 2,
        "one column and one row per point"
    );
    // [column, row] order is preserved, not swapped.
    assert_eq!(values[0], 10.0, "first point's column");
    assert_eq!(values[1], 20.0, "first point's row");
    assert_eq!(values[4], 30.0, "third point's column");
    assert_eq!(values[5], 40.0, "third point's row");
}

#[test]
fn the_scord_identifier_names_the_image_item_of_its_own_group() {
    // Two groups, the FIRST carrying a region and the second not, so an
    // identifier that named the wrong group would resolve to a group with
    // no region and the check below would notice.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];
    let bytes = serialize(
        &identity(),
        &profile(),
        &block_with_region(2, Some(outline(&points))),
    );
    let dataset = dataset(&bytes);
    let nodes = walk(dataset, 0);

    let identifier = node_with(&nodes, (0x0040, 0xDB73)).expect("an identifier is written");
    let Node::Element { vr, .. } = identifier else {
        panic!("not an element");
    };
    assert_eq!(*vr, *b"UL", "(0040,DB73) is UL");
    let levels = uls(identifier);
    assert_eq!(
        levels.len(),
        3,
        "root -> heading -> group -> IMAGE is three levels"
    );

    // THE CHECK THAT MATTERS: the path resolves, and what it names is the
    // IMAGE item - the thing a reader needs to draw the region on.
    let named = resolve_path(dataset, &levels);
    assert!(
        has(&named, (0x0040, 0xA040), "IMAGE"),
        "the identifier must resolve to the group's IMAGE item, found: {named:?}"
    );
    assert!(
        has(&named, (0x0008, 0x1155), "1.2.826.0.1.3680043.10.1234.7004"),
        "the resolved item names the source instance"
    );
}

#[test]
fn the_scord_identifier_names_the_heading_not_the_title_row() {
    // The root's children are the TID 1500 R4 title row and then the
    // heading container, and the title row is CONDITIONAL. An identifier
    // that hard-coded the heading at child 1 would name the title item in
    // a titled report - a CODE item with no image under it, so a reader
    // would resolve nothing and a Weasis click would find no link.
    //
    // The two documents below differ only in whether the rule titles the
    // report, and both must resolve to the same IMAGE item.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];

    let resolved_instance = |bytes: &[u8]| -> String {
        let dataset = dataset(bytes);
        let nodes = walk(dataset, 0);
        let levels = uls(node_with(&nodes, (0x0040, 0xDB73)).expect("identifier"));
        let named = resolve_path(dataset, &levels);
        let node = node_with(&named, (0x0008, 0x1155)).expect("the named instance");
        text(node)
    };

    let titled = serialize(
        &identity(),
        &profile(),
        &block_with_region(1, Some(outline(&points))),
    );
    let mut untitled = profile();
    untitled.title = None;
    let bare = serialize(
        &identity(),
        &untitled,
        &block_with_region(1, Some(outline(&points))),
    );

    assert_eq!(
        resolved_instance(&titled),
        "1.2.826.0.1.3680043.10.1234.7004",
        "a titled report's region must still name its IMAGE item"
    );
    assert_eq!(
        resolved_instance(&bare),
        "1.2.826.0.1.3680043.10.1234.7004",
        "an untitled report's region must name the same IMAGE item"
    );

    // And the heading really does move, so the check above is not
    // satisfied by a constant: the titled document has to skip the title
    // row, so its first level is one larger.
    let first_level = |bytes: &[u8]| {
        let nodes = walk(dataset(bytes), 0);
        uls(node_with(&nodes, (0x0040, 0xDB73)).expect("identifier"))[0]
    };
    assert_eq!(
        first_level(&titled),
        2,
        "the heading is the root's 2nd child"
    );
    assert_eq!(first_level(&bare), 1, "with no title it is the 1st child");
}

#[test]
fn the_scord_identifier_follows_the_algorithm_rows_it_is_counted_past() {
    // The heading's ContentSequence holds the TID 4019 rows BEFORE the
    // groups, so the item number of group N depends on whether the
    // conditional Algorithm Version row is present. A hard-coded number
    // would break for exactly one of these two documents.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];

    let with_version = serialize(
        &identity(),
        &profile(),
        &block_with_region(1, Some(outline(&points))),
    );
    let mut without = identity();
    without.algorithm_version = None;
    let without_version = serialize(
        &without,
        &profile(),
        &block_with_region(1, Some(outline(&points))),
    );

    let named_instance = |bytes: &[u8]| -> String {
        let dataset = dataset(bytes);
        let nodes = walk(dataset, 0);
        let identifier = node_with(&nodes, (0x0040, 0xDB73)).expect("identifier");
        let levels = uls(identifier);
        let named = resolve_path(dataset, &levels);
        let node = node_with(&named, (0x0008, 0x1155)).expect("the named instance");
        text(node)
    };

    assert_eq!(
        named_instance(&with_version),
        "1.2.826.0.1.3680043.10.1234.7004",
        "with the version row present"
    );
    assert_eq!(
        named_instance(&without_version),
        "1.2.826.0.1.3680043.10.1234.7004",
        "with the version row absent"
    );

    // And the numbers really do differ between the two documents, so the
    // check above is not trivially satisfied by the same constant.
    let levels_of = |bytes: &[u8]| {
        let nodes = walk(dataset(bytes), 0);
        uls(node_with(&nodes, (0x0040, 0xDB73)).expect("identifier"))
    };
    assert_ne!(
        levels_of(&with_version),
        levels_of(&without_version),
        "the group index must shift when a row before it disappears"
    );
}

#[test]
fn a_region_writes_the_polyline_graphic_type_with_its_points() {
    // POLYLINE is the ONLY graphic type a region can carry: PS3.3
    // C.18.6.1.2 does not enumerate POLYGON, and a closed outline is a
    // POLYLINE whose last point repeats the first.
    let points = [[7.0, 8.0], [9.0, 10.0], [7.0, 8.0]];
    let region = Region {
        points: points.to_vec(),
    };
    let bytes = serialize(&identity(), &profile(), &block_with_region(1, Some(region)));
    let nodes = walk(dataset(&bytes), 0);
    assert!(has(&nodes, (0x0070, 0x0023), "POLYLINE"));
    assert!(
        !has(&nodes, (0x0070, 0x0023), "POLYGON"),
        "POLYGON is never written: it is not a legal SCOORD Graphic Type"
    );
    let values = fls(node_with(&nodes, (0x0070, 0x0022)).expect("Graphic Data"));
    assert_eq!(values, vec![7.0, 8.0, 9.0, 10.0, 7.0, 8.0]);
}

#[test]
fn only_the_measurement_that_has_a_region_gets_scord() {
    // Two measurements in one group: one with a region, one without. The
    // SCOORD count must be one, not two.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];
    let mut data = block(1);
    data.groups[0].measurements.push(Measurement {
        concept: None,
        value: 0.25,
        threshold: None,
        region: None,
    });
    data.groups[0].measurements[0].region = Some(outline(&points));

    let bytes = serialize(&identity(), &profile(), &data);
    let nodes = walk(dataset(&bytes), 0);
    let scord = nodes
        .iter()
        .filter(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA040)))
        .filter(|node| text(node) == "SCOORD")
        .count();
    assert_eq!(scord, 1, "one region, one SCOORD item");
    // Two measurements in the group still means two NUMs.
    let nums = nodes
        .iter()
        .filter(|node| matches!(node, Node::Element { tag, .. } if *tag == (0x0040, 0xA040)))
        .filter(|node| text(node) == "NUM")
        .count();
    assert_eq!(nums, 2, "both measurements keep their NUM");
}

#[test]
fn the_scord_item_names_the_image_region_it_depicts() {
    // TID 1501 R10c names the row by a parameter, and the value passed is
    // (111030, DCM, "Image Region"): an outline is a region of an image,
    // and the finding it depicts is already on the group's NUM.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];
    let bytes = serialize(
        &identity(),
        &profile(),
        &block_with_region(1, Some(outline(&points))),
    );
    let dataset = dataset(&bytes);
    let nodes = walk(dataset, 0);
    let identifier = uls(node_with(&nodes, (0x0040, 0xDB73)).expect("identifier"));

    // The SCOORD item is the LAST child of the group (after the IMAGE and
    // the NUM), so resolve to it directly.
    let mut levels = identifier.clone();
    // Replace the trailing IMAGE index with the SCOORD's own position: the
    // group holds IMAGE, NUM, then SCOORD.
    *levels.last_mut().expect("a last level") = 3;
    let scord = resolve_path(dataset, &levels);
    assert!(
        has(&scord, (0x0040, 0xA040), "SCOORD"),
        "item 3 of the group is the SCOORD"
    );
    assert!(
        has(&scord, (0x0008, 0x0100), "111030"),
        "the region row's concept name is (111030) Image Region"
    );
    assert!(has(&scord, (0x0008, 0x0104), "Image Region"));
}

#[test]
fn the_scord_has_a_selected_from_image_child_naming_the_group_image() {
    // The shape a Weasis reader needs: SRReader's SCOORD branch builds a
    // graphic ONLY inside a loop over the item's CHILDREN, and attaches it
    // to the map entry the CHILD's (0040,DB73) names. A SCOORD with no
    // children draws nothing; a child naming nothing registered draws
    // nothing. This test pins both the child's existence and the value of
    // its identifier.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];
    let bytes = serialize(
        &identity(),
        &profile(),
        &block_with_region(1, Some(outline(&points))),
    );
    let dataset = dataset(&bytes);
    let nodes = walk(dataset, 0);

    // Descend: root -> heading -> group -> SCOORD (child 3).
    let group_image = uls(node_with(&nodes, (0x0040, 0xDB73)).expect("identifier"));
    let mut scord_levels = group_image.clone();
    *scord_levels.last_mut().expect("a last level") = 3;
    let scord = resolve_path(dataset, &scord_levels);

    // The child exists, is an IMAGE, and is SELECTED FROM - TID 1501 R10d.
    assert!(has(&scord, (0x0040, 0xA040), "IMAGE"));
    assert!(has(&scord, (0x0040, 0xA010), "SELECTED FROM"));

    // The child carries NO identifier: Weasis keys its image map by its
    // own node levels (root included, "1.1.G.1"), which a conformant
    // identifier ("1.G.1") can never equal. A child that carried one would
    // be registered as a reference-less graphics entry - the outline would
    // attach to an entry that can never open. The ONLY identifier in this
    // subtree is the SCOORD's own.
    assert_eq!(
        scord
            .iter()
            .filter(|n| matches!(
                n,
                Node::Element {
                    tag: (0x0040, 0xDB73),
                    ..
                }
            ))
            .count(),
        1,
        "exactly one identifier: the SCOORD's own"
    );

    // And the SCOORD's own identifier names the group's IMAGE item - the
    // item a conformant reader attaches the region to, and the item whose
    // instance the child's (0008,1199) repeats for Weasis's fallback key.
    let named = resolve_path(dataset, &group_image);
    assert!(
        has(&named, (0x0040, 0xA040), "IMAGE"),
        "the path resolves to the IMAGE item"
    );
    assert!(
        has(&named, (0x0008, 0x1155), "1.2.826.0.1.3680043.10.1234.7004"),
        "and that item names the source instance"
    );
}

#[test]
fn the_scord_child_carries_the_image_reference_the_click_needs() {
    // SRView.openRelatedSeries looks the identifier up in a map keyed by
    // node level and requires the entry to have a SOPInstanceReference. An
    // IMAGE item registers one from (0008,1199), so the child must carry
    // that sequence or the link renders and does nothing.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];
    let bytes = serialize(
        &identity(),
        &profile(),
        &block_with_region(1, Some(outline(&points))),
    );
    let dataset = dataset(&bytes);
    let nodes = walk(dataset, 0);

    let mut scord_levels = uls(node_with(&nodes, (0x0040, 0xDB73)).expect("identifier"));
    *scord_levels.last_mut().expect("a last level") = 3;
    let scord = resolve_path(dataset, &scord_levels);

    // (0008,1199) is present on the child, with the class and instance.
    assert!(
        has_tag(&scord, (0x0008, 0x1199)),
        "the child carries a Referenced SOP Sequence"
    );
    assert!(has(&scord, (0x0008, 0x1150), "1.2.840.10008.5.1.4.1.1.1"));
    assert!(has(
        &scord,
        (0x0008, 0x1155),
        "1.2.826.0.1.3680043.10.1234.7004"
    ));

    // The graphic data still lives on the SCOORD itself, because that is
    // the item PrGraphicUtil.buildGraphic is handed.
    assert!(has_tag(&scord, (0x0070, 0x0023)));
    assert!(has_tag(&scord, (0x0070, 0x0022)));
}

#[test]
fn the_scord_identifier_is_counted_past_the_algorithm_rows() {
    // The SCOORD's own identifier names the group's IMAGE, so it must
    // shift with the conditional version row: the group's ordinal under
    // the heading is 3 with the version row and 2 without. A hard-coded
    // path would name the wrong group in one of the two shapes.
    let points = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];

    let named_instance = |bytes: &[u8]| -> String {
        let dataset = dataset(bytes);
        let nodes = walk(dataset, 0);
        let identifier = uls(node_with(&nodes, (0x0040, 0xDB73)).expect("the SCOORD identifier"));
        let named = resolve_path(dataset, &identifier);
        text(node_with(&named, (0x0008, 0x1155)).expect("the named instance"))
    };

    assert_eq!(
        named_instance(&serialize(
            &identity(),
            &profile(),
            &block_with_region(1, Some(outline(&points)))
        )),
        "1.2.826.0.1.3680043.10.1234.7004",
        "with the version row present"
    );

    let mut without = identity();
    without.algorithm_version = None;
    assert_eq!(
        named_instance(&serialize(
            &without,
            &profile(),
            &block_with_region(1, Some(outline(&points)))
        )),
        "1.2.826.0.1.3680043.10.1234.7004",
        "with the version row absent"
    );
}

// -- Data Set ordering (PS3.5 7.1) ----------------------------------------

/// Every upper-level tag, in written order, stepping over sequence contents.
///
/// The writer groups its attributes by MODULE (the way PS3.3 presents them),
/// which is not the order they must appear in. PS3.5 7.1 requires the Data Set
/// to be an ordered list in ascending tag order, and a reader that trusts the
/// order stops at the first descent. The sequences here are all
/// undefined-length with explicit delimiters, and items nest, so the walk
/// tracks depth rather than relying on a length.
fn top_level_tags(bytes: &[u8]) -> Vec<(u16, u16)> {
    const LONG_VR: &[&[u8; 2]] = &[
        b"OB", b"OD", b"OF", b"OL", b"OV", b"OW", b"SQ", b"SV", b"UC", b"UN", b"UR", b"UT", b"UV",
    ];
    let data = dataset(bytes);
    let mut tags = Vec::new();
    let mut at = 0usize;
    let mut depth = 0usize;
    while at + 8 <= data.len() {
        let group = u16::from_le_bytes([data[at], data[at + 1]]);
        let element = u16::from_le_bytes([data[at + 2], data[at + 3]]);
        if group == 0xFFFE {
            let len = u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]])
                as usize;
            at += 8;
            match element {
                0xE000 => {
                    if len == 0xFFFF_FFFF {
                        depth += 1;
                    } else {
                        at += len;
                    }
                }
                0xE00D | 0xE0DD => depth = depth.saturating_sub(1),
                _ => return tags,
            }
            continue;
        }
        let vr = [data[at + 4], data[at + 5]];
        let (length, head) = if LONG_VR.contains(&&vr) {
            (
                u32::from_le_bytes([data[at + 8], data[at + 9], data[at + 10], data[at + 11]])
                    as usize,
                12,
            )
        } else {
            (u16::from_le_bytes([data[at + 6], data[at + 7]]) as usize, 8)
        };
        if depth == 0 {
            tags.push((group, element));
        }
        if length == 0xFFFF_FFFF {
            depth += 1;
            at += head;
            continue;
        }
        at += head + length;
    }
    tags
}

/// The Enhanced SR Data Set ascends by tag.
///
/// This pinned nothing before: the writer emitted the study/series UIDs, then
/// the patient block, then more of group 0008, so the top level ran
/// 0020 -> 0010 -> 0008. A strict reader - and a conformance checker - rejects
/// that outright, and a lenient one may silently stop at the descent.
#[test]
fn the_enhanced_sr_dataset_ascends_by_tag() {
    let bytes = serialize(&identity(), &profile(), &block(1));
    let tags = top_level_tags(&bytes);
    assert!(
        tags.len() > 20,
        "the walk must cover the document header: {tags:?}"
    );
    for pair in tags.windows(2) {
        assert!(
            pair[0] < pair[1],
            "the Data Set descends: {:04X},{:04X} before {:04X},{:04X} (all: {tags:?})",
            pair[0].0,
            pair[0].1,
            pair[1].0,
            pair[1].1
        );
    }
    // The document's own identity and content must be present and in order.
    for tag in [
        (0x0008, 0x0005),
        (0x0008, 0x0016),
        (0x0010, 0x0010),
        (0x0020, 0x000D),
        (0x0020, 0x000E),
        (0x0040, 0xA491),
        (0x0040, 0xA730),
    ] {
        assert!(
            tags.contains(&tag),
            "{:04X},{:04X} is written",
            tag.0,
            tag.1
        );
    }
}
