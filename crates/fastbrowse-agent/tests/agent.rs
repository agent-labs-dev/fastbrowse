//! Whole agent loops on a scripted site: the chat driver plays each case's `chat` script, Jev answers the gate and
//! check questions from the case's `jev` script, and every other LLM request from its `llm` answers. Every request is
//! kept, so a test can read what a model was shown.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedDriver, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{
    Artifact, Authorization, CostComponent, CostLine, LLMPurpose, Limits, Operation, StepOutcome,
};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Observation, Page, PageResult};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, dumps, write};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::Value;

fn cases() -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_sites.json");
    serde_json::from_str(&fs::read_to_string(path).expect("agent_sites.json")).expect("fixture JSON")
}

fn case(name: &str) -> Value {
    cases().into_iter().find(|case| case["name"] == name).unwrap_or_else(|| panic!("no case {name:?}"))
}

/// The scripted site: screens, the addresses that open them and the actions that move between them.
struct FakeSite {
    site: Value,
    at: Mutex<(String, Vec<String>)>,
    /// Times each settling screen has been observed.
    seen: Mutex<HashMap<String, u64>>,
    /// Times each address has been navigated to, for an address that opens a different screen per visit.
    visits: Mutex<HashMap<String, usize>>,
}

impl FakeSite {
    fn new(site: &Value) -> FakeSite {
        FakeSite {
            site: site.clone(),
            at: Mutex::new((site["start"].as_str().unwrap().to_string(), Vec::new())),
            seen: Mutex::default(),
            visits: Mutex::default(),
        }
    }

    fn screen(&self) -> Value {
        let at = self.at.lock().unwrap();
        self.site["screens"][&at.0].clone()
    }
}

fn ready<'a, T: Send + 'a>(value: T) -> BoxFuture<'a, T> {
    Box::pin(async move { value })
}

impl Page for FakeSite {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn keep_capture<'a>(&'a self, name: &'a str, text: &'a str) -> BoxFuture<'a, PageResult<Option<Artifact>>> {
        let kept = self.site["keeps_captures"].as_bool().unwrap_or(false).then(|| Artifact {
            kind: fastbrowse_core::models::ArtifactKind::Capture,
            name: name.into(),
            mime_type: "text/plain; charset=utf-8".into(),
            size_bytes: text.len() as i64,
            sha256: fastbrowse_core::py::sha256_hex(text.as_bytes()),
            uri: format!("memory://{name}"),
        });
        ready(Ok(kept))
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        // A screen that `settles` turns into another once it has been observed `after` times.
        let settles = self.screen()["settles"].clone();
        if !settles.is_null() {
            let mut at = self.at.lock().unwrap();
            let mut seen = self.seen.lock().unwrap();
            let count = seen.entry(at.0.clone()).or_default();
            *count += 1;
            if *count > settles["after"].as_u64().unwrap() {
                at.0 = settles["to"].as_str().unwrap().to_string();
            }
        }
        ready(Ok(serde_json::from_value(self.screen()["observation"].clone()).expect("observation")))
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        ready(Ok(false))
    }

    fn navigate<'a>(&'a self, url: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        // A list is one screen per visit, the last repeated: a reload that mints a new document each time.
        let screens = &self.site["urls"][url];
        let screen = match screens.as_array() {
            Some(list) => {
                let mut visits = self.visits.lock().unwrap();
                let visit = visits.entry(url.to_string()).or_default();
                *visit += 1;
                list[(*visit - 1).min(list.len() - 1)].as_str()
            }
            None => screens.as_str(),
        };
        let Some(screen) = screen else {
            return ready(Err(BrowserError::failed("test/no-page", format!("no page at {url}"))));
        };
        let mut at = self.at.lock().unwrap();
        let left = std::mem::replace(&mut at.0, screen.to_string());
        at.1.push(left);
        ready(Ok(()))
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        ready(Ok(serde_json::from_value(self.screen()["capture"].clone()).expect("capture")))
    }

    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        let mut at = self.at.lock().unwrap();
        let result = |outcome: StepOutcome, page_changed: bool| ActResult {
            outcome,
            failure: None,
            page_changed,
            detail: None,
            moved: false,
            landed: None,
        };
        if action.operation == Operation::Navigate
            && let Some(screen) = action.url.as_deref().and_then(|url| self.site["urls"][url].as_str())
        {
            let left = std::mem::replace(&mut at.0, screen.to_string());
            at.1.push(left);
            return ready(Ok(result(StepOutcome::Executed, true)));
        }
        if action.operation == Operation::Back
            && let Some(back) = at.1.pop()
        {
            at.0 = back;
            return ready(Ok(result(StepOutcome::Executed, true)));
        }
        let rules = self.site["actions"].as_array().cloned().unwrap_or_default();
        for rule in rules {
            let target = rule["target"].as_str();
            if rule["screen"] == at.0.as_str()
                && rule["operation"] == action.operation.as_str()
                && (target.is_none() || target == action.target_id.as_deref())
            {
                let next = rule["next"].as_str().unwrap_or(&at.0).to_string();
                if next != at.0 {
                    let left = std::mem::replace(&mut at.0, next);
                    at.1.push(left);
                }
                let scripted = if rule["result"].is_null() {
                    serde_json::json!({"outcome": "executed", "page_changed": true})
                } else {
                    rule["result"].clone()
                };
                return ready(Ok(serde_json::from_value(scripted).expect("act result")));
            }
        }
        ready(Ok(result(StepOutcome::Executed, false)))
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        ready(Ok(false))
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        ready(Ok(b"png".to_vec()))
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        ready(Ok(self.screen()["observation"]["url"].as_str().unwrap().to_string()))
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        ready(Ok(self.screen()["observation"]["response_status"].as_i64()))
    }

    // A scripted site reports no lifecycle events, so a wait on one ends at once with nothing new.
    fn lifecycle_mark(&self) -> u64 {
        0
    }

    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        ready(false)
    }
}

