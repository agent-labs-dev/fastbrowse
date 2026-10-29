//! Port of `src/fastbrowse/verification.py`: deciding whether a run is actually finished, and whether its answer and
//! extracted data hold up.
//!
//! Jev checks completion, individual action requirements and draft quality. An LLM looks at a screenshot when those
//! answers leave completion uncertain.
//!
//! # Concurrency
//!
//! Python sends the transaction question beside the claim questions with `asyncio.gather(..., return_exceptions=True)`.
//! A replay keys each request on its body, so what has to match Python is the set of requests and every decision, not
//! the order they leave in. Here such a group runs through [`crate::support::join2`], which polls both to their end
//! before any error is looked at, so a failure never leaves a request Python sent unsent. Errors are then raised in the
//! order Python raises them: the claim questions before the transaction question.

mod prompts;

use std::collections::HashSet;
use std::sync::Mutex;

use fastbrowse_core::config::{Config, Thresholds, TokenBudget};
use fastbrowse_core::decision::{Answer, DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::{Generation, LLMClient, Message};
use fastbrowse_core::models::{Bytes, CostLine, Evidence, LLMPurpose, SourceControl, UNTRUSTED};
use fastbrowse_core::output_schema::OutputModel;
use fastbrowse_core::page::{Capture, Control, Observation, cut_text};
use fastbrowse_core::py::dump::{Dump, include};
use fastbrowse_core::py::json::dumps;
use fastbrowse_core::py::{round, sha256_hex, text};
use fastbrowse_core::schema::{Field, ModelTy, Schema, Ty};
use fastbrowse_core::telemetry::{Ledger, Trace};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::batches::evaluate_batches;
use crate::error::{AgentError, Result};
use crate::memory::{Notes, Standing, fact_id};
use crate::planner::{Plan, RequirementKind};
use crate::policy::HistoryEntry;
use crate::retrieval::{
    AnswerCorrection, Claim, ComposedAnswer, OUTPUT_MISSING, ScalarField, Stage, TRANSACTION_CONTRADICTED,
    UnsupportedField, assemble_answer, choose_candidate, claim_check_questions, field_candidates,
    field_candidates_from_notes, merge_candidates, propose_text_fields, propose_text_fields_from_notes,
    transaction_check_question,
};
use crate::support::{generate, join2};

use prompts::{STALE_REPAIR, UNFINISHED_REPAIR, UNGROUNDED, UNSTATED_REPAIR};

str_enum! {
    pub enum DoneVerdict {
        Accept = "accept",
        /// Uncertain: the LLM verifier decides.
        Verify = "verify",
        Reject = "reject",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DoneCheck {
    pub verdict: DoneVerdict,
    pub complete: f64,
    /// Requirement ids that are not satisfied or not evidenced.
    pub unmet: Vec<String>,
    /// Requirement ids Jev did not confidently confirm, which the verifier must see shown rather than assume.
    pub doubted: Vec<String>,
    /// The offered draft when Jev judged it already answers the task, so no composer needs to run.
    pub answer: Option<ComposedAnswer>,
    pub cost: CostLine,
}

/// A verdict is three short lists and a boolean. Measured on a live count-pagination run, an uncapped verdict spent
/// 1166 output tokens and 13.7s restating the answer it was judging; the cap bounds that pathology while leaving room
/// for every requirement id on a long plan.
pub const VERDICT_TOKENS: i64 = 400;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LLMVerdict {
    pub missing: Vec<String>,
    #[serde(default)]
    pub ungrounded: Vec<String>,
    pub complete: bool,
}

impl Schema for LLMVerdict {
    fn ty() -> Ty {
        ModelTy::frozen(
            "LLMVerdict",
            vec![
                Field::required("missing", Ty::list(Ty::Str))
                    .doc("Requirement ids the page or notes do not show as satisfied."),
                Field::default("ungrounded", Ty::list(Ty::Str), json!([])).doc(UNGROUNDED),
                Field::required("complete", Ty::Bool)
                    .doc("Whether the evidence shows that every task requirement is satisfied."),
            ],
        )
        .ty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Extraction {
    pub data: Option<Value>,
    pub evidence: Vec<Evidence>,
    pub problem: Option<String>,
}

fn controls<'a>(controls: impl IntoIterator<Item = &'a Control>) -> Vec<Value> {
    controls
        .into_iter()
        .map(|control| {
            include(control.dump_exclude_none(), &["label", "context", "role", "value", "checked", "selected"])
        })
        .collect()
}

/// The controls holding a value, a check or a selection: the state page text does not show.
fn stateful(controls: &[Control]) -> Vec<&Control> {
    controls.iter().filter(|c| c.value.is_some() || c.checked.is_some() || c.selected.is_some()).collect()
}

/// `page_state`: the page, its stateful controls and the notes for a Jev verdict, the notes' requirement evidence kept
/// whole.
pub fn page_state(
    observation: &Observation,
    notes: &Notes,
    tokens: &TokenBudget,
    questions: &[String],
    context: Option<&Map<String, Value>>,
) -> Result<Value> {
    let mut state: Map<String, Value> = context.cloned().unwrap_or_default();
    // The dropdown choice runs apart from the action choice; it also needs today's date for relative months.
    state.insert("date".into(), observation.today().into());
    state.insert(
        "page".into(),
        json!({"url": observation.url, "title": observation.title, "text": observation.viewport_text}),
    );
    // Inputs and ARIA selection states are absent from innerText; without them a preview can pass completion even
    // though the requested filters were never applied.
    state.insert("controls".into(), Value::Array(controls(&observation.controls)));
    state.insert("notes".into(), "".into());
    let room =
        |state: &Map<String, Value>| tokens.remaining_chars(&dumps(&Value::Object(state.clone())), questions, true);
    match notes.render(room(&state), true, true) {
        Ok(rendered) => {
            state.insert("notes".into(), rendered.into());
        }
        Err(AgentError::NotesTooLarge(_)) => {
            // Preserve requirement evidence by cutting page text, then stateless controls, before refusing a verdict.
            state["page"]["text"] = "".into();
            match notes.render(room(&state), true, true) {
                Ok(rendered) => {
                    state.insert("notes".into(), rendered.into());
                }
                Err(AgentError::NotesTooLarge(_)) => {
                    let kept = stateful(&observation.controls);
                    state.insert("controls_omitted".into(), (observation.controls.len() - kept.len()).into());
                    state.insert("controls".into(), Value::Array(controls(kept)));
                    let rendered = notes.render(room(&state), true, true)?;
                    state.insert("notes".into(), rendered.into());
                }
                Err(other) => return Err(other),
            }
            let cut = cut_text(&observation.viewport_text, room(&state) as usize, true);
            state["page"]["text"] = cut.into();
        }
        Err(other) => return Err(other),
    }
    Ok(Value::Object(state))
}

/// `check_done`'s keyword arguments.
#[derive(Default)]
pub struct DoneOptions<'a> {
    pub tokens: TokenBudget,
    /// The run's action record. A page shows only where a process ended, so an accepted check that never saw the
    /// record would pass "enter one name, go back and correct it" on a run that typed the correction first.
    pub history: &'a [HistoryEntry],
    /// Every address the run has been on, first the one it began on, which no action records.
    pub visited: &'a [String],
    pub trace: Trace,
}

/// How a requirement that only moves through pages is worded (`_TRAVERSAL`), each a whole-word match ignoring case.
const TRAVERSAL: &[&str] = &[
    "navigate",
    "browse",
    "go through",
    "going through",
    "go to each",
    "visit",
    "scroll",
    "page through",
    "paging through",
    "read through",
    "look through",
    "open each",
    "move through",
    "click through",
    "walk through",
    "search through",
    "explore",
];

/// The verbs that make a requirement something the caller receives instead (`_COMMITTING`): a submission, an account,
/// a purchase. Python's optional words are spelled out.
const COMMITTING: &[&str] = &[
    "submit",
    "send",
    "sign in",
    "sign up",
    "log in",
    "log out",
    "login",
    "logout",
    "register",
    "add to cart",
    "add to basket",
    "add to bag",
    "add to the cart",
    "add to the basket",
    "add to the bag",
    "place order",
    "place the order",
    "place an order",
    "place your order",
    "buy",
    "purchase",
    "pay",
    "check out",
    "checkout",
    "book",
    "reserve",
    "delete",
    "remove",
    "upload",
    "download",
    "fill",
    "type",
    "enter",
    "apply",
    "save",
    "create",
    "post",
    "publish",
    "confirm",
    "accept",
    "subscribe",
];

/// Whether a requirement only directs the browsing, rather than naming something the caller receives.
///
/// A traversal requirement is instrumental: its point is the information it reaches, so once that information is
/// evidenced and verified, an unconfirmed visit does not leave the task unanswered. Anything that changes state on
/// the site keeps blocking, because a run that never submitted the form or placed the order has not done what the
/// caller asked for however good its evidence is.
pub fn traversal_only(text: &str) -> bool {
    text::find_word(text, TRAVERSAL).is_some() && text::find_word(text, COMMITTING).is_none()
}

fn probability(answers: &IndexMap<String, Answer>, key: &str) -> f64 {
    match answers.get(key) {
        Some(Answer::Noul(answer)) => answer.probability,
        _ => 0.0,
    }
}

/// Judge completion, and whether `draft` answers the task as written, in the one Jev call.
#[allow(clippy::too_many_arguments)]
pub async fn check_done(
    jev: &dyn DecisionClient,
    task: &str,
    plan: &Plan,
    observation: &Observation,
    notes: &Notes,
    thresholds: &Thresholds,
    draft: Option<&ComposedAnswer>,
    options: &DoneOptions<'_>,
) -> Result<DoneCheck> {
    let mut questions: IndexMap<String, Question> = IndexMap::new();
    questions.insert(
        "complete".into(),
        NoulQuestion::new(
            prompts::complete(),
            Some("Everything the task asks for is visibly done."),
            Some("Something the task asks for is missing, unsubmitted or unconfirmed."),
        )
        .into(),
    );
    let unevidenced: HashSet<&str> = plan
        .requirements
        .iter()
        .filter(|r| r.kind == RequirementKind::Information && !notes.evidenced(&r.id))
        .map(|r| r.id.as_str())
        .collect();
    let mut unmet: Vec<String> = unevidenced.iter().map(|id| id.to_string()).collect();
    unmet.sort();
    // A requirement that only directs the browsing -- go through every page, browse the category, visit each listing
    // -- is how the run reaches the information, not something the caller receives. Once every information
    // requirement is evidenced, holding the run open for one of these buys nothing: a count read off the site's own
    // exhaustive tag listing still looped on "navigate through every page" until its recoveries ran out. The answer
    // stands; the browsing step is reported as unconfirmed rather than blocking an evidenced answer. The exemption
    // stands in for an answer that is already proved. A plan that asks for no answer, or one with no information
    // requirement to prove, has no answer for a visit to stand in for, so its navigation is the task.
    let standing_answer =
        plan.page_answer_expected() && plan.requirements.iter().any(|r| r.kind == RequirementKind::Information);
    let instrumental: HashSet<&str> = if standing_answer && unevidenced.is_empty() {
        plan.requirements
            .iter()
            .filter(|r| r.kind == RequirementKind::Action && traversal_only(&r.text))
            .map(|r| r.id.as_str())
            .collect()
    } else {
        HashSet::new()
    };
    for requirement in plan.requirements.iter().filter(|r| !instrumental.contains(r.id.as_str())) {
        match requirement.kind {
            RequirementKind::Action => {
                questions.insert(
                    format!("unmet_{}", requirement.id),
                    NoulQuestion::new(
                        prompts::unmet_action(&requirement.text),
                        Some("It is not satisfied, or there is no visible confirmation."),
                        Some("The page visibly confirms it is satisfied."),
                    )
                    .into(),
                );
            }
            // Evidence proves a quote came from a page, not that it answers: the wrong package's date is evidenced
            // too. Asked per requirement, a lookup gets the per-requirement confirmation an action has, rather than
            // every lookup going to the verifier on the strict holistic question alone.
            RequirementKind::Information if !unevidenced.contains(requirement.id.as_str()) => {
                questions.insert(
                    format!("unmet_{}", requirement.id),
                    NoulQuestion::new(
                        prompts::unmet_information(&requirement.text),
                        Some("A fact it needs is missing, about something else, or only a preview."),
                        Some("The notes hold every fact it needs."),
                    )
                    .into(),
                );
            }
            RequirementKind::Information => {}
        }
    }
    let mut draft = draft;
    if draft.is_some() {
        // Asked here rather than on its own because this call is already being paid for: judging the draft costs one
        // more answer in a request the run makes anyway, where a composer costs seconds.
        questions.insert(
            "draft_needs_writing".into(),
            NoulQuestion::new(
                prompts::draft_needs_writing(),
                Some("Yes, it needs rewriting before it answers the task."),
                Some("No, it answers the task as written."),
            )
            .into(),
        );
    }
    let mut context = Map::new();
    context.insert("task".into(), task.into());
    context.insert("run_reports".into(), json!(plan.run_reports));
    if !options.history.is_empty() {
        context.insert("actions".into(), options.history.iter().map(step).collect::<Vec<_>>().join("\n").into());
    }
    if !options.visited.is_empty() {
        context.insert("visited".into(), json!(options.visited));
    }
    if let Some(draft) = draft {
        context.insert("draft".into(), draft.answer.clone().into());
    }
    let written = |questions: &IndexMap<String, Question>| questions.values().map(Dump::dump_json).collect::<Vec<_>>();
    let state = match page_state(observation, notes, &options.tokens, &written(&questions), Some(&context)) {
        Ok(state) => state,
        Err(AgentError::NotesTooLarge(message)) => {
            if draft.is_none() {
                return Err(AgentError::NotesTooLarge(message));
            }
            // The optional answer draft crowded out the quotes required to judge completion.
            context.shift_remove("draft");
            questions.shift_remove("draft_needs_writing");
            draft = None;
            page_state(observation, notes, &options.tokens, &written(&questions), Some(&context))?
        }
        Err(other) => return Err(other),
    };
    let evaluation = jev.evaluate(&state, &questions).await?;
    let answers = &evaluation.answers;
    for requirement in plan.requirements.iter().filter(|r| !instrumental.contains(r.id.as_str())) {
        if probability(answers, &format!("unmet_{}", requirement.id)) > thresholds.claim_problem_above {
            unmet.push(requirement.id.clone());
        }
    }
    let complete = probability(answers, "complete");
    // Every requirement confirmed one by one is stronger evidence than the strict holistic question alone, which asks
    // about the whole task at once and doubts a right page as often as it confirms it. An answer Jev did not give
    // confirms nothing, and a task with nothing to do keeps the verifier.
    let doubted: Vec<String> = plan
        .requirements
        .iter()
        .filter(|r| {
            !instrumental.contains(r.id.as_str())
                && !matches!(answers.get(&format!("unmet_{}", r.id)),
                    Some(Answer::Noul(doubt)) if doubt.probability < thresholds.requirement_confirmed_below)
        })
        .map(|r| r.id.clone())
        .collect();
    let confirmed = !plan.requirements.is_empty() && doubted.is_empty();
    // Jev reliably confirms a visible result but is too strict to reject one on its own, so apart from information
    // nobody has read, doubt goes to the verifier rather than straight back to work.
    let verdict = if !unevidenced.is_empty() {
        DoneVerdict::Reject
    } else if unmet.is_empty()
        && (complete >= thresholds.done_accept_from || (confirmed && complete >= thresholds.done_confirmed_from))
    {
        DoneVerdict::Accept
    } else {
        DoneVerdict::Verify
    };
    // An answer Jev did not give is not a yes: only a present, confident "no rewrite needed" skips the composer.
    let rewrite = match answers.get("draft_needs_writing") {
        Some(Answer::Noul(doubt)) => Some(doubt.probability),
        _ => None,
    };
    let ready = rewrite.is_some_and(|p| p < thresholds.rewrite_from);
    if let Some(draft) = draft {
        options.trace.event("draft", || {
            let mut fields = Map::new();
            fields.insert("rewrite".into(), json!(rewrite));
            fields.insert("ready".into(), ready.into());
            fields.insert("claims".into(), draft.claims.len().into());
            fields
        });
    }
    Ok(DoneCheck {
        verdict,
        complete,
        unmet,
        doubted,
        answer: if ready { draft.cloned() } else { None },
        cost: evaluation.cost,
    })
}

/// Where each requirement's facts were read, and which addresses the run guessed rather than clicked to.
///
/// A requirement can be evidenced from a page that is not the search the task asked for: a proposed address opened a
/// flights summary, the reader quoted a price from it, and every check passed on evidence the run should never have
/// had. The verifier cannot see that without being told which page each fact came from.
#[doc(hidden)]
pub fn grounding(notes: &Notes, plan: &Plan, invented: &[String]) -> String {
    let mut lines = Vec::new();
    for requirement in &plan.requirements {
        let mut urls: Vec<String> = notes
            .supporting_evidence(&requirement.id)
            .into_iter()
            .map(|e| e.url)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        urls.sort();
        if !urls.is_empty() {
            lines.push(format!("- {}: {}", requirement.id, urls.join(", ")));
        }
    }
    let mut parts = Vec::new();
    if !lines.is_empty() {
        parts.push(format!("## Where each requirement's facts were read\n{}", lines.join("\n")));
    }
    if !invented.is_empty() {
        let listed: Vec<String> = invented.iter().map(|u| format!("- {u}")).collect();
        parts.push(format!("## Addresses this run built from the task\n{}", listed.join("\n")));
    }
    if parts.is_empty() { String::new() } else { format!("{}\n\n", parts.join("\n\n")) }
}

fn visited_section(visited: &[String]) -> String {
    if visited.is_empty() {
        return String::new();
    }
    let listed: String = visited.iter().map(|url| format!("- {url}\n")).collect();
    format!("## Visited addresses\n{listed}\n")
}

/// One action as the run recorded it: the value typed and what it visibly did, both already redacted.
fn step(entry: &HistoryEntry) -> String {
    let operation = entry.operation.map_or("open", |op| op.as_str());
    let action = format!("{operation} {}", entry.target.as_deref().unwrap_or(""));
    let action = text::strip(&action);
    let typed = entry.text.as_ref().map(|t| format!(" = {}", text::repr(t))).unwrap_or_default();
    let effect = entry.effect.as_ref().filter(|e| !e.is_empty()).map(|e| format!(" ({e})")).unwrap_or_default();
    format!("- {action}{typed} -> {}{effect}", entry.outcome.as_str())
}

const VISUAL_ONLY_INPUTS: [&str; 8] = ["file", "color", "range", "date", "time", "month", "week", "datetime-local"];

/// Whether this page's state is carried by pixels, so a verifier needs the screenshot.
///
/// Page text, the controls and the notes cover a form, a listing and a quoted answer. What they do not carry is a
/// checked box, a selected option, a calendar's highlighted day, a canvas or a chart, a file input's chosen file, or a
/// dialog drawn over the page. A page with almost no text is judged the same way: there is nothing in it to read, so
/// the screenshot is the only evidence there is.
pub fn visual_only_state(observation: &Observation) -> bool {
    if observation.dialog.is_some() {
        return true;
    }
    // A page can carry its state in pixels without a single visual control: snapshot.js offers ordinary images
    // and canvases as content, not as controls, so the control list alone cannot see a chart or a picture.
    if observation.visual_content {
        return true;
    }
    if text::len(text::strip(&observation.viewport_text)) < 40 {
        return true;
    }
    observation.controls.iter().any(|c| {
        c.checked.is_some()
            || c.selected.is_some()
            || VISUAL_ONLY_INPUTS.contains(&c.input_type.as_deref().unwrap_or("").to_lowercase().as_str())
            || ["canvas", "img", "image"].contains(&c.role.to_lowercase().as_str())
    })
}

/// `verify_messages`' keyword arguments.
#[derive(Default)]
pub struct VerifyOptions<'a> {
    /// The requirements the done check doubted, which the verifier must see shown, not assume.
    pub doubted: &'a [String],
    /// Addresses this run built from the task rather than reached by clicking, the ones that can land on a page that
    /// looks like the answer without being the search the task asked for.
    pub invented: &'a [String],
    /// Every address the run has been on, first the one it began on.
    pub visited: &'a [String],
    pub config: Config,
    pub ledger: Option<&'a Ledger>,
}

fn list_repr(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| text::repr(v)).collect();
    format!("[{}]", items.join(", "))
}

/// The verifier's request, in the order that keeps the stable prefix ahead of the page.
///
/// The task, its requirements and the verdict rules are the same on every finish attempt of a run, so they sit in the
/// system message and at the head of the user message; the page text and notes, which change every time, come last.
/// A provider that caches a request prefix can then reuse it across attempts.
#[allow(clippy::too_many_arguments)]
pub fn verify_messages(
    task: &str,
    plan: &Plan,
    observation: &Observation,
    screenshots: &[Vec<u8>],
    notes: &Notes,
    history: &[HistoryEntry],
    options: &VerifyOptions<'_>,
) -> Result<Vec<Message>> {
    // A flights search passed here with Jev doubting its one requirement at 0.14: the rows matched, and nothing asked
    // whether the nonstop filter the task named had ever been applied.
    let requirements: Vec<String> = plan
        .requirements
        .iter()
        .map(|r| {
            let doubt = if options.doubted.contains(&r.id) {
                " (doubted: show it is satisfied, or name it missing)"
            } else {
                ""
            };
            format!("- {}: {}{doubt}", r.id, r.text)
        })
        .collect();
    // What was typed and what each action visibly did, not only that it ran: told "fill First Name -> executed", the
    // verifier could not tell a run that corrected a value from one that typed the correction first.
    let steps: Vec<String> = history.iter().map(step).collect();
    // A filter's checked state is absent from page text, and a screenshot shows it only when it is in view. Only what
    // is set: every empty field on a long form would crowd the request without saying anything.
    let set: Vec<&Control> = observation
        .controls
        .iter()
        .filter(|c| {
            c.checked == Some(true) || c.selected == Some(true) || c.value.as_deref().is_some_and(|v| !v.is_empty())
        })
        .collect();
    let evidence = if screenshots.is_empty() {
        "the set controls, page text and notes"
    } else {
        "the screenshot, set controls, page text and notes"
    };
    let reports: Vec<String> = plan.run_reports.iter().map(|r| r.as_str().to_string()).collect();
    let mut messages = vec![
        Message::system(format!("# Verifier\n{UNTRUSTED}\n\n{}", prompts::verdict(evidence))),
        Message {
            images: screenshots.iter().map(|image| Bytes(image.clone())).collect(),
            ..Message::user(prompts::verify_user(
                task,
                &observation.today(),
                &requirements.join("\n"),
                &list_repr(&reports),
                &steps.join("\n"),
                &dumps(&Value::Array(controls(set))),
                &grounding(notes, plan, options.invented),
                &visited_section(options.visited),
                &observation.url,
            ))
        },
    ];
    let schema = dumps(&LLMVerdict::ty_schema());
    let room = |messages: &[Message], parts: &[&str]| {
        let prompt: String = messages.iter().map(|m| m.content.as_str()).chain(parts.iter().copied()).collect();
        options.config.tokens.remaining_chars::<&str>(&format!("{prompt}{schema}"), &[], false)
    };
    // As in the done check's page state, the notes' requirement evidence is placed first and the page text is cut to
    // what remains: a "View more" results page once left the notes no room at all and ended the run.
    let rendered = notes.render(room(&messages, &["\n\n## Notes\n"]), true, false)?;
    let cut = cut_text(&observation.viewport_text, room(&messages, &["\n\n## Notes\n", &rendered]) as usize, false);
    let last = messages.len() - 1;
    messages[last].content = format!("{}\n\n## Notes\n{rendered}\n\n{cut}", messages[last].content);
    Ok(messages)
}

impl LLMVerdict {
    fn ty_schema() -> Value {
        fastbrowse_core::schema::json_schema(&LLMVerdict::ty())
    }
}

fn digest(data: &[u8]) -> Vec<u8> {
    let hex = sha256_hex(data);
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or_default()).collect()
}

