//! Turning a decision into an action: the gates code owns and the text a field gets.

use std::collections::HashSet;

use fastbrowse_core::decision::{Answer, ChoiceQuestion, EVEN_ODDS, NoulQuestion, Question};
use fastbrowse_core::llm::Message;
use fastbrowse_core::models::{CostComponent, Decider, LLMPurpose, Operation, Status, UNTRUSTED};
use fastbrowse_core::navigation::task_urls;
use fastbrowse_core::page::{Action, Control, Observation};
use fastbrowse_core::py::dump::{Dump, include, to_value};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::text;
use fastbrowse_core::safety::{
    asks_for_credentials, changes_credentials, control_action, downloads_question, irreversible_question, is_password,
    may_be_irreversible, requested_question, resolve_secret, sets_new_password, starts_download,
};
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::driving::require;
use super::helpers::{caller_link, describe, dialog_question, navigation_question, next_value};
use super::state::{FieldText, Flow, RunState, Step, Stop, Unsure};
use super::{Agent, GENERATE};
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
        "Infer its meaning from the task, current value, page context and recent actions. A validation message may replace the displayed label. Use input_type, input_name and autocomplete to distinguish the field from nearby fields; an email address and phone number are not interchangeable. Use the field's displayed format for dates, except in a field whose input_type is date, datetime-local, month, week or time, which takes ISO 8601 (2026-09-25, 2026-09-25T14:30, 2026-09, 2026-W39, 14:30). A day named relative to today (tomorrow, the next Friday) is the first matching day after today in the calendar under page.date, never a date you count yourself. A fact the task states in another shape is given, not missing: take the part of a stated name, address or date this field asks for and write it in the field's shape. A field for a name as a whole takes the whole name as stated; only a field for one part, such as a first name, takes that part. The requirements, or the task itself when no requirements are given, list its steps in the order it wants them done. When more than one gives this field a value (enter one, later change it), list every such value in values, in that order, and write in text the earliest one no recent action has already typed into this field.\n\n# Trust\n{UNTRUSTED}"
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
        decided_by: Decider,
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
                self.gate_download(state, observation, decision, decided_by, &caller_link(&url)).await?;
                let irreversible = navigation_question(&state.task, &url);
                let requested = requested_question(&state.task, &format!("open the address {url}.\n"));
                self.gate_question(state, observation, decision, decided_by, &url, Some(irreversible), requested)
                    .await?;
                Ok(Action { url: Some(url), ..Action::new(Operation::Navigate) })
            }
            Operation::Click | Operation::Enter => {
                self.gate_irreversible(state, observation, decision, decided_by).await?;
                Ok(Action { target_id: target.map(|t| t.id.clone()), ..Action::new(decision.operation) })
            }
            Operation::Drag => {
                let Some(destination) = &decision.destination else {
                    return Err(Stop::new(Status::NeedsInput, "the page offers no drop target for the drag").into());
                };
                self.gate_irreversible(state, observation, decision, decided_by).await?;
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
                    self.gate_question(
                        state,
                        observation,
                        decision,
                        decided_by,
                        &dialog.message,
                        Some(irreversible),
                        requested,
                    )
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
            Operation::Read | Operation::Done | Operation::Escalate => Err(Flow::Agent(crate::AgentError::Invalid(
                format!("{} is handled by the loop, not dispatched", decision.operation),
            ))),
        }
    }

    async fn gate_irreversible(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        decided_by: Decider,
    ) -> Step {
        let Some(target) = decision.target.as_ref().filter(|_| may_be_irreversible(decision.operation)) else {
            return Ok(());
        };
        self.gate_download(state, observation, decision, decided_by, target).await?;
        let mut label = describe(target);
        if let Some(destination) = &decision.destination {
            label += &format!(" onto {}", describe(destination));
        }
        // A credential change is irreversible on the page's own structure, so Jev is not asked whether it is.
        let irreversible = (!changes_credentials(&observation.controls, target, decision.operation))
            .then(|| irreversible_question(&state.task, decision.operation, target, decision.destination.as_ref()));
        let requested =
            requested_question(&state.task, &control_action(decision.operation, target, &observation.controls));
        self.gate_question(state, observation, decision, decided_by, &label, irreversible, requested).await
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
        decided_by: Decider,
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
        let confidence = (decided_by == Decider::Jev).then(|| decision.confidence());
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

    /// The one rule for an action that may commit: it proceeds when Jev judges it not irreversible, or when the run is
    /// authorized and Jev judges the task asked for it. Otherwise the run stops for confirmation.
    ///
    /// `irreversible` is None for an action the page's structure shows commits, which only the task's request lifts.
    /// A pick under `recover_below` never gets here: it goes to recovery before it acts.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn gate_question(
        &self,
        state: &mut RunState,
        observation: &Observation,
        decision: &Decision,
        decided_by: Decider,
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
            if probability.is_some_and(|p| p <= thresholds.irreversible_above)
                || requested.is_some_and(|p| p > thresholds.requested_above)
            {
                return Ok(());
            }
        }
        let reason = format!("{} {} needs confirmation", decision.operation, text::repr(label));
        let confidence = (decided_by == Decider::Jev).then(|| decision.confidence());
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
        Err(Stop::new(Status::NeedsConfirmation, reason).into())
    }

    pub(super) async fn text(&self, state: &mut RunState, observation: &Observation, target: &Control) -> Step<String> {
        let origin = self.target_origin(target);
        // Only what this field's own frame may receive: a password manager offers its names across every site it
        // holds, and offering one that cannot be typed here ended a sign-in at needs_input, naming a secret the
        // page was never going to get, where needs_login says what the caller can fix.
        let secrets = self.secret_names(&origin);
        if is_password(target) {
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

    async fn generate_text(&self, state: &mut RunState, observation: &Observation, target: &Control) -> Step<String> {
        let key = (observation.document_key.clone(), target.id.clone());
        if !state.form_values.contains_key(&key) {
            self.write_form(state, observation, target).await?;
        }
        // A value is used only while the field's identity, meaning and empty value survive the fresh observation, and
        // the value due now moves on as fills are typed.
        if let Some((control, value)) = state.form_values.remove(&key)
            && (Control { offscreen: target.offscreen, ..control }) == *target
        {
            return Ok(due(&value, &state.history, &target.label));
        }
        let context = self.field_context(state, observation, target).await?;
        // Mid-run a task's next item is searched for too, not only an output the answer check found missing: given the
        // task without this rule, the writer called a search box's query missing and ended a six-part cart run.
        let system = format!("{}\n\n{OUTPUT_SEARCH}", field_writer());
        let messages = vec![Message::system(system), Message::user(dumps(&Value::Object(context)))];
        if let Some(written) = self.write_field(state, &messages, target).await? {
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
        match self.write_field(state, &insisted, target).await? {
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
        context.insert(
            "page".into(),
            json!({
                "url": observation.url,
                "title": observation.title,
                "text": observation.viewport_text,
                "date": observation.today(),
            }),
        );
        let recent: Vec<Value> = state.history.iter().map(Dump::dump_exclude_none).collect();
        context.insert("recent_actions".into(), Value::Array(recent));
        context.insert("notes".into(), state.notes.render_for_navigation().into());
        Ok(context)
    }

    /// Write every empty field of `target`'s form in one writer call and keep the values, so each fill Jev picks in
    /// that form types its value without a call of its own. Jev still picks every fill.
    ///
    /// Nothing is written while any field of the form still has a kept value. A writer failure leaves the field to
    /// its own writer.
    async fn write_form(&self, state: &mut RunState, observation: &Observation, target: &Control) -> Step {
        let Some(form_id) = &target.form_id else { return Ok(()) };
        let fields: IndexMap<String, Control> = observation
            .controls
            .iter()
            .filter(|c| {
                c.frame_id == target.frame_id
                    && c.form_id.as_deref() == Some(form_id.as_str())
                    && c.operations.contains(&Operation::Fill)
                    && c.value.as_deref().is_none_or(str::is_empty)
                    && c.role == "textbox"
                    && !c.sensitive
            })
            .map(|c| (c.id.clone(), c.clone()))
            .collect();
        let kept =
            fields.keys().any(|id| state.form_values.contains_key(&(observation.document_key.clone(), id.clone())));
        if !fields.contains_key(&target.id) || fields.len() < 2 || kept {
            return Ok(());
        }
        let mut context = self.field_context(state, observation, target).await?;
        let field = context.shift_remove("field").unwrap_or(Value::Null);
        context.insert("chosen_field".into(), field);
        context.insert("fields".into(), Value::Array(fields.values().map(Dump::dump_exclude_none).collect()));
        let messages = vec![Message::system(form_writer()), Message::user(dumps(&Value::Object(context)))];
        let written =
            match generate::<FormText>(&*self.llm, LLMPurpose::FieldText, &messages, Some(&state.ledger)).await {
                Ok(generation) => {
                    state.ledger.record([generation.cost.clone()])?;
                    generation.data
                }
                Err(error) => {
                    let flow = Flow::from(error);
                    return if flow.is_llm() { Ok(()) } else { Err(flow) };
                }
            };
        let ids: Vec<&String> = written.fields.iter().map(|f| &f.id).collect();
        self.event("form_values", || {
            Map::from_iter([("chosen".into(), target.id.clone().into()), ("fields".into(), json!(ids))])
        });
        for value in written.fields {
            let Some(control) = fields.get(&value.id) else { continue };
            if !value.missing && !value.text.is_empty() && !self.reveals_text(&value.dump_json()) {
                state
                    .form_values
                    .insert((observation.document_key.clone(), value.id.clone()), (control.clone(), value.as_text()));
            }
        }
        Ok(())
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
    async fn write_field(&self, state: &mut RunState, messages: &[Message], target: &Control) -> Step<Option<String>> {
        let generation =
            generate::<FieldText>(&*self.llm, LLMPurpose::FieldText, messages, Some(&state.ledger)).await?;
        state.ledger.record([generation.cost.clone()])?;
        let written = generation.data;
        Ok((!written.missing).then(|| due(&written, &state.history, &target.label)))
    }

    pub(super) async fn value_absent(
        &self,
        state: &mut RunState,
        observation: &Observation,
        target: &Control,
    ) -> Step<bool> {
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
        // No answer is no finding: it says nothing about the value, so it cannot stop the run as absent.
        let Some(Answer::Noul(answer)) = evaluation.answers.get("stated") else {
            return Err(fastbrowse_core::decision::DecisionError::Failed("expected a noul answer".into()).into());
        };
        let stated = answer.probability;
        // Jev's own answer: a NEEDS_INPUT stop stands unless Jev says the value is given.
        Ok(stated <= EVEN_ODDS)
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
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let offered: HashSet<String> = criteria.keys().cloned().collect();
        let choice = Question::Choice(ChoiceQuestion::new(
            format!("{UNTRUSTED}\n\n# Task\n{}\n\n{question}", state.task),
            criteria,
        ));
        let page = page_state(observation, &state.notes, &self.config.tokens, &[choice.dump_json()], None)?;
        let questions = IndexMap::from([("pick".to_string(), choice)]);
        let evaluation = self.jev.evaluate(&page, &questions).await?;
        state.ledger.record([evaluation.cost.clone()])?;
        let Some(Answer::Choice(answer)) = evaluation.answers.get("pick") else {
            return Err(fastbrowse_core::decision::DecisionError::Failed("expected a choice answer".into()).into());
        };
        if !offered.contains(&answer.choice) {
            let message = format!("Jev picked {}, which was not offered", answer.choice);
            return Err(fastbrowse_core::decision::DecisionError::Failed(message).into());
        }
        Ok(answer.choice.clone())
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
