//! The bytes Python wrote, fixed in `tests/golden`, written again by Rust and compared byte for byte.

use std::fs;
use std::path::Path;

use fastbrowse_core::config::Config;
use fastbrowse_core::decision::{Evaluation, Question};
use fastbrowse_core::effects::{effect, state_key, step_key};
use fastbrowse_core::llm::Message;
use fastbrowse_core::models::*;
use fastbrowse_core::navigation::{navigation_urls, task_urls};
use fastbrowse_core::output_schema::output_model;
use fastbrowse_core::page::{ActResult, Action, Block, Capture, Control, Dialog, Observation, Tab, cut_text};
use fastbrowse_core::protocol::*;
use fastbrowse_core::py::datetime::DateTime;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{Floats, Style, dumps, float, write};
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{self, Redactor, downloads_question, irreversible_question};
use fastbrowse_core::schema::{Schema, Ty, json_schema, strict_schema};
use fastbrowse_core::telemetry::dollars;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    serde_json::from_str(&fs::read_to_string(path).expect("golden file")).expect("golden JSON")
}

fn text_of<'a>(case: &'a Value, key: &str) -> &'a str {
    case[key].as_str().unwrap_or_else(|| panic!("{key} in {case}"))
}

fn round_trip<T: DeserializeOwned + Serialize>(case: &Value) {
    let written = text_of(case, "dump_json");
    let parsed: T = serde_json::from_str(written).unwrap_or_else(|e| panic!("{}: {e}\n{written}", case["type"]));
    assert_eq!(parsed.dump_json(), written, "model_dump_json of {}", case["type"]);
    assert_eq!(dumps(&parsed.dump()), text_of(case, "dumps"), "json.dumps of {}", case["type"]);
    assert_eq!(dumps(&parsed.dump_exclude_none()), text_of(case, "exclude_none"), "exclude_none of {}", case["type"]);
    assert_eq!(write(&parsed.dump(), Style::HTTPX), text_of(case, "httpx"), "httpx body of {}", case["type"]);
}

#[test]
fn every_model_writes_the_bytes_python_writes() {
    let cases = golden("models.json");
    let cases = cases.as_array().unwrap();
    assert!(cases.len() > 50);
    for case in cases {
        match text_of(case, "type") {
            "SourceControl" => round_trip::<SourceControl>(case),
            "Evidence" => round_trip::<Evidence>(case),
            "Citation" => round_trip::<Citation>(case),
            "StepFact" => round_trip::<StepFact>(case),
            "Artifact" => round_trip::<Artifact>(case),
            "CostLine" => round_trip::<CostLine>(case),
            "CostBreakdown" => round_trip::<CostBreakdown>(case),
            "StepResult" => round_trip::<StepResult>(case),
            "BudgetStop" => round_trip::<BudgetStop>(case),
            "RunResult" => round_trip::<RunResult>(case),
            "BrowserConnection" => round_trip::<BrowserConnection>(case),
            "LocalChrome" => round_trip::<LocalChrome>(case),
            "Limits" => round_trip::<Limits>(case),
            "Authorization" => round_trip::<Authorization>(case),
            "SecretRef" => round_trip::<SecretRef>(case),
            "StepEvent" => round_trip::<StepEvent>(case),
            "BrowserEvent" => round_trip::<BrowserEvent>(case),
            "Control" => round_trip::<Control>(case),
            "Tab" => round_trip::<Tab>(case),
            "Dialog" => round_trip::<Dialog>(case),
            "Observation" => round_trip::<Observation>(case),
            "Block" => round_trip::<Block>(case),
            "Capture" => round_trip::<Capture>(case),
            "Action" => round_trip::<Action>(case),
            "ActResult" => round_trip::<ActResult>(case),
            "Config" => round_trip::<Config>(case),
            "Question" => round_trip::<Question>(case),
            "Evaluation" => round_trip::<Evaluation>(case),
            "Message" => round_trip::<Message>(case),
            "Request" => round_trip::<Request>(case),
            "Response" => round_trip::<Response>(case),
            "ErrorResponse" => round_trip::<ErrorResponse>(case),
            "InitializeResult" => round_trip::<InitializeResult>(case),
            "RunParams" => round_trip::<RunParams>(case),
            "RunCancelParams" => round_trip::<RunCancelParams>(case),
            "RunEvent" => round_trip::<RunEvent>(case),
            "RunFrame" => round_trip::<RunFrame>(case),
            "Notification" => round_trip::<Notification>(case),
            "ServerRequest" => round_trip::<ServerRequest>(case),
            "SecretsResolveParams" => round_trip::<SecretsResolveParams>(case),
            "RunUntilParams" => round_trip::<RunUntilParams>(case),
            other => panic!("no Rust type for {other}"),
        }
    }
}