/// A digest of the exact request: the prompt and every image byte, in order.
///
/// Two finish attempts whose rendered request is byte-identical cannot need a second verdict, so the verdict paid for
/// by the first is reused.
pub fn verify_cache_key(messages: &[Message]) -> String {
    let mut fed = Vec::new();
    for message in messages {
        fed.extend_from_slice(message.role.as_str().as_bytes());
        fed.extend_from_slice(message.content.as_bytes());
        for image in &message.images {
            fed.extend(digest(&image.0));
        }
    }
    sha256_hex(&fed)
}

/// The verifier's verdict for one rendered request; see [`verify_messages`] for the shape of the evidence.
#[allow(clippy::too_many_arguments)]
pub async fn llm_verify(
    llm: &dyn LLMClient,
    task: &str,
    plan: &Plan,
    observation: &Observation,
    screenshots: &[Vec<u8>],
    notes: &Notes,
    history: &[HistoryEntry],
    options: &VerifyOptions<'_>,
) -> Result<Generation<LLMVerdict>> {
    let messages = verify_messages(task, plan, observation, screenshots, notes, history, options)?;
    Ok(generate::<LLMVerdict>(llm, LLMPurpose::Verify, &messages, VERDICT_TOKENS, options.ledger).await?)
}

// --- output checks ----------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
struct OutputSource {
    url_ref: String,
    quote: String,
    complete_source: bool,
    source_id: String,
    frame_id: Option<String>,
    page_title: Option<String>,
    control_context: Option<SourceControl>,
}

