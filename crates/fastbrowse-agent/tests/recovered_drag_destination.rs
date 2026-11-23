//! A recovered drag is invalidated when its drop target changes before the next observation.

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

struct RefusePost(ScriptedJev, Arc<FormPage>);

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
            if questions.contains_key("drag_destination") {
                let mut page = self.1.page.lock().unwrap();
                page.controls[0].label = "Trash".into();
                page.page_key = "drop-repurposed".into();
            }
            Ok(answer)
        })
    }
}

#[tokio::test]
async fn a_recovered_drag_does_not_use_a_repurposed_drop_target() {
    // The first control is the default scripted drop choice. Its id survives a same-document redraw.
    let destination = Control::new("drop", "button", "Archive", [Operation::Click]);
    let source = Control::new("draft", "button", "Card", [Operation::Drag]);
    let post = Control::new("post", "button", "Post", [Operation::Click]);
    let observation = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Cards", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [destination, source, post],
        "viewport_text": "Cards", "tabs": [],
    }))
    .unwrap();
    let page = Arc::new(FormPage { page: Mutex::new(observation), confirms_post: false });
    let agent = Agent::new(
        page.clone(),
        Arc::new(RefusePost(ScriptedJev::new(steps("click Post; drag Card; wait").unwrap()), page)),
        Arc::new(ScriptedLLM),
    );
    let result = agent
        .run(
            "Drag Card to Archive without clicking Post",
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
        Operation::Wait,
        "a changed drop target must cause a fresh decision before any drag; steps: {:?}",
        result.steps,
    );
}
