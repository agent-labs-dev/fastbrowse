//! The run's clients, which send nothing for a decision the page redrew under ([`fastbrowse_core::superseding`]), and
//! blank every registered secret and address credential in what they do send.
//!
//! Every Jev and LLM request of a run passes through here, so this is where masking cannot be missed: each request
//! builder masked what it took from the page, and one that took a raw value (a control's value in the history of what
//! was typed, an input's value offered as a source) sent it to the model unmasked (#1381). Masking keeps lengths, so
//! an answer's offsets into what was sent stay valid. Only string values are masked: map keys, question and option
//! keys, and the ids the run keeps (`Redactor::keep`) are code-made, and an answer names them, so a password that is
//! one letter of a key or equal to a control id left a choice that no longer resolved. A name a caller chose is never
//! a key here: it reaches a model as a value under a numbered handle that maps back to it.
//!
//! No image goes out once the run holds an address credential (`Redactor::holds_credentials`): a page can paint its
//! own address on a canvas, and pixels cannot be masked. The agent takes no image then (`Agent::image`); a request
//! that carries one anyway is refused here, so an image path added later cannot send one.

use std::sync::{Arc, Mutex};

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    BoxQuestion, ChoiceQuestion, DecisionClient, DecisionError, DecisionProfile, Evaluation, Images, NoulQuestion,
    Question, ScoreQuestion,
};
use fastbrowse_core::llm::{FaultClass, Generation, LLMClient, LLMError, Message, ResponseSchema};
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

/// Why a request carrying an image is refused once the run holds an address credential.
const WITHHELD: &str =
    "an image of the page was sent after the run learnt an address credential, which pixels cannot mask";

fn held(redactor: &Mutex<Redactor>) -> bool {
    redactor.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).holds_credentials()
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
            if !images.is_empty() && held(&self.redactor) {
                return Err(DecisionError::failed(WITHHELD));
            }
            let (state, questions) = self.masked(state, questions);
            self.client.evaluate_images(&state, &questions, images).await
        })
    }

    fn profile(&self) -> DecisionProfile {
        self.client.profile()
    }

    fn image_tokens(&self, state: &Value, questions: &IndexMap<String, Question>, images: &Images) -> i64 {
        self.client.image_tokens(state, questions, images)
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
            if messages.iter().any(|message| !message.images.is_empty()) && held(&self.redactor) {
                return Err(LLMError::failed(FaultClass::Request, WITHHELD));
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
        let state = json!({"history": [raw], "sources": {"input:0": raw}});
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

    /// A password that is one letter, or equal to a control id, bulleted the keys and ids an answer names, so the
    /// choice no longer resolved and the run stalled: only values are masked, and kept ids are left whole.
    #[tokio::test]
    async fn test_keys_and_kept_ids_reach_jev_whole_whatever_the_password() {
        for password in ["p", "e1"] {
            let redactor = Arc::new(Mutex::new(Redactor::new()));
            redactor.lock().unwrap().register_address(&format!("https://ops:{password}@shop.test/"));
            redactor.lock().unwrap().keep("e1");
            let sent = Arc::new(Sent::default());
            let jev = decisions(&(sent.clone() as Arc<dyn DecisionClient>), &redactor);
            let state = json!({"page": {"controls": [{"id": "e1", "label": "Open"}]}});
            let criteria = IndexMap::from([("e1".to_string(), json!("the control labelled Open"))]);
            let choice = ChoiceQuestion::new("Which control?", criteria);
            let questions = IndexMap::from([("pick".to_string(), Question::Choice(choice))]);
            let _ = jev.evaluate(&state, &questions).await;
            let sent = sent.0.lock().unwrap();
            assert!(sent[0].contains(r#"{"page":{"controls":[{"id":"e1","#), "{password}: {sent:?}");
            assert!(sent[0].contains(r#""criteria":{"e1":"#), "{password}: {sent:?}");
        }
    }

    /// A page can paint its address, credentials and all, on a canvas: once the run holds one, a request carrying an
    /// image is refused, whichever path built it.
    #[tokio::test]
    async fn test_no_image_reaches_jev_or_the_llm_once_the_run_holds_a_credential() {
        let redactor = Arc::new(Mutex::new(Redactor::new()));
        let sent = Arc::new(Sent::default());
        let jev = decisions(&(sent.clone() as Arc<dyn DecisionClient>), &redactor);
        let llm = generations(&(sent.clone() as Arc<dyn LLMClient>), &redactor);
        let images: Images = IndexMap::from([("page".to_string(), b"png".to_vec())]);
        let message =
            Message { images: vec![fastbrowse_core::models::Bytes(b"png".to_vec())], ..Message::user("Read it") };
        let questions = IndexMap::new();
        let _ = jev.evaluate_images(&json!({}), &questions, &images).await;
        let _ = llm.generate_value(LLMPurpose::ImageRead, std::slice::from_ref(&message), &Anything, None).await;
        assert_eq!(sent.0.lock().unwrap().len(), 2, "refused before any credential");
        redactor.lock().unwrap().register_address(&format!("https://ops:{CANARY}@shop.test/"));
        let refused = jev.evaluate_images(&json!({}), &questions, &images).await.unwrap_err();
        assert!(refused.to_string().contains("pixels cannot mask"), "{refused}");
        let refused = llm.generate_value(LLMPurpose::ImageRead, &[message], &Anything, None).await.unwrap_err();
        assert!(refused.to_string().contains("pixels cannot mask"), "{refused}");
        assert_eq!(sent.0.lock().unwrap().len(), 2, "an image went out");
    }
}