/// The read's place in reading order, from 1: one address can be read in several states.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct CollectionReadRef {
    read: usize,
    url_ref: String,
}

/// A single-page list or a lone group has no pages or standing to show; an empty field there would only describe a
/// proof that does not apply.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct OutputCount {
    scope: Option<String>,
    group: String,
    count: usize,
    complete: bool,
    record_indices: Vec<usize>,
    /// This group's rank among every group counted in the same scope; None when the scope has one group.
    #[serde(skip_serializing_if = "Option::is_none")]
    standing: Option<Standing>,
    /// Every read code made of this collection, in order, including reads with no counted record.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pages_read: Vec<CollectionReadRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct OutputComparison {
    scope: Option<String>,
    /// The comparison's code-owned requirement id: two different lists can share a heading, so the scope text the
    /// reader wrote cannot identify a collection on its own.
    requirement: String,
    complete: bool,
    record_indices: Vec<usize>,
    /// Every read code made of this collection, in order, including reads with no compared record.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pages_read: Vec<CollectionReadRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct OutputClaim {
    text: String,
    cited_sources: Vec<OutputSource>,
    counted_records: Vec<OutputCount>,
    compared_records: Vec<OutputComparison>,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputContext {
    answer: String,
    claims: Vec<OutputClaim>,
    urls: IndexMap<String, String>,
}

