//! The run's clients, which send nothing for a decision the page redrew under ([`fastbrowse_core::superseding`]), and
//! blank every registered secret and address credential in what they do send.
//!
//! Every Jev and LLM request of a run passes through here, so this is where masking cannot be missed: each request
//! builder masked what it took from the page, and one that took a raw value (a control's value in the history of what
//! was typed, an input's value offered as a source) sent it to the model unmasked (#1381). Masking keeps lengths, so
//! an answer's offsets into what was sent stay valid. Option and question keys are code-made and kept: an answer names
//! them.

use std::sync::{Arc, Mutex};

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    BoxQuestion, ChoiceQuestion, DecisionClient, DecisionError, DecisionProfile, Evaluation, Images, NoulQuestion,
    Question, ScoreQuestion,
};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostComponent, JevPurpose, LLMPurpose};
use fastbrowse_core::safety::Redactor;
use fastbrowse_core::superseding::superseded;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::Value;

/// `client` as the run's, masking with `redactor`.
pub(super) fn decisions(client: &Arc<dyn DecisionClient>, redactor: &Arc<Mutex<Redactor>>) -> Arc<dyn DecisionClient> {
    Arc::new(Decisions { client: Arc::clone(client), redactor: Arc::clone(redactor) })
}

/// `client` as the run's, masking with `redactor`.
pub(super) fn generations(client: &Arc<dyn LLMClient>, redactor: &Arc<Mutex<Redactor>>) -> Arc<dyn LLMClient> {
    Arc::new(Generations { client: Arc::clone(client), redactor: Arc::clone(redactor) })
}

/// The run's decision client, sending nothing for a superseded decision.
struct Decisions {
    client: Arc<dyn DecisionClient>,
    redactor: Arc<Mutex<Redactor>>,
}

impl Decisions {
    fn masked(&self, state: &Value, questions: &IndexMap<String, Question>) -> (Value, IndexMap<String, Question>) {
        let redactor = self.redactor.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut state = state.clone();
        redactor.mask_value(&mut state);
        let mask = |text: &str| redactor.mask(text);
        let question = |question: &Question| match question {
            Question::Choice(choice) => Question::Choice(ChoiceQuestion {
                instructions: mask(&choice.instructions),
                criteria: choice
                    .criteria
                    .iter()
                    .map(|(key, value)| {
                        let mut value = value.clone();
                        redactor.mask_value(&mut value);
                        (key.clone(), value)
                    })
                    .collect(),
                ..choice.clone()
            }),
            Question::Noul(noul) => Question::Noul(NoulQuestion {
                instructions: mask(&noul.instructions),
                r#true: noul.r#true.as_deref().map(mask),
                r#false: noul.r#false.as_deref().map(mask),
                ..noul.clone()
            }),
            Question::Score(score) => Question::Score(ScoreQuestion {
                instructions: mask(&score.instructions),
                criteria: score.criteria.iter().map(|criterion| mask(criterion)).collect(),
                ..score.clone()
            }),
            Question::Box(boxed) => {
                Question::Box(BoxQuestion { instructions: mask(&boxed.instructions), ..boxed.clone() })
            }
        };
        (state, questions.iter().map(|(key, item)| (key.clone(), question(item))).collect())
    }
}

impl DecisionClient for Decisions {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if superseded() {
                return Err(DecisionError::Superseded);
            }
            let (state, questions) = self.masked(state, questions);
            self.client.evaluate(&state, &questions).await
        })
    }

    fn evaluate_images<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
        images: &'a Images,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if superseded() {
                return Err(DecisionError::Superseded);
            }
            let (state, questions) = self.masked(state, questions);
            self.client.evaluate_images(&state, &questions, images).await
        })
    }

    fn profile(&self) -> DecisionProfile {
        self.client.profile()
    }

    fn reserves(&self, purpose: JevPurpose) -> Option<CostComponent> {
        self.client.reserves(purpose)
    }
}

/// The run's LLM client, sending nothing for a superseded decision.
struct Generations {
    client: Arc<dyn LLMClient>,
    redactor: Arc<Mutex<Redactor>>,
}

impl LLMClient for Generations {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            if superseded() {
                return Err(LLMError::Superseded);
            }
            let messages: Vec<Message> = {
                let redactor = self.redactor.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                messages
                    .iter()
                    .map(|message| Message { content: redactor.mask(&message.content), ..message.clone() })
                    .collect()
            };
            self.client.generate_value(purpose, &messages, schema, ledger).await
        })
    }

    fn context_tokens(&self, purpose: LLMPurpose) -> BoxFuture<'_, Option<i64>> {
        self.client.context_tokens(purpose)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CANARY: &str = "Bound4ry9w";

    /// Every request as the provider would be sent it.
    #[derive(Default)]
    struct Sent(Mutex<Vec<String>>);

    impl DecisionClient for Sent {
        fn profile(&self) -> DecisionProfile {
            DecisionProfile::jev()
        }

        fn evaluate<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
            self.0.lock().unwrap().push(format!("{state}{}", serde_json::to_string(questions).unwrap()));
            Box::pin(async { Err(DecisionError::Superseded) })
        }

        fn evaluate_images<'a>(
            &'a self,
            state: &'a Value,
            questions: &'a IndexMap<String, Question>,
            _: &'a Images,
        ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
            self.evaluate(state, questions)
        }
    }

    impl LLMClient for Sent {
        fn generate_value<'a>(
            &'a self,
            _: LLMPurpose,
            messages: &'a [Message],
            _: &'a dyn ResponseSchema,
            _: Option<&'a Ledger>,
        ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
            self.0.lock().unwrap().extend(messages.iter().map(|message| message.content.clone()));
            Box::pin(async { Err(LLMError::Superseded) })
        }
    }

    struct Anything;

    impl ResponseSchema for Anything {
        fn name(&self) -> String {
            "Anything".into()
        }

        fn json_schema(&self) -> Value {
            json!({})
        }

        fn validate_json(&self, text: &str) -> Result<Value, String> {
            serde_json::from_str(text).map_err(|error| error.to_string())
        }
    }

    /// A request builder that takes a raw value (what was typed, an input's value offered as a source) still sends it
    /// masked: the history of typed text and an input's value went to Jev unmasked (#1381).
    #[tokio::test]
    async fn test_a_request_built_from_raw_text_reaches_jev_and_the_llm_masked() {
        let redactor = Arc::new(Mutex::new(Redactor::new()));
        redactor.lock().unwrap().register_address(&format!("https://ops:{CANARY}@shop.test/"));
        let sent = Arc::new(Sent::default());
        let jev = decisions(&(sent.clone() as Arc<dyn DecisionClient>), &redactor);
        let llm = generations(&(sent.clone() as Arc<dyn LLMClient>), &redactor);
        let raw = format!("typed {CANARY} into Password");
        let state = json!({"history": [raw], "sources": {raw.clone(): raw}});
        let questions = IndexMap::from([(
            "next".to_string(),
            Question::Noul(NoulQuestion::new(format!("Was {CANARY} typed?"), Some(&raw), None)),
        )]);
        let _ = jev.evaluate(&state, &questions).await;
        let _ = llm.generate_value(LLMPurpose::Compose, &[Message::user(raw.clone())], &Anything, None).await;
        let sent = sent.0.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|request| !request.contains(CANARY)), "{sent:?}");
    }
}
