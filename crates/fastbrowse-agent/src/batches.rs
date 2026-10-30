//! Many independent questions against one shared state, packed under Jev's input limits and sent together.

use std::sync::{Mutex, MutexGuard};

use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, Question};
use fastbrowse_core::models::{CostComponent, CostLine};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::Value;

use crate::error::Result;
use crate::support::try_join_all;

#[derive(Debug, Clone, PartialEq)]
pub struct Answered {
    /// Answers from every batch Jev accepted; a key left out went unscored.
    pub answers: IndexMap<String, Answer>,
    pub cost: Vec<CostLine>,
    pub input_tokens: i64,
    pub requests: i64,
}

/// Called with each answer as its batch comes back, for a caller that keeps answers a cancelled pass already paid for.
pub type OnAnswer<'a> = &'a (dyn Fn(&str, &Answer) + Send + Sync);

/// One batch's answers, or None when its request failed and failures are allowed.
type BatchAnswers = Option<IndexMap<String, Answer>>;

#[derive(Default)]
struct Spent {
    paid: Vec<CostLine>,
    input_tokens: i64,
    requests: i64,
}

/// Keeps what the pass was billed on every exit, a dropped future included.
///
/// A deadline can cancel the pass after some batches were billed. Their cost is kept without the limit check, so the
/// cancellation, not a budget stop raised over it, is what ends the run.
struct Billing<'a> {
    ledger: Option<&'a Ledger>,
    spent: &'a Mutex<Spent>,
}

impl Drop for Billing<'_> {
    fn drop(&mut self) {
        if let Some(ledger) = self.ledger {
            let paid = lock(self.spent).paid.clone();
            ledger.state().lines.extend(paid);
        }
    }
}

fn lock(spent: &Mutex<Spent>) -> MutexGuard<'_, Spent> {
    // A panic while the lock was held leaves totals that are still the best record there is.
    spent.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// None when no batch was answered; a question too large to send with the state alone goes unscored.
