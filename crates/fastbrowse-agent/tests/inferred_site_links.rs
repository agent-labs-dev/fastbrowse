//! An agent-selected opening page still defines the site a run opened for its task.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::telemetry::Ledger;

use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, LLMPurpose, Limits, Operation, Status};
use fastbrowse_core::page::Control;
use indexmap::IndexMap;
use serde_json::{Value, json};

#[path = "common/form_page.rs"]
mod form_page;
use form_page::FormPage;

struct ChooseOpening;

impl LLMClient for ChooseOpening {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        if purpose != LLMPurpose::Start {
            return ScriptedLLM.generate_value(purpose, messages, schema, ledger);
        }
        Box::pin(async move {
            let data = schema.validate_json(r#"{"url":"https://preview.pages.dev/"}"#).map_err(LLMError::schema)?;
            Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)) })
        })
    }
}

struct ObserveQuestions {
    inner: ScriptedJev,
    states: Arc<Mutex<Vec<Value>>>,
}

impl DecisionClient for ObserveQuestions {
    fn profile(&self) -> DecisionProfile {
        self.inner.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        if questions.contains_key("operation") {
            self.states.lock().unwrap().push(state.clone());
        }
        self.inner.evaluate(state, questions)
    }
}

#[tokio::test]
async fn an_agent_selected_opening_site_marks_its_external_links() {
    let external = Control {
        href: Some("https://store.example.com/item".into()),
        ..Control::new("store", "link", "Store", [Operation::Click])
    };
    let page = serde_json::from_value(json!({
        "url": "https://preview.pages.dev/", "title": "Preview", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [external], "viewport_text": "Preview", "tabs": [],
    }))
    .unwrap();
    let states = Arc::new(Mutex::new(Vec::new()));
    let jev = ObserveQuestions { inner: ScriptedJev::new(steps("wait").unwrap()), states: states.clone() };
    let agent = Agent::new(
        Arc::new(FormPage { page: Mutex::new(page), confirms_post: false }),
        Arc::new(jev),
        Arc::new(ChooseOpening),
    );
    let result = agent
        .run(
            "Check the preview.pages.dev page",
            RunOptions {
                choose_start: true,
                limits: Some(Limits { max_steps: Some(1), ..Limits::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.status, Status::BudgetExceeded, "{:?}", result.error);
    let states = states.lock().unwrap();
    assert!(!states.is_empty(), "the run asked for an action after opening the proposed page");
    assert_eq!(
        states[0]["elements"][0]["other_site"],
        json!(true),
        "the browser opened the preview for the task, so Store must be marked as leaving it"
    );
}
