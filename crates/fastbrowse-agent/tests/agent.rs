//! Replays whole agent loops recorded in `tests/golden/agent.json`: the same scripted site, every model request
//! looked up by its bytes (so requests sent side by side may leave in any order), and the same result.
//!
//! `FASTBROWSE_UPDATE_GOLDEN=1` re-records them after an intended behaviour change: recorded answers are kept, a new
//! request is answered from the case's `jev` and `llm` scripts, and the diff of `agent.json` is the behaviour change.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::{DecisionClient, DecisionError, Evaluation, Question};
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
        let result =
            |outcome: StepOutcome, page_changed: bool| ActResult { outcome, page_changed, detail: None, moved: false };
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
///
/// With `FASTBROWSE_UPDATE_GOLDEN=1` a request with no recorded answer keeps the answer recorded at its place in the
/// case where that still fits the request, and is otherwise answered from the case's script. Every call made is kept so
/// the case can be written back with its new calls, result and events.
struct Replies {
    name: String,
    kind: &'static str,
    answers: Mutex<HashMap<String, VecDeque<Value>>>,
    ordered: Vec<Value>,
    log: Option<Arc<Mutex<Vec<Value>>>>,
}

impl Replies {
    fn new(
        case: &Value,
        kind: &'static str,
        log: Option<Arc<Mutex<Vec<Value>>>>,
        key: impl Fn(&Value) -> String,
    ) -> Replies {
        let mut answers: HashMap<String, VecDeque<Value>> = HashMap::new();
        let mut ordered = Vec::new();
        for call in case["calls"].as_array().unwrap() {
            if let Some(request) = call.get(kind) {
                answers.entry(key(request)).or_default().push_back(call["response"].clone());
                ordered.push(call.clone());
            }
        }
        Replies { name: case["name"].as_str().unwrap().to_string(), kind, answers: Mutex::new(answers), ordered, log }
    }

    fn take(&self, key: &str, request: Value, script: impl FnOnce(&[Value]) -> Value) -> Value {
        let mut answers = self.answers.lock().unwrap();
        let recorded = answers.get_mut(key).and_then(VecDeque::pop_front);
        let Some(log) = &self.log else {
            return recorded.unwrap_or_else(|| self.missing(key, &answers));
        };
        let mut log = log.lock().unwrap();
        let response = recorded.unwrap_or_else(|| {
            let made = log.iter().filter(|call| call.get(self.kind).is_some()).count();
            let scripted = script(&log);
            match self.ordered.get(made) {
                Some(old) => in_place(self.kind, old, &request, scripted),
                None => scripted,
            }
        });
        log.push(serde_json::json!({self.kind: request, "response": response}));
        response
    }

    fn missing(&self, key: &str, answers: &HashMap<String, VecDeque<Value>>) -> ! {
        // Show where the nearest recorded request parts from this one.
        let shared = |other: &str| key.bytes().zip(other.bytes()).take_while(|(a, b)| a == b).count();
        let nearest = answers.keys().max_by_key(|other| shared(other)).cloned().unwrap_or_default();
        let at = shared(&nearest);
        let around =
            |text: &str| text.get(at.saturating_sub(200)..(at + 300).min(text.len())).unwrap_or("").to_string();
        panic!(
            "{}: a request never recorded, parting at byte {at}:\nRust:     {}\nRecorded: {}",
            self.name,
            around(key),
            around(&nearest)
        );
    }

    fn finish(&self) {
        if self.log.is_some() {
            return;
        }
        let answers = self.answers.lock().unwrap();
        let left: Vec<&String> = answers.iter().filter(|(_, queue)| !queue.is_empty()).map(|(key, _)| key).collect();
        assert!(left.is_empty(), "{}: requests recorded and not made:\n{left:#?}", self.name);
    }
}

