//! A grouped follow-up must fit even when there are no pictures left to remove.

mod common;

use std::sync::mpsc;
use std::time::Duration;

use fastbrowse_agent::policy::{StepContext, build_request, decide, fits};
use fastbrowse_agent::scripted::{ScriptedJev, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::{Config, JSON_CHARS_PER_TOKEN};
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Control, Observation};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::telemetry::Trace;
use indexmap::IndexMap;
use serde_json::{Value, json};

struct Grouped(ScriptedJev);

impl DecisionClient for Grouped {
    fn profile(&self) -> DecisionProfile {
        DecisionProfile { max_choice_options: 10, ..DecisionProfile::jev() }
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.0.evaluate(state, questions)
    }
}

#[test]
fn a_group_with_text_larger_than_the_initial_question_does_not_spin() {
    let controls: Vec<_> = (0..12)
        .map(|i| Control {
            href: Some(format!("https://example.test/{}", "x".repeat(2000))),
            ..Control::new(&format!("b{i}"), "link", &format!("Link {i}"), [Operation::Click])
        })
        .collect();
    let page: Observation = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Links", "page_key": "k",
        "captured_at": "2026-09-26T20:00:00Z", "controls": controls,
        "viewport_text": "", "tabs": [],
    }))
    .unwrap();
    let context = StepContext::new("Open Link 0");
    let mut config = Config::default();
    config.observation.group_size = 4;
    let request = build_request(&page, &page.controls, &context, &config, 10);
    let largest = request.questions.values().map(|q| q.dump_json().chars().count()).max().unwrap();
    let state_chars = dumps(&request.state).chars().count();
    config.tokens.state_plus_largest_question =
        ((state_chars + largest + 100) as f64 / JSON_CHARS_PER_TOKEN).ceil() as i64;
    assert!(fits(&request, &config));
    assert!(request.images.is_empty());
    assert!(request.groups.contains_key(&Operation::Click));

    // The fitting loop is synchronous. A worker makes a regression fail instead of hanging the test runner.
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let jev = Grouped(ScriptedJev::new(steps("click Link 0").unwrap()));
        let decision = common::block_on(decide(&jev, &page, &context, &config, None, &Trace::disabled()));
        let _ = sent.send(decision);
    });
    let decision = received
        .recv_timeout(Duration::from_secs(2))
        .expect("fitting a text-only grouped request must terminate")
        .expect("reducing the request leaves room to choose a link");
    assert_eq!(decision.target.expect("Jev chose a link").id, "b0");
}
