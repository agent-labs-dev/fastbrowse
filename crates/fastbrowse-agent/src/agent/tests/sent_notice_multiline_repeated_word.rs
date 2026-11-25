//! A kept heading must not consume the first source line of a newly shown reply.

use super::*;

#[test]
fn a_kept_heading_does_not_consume_a_source_line_of_the_sent_reply() {
    // <p>Changes\nsaved\nsuccessfully</p><h2>Changes</h2> after saving.
    // The observation collapses source whitespace; the capture retains it.
    let before = Observation { viewport_text: "Not saved\nChanges".into(), ..buttons_at(FILES, &["Save"]) };
    let after = Observation { viewport_text: "Changes saved successfully\nChanges".into(), ..before.clone() };
    let page = capture(&[(BlockKind::Paragraph, "Changes\nsaved\nsuccessfully"), (BlockKind::Heading, "Changes")]);
    let agent = agent();
    let mut state = run_state();
    state.acted_from = Some(before);
    state.keeping = true;
    state.history.push(executed(Some(Operation::Click), Some("Save"), true));
    agent.note_effect(&mut state, &after);
    let shown = std::mem::take(&mut state.sent_text);
    assert_eq!(
        shown.iter().filter(|piece| piece.added).map(|piece| piece.text.as_str()).collect::<Vec<_>>(),
        ["Changes saved successfully"]
    );
    agent.record_sent_notice(&mut state, &page, &shown);
    let quotes: Vec<_> = state.notes.evidence().values().map(|quote| quote.quote.clone()).collect();
    assert_eq!(quotes, ["Changes\nsaved\nsuccessfully"], "the sent reply must already be a citable note");
}
