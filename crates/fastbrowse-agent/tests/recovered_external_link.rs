//! A link's agent-owned site marker does not invalidate a recovery decision.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Limits, Operation, StepOutcome};
use fastbrowse_core::page::Control;
use indexmap::IndexMap;
use serde_json::{Value, json};

#[path = "common/form_page.rs"]
mod form_page;
use form_page::FormPage;

struct RefusePost(ScriptedJev);

impl DecisionClient for RefusePost {
    fn profile(&self) -> DecisionProfile {
        self.0.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            let mut answer = self.0.evaluate(state, questions).await?;
            for key in ["prohibits", "forbidden"] {
                if let Some(Answer::Noul(value)) = answer.answers.get_mut(key) {
                    value.probability = 1.0;
                }
            }
            Ok(answer)
        })
    }
}

#[tokio::test]
async fn recovered_external_link_survives_an_unchanged_page() {
    let external = Control {
        href: Some("https://store.example.com/item".into()),
        ..Control::new("draft", "link", "Store", [Operation::Hover])
    };
    let post = Control::new("post", "button", "Post", [Operation::Click]);
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Links", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [external, post], "viewport_text": "Links", "tabs": [],
    }))
    .unwrap();
    let agent = Agent::new(
        Arc::new(FormPage { page: Mutex::new(page), confirms_post: false }),
        Arc::new(RefusePost(ScriptedJev::new(steps("click Post; hover Store; wait").unwrap()))),
        Arc::new(ScriptedLLM),
    );
    let result = agent
        .run(
            "Hover Store without clicking Post",
            RunOptions {
                start: Some("https://example.test/".into()),
                limits: Some(Limits { max_steps: Some(2), ..Limits::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.steps.len(), 2, "{:?}", result.error);
    assert_eq!(result.steps[0].outcome, StepOutcome::Failed);
    assert_eq!(
        result.steps[1].operation,
        Operation::Hover,
        "the unchanged external link must retain Jev's recovery pick; steps: {:?}",
        result.steps,
    );
}
