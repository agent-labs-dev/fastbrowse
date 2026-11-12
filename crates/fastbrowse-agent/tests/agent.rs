//! Replays whole agent loops recorded in `tests/golden/agent.json`: the same scripted site, every model request
//! looked up by its bytes (so requests sent side by side may leave in any order), and the same result.
//!
//! `FASTBROWSE_UPDATE_GOLDEN=1` re-records them after an intended behaviour change: recorded answers are kept, a new
//! request is answered from the case's `jev` and `llm` scripts, and the diff of `agent.json` is the behaviour change.

mod common;

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
/// under `pick` any other choice, `nouls` a probability per noul (a list is taken in turn) and `noul` the rest, `confidence` the pick's.
/// `recover` gives the picks of a call asked again while recovering.
/// The wall questions default to no.
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
            // A case's answer is the one it expects to stand, so the checks of that answer pass unless it scripts them.
            let default =
                if matches!(key.as_str(), "login_required" | "bot_check" | "site_failure" | "unobtainable" | "dates") {
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
        let general = (key != "operation" && !key.ends_with("_target") && !key.ends_with("_group"))
            .then(|| &script["pick"]["pick"]);
        // A call asked again while recovering takes `recover`'s pick when the case scripts one.
        let recovering = asked_again.then(|| script["recover"][key].as_str()).flatten().map(Value::from);
        let picked = recovering
            .as_ref()
            .or_else(|| nth(&script["pick"][key], answered(key)))
            .or_else(|| general.and_then(|pick| nth(pick, 0)))
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| (key == "operation").then(|| "done".to_string()));
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
    // A case recorded before the run asked for known pages (#983) knows of none.
    if schema_name == "KnownPages" && case["llm"].get(schema_name).is_none() {
        return serde_json::json!({"data": {"urls": []}, "cost": cost});
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
    let (result, events) = play(&case, log.clone()).await;
    let events = Value::Array(events);
    if let Some(log) = log {
        case["calls"] = Value::Array(log.lock().unwrap().clone());
        case["outcome"] = normalized(result);
        case["events"] = events;
        return case;
    }
    assert_eq!(normalized(result), normalized(case["outcome"].clone()), "{name}: result");
    assert_eq!(events, case["events"], "{name}: step events");
    case
}

