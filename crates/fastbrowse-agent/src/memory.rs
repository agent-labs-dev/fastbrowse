//! Small evidence-backed memory with stable citation ids and explicit truncation.
//!
//! Every map beside the facts is an `IndexMap`, so iteration follows insertion order, and each set is only ever asked
//! for membership or sorted first.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash::Hash;

use fastbrowse_core::models::{Evidence, FactReader};
use fastbrowse_core::page::Capture;
use fastbrowse_core::py::datetime::DateTime;
use fastbrowse_core::py::dump::Dump;
use fastbrowse_core::py::json::{dumped_len, dumps, write_string};
use fastbrowse_core::py::{lax, sha256_hex, text};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{AgentError, Result};
use crate::planner::{Plan, Requirement};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tally {
    pub requirement_id: String,
    pub key: String,
    /// Citation ids of distinct records assigned to this key, never a model-written total.
    pub records: Vec<String>,
}

impl Tally {
    pub fn count(&self) -> usize {
        self.records.iter().collect::<HashSet<_>>().len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    pub requirement_id: String,
    /// Quoted operands whose collection scope was checked by the reader.
    pub records: Vec<String>,
    #[serde(default)]
    pub complete: bool,
}

/// `Fact`, and its subclass `CollectionFact` when `scope` is set: coverage of a quoted collection, independent of
/// whether it answers every requested field. A collection fact always has a comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "FactFields")]
pub struct Fact {
    pub requirement_id: Option<String>,
    pub tally: Option<Tally>,
    pub comparison: Option<Comparison>,
    pub text: String,
    /// The span the fact was read from; None when a repeated choice, count, total or winner rests on its basis.
    pub evidence: Option<Evidence>,
    /// Fact ids of the sources this choice, count or comparison rests on.
    pub basis: Vec<String>,
    /// Which reader produced the fact; citations are built from it.
    pub reader: FactReader,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FactFields {
    #[serde(default)]
    requirement_id: Option<String>,
    #[serde(default)]
    tally: Option<Tally>,
    #[serde(default)]
    comparison: Option<Comparison>,
    text: String,
    #[serde(deserialize_with = "lax::nullable")]
    evidence: Option<Evidence>,
    #[serde(default)]
    basis: Vec<String>,
    reader: FactReader,
    #[serde(default)]
    scope: Option<String>,
}

impl TryFrom<FactFields> for Fact {
    type Error = String;

    fn try_from(fields: FactFields) -> Result<Self, String> {
        if fields.scope.is_some() && fields.comparison.is_none() {
            return Err("comparison: Input should be a valid dictionary or instance of Comparison".into());
        }
        Fact {
            requirement_id: fields.requirement_id,
            tally: fields.tally,
            comparison: fields.comparison,
            text: fields.text,
            evidence: fields.evidence,
            basis: fields.basis,
            reader: fields.reader,
            scope: fields.scope,
        }
        .grounded()
    }
}

impl Fact {
    /// A fact read from one span.
    pub fn read(requirement_id: Option<&str>, text: &str, evidence: Evidence, reader: FactReader) -> Fact {
        Fact {
            requirement_id: requirement_id.map(str::to_string),
            tally: None,
            comparison: None,
            text: text.into(),
            evidence: Some(evidence),
            basis: Vec::new(),
            reader,
            scope: None,
        }
    }

    /// A fact concluded from others; fails as the model validator does when `basis` is empty.
    pub fn derived(requirement_id: Option<&str>, text: &str, basis: Vec<String>, reader: FactReader) -> Result<Fact> {
        Fact {
            requirement_id: requirement_id.map(str::to_string),
            tally: None,
            comparison: None,
            text: text.into(),
            evidence: None,
            basis,
            reader,
            scope: None,
        }
        .grounded()
        .map_err(AgentError::Invalid)
    }

    /// The model validator: a fact with no evidence must be derived from a basis.
    pub fn grounded(self) -> Result<Fact, String> {
        if self.evidence.is_none() && self.basis.is_empty() {
            return Err("Value error, a fact with no evidence must be derived from a basis".into());
        }
        Ok(self)
    }