/// `_output_context`: the claims as the output check sees them, each with its cited sources and the counts and comparisons
/// it rests on, or None when a cited fact is no longer current or a record is missing.
#[doc(hidden)]
pub fn output_context(composed: &ComposedAnswer, notes: &Notes) -> Option<OutputContext> {
    let requirements: IndexMap<&str, &str> =
        composed.requirements.iter().map(|r| (r.id.as_str(), r.text.as_str())).collect();
    let mut urls: IndexMap<String, String> = IndexMap::new();
    let alias = |urls: &mut IndexMap<String, String>, url: &str| -> String {
        let next = format!("u{}", urls.len());
        urls.entry(url.to_string()).or_insert(next).clone()
    };
    let facts = notes.facts();
    let mut claims = Vec::new();
    for claim in &composed.claims {
        let expanded = notes.expand_evidence_ids(claim.evidence_ids.iter().map(String::as_str));
        if expanded.iter().any(|key| !notes.current(key)) {
            return None;
        }
        // A refused quote is dropped from the check, so a claim resting on it alone has no source and one resting on it
        // beside a valid quote is judged against the valid quote only.
        let known = notes.usable_citations(expanded.iter().map(String::as_str));
        let keys: Vec<&String> = expanded.iter().filter(|key| known.contains_key(*key)).collect();
        if keys.is_empty() {
            return None;
        }
        let mut sources = Vec::new();
        for key in &keys {
            let evidence = &known[*key];
            let page = notes.captured_page(evidence);
            sources.push(OutputSource {
                url_ref: alias(&mut urls, &evidence.url),
                quote: evidence.quote.clone(),
                complete_source: evidence.complete_source,
                source_id: evidence.source_id.clone(),
                frame_id: evidence.frame_id.clone(),
                page_title: page.filter(|_| evidence.frame_id.is_none()).map(|page| page.title.clone()),
                control_context: evidence.control_context.clone(),
            });
        }
        let positions: IndexMap<&String, usize> = keys.iter().enumerate().map(|(i, key)| (*key, i)).collect();
        let pages_read = |urls: &mut IndexMap<String, String>, requirement_id: &str| -> Vec<CollectionReadRef> {
            notes
                .collection_pages(requirement_id)
                .into_iter()
                .enumerate()
                .map(|(index, read)| CollectionReadRef { read: index + 1, url_ref: alias(urls, &read.url) })
                .collect()
        };
        let mut counted = Vec::new();
        let mut compared = Vec::new();
        for fact in &facts {
            let key = fact_id(fact);
            if let Some(comparison) = &fact.comparison {
                let within = |ids: &[String]| ids.iter().all(|id| positions.contains_key(id));
                let shown = expanded.contains(&key)
                    || (fact.is_collection()
                        && comparison.complete
                        && !comparison.records.is_empty()
                        && !fact.basis.is_empty()
                        && within(&fact.basis)
                        && notes.current(&key)
                        && within(&comparison.records));
                if shown {
                    if !within(&comparison.records) {
                        return None;
                    }
                    let mut indices: IndexSet<usize> = IndexSet::new();
                    indices.extend(comparison.records.iter().map(|record| positions[record]));
                    compared.push(OutputComparison {
                        scope: if fact.is_collection() {
                            fact.scope.clone()
                        } else {
                            requirements.get(comparison.requirement_id.as_str()).map(|t| t.to_string())
                        },
                        requirement: comparison.requirement_id.clone(),
                        complete: comparison.complete
                            && (fact.is_collection()
                                || notes.fact_requirements(&key).contains(&comparison.requirement_id)),
                        record_indices: indices.into_iter().collect(),
                        pages_read: pages_read(&mut urls, &comparison.requirement_id),
                    });
                }
            }
            let Some(tally) = &fact.tally else { continue };
            if !expanded.contains(&key) {
                continue;
            }
            if tally.records.iter().any(|record| !positions.contains_key(record)) {
                return None;
            }
            let mut indices: IndexSet<usize> = IndexSet::new();
            indices.extend(tally.records.iter().map(|record| positions[record]));
            counted.push(OutputCount {
                scope: requirements.get(tally.requirement_id.as_str()).map(|t| t.to_string()),
                group: tally.key.clone(),
                count: tally.count(),
                complete: notes.fact_requirements(&key).contains(&tally.requirement_id),
                record_indices: indices.into_iter().collect(),
                standing: if claim.ranks { notes.standing(tally) } else { None },
                pages_read: pages_read(&mut urls, &tally.requirement_id),
            });
        }
        // Flattening a count or comparison loses its collection scope and whether every page was read.
        claims.push(OutputClaim {
            text: claim.text.clone(),
            cited_sources: sources,
            counted_records: counted,
            compared_records: compared,
        });
    }
    Some(OutputContext {
        answer: composed.answer.clone(),
        claims,
        urls: urls.into_iter().map(|(url, alias)| (alias, url)).collect(),
    })
}

