//! tests/clients/test_failover.py: a run moves to its backup Jev provider only after the primary's retries run out.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::answering;
use fastbrowse_clients::environment::{DecisionSource, Settings};
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_clients::validation::{RETRY_DELAYS_SECONDS, RETRYABLE_STATUS};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    Answer, DecisionError, JEV_DOLLARS_PER_INPUT_TOKEN, NoulAnswer, NoulQuestion, Question,
};
use fastbrowse_core::models::CostBasis;
use indexmap::IndexMap;
use serde_json::json;

const ATTEMPTS: usize = RETRY_DELAYS_SECONDS.len() + 1;

fn questions() -> IndexMap<String, Question> {
    IndexMap::from([("q".to_string(), NoulQuestion::new("Is it?", None, None).into())])
}

fn yes() -> IndexMap<String, Answer> {
    IndexMap::from([("q".to_string(), Answer::Noul(NoulAnswer { kind: Default::default(), probability: 1.0 }))])
}

fn source(request: &Request) -> DecisionSource {
    match url::Url::parse(&request.url).unwrap().host_str().unwrap() {
        "openrouter.ai" => DecisionSource::OpenRouter,
        "api.typesafe.ai" => DecisionSource::TypeSafe,
        "ai-gateway.vercel.sh" => DecisionSource::Gateway,
        other => panic!("unexpected host {other}"),
    }
}

fn answer(provider: DecisionSource) -> Response {
    if provider != DecisionSource::Gateway {
        return Response::json(
            200,
            &json!({"answers": {"q": {"type": "noul", "noul": 1}}, "usage": {"input_tokens": 100, "output_tokens": 7}}),
        );
    }
    Response::json(
        200,
        &json!({
            "answers": {"q": {"type": "boolean", "probability": 1}},
            "usage": {"inputTokens": 100, "outputTokens": 7},
            "providerMetadata": {"gateway": {"cost": "0.001"}},
        }),
    )
}

fn settings(primary: Option<DecisionSource>, direct_key: bool) -> Settings {
    let mut env = vec![("AI_GATEWAY_API_KEY", "gateway-key"), ("OPENROUTER_API_KEY", "openrouter-key")];
    if direct_key {
        env.push(("TYPESAFE_API_KEY", "direct-key"));
    }
    if let Some(primary) = primary {
        env.push(("FASTBROWSE_JEV_SOURCE", primary.as_str()));
    }
    Settings::from_sources(&env, None).unwrap()
}

fn backup_of(primary: DecisionSource) -> DecisionSource {
    if primary == DecisionSource::Gateway { DecisionSource::TypeSafe } else { DecisionSource::Gateway }
}

type Seen = Arc<Mutex<Vec<DecisionSource>>>;

/// A transport that records which provider each request reached and answers with `respond`.
fn recording<F>(seen: &Seen, respond: F) -> Http
where
    F: Fn(DecisionSource, usize, &Request) -> BoxFuture<'static, Result<Response, TransportError>>
        + Send
        + Sync
        + 'static,
{
    let seen = seen.clone();
    answering(move |request| {
        let provider = source(request);
        let count = {
            let mut seen = seen.lock().unwrap();
            seen.push(provider);
            seen.iter().filter(|p| **p == provider).count()
        };
        respond(provider, count, request)
    })
}

fn ready(response: Response) -> BoxFuture<'static, Result<Response, TransportError>> {
    Box::pin(async move { Ok(response) })
}

