// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! `--inspect <study>`: print what the origin actually holds.
//!
//! A field engineer needs to see which of the tags a rule matches on the
//! device really writes. The command reads the study, its series and its
//! instances, and prints the NON-EMPTY values, because an empty tag is
//! exactly the fact worth seeing: a site whose rules never match is
//! usually a site whose StudyDescription is blank.

use crate::dicomweb::{Dicomweb, Result};
use crate::json::Json;

/// Render one level of the QIDO tree.
///
/// The output is meant to be grepped and read in a terminal, so it is one
/// `tag  value` line per attribute with no decoration.
pub fn inspect(origin: &Dicomweb, study_uid: &str) -> Result<String> {
    let mut out = String::with_capacity(1024);
    out.push_str(&format!("study {study_uid}\n"));
    // The study comes from the SearchForStudies transaction, which answers a
    // list; a study that is not there yields an empty list and no attributes.
    let study_document = origin.study(study_uid)?;
    if let Some(study) = first_item(&study_document) {
        write_attributes(&mut out, study, "  ");
    }

    let series = origin.series(study_uid)?;
    // PS3.18 F.2.1 makes a search answer a top-level array, so a non-array is
    // a protocol violation and the inspection stops with the reason rather
    // than printing an empty tree. An empty array is a real answer - the
    // study has no series - and prints as one.
    let series_items = series.as_array().ok_or_else(|| non_array("series"))?;
    for item in series_items {
        let uid = item
            .attribute(crate::dicomweb::TAG_SERIES_INSTANCE_UID)
            .unwrap_or("<no SeriesInstanceUID>");
        out.push_str(&format!("  series {uid}\n"));
        write_attributes(&mut out, item, "    ");
        if uid == "<no SeriesInstanceUID>" {
            continue;
        }
        let instances = origin.instances(study_uid, uid)?;
        let instance_items = instances.as_array().ok_or_else(|| non_array("instances"))?;
        for instance in instance_items {
            out.push_str("    instance\n");
            write_attributes(&mut out, instance, "      ");
        }
    }
    Ok(out)
}

/// The failure an inspection reports when a search answered something other
/// than the array PS3.18 F.2.1 requires.
fn non_array(level: &str) -> crate::dicomweb::Error {
    crate::dicomweb::Error::Status(crate::dicomweb::StatusError {
        status: 0,
        body: format!(
            "the {level} search answered a non-array; PS3.18 F.2.1 makes the \
             result of a search a top-level JSON array"
        ),
    })
}

/// The first item of a QIDO answer, or `None` when the answer is not a list
/// or the list is empty.
///
/// The search transactions answer a list even when exactly one study matches,
/// so the single result an inspection is after is the first element. An
/// origin that answers the bare object is still read, because both shapes
/// occur in the field.
fn first_item(document: &Json) -> Option<&Json> {
    match document.as_array() {
        Some(items) => items.first(),
        None => Some(document),
    }
}

/// Print every attribute of one QIDO item that carries a value.
fn write_attributes(out: &mut String, item: &Json, indent: &str) {
    let Json::Obj(entries) = item else {
        return;
    };
    for (tag, value) in entries {
        if tag == "vr" {
            continue;
        }
        let Some(text) = rendered(value) else {
            continue;
        };
        out.push_str(&format!("{indent}{tag}  {text}\n"));
    }
}

