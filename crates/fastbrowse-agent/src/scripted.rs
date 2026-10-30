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
    Answer, ChoiceAnswer, ChoiceQuestion, ChoiceTag, DecisionClient, DecisionError, Evaluation, NoulAnswer, NoulTag,
    Question,
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

/// Answers each choice as the script's current step says, and every yes/no with no.
///
/// A no keeps a wall, a sign-in or the irreversible check from stopping the scripted run, and the run ends when the
/// script runs out and the operation choice falls back to DONE.
pub struct ScriptedJev {
    state: Mutex<Script>,
}

struct Script {
    ahead: VecDeque<Step>,
    step: Option<Step>,
}

impl ScriptedJev {
    pub fn new(script: Vec<Step>) -> ScriptedJev {
        ScriptedJev { state: Mutex::new(Script { ahead: script.into(), step: None }) }
    }

    fn choice(step: Option<&Step>, key: &str, question: &ChoiceQuestion) -> Option<String> {
        if key == "operation" {
            return Some(step.map_or(Operation::Done, |step| step.operation).as_str().to_string());
        }
        if key == "read_assessment" {
            return Some("absent".into());
        }
        let step = step?;
        if key == "pick" {
            return step.pick.clone();
        }
        if key == format!("{}_target", step.operation) {
            // A control's id is only known once the page is indexed, so the script names it by its label.
            return question
                .criteria
                .iter()
                .find(|(_, described)| described.get("label").and_then(Value::as_str) == Some(step.label.as_str()))
                .map(|(option, _)| option.clone());
        }
        None
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
        let mut script = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if questions.contains_key("operation") {
            // The page is asked about once per step, so this question is what moves the script on.
            script.step = script.ahead.pop_front();
        }
        let mut answers: IndexMap<String, Answer> = IndexMap::new();
        let mut failure = None;
        for (key, question) in questions {
            match question {
                Question::Choice(question) => {
                    let options: Vec<&String> = question.criteria.keys().collect();
                    let choice = ScriptedJev::choice(script.step.as_ref(), key, question)
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
                    answers.insert(key.clone(), Answer::Noul(NoulAnswer { kind: NoulTag, probability: 0.0 }));
                }
                Question::Score(_) | Question::Box(_) => {
                    failure =
                        Some(DecisionError::Failed(format!("the scripted Jev has no answer for {}", text::repr(key))));
                    break;
                }
            }
        }
        drop(script);
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

/// A plan that requires nothing and no shortcut: the answer by what is asked.
///
/// Planning and the shortcut are asked for at the same moment, so the order of the calls says nothing.
pub struct ScriptedLLM;

impl ScriptedLLM {
    fn reply(purpose: LLMPurpose) -> Option<Value> {
        match purpose {
            LLMPurpose::Plan => Some(json!({"requirements": [], "answer_expected": false})),
            LLMPurpose::Shortcut => Some(json!({"url": null})),
            _ => None,
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
            let reply =
                ScriptedLLM::reply(purpose).ok_or_else(|| LLMError::Failed(format!("<LLMPurpose.{}>", purpose)))?;
            let data = schema.validate_json(&reply.to_string()).map_err(LLMError::Failed)?;
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
    use crate::planner::Plan;
    use crate::shortcut::Shortcut;
    use crate::support::generate;
    use crate::support::testing::block_on;

    fn operation() -> Question {
        let criteria = [("read", "Read"), ("click", "Click"), ("fill", "Fill"), ("done", "Done")];
        ChoiceQuestion::new("next", criteria.iter().map(|(k, v)| (k.to_string(), json!(v))).collect()).into()
    }

    fn targets() -> Question {
        let criteria = [
            ("c1".to_string(), json!({"label": "Remove", "role": "button"})),
            ("c2".to_string(), json!({"label": "Add one", "role": "button"})),
        ];
        ChoiceQuestion::new("which", criteria.into_iter().collect()).into()
    }

    fn ask(jev: &ScriptedJev, questions: &[(&str, Question)]) -> Evaluation {
        let questions: IndexMap<String, Question> = questions.iter().map(|(k, q)| (k.to_string(), q.clone())).collect();
        block_on(jev.evaluate(&json!({}), &questions)).unwrap()
    }

    fn chosen(evaluation: &Evaluation, key: &str) -> String {
        match &evaluation.answers[key] {
            Answer::Choice(answer) => answer.choice.clone(),
            other => panic!("not a choice: {other:?}"),
        }
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
    fn test_each_look_at_the_page_takes_the_next_step_and_finds_its_control_by_label() {
        let jev = ScriptedJev::new(steps("click Add one").unwrap());
        let first = ask(&jev, &[("operation", operation()), ("click_target", targets())]);
        let second = ask(&jev, &[("operation", operation()), ("click_target", targets())]);
        assert_eq!((chosen(&first, "operation"), chosen(&first, "click_target")), ("click".into(), "c2".into()));
        assert_eq!(chosen(&second, "operation"), "done");
    }

    #[test]
    fn test_the_value_a_step_names_is_picked_after_the_page_was_looked_at() {
        let jev = ScriptedJev::new(steps("fill Token = secret:token").unwrap());
        ask(&jev, &[("operation", operation())]);
        let criteria = [("none", "Nothing"), ("secret:token", "The token")];
        let pick = ChoiceQuestion::new("value", criteria.iter().map(|(k, v)| (k.to_string(), json!(v))).collect());
        let picked = ask(&jev, &[("pick", pick.into())]);
        assert_eq!(chosen(&picked, "pick"), "secret:token");
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
    fn test_the_plan_and_the_shortcut_are_told_apart_by_purpose_not_by_order() {
        let shortcut = block_on(generate::<Shortcut>(&ScriptedLLM, LLMPurpose::Shortcut, &[], None)).unwrap();
        let plan = block_on(generate::<Plan>(&ScriptedLLM, LLMPurpose::Plan, &[], None)).unwrap();
        assert_eq!(shortcut.data.url, None);
        assert!(plan.data.requirements.is_empty() && !plan.data.answer_expected);
    }
}
