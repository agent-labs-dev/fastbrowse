//! A notice raised by the latest post must not be attributed to an older post.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedDriver, ScriptedJev, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Artifact, Authorization, Limits, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct Gallery(Mutex<Observation>);

impl Page for Gallery {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }
    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let page = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(page) })
    }
    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Gallery", "captured_at": "2026-11-23T00:00:00Z",
                "sha256": "empty", "text": "", "blocks": [],
            }))
            .unwrap())
        })
    }
    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        let mut page = self.0.lock().unwrap();
        match action.target_id.as_deref() {
            Some("first" | "second" | "third") => page.viewport_text = "Your reply was posted.".into(),
            Some("clear") => page.viewport_text.clear(),
            other => panic!("unexpected action: {other:?}"),
        }
        page.page_key.push('x');
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
        Box::pin(async { Err(BrowserError::ScreenshotsUnavailable("scripted page".into())) })
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

struct Questions {
    script: ScriptedJev,
    checks: Arc<Mutex<Vec<Value>>>,
}

impl DecisionClient for Questions {
    fn profile(&self) -> DecisionProfile {
        self.script.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if questions.contains_key("carried")
                || (questions.contains_key("met")
                    && state["recent_actions"]
                        .as_array()
                        .and_then(|actions| actions.last())
                        .is_some_and(|last| last["target"] == "Post (Grace)"))
            {
                self.checks.lock().unwrap().push(state.clone());
            }
            let mut evaluation = self.script.evaluate(state, questions).await?;
            if let Some(Question::Noul(question)) = questions.get("irreversible") {
                let posted = question.instructions.contains("Post");
                if let Some(fastbrowse_core::decision::Answer::Noul(answer)) =
                    evaluation.answers.get_mut("irreversible")
                {
                    answer.probability = if posted { 1.0 } else { 0.0 };
                }
            }
            if let Some(fastbrowse_core::decision::Answer::Noul(answer)) = evaluation.answers.get_mut("requested") {
                answer.probability = 1.0;
            }
            for (key, choice) in [("pick", "carried_out"), ("evidence", "page_text")] {
                if let Some(fastbrowse_core::decision::Answer::Choice(answer)) = evaluation.answers.get_mut(key) {
                    answer.choice = choice.into();
                    for (option, probability) in &mut answer.probabilities {
                        *probability = if option == choice { 1.0 } else { 0.0 };
                    }
                }
            }
            Ok(evaluation)
        })
    }
}

#[tokio::test]
async fn the_latest_posts_notice_is_not_given_to_the_first_post() {
    let controls: Vec<_> =
        [("first", "Post (Ada)"), ("clear", "Dismiss notice"), ("second", "Post (Grace)"), ("third", "Post (Lin)")]
            .into_iter()
            .map(|(id, label)| Control::new(id, "button", label, [Operation::Click]))
            .collect();
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Replies", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-11-23T00:00:00Z", "controls": controls, "viewport_text": "", "tabs": [],
    }))
    .unwrap();
    let checks = Arc::new(Mutex::new(Vec::new()));
    let script = steps("click Post (Ada); click Dismiss notice; click Post (Grace); click Post (Lin)").unwrap();
    let agent = Agent::new(
        Arc::new(Gallery(Mutex::new(page))),
        Arc::new(Questions { script: ScriptedJev::new(script.clone()), checks: checks.clone() }),
        Arc::new(ScriptedDriver::new(script)),
    );
    let result = agent
        .run(
            "Post replies to Ada, Grace and Lin, dismissing the first notice after posting to Ada.",
            RunOptions {
                start: Some("https://example.test/".into()),
                authorization: Some(Authorization { irreversible_actions: true }),
                limits: Some(Limits { max_steps: Some(6), ..Limits::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.status, fastbrowse_core::models::Status::Complete, "{result:?}");
    let checks = checks.lock().unwrap();
    assert!(!checks.is_empty(), "the run must check the guarded post");
    for check in checks.iter() {
        let facts = check["page_facts"].to_string();
        assert!(
            !facts.contains("appeared when the run clicked 'Post (Ada)'; it tells of that step, not of any later one"),
            "the notice was dismissed and raised again by Grace's post, but the check is told: {facts}"
        );
    }
}
