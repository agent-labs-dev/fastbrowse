//! Provider partial receipts reach the run ledger once through direct and batched callers.

use std::sync::Arc;

use fastbrowse_agent::batches::evaluate_batches;
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_clients::system_one::{SystemOneClient, TYPESAFE_URL};
use fastbrowse_clients::validation::{Lines, jev_purpose, jev_spend};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{DecisionClient, JEV_MODEL, NoulQuestion, Question};
use fastbrowse_core::models::{CostBasis, JevPurpose, Limits};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

fn partial() -> SystemOneClient {
    let http = Http::new(Arc::new(|request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let keys: Vec<_> = body["questions"].as_object().unwrap().keys().cloned().collect();
        let response = if keys == ["first"] {
            Response::json(
                200,
                &json!({"answers": {"first": {"type": "noul", "noul": 1}},
                "usage": {"input_tokens": 1, "cost": 0.01}}),
            )
        } else {
            Response::new(503, "")
        };
        Box::pin(async move { Ok(response) })
    }));
    SystemOneClient::new("test", http, TYPESAFE_URL, JEV_MODEL).typesafe()
}

fn questions() -> IndexMap<String, Question> {
    ["first", "second"].into_iter().map(|key| (key.into(), NoulQuestion::new(key, None, None).into())).collect()
}

