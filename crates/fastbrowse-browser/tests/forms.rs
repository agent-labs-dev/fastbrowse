//! A parametrized Python test is one Rust test that runs each case on a fresh session, in pytest's order.

mod common;
mod dispatch_support;

use std::time::Duration;

use common::{Harness, find, harness, harness_on, observe_until, sites};
use dispatch_support::{action, eval_active, fill};
use fastbrowse_browser::session::SessionOptions;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{Attachment, BrowserConnection, Bytes, Operation, StepOutcome};
use fastbrowse_core::page::Action;
use serde_json::json;

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
async fn test_a_dialog_a_fill_opens_does_not_hang_it() {
    let Some(h) = harness().await else { return };
    form(&h, "document.getElementById('name').oninput = () => alert('Pause')").await;
    let before = h.observe().await;
    tokio::time::timeout(Duration::from_secs(5), h.page.act(&fill(find(&before, "Name"), "Ada"), &before))
        .await
        .expect("fill returns")
        .expect("act");
    assert!(h.session.pending_dialog().is_some());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_dialog_opened_by_leaving_a_field_can_be_answered() {
    let Some(h) = harness().await else { return };
    form(&h, "document.getElementById('name').onchange = () => alert('Check the name')").await;
    let before = h.observe().await;
    h.page.act(&fill(find(&before, "Name"), "Ada"), &before).await.unwrap();
    let typed = h.observe().await;
    let returned = tokio::time::timeout(
        Duration::from_secs(5),
        h.page.act(&action(Operation::Blur, find(&typed, "Name")), &typed),
    )
    .await;
    let pending = h.session.pending_dialog().is_some();
    if pending {
        h.page.act(&Action { accept_dialog: Some(true), ..Action::new(Operation::Dialog) }, &typed).await.unwrap();
    }
    let observed = h.observe().await;
    h.close().await;
    assert!(returned.is_ok(), "blur must return while its validation dialog is pending");
    assert!(pending, "the validation dialog remains for the next decision");
    assert_eq!(find(&observed, "Name").value.as_deref(), Some("Ada"));
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
    let result = h.page.act(&fill(find(&before, "Name"), "Ada"), &before).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed);
    find(&h.observe().await, "Suggested person");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_button_carries_the_form_it_submits() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    let html = "<form><label>Name<input></label><button>Send</button></form><button>Elsewhere</button>";
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(html).unwrap())).await;
    let before = h.observe().await;
    assert!(find(&before, "Send").form_id.is_some());
    assert_eq!(find(&before, "Send").form_id, find(&before, "Name").form_id);
    assert_eq!(find(&before, "Elsewhere").form_id, None);
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
async fn test_capture_says_which_choice_is_checked() {
    // #897: a checkout's order summary re-rendered in place for each shipping method, and no read could say which
    // method its totals were for, so the run took one method's totals as covering all five.
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = `
          <label><input type=radio name=ship style='opacity:0;position:absolute'>DHL Express $28.40</label>
          <label><input type=radio name=ship checked style='opacity:0;position:absolute'>UPS Saver $32.61</label>
          <input type=radio name=ship id=fedex hidden><label for=fedex>FedEx Priority $26.95</label>
          <label><input type=checkbox checked>Gift wrap</label>
          <label><input type=checkbox>Insurance</label>
          <div role=radio aria-checked=true tabindex=0>Pickup</div>
          <p>Grand Total $174.69</p>`",
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    for checked in ["UPS Saver $32.61: checked", "Gift wrap: checked", "Pickup: checked"] {
        assert!(capture.text.contains(checked), "{checked:?} in {}", capture.text);
    }
    for unchecked in ["DHL Express $28.40: checked", "Insurance: checked", "FedEx Priority $26.95: checked"] {
        assert!(!capture.text.contains(unchecked), "{unchecked:?} in {}", capture.text);
    }
    // A hidden choice reached through its label still says it is the one chosen.
    eval_active(&h.page, "document.querySelector('#fedex').checked = true").await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("FedEx Priority $26.95: checked"), "{}", capture.text);
    assert!(!capture.text.contains("UPS Saver $32.61: checked"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_reads_shadow_roots() {
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
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_reads_cross_origin_frame_documents() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    observe_until(&h.page, "Frame button").await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Inside the frame"), "{}", capture.text);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_counts_frames_it_may_not_read() {
    // test_safety.py's `scoped`: the run may control the main site only, so its cross-origin frame is withheld.
    let Some(chrome) = common::chrome() else { return };
    let connection = BrowserConnection { allowed_origins: Some(vec![sites().main.clone()]), ..chrome.clone() };
    let h = harness_on(connection, Config::default(), SessionOptions::default()).await.expect("harness");
    h.goto("").await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.inaccessible_frames > 0);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_every_empty_or_disabled_control_is_observed() {
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
    let observations = capture
        .blocks
        .iter()
        .filter(|b| {
            let text = capture.block_text(b);
            text.contains("[empty]") || text.contains(": disabled")
        })
        .count();
    assert_eq!(observations, 600);
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
        assert_eq!(control.operations, [Operation::Upload].into(), "{mutation:?}");
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
                content: Bytes(b"provided bytes".to_vec()),
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

/// A Reference field whose first change replaces what was typed, and a Review button that submits nothing. Counts
/// submissions, Review presses and the blur events each field receives.
async fn replacing_form(h: &Harness) {
    h.goto("/dispatch.html").await;
    let html = "<form><label>Reference<input id=reference></label><label>Email<input id=email></label>\
                <button type=button id=review>Review</button></form>";
    let script = "window.seen = {submitted: 0, reviewed: 0, blurs: {reference: 0, email: 0}}; \
                  const form = document.querySelector('form'); \
                  form.onsubmit = event => { event.preventDefault(); seen.submitted += 1; }; \
                  document.getElementById('review').onclick = () => { seen.reviewed += 1; }; \
                  for (const id of ['reference', 'email']) \
                  document.getElementById(id).addEventListener('blur', () => { seen.blurs[id] += 1; }); \
                  let first = true; \
                  document.getElementById('reference').onchange = event => { \
                  if (first) { first = false; event.target.value = 'Beta42'; } };";
    eval_active(&h.page, &format!("document.body.innerHTML = {}; {script}", serde_json::to_string(html).unwrap()))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_leaving_a_field_runs_its_change_handler_and_submits_nothing() {
    let Some(h) = harness().await else { return };
    replacing_form(&h).await;
    let before = h.observe().await;
    assert_eq!(find(&before, "Reference").focused, Some(false));
    let mut expected = ["Beta42", "Alpha-17"].into_iter();
    let mut observation = before;
    for _ in 0..2 {
        let reference = find(&observation, "Reference");
        assert_eq!(
            h.page.act(&fill(reference, "Alpha-17"), &observation).await.unwrap().outcome,
            StepOutcome::Executed
        );
        let typed = h.observe().await;
        let reference = find(&typed, "Reference");
        assert_eq!((reference.value.as_deref(), reference.focused), (Some("Alpha-17"), Some(true)));
        let left = h.page.act(&action(Operation::Blur, reference), &typed).await.unwrap();
        assert_eq!(left.outcome, StepOutcome::Executed);
        observation = h.observe().await;
        let reference = find(&observation, "Reference");
        assert_eq!((reference.value.as_deref(), reference.focused), (expected.next(), Some(false)));
    }
    let seen = eval_active(&h.page, "seen").await;
    assert_eq!(seen, json!({"submitted": 0, "reviewed": 0, "blurs": {"reference": 2, "email": 0}}));
    assert_eq!(eval_active(&h.page, "document.activeElement === document.body").await, json!(true));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_blur_leaves_alone_a_field_that_lost_focus_or_was_replaced() {
    let Some(h) = harness().await else { return };
    replacing_form(&h).await;
    let before = h.observe().await;
    h.page.act(&fill(find(&before, "Email"), "ada@example.test"), &before).await.unwrap();
    let typed = h.observe().await;
    let email = find(&typed, "Email");
    assert_eq!(email.focused, Some(true));

    // Focus moved on since Jev chose: the field it chose is not left, and the one now focused is not touched.
    eval_active(&h.page, "document.getElementById('reference').focus()").await;
    let moved = h.page.act(&action(Operation::Blur, email), &typed).await.unwrap();
    assert_eq!(moved.outcome, StepOutcome::Failed);
    assert_eq!(eval_active(&h.page, "document.activeElement.id").await, json!("reference"));
    assert_eq!(eval_active(&h.page, "seen.blurs.reference").await, json!(0));

    // The field was redrawn as a twin that holds focus: the twin is another field and is not left either.
    let focused = h.observe().await;
    let reference = find(&focused, "Reference");
    assert_eq!(reference.focused, Some(true));
    eval_active(
        &h.page,
        "const old = document.getElementById('reference'), twin = old.cloneNode(); \
         old.replaceWith(twin); twin.focus(); window.twin = twin",
    )
    .await;
    let replaced = h.page.act(&action(Operation::Blur, reference), &focused).await.unwrap();
    assert_eq!(replaced.outcome, StepOutcome::Stale);
    assert_eq!(eval_active(&h.page, "document.activeElement === window.twin").await, json!(true));
    h.close().await;
}

// A field role is named by its label, never by what it holds (#838).

/// Replaces the page with `html` and returns the one control offered as a combobox.
async fn combobox(h: &Harness, html: &str) -> fastbrowse_core::page::Control {
    h.goto("/dispatch.html").await;
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(html).unwrap())).await;
    let observation = h.observe().await;
    let found: Vec<_> = observation.controls.iter().filter(|c| c.role == "combobox").collect();
    assert_eq!(found.len(), 1, "{:?}", observation.controls);
    found[0].clone()
}

const VISIBILITY: &str = "<fieldset><legend>Visibility</legend><label><input type=checkbox>Unlisted</label>\
                          <label><input type=checkbox>Hidden</label></fieldset>";

#[tokio::test(flavor = "multi_thread")]
async fn test_an_unlabelled_list_select_is_named_by_its_section_not_its_options() {
    let Some(h) = harness().await else { return };
    let html = format!(
        "<form><fieldset><legend>Categories</legend><select multiple size=5><option>Achievements</option>\
         <option>Bags</option><option selected>Chat</option><option>Combat</option></select></fieldset>{VISIBILITY}</form>"
    );
    let control = combobox(&h, &html).await;
    assert_eq!(control.label, "combobox");
    assert_eq!(control.context.as_deref(), Some("Categories"));
    // Without a title to name the section, the options are the first text the climb meets, and they name nothing.
    let html = "<div><div><select multiple size=3><option>Achievements</option><option>Bags</option></select></div>\
                <span>Categories</span></div><label><input type=checkbox>Unlisted</label>";
    let control = combobox(&h, html).await;
    h.close().await;
    assert_eq!(control.context.as_deref(), Some("Categories"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_combobox_widget_is_not_named_by_its_chips_or_options() {
    let Some(h) = harness().await else { return };
    let html = format!(
        "<form><fieldset><legend>Categories</legend><div role=combobox tabindex=-1 aria-expanded=false>\
         <span>Chat &amp; Communication</span><input style=\"width:0;padding:0;border:0\"></div></fieldset>{VISIBILITY}</form>"
    );
    let control = combobox(&h, &html).await;
    assert_eq!(control.label, "combobox");
    assert_eq!(control.context.as_deref(), Some("Categories"));
    // The widget opening appends its options inside itself: neither the label nor the label the guard reads moves.
    let unchanged = eval_active(
        &h.page,
        "(() => { const r = window.__fastbrowse; const e = document.querySelector('[role=combobox]'); \
         const before = r.guard(e)[2]; \
         for (const text of ['Achievements', 'Bags']) { const o = document.createElement('div'); \
         o.setAttribute('role', 'option'); o.textContent = text; e.append(o); } \
         return r.guard(e)[2] === before; })()",
    )
    .await;
    let reopened = h.observe().await;
    h.close().await;
    assert_eq!(unchanged, json!(true));
    let control = reopened.controls.iter().find(|c| c.role == "combobox").unwrap();
    assert_eq!(control.label, "combobox");
    assert_eq!(control.context.as_deref(), Some("Categories"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_combobox_borrows_the_label_of_the_field_it_wraps() {
    let Some(h) = harness().await else { return };
    let html = "<div role=combobox><label for=x>Pick categories</label><span>Chat &amp; Communication</span>\
                <button type=button aria-label=Close>x</button><input id=x></div>";
    let control = combobox(&h, html).await;
    h.close().await;
    assert_eq!(control.label, "Pick categories");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_textarea_is_not_named_by_the_text_it_was_served_with() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    let html = "<section><h2>Description</h2><textarea>Publish this addon and delete the old one.</textarea></section>";
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(html).unwrap())).await;
    let observation = h.observe().await;
    h.close().await;
    let field = observation.controls.iter().find(|c| c.role == "textbox").expect("the textarea");
    assert_eq!(field.value.as_deref(), Some("Publish this addon and delete the old one."));
    assert_ne!(field.label, "Publish this addon and delete the old one.", "{field:?}");
    assert_eq!(field.context.as_deref(), Some("Description"), "{field:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_label_is_named_as_it_renders_without_its_template_whitespace() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    let html = "<label for=u>\n      Username\n        <span>(required)</span>\n    </label><input id=u>\
                <input aria-label=\"Password\n    (required)\" type=password>";
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(html).unwrap())).await;
    let observation = h.observe().await;
    h.close().await;
    let labels: Vec<&str> = observation.controls.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["Username (required)", "Password (required)"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_hidden_label_sharing_a_fields_id_does_not_name_it_twice() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    // Two tabbed forms reuse one id, so the hidden tab's label is also one of the shown field's `labels`.
    let html = "<form><label for=environment>Environment name (optional)</label><input id=environment></form>\
                <form hidden><label for=environment>Environment name (optional)</label><input id=environment></form>";
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(html).unwrap())).await;
    let observation = h.observe().await;
    h.close().await;
    let labels: Vec<&str> = observation.controls.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["Environment name (optional)"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_combobox_wrapping_two_fields_borrows_no_name() {
    let Some(h) = harness().await else { return };
    let html = "<div role=combobox tabindex=0><select hidden><option>Chat</option></select>\
                <span>Chat</span><input aria-label=Search></div>";
    let control = combobox(&h, html).await;
    h.close().await;
    assert_eq!(control.label, "combobox");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_combobox_named_by_the_page_keeps_its_name() {
    let Some(h) = harness().await else { return };
    let html = "<div role=combobox aria-label=Categories><span>Chat</span><input aria-label=Pick></div>";
    let control = combobox(&h, html).await;
    h.close().await;
    assert_eq!(control.label, "Categories");
}

/// react-select under a form library's label, as Wago's addon settings render it: the label's `for` names the wrapper
/// div, which takes the id, and the combobox input inside has a generated one.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_label_naming_a_widgets_wrapper_names_its_one_field() {
    let Some(h) = harness().await else { return };
    let html = "<form><h2>General</h2><label for=categories>Categories</label><div id=categories>\
                <div><span>Chat &amp; Communication</span>\
                <div role=button aria-label=\"Remove Chat &amp; Communication\">x</div>\
                <input id=react-select-2-input role=combobox aria-autocomplete=list aria-expanded=false></div>\
                <input type=hidden name=categories value=1></div>\
                <div><input type=checkbox id=is_unlisted><label for=is_unlisted>Unlisted</label></div>\
                <button>Save changes</button></form>";
    let control = combobox(&h, html).await;
    let observation = h.observe().await;
    h.close().await;
    assert_eq!(control.label, "Categories");
    // Tied to a field, the label no longer titles what follows it as a section of its own.
    for label in ["Unlisted", "Save changes"] {
        assert_eq!(find(&observation, label).context.as_deref(), Some("General"), "{label}");
    }
}

/// The redraw watch reads every observed control's guard on each mutation batch, so a guard must reuse the
/// snapshot's index of wrapper-naming labels rather than search the page's labels again: on a grid of 2000 unlabelled
/// inputs that search cost a second per pass.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_guard_reuses_the_snapshots_wrapper_labels() {
    let Some(h) = harness().await else { return };
    let html = format!(
        "<form><label for=categories>Categories</label><div id=categories>\
         <input id=react-select-2-input role=combobox aria-autocomplete=list aria-expanded=false></div>\
         {}</form>",
        "<input>".repeat(200)
    );
    combobox(&h, &html).await;
    let session_id = h.session.active_session_id().unwrap();
    let guards = h
        .page
        .evaluate(
            &session_id,
            "(() => { const r = window.__fastbrowse; let searches = 0; \
             const count = proto => { const native = proto.querySelectorAll; \
               proto.querySelectorAll = function (s) { if (s === 'label[for]') searches++; return native.call(this, s); }; \
               return () => { proto.querySelectorAll = native; }; }; \
             const restore = [count(Document.prototype), count(Element.prototype), count(DocumentFragment.prototype)]; \
             const changed = [...r.observed].filter(([id, guard]) => r.guardKey(r.nodes.get(id)) !== guard).length; \
             document.querySelector('label').textContent = 'Tags'; \
             const field = [...r.observed.keys()].find(id => r.nodes.get(id).id === 'react-select-2-input'); \
             const renamed = r.guardKey(r.nodes.get(field)) !== r.observed.get(field); \
             restore.forEach(f => f()); \
             return { searches, changed, renamed }; })()",
        )
        .await
        .expect("evaluate");
    h.close().await;
    assert_eq!(guards, json!({"searches": 0, "changed": 0, "renamed": true}));
}

// A control clipped to nothing is not on the page for a person, and a widget that mirrors it is (#838).

/// How Tom Select and select2 hide the original field they keep in place.
const CLIPPED: &str = "position:absolute;width:1px;height:1px;clip:rect(0 0 0 0);clip-path:inset(50%);overflow:hidden";

#[tokio::test(flavor = "multi_thread")]
async fn test_a_clipped_select_beside_its_widget_is_not_offered() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    let html = format!(
        "<form><fieldset><legend>Categories</legend><select multiple style=\"{CLIPPED}\">\
         <option selected>Chat &amp; Communication</option><option>Achievements</option></select>\
         <div><span>Chat &amp; Communication</span><input role=combobox aria-label=\"Pick categories\"></div>\
         </fieldset></form>"
    );
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(&html).unwrap())).await;
    let observation = h.observe().await;
    h.close().await;
    let selects: Vec<_> = observation.controls.iter().filter(|c| c.operations.contains(&Operation::Select)).collect();
    assert!(selects.is_empty(), "{selects:?}");
    find(&observation, "Pick categories");
}

/// The clip check covers a control a script made pressable without a role, and reads an `auto` side as the box's edge.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_scripted_control_clipped_to_nothing_is_not_offered() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    let html = "<div onclick=\"\" style=\"position:absolute;clip:rect(auto, 0px, auto, 0px)\">Remove hidden</div>\
                <div onclick=\"\">Remove shown</div>";
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(html).unwrap())).await;
    let observation = h.observe().await;
    h.close().await;
    let labels: Vec<_> = observation.controls.iter().map(|c| c.label.as_str()).collect();
    assert!(labels.contains(&"Remove shown"), "{labels:?}");
    assert!(!labels.contains(&"Remove hidden"), "{labels:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_clipped_checkbox_is_offered_through_its_label() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    let html = format!("<form><label><input type=checkbox style=\"{CLIPPED}\">Notify me</label></form>");
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(&html).unwrap())).await;
    let observation = h.observe().await;
    let offered: Vec<_> = observation.controls.iter().filter(|c| c.label == "Notify me").collect();
    assert_eq!(offered.len(), 1, "{:?}", observation.controls);
    assert_eq!(offered[0].role, "checkbox");
    let result = h.page.act(&action(Operation::Click, offered[0]), &observation).await.expect("act");
    let checked = eval_active(&h.page, "document.querySelector('input').checked").await;
    h.close().await;
    assert_eq!(result.outcome, StepOutcome::Executed, "{:?}", result.detail);
    assert_eq!(checked, json!(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_clipped_file_input_uploads_through_its_label() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    let html = format!("<input id=file type=file style=\"{CLIPPED}\"><label for=file>Attach a photo</label>");
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(&html).unwrap())).await;
    let observation = h.observe().await;
    let offered: Vec<_> = observation.controls.iter().filter(|c| c.operations.contains(&Operation::Upload)).collect();
    assert_eq!(offered.len(), 1, "{:?}", observation.controls);
    assert_eq!(offered[0].label, "Attach a photo");
    let upload = Action {
        files: vec![Attachment { name: "a.txt".into(), mime_type: "text/plain".into(), content: Bytes(b"x".to_vec()) }],
        ..action(Operation::Upload, offered[0])
    };
    let result = h.page.act(&upload, &observation).await.expect("act");
    let count = eval_active(&h.page, "document.getElementById('file').files.length").await;
    h.close().await;
    assert_eq!(result.outcome, StepOutcome::Executed, "{:?}", result.detail);
    assert_eq!(count, json!(1));
}

