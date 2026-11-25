//! Model clients that follow a script, so a run can be driven with no model behind it.
//!
//! The frozen binary is tested as it is released, and CI has no model keys, so its smoke run answers from a script.
//! The task is the script. Each step is an operation and the label of the control it lands on, steps are separated by
//! `;`, and a step that has to pick a value names it after `=`:
//!
//! ```text
//! click Add one; fill Token = secret:token
//! ```
//!
//! Once the steps are spent the run is called done.

use std::collections::VecDeque;
use std::sync::Mutex;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    Answer, ChoiceAnswer, ChoiceTag, DecisionClient, DecisionError, Evaluation, NoulAnswer, NoulTag, Question,
};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, LLMPurpose, Operation};
use fastbrowse_core::py::text;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{AgentError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub operation: Operation,
    pub label: String,
    #[serde(default)]
    pub pick: Option<String>,
}

/// `str.partition(separator)`, each part stripped as the script reader strips them.
fn partition<'a>(text: &'a str, separator: &str) -> (&'a str, &'a str) {
    text.split_once(separator).unwrap_or((text, ""))
}

/// The script a task spells out.
pub fn steps(task: &str) -> Result<Vec<Step>> {
    let mut found = Vec::new();
    for part in task.split(';').map(text::strip).filter(|part| !part.is_empty()) {
        let (action, pick) = partition(part, "=");
        let (action, pick) = (text::strip(action), text::strip(pick));
        let (operation, label) = partition(action, " ");
        let operation = Operation::parse(operation)
            .ok_or_else(|| AgentError::Invalid(format!("{} is not a valid Operation", text::repr(operation))))?;
        found.push(Step {
            operation,
            label: text::strip(label).to_string(),
            pick: (!pick.is_empty()).then(|| pick.to_string()),
        });
    }
    Ok(found)
}

/// Answers every yes/no with no but whether the answer covers the task, and the step-kind choice by whether the script
/// acts.
///
/// A no keeps a wall, a sign-in or the irreversible check from stopping the scripted run.
pub struct ScriptedJev {
    /// Whether the script takes a step besides opening and reading pages, which is how it answers what the task asks
    /// to be done on a page.
    acts: bool,
}

impl ScriptedJev {
    pub fn new(script: Vec<Step>) -> ScriptedJev {
        let acts = script.iter().any(|step| {
            !matches!(
                step.operation,
                Operation::Read
                    | Operation::Search
                    | Operation::Done
                    | Operation::Navigate
                    | Operation::Scroll
                    | Operation::ScrollUp
                    | Operation::Back
                    | Operation::SwitchTab
                    | Operation::Wait
            )
        });
        ScriptedJev { acts }
    }
}

impl DecisionClient for ScriptedJev {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        fastbrowse_core::decision::DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        _: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, std::result::Result<Evaluation, DecisionError>> {
        let mut answers: IndexMap<String, Answer> = IndexMap::new();
        let mut failure = None;
        for (key, question) in questions {
            match question {
                Question::Choice(question) => {
                    let options: Vec<&String> = question.criteria.keys().collect();
                    let scripted = (key == "steps").then(|| if self.acts { "acts" } else { "reads" }.to_string());
                    let choice = scripted
                        .filter(|choice| options.contains(&choice))
                        .or_else(|| options.first().map(|option| option.to_string()))
                        .unwrap_or_default();
                    let probabilities = options
                        .iter()
                        .map(|option| ((*option).clone(), f64::from(u8::from(**option == choice))))
                        .collect();
                    answers.insert(
                        key.clone(),
                        Answer::Choice(ChoiceAnswer { kind: ChoiceTag, choice, probabilities, confidence: 1.0 }),
                    );
                }
                Question::Noul(_) => {
                    let holds = matches!(key.as_str(), "coverage" | "exact" | "carried");
                    let probability = f64::from(u8::from(holds));
                    answers.insert(key.clone(), Answer::Noul(NoulAnswer { kind: NoulTag, probability }));
                }
                Question::Score(_) | Question::Box(_) => {
                    failure =
                        Some(DecisionError::failed(format!("the scripted Jev has no answer for {}", text::repr(key))));
                    break;
                }
            }
        }
        Box::pin(async move {
            if let Some(failure) = failure {
                return Err(failure);
            }
            Ok(Evaluation {
                model: "scripted".into(),
                answers,
                input_tokens: 0,
                cost: CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0)),
                requests: 1,
            })
        })
    }
}