/// Every request a run made, each `{"jev": {state, questions}}` or `{"llm": {purpose, schema_name, messages}}`, as the
/// text the client was sent.
type Log = Arc<Mutex<Vec<Value>>>;

/// The `n`th entry of a scripted answer that is a list, the last repeated, or the answer itself.
fn nth(script: &Value, n: usize) -> Option<&Value> {
    match script.as_array() {
        Some(list) => list.get(n.min(list.len().saturating_sub(1))),
        None => (!script.is_null()).then_some(script),
    }
}

/// Jev's answer from a case's `jev` script: `nouls` a probability per noul (a list is taken in turn) and `noul` the
/// rest; a choice takes `pick`'s entry for its key, or its first option. A case's answer is the one it expects to
/// stand, so the checks of that answer pass unless it scripts them, and the wall questions, and whether the task forbids
/// an action, default to no.
fn scripted_jev(script: &Value, questions: &Value, log: &[Value]) -> Value {
    let answered = |key: &str| {
        log.iter()
            .filter(|call| call["jev"]["questions"].as_str().is_some_and(|q| q.contains(&format!("\"{key}\":"))))
            .count()
    };
    let mut answers = serde_json::Map::new();
    for (key, question) in questions.as_object().unwrap() {
        if question["type"] == "noul" {
            let default = if matches!(
                key.as_str(),
                "login_required"
                    | "bot_check"
                    | "site_failure"
                    | "unobtainable"
                    | "inside_drawn"
                    | "dates"
                    | "prohibits"
            ) {
                0.0
            } else if matches!(key.as_str(), "coverage" | "exact" | "carried") || key.starts_with("claim") {
                1.0
            } else {
                script["noul"].as_f64().unwrap_or(0.0)
            };
            let probability = nth(&script["nouls"][key], answered(key)).and_then(Value::as_f64).unwrap_or(default);
            answers.insert(key.clone(), serde_json::json!({"type": "noul", "probability": probability}));
            continue;
        }
        let options: Vec<&String> = question["criteria"].as_object().unwrap().keys().collect();
        let picked = nth(&script["pick"][key], answered(key)).and_then(Value::as_str);
        let choice = picked.filter(|p| options.iter().any(|o| o == p)).map(String::from).unwrap_or(options[0].clone());
        let probabilities: serde_json::Map<String, Value> =
            options.iter().map(|o| ((*o).clone(), Value::from(if **o == choice { 1.0 } else { 0.0 }))).collect();
        answers.insert(
            key.clone(),
            serde_json::json!({"type": "choice", "choice": choice, "probabilities": probabilities, "confidence": 0.9}),
        );
    }
    serde_json::json!({"model": "test", "answers": answers, "input_tokens": 0, "cost": {"component": "jev", "basis": "estimated", "dollars": 0.0, "purpose": null, "input_tokens": 0, "output_tokens": 0, "seconds": null}, "requests": 1})
}

struct CaseJev(Value, Log);