/// The keyword arguments of `check_answer_outputs` and `check_claims`.
pub struct CheckOptions<'a> {
    pub tokens: TokenBudget,
    pub ledger: Option<&'a Ledger>,
    pub missing_outputs: Option<&'a Mutex<Vec<String>>>,
    pub corrections: Option<&'a Mutex<Vec<AnswerCorrection>>>,
    pub verified_outputs: Option<&'a Mutex<Vec<String>>>,
    pub independent_fields: bool,
    pub trace: Trace,
    /// `check_claims` only.
    pub transaction_evidence_ids: Vec<String>,
    /// `check_claims` only.
    pub answer_checks: Vec<String>,
    /// `check_claims` only: each field answer whose outputs passed, for a later pass to keep.
    pub verified_fields: Option<&'a Mutex<Vec<ComposedAnswer>>>,
}

impl Default for CheckOptions<'_> {
    fn default() -> Self {
        CheckOptions {
            tokens: TokenBudget::default(),
            ledger: None,
            missing_outputs: None,
            corrections: None,
            verified_outputs: None,
            independent_fields: true,
            trace: Trace::disabled(),
            transaction_evidence_ids: Vec::new(),
            answer_checks: Vec::new(),
            verified_fields: None,
        }
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Appends each of `found` that `into` does not hold yet.
fn extend_new(into: &Mutex<Vec<String>>, found: &[String]) {
    let mut into = locked(into);
    for check in found {
        if !into.contains(check) {
            into.push(check.clone());
        }
    }
}

/// Records a source correction for each criterion that has none yet.
fn correct(corrections: &Mutex<Vec<AnswerCorrection>>, failed: &[String], claims: &[Claim], reason: &str) {
    let mut corrections = locked(corrections);
    let corrected: HashSet<String> =
        corrections.iter().filter(|c| c.stage == Stage::Source).map(|c| c.criterion.clone()).collect();
    let added: Vec<AnswerCorrection> = failed
        .iter()
        .filter(|criterion| !corrected.contains(*criterion))
        .map(|criterion| AnswerCorrection {
            stage: Stage::Source,
            criterion: criterion.clone(),
            claims: claims.to_vec(),
            reason: reason.to_string(),
        })
        .collect();
    corrections.extend(added);
}

/// Whether code can ground every answer check: each one has a claim, and every claim cites current quotes and the
/// records its count or comparison rests on.
///
/// No model is asked. A second LLM opinion on quoted answers used to follow, and over the 0.7.1 campaign and 27 traced
/// reruns it never caught a wrong answer while rejecting correct ones (a version cited without the "latest release"
/// label beside it, a table total without proof the rows were all the rows); it was most of a lookup's wall time.
/// Whether the claims are supported, contradicted or leave a requirement out is Jev's claim check beside this one.
pub fn check_answer_outputs(
    composed: &ComposedAnswer,
    notes: &Notes,
    checks: &[String],
    options: &CheckOptions<'_>,
) -> bool {
    if options.independent_fields
        && let Some(outputs) = &composed.output_claims
    {
        let mut passed: Vec<String> = Vec::new();
        for check in checks {
            let mut indices: Vec<usize> = outputs.get(check).cloned().unwrap_or_default();
            if indices.iter().any(|index| *index >= composed.claims.len()) {
                indices = Vec::new();
            }
            // A field with no claim is still checked, so its rejection carries the correction the composer needs.
            let claims: Vec<Claim> = indices.iter().map(|index| composed.claims[*index].clone()).collect();
            let base = assemble_answer(&claims, notes, &composed.requirements, 0);
            let field = base.with_outputs(IndexMap::from([(check.clone(), (0..claims.len()).collect())]));
            if outputs_grounded(&field, notes, std::slice::from_ref(check), options) {
                passed.push(check.clone());
            }
        }
        if let Some(verified) = options.verified_outputs {
            locked(verified).extend(passed.iter().cloned());
        }
        let missing: Vec<String> = checks.iter().filter(|check| !passed.contains(check)).cloned().collect();
        if let Some(found) = options.missing_outputs
            && !missing.is_empty()
        {
            extend_new(found, &missing);
        }
        return missing.is_empty();
    }
    outputs_grounded(composed, notes, checks, options)
}

/// `_outputs_grounded`.
fn outputs_grounded(composed: &ComposedAnswer, notes: &Notes, checks: &[String], options: &CheckOptions<'_>) -> bool {
    let reject = |failed: &[String], repair: &str| -> bool {
        if let Some(missing) = options.missing_outputs {
            extend_new(missing, failed);
        }
        if let Some(corrections) = options.corrections {
            // Missing outputs can be repaired from uncited notes before browsing again.
            correct(corrections, failed, &composed.claims, repair);
        }
        false
    };
    if checks.is_empty() {
        return true;
    }
    if checks.iter().any(|check| text::strip(check).is_empty()) {
        return false;
    }
    if let Some(outputs) = &composed.output_claims {
        let missing: Vec<String> = checks
            .iter()
            .filter(|check| {
                outputs.get(*check).is_none_or(|indices| {
                    indices.is_empty() || indices.iter().any(|index| *index >= composed.claims.len())
                })
            })
            .cloned()
            .collect();
        if !missing.is_empty() {
            return reject(&missing, "This field has no claim. Report its value or a supported bounded absence.");
        }
    }
    let Some(context) = output_context(composed, notes) else {
        let known: HashSet<String> = notes.facts().iter().map(|fact| fact_id(fact)).collect();
        let unknown = composed
            .claims
            .iter()
            .flat_map(|claim| notes.expand_evidence_ids(claim.evidence_ids.iter().map(String::as_str)))
            .any(|key| !known.contains(&key));
        if unknown {
            return false;
        }
        return reject(checks, STALE_REPAIR);
    };
    // A count, ranking, winner or absence is only as good as the reading of its list: code knows whether every page
    // and chunk was read, so an unfinished one is refused here rather than argued over by a model.
    // A partial comparison read is superseded by a complete comparison of the same list: reading the rest of a
    // sorted list records the whole list as its own comparison, and a claim carrying both rests on the list read to
    // its end. Only a partial comparison no complete one of the same scope covers leaves the claim unfinished.
    let unfinished = context.claims.iter().any(|claim| {
        claim.counted_records.iter().any(|record| !record.complete)
            || claim.compared_records.iter().any(|record| {
                !record.complete
                    && !claim.compared_records.iter().any(|other| {
                        other.complete
                            && other.requirement == record.requirement
                            && other.scope == record.scope
                            && record.record_indices.iter().all(|index| other.record_indices.contains(index))
                    })
            })
    });
    if unfinished {
        return reject(checks, UNFINISHED_REPAIR);
    }
    true
}

const OMITTED: &str = "requirement_omitted";

async fn ask(
    jev: &dyn DecisionClient,
    composed: &ComposedAnswer,
    questions: &IndexMap<String, NoulQuestion>,
    ledger: Option<&Ledger>,
    tokens: &TokenBudget,
) -> Result<IndexMap<String, Answer>> {
    let asked: IndexMap<String, Question> = questions.iter().map(|(k, q)| (k.clone(), q.clone().into())).collect();
    let answered =
        evaluate_batches(jev, &json!({"answer": composed.answer}), &asked, tokens, ledger, None, false).await?;
    Ok(answered.map(|answered| answered.answers).unwrap_or_default())
}

/// The answer without any claim a check doubts, or None when a requirement is omitted from what is left or the pages
/// where the run committed an action contradict it.
///
/// The composer adds claims its quotes do not cover (a login page's nav links, a Log out button), and one of those
/// failed three runs in four of a correct sign-in answer. Removing a doubted claim asserts nothing new, so it is honest
/// as long as the rest still answers: the omission check is asked again of what remains.
pub async fn check_claims(
    jev: &dyn DecisionClient,
    composed: &ComposedAnswer,
    notes: &Notes,
    thresholds: &Thresholds,
    options: &CheckOptions<'_>,
) -> Result<Option<ComposedAnswer>> {
    let tokens = &options.tokens;
    let ledger = options.ledger;
    let answer_checks = &options.answer_checks;
    let mut questions = claim_check_questions(composed, notes, tokens, true, answer_checks)?;
    // An action-only task can finish without factual claims. Its completion was checked already, and Jev rejects an
    // empty question batch; dropped or uncited answer text still cannot pass.
    if questions.is_empty() && answer_checks.is_empty() {
        return Ok((composed.answer.is_empty() && composed.dropped_claims == 0).then(|| composed.clone()));
    }
    let transaction = transaction_check_question(composed, notes, &options.transaction_evidence_ids, tokens);
    let claims = async {
        let Some(transaction) = &transaction else { return ask(jev, composed, &questions, ledger, tokens).await };
        // Asked apart so the committed pages cannot take the omission check's notes budget.
        let checked = IndexMap::from([(TRANSACTION_CONTRADICTED.to_string(), transaction.clone())]);
        // Both asks finish before either failure propagates, so neither is billed after the check has returned.
        let (claimed, committed) =
            join2(ask(jev, composed, &questions, ledger, tokens), ask(jev, composed, &checked, ledger, tokens)).await;
        let mut claimed = claimed?;
        claimed.extend(committed?);
        Ok(claimed)
    };
    let verified_outputs: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let outputs_options = CheckOptions {
        tokens: tokens.clone(),
        ledger,
        missing_outputs: options.missing_outputs,
        corrections: options.corrections,
        verified_outputs: Some(&verified_outputs),
        independent_fields: options.independent_fields,
        trace: options.trace.clone(),
        ..CheckOptions::default()
    };
    let outputs_supported = check_answer_outputs(composed, notes, answer_checks, &outputs_options);
    let answers: IndexMap<String, Answer> = claims.await?;
    if let Some(transaction) = transaction {
        questions.insert(TRANSACTION_CONTRADICTED.into(), transaction);
    }
    if questions.keys().any(|key| !matches!(answers.get(key), Some(Answer::Noul(_)))) {
        return Ok(None);
    }
    let limit = thresholds.claim_problem_above;
    options.trace.event("claims", || {
        let mut fields = Map::new();
        fields.insert("limit".into(), limit.into());
        fields.insert(
            "scores".into(),
            Value::Object(
                questions.keys().map(|key| (key.clone(), json!(round(probability(&answers, key), 3)))).collect(),
            ),
        );
        fields.insert("cited".into(), json!(composed.claims.iter().map(|c| &c.evidence_ids).collect::<Vec<_>>()));
        fields.insert("dropped".into(), composed.dropped_claims.into());
        fields
    });
    if composed.dropped_claims != 0
        || probability(&answers, OMITTED).max(probability(&answers, TRANSACTION_CONTRADICTED)) > limit
    {
        return Ok(None);
    }
    let kept: Vec<Claim> = composed
        .claims
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            probability(&answers, &format!("unsupported_{index}"))
                .max(probability(&answers, &format!("contradicted_{index}")))
                <= limit
        })
        .map(|(_, claim)| claim.clone())
        .collect();
    let unstated: Vec<String> = answer_checks
        .iter()
        .enumerate()
        .filter(|(index, _)| probability(&answers, &format!("{OUTPUT_MISSING}{index}")) > limit)
        .map(|(_, check)| check.clone())
        .collect();
    if let (Some(verified_fields), Some(output_claims)) = (options.verified_fields, &composed.output_claims) {
        // Fields Jev found stated and supported are kept even when a sibling is missing, so the next compose rewrites
        // only what failed.
        let verified = locked(&verified_outputs).clone();
        let outputs: IndexMap<&String, &Vec<usize>> = output_claims
            .iter()
            .filter(|(check, indices)| {
                verified.contains(check)
                    && !unstated.contains(check)
                    && !indices.is_empty()
                    && indices.iter().all(|i| kept.contains(&composed.claims[*i]))
            })
            .collect();
        let mut accepted: IndexSet<Claim> = IndexSet::new();
        for indices in outputs.values() {
            accepted.extend(indices.iter().map(|i| composed.claims[*i].clone()));
        }
        if !accepted.is_empty() {
            let accepted: Vec<Claim> = accepted.into_iter().collect();
            let base = assemble_answer(&accepted, notes, &composed.requirements, 0);
            let remapped = outputs
                .iter()
                .map(|(check, indices)| {
                    let at = indices
                        .iter()
                        .map(|i| accepted.iter().position(|c| *c == composed.claims[*i]).unwrap_or_default())
                        .collect();
                    ((*check).clone(), at)
                })
                .collect();
            locked(verified_fields).push(base.with_outputs(remapped));
        }
    }
    if !unstated.is_empty() {
        // Named, so recovery looks for the missing output and the composer is told what to add.
        if let Some(missing) = options.missing_outputs {
            extend_new(missing, &unstated);
        }
        if let Some(corrections) = options.corrections {
            correct(corrections, &unstated, &composed.claims, UNSTATED_REPAIR);
        }
        return Ok(None);
    }
    if !outputs_supported {
        return Ok(None);
    }
    if kept.len() == composed.claims.len() {
        return Ok(Some(composed.clone()));
    }
    // An action-only task needs no answer: the done check judged its completion, so a restatement of the action that
    // its confirmation quote cannot carry is dropped, not fatal.
    if kept.is_empty() && composed.requirements.iter().any(|r| r.kind == RequirementKind::Information) {
        return Ok(None);
    }
    // A claim can be doubted for proving too little (a title cited as "the most expensive" without the prices it
    // beat), and the omission check then passed the price alone as the whole answer. A requirement the answer cited
    // evidence for and no longer does is omitted, whatever the check says, so the composer writes it again.
    let cited: HashSet<&String> = kept.iter().flat_map(|c| &c.evidence_ids).collect();
    let was_cited: HashSet<&String> = composed.claims.iter().flat_map(|c| &c.evidence_ids).collect();
    for requirement in &composed.requirements {
        let supporting: HashSet<String> = notes.supporting(&requirement.id).into_iter().map(|(key, _)| key).collect();
        if supporting.iter().any(|k| was_cited.contains(k)) && !supporting.iter().any(|k| cited.contains(k)) {
            return Ok(None);
        }
    }
    let mut pruned = assemble_answer(&kept, notes, &composed.requirements, 0);
    if let Some(output_claims) = &composed.output_claims {
        let remapped = output_claims
            .iter()
            .map(|(check, indices)| {
                let at = indices.iter().filter_map(|i| kept.iter().position(|c| *c == composed.claims[*i])).collect();
                (check.clone(), at)
            })
            .collect();
        pruned = pruned.with_outputs(remapped);
    }
    if !answer_checks.is_empty() {
        let again = CheckOptions {
            tokens: tokens.clone(),
            ledger,
            missing_outputs: options.missing_outputs,
            corrections: options.corrections,
            independent_fields: options.independent_fields,
            trace: options.trace.clone(),
            ..CheckOptions::default()
        };
        if !check_answer_outputs(&pruned, notes, answer_checks, &again) {
            return Ok(None);
        }
    }
    let mut omission: IndexMap<String, NoulQuestion> =
        claim_check_questions(&pruned, notes, tokens, true, answer_checks)?
            .into_iter()
            .filter(|(key, _)| key == OMITTED || key.starts_with(OUTPUT_MISSING))
            .collect();
    // A grouped requirement can retain citations while pruning one of its requested fields. Check each removed claim
    // against the remaining answer without notes supplying the missing output.
    let information: Vec<String> = composed
        .requirements
        .iter()
        .filter(|r| r.kind == RequirementKind::Information)
        .map(|r| r.dump_json())
        .collect();
    if !information.is_empty() {
        let requirements = information.join("\n");
        for (index, claim) in composed.claims.iter().enumerate() {
            if kept.contains(claim) {
                continue;
            }
            omission.insert(
                format!("removed_output_{index}"),
                NoulQuestion::new(
                    prompts::removed(&requirements, &pruned.answer, &claim.text),
                    Some("Yes, a requested output is missing after this removal."),
                    Some("No, the remaining answer still states every output affected by this removal."),
                ),
            );
        }
    }
    if !omission.is_empty() {
        let answers = ask(jev, &pruned, &omission, ledger, tokens).await?;
        // Removing a requested field cannot be waved through on an uncertain coverage decision.
        let failed = omission.keys().any(|key| {
            let bound = if key == OMITTED || key.starts_with(OUTPUT_MISSING) { limit } else { limit.min(1.0 - limit) };
            !matches!(answers.get(key), Some(Answer::Noul(_))) || probability(&answers, key) > bound
        });
        if failed {
            return Ok(None);
        }
    }
    Ok(Some(pruned))
}

