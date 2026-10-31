//! FASTBROWSE_LLM_PROVIDER: each preset sends its own key to its own host, in the request shape that provider documents
//! (docs/llm-providers.md), and a missing key, model or address is named before any request goes out.

mod common;

use common::{Script, Step, answer_model, body, config_error, settings};
use fastbrowse_clients::environment::{LLMProvider, Settings};
use fastbrowse_clients::http::{Request, Response};
use fastbrowse_core::llm::{Generation, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, LLMPurpose};
use serde_json::{Value, json};

fn completion(content: &str, finish_reason: &str) -> Step {
    Step::Reply(Response::json(
        200,
        &json!({
            "choices": [{"message": {"content": content}, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2},
        }),
    ))
}

async fn generate(env: &[(&str, &str)], replies: Vec<Step>) -> (Vec<Request>, Generation<Value>) {
    let script = Script::new(replies);
    let llm = settings(env).llm(&script.http()).unwrap();
    let result = llm.generate_value(LLMPurpose::Read, &[Message::user("?")], &answer_model(), None).await.unwrap();
    (script.requests(), result)
}

async fn one(env: &[(&str, &str)]) -> Request {
    generate(env, vec![completion(r#"{"answer":"ok"}"#, "stop")]).await.0.remove(0)
}

/// Every key a run could hold, each a value no other provider's request may carry.
const EVERY_KEY: [(&str, &str); 14] = [
    ("OPENROUTER_API_KEY", "openrouter-secret"),
    ("AI_GATEWAY_API_KEY", "gateway-secret"),
    ("TYPESAFE_API_KEY", "typesafe-secret"),
    ("FASTBROWSE_LLM_API_KEY", "local-secret"),
    ("OPENAI_API_KEY", "openai-secret"),
    ("ANTHROPIC_API_KEY", "anthropic-secret"),
    ("GEMINI_API_KEY", "gemini-secret"),
    ("GROQ_API_KEY", "groq-secret"),
    ("TOGETHER_API_KEY", "together-secret"),
    ("MISTRAL_API_KEY", "mistral-secret"),
    ("DEEPSEEK_API_KEY", "deepseek-secret"),
    ("XAI_API_KEY", "xai-secret"),
    ("FIREWORKS_API_KEY", "fireworks-secret"),
    ("AZURE_OPENAI_API_KEY", "azure-secret"),
];

struct Hosted {
    provider: &'static str,
    url: &'static str,
    key: &'static str,
    format: &'static str,
    strict: bool,
}

const HOSTED: [Hosted; 9] = [
    Hosted {
        provider: "openai",
        url: "https://api.openai.com/v1/chat/completions",
        key: "openai-secret",
        format: "json_schema",
        strict: true,
    },
    Hosted {
        provider: "gemini",
        url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions",
        key: "gemini-secret",
        format: "json_schema",
        strict: true,
    },
    Hosted {
        provider: "groq",
        url: "https://api.groq.com/openai/v1/chat/completions",
        key: "groq-secret",
        format: "json_schema",
        strict: true,
    },
    Hosted {
        provider: "together",
        url: "https://api.together.ai/v1/chat/completions",
        key: "together-secret",
        format: "json_schema",
        strict: false,
    },
    Hosted {
        provider: "mistral",
        url: "https://api.mistral.ai/v1/chat/completions",
        key: "mistral-secret",
        format: "json_schema",
        strict: false,
    },
    Hosted {
        provider: "deepseek",
        url: "https://api.deepseek.com/chat/completions",
        key: "deepseek-secret",
        format: "json_object",
        strict: false,
    },
    Hosted {
        provider: "xai",
        url: "https://api.x.ai/v1/chat/completions",
        key: "xai-secret",
        format: "json_schema",
        strict: true,
    },
    Hosted {
        provider: "fireworks",
        url: "https://api.fireworks.ai/inference/v1/chat/completions",
        key: "fireworks-secret",
        format: "json_schema",
        strict: false,
    },
    Hosted {
        provider: "azure",
        url: "https://example.openai.azure.com/openai/v1/chat/completions",
        key: "azure-secret",
        format: "json_schema",
        strict: true,
    },
];

#[tokio::test]
async fn test_each_hosted_preset_sends_its_own_key_to_its_own_host_only() {
    for case in HOSTED {
        let mut env = EVERY_KEY.to_vec();
        env.extend([("FASTBROWSE_LLM_PROVIDER", case.provider), ("FASTBROWSE_LLM_MODEL", "m")]);
        if case.provider == "azure" {
            env.push(("FASTBROWSE_LLM_BASE_URL", "https://example.openai.azure.com/openai/v1"));
        }
        let request = one(&env).await;
        assert_eq!(request.url, case.url, "{}", case.provider);
        let sent = format!("{:?}{}", request.headers, String::from_utf8_lossy(&request.body));
        let foreign: Vec<_> =
            EVERY_KEY.iter().map(|(_, key)| *key).filter(|key| *key != case.key && sent.contains(key)).collect();
        assert!(foreign.is_empty(), "{} carried {foreign:?}", case.provider);
        if case.provider == "azure" {
            assert_eq!(request.header("api-key"), Some(case.key));
            assert_eq!(request.header("authorization"), None);
        } else {
            assert_eq!(request.header("authorization"), Some(format!("Bearer {}", case.key).as_str()));
        }
        let sent = body(&request);
        assert_eq!(sent["response_format"]["type"], case.format, "{}", case.provider);
        if case.format == "json_schema" {
            assert_eq!(sent["response_format"]["json_schema"]["strict"], case.strict, "{}", case.provider);
        }
        for cap in ["max_tokens", "max_completion_tokens"] {
            assert!(sent.get(cap).is_none(), "{} sent {cap}", case.provider);
        }
        // OpenRouter's fields and the reasoning effort go nowhere they were not asked for.
        for absent in ["provider", "reasoning", "reasoning_effort"] {
            assert!(sent.get(absent).is_none(), "{} sent {absent}", case.provider);
        }
    }
}

#[test]
fn test_a_preset_keeps_no_other_providers_key() {
    let mut env = EVERY_KEY.to_vec();
    env.push(("FASTBROWSE_LLM_PROVIDER", "groq"));
    let loaded = settings(&env);
    assert_eq!(loaded.llm_provider_api_key.as_ref().map(|key| key.expose()), Some("groq-secret"));
    assert!(!format!("{loaded:?}").contains("groq-secret"));
}

#[test]
fn test_a_hosted_preset_refuses_another_address() {
    for provider in ["openai", "anthropic", "openrouter", "gateway"] {
        let loaded = settings(&[
            ("FASTBROWSE_LLM_PROVIDER", provider),
            ("FASTBROWSE_LLM_BASE_URL", "https://proxy.example/v1"),
            ("OPENAI_API_KEY", "k"),
            ("ANTHROPIC_API_KEY", "k"),
            ("OPENROUTER_API_KEY", "k"),
            ("AI_GATEWAY_API_KEY", "k"),
        ]);
        let error = config_error(loaded.llm(&Script::default().http()));
        assert!(error.contains("FASTBROWSE_LLM_PROVIDER=custom"), "{provider}: {error}");
    }
}

#[test]
fn test_a_preset_without_its_key_names_the_variable() {
    for (provider, variable) in [("openai", "OPENAI_API_KEY"), ("xai", "XAI_API_KEY"), ("deepseek", "DEEPSEEK_API_KEY")]
    {
        // Another provider's key is not a substitute.
        let loaded = settings(&[
            ("FASTBROWSE_LLM_PROVIDER", provider),
            ("FASTBROWSE_LLM_MODEL", "m"),
            ("OPENROUTER_API_KEY", "o"),
            ("FASTBROWSE_LLM_API_KEY", "l"),
        ]);
        let error = config_error(loaded.llm(&Script::default().http()));
        assert!(error.contains(variable), "{provider}: {error}");
    }
}

#[test]
fn test_a_preset_without_a_default_model_asks_for_one() {
    let loaded = settings(&[("FASTBROWSE_LLM_PROVIDER", "openai"), ("OPENAI_API_KEY", "k")]);
    let error = config_error(loaded.llm(&Script::default().http()));
    assert!(error.contains("FASTBROWSE_LLM_MODEL") && error.contains("OpenAI"), "{error}");
    assert!(loaded.providers().contains("LLM model unset via OpenAI"), "{}", loaded.providers());
}

#[tokio::test]
async fn test_presets_with_defaults_use_them_and_a_pin_wins() {
    let groq = [("FASTBROWSE_LLM_PROVIDER", "groq"), ("GROQ_API_KEY", "k")];
    assert_eq!(body(&one(&groq).await)["model"], "openai/gpt-oss-120b");
    let pinned = [groq.as_slice(), &[("FASTBROWSE_LLM_MODEL", "llama")]].concat();
    assert_eq!(body(&one(&pinned).await)["model"], "llama");
    let providers = settings(&groq).providers();
    assert!(providers.contains("LLM openai/gpt-oss-120b via Groq"), "{providers}");
}

#[tokio::test]
async fn test_reasoning_effort_goes_in_the_field_the_preset_documents() {
    let env = [("FASTBROWSE_LLM_PROVIDER", "openai"), ("OPENAI_API_KEY", "k"), ("FASTBROWSE_LLM_MODEL", "m")];
    let asked = [env.as_slice(), &[("FASTBROWSE_LLM_REASONING", "none")]].concat();
    let sent = body(&one(&asked).await);
    assert_eq!(sent["reasoning_effort"], "none");
    assert!(sent.get("reasoning").is_none());
    let ollama =
        [("FASTBROWSE_LLM_PROVIDER", "ollama"), ("FASTBROWSE_LLM_MODEL", "m"), ("FASTBROWSE_LLM_REASONING", "high")];
    assert_eq!(body(&one(&ollama).await)["reasoning_effort"], "high");
    // vLLM and LM Studio stay on the field a custom endpoint has always been sent.
    let vllm =
        [("FASTBROWSE_LLM_PROVIDER", "vllm"), ("FASTBROWSE_LLM_MODEL", "m"), ("FASTBROWSE_LLM_REASONING", "high")];
    assert_eq!(body(&one(&vllm).await)["reasoning"], json!({"effort": "high"}));
}

#[test]
fn test_reasoning_on_a_preset_that_takes_none_is_a_configuration_error() {
    for (provider, key) in [("together", "TOGETHER_API_KEY"), ("deepseek", "DEEPSEEK_API_KEY")] {
        let loaded = settings(&[
            ("FASTBROWSE_LLM_PROVIDER", provider),
            (key, "k"),
            ("FASTBROWSE_LLM_MODEL", "m"),
            ("FASTBROWSE_LLM_REASONING", "low"),
        ]);
        let error = config_error(loaded.llm(&Script::default().http()));
        assert!(error.contains("FASTBROWSE_LLM_REASONING"), "{provider}: {error}");
    }
}

#[tokio::test]
async fn test_json_object_mode_states_the_schema_and_reads_a_fenced_reply() {
    let env = [("FASTBROWSE_LLM_PROVIDER", "deepseek"), ("DEEPSEEK_API_KEY", "k"), ("FASTBROWSE_LLM_MODEL", "m")];
    let (seen, result) = generate(&env, vec![completion("```json\n{\"answer\":\"ok\"}\n```", "stop")]).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(result.data["answer"], "ok");
    let sent = body(&seen[0]);
    assert_eq!(sent["response_format"], json!({"type": "json_object"}));
    let last = sent["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "system");
    let instruction = last["content"].as_str().unwrap();
    assert!(instruction.contains("JSON") && instruction.contains(&answer_model().json_schema().to_string()));
}

#[test]
fn test_an_extra_body_cannot_replace_the_presets_own_fields() {
    let loaded = settings(&[
        ("FASTBROWSE_LLM_PROVIDER", "openai"),
        ("OPENAI_API_KEY", "k"),
        ("FASTBROWSE_LLM_MODEL", "m"),
        ("FASTBROWSE_LLM_EXTRA_BODY", r#"{"reasoning_effort": "high"}"#),
    ]);
    let error = config_error(loaded.llm(&Script::default().http()));
    assert!(error.contains("reasoning_effort"), "{error}");
    // Nor can it set an output cap, which fastbrowse never sends.
    let loaded = settings(&[
        ("FASTBROWSE_LLM_PROVIDER", "openai"),
        ("OPENAI_API_KEY", "k"),
        ("FASTBROWSE_LLM_MODEL", "m"),
        ("FASTBROWSE_LLM_EXTRA_BODY", r#"{"max_completion_tokens": 1}"#),
    ]);
    let error = config_error(loaded.llm(&Script::default().http()));
    assert!(error.contains("max_completion_tokens"), "{error}");
}

#[tokio::test]
async fn test_strict_setting_overrides_a_presets_default() {
    let env = [
        ("FASTBROWSE_LLM_PROVIDER", "together"),
        ("TOGETHER_API_KEY", "k"),
        ("FASTBROWSE_LLM_MODEL", "m"),
        ("FASTBROWSE_LLM_STRICT", "1"),
    ];
    assert_eq!(body(&one(&env).await)["response_format"]["json_schema"]["strict"], true);
}

#[test]
fn test_azure_takes_only_an_https_azure_resource() {
    for url in [None, Some("https://example.com/openai/v1"), Some("http://example.openai.azure.com/openai/v1")] {
        let mut env =
            vec![("FASTBROWSE_LLM_PROVIDER", "azure"), ("AZURE_OPENAI_API_KEY", "k"), ("FASTBROWSE_LLM_MODEL", "m")];
        env.extend(url.map(|url| ("FASTBROWSE_LLM_BASE_URL", url)));
        let error = config_error(settings(&env).llm(&Script::default().http()));
        assert!(error.contains(".openai.azure.com"), "{url:?}: {error}");
    }
}

#[tokio::test]
async fn test_local_presets_default_their_address_keyless_and_take_an_override() {
    let cases = [
        (&[("FASTBROWSE_LLM_PROVIDER", "ollama")][..], "http://localhost:11434/v1/chat/completions"),
        (&[("FASTBROWSE_LLM_PROVIDER", "lmstudio")][..], "http://localhost:1234/v1/chat/completions"),
        (&[("FASTBROWSE_LLM_PROVIDER", "vllm")][..], "http://localhost:8000/v1/chat/completions"),
        (
            &[("FASTBROWSE_LLM_PROVIDER", "ollama"), ("FASTBROWSE_LLM_BASE_URL", "http://gpu-box:11434/v1")][..],
            "http://gpu-box:11434/v1/chat/completions",
        ),
    ];
    for (preset, url) in cases {
        let env = [preset, &[("FASTBROWSE_LLM_MODEL", "m"), ("OPENROUTER_API_KEY", "o")]].concat();
        let request = one(&env).await;
        assert_eq!(request.url, url);
        assert_eq!(request.header("authorization"), None);
    }
}

#[tokio::test]
async fn test_custom_is_the_base_url_route_by_name() {
    let error = config_error(settings(&[("FASTBROWSE_LLM_PROVIDER", "custom")]).llm(&Script::default().http()));
    assert!(error.contains("FASTBROWSE_LLM_BASE_URL"), "{error}");
    let env = [
        ("FASTBROWSE_LLM_PROVIDER", "custom"),
        ("FASTBROWSE_LLM_BASE_URL", "http://127.0.0.1:9000/v1"),
        ("FASTBROWSE_LLM_MODEL", "m"),
        ("FASTBROWSE_LLM_API_KEY", "l"),
    ];
    let request = one(&env).await;
    assert_eq!(request.url, "http://127.0.0.1:9000/v1/chat/completions");
    assert_eq!(request.header("authorization"), Some("Bearer l"));
}

#[tokio::test]
async fn test_a_preset_without_a_reported_price_is_unknown() {
    let env = [("FASTBROWSE_LLM_PROVIDER", "openai"), ("OPENAI_API_KEY", "k"), ("FASTBROWSE_LLM_MODEL", "m")];
    let (seen, result) = generate(&env, vec![completion(r#"{"answer":"ok"}"#, "stop")]).await;
    // No receipt or price lookup: those belong to OpenRouter and the gateway.
    assert_eq!(seen.len(), 1);
    assert_eq!((result.cost.basis, result.cost.dollars), (CostBasis::Unknown, None));
    assert_eq!((result.cost.input_tokens, result.cost.output_tokens), (3, 2));
}

#[test]
fn test_the_provider_is_named_and_parsed() {
    let loaded = settings(&[("FASTBROWSE_LLM_PROVIDER", "openrouter"), ("AI_GATEWAY_API_KEY", "g")]);
    assert_eq!(loaded.llm_source(), LLMProvider::OpenRouter);
    let loaded = settings(&[("FASTBROWSE_LLM_PROVIDER", "mistral"), ("FASTBROWSE_LLM_MODEL", "m")]);
    assert!(loaded.providers().contains("LLM m via Mistral"), "{}", loaded.providers());
    let error = Settings::from_sources(&[("FASTBROWSE_LLM_PROVIDER", "bedrock")], None).unwrap_err();
    assert_eq!(error.to_string(), "invalid settings: llm_provider");
    for provider in LLMProvider::ALL {
        assert_eq!(LLMProvider::parse(provider.as_str()), Some(provider));
    }
}

#[tokio::test]
async fn test_an_error_that_echoes_a_key_outside_the_authorization_header_is_scrubbed() {
    let env = [
        ("FASTBROWSE_LLM_PROVIDER", "azure"),
        ("AZURE_OPENAI_API_KEY", "azure-secret"),
        ("FASTBROWSE_LLM_MODEL", "m"),
        ("FASTBROWSE_LLM_BASE_URL", "https://example.openai.azure.com/openai/v1"),
    ];
    let script =
        Script::new([Step::Reply(Response::json(401, &json!({"error": {"message": "bad key azure-secret"}})))]);
    let llm = settings(&env).llm(&script.http()).unwrap();
    let error = llm
        .generate_value(LLMPurpose::Read, &[Message::user("?")], &answer_model(), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("azure-secret") && error.contains("[api key]"), "{error}");
}

#[tokio::test]
async fn a_refused_openrouter_key_moves_the_run_to_the_gateway_key_beside_it() {
    let refused = || Step::Reply(Response::json(401, &json!({"error": {"message": "User not found."}})));
    let env = [("OPENROUTER_API_KEY", "openrouter-secret"), ("AI_GATEWAY_API_KEY", "gateway-secret")];
    // The gateway states each call's cost, as it does in production.
    let costed = || {
        Step::Reply(Response::json(
            200,
            &json!({
                "choices": [{"message": {"content": r#"{"answer":"ok"}"#}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "cost": 0.0001},
            }),
        ))
    };
    let script = Script::new(vec![refused(), costed(), costed()]);
    let llm = settings(&env).llm(&script.http()).unwrap();
    for _ in 0..2 {
        let result = llm.generate_value(LLMPurpose::Read, &[Message::user("?")], &answer_model(), None).await;
        assert_eq!(result.unwrap().data, json!({"answer": "ok"}));
    }
    let hosts: Vec<String> =
        script.requests().iter().map(|r| url::Url::parse(&r.url).unwrap().host_str().unwrap().to_string()).collect();
    // The switch holds for the rest of the run: the second call goes straight to the gateway.
    assert_eq!(hosts, ["openrouter.ai", "ai-gateway.vercel.sh", "ai-gateway.vercel.sh"]);

    // Without a gateway key the refusal names the host that refused it.
    let script = Script::new(vec![refused()]);
    let llm = settings(&env[..1]).llm(&script.http()).unwrap();
    let error = llm.generate_value(LLMPurpose::Read, &[Message::user("?")], &answer_model(), None).await;
    assert_eq!(error.unwrap_err().to_string(), "LLM HTTP 401: User not found. (openrouter.ai rejected the API key)");
}
