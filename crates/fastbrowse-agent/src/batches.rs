//! Many independent questions against one shared state, packed under Jev's input limits and sent together.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use fastbrowse_clients::validation::{Lines, Paid, jev_purpose, jev_spend};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::TokenBudget;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, Question};
use fastbrowse_core::models::{CostComponent, CostLine, JevPurpose};
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
    receipts: Vec<Paid>,
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
        let mut spent = lock(self.spent);
        for receipt in std::mem::take(&mut spent.receipts) {
            spent.paid.extend(receipt.lines());
        }
        if let Some(ledger) = self.ledger {
            ledger.state().lines.extend(spent.paid.clone());
        }
    }
}

fn lock(spent: &Mutex<Spent>) -> MutexGuard<'_, Spent> {
    // A panic while the lock was held leaves totals that are still the best record there is.
    spent.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The answers a run has had from Jev, by the exact state and question it sent.
///
/// A question asked again beside the same state is the same input, so its answer is already known: a tally checked
/// again after its claim was sent back re-asked fifty requests of identical questions on PyPI. The state is the whole
/// input beside the question, so a page that changed, even outside the viewport, is a different state and asked anew.
#[derive(Debug, Default)]
pub struct Known(Mutex<HashMap<String, HashMap<String, Answer>>>);

impl Known {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, HashMap<String, Answer>>> {
        // A panic while the lock was held leaves answers that are still what Jev said.
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn get(&self, state: &str, question: &str) -> Option<Answer> {
        self.lock().get(state)?.get(question).cloned()
    }