/// The record a caller asked for, as `extract` reads it: each field by name as retrieval copies it, and the model
/// whose validation the extracted values must pass.
#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub fields: IndexMap<String, ScalarField>,
    pub model: OutputModel,
}

impl RecordSchema {
    /// The record an output schema describes, its fields under their Python names.
    pub fn from_output(model: OutputModel) -> RecordSchema {
        let fields = model.fields().iter().map(|field| (field.name.clone(), ScalarField::from_output(field))).collect();
        RecordSchema { fields, model }
    }
}

/// `extract`'s keyword arguments.
#[derive(Default)]
pub struct ExtractOptions<'a> {
    pub notes: Option<&'a Notes>,
    pub tokens: TokenBudget,
    pub ledger: Option<&'a Ledger>,
}

/// Text fields are proposed by the LLM and kept only when quoted verbatim from the page; other scalars are copied from
/// the typed spans Jev points at. A field with no supported value fails the extraction.
pub async fn extract(
    jev: &dyn DecisionClient,
    llm: &dyn LLMClient,
    task: &str,
    capture: &Capture,
    schema: &RecordSchema,
    options: &ExtractOptions<'_>,
) -> Result<Extraction> {
    use crate::retrieval::{Annotation, READ_CHUNK_CHARS};
    let mut values: Map<String, Value> = Map::new();
    let mut evidence: Vec<Evidence> = Vec::new();
    let text_fields: IndexMap<String, ScalarField> = schema
        .fields
        .iter()
        .filter(|(_, f)| f.annotation == Annotation::Str)
        .map(|(k, f)| (k.clone(), f.clone()))
        .collect();
    if !text_fields.is_empty() {
        // Notes first: they hold every page a comparison read, where the final page shows one side of it.
        let mut proposed = match options.notes {
            Some(notes) => {
                propose_text_fields_from_notes(
                    llm,
                    task,
                    notes,
                    &text_fields,
                    Some(capture),
                    &options.tokens,
                    options.ledger,
                )
                .await?
            }
            None => IndexMap::new(),
        };
        let unseen: IndexMap<String, ScalarField> = text_fields
            .iter()
            .filter(|(name, _)| !proposed.contains_key(*name))
            .map(|(k, f)| (k.clone(), f.clone()))
            .collect();
        if !unseen.is_empty() {
            let (found, _) =
                propose_text_fields(llm, task, capture, &unseen, READ_CHUNK_CHARS, &options.tokens, options.ledger)
                    .await?;
            proposed.extend(found);
        }
        for (name, (value, quoted)) in proposed {
            values.insert(name, value.into());
            evidence.push(quoted);
        }
    }
    let record_fields: Vec<String> = schema.fields.keys().cloned().collect();
    for (name, field) in &schema.fields {
        if text_fields.contains_key(name) {
            continue;
        }
        let mut candidates = match field_candidates(capture, field) {
            Ok(candidates) => candidates,
            Err(UnsupportedField { reason }) => {
                return Ok(Extraction { data: None, evidence: Vec::new(), problem: Some(format!("{name}: {reason}")) });
            }
        };
        if let Some(notes) = options.notes {
            // The notes hold every page the run read; the final capture holds only the one it ended on, which for a
            // sorted comparison is one side of it. Both pages are offered, the current one first.
            match field_candidates_from_notes(notes, field, Some(capture)) {
                Ok(quoted) => candidates = merge_candidates(&[&candidates, &quoted]),
                Err(UnsupportedField { reason }) => {
                    return Ok(Extraction {
                        data: None,
                        evidence: Vec::new(),
                        problem: Some(format!("{name}: {reason}")),
                    });
                }
            }
        }
        if candidates.is_empty() {
            continue;
        }
        let state = json!({"task": task, "page": {"url": capture.url, "title": capture.title}});
        match choose_candidate(jev, &state, field, &candidates, name, task, &record_fields, options.ledger).await {
            Ok(Some((value, quoted))) => {
                values.insert(name.clone(), value.to_json());
                evidence.push(quoted);
            }
            Ok(None) => {}
            Err(AgentError::Invalid(error)) => {
                return Ok(Extraction { data: None, evidence, problem: Some(format!("{name}: {error}")) });
            }
            Err(other) => return Err(other),
        }
    }
    // A default the page never showed is the caller's placeholder, not data; only None may stand for absent.
    let unsupported: Vec<&String> = schema
        .fields
        .iter()
        .filter(|(name, field)| !values.contains_key(*name) && !field.required && !field.default.is_null())
        .map(|(name, _)| name)
        .collect();
    if !unsupported.is_empty() {
        let names: Vec<&str> = unsupported.iter().map(|n| n.as_str()).collect();
        return Ok(Extraction {
            data: None,
            evidence,
            problem: Some(format!("no value on the page for {}", names.join(", "))),
        });
    }
    match schema.model.validate(&Value::Object(values)) {
        Ok(data) => Ok(Extraction { data: Some(data), evidence, problem: None }),
        Err(error) => {
            Ok(Extraction { data: None, evidence, problem: Some(text::prefix(&error.to_string(), 500).to_string()) })
        }
    }
}