/// A hidden, unlabelled file input that only a script-driven button opens: the click opens the browser's file chooser,
/// and that button is then offered as the place to upload, as a label is for its input.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_button_that_opens_a_hidden_file_input_uploads_through_its_chooser() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = '<input id=file type=file style=display:none><button>Replace File</button>'; \
         document.querySelector('button').onclick = () => document.getElementById('file').click();",
    )
    .await;
    let observation = h.observe().await;
    let uploads =
        |o: &fastbrowse_core::page::Observation| o.controls.iter().any(|c| c.operations.contains(&Operation::Upload));
    assert!(!uploads(&observation), "{:?}", observation.controls);
    let clicked =
        h.page.act(&action(Operation::Click, find(&observation, "Replace File")), &observation).await.expect("act");
    assert_eq!(clicked.outcome, StepOutcome::Executed, "{:?}", clicked.detail);
    let observation = h.observe().await;
    let opener = find(&observation, "Replace File");
    assert_eq!(opener.operations, [Operation::Upload].into(), "{:?}", observation.controls);
    let upload = Action {
        files: vec![Attachment {
            name: "lid.stl".into(),
            mime_type: "model/stl".into(),
            content: Bytes(b"solid".to_vec()),
        }],
        ..action(Operation::Upload, opener)
    };
    let result = h.page.act(&upload, &observation).await.expect("act");
    let name = eval_active(&h.page, "document.getElementById('file').files[0]?.name ?? null").await;
    h.close().await;
    assert_eq!(result.outcome, StepOutcome::Executed, "{:?}", result.detail);
    assert_eq!(name, json!("lid.stl"));
}