    fn insert(&self, state: &str, question: String, answer: Answer) {
        self.lock().entry(state.to_string()).or_default().insert(question, answer);
    }
}

tokio::task_local! {
    static KNOWN: Arc<Known>;
}

/// Run `future` with `known` answering the questions its passes repeat. A run holds one for its whole length.
pub async fn knowing<F: Future>(known: Arc<Known>, future: F) -> F::Output {
    KNOWN.scope(known, future).await
}

/// None when no batch was answered; a question too large to send with the state alone goes unscored.
#[allow(clippy::too_many_arguments)]
pub async fn evaluate_batches(
    purpose: JevPurpose,
    jev: &dyn DecisionClient,
    state: &Value,
    questions: &IndexMap<String, Question>,
    tokens: &TokenBudget,
    ledger: Option<&Ledger>,
    on_answer: Option<OnAnswer<'_>>,
    allow_failed_batches: bool,
) -> Result<Option<Answered>> {
    let ratio = tokens.chars_per_token;
    let written_state = dumps(state);
    let state_chars = text::len(&written_state) as i64;
    let state_size = state_chars as f64 / ratio;
    let known = KNOWN.try_with(Arc::clone).ok();
    // Answers already had for this state.
    let mut answered: IndexMap<String, Answer> = IndexMap::new();
    let mut batches: Vec<IndexMap<String, Question>> = Vec::new();
    let mut batch: IndexMap<String, Question> = IndexMap::new();
    let mut written: Vec<String> = Vec::new();
    let mut batch_size = 0.0;
    let mut sent_chars = 0usize;
    // Bounds the questions alone: every batch resends the state, so a state counted here would leave a long one room
    // for a single question per request. The state itself is held to the model's limits below.
    let limit = tokens.batch_tokens.min(tokens.state_plus_all_questions) as f64;
    let profile = jev.profile();
    let most = profile.max_questions.unwrap_or(usize::MAX);
    for (key, question) in questions {
        let json = question.dump_json();
        if let Some(answer) = known.as_ref().and_then(|known| known.get(&written_state, &json)) {
            answered.insert(key.clone(), answer);
            continue;
        }
        let size = text::len(&json) as f64 / ratio;
        if state_chars > tokens.input_chars(&[&json]) {
            continue;
        }
        if !batch.is_empty()
            && (batch.len() >= most || batch_size + size > limit || {
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
        return Ok((!answered.is_empty()).then(|| Answered {
            answers: answered,
            cost: Vec::new(),
            input_tokens: 0,
            requests: 0,
        }));
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
        let receipt = Paid::default();
        lock(&spent).receipts.push(receipt.clone());
        let (spent, known, written_state) = (&spent, &known, &written_state);
        tasks.push(Box::pin(async move {
            let evaluation = match jev_purpose(
                purpose,
                jev_spend(Lines::Paid(receipt.clone()), None, jev.evaluate(state, questions)),
            )
            .await
            {
                Ok(evaluation) => evaluation,
                Err(error) => {
                    // Provider adapters retain partial receipts in the batch's spend scope. Custom clients can
                    // return answered singles without recording them, so only those need the error's fallback.
                    if receipt.is_empty()
                        && let DecisionError::RetriesExhausted(exhausted) = &error
                    {
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
            // Kept as each batch comes back, so a pass cancelled partway still knows what it paid for.
            for (key, answer) in &evaluation.answers {
                let Some(question) = questions.get(key) else { continue };
                if let Some(known) = known {
                    known.insert(written_state, question.dump_json(), answer.clone());
                }
                if let Some(on_answer) = on_answer {
                    on_answer(key, answer);
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
    if spent.paid.is_empty() && answered.is_empty() {
        return Ok(None);
    }
    answered.extend(results.into_iter().flatten().flatten());
    Ok(Some(Answered {
        answers: answered,
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
    fn test_a_state_past_the_batch_size_still_packs_its_questions() {
        // A long history took a page's four thousand relevance questions to one request each, and the run's memory with them.
        let jev = fake("");
        let state = json!({"recent_actions": "x".repeat(30_000)});
        let many: IndexMap<String, Question> = (0..4000)
            .map(|i| (format!("r{i}"), NoulQuestion::new(format!("Is element {i} relevant?"), None, None).into()))
            .collect();
        let answered = block_on(evaluate_batches(
            JevPurpose::Verify,
            &jev,
            &state,
            &many,
            &TokenBudget::default(),
            None,
            None,
            false,
        ));
        assert_eq!(answered.unwrap().unwrap().answers.len(), 4000);
        let sent = jev.sent.lock().unwrap();
        assert!(sent.len() < 40, "{} requests for 4000 questions", sent.len());
    }

    #[test]
    fn test_a_pass_cancelled_by_a_deadline_keeps_the_cost_already_billed() {
        let jev = fake("b");
        let ledger = Ledger::new(Limits::default());
        let questions = questions(&[("a", "first"), ("b", "second")]);
        let tokens = one_per_batch();
        let state = json!({});
        let mut pass = Box::pin(evaluate_batches(
            JevPurpose::Verify,
            &jev,
            &state,
            &questions,
            &tokens,
            Some(&ledger),
            None,
            true,
        ));
        assert!(pass.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        // Dropped where a deadline would cancel it.
        drop(pass);
        assert_eq!(jev.sent.lock().unwrap().len(), 2);
        assert!((ledger.breakdown().known_dollars() - 0.01).abs() < 1e-12);
        assert!(ledger.breakdown().lines.iter().all(|line| line.decision_purpose == Some(JevPurpose::Verify)));
    }

    #[test]
    fn test_a_pass_the_call_limit_cannot_cover_counts_no_call() {
        let jev = fake("b");
        let ledger = Ledger::new(Limits { max_jev_calls: Some(1), ..Limits::default() });
        let questions = questions(&[("a", "first"), ("b", "second")]);
        let tokens = one_per_batch();
        let state = json!({});
        let mut pass = Box::pin(evaluate_batches(
            JevPurpose::Verify,
            &jev,
            &state,
            &questions,
            &tokens,
            Some(&ledger),
            None,
            true,
        ));
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
        let answered =
            block_on(evaluate_batches(JevPurpose::Verify, &jev, &json!({}), &many, &tokens, None, None, true))
                .unwrap()
                .unwrap();
        assert_eq!(answered.answers.len(), 40);
        let sent = jev.sent.lock().unwrap();
        assert!(sent.len() > 1 && sent.iter().all(|batch| batch.len() < 40));
    }

    #[test]
    fn a_request_carries_no_more_questions_than_the_model_takes() {
        let jev = Fake { max_questions: Some(3), ..fake("") };
        let many: IndexMap<String, Question> =
            (0..7).map(|i| (format!("q{i}"), NoulQuestion::new("x", None, None).into())).collect();
        let answered = block_on(evaluate_batches(
            JevPurpose::Verify,
            &jev,
            &json!({}),
            &many,
            &TokenBudget::default(),
            None,
            None,
            true,
        ))
        .unwrap();
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
        assert_eq!(
            block_on(evaluate_batches(JevPurpose::Verify, &jev, &state, &asked, &tiny, None, None, true)).unwrap(),
            None
        );
        assert!(jev.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_pass_asked_again_beside_the_same_state_sends_no_request() {
        // A tally checked again after its claim was sent back asked every member question a second time.
        let jev = fake("");
        let ledger = Ledger::new(Limits::default());
        let asked = questions(&[("a", "first"), ("b", "second")]);
        let tokens = one_per_batch();
        let state = json!({"notes": ["Released: Dec 6, 2024"]});
        let pass = || evaluate_batches(JevPurpose::Verify, &jev, &state, &asked, &tokens, Some(&ledger), None, false);
        let (first, again, changed) = block_on(knowing(Default::default(), async {
            let first = pass().await.unwrap().unwrap();
            let again = pass().await.unwrap().unwrap();
            let other = json!({"notes": ["Released: May 14, 2026"]});
            let changed =
                evaluate_batches(JevPurpose::Verify, &jev, &other, &asked, &tokens, Some(&ledger), None, false).await;
            (first, again, changed.unwrap().unwrap())
        }));
        assert_eq!((again.answers.len(), again.requests, again.cost.len()), (2, 0, 0));
        assert_eq!(again.answers, first.answers);
        assert_eq!(changed.requests, 2, "another state is asked anew");
        assert_eq!((jev.sent.lock().unwrap().len(), ledger.state().jev_calls), (4, 4));
    }

    #[test]
    fn a_cancelled_pass_keeps_the_answers_it_paid_for() {
        let jev = fake("b");
        let asked = questions(&[("a", "first"), ("b", "second")]);
        let tokens = one_per_batch();
        let state = json!({});
        let known = Arc::new(Known::default());
        let mut pass = Box::pin(knowing(
            known.clone(),
            evaluate_batches(JevPurpose::Verify, &jev, &state, &asked, &tokens, None, None, true),
        ));
        assert!(pass.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        drop(pass);
        let only_a = questions(&[("a", "first")]);
        let again = block_on(knowing(
            known,
            evaluate_batches(JevPurpose::Verify, &jev, &state, &only_a, &tokens, None, None, true),
        ));
        assert_eq!(again.unwrap().unwrap().requests, 0);
        assert_eq!(jev.sent.lock().unwrap().len(), 2);
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
        assert_eq!(
            block_on(evaluate_batches(JevPurpose::Verify, &Failing, &json!({}), &asked, &tokens, None, None, true))
                .unwrap(),
            None
        );
        let error =
            block_on(evaluate_batches(JevPurpose::Verify, &Failing, &json!({}), &asked, &tokens, None, None, false))
                .unwrap_err();
        assert_eq!(error, AgentError::Jev(DecisionError::Failed("down".into())));
    }
}