/// The printable form of one attribute value, or `None` when it is empty.
///
/// The `vr` marker is dropped: an engineer reading this wants the value,
/// and the tag name already locates it in the standard.
fn rendered(value: &Json) -> Option<String> {
    let inner = value.get("Value")?;
    let items = inner.as_array()?;
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        parts.push(match item {
            Json::Str(text) if !text.trim().is_empty() => text.clone(),
            Json::Str(_) => continue,
            Json::U64(number) => number.to_string(),
            Json::F64(number) => number.to_string(),
            Json::Bool(flag) => flag.to_string(),
            Json::Obj(_) => {
                // A PersonName or a coded entry: show its parts.
                let mut fields = Vec::new();
                if let Json::Obj(pairs) = item {
                    for (key, field) in pairs {
                        if field.as_str().is_some_and(|text| !text.trim().is_empty()) {
                            fields.push(format!("{key}={}", field.as_str().unwrap_or("")));
                        }
                    }
                }
                if fields.is_empty() {
                    continue;
                }
                fields.join(" ")
            }
            _ => continue,
        });
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_render_one_line_per_tag() {
        let item = crate::json::parse(
            br#"{"00100010":{"vr":"PN","Value":[{"Alphabetic":"PETROV^IVAN",
                                                   "Ideographic":"","Phonetic":""}]},
                 "00081030":{"vr":"LO","Value":["CHEST PA"]}}"#,
        )
        .expect("parses");
        let mut out = String::new();
        write_attributes(&mut out, &item, "  ");
        assert!(
            out.contains("  00100010  Alphabetic=PETROV^IVAN\n"),
            "{out}"
        );
        assert!(out.contains("  00081030  CHEST PA\n"), "{out}");
        // The `vr` marker is not an attribute and is not printed.
        assert!(!out.contains("vr"), "{out}");
    }

    #[test]
    fn empty_values_are_not_printed() {
        let item = crate::json::parse(
            br#"{"00081030":{"vr":"LO","Value":[""]},
                 "00080060":{"vr":"CS","Value":["  "]},
                 "00100020":{"vr":"LO","Value":["PT-1"]}}"#,
        )
        .expect("parses");
        let mut out = String::new();
        write_attributes(&mut out, &item, "");
        // The whole point of the command: a blank StudyDescription is the
        // fact an engineer is looking for, and it shows by being absent.
        assert!(!out.contains("00081030"), "{out}");
        assert!(!out.contains("00080060"), "{out}");
        assert!(out.contains("00100020  PT-1"), "{out}");
    }

    #[test]
    fn a_multi_valued_attribute_is_joined() {
        let item =
            crate::json::parse(br#"{"00080104":{"Value":["Thorax","Chest"]}}"#).expect("parses");
        let mut out = String::new();
        write_attributes(&mut out, &item, "");
        assert!(out.contains("Thorax\\Chest"), "{out}");
    }

    #[test]
    fn an_attribute_without_a_value_array_is_skipped() {
        let item = crate::json::parse(br#"{"00081030":{"vr":"LO"}}"#).expect("parses");
        let mut out = String::new();
        write_attributes(&mut out, &item, "");
        assert!(out.is_empty(), "{out}");
    }

    #[test]
    fn the_search_answer_is_read_as_its_first_item() {
        // The SearchForStudies transaction answers a list, so the one study
        // an inspection wants is the first element.
        let list = crate::json::parse(
            br#"[{"0020000D":{"vr":"UI","Value":["9.9.9"]}},
                 {"0020000D":{"vr":"UI","Value":["8.8.8"]}}]"#,
        )
        .expect("parses");
        let first = first_item(&list).expect("a list has a first item");
        assert_eq!(
            first.attribute(crate::dicomweb::TAG_STUDY_INSTANCE_UID),
            Some("9.9.9")
        );

        // An empty list is not an error: the study is simply not there.
        let none = crate::json::parse(b"[]").expect("parses");
        assert!(first_item(&none).is_none(), "an empty list yields nothing");
    }

    #[test]
    fn a_bare_object_is_still_read_as_one_item() {
        // The retrieve form answers the object itself; both shapes occur, so
        // both are read rather than only the one this build asks for.
        let object =
            crate::json::parse(br#"{"0020000D":{"vr":"UI","Value":["7.7.7"]}}"#).expect("parses");
        let first = first_item(&object).expect("an object is its own first item");
        assert_eq!(
            first.attribute(crate::dicomweb::TAG_STUDY_INSTANCE_UID),
            Some("7.7.7")
        );
    }
}
