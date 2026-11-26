//! Returning to a page state must make that state the source of a text read.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{Artifact, CostBasis, CostComponent, CostLine, LLMPurpose, Limits, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use fastbrowse_core::telemetry::Ledger;
use serde_json::{Value, json};

#[derive(Default)]
struct TogglePage(AtomicBool);

impl TogglePage {
    fn text(&self) -> &'static str {
        if self.0.load(Ordering::SeqCst) { "State B" } else { "State A" }
    }
}

impl Page for TogglePage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let text = self.text();
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Options", "page_key": text,
                "document_key": "doc", "captured_at": "2026-11-22T00:00:00Z",
                "viewport_text": text, "tabs": [],
                "controls": [{"id": "toggle", "frame_id": null, "role": "button", "label": "Toggle",
                              "operations": ["click"]}]
            }))
            .unwrap())
        })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        let text = self.text();
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Options", "captured_at": "2026-11-22T00:00:00Z",
                "sha256": fastbrowse_core::py::sha256_hex(text.as_bytes()), "text": text,
                "blocks": [{"source_id": "p", "kind": "paragraph", "frame_id": null,
                            "start": 0, "end": text.len()}]
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        self.0.fetch_xor(true, Ordering::SeqCst);
        Box::pin(async {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                page_changed: true,
                detail: None,
                moved: false,
                landed: None,
            })
        })
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn withhold_frames(&self, _: bool) {}
    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async { Ok("https://example.test/".into()) })
    }
    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(Some(200)) })
    }
    fn lifecycle_mark(&self) -> u64 {
        0
    }
    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
}

struct Reader {
    turn: AtomicUsize,
    named_page: bool,
    result: Mutex<String>,
}

impl LLMClient for Reader {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        if purpose != LLMPurpose::Action {
            return ScriptedLLM.generate_value(purpose, messages, schema, ledger);
        }
        let turn = self.turn.fetch_add(1, Ordering::SeqCst);
        let action = match turn {
            0 | 1 => json!({"tool": "click", "id": "1"}),
            2 => {
                json!({"tool": "read_text", "page": if self.named_page { Some("https://example.test/") } else { None },
                        "from": 1, "lines": 1, "find": null})
            }
            _ => {
                *self.result.lock().unwrap() = messages.last().unwrap().content.clone();
                json!({"tool": "give_up", "status": "stuck", "reason": "test over"})
            }
        };
        Box::pin(async move {
            Ok(Generation {
                data: schema
                    .validate_json(&json!({"next": "", "actions": [action]}).to_string())
                    .map_err(LLMError::schema)?,
                cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)),
            })
        })
    }
}

async fn check_revisited_state(named_page: bool) {
    let reader = Arc::new(Reader { turn: AtomicUsize::new(0), named_page, result: Mutex::new(String::new()) });
    Agent::new(Arc::new(TogglePage::default()), Arc::new(ScriptedJev::new(Vec::new())), reader.clone())
        .run(
            "Read the options after toggling twice.",
            RunOptions {
                start: Some("https://example.test/".into()),
                limits: Some(Limits { max_steps: Some(10), ..Default::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let shown = reader.result.lock().unwrap();
    assert!(
        shown.contains("read_text shows lines 1 to 1 of 1 of https://example.test/:\nState A"),
        "After A -> B -> A, read_text must read A, the latest shown state: {shown}"
    );
}

#[tokio::test]
async fn the_current_page_read_uses_its_revisited_state() {
    check_revisited_state(false).await;
}

#[tokio::test]
async fn a_page_read_by_address_uses_its_revisited_state() {
    check_revisited_state(true).await;
}