/// The answer recorded at a request's place in the case, where it still fits: an LLM answer for the same schema, or
/// each Jev answer whose question is still asked with its recorded choice still offered. The script answers the rest.
fn in_place(kind: &str, old: &Value, request: &Value, scripted: Value) -> Value {
    if kind == "llm" {
        return if old["llm"]["schema_name"] == request["schema_name"] { old["response"].clone() } else { scripted };
    }
    let mut response = scripted;
    let Some(recorded) = old["response"]["evaluation"]["answers"].as_object() else { return response };
    let Some(answers) = response["evaluation"]["answers"].as_object_mut() else { return response };
    for (key, answer) in answers.iter_mut() {
        let Some(kept) = recorded.get(key) else { continue };
        let offered = answer["probabilities"]
            .as_object()
            .is_none_or(|p| kept["choice"].as_str().is_some_and(|c| p.contains_key(c)));
        if kept["type"] == answer["type"] && offered {
            *answer = kept.clone();
        }
    }
    response
}

/// The `n`th entry of a scripted answer that is a list, the last repeated, or the answer itself.
fn nth(script: &Value, n: usize) -> Option<&Value> {
    match script.as_array() {
        Some(list) => list.get(n.min(list.len().saturating_sub(1))),
        None => (!script.is_null()).then_some(script),
    }
}

/// Jev's answer from a case's `jev` script: `pick` names a choice per question key (a list is taken in turn), `pick`
/// under `pick` any other choice, `nouls` a probability per noul and `noul` the rest, `confidence` the pick's. Asked
/// again with a problem, Jev escalates; the wall questions default to no.
fn scripted_jev(script: &Value, state: &Value, questions: &Value, log: &[Value]) -> Value {
    let asked_again = state.get("problem").is_some();
    let confidence = if asked_again { 0.0 } else { script["confidence"].as_f64().unwrap_or(0.9) };
    let answered = |key: &str| {
        log.iter()
            .filter(|call| {
                call["jev"]["questions"].as_str().is_some_and(|q| q.contains(&format!("\"{key}\":")))
                    && !call["jev"]["state"].as_str().unwrap_or_default().contains("\"problem\":")
            })
            .count()
    };
    let mut answers = serde_json::Map::new();
    for (key, question) in questions.as_object().unwrap() {
        if question["type"] == "noul" {
            let default = if matches!(key.as_str(), "login_required" | "bot_check") {
                0.0
            } else {
                script["noul"].as_f64().unwrap_or(0.0)
            };
            let probability = script["nouls"][key].as_f64().unwrap_or(default);
            answers.insert(key.clone(), serde_json::json!({"type": "noul", "probability": probability}));
            continue;
        }
        let options: Vec<&String> = question["criteria"].as_object().unwrap().keys().collect();
        let picked = if key == "operation" && asked_again {
            Some("escalate".to_string())
        } else {
            let general = (key != "operation"
                && key != "read_assessment"
                && !key.ends_with("_target")
                && !key.ends_with("_group"))
            .then(|| &script["pick"]["pick"]);
            nth(&script["pick"][key], answered(key))
                .or_else(|| general.and_then(|pick| nth(pick, 0)))
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| (key == "operation").then(|| "escalate".to_string()))
        };
        let choice = picked.filter(|p| options.contains(&p)).unwrap_or_else(|| options[0].clone());
        let probabilities: serde_json::Map<String, Value> =
            options.iter().map(|o| ((*o).clone(), Value::from(if **o == choice { 1.0 } else { 0.0 }))).collect();
        answers.insert(
            key.clone(),
            serde_json::json!({"type": "choice", "choice": choice, "probabilities": probabilities, "confidence": confidence}),
        );
    }
    serde_json::json!({"evaluation": {"model": "test", "answers": answers, "input_tokens": 0, "cost": {"component": "jev", "basis": "estimated", "dollars": 0.0, "purpose": null, "input_tokens": 0, "output_tokens": 0, "seconds": null}, "requests": 1}})
}

/// The LLM's answer from a case's `llm` script: the next of the schema's listed answers, the last repeated. A schema
/// the script does not list repeats the last answer the case recorded for it. An answer that is `{"raise": ...}` fails.
fn scripted_llm(case: &Value, purpose: &str, schema_name: &str, log: &[Value]) -> Value {
    let made = log.iter().filter(|call| call["llm"]["schema_name"] == schema_name).count();
    let cost = serde_json::json!({"component": "llm", "basis": "metered", "dollars": 0.001, "purpose": purpose, "input_tokens": 0, "output_tokens": 0, "seconds": null});
    if let Some(data) = nth(&case["llm"][schema_name], made) {
        if data.get("raise").is_some() {
            return data.clone();
        }
        return serde_json::json!({"data": data, "cost": cost});
    }
    let calls = case["calls"].as_array().unwrap();
    let recorded = calls.iter().rev().find(|call| call["llm"]["schema_name"] == schema_name);
    recorded
        .map(|call| call["response"].clone())
        .unwrap_or_else(|| panic!("{}: no scripted {schema_name} answer", case["name"]))
}

