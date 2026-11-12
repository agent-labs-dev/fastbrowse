//! What Jev sees when asked about the page beside its notes, and the record a caller asked for.

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::DecisionClient;
use fastbrowse_core::models::Evidence;
use fastbrowse_core::output_schema::OutputModel;
use fastbrowse_core::page::{Capture, Control, Observation, cut_text};
use fastbrowse_core::py::dump::{Dump, include};
use fastbrowse_core::py::json::{Style, dumps, write};
use fastbrowse_core::py::text;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::error::{AgentError, Result};
use crate::memory::Notes;
use crate::retrieval::{
    ScalarField, UnsupportedField, choose_candidate, field_candidates, field_candidates_from_notes, merge_candidates,
};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Extraction {
    pub data: Option<Value>,
    pub evidence: Vec<Evidence>,
    pub problem: Option<String>,
}

fn controls<'a>(controls: impl IntoIterator<Item = &'a Control>, compact: bool) -> Value {
    let records: Vec<Value> = controls
        .into_iter()
        .map(|control| {
            include(control.dump_exclude_none(), &["label", "context", "role", "value", "checked", "selected"])
        })
        .collect();
    if !compact || records.is_empty() {
        return Value::Array(records);
    }
    // Verdicts need every control's values, but repeating the same field names spends input on their encoding.
    // Every row keeps its control's position; null means that control has no value for the named column.
    let mut columns: Vec<&str> = Vec::new();
    for record in &records {
        for key in record.as_object().expect("a control is an object").keys() {
            if !columns.contains(&key.as_str()) {
                columns.push(key);
            }
        }
    }
    let rows: Vec<Vec<&Value>> =
        records.iter().map(|record| columns.iter().map(|key| &record[*key]).collect()).collect();
    let table = json!({"columns": columns, "rows": rows});
    let records = Value::Array(records);
    // A table header costs more than it removes for a short list. Compare the complete HTTP wire encodings.
    if write(&table, Style::HTTPX).len() < write(&records, Style::HTTPX).len() { table } else { records }
}

/// The controls holding a value, a check or a selection: the state page text does not show.
fn stateful(controls: &[Control]) -> Vec<&Control> {
    controls.iter().filter(|c| c.value.is_some() || c.checked.is_some() || c.selected.is_some()).collect()
}

/// `page_state`: the page, its stateful controls and the notes for a Jev verdict within Jev's documented input limit,
/// the notes' answering evidence kept whole. The page text is cut when the rest leaves it no room, and a state that
/// still does not fit is refused rather than sent.
pub fn page_state(
    observation: &Observation,
    notes: &Notes,
    tokens: &TokenBudget,
    questions: &[String],
    context: Option<&Map<String, Value>>,
    dates: &[crate::dates::DateReference],
) -> Result<Value> {
    page_state_with(observation, notes, tokens, questions, context, dates, false)
}

/// Completion verdicts share a control schema; text extraction keeps each field beside its name.
pub fn page_verdict_state(
    observation: &Observation,
    notes: &Notes,
    tokens: &TokenBudget,
    questions: &[String],
    context: Option<&Map<String, Value>>,
    dates: &[crate::dates::DateReference],
) -> Result<Value> {
    page_state_with(observation, notes, tokens, questions, context, dates, true)
}

