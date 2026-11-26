//! An observation and a capture as models and traces see them: every string the page wrote, masked
//! (`Redactor::mask`).
//!
//! Each struct is taken apart without `..`, so a field added later does not compile until it is masked or named as
//! code-made here: field-by-field masking at the call site left an observation's history entry, its media titles and
//! a control's validation message reaching every model unmasked (#1381). Masking keeps lengths, so every offset stays
//! valid. A code-made field (an id, a fingerprint, a kind the browser layer names) is kept: it holds no page text,
//! and masking a short secret inside `e12` would point an action at no control.

use fastbrowse_core::models::{Held, Link, SourceControl};
use fastbrowse_core::page::{Block, Capture, Control, Dialog, Media, Observation, SourceField, Tab};
use fastbrowse_core::py::sha256_hex;
use fastbrowse_core::safety::Redactor;

/// The observation's code-made names, kept whole by the run's model boundary (`Redactor::keep`) as they are here: the
/// boundary masks every string a request holds, and a password `e1` or `p` masked inside a control's id there left
/// Jev picking an option no control has.
pub(super) fn keep_code_made(redactor: &mut Redactor, observation: &Observation) {
    let mut kept = vec![&observation.page_key, &observation.document_key, &observation.viewport_key];
    for control in &observation.controls {
        kept.push(&control.id);
        kept.extend(
            [&control.frame_id, &control.frame_document_key, &control.form_id, &control.input_type]
                .into_iter()
                .flatten(),
        );
    }
    for tab in &observation.tabs {
        kept.push(&tab.id);
        kept.extend(&tab.opener_id);
    }
    kept.extend(observation.media.iter().map(|media| &media.kind));
    kept.extend(observation.dialog.iter().map(|dialog| &dialog.kind));
    kept.into_iter().for_each(|id| redactor.keep(id));
}

/// A capture's code-made names, kept as an observation's are (`keep_code_made`).
pub(super) fn keep_capture_ids(redactor: &mut Redactor, capture: &Capture) {
    for block in &capture.blocks {
        redactor.keep(&block.source_id);
        block.frame_id.iter().for_each(|id| redactor.keep(id));
    }
}

pub(super) fn observation(redactor: &Redactor, observation: &Observation) -> Observation {
    let mask = |value: &str| redactor.mask(value);
    let mask_opt = |value: &Option<String>| value.as_deref().map(mask);
    let Observation {
        url,
        title,
        page_key,
        captured_at,
        response_status,
        document_key,
        viewport_key,
        controls,
        viewport_text,
        offscreen_text,
        visual_content,
        modal,
        selection,
        media,
        tabs,
        dialog,
        inaccessible_frames,
        can_go_back,
        history_entry,
    } = observation;
    Observation {
        url: mask(url),
        title: mask(title),
        page_key: page_key.clone(),
        captured_at: *captured_at,
        response_status: *response_status,
        document_key: document_key.clone(),
        // A fingerprint of the place the viewport shows, as code's own keys above.
        viewport_key: viewport_key.clone(),
        controls: controls.iter().map(|item| self::control(redactor, item)).collect(),
        viewport_text: mask(viewport_text),
        offscreen_text: mask(offscreen_text),
        visual_content: *visual_content,
        // A page can name its modal with what it echoes back, a resolved secret among it.
        modal: mask_opt(modal),
        // Selected text is page text, a typed secret among it.
        selection: mask_opt(selection),
        media: media.iter().map(|item| self::media(redactor, item)).collect(),
        tabs: tabs.iter().map(|item| self::tab(redactor, item)).collect(),
        dialog: dialog.as_ref().map(|item| self::dialog(redactor, item)),
        inaccessible_frames: *inaccessible_frames,
        can_go_back: *can_go_back,
        history_entry: mask_opt(history_entry),
    }
}

