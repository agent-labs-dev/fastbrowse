//! A form id is local to its frame and document, not to the entire run.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedDriver, ScriptedJev, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Authorization, Limits, Operation, Status};
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
        Arc::new(ScriptedDriver::new(steps("click Draft; click Post").unwrap())),
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
        Arc::new(ScriptedDriver::new(steps("click Draft; click Next page; click Post").unwrap())),
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
        Arc::new(ScriptedDriver::new(steps("click Draft; click Post; dialog").unwrap())),
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

#[tokio::test]
async fn a_portalled_option_reports_the_field_it_sets_as_part_of_the_submitted_form() {
    let field = Control {
        form_id: Some("ticket-form".into()),
        value: Some("Round trip".into()),
        ..Control::new("ticket", "combobox", "Ticket type", [Operation::Click])
    };
    // A listbox rendered outside the form has no form owner, while the combobox it sets does.
    let choice = Control::new("choice", "option", "One way", [Operation::Click]);
    let post =
        Control { form_id: Some("ticket-form".into()), ..Control::new("post", "button", "Post", [Operation::Click]) };
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Ticket form", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-09-26T20:00:00Z", "controls": [field, choice, post], "viewport_text": "", "tabs": [],
    }))
    .unwrap();
    let jev = RequestedPost(ScriptedJev::new(steps("click One way; click Post").unwrap()));
    let agent = Agent::new(
        Arc::new(FormPage { page: Mutex::new(page), confirms_post: false }),
        Arc::new(jev),
        Arc::new(ScriptedDriver::new(steps("click One way; click Post").unwrap())),
    );
    let result = agent
        .run(
            "Choose One way and post the form",
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
    let answer = result.answer.expect("the unfinished run reports its commit");
    let committed = answer.split("This run ended without finishing after changing fields").next().unwrap();
    assert!(
        committed.contains("Ticket type: 'One way'"),
        "the field submitted by Post belongs to that commit: {answer}"
    );
    assert!(!answer.contains("may still hold what it entered"), "the submitted choice is not an unsent edit: {answer}");
}

#[tokio::test]
async fn a_commit_keeps_fields_prefilled_in_another_form_among_unsent_changes() {
    for (article_form, share_form) in [(Some("article"), Some("social")), (None, None)] {
        let publish = Control {
            form_id: article_form.map(str::to_owned),
            checked: Some(false),
            ..Control::new("publish", "switch", "Publish article", [Operation::Click])
        };
        let share = Control {
            form_id: share_form.map(str::to_owned),
            value: Some(String::new()),
            ..Control::new("share", "textbox", "Share message", [Operation::Fill])
        };
        let page = serde_json::from_value(json!({
            "url": "https://example.test/", "title": "Article and sharing", "page_key": "k", "document_key": "doc",
            "captured_at": "2026-09-26T20:00:00Z", "controls": [publish, share], "viewport_text": "", "tabs": [],
        }))
        .unwrap();
        let jev = RequestedPost(ScriptedJev::new(steps("click Publish article").unwrap()));
        let agent = Agent::new(
            Arc::new(FormPage { page: Mutex::new(page), confirms_post: false }),
            Arc::new(jev),
            Arc::new(ScriptedDriver::new(steps("click Publish article").unwrap())),
        );
        let result = agent
            .run(
                "Publish the article, then prepare a message to share it",
                RunOptions {
                    start: Some("https://example.test/".into()),
                    authorization: Some(Authorization { irreversible_actions: true }),
                    limits: Some(Limits { max_steps: Some(1), ..Limits::default() }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(result.status, Status::BudgetExceeded, "{:?}", result.error);
        let answer = result.answer.expect("the unfinished run reports what it changed");
        let (committed, changed) = answer
            .split_once("This run ended without finishing after changing fields")
            .unwrap_or_else(|| panic!("the separately owned sharing field is still unsent: {answer}"));
        assert!(committed.contains("Publish article: checked"), "{answer}");
        assert!(!committed.contains("Share message:"), "the share message was never sent: {answer}");
        assert!(changed.contains("Share message: 'Our new article'"), "{answer}");
    }
}