fn page_state_with(
    observation: &Observation,
    notes: &Notes,
    tokens: &TokenBudget,
    questions: &[String],
    context: Option<&Map<String, Value>>,
    dates: &[crate::dates::DateReference],
    compact: bool,
) -> Result<Value> {
    let mut state: Map<String, Value> = context.cloned().unwrap_or_default();
    // The dropdown choice runs apart from the action choice; it also needs today's date for relative months.
    state.insert("date".into(), crate::dates::dated(observation.captured_at, dates).into());
    state.insert(
        "page".into(),
        json!({"url": observation.url, "title": observation.title, "text": observation.viewport_text}),
    );
    if !observation.media.is_empty() {
        state["page"]["media"] = json!(observation.media);
    }
    // Inputs and ARIA selection states are absent from innerText; without them a preview can pass completion even
    // though the requested filters were never applied.
    state.insert("controls".into(), controls(&observation.controls, compact));
    state.insert("notes".into(), "".into());
    let budget = tokens.input_chars(questions);
    let size = |state: &Map<String, Value>| text::len(&dumps(&Value::Object(state.clone()))) as i64;
    let room = |state: &Map<String, Value>| (budget - size(state)).max(0);
    let mut cut_page = false;
    match notes.render(room(&state), true, true) {
        Ok(rendered) => {
            state.insert("notes".into(), rendered.into());
        }
        Err(AgentError::NotesTooLarge(_)) => {
            // Preserve the answer's evidence by cutting page text, then stateless controls, before refusing a verdict.
            cut_page = true;
            state["page"]["text"] = "".into();
            match notes.render(room(&state), true, true) {
                Ok(rendered) => {
                    state.insert("notes".into(), rendered.into());
                }
                Err(AgentError::NotesTooLarge(_)) => {
                    let kept = stateful(&observation.controls);
                    state.insert("controls_omitted".into(), (observation.controls.len() - kept.len()).into());
                    state.insert("controls".into(), controls(kept, compact));
                    let rendered = notes.render(room(&state), true, true)?;
                    state.insert("notes".into(), rendered.into());
                }
                Err(other) => return Err(other),
            }
        }
        Err(other) => return Err(other),
    }
    if cut_page || size(&state) > budget {
        state["page"]["text"] = "".into();
        state["page"]["text"] = cut_text(&observation.viewport_text, room(&state) as usize, true).into();
    }
    // A long article's links alone can outgrow the budget once its text is gone: they say nothing a verdict needs,
    // where inputs and selections do.
    if size(&state) > budget && !state.contains_key("controls_omitted") {
        let kept = stateful(&observation.controls);
        state.insert("controls_omitted".into(), (observation.controls.len() - kept.len()).into());
        state.insert("controls".into(), controls(kept, compact));
    }
    if size(&state) > budget {
        return Err(AgentError::ObservationTooLarge(format!(
            "the state on {} exceeds Jev's input limits with no page text",
            observation.url
        )));
    }
    Ok(Value::Object(state))
}

/// The record a caller asked for, as `extract` reads it: each field by name as retrieval copies it, and the model
/// whose validation the extracted values must pass.
#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub fields: IndexMap<String, ScalarField>,
    pub model: OutputModel,
}

impl RecordSchema {
    /// The record an output schema describes, its fields under their Python names.
    pub fn from_output(model: OutputModel) -> RecordSchema {
        let fields = model.fields().iter().map(|field| (field.name.clone(), ScalarField::from_output(field))).collect();
        RecordSchema { fields, model }
    }
}

/// `extract`'s keyword arguments.
#[derive(Default)]
pub struct ExtractOptions<'a> {
    pub notes: Option<&'a Notes>,
    pub tokens: TokenBudget,
    pub ledger: Option<&'a Ledger>,
}