impl DecisionClient for CaseJev {
    fn profile(&self) -> DecisionProfile {
        DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        let written: serde_json::Map<String, Value> =
            questions.iter().map(|(key, question)| (key.clone(), question.dump())).collect();
        let written = Value::Object(written);
        let mut log = self.1.lock().unwrap();
        let evaluation = scripted_jev(&self.0, &written, &log);
        log.push(serde_json::json!({"jev": {
            "state": write(state, Style::HTTPX), "questions": write(&written, Style::HTTPX),
        }}));
        ready(Ok(serde_json::from_value(evaluation).expect("evaluation")))
    }
}

/// The chat driver plays the case's `chat` script; every other request takes the next of its schema's `llm` answers,
/// the last repeated. An answer that is `{"raise": ...}` fails.
struct CaseLLM {
    case: Value,
    driver: ScriptedDriver,
    log: Log,
}

impl LLMClient for CaseLLM {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        let schema_name = schema.name();
        let made = {
            let mut log = self.log.lock().unwrap();
            let made = log.iter().filter(|call| call["llm"]["schema_name"] == schema_name.as_str()).count();
            log.push(serde_json::json!({"llm": {
                "purpose": purpose.as_str(),
                "schema_name": schema_name,
                "messages": write(&Value::Array(messages.iter().map(Dump::dump).collect()), Style::HTTPX),
            }}));
            made
        };
        if purpose == LLMPurpose::Action {
            return self.driver.generate_value(purpose, messages, schema, ledger);
        }
        Box::pin(async move {
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let data = match nth(&self.case["llm"][&schema_name], made) {
                Some(data) => data.clone(),
                // A case knows of no pages unless it says so.
                None if matches!(schema_name.as_str(), "KnownPages" | "RecoveryPages") => {
                    serde_json::json!({"urls": []})
                }
                None => panic!("{}: no scripted {schema_name} answer", self.case["name"]),
            };
            if let Some(failure) = data["raise"].as_str() {
                return Err(LLMError::failed(fastbrowse_core::llm::FaultClass::Client, failure.to_string()));
            }
            let data = schema.validate_json(&dumps(&data)).map_err(LLMError::schema)?;
            let cost: CostLine = serde_json::from_value(serde_json::json!({"component": "llm", "basis": "metered", "dollars": 0.001, "purpose": purpose.as_str(), "input_tokens": 0, "output_tokens": 0, "seconds": null})).expect("cost");
            Ok(Generation { data, cost })
        })
    }
}

/// The result of a run against the case's scripted site and scripted models, with every request it made.
async fn play(case: &Value) -> (Value, Vec<Value>) {
    let log = Log::default();
    let jev = Arc::new(CaseJev(case["jev"].clone(), log.clone()));
    let script = steps(case["chat"].as_str().unwrap_or_default()).expect("chat script");
    let llm = Arc::new(CaseLLM { case: case.clone(), driver: ScriptedDriver::new(script), log: log.clone() });
    let page = Arc::new(FakeSite::new(&case["site"]));
    let mut config = case["config"].clone();
    if config.is_null() {
        config = serde_json::json!({});
    }
    let config: Config = serde_json::from_value(config).expect("config");
    let agent = Agent::new(page, jev, llm).with_config(config);
    let options = RunOptions {
        start: case["start"].as_str().map(String::from),
        choose_start: case["choose_start"].as_bool().unwrap_or(false),
        limits: (!case["limits"].is_null()).then(|| serde_json::from_value::<Limits>(case["limits"].clone()).unwrap()),
        authorization: (!case["authorization"].is_null())
            .then(|| serde_json::from_value::<Authorization>(case["authorization"].clone()).unwrap()),
        ..RunOptions::default()
    };
    let result = agent.run(case["task"].as_str().unwrap(), options).await.expect("run");
    let log = log.lock().unwrap().clone();
    (result.dump(), log)
}

