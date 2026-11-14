mod common;

use common::answering;
use fastbrowse_clients::environment::Secret;
use fastbrowse_clients::http::Response;
use fastbrowse_clients::search::Exa;
use fastbrowse_core::models::CostBasis;
use serde_json::{Value, json};

#[tokio::test]
async fn search_retains_the_full_results_and_the_actual_receipt() {
    let results = json!([{"url":"https://example.org/source", "title":"Source", "extra":{"raw":"A  & B"}}]);
    let expected = results.clone();
    let http = answering(move |request| {
        assert_eq!(request.url, "https://api.exa.ai/search");
        assert_eq!(request.header("x-api-key"), Some("test-key"));
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["query"], "a research question");
        assert!(body.get("contents").is_none());
        let response = Response::json(200, &json!({"results":results,"costDollars":{"total":0.006}}));
        Box::pin(async move { Ok(response) })
    });
    let result = Exa::new(http, Secret::new("test-key")).search("a research question", 30.0).await.unwrap();
    assert_eq!(result.results, expected.as_array().unwrap().clone());
    assert_eq!(result.cost.dollars, Some(0.006));
    assert_eq!(result.cost.basis, CostBasis::Estimated);
}

#[tokio::test]
async fn invalid_responses_keep_the_charge_and_never_quote_provider_errors() {
    let http = answering(|_| Box::pin(async { Ok(Response::json(500, &json!({"error":"test-key"}))) }));
    let failure = Exa::new(http, Secret::new("test-key")).search("question", 30.0).await.unwrap_err();
    assert!(!failure.message.contains("test-key"));
    assert_eq!(failure.cost.basis, CostBasis::Bounded);
    assert_eq!(failure.cost.dollars, Some(0.007));
}