/// #979: an upload dialog's button that makes a file input in script and clicks it, never adding it to the page. Only
/// the input's own change handler refers to it, so once the click returns nothing in the page holds it; the button
/// uploads to it all the same.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_button_that_opens_a_file_input_never_on_the_page_uploads_through_its_chooser() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = '<div role=dialog aria-label=\"Add media\"><button>Upload or Drag Image</button></div>'; \
         document.querySelector('button').onclick = () => { const input = document.createElement('input'); \
         input.type = 'file'; input.onchange = () => { window.chosen = input.files[0]?.name; }; input.click(); };",
    )
    .await;
    let observation = h.observe().await;
    let clicked = h
        .page
        .act(&action(Operation::Click, find(&observation, "Upload or Drag Image")), &observation)
        .await
        .expect("act");
    assert_eq!(clicked.outcome, StepOutcome::Executed, "{:?}", clicked.detail);
    // Nothing but the chooser refers to the input now, so a collection before the next observation would take it.
    let active = h.session.active_session_id().expect("session");
    h.session.send("HeapProfiler.collectGarbage", json!({}), Some(&active)).await.expect("gc");
    let observation = h.observe().await;
    let opener = find(&observation, "Upload or Drag Image");
    assert_eq!(opener.operations, [Operation::Upload].into(), "{:?}", observation.controls);
    let upload = Action {
        files: vec![Attachment {
            name: "docks.png".into(),
            mime_type: "image/png".into(),
            content: Bytes(b"png".to_vec()),
        }],
        ..action(Operation::Upload, opener)
    };
    let result = h.page.act(&upload, &observation).await.expect("act");
    let chosen = eval_active(&h.page, "window.chosen ?? null").await;
    h.close().await;
    assert_eq!(result.outcome, StepOutcome::Executed, "{:?}", result.detail);
    assert_eq!(chosen, json!("docks.png"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_select_clipped_after_it_was_observed_is_stale() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    let html = "<form><label>Plan<select><option>Free</option><option>Pro</option></select></label></form>";
    eval_active(&h.page, &format!("document.body.innerHTML = {}", serde_json::to_string(html).unwrap())).await;
    let observation = h.observe().await;
    let plan = find(&observation, "Plan");
    eval_active(
        &h.page,
        &format!("document.querySelector('select').style.cssText = {}", serde_json::to_string(CLIPPED).unwrap()),
    )
    .await;
    let select = Action { text: Some("Pro".into()), ..action(Operation::Select, plan) };
    let result = h.page.act(&select, &observation).await.expect("act");
    let value = eval_active(&h.page, "document.querySelector('select').value").await;
    h.close().await;
    assert_eq!(result.outcome, StepOutcome::Stale, "{:?}", result.detail);
    assert_eq!(value, json!("Free"));
}

/// A comment form that posts answers in text alone: it empties its box and lists the new comment. A run told only
/// that the box emptied filled it and posted again, twice, so a commit's effect names the text the page added.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_posted_comment_is_named_in_the_effect_of_the_click_that_posted_it() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    let html = "<section aria-label=Comments><textarea id=box aria-label='Comment text area'></textarea>\
                <button type=button id=post>Post</button><div id=list>\
                <article><b>Serzath</b> <time>2 days ago</time><p>Works great, thanks.</p>\
                <button type=button>Reply</button></article></div></section>";
    let script = "document.getElementById('post').onclick = () => { \
                  const box = document.getElementById('box'), a = document.createElement('article'); \
                  a.innerHTML = '<b>cjber</b> <time>just now</time><p></p><button type=button>Reply</button>'; \
                  a.querySelector('p').textContent = box.value; document.getElementById('list').prepend(a); \
                  box.value = ''; }";
    eval_active(&h.page, &format!("document.body.innerHTML = {}; {script}", serde_json::to_string(html).unwrap()))
        .await;
    let before = h.observe().await;
    h.page.act(&fill(find(&before, "Comment text area"), "Version 2.4.1 is out."), &before).await.unwrap();
    let typed = h.observe().await;
    h.page.act(&action(Operation::Click, find(&typed, "Post")), &typed).await.unwrap();
    let posted = h.observe().await;
    let summary = fastbrowse_core::effects::effect_of_commit(&typed, &posted, None, false).summary;
    assert!(
        summary.ends_with("added the text: \"cjber\", \"just now\", \"Version 2.4.1 is out.\", \"Reply\""),
        "the click's effect is {summary:?}"
    );
    h.close().await;
}