fn control(redactor: &Redactor, control: &Control) -> Control {
    let mask = |value: &str| redactor.mask(value);
    let mask_opt = |value: &Option<String>| value.as_deref().map(mask);
    let Control {
        id,
        frame_id,
        frame_origin,
        frame_document_key,
        form_id,
        retarget_key,
        role,
        label,
        context,
        operations,
        value,
        focused,
        href,
        options,
        input_type,
        input_name,
        max_length,
        autocomplete,
        submit_semantics,
        checked,
        selected,
        expanded,
        sensitive,
        offscreen,
        disabled,
        required,
        read_only,
        layer,
        invalid,
        validation_message,
        next_page,
        download,
        drawn,
        pictured,
        already_read,
        role_description,
        description,
        item_position,
        item_count,
        other_site,
    } = control;
    Control {
        id: id.clone(),
        frame_id: frame_id.clone(),
        frame_origin: mask_opt(frame_origin),
        // A document key, as code's own, so an action still matches it after a reload.
        frame_document_key: frame_document_key.clone(),
        form_id: form_id.clone(),
        // Never written out, so no model or trace reads it, and acting matches it against the live page.
        retarget_key: retarget_key.clone(),
        // Hover controls can carry a page-defined role that echoes a credential.
        role: mask(role),
        label: mask(label),
        context: mask_opt(context),
        operations: operations.clone(),
        value: mask_opt(value),
        focused: *focused,
        href: mask_opt(href),
        options: options.iter().map(|option| mask(option)).collect(),
        input_type: input_type.clone(),
        input_name: mask_opt(input_name),
        max_length: *max_length,
        autocomplete: mask_opt(autocomplete),
        submit_semantics: mask_opt(submit_semantics),
        checked: *checked,
        selected: *selected,
        expanded: *expanded,
        sensitive: *sensitive,
        offscreen: *offscreen,
        disabled: *disabled,
        required: *required,
        read_only: *read_only,
        layer: *layer,
        invalid: *invalid,
        validation_message: mask_opt(validation_message),
        next_page: *next_page,
        download: *download,
        drawn: *drawn,
        pictured: *pictured,
        already_read: *already_read,
        role_description: mask_opt(role_description),
        description: mask_opt(description),
        item_position: *item_position,
        item_count: *item_count,
        other_site: *other_site,
    }
}

fn media(redactor: &Redactor, media: &Media) -> Media {
    let Media { kind, label, heading, paused, ended, muted, elapsed_seconds, duration_seconds } = media;
    Media {
        kind: kind.clone(),
        label: label.as_deref().map(|value| redactor.mask(value)),
        heading: heading.as_deref().map(|value| redactor.mask(value)),
        paused: *paused,
        ended: *ended,
        muted: *muted,
        elapsed_seconds: *elapsed_seconds,
        duration_seconds: *duration_seconds,
    }
}

fn tab(redactor: &Redactor, tab: &Tab) -> Tab {
    let Tab { id, url, title, active, opener_id } = tab;
    Tab {
        id: id.clone(),
        url: redactor.mask(url),
        title: redactor.mask(title),
        active: *active,
        opener_id: opener_id.clone(),
    }
}

fn dialog(redactor: &Redactor, dialog: &Dialog) -> Dialog {
    let Dialog { kind, message, default_prompt } = dialog;
    Dialog {
        kind: kind.clone(),
        message: redactor.mask(message),
        default_prompt: default_prompt.as_deref().map(|value| redactor.mask(value)),
    }
}

/// The capture's text masked, with its masked spans counted in the raw text and its digest taken over what is shown.
pub(super) fn capture(redactor: &Redactor, capture: &Capture) -> Capture {
    let Capture { url, title, captured_at, sha256: _, text, blocks, inaccessible_frames, masked: _ } = capture;
    let shown = redactor.mask(text);
    Capture {
        url: redactor.mask(url),
        title: redactor.mask(title),
        captured_at: *captured_at,
        sha256: sha256_hex(shown.as_bytes()),
        text: shown,
        blocks: blocks.iter().map(|item| block(redactor, item)).collect(),
        inaccessible_frames: *inaccessible_frames,
        masked: redactor.masked_spans(text),
    }
}

