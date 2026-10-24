//! tests/browser/test_forms.py, test_form_evidence.py, test_deceptive_form.py and test_hidden_upload.py.
//!
//! A parametrized Python test is one Rust test that runs each case on a fresh session, in pytest's order.

mod common;
mod dispatch_support;

use std::time::Duration;

use common::{Harness, harness, harness_on, sites};
use dispatch_support::{action, eval_active, eval_value, fill, find, observe_until};
use fastbrowse_browser::session::SessionOptions;
use fastbrowse_browser::shared::{Action, Attachment, BrowserConnection, Config, Operation, StepOutcome};
use fastbrowse_browser::testing;
use serde_json::json;

fn form_fill(target: &fastbrowse_browser::shared::Control, text: &str) -> Action {
    Action { form_fill: true, ..fill(target, text) }
}

// test_forms.py: a batch continues only while the original form retains exactly the requested edits.

#[tokio::test(flavor = "multi_thread")]
async fn test_validation_labels_preserve_field_semantics() {
    for kind in ["email", "tel"] {
        let Some(h) = harness().await else { return };
        h.goto("/dispatch.html").await;
        let html = format!(
            "<form><input type={kind} name=contact_{kind} autocomplete={kind} aria-label=\"Invalid contact\"></form>"
        );
        eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(&html).unwrap())).await;
        let observation = h.observe().await;
        let control = find(&observation, "Invalid contact");
        assert_eq!(control.input_name.as_deref(), Some(format!("contact_{kind}").as_str()));
        assert_eq!(control.autocomplete.as_deref(), Some(kind));
        h.close().await;
    }
}

