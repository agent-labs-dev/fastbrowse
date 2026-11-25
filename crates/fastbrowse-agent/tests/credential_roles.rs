//! Page-provided roles must not become exemptions from credential masking.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{Artifact, LLMPurpose, Operation};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Observation, Page, PageResult};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

const PASSWORD: &str = "RoleCanary9";
const ADDRESS: &str = "https://shop.test/";

struct HoverPage;

impl Page for HoverPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }
    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async {
            let mut observation: Observation = serde_json::from_value(json!({
                "url": ADDRESS, "title": "Orders", "page_key": "page-1",
                "captured_at": "2026-01-01T00:00:00Z", "controls": [],
                "viewport_text": "Orders", "inaccessible_frames": 0, "tabs": []
            }))
            .unwrap();
            // snapshot.js offerHover copies an arbitrary role attribute verbatim.
            observation.controls.push(Control::new("e1", PASSWORD, "Order details", [Operation::Hover]));
            Ok(observation)
        })
    }
    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        // The driver reads the page's captured text with its controls.
        Box::pin(async {
            Ok(serde_json::from_value(json!({
                "url": ADDRESS, "title": "Orders", "captured_at": "2026-01-01T00:00:00Z", "sha256": "", "text": "Orders",
                "blocks": [{"source_id": "s0", "kind": "paragraph", "frame_id": null, "start": 0, "end": 6}]
            }))
            .unwrap())
        })
    }
    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async { Err(BrowserError::failed("test", "no action needed")) })
    }
    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Err(BrowserError::ScreenshotsUnavailable("no image needed".into())) })
    }
    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn withhold_frames(&self, _: bool) {}
    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async { Ok(ADDRESS.into()) })
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

struct RecordingJev {
    sent: Arc<Mutex<Vec<String>>>,
    scripted: ScriptedJev,
}

impl DecisionClient for RecordingJev {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.scripted.profile()
    }
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.sent.lock().unwrap().push(format!("{state}{}", serde_json::to_string(questions).unwrap()));
        self.scripted.evaluate(state, questions)
    }
}

/// The driver's requests, recorded, answered as `ScriptedLLM` answers.
struct RecordingLLM(Arc<Mutex<Vec<String>>>);

impl LLMClient for RecordingLLM {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        self.0.lock().unwrap().push(format!("{purpose:?} {messages:?}"));
        ScriptedLLM.generate_value(purpose, messages, schema, ledger)
    }
}

#[tokio::test]
async fn test_a_page_chosen_hover_role_cannot_exempt_a_password_from_the_model_boundary() {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let jev = Arc::new(RecordingJev { sent: sent.clone(), scripted: ScriptedJev::new(Vec::new()) });
    // The driver is shown the page, and Jev the checks on it: neither receives the password.
    let agent = Agent::new(Arc::new(HoverPage), jev, Arc::new(RecordingLLM(sent.clone())));
    let options = RunOptions { start: Some(format!("https://ops:{PASSWORD}@shop.test/")), ..Default::default() };
    let _ = agent.run("Inspect the order details", options).await;
    let sent = sent.lock().unwrap();
    assert!(sent.iter().any(|request| request.contains("Order details")), "no page was sent");
    assert!(sent.iter().all(|request| !request.contains(PASSWORD)), "the provider received the password as a role");
}
