//! Turning a decision into an action: the gates code owns, the text a field gets, and the reads an interaction owes.

use std::collections::HashSet;

use fastbrowse_core::decision::{Answer, ChoiceQuestion, NoulQuestion, Question};
use fastbrowse_core::effects::state_key;
use fastbrowse_core::llm::Message;
use fastbrowse_core::models::{CostComponent, Decider, LLMPurpose, Operation, Status, UNTRUSTED};
use fastbrowse_core::navigation::task_urls;
use fastbrowse_core::page::{Action, Control, Observation};
use fastbrowse_core::py::dump::{Dump, include, to_value};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{
    asks_for_credentials, changes_credentials, downloads_question, irreversible_question, may_be_irreversible,
    requested_question, resolve_secret, reversible_toggle, sets_new_password, starts_download,
};
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::driving::{StepOptions, Watched, require};
use super::helpers::{
    caller_link, code_decision, describe, dialog_question, lookup, misfit, navigation_question, next_value,
    only_typed_since_read, read_exhausted, record, seen, unread, unread_transaction,
};
use super::reading::covered;
use super::sign_in::shows_sign_in;
use super::state::{FieldText, Flow, RunState, Step, Stop, Unsure};
use super::{Agent, GENERATE};
use crate::memory::shows;
use crate::planner::Plan;
use crate::policy::Decision;
use crate::support::generate;
use crate::verification::page_state;

const FIELD_MODULE: &str = "fastbrowse.agent";
const MISSING: &str = "True when the field wants a fact about the user (a phone number, address, account or order id) that the task and notes neither state nor state a part of. Such a value is never invented.";
const TEXT: &str = "Exactly the text to type into the field, with no commentary; empty when missing.";
const VALUES: &str = "Only when the task has this field hold more than one value in turn (enter one, later change it): each value, written as it would be typed, in the order the task wants them typed. Otherwise empty.";

fn field_text_fields() -> Vec<Field> {
    vec![
        Field::required("missing", Ty::Bool).doc(MISSING),
        Field::required("text", Ty::Str).doc(TEXT),
        Field::factory("values", Ty::list(Ty::Str)).doc(VALUES),
    ]
}

