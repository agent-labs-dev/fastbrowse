//! Live status text must not renew scroll progress at a place already shown.

use fastbrowse_core::effects::state_key;
use fastbrowse_run::desktop::tree::{Element, Window, observation};

fn document(scroll_y: i64, clock: &str) -> Element {
    let mut children: Vec<Element> = (0..20)
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
        .collect();
    children.push(Element {
        id: "status-clock".into(),
        role: "label".into(),
        text: Some(clock.into()),
        states: vec!["showing".into(), "visible".into()],
        x: 0,
        y: 100,
        width: 400,
        height: 20,
        ..Element::default()
    });
    Element {
        id: "window".into(),
        role: "frame".into(),
        states: vec!["showing".into()],
        width: 400,
        height: 120,
        children,
        ..Element::default()
    }
}

#[test]
fn returning_to_shown_rows_after_a_clock_tick_keeps_the_viewport_identity() {
    let window = Window { address: "0xabc".into(), class: "reader".into(), title: "Document".into() };
    let top = observation(&window, &document(0, "12:00:00"), &[]);
    let below = observation(&window, &document(90, "12:00:01"), &[]);
    let returned = observation(&window, &document(0, "12:00:02"), &[]);
    assert_ne!(top.viewport_key, below.viewport_key, "unseen rows must still count as progress");
    assert_eq!(state_key(&top), state_key(&returned), "a clock tick changes no control");
    assert_eq!(
        top.viewport_key, returned.viewport_key,
        "returning to shown rows must not renew the scroll stall budget just because a status clock ticked"
    );
}
