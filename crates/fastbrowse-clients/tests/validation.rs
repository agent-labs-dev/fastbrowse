//! tests/clients/test_validation.py: retries, races and the accounting of what they cost, seen through the Jev clients
//! since the request loop itself is crate-private.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{Script, answering, reply, text};
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_clients::typesafe::TypeSafeJevClient;
use fastbrowse_clients::validation::{
    JEV_HEDGE_SECONDS, Lines, Paid, RETRY_DELAYS_SECONDS, RequestUsage, jev_spend, with_discarded,
};
use fastbrowse_clients::vercel::{GATEWAY_URL, VercelGatewayJevClient};
use fastbrowse_core::jev::{JEV_DOLLARS_PER_INPUT_TOKEN, JevClient, JevError, NoulQuestion, Question};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, MaybeUnavailable};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::telemetry::Trace;
use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::Instant;

fn noul(instructions: &str) -> Question {
    NoulQuestion::new(instructions, None, None).into()
}

fn one() -> IndexMap<String, Question> {
    IndexMap::from([("q".to_string(), noul("Is it?"))])
}

fn pair() -> IndexMap<String, Question> {
    IndexMap::from([("a".to_string(), noul("Is it?")), ("b".to_string(), noul("Is it?"))])
}

fn keys(request: &Request) -> Vec<String> {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    body["questions"].as_object().unwrap().keys().cloned().collect()
}

fn answered() -> Response {
    Response::json(200, &json!({"answers": {"q": {"type": "noul", "noul": 1}}, "usage": {"input_tokens": 100}}))
}

fn direct(http: Http) -> TypeSafeJevClient {
    TypeSafeJevClient::direct("key", http)
}

/// Sets its flag when dropped, which is how a test sees a request cancelled.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn events(trace: &Trace, kind: &str) -> Vec<Value> {
    trace.events().into_iter().filter(|event| event["event"] == kind).collect()
}

#[tokio::test(start_paused = true)]
async fn test_a_stalled_request_is_raced_and_the_loser_cancelled() {
    for winner in [1, 2] {
        let calls = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let hedged = Arc::new(Notify::new());
        let (count, flag, race) = (calls.clone(), cancelled.clone(), hedged.clone());
        let http = answering(move |_| {
            let number = count.fetch_add(1, Ordering::SeqCst) + 1;
            let (flag, race) = (flag.clone(), race.clone());
            Box::pin(async move {
                if number == 2 {
                    race.notify_waiters();
                }
                if number != winner {
                    let _cancelled = SetOnDrop(flag);
                    std::future::pending::<()>().await;
                }
                if number == 1 {
                    race.notified().await;
                }
                Ok(answered())
            })
        });
        let started = Instant::now();
        let result = direct(http).evaluate(&json!("page"), &one()).await.unwrap();
        // Answered once the race delay passed, not after the stalled request's attempt deadline.
        assert!(started.elapsed().as_secs_f64() < JEV_HEDGE_SECONDS + 0.5);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(cancelled.load(Ordering::SeqCst));
        // The loser carried the same input, so it is charged as an estimate beside the winner.
        assert_eq!(result.requests, 2);
        assert_eq!((result.cost.basis, result.cost.input_tokens), (CostBasis::Estimated, 200));
    }
}

#[tokio::test(start_paused = true)]
async fn test_a_fast_request_is_not_hedged() {
    let script = Script::new([common::Step::Reply(answered())]);
    let result = direct(script.http()).evaluate(&json!("page"), &one()).await.unwrap();
    assert_eq!(script.requests().len(), 1);
    assert_eq!((result.requests, result.cost.basis), (1, CostBasis::Estimated));
    assert_eq!(result.cost.input_tokens, 100);
}

