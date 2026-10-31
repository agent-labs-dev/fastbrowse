//! Reading and extraction grounded in immutable capture spans.
//!
//! Scalar extraction accepts a field whose annotation it can copy by span; any other returns [`UnsupportedField`].
//! The parts split by what each reads: `chunks` cuts spans from a capture, `choices` is Jev's read, `fields` scalar
//! extraction and `answers` the answer built from what Jev read.

mod answers;
mod choices;
mod chunks;
mod fields;
pub(crate) mod text;

pub use answers::{Claim, ComposedAnswer, assemble_answer, compose, draft_answer, partial_answer, verdict};
pub use choices::read_candidates;
pub(crate) use choices::{PageRead, read_page};
pub use chunks::page_quotes;
pub use fields::{
    Annotation, Candidate, ScalarField, UnsupportedField, choose_candidate, copy_field, field_candidates,
    field_candidates_from_notes, field_question, merge_candidates,
};

#[cfg(test)]
pub(crate) mod testing {
    use fastbrowse_core::page::{Block, BlockKind, Capture};
    use fastbrowse_core::py::datetime::DateTime;
    use fastbrowse_core::py::{sha256_hex, text};

    /// The parts joined by blank lines, one block each.
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
