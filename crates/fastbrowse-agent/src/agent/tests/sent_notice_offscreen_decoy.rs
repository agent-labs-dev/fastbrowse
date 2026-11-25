//! A capture keeps document order while observations put viewport text first.

use super::*;

#[test]
fn an_offscreen_example_above_the_viewport_does_not_take_the_sent_reply() {
    let before = Observation {
        viewport_text: "Save status:\nNot saved".into(),
        offscreen_text: "Help example:\nChanges saved successfully".into(),
        ..buttons_at(FILES, &["Save"])
    };
    let after = Observation { viewport_text: "Save status:\nChanges saved successfully".into(), ..before.clone() };
    let page = capture(&[
        (BlockKind::Heading, "Help example:"),
        (BlockKind::Paragraph, "Changes saved successfully (title: This is an example confirmation)"),
        (BlockKind::Heading, "Save status:"),
        (BlockKind::Paragraph, "Changes saved successfully (title: Save status)"),
    ]);
    let mut state = run_state();
    state.acted_from = Some(before);
    state.keeping = true;
    state.history.push(executed(Some(Operation::Click), Some("Save"), true));
    agent().note_effect(&mut state, &after);
    let shown = std::mem::take(&mut state.sent_text);
    assert!(shown.iter().any(|piece| piece.added), "the Save produced a reply");
    state.record_sent_notice(&page, &shown);
    let quotes: Vec<_> = state.notes.evidence().values().map(|evidence| evidence.quote.clone()).collect();
    assert_eq!(quotes, ["Changes saved successfully (title: Save status)"]);
}
