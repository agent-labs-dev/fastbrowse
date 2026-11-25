//! A refusal retains who chose the action; Jev classifies its authorization only.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedDriver, ScriptedJev, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Authorization, Decider, Limits, Operation, Status, StepOutcome};
use fastbrowse_core::page::Control;
use indexmap::IndexMap;
use serde_json::{Value, json};

#[path = "common/form_page.rs"]
mod form_page;
use form_page::FormPage;

struct RequestedPost(ScriptedJev);

impl DecisionClient for RequestedPost {
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
            if let Some(Question::Noul(question)) = questions.get("irreversible") {
                let posting =
                    question.instructions.contains("Post") || question.instructions.contains("Publish article");
                if let Some(Answer::Noul(irreversible)) = answer.answers.get_mut("irreversible") {
                    irreversible.probability = if posting { 0.9 } else { 0.0 };
                }
            }
            if let Some(Answer::Noul(requested)) = answer.answers.get_mut("requested") {
                requested.probability = 0.9;
            }
            Ok(answer)
        })
    }
}

#[tokio::test]
async fn a_refused_action_keeps_the_driver_as_its_decider() {
    let post = Control::new("post", "button", "Post", [Operation::Click]);
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Reply", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [post], "viewport_text": "", "tabs": [],
    }))
    .unwrap();
    let script = steps("click Post").unwrap();
    let page = Arc::new(FormPage { page: Mutex::new(page), confirms_post: false });
    let agent = Agent::new(
        page.clone(),
        Arc::new(RequestedPost(ScriptedJev::new(script.clone()))),
        Arc::new(ScriptedDriver::new(script)),
    );
    let result = agent
        .run(
            "Post the comment",
            RunOptions {
                start: Some("https://example.test/".into()),
                authorization: Some(Authorization { irreversible_actions: false }),
                limits: Some(Limits { max_steps: Some(3), ..Limits::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.status, Status::NeedsConfirmation, "{result:?}");
    assert_eq!(page.page.lock().unwrap().viewport_text, "", "a refused action must not dispatch");
    assert_eq!(result.steps.len(), 1);
    assert_eq!(result.steps[0].outcome, StepOutcome::Refused);
    assert_eq!(
        result.steps[0].decided_by,
        Decider::Llm,
        "the driver chose Post; the authorization classifier must not replace its attribution"
    );
}