fn model_ty(name: &str) -> Ty {
    match name {
        "SourceControl" => SourceControl::ty(),
        "Evidence" => Evidence::ty(),
        "CostLine" => CostLine::ty(),
        "BudgetStop" => BudgetStop::ty(),
        "StepFact" => StepFact::ty(),
        "StepResult" => StepResult::ty(),
        "StepEvent" => StepEvent::ty(),
        "Limits" => Limits::ty(),
        "Evaluation" => Evaluation::ty(),
        other => panic!("no schema for {other}"),
    }
}

#[test]
fn every_schema_is_the_one_pydantic_writes() {
    let cases = golden("schemas.json");
    for case in cases.as_array().unwrap() {
        let name = text_of(case, "type");
        let schema = match case.get("output") {
            Some(source) => output_model(source).unwrap().json_schema(),
            None => json_schema(&model_ty(name)),
        };
        assert_eq!(dumps(&schema), text_of(case, "schema"), "schema of {name}");
        let strict = match strict_schema(&schema) {
            Ok(strict) => dumps(&strict),
            Err(error) => format!("error: {error}"),
        };
        assert_eq!(strict, text_of(case, "strict"), "strict schema of {name}");
    }
}

fn fixtures() -> (Observation, Capture) {
    let models = golden("models.json");
    let find = |name: &str| {
        let case = models.as_array().unwrap().iter().find(|case| case["type"] == name).unwrap();
        text_of(case, "dump_json").to_string()
    };
    (serde_json::from_str(&find("Observation")).unwrap(), serde_json::from_str(&find("Capture")).unwrap())
}

#[test]
fn floats_and_strings_are_spelled_as_python_spells_them() {
    let functions = golden("functions.json");
    for case in functions["floats"].as_array().unwrap() {
        let value = case["value"].as_f64().unwrap();
        assert_eq!(float(value, Floats::Repr), text_of(case, "repr"), "repr of {value}");
        let line = CostLine::new(CostComponent::Jev, CostBasis::Metered, Some(value.abs()));
        assert_eq!(line.dump_json(), text_of(case, "pydantic"), "pydantic of {value}");
    }
    for case in functions["strings"].as_array().unwrap() {
        let value = text_of(case, "value");
        assert_eq!(dumps(&Value::from(value)), text_of(case, "dumps"));
        assert_eq!(write(&Value::from(value), Style::DUMPS.unicode()), text_of(case, "unicode"));
        assert_eq!(text::repr(value), text_of(case, "repr"), "repr of {value:?}");
        let control = SourceControl { role: "r".into(), label: value.into(), context: None };
        assert_eq!(control.dump_json(), text_of(case, "pydantic"));
    }
    for case in functions["dollars"].as_array().unwrap() {
        assert_eq!(dollars(case["value"].as_f64().unwrap()), text_of(case, "shown"));
    }
}

#[test]
fn page_keys_and_dates_match_python() {
    let functions = golden("functions.json");
    let (observation, capture) = fixtures();
    assert_eq!(state_key(&observation), text_of(&functions, "state_key"));
    assert_eq!(step_key(&observation, &capture), text_of(&functions, "step_key"));
    assert_eq!(capture.read_key(), text_of(&functions, "read_key"));
    let dates = [
        observation.captured_at,
        DateTime::new(2026, 1, 31, 0, 0, 0),
        DateTime::new(2024, 2, 29, 23, 59, 0),
        DateTime::new(2026, 12, 15, 0, 0, 0).utc(),
    ];
    for (when, today) in dates.into_iter().zip(functions["today"].as_array().unwrap()) {
        let dated = Observation { captured_at: when, ..observation.clone() };
        assert_eq!(dated.today(), today.as_str().unwrap());
    }
    for case in functions["cut_text"].as_array().unwrap() {
        let cut = cut_text(text_of(case, "text"), case["max"].as_u64().unwrap() as usize, case["json"] == true);
        assert_eq!(cut, text_of(case, "cut"));
    }
}