#[tokio::test(start_paused = true)]
async fn an_exhausted_split_keeps_its_answered_receipt_once_for_each_caller() {
    for batched in [false, true] {
        let client = partial();
        let ledger = Arc::new(Ledger::new(Limits::default()));
        let page = json!("page");
        let questions = questions();
        jev_spend(Lines::Ledger(ledger.clone()), Some(ledger.clone()), async {
            if batched {
                evaluate_batches(
                    JevPurpose::Verify,
                    &client,
                    &page,
                    &questions,
                    &TokenBudget::default(),
                    Some(&ledger),
                    true,
                )
                .await
                .unwrap();
            } else {
                assert!(client.evaluate(&page, &questions).await.is_err());
            }
        })
        .await;
        let paid: Vec<_> =
            ledger.state().lines.iter().filter(|line| line.basis == CostBasis::Metered).cloned().collect();
        assert_eq!(paid.len(), 1, "batched={batched}: {paid:?}");
        assert_eq!(paid[0].dollars, Some(0.01));
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_split_batch_keeps_answered_and_inflight_receipts_once() {
    let sent = Arc::new(tokio::sync::Notify::new());
    let signal = sent.clone();
    let http = Http::new(Arc::new(move |request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let keys: Vec<_> = body["questions"].as_object().unwrap().keys().cloned().collect();
        let signal = signal.clone();
        Box::pin(async move {
            if keys == ["second"] {
                signal.notify_one();
                std::future::pending().await
            } else if keys == ["first"] {
                Ok(Response::json(
                    200,
                    &json!({"answers": {"first": {"type": "noul", "noul": 1}},
                    "usage": {"input_tokens": 1, "cost": 0.01}}),
                ))
            } else {
                Ok(Response::new(503, ""))
            }
        })
    }));
    let client = SystemOneClient::new("test", http, TYPESAFE_URL, JEV_MODEL).typesafe();
    let ledger = Arc::new(Ledger::new(Limits::default()));
    let page = json!("page");
    let questions = questions();
    let tokens = TokenBudget::default();
    tokio::select! {
        _ = jev_spend(Lines::Ledger(ledger.clone()), Some(ledger.clone()),
            evaluate_batches(JevPurpose::Verify, &client, &page, &questions, &tokens, Some(&ledger), true)) => panic!("cancelled"),
        _ = sent.notified() => {}
    }
    let paid = ledger.state().lines.clone();
    assert_eq!(paid.len(), 2, "{paid:?}");
    assert_eq!(paid.iter().filter(|line| line.basis == CostBasis::Metered).count(), 1);
    assert_eq!(paid.iter().filter(|line| line.basis == CostBasis::Estimated).count(), 1);
}

#[tokio::test(start_paused = true)]
async fn failover_receipts_match_direct_and_batched_callers() {
    for succeeds in [false, true] {
        let mut receipts = Vec::new();
        for batched in [false, true] {
            let http =
                Http::new(Arc::new(move |request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let answers: serde_json::Map<_, _> = body["questions"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .map(|key| (key.clone(), json!({"type": "noul", "noul": 1})))
                        .collect();
                    let response = if succeeds {
                        Response::json(200, &json!({"answers": answers, "usage": {"input_tokens": 1, "cost": 0.02}}))
                    } else {
                        Response::new(503, "")
                    };
                    Box::pin(async move { Ok(response) })
                }));
            let backup = SystemOneClient::new("test", http.clone(), TYPESAFE_URL, JEV_MODEL).typesafe();
            let client =
                fastbrowse_clients::failover::FailoverDecisionClient::new(Box::new(partial()), Box::new(backup), http);
            let ledger = Arc::new(Ledger::new(Limits::default()));
            let page = json!("page");
            let questions = questions();
            jev_spend(Lines::Ledger(ledger.clone()), Some(ledger.clone()), async {
                if batched {
                    evaluate_batches(
                        JevPurpose::Verify,
                        &client,
                        &page,
                        &questions,
                        &TokenBudget::default(),
                        Some(&ledger),
                        true,
                    )
                    .await
                    .unwrap();
                } else if let Ok(evaluation) = jev_purpose(JevPurpose::Verify, client.evaluate(&page, &questions)).await
                {
                    ledger.record([evaluation.cost]).unwrap();
                }
            })
            .await;
            let mut lines = ledger.state().lines.clone();
            for line in &mut lines {
                line.seconds = None;
            }
            assert!(!lines.is_empty());
            receipts.push(lines);
        }
        assert_eq!(receipts[0], receipts[1], "backup succeeds={succeeds}");
    }
}

struct CustomPartial;

impl DecisionClient for CustomPartial {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        _: &'a Value,
        _: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>> {
        use fastbrowse_core::decision::{DecisionError, Evaluation, RetriesExhausted};
        use fastbrowse_core::models::{CostComponent, CostLine};
        Box::pin(async {
            let single = Evaluation {
                model: JEV_MODEL.into(),
                answers: IndexMap::new(),
                input_tokens: 1,
                requests: 1,
                cost: CostLine::new(CostComponent::Jev, CostBasis::Metered, Some(0.01)),
            };
            Err(DecisionError::RetriesExhausted(Box::new(RetriesExhausted {
                message: "partial".into(),
                seconds: 0.0,
                unaccounted_requests: 0,
                requests: 3,
                answered: vec![single.clone(), single],
                last: "failed".into(),
            })))
        })
    }
}

#[tokio::test]
async fn a_custom_clients_distinct_equal_cost_receipts_are_both_retained() {
    let ledger = Ledger::new(Limits::default());
    evaluate_batches(
        JevPurpose::Verify,
        &CustomPartial,
        &json!("page"),
        &questions(),
        &TokenBudget::default(),
        Some(&ledger),
        true,
    )
    .await
    .unwrap();
    let paid = ledger.state().lines.clone();
    assert_eq!(paid.len(), 2);
    assert_eq!(paid[0], paid[1]);
    assert_eq!(ledger.breakdown().known_dollars(), 0.02);
}

#[tokio::test(start_paused = true)]
async fn partial_receipts_are_returned_without_a_run_ledger() {
    let clients: [(Box<dyn DecisionClient>, usize, f64); 2] =
        [(Box::new(CustomPartial), 2, 0.02), (Box::new(partial()), 1, 0.01)];
    for (client, count, dollars) in clients {
        let answered = evaluate_batches(
            JevPurpose::Verify,
            &*client,
            &json!("page"),
            &questions(),
            &TokenBudget::default(),
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(answered.cost.len(), count);
        assert_eq!(answered.cost.iter().map(|line| line.dollars.unwrap()).sum::<f64>(), dollars);
    }
}