#[cfg(test)]
mod tests {
    //! What the golden replay cannot show on its own: the helpers' exact text, and that a failure beside a check never
    //! leaves a request unsent.
    use super::*;
    use crate::support::testing::block_on;
    use fastbrowse_core::models::{Operation, StepOutcome};

    fn entry(
        operation: Option<Operation>,
        target: Option<&str>,
        typed: Option<&str>,
        effect: Option<&str>,
    ) -> HistoryEntry {
        HistoryEntry {
            operation,
            target: target.map(str::to_string),
            outcome: StepOutcome::Executed,
            page_changed: false,
            note: None,
            text: typed.map(str::to_string),
            effect: effect.map(str::to_string),
            read_progress: false,
            setting: None,
        }
    }

    #[test]
    fn a_step_reads_as_python_writes_it() {
        let filled = entry(Some(Operation::Fill), Some("Name"), Some("O'Neil"), Some("value set"));
        assert_eq!(step(&filled), "- fill Name = \"O'Neil\" -> executed (value set)");
        assert_eq!(step(&entry(None, None, None, None)), "- open -> executed");
    }

    #[test]
    fn the_cache_key_digests_every_image() {
        let messages =
            vec![Message::system("a"), Message { images: vec![Bytes(b"png".to_vec())], ..Message::user("b") }];
        assert_eq!(verify_cache_key(&messages), "6702eeba981cf84a9e9ccead2ff7cae522f4d64fbd56fc65b4a76e24c7ece4d1");
    }