    /// `isinstance(fact, CollectionFact)`.
    pub fn is_collection(&self) -> bool {
        self.scope.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedNotes {
    pub text: String,
    pub evidence_ids: Vec<String>,
}

/// `json.dumps(text)`, or with `ensure_ascii=False` when `ascii` is off.
fn quoted(text: &str, ascii: bool) -> String {
    let mut out = String::new();
    write_string(&mut out, text, ascii);
    out
}

/// Whether `text` still shows `quote` as whole words: "Priya Sharma" is inside the corrected "Priya Sharman".
///
/// `re.search(rf"(?<!\w){re.escape(quote)}(?!\w)", text)`, trying every start as the regex does.
pub fn shows(text: &str, quote: &str) -> bool {
    let mut previous: Option<char> = None;
    for (at, c) in text.char_indices().map(|(at, c)| (at, Some(c))).chain([(text.len(), None)]) {
        if !previous.is_some_and(text::is_word)
            && text[at..].starts_with(quote)
            && !text[at + quote.len()..].chars().next().is_some_and(text::is_word)
        {
            return true;
        }
        previous = c;
    }
    false
}

/// `urlsplit`'s scheme, lowercased, and what follows it, after the leading C0 controls and spaces and every tab and
/// line break are dropped as CPython 3.13 drops them.
pub(crate) fn split_scheme(url: &str) -> (String, String) {
    let url: String =
        url.trim_start_matches(|c| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    if let Some(colon) = url.find(':')
        && colon > 0
        && url.starts_with(|c: char| c.is_ascii_alphabetic())
        && url[..colon].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return (url[..colon].to_ascii_lowercase(), url[colon + 1..].to_string());
    }
    (String::new(), url)
}

/// `urlsplit(url)._replace(fragment="").geturl()`, as CPython 3.13 splits and joins.
///
/// An evidence id hashes this address, and ids reach every prompt that shows notes, so it is CPython's reading of
/// the address rather than WHATWG's: a recorded prompt holds the id Python computed.
pub(crate) fn address(url: &str) -> String {
    let (scheme, cleaned) = split_scheme(url);
    let mut rest = cleaned.as_str();
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        rest = &after[end..];
    }
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut joined = String::new();
    const USES_NETLOC: [&str; 27] = [
        "",
        "ftp",
        "http",
        "gopher",
        "nntp",
        "telnet",
        "imap",
        "wais",
        "file",
        "mms",
        "https",
        "shttp",
        "snews",
        "prospero",
        "rtsp",
        "rtsps",
        "rtspu",
        "rsync",
        "svn",
        "svn+ssh",
        "sftp",
        "nfs",
        "git",
        "git+ssh",
        "ws",
        "wss",
        "itms-services",
    ];
    if !netloc.is_empty() {
        joined.push_str("//");
        joined.push_str(netloc);
        if !path.is_empty() && !path.starts_with('/') {
            joined.push('/');
        }
    } else if path.starts_with("//")
        || (!scheme.is_empty() && USES_NETLOC.contains(&scheme.as_str()) && (path.is_empty() || path.starts_with('/')))
    {
        joined.push_str("//");
    }
    joined.push_str(path);
    let mut out = String::new();
    if !scheme.is_empty() {
        out.push_str(&scheme);
        out.push(':');
    }
    out.push_str(&joined);
    if !query.is_empty() {
        out.push('?');
        out.push_str(query);
    }
    out
}

fn digest16(text: &str) -> String {
    sha256_hex(text.as_bytes())[..16].to_string()
}

pub fn evidence_id(evidence: &Evidence) -> String {
    // Equal text on different pages must not inherit the first page's citation or record identity.
    let address = digest16(&address(&evidence.url));
    let mut identity = format!("{}:{}:{}:{address}", evidence.capture_sha256, evidence.start, evidence.end);
    if let Some(control) = &evidence.control_context {
        // Equal captions on different controls must not inherit one another's structural association.
        let context = digest16(&dumps(&json!([evidence.frame_id, evidence.source_id, control.dump()])));
        identity.push(':');
        identity.push_str(&context);
    }
    identity
}

/// A read fact is keyed by its span; a derived one by what it concludes from which facts.
pub fn fact_id(fact: &Fact) -> String {
    if let Some(tally) = &fact.tally {
        return format!("tally:{}", digest16(&dumps(&json!([tally.requirement_id, tally.key]))));
    }
    if let Some(evidence) = &fact.evidence {
        return evidence_id(evidence);
    }
    let mut basis = fact.basis.clone();
    basis.sort();
    let mut parts = vec![Value::from(fact.text.clone()), json!(basis)];
    // One collection can cover two or more requirements; each keeps its own coverage fact.
    if let (true, Some(comparison)) = (fact.is_collection(), &fact.comparison) {
        parts.push(Value::from(comparison.requirement_id.clone()));
    }
    format!("derived:{}", digest16(&dumps(&Value::Array(parts))))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPage {
    pub url: String,
    pub title: String,
}

/// A requirement-scoped row identity: requirement, address without its fragment, squashed quote.
pub type RecordIdentity = (String, String, String);

#[derive(Clone, Default)]
pub struct Notes {
    facts: IndexMap<String, Fact>,
    requirements: IndexMap<String, BTreeSet<String>>,
    owners: HashMap<String, BTreeSet<String>>,
    retired: HashSet<String>,
    tally_records: HashSet<String>,
    record_ids: IndexMap<RecordIdentity, Vec<IndexMap<String, String>>>,
    continuation_records: IndexMap<String, HashSet<String>>,
    pages: IndexMap<(String, String, DateTime), CapturedPage>,
}

/// Everything a `Notes` holds, in the golden fixtures' form, so a recorded call can start from recorded notes. Sets
/// are sorted; maps keep insertion order.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotesState {
    pub facts: Vec<(String, Fact)>,
    pub requirements: Vec<(String, Vec<String>)>,
    pub owners: Vec<(String, Vec<String>)>,
    pub retired: Vec<String>,
    pub tally_records: Vec<String>,
    pub record_ids: Vec<(RecordIdentity, Vec<IndexMap<String, String>>)>,
    pub continuation_records: Vec<(String, Vec<String>)>,
    pub pages: Vec<((String, String, DateTime), CapturedPage)>,
}

impl fmt::Debug for Notes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notes").field("facts", &self.facts).field("requirements", &self.requirements).finish()
    }
}

impl Notes {
    pub fn new(facts: impl IntoIterator<Item = Fact>) -> Notes {
        let mut notes = Notes::default();
        for fact in facts {
            notes.add(fact);
        }
        notes
    }