/// test_forms.py's `form`: a two-field form, then `script`.
async fn form(h: &Harness, script: &str) {
    h.goto("/dispatch.html").await;
    let html = "<form><label>Name<input id=name required></label><label>Email<input id=email></label></form>";
    eval_active(&h.page, &format!("document.body.innerHTML = {}; {script}", serde_json::to_string(html).unwrap()))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_two_fills_use_one_observation() {
    let Some(h) = harness().await else { return };
    form(&h, "").await;
    let before = h.observe().await;
    let (name, email) = (find(&before, "Name"), find(&before, "Email"));
    assert!(name.form_id == email.form_id && name.form_id.is_some());
    for (target, text) in [(name, "Ada"), (email, "ada@example.com")] {
        let result = h.page.act(&form_fill(target, text), &before).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Executed);
        assert!(result.form_unchanged);
    }
    let after = h.observe().await;
    assert_eq!(find(&after, "Name").value.as_deref(), Some("Ada"));
    assert_eq!(find(&after, "Email").value.as_deref(), Some("ada@example.com"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dependent_change_stops_batch() {
    for change in [
        "document.querySelector('form').insertAdjacentHTML('beforeend', '<input aria-label=Extra>')",
        "document.getElementById('email').disabled = true",
        "document.getElementById('email').value = 'dependency'",
        "document.getElementById('email').setAttribute('aria-invalid', 'true')",
        "document.getElementById('name').value = ''",
        "document.getElementById('email').outerHTML = '<input id=email aria-label=Email>'",
        "document.querySelector('form').action = '/delete'",
        "location.hash = 'changed'",
    ] {
        let Some(h) = harness().await else { return };
        form(&h, &format!("document.getElementById('name').oninput = () => {{ {change}; }}")).await;
        let before = h.observe().await;
        let result = h.page.act(&form_fill(find(&before, "Name"), "Ada"), &before).await.expect("act");
        assert!(!result.form_unchanged, "{change}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_change_between_fills_stops_before_typing() {
    let Some(h) = harness().await else { return };
    form(&h, "").await;
    let before = h.observe().await;
    let result = h.page.act(&form_fill(find(&before, "Name"), "Ada"), &before).await.expect("act");
    assert!(result.form_unchanged);
    eval_active(&h.page, "document.querySelector('form').insertAdjacentHTML('beforeend', '<input aria-label=Extra>')")
        .await;
    let result = h.page.act(&form_fill(find(&before, "Email"), "ada@example.com"), &before).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Stale);
    assert_eq!(find(&h.observe().await, "Email").value.as_deref(), Some(""));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dialog_stops_form_fill() {
    let Some(h) = harness().await else { return };
    form(&h, "document.getElementById('name').oninput = () => alert('Pause')").await;
    let before = h.observe().await;
    let result =
        tokio::time::timeout(Duration::from_secs(5), h.page.act(&form_fill(find(&before, "Name"), "Ada"), &before))
            .await
            .expect("fill returns")
            .expect("act");
    assert!(!result.form_unchanged);
    assert!(h.session.pending_dialog().is_some());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigation_during_input_stops_batch() {
    let Some(h) = harness().await else { return };
    form(&h, "document.getElementById('name').oninput = () => { location.href = '/safety.html'; }").await;
    let before = h.observe().await;
    let result =
        tokio::time::timeout(Duration::from_secs(5), h.page.act(&form_fill(find(&before, "Name"), "Ada"), &before))
            .await
            .expect("fill returns")
            .expect("act");
    assert!(!result.form_unchanged);
    let target = (h.session.active_session_id().expect("active session"), "main".to_owned(), 1, None);
    assert!(!testing::dispatch::form_unchanged(&h.page, target, "Ada").await);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_input_can_announce_suggestions_after_typing() {
    let Some(h) = harness().await else { return };
    form(
        &h,
        "document.getElementById('name').oninput = event => { \
         event.target.setAttribute('aria-controls', 'suggestions'); \
         document.body.insertAdjacentHTML('beforeend', '<ul id=suggestions role=listbox></ul>'); \
         setTimeout(() => { document.getElementById('suggestions').innerHTML = \
         '<li role=option>Suggested person</li>'; }, 500); };",
    )
    .await;
    let before = h.observe().await;
    let result = h.page.act(&form_fill(find(&before, "Name"), "Ada"), &before).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert!(!result.form_unchanged);
    find(&h.observe().await, "Suggested person");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_local_field_groups_do_not_include_a_sidebar_search() {
    let Some(h) = harness().await else { return };
    form(&h, "").await;
    let html = "<aside><label>Search<input type=search></label></aside>\
                <main><div><label>Name<input></label><label>Email<input></label></div>\
                <div><label>Other<input></label></div></main>";
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(html).unwrap())).await;
    let before = h.observe().await;
    assert_eq!(find(&before, "Name").form_id, find(&before, "Email").form_id);
    assert!(find(&before, "Name").form_id.is_some());
    assert_eq!(find(&before, "Search").form_id, None);
    assert_eq!(find(&before, "Other").form_id, None);
    h.close().await;
}

// test_form_evidence.py

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_preserves_empty_fields_disabled_actions_and_layout() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(&h.page, "document.body.innerHTML = '<label>Name<input></label><button disabled>Create</button>'")
        .await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Name: [empty]"), "{}", capture.text);
    assert!(capture.text.contains("Create: disabled"), "{}", capture.text);
    assert!(capture.text.contains("Horizontal overflow: no"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_evidences_absent_visible_headings() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(&h.page, "document.body.innerHTML = '<h1 hidden>Hidden</h1><p>Welcome</p>'").await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Visible h1 headings: 0"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_heading_absence_does_not_ignore_shadow_roots() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = '<div id=host></div>'; \
         document.querySelector('#host').attachShadow({mode:'open'}).innerHTML = '<h1>Visible title</h1>'",
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Visible title"), "{}", capture.text);
    assert!(!capture.text.contains("Visible h1 headings: 0"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_empty_shadow_scope_does_not_contradict_document_heading() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = '<h1>Visible title</h1><div id=host></div>'; \
         document.querySelector('#host').attachShadow({mode:'open'}).innerHTML = '<p>Other content</p>'",
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    assert!(!capture.text.contains("Visible h1 headings: 0"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_heading_absence_includes_cross_origin_frame_documents() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    observe_until(&h.page, "Frame button").await;
    eval_active(&h.page, "document.querySelectorAll('h1').forEach(e => e.remove())").await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Inside the frame"), "{}", capture.text);
    assert!(!capture.text.contains("Visible h1 headings: 0"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_inaccessible_frames_prevent_document_wide_absence_claims() {
    // test_safety.py's `scoped`: the run may control the main site only, so its cross-origin frame is withheld.
    let Some(chrome) = common::chrome() else { return };
    let connection = BrowserConnection { allowed_origins: Some(vec![sites().main.clone()]), ..chrome.clone() };
    let h = harness_on(connection, Config::default(), SessionOptions::default()).await.expect("harness");
    h.goto("").await;
    let session_id = h.session.active_session_id().expect("active session");
    eval_value(&h.session, &session_id, "document.querySelectorAll('h1').forEach(e => e.remove())").await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.inaccessible_frames > 0);
    assert!(!capture.text.contains("Visible h1 headings: 0"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_empty_frame_documents_emit_one_absence_observation() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    observe_until(&h.page, "Frame button").await;
    let mut frame_sessions = vec![h.session.active_session_id().expect("active session")];
    frame_sessions.extend(h.session.frame_sessions().into_iter().map(|(_, session)| session));
    for frame_session in &frame_sessions {
        eval_value(&h.session, frame_session, "document.querySelectorAll('h1').forEach(e => e.remove())").await;
    }
    let capture = h.page.capture().await.expect("capture");
    assert_eq!(capture.inaccessible_frames, 0);
    assert_eq!(capture.text.matches("Visible h1 headings: 0").count(), 1, "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_control_observations_are_bounded() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "
        document.body.innerHTML = Array.from({length: 300}, (_, i) =>
          `<label>Field ${i}<input></label><button disabled>Action ${i}</button>`).join('');
        ",
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    // Block offsets count code points, as Python indexes a str.
    let chars: Vec<char> = capture.text.chars().collect();
    let observations = capture
        .blocks
        .iter()
        .filter(|b| {
            let text: String = chars[b.start..b.end].iter().collect();
            text.contains("[empty]") || text.contains(": disabled")
        })
        .count();
    assert_eq!(observations, 64);
    h.close().await;
}

// test_deceptive_form.py: test_a_harmless_caption_does_not_hide_a_destructive_form is not ported. It runs
// `Agent.run` with a scripted Jev and LLM to show the agent asks for confirmation; the agent loop and policy are not
// in this crate.

// test_hidden_upload.py: a styled upload label must retain the file input it submits.

#[tokio::test(flavor = "multi_thread")]
async fn test_visible_label_uploads_to_hidden_file_input() {
    for mutation in [None, Some("disabled"), Some("retargeted")] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        eval_active(
            &h.page,
            "
            document.body.innerHTML = '<input id=\"file\" type=\"file\" style=\"display:none\">' +
                '<label for=\"file\">Upload or Drag Image</label>';
        ",
        )
        .await;
        let observation = h.observe().await;
        let control =
            observation.controls.iter().find(|c| c.label == "Upload or Drag Image").expect("the upload label");
        assert_eq!(control.operations, vec![Operation::Upload], "{mutation:?}");
        if let Some(mutation) = mutation {
            let expression = if mutation == "disabled" {
                "document.getElementById('file').disabled = true"
            } else {
                "document.body.insertAdjacentHTML('beforeend', '<input id=other type=file style=display:none>'); \
                 document.querySelector('label').htmlFor = 'other'"
            };
            eval_active(&h.page, expression).await;
        }
        let upload = Action {
            files: vec![Attachment {
                name: "image.png".into(),
                mime_type: "image/png".into(),
                content: b"provided bytes".to_vec(),
            }],
            ..action(Operation::Upload, control)
        };
        let result = h.page.act(&upload, &observation).await.expect("act");
        if mutation.is_some() {
            assert_eq!(result.outcome, StepOutcome::Stale, "{mutation:?}: {:?}", result.detail);
            assert_eq!(
                eval_active(&h.page, "[...document.querySelectorAll('input')].every(e => !e.files.length)").await,
                json!(true),
                "{mutation:?}"
            );
            h.close().await;
            continue;
        }
        assert_eq!(result.outcome, StepOutcome::Executed, "{:?}", result.detail);
        let out = h
            .session
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": "document.getElementById('file').files[0].text()",
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
                Some(&h.session.active_session_id().expect("active session")),
            )
            .await
            .expect("Runtime.evaluate");
        assert_eq!(out["result"]["value"], json!("provided bytes"));
        h.close().await;
    }
}
