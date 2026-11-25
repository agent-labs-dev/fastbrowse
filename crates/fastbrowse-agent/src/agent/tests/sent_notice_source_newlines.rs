//! An observation collapses a text node's source newlines; the capture can retain them.

use super::*;

#[test]
fn a_sent_notice_with_source_newlines_is_kept_in_the_notes() {
    // snapshot.js normalizes node.textContent with split(/\s+/).join(' '), while capture.js's
    // flush reads a bare text node with clean(), which retains newlines in its textContent.
    for captured in ["Changes saved successfully", "Changes saved\nsuccessfully"] {
        let before = buttons_at(FILES, &["Save"]);
        let after = Observation { viewport_text: "Changes saved successfully".into(), ..before.clone() };
        let page = capture(&[(BlockKind::Paragraph, captured)]);
        let mut state = run_state();
        state.acted_from = Some(before);
        state.keeping = true;
        state.history.push(executed(Some(Operation::Click), Some("Save"), true));
        agent().note_effect(&mut state, &after);
        let shown = std::mem::take(&mut state.sent_text);
        assert!(shown.iter().any(|piece| piece.added), "the Save produced a reply");
        agent().record_sent_notice(&mut state, &page, &shown);
        let quotes: Vec<_> = state.notes.evidence().values().map(|quote| quote.quote.clone()).collect();
        assert_eq!(quotes.join("\n"), captured, "the sent reply must be citable for capture {captured:?}");
    }
}