/// Every field is copied from the spans Jev points at. A field with no supported value fails the extraction.
pub async fn extract(
    jev: &dyn DecisionClient,
    task: &str,
    capture: &Capture,
    schema: &RecordSchema,
    options: &ExtractOptions<'_>,
) -> Result<Extraction> {
    let mut values: Map<String, Value> = Map::new();
    let mut evidence: Vec<Evidence> = Vec::new();
    let record_fields: Vec<String> = schema.fields.keys().cloned().collect();
    for (name, field) in &schema.fields {
        let mut candidates = match field_candidates(capture, field) {
            Ok(candidates) => candidates,
            Err(UnsupportedField { reason }) => {
                return Ok(Extraction { data: None, evidence: Vec::new(), problem: Some(format!("{name}: {reason}")) });
            }
        };
        if let Some(notes) = options.notes {
            // The notes hold every page the run read; the final capture holds only the one it ended on, which for a
            // sorted comparison is one side of it. Both pages are offered, the current one first.
            match field_candidates_from_notes(notes, field, Some(capture)) {
                Ok(quoted) => candidates = merge_candidates(&[&candidates, &quoted]),
                Err(UnsupportedField { reason }) => {
                    return Ok(Extraction {
                        data: None,
                        evidence: Vec::new(),
                        problem: Some(format!("{name}: {reason}")),
                    });
                }
            }
        }
        if candidates.is_empty() {
            continue;
        }
        let state = json!({"task": task, "page": {"url": capture.url, "title": capture.title}});
        match choose_candidate(jev, &state, field, &candidates, name, task, &record_fields, options.ledger).await {
            Ok(Some((value, quoted))) => {
                values.insert(name.clone(), value.to_json());
                evidence.push(quoted);
            }
            Ok(None) => {}
            Err(AgentError::Invalid(error)) => {
                return Ok(Extraction { data: None, evidence, problem: Some(format!("{name}: {error}")) });
            }
            Err(other) => return Err(other),
        }
    }
    // A default the page never showed is the caller's placeholder, not data; only None may stand for absent.
    let unsupported: Vec<&String> = schema
        .fields
        .iter()
        .filter(|(name, field)| !values.contains_key(*name) && !field.required && !field.default.is_null())
        .map(|(name, _)| name)
        .collect();
    if !unsupported.is_empty() {
        let names: Vec<&str> = unsupported.iter().map(|n| n.as_str()).collect();
        return Ok(Extraction {
            data: None,
            evidence,
            problem: Some(format!("no value on the page for {}", names.join(", "))),
        });
    }
    match schema.model.validate(&Value::Object(values)) {
        Ok(data) => Ok(Extraction { data: Some(data), evidence, problem: None }),
        Err(error) => Ok(Extraction { data: None, evidence, problem: Some(error.to_string()) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Observation {
        serde_json::from_value(json!({
            "url": "https://example.test/encode/httpx", "title": "encode/httpx", "page_key": "k",
            "captured_at": "2026-01-01T00:00:00Z", "controls": [],
            "viewport_text": "encode/httpx", "tabs": [],
        }))
        .expect("observation")
    }

    #[test]
    fn control_rows_keep_false_empty_values_and_duplicate_labels() {
        let controls = vec![
            Control {
                checked: Some(false),
                context: Some("First".into()),
                ..Control::new("a", "checkbox", "Choice", [])
            },
            Control {
                checked: Some(true),
                context: Some("Second".into()),
                ..Control::new("b", "checkbox", "Choice", [])
            },
            Control { value: Some(String::new()), ..Control::new("c", "textbox", "Name", []) },
        ];
        let table = super::controls(&controls, true);
        let columns = table["columns"].as_array().unwrap();
        let restored: Vec<Value> = table["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                Value::Object(
                    columns
                        .iter()
                        .zip(row.as_array().unwrap())
                        .filter(|(_, value)| !value.is_null())
                        .map(|(key, value)| (key.as_str().unwrap().to_string(), value.clone()))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            restored,
            controls
                .iter()
                .map(|control| {
                    include(control.dump_exclude_none(), &["label", "context", "role", "value", "checked", "selected"])
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn extraction_keeps_named_control_fields_and_short_verdicts_keep_objects() {
        let mut page = page();
        page.controls = (0..4)
            .map(|id| Control {
                value: Some(format!("value {id}")),
                ..Control::new(&id.to_string(), "textbox", &format!("Field {id}"), [])
            })
            .collect();
        let original = page_state(&page, &Notes::default(), &TokenBudget::default(), &[], None, &[]).unwrap();
        let verdict = page_verdict_state(&page, &Notes::default(), &TokenBudget::default(), &[], None, &[]).unwrap();
        assert!(original["controls"].is_array());
        assert!(verdict["controls"]["rows"].is_array());
        let single = &page.controls[..1];
        assert_eq!(super::controls(single, true), super::controls(single, false));
    }

    #[test]
    fn the_verifier_sees_each_players_state() {
        let mut page = page();
        page.media = vec![
            serde_json::from_value(json!({"kind": "video", "paused": true, "ended": true,
            "muted": false, "elapsed_seconds": 30.0, "duration_seconds": 30.0}))
            .unwrap(),
        ];
        let state = page_state(&page, &Notes::default(), &TokenBudget::default(), &[], None, &[]).unwrap();
        assert_eq!(state["page"]["media"][0]["ended"], true);
    }

    #[test]
    fn a_long_articles_links_give_way_before_its_verdict_is_refused() {
        let mut page = page();
        page.controls = (0..400)
            .map(|n| {
                fastbrowse_core::page::Control::new(&format!("a:{n}"), "link", &format!("Article link number {n}"), [])
            })
            .collect();
        let tokens =
            TokenBudget { state_plus_largest_question: 2000, state_plus_all_questions: 4000, ..TokenBudget::default() };
        let state = page_state(&page, &Notes::default(), &tokens, &["q".to_string()], None, &[]).unwrap();
        assert_eq!(state["controls_omitted"], 400);
        page.controls.extend((0..3).map(|n| Control {
            value: Some(format!("value {n}")),
            ..Control::new(&format!("field:{n}"), "textbox", &format!("Field {n}"), [])
        }));
        let verdict = page_verdict_state(&page, &Notes::default(), &tokens, &["q".to_string()], None, &[]).unwrap();
        assert_eq!(verdict["controls_omitted"], 400);
        assert_eq!(verdict["controls"]["rows"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_state_that_cannot_fit_is_refused_rather_than_sent() {
        let context = Map::from_iter([("task".to_string(), Value::from("word ".repeat(2000)))]);
        let tokens =
            TokenBudget { state_plus_largest_question: 2000, state_plus_all_questions: 4000, ..TokenBudget::default() };
        let error = page_state(&page(), &Notes::default(), &tokens, &["q".to_string()], Some(&context), &[]);
        assert!(matches!(error, Err(AgentError::ObservationTooLarge(_))), "{error:?}");
    }
}