    #[test]
    fn a_failure_beside_a_sibling_waits_for_it() {
        // gather(return_exceptions=True): the sibling that needs two polls still finishes after the other failed.
        let finished = std::sync::atomic::AtomicBool::new(false);
        let slow = async {
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if yielded {
                    return std::task::Poll::Ready(());
                }
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            })
            .await;
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<(), AgentError>(())
        };
        let failing = async { Err::<(), AgentError>(AgentError::Invalid("no".into())) };
        let (first, second) = block_on(join2(failing, slow));
        assert!(first.is_err() && second.is_ok());
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    // -- tests/test_accuracy_fixes.py: a browsing step cannot outvote an evidenced answer ------------------------

    #[test]
    fn only_browsing_steps_are_instrumental() {
        // A browsing step is how the run reaches the information. Anything that changes state on the site is what
        // the caller asked for, so it keeps blocking however good the evidence is.
        for (text, traversal) in [
            ("Navigate through every page of the site.", true),
            ("Browse the Sequential Art category, including all of its pages.", true),
            ("Scroll through the listing until every result has been shown.", true),
            ("Sign in with username standard_user and password secret_sauce.", false),
            ("Add both products to the cart and place the order.", false),
            ("Submit the contact form with the prepared message.", false),
            ("Go through every page and submit the last page's form.", false),
            ("Report the country with the 12th-highest GDP.", false),
        ] {
            assert_eq!(traversal_only(text), traversal, "{text}");
        }
    }

    /// `_Jev` in tests/test_done_check.py: answers each question it is asked from a table of probabilities.
    struct TableJev(Vec<(&'static str, f64)>);

    impl DecisionClient for TableJev {
        fn evaluate<'a>(
            &'a self,
            _: &'a Value,
            questions: &'a IndexMap<String, Question>,
        ) -> fastbrowse_core::BoxFuture<
            'a,
            std::result::Result<fastbrowse_core::decision::Evaluation, fastbrowse_core::decision::DecisionError>,
        > {
            let answers: Map<String, Value> = self
                .0
                .iter()
                .filter(|(key, _)| questions.contains_key(*key))
                .map(|(key, p)| (key.to_string(), json!({"type": "noul", "probability": p})))
                .collect();
            let evaluation = json!({"model": "test", "answers": answers, "input_tokens": 1,
                "cost": {"component": "jev", "basis": "estimated", "dollars": 0.0}});
            Box::pin(async move { Ok(serde_json::from_value(evaluation).expect("evaluation")) })
        }
    }

    fn count_plan() -> Plan {
        serde_json::from_value(json!({
            "requirements": [
                {"id": "r1", "text": "Navigate through every page of the site.", "kind": "action"},
                {"id": "r2", "text": "Count the quotes tagged love.", "kind": "information"},
            ],
            "answer_expected": true,
        }))
        .expect("plan")
    }

    fn counted_notes() -> Notes {
        let total = "14 quotes are tagged love";
        let evidence: Evidence = serde_json::from_value(json!({
            "source_id": "s0", "url": "https://example.test", "frame_id": null,
            "captured_at": "2026-01-01T00:00:00Z", "capture_sha256": "love", "start": 0, "end": total.len(),
            "quote": total,
        }))
        .expect("evidence");
        Notes::new([crate::memory::Fact::read(Some("r2"), total, evidence, fastbrowse_core::models::FactReader::Llm)])
    }

    fn page() -> Observation {
        serde_json::from_value(json!({
            "url": "https://example.test/encode/httpx", "title": "encode/httpx", "page_key": "k",
            "captured_at": "2026-01-01T00:00:00Z", "controls": [], "omitted_controls": 0,
            "viewport_text": "encode/httpx", "tabs": [],
        }))
        .expect("observation")
    }

    fn done(answers: Vec<(&'static str, f64)>, notes: &Notes) -> DoneCheck {
        let jev = TableJev(answers);
        let options = DoneOptions::default();
        block_on(check_done(&jev, "Count them.", &count_plan(), &page(), notes, &Thresholds::default(), None, &options))
            .expect("check")
    }

    #[test]
    fn a_page_whose_state_is_a_picture_is_judged_with_the_screenshot() {
        // A chart, picture or drawing is offered as content, not as a control, so the control list cannot see it.
        // Only the snapshot's own read of the viewport can, and the verifier needs the screenshot for it.
        let mut observation = page();
        observation.viewport_text =
            "a long stretch of article prose that no one could mistake for a picture on the page".into();
        assert!(!visual_only_state(&observation));
        observation.visual_content = true;
        assert!(visual_only_state(&observation));
    }

    #[test]
    fn an_unconfirmed_browsing_step_does_not_hold_an_evidenced_answer() {
        // Every information requirement is evidenced; only the visit is unconfirmed, and the page is doubted as a
        // whole. The tag count was read off the site's own exhaustive listing.
        let check = done(vec![("complete", 0.63), ("unmet_r1", 0.9), ("unmet_r2", 0.1)], &counted_notes());
        assert_eq!(check.verdict, DoneVerdict::Accept);
    }

    #[test]
    fn an_unevidenced_answer_still_keeps_the_run_working() {
        // The traversal exemption is for a run that has read what it was asked for, never for one that has not.
        assert_eq!(done(vec![("complete", 0.9)], &Notes::default()).verdict, DoneVerdict::Reject);
    }
}
