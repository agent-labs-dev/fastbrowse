//! Port of `src/fastbrowse/retrieval.py`: reading and extraction grounded in immutable capture spans.
//!
//! Chunk budgets are soft only for an indivisible block or row plus its table header. Large Markdown tables split on
//! row boundaries, keeping the original source id. Scalar extraction accepts a field whose annotation it can copy by
//! span; any other returns [`UnsupportedField`] so callers can choose another strategy.
//!
//! Python's one module is split here by what each part reads: `chunks` cuts a capture into prompts and spans,
//! `choices` is Jev's short-fact read, `reading` the LLM reader, `fields` scalar extraction and `answers` the composer
//! and the claim checks. Everything public is re-exported here under its Python name.

mod answers;
mod choices;
mod chunks;
mod fields;
mod reading;
pub(crate) mod text;

use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::llm::Message;
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::schema::{Ty, json_schema};

pub use answers::{
    AnswerCorrection, Claim, ComposeOptions, ComposedAnswer, Stage, TRANSACTION_CONTRADICTED, assemble_answer,
    claim_check_questions, compose, draft_answer, partial_answer, retain_fields, transaction_check_question,
};
pub use choices::read_candidates;
pub use chunks::{Chunk, chunk, leading_quotes, read_message_for};
pub use fields::{
    Annotation, Candidate, Proposed, ScalarField, ScalarValue, UnsupportedField, choose_candidate, copy_field,
    field_candidate_groups, field_candidates, field_candidates_from_notes, field_group_question, field_question,
    merge_candidates, propose_text_fields, propose_text_fields_from_notes,
};
pub use reading::{
    ReadOptions, ReadOutcome, TallyReader, counted_collections_for, names_subject, read, read_tallies, subject_words,
};

/// `_READ_CHUNK_CHARS`, the default chunk for `propose_text_fields`.
pub const READ_CHUNK_CHARS: i64 = reading::READ_CHUNK_CHARS;

/// `_notes_room`: reserve room for the prompt and response schema so notes cannot exceed the input budget.
pub(crate) fn notes_room(tokens: &TokenBudget, messages: &[Message], response: &Ty) -> i64 {
    let prompt: String = messages.iter().map(|message| message.content.as_str()).collect();
    tokens.remaining_chars::<&str>(&format!("{prompt}{}", dumps(&json_schema(response))), &[], false)
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