#[test]
fn the_effect_summary_matches_python() {
    let functions = golden("functions.json");
    let (observation, _) = fixtures();
    let field = observation.controls[0].clone();
    let before = Observation { document_key: "doc".into(), ..observation.clone() };
    let mut after = before.clone();
    after.url = "https://example.test/next".into();
    after.response_status = Some(404);
    let mut filled = field.clone();
    filled.value = Some("London".into());
    filled.expanded = Some(false);
    let mut checked = observation.controls[2].clone();
    checked.checked = Some(true);
    let long = Control::new("n1", "button", &"x".repeat(70), []);
    after.controls = vec![filled, checked, long];
    assert_eq!(effect(&before, &after, Some(&field)).summary, text_of(&functions, "effect"));
}

#[test]
fn the_questions_jev_is_asked_are_the_bytes_python_sends() {
    let functions = golden("functions.json");
    let questions = &functions["questions"];
    let mut target = Control::new("b", "button", "Yes, I'm sure", [Operation::Click]);
    target.context = Some("Delete \"all\" \u{200b}".into());
    target.submit_semantics = Some("POST /delete".into());
    let mut destination = Control::new("d", "listitem", "Trash", [Operation::Click]);
    destination.context = Some("Sidebar".into());
    let irreversible = irreversible_question("Tidy up \u{e9}", Operation::Click, &target, Some(&destination));
    assert_eq!(irreversible.dump_json(), text_of(questions, "irreversible"));
    assert_eq!(
        irreversible_question("Send it", Operation::Enter, &target, None).dump_json(),
        text_of(questions, "enter")
    );
    // A link names where it goes, unless its label already says so; only a click follows it.
    let mut link = Control::new("l1", "link", "Next \u{203a}", [Operation::Click]);
    link.href = Some("/page/2".into());
    assert_eq!(irreversible_question("Go on", Operation::Click, &link, None).dump_json(), text_of(questions, "link"));
    assert_eq!(
        irreversible_question("Go on", Operation::Enter, &link, None).dump_json(),
        text_of(questions, "link_enter")
    );
    assert_eq!(
        downloads_question("Read the report, do not save anything").dump_json(),
        text_of(questions, "downloads")
    );
}

#[test]
fn redaction_matches_python() {
    let functions = golden("functions.json");
    let mut redactor = Redactor::new();
    redactor.register("username", "practice", Some("https://practice.expandtesting.com/login"));
    redactor.register("password", "p@ss w\"ord\u{e9}", None);
    for case in functions["redact"].as_array().unwrap() {
        let text = text_of(case, "text");
        assert_eq!(redactor.redact(text), text_of(case, "redacted"), "{text}");
        assert_eq!(redactor.mask(text), text_of(case, "masked"), "{text}");
    }
    for case in functions["safety_origin"].as_array().unwrap() {
        assert_eq!(safety::origin_of(text_of(case, "url")), text_of(case, "origin"));
    }
}

#[test]
fn task_addresses_match_python() {
    let functions = golden("functions.json");
    for case in functions["task_urls"].as_array().unwrap() {
        let task = text_of(case, "task");
        let start = case["start"].as_str();
        let urls: Vec<Value> = task_urls(task, start, true).into_iter().map(Value::from).collect();
        assert_eq!(Value::from(urls), case["urls"], "{task}");
        let navigation: Vec<Value> =
            navigation_urls(task, "https://example.test/", start, None, false).into_iter().map(Value::from).collect();
        assert_eq!(Value::from(navigation), case["navigation"], "{task}");
    }
}

#[test]
fn output_data_is_dumped_as_python_dumps_it() {
    let functions = golden("functions.json");
    let sources = golden("schemas.json");
    for case in functions["validate"].as_array().unwrap() {
        let name = text_of(case, "schema");
        let source =
            sources.as_array().unwrap().iter().find(|s| s["type"] == name && s.get("output").is_some()).unwrap();
        let model = output_model(&source["output"]).unwrap();
        let dumped = model.validate(&case["data"]).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(dumps(&dumped), text_of(case, "dumped"), "{name}");
    }
}
