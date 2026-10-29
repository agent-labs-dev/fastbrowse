//! Port of `src/fastbrowse/retrieval.py`: reading and extraction grounded in immutable capture spans.
//!
//! Chunk budgets are soft only for an indivisible block or row plus its table header. Large Markdown tables split on
//! row boundaries, keeping the original source id. Scalar extraction accepts a field whose annotation it can copy by
//! span; any other returns [`UnsupportedField`] so callers can choose another strategy.
//!
//! Python's one module is split here by what each part reads: `chunks` cuts a capture into prompts and spans,
//! `choices` is Jev's short-fact read, `records` Jev's record reader, `reading` the LLM reader, `fields` scalar
//! extraction and `answers` the composer and the claim checks. Everything public is re-exported here under its Python name.

mod answers;
mod choices;
mod chunks;
mod fields;
mod reading;
mod records;
pub(crate) mod text;

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::llm::Message;
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::schema::{Ty, json_schema};

pub use answers::{
    AnswerCorrection, Claim, ComposeOptions, ComposedAnswer, assemble_answer, compose, draft_answer, partial_answer,
    retain_fields,
};
pub use choices::read_candidates;
pub use chunks::{Chunk, chunk, page_quotes, read_message_for};
pub use fields::{
    Annotation, Candidate, Proposed, ScalarField, ScalarValue, UnsupportedField, choose_candidate, copy_field,
    field_candidate_groups, field_candidates, field_candidates_from_notes, field_group_question, field_question,
    merge_candidates, propose_text_fields, propose_text_fields_from_notes,
};
pub use reading::{
    ReadOptions, ReadOutcome, TallyReader, counted_collections_for, names_subject, read, read_tallies, subject_words,
};
pub use records::filter_tallies;

/// `_route`: records which reader answers a read and why, so a run's trace shows where its LLM reads come from.
pub(crate) fn route(
    trace: &fastbrowse_core::telemetry::Trace,
    reader: &str,
    reason: &str,
    requirement: Option<&str>,
    fields: Vec<(&str, serde_json::Value)>,
) {
    trace.event("read_route", || {
        let mut record = serde_json::Map::new();
        record.insert("reader".into(), reader.into());
        record.insert("reason".into(), reason.into());
        record.insert("requirement".into(), requirement.into());
        record.extend(fields.into_iter().map(|(key, value)| (key.to_string(), value)));
        record
    });
}

/// `_notes_room`: reserve room for the prompt and response schema so notes cannot exceed the input budget.
pub(crate) fn notes_room(tokens: &TokenBudget, messages: &[Message], response: &Ty) -> i64 {
    let prompt: String = messages.iter().map(|message| message.content.as_str()).collect();
    tokens.remaining_chars::<&str>(&format!("{prompt}{}", dumps(&json_schema(response))), &[], false)
}

/// The response models the readers and the composer ask for, by their Python names, for tests that hold them to
/// pydantic's validation. `_FieldAnswerDraft` is the one `tests/golden/validation.json` records, with fields 0 and 2.
#[doc(hidden)]
pub fn response_schema(name: &str) -> Option<fastbrowse_core::validation::ModelSchema> {
    use fastbrowse_core::schema::Schema;
    let ty = match name {
        "_ReadResponse" => reading::read_response_ty(false),
        "_CollectionResponse" => reading::read_response_ty(true),
        "_RecordsResponse" => reading::RecordsResponse::ty(),
        "_TextProposals" => fields::TextProposals::ty(),
        "_AnswerDraft" => answers::AnswerDraft::ty(),
        "_FieldAnswerDraft" => answers::field_answer_draft(&[
            (0, "The price of the first item".into()),
            (2, "The name of the seller".into()),
        ]),
        _ => return None,
    };
    Some(fastbrowse_core::validation::ModelSchema { ty })
}

#[cfg(test)]
pub(crate) mod testing {
    use fastbrowse_core::page::{Block, BlockKind, Capture};
    use fastbrowse_core::py::datetime::DateTime;
    use fastbrowse_core::py::{sha256_hex, text};

    /// tests/test_retrieval.py's `capture`: the parts joined by blank lines, one block each.
    pub fn capture(parts: &[(BlockKind, &str)]) -> Capture {
        let joined = parts.iter().map(|(_, part)| *part).collect::<Vec<_>>().join("\n\n");
        let mut blocks = Vec::new();
        let mut start = 0i64;
        for (index, (kind, part)) in parts.iter().enumerate() {
            let length = text::len(part) as i64;
            blocks.push(Block {
                source_id: format!("s{index}"),
                source_url: None,
                complete_source: false,
                kind: *kind,
                frame_id: None,
                start,
                end: start + length,
                heading_path: Vec::new(),
                control_context: None,
                href: None,
                list_id: None,
                list_count: None,
                list_frameless: false,
            });
            start += length + 2;
        }
        Capture {
            url: "https://example.test".into(),
            title: "Example".into(),
            captured_at: DateTime::new(2026, 1, 1, 0, 0, 0).utc(),
            sha256: sha256_hex(joined.as_bytes()),
            text: joined,
            blocks,
            inaccessible_frames: 0,
        }
    }
}
