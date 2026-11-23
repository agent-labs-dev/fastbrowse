//! A scroll through regularly spaced text must distinguish the newly shown rows.

use fastbrowse_core::effects::state_key;
use fastbrowse_run::desktop::tree::{Element, Window, observation};

fn document(scroll_y: i64) -> Element {
    Element {
        id: "window".into(),
        role: "frame".into(),
        states: vec!["showing".into()],
        width: 400,
        height: 100,
        children: (0..20)
            .map(|row| Element {
                id: format!("line-{row}"),
                role: "label".into(),
                text: Some(format!("Document line {row}")),
                states: vec!["showing".into(), "visible".into()],
                x: 0,
                y: row * 20 - scroll_y,
                width: 400,
                height: 20,
                ..Element::default()
            })
            .collect(),
        ..Element::default()
    }
}

#[test]
fn scrolling_to_unseen_rows_is_a_different_viewport() {
    let window = Window { address: "0xabc".into(), class: "reader".into(), title: "Document".into() };
    let top = observation(&window, &document(0), &[]);
    let below = observation(&window, &document(100), &[]);
    assert!(top.viewport_text.contains("Document line 0"));
    assert!(below.viewport_text.contains("Document line 5"));
    assert_ne!(top.viewport_text, below.viewport_text);
    assert_eq!(state_key(&top), state_key(&below), "scrolling changes no control");
    assert_ne!(
        top.viewport_key, below.viewport_key,
        "new document rows must count as a new viewport even when their layout repeats"
    );
}
