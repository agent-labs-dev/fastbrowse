//! Turning a decision into an action: the gates code owns and the text a field gets.

use std::collections::HashSet;

use fastbrowse_clients::validation::jev_purpose;
use fastbrowse_core::decision::{Answer, ChoiceQuestion, NoulQuestion, Question};
use fastbrowse_core::models::{CostComponent, Decider, JevPurpose, Operation, Status, UNTRUSTED};
use fastbrowse_core::navigation::task_urls;
use fastbrowse_core::page::{Action, Control, Observation};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::text;
use fastbrowse_core::recipe::RecipeText;
use fastbrowse_core::safety::{
    asks_for_credentials, changes_credentials, control_action, downloads_question, irreversible_question, is_password,
    may_be_irreversible, origin_of, requested_question, resolve_secret, sets_new_password, starts_download,
};
use indexmap::IndexMap;
use serde_json::{Map, Value as JsonValue, json};

use super::Agent;
use super::driving::require;
use super::helpers::{caller_link, describe, dialog_question, navigation_question};
use super::state::{Flow, OutputIntent, RunState, Step, Stop, Unsure};
use crate::policy::{Ahead, Decision};
use crate::retrieval::text::{Word, words};
use crate::verification::page_state;
use crate::writing::write_value;

/// The field's value is words of the task, which Jev marks.
const IN_TASK: &str = "task";
/// The field's value is words of what was read, which Jev marks. Offered with the task's words, a word read from the
/// page was typed into every field of a form whose values the task gave.
pub(super) const IN_READ: &str = "read";
/// The task asks for the value without stating it word for word, as a search box's query, so the LLM writes it.
const WRITTEN: &str = "write";
/// Nothing given belongs in the field.
const NO_VALUE: &str = "none";
/// Words shown either side of one Jev may pick, so a word the task repeats reads as a different place.
const WORD_CONTEXT: usize = 3;

/// The words Jev may pick from, each with where it stands in the task.
fn word_options(task: &str, shown: &[Word]) -> IndexMap<String, JsonValue> {
    let chars: Vec<char> = task.chars().collect();
    shown
        .iter()
        .map(|word| {
            let around: String = chars[word.before(WORD_CONTEXT)..word.after(WORD_CONTEXT)].iter().collect();
            let picked: String = chars[word.start..word.end].iter().collect();
            let piece = word.piece.map_or(String::new(), |(n, of)| format!(" (piece {n} of {of})"));
            (word.index.to_string(), JsonValue::from(format!("{}{piece} in: {around}", text::repr(&picked))))
        })
        .collect()
}

/// What a field's value is picked from.
pub(super) struct Value {
    /// The task.
    task: String,
    /// What was read that answers the task.
    read: String,
    label: String,
    pub(super) group: Vec<Word>,
    source_question: String,
    pub(super) sources: IndexMap<String, JsonValue>,
}

/// One question about the page, with the options it offers.
type Asked<'a> = (&'a str, String, IndexMap<String, JsonValue>);

/// What Jev is asked about a field: where its value comes from and, among `group`, where it begins and ends.
fn value_questions<'a>(
    state: &RunState,
    target: &Control,
    (text, holder): (&str, &str),
    label: &str,
    group: &[Word],
    source_question: String,
    sources: IndexMap<String, JsonValue>,
) -> Vec<Asked<'a>> {
    let tried = typed_into(state, target);
    let tried = if tried.is_empty() {
        String::new()
    } else {
        format!(
            " Already typed here: {}. Earlier typing does not prove the page rejected the value. Choose what the task requires; the page may have changed it.",
            tried.join(", ")
        )
    };
    let options = word_options(text, group);
    let mut asked = vec![("source", source_question, sources)];
    if !group.is_empty() {
        let span = |end: &str| {
            format!(
                "If {holder} states the value for {label}, which of its words {end} the value? When it gives this field several values, the first comes first. A search takes what is looked for, not the request around it.{tried}"
            )
        };
        asked.push(("first", span("begins"), options.clone()));
        asked.push(("last", span("ends"), options));
    }
    asked
}

/// What was already typed into `target` this run, quoted.
fn typed_into(state: &RunState, target: &Control) -> Vec<String> {
    let described = describe(target);
    state
        .history
        .iter()
        .filter(|entry| entry.operation == Some(Operation::Fill) && entry.target.as_deref() == Some(&described))
        .filter_map(|entry| entry.text.as_deref().map(text::repr))
        .collect()
}

fn page_question(task: &str, question: &str, criteria: IndexMap<String, JsonValue>) -> Question {
    let content = format!("{UNTRUSTED}\n\n# Task\n{task}\n\n{question}");
    Question::Choice(ChoiceQuestion::new(content, criteria))
}