#[tokio::test(start_paused = true)]
async fn test_exhausted_primary_switches_once_and_each_run_starts_fresh() {
    let routes = [
        (None, true),
        (None, false),
        (Some(DecisionSource::OpenRouter), true),
        (Some(DecisionSource::TypeSafe), true),
        (Some(DecisionSource::Gateway), true),
        (Some(DecisionSource::Gateway), false),
    ];
    for (setting, direct_key) in routes {
        for status in RETRYABLE_STATUS {
            let fallback = if direct_key { DecisionSource::TypeSafe } else { DecisionSource::OpenRouter };
            let primary = setting.unwrap_or(fallback);
            let backup = if primary == DecisionSource::Gateway { fallback } else { DecisionSource::Gateway };
            let seen = Seen::default();
            let http = recording(&seen, move |provider, _, request| {
                let expected = match provider {
                    DecisionSource::OpenRouter => "openrouter-key",
                    DecisionSource::TypeSafe => "direct-key",
                    DecisionSource::Gateway => "gateway-key",
                };
                assert_eq!(request.header("authorization"), Some(format!("Bearer {expected}").as_str()));
                ready(if provider == primary { Response::new(status, "") } else { answer(provider) })
            });
            let configured = settings(setting, direct_key);
            let client = configured.decision(&http).unwrap();
            let first = client.evaluate(&json!("page"), &questions()).await.unwrap();
            let second = client.evaluate(&json!("page"), &questions()).await.unwrap();
            let mut expected = vec![primary; ATTEMPTS];
            expected.extend([backup, backup]);
            assert_eq!(*seen.lock().unwrap(), expected, "{setting:?} {direct_key} {status}");
            assert_eq!(first.answers, yes());
            assert_eq!(second.answers, yes());
            assert_eq!(first.cost.dollars, second.cost.dollars);
            // A new run starts on the primary again.
            configured.decision(&http).unwrap().evaluate(&json!("page"), &questions()).await.unwrap();
            expected.extend(vec![primary; ATTEMPTS]);
            expected.push(backup);
            assert_eq!(*seen.lock().unwrap(), expected);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_simultaneous_failures_stay_on_backup() {
    for primary in DecisionSource::ALL {
        let seen = Seen::default();
        let (started, _) = tokio::sync::watch::channel(false);
        let started = Arc::new(started);
        let http = recording(&seen, move |provider, count, _| {
            let started = started.clone();
            Box::pin(async move {
                if provider != primary {
                    return Ok(answer(provider));
                }
                if count == 2 {
                    started.send_replace(true);
                }
                started.subscribe().wait_for(|both| *both).await.unwrap();
                Ok(Response::new(503, ""))
            })
        });
        let client = settings(Some(primary), true).decision(&http).unwrap();
        let (asked, first_state, second_state) = (questions(), json!("first"), json!("second"));
        let (first, second) =
            tokio::join!(client.evaluate(&first_state, &asked), client.evaluate(&second_state, &asked));
        client.evaluate(&json!("third"), &questions()).await.unwrap();
        assert_eq!(first.unwrap().answers, yes());
        assert_eq!(second.unwrap().answers, yes());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|p| **p == primary).count(), 2 * ATTEMPTS);
        assert_eq!(seen.len(), 2 * ATTEMPTS + 3);
    }
}

#[tokio::test(start_paused = true)]
async fn test_request_errors_and_malformed_responses_do_not_switch() {
    for primary in DecisionSource::ALL {
        for status in [400, 401, 403, 404, 422, 200] {
            let seen = Seen::default();
            let http = recording(&seen, move |_, _, _| ready(Response::json(status, &json!({"error": "invalid"}))));
            let client = settings(Some(primary), true).decision(&http).unwrap();
            for _ in 0..2 {
                assert!(client.evaluate(&json!("page"), &questions()).await.is_err());
            }
            assert_eq!(*seen.lock().unwrap(), vec![primary, primary], "{primary} {status}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_input_limits_and_transport_errors_do_not_switch() {
    for primary in DecisionSource::ALL {
        let seen = Seen::default();
        let http = recording(&seen, |_, count, _| {
            Box::pin(async move {
                if count == 1 {
                    Ok(Response::new(400, "max_tokens_exceeded"))
                } else {
                    Err(TransportError::Transient("ConnectError"))
                }
            })
        });
        let client = settings(Some(primary), true).decision(&http).unwrap();
        let first = client.evaluate(&json!("page"), &questions()).await.unwrap_err();
        assert!(matches!(first, DecisionError::InputTooLarge(_)), "{first}");
        let second = client.evaluate(&json!("page"), &questions()).await.unwrap_err();
        assert!(matches!(second, DecisionError::TransportFailed(_)), "{second}");
        assert!(second.to_string().contains("transport failed"));
        assert_eq!(*seen.lock().unwrap(), vec![primary; 1 + ATTEMPTS]);
    }
}

#[tokio::test(start_paused = true)]
async fn test_recovery_within_retry_budget_keeps_primary() {
    for primary in DecisionSource::ALL {
        let seen = Seen::default();
        let http = recording(&seen, move |_, count, _| {
            ready(if count < ATTEMPTS { Response::new(503, "") } else { answer(primary) })
        });
        let client = settings(Some(primary), true).decision(&http).unwrap();
        client.evaluate(&json!("page"), &questions()).await.unwrap();
        client.evaluate(&json!("page"), &questions()).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![primary; ATTEMPTS + 1]);
    }
}

#[tokio::test(start_paused = true)]
async fn test_backup_exhaustion_does_not_switch_back() {
    // Python pins the clock to check `seconds` exactly (5 and 4); tokio's paused clock also counts the backoff, so
    // only the request counts and the last failure are compared here.
    for primary in DecisionSource::ALL {
        let seen = Seen::default();
        let http = recording(&seen, move |provider, count, _| {
            Box::pin(async move {
                if count == 1 {
                    return Err(TransportError::Timeout("ReadTimeout"));
                }
                Ok(Response::new(if provider == primary { 503 } else { 429 }, ""))
            })
        });
        let client = settings(Some(primary), true).decision(&http).unwrap();
        let first = client.evaluate(&json!("page"), &questions()).await.unwrap_err();
        let second = client.evaluate(&json!("page"), &questions()).await.unwrap_err();
        let (DecisionError::RetriesExhausted(first), DecisionError::RetriesExhausted(second)) = (first, second) else {
            panic!("both calls exhaust their retries");
        };
        assert!(first.message.contains("HTTP 429") && second.message.contains("HTTP 429"));
        assert_eq!(first.unaccounted_requests, 2);
        assert_eq!(second.unaccounted_requests, 0);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|p| **p == primary).count(), ATTEMPTS);
        assert_eq!(seen.len(), 3 * ATTEMPTS);
    }
}

/// Marks a request's future as dropped, as Python's `finally` marks its task cancelled.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn test_cancellation_cleans_up_and_keeps_the_selected_provider() {
    for primary in DecisionSource::ALL {
        for after_switch in [false, true] {
            let seen = Seen::default();
            let cancelled = Arc::new(AtomicBool::new(false));
            let started = Arc::new(tokio::sync::Notify::new());
            let (flag, signal) = (cancelled.clone(), started.clone());
            let http = recording(&seen, move |provider, _, _| {
                let (flag, signal) = (flag.clone(), signal.clone());
                Box::pin(async move {
                    if flag.load(Ordering::SeqCst) {
                        return Ok(answer(provider));
                    }
                    if after_switch && provider == primary {
                        return Ok(Response::new(503, ""));
                    }
                    let _guard = Dropped(flag);
                    signal.notify_one();
                    std::future::pending().await
                })
            });
            let client = settings(Some(primary), true).decision(&http).unwrap();
            let questions = questions();
            let state = json!("page");
            tokio::select! {
                _ = client.evaluate(&state, &questions) => panic!("the call should have been cancelled"),
                _ = started.notified() => {}
            }
            assert!(cancelled.load(Ordering::SeqCst));
            let result = client.evaluate(&state, &questions).await.unwrap();
            assert_eq!(result.answers, yes());
            let backup = backup_of(primary);
            let expected =
                if after_switch { [vec![primary; ATTEMPTS], vec![backup, backup]].concat() } else { vec![primary; 2] };
            assert_eq!(*seen.lock().unwrap(), expected, "{primary} {after_switch}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_failover_accounts_for_each_providers_unanswered_requests() {
    // Python ticks a fake clock once per request to check `seconds` (ATTEMPTS + 2 and 1); the paused tokio clock also
    // counts the backoff, so those two assertions are left out and every token and dollar count kept.
    for primary in DecisionSource::ALL {
        let seen = Seen::default();
        let http = recording(&seen, move |provider, count, _| {
            Box::pin(async move {
                if count == 1 {
                    return Err(TransportError::Timeout("ReadTimeout"));
                }
                Ok(if provider == primary { Response::new(503, "") } else { answer(provider) })
            })
        });
        let client = settings(Some(primary), true).decision(&http).unwrap();
        let first = client.evaluate(&json!("page"), &questions()).await.unwrap();
        let second = client.evaluate(&json!("page"), &questions()).await.unwrap();
        assert_eq!(first.cost.basis, CostBasis::Estimated);
        assert_eq!(first.input_tokens, 100);
        assert_eq!(first.cost.input_tokens, 300);
        assert_eq!(second.cost.input_tokens, 100);
        assert_eq!((first.cost.output_tokens, second.cost.output_tokens), (7, 7));
        let expected = second.cost.dollars.unwrap() * 2.0 + 100.0 * JEV_DOLLARS_PER_INPUT_TOKEN;
        assert!((first.cost.dollars.unwrap() - expected).abs() < 1e-12, "{primary}");
        assert_eq!(seen.lock().unwrap().iter().filter(|p| **p == primary).count(), ATTEMPTS);
    }
}