#[tokio::test(start_paused = true)]
async fn test_batch_hedges_consume_the_split_retry_budget() {
    // Python lowers the race delay to 1ms; on paused time the real one passes at once to the same effect.
    for gateway_route in [false, true] {
        for earlier_failures in [0, 2, 4] {
            let sent: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
            let paired = Arc::new(Notify::new());
            let (log, twin) = (sent.clone(), paired.clone());
            let http = answering(move |request| {
                let keys = keys(request);
                let number = {
                    let mut log = log.lock().unwrap();
                    log.push(keys.clone());
                    log.len()
                };
                let twin = twin.clone();
                Box::pin(async move {
                    if number <= earlier_failures {
                        return Ok(Response::new(429, ""));
                    }
                    if keys.len() > 1 {
                        if number == earlier_failures + 2 {
                            twin.notify_waiters();
                        } else {
                            twin.notified().await;
                        }
                    }
                    Ok(Response::new(503, ""))
                })
            });
            let client: Box<dyn JevClient> = if gateway_route {
                Box::new(VercelGatewayJevClient::new("key", http, GATEWAY_URL))
            } else {
                Box::new(direct(http))
            };
            let error = client.evaluate(&json!("page"), &pair()).await.unwrap_err();
            let JevError::RetriesExhausted(error) = error else { panic!("exhausted") };
            let sent = sent.lock().unwrap();
            let batch_requests = earlier_failures + 2;
            let both = vec!["a".to_string(), "b".to_string()];
            assert!(sent[..batch_requests].iter().all(|keys| *keys == both));
            assert!(sent[batch_requests..].iter().all(|keys| *keys == ["a"]));
            assert_eq!(sent.len() - batch_requests, RETRY_DELAYS_SECONDS.len() + 1 - batch_requests);
            assert_eq!(error.requests as usize, sent.len());
            assert_eq!(sent.len(), RETRY_DELAYS_SECONDS.len() + 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_an_exhausted_split_counts_every_request_it_sent() {
    // A wiki-godel row said "after 1 failed requests in 0.7s" for a call whose events showed five: the message counted
    // only the last single's own failures, not the batch and the singles before it.
    for gateway_route in [false, true] {
        let sent = Arc::new(AtomicUsize::new(0));
        let count = sent.clone();
        let http = answering(move |request| {
            count.fetch_add(1, Ordering::SeqCst);
            let response = if keys(request) != ["a"] {
                Response::json(503, &json!({"error": {"message": "Service temporarily unavailable"}}))
            } else {
                let answer = if gateway_route {
                    json!({"type": "boolean", "probability": 1})
                } else {
                    json!({"type": "noul", "noul": 1})
                };
                let tokens = if gateway_route { "inputTokens" } else { "input_tokens" };
                Response::json(200, &json!({"answers": {"a": answer}, "usage": {tokens: 1}}))
            };
            Box::pin(async move { Ok(response) })
        });
        let client: Box<dyn JevClient> = if gateway_route {
            Box::new(VercelGatewayJevClient::new("key", http, GATEWAY_URL))
        } else {
            Box::new(direct(http))
        };
        let message = client.evaluate(&json!("page"), &pair()).await.unwrap_err().to_string();
        let sent = sent.load(Ordering::SeqCst);
        assert!(sent > 3);
        assert!(message.contains(&format!("after {sent} requests in ")), "{message}");
        assert!(message.ends_with("last: HTTP 503: Service temporarily unavailable"), "{message}");
    }
}

#[tokio::test(start_paused = true)]
async fn test_retry_waits_for_the_servers_retry_after() {
    let trace = Trace::collecting();
    let script = Script::new([
        common::Step::Reply(Response::new(429, "").with_header("retry-after-ms", "250")),
        common::Step::Reply(answered()),
    ]);
    let result = direct(script.traced(&trace)).evaluate(&json!("page"), &one()).await.unwrap();
    let waits: Vec<Value> = events(&trace, "request_retry").iter().map(|event| event["wait"].clone()).collect();
    assert_eq!(waits, [json!(0.25)]);
    assert_eq!((result.requests, result.cost.input_tokens), (2, 100), "a refused request costs nothing");
}

#[tokio::test(start_paused = true)]
async fn test_a_brief_outage_is_outlasted() {
    // A Cloudflare edge error in front of the provider (520 to 524) is as transient as its 503: issue #125.
    for status in [503, 520] {
        let trace = Trace::collecting();
        let mut steps: Vec<common::Step> = (0..4).map(|_| text(status, "")).collect();
        steps.push(common::Step::Reply(answered()));
        let script = Script::new(steps);
        direct(script.traced(&trace)).evaluate(&json!("page"), &one()).await.unwrap();
        let waited: f64 = events(&trace, "request_retry").iter().map(|event| event["wait"].as_f64().unwrap()).sum();
        assert!(waited > 10.0, "{waited}");
    }
}

#[tokio::test(start_paused = true)]
async fn test_a_completed_hedge_loser_still_counts() {
    let calls = Arc::new(AtomicUsize::new(0));
    let hedged = Arc::new(Notify::new());
    let (count, race) = (calls.clone(), hedged.clone());
    let http = answering(move |_| {
        let number = count.fetch_add(1, Ordering::SeqCst) + 1;
        let race = race.clone();
        Box::pin(async move {
            if number == 2 {
                race.notify_waiters();
            } else {
                race.notified().await;
            }
            Ok(answered())
        })
    });
    let result = direct(http).evaluate(&json!("page"), &one()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!((result.cost.basis, result.cost.input_tokens), (CostBasis::Estimated, 200));
}

#[test]
fn test_a_discarded_jev_request_is_charged_as_an_estimate() {
    let cost = CostLine { input_tokens: 900, ..CostLine::new(CostComponent::Jev, CostBasis::Metered, Some(0.002)) };
    let charged = with_discarded(cost.clone(), &RequestUsage { unaccounted_requests: 1, ..RequestUsage::default() });
    assert_eq!((charged.basis, charged.dollars, charged.input_tokens), (CostBasis::Estimated, Some(0.004), 1800));
    assert_eq!(with_discarded(cost.clone(), &RequestUsage::default()), cost);
}

/// Python fixes the clock to check the error's seconds (27); a paused clock counts the backoff waits, so only the
/// usage and the redaction are checked.
#[tokio::test(start_paused = true)]
async fn test_exhausted_status_keeps_timing_usage_and_redacts_the_key() {
    let mut steps = vec![common::Step::Fail(TransportError::Timeout("ReadTimeout"))];
    steps.extend((0..5).map(|_| text(503, &format!("secret-key{}", "x".repeat(1000)))));
    let script = Script::new(steps);
    let client = TypeSafeJevClient::direct("secret-key", script.http());
    let error = client.evaluate(&json!("page"), &one()).await.unwrap_err();
    let JevError::RetriesExhausted(exhausted) = &error else { panic!("exhausted") };
    assert_eq!(exhausted.unaccounted_requests, 1);
    assert!(exhausted.seconds > 0.0);
    assert!(!error.to_string().contains("secret-key"));
    assert!(error.to_string().len() < 500);
}

#[tokio::test(start_paused = true)]
async fn test_a_request_that_can_never_be_sent_is_not_an_outage() {
    // Retrying it would fail the same way every time, and an eval would re-run the task forever.
    let script = Script::new([common::Step::Fail(TransportError::Fatal("UnsupportedProtocol"))]);
    let error =
        TypeSafeJevClient::direct("secret-key", script.http()).evaluate(&json!("page"), &one()).await.unwrap_err();
    assert!(error.to_string().contains("could not be sent"), "{error}");
    assert_eq!(script.requests().len(), 1);
    assert!(!error.is_unavailable());
}

#[tokio::test(start_paused = true)]
async fn test_time_lost_to_a_503_is_traced_for_evals_to_leave_out() {
    let trace = Trace::collecting();
    let script = Script::new([text(503, ""), common::Step::Reply(answered()), common::Step::Reply(answered())]);
    let client = direct(script.traced(&trace));
    for _ in 0..2 {
        client.evaluate(&json!("page"), &one()).await.unwrap();
    }
    let lost = events(&trace, "request_transient");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["call"], "jev");
    assert!(lost[0]["ended"].as_f64() >= lost[0]["began"].as_f64());
}

#[tokio::test(start_paused = true)]
async fn test_a_slow_jev_call_is_traced_for_evals_to_rerun() {
    // Jev answers in about half a second at any page size; a call past the limit is its outage, not the agent. Python
    // fixes the clock; here the provider takes 0.6s, then 2.5s, on paused time. The 2.5s call is raced at 1.5s and
    // its twin answers later, so the first still answers at 2.5s.
    let trace = Trace::collecting();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let http = answering(move |_| {
        let number = count.fetch_add(1, Ordering::SeqCst);
        let seconds = if number == 0 { 0.6 } else { 2.5 };
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_secs_f64(seconds)).await;
            Ok(answered())
        })
    });
    let client = direct(http.with_trace(trace.clone()));
    client.evaluate(&json!("page"), &one()).await.unwrap();
    client.evaluate(&json!("page"), &one()).await.unwrap();
    assert_eq!(events(&trace, "request_slow"), [json!({"event": "request_slow", "call": "jev", "seconds": 2.5})]);
}

#[tokio::test]
async fn test_nested_gateway_reason_redacts_before_truncating() {
    // The credential the response was requested with is only known to the client, so the description is read off the
    // error a gateway client reports.
    let key = "gateway-secret-key-that-must-not-leak";
    let reason = format!("{}{key}", "x".repeat(190));
    let body = json!({"error": {"message": "provider returned 400", "param": {"message": reason}}});
    let script = Script::new([reply(400, body)]);
    let error = VercelGatewayJevClient::new(key, script.http(), GATEWAY_URL)
        .evaluate(&json!("page"), &one())
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("gateway-se"), "{error}");
    assert!(error.contains("[api key]"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn test_cancelled_dispatched_request_retains_estimated_input_cost() {
    for physical_requests in [1, 2] {
        let sent = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let (signal, count, kept) = (sent.clone(), calls.clone(), bodies.clone());
        let http = answering(move |request| {
            kept.lock().unwrap().push(request.body.clone());
            let number = count.fetch_add(1, Ordering::SeqCst) + 1;
            let signal = signal.clone();
            Box::pin(async move {
                if number == physical_requests {
                    signal.notify_one();
                }
                std::future::pending().await
            })
        });
        let paid = Paid::default();
        let client = direct(http);
        let page = json!("page");
        let questions = one();
        tokio::select! {
            _ = jev_spend(Lines::Paid(paid.clone()), None, client.evaluate(&page, &questions)) => panic!("cancelled"),
            _ = sent.notified() => {}
        }
        let costs = paid.lines();
        assert_eq!(costs.len(), physical_requests);
        // Priced as Python prices it: `json.dumps(body)` characters over three per token.
        let body: Value = serde_json::from_slice(&bodies.lock().unwrap()[0]).unwrap();
        let tokens = (dumps(&body).len() as f64 / 3.0).ceil() as i64;
        assert_eq!((costs[0].component, costs[0].basis), (CostComponent::Jev, CostBasis::Estimated));
        assert_eq!(costs[0].input_tokens, tokens);
        assert_eq!(costs[0].dollars, Some(tokens as f64 * JEV_DOLLARS_PER_INPUT_TOKEN));
    }
}

#[tokio::test(start_paused = true)]
async fn test_split_cancellation_retains_answered_and_cancelled_costs_once() {
    // Python drives asking_split with a fake ask; it is crate-private here, so a real batch splits instead: two 503s,
    // the first single answers, and the second is cancelled in flight.
    let sent = Arc::new(Notify::new());
    let seen: Arc<Mutex<HashMap<Vec<String>, usize>>> = Arc::default();
    let (signal, counts) = (sent.clone(), seen.clone());
    let http = answering(move |request| {
        let keys = keys(request);
        *counts.lock().unwrap().entry(keys.clone()).or_default() += 1;
        let signal = signal.clone();
        Box::pin(async move {
            match keys.as_slice() {
                [_, _] => Ok(Response::new(503, "")),
                [first] if first == "first" => Ok(Response::json(
                    200,
                    &json!({"answers": {"first": {"type": "noul", "noul": 1}},
                        "usage": {"input_tokens": 1, "cost": 0.01}}),
                )),
                _ => {
                    signal.notify_one();
                    std::future::pending().await
                }
            }
        })
    });
    let paid = Paid::default();
    let client = direct(http);
    let page = json!("page");
    let questions = IndexMap::from([("first".to_string(), noul("First")), ("second".to_string(), noul("Second"))]);
    tokio::select! {
        _ = jev_spend(Lines::Paid(paid.clone()), None, client.evaluate(&page, &questions)) => panic!("cancelled"),
        _ = sent.notified() => {}
    }
    let costs = paid.lines();
    assert_eq!(costs.len(), 2);
    assert_eq!((costs[0].basis, costs[0].dollars), (CostBasis::Metered, Some(0.01)));
    assert_eq!(costs[1].basis, CostBasis::Estimated);
    assert_eq!(costs[1].dollars, Some(costs[1].input_tokens as f64 * JEV_DOLLARS_PER_INPUT_TOKEN));
}