fn block(redactor: &Redactor, block: &Block) -> Block {
    let mask = |value: &str| redactor.mask(value);
    let Block {
        source_id,
        source_url,
        complete_source,
        kind,
        frame_id,
        start,
        end,
        heading_path,
        control_context,
        href,
        links,
        whole_record,
        item_place,
        fields,
    } = block;
    Block {
        source_id: source_id.clone(),
        source_url: source_url.as_deref().map(mask),
        complete_source: *complete_source,
        kind: *kind,
        frame_id: frame_id.clone(),
        start: *start,
        end: *end,
        heading_path: heading_path.iter().map(|heading| mask(heading)).collect(),
        control_context: control_context.as_ref().map(|control| SourceControl {
            role: mask(&control.role),
            label: mask(&control.label),
            context: control.context.as_deref().map(mask),
            document: control.document.clone(),
            held: control
                .held
                .iter()
                .map(|held| Held { element: held.element.clone(), value: mask(&held.value).into() })
                .collect(),
        }),
        href: href.as_deref().map(mask),
        // An address can carry a token in its query, and a link's text is page text like the rest.
        links: links
            .iter()
            .map(|Link { text, start, end, href }| Link {
                text: mask(text),
                start: *start,
                end: *end,
                href: mask(href),
            })
            .collect(),
        whole_record: *whole_record,
        item_place: *item_place,
        fields: fields
            .iter()
            .map(|SourceField { path, start, end }| SourceField {
                path: path.iter().map(|part| mask(part)).collect(),
                start: *start,
                end: *end,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const CANARY: &str = "Canary7pw";

    fn leaves(value: &Value, path: String, out: &mut Vec<(String, String)>) {
        match value {
            Value::String(text) => out.push((path, text.clone())),
            Value::Array(items) => {
                items.iter().enumerate().for_each(|(at, item)| leaves(item, format!("{path}[{at}]"), out));
            }
            Value::Object(fields) => fields.iter().for_each(|(key, item)| leaves(item, format!("{path}.{key}"), out)),
            _ => {}
        }
    }

    /// `value` with the string at `path` replaced.
    fn written(value: &Value, path: &str, text: &str) -> Value {
        let mut value = value.clone();
        let mut at = &mut value;
        for part in path.trim_start_matches('.').split('.').flat_map(|part| part.split('[')) {
            at = match part.strip_suffix(']') {
                Some(index) => &mut at[index.parse::<usize>().unwrap()],
                None => &mut at[part],
            };
        }
        *at = Value::String(text.into());
        value
    }

    /// Each string field of an observation, written with a registered credential, reaches a model masked unless the
    /// browser layer makes it: the history entry, media titles and a validation message went out unmasked (#1381).
    #[test]
    fn test_every_string_an_observation_shows_a_model_is_masked_but_its_code_made_keys() {
        let control = json!({"id": "e1", "frame_id": "f", "frame_origin": "https://shop.test", "form_id": "g",
                "role": "textbox", "label": "l", "context": "c", "operations": ["fill"], "value": "v",
                "focused": false, "href": "https://shop.test/a", "options": ["o"], "input_type": "text",
                "input_name": "n", "max_length": 9, "autocomplete": "a", "submit_semantics": "s", "checked": false,
                "selected": false, "expanded": false, "sensitive": false, "offscreen": false, "required": false,
                "invalid": true, "validation_message": "vm", "next_page": false, "download": false, "drawn": false,
                "pictured": false, "already_read": false, "read_only": true, "layer": 1});
        let mut full = json!({
            "url": "https://shop.test/", "title": "t", "page_key": "k", "captured_at": "2026-01-01T00:00:00Z",
            "response_status": 200, "document_key": "d", "viewport_text": "v", "offscreen_text": "o",
            "visual_content": false, "modal": "m", "inaccessible_frames": 0, "can_go_back": false,
            "history_entry": "h",
            "media": [{"kind": "video", "label": "l", "heading": "h", "paused": true, "ended": false, "muted": false,
                "elapsed_seconds": 0.0, "duration_seconds": 1.0}],
            "tabs": [{"id": "t1", "url": "https://shop.test/", "title": "t", "active": true, "opener_id": "t0"}],
            "dialog": {"kind": "prompt", "message": "m", "default_prompt": "p"},
        });

        full["controls"] = json!([control]);
        serde_json::from_value::<Observation>(full.clone()).expect("the sample is an observation");
        let mut redactor = Redactor::new();
        redactor.register_address(&format!("https://ops:{CANARY}@shop.test/"));
        let mut paths = Vec::new();
        leaves(&full, String::new(), &mut paths);
        let mut kept = Vec::new();
        for (path, original) in paths {
            let canaried = written(&full, &path, &format!("{original} {CANARY}"));
            // A field that takes only the browser layer's words (a kind, an operation, a date) cannot hold it.
            let Ok(observation) = serde_json::from_value::<Observation>(canaried) else { continue };
            let shown = serde_json::to_string(&super::observation(&redactor, &observation)).unwrap();
            if shown.contains(CANARY) {
                kept.push(path);
            }
        }
        let code_made = [
            ".page_key",
            ".document_key",
            ".media[0].kind",
            ".tabs[0].id",
            ".tabs[0].opener_id",
            ".dialog.kind",
            ".controls[0].id",
            ".controls[0].frame_id",
            ".controls[0].form_id",
            ".controls[0].input_type",
        ];
        assert_eq!(kept, code_made);
    }
}