impl Schema for FieldText {
    fn ty() -> Ty {
        ModelTy { module: FIELD_MODULE, ..ModelTy::frozen("_FieldText", field_text_fields()) }.ty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FormField {
    #[serde(deserialize_with = "fastbrowse_core::py::lax::boolean")]
    missing: bool,
    text: String,
    #[serde(default)]
    values: Vec<String>,
    id: String,
}

impl FormField {
    fn as_text(&self) -> FieldText {
        FieldText { missing: self.missing, text: self.text.clone(), values: self.values.clone() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FormText {
    fields: Vec<FormField>,
}

impl Schema for FormText {
    fn ty() -> Ty {
        let mut fields = field_text_fields();
        fields.push(Field::required("id", Ty::Str));
        let field = ModelTy { module: FIELD_MODULE, ..ModelTy::frozen("_FormField", fields) }.ty();
        ModelTy {
            module: FIELD_MODULE,
            ..ModelTy::frozen(
                "_FormText",
                vec![Field::required("fields", Ty::list(field)).doc(
                    "Fields safe to fill now, in order, starting with the chosen field. Omit any uncertain field.",
                )],
            )
        }
        .ty()
    }
}

fn field_rules() -> String {
    format!(
        "Infer its meaning from the task, current value, page context and recent actions. A validation message may replace the displayed label. Use input_type, input_name and autocomplete to distinguish the field from nearby fields; an email address and phone number are not interchangeable. Use the field's displayed format for dates, except in a field whose input_type is date, datetime-local, month, week or time, which takes ISO 8601 (2026-09-25, 2026-09-25T14:30, 2026-09, 2026-W39, 14:30). A day named relative to today (tomorrow, the next Friday) is the first matching entry of page.next_days, never a date you count yourself. A fact the task states in another shape is given, not missing: take the part of a stated name, address or date this field asks for and write it in the field's shape. The requirements, or the task itself when no requirements are given, list its steps in the order it wants them done. When more than one gives this field a value (enter one, later change it), list every such value in values, in that order, and write in text the earliest one no recent action has already typed into this field.\n\n# Trust\n{UNTRUSTED}"
    )
}

fn field_writer() -> String {
    format!("# Field writer\nWrite only the text for one form field. {}", field_rules())
}

const OUTPUT_SEARCH: &str = "For a search field, form a query from a named target in the task that still has unresolved requirements or unverified outputs. A previous search does not establish that its outputs were found. A read-only task permits searching for its requested information. Do not clear a search merely because the task does not dictate a literal query. ";

fn form_writer() -> String {
    format!(
        "# Form writer\nWrite text for ALL the listed empty fields whose values the task or notes supply. These fields belong to one form. Filling several contact or address fields together is one task step: include every known value, starting with chosen_field. Each value will be typed separately. Omit fields that need a result from an intervening task step, or that would require inventing a value. Omit optional fields the task does not supply. If only the chosen field can be filled yet, return it alone. Copy each field's id exactly. For a value to correct later, still list all values in task order; only the first pending value will be typed now. {}",
        field_rules()
    )
}

/// The value due now, of a field written several in turn.
fn due(value: &FieldText, entries: &[crate::policy::HistoryEntry], label: &str) -> String {
    if value.values.len() > 1 { next_value(&value.values, entries, Some(label)) } else { value.text.clone() }
}

impl Agent {
    pub(super) async fn action(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        gate: bool,
    ) -> Step<Action> {
        let target = decision.target.as_ref();
        match decision.operation {
            Operation::Navigate => {
                let observed = decision.directed
                    && decision.url.as_ref().is_some_and(|url| state.observed_links.contains_key(url));
                let Some(url) = decision.url.clone().filter(|url| {
                    task_urls(&state.task, state.caller_start.as_deref(), true).contains(url) || observed
                }) else {
                    return Err(Stop::new(Status::Stuck, "navigation address was not supplied by the caller").into());
                };
                if self.reveals_text(&url) {
                    return Err(Stop::new(Status::NeedsInput, "navigation address contains a resolved secret").into());
                }
                if gate && !observed {
                    state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
                    let question = NoulQuestion::new(
                        "Does the caller ask the browser to open this address for the task?",
                        Some("The address is a browsing destination or a source to inspect."),
                        Some("The address is field data or quoted text, not a page to visit."),
                    );
                    let questions = IndexMap::from([("destination".to_string(), Question::Noul(question))]);
                    let evaluation =
                        self.jev.evaluate(&json!({"task": state.task, "address": url}), &questions).await?;
                    state.ledger.record([evaluation.cost.clone()])?;
                    let sure = matches!(evaluation.answers.get("destination"), Some(Answer::Noul(answer))
                        if answer.probability >= self.config.thresholds.recover_below);
                    if !sure {
                        return Err(
                            Unsure::new("the supplied address is not confidently a browsing destination").into()
                        );
                    }
                }
                if gate {
                    self.gate_download(state, observation, decision, &caller_link(&url)).await?;
                    let question = navigation_question(&state.task, &url);
                    self.gate_question(state, observation, decision, &url, question, false).await?;
                }
                Ok(Action { url: Some(url), ..Action::new(Operation::Navigate) })
            }
            Operation::Click | Operation::Enter => {
                // Only a pager link to another address goes ungated: it opens a page and commits nothing, so asking Jev
                // would buy a call per page and nothing else.
                if gate {
                    self.gate_irreversible(state, observation, decision).await?;
                }
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
                    && let Some(question) = dialog_question(&state.task, Some(dialog))
                {
                    self.gate_question(state, observation, decision, &dialog.message, question, false).await?;
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
            Operation::Read | Operation::Done | Operation::Escalate => Err(Flow::Agent(crate::AgentError::Invalid(
                format!("{} is handled by the loop, not dispatched", decision.operation),
            ))),
        }
    }

    async fn gate_irreversible(&self, state: &mut RunState, observation: &Observation, decision: &Decision) -> Step {
        let Some(target) = decision.target.as_ref().filter(|t| may_be_irreversible(decision.operation, Some(t))) else {
            return Ok(());
        };
        self.gate_download(state, observation, decision, target).await?;
        if decision.operation == Operation::Click && reversible_toggle(target) {
            // A media pause, play or mute is undone by the same control and commits nothing off the page, so it is
            // not a commit to confirm.
            self.event("reversible_toggle", || Map::from_iter([("target".into(), describe(target).into())]));
            return Ok(());
        }
        let mut label = describe(target);
        if let Some(destination) = &decision.destination {
            label += &format!(" onto {}", describe(destination));
        }
        let question = irreversible_question(&state.task, decision.operation, target, decision.destination.as_ref());
        let force = matches!(decision.operation, Operation::Click | Operation::Enter)
            && changes_credentials(&observation.controls, target, decision.operation);
        self.gate_question(state, observation, decision, &label, question, force).await
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
        let decided_by = if decision.directed { Decider::Llm } else { Decider::Jev };
        let confidence = (!decision.directed).then(|| decision.confidence());
        let redacted = self.redact(&reason);
        self.record_failure(
            state,
            observation,
            decision.operation,
            &redacted,
            Some(self.redact(&label)),
            confidence,
            Some(false),
            decided_by,
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

    pub(super) async fn gate_question(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        label: &str,
        question: NoulQuestion,
        force: bool,
    ) -> Step {
        let thresholds = &self.config.thresholds;
        let authorized = state.authorization.irreversible_actions;
        let commit = (
            decision.operation,
            decision.target.as_ref().map(|t| t.id.clone()),
            label.to_string(),
            observation.url.clone(),
        );
        // Spent by whatever is gated next, and only good for the action straight after the recovery that gave it.
        let confirmed = state.confirmed_commit.take();
        let fresh = state.recovered_at == state.history.len();
        // Authorized and confident proceeds whatever Jev would say about the action, so it is not asked. Only Jev's own
        // confidence counts: a directed action carries the score of the action Jev chose instead.
        if authorized && !decision.directed && decision.confidence() >= thresholds.sensitive_act_from {
            return Ok(());
        }
        // Recovery saw the page and the task after this gate refused, and named this same commit: that is the second
        // opinion the refusal asked for. A credential change is never waved through this way.
        if authorized && !force && fresh && confirmed.as_ref() == Some(&commit) {
            return Ok(());
        }
        let what = format!("{} {}", decision.operation, text::repr(label));
        if !force {
            state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
            // Only the address goes with the question: the page's own text is what would argue a harmful action is
            // harmless, and the control's label and form are already in the question.
            let mut questions = IndexMap::from([("irreversible".to_string(), Question::Noul(question))]);
            // Asked in the same call, so an authorized run whose task names the action pays no recovery for it.
            if let Some(target) = decision.target.as_ref().filter(|_| authorized && !decision.directed) {
                let requested = requested_question(&state.task, decision.operation, target);
                questions.insert("requested".to_string(), Question::Noul(requested));
            }
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
            if probability.is_some_and(|p| p <= thresholds.irreversible_above) {
                return Ok(());
            }
            if requested.is_some_and(|p| p >= thresholds.sensitive_act_from)
                && decision.confidence() >= thresholds.recover_below
            {
                return Ok(());
            }
        }
        // An unsure pick that may commit something is more likely the wrong pick than the step to confirm.
        let unsure = authorized || decision.confidence() < thresholds.recover_below;
        let reason = if unsure {
            format!("unsure {what} is the irreversible action the task means ({:.2})", decision.confidence())
        } else {
            format!("{what} needs confirmation")
        };
        let decided_by = if decision.directed { Decider::Llm } else { Decider::Jev };
        let confidence = (!decision.directed).then(|| decision.confidence());
        self.record_failure(
            state,
            observation,
            decision.operation,
            &self.redact(&reason),
            Some(self.redact(label)),
            confidence,
            Some(false),
            decided_by,
        )
        .await?;
        if unsure {
            if authorized {
                state.unsure_commit = Some(commit);
            }
            return Err(Unsure::new(reason).into());
        }
        Err(Stop::new(Status::NeedsConfirmation, reason).into())
    }

    pub(super) async fn text(&self, state: &mut RunState, observation: &Observation, target: &Control) -> Step<String> {
        let origin = self.target_origin(target);
        // Only what this field's own frame may receive: a password manager offers its names across every site it
        // holds, and offering one that cannot be typed here ended a sign-in at needs_input, naming a secret the
        // page was never going to get, where needs_login says what the caller can fix.
        let secrets = self.secret_names(&origin);
        if target.sensitive {
            return self.sensitive_text(state, observation, target, secrets, &origin).await;
        }
        let mut criteria: IndexMap<String, Value> = state
            .inputs
            .iter()
            .map(|(k, v)| (format!("input:{k}"), Value::from(format!("The provided value named {k}: {v}"))))
            .collect();
        for name in &secrets {
            criteria.insert(format!("secret:{name}"), format!("The stored secret named {name} (value hidden)").into());
        }
        criteria.insert(GENERATE.into(), "None of these; write new text stated in the task or notes.".into());
        // Amazon labels its sign-in field "Enter mobile number or email", and Jev, seeing only a secret named
        // `username`, chose to write new text. Only for signing in: a checkout's contact email is the task's value.
        let question = format!(
            "What should be typed into {}? Stored secrets are this site's sign-in credentials, named by role, and are only for signing in to the account: when this field is a sign-in field, the username is also the account's email address or phone number. Any other field takes a value the task gives.",
            text::repr(&target.label)
        );
        let choice = if criteria.len() > 1 {
            self.ask_choice(state, observation, &question, criteria).await?
        } else {
            GENERATE.to_string()
        };
        if let Some(input) = choice.strip_prefix("input:") {
            return Ok(state.inputs.get(input).cloned().unwrap_or_default());
        }
        if let Some(name) = choice.strip_prefix("secret:") {
            return self.secret(name, &origin, asks_for_credentials(&self.raw_target(target))).await;
        }
        self.generate_text(state, observation, target).await
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
        if sets_new_password(target) {
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
        self.seen().redactor.register(name, &value, Some(origin));
        Ok(value)
    }

    async fn generate_text(&self, state: &mut RunState, observation: &Observation, target: &Control) -> Step<String> {
        let known = state.form_values.remove(&(observation.document_key.clone(), target.id.clone()));
        if let Some((control, value)) = known
            && (Control { offscreen: target.offscreen, ..control }) == *target
        {
            let text = due(&value, &state.history, &target.label);
            // The value due now moves on as fills are typed, so a kept one is checked when it is used, not only when
            // the batch kept it.
            if misfit(target, &text).is_none() {
                return Ok(text);
            }
        }
        let context = self.field_context(state, observation, target).await?;
        // Mid-run a task's next item is searched for too, not only an output the answer check found missing: given the
        // task without this rule, the writer called a search box's query missing and ended a six-part cart run.
        let system = format!("{}\n\n{OUTPUT_SEARCH}", field_writer());
        let messages = vec![Message::system(system), Message::user(dumps(&Value::Object(context)))];
        if let Some(written) = self.write_field(state, &messages, target, true).await? {
            return Ok(written);
        }
        // A surname can be present inside a full name. Confirm absence before ending the run on one writer's doubt.
        if self.value_absent(state, observation, target).await? {
            return Err(self.missing(state, target));
        }
        let mut insisted = messages;
        insisted.push(Message::user(
            "You reported this value as missing, but the task or notes appear to state it, or to state something it is part of. Look again and write it. Report it missing only if it truly is not there: never invent one.",
        ));
        match self.write_field(state, &insisted, target, true).await? {
            Some(written) => Ok(written),
            None => Err(self.missing(state, target)),
        }
    }

    pub(super) async fn field_context(
        &self,
        state: &mut RunState,
        observation: &Observation,
        target: &Control,
    ) -> Step<Map<String, Value>> {
        // Adapted from browser-use/jev-ultrafast (MIT), model.py:field_context. A popup's field can have a generic
        // label; the opening action and surrounding values explain its purpose.
        // The plan is taken once it has arrived, never waited for: a sign-in's first fill comes before it, and waiting
        // left the browser idle for the whole plan call, 33s on a planner that timed out once.
        let plan = if state.planning.done() { Some(state.await_plan().await?) } else { state.ready_plan.clone() };
        let mut context = Map::new();
        context.insert("task".into(), state.task.clone().into());
        // Given only the task, the writer typed "enter X, later correct it to Y" as Y on the first pass every time; in
        // order, it typed X first and Y after going back.
        if let Some(plan) = &plan {
            context.insert("requirements".into(), json!(plan.requirements.iter().map(|r| &r.text).collect::<Vec<_>>()));
            let unresolved: Vec<String> = state.notes.unresolved(plan).into_iter().map(|r| r.text).collect();
            context.insert("unresolved_requirements".into(), json!(unresolved));
        }
        if !state.open_answer_outputs.is_empty() {
            context.insert("unverified_outputs".into(), json!(state.open_answer_outputs));
        }
        context.insert("subgoal".into(), json!(state.hint));
        context.insert("field".into(), target.dump_exclude_none());
        let others: Vec<Value> = observation
            .controls
            .iter()
            .filter(|c| c.id != target.id && (c.operations.contains(&Operation::Fill) || c.role == "combobox"))
            .map(|c| {
                include(
                    to_value(c, true).unwrap_or(Value::Null),
                    &["label", "context", "role", "value", "input_type", "input_name", "autocomplete"],
                )
            })
            .collect();
        context.insert("other_fields".into(), Value::Array(others));
        // Told only that today was Sunday, the writer still booked "the next Monday" a week late in one run of four: a
        // relative day is looked up here rather than counted.
        let next_days: Vec<String> = (1..8)
            .map(|ahead| {
                let day = observation.captured_at.add_days(ahead);
                format!("{} {}", day.weekday_name(), day.date_string())
            })
            .collect();
        context.insert(
            "page".into(),
            json!({
                "url": observation.url,
                "title": observation.title,
                "text": observation.viewport_text,
                "date": observation.today(),
                "next_days": next_days,
            }),
        );
        let recent: Vec<Value> =
            record(&state.history, &self.config.observation).iter().map(Dump::dump_exclude_none).collect();
        context.insert("recent_actions".into(), Value::Array(recent));
        context.insert(
            "notes".into(),
            state.notes.render_for_navigation(self.config.observation.working_notes_chars, true)?.into(),
        );
        Ok(context)
    }

    pub(super) async fn fill_form(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
    ) -> Step<bool> {
        let Some(target) = decision.target.as_ref() else { return Ok(false) };
        if decision.operation != Operation::Fill || !state.inputs.is_empty() || observation.dialog.is_some() {
            return Ok(false);
        }
        let Some(form_id) = &target.form_id else { return Ok(false) };
        if observation.omitted_controls > 0 || observation.inaccessible_frames > 0 {
            return Ok(false);
        }
        let form_key = (observation.document_key.clone(), target.frame_id.clone(), form_id.clone());
        if state.form_rejected.contains(&form_key) {
            return Ok(false);
        }
        let same_form: Vec<&Control> = observation
            .controls
            .iter()
            .filter(|c| c.frame_id == target.frame_id && c.form_id.as_deref() == Some(form_id.as_str()))
            .collect();
        if same_form.iter().any(|c| c.sensitive) {
            return Ok(false);
        }
        let fields: IndexMap<String, Control> = same_form
            .iter()
            .filter(|c| {
                c.operations.contains(&Operation::Fill)
                    && c.value.as_deref().is_none_or(str::is_empty)
                    && c.role == "textbox"
            })
            .map(|c| (c.id.clone(), (*c).clone()))
            .collect();
        if !fields.contains_key(&target.id) || fields.len() < 2 {
            return Ok(false);
        }
        // A form that cannot be batched gets the ordinary field path on its next decision.
        state.form_rejected.insert(form_key.clone());
        let redecided = state.redecided;
        let ledger = state.ledger.clone();
        let mut context = self.field_context(state, observation, target).await?;
        let field = context.shift_remove("field").unwrap_or(Value::Null);
        context.insert("chosen_field".into(), field);
        context.insert("fields".into(), Value::Array(fields.values().map(Dump::dump_exclude_none).collect()));
        let messages = vec![Message::system(form_writer()), Message::user(dumps(&Value::Object(context)))];
        let writing = async {
            let generation =
                generate::<FormText>(&*self.llm, LLMPurpose::FieldText, &messages, 0, Some(&ledger)).await?;
            ledger.record([generation.cost.clone()])?;
            Ok(generation.data)
        };
        let written = match self.unless_redrawn(redecided, writing, observation, Some(&target.id)).await {
            Err(flow) if flow.is_llm() => return Ok(false),
            Err(flow) => return Err(flow),
            Ok(Watched::Redrawn) => {
                self.redecide(state, observation);
                return Ok(true);
            }
            Ok(Watched::Done(written)) => written,
        };
        let ids: Vec<&String> = written.fields.iter().map(|f| &f.id).collect();
        self.event("form_values", || {
            Map::from_iter([("chosen".into(), target.id.clone().into()), ("fields".into(), json!(ids))])
        });
        let unique: HashSet<&&String> = ids.iter().collect();
        if ids.first() != Some(&&target.id)
            || unique.len() != ids.len()
            || ids.iter().any(|id| !fields.contains_key(*id))
        {
            return Ok(false);
        }
        self.event("form_batch", || Map::from_iter([("fields".into(), json!(ids))]));
        // Scrolling to the first field ends a batch too. A later Jev choice can reuse its value only while the field's
        // identity, meaning and empty value survive the fresh observation.
        for value in &written.fields {
            let control = &fields[&value.id];
            let text = due(&value.as_text(), &state.history, &control.label);
            if !value.missing
                && !text.is_empty()
                && misfit(control, &text).is_none()
                && !self.reveals_text(&value.dump_json())
            {
                state
                    .form_values
                    .insert((observation.document_key.clone(), value.id.clone()), (control.clone(), value.as_text()));
            }
        }
        let mut observation = observation.clone();
        let mut completed = true;
        for (index, value) in written.fields.iter().enumerate() {
            let control = fields[&value.id].clone();
            let text = due(&value.as_text(), &state.history, &control.label);
            // A value that does not fit its field's declared kind ends the batch, so the field's own path can check it
            // and ask again before typing.
            if value.missing || text.is_empty() || self.reveals_text(&text) || misfit(&control, &text).is_some() {
                return Ok(index > 0);
            }
            state.ledger.check(0.0)?;
            state.form_values.remove(&(observation.document_key.clone(), control.id.clone()));
            let action = Action {
                target_id: Some(control.id.clone()),
                text: Some(text.clone()),
                form_fill: true,
                ..Action::new(Operation::Fill)
            };
            let (step_decision, decided_by) = if index == 0 {
                (decision.clone(), Decider::Jev)
            } else {
                (code_decision(Operation::Fill, Some(control.clone())), Decider::Llm)
            };
            let options = StepOptions { prepared_action: Some(action), ..StepOptions::default() };
            Box::pin(self.step(state, &observation, &step_decision, decided_by, options)).await?;
            if !state.form_continues {
                completed = false;
                break;
            }
            observation = Observation {
                controls: observation
                    .controls
                    .iter()
                    .map(|c| {
                        if c.id == control.id {
                            Control { value: Some(text.clone()), blocking: None, ..c.clone() }
                        } else {
                            c.clone()
                        }
                    })
                    .collect(),
                ..observation
            };
            self.note_effect(state, &observation);
            self.settle(state, &observation);
        }
        if completed {
            state.form_rejected.remove(&form_key);
        }
        Ok(true)
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

    /// The text for one field, or None when the writer says the value was never given.
    async fn write_field(
        &self,
        state: &mut RunState,
        messages: &[Message],
        target: &Control,
        refit: bool,
    ) -> Step<Option<String>> {
        let generation =
            generate::<FieldText>(&*self.llm, LLMPurpose::FieldText, messages, 0, Some(&state.ledger)).await?;
        state.ledger.record([generation.cost.clone()])?;
        let written = generation.data;
        if written.missing {
            return Ok(None);
        }
        let text = due(&written, &state.history, &target.label);
        // Shown a field declaring input_type email and a code saved in the notes, the writer typed the code into Email
        // in 7 of 12 two-step sign-ins. The field's own declaration is checked here instead, and the writer is asked
        // once more before anything is typed.
        let Some(wanted) = misfit(target, &text).filter(|_| refit) else { return Ok(Some(text)) };
        let mut corrected = messages.to_vec();
        corrected.push(Message::user(format!(
            "The field declares that it takes {wanted}, and {} is not one. Write the {wanted} the task or notes give for this field; a value saved for another field does not belong in it.",
            text::repr(&self.mask(&text))
        )));
        Box::pin(self.write_field(state, &corrected, target, false)).await
    }

    async fn value_absent(&self, state: &mut RunState, observation: &Observation, target: &Control) -> Step<bool> {
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let question = NoulQuestion::new(
            format!(
                "{UNTRUSTED}\n\n# Task\n{}\n\nThe agent must fill the field labelled {}. Do the task or the notes give what belongs in it?",
                state.task,
                text::repr(&target.label)
            ),
            Some("The task or the notes state that value, or state something it is a part of."),
            Some("Neither the task nor the notes say it; filling the field would mean inventing one."),
        );
        let question = Question::Noul(question);
        let page = page_state(observation, &state.notes, &self.config.tokens, &[question.dump_json()], None)?;
        let questions = IndexMap::from([("stated".to_string(), question)]);
        let evaluation = self.jev.evaluate(&page, &questions).await?;
        state.ledger.record([evaluation.cost.clone()])?;
        let stated = match evaluation.answers.get("stated") {
            Some(Answer::Noul(answer)) => answer.probability,
            _ => 0.0,
        };
        Ok(stated <= self.config.thresholds.value_stated_above)
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
        let shown = self.config.observation.max_choice_options.max(0) as usize;
        let keys: IndexMap<String, String> =
            options.iter().take(shown).enumerate().map(|(i, option)| (i.to_string(), option.clone())).collect();
        let criteria: IndexMap<String, Value> =
            keys.iter().map(|(key, option)| (key.clone(), self.mask(option).into())).collect();
        let picked = self.ask_choice(state, observation, question, criteria).await?;
        keys.get(&picked).cloned().ok_or_else(|| {
            Flow::from(fastbrowse_core::decision::DecisionError::Failed(format!(
                "Jev picked {picked}, which was not offered"
            )))
        })
    }

    async fn ask_choice(
        &self,
        state: &mut RunState,
        observation: &Observation,
        question: &str,
        criteria: IndexMap<String, Value>,
    ) -> Step<String> {
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let choice = Question::Choice(ChoiceQuestion::new(
            format!("{UNTRUSTED}\n\n# Task\n{}\n\n{question}", state.task),
            criteria,
        ));
        let page = page_state(observation, &state.notes, &self.config.tokens, &[choice.dump_json()], None)?;
        let questions = IndexMap::from([("pick".to_string(), choice)]);
        let evaluation = self.jev.evaluate(&page, &questions).await?;
        state.ledger.record([evaluation.cost.clone()])?;
        match evaluation.answers.get("pick") {
            Some(Answer::Choice(answer)) => Ok(answer.choice.clone()),
            _ => Err(fastbrowse_core::decision::DecisionError::Failed("expected a choice answer".into()).into()),
        }
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

    /// Stop at a sign-in wall Jev saw only on the page's evidence for it; return once the page proved readable.
    ///
    /// Returns whether the run should look at this page again: true after a read changed the notes, false when the
    /// page shows no sign-in at all. Absent controls do not establish missing authentication, so a page with neither a
    /// credential field nor a sign-in to act on is not a wall, and the run acts on its decision.
    pub(super) async fn meet_wall(
        &self,
        state: &mut RunState,
        observation: &Observation,
        raw: &Observation,
        plan: &Plan,
    ) -> Step<bool> {
        let origin = fastbrowse_core::safety::origin_of(&observation.url);
        // The error reaches the caller as it is, so it is built from what models see and redacted as well: a page can
        // echo a typed secret into its title or a field's label.
        let wall = |evidence: String| -> Flow {
            Stop::new(Status::NeedsLogin, self.redact(&format!("sign-in required at {origin}: {evidence}"))).into()
        };
        let page = if observation.title.is_empty() { observation.url.clone() } else { observation.title.clone() };
        // Which fields ask for credentials is read from the raw page, whose flags masking cannot have changed.
        let credential: HashSet<&String> =
            raw.controls.iter().filter(|c| asks_for_credentials(c)).map(|c| &c.id).collect();
        if !credential.is_empty() {
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
            let asked = if asked.is_empty() { "a password".to_string() } else { asked.join(", ") };
            return Err(wall(format!("the page asks for {asked}")));
        }
        if !shows_sign_in(observation) {
            return Ok(false);
        }
        if !unread(plan, &state.notes) {
            return Err(wall(format!("{page} offers only sign-in")));
        }
        // A prompt to sign in can gate only writing, such as posting a reply, so a task that reads is asked of the page
        // before any credential is: what the page shows decides whether the wall stands between them.
        let facts = state.notes.facts().len();
        let read = code_decision(Operation::Read, None);
        Box::pin(self.step(state, observation, &read, Decider::Jev, StepOptions::default())).await?;
        if state.notes.facts().len() == facts {
            return Err(wall(format!("reading {page} found none of the requested information")));
        }
        Ok(true)
    }

    pub(super) async fn read_before_interaction(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        decided_by: Decider,
    ) -> Step<bool> {
        // A native dialog pauses JavaScript, so capture cannot run until the dialog has been handled.
        if observation.dialog.is_some() {
            return Ok(false);
        }
        if state.oversized_answer
            && !state.open_answer_outputs.is_empty()
            && state.owes_read
            && state.ready_plan.as_ref().is_some_and(lookup)
            && !state.reads.iter().any(|key| key.0 == observation.document_key && key.3 == state.open_answer_outputs)
        {
            // Recovery selected this source to repair missing fields. Uncertainty about the next click cannot spend
            // another recovery before its evidence is read.
            let read = code_decision(Operation::Read, None);
            return Ok(!Box::pin(self.step(state, observation, &read, Decider::Llm, StepOptions::default())).await?);
        }
        if decision.operation == Operation::Back
            && !state.record_read_documents.contains(&observation.document_key)
            && !state.reads.iter().any(|key| key.0 == observation.document_key)
        {
            let plan = state.await_plan().await?;
            if !state.notes.unresolved(&plan).is_empty() {
                // Backing out of an unread document can lose a value needed by a pending action, even after every
                // information requirement is answered.
                let reading = Decision { operation: Operation::Read, target: None, ..decision.clone() };
                return Ok(
                    !Box::pin(self.step(state, observation, &reading, decided_by, StepOptions::default())).await?
                );
            }
        }
        if state.ready_plan.is_none() && state.transaction_candidates.iter().any(|c| !c.outcome_read) {
            state.await_plan().await?;
        }
        // A read takes in the whole page, so a scroll over one never read only spends steps.
        let unread_scroll =
            decision.operation.is_scrolling() && !state.reads.iter().any(|key| key.0 == observation.document_key);
        let transaction_pending = unread_transaction(state);
        if matches!(decision.operation, Operation::Read | Operation::Done)
            || (decision.read_assessment != crate::policy::ReadAssessment::Evidence
                && !unread_scroll
                && !transaction_pending)
        {
            return Ok(false);
        }
        if !transaction_pending && state.read_here && only_typed_since_read(&state.history) {
            // The page was read, and since then only a field's own text changed, which is the run's own writing.
            return Ok(false);
        }
        let here = seen(observation);
        if !transaction_pending
            && ((state.passed.contains(&here) && state.acted_on.as_ref() != Some(&here)) || state.typed_on_passed)
        {
            // Back on a page exactly as the run left it unread, perhaps with its own typing added: Jev judged it had
            // nothing to read then, and nothing it holds has changed.
            return Ok(false);
        }
        let plan = state.await_plan().await?;
        if !transaction_pending && !unread(&plan, &state.notes) {
            return Ok(false);
        }
        // An interaction can remove evidence, so read first and reconsider before authorizing the next action. But an
        // interaction that only redrew what the notes hold, such as a tab opened on text already read or a row
        // expanded over it, leaves nothing to preserve, and reading it again spends a model call on nothing.
        let capture = self.capture().await?;
        if !transaction_pending && covered(state, observation, &capture) {
            self.event("read_not_owed", || Map::from_iter([("url".into(), self.redact(&observation.url).into())]));
            state.owes_read = false;
            state.read_here = true;
            return Ok(false);
        }
        let reading = Decision { operation: Operation::Read, target: None, ..decision.clone() };
        let options = StepOptions { capture: Some(capture), ..StepOptions::default() };
        let skipped = Box::pin(self.step(state, observation, &reading, decided_by, options)).await?;
        Ok(transaction_pending || !skipped)
    }

    /// Read a page state again when what an earlier read of it evidenced is no longer on it; whether it read.
    pub(super) async fn reread_if_changed(&self, state: &mut RunState, observation: &Observation) -> Step<bool> {
        if !state.history.last().is_some_and(|entry| entry.page_changed) {
            return Ok(false);
        }
        let key = state_key(observation);
        let read: IndexMap<String, String> = state.evidenced_at.get(&key).cloned().unwrap_or_default();
        let ids: IndexMap<String, String> = read.into_iter().filter(|(r, _)| state.notes.evidenced(r)).collect();
        let last = state.read_at.get(&key).cloned();
        if ids.is_empty() && last.is_none() {
            return Ok(false);
        }
        let capture = self.capture().await?;
        if Some(&capture.sha256) == last.as_ref() {
            return Ok(false);
        }
        // A read of this state is checked even once a read of another state of the same address superseded it.
        let mut stale: Vec<String> = ids
            .iter()
            .filter(|(r, sha)| {
                state.notes.read_for(r).iter().any(|e| &e.capture_sha256 == *sha && !shows(&capture.text, &e.quote))
            })
            .map(|(r, _)| r.clone())
            .collect();
        stale.sort();
        // A read that evidenced nothing yet still left facts the answer can draw on once another page completes it.
        let drew = state.notes.facts().iter().any(|fact| {
            fact.evidence
                .as_ref()
                .is_some_and(|e| Some(&e.capture_sha256) == last.as_ref() && !shows(&capture.text, &e.quote))
        });
        if stale.is_empty() && !drew {
            return Ok(false);
        }
        self.event("reread", || Map::from_iter([("requirements".into(), json!(stale))]));
        state.notes.unevidence(stale.iter().map(String::as_str));
        if let Some(read) = state.evidenced_at.get_mut(&key) {
            for r in &stale {
                read.shift_remove(r);
            }
        }
        // What the page paid out before was read off content it no longer shows.
        state.barren.retain(|k, _| !(k.0 == observation.document_key && k.1 == key));
        let reading = code_decision(Operation::Read, None);
        let options = StepOptions { capture: Some(capture), ..StepOptions::default() };
        if Box::pin(self.step(state, observation, &reading, Decider::Llm, options)).await? {
            let reason = read_exhausted(state);
            Box::pin(self.recover(state, observation, &reason, Status::Stuck)).await?;
        }
        Ok(true)
    }
}