/// No start page: the answer by what is asked.
pub struct ScriptedLLM;

impl ScriptedLLM {
    fn reply(purpose: LLMPurpose) -> Option<Value> {
        match purpose {
            LLMPurpose::Start => Some(json!({"url": null})),
            LLMPurpose::Compose => Some(json!({"claims": []})),
            LLMPurpose::ImageRead => Some(json!({"text": null})),
            LLMPurpose::Fill => Some(json!({"value": null})),
            LLMPurpose::Pages => Some(json!({"first": null, "last": null})),
            LLMPurpose::Dates => Some(json!({"dates": []})),
            LLMPurpose::KnownPages | LLMPurpose::RecoveryPages => Some(json!({"urls": []})),
            LLMPurpose::Unobtainable => Some(json!({"item": "", "reason": "", "quote": ""})),
            LLMPurpose::Groups => Some(json!({"records": []})),
            LLMPurpose::Search => Some(json!({"queries": []})),
            // With no script the driver has nothing to do, and a run whose steps are spent is called done.
            LLMPurpose::Action => {
                Some(json!({"next": "", "actions": [{"tool": "done", "answer": [], "numbered": false}]}))
            }
        }
    }
}

impl LLMClient for ScriptedLLM {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        _: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, std::result::Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let reply = ScriptedLLM::reply(purpose).ok_or_else(|| {
                LLMError::failed(fastbrowse_core::llm::FaultClass::Client, format!("<LLMPurpose.{}>", purpose))
            })?;
            let data = schema.validate_json(&reply.to_string()).map_err(LLMError::schema)?;
            let cost = CostLine {
                purpose: Some(purpose),
                ..CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0))
            };
            Ok(Generation { data, cost })
        })
    }
}

/// Drives the chat driver from a script: each turn takes the script's next step and names its control by the number
/// the turn's page shows it under, found by its exact label, since a control's number is only known once the page is
/// shown. Once the steps are spent the run is called done. Every other request is answered as [`ScriptedLLM`] answers it.
pub struct ScriptedDriver {
    ahead: Mutex<VecDeque<Step>>,
}

impl ScriptedDriver {
    pub fn new(script: Vec<Step>) -> ScriptedDriver {
        ScriptedDriver { ahead: Mutex::new(script.into()) }
    }

    /// The number of the control labelled `label` on the page `shown` lists, one control a line as
    /// `[number] role "label" ...`, with a `*` before the number of one new since the last look. A label no control
    /// has is sent as it is, as a model naming a control the page lacks would, and the driver is told it is not there.
    fn number(shown: &str, label: &str) -> String {
        shown
            .lines()
            .find_map(|line| {
                let (number, rest) = line.trim_start_matches('*').strip_prefix('[')?.split_once("] ")?;
                let (_, quoted) = rest.split_once(' ')?;
                let found = serde_json::Deserializer::from_str(quoted).into_iter::<String>().next()?.ok()?;
                (found == label).then(|| number.to_string())
            })
            .unwrap_or_else(|| label.to_string())
    }