fn jev_key(state: &str, questions: &str) -> String {
    format!("{state}\n{questions}")
}

struct ReplayJev(Replies, Value);

impl DecisionClient for ReplayJev {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        let written: serde_json::Map<String, Value> =
            questions.iter().map(|(key, question)| (key.clone(), question.dump())).collect();
        let (state, questions) = (write(state, Style::HTTPX), write(&Value::Object(written.clone()), Style::HTTPX));
        let key = jev_key(&state, &questions);
        let request = serde_json::json!({"state": state, "questions": questions});
        let response = self.0.take(&key, request, |log| {
            scripted_jev(&self.1, &serde_json::from_str(&state).unwrap(), &Value::Object(written), log)
        });
        ready(Ok(serde_json::from_value(response["evaluation"].clone()).expect("evaluation")))
    }
}

fn llm_key(purpose: &str, messages: &str, schema_name: &str, schema: &str) -> String {
    format!("{purpose}\n{schema_name}\n{schema}\n{messages}")
}

struct ReplayLLM(Replies, Value);

impl LLMClient for ReplayLLM {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let written = write(&Value::Array(messages.iter().map(Dump::dump).collect()), Style::HTTPX);
            let (schema_name, schema_text) = (schema.name(), dumps(&schema.json_schema()));
            let key = llm_key(purpose.as_str(), &written, &schema_name, &schema_text);
            let request = serde_json::json!({
                "purpose": purpose.as_str(),
                "messages": written,
                "schema_name": schema_name,
                "schema": schema_text,
            });
            let response = self.0.take(&key, request, |log| scripted_llm(&self.1, purpose.as_str(), &schema_name, log));
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
    if outcome["wall"].is_object() {
        outcome["wall"]["seen_at_ms"] = 0.into();
    }
    outcome
}

/// Replays `case`, or with `log` records it: the case comes back with the calls made, the result and the events.
async fn replay(mut case: Value, log: Option<Arc<Mutex<Vec<Value>>>>) -> Value {
    let name = case["name"].as_str().unwrap().to_string();
    let jev = Replies::new(&case, "jev", log.clone(), |r| {
        jev_key(r["state"].as_str().unwrap(), r["questions"].as_str().unwrap())
    });
    let jev = Arc::new(ReplayJev(jev, case["jev"].clone()));
    let llm = Replies::new(&case, "llm", log.clone(), |r| {
        llm_key(
            r["purpose"].as_str().unwrap(),
            r["messages"].as_str().unwrap(),
            r["schema_name"].as_str().unwrap(),
            r["schema"].as_str().unwrap(),
        )
    });
    let llm = Arc::new(ReplayLLM(llm, case.clone()));
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
    let events = Value::Array(events.lock().unwrap().clone());
    if let Some(log) = log {
        case["calls"] = Value::Array(log.lock().unwrap().clone());
        case["outcome"] = normalized(result.dump());
        case["events"] = events;
        return case;
    }
    assert_eq!(normalized(result.dump()), normalized(case["outcome"].clone()), "{name}: result");
    assert_eq!(events, case["events"], "{name}: step events");
    case
}

#[tokio::test(start_paused = true)]
async fn whole_loops_match_python() {
    let update = std::env::var_os("FASTBROWSE_UPDATE_GOLDEN").is_some();
    let mut cases_out = Vec::new();
    for case in cases() {
        cases_out.push(replay(case, update.then(Arc::default)).await);
    }
    if update {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/agent.json");
        let mut out = Vec::new();
        let mut writer =
            serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b" "));
        serde::Serialize::serialize(&Value::Array(cases_out), &mut writer).unwrap();
        out.push(b'\n');
        fs::write(path, out).unwrap();
    }
}
