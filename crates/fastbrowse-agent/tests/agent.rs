//! Replays the whole agent loops `tests/golden/generate_agent.py` ran in Python: the same scripted site, every model
//! request looked up by its bytes (so requests sent side by side may leave in any order), and the same result.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::Config;
use fastbrowse_core::jev::{Evaluation, JevClient, JevError, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{
    Artifact, Authorization, CostComponent, CostLine, Event, EventHandler, LLMPurpose, Limits, Operation, SecretValue,
    StepOutcome, UntilCheck,
};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Observation, Page, PageResult};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Style, dumps, write};
use fastbrowse_core::safety::ScopedSecrets;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::Value;

fn cases() -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/agent.json");
    serde_json::from_str(&fs::read_to_string(path).expect("agent.json")).expect("golden JSON")
}

/// The scripted site: screens, the addresses that open them and the actions that move between them.
struct FakeSite {
    site: Value,
    at: Mutex<(String, Vec<String>)>,
    /// Times each settling screen has been observed.
    seen: Mutex<HashMap<String, u64>>,
}

impl FakeSite {
    fn new(site: &Value) -> FakeSite {
        FakeSite {
            site: site.clone(),
            at: Mutex::new((site["start"].as_str().unwrap().to_string(), Vec::new())),
            seen: Mutex::default(),
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
        let Some(screen) = self.site["urls"][url].as_str() else {
            return ready(Err(BrowserError::Failed(format!("no page at {url}"))));
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
            page_changed,
            detail: None,
            form_unchanged: false,
            moved: false,
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

/// Recorded answers by the bytes of the request that got them; identical requests are answered in recorded order.
struct Replies {
    name: String,
    answers: Mutex<HashMap<String, VecDeque<Value>>>,
}

impl Replies {
    fn new(case: &Value, kind: &str, key: impl Fn(&Value) -> String) -> Replies {
        let mut answers: HashMap<String, VecDeque<Value>> = HashMap::new();
        for call in case["calls"].as_array().unwrap() {
            if let Some(request) = call.get(kind) {
                answers.entry(key(request)).or_default().push_back(call["response"].clone());
            }
        }
        Replies { name: case["name"].as_str().unwrap().to_string(), answers: Mutex::new(answers) }
    }

    fn take(&self, key: &str) -> Value {
        let mut answers = self.answers.lock().unwrap();
        if answers.get(key).is_none_or(|queue| queue.is_empty()) {
            // Show where the nearest request Python made parts from this one.
            let shared = |other: &str| key.bytes().zip(other.bytes()).take_while(|(a, b)| a == b).count();
            let nearest = answers.keys().max_by_key(|other| shared(other)).cloned().unwrap_or_default();
            let at = shared(&nearest);
            let around =
                |text: &str| text.get(at.saturating_sub(200)..(at + 300).min(text.len())).unwrap_or("").to_string();
            panic!(
                "{}: a request Python never made, parting at byte {at}:\nRust:   {}\nPython: {}",
                self.name,
                around(key),
                around(&nearest)
            );
        }
        let queue = answers.get_mut(key).unwrap();
        queue.pop_front().unwrap()
    }

    fn finish(&self) {
        let answers = self.answers.lock().unwrap();
        let left: Vec<&String> = answers.iter().filter(|(_, queue)| !queue.is_empty()).map(|(key, _)| key).collect();
        assert!(left.is_empty(), "{}: requests Python made and Rust did not:\n{left:#?}", self.name);
    }
}

fn jev_key(state: &str, questions: &str) -> String {
    format!("{state}\n{questions}")
}

struct ReplayJev(Replies);

impl JevClient for ReplayJev {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, JevError>> {
        let written: serde_json::Map<String, Value> =
            questions.iter().map(|(key, question)| (key.clone(), question.dump())).collect();
        let key = jev_key(&write(state, Style::HTTPX), &write(&Value::Object(written), Style::HTTPX));
        let response = self.0.take(&key);
        ready(Ok(serde_json::from_value(response["evaluation"].clone()).expect("evaluation")))
    }
}

fn llm_key(purpose: &str, messages: &str, schema_name: &str, schema: &str, cap: i64) -> String {
    format!("{purpose}\n{schema_name}\n{cap}\n{schema}\n{messages}")
}

struct ReplayLLM(Replies);

impl LLMClient for ReplayLLM {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        max_output_tokens: i64,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let written = Value::Array(messages.iter().map(Dump::dump).collect());
            let key = llm_key(
                purpose.as_str(),
                &write(&written, Style::HTTPX),
                &schema.name(),
                &dumps(&schema.json_schema()),
                max_output_tokens,
            );
            let response = self.0.take(&key);
            if let Some(failure) = response["raise"].as_str() {
                return Err(LLMError::Failed(failure.to_string()));
            }
            let data = schema.validate_json(&dumps(&response["data"])).map_err(LLMError::Failed)?;
            let cost: CostLine = serde_json::from_value(response["cost"].clone()).expect("cost");
            Ok(Generation { data, cost })
        })
    }
}

/// A result's lines in a fixed order: calls sent side by side bill in whichever order they finish.
fn normalized(mut outcome: Value) -> Value {
    if let Some(lines) = outcome["cost"]["lines"].as_array_mut() {
        lines.sort_by_key(|line| write(line, Style::DUMPS.sorted()));
    }
    if let Some(steps) = outcome["steps"].as_array_mut() {
        for step in steps {
            step["duration_ms"] = 0.into();
        }
    }
    outcome
}

async fn replay(case: Value) {
    let name = case["name"].as_str().unwrap().to_string();
    let jev = Arc::new(ReplayJev(Replies::new(&case, "jev", |r| {
        jev_key(r["state"].as_str().unwrap(), r["questions"].as_str().unwrap())
    })));
    let llm = Arc::new(ReplayLLM(Replies::new(&case, "llm", |r| {
        llm_key(
            r["purpose"].as_str().unwrap(),
            r["messages"].as_str().unwrap(),
            r["schema_name"].as_str().unwrap(),
            r["schema"].as_str().unwrap(),
            r["max_output_tokens"].as_i64().unwrap(),
        )
    })));
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = events.clone();
    let on_event: EventHandler = Arc::new(move |event: Event| {
        if let Event::Step(step) = event {
            let mut dumped = step.step.dump();
            dumped["duration_ms"] = 0.into();
            sink.lock().unwrap().push(serde_json::json!({"step": dumped, "frame": step.frame.is_some()}));
        }
        Box::pin(async {})
    });
    let secrets = case["secrets"].as_object().map(|secrets| {
        let held = secrets.iter().map(|(name, spec)| {
            let origins = spec[1].as_array().unwrap().iter().map(|o| o.as_str().unwrap().to_string()).collect();
            (name.clone(), (SecretValue::from(spec[0].as_str().unwrap()), origins))
        });
        Arc::new(ScopedSecrets::per_secret(held)) as Arc<dyn fastbrowse_core::models::SecretResolver>
    });
    let page = Arc::new(FakeSite::new(&case["site"]));
    let mut config = case["config"].clone();
    if config.is_null() {
        config = serde_json::json!({});
    }
    config["step_frames"] = case["step_frames"].as_bool().unwrap_or(false).into();
    let config: Config = serde_json::from_value(config).expect("config");
    let agent = Agent::new(page, jev.clone(), llm.clone())
        .with_config(config)
        .with_secrets(secrets)
        .with_events(Some(on_event));
    let until = case["until"].as_array().map(|accepted| {
        let accepted: Vec<String> = accepted.iter().map(|url| url.as_str().unwrap().to_string()).collect();
        let check: UntilCheck = Arc::new(move |url: String| {
            let held = accepted.contains(&url);
            Box::pin(async move { held })
        });
        check
    });
    let options = RunOptions {
        start: case["start"].as_str().map(String::from),
        choose_start: case["choose_start"].as_bool().unwrap_or(false),
        inputs: serde_json::from_value(case["inputs"].clone()).unwrap_or_default(),
        attachments: serde_json::from_value(case["attachments"].clone()).unwrap_or_default(),
        limits: (!case["limits"].is_null()).then(|| serde_json::from_value::<Limits>(case["limits"].clone()).unwrap()),
        authorization: (!case["authorization"].is_null())
            .then(|| serde_json::from_value::<Authorization>(case["authorization"].clone()).unwrap()),
        until,
        ..RunOptions::default()
    };
    let result = agent.run(case["task"].as_str().unwrap(), options).await.expect("run");
    // A request left unmade explains a differing result better than the result does.
    jev.0.finish();
    llm.0.finish();
    assert_eq!(normalized(result.dump()), normalized(case["outcome"].clone()), "{name}: result");
    assert_eq!(Value::Array(events.lock().unwrap().clone()), case["events"], "{name}: step events");
}

#[tokio::test(start_paused = true)]
async fn whole_loops_match_python() {
    for case in cases() {
        replay(case).await;
    }
}