    /// The turn that takes `step` on the page `shown`.
    fn turn(step: Option<Step>, shown: &str) -> std::result::Result<Value, String> {
        let Some(step) = step else {
            return Ok(json!({"tool": "done", "answer": [], "numbered": false}));
        };
        let id = || ScriptedDriver::number(shown, &step.label);
        let pick = || step.pick.clone().ok_or_else(|| format!("the step {} names no value", text::repr(&step.label)));
        Ok(match step.operation {
            Operation::Click => json!({"tool": "click", "id": id()}),
            Operation::Fill => match pick()?.strip_prefix("secret:") {
                Some(name) => json!({"tool": "type_secret", "id": id(), "name": name}),
                None => json!({"tool": "type", "id": id(), "text": pick()?}),
            },
            Operation::Select => json!({"tool": "select", "id": id(), "option": pick()?}),
            Operation::Enter => json!({"tool": "key", "id": id(), "key": "Enter"}),
            Operation::Escape => json!({"tool": "key", "id": id(), "key": "Escape"}),
            Operation::Press => json!({"tool": "key", "id": id(), "key": pick()?}),
            Operation::Scroll | Operation::ScrollUp => json!({
                "tool": "scroll",
                "id": (!step.label.is_empty()).then(id),
                "down": step.operation == Operation::Scroll,
            }),
            Operation::Navigate => json!({"tool": "goto", "url": step.label}),
            // `drag Card A = Done` drops the control on the one labelled after `=`; a bare drag draws its path.
            Operation::Drag => json!({
                "tool": "drag",
                "id": id(),
                "to": step.pick.as_deref().map(|to| ScriptedDriver::number(shown, to)),
            }),
            Operation::Back => json!({"tool": "back"}),
            // `wait=N` asks for N seconds; a bare `wait` asks for an hour, longer than any scripted run lasts.
            Operation::Wait => {
                json!({"tool": "wait", "seconds": step.pick.as_deref().map_or(Ok(3600.0), str::parse::<f64>)
                .map_err(|error| format!("the wait seconds: {error}"))?})
            }
            Operation::Dialog => json!({"tool": "dialog"}),
            // `done=[claims]` writes the answer as the driver does, quoting what the pages it was shown said.
            Operation::Done => {
                let answer: Value = match &step.pick {
                    Some(claims) => {
                        serde_json::from_str(claims).map_err(|error| format!("the done answer {claims}: {error}"))?
                    }
                    None => json!([]),
                };
                json!({"tool": "done", "answer": answer, "numbered": false})
            }
            other => return Err(format!("the chat driver has no tool for {other}")),
        })
    }
}

impl LLMClient for ScriptedDriver {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, std::result::Result<Generation<Value>, LLMError>> {
        if purpose != LLMPurpose::Action {
            return ScriptedLLM.generate_value(purpose, messages, schema, ledger);
        }
        let step = self.ahead.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).pop_front();
        let shown = messages.last().map(|message| message.content.as_str()).unwrap_or_default();
        let turn = ScriptedDriver::turn(step, shown);
        Box::pin(async move {
            if let Some(ledger) = ledger {
                ledger.reserve(CostComponent::Llm, 0.0, 1)?;
            }
            let action = turn.map_err(|error| LLMError::failed(fastbrowse_core::llm::FaultClass::Client, error))?;
            let reply = json!({"next": "", "actions": [action]});
            let data = schema.validate_json(&reply.to_string()).map_err(LLMError::schema)?;
            let cost = CostLine {
                purpose: Some(purpose),
                ..CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0))
            };
            Ok(Generation { data, cost })
        })
    }
}

#[cfg(test)]
mod tests {
    //! The scripted model clients the binary's smoke run uses, asked what a run would ask them.
    use fastbrowse_core::decision::NoulQuestion;

    use super::*;
    use crate::start::StartPage;
    use crate::support::generate;
    use crate::support::testing::block_on;

    fn ask(jev: &ScriptedJev, questions: &[(&str, Question)]) -> Evaluation {
        let questions: IndexMap<String, Question> = questions.iter().map(|(k, q)| (k.to_string(), q.clone())).collect();
        block_on(jev.evaluate(&json!({}), &questions)).unwrap()
    }

    #[test]
    fn test_a_task_is_read_as_steps_with_the_value_a_step_picks() {
        assert_eq!(
            steps("click Add one; fill Token = secret:token").unwrap(),
            [
                Step { operation: Operation::Click, label: "Add one".into(), pick: None },
                Step { operation: Operation::Fill, label: "Token".into(), pick: Some("secret:token".into()) },
            ]
        );
        assert!(steps("fly away").is_err());
        assert!(steps(" ; ;").unwrap().is_empty());
    }

    #[test]
    fn test_no_page_is_a_wall_or_irreversible() {
        let question: Question = NoulQuestion::new("?", None, None).into();
        let jev = ScriptedJev::new(vec![]);
        let walls = ask(
            &jev,
            &[("login_required", question.clone()), ("bot_check", question.clone()), ("irreversible", question)],
        );
        // The shipped thresholds stop a run above 0.70 for either wall and above 0.50 for an irreversible action.
        let probabilities: Vec<Option<f64>> = walls.answers.values().map(Answer::probability).collect();
        assert_eq!(probabilities, [Some(0.0), Some(0.0), Some(0.0)]);
    }

    #[test]
    fn test_the_start_page_is_answered_by_purpose() {
        let start = block_on(generate::<StartPage>(&ScriptedLLM, LLMPurpose::Start, &[], None)).unwrap();
        assert_eq!(start.data.url, None);
    }
}
