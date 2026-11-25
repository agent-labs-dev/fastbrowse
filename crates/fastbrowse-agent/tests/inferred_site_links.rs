//! An agent-selected opening page still defines the site a run opened for its task.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedDriver, ScriptedJev, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::telemetry::Ledger;

use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, LLMPurpose, Limits, Operation, Status};
use fastbrowse_core::page::Control;
use serde_json::{Value, json};

#[path = "common/form_page.rs"]
mod form_page;
use form_page::FormPage;

/// Proposes the preview as the opening page, and keeps each page the driver is shown.
struct ChooseOpening {
    driver: ScriptedDriver,
    shown: Mutex<Vec<String>>,
}

impl LLMClient for ChooseOpening {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        if purpose == LLMPurpose::Action
            && let Some(last) = messages.last()
        {
            self.shown.lock().unwrap().push(last.content.clone());
        }
        if purpose != LLMPurpose::Start {
            return self.driver.generate_value(purpose, messages, schema, ledger);
        }
        Box::pin(async move {
            let data = schema.validate_json(r#"{"url":"https://preview.pages.dev/"}"#).map_err(LLMError::schema)?;
            Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)) })
        })
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
    let llm =
        Arc::new(ChooseOpening { driver: ScriptedDriver::new(steps("wait").unwrap()), shown: Mutex::new(Vec::new()) });
    let agent = Agent::new(
        Arc::new(FormPage { page: Mutex::new(page), confirms_post: false }),
        Arc::new(ScriptedJev::new(steps("wait").unwrap())),
        llm.clone(),
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
    let shown = llm.shown.lock().unwrap();
    let first = shown.first().expect("the run asked for an action after opening the proposed page");
    let store = first.lines().find(|line| line.contains("\"Store\"")).expect("Store is listed");
    assert!(
        store.contains("leads to another site"),
        "the browser opened the preview for the task, so Store must be marked as leaving it: {store}"
    );
}
