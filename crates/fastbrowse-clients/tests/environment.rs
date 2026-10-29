//! tests/clients/test_environment.py: settings choose the Jev and LLM routes, and a custom endpoint gets only its own
//! key. Python builds `Settings(**values)`; these tests give the environment variables those values come from, which
//! also covers what pydantic-settings does for Python: names without regard to case, and `.env` under the environment.

mod common;

use std::sync::{Arc, Mutex};

use common::{Script, answer_model, answering};
use fastbrowse_clients::environment::{ConfigurationError, JevSource, LLMSource, Settings, default_model};
use fastbrowse_clients::http::{Http, Request, Response};
use fastbrowse_core::jev::{JevError, NoulQuestion, Question};
use fastbrowse_core::llm::{Generation, Message};
use fastbrowse_core::models::{CostBasis, CostBreakdown, LLMPurpose};
use indexmap::IndexMap;
use serde_json::{Value, json};

fn settings(env: &[(&str, &str)]) -> Settings {
    Settings::from_sources(env, None).unwrap()
}

fn question() -> IndexMap<String, Question> {
    IndexMap::from([("q".to_string(), NoulQuestion::new("Is it?", None, None).into())])
}

/// Every request's URL, each answered with `status`.
fn urls(status: u16) -> (Http, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    let http = answering(move |request| {
        kept.lock().unwrap().push(request.url.clone());
        Box::pin(async move { Ok(Response::json(status, &json!({"error": "stop"}))) })
    });
    (http, seen)
}

fn config_error<T>(built: Result<T, ConfigurationError>) -> String {
    match built {
        Ok(_) => panic!("expected a configuration error"),
        Err(error) => error.to_string(),
    }
}

#[tokio::test(start_paused = true)]
async fn test_jev_requests_go_to_the_chosen_source() {
    let cases: [(&[(&str, &str)], &str); 8] = [
        (&[("OPENROUTER_API_KEY", "o")], "https://openrouter.ai/api/v1/systemone"),
        (
            &[("OPENROUTER_API_KEY", "o"), ("TYPESAFE_API_KEY", "t"), ("AI_GATEWAY_API_KEY", "g")],
            "https://api.typesafe.ai/v1/systemone",
        ),
        (
            &[("OPENROUTER_API_KEY", "o"), ("TYPESAFE_API_KEY", "t"), ("FASTBROWSE_JEV_SOURCE", "openrouter")],
            "https://openrouter.ai/api/v1/systemone",
        ),
        (&[("TYPESAFE_API_KEY", "t"), ("AI_GATEWAY_API_KEY", "g")], "https://api.typesafe.ai/v1/systemone"),
        (
            &[("TYPESAFE_API_KEY", "t"), ("AI_GATEWAY_API_KEY", "g"), ("FASTBROWSE_JEV_SOURCE", "gateway")],
            "https://ai-gateway.vercel.sh/v4/ai/evaluation-model",
        ),
        (
            &[("AI_GATEWAY_API_KEY", "g"), ("FASTBROWSE_JEV_BASE_URL", "http://proxy:8080/")],
            "http://proxy:8080/v4/ai/evaluation-model",
        ),
        (
            &[("TYPESAFE_API_KEY", "t"), ("FASTBROWSE_JEV_BASE_URL", "http://proxy:8080")],
            "http://proxy:8080/v1/systemone",
        ),
        (
            &[("OPENROUTER_API_KEY", "o"), ("FASTBROWSE_JEV_BASE_URL", "http://proxy:8080/")],
            "http://proxy:8080/v1/systemone",
        ),
    ];
    for (env, expected) in cases {
        let (http, seen) = urls(400);
        let jev = settings(env).jev(&http).unwrap();
        assert!(jev.evaluate(&json!("page"), &question()).await.is_err());
        assert_eq!(*seen.lock().unwrap(), [expected], "{env:?}");
    }
}

#[test]
fn test_a_chosen_source_without_its_key_is_a_configuration_error() {
    let keys = ["OPENROUTER_API_KEY", "TYPESAFE_API_KEY", "AI_GATEWAY_API_KEY"];
    for (source, missing) in [("openrouter", keys[0]), ("typesafe", keys[1]), ("gateway", keys[2])] {
        let mut env: Vec<(&str, &str)> = keys.iter().filter(|key| **key != missing).map(|key| (*key, "key")).collect();
        env.push(("FASTBROWSE_JEV_SOURCE", source));
        let (http, _) = urls(400);
        assert!(config_error(settings(&env).jev(&http)).contains(missing), "{source}");
    }
}