    #[doc(hidden)]
    pub fn from_state(state: NotesState) -> Notes {
        let sorted = |values: Vec<String>| values.into_iter().collect::<HashSet<_>>();
        Notes {
            facts: state.facts.into_iter().collect(),
            requirements: state.requirements.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(),
            owners: state.owners.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(),
            retired: sorted(state.retired),
            tally_records: sorted(state.tally_records),
            record_ids: state.record_ids.into_iter().collect(),
            continuation_records: state
                .continuation_records
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().collect()))
                .collect(),
            pages: state.pages.into_iter().collect(),
        }
    }

    #[doc(hidden)]
    pub fn state(&self) -> NotesState {
        let sorted = |values: &HashSet<String>| {
            let mut values: Vec<String> = values.iter().cloned().collect();
            values.sort();
            values
        };
        NotesState {
            facts: self.facts.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            requirements: self.requirements.iter().map(|(k, v)| (k.clone(), v.iter().cloned().collect())).collect(),
            owners: {
                // A HashMap has no order to keep, so owners are listed by fact id; compare Python's sorted too.
                let mut owners: Vec<(String, Vec<String>)> =
                    self.owners.iter().map(|(k, v)| (k.clone(), v.iter().cloned().collect())).collect();
                owners.sort();
                owners
            },
            retired: sorted(&self.retired),
            tally_records: sorted(&self.tally_records),
            record_ids: self.record_ids.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            continuation_records: self.continuation_records.iter().map(|(k, v)| (k.clone(), sorted(v))).collect(),
            pages: self.pages.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }
    }

    fn requirements_of(&self, key: &str) -> Option<&BTreeSet<String>> {
        self.requirements.get(key)
    }

    fn answers_any(&self, key: &str) -> bool {
        self.requirements_of(key).is_some_and(|ids| !ids.is_empty())
    }

    fn requirement_list(&self, key: &str) -> String {
        let ids: Vec<&str> =
            self.requirements_of(key).map_or_else(Vec::new, |ids| ids.iter().map(String::as_str).collect());
        if ids.is_empty() { "-".into() } else { ids.join(",") }
    }

    pub fn remember_capture(&mut self, capture: &Capture) {
        // A title can change while the quoted body does not; recapturing must not rename older evidence.
        self.pages
            .entry((capture.sha256.clone(), capture.url.clone(), capture.captured_at))
            .or_insert_with(|| CapturedPage { url: capture.url.clone(), title: capture.title.clone() });
    }

    pub fn captured_page(&self, evidence: &Evidence) -> Option<&CapturedPage> {
        self.pages.get(&(evidence.capture_sha256.clone(), evidence.url.clone(), evidence.captured_at))
    }

    pub fn facts(&self) -> Vec<&Fact> {
        self.facts.values().collect()
    }

    /// Facts by id, in read order.
    pub fn facts_by_id(&self) -> &IndexMap<String, Fact> {
        &self.facts
    }

    pub fn evidence(&self) -> IndexMap<String, Evidence> {
        self.facts
            .iter()
            .filter_map(|(key, fact)| fact.evidence.as_ref().map(|evidence| (key.clone(), evidence.clone())))
            .collect()
    }

    /// The quotes anything may build on: the current ones.
    pub fn usable_evidence(&self) -> IndexMap<String, Evidence> {
        self.facts
            .iter()
            .filter(|(key, _)| self.current(key))
            .filter_map(|(key, fact)| fact.evidence.as_ref().map(|evidence| (key.clone(), evidence.clone())))
            .collect()
    }

    /// The usable evidence a later read has not superseded, and a quote the given capture still shows there.
    ///
    /// A superseded fact stays as context but is not a current value, so offering it would let the extraction copy a
    /// price the final page corrected; a quote from another page still stands.
    pub fn current_evidence(&self, capture: Option<&Capture>) -> IndexMap<String, Evidence> {
        let current = self.usable_evidence();
        let Some(capture) = capture else { return current };
        let page = address(&capture.url);
        current
            .into_iter()
            .filter(|(_, evidence)| address(&evidence.url) != page || shows(&capture.text, &evidence.quote))
            .collect()
    }

    pub fn derived(&self, key: &str) -> bool {
        self.facts.get(key).is_some_and(|fact| fact.evidence.is_none())
    }

    /// Retired facts remain history, including derived facts whose original quotes remain in context.
    pub fn current(&self, key: &str) -> bool {
        self.facts
            .get(key)
            .is_some_and(|fact| !self.retired.contains(key) && (fact.requirement_id.is_none() || self.answers_any(key)))
    }

    pub fn tallies(&self) -> Vec<Tally> {
        let mut tallies: Vec<Tally> = self.facts.values().filter_map(|fact| fact.tally.clone()).collect();
        tallies.sort_by(|a, b| {
            (&a.requirement_id, std::cmp::Reverse(a.count()), &a.key).cmp(&(
                &b.requirement_id,
                std::cmp::Reverse(b.count()),
                &b.key,
            ))
        });
        tallies
    }

    pub fn add_tally(&mut self, tally: Tally) -> Result<Fact> {
        let mut records: Vec<String> = Vec::new();
        for key in &tally.records {
            let fact = self.facts.get(key).ok_or_else(|| AgentError::Invalid(format!("'{key}'")))?;
            let Some(evidence) = fact.evidence.clone() else {
                return Err(AgentError::Invalid("a tally record must cite a captured span".into()));
            };
            // Equal quotes on different addresses can be distinct rows; only recaptures share an identity.
            let identity = (tally.requirement_id.clone(), address(&evidence.url), text::squash(&evidence.quote));
            let sha = evidence.capture_sha256.clone();
            let occurrences = self.record_ids.entry(identity.clone()).or_default();
            let position = occurrences
                .iter()
                .position(|item| item.get(&sha) == Some(key))
                .or_else(|| occurrences.iter().position(|item| !item.contains_key(&sha)))
                .unwrap_or_else(|| {
                    occurrences.push(IndexMap::new());
                    occurrences.len() - 1
                });
            occurrences[position].insert(sha, key.clone());
            // One row read twice names one record: its first reading.
            let canonical =
                self.record_ids[&identity][position].values().next().cloned().unwrap_or_else(|| key.clone());
            records.push(canonical);
            self.tally_records.insert(key.clone());
        }
        let probe = Fact {
            requirement_id: None,
            tally: Some(tally.clone()),
            comparison: None,
            text: String::new(),
            evidence: None,
            basis: records.clone(),
            reader: FactReader::Llm,
            scope: None,
        }
        .grounded()
        .map_err(AgentError::Invalid)?;
        let key = fact_id(&probe);
        let carried: Vec<String> = self.facts.get(&key).map_or_else(Vec::new, |previous| previous.basis.clone());
        let records = unique(carried.into_iter().chain(records));
        let tally = Tally { records: records.clone(), ..tally };
        let fact =
            Fact { text: format!("{}: {}", tally.key, tally.count()), basis: records, tally: Some(tally), ..probe };
        let requirement_id = tally_requirement(&fact);
        self.unevidence([requirement_id.as_str()]);
        self.facts.insert(key.clone(), fact.clone());
        self.requirements.entry(key).or_default();
        Ok(fact)
    }

    pub fn add_continuation(&mut self, requirement_id: &str, record_id: &str) {
        self.continuation_records.entry(requirement_id.into()).or_default().insert(record_id.into());
    }

    pub fn comparison_records(&self, requirement_id: Option<&str>) -> Vec<String> {
        let records: HashSet<&String> = match requirement_id {
            None => self.continuation_records.values().flatten().collect(),
            Some(id) => self.continuation_records.get(id).map_or_else(HashSet::new, |records| records.iter().collect()),
        };
        self.facts.keys().filter(|key| records.contains(key)).cloned().collect()
    }

    pub fn has_untallied_records(&self, requirement_id: &str) -> bool {
        let tallied: HashSet<&String> = self
            .record_ids
            .iter()
            .filter(|(identity, _)| identity.0 == requirement_id)
            .flat_map(|(_, occurrences)| occurrences.iter().flat_map(|occurrence| occurrence.values()))
            .collect();
        self.continuation_records
            .get(requirement_id)
            .is_some_and(|records| records.iter().any(|r| !tallied.contains(r)))
    }

    pub fn complete_tallies(&mut self, requirement_id: &str) {
        let keys: Vec<String> = self
            .facts
            .iter()
            .filter(|(_, fact)| fact.tally.as_ref().is_some_and(|tally| tally.requirement_id == requirement_id))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.requirements.entry(key.clone()).or_default().insert(requirement_id.into());
            if let Some(fact) = self.facts.get_mut(&key) {
                fact.requirement_id = Some(requirement_id.into());
            }
        }
    }

    /// Whether a new span was added; reused spans still evidence other requirements.
    pub fn add(&mut self, fact: Fact) -> bool {
        let key = fact_id(&fact);
        self.retired.remove(&key);
        self.requirements.entry(key.clone()).or_default();
        if let Some(requirement_id) = &fact.requirement_id {
            self.requirements.entry(key.clone()).or_default().insert(requirement_id.clone());
            self.owners.entry(key.clone()).or_default().insert(requirement_id.clone());
        }
        if let Some(previous) = self.facts.get(&key) {
            let basis = unique(previous.basis.iter().chain(&fact.basis).cloned());
            // A winner can quote a row already collected as context; its answer must survive that reuse.
            let new_kept = previous.requirement_id.is_none() && fact.requirement_id.is_some();
            let kept = if new_kept { &fact } else { previous };
            // One block can answer two requirements (a card's title and its price): the draft answers each
            // requirement with this fact's text, so every requirement's claim must survive the reuse.
            let mut text = kept.text.clone();
            if !new_kept && fact.requirement_id.is_some() && !text.contains(&fact.text) {
                text = format!("{text}\n{}", fact.text);
            }
            // The context claim it was read as stays too: a Phone page line read for the sign-in code, then again as
            // the account email, kept only the email claim, and Jev went back for a code the notes no longer said.
            let quote = previous.evidence.as_ref().map(|evidence| evidence.quote.as_str());
            if new_kept && Some(previous.text.as_str()) != quote && !text.contains(&previous.text) {
                text = format!("{}\n{text}", previous.text);
            }
            let comparison = fact.comparison.clone().or_else(|| kept.comparison.clone());
            let merged = Fact { basis, text, comparison, ..kept.clone() };
            self.facts.insert(key, merged);
            return false;
        }
        self.facts.insert(key, fact);
        true
    }

    pub fn fact_requirements(&self, key: &str) -> Vec<String> {
        self.requirements_of(key).map_or_else(Vec::new, |ids| ids.iter().cloned().collect())
    }

    /// Whether source quotes joined a requested group, without declaring any requirement answered.
    pub fn retain_for<'a>(
        &mut self,
        requirement_ids: impl IntoIterator<Item = &'a str>,
        fact_ids: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let owners: BTreeSet<String> = requirement_ids.into_iter().map(str::to_string).collect();
        let mut added = false;
        for key in self.expand_evidence_ids(fact_ids) {
            if self.facts.get(&key).is_some_and(|fact| fact.evidence.is_some()) {
                let held = self.owners.entry(key).or_default();
                added |= !owners.is_subset(held);
                held.extend(owners.iter().cloned());
            }
        }
        added
    }

    /// Keep a requirement's sources, basis and same-page counterevidence without changing their validity.
    pub fn for_requirements<'a>(
        &self,
        requirement_ids: impl IntoIterator<Item = &'a str>,
        evidence_ids: impl IntoIterator<Item = &'a str>,
    ) -> Notes {
        let wanted: HashSet<&str> = requirement_ids.into_iter().collect();
        let intersects =
            |ids: Option<&BTreeSet<String>>| ids.is_some_and(|ids| ids.iter().any(|id| wanted.contains(id.as_str())));
        let mut roots: Vec<String> = self
            .facts
            .iter()
            .filter(|(key, fact)| {
                intersects(self.owners.get(*key))
                    || intersects(self.requirements_of(key))
                    || fact.tally.as_ref().is_some_and(|tally| wanted.contains(tally.requirement_id.as_str()))
                    || (fact.is_collection()
                        && fact.comparison.as_ref().is_some_and(|c| wanted.contains(c.requirement_id.as_str())))
            })
            .map(|(key, _)| key.clone())
            .collect();
        for requirement in &wanted {
            roots.extend(self.comparison_records(Some(requirement)));
        }
        roots.extend(evidence_ids.into_iter().map(str::to_string));
        let mut kept: HashSet<String> =
            self.expand_evidence_ids(roots.iter().map(String::as_str)).into_iter().collect();
        let evidence = self.evidence();
        let locations: HashSet<(String, Option<String>)> = evidence
            .iter()
            .filter(|(key, _)| kept.contains(*key))
            .map(|(_, source)| (address(&source.url), source.frame_id.clone()))
            .collect();
        // Identity quotes and conflicting values need not have been selected as answers by the reader.
        kept.extend(
            evidence
                .iter()
                .filter(|(_, source)| locations.contains(&(address(&source.url), source.frame_id.clone())))
                .map(|(key, _)| key.clone()),
        );
        let kept: HashSet<String> = self.expand_evidence_ids(kept.iter().map(String::as_str)).into_iter().collect();
        let facts: IndexMap<String, Fact> =
            self.facts.iter().filter(|(key, _)| kept.contains(*key)).map(|(k, f)| (k.clone(), f.clone())).collect();
        Notes {
            requirements: facts
                .keys()
                .map(|key| (key.clone(), self.requirements_of(key).cloned().unwrap_or_default()))
                .collect(),
            owners: facts.keys().map(|key| (key.clone(), self.owners.get(key).cloned().unwrap_or_default())).collect(),
            facts,
            retired: self.retired.intersection(&kept).cloned().collect(),
            tally_records: self.tally_records.intersection(&kept).cloned().collect(),
            continuation_records: self
                .continuation_records
                .iter()
                .map(|(requirement, records)| (requirement.clone(), records.intersection(&kept).cloned().collect()))
                .collect(),
            record_ids: self
                .record_ids
                .iter()
                .filter(|(identity, _)| wanted.contains(identity.0.as_str()))
                .map(|(identity, occurrences)| (identity.clone(), occurrences.clone()))
                .collect(),
            pages: self.pages.clone(),
        }
    }

    pub fn evidenced(&self, requirement_id: &str) -> bool {
        !self.supporting(requirement_id).is_empty()
    }

    /// Reopen requirements whose facts were read off the wrong page, keeping the facts as context.
    ///
    /// A price quoted from a summary a guessed address opened is still worth seeing, but left as the answer it turned
    /// every later click into DONE and every DONE into the same refusal.
    pub fn unevidence<'a>(&mut self, requirement_ids: impl IntoIterator<Item = &'a str>) {
        let dropped: HashSet<&str> = requirement_ids.into_iter().collect();
        for requirements in self.requirements.values_mut() {
            requirements.retain(|id| !dropped.contains(id.as_str()));
        }
    }

    /// Once a read of `url` evidences a requirement, earlier reads of that address whose quote the page no longer shows
    /// stop evidencing it, and stay as context.
    ///
    /// After a date picker's choice was corrected from 31/10 to 28/11, a new read quoted the new date, but the answer
    /// still quoted the old one, since both facts evidenced the requirement. A product page that swaps its name and
    /// price together must not answer with the old name beside the new price either, so a vanished value is retired
    /// whichever requirement it answered.
    pub fn supersede(&mut self, requirement_id: &str, url: &str, sha256: &str, text: &str) {
        let fresh: Vec<Evidence> = self
            .supporting_evidence(requirement_id)
            .into_iter()
            .filter(|evidence| evidence.capture_sha256 == sha256)
            .collect();
        if fresh.is_empty() {
            return;
        }
        // A capture includes child frames whose quotes belong to their own addresses, not the parent page's.
        let mut addresses: HashSet<String> = HashSet::from([address(url)]);
        addresses.extend(fresh.iter().map(|span| address(&span.url)));
        let evidence = self.evidence();
        // Virtualized lists replace visible rows while a tally or comparison still depends on those already read.
        let mut kept: HashSet<String> = self.tally_records.clone();
        kept.extend(self.comparison_records(None));
        // The quote this read cut is on the page by construction. A confirmation claim that drew on the Review step it
        // followed was retired along with that step, and the requirement it had just evidenced was open again.
        let own: Vec<String> = self
            .facts
            .iter()
            .filter(|(_, fact)| {
                fact.tally.is_none() && fact.evidence.as_ref().is_some_and(|e| e.capture_sha256 == sha256)
            })
            .map(|(key, _)| key.clone())
            .collect();
        let vanished = |source: &Evidence| {
            source.capture_sha256 != sha256 && addresses.contains(&address(&source.url)) && !shows(text, &source.quote)
        };
        let mut retired: HashSet<String> = HashSet::new();
        for (key, fact) in &self.facts {
            if kept.contains(key) || own.contains(key) {
                continue;
            }
            let stale = if !fact.basis.is_empty() && fact.tally.is_none() {
                self.expand_evidence_ids([key.as_str()]).iter().filter_map(|source| evidence.get(source)).any(vanished)
            } else {
                fact.evidence.as_ref().is_some_and(vanished)
            };
            if stale {
                // Context quotes can contradict a later value even when they never evidenced a requirement.
                retired.insert(key.clone());
            }
        }
        for key in &retired {
            self.retired.insert(key.clone());
            if let Some(requirements) = self.requirements.get_mut(key) {
                requirements.clear();
            }
        }
        for key in own {
            if let Some(fact) = self.facts.get_mut(&key)
                && fact.basis.iter().any(|source| retired.contains(source))
            {
                // The vanished quote stays in the notes as history; the fresh quote no longer rests on it.
                fact.basis.retain(|source| !retired.contains(source));
            }
        }
    }

    /// The spans read as evidence of a requirement, including those a later read superseded.
    pub fn read_for(&self, requirement_id: &str) -> Vec<Evidence> {
        let mut supporting: Vec<String> = self.supporting(requirement_id).into_iter().map(|(key, _)| key).collect();
        supporting.extend(
            self.facts
                .iter()
                .filter(|(_, fact)| fact.requirement_id.as_deref() == Some(requirement_id))
                .map(|(key, _)| key.clone()),
        );
        let sources: HashSet<String> =
            self.expand_evidence_ids(supporting.iter().map(String::as_str)).into_iter().collect();
        self.facts
            .iter()
            .filter(|(key, _)| sources.contains(*key))
            .filter_map(|(_, fact)| fact.evidence.clone())
            .collect()
    }

    /// A tally is ranked by code; other facts keep the order they were read in.
    pub fn supporting(&self, requirement_id: &str) -> Vec<(String, Fact)> {
        let mut supporting: Vec<(String, Fact)> = self
            .requirements
            .iter()
            .filter(|(_, ids)| ids.contains(requirement_id))
            .filter_map(|(key, _)| Some((key.clone(), self.facts.get(key)?.clone())))
            .collect();
        supporting.sort_by_key(|(_, fact)| std::cmp::Reverse(fact.tally.as_ref().map_or(0, Tally::count)));
        supporting
    }

    pub fn supporting_evidence(&self, requirement_id: &str) -> Vec<Evidence> {
        let roots: Vec<String> = self.supporting(requirement_id).into_iter().map(|(key, _)| key).collect();
        let keys = self.expand_evidence_ids(roots.iter().map(String::as_str));
        keys.iter().filter_map(|key| self.facts.get(key)?.evidence.clone()).collect()
    }

    /// Cited facts and their transitive basis, once each in read order.
    ///
    /// An id no fact has is kept, after the known ones: dropping it would pass a claim whose citation the claim check
    /// must see fail.
    pub fn expand_evidence_ids<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let cited: IndexSet<&str> = keys.into_iter().collect();
        let mut pending: Vec<&str> = cited.iter().copied().collect();
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(key) = pending.pop() {
            if !seen.insert(key) {
                continue;
            }
            if let Some(fact) = self.facts.get(key) {
                pending.extend(fact.basis.iter().map(String::as_str));
            }
        }
        let mut expanded: Vec<String> = self.facts.keys().filter(|key| seen.contains(key.as_str())).cloned().collect();
        expanded.extend(cited.iter().filter(|key| !self.facts.contains_key(**key)).map(|key| key.to_string()));
        expanded
    }

    pub fn unresolved(&self, plan: &Plan) -> Vec<Requirement> {
        plan.requirements.iter().filter(|requirement| !self.evidenced(&requirement.id)).cloned().collect()
    }

    /// Claims across read sources, each with the page text it quotes, for picking the next action and writing a field.
    ///
    /// A claim is model prose and can name a value wrongly; the quote is what the page said. A reader called the Phone
    /// page's "katherine@example.com: 423345" a saved password, and Jev, shown only that, went back to the Phone page
    /// for the code instead of typing it.
    pub fn render_for_navigation(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let mut sources: IndexMap<String, usize> = IndexMap::new();
        let mut ranks: HashMap<Option<&str>, usize> = HashMap::new();
        let mut ranked: Vec<(usize, &String, &Fact)> = Vec::new();
        // Each source's latest claim comes first, so a page read many times does not bury the others.
        for (key, fact) in self.facts.iter().rev() {
            let address = fact.evidence.as_ref().map(|evidence| evidence.url.as_str());
            let rank = ranks.get(&address).copied().unwrap_or(0);
            ranked.push((rank, key, fact));
            ranks.insert(address, rank + 1);
        }
        ranked.sort_by_key(|entry| entry.0);
        for (_, key, fact) in ranked {
            let mut source = String::new();
            if let Some(evidence) = &fact.evidence {
                let address = &evidence.url;
                let number = sources.get(address).copied().unwrap_or(sources.len() + 1);
                source = format!(" source={number}");
                if !sources.contains_key(address) {
                    source.push_str(&format!(" url={}", quoted(address, true)));
                }
            }
            let mut line = format!("{} requirements={}{source}", quoted(&fact.text, false), self.requirement_list(key));
            if let Some(evidence) = fact.evidence.as_ref().filter(|evidence| evidence.quote != fact.text) {
                line = format!("{line} quote={}", quoted(&evidence.quote, false));
            }
            lines.push(line);
            if let Some(evidence) = &fact.evidence {
                let next = sources.len() + 1;
                sources.entry(evidence.url.clone()).or_insert(next);
            }
        }
        lines.join("\n")
    }

    pub fn render(&self, max_chars: i64, preserve_requirements: bool, json_encoded: bool) -> Result<String> {
        let options = RenderOptions { preserve_requirements, json_encoded, ..RenderOptions::default() };
        self.render_with_ids(max_chars, &options).map(|rendered| rendered.text)
    }

    /// Every fact in read order when they all fit; otherwise unrelated context is dropped before requirement evidence
    /// and its basis, each group kept in read order.
    ///
    /// Read order is what the composer weighs: listing requirement facts first put a reader's one-quote conclusion ("X
    /// is the most expensive") above the prices it compared, and the answer cited only that. Verdicts must fail when
    /// requirement evidence cannot fit, rather than decide without it.
    pub fn render_with_ids(&self, max_chars: i64, options: &RenderOptions) -> Result<RenderedNotes> {
        if max_chars < 0 {
            return Err(AgentError::Invalid("max_chars must be nonnegative".into()));
        }
        let budget = max_chars as usize;
        let empty = IndexMap::new();
        let shown_ids = options.labels.as_ref().unwrap_or(&empty);
        let shown = |key: &str| shown_ids.get(key).cloned().unwrap_or_else(|| key.to_string());
        let evidence = self.evidence();
        // A tally's count rests on the records it names, so they are shown with it: a reader or a check judging the
        // count sees the rows it counted. Only a second reading of a row a tally already names is left out.
        let counted: HashSet<&String> =
            self.facts.values().filter(|fact| fact.tally.is_some()).flat_map(|fact| &fact.basis).collect();
        // A record can be counted and compared by price; its tally alone cannot evidence the price comparison.
        let compared: HashSet<String> = self.comparison_records(None).into_iter().collect();
        // Records a tally stands for when the notes run over the budget: the tally line is kept without them.
        let omittable: HashSet<String> = self.tally_records.difference(&compared).cloned().collect();
        let summarized: HashSet<&String> = omittable.iter().filter(|key| !counted.contains(key)).collect();

        let basis_text = |fact: &Fact| -> String {
            // Each counted record is named by the id its own line shows. Numbered by position in the notes, which no
            // line shows, a tally of 14 products read across three pages looked unsupported to the completion check.
            let records: Vec<String> =
                fact.basis.iter().filter(|key| counted.contains(key)).map(|key| shown(key)).collect();
            let other: Vec<Value> =
                fact.basis.iter().filter(|key| !counted.contains(key)).map(|key| Value::from(shown(key))).collect();
            let mut parts = Vec::new();
            if !records.is_empty() {
                parts.push(format!("records({})", records.join(",")));
            }
            if !other.is_empty() {
                parts.push(dumps(&Value::Array(other)));
            }
            if parts.is_empty() { String::new() } else { format!(" basis={}", parts.join(" + ")) }
        };

        let line = |key: &str, fact: &Fact, shared_url: bool| -> String {
            if let Some(tally) = &fact.tally {
                let urls = unique(fact.basis.iter().filter_map(|record| evidence.get(record)).map(|e| e.url.clone()));
                return format!(
                    "[{}] {} requirements={} tally_for={}{} urls={}",
                    shown(key),
                    quoted(&fact.text, false),
                    self.requirement_list(key),
                    tally.requirement_id,
                    basis_text(fact),
                    dumps(&json!(urls)),
                );
            }
            // A note's bracketed id is the only handle a model may cite. Its page block id beside it was a second one:
            // a composer or field reader could cite it and lose the claim, and a reader could take an earlier page's
            // "s3" for the live block [s3] of the page it is reading.
            let mut source = match &fact.evidence {
                None => "derived".to_string(),
                Some(evidence) => {
                    let url = if shared_url { String::new() } else { format!("url={} ", quoted(&evidence.url, true)) };
                    format!("{url}quote={}", quoted(&evidence.quote, false))
                }
            };
            if let Some(control) = fact.evidence.as_ref().and_then(|evidence| evidence.control_context.as_ref()) {
                source.push_str(&format!(" control_context={}", control.dump_json()));
            }
            if let (true, Some(comparison)) = (options.preserve_collections, &fact.comparison) {
                let complete = comparison.complete
                    && (fact.is_collection()
                        || self.requirements_of(key).is_some_and(|ids| ids.contains(&comparison.requirement_id)));
                source.push_str(&format!(" collection_for={} complete={complete}", comparison.requirement_id));
                if let Some(scope) = &fact.scope {
                    source.push_str(&format!(" scope={}", quoted(scope, true)));
                }
            }
            // Quoted basis facts use the source text as their claim; sending it twice inflates every later read.
            let plain = fact.evidence.as_ref().is_some_and(|evidence| fact.text == evidence.quote)
                || (options.source_only && fact.tally.is_none() && fact.comparison.is_none());
            let claim = if plain { String::new() } else { format!("{} ", quoted(&fact.text, false)) };
            format!("[{}] {claim}requirements={} {source}{}", shown(key), self.requirement_list(key), basis_text(fact))
        };

        // A JSON state escapes quotes and newlines; its notes budget must count those extra characters.
        let size = |text: &str| if options.json_encoded { dumped_len(text) } else { text::len(text) };

        let render = |entries: &[(&String, &Fact)]| -> String {
            let mut groups: IndexMap<&str, HashSet<&String>> = IndexMap::new();
            for (_, fact) in entries {
                if let Some(tally) = &fact.tally {
                    groups.entry(tally.requirement_id.as_str()).or_default().extend(&tally.records);
                }
            }
            let mut rendered: Vec<String> = groups
                .iter()
                .map(|(key, records)| {
                    format!("Tally {key}: {} distinct records in shown groups, descending counts.", records.len())
                })
                .collect();
            let mut previous_shared = false;
            // Referral URLs can outweigh the quotes; a consecutive source group shares its exact address.
            let mut start = 0;
            while start < entries.len() {
                let url = entries[start].1.evidence.as_ref().map(|e| e.url.as_str());
                let mut end = start + 1;
                while end < entries.len() && entries[end].1.evidence.as_ref().map(|e| e.url.as_str()) == url {
                    end += 1;
                }
                let group = &entries[start..end];
                start = end;
                let mut inline = group.iter().map(|(key, fact)| line(key, fact, false)).collect::<Vec<_>>().join("\n");
                let use_shared = url.is_some_and(|url| {
                    let shared = shared_group(url, group, &line);
                    size(&shared) < size(&inline)
                });
                if previous_shared && !use_shared {
                    inline = format!("# Other evidence\n{inline}");
                }
                rendered.push(if use_shared { shared_group(url.unwrap_or_default(), group, &line) } else { inline });
                previous_shared = use_shared;
            }
            rendered.join("\n")
        };

        let mut ranked: Vec<(&String, &Fact)> =
            self.facts.iter().filter(|(key, _)| !summarized.contains(*key) || self.answers_any(key)).collect();
        // Counts are ranked in code; the reader need only select the output the task asked for.
        ranked.sort_by(|a, b| tally_order(a.1).cmp(&tally_order(b.1)));

        let complete = render(&ranked);
        if size(&complete) <= budget {
            let ids = self.facts.keys().cloned().collect();
            return Ok(RenderedNotes { text: complete, evidence_ids: ids });
        }
        let answering: Vec<&str> =
            self.requirements.iter().filter(|(_, ids)| !ids.is_empty()).map(|(key, _)| key.as_str()).collect();
        let required: HashSet<String> = self.expand_evidence_ids(answering).into_iter().collect();
        let mut ordered = ranked.clone();
        ordered.sort_by_key(|(key, _)| !required.contains(*key));
        let keys: Vec<&str> = ordered.iter().map(|(key, _)| key.as_str()).collect();
        for count in (0..ordered.len()).rev() {
            let mut kept: HashSet<String> = keys[..count].iter().map(|key| key.to_string()).collect();
            kept.extend(
                self.expand_evidence_ids(keys[..count].iter().copied())
                    .into_iter()
                    .filter(|key| omittable.contains(key)),
            );
            if options.preserve_requirements && required.iter().any(|key| !kept.contains(key)) {
                return Err(AgentError::NotesTooLarge(format!(
                    "Requirement evidence exceeds the {max_chars} character notes budget"
                )));
            }
            if ordered[..count]
                .iter()
                .any(|(_, fact)| fact.basis.iter().any(|key| !kept.contains(key) && !omittable.contains(key)))
            {
                continue;
            }
            let body = render(&ordered[..count]);
            let marker = format!("[{} facts omitted]", ordered.len() - count);
            let result = if body.is_empty() { marker } else { format!("{body}\n{marker}") };
            if size(&result) <= budget {
                let ids = self.facts.keys().filter(|key| kept.contains(*key)).cloned().collect();
                return Ok(RenderedNotes { text: result, evidence_ids: ids });
            }
        }
        if options.preserve_requirements {
            return Err(AgentError::NotesTooLarge(format!(
                "The {max_chars} character notes budget cannot report omitted facts"
            )));
        }
        Err(AgentError::Invalid("max_chars is too small to report omitted citations".into()))
    }
}