/// The result and step events of a run against the case's scripted site and recorded model replies.
async fn play(case: &Value, log: Option<Arc<Mutex<Vec<Value>>>>) -> (Value, Vec<Value>) {
    let jev = Replies::new(case, "jev", log.clone(), |r| {
        jev_key(r["state"].as_str().unwrap(), r["questions"].as_str().unwrap())
    });
    let jev = Arc::new(ReplayJev(jev, case["jev"].clone()));
    let llm = Replies::new(case, "llm", log.clone(), |r| {
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
    // The recorded loops predate the plan asked with each decision, so they replay without it; the plan has its own
    // tests in `policy.rs`.
    if config.get("plan_ahead").is_none() {
        config["plan_ahead"] = false.into();
    }
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
    let events = events.lock().unwrap().clone();
    (result.dump(), events)
}

#[tokio::test(start_paused = true)]
async fn whole_loops_match_python() {
    let update = common::capture::updating();
    let mut cases_out = Vec::new();
    for case in cases() {
        cases_out.push(replay(case, update.then(Arc::default)).await);
    }
    if update {
        common::save("agent.json", &Value::Array(cases_out));
    }
}

#[tokio::test(start_paused = true)]
async fn a_blank_navigation_waits_for_its_content_before_asking_jev() {
    let mut case = cases().into_iter().find(|case| case["name"] == "a step limit ends budget_exceeded").unwrap();
    case["name"] = "a blank navigation waits for its content".into();
    case["calls"] = serde_json::json!([]);
    case["limits"]["max_steps"] = 2.into();
    case["jev"]["pick"] = serde_json::json!({"operation": "click", "click_target": ["go", "next"]});
    let mut home = case["site"]["screens"]["home"].clone();
    home["observation"]["controls"] = serde_json::json!([
        {"id": "go", "frame_id": null, "role": "button", "label": "Go", "operations": ["click"]}
    ]);
    let mut loading = case["site"]["screens"]["blank"].clone();
    loading["observation"]["url"] = "https://books.test/catalogue".into();
    loading["observation"]["page_key"] = "loading".into();
    loading["observation"]["document_key"] = "catalogue".into();
    loading["settles"] = serde_json::json!({"after": 1, "to": "catalogue"});
    let mut catalogue = home.clone();
    catalogue["observation"]["url"] = "https://books.test/catalogue".into();
    catalogue["observation"]["page_key"] = "catalogue-ready".into();
    catalogue["observation"]["document_key"] = "catalogue".into();
    catalogue["observation"]["controls"] = serde_json::json!([
        {"id": "next", "frame_id": null, "role": "button", "label": "Next", "operations": ["click"]}
    ]);
    case["site"] = serde_json::json!({
        "start": "home", "urls": {"https://books.test/": "home"},
        "screens": {"home": home, "loading": loading, "catalogue": catalogue},
        "actions": [{"screen": "home", "operation": "click", "target": "go", "next": "loading"}]
    });
    let log: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (outcome, _) = play(&case, Some(log.clone())).await;
    assert_eq!(outcome["status"], "budget_exceeded");
    let log = log.lock().unwrap();
    for call in log.iter().filter(|call| call["jev"].is_object()) {
        let questions: Value = serde_json::from_str(call["jev"]["questions"].as_str().unwrap()).unwrap();
        if questions.get("operation").is_some() {
            let state: Value = serde_json::from_str(call["jev"]["state"].as_str().unwrap()).unwrap();
            assert!(!state["elements"].as_array().unwrap().is_empty(), "Jev was asked on the loading document");
        }
    }
    assert_eq!(outcome["steps"][1]["target"], "Next");
}

/// A product page saying the task's item is sold out, for a task that needs it in a basket.
fn sold_out_case(quote: &str) -> Value {
    let mut case = cases().into_iter().find(|case| case["name"] == "a step limit ends budget_exceeded").unwrap();
    case["name"] = "a page saying the task's item is sold out".into();
    case["task"] = "Add a Raspberry Pi Pico 2 W to the basket".into();
    case["calls"] = serde_json::json!([]);
    case["limits"]["max_steps"] = 5.into();
    case["jev"]["pick"] = serde_json::json!({"operation": "click"});
    case["jev"]["nouls"]["unobtainable"] = 0.97.into();
    case["llm"]["UnobtainableItem"] =
        serde_json::json!({"item": "Raspberry Pi Pico 2 W", "reason": "it is marked sold out", "quote": quote});
    let text = "Raspberry Pi Pico 2 W\n\nSold out";
    let home = &mut case["site"]["screens"]["home"];
    home["observation"]["url"] = "https://shop.test/pico-2-w".into();
    home["observation"]["viewport_text"] = text.into();
    home["observation"]["controls"] = serde_json::json!([
        {"id": "home", "frame_id": null, "role": "link", "label": "Home", "operations": ["click"]}
    ]);
    home["capture"]["url"] = "https://shop.test/pico-2-w".into();
    home["capture"]["text"] = text.into();
    home["capture"]["blocks"][1]["end"] = 31.into();
    home["capture"]["blocks"][0]["end"] = 21.into();
    home["capture"]["blocks"][1]["start"] = 23.into();
    case["site"]["urls"] = serde_json::json!({"https://shop.test/pico-2-w": "home"});
    case["start"] = "https://shop.test/pico-2-w".into();
    case
}

#[tokio::test(start_paused = true)]
async fn a_page_saying_the_tasks_item_is_sold_out_ends_the_run_unavailable_naming_it() {
    // #960: a run that read "Sold out" on its first step spent 74 more steps because nothing asked whether the task
    // could still be done.
    let (outcome, _) = play(&sold_out_case("Sold out"), Some(Arc::default())).await;
    assert_eq!(outcome["status"], "unavailable", "{outcome}");
    assert!(outcome["steps"].as_array().unwrap().is_empty(), "{outcome}");
    let error = outcome["error"].as_str().unwrap();
    assert!(error.contains("Raspberry Pi Pico 2 W") && error.contains("sold out"), "{error}");
    assert_eq!(outcome["evidence"][0]["quote"], "Sold out", "{outcome}");
    // A quote the page does not hold cites nothing and leaves the model's item and reason unsupported, so the stop
    // names neither, and the run still ends as Jev judged it.
    let (outcome, _) = play(&sold_out_case("Out of stock"), Some(Arc::default())).await;
    assert_eq!(outcome["status"], "unavailable", "{outcome}");
    let error = "https://shop.test/pico-2-w says an item the task requires cannot be bought.";
    assert_eq!(outcome["error"], error, "{outcome}");
    assert_eq!(outcome["evidence"], serde_json::json!([]), "{outcome}");
}

#[tokio::test(start_paused = true)]
async fn a_date_the_task_names_reaches_each_step_worked_out() {
    let mut case = cases().into_iter().find(|case| case["name"] == "a step limit ends budget_exceeded").unwrap();
    case["name"] = "a date the task names".into();
    case["task"] = "Book the first Friday of December".into();
    case["calls"] = serde_json::json!([]);
    case["jev"]["nouls"]["dates"] = 0.9.into();
    case["llm"]["TaskDates"] = serde_json::json!({"dates": [{
        "text": "the first Friday of December", "year": null, "month": 12, "day": null, "weekday": "friday",
        "nth": 1, "days_from_today": null, "months_from_now": null
    }]});
    let log: Arc<Mutex<Vec<Value>>> = Arc::default();
    play(&case, Some(log.clone())).await;
    let log = log.lock().unwrap();
    let steps: Vec<Value> = log
        .iter()
        .filter(|call| call["jev"]["questions"].as_str().is_some_and(|q| q.contains("\"operation\":")))
        .map(|call| serde_json::from_str(call["jev"]["state"].as_str().unwrap()).unwrap())
        .collect();
    assert!(!steps.is_empty());
    for state in steps {
        let date = state["date"].as_str().unwrap();
        assert!(date.ends_with("\nthe first Friday of December: 2026-12-04 (Friday)"), "{date}");
        assert!(!date.contains("Mon "), "no calendar: {date}");
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
        let ran = replay(case, Some(Arc::default())).await;
        let outcome = &ran["outcome"];
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

/// Asking for the choices adds them to the steps Jev decided and changes nothing else about the run: not the steps, the
/// events, nor which control Jev picked, which is the one marked chosen.
#[tokio::test(start_paused = true)]
async fn step_choices_list_what_jev_was_offered_and_change_nothing_else() {
    let names = [
        "a select asks Jev which option, then a step limit ends the run",
        "a stored secret is typed at fill time and never shown to a model",
    ];
    let picked: Vec<Value> =
        cases().into_iter().filter(|case| names.iter().any(|name| case["name"].as_str() == Some(name))).collect();
    assert_eq!(picked.len(), names.len());
    for mut case in picked {
        let name = case["name"].as_str().unwrap().to_string();
        if case["config"].is_null() {
            case["config"] = serde_json::json!({});
        }
        case["config"]["step_choices"] = true.into();
        let (mut result, mut events) = play(&case, None).await;
        let mut listed = 0;
        let steps = result["steps"].as_array_mut().unwrap().iter_mut();
        for step in steps.chain(events.iter_mut().map(|event| &mut event["step"])) {
            let Some(choices) = step.as_object_mut().unwrap().remove("choices") else { continue };
            listed += 1;
            let chosen: Vec<&Value> = choices.as_array().unwrap().iter().filter(|c| c["chosen"] == true).collect();
            // A read or DONE takes no target, so its list has nothing chosen; a drag marks its destination too.
            assert!(chosen.len() <= 2, "{name}: {choices}");
            if let Some(target) = step["target"].as_str().filter(|_| step["decided_by"] == "jev") {
                let labels = chosen.iter().map(|choice| choice["label"].as_str().unwrap());
                assert!(labels.into_iter().any(|label| target.contains(label)), "{name}: {step}");
            }
            assert!(!choices.to_string().contains("hunter2"), "{name}: {choices}");
        }
        assert!(listed > 0, "{name}: no step listed its choices");
        assert_eq!(normalized(result), normalized(case["outcome"].clone()), "{name}: result");
        assert_eq!(Value::Array(events), case["events"], "{name}: step events");
    }
}

#[tokio::test(start_paused = true)]
async fn sign_in_judgments_get_the_navigation_source_and_still_decide_whether_it_blocks() {
    // The page question says only that a wall is there; whether it stops the run is the task's account question.
    for (name, account, opening_wall, expected) in [
        ("public alternative", 0.1, false, "budget_exceeded"),
        ("authenticated destination", 0.9, false, "needs_login"),
        ("opening sign-in wall", 0.9, true, "needs_login"),
    ] {
        let mut case = cases().into_iter().find(|case| case["name"] == "a step limit ends budget_exceeded").unwrap();
        case["name"] = name.into();
        case["calls"] = serde_json::json!([]);
        case["limits"]["max_steps"] = 3.into();
        // The wall's own pick is set aside for the recovery's, which goes back.
        case["jev"]["pick"] = serde_json::json!({
            "operation": ["click", "fill", "click"], "click_target": ["query", "public"]
        });
        case["jev"]["recover"] = serde_json::json!({"operation": "back"});
        case["jev"]["nouls"]["login_required"] =
            if opening_wall { serde_json::json!([0.95]) } else { serde_json::json!([0.12, 0.95, 0.12]) };
        case["jev"]["nouls"]["account"] = account.into();
        let mut home = case["site"]["screens"]["home"].clone();
        home["observation"]["document_key"] = "public-document".into();
        home["observation"]["viewport_text"] = "Browse public inventory or query your account".into();
        home["observation"]["controls"] = serde_json::json!([
            {"id": "query", "frame_id": null, "role": "button", "label": "Account query", "operations": ["click"]},
            {"id": "public", "frame_id": null, "role": "button", "label": "Public inventory", "operations": ["click"]}
        ]);
        let mut wall = home.clone();
        wall["observation"]["url"] = "https://books.test/login".into();
        wall["observation"]["document_key"] = "login-document".into();
        wall["observation"]["page_key"] = "login".into();
        wall["observation"]["can_go_back"] = (!opening_wall).into();
        wall["observation"]["viewport_text"] = "Sign in to query your account".into();
        wall["observation"]["controls"] = serde_json::json!([
            {"id": "password", "frame_id": null, "role": "textbox", "label": "Password",
             "operations": ["fill"], "input_type": "password"}
        ]);
        case["site"] = serde_json::json!({
            "start": if opening_wall {"wall"} else {"home"},
            "urls": {"https://books.test/": if opening_wall {"wall"} else {"home"}},
            "screens": {"home": home, "wall": wall},
            "actions": [{"screen": "home", "operation": "click", "target": "query", "next": "wall"}]
        });
        let log: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (outcome, _) = play(&case, Some(log.clone())).await;
        assert_eq!(outcome["status"], expected, "{name}: {outcome}");
        let log = log.lock().unwrap();
        let wall_call = log
            .iter()
            .find_map(|call| {
                let state: Value = serde_json::from_str(call["jev"]["state"].as_str()?).ok()?;
                (state["page"]["url"] == "https://books.test/login").then_some((call, state))
            })
            .expect("login judgment");
        let facts = wall_call.1["page_facts"].to_string();
        assert_eq!(facts.contains("Public inventory"), !opening_wall, "{name}: {facts}");
        if !opening_wall {
            assert!(facts.contains("login_required_probability") && facts.contains("0.12"));
            let questions: Value = serde_json::from_str(wall_call.0["jev"]["questions"].as_str().unwrap()).unwrap();
            let wall = questions["login_required"]["instructions"].as_str().unwrap();
            assert!(
                wall.contains("Is signing in what this page asks of its visitor") && !wall.contains("task"),
                "{wall}"
            );
        }
        // What the task needs is asked of the task alone, once.
        let asked: Vec<Value> = log
            .iter()
            .filter(|call| call["jev"]["questions"].as_str().is_some_and(|q| q.contains("\"account\":")))
            .map(|call| serde_json::from_str(call["jev"]["state"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(asked.len(), 1, "{name}");
        assert_eq!(asked[0].as_object().unwrap().keys().collect::<Vec<_>>(), ["task"], "{name}");
        if expected == "budget_exceeded" {
            assert_eq!(outcome["steps"][1]["operation"], "back");
            assert_eq!(outcome["steps"][2]["target"], "Public inventory");
        }
    }
}

/// A status page whose line a wait changes, read before and after the wait: two captures, each quoted once.
fn status_case() -> Value {
    let mut case = cases()
        .into_iter()
        .find(|case| case["name"] == "an HTTP 404 page goes to Jev, and its DONE ends the run")
        .unwrap();
    case["name"] = "read, wait, read".into();
    case["calls"] = serde_json::json!([]);
    case["task"] = "Open https://status.test/ and report its status line, then wait a few seconds and report the \
                    status line again"
        .into();
    case["start"] = "https://status.test/".into();
    case["jev"]["pick"] = serde_json::json!({"operation": ["read", "wait", "read", "done"]});
    // Jev holds each claim supported by its quote, and the two together to cover the task.
    case["jev"]["nouls"] = serde_json::json!({"claim0": 0.9, "claim1": 0.9, "coverage": 0.9});
    case["llm"]["AnswerDraft"] = serde_json::json!([{"claims": [
        {"text": ["First: live, updated 16:59:21"], "evidence_ids": ["e0"]},
        {"text": ["After waiting: live, updated 16:59:27"], "evidence_ids": ["e1"]}
    ]}]);
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

/// A task that reads a value, waits and reads it again finishes: Jev can pick a wait, the page moves on under it, and
/// the second read cites what the page shows afterwards.
#[tokio::test(start_paused = true)]
async fn a_task_that_reads_waits_and_reads_again_completes_with_both_readings() {
    let (outcome, _) = play(&status_case(), Some(Arc::default())).await;
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
    let (outcome, _) = play(&case, Some(Arc::default())).await;
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

fn list_case() -> Value {
    let mut case = status_case();
    case["name"] = "read a paginated list".into();
    case["task"] = "Report the status from every page of the list at https://list.test/.".into();
    case["start"] = "https://list.test/".into();
    case["config"] = serde_json::json!({"plan_ahead":true});
    case["jev"]["pick"] = serde_json::json!({
        "operation": ["read_list", "read_list", "read_list", "done"],
        "extent": "every_page", "pick": "text", "steps": "reads"
    });
    case["jev"]["nouls"] = serde_json::json!({
        "claim0": 0.9, "claim1": 0.9, "claim2": 0.9, "coverage": 0.9, "actions": 0.9
    });
    let template = case["site"]["screens"]["early"].clone();
    let mut screens = serde_json::Map::new();
    screens.insert("blank".into(), case["site"]["screens"]["blank"].clone());
    let mut urls = serde_json::Map::new();
    let mut claims = Vec::new();
    for (index, name) in ["Alpha", "Beta", "Gamma"].into_iter().enumerate() {
        let url =
            if index == 0 { "https://list.test/".into() } else { format!("https://list.test/page/{}/", index + 1) };
        let key = format!("page{index}");
        let text = format!("Status {name}.");
        let mut screen = template.clone();
        screen["observation"]["url"] = url.clone().into();
        screen["observation"]["page_key"] = key.clone().into();
        screen["observation"]["document_key"] = key.clone().into();
        screen["observation"]["viewport_text"] = text.clone().into();
        let controls = if index < 2 {
            vec![
                fastbrowse_core::page::Control {
                    next_page: Some(true),
                    href: Some(format!("https://list.test/page/{}/", index + 2)),
                    ..fastbrowse_core::page::Control::new("next", "link", "Next", [Operation::Click])
                }
                .dump(),
            ]
        } else {
            Vec::new()
        };
        screen["observation"]["controls"] = controls.into();
        screen["capture"]["url"] = url.clone().into();
        screen["capture"]["text"] = text.clone().into();
        screen["capture"]["sha256"] = fastbrowse_core::py::sha256_hex(text.as_bytes()).into();
        screen["capture"]["blocks"][0]["end"] = text.chars().count().into();
        claims.push(serde_json::json!({"text": [text], "evidence_ids": [format!("e{index}")]}));
        screens.insert(key.clone(), screen);
        urls.insert(url, key.into());
    }
    case["site"] = serde_json::json!({"start":"blank", "urls": urls, "screens":screens, "actions": []});
    case["llm"]["AnswerDraft"] = serde_json::json!([{"claims":claims}]);
    case
}

#[tokio::test(start_paused = true)]
async fn a_list_read_keeps_each_pages_citations_and_checks_without_target_decisions() {
    let log = Arc::default();
    let (outcome, _) = play(&list_case(), Some(Arc::clone(&log))).await;
    assert_eq!(outcome["status"], "complete", "{outcome}");
    let evidence = outcome["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 3, "{outcome}");
    for url in ["https://list.test/", "https://list.test/page/2/", "https://list.test/page/3/"] {
        assert!(evidence.iter().any(|quote| quote["url"] == url), "{outcome}");
    }
    let log = log.lock().unwrap();
    let continuation: Vec<Value> = log
        .iter()
        .filter_map(|call| {
            let questions: Value = serde_json::from_str(call["jev"]["questions"].as_str()?).ok()?;
            questions["operation"]["criteria"].get("stop_list").map(|_| questions.clone())
        })
        .collect();
    assert_eq!(continuation.len(), 2, "{log:?}");
    for questions in continuation {
        for key in ["login_required", "bot_check", "site_failure", "unobtainable", "shows_answer"] {
            assert!(questions.get(key).is_some(), "{questions}");
        }
        assert!(questions.get("click_target").is_none(), "{questions}");
    }
    let navigations: Vec<_> =
        outcome["steps"].as_array().unwrap().iter().filter(|step| step["operation"] == "navigate").collect();
    assert_eq!(navigations.len(), 2);
    assert!(navigations.iter().all(|step| step["decided_by"] == "code"));
}

#[tokio::test(start_paused = true)]
async fn a_list_read_checks_its_step_budget_before_the_next_navigation() {
    let mut case = list_case();
    case["limits"] = serde_json::json!({"max_steps":3});
    let (outcome, _) = play(&case, Some(Arc::default())).await;
    assert_eq!(outcome["status"], "budget_exceeded", "{outcome}");
    assert_eq!(outcome["steps"].as_array().unwrap().len(), 3, "{outcome}");
    assert_eq!(outcome["final_url"], "https://list.test/page/2/", "{outcome}");
    assert!(outcome["evidence"].as_array().unwrap().iter().all(|quote| quote["url"] != "https://list.test/page/3/"));
}

#[tokio::test(start_paused = true)]
async fn a_list_read_checks_login_on_the_next_page_before_reading_it() {
    let mut case = list_case();
    case["jev"]["nouls"]["login_required"] = serde_json::json!([0.0, 1.0]);
    case["jev"]["nouls"]["account"] = 1.0.into();
    let log = Arc::default();
    let (outcome, _) = play(&case, Some(Arc::clone(&log))).await;
    assert_eq!(outcome["status"], "needs_login", "{outcome}");
    assert_eq!(outcome["steps"].as_array().unwrap().len(), 2, "{outcome}");
    assert!(outcome["evidence"].as_array().unwrap().iter().all(|quote| quote["url"] == "https://list.test/"));
    assert_eq!(outcome["final_url"], "https://list.test/page/2/");
}

#[tokio::test(start_paused = true)]
async fn a_failed_page_cannot_spin_a_list_read_outside_the_step_budget() {
    let mut case = list_case();
    case["limits"] = serde_json::json!({"max_steps": 2});
    case["jev"]["nouls"]["site_failure"] = 1.0.into();
    let (outcome, _) = play(&case, Some(Arc::default())).await;
    assert_eq!(outcome["status"], "budget_exceeded", "{outcome}");
}

#[tokio::test(start_paused = true)]
async fn a_list_handoff_restores_target_choices_without_recording_a_step() {
    let mut case = list_case();
    case["limits"] = serde_json::json!({"max_steps": 3});
    case["jev"]["pick"]["operation"] = serde_json::json!(["read_list", "stop_list", "wait"]);
    let log = Arc::default();
    let (outcome, _) = play(&case, Some(Arc::clone(&log))).await;
    assert_eq!(outcome["status"], "budget_exceeded", "{outcome}");
    assert_eq!(outcome["steps"].as_array().unwrap().len(), 3);
    let decisions: Vec<Value> = log
        .lock()
        .unwrap()
        .iter()
        .filter_map(|call| {
            let q: Value = serde_json::from_str(call["jev"]["questions"].as_str()?).ok()?;
            q.get("operation").map(|_| q.clone())
        })
        .collect();
    assert!(decisions[1]["operation"]["criteria"].get("stop_list").is_some());
    assert!(decisions[2]["operation"]["criteria"].get("stop_list").is_none());
    assert!(decisions[2].get("click_target").is_some());
}

#[tokio::test(start_paused = true)]
async fn a_credentialed_list_wall_returns_to_actions_before_reading_it() {
    let mut case = list_case();
    case["limits"] = serde_json::json!({"max_steps": 3});
    case["secrets"] = serde_json::json!({"password": ["test-password", ["https://list.test"]]});
    case["jev"]["nouls"]["login_required"] = 1.0.into();
    case["jev"]["nouls"]["credentials"] = 1.0.into();
    case["jev"]["nouls"]["account"] = 1.0.into();
    case["jev"]["pick"]["operation"] = serde_json::json!(["read_list", "read_list", "wait"]);
    let (outcome, _) = play(&case, Some(Arc::default())).await;
    assert_eq!(outcome["status"], "budget_exceeded", "{outcome}");
    assert!(
        outcome["evidence"].as_array().unwrap().iter().all(|quote| quote["url"] == "https://list.test/"),
        "{outcome}"
    );
    assert_eq!(outcome["final_url"], "https://list.test/page/2/");
}
