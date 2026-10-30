//! Pointer handlers can invalidate a target before its press or replace an editor before typing.
//!
//! A parametrized Python test is one Rust test that runs each case on a fresh session, in pytest's order.

mod common;
mod dispatch_support;

use std::time::Duration;

use common::{harness, harness_with, sites};
use dispatch_support::{PROXY_ERROR, action, eval_active, eval_value, failing_proxy, fill, find, observe_until};
use fastbrowse_browser::page::scripts::UNCOVER_JS;
use fastbrowse_browser::session::SessionOptions;
use fastbrowse_browser::testing;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;
use serde_json::{Value, json};

/// A string as a JS literal, where Python's tests interpolate `repr()` or `json.dumps()`.
fn js(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_click_uses_an_exposed_point_but_never_passes_through_a_cover() {
    // Flight rows had a covered centre even when another part of the same control was exposed.
    for nested in ["document", "shadow", "iframe"] {
        for covered in [false, true] {
            let case = format!("nested={nested} covered={covered}");
            let Some(h) = harness().await else { return };
            h.goto("/dispatch.html").await;
            if nested == "iframe" {
                eval_active(
                    &h.page,
                    "document.body.innerHTML = '<iframe width=700 height=300 src=/dispatch.html></iframe>'",
                )
                .await;
                observe_until(&h.page, "One way").await;
            }
            let root = match nested {
                "document" => "document",
                "iframe" => "document.querySelector('iframe').contentDocument",
                _ => "document.body.appendChild(document.createElement('div')).attachShadow({mode: 'open'})",
            };
            let script = format!(
                "window.fixtureRoot = {root}; {}const cover = fixtureRoot.querySelector('#cover'); \
                 cover.style.display = 'block'; {}window.clickedAt = null; \
                 fixtureRoot.querySelector('#target').addEventListener('click', \
                 e => window.clickedAt = [e.clientX, e.clientY]);",
                if nested == "shadow" {
                    "fixtureRoot.append(...document.querySelectorAll('style, #target, #decoy, #cover')); "
                } else {
                    ""
                },
                if covered { "" } else { "cover.style.cssText += 'left:130px;top:90px;width:40px;height:20px'; " },
            );
            eval_active(&h.page, &script).await;
            let before = h.observe().await;
            let target = find(&before, "One way").clone();
            let result = h.page.act(&action(Operation::Click, &target), &before).await.expect("act");
            let point = eval_active(&h.page, "window.clickedAt").await;
            let after = h.observe().await;
            if covered {
                assert!(result.outcome == StepOutcome::Covered && !result.page_changed, "{case}: {result:?}");
                assert_eq!(point, Value::Null, "{case}");
                assert_eq!(find(&after, "One way").selected, Some(false), "{case}");
            } else {
                assert!(result.outcome == StepOutcome::Executed && result.page_changed, "{case}: {result:?}");
                assert_eq!(point, json!([105, 90]), "{case}");
                assert_eq!(find(&after, "One way").selected, Some(true), "{case}");
                // Python also asserts `effect(before, after, target).set_something`; `effects.py` belongs to the
                // agent layer, which is not ported. The selected flip above is the change it reads.
            }
            h.close().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exposed_edge_belonging_to_a_nested_control_is_not_the_target() {
    // A covered row whose exposed edges are its own Delete button must not have Delete pressed for it.
    for nested in
        ["<button>Delete</button>", "<label><input type=checkbox>Archive</label>", "<div role=gridcell>Cell</div>"]
    {
        let Some(h) = harness().await else { return };
        h.goto("/dispatch.html").await;
        let script = format!(
            "const row = document.createElement('div'); row.id = 'target'; row.setAttribute('role', 'option'); \
             row.style.cssText = getComputedStyle(document.getElementById('target')).cssText; \
             row.textContent = 'One way'; document.getElementById('target').replaceWith(row); \
             row.insertAdjacentHTML('beforeend', {}); \
             row.lastElementChild.style.cssText = 'position:absolute;inset:0;margin:0'; \
             const cover = document.getElementById('cover'); cover.style.display = 'block'; \
             cover.style.cssText += 'left:130px;top:90px;width:40px;height:20px';",
            js(nested)
        );
        eval_active(&h.page, &script).await;
        let before = h.observe().await;
        let result = h.page.act(&action(Operation::Click, find(&before, "One way")), &before).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Covered, "{nested}");
        assert_eq!(eval_active(&h.page, "window.clicks").await, json!([]), "{nested}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_transparent_checkbox_filling_its_label_is_the_labels_target() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = '<label style=\"position:relative;display:inline-block;padding:12px\">\
         Direct service<input type=checkbox style=\"opacity:0;position:absolute;inset:0;margin:0\"></label>'; true",
    )
    .await;
    let obs = h.observe().await;
    let result = h.page.act(&action(Operation::Click, find(&obs, "Direct service")), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert_eq!(eval_active(&h.page, "document.querySelector('input').checked").await, json!(true));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hover_target_filling_a_button_does_not_cover_it() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.head.insertAdjacentHTML('beforeend', '<style>.tip{display:none}.face:hover .tip{display:block}\
         </style>'); document.body.innerHTML = '<button onclick=\"window.pressed = (window.pressed || 0) + 1\" \
         style=\"padding:0\"><span class=face style=\"display:block;padding:12px\">Save<span class=tip>\
         <img alt=\"\" width=8 height=8 src=\"data:image/gif;base64,R0lGODlhAQABAAAAACw=\"></span></span></button>'; true",
    )
    .await;
    let obs = h.observe().await;
    let button = obs.controls.iter().find(|c| c.role == "button").expect("a button");
    let result = h.page.act(&action(Operation::Click, button), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed, "{:?}", result.detail);
    assert_eq!(eval_active(&h.page, "window.pressed").await, json!(1));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_transparent_input_hit_must_be_the_input_or_its_own_label() {
    for associated in [false, true] {
        let Some(h) = harness().await else { return };
        h.goto("/todos.html").await;
        eval_active(
            &h.page,
            &format!(
                "const input = document.querySelectorAll('input')[1]; input.id = 'choice'; \
                 const label = input.nextElementSibling; \
                 label.style.cssText = 'position:absolute;inset:0;background:white'; {}",
                if associated { "label.htmlFor = 'choice';" } else { "" }
            ),
        )
        .await;
        let obs = h.observe().await;
        // An associated visible label is the control itself, named by its text; an unassociated one only covers.
        let target = obs
            .controls
            .iter()
            .find(|c| c.context.as_deref() == Some("walk the dog") || c.label == "walk the dog")
            .expect("the walk the dog control");
        let result = h.page.act(&action(Operation::Click, target), &obs).await.expect("act");
        let expected = if associated { StepOutcome::Executed } else { StepOutcome::Covered };
        assert_eq!(result.outcome, expected, "associated={associated}");
        assert_eq!(result.page_changed, associated, "associated={associated}");
        assert_eq!(eval_active(&h.page, "input.checked").await, json!(associated), "associated={associated}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_link_inside_a_label_does_not_count_as_its_input() {
    // A link filling the label takes the click itself; the input never toggles.
    let Some(h) = harness().await else { return };
    h.goto("/todos.html").await;
    eval_active(
        &h.page,
        "const input = document.querySelectorAll('input')[1]; input.id = 'choice'; \
         input.style.opacity = '1'; const label = input.nextElementSibling; label.htmlFor = 'choice'; \
         label.style.cssText = 'position:absolute;inset:0;background:white'; \
         label.innerHTML = '<a href=\\'#linked\\' style=\\'display:block;height:100%\\'>walk the dog</a>';",
    )
    .await;
    let obs = h.observe().await;
    let target = obs
        .controls
        .iter()
        .find(|c| {
            c.input_type.as_deref() == Some("checkbox")
                && (c.context.as_deref() == Some("walk the dog") || c.label == "walk the dog")
        })
        .expect("the walk the dog checkbox");
    let result = h.page.act(&action(Operation::Click, target), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Covered);
    assert_eq!(eval_active(&h.page, "[input.checked, location.hash]").await, json!([false, ""]));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fingerprint_tracks_selection_without_counting_text_field_values() {
    // Checked and selected properties can change without changing any text or DOM attributes.
    for nested in ["document", "shadow", "iframe"] {
        let Some(h) = harness().await else { return };
        h.goto("/todos.html").await;
        let root = match nested {
            "document" => "document.querySelector('div')",
            "shadow" => "document.querySelector('div').attachShadow({mode: 'open'})",
            _ => "document.querySelector('iframe').contentDocument.body",
        };
        eval_active(
            &h.page,
            &format!(
                "document.body.innerHTML = '<div></div><iframe></iframe>'; window.fixtureRoot = {root}; \
                 fixtureRoot.innerHTML = '<input type=checkbox><select multiple><option>First</option>' \
                 + '<option>Second</option></select><button role=switch aria-checked=false>Nonstop only</button>' \
                 + '<button role=option aria-selected=false>One way</button><input type=text>' \
                 + '<button hidden role=tab aria-selected=false>Slide 2</button>';"
            ),
        )
        .await;
        let mut before = h.page.fingerprint().await.expect("fingerprint");
        for change in [
            "fixtureRoot.querySelector('input').checked = true",
            "fixtureRoot.querySelectorAll('option')[1].selected = true",
            "fixtureRoot.querySelector('[role=switch]').setAttribute('aria-checked', 'true')",
            "fixtureRoot.querySelector('[role=option]').setAttribute('aria-selected', 'true')",
        ] {
            eval_active(&h.page, change).await;
            let after = h.page.fingerprint().await.expect("fingerprint");
            assert_ne!(after, before, "{nested}: {change}");
            before = after;
        }
        eval_active(
            &h.page,
            "fixtureRoot.querySelector('[type=text]').value = 'requests'; \
             fixtureRoot.querySelector('[role=tab]').setAttribute('aria-selected', 'true')",
        )
        .await;
        // Neither a typed value nor a hidden carousel's selection is progress.
        assert_eq!(h.page.fingerprint().await.expect("fingerprint"), before, "{nested}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pointer_entry_rechecks_the_target_before_pressing() {
    for mode in ["move", "animate", "cover", "relabel"] {
        let Some(h) = harness().await else { return };
        h.goto(&format!("/dispatch.html?{mode}")).await;
        let obs = h.observe().await;
        let target = find(&obs, "One way");
        let result = h.page.act(&action(Operation::Click, target), &obs).await.expect("act");
        let state =
            eval_active(&h.page, "[presses, clicks, document.getElementById('target').getAttribute('aria-selected')]")
                .await;
        let (presses, clicks, selected) = (&state[0], &state[1], &state[2]);
        if mode == "move" || mode == "animate" {
            assert_eq!(result.outcome, StepOutcome::Executed, "{mode}");
            assert!(presses == clicks && *presses == json!(["target"]), "{mode}: {state}");
            assert_eq!(*selected, json!("true"), "{mode}");
            assert_eq!(eval_active(&h.page, "pressPositions").await, json!([400]), "{mode}");
        } else {
            let expected = if mode == "cover" { StepOutcome::Covered } else { StepOutcome::Stale };
            assert_eq!(result.outcome, expected, "{mode}");
            assert!(presses == clicks && *presses == json!([]), "{mode}: {state}");
            assert_eq!(*selected, json!("false"), "{mode}");
        }
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_target_changed_in_a_pointer_entry_frame_is_never_pressed() {
    for change in ["cover", "relabel", "detach"] {
        for framed in [false, true] {
            let case = format!("change={change} framed={framed}");
            let Some(h) = harness().await else { return };
            h.goto("").await;
            let mut session_id = h.session.active_session_id().expect("active session");
            if framed {
                // Chrome throttles animation frames in offscreen children; this fixture needs its entry frame to run.
                eval_value(&h.session, &session_id, "document.querySelector('iframe').scrollIntoView()").await;
                let obs = observe_until(&h.page, "Frame button").await;
                let frame_id = find(&obs, "Frame button").frame_id.clone().expect("a frame id");
                session_id = h
                    .session
                    .frame_sessions()
                    .into_iter()
                    .find(|(frame, _)| *frame == frame_id)
                    .map(|(_, session)| session)
                    .expect("the frame's session");
            }
            let mutation = match change {
                "cover" => "document.getElementById('cover').style.display = 'block'",
                "relabel" => "e.textContent = 'Delete reservation'",
                _ => "e.replaceWith(e.cloneNode(true))",
            };
            eval_value(
                &h.session,
                &session_id,
                &format!(
                    "document.body.innerHTML = '<button id=target>One way</button><div id=cover \
                     style=\"display:none;position:fixed;inset:0;background:white;z-index:1\"></div>'; \
                     window.presses = 0; document.addEventListener('mousedown', () => window.presses++); \
                     const e = document.getElementById('target'); \
                     e.onpointerenter = () => requestAnimationFrame(() => {{ {mutation}; }});"
                ),
            )
            .await;
            let obs = observe_until(&h.page, "One way").await;
            let result = h.page.act(&action(Operation::Click, find(&obs, "One way")), &obs).await.expect("act");
            let expected = if change == "cover" { StepOutcome::Covered } else { StepOutcome::Stale };
            assert_eq!(result.outcome, expected, "{case}: {:?}", result.detail);
            assert_eq!(eval_value(&h.session, &session_id, "window.presses").await, json!(0), "{case}");
            h.close().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fill_follows_a_replacement_only_at_the_original_position() {
    for framed in [false, true] {
        for mode in ["replace", "replace-late", "replace-decoy"] {
            let case = format!("mode={mode} framed={framed}");
            let Some(h) = harness().await else { return };
            h.goto(&format!("/dispatch.html?{mode}")).await;
            if framed {
                eval_active(
                    &h.page,
                    &format!(
                        "document.body.innerHTML = '<iframe width=700 height=300 src=\"/dispatch.html?{mode}\"></iframe>'"
                    ),
                )
                .await;
            }
            let obs = observe_until(&h.page, "Origin").await;
            let result = h.page.act(&fill(find(&obs, "Origin"), "London"), &obs).await.expect("act");
            let decoy = mode == "replace-decoy";
            let expected = if decoy { StepOutcome::Failed } else { StepOutcome::Executed };
            assert_eq!(result.outcome, expected, "{case}: {:?}", result.detail);
            let value = find(&h.observe().await, "Origin").value.clone();
            assert_eq!(value.as_deref(), Some(if decoy { "" } else { "London" }), "{case}");
            let view = if framed { "document.querySelector('iframe').contentWindow" } else { "window" };
            assert_eq!(eval_active(&h.page, &format!("{view}.clicks")).await, json!(["target"]), "{case}");
            h.close().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_native_date_field_is_committed_through_the_value_setter() {
    // Typing into a native date input lands in whichever locale segment its own picker has focused, not the ISO
    // value a field writer produces, so the value is set the way the picker itself would commit one.
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    eval_active(&h.page, "document.body.innerHTML = '<label>Start Date<input type=\"date\" id=\"start\"></label>'")
        .await;
    let obs = observe_until(&h.page, "Start Date").await;
    let target = find(&obs, "Start Date");
    assert_eq!(target.input_type.as_deref(), Some("date"));
    let result = h.page.act(&fill(target, "2026-09-28"), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert_eq!(find(&h.observe().await, "Start Date").value.as_deref(), Some("2026-09-28"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_native_date_field_that_refuses_a_malformed_value_fails() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    eval_active(&h.page, "document.body.innerHTML = '<label>Start Date<input type=\"date\" id=\"start\"></label>'")
        .await;
    let obs = observe_until(&h.page, "Start Date").await;
    let target = find(&obs, "Start Date");
    // A native date input silently keeps an empty value for anything that is not its own ISO shape.
    let result = h.page.act(&fill(target, "28 September 2026"), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Failed);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_secret_fill_does_not_follow_a_replacement() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html?replace").await;
    let obs = h.observe().await;
    let secret = Action {
        secret: true,
        secret_origin: Some(sites().main.clone()),
        ..fill(find(&obs, "Origin"), "secret-value")
    };
    let result = h.page.act(&secret, &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Failed);
    assert_eq!(find(&h.observe().await, "Origin").value.as_deref(), Some(""));
    assert_eq!(eval_active(&h.page, "clicks").await, json!(["target"]));
    h.close().await;
}

// test_retargeting_requires_the_receiving_document_and_guard_to_survive is not ported: it drives
// `Agent._act_on_twin`, the agent loop's retargeting, which this crate does not have.

#[tokio::test(flavor = "multi_thread")]
async fn test_uncertain_press_is_never_replayed() {
    // Python patches `page._input` to raise once the second input (the press) has been sent. The proxy does the same
    // from outside: Chrome receives the press, and its reply comes back as an error.
    let Some(chrome) = common::chrome() else { return };
    let mut pressed = false;
    let proxy = failing_proxy(chrome.cdp_url.clone(), move |command| {
        let doomed =
            !pressed && command["method"] == "Input.dispatchMouseEvent" && command["params"]["type"] == "mousePressed";
        pressed |= doomed;
        doomed
    })
    .await;
    let connection = fastbrowse_core::models::BrowserConnection { cdp_url: proxy, ..chrome.clone() };
    let h = common::harness_on(connection, Config::default(), SessionOptions::default()).await.expect("harness");
    h.goto("/dispatch.html").await;
    let obs = h.observe().await;
    let error = h.page.act(&action(Operation::Click, find(&obs, "One way")), &obs).await.expect_err("press is lost");
    assert_eq!(error.to_string(), format!("Input.dispatchMouseEvent failed (CDP {PROXY_ERROR})"));
    assert_eq!(eval_active(&h.page, "[presses, clicks]").await, json!([["target"], []]));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_pointer_entry_dialog_does_not_block_a_fresh_hit_test() {
    for deferred in [false, true] {
        let Some(h) = harness().await else { return };
        h.goto("/dispatch.html").await;
        eval_active(
            &h.page,
            &format!(
                "document.getElementById('target').onpointerenter = () => {}",
                if deferred { "requestAnimationFrame(() => confirm('Continue?'))" } else { "confirm('Continue?')" }
            ),
        )
        .await;
        let obs = h.observe().await;
        // The bound catches a click that hangs behind the dialog; a busy CI runner took over 2s to get through it.
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            h.page.act(&action(Operation::Click, find(&obs, "One way")), &obs),
        )
        .await
        .expect("click returns")
        .expect("act");
        assert_eq!(result.outcome, StepOutcome::Failed, "deferred={deferred}");
        assert!(h.session.pending_dialog().is_some(), "deferred={deferred}");
        h.session.handle_dialog(false, None).await.expect("dialog");
        assert_eq!(eval_active(&h.page, "presses").await, json!([]), "deferred={deferred}");
        h.close().await;
    }
}

fn drag(source: &fastbrowse_core::page::Control, destination: &fastbrowse_core::page::Control) -> Action {
    Action { destination_id: Some(destination.id.clone()), ..action(Operation::Drag, source) }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_drag_hit_tests_the_destination_before_pressing() {
    for destination_change in ["none", "covered", "renamed"] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        eval_active(
            &h.page,
            "document.body.innerHTML = `
              <div id=source draggable=true style=\"position:fixed;left:20px;top:20px;width:100px;height:80px\">Card A</div>
              <button id=drop style=\"position:fixed;left:220px;top:20px;width:100px;height:80px\">Done</button>`;
            window.presses = 0;
            source.onpointerdown = () => window.presses++;
            source.ondragstart = e => e.dataTransfer.setData('text/plain', 'Card A');
            drop.ondragover = e => e.preventDefault();
            drop.ondrop = e => { e.preventDefault(); drop.textContent = e.dataTransfer.getData('text/plain'); };
            true",
        )
        .await;
        let obs = h.observe().await;
        let (source, destination) = (find(&obs, "Card A"), find(&obs, "Done"));
        if destination_change != "none" {
            eval_active(
                &h.page,
                if destination_change == "covered" {
                    "document.body.insertAdjacentHTML('beforeend', \
                     '<div style=\"position:fixed;inset:0 auto auto 200px;width:160px;height:120px;z-index:10\"></div>'); true"
                } else {
                    "drop.textContent = 'Trash'; true"
                },
            )
            .await;
        }

        let result = h.page.act(&drag(source, destination), &obs).await.expect("act");
        let none = destination_change == "none";
        let expected = if none { StepOutcome::Executed } else { StepOutcome::Stale };
        assert_eq!(result.outcome, expected, "{destination_change}: {:?}", result.detail);
        assert_eq!(
            eval_active(&h.page, "window.presses").await,
            json!(if none { 1 } else { 0 }),
            "{destination_change}"
        );
        if none {
            assert!(result.page_changed);
            assert_eq!(eval_active(&h.page, "drop.textContent").await, json!("Card A"));
        }
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_drag_revalidates_its_endpoints_after_scrolling_and_pointer_entry() {
    for change in ["scroll", "move", "replace", "destination"] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        let handler = match change {
            "scroll" => "drop.style.top = '2000px';",
            "move" => "source.onpointerenter = () => { source.style.top = '200px'; };",
            "replace" => "source.onpointerenter = () => { source.replaceWith(source.cloneNode(true)); };",
            _ => "source.onpointerenter = () => { drop.textContent = 'Trash'; };",
        };
        eval_active(
            &h.page,
            &format!(
                "document.body.innerHTML = `
                  <div id=source draggable=true
                    style=\"position:absolute;left:20px;top:20px;width:100px;height:80px\">Card A</div>
                  <button id=drop style=\"position:absolute;left:220px;top:20px;width:100px;height:80px\">Done</button>`;
                window.presses = [];
                document.addEventListener('pointerdown', e => window.presses.push(e.target.id));
                source.ondragstart = e => e.dataTransfer.setData('text/plain', 'Card A');
                drop.ondragover = e => e.preventDefault();
                drop.ondrop = e => {{ e.preventDefault(); drop.textContent = e.dataTransfer.getData('text/plain'); }};
                {handler}"
            ),
        )
        .await;
        let obs = h.observe().await;
        let result = h.page.act(&drag(find(&obs, "Card A"), find(&obs, "Done")), &obs).await.expect("act");

        let moved = change == "move";
        let expected = if moved { StepOutcome::Executed } else { StepOutcome::Stale };
        assert_eq!(result.outcome, expected, "{change}: {:?}", result.detail);
        assert_eq!(
            eval_active(&h.page, "window.presses").await,
            if moved { json!(["source"]) } else { json!([]) },
            "{change}"
        );
        if moved {
            assert_eq!(eval_active(&h.page, "drop.textContent").await, json!("Card A"));
        }
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_destination_changed_by_the_drag_itself_is_dropped_on_only_if_it_is_the_same_control() {
    let cases: [(&str, &str, &[&str]); 4] = [
        // Another control under the pointer: the native drag is cancelled, so not even the source sees a drop.
        ("renamed", "drop.textContent = 'Trash';", &[]),
        // The same control somewhere else: the release follows it, after the dragover that lets it take the drop.
        ("moved", "drop.style.top = '300px';", &["drop"]),
        ("nudged", "drop.ondragover = () => { drop.style.transform = 'translateY(2px)'; };", &["drop"]),
        // Away and back with no move in between: Chrome still targets the control it last hovered.
        (
            "returned",
            "drop.style.left = '600px'; document.body.insertAdjacentHTML('beforeend', \
             '<div id=decoy style=\"position:fixed;left:220px;top:20px;width:100px;height:80px\"></div>'); \
             decoy.ondragover = e => { if (e.clientX >= 270) { drop.style.left = '220px'; \
             decoy.style.left = '600px'; } };",
            &["drop"],
        ),
    ];
    for (id, change, dropped) in cases {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        eval_active(
            &h.page,
            &format!(
                "document.body.innerHTML = `
                  <div id=home style=\"position:fixed;left:0;top:0;width:160px;height:120px\">
                    <div id=source draggable=true
                      style=\"position:absolute;left:20px;top:20px;width:100px;height:80px\">Card A</div></div>
                  <button id=drop style=\"position:fixed;left:220px;top:20px;width:100px;height:80px\">Done</button>`;
                window.drops = [];
                document.addEventListener('dragover', e => e.preventDefault());
                document.addEventListener('drop', e => {{ e.preventDefault(); window.drops.push(e.target.id); }});
                source.ondragstart = e => {{ e.dataTransfer.setData('text/plain', 'Card A'); {change} }}; true"
            ),
        )
        .await;
        let obs = h.observe().await;
        let result = h.page.act(&drag(find(&obs, "Card A"), find(&obs, "Done")), &obs).await.expect("act");

        let expected = if dropped.is_empty() { StepOutcome::Stale } else { StepOutcome::Executed };
        assert_eq!(result.outcome, expected, "{id}: {:?}", result.detail);
        assert_eq!(eval_active(&h.page, "window.drops").await, json!(dropped), "{id}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_drag_onto_its_own_source_is_refused_before_pressing() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = '<button id=source draggable=true>Card A</button>'; \
         window.clicks = 0; source.onclick = () => window.clicks++; true",
    )
    .await;
    let obs = h.observe().await;
    let source = find(&obs, "Card A");
    let result = h.page.act(&drag(source, source), &obs).await.expect("act");

    assert_eq!(result.outcome, StepOutcome::Failed);
    assert_eq!(eval_active(&h.page, "window.clicks").await, json!(0));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_drag_across_frame_sessions_is_refused_before_pressing() {
    let Some(h) = harness().await else { return };
    h.goto("").await;
    observe_until(&h.page, "Frame button").await;
    eval_active(
        &h.page,
        "document.body.insertAdjacentHTML('afterbegin', '<div draggable=true id=source>Card A</div>'); \
         window.presses = 0; document.addEventListener('pointerdown', () => window.presses++);",
    )
    .await;
    let obs = h.observe().await;
    let destination = find(&obs, "Frame button");
    let frame_id = destination.frame_id.clone();
    assert!(h.session.frame_sessions().iter().any(|(frame, _)| Some(frame) == frame_id.as_ref()));
    let result = h.page.act(&drag(find(&obs, "Card A"), destination), &obs).await.expect("act");

    assert_eq!(result.outcome, StepOutcome::Failed);
    assert_eq!(result.detail.as_deref(), Some("drag across browser frame sessions is not supported"));
    assert_eq!(eval_active(&h.page, "window.presses").await, json!(0));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_unstable_target_expires_without_a_press() {
    // Python zeroes `_TARGET_STABILITY_SECONDS` and mocks `_before_action` to report the target somewhere new on
    // every check. A control that never stops moving gives `_click_point` the same answer through the real path:
    // each check finds it moved, the stability deadline runs out, and nothing is pressed.
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.head.insertAdjacentHTML('beforeend', '<style>@keyframes drift{from{left:20px}to{left:400px}}\
         </style>'); document.body.innerHTML = '<button style=\"position:absolute;top:20px;\
         animation:drift 2s linear infinite alternate\">Drifting</button>'; window.presses = 0; \
         document.addEventListener('mousedown', () => window.presses++); true",
    )
    .await;
    let obs = h.observe().await;
    let result = h.page.act(&action(Operation::Click, find(&obs, "Drifting")), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Stale, "{:?}", result.detail);
    assert_eq!(result.detail.as_deref(), Some("target did not stop moving before pointer press"));
    assert_eq!(eval_active(&h.page, "window.presses").await, json!(0));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_hydration_marking_a_control_enabled_does_not_make_it_stale() {
    // Google Flights adds aria-disabled="false" as it hydrates, after the run first observed its controls.
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = '<button onclick=\"this.dataset.clicked = 1\">Round trip</button>'; true",
    )
    .await;
    let obs = h.observe().await;
    eval_active(&h.page, "document.querySelector('button').setAttribute('aria-disabled', 'false'); true").await;
    let target = obs.controls.iter().find(|c| c.label == "Round trip").expect("Round trip");
    let result = h.page.act(&action(Operation::Click, target), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_select_the_page_refuses_is_not_executed() {
    // A change handler that restores the previous option must not read as a first write, which is progress.
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(
        &h.page,
        "document.body.innerHTML = '<label>Plan <select id=plan><option>Free</option><option>Pro</option>\
         </select></label>'; document.getElementById('plan').onchange = e => { e.target.value = 'Free'; }; true",
    )
    .await;
    let obs = h.observe().await;
    let select = Action { text: Some("Pro".into()), ..action(Operation::Select, find(&obs, "Plan")) };
    let result = h.page.act(&select, &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Failed);
    assert!(result.detail.as_deref().is_some_and(|d| d.contains("'Free'")), "{:?}", result.detail);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_covered_target_reports_visible_overlay_controls() {
    for semantic in [false, true] {
        for detail_race in [false, true] {
            let case = format!("semantic={semantic} detail_race={detail_race}");
            // Python patches `page._evaluate` to raise on the obstruction description; the proxy answers that
            // evaluation with an error instead, as a context lost while describing the obstruction would.
            let h = if detail_race {
                let Some(chrome) = common::chrome() else { return };
                let proxy = failing_proxy(chrome.cdp_url.clone(), |command| {
                    command["method"] == "Runtime.evaluate"
                        && command["params"]["expression"]
                            .as_str()
                            .is_some_and(|e| e.contains("let cover = hit?.closest"))
                })
                .await;
                let connection = fastbrowse_core::models::BrowserConnection { cdp_url: proxy, ..chrome.clone() };
                common::harness_on(connection, Config::default(), SessionOptions::default()).await.expect("harness")
            } else {
                let Some(h) = harness_with(Config::default(), SessionOptions::default()).await else { return };
                h
            };
            h.goto("/dispatch.html").await;
            eval_active(
                &h.page,
                "const cover = document.getElementById('cover'); cover.style.display = 'block'; \
                 cover.setAttribute('role','dialog'); cover.setAttribute('aria-label','Basket'); \
                 cover.innerHTML = '<button aria-label=Close>Close</button>'; \
                 cover.firstElementChild.onclick = () => cover.style.display = 'none';",
            )
            .await;
            if !semantic {
                eval_active(
                    &h.page,
                    "const cover = document.getElementById('cover'); cover.removeAttribute('role'); \
                     cover.removeAttribute('aria-label'); \
                     cover.style.cssText = 'display:block;position:fixed;inset:0;width:auto;height:auto;z-index:20'; \
                     cover.innerHTML = '<div style=\"position:absolute;inset:0;background:white\"></div>\
                     <button style=\"position:relative\" aria-label=Close>Close</button>'; \
                     cover.lastElementChild.onclick = () => cover.style.display = 'none';",
                )
                .await;
            }
            let before = h.observe().await;
            let result = h.page.act(&action(Operation::Click, find(&before, "One way")), &before).await.expect("act");
            assert_eq!(result.outcome, StepOutcome::Covered, "{case}");
            let detail = result.detail.clone().unwrap_or_default();
            if detail_race {
                assert_eq!(detail, "Target is covered.", "{case}");
            } else {
                assert!(detail.contains("Close"), "{case}: {detail}");
                if semantic {
                    assert!(detail.contains("Basket"), "{case}: {detail}");
                }
            }
            assert_eq!(eval_active(&h.page, "window.clicks").await, json!([]), "{case}");
            let close = find(&h.observe().await, "Close").clone();
            let closed = h.page.act(&action(Operation::Click, &close), &h.observe().await).await.expect("act");
            assert_eq!(closed.outcome, StepOutcome::Executed, "{case}: {:?}", closed.detail);
            let after = h.observe().await;
            let retried = h.page.act(&action(Operation::Click, find(&after, "One way")), &after).await.expect("act");
            assert_eq!(retried.outcome, StepOutcome::Executed, "{case}: {:?}", retried.detail);
            h.close().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_covered_target_does_not_offer_its_fixed_parent_controls() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    eval_active(
        &h.page,
        "const shell = document.createElement('div'); shell.style.cssText = 'position:fixed;inset:0'; \
         document.body.append(shell); \
         shell.append(document.getElementById('target'), document.getElementById('cover')); \
         const cover = document.getElementById('cover'); cover.style.display = 'block'; \
         shell.insertAdjacentHTML('beforeend', '<button>Unrelated action</button>'); true",
    )
    .await;
    let obs = h.observe().await;
    let result = h.page.act(&action(Operation::Click, find(&obs, "One way")), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Covered);
    let detail = result.detail.unwrap_or_default();
    assert!(!detail.contains("Unrelated action"), "{detail}");
    assert!(!detail.contains("One way"), "{detail}");
    h.close().await;
}

/// A login form whose button sits under a band fixed to the window's foot, as an anchored ad does.
fn anchored_form(scrolls: bool, band: &str) -> String {
    let body = "<form id=login onsubmit=\"event.preventDefault(); window.submitted = (window.submitted || 0) + 1\" \
                style=\"position:absolute;left:20px;bottom:30px\">\
                <input name=user aria-label=Username><input name=pass type=password aria-label=Password>\
                <button id=go onclick=\"window.pressed = (window.pressed || 0) + 1\">Login</button></form>";
    let inside = if band == "iframe" {
        "<iframe aria-label=Advertisement srcdoc=\"<p>Ad</p>\" style=\"width:100%;height:100%;border:0\"></iframe>"
    } else {
        "<button onclick=\"window.adClicks++\">Shop now</button>"
    };
    format!(
        "window.adClicks = 0; document.documentElement.style.height = '100%'; \
         document.body.style.cssText = 'margin:0;position:relative;height:{}'; \
         document.body.innerHTML = {}; \
         const band = document.createElement('ins'); \
         band.style.cssText = 'display:block;position:fixed;left:0;right:0;bottom:0;height:120px;\
         background:white;z-index:9'; \
         band.addEventListener('mousedown', () => window.adClicks++); \
         band.innerHTML = {}; document.body.append(band); \
         document.getElementById('login').style.top = (innerHeight - 70) + 'px'; \
         document.getElementById('login').style.bottom = ''; true",
        // The form starts in the band's place on the first screen, whatever the page's length.
        if scrolls { "3000px" } else { "100%;overflow:hidden" },
        js(body),
        js(inside),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_control_under_a_band_fixed_to_the_window_is_reached_without_touching_the_band() {
    // expandtesting-login: an ad anchored to the window's foot covered Login twice and the run ended stuck. A
    // person scrolls the button clear of it; where the page cannot scroll, Login is pressed from the keyboard.
    for scrolls in [true, false] {
        for band in ["div", "iframe"] {
            let case = format!("scrolls={scrolls} band={band}");
            let Some(h) = harness().await else { return };
            h.goto("").await;
            eval_active(&h.page, &anchored_form(scrolls, band)).await;
            assert_ne!(
                eval_active(
                    &h.page,
                    "(r => document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2)?.id)\
                     (document.getElementById('go').getBoundingClientRect())"
                )
                .await,
                json!("go"),
                "the fixture must start with Login under the band"
            );
            let obs = h.observe().await;
            let result = h.page.act(&action(Operation::Click, find(&obs, "Login")), &obs).await.expect("act");
            assert_eq!(result.outcome, StepOutcome::Executed, "{case}: {:?}", result.detail);
            // A page that scrolls has Login clicked where it was scrolled clear of the band; the keyboard is the
            // fallback.
            assert_eq!(result.detail.is_none(), scrolls, "{case}: {:?}", result.detail);
            assert_eq!(
                eval_active(&h.page, "[window.pressed, window.submitted, window.adClicks]").await,
                json!([1, 1, 0]),
                "{case}"
            );
            h.close().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_field_under_a_band_is_filled_and_submitted_from_the_keyboard() {
    // The same ad covered the password field once the run had typed the username, so Enter was refused too.
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(&h.page, &anchored_form(false, "div")).await;
    let obs = h.observe().await;
    let filled = h.page.act(&fill(find(&obs, "Password"), "hunter2"), &obs).await.expect("act");
    assert_eq!(filled.outcome, StepOutcome::Executed, "{:?}", filled.detail);
    let obs = h.observe().await;
    let entered = h.page.act(&action(Operation::Enter, find(&obs, "Password")), &obs).await.expect("act");
    assert_eq!(entered.outcome, StepOutcome::Executed, "{:?}", entered.detail);
    assert_eq!(
        eval_active(&h.page, "[document.querySelector('[name=pass]').value, window.submitted, window.adClicks]").await,
        json!(["hunter2", 1, 0])
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_fixed_cover_the_page_means_to_be_answered_is_still_reported() {
    // Only a band floating over the page is routed around. A dialog asks its question first, an open menu or
    // popover is the page's to close, and a cover over the whole window shows something other than the control, so
    // none is pressed through, by pointer or keyboard.
    for cover in ["dialog", "menu", "popover", "full-window"] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        eval_active(&h.page, &anchored_form(false, "div")).await;
        let setup = match cover {
            "dialog" => "document.querySelector('ins').setAttribute('role', 'dialog'); true",
            "menu" => "document.querySelector('ins').setAttribute('role', 'menu'); true",
            // A shown popover sits in the top layer; its own inset is kept at the window's foot.
            "popover" => {
                "{ const shown = document.querySelector('ins'); shown.setAttribute('popover', 'manual'); \
                 shown.style.cssText += ';top:auto;margin:0;border:0;padding:0;width:auto'; shown.showPopover(); } \
                 document.querySelector('ins').matches(':popover-open')"
            }
            _ => "document.querySelector('ins').style.cssText += ';top:0;height:auto'; true",
        };
        // A setup script that throws evaluates to nothing, and the cover it meant to draw would never be tested.
        assert_eq!(eval_active(&h.page, setup).await, json!(true), "{cover}");
        let obs = h.observe().await;
        let result = h.page.act(&action(Operation::Click, find(&obs, "Login")), &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Covered, "{cover}: {:?}", result.detail);
        let entered = h.page.act(&action(Operation::Enter, find(&obs, "Password")), &obs).await.expect("act");
        assert_eq!(entered.outcome, StepOutcome::Covered, "{cover}: {:?}", entered.detail);
        assert_eq!(
            eval_active(&h.page, "[window.pressed, window.submitted, window.adClicks]").await,
            json!([null, null, 0]),
            "{cover}"
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_band_drawn_inside_a_shadow_root_is_classed_as_a_band() {
    // Ad overlays are often custom elements: the element itself is fixed and what it draws lives in its shadow root.
    // The walk that classes a cover went up from what was hit and stopped at the shadow root, so the band read as
    // something else and Login was refused; a dialog drawn the same way must still be refused, never pressed through.
    for (role, reached) in [(None, true), (Some("dialog"), false)] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        eval_active(&h.page, &anchored_form(false, "div")).await;
        let inner = "<div style=\"height:100%\"><button onclick=\"window.adClicks++\">Shop now</button></div>";
        let script = format!(
            "{{ document.querySelector('ins').remove(); const host = document.createElement('ad-slot'); \
             host.style.cssText = 'display:block;position:fixed;left:0;right:0;bottom:0;height:120px;\
             background:white;z-index:9'; {}document.body.append(host); \
             host.attachShadow({{mode: 'open'}}).innerHTML = {}; \
             const r = document.getElementById('go').getBoundingClientRect(); \
             document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) === host }}",
            role.map(|r| format!("host.setAttribute('role', {}); ", js(r))).unwrap_or_default(),
            js(inner),
        );
        assert_eq!(eval_active(&h.page, &script).await, json!(true), "{role:?}");
        let obs = h.observe().await;
        let result = h.page.act(&action(Operation::Click, find(&obs, "Login")), &obs).await.expect("act");
        let state = eval_active(&h.page, "[window.pressed, window.adClicks]").await;
        if reached {
            assert_eq!((result.outcome, state), (StepOutcome::Executed, json!([1, 0])), "{:?}", result.detail);
        } else {
            assert_eq!((result.outcome, state), (StepOutcome::Covered, json!([null, 0])), "{:?}", result.detail);
        }
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_control_repurposed_while_scrolled_clear_of_a_band_is_never_pressed_or_typed_into() {
    // Scrolling a control out from under a band runs the page's scroll handlers. One that turned an authorized
    // Login into "Delete account", or a username field into a card number field, would have had the keyboard act on
    // what was never classified or authorized; the pointer path rechecks before every press, and so does this one. A
    // date field's value is set without a key, and that branch once returned before the check could run.
    for control in ["button", "field", "date"] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        eval_active(&h.page, &anchored_form(true, "div")).await;
        // Bands at the top, middle and foot of the window leave no alignment where the control is clear of them.
        assert_eq!(
            eval_active(
                &h.page,
                "{ const foot = document.querySelector('ins'); \
                 for (const top of ['0px', 'calc(50% - 60px)']) { const band = foot.cloneNode(true); \
                 band.style.top = top; band.style.bottom = 'auto'; band.addEventListener('mousedown', () => \
                 window.adClicks++); document.body.append(band); } } \
                 document.getElementById('login').insertAdjacentHTML('beforeend', \
                 '<input type=date name=when aria-label=Departure>'); \
                 window.keys = 0; document.addEventListener('keydown', () => window.keys++, true); \
                 addEventListener('scroll', () => { document.getElementById('go').textContent = 'Delete account'; \
                 document.querySelector('[name=user]').setAttribute('aria-label', 'Card number'); \
                 document.querySelector('[name=when]').setAttribute('aria-label', 'Return date'); }, {once: true}); true"
            )
            .await,
            json!(true),
            "{control}"
        );
        let obs = h.observe().await;
        let act = match control {
            "button" => action(Operation::Click, find(&obs, "Login")),
            "field" => fill(find(&obs, "Username"), "ada"),
            _ => fill(find(&obs, "Departure"), "2026-11-14"),
        };
        let result = h.page.act(&act, &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Stale, "{control}: {:?}", result.detail);
        // Refused by the route around the bands, never by a pointer path they do not let it take: the control changed,
        // or, on a loaded machine, its scroll had not been seen to finish, so nothing was acted on either way.
        assert!(
            matches!(
                result.detail.as_deref(),
                Some("control changed before keyboard use" | "page did not finish scrolling the control clear")
            ),
            "{control}: {:?}",
            result.detail
        );
        assert_eq!(
            eval_active(
                &h.page,
                "[window.keys, window.pressed, ['user', 'when'].map(n => document.querySelector(`[name=${n}]`).value), \
                 window.adClicks]"
            )
            .await,
            json!([0, null, ["", ""], 0]),
            "{control}"
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_list_scrolled_inside_a_shadow_root_is_waited_on_and_not_refused() {
    // A scroll event is not composed, so a list scrolling inside a shadow root never reaches the document. The
    // route counted the list as moved, heard no scroll, waited out its deadline and refused a Login it had exposed;
    // one the list's handler relabels must still be refused.
    for relabel in [false, true] {
        let Some(h) = harness().await else { return };
        h.goto("").await;
        let list_html = "<div id=list style=\"height:400px;overflow:auto\"><div style=\"height:330px\"></div>\
                         <button id=go onclick=\"window.pressed = this.textContent\">Login</button>\
                         <div style=\"height:2000px\"></div></div>";
        let script = format!(
            "{{ window.adClicks = 0; document.body.style.cssText = 'margin:0;height:100vh;overflow:hidden'; \
             document.body.innerHTML = ''; const host = document.createElement('div'); \
             host.style.cssText = 'position:fixed;left:0;right:0;bottom:0;height:400px'; document.body.append(host); \
             const root = host.attachShadow({{mode: 'open'}}); root.innerHTML = {}; \
             const band = document.createElement('ins'); band.style.cssText = 'display:block;position:fixed;\
             left:0;right:0;bottom:0;height:120px;background:white;z-index:9'; \
             band.addEventListener('mousedown', () => window.adClicks++); document.body.append(band); \
             {}(r => document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) === band)\
             (root.getElementById('go').getBoundingClientRect()) }}",
            js(list_html),
            if relabel {
                "root.getElementById('list').addEventListener('scroll', () => { \
                 root.getElementById('go').textContent = 'Delete account'; }, {once: true}); "
            } else {
                ""
            },
        );
        assert_eq!(eval_active(&h.page, &script).await, json!(true), "relabel={relabel}");
        let obs = h.observe().await;
        let result = h.page.act(&action(Operation::Click, find(&obs, "Login")), &obs).await.expect("act");
        let state = eval_active(&h.page, "[window.pressed, window.adClicks]").await;
        if relabel {
            assert_eq!((result.outcome, state), (StepOutcome::Stale, json!([null, 0])), "{:?}", result.detail);
        } else {
            assert_eq!((result.outcome, state), (StepOutcome::Executed, json!(["Login", 0])), "{:?}", result.detail);
        }
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_control_repurposed_when_its_container_scrolls_clear_of_a_band_is_never_acted_on() {
    // A list that scrolls inside the page, not the window, moved the control clear of a band while the window
    // stayed put, and its scroll events never reach the window. The route only waited on the window's, so it went
    // ahead before the list's handler ran; one that settles a carousel on its scrollend swapped an authorized Login
    // for "Delete account". A later frame wait happened to cover for it unless a frame stalled, so the click case pins
    // the route's own contract, and the fill case the whole action.
    for operation in [Operation::Click, Operation::Fill] {
        for event in ["scroll", "scrollend"] {
            let case = format!("operation={operation:?} event={event}");
            let Some(h) = harness().await else { return };
            h.goto("").await;
            let session_id = h.session.active_session_id().expect("active session");
            let script = format!(
                "window.adClicks = 0; window.keys = 0; document.addEventListener('keydown', () => window.keys++, true); \
                 document.body.style.cssText = 'margin:0;height:100vh;overflow:hidden'; \
                 document.body.innerHTML = '<div id=list style=\"position:fixed;left:0;right:0;bottom:0;height:400px;\
                 overflow:auto\"><div style=\"height:330px\"></div><form onsubmit=\"event.preventDefault()\">\
                 <input name=user aria-label=Username> <button id=go type=button \
                 onclick=\"window.pressed = this.textContent\">Login</button></form>\
                 <div style=\"height:2000px\"></div></div>'; \
                 const band = document.createElement('ins'); band.style.cssText = 'display:block;position:fixed;\
                 left:0;right:0;bottom:0;height:120px;background:white;z-index:9'; \
                 band.addEventListener('mousedown', () => window.adClicks++); document.body.append(band); \
                 document.getElementById('list').addEventListener({}, () => {{ \
                 document.getElementById('go').textContent = 'Delete account'; \
                 document.querySelector('[name=user]').setAttribute('aria-label', 'Card number'); }}, {{once: true}}); \
                 (r => document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) === band)\
                 (document.getElementById('go').getBoundingClientRect())",
                js(event)
            );
            assert_eq!(eval_value(&h.session, &session_id, &script).await, json!(true), "{case}");
            let obs = h.observe().await;
            let act = if operation == Operation::Fill {
                fill(find(&obs, "Username"), "ada")
            } else {
                action(operation, find(&obs, "Login"))
            };
            if operation == Operation::Click {
                // The route's own contract, which nothing later in the action stands in for: once it hands back a
                // place to act, every handler the scroll it made owes has run. On 11e07f2 it returned before the
                // list's scroll was delivered at all, leaving a later frame wait with a 100ms fallback to cover
                // for it.
                let (_session, _frame, local_id, guard) =
                    testing::last_target(&h.page, &find(&obs, "Login").id).expect("page._last");
                let out = h
                    .session
                    .send(
                        "Runtime.evaluate",
                        json!({
                            "expression": format!(
                                "(async () => {{ await ({})({local_id}, {}); \
                                 return document.getElementById('go').textContent; }})()",
                                *UNCOVER_JS,
                                serde_json::to_string(&guard).unwrap()
                            ),
                            "awaitPromise": true,
                            "returnByValue": true,
                        }),
                        Some(&session_id),
                    )
                    .await
                    .expect("Runtime.evaluate");
                assert_eq!(out["result"]["value"], json!("Delete account"), "{case}");
                h.close().await;
                continue;
            }
            let result = h.page.act(&act, &obs).await.expect("act");
            assert_eq!(result.outcome, StepOutcome::Stale, "{case}: {:?}", result.detail);
            assert_eq!(
                eval_value(
                    &h.session,
                    &session_id,
                    "[window.pressed, document.querySelector('[name=user]').value, window.keys, window.adClicks]"
                )
                .await,
                json!([null, "", 0, 0]),
                "{case}"
            );
            h.close().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nothing_is_sent_through_an_obstruction_that_focusing_the_control_opened() {
    // Focusing a control can open a menu or a dialog over it while the control itself stays as it was, so its
    // guard still holds. Keys go to the focused control whatever is drawn over it, and the cover was judged only
    // before focus: Login was pressed through the menu its focus had opened. A field clicked with the pointer, its
    // focus opening a dialog over it, was typed into behind the dialog the same way.
    let cases = [
        (Operation::Click, "Login", "menu", true),
        (Operation::Enter, "Username", "dialog", true),
        (Operation::Fill, "Username", "dialog", true),
        (Operation::Fill, "Username", "dialog", false),
    ];
    for (operation, label, role, band) in cases {
        let case = format!("{operation:?} {label} {role} band={band}");
        let Some(h) = harness().await else { return };
        h.goto("").await;
        eval_active(&h.page, &anchored_form(false, "div")).await;
        let script = format!(
            "{}window.keys = 0; document.addEventListener('keydown', () => window.keys++, true); \
             const e = document.querySelector({}); \
             e.addEventListener('focus', () => {{ const r = e.getBoundingClientRect(), o = \
             document.createElement('div'); o.setAttribute('role', {}); o.textContent = 'Opened on focus'; \
             o.style.cssText = `position:absolute;z-index:99;background:white;left:${{scrollX + r.left - 10}}px;\
             top:${{scrollY + r.top - 10}}px;width:${{r.width + 20}}px;height:${{r.height + 20}}px`; \
             document.body.append(o); }}, {{once: true}}); true",
            if band { "" } else { "document.querySelector('ins').remove(); " },
            js(if label == "Login" { "#go" } else { "[name=user]" }),
            js(role),
        );
        assert_eq!(eval_active(&h.page, &script).await, json!(true), "{case}");
        let obs = h.observe().await;
        let target = find(&obs, label);
        let act = if operation == Operation::Fill { fill(target, "ada") } else { action(operation, target) };
        let result = h.page.act(&act, &obs).await.expect("act");
        assert_eq!(result.outcome, StepOutcome::Covered, "{case}: {:?}", result.detail);
        assert!(result.detail.as_deref().unwrap_or_default().contains(role), "{case}: {:?}", result.detail);
        assert_eq!(
            eval_active(
                &h.page,
                "[window.keys, window.pressed, window.submitted, document.querySelector('[name=user]').value]"
            )
            .await,
            json!([0, null, null, ""]),
            "{case}"
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_full_window_frame_over_the_control_is_reported_with_the_frames_own_controls() {
    // A full-window ad frame (an interstitial) covered a login form. Its close button lives in its own,
    // cross-origin document, so the report named no controls, and recovery closed another ad while this one stayed.
    // Nothing is pressed through a cover the size of the window; the frame's controls are named so it can be closed.
    let Some(h) = harness().await else { return };
    h.goto("").await;
    eval_active(&h.page, &anchored_form(false, "div")).await;
    let script = format!(
        "{{ const shown = document.querySelector('ins'); shown.style.cssText += ';top:0;height:auto'; \
         shown.innerHTML = '<iframe aria-label=Advertisement src=\"{}/interstitial.html\" \
         style=\"width:100%;height:100%;border:0\"></iframe>'; }} true",
        sites().iframe
    );
    assert_eq!(eval_active(&h.page, &script).await, json!(true));
    let obs = observe_until(&h.page, "Close ad").await;
    let result = h.page.act(&action(Operation::Click, find(&obs, "Login")), &obs).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Covered);
    let detail = result.detail.unwrap_or_default();
    assert!(
        detail.contains("Advertisement") && detail.contains("Close ad") && detail.contains("spring offer"),
        "{detail}"
    );
    assert_eq!(eval_active(&h.page, "[window.pressed, window.submitted]").await, json!([null, null]));
    h.close().await;
}