/// `render_with_ids`'s keyword arguments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderOptions {
    pub preserve_requirements: bool,
    pub preserve_collections: bool,
    pub source_only: bool,
    pub json_encoded: bool,
    pub labels: Option<IndexMap<String, String>>,
}

fn shared_group(url: &str, group: &[(&String, &Fact)], line: &dyn Fn(&str, &Fact, bool) -> String) -> String {
    std::iter::once(format!("# Source URL: {}", quoted(url, true)))
        .chain(group.iter().map(|(key, fact)| format!("- {}", line(key, fact, true))))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tallies first, highest count first, then everything else in read order.
fn tally_order(fact: &Fact) -> (u8, std::cmp::Reverse<usize>, &str) {
    match &fact.tally {
        Some(tally) => (0, std::cmp::Reverse(tally.count()), tally.key.as_str()),
        None => (1, std::cmp::Reverse(0), ""),
    }
}

fn tally_requirement(fact: &Fact) -> String {
    fact.tally.as_ref().map_or_else(String::new, |tally| tally.requirement_id.clone())
}

/// Each value once, in first-seen order.
pub(crate) fn unique<T: Eq + Hash>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    let set: IndexSet<T> = values.into_iter().collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    //! tests/golden_memory.rs replays the recorded memory scenarios.
    use fastbrowse_core::models::SourceControl;

    use super::*;
    use crate::planner::RequirementKind;

    fn span(sha: &str, start: i64, end: i64) -> Evidence {
        serde_json::from_value(json!({
            "source_id": "s1", "url": "https://example.test", "frame_id": null,
            "captured_at": "2026-01-01T00:00:00Z", "capture_sha256": sha, "start": start, "end": end, "quote": "fact",
        }))
        .unwrap()
    }

    fn quoted(quote: &str, sha: &str, url: &str) -> Fact {
        let evidence =
            Evidence { quote: quote.into(), url: url.into(), end: text::len(quote) as i64, ..span(sha, 0, 4) };
        Fact::read(Some("r1"), quote, evidence, FactReader::Llm)
    }

    fn context(fact: Fact) -> Fact {
        Fact { requirement_id: None, ..fact }
    }

    fn texts(notes: &Notes, requirement: &str) -> Vec<String> {
        notes.supporting(requirement).into_iter().map(|(_, fact)| fact.text).collect()
    }

    #[test]
    fn a_requirement_claim_on_a_context_quote_keeps_the_context_claim() {
        let line = quoted("katherine@example.com: 423345", "a", "https://example.test/phone");
        let mut notes = Notes::default();
        notes.add(Fact { text: "The code for katherine@example.com is 423345.".into(), ..context(line.clone()) });
        notes.add(Fact { text: "The account email is katherine@example.com.".into(), ..line });
        assert_eq!(
            texts(&notes, "r1"),
            ["The code for katherine@example.com is 423345.\nThe account email is katherine@example.com."]
        );
    }

    #[test]
    fn test_a_new_read_of_an_address_retires_quotes_it_no_longer_shows() {
        let old = "SelectedDate: 31/10/2026";
        let new = "SelectedDate: 28/11/2026";
        for (shown, url, kept) in [
            (new.to_string(), "https://example.test", vec![new]),
            (format!("{new} was {old}"), "https://example.test", vec![old, new]),
            (new.to_string(), "https://example.test/2", vec![old, new]),
        ] {
            let mut notes = Notes::new([quoted(old, "before", "https://example.test"), quoted(new, "after", url)]);
            notes.supersede("r1", url, "after", &shown);
            assert_eq!(texts(&notes, "r1"), kept);
            assert_eq!(notes.facts().len(), 2);
        }
    }

    #[test]
    fn test_a_quote_inside_a_longer_word_is_not_still_shown() {
        let mut notes = Notes::new([
            quoted("Priya Sharma", "before", "https://example.test"),
            quoted("Priya Sharman", "after", "https://example.test"),
        ]);
        notes.supersede("r1", "https://example.test", "after", "Name: Priya Sharman");
        assert_eq!(texts(&notes, "r1"), ["Priya Sharman"]);
        let read: Vec<String> = notes.read_for("r1").into_iter().map(|e| e.quote).collect();
        assert_eq!(read, ["Priya Sharma", "Priya Sharman"]);
    }

    #[test]
    fn test_a_read_that_evidenced_nothing_retires_nothing() {
        let mut notes = Notes::new([quoted("SelectedDate: 31/10/2026", "before", "https://example.test")]);
        notes.supersede("r1", "https://example.test", "after", "SelectedDate: 28/11/2026");
        assert!(notes.evidenced("r1"));
    }

    #[test]
    fn test_updated_page_retires_vanished_context_quotes() {
        for still_shown in [false, true] {
            let old = context(quoted("Subtotal: GBP 34.50", "before", "https://example.test"));
            let fresh = quoted("Subtotal: GBP 43.25", "after", "https://example.test");
            let mut notes = Notes::new([old.clone(), fresh.clone()]);
            let shown = if still_shown { format!("{}\n{}", fresh.text, old.text) } else { fresh.text.clone() };
            notes.supersede("r1", "https://example.test", "after", &shown);
            assert_eq!(notes.current_evidence(None).contains_key(&fact_id(&old)), still_shown);
            assert!(notes.current_evidence(None).contains_key(&fact_id(&fresh)));
            notes.add(old.clone());
            assert!(notes.current_evidence(None).contains_key(&fact_id(&old)));
        }
    }

    #[test]
    fn test_changed_visible_rows_do_not_retire_a_comparisons_records() {
        let first = context(quoted("Order A: 12", "before", "https://example.test"));
        let last = quoted("Order B: 7", "after", "https://example.test");
        let mut notes = Notes::new([first.clone(), last.clone()]);
        notes.add_continuation("r1", &fact_id(&first));
        notes.supersede("r1", "https://example.test", "after", &last.text);
        assert!(notes.current_evidence(None).contains_key(&fact_id(&first)));
    }

    #[test]
    fn test_notes_deduplicate_spans_without_losing_requirement_coverage() {
        let mut notes = Notes::default();
        assert!(notes.add(Fact::read(Some("r1"), "First fact", span("capture", 0, 4), FactReader::Llm)));
        let same = Fact::read(Some("r2"), "Same span, another requirement", span("capture", 0, 4), FactReader::Llm);
        assert!(!notes.add(same));
        assert!(notes.evidenced("r1") && notes.evidenced("r2"));
        assert_eq!(notes.facts()[0].text, "First fact\nSame span, another requirement");
        assert!(!notes.add(Fact::read(Some("r2"), "First fact", span("capture", 0, 4), FactReader::Llm)));
        assert_eq!(notes.facts()[0].text, "First fact\nSame span, another requirement");
        let plan = Plan::new(
            (1..4)
                .map(|i| Requirement::new(&format!("r{i}"), &format!("Requirement {i}"), RequirementKind::Information))
                .collect(),
            true,
        );
        let unresolved: Vec<String> = notes.unresolved(&plan).into_iter().map(|r| r.id).collect();
        assert_eq!(unresolved, ["r3"]);
        assert!(notes.add(Fact::read(None, "Another capture", span("different", 0, 4), FactReader::Llm)));
        assert!(notes.add(Fact::read(None, "Another span", span("capture", 10, 14), FactReader::Llm)));
        assert_eq!(notes.evidence().len(), 3);
    }

    #[test]
    fn test_render_reports_omissions_and_never_slices_a_citation() {
        let first = Fact::read(None, "A long cited fact", span("capture", 0, 4), FactReader::Llm);
        let second = Fact::read(None, "A second cited fact", span("second", 0, 4), FactReader::Llm);
        let notes = Notes::new([first.clone(), second.clone()]);
        let complete = notes.render(1000, false, false).unwrap();
        assert!(complete.contains(&fact_id(&first)) && complete.contains(&fact_id(&second)));
        assert!(complete.contains("quote=\"fact\""));
        let one_line = Notes::new([first]).render(1000, false, false).unwrap();
        let budget = (one_line.len() + "\n[1 facts omitted]".len()) as i64;
        assert_eq!(notes.render(budget, false, false).unwrap(), format!("{one_line}\n[1 facts omitted]"));
        assert_eq!(notes.render(20, false, false).unwrap(), "[2 facts omitted]");
        assert!(matches!(notes.render(1, false, false), Err(AgentError::Invalid(m)) if m.contains("too small")));
        assert_eq!(Notes::default().render(0, false, false).unwrap(), "");
    }

    #[test]
    fn test_reused_span_keeps_the_answer_and_unions_its_basis_in_read_order() {
        let records: Vec<Fact> =
            ["A", "B", "C"].iter().map(|n| Fact::read(None, n, span(n, 0, 4), FactReader::Llm)).collect();
        let mut notes = Notes::new(records.clone());
        let ids: Vec<String> = notes.evidence().keys().cloned().collect();
        let (a, b, c) = (&ids[0], &ids[1], &ids[2]);
        let winner = Fact {
            requirement_id: Some("r".into()),
            text: "A wins".into(),
            basis: vec![b.clone()],
            ..records[0].clone()
        };
        assert!(!notes.add(winner));
        assert!(!notes.add(Fact { basis: vec![c.clone(), b.clone(), a.clone()], ..records[0].clone() }));
        assert_eq!(notes.facts()[0].text, "A wins");
        assert_eq!(notes.facts()[0].basis, [b.clone(), c.clone(), a.clone()]);
        assert_eq!(notes.expand_evidence_ids([a.as_str(), c.as_str(), a.as_str()]), [a.clone(), b.clone(), c.clone()]);
    }

    #[test]
    fn test_equal_quoted_values_from_different_controls_keep_separate_evidence() {
        let mut notes = Notes::default();
        for position in ["1 of 2", "2 of 2"] {
            let control =
                SourceControl { role: "figure".into(), label: "Avatar".into(), context: Some(position.into()) };
            let evidence = Evidence { control_context: Some(control), ..span("capture", 0, 4) };
            assert!(notes.add(Fact::read(None, "fact", evidence, FactReader::Llm)));
        }
        assert_eq!(notes.facts().len(), 2);
    }

    #[test]
    fn a_fact_with_neither_evidence_nor_basis_is_refused() {
        assert!(Fact::derived(None, "x", vec![], FactReader::Llm).is_err());
        let written = json!({"text": "x", "evidence": null, "reader": "llm"});
        assert!(serde_json::from_value::<Fact>(written).is_err());
        assert!(serde_json::from_value::<Fact>(json!({"text": "x", "basis": ["a"], "reader": "llm"})).is_err());
    }

    #[test]
    fn addresses_join_as_urlsplit_and_geturl_do() {
        for (url, joined) in [
            ("https://example.test/rows?page=1#row", "https://example.test/rows?page=1"),
            ("HTTPS://example.test/a?", "https://example.test/a"),
            ("https://example.test", "https://example.test"),
            ("  https://ex\tample.test/", "https://example.test/"),
            ("//host/path", "//host/path"),
            ("mailto:ada@example.test", "mailto:ada@example.test"),
            ("file:///etc/hosts", "file:///etc/hosts"),
            ("http:path", "http:path"),
            ("http:/path", "http:///path"),
            ("relative#x", "relative"),
        ] {
            assert_eq!(address(url), joined, "{url}");
        }
    }
}