pub async fn evaluate_batches(
    jev: &dyn DecisionClient,
    state: &Value,
    questions: &IndexMap<String, Question>,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
    on_answer: Option<OnAnswer<'_>>,
    allow_failed_batches: bool,
) -> Result<Option<Answered>> {
    let ratio = tokens.chars_per_token;
    let state_chars = text::len(&dumps(state)) as i64;
    let state_size = state_chars as f64 / ratio;
    let mut batches: Vec<IndexMap<String, Question>> = Vec::new();
    let mut batch: IndexMap<String, Question> = IndexMap::new();
    let mut written: Vec<String> = Vec::new();
    let mut batch_size = 0.0;
    let mut sent_chars = 0usize;
    let limit = tokens.batch_tokens.min(tokens.state_plus_all_questions) as f64;
    let profile = jev.profile();
    let most = profile.max_questions.unwrap_or(usize::MAX);
    for (key, question) in questions {
        let json = question.dump_json();
        let size = text::len(&json) as f64 / ratio;
        if state_chars > tokens.input_chars(&[&json]) {
            continue;
        }
        if !batch.is_empty()
            && (batch.len() >= most || state_size + batch_size + size > limit || {
                let with: Vec<&str> = written.iter().map(String::as_str).chain([json.as_str()]).collect();
                state_chars > tokens.input_chars(&with)
            })
        {
            batches.push(std::mem::take(&mut batch));
            written.clear();
            batch_size = 0.0;
        }
        sent_chars += text::len(&json);
        batch.insert(key.clone(), question.clone());
        written.push(json);
        batch_size += size;
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    if batches.is_empty() {
        return Ok(None);
    }

    if let Some(ledger) = ledger {
        // The batches run together, so a limit checked per batch would pass them all and learn of the overrun only
        // once billed: the whole pass is priced, and every call counted, before any is sent.
        let sent = sent_chars as f64 / ratio;
        let estimate = profile.input_dollars(batches.len() as f64 * state_size + sent).unwrap_or(0.0);
        ledger.reserve(CostComponent::Jev, estimate, batches.len() as i64)?;
    }

    let spent = Mutex::new(Spent::default());
    let mut tasks: Vec<BoxFuture<'_, Result<BatchAnswers>>> = Vec::new();
    for questions in &batches {
        let spent = &spent;
        tasks.push(Box::pin(async move {
            let evaluation = match jev.evaluate(state, questions).await {
                Ok(evaluation) => evaluation,
                Err(error) => {
                    // Singles a split batch had answered were paid for even though the batch as a whole failed.
                    if let DecisionError::RetriesExhausted(exhausted) = &error {
                        lock(spent).paid.extend(exhausted.answered.iter().map(|single| single.cost.clone()));
                    }
                    if !allow_failed_batches {
                        return Err(error.into());
                    }
                    return Ok(None);
                }
            };
            {
                let mut spent = lock(spent);
                spent.paid.push(evaluation.cost.clone());
                spent.input_tokens += evaluation.input_tokens;
                spent.requests += evaluation.requests;
            }
            if let Some(on_answer) = on_answer {
                for (key, answer) in &evaluation.answers {
                    if questions.contains_key(key) {
                        on_answer(key, answer);
                    }
                }
            }
            Ok(Some(evaluation.answers))
        }));
    }
    let billing = Billing { ledger, spent: &spent };
    let results = try_join_all(tasks).await;
    drop(billing);
    let results = results?;
    if let Some(ledger) = ledger {
        ledger.check(0.0)?;
    }
    let spent = lock(&spent);
    if spent.paid.is_empty() {
        return Ok(None);
    }
    Ok(Some(Answered {
        answers: results.into_iter().flatten().flatten().collect(),
        cost: spent.paid.clone(),
        input_tokens: spent.input_tokens,
        requests: spent.requests,
    }))
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use fastbrowse_core::decision::{DecisionProfile, Evaluation, NoulAnswer, NoulQuestion, NoulTag};
    use fastbrowse_core::models::{BudgetResource, CostBasis, Limits};
    use serde_json::json;

    use super::*;
    use crate::error::AgentError;
    use crate::support::testing::block_on;

    fn one_per_batch() -> TokenBudget {
        // Room for one question per request, so every question is its own batch.
        TokenBudget { state_plus_largest_question: 80, state_plus_all_questions: 80, ..TokenBudget::default() }
    }

    fn questions(names: &[(&str, &str)]) -> IndexMap<String, Question> {
        names.iter().map(|(k, v)| (k.to_string(), NoulQuestion::new(*v, None, None).into())).collect()
    }

    /// Answers every question 0.5 at a cent; `stall` keys never come back.
    struct Fake {
        sent: Mutex<Vec<Vec<String>>>,
        stall: &'static str,
        max_questions: Option<usize>,
    }

    impl DecisionClient for Fake {
        fn evaluate<'a>(
            &'a self,
            _: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            self.sent.lock().unwrap().push(questions.keys().cloned().collect());
            let stall = questions.contains_key(self.stall);
            Box::pin(async move {
                if stall {
                    std::future::pending::<()>().await;
                }
                Ok(Evaluation {
                    model: "test".into(),
                    answers: questions
                        .keys()
                        .map(|k| (k.clone(), Answer::Noul(NoulAnswer { kind: NoulTag, probability: 0.5 })))
                        .collect(),
                    input_tokens: 10,
                    cost: CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.01)),
                    requests: 1,
                })
            })
        }

        fn profile(&self) -> DecisionProfile {
            DecisionProfile { max_questions: self.max_questions, ..DecisionProfile::jev() }
        }
    }

    fn fake(stall: &'static str) -> Fake {
        Fake { sent: Mutex::new(Vec::new()), stall, max_questions: None }
    }

    #[test]
    fn test_a_pass_cancelled_by_a_deadline_keeps_the_cost_already_billed() {
        let jev = fake("b");
        let ledger = Ledger::new(Limits::default());
        let questions = questions(&[("a", "first"), ("b", "second")]);
        let tokens = one_per_batch();
        let state = json!({});
        let mut pass = Box::pin(evaluate_batches(&jev, &state, &questions, &tokens, Some(&ledger), None, true));
        assert!(pass.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        // Dropped where a deadline would cancel it.
        drop(pass);
        assert_eq!(jev.sent.lock().unwrap().len(), 2);
        assert!((ledger.breakdown().known_dollars() - 0.01).abs() < 1e-12);
    }

    #[test]
    fn test_a_pass_the_call_limit_cannot_cover_counts_no_call() {
        let jev = fake("b");
        let ledger = Ledger::new(Limits { max_jev_calls: Some(1), ..Limits::default() });
        let questions = questions(&[("a", "first"), ("b", "second")]);
        let tokens = one_per_batch();
        let state = json!({});
        let mut pass = Box::pin(evaluate_batches(&jev, &state, &questions, &tokens, Some(&ledger), None, true));
        let Poll::Ready(Err(AgentError::Budget(stop))) = pass.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("the pass should stop before sending")
        };
        assert_eq!(stop.budget.resource, BudgetResource::JevCalls);
        assert!(jev.sent.lock().unwrap().is_empty());
        assert_eq!(ledger.state().jev_calls, 0);
    }

    #[test]
    fn test_many_questions_go_as_small_requests_even_when_one_would_fit() {
        let jev = fake("");
        let many: IndexMap<String, Question> =
            (0..40).map(|i| (format!("q{i}"), NoulQuestion::new("x".repeat(300), None, None).into())).collect();
        let tokens = TokenBudget { batch_tokens: 1000, ..TokenBudget::default() };
        let answered = block_on(evaluate_batches(&jev, &json!({}), &many, &tokens, None, None, true)).unwrap().unwrap();
        assert_eq!(answered.answers.len(), 40);
        let sent = jev.sent.lock().unwrap();
        assert!(sent.len() > 1 && sent.iter().all(|batch| batch.len() < 40));
    }

    #[test]
    fn a_request_carries_no_more_questions_than_the_model_takes() {
        let jev = Fake { max_questions: Some(3), ..fake("") };
        let many: IndexMap<String, Question> =
            (0..7).map(|i| (format!("q{i}"), NoulQuestion::new("x", None, None).into())).collect();
        let answered =
            block_on(evaluate_batches(&jev, &json!({}), &many, &TokenBudget::default(), None, None, true)).unwrap();
        assert_eq!(answered.unwrap().answers.len(), 7);
        let sizes: Vec<usize> = jev.sent.lock().unwrap().iter().map(Vec::len).collect();
        assert_eq!(sizes, [3, 3, 1]);
    }

    #[test]
    fn a_question_too_large_for_the_state_goes_unscored() {
        let jev = fake("");
        let tiny =
            TokenBudget { state_plus_largest_question: 1, state_plus_all_questions: 1, ..TokenBudget::default() };
        let asked = questions(&[("a", "first")]);
        let state = json!({"text": "x".repeat(100)});
        assert_eq!(block_on(evaluate_batches(&jev, &state, &asked, &tiny, None, None, true)).unwrap(), None);
        assert!(jev.sent.lock().unwrap().is_empty());
    }

    struct Failing;

    impl DecisionClient for Failing {
        fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
            fastbrowse_core::decision::DecisionProfile::jev()
        }

        fn evaluate<'a>(
            &'a self,
            _: &'a Value,
            _: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
            Box::pin(async { Err(DecisionError::Failed("down".into())) })
        }
    }

    #[test]
    fn a_failed_batch_is_unscored_unless_the_caller_wants_the_error() {
        let asked = questions(&[("a", "first")]);
        let tokens = TokenBudget::default();
        assert_eq!(block_on(evaluate_batches(&Failing, &json!({}), &asked, &tokens, None, None, true)).unwrap(), None);
        let error = block_on(evaluate_batches(&Failing, &json!({}), &asked, &tokens, None, None, false)).unwrap_err();
        assert_eq!(error, AgentError::Jev(DecisionError::Failed("down".into())));
    }
}