/// Asked to open example.com, follow its link and report the heading the link leads to, a run that had read that
/// heading had its DONE refused, the opening of example.com judged never carried out, and ended stuck (#1373): the
/// done check was shown the click and the read, not the run's own opening of its first page. Shown that opening first,
/// Jev judged every step carried out. It is shown whether the caller gave the page or the run worked it out from the
/// task, with the page it landed on, and is no step: a run that only opened and read is still told it took none.
#[tokio::test(start_paused = true)]
async fn the_done_check_is_shown_the_runs_opening_of_its_first_page() {
    for (name, chat, opened, landed, stepped) in [
        (
            "an HTTP 404 page goes to Jev, and its DONE ends the run",
            "done",
            "https://shop.test/missing",
            "https://shop.test/missing",
            false,
        ),
        (
            "a start page worked out from the task that opens nothing falls back to the front page",
            "click About; done",
            "https://site.test/deep/path",
            "https://site.test",
            true,
        ),
    ] {
        let mut case = case(name);
        case["chat"] = chat.into();
        let (_, log) = play(&case).await;
        let state: Value = log
            .iter()
            .find(|call| call["jev"]["questions"].as_str().is_some_and(|q| q.contains("\"carried\":")))
            .map(|call| serde_json::from_str(call["jev"]["state"].as_str().unwrap()).unwrap())
            .unwrap_or_else(|| panic!("{name}: a done check in {log:#?}"));
        assert_eq!(
            state["recent_actions"][0],
            serde_json::json!({
                "operation": "navigate", "target": opened, "outcome": "executed", "page_changed": true,
                "effect": format!("went to {landed}"), "read_progress": false,
            }),
            "{name}"
        );
        let untouched = state["page_facts"].to_string().contains("This run has taken no step on any page");
        assert_eq!(untouched, !stepped, "{name}: {state}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_date_the_task_names_reaches_the_driver_worked_out() {
    let mut case = case("a step limit ends budget_exceeded");
    case["name"] = "a date the task names".into();
    case["task"] = "Book the first Friday of December".into();
    case["chat"] = "fill Search = dune".into();
    case["jev"]["nouls"]["dates"] = 0.9.into();
    case["llm"]["TaskDates"] = serde_json::json!({"dates": [{
        "text": "the first Friday of December", "year": null, "month": 12, "day": null, "weekday": "friday",
        "nth": 1, "days_from_today": null, "months_from_now": null
    }]});
    let (_, log) = play(&case).await;
    let extraction = log.iter().find(|call| call["llm"]["schema_name"] == "TaskDates").unwrap();
    let prompt = extraction["llm"]["messages"].as_str().unwrap();
    assert!(prompt.contains("Use month only for a named or numbered calendar month."), "{prompt}");
    assert!(prompt.contains("copy the referenced offset into months_from_now and leave month null"), "{prompt}");
    // The task message every request carries states the date worked out, and no calendar: it is sent once, ahead of
    // the turns, so the conversation's prefix stays the same from one request to the next.
    let turns: Vec<Value> = log
        .iter()
        .filter(|call| call["llm"]["purpose"] == "action")
        .map(|call| serde_json::from_str(call["llm"]["messages"].as_str().unwrap()).unwrap())
        .collect();
    assert!(!turns.is_empty());
    for messages in turns {
        let turn = messages.as_array().unwrap()[1]["content"].as_str().unwrap().to_string();
        assert!(turn.contains("\nthe first Friday of December: 2026-12-04 (Friday)"), "{turn}");
        assert!(!turn.contains("Mon "), "no calendar: {turn}");
    }
}

/// Transient site failures, each a scripted site run from its script alone and checked against what it must do: the
/// status it ends with, the steps it must take in order, the address it ends on and the words its error must carry.
#[tokio::test(start_paused = true)]
async fn transient_site_failures_are_retried_and_never_answer() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/site_failures.json");
    let cases: Vec<Value> = serde_json::from_str(&fs::read_to_string(path).expect("site_failures.json")).expect("JSON");
    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap().to_string();
        let expect = case["expect"].clone();
        let (outcome, _) = play(&case).await;
        let steps: Vec<String> = outcome["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| format!("{} {}", step["operation"].as_str().unwrap(), step["target"].as_str().unwrap_or("")))
            .map(|step| step.trim().to_string())
            .collect();
        let ran = format!("{name}: {} after {steps:?}, error {}", outcome["status"], outcome["error"]);
        if outcome["status"] != expect["status"] {
            failures.push(format!("{ran}: expected {}", expect["status"]));
        }
        let mut taken = steps.iter();
        for wanted in expect["steps"].as_array().into_iter().flatten() {
            let wanted = wanted.as_str().unwrap();
            if !taken.any(|step| step == wanted) {
                failures.push(format!("{ran}: no {wanted:?} in order"));
            }
        }
        if let Some(url) = expect["final_url"].as_str()
            && outcome["final_url"] != url
        {
            failures.push(format!("{ran}: ended on {}, expected {url}", outcome["final_url"]));
        }
        if let Some(words) = expect["error"].as_str()
            && !outcome["error"].as_str().unwrap_or_default().contains(words)
        {
            failures.push(format!("{ran}: error lacks {words:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A status page read, waited on and read again, its two status lines the answer's two claims.
fn status_case() -> Value {
    let mut case = case("an HTTP 404 page goes to Jev, and its DONE ends the run");
    case["name"] = "read, wait, read".into();
    case["task"] = "Open https://status.test/ and report its status line, then wait a few seconds and report the \
                    status line again"
        .into();
    case["start"] = "https://status.test/".into();
    // The driver writes both status lines as claims, each quoting the page it was shown before and after the wait.
    let claim =
        |text: &str, quote: &str| serde_json::json!({"text": text, "quotes": [quote], "tally": null, "within": null});
    let answer = serde_json::json!([
        claim("First: live, updated 16:59:21", "live, updated 16:59:21"),
        claim("After waiting: live, updated 16:59:27", "live, updated 16:59:27"),
    ]);
    case["chat"] = format!("wait; done={answer}").into();
    // Jev holds each claim supported by its quote, and the two together to cover the task.
    case["jev"]["nouls"] = serde_json::json!({"claim0": 0.9, "claim1": 0.9, "coverage": 0.9});
    let screen = |key: &str, text: &str| {
        let mut screen = case["site"]["screens"]["missing"].clone();
        let observation = &mut screen["observation"];
        observation["url"] = "https://status.test/".into();
        observation["title"] = "Status".into();
        observation["page_key"] = key.into();
        observation["document_key"] = "status".into();
        observation["viewport_text"] = text.into();
        observation.as_object_mut().unwrap().remove("response_status");
        let capture = &mut screen["capture"];
        capture["url"] = "https://status.test/".into();
        capture["title"] = "Status".into();
        capture["sha256"] = fastbrowse_core::py::sha256_hex(text.as_bytes()).into();
        capture["text"] = text.into();
        capture["blocks"] = serde_json::json!([
            {"source_id": "s0", "kind": "paragraph", "frame_id": null, "start": 0, "end": text.chars().count()}
        ]);
        screen
    };
    case["site"] = serde_json::json!({
        "start": "blank",
        "urls": {"https://status.test/": "early"},
        "screens": {
            "blank": case["site"]["screens"]["blank"].clone(),
            "early": screen("early", "live, updated 16:59:21"),
            "later": screen("later", "live, updated 16:59:27"),
        },
        "actions": [{"screen": "early", "operation": "wait", "next": "later"}]
    });
    case
}

#[tokio::test(start_paused = true)]
async fn a_task_that_reads_waits_and_reads_again_completes_with_both_readings() {
    let (outcome, _) = play(&status_case()).await;
    let operations: Vec<&str> =
        outcome["steps"].as_array().unwrap().iter().map(|step| step["operation"].as_str().unwrap()).collect();
    assert!(operations.contains(&"wait"), "no wait was taken: {operations:?}");
    let quotes: Vec<&str> =
        outcome["evidence"].as_array().unwrap().iter().map(|e| e["quote"].as_str().unwrap()).collect();
    assert!(quotes.contains(&"live, updated 16:59:21"), "{outcome}");
    assert!(quotes.contains(&"live, updated 16:59:27"), "{outcome}");
    assert_eq!(outcome["status"], "complete", "{outcome}");
}

/// Each page a citation quotes is kept as a `capture` artifact, and the evidence names it, so the quote can be checked
/// against the text it was cut from.
#[tokio::test(start_paused = true)]
async fn each_cited_capture_is_kept_as_an_artifact_the_evidence_names() {
    let mut case = status_case();
    case["site"]["keeps_captures"] = true.into();
    let (outcome, _) = play(&case).await;
    assert_eq!(outcome["status"], "complete", "{outcome}");
    let artifacts = outcome["artifacts"].as_array().unwrap();
    let evidence = outcome["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 2, "{outcome}");
    for quoted in evidence {
        let uri = quoted["capture_artifact"].as_str().unwrap_or_else(|| panic!("no capture kept: {quoted}"));
        let artifact = artifacts.iter().find(|artifact| artifact["uri"] == uri).expect("the named artifact");
        assert_eq!(artifact["kind"], "capture");
        // The kept bytes are the page text the quote was cut from: its digest is the evidence's capture digest.
        assert_eq!(artifact["sha256"], quoted["capture_sha256"]);
        assert_eq!(artifact["size_bytes"], quoted["quote"].as_str().unwrap().len());
    }
}
