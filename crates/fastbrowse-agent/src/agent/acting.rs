//! Turning a decision into an action: the gates code owns and the text a field gets.

use std::collections::HashSet;

use fastbrowse_core::decision::{Answer, ChoiceQuestion, NoulQuestion, Question};
use fastbrowse_core::models::{CostComponent, Decider, Operation, Status, UNTRUSTED};
use fastbrowse_core::navigation::task_urls;
use fastbrowse_core::page::{Action, Control, Observation};
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{
    asks_for_credentials, changes_credentials, control_action, downloads_question, irreversible_question, is_password,
    may_be_irreversible, requested_question, resolve_secret, sets_new_password, starts_download,
};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::Agent;
use super::driving::require;
use super::helpers::{caller_link, describe, dialog_question, navigation_question};
use super::state::{Flow, RunState, Step, Stop, Unsure};
use crate::policy::Decision;
use crate::retrieval::text::{Word, words};
use crate::verification::page_state;

/// The field's value is words of the task, which Jev marks.
const IN_TASK: &str = "task";
/// Nothing given belongs in the field.
const NO_VALUE: &str = "none";
/// Words shown either side of one Jev may pick, so a word the task repeats reads as a different place.
const WORD_CONTEXT: usize = 3;

/// The words Jev may pick from, each with where it stands in the task.
fn word_options(task: &str, shown: &[Word]) -> IndexMap<String, Value> {
    let chars: Vec<char> = task.chars().collect();
    shown
        .iter()
        .map(|word| {
            let around: String = chars[word.before(WORD_CONTEXT)..word.after(WORD_CONTEXT)].iter().collect();
            let picked: String = chars[word.start..word.end].iter().collect();
            let piece = word.piece.map_or(String::new(), |(n, of)| format!(" (piece {n} of {of})"));
            (word.index.to_string(), Value::from(format!("{}{piece} in: {around}", text::repr(&picked))))
        })
        .collect()
}

