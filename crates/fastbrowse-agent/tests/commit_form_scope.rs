//! A form id is local to its frame and document, not to the entire run.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Artifact, Authorization, Limits, Operation, Status, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Dialog, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct FormPage {
    page: Mutex<Observation>,
    confirms_post: bool,
}

impl Page for FormPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let page = self.page.lock().unwrap().clone();
        Box::pin(async move { Ok(page) })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        let page = self.page.lock().unwrap().clone();
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": page.url, "title": page.title, "captured_at": page.captured_at,
                "sha256": "empty", "text": "", "blocks": [],
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        let mut page = self.page.lock().unwrap();
        match (action.operation, action.target_id.as_deref()) {
            (Operation::Dialog, _) => {
                assert_eq!(action.accept_dialog, Some(true));
                page.dialog = None;
                page.viewport_text = "Comment posted".into();
            }
            (_, Some("draft")) => page.controls[0].checked = Some(true),
            (_, Some("post")) if self.confirms_post => {
                page.dialog =
                    Some(Dialog { kind: "confirm".into(), message: "Post this comment?".into(), default_prompt: None });
            }
            (_, Some("post")) => page.viewport_text = "Comment posted".into(),
            (_, Some("next")) => {
                page.url = "https://example.test/second".into();
                page.document_key = "second-document".into();
                page.controls = vec![Control {
                    form_id: Some("1".into()),
                    ..Control::new("post", "button", "Post", [Operation::Click])
                }];
            }
            other => panic!("unexpected action: {other:?}"),
        }
        Box::pin(async {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                page_changed: true,
                detail: None,
                moved: false,
            })
        })
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(true) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Err(BrowserError::ScreenshotsUnavailable("scripted page".into())) })
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        let url = self.page.lock().unwrap().url.clone();
        Box::pin(async move { Ok(url) })
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(Some(200)) })
    }

    fn lifecycle_mark(&self) -> u64 {
        0
    }

    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        Box::pin(async { true })
    }
}

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
                let posting = question.instructions.contains("Post");
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
async fn a_commit_does_not_consume_a_draft_in_another_frames_same_numbered_form() {
    let draft = Control {
        frame_id: Some("draft-frame".into()),
        form_id: Some("1".into()),
        checked: Some(false),
        ..Control::new("draft", "checkbox", "Draft", [Operation::Click])
    };
    let post = Control {
        frame_id: Some("post-frame".into()),
        form_id: Some("1".into()),
        ..Control::new("post", "button", "Post", [Operation::Click])
    };
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Two forms", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [draft, post], "viewport_text": "", "tabs": [],
    }))
    .unwrap();
    let jev = RequestedPost(ScriptedJev::new(steps("click Draft; click Post").unwrap()));
    let agent = Agent::new(
        Arc::new(FormPage { page: Mutex::new(page), confirms_post: false }),
        Arc::new(jev),
        Arc::new(ScriptedLLM),
    );
    let result = agent
        .run(
            "Check Draft and post the comment",
            RunOptions {
                start: Some("https://example.test/".into()),
                authorization: Some(Authorization { irreversible_actions: true }),
                limits: Some(Limits { max_steps: Some(2), ..Limits::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.status, Status::BudgetExceeded, "{:?}", result.error);
    let answer = result.answer.expect("the run reports what it changed");
    let (committed, changed) = answer
        .split_once("This run ended without finishing after changing fields")
        .unwrap_or_else(|| panic!("the other frame's draft must remain a changed field: {answer}"));
    assert!(!committed.contains("Draft: checked"), "{answer}");
    assert!(changed.contains("Draft: checked"), "{answer}");
}

#[tokio::test]
async fn a_commit_does_not_consume_a_draft_in_a_previous_documents_same_numbered_form() {
    let draft = Control {
        form_id: Some("1".into()),
        checked: Some(false),
        ..Control::new("draft", "checkbox", "Draft", [Operation::Click])
    };
    let next = Control::new("next", "link", "Next page", [Operation::Click]);
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "First form", "page_key": "k", "document_key": "first-document",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [draft, next], "viewport_text": "", "tabs": [],
    }))
    .unwrap();
    let jev = RequestedPost(ScriptedJev::new(steps("click Draft; click Next page; click Post").unwrap()));
    let agent = Agent::new(
        Arc::new(FormPage { page: Mutex::new(page), confirms_post: false }),
        Arc::new(jev),
        Arc::new(ScriptedLLM),
    );
    let result = agent
        .run(
            "Check Draft, open Next page and post the comment",
            RunOptions {
                start: Some("https://example.test/".into()),
                authorization: Some(Authorization { irreversible_actions: true }),
                limits: Some(Limits { max_steps: Some(3), ..Limits::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.status, Status::BudgetExceeded, "{:?}", result.error);
    let answer = result.answer.expect("the run reports what it changed");
    let (committed, changed) = answer
        .split_once("This run ended without finishing after changing fields")
        .unwrap_or_else(|| panic!("the previous page's draft must remain a changed field: {answer}"));
    assert!(!committed.contains("Draft: checked"), "{answer}");
    assert!(changed.contains("Draft: checked"), "{answer}");
}

#[tokio::test]
async fn accepting_a_confirmation_reports_the_submitted_fields_with_the_commit() {
    let draft = Control {
        frame_id: None,
        form_id: Some("1".into()),
        checked: Some(false),
        ..Control::new("draft", "checkbox", "Draft", [Operation::Click])
    };
    let post = Control {
        frame_id: None,
        form_id: Some("1".into()),
        ..Control::new("post", "button", "Post", [Operation::Click])
    };
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Two forms", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [draft, post], "viewport_text": "", "tabs": [],
    }))
    .unwrap();
    let jev = RequestedPost(ScriptedJev::new(steps("click Draft; click Post; dialog").unwrap()));
    let agent = Agent::new(
        Arc::new(FormPage { page: Mutex::new(page), confirms_post: true }),
        Arc::new(jev),
        Arc::new(ScriptedLLM),
    );
    let result = agent
        .run(
            "Check Draft and post the comment",
            RunOptions {
                start: Some("https://example.test/".into()),
                authorization: Some(Authorization { irreversible_actions: true }),
                limits: Some(Limits { max_steps: Some(3), ..Limits::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.status, Status::BudgetExceeded, "{:?}", result.error);
    let answer = result.answer.expect("the run reports what it changed");
    let committed = answer.split("This run ended without finishing after changing fields").next().unwrap();
    assert!(committed.contains("Draft: checked"), "the submitted field must belong to the commit: {answer}");
    assert!(!answer.contains("may still hold what it entered"), "the submitted field is not an unsent edit: {answer}");
}
