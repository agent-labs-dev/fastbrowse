//! Search results are source links, never a provider-generated answer to the task.

use fastbrowse_core::models::{CostBasis, CostComponent, CostLine};
use serde_json::{Value, json};

use crate::environment::Secret;
use crate::http::Http;

/// Exa's auto search includes ten results for $0.007, without generated summaries or contents.
/// https://exa.ai/docs/admin/pricing.md, verified against a live receipt on 2026-11-12.
pub const SEARCH_DOLLARS: f64 = 0.007;

#[derive(Debug)]
pub struct SearchResponse {
    /// The complete result objects retain every field returned by the provider.
    pub results: Vec<Value>,
    pub cost: CostLine,
}

#[derive(Debug)]
pub struct SearchFailure {
    pub message: String,
    pub cost: CostLine,
}

#[derive(Clone)]
pub struct Exa {
    http: Http,
    key: Secret,
}

impl Exa {
    pub fn new(http: Http, key: Secret) -> Self {
        Self { http: http.with_credential_header(Some("x-api-key")), key }
    }

    pub async fn search(&self, query: &str, timeout_seconds: f64) -> Result<SearchResponse, SearchFailure> {
        let bounded = || CostLine::new(CostComponent::Search, CostBasis::Bounded, Some(SEARCH_DOLLARS));
        let response = self
            .http
            .post_json(
                "https://api.exa.ai/search",
                &json!({"query": query, "type": "auto", "numResults": 10}),
                &[("x-api-key".into(), self.key.expose().into())],
                timeout_seconds,
            )
            .await
            .map_err(|error| SearchFailure { message: error.name().into(), cost: bounded() })?;
        let body: Option<Value> = serde_json::from_str(&response.scrub(&response.text())).ok();
        // Exa documents costDollars as an estimate, including the value it reports for this request.
        // https://exa.ai/docs/reference/search.md
        let cost = body
            .as_ref()
            .and_then(|body| body.pointer("/costDollars/total"))
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .map(|cost| CostLine::new(CostComponent::Search, CostBasis::Estimated, Some(cost)))
            .unwrap_or_else(bounded);
        if !response.is_success() {
            return Err(SearchFailure { message: format!("search provider returned HTTP {}", response.status), cost });
        }
        let results =
            body.as_ref().and_then(|body| body.get("results")).and_then(Value::as_array).cloned().ok_or_else(|| {
                SearchFailure { message: "search provider returned no result array".into(), cost: cost.clone() }
            })?;
        Ok(SearchResponse { results, cost })
    }
}