impl Agent {
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
                let irreversible = navigation_question(&state.task, &url);
                let requested = requested_question(&state.task, &format!("open the address {url}.\n"));
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
                let text = self.text(state, observation, target).await?;
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
                    let requested = requested_question(&state.task, &action);
                    self.gate_question(state, observation, decision, &dialog.message, Some(irreversible), requested)
                        .await?;
                }
                Ok(Action { accept_dialog: Some(accept), ..Action::new(Operation::Dialog) })
            }
            Operation::SwitchTab => Ok(Action { tab_id: decision.tab_id.clone(), ..Action::new(Operation::SwitchTab) }),
            Operation::Hover => {
                Ok(Action { target_id: Some(require(target)?.id.clone()), ..Action::new(Operation::Hover) })
            }
            Operation::Escape | Operation::Scroll | Operation::ScrollUp | Operation::Back => {
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
        // A credential change is irreversible on the page's own structure, so Jev is not asked whether it is.
        let irreversible = (!changes_credentials(&observation.controls, target, decision.operation))
            .then(|| irreversible_question(&state.task, decision.operation, target, decision.destination.as_ref()));
        let requested =
            requested_question(&state.task, &control_action(decision.operation, target, &observation.controls));
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
            Decider::Jev,
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
        let evaluation = self.jev.evaluate(&json!({"task": state.task}), &questions).await?;
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
            // Only the address goes with the questions: the page's own text is what would argue a harmful action is
            // harmless, and the control's label and form are already in them.
            let evaluation = self
                .jev
                .evaluate(&json!({"page": {"url": observation.url, "title": observation.title}}), &questions)
                .await?;
            state.ledger.record([evaluation.cost.clone()])?;
            let noul = |key: &str| match evaluation.answers.get(key) {
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
            Decider::Jev,
        )
        .await?;
        Err(Stop::new(Status::NeedsConfirmation, reason).into())
    }

    /// The text a field gets: an input, a stored secret, or words Jev marks in the task, copied as the task has them.
    pub(super) async fn text(&self, state: &mut RunState, observation: &Observation, target: &Control) -> Step<String> {
        let origin = self.target_origin(target);
        // Only what this field's own frame may receive: a password manager offers its names across every site it
        // holds, and offering one that cannot be typed here ended a sign-in at needs_input, naming a secret the
        // page was never going to get, where needs_login says what the caller can fix.
        let secrets = self.secret_names(&origin);
        if is_password(target) {
            return self.sensitive_text(state, observation, target, secrets, &origin).await;
        }
        // A value can come from a page, such as a code a site sent: the words offered are the task's, then what Jev read.
        let read: Vec<&str> = state.notes.facts().iter().filter(|f| f.answers).map(|f| f.text.as_str()).collect();
        let task = if read.is_empty() { state.task.clone() } else { format!("{}\n{}", state.task, read.join("\n")) };
        let mut group = words(&task, target.max_length);
        let max = self.jev.profile().max_choice_options;
        let mut sources: IndexMap<String, Value> = state
            .inputs
            .iter()
            .map(|(k, v)| (format!("input:{k}"), Value::from(format!("The provided value named {k}: {v}"))))
            .collect();
        for name in &secrets {
            sources.insert(format!("secret:{name}"), format!("The stored secret named {name} (value hidden)").into());
        }
        sources.insert(
            IN_TASK.into(),
            "The task or what was read states this field's value, whole or as part of something longer.".into(),
        );
        sources.insert(NO_VALUE.into(), "Neither the task nor what was read gives a value for this field.".into());
        let mut label = text::repr(&target.label);
        if let Some(limit) = target.max_length {
            label = format!("{label}, which holds at most {limit} characters,");
        }
        // Amazon labels its sign-in field "Enter mobile number or email", and Jev, seeing only a secret named
        // `username`, chose the task. Only for signing in: a checkout's contact email is the task's value.
        let source_question = format!(
            "What should be typed into {label}? Stored secrets are this site's sign-in credentials, named by role, and are only for signing in to the account: when this field is a sign-in field, the username is also the account's email address or phone number. Any other field takes a value the task gives, and a search field takes what the task is looking for."
        );
        // A task with more words than one choice can offer is narrowed first, over parts that overlap so a value
        // across any cut stands whole in one of them.
        while group.len() > max {
            let step = (max / 2).max(1);
            let parts: Vec<&[Word]> = group
                .chunks(step)
                .enumerate()
                .map(|(n, _)| {
                    let from = n * step;
                    &group[from..(from + 2 * step).min(group.len())]
                })
                .take(max)
                .collect();
            let chars: Vec<char> = task.chars().collect();
            let criteria: IndexMap<String, Value> = parts
                .iter()
                .enumerate()
                .map(|(n, part)| {
                    let shown: String = chars[part[0].start..part[part.len() - 1].end].iter().collect();
                    (n.to_string(), Value::from(shown))
                })
                .collect();
            let question = format!("Which part of the task holds the value for {label}, if any does?");
            let picked = self.ask_choice(state, observation, &question, criteria).await?;
            let chosen = picked.parse::<usize>().unwrap_or_default();
            group = parts[chosen].to_vec();
        }
        let options = word_options(&task, &group);
        let described = describe(target);
        let tried: Vec<String> = state
            .history
            .iter()
            .filter(|entry| entry.operation == Some(Operation::Fill) && entry.target.as_deref() == Some(&described))
            .filter_map(|entry| entry.text.as_deref().map(text::repr))
            .collect();
        let tried =
            if tried.is_empty() { String::new() } else { format!(" Already typed here: {}.", tried.join(", ")) };
        let mut asked = vec![("source", source_question, sources)];
        if !group.is_empty() {
            asked.push((
                "first",
                format!(
                    "If the task states the value for {label}, which of its words begins the value? Take the shortest span the field needs: a search takes what is looked for, not the request around it. When the task gives this field several values, the one it gives first comes first, and a later one follows once the page has refused the one before.{tried}"
                ),
                options.clone(),
            ));
            asked.push((
                "last",
                format!(
                    "If the task states the value for {label}, which of its words ends the value? Take the shortest span the field needs: a search takes what is looked for, not the request around it. When the task gives this field several values, the one it gives first comes first, and a later one follows once the page has refused the one before.{tried}"
                ),
                options,
            ));
        }
        let picks = self.ask_choices(state, observation, asked).await?;
        let source = picks.get("source").map(String::as_str).unwrap_or(NO_VALUE);
        if let Some(input) = source.strip_prefix("input:") {
            return Ok(state.inputs.get(input).cloned().unwrap_or_default());
        }
        if let Some(name) = source.strip_prefix("secret:") {
            return self.secret(name, &origin, asks_for_credentials(&self.raw_target(target))).await;
        }
        let word = |key: &str| {
            let index = picks.get(key)?.parse::<usize>().ok()?;
            group.iter().find(|word| word.index == index)
        };
        // A field with a limit takes one piece, whatever the end was picked as.
        let (first, last) = match (target.max_length, word("first"), word("last")) {
            (Some(limit), Some(first), Some(last)) if last.end.saturating_sub(first.start) > limit => {
                (Some(last), Some(last))
            }
            (Some(_), Some(first), Some(last)) if last.index < first.index => (Some(first), Some(first)),
            (_, first, last) => (first, last),
        };
        match (source, first, last) {
            (IN_TASK, Some(first), Some(last)) if first.index <= last.index => {
                // The two ends are picked apart, and each can land on a different value the task gives: the end is
                // asked again knowing where this value begins.
                let last = if first.index == last.index {
                    last
                } else {
                    let rest: Vec<Word> = group.iter().filter(|word| word.index >= first.index).cloned().collect();
                    let begins: String = task.chars().skip(first.start).take(first.end - first.start).collect();
                    let question = format!(
                        "The value for {label} begins at {}. Which word ends this one value? Take the shortest span the field needs.",
                        text::repr(&begins)
                    );
                    let picked = self.ask_choice(state, observation, &question, word_options(&task, &rest)).await?;
                    group.iter().find(|word| word.index.to_string() == picked).unwrap_or(last)
                };
                Ok(task.chars().skip(first.start).take(last.end - first.start).collect())
            }
            _ => Err(self.missing(state, target)),
        }
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
        let Some(replacement) = sets_new_password(&observation.controls, target) else {
            return Err(Stop::new(
                Status::NeedsInput,
                format!(
                    "{} is one of several password fields outside a form, so whether it takes the current password or a new one is unclear",
                    text::repr(&target.label)
                ),
            )
            .into());
        };
        if replacement {
            if !secrets.iter().any(|name| name == "new_password") {
                return Err(Stop::new(
                    Status::NeedsInput,
                    "A replacement password requires --secret new_password=ENV_VAR@https://host; set ENV_VAR to the new value instead of putting it in the task.",
                )
                .into());
            }
            return self.secret("new_password", origin, false).await;
        }
        let secrets: Vec<String> = secrets.into_iter().filter(|name| name != "new_password").collect();
        match secrets.as_slice() {
            [] => Err(Stop::new(
                Status::NeedsLogin,
                format!("{} wants a secret and none is stored", text::repr(&target.label)),
            )
            .into()),
            [only] => self.secret(only, origin, true).await,
            _ => {
                let criteria: IndexMap<String, Value> = secrets
                    .iter()
                    .map(|name| (name.clone(), format!("The stored secret named {name}").into()))
                    .collect();
                let question = format!("Which stored secret belongs in {}?", text::repr(&target.label));
                let name = self.ask_choice(state, observation, &question, criteria).await?;
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

    fn target_origin(&self, target: &Control) -> String {
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

    /// A field the task gives no value for is first a wrong pick, then a missing input.
    ///
    /// Most such fields are optional, and the right move is to leave them. Recovery is told once, so it can pick
    /// another step; a required field it sends the run back to still ends it here.
    fn missing(&self, state: &mut RunState, target: &Control) -> Flow {
        let key = (target.label.clone(), target.context.clone());
        if state.missing.contains(&key) {
            return Stop::new(
                Status::NeedsInput,
                format!("{} needs a value the task does not give", text::repr(&target.label)),
            )
            .into();
        }
        state.missing.insert(key);
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
        let criteria: IndexMap<String, Value> =
            keys.iter().map(|(key, option)| (key.clone(), self.mask(option).into())).collect();
        let picked = self.ask_choice(state, observation, question, criteria).await?;
        Ok(keys[&picked].clone())
    }

    async fn ask_choice(
        &self,
        state: &mut RunState,
        observation: &Observation,
        question: &str,
        criteria: IndexMap<String, Value>,
    ) -> Step<String> {
        let picks = self.ask_choices(state, observation, vec![("pick", question.to_string(), criteria)]).await?;
        Ok(picks["pick"].clone())
    }

    /// Whether the task wants something reported back, rather than only something carried out.
    pub(super) async fn asks_to_be_told(&self, state: &mut RunState, observation: &Observation) -> Step<bool> {
        let criteria = IndexMap::from([
            ("told".to_string(), Value::from("The task asks to be told something, such as a value, a name or a list.")),
            ("carried_out".to_string(), Value::from("The task only asks for something to be carried out.")),
        ]);
        let question = "What does the task want at its end?";
        Ok(self.ask_choice(state, observation, question, criteria).await? == "told")
    }

    /// Several choices about this page in one request, each answer one its question offered.
    async fn ask_choices(
        &self,
        state: &mut RunState,
        observation: &Observation,
        asked: Vec<(&str, String, IndexMap<String, Value>)>,
    ) -> Step<IndexMap<String, String>> {
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let offered: IndexMap<String, HashSet<String>> =
            asked.iter().map(|(key, _, criteria)| (key.to_string(), criteria.keys().cloned().collect())).collect();
        let questions: IndexMap<String, Question> = asked
            .into_iter()
            .map(|(key, question, criteria)| {
                let content = format!("{UNTRUSTED}\n\n# Task\n{}\n\n{question}", state.task);
                (key.to_string(), Question::Choice(ChoiceQuestion::new(content, criteria)))
            })
            .collect();
        let dumped: Vec<String> = questions.values().map(Dump::dump_json).collect();
        let page = page_state(observation, &state.notes, &self.config.tokens, &dumped, None)?;
        let evaluation = self.jev.evaluate(&page, &questions).await?;
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
            ("accept".to_string(), Value::from("Accept / OK")),
            ("dismiss".to_string(), Value::from("Dismiss / Cancel")),
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