/// Jev's picks from the decision's own request, when it answered all of `asked` there.
fn ahead_picks(decision: &Decision, asked: &[Asked<'_>]) -> Option<IndexMap<String, String>> {
    asked
        .iter()
        .map(|(key, _, options)| match decision.ahead.get(*key) {
            Some(Answer::Choice(answer)) if options.contains_key(&answer.choice) => {
                Some((key.to_string(), answer.choice.clone()))
            }
            _ => None,
        })
        .collect()
}

/// What is asked before a control is pressed: whether that commits anything, and when the run is authorized, whether
/// the task asked for it. A credential change is irreversible on the page's own structure, so Jev is not asked whether
/// it is.
fn gate_questions(
    operation: Operation,
    target: &Control,
    destination: Option<&Control>,
    controls: &[Control],
) -> (Option<NoulQuestion>, NoulQuestion) {
    let irreversible = (!changes_credentials(controls, target, operation))
        .then(|| irreversible_question(operation, target, destination, controls));
    (irreversible, requested_question(&control_action(operation, target, destination, controls)))
}

impl Agent {
    /// The follow-ups each action on this page would need, to ask beside the decision. A password is never picked
    /// from words, and a task too long for one choice is narrowed first, so those stay follow-ups.
    pub(super) fn ahead(&self, state: &RunState, observation: &Observation) -> Vec<Ahead> {
        let max = self.jev.profile().max_choice_options;
        let mut ahead = Vec::new();
        for control in &observation.controls {
            let mut add = |operation: Operation, name: &str, question: Question| {
                let (control, name) = (control.id.clone(), name.to_string());
                ahead.push(Ahead { operation, control, name, question });
            };
            if control.operations.contains(&Operation::Fill) && !is_password(control) {
                let Value { task, label, group, sources, source_question, .. } = self.value(state, control);
                if group.len() <= max {
                    for (name, question, criteria) in
                        value_questions(state, control, (&task, "the task"), &label, &group, source_question, sources)
                    {
                        add(Operation::Fill, name, page_question(&state.task, &question, criteria));
                    }
                }
            }
        }
        ahead
    }

    pub(super) async fn action(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
    ) -> Step<Action> {
        let target = decision.target.as_ref();
        match decision.operation {
            Operation::Navigate => {
                let Some(url) = decision.url.clone().filter(|url| {
                    task_urls(&state.task, state.caller_start.as_deref(), true).contains(url)
                        || state.reload_target(&observation.url).as_ref() == Some(url)
                }) else {
                    return Err(Stop::new(Status::Stuck, "navigation address was not supplied by the caller").into());
                };
                if self.reveals_text(&url) {
                    return Err(Stop::new(Status::NeedsInput, "navigation address contains a resolved secret").into());
                }
                self.gate_download(state, observation, decision, &caller_link(&url)).await?;
                let irreversible = navigation_question(&url);
                let requested = requested_question(&format!("open the address {url}.\n"));
                self.gate_question(state, observation, decision, &url, Some(irreversible), requested).await?;
                Ok(Action { url: Some(url), ..Action::new(Operation::Navigate) })
            }
            Operation::Click | Operation::Enter => {
                self.gate_irreversible(state, observation, decision).await?;
                Ok(Action { target_id: target.map(|t| t.id.clone()), ..Action::new(decision.operation) })
            }
            Operation::Drag => {
                let Some(destination) = &decision.destination else {
                    return Err(Stop::new(Status::NeedsInput, "the page offers no drop target for the drag").into());
                };
                self.gate_irreversible(state, observation, decision).await?;
                Ok(Action {
                    target_id: Some(require(target)?.id.clone()),
                    destination_id: Some(destination.id.clone()),
                    ..Action::new(Operation::Drag)
                })
            }
            Operation::Fill => {
                let target = require(target)?;
                // A fetched page takes no typing, so its value is worked out, and a secret asked for, in the browser.
                if self.page.over_http() {
                    let reason = format!("{} can only be filled in a browser", text::repr(&target.label));
                    return Err(self.page.needs_browser(reason).into());
                }
                let text = self.text(state, observation, decision, target).await?;
                Ok(Action {
                    target_id: Some(target.id.clone()),
                    secret: self.reveals_text(&text),
                    text: Some(text),
                    secret_origin: Some(self.target_origin(target)),
                    ..Action::new(Operation::Fill)
                })
            }
            Operation::Select => {
                let target = require(target)?;
                let raw = self.raw_target(target);
                if raw.options.is_empty() {
                    // A fetched page does not list a select's options, and a live one can fill it once another field is set.
                    let reason = format!("{} offers no options yet", text::repr(&target.label));
                    if self.page.over_http() {
                        return Err(self.page.needs_browser(reason).into());
                    }
                    return Err(Unsure { reason, gives_up_as: Status::Stuck }.into());
                }
                let question = format!("Which option should {} be set to?", text::repr(&target.label));
                let option = self.choose(state, observation, &question, &raw.options).await?;
                Ok(Action { target_id: Some(raw.id.clone()), text: Some(option), ..Action::new(Operation::Select) })
            }
            Operation::Upload => {
                if state.attachments.is_empty() {
                    return Err(Stop::new(Status::NeedsInput, "the page asks for a file and none was provided").into());
                }
                let names: Vec<String> = state.attachments.iter().map(|a| a.name.clone()).collect();
                let name = self.choose(state, observation, "Which file belongs in this input?", &names).await?;
                let files: Vec<_> = state.attachments.iter().filter(|a| a.name == name).cloned().collect();
                if files.iter().map(|f| f.content.0.len() as i64).sum::<i64>() > self.config.max_upload_bytes {
                    return Err(Stop::new(Status::NeedsInput, format!("{name} exceeds the upload size limit")).into());
                }
                Ok(Action { target_id: Some(require(target)?.id.clone()), files, ..Action::new(Operation::Upload) })
            }
            Operation::Dialog => {
                let accept = self.accept_dialog(state, observation).await?;
                if accept
                    && let Some(dialog) = &observation.dialog
                    && let Some(irreversible) = dialog_question(&state.task, Some(dialog))
                {
                    let action = format!("accept this {} dialog: {}.\n", dialog.kind, text::repr(&dialog.message));
                    let requested = requested_question(&action);
                    self.gate_question(state, observation, decision, &dialog.message, Some(irreversible), requested)
                        .await?;
                }
                Ok(Action { accept_dialog: Some(accept), ..Action::new(Operation::Dialog) })
            }
            Operation::SwitchTab => Ok(Action { tab_id: decision.tab_id.clone(), ..Action::new(Operation::SwitchTab) }),
            Operation::Hover | Operation::Blur => {
                Ok(Action { target_id: Some(require(target)?.id.clone()), ..Action::new(decision.operation) })
            }
            Operation::Escape | Operation::Scroll | Operation::ScrollUp | Operation::Back | Operation::Wait => {
                Ok(Action::new(decision.operation))
            }
            Operation::Read | Operation::Done => Err(Flow::Agent(crate::AgentError::Invalid(format!(
                "{} is handled by the loop, not dispatched",
                decision.operation
            )))),
        }
    }

    async fn gate_irreversible(&self, state: &mut RunState, observation: &Observation, decision: &Decision) -> Step {
        let Some(target) = decision.target.as_ref().filter(|_| may_be_irreversible(decision.operation)) else {
            return Ok(());
        };
        self.gate_download(state, observation, decision, target).await?;
        let mut label = describe(target);
        if let Some(destination) = &decision.destination {
            label += &format!(" onto {}", describe(destination));
        }
        let (irreversible, requested) =
            gate_questions(decision.operation, target, decision.destination.as_ref(), &observation.controls);
        self.gate_question(state, observation, decision, &label, irreversible, requested).await
    }

    /// Refuse a control the page says downloads a file when the task rules downloading out.
    ///
    /// The irreversible question cannot catch it: a download commits nothing anyone else sees. Authorization does not
    /// lift this, since it lets a run commit what the task asks for, never what the task forbids.
    pub(super) async fn gate_download(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        control: &Control,
    ) -> Step {
        if !starts_download(control) || !self.downloads_forbidden(state).await? {
            return Ok(());
        }
        let label = describe(control);
        let reason = format!(
            "{} {} downloads a file, and the task rules out downloading",
            decision.operation,
            text::repr(&label)
        );
        let confidence = Some(decision.confidence());
        let redacted = self.redact(&reason);
        self.record_failure(
            state,
            observation,
            decision.operation,
            &redacted,
            Some(self.redact(&label)),
            confidence,
            Some(false),
            if decision.replayed { Decider::Recipe } else { Decider::Jev },
        )
        .await?;
        Err(Unsure::new(redacted).into())
    }

    /// Whether the task rules out downloading, asked of Jev about the task alone and kept for the run.
    pub(super) async fn downloads_forbidden(&self, state: &mut RunState) -> Step<bool> {
        if let Some(forbidden) = state.downloads_forbidden {
            return Ok(forbidden);
        }
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let questions = IndexMap::from([("downloads".to_string(), Question::Noul(downloads_question(&state.task)))]);
        let evaluation =
            jev_purpose(JevPurpose::Authorization, self.jev.evaluate(&json!({"task": state.task}), &questions)).await?;
        state.ledger.record([evaluation.cost.clone()])?;
        // No answer refuses this once, as the irreversible gate does, but is not kept: one failed call must not forbid
        // every download for the rest of a run that asked for one.
        let Some(Answer::Noul(answer)) = evaluation.answers.get("downloads") else { return Ok(true) };
        let forbidden = answer.probability > self.config.thresholds.irreversible_above;
        state.downloads_forbidden = Some(forbidden);
        Ok(forbidden)
    }

    /// The one rule for an action that may commit: it proceeds when Jev judges it not irreversible, or when the run is
    /// authorized and Jev judges the task asked for it. Otherwise the run stops for confirmation.
    ///
    /// `irreversible` is None for an action the page's structure shows commits, which only the task's request lifts.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn gate_question(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        label: &str,
        irreversible: Option<NoulQuestion>,
        requested: NoulQuestion,
    ) -> Step {
        let thresholds = &self.config.thresholds;
        let authorized = state.authorization.irreversible_actions;
        let mut questions = IndexMap::new();
        if let Some(irreversible) = irreversible {
            questions.insert("irreversible".to_string(), Question::Noul(irreversible));
        }
        if authorized {
            questions.insert("requested".to_string(), Question::Noul(requested));
        }
        if !questions.is_empty() {
            state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
            // Never asked beside the decision: the page's own text is what would argue a harmful action is harmless,
            // and the control's label and form are already in the questions. The title, path and query are the page's
            // to write too, so only the origin goes with them.
            let page = json!({"task": state.task, "page": {"origin": origin_of(&observation.url)}});
            let evaluation = jev_purpose(JevPurpose::Authorization, self.jev.evaluate(&page, &questions)).await?;
            state.ledger.record([evaluation.cost.clone()])?;
            let answers = evaluation.answers;
            let noul = |key: &str| match answers.get(key) {
                Some(Answer::Noul(answer)) => Some(answer.probability),
                _ => None,
            };
            let (probability, requested) = (noul("irreversible"), noul("requested"));
            self.event("irreversible", || {
                Map::from_iter([
                    ("target".into(), label.into()),
                    ("context".into(), json!(decision.target.as_ref().and_then(|t| t.context.clone()))),
                    ("probability".into(), json!(probability)),
                    ("requested".into(), json!(requested)),
                ])
            });
            let reversible = probability.is_some_and(|p| p <= thresholds.irreversible_above);
            if reversible || requested.is_some_and(|p| p > thresholds.requested_above) {
                state.committing = !reversible;
                return Ok(());
            }
        }
        let reason = format!("{} {} needs confirmation", decision.operation, text::repr(label));
        let confidence = Some(decision.confidence());
        self.record_failure(
            state,
            observation,
            decision.operation,
            &self.redact(&reason),
            Some(self.redact(label)),
            confidence,
            Some(false),
            if decision.replayed { Decider::Recipe } else { Decider::Jev },
        )
        .await?;
        Err(Stop::new(Status::NeedsConfirmation, reason).into())
    }

    /// The text a field gets: an input, a stored secret, or words Jev marks in the task, copied as the task has them.
    pub(super) async fn text(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        target: &Control,
    ) -> Step<String> {
        let origin = self.target_origin(target);
        state.text_source = Some(RecipeText::Live);
        let missing = state.missing.contains(&(observation.url.clone(), target.label.clone(), target.context.clone()));
        let recorded = decision
            .replayed
            .then(|| state.replay.as_ref().and_then(|replay| replay.text_for(target, missing)).cloned())
            .flatten();
        if let Some(source) = recorded {
            match source {
                RecipeText::Literal(text) => {
                    state.text_source = Some(RecipeText::Literal(text.clone()));
                    return Ok(text);
                }
                RecipeText::Input(name) => {
                    let value = state
                        .inputs
                        .get(&name)
                        .cloned()
                        .ok_or_else(|| Stop::new(Status::NeedsInput, format!("the recipe needs input {name}")))?;
                    state.text_source = Some(RecipeText::Input(name));
                    return Ok(value);
                }
                RecipeText::Secret(name) => {
                    let value = self
                        .secret(&name, &origin, target.sensitive || asks_for_credentials(&self.raw_target(target)))
                        .await?;
                    state.text_source = Some(RecipeText::Secret(name));
                    return Ok(value);
                }
                RecipeText::Live => {}
            }
        }
        if is_password(target) {
            let secrets = self.secret_names(&origin);
            return self.sensitive_text(state, observation, target, secrets, &origin).await;
        }
        let Value { mut task, read, label, group, sources, source_question } = self.value(state, target);
        let mut group = self.narrowed(state, observation, &task, &label, group).await?;
        let asked = value_questions(state, target, (&task, "the task"), &label, &group, source_question, sources);
        let mut picks = match ahead_picks(decision, &asked) {
            Some(picks) => picks,
            None => self.ask_choices(state, Some(observation), asked).await?,
        };
        let source = picks.get("source").cloned().unwrap_or_else(|| NO_VALUE.into());
        if source == WRITTEN {
            let written = write_value(
                &*self.llm,
                &state.task,
                &label,
                &typed_into(state, target),
                &observation.today(),
                Some(&state.ledger),
            )
            .await?;
            state.ledger.record([written.cost])?;
            return match written.data.value.filter(|value| !value.trim().is_empty()) {
                Some(value) => Ok(value),
                None => Err(self.missing(state, observation, target)),
            };
        }
        if source == IN_READ {
            state.text_source = Some(RecipeText::Live);
            task = read;
            group = words(&task, target.max_length);
            group = self.narrowed(state, observation, &task, &label, group).await?;
            let mut asked = value_questions(
                state,
                target,
                (&task, "what was read"),
                &label,
                &group,
                String::new(),
                IndexMap::new(),
            );
            asked.retain(|(key, ..)| *key != "source");
            if asked.is_empty() {
                return Err(self.missing(state, observation, target));
            }
            picks.extend(self.ask_choices(state, Some(observation), asked).await?);
        }
        if let Some(input) = source.strip_prefix("input:") {
            state.text_source = Some(RecipeText::Input(input.to_string()));
            return Ok(state.inputs.get(input).cloned().unwrap_or_default());
        }
        if let Some(name) = source.strip_prefix("secret:") {
            state.text_source = Some(RecipeText::Secret(name.to_string()));
            return self.secret(name, &origin, asks_for_credentials(&self.raw_target(target))).await;
        }
        let word = |key: &str| {
            let index = picks.get(key)?.parse::<usize>().ok()?;
            group.iter().find(|word| word.index == index)
        };
        // A field with a limit takes the piece the value begins at, whatever the end was picked as.
        let (first, last) = match (target.max_length, word("first"), word("last")) {
            (Some(limit), Some(first), Some(last)) if last.end.saturating_sub(first.start) > limit => {
                (Some(first), Some(first))
            }
            (Some(_), Some(first), Some(last)) if last.index < first.index => (Some(first), Some(first)),
            (_, first, last) => (first, last),
        };
        match (source.as_str(), first, last) {
            (IN_TASK | IN_READ, Some(first), Some(last)) => {
                // The two ends are picked apart, and each can land on a different value the task gives, or cross: the
                // end is asked again knowing where this value begins.
                let last = if first.index == last.index {
                    last
                } else {
                    let rest: Vec<Word> = group.iter().filter(|word| word.index >= first.index).cloned().collect();
                    let chars: Vec<char> = task.chars().collect();
                    let options = rest
                        .iter()
                        .map(|word| {
                            let value: String = chars[first.start..word.end].iter().collect();
                            (word.index.to_string(), JsonValue::String(value))
                        })
                        .collect();
                    let question = format!(
                        "Which exact text belongs in {label}? Each choice is the complete value that would be copied from {holder}, beginning at the selected word. Choose only the field's value, without surrounding task instructions.",
                        holder = if source == IN_READ { "what was read" } else { "the task" },
                    );
                    let picked = self.ask_choice(state, observation, &question, options).await?;
                    group
                        .iter()
                        .find(|word| word.index >= first.index && word.index.to_string() == picked)
                        .unwrap_or(first)
                };
                let value: String = task.chars().skip(first.start).take(last.end - first.start).collect();
                if source == IN_TASK {
                    state.text_source = Some(RecipeText::Literal(value.clone()));
                }
                Ok(value)
            }
            _ => Err(self.missing(state, observation, target)),
        }
    }

    /// `group` cut down to what one choice can offer.
    async fn narrowed(
        &self,
        state: &mut RunState,
        observation: &Observation,
        task: &str,
        label: &str,
        mut group: Vec<Word>,
    ) -> Step<Vec<Word>> {
        let max = self.jev.profile().max_choice_options;
        if max < 3 {
            return Err(fastbrowse_core::decision::DecisionError::Failed(
                "value selection needs a provider supporting at least three choices".into(),
            )
            .into());
        }
        // Parts cover the whole source and overlap so a value across a cut remains whole. Reserving one choice
        // for no value lets Jev refuse a source instead of being forced to pick unrelated words.
        while group.len() > max {
            let step = group.len().div_ceil(max - 1);
            let width = (2 * step).min(group.len() - 1);
            let parts: Vec<&[Word]> = group
                .chunks(step)
                .enumerate()
                .map(|(n, _)| {
                    let from = n * step;
                    &group[from..(from + width).min(group.len())]
                })
                .collect();
            let chars: Vec<char> = task.chars().collect();
            let mut criteria: IndexMap<String, JsonValue> = parts
                .iter()
                .enumerate()
                .map(|(n, part)| {
                    let shown: String = chars[part[0].start..part[part.len() - 1].end].iter().collect();
                    (n.to_string(), JsonValue::from(shown))
                })
                .collect();
            criteria.insert(NO_VALUE.into(), JsonValue::from("None of these parts states the field's value."));
            let question = format!("Which part of this source holds the value for {label}, if any does?");
            let picked = self.ask_choice(state, observation, &question, criteria).await?;
            if picked == NO_VALUE {
                return Ok(Vec::new());
            }
            let chosen = picked
                .parse::<usize>()
                .map_err(|_| fastbrowse_core::decision::DecisionError::Failed("expected a source part index".into()))?;
            group = parts[chosen].to_vec();
        }
        Ok(group)
    }

    /// What a field's value is picked from, and the question that says where it comes from.
    pub(super) fn value(&self, state: &RunState, target: &Control) -> Value {
        // Only what this field's own frame may receive: a password manager offers its names across every site it
        // holds, and offering one that cannot be typed here ended a sign-in at needs_input, naming a secret the
        // page was never going to get, where needs_login says what the caller can fix.
        let secrets = self.secret_names(&self.target_origin(target));
        // A value can come from a page, such as a code a site sent.
        let read: Vec<&str> = state.notes.facts().iter().filter(|f| f.answers).map(|f| f.text.as_str()).collect();
        let read = read.join("\n");
        let task = state.task.clone();
        let group = words(&task, target.max_length);
        let mut sources: IndexMap<String, JsonValue> = state
            .inputs
            .iter()
            .map(|(k, v)| (format!("input:{k}"), JsonValue::from(format!("The provided value named {k}: {v}"))))
            .collect();
        for name in &secrets {
            sources.insert(format!("secret:{name}"), format!("The stored secret named {name} (value hidden)").into());
        }
        sources.insert(
            IN_TASK.into(),
            "The task states this field's value word for word, whole or as part of something longer.".into(),
        );
        sources.insert(
            WRITTEN.into(),
            "The task asks for a value it does not state word for word, such as a search query for what it looks for, so it is written from the task.".into(),
        );
        if !read.is_empty() {
            sources
                .insert(IN_READ.into(), format!("The task does not state it, and what was read does: {read}").into());
        }
        sources.insert(NO_VALUE.into(), "Nothing given states a value for this field.".into());
        let mut label = text::repr(&target.label);
        if let Some(limit) = target.max_length {
            label = format!("{label}, which holds at most {limit} characters,");
        }
        // Amazon labels its sign-in field "Enter mobile number or email", and Jev, seeing only a secret named
        // `username`, chose the task. Only for signing in: a checkout's contact email is the task's value.
        let source_question = format!(
            "What should be typed into {label}? Stored secrets are this site's sign-in credentials, named by role, and are only for signing in to the account: when this field is a sign-in field, the username is also the account's email address or phone number. Any other field takes a value the task gives, and a search field takes what the task is looking for."
        );
        Value { task, read, label, group, source_question, sources }
    }

    /// Resolve replacement passwords separately from existing credentials, without model-written values.
    async fn sensitive_text(
        &self,
        state: &mut RunState,
        observation: &Observation,
        target: &Control,
        secrets: Vec<String>,
        origin: &str,
    ) -> Step<String> {
        let replacement = match sets_new_password(&observation.controls, target) {
            Some(replacement) => replacement,
            None => {
                let criteria = IndexMap::from([
                    (
                        "current".into(),
                        JsonValue::from(
                            "The field takes the account's existing password for signing in or confirming identity.",
                        ),
                    ),
                    (
                        "replacement".into(),
                        JsonValue::from("The field sets a new account password or confirms that new password."),
                    ),
                    (
                        "unclear".into(),
                        JsonValue::from("The task and form do not establish which password the field takes."),
                    ),
                ]);
                let question = format!(
                    "Which password role does {} have for this task? Consider the task, field context and form controls. A sign-in page may mark its password field new-password to prevent autofill, so that marker alone does not establish a replacement. Choose unclear when the role cannot be established.",
                    text::repr(&target.label),
                );
                match self.ask_choice(state, observation, &question, criteria).await?.as_str() {
                    "replacement" => true,
                    "current" => false,
                    _ => {
                        return Err(Stop::new(
                            Status::NeedsInput,
                            format!(
                                "{} does not establish whether it takes the current password or a replacement",
                                text::repr(&target.label),
                            ),
                        )
                        .into());
                    }
                }
            }
        };
        if replacement {
            if !secrets.iter().any(|name| name == "new_password") {
                return Err(Stop::new(
                    Status::NeedsInput,
                    "A replacement password requires --secret new_password=ENV_VAR@https://host; set ENV_VAR to the new value instead of putting it in the task.",
                )
                .into());
            }
            state.text_source = Some(RecipeText::Secret("new_password".into()));
            return self.secret("new_password", origin, false).await;
        }
        let secrets: Vec<String> = secrets.into_iter().filter(|name| name != "new_password").collect();
        match secrets.as_slice() {
            [] => Err(Stop::new(
                Status::NeedsLogin,
                format!("{} wants a secret and none is stored", text::repr(&target.label)),
            )
            .into()),
            [only] => {
                state.text_source = Some(RecipeText::Secret(only.clone()));
                self.secret(only, origin, true).await
            }
            _ => {
                let criteria: IndexMap<String, JsonValue> = secrets
                    .iter()
                    .map(|name| (name.clone(), format!("The stored secret named {name}").into()))
                    .collect();
                let question = format!("Which stored secret belongs in {}?", text::repr(&target.label));
                let name = self.ask_choice(state, observation, &question, criteria).await?;
                state.text_source = Some(RecipeText::Secret(name.clone()));
                self.secret(&name, origin, true).await
            }
        }
    }

    pub(super) fn raw_target(&self, target: &Control) -> Control {
        match self.raw() {
            Some(raw) => raw.controls.into_iter().find(|c| c.id == target.id).unwrap_or_else(|| target.clone()),
            None => target.clone(),
        }
    }

    pub(super) fn target_origin(&self, target: &Control) -> String {
        self.raw_target(target).frame_origin.unwrap_or_else(|| "null".into())
    }

    /// The value of `name` for `origin`. A credential field nothing resolves for is a sign-in the user has to make,
    /// not a value the caller forgot: a caller told needs_input asks for a value it has already given.
    async fn secret(&self, name: &str, origin: &str, credential: bool) -> Step<String> {
        let value = match &self.secrets {
            Some(secrets) => resolve_secret(&**secrets, name, origin).await,
            None => None,
        };
        let Some(value) = value else {
            if credential {
                let error = format!("sign-in required at {origin}: secret {name} is not available there");
                return Err(Stop::new(Status::NeedsLogin, error).into());
            }
            return Err(Stop::new(Status::NeedsInput, format!("secret {name} is not available for {origin}")).into());
        };
        self.redactor().register(name, &value, Some(origin));
        Ok(value)
    }

    /// A field the task gives no value for is a wrong pick, and a missing input if the run cannot go on without it.
    ///
    /// Most such fields are optional, and the right move is to leave them on every page. The field is then not offered
    /// at this address, and a run that stalls here without it ends needing that input.
    fn missing(&self, state: &mut RunState, observation: &Observation, target: &Control) -> Flow {
        state.missing.insert((observation.url.clone(), target.label.clone(), target.context.clone()));
        Unsure {
            reason: format!(
                "the task gives no value for {}: leave it unless the task cannot go on without it",
                text::repr(&target.label)
            ),
            gives_up_as: Status::NeedsInput,
        }
        .into()
    }

    async fn choose(
        &self,
        state: &mut RunState,
        observation: &Observation,
        question: &str,
        options: &[String],
    ) -> Step<String> {
        match options {
            [only] => return Ok(only.clone()),
            [] => return Err(Stop::new(Status::Stuck, format!("no options to answer: {question}")).into()),
            _ => {}
        }
        // One Choice offers at most the options the decision model documents.
        let shown = self.jev.profile().max_choice_options;
        let keys: IndexMap<String, String> =
            options.iter().take(shown).enumerate().map(|(i, option)| (i.to_string(), option.clone())).collect();
        let criteria: IndexMap<String, JsonValue> =
            keys.iter().map(|(key, option)| (key.clone(), self.mask(option).into())).collect();
        let picked = self.ask_choice(state, observation, question, criteria).await?;
        Ok(keys[&picked].clone())
    }

    async fn ask_choice(
        &self,
        state: &mut RunState,
        observation: &Observation,
        question: &str,
        criteria: IndexMap<String, JsonValue>,
    ) -> Step<String> {
        let picks = self.ask_choices(state, Some(observation), vec![("pick", question.to_string(), criteria)]).await?;
        Ok(picks["pick"].clone())
    }

    pub(super) async fn output_intent(&self, state: &mut RunState) -> Step<OutputIntent> {
        if state.output_intent != OutputIntent::Unknown {
            return Ok(state.output_intent);
        }
        let criteria = IndexMap::from([
            (
                "text".to_string(),
                JsonValue::from(
                    "The task asks to be told text, such as a value, a name or a list, and does not ask for an image.",
                ),
            ),
            ("image".to_string(), JsonValue::from("The task asks only for an image or screenshot as its report.")),
            (
                "text_image".to_string(),
                JsonValue::from("The task asks for both a textual answer and an image or screenshot."),
            ),
            (
                "carried_out".to_string(),
                JsonValue::from("The task only asks for something to be carried out, with no report."),
            ),
        ]);
        // Asked in the same request, since both are about the task alone: a theme check on a page styled by CSS shows
        // its answer only in pixels, and with no picture on the page its screenshot was never read.
        let evidence = IndexMap::from([
            (
                "page_text".to_string(),
                JsonValue::from("What the task asks about is something a page states in its text or controls."),
            ),
            (
                "appearance".to_string(),
                JsonValue::from(
                    "The task asks how the page looks, such as its colours, styling, artwork or layout, which only the rendered page shows.",
                ),
            ),
        ]);
        let picks = self
            .ask_choices(
                state,
                None,
                vec![
                    ("pick", "What output does the task want at its end?".into(), criteria),
                    ("evidence", "Where can what the task asks about be seen?".into(), evidence),
                ],
            )
            .await?;
        state.appearance = picks["evidence"] == "appearance";
        let intent = match picks["pick"].as_str() {
            "text" => OutputIntent::Text,
            "image" => OutputIntent::Image,
            "text_image" => OutputIntent::TextAndImage,
            "carried_out" => OutputIntent::CarriedOut,
            _ => unreachable!(),
        };
        state.output_intent = intent;
        Ok(intent)
    }

    pub(super) async fn stops_to_confirm(&self, state: &mut RunState, observation: &Observation) -> Step<bool> {
        let criteria = IndexMap::from([
            ("confirm".to_string(), JsonValue::from("The task says to stop before sending and ask for confirmation.")),
            ("no".to_string(), JsonValue::from("The task does not say that.")),
        ]);
        let question = "What does the task say about sending what was typed?";
        Ok(self.ask_choice(state, observation, question, criteria).await? == "confirm")
    }

    /// Several choices in one request, each answer one its question offered.
    async fn ask_choices(
        &self,
        state: &mut RunState,
        observation: Option<&Observation>,
        asked: Vec<Asked<'_>>,
    ) -> Step<IndexMap<String, String>> {
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let offered: IndexMap<String, HashSet<String>> =
            asked.iter().map(|(key, _, criteria)| (key.to_string(), criteria.keys().cloned().collect())).collect();
        let questions: IndexMap<String, Question> = asked
            .into_iter()
            .map(|(key, question, criteria)| (key.to_string(), page_question(&state.task, &question, criteria)))
            .collect();
        let dumped: Vec<String> = questions.values().map(Dump::dump_json).collect();
        let page = match observation {
            Some(observation) => page_state(observation, &state.notes, &self.config.tokens, &dumped, None)?,
            // Output format is a requirement of the task, independent of what the page or notes say.
            None => JsonValue::Object(Map::new()),
        };
        let evaluation = jev_purpose(JevPurpose::Action, self.jev.evaluate(&page, &questions)).await?;
        state.ledger.record([evaluation.cost.clone()])?;
        let mut picks = IndexMap::new();
        for (key, options) in &offered {
            let Some(Answer::Choice(answer)) = evaluation.answers.get(key) else {
                return Err(fastbrowse_core::decision::DecisionError::Failed("expected a choice answer".into()).into());
            };
            if !options.contains(&answer.choice) {
                let message = format!("Jev picked {}, which was not offered", answer.choice);
                return Err(fastbrowse_core::decision::DecisionError::Failed(message).into());
            }
            picks.insert(key.clone(), answer.choice.clone());
        }
        Ok(picks)
    }

    async fn accept_dialog(&self, state: &mut RunState, observation: &Observation) -> Step<bool> {
        let Some(dialog) = &observation.dialog else {
            return Err(Stop::new(Status::Stuck, "DIALOG chosen with no dialog open").into());
        };
        let question = format!(
            "The page shows a {} dialog saying {}, opened by the agent's last action. Accept it if it asks to go ahead with what the task wants done; dismiss it if it would do something the task did not ask for.",
            dialog.kind,
            text::repr(&dialog.message)
        );
        let criteria = IndexMap::from([
            ("accept".to_string(), JsonValue::from("Accept / OK")),
            ("dismiss".to_string(), JsonValue::from("Dismiss / Cancel")),
        ]);
        Ok(self.ask_choice(state, observation, &question, criteria).await? == "accept")
    }

    /// The stop at a sign-in wall Jev judged blocks the task, naming the credentials the page asks for.
    pub(super) fn meet_wall(&self, observation: &Observation, raw: &Observation) -> Flow {
        let origin = fastbrowse_core::safety::origin_of(&observation.url);
        // Which fields ask for credentials is read from the raw page, whose flags masking cannot have changed.
        let credential: HashSet<&String> =
            raw.controls.iter().filter(|c| asks_for_credentials(c)).map(|c| &c.id).collect();
        let asked: Vec<String> = observation
            .controls
            .iter()
            .filter(|c| credential.contains(&c.id))
            .map(|c| {
                if !c.label.is_empty() {
                    c.label.clone()
                } else {
                    c.input_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "a password".into())
                }
            })
            .collect();
        let evidence = if !asked.is_empty() {
            format!("the page asks for {}", asked.join(", "))
        } else if credential.is_empty() {
            let page = if observation.title.is_empty() { &observation.url } else { &observation.title };
            format!("{page} offers only sign-in")
        } else {
            "the page asks for a password".to_string()
        };
        // The error reaches the caller as it is, so it is built from what models see and redacted as well: a page can
        // echo a typed secret into its title or a field's label.
        Stop::new(Status::NeedsLogin, self.redact(&format!("sign-in required at {origin}: {evidence}"))).into()
    }
}