#[tokio::test(start_paused = true)]
async fn test_failover_requires_both_keys_and_default_routing() {
    for source in JevSource::ALL {
        for restriction in ["one_key", "proxy", "model", "default_model"] {
            let mut env = vec![("FASTBROWSE_JEV_SOURCE", source.as_str())];
            for (key, owner) in [
                ("TYPESAFE_API_KEY", JevSource::TypeSafe),
                ("AI_GATEWAY_API_KEY", JevSource::Gateway),
                ("OPENROUTER_API_KEY", JevSource::OpenRouter),
            ] {
                if restriction != "one_key" || source == owner {
                    env.push((key, "k"));
                }
            }
            if restriction == "proxy" {
                env.push(("FASTBROWSE_JEV_BASE_URL", "https://proxy.test"));
            }
            let default_pin = match source {
                JevSource::OpenRouter => "jev-1.13",
                JevSource::TypeSafe => "jev-1.13.0",
                JevSource::Gateway => "typesafe-ai/jev",
            };
            match restriction {
                "model" => env.push(("FASTBROWSE_JEV_MODEL", "custom-jev")),
                "default_model" => env.push(("FASTBROWSE_JEV_MODEL", default_pin)),
                _ => {}
            }
            let (http, seen) = urls(503);
            let error = settings(&env).jev(&http).unwrap().evaluate(&json!("page"), &question()).await.unwrap_err();
            assert!(matches!(error, JevError::RetriesExhausted(_)), "{source} {restriction}: {error}");
            let host = match (restriction, source) {
                ("proxy", _) => "proxy.test",
                (_, JevSource::OpenRouter) => "openrouter.ai/api",
                (_, JevSource::TypeSafe) => "api.typesafe.ai",
                (_, JevSource::Gateway) => "ai-gateway.vercel.sh",
            };
            let path = if source == JevSource::Gateway { "/v4/ai/evaluation-model" } else { "/v1/systemone" };
            assert_eq!(*seen.lock().unwrap(), vec![format!("https://{host}{path}"); 6], "{source} {restriction}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_source_model_defaults_and_explicit_pins() {
    for (source, pin, expected) in [
        ("openrouter", None, "jev-1.13"),
        ("typesafe", None, "jev-1.13.0"),
        ("openrouter", Some("jev-1.13.0"), "jev-1.13.0"),
        ("typesafe", Some("jev-latest"), "jev-latest"),
    ] {
        let script = Script::new([common::text(400, "")]);
        let mut env = vec![("OPENROUTER_API_KEY", "o"), ("TYPESAFE_API_KEY", "t"), ("FASTBROWSE_JEV_SOURCE", source)];
        if let Some(pin) = pin {
            env.push(("FASTBROWSE_JEV_MODEL", pin));
        }
        assert!(settings(&env).jev(&script.http()).unwrap().evaluate(&json!("page"), &question()).await.is_err());
        assert_eq!(script.bodies()[0]["model"], expected);
    }
}

fn completion(cost: Option<f64>) -> Response {
    let mut usage = json!({"prompt_tokens": 3, "completion_tokens": 2});
    if let Some(cost) = cost {
        usage["cost"] = cost.into();
    }
    Response::json(200, &json!({"choices": [{"message": {"content": "{\"answer\":\"ok\"}"}}], "usage": usage}))
}

/// One READ generation through the LLM the settings build, its requests and its result.
async fn llm_requests(env: &[(&str, &str)], cost: Option<f64>) -> (Vec<Request>, Generation<Value>) {
    let script = Script::new([common::Step::Reply(completion(cost))]);
    let llm = settings(env).llm(&script.http()).unwrap();
    let result =
        llm.generate_value(LLMPurpose::Read, &[Message::user("?")], &answer_model(), 2000, None).await.unwrap();
    (script.requests(), result)
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

#[tokio::test]
async fn test_llm_prefers_openrouter_when_its_key_is_set() {
    let (seen, result) = llm_requests(&[("OPENROUTER_API_KEY", "o"), ("AI_GATEWAY_API_KEY", "g")], Some(0.0001)).await;
    assert_eq!(seen[0].url, "https://openrouter.ai/api/v1/chat/completions");
    assert_eq!(seen[0].header("authorization"), Some("Bearer o"));
    assert_eq!(body(&seen[0])["provider"], json!({"require_parameters": true, "sort": "latency"}));
    assert_eq!(result.cost.basis, CostBasis::Metered);
}

#[tokio::test]
async fn test_llm_uses_the_gateway_without_an_openrouter_key() {
    let (seen, result) = llm_requests(&[("AI_GATEWAY_API_KEY", "g")], Some(0.0001)).await;
    assert_eq!(seen[0].url, "https://ai-gateway.vercel.sh/v1/chat/completions");
    // The credential goes only to the host it belongs to.
    assert_eq!(seen[0].header("authorization"), Some("Bearer g"));
    assert_eq!(body(&seen[0])["model"], default_model(LLMPurpose::Read));
    assert_eq!(body(&seen[0])["provider"], json!({"sort": "ttft"}));
    assert_eq!(result.cost.basis, CostBasis::Metered);
}

#[tokio::test]
async fn test_gateway_usage_without_a_cost_stays_unknown() {
    // The gateway reports `usage.cost`; a response without it must not read as free.
    let (_, result) = llm_requests(&[("AI_GATEWAY_API_KEY", "g")], None).await;
    assert_eq!((result.cost.basis, result.cost.dollars), (CostBasis::Unknown, None));
}

#[test]
fn test_llm_without_a_key_names_both_settings() {
    let error = config_error(settings(&[]).llm(&Script::default().http()));
    assert!(error.contains("OPENROUTER_API_KEY") && error.contains("AI_GATEWAY_API_KEY"), "{error}");
}

#[test]
fn test_providers_names_the_llm_route() {
    let cases: [(&[(&str, &str)], &str); 3] = [
        (&[("OPENROUTER_API_KEY", "o"), ("AI_GATEWAY_API_KEY", "g")], "via OpenRouter"),
        (&[("AI_GATEWAY_API_KEY", "g")], "via Vercel AI Gateway"),
        (
            &[
                ("OPENROUTER_API_KEY", "o"),
                ("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:8000/v1"),
                ("FASTBROWSE_LLM_MODEL", "qwen"),
            ],
            "LLM qwen via local endpoint",
        ),
    ];
    for (env, expected) in cases {
        let providers = settings(env).providers();
        assert!(providers.contains(expected), "{providers}");
    }
}

const LOCAL: [(&str, &str); 2] =
    [("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:8000/v1/"), ("FASTBROWSE_LLM_MODEL", "qwen")];

fn local(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    [LOCAL.as_slice(), extra].concat()
}

#[tokio::test]
async fn test_a_base_url_takes_the_llm_from_openrouter_and_the_gateway() {
    let key_sets: [&[(&str, &str)]; 4] = [
        &[],
        &[("OPENROUTER_API_KEY", "o")],
        &[("AI_GATEWAY_API_KEY", "g")],
        &[("OPENROUTER_API_KEY", "o"), ("AI_GATEWAY_API_KEY", "g")],
    ];
    for keys in key_sets {
        let (seen, result) = llm_requests(&local(keys), None).await;
        assert_eq!(
            seen.iter().map(|r| r.url.as_str()).collect::<Vec<_>>(),
            ["http://127.0.0.1:8000/v1/chat/completions"]
        );
        // A keyless local server gets no Authorization header, and never the OpenRouter or gateway key.
        assert_eq!(seen[0].header("authorization"), None);
        assert_eq!(body(&seen[0])["model"], "qwen");
        assert!(body(&seen[0]).get("provider").is_none());
        assert_eq!(result.data["answer"], "ok");
    }
}

#[test]
fn test_an_empty_base_url_leaves_the_llm_on_openrouter() {
    let loaded = settings(&[("OPENROUTER_API_KEY", "o"), ("FASTBROWSE_LLM_BASE_URL", "")]);
    assert_eq!(loaded.llm_source(), LLMSource::OpenRouter);
}

#[tokio::test]
async fn test_a_base_url_sends_its_own_key_and_no_other() {
    let env = local(&[
        ("OPENROUTER_API_KEY", "o-secret"),
        ("AI_GATEWAY_API_KEY", "g-secret"),
        ("FASTBROWSE_LLM_API_KEY", "local-secret"),
    ]);
    let (seen, _) = llm_requests(&env, None).await;
    assert_eq!(seen[0].header("authorization"), Some("Bearer local-secret"));
    let sent = format!("{:?}{}", seen[0].headers, String::from_utf8_lossy(&seen[0].body));
    assert!(!sent.contains("o-secret") && !sent.contains("g-secret"));
}

#[test]
fn test_the_local_key_is_not_printed_with_the_settings() {
    let loaded = settings(&local(&[("FASTBROWSE_LLM_API_KEY", "local-secret")]));
    assert!(!format!("{loaded:?}").contains("local-secret"));
}

#[test]
fn test_a_base_url_without_a_model_names_the_setting() {
    let loaded = settings(&[("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:8000/v1")]);
    assert!(config_error(loaded.llm(&Script::default().http())).contains("FASTBROWSE_LLM_MODEL"));
}

const PINS: [&str; 7] = [
    "FASTBROWSE_LLM_MODEL_PLAN",
    "FASTBROWSE_LLM_MODEL_READ",
    "FASTBROWSE_LLM_MODEL_FIELD_TEXT",
    "FASTBROWSE_LLM_MODEL_RECOVER",
    "FASTBROWSE_LLM_MODEL_COMPOSE",
    "FASTBROWSE_LLM_MODEL_VERIFY",
    "FASTBROWSE_LLM_MODEL_SHORTCUT",
];

#[tokio::test]
async fn test_every_purpose_pinned_is_enough_without_one_model() {
    let mut env: Vec<(&str, &str)> = PINS.iter().map(|pin| (*pin, "qwen")).collect();
    env.push(("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:8000/v1"));
    let (seen, _) = llm_requests(&env, None).await;
    assert_eq!(body(&seen[0])["model"], "qwen");
}

#[tokio::test]
async fn test_strict_off_sends_the_models_own_schema() {
    let (seen, _) = llm_requests(&local(&[("FASTBROWSE_LLM_STRICT", "0")]), None).await;
    let json_schema = &body(&seen[0])["response_format"]["json_schema"];
    assert_eq!(json_schema["strict"], false);
    use fastbrowse_core::llm::ResponseSchema;
    assert_eq!(json_schema["schema"], answer_model().json_schema());
}

#[tokio::test]
async fn test_strict_by_default_on_a_local_endpoint() {
    let (seen, _) = llm_requests(&local(&[]), None).await;
    let json_schema = &body(&seen[0])["response_format"]["json_schema"];
    assert_eq!(json_schema["strict"], true);
    assert_eq!(json_schema["schema"]["additionalProperties"], false);
}

#[tokio::test]
async fn test_extra_body_is_merged_into_each_request() {
    let extra = r#"{"chat_template_kwargs": {"enable_thinking": false}, "top_k": 20}"#;
    let (seen, _) = llm_requests(&local(&[("FASTBROWSE_LLM_EXTRA_BODY", extra)]), None).await;
    let sent = body(&seen[0]);
    assert_eq!(sent["chat_template_kwargs"], json!({"enable_thinking": false}));
    assert_eq!(sent["top_k"], 20);
    assert!(sent["model"] == "qwen" && sent.get("response_format").is_some());
}

#[test]
fn test_an_unusable_extra_body_is_a_configuration_error() {
    for (extra, message) in [
        ("{not json", "not valid JSON"),
        ("[1, 2]", "must be a JSON object"),
        ("\"text\"", "must be a JSON object"),
        (r#"{"model": "other"}"#, "cannot set model"),
        (r#"{"messages": [], "response_format": {}}"#, "cannot set messages, response_format"),
        (r#"{"provider": {}}"#, "cannot set provider"),
        (r#"{"reasoning": {"effort": "high"}}"#, "cannot set reasoning"),
        (r#"{"max_tokens": 1}"#, "cannot set max_tokens"),
    ] {
        let loaded = settings(&local(&[("FASTBROWSE_LLM_EXTRA_BODY", extra)]));
        let error = config_error(loaded.llm(&Script::default().http()));
        assert!(error.contains(message), "{extra}: {error}");
    }
}

#[tokio::test]
async fn test_strict_and_extra_body_do_not_reshape_openrouter_requests() {
    let env = [
        ("OPENROUTER_API_KEY", "o"),
        ("FASTBROWSE_LLM_STRICT", "0"),
        ("FASTBROWSE_LLM_EXTRA_BODY", r#"{"top_k": 20}"#),
    ];
    let (seen, _) = llm_requests(&env, Some(0.0001)).await;
    let sent = body(&seen[0]);
    assert!(sent.get("top_k").is_none());
    assert_eq!(sent["response_format"]["json_schema"]["strict"], true);
}

#[tokio::test]
async fn test_a_local_endpoint_without_a_price_reports_an_unknown_cost_and_asks_nowhere_else() {
    let (seen, result) = llm_requests(&local(&[]), None).await;
    // No receipt or price lookup: those belong to OpenRouter and the gateway, and the key would follow them.
    assert_eq!(seen.len(), 1);
    assert_eq!((result.cost.basis, result.cost.dollars), (CostBasis::Unknown, None));
    assert_eq!((result.cost.input_tokens, result.cost.output_tokens), (3, 2));
    assert!(CostBreakdown { lines: vec![result.cost] }.has_unknown());
}

#[tokio::test]
async fn test_a_local_endpoint_that_reports_a_price_is_metered() {
    let (_, result) = llm_requests(&local(&[]), Some(0.0)).await;
    assert_eq!((result.cost.basis, result.cost.dollars), (CostBasis::Metered, Some(0.0)));
}

#[test]
fn test_the_local_settings_read_their_documented_variables() {
    let loaded = settings(&[
        ("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:8000/v1"),
        ("FASTBROWSE_LLM_API_KEY", "k"),
        ("FASTBROWSE_LLM_STRICT", "0"),
        ("FASTBROWSE_LLM_EXTRA_BODY", r#"{"top_k": 20}"#),
    ]);
    assert_eq!(loaded.llm_source(), LLMSource::Local);
    assert_eq!(loaded.llm_api_key.as_ref().map(|key| key.expose()), Some("k"));
    assert!(!loaded.llm_strict);
    assert_eq!(loaded.llm_extra_body.as_deref(), Some(r#"{"top_k": 20}"#));
}

#[tokio::test]
async fn test_a_local_endpoint_gets_no_reasoning_field_by_default() {
    let (seen, _) = llm_requests(&local(&[]), None).await;
    assert!(body(&seen[0]).get("reasoning").is_none());
}

#[tokio::test]
async fn test_a_local_endpoint_gets_the_reasoning_effort_that_was_asked_for() {
    let (seen, _) = llm_requests(&local(&[("FASTBROWSE_LLM_REASONING", "high")]), None).await;
    assert_eq!(body(&seen[0])["reasoning"], json!({"effort": "high"}));
}

#[tokio::test]
async fn test_openrouter_still_gets_the_default_reasoning_effort() {
    let (seen, _) = llm_requests(&[("OPENROUTER_API_KEY", "o")], Some(0.0001)).await;
    assert_eq!(body(&seen[0])["reasoning"], json!({"effort": "low"}));
}

#[tokio::test]
async fn test_openrouter_gets_reasoning_turned_off_when_asked() {
    let (seen, _) =
        llm_requests(&[("OPENROUTER_API_KEY", "o"), ("FASTBROWSE_LLM_REASONING", "none")], Some(0.0001)).await;
    assert_eq!(body(&seen[0])["reasoning"], json!({"effort": "none"}));
}

#[test]
fn test_a_blank_model_on_a_local_endpoint_is_unset() {
    for blank in ["", "   "] {
        let loaded =
            settings(&[("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:8000/v1"), ("FASTBROWSE_LLM_MODEL", blank)]);
        assert!(config_error(loaded.llm(&Script::default().http())).contains("FASTBROWSE_LLM_MODEL"));
    }
}

#[tokio::test]
async fn test_a_blank_purpose_pin_falls_back_to_the_global_model_not_a_default() {
    for blank in ["", "   "] {
        let pins: Vec<(&str, &str)> = PINS.iter().map(|pin| (*pin, blank)).collect();
        let (seen, _) = llm_requests(&local(&pins), None).await;
        assert_eq!(body(&seen[0])["model"], "qwen");
    }
}

#[test]
fn test_blank_pins_without_a_global_model_are_unset() {
    let mut env: Vec<(&str, &str)> =
        PINS.iter().map(|pin| (*pin, if *pin == "FASTBROWSE_LLM_MODEL_READ" { " " } else { "qwen" })).collect();
    env.extend([("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:8000/v1"), ("FASTBROWSE_LLM_MODEL", "")]);
    assert!(config_error(settings(&env).llm(&Script::default().http())).contains("FASTBROWSE_LLM_MODEL"));
}

// What pydantic-settings does for Python before any of the above runs.

#[test]
fn a_dotenv_file_is_read_as_python_dotenv_reads_it() {
    let dotenv = "\
# a comment
export OPENROUTER_API_KEY=\"o-key\"  # quoted, exported
FASTBROWSE_LLM_MODEL=qwen   # trailing comment
FASTBROWSE_LLM_STRICT='0'
FASTBROWSE_JEV_MODEL=jev#1

FASTBROWSE_LLM_EXTRA_BODY={\"top_k\": 20}
";
    let loaded = Settings::from_sources::<&str, &str>(&[], Some(dotenv)).unwrap();
    assert_eq!(loaded.openrouter_api_key.as_ref().map(|key| key.expose()), Some("o-key"));
    assert_eq!(loaded.llm_model.as_deref(), Some("qwen"));
    assert!(!loaded.llm_strict);
    // A `#` with no space before it is part of the value.
    assert_eq!(loaded.jev_model.as_deref(), Some("jev#1"));
    assert_eq!(loaded.llm_extra_body.as_deref(), Some("{\"top_k\": 20}"));
}

#[test]
fn the_environment_beats_dotenv_and_names_ignore_case() {
    let dotenv = "FASTBROWSE_LLM_MODEL=from-file\nopenrouter_api_key=file-key\nFASTBROWSE_CHROME=chromium\n";
    let loaded = Settings::from_sources(
        &[("fastbrowse_llm_model", "from-env"), ("OpenRouter_API_Key", "env-key")],
        Some(dotenv),
    )
    .unwrap();
    assert_eq!(loaded.llm_model.as_deref(), Some("from-env"));
    assert_eq!(loaded.openrouter_api_key.as_ref().map(|key| key.expose()), Some("env-key"));
    assert_eq!(loaded.chrome.as_deref(), Some("chromium"));
    // The keys are read by their own names, never under the FASTBROWSE_ prefix.
    let prefixed = settings(&[("FASTBROWSE_OPENROUTER_API_KEY", "o")]);
    assert!(prefixed.openrouter_api_key.is_none());
}

#[test]
fn the_example_dotenv_reads_as_python_dotenv_reads_it() {
    let example = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env.example")).unwrap();
    let loaded = Settings::from_sources::<&str, &str>(&[], Some(&example)).unwrap();
    // python-dotenv reads `OPENROUTER_API_KEY=            # url` as the comment itself, since the spaces before the
    // `#` go with the `=` (checked against dotenv_values), so an example copied and never filled sends it as a key.
    assert_eq!(
        loaded.openrouter_api_key.as_ref().map(|key| key.expose()),
        Some("# the LLM, and a supported Jev text route, https://openrouter.ai/keys")
    );
    assert_eq!(loaded.llm_source(), LLMSource::OpenRouter);
    assert!(loaded.ai_gateway_api_key.is_none());
}

#[test]
fn an_invalid_setting_is_named_without_its_value() {
    let error =
        Settings::from_sources(&[("FASTBROWSE_LLM_REASONING", "extreme"), ("FASTBROWSE_HEADED", "maybe")], None)
            .unwrap_err()
            .to_string();
    assert_eq!(error, "invalid settings: llm_reasoning, headed");
    let loaded = settings(&[("FASTBROWSE_HEADED", "yes"), ("FASTBROWSE_PROFILE", "/tmp/profile")]);
    assert!(loaded.local_chrome().headed);
}
