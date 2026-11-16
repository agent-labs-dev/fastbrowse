//! Private, deterministic reduction of values within source-bound groups.

use std::collections::HashMap;

use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Extreme {
    Min,
    Max,
}

/// The private selection requested before the public tally operates on the records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Within {
    pub by: String,
    pub op: Extreme,
    pub field: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupRead {
    pub record: String,
    pub text: Option<String>,
    pub heading: Option<i64>,
}

impl Schema for GroupRead {
    fn ty() -> Ty {
        ModelTy::frozen(
            "GroupRead",
            vec![
                Field::required("record", Ty::Str),
                Field::required("text", Ty::optional(Ty::Str)),
                Field::required("heading", Ty::optional(Ty::Int)),
            ],
        )
        .ty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupReads {
    pub records: Vec<GroupRead>,
}

impl Schema for GroupReads {
    fn ty() -> Ty {
        ModelTy::frozen("GroupReads", vec![Field::required("records", Ty::list(GroupRead::ty()))]).ty()
    }
}

/// Validate and return the model's original group text from its claimed source.
pub(super) fn group_text(read: &GroupRead, quote: &str, headings: &[String], allow_heading: bool) -> Option<String> {
    let text = read.text.as_deref()?.to_string();
    if text.trim().is_empty() {
        return None;
    }
    match read.heading {
        None if quote.contains(&text) => Some(text),
        Some(index) if allow_heading => {
            headings.get(usize::try_from(index).ok()?).filter(|heading| heading.contains(&text)).map(|_| text)
        }
        _ => None,
    }
}

impl Schema for Within {
    fn ty() -> Ty {
        ModelTy::frozen(
            "Within",
            vec![
                Field::required("by", Ty::Str),
                Field::required("op", Ty::literals(&["min", "max"])),
                Field::required("field", Ty::Str),
            ],
        )
        .ty()
    }
}

/// Select each group's extreme, retaining ties and preserving the input order.
pub(super) fn select_extremes(records: &[(usize, String, f64)], op: Extreme) -> Vec<usize> {
    let mut extremes: HashMap<&str, f64> = HashMap::new();
    for (_, group, value) in records {
        extremes
            .entry(group.as_str())
            .and_modify(|current| {
                let better = match op {
                    Extreme::Min => value.total_cmp(current).is_lt(),
                    Extreme::Max => value.total_cmp(current).is_gt(),
                };
                if better {
                    *current = *value;
                }
            })
            .or_insert(*value);
    }
    records
        .iter()
        .filter(|(_, group, value)| {
            extremes.get(group.as_str()).is_some_and(|extreme| value.total_cmp(extreme).is_eq())
        })
        .map(|(record, _, _)| *record)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Extreme, GroupRead, group_text, select_extremes};

    #[test]
    fn group_text_requires_the_claimed_source() {
        let headings = vec!["Package".into(), "Releases".into()];
        assert_eq!(
            group_text(
                &GroupRead { record: "r0".into(), text: Some("Package".into()), heading: None },
                "Package 1.0",
                &headings,
                false
            ),
            Some("Package".into())
        );
        assert_eq!(
            group_text(
                &GroupRead { record: "r0".into(), text: Some("Releases".into()), heading: Some(1) },
                "Version 1.0",
                &headings,
                true
            ),
            Some("Releases".into())
        );
        assert_eq!(
            group_text(
                &GroupRead { record: "r0".into(), text: Some("Releases".into()), heading: Some(2) },
                "Version 1.0",
                &headings,
                true
            ),
            None
        );
        assert_eq!(
            group_text(
                &GroupRead { record: "r0".into(), text: Some("Releases".into()), heading: Some(1) },
                "Version 1.0",
                &headings,
                false
            ),
            None
        );
        assert_eq!(
            group_text(
                &GroupRead { record: "r0".into(), text: Some("Package".into()), heading: None },
                "Version 1.0",
                &headings,
                false
            ),
            None
        );
        assert_eq!(
            group_text(
                &GroupRead { record: "r0".into(), text: Some("".into()), heading: None },
                "Package",
                &headings,
                false
            ),
            None
        );
        assert_eq!(
            group_text(&GroupRead { record: "r0".into(), text: None, heading: None }, "Package", &headings, false),
            None
        );
    }

    #[test]
    fn a_negative_heading_is_rejected_without_failing_deserialization() {
        let read: GroupRead = serde_json::from_value(serde_json::json!({
            "record": "r0", "text": "Package", "heading": -1
        }))
        .unwrap();
        assert_eq!(group_text(&read, "Version 1.0", &["Package".into()], true), None);
    }

    #[test]
    fn minimum_of_group_maxima_is_not_global_minimum() {
        let records = vec![(0, "A".into(), 2019.0), (1, "A".into(), 2024.0), (2, "B".into(), 2026.0)];
        assert_eq!(select_extremes(&records, Extreme::Max), [1, 2]);
        let selected = [(1, "outer".into(), 2024.0), (2, "outer".into(), 2026.0)];
        assert_eq!(select_extremes(&selected, Extreme::Min), [1]);
    }

    #[test]
    fn keeps_ties_and_distinct_groups_with_identical_values() {
        let records = vec![(0, "A".into(), 10.0), (1, "A".into(), 10.0), (2, "B".into(), 10.0), (3, "B".into(), 9.0)];
        assert_eq!(select_extremes(&records, Extreme::Max), [0, 1, 2]);
        assert_eq!(select_extremes(&records, Extreme::Min), [0, 1, 3]);
    }
}
