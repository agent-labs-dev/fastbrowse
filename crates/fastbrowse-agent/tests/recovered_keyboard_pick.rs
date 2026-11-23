//! Recovery uses the policy's derived keyboard operations, not just the raw browser list.

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
                    value.probability = if key == "prohibits" {
                        1.0
                    } else if let Some(Question::Noul(question)) = questions.get(key) {
                        f64::from(u8::from(question.instructions.contains("Post")))
                    } else {
                        0.0
                    };
                }
            }
            Ok(answer)
        })
    }
}

async fn recovered_keyboard_pick(operation: Operation) {
    let field = Control { focused: Some(true), ..Control::new("draft", "spinbutton", "Quantity", [Operation::Fill]) };
    let post = Control::new("post", "button", "Post", [Operation::Click]);
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Form", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [field, post], "viewport_text": "Form", "tabs": [],
    }))
    .unwrap();
    let agent = Agent::new(
        Arc::new(FormPage { page: Mutex::new(page), confirms_post: false }),
        Arc::new(RefusePost(ScriptedJev::new(steps(&format!("click Post; {operation} Quantity; wait")).unwrap()))),
        Arc::new(ScriptedLLM),
    );
    let result = agent
        .run(
            "Leave Quantity or press ArrowUp without clicking Post",
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
        result.steps[1].operation, operation,
        "the focused field still supports the recovered keyboard operation; steps: {:?}",
        result.steps,
    );
}

#[tokio::test]
async fn recovered_blur_survives_an_unchanged_focused_field() {
    recovered_keyboard_pick(Operation::Blur).await;
}

#[tokio::test]
async fn recovered_press_survives_an_unchanged_editable_field() {
    recovered_keyboard_pick(Operation::Press).await;
}
