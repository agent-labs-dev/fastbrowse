//! Small evidence-backed memory with stable citation ids and explicit truncation.
//!
//! Every map beside the facts is an `IndexMap`, so iteration follows insertion order, and each set is only ever asked
//! for membership or sorted first.

use std::collections::{HashMap, HashSet};
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
use serde_json::json;

use crate::error::{AgentError, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "FactFields")]
pub struct Fact {
    /// The reader wrote this fact as answering the task, rather than as context a claim can rest on.
    pub answers: bool,
    pub text: String,
    /// The span the fact was read from; None when a repeated choice rests on its basis.
    pub evidence: Option<Evidence>,
    /// Fact ids of the sources this choice rests on.
    pub basis: Vec<String>,
    /// Which reader produced the fact; citations are built from it.
    pub reader: FactReader,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FactFields {
    #[serde(default)]
    answers: bool,
    text: String,
    #[serde(deserialize_with = "lax::nullable")]
    evidence: Option<Evidence>,
    #[serde(default)]
    basis: Vec<String>,
    reader: FactReader,
}

impl TryFrom<FactFields> for Fact {
    type Error = String;

    fn try_from(fields: FactFields) -> Result<Self, String> {
        Fact {
            answers: fields.answers,
            text: fields.text,
            evidence: fields.evidence,
            basis: fields.basis,
            reader: fields.reader,
        }
        .grounded()
    }
}

impl Fact {
    /// A fact read from one span, kept as context until it is marked as answering.
    pub fn read(text: &str, evidence: Evidence, reader: FactReader) -> Fact {
        Fact { answers: false, text: text.into(), evidence: Some(evidence), basis: Vec::new(), reader }
    }

    /// This fact, written as answering the task.
    pub fn answering(self) -> Fact {
        Fact { answers: true, ..self }
    }

    /// The model validator: a fact with no evidence must be derived from a basis.
    pub fn grounded(self) -> Result<Fact, String> {
        if self.evidence.is_none() && self.basis.is_empty() {
            return Err("Value error, a fact with no evidence must be derived from a basis".into());
        }
        Ok(self)
    }
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
    if let Some(evidence) = &fact.evidence {
        return evidence_id(evidence);
    }
    let mut basis = fact.basis.clone();
    basis.sort();
    format!("derived:{}", digest16(&dumps(&json!([fact.text, basis]))))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPage {
    pub url: String,
    pub title: String,
}

#[derive(Clone, Default)]
pub struct Notes {
    facts: IndexMap<String, Fact>,
    /// The facts standing as the task's answer: a later read or a reopened count takes one out, and it stays as history.
    answering: IndexSet<String>,
    retired: HashSet<String>,
    pages: IndexMap<(String, String, DateTime), CapturedPage>,
}

impl fmt::Debug for Notes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notes").field("facts", &self.facts).field("answering", &self.answering).finish()
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
            .is_some_and(|fact| !self.retired.contains(key) && (!fact.answers || self.answering.contains(key)))
    }

    /// Whether a new span was added.
    pub fn add(&mut self, fact: Fact) -> bool {
        let key = fact_id(&fact);
        self.retired.remove(&key);
        if fact.answers {
            self.answering.insert(key.clone());
        }
        if let Some(previous) = self.facts.get(&key) {
            let basis = unique(previous.basis.iter().chain(&fact.basis).cloned());
            // A winner can quote a row already collected as context; its answer must survive that reuse.
            let new_kept = !previous.answers && fact.answers;
            let kept = if new_kept { &fact } else { previous };
            // One block can state two things the task asks for (a card's title and its price): the draft states this
            // fact's text, so each claim must survive the reuse.
            let mut text = kept.text.clone();
            if !new_kept && fact.answers && !text.contains(&fact.text) {
                text = format!("{text}\n{}", fact.text);
            }
            // The context claim it was read as stays too: a Phone page line read for the sign-in code, then again as
            // the account email, kept only the email claim, and Jev went back for a code the notes no longer said.
            let quote = previous.evidence.as_ref().map(|evidence| evidence.quote.as_str());
            if new_kept && Some(previous.text.as_str()) != quote && !text.contains(&previous.text) {
                text = format!("{}\n{text}", previous.text);
            }
            let merged = Fact { basis, text, ..kept.clone() };
            self.facts.insert(key, merged);
            return false;
        }
        self.facts.insert(key, fact);
        true
    }

    /// Whether the notes hold a fact standing as the task's answer.
    pub fn answered(&self) -> bool {
        !self.answering.is_empty()
    }

    /// Whether this fact stands as the task's answer.
    pub fn answers(&self, key: &str) -> bool {
        self.answering.contains(key)
    }

    /// Once a read of `url` supports the answer, earlier reads of that address whose quote the page no longer shows stop
    /// supporting it, and stay as context.
    ///
    /// After a date picker's choice was corrected from 31/10 to 28/11, a new read quoted the new date, but the answer
    /// still quoted the old one, since both facts stood. A product page that swaps its name and price together must not
    /// answer with the old name beside the new price either, so a vanished value is retired whatever it was read for.
    pub fn supersede(&mut self, url: &str, sha256: &str, text: &str) {
        let fresh: Vec<Evidence> =
            self.supporting_evidence().into_iter().filter(|evidence| evidence.capture_sha256 == sha256).collect();
        if fresh.is_empty() {
            return;
        }
        // A capture includes child frames whose quotes belong to their own addresses, not the parent page's.
        let mut addresses: HashSet<String> = HashSet::from([address(url)]);
        addresses.extend(fresh.iter().map(|span| address(&span.url)));
        let evidence = self.evidence();
        // The quote this read cut is on the page by construction. A confirmation claim that drew on the Review step it
        // followed was retired along with that step, and the answer it had just given was gone again.
        let own: Vec<String> = self
            .facts
            .iter()
            .filter(|(_, fact)| fact.evidence.as_ref().is_some_and(|e| e.capture_sha256 == sha256))
            .map(|(key, _)| key.clone())
            .collect();
        let vanished = |source: &Evidence| {
            source.capture_sha256 != sha256 && addresses.contains(&address(&source.url)) && !shows(text, &source.quote)
        };
        let mut retired: HashSet<String> = HashSet::new();
        for (key, fact) in &self.facts {
            if own.contains(key) {
                continue;
            }
            let stale = if !fact.basis.is_empty() {
                self.expand_evidence_ids([key.as_str()]).iter().filter_map(|source| evidence.get(source)).any(vanished)
            } else {
                fact.evidence.as_ref().is_some_and(vanished)
            };
            if stale {
                // Context quotes can contradict a later value even when they never answered anything.
                retired.insert(key.clone());
            }
        }
        for key in &retired {
            self.retired.insert(key.clone());
            self.answering.shift_remove(key);
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

    /// The facts standing as the answer, in the order they were read.
    pub fn supporting(&self) -> Vec<(String, Fact)> {
        self.answering.iter().filter_map(|key| Some((key.clone(), self.facts.get(key)?.clone()))).collect()
    }

    pub fn supporting_evidence(&self) -> Vec<Evidence> {
        let roots: Vec<String> = self.supporting().into_iter().map(|(key, _)| key).collect();
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
            let answers = if self.answers(key) { " answers=true" } else { "" };
            let mut line = format!("{}{answers}{source}", quoted(&fact.text, false));
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

    /// Every fact in read order when they all fit; otherwise unrelated context is dropped before the answer's facts and
    /// their basis, each group kept in read order.
    pub fn render(&self, max_chars: i64, preserve_answer: bool, json_encoded: bool) -> Result<String> {
        if max_chars < 0 {
            return Err(AgentError::Invalid("max_chars must be nonnegative".into()));
        }
        let budget = max_chars as usize;
        let basis_text = |fact: &Fact| -> String {
            if fact.basis.is_empty() { String::new() } else { format!(" basis={}", dumps(&json!(fact.basis))) }
        };

        let line = |key: &str, fact: &Fact, shared_url: bool| -> String {
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
            // Quoted basis facts use the source text as their claim; sending it twice inflates every later read.
            let plain = fact.evidence.as_ref().is_some_and(|evidence| fact.text == evidence.quote);
            let claim = if plain { String::new() } else { format!("{} ", quoted(&fact.text, false)) };
            let answers = if self.answers(key) { "answers=true " } else { "" };
            format!("[{key}] {claim}{answers}{source}{}", basis_text(fact))
        };

        // A JSON state escapes quotes and newlines; its notes budget must count those extra characters.
        let size = |text: &str| if json_encoded { dumped_len(text) } else { text::len(text) };

        let render = |entries: &[(&String, &Fact)]| -> String {
            let mut rendered: Vec<String> = Vec::new();
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

        let ranked: Vec<(&String, &Fact)> = self.facts.iter().collect();
        let complete = render(&ranked);
        if size(&complete) <= budget {
            return Ok(complete);
        }
        let required: HashSet<String> =
            self.expand_evidence_ids(self.answering.iter().map(String::as_str)).into_iter().collect();
        let mut ordered = ranked.clone();
        ordered.sort_by_key(|(key, _)| !required.contains(*key));
        let keys: Vec<&str> = ordered.iter().map(|(key, _)| key.as_str()).collect();
        for count in (0..ordered.len()).rev() {
            let kept: HashSet<String> = keys[..count].iter().map(|key| key.to_string()).collect();
            if preserve_answer && required.iter().any(|key| !kept.contains(key)) {
                return Err(AgentError::NotesTooLarge(format!(
                    "The answer's evidence exceeds the {max_chars} character notes budget"
                )));
            }
            if ordered[..count].iter().any(|(_, fact)| fact.basis.iter().any(|key| !kept.contains(key))) {
                continue;
            }
            let body = render(&ordered[..count]);
            let marker = format!("[{} facts omitted]", ordered.len() - count);
            let result = if body.is_empty() { marker } else { format!("{body}\n{marker}") };
            if size(&result) <= budget {
                return Ok(result);
            }
        }
        if preserve_answer {
            return Err(AgentError::NotesTooLarge(format!(
                "The {max_chars} character notes budget cannot report omitted facts"
            )));
        }
        Err(AgentError::Invalid("max_chars is too small to report omitted citations".into()))
    }
}

fn shared_group(url: &str, group: &[(&String, &Fact)], line: &dyn Fn(&str, &Fact, bool) -> String) -> String {
    std::iter::once(format!("# Source URL: {}", quoted(url, true)))
        .chain(group.iter().map(|(key, fact)| format!("- {}", line(key, fact, true))))
        .collect::<Vec<_>>()
        .join("\n")
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
        Fact::read(quote, evidence, FactReader::JevChoice).answering()
    }

    fn context(fact: Fact) -> Fact {
        Fact { answers: false, ..fact }
    }

    fn texts(notes: &Notes) -> Vec<String> {
        notes.supporting().into_iter().map(|(_, fact)| fact.text).collect()
    }

    #[test]
    fn an_answering_claim_on_a_context_quote_keeps_the_context_claim() {
        let line = quoted("katherine@example.com: 423345", "a", "https://example.test/phone");
        let mut notes = Notes::default();
        notes.add(Fact { text: "The code for katherine@example.com is 423345.".into(), ..context(line.clone()) });
        notes.add(Fact { text: "The account email is katherine@example.com.".into(), ..line });
        assert_eq!(
            texts(&notes),
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
            notes.supersede(url, "after", &shown);
            assert_eq!(texts(&notes), kept);
            assert_eq!(notes.facts().len(), 2);
        }
    }

    #[test]
    fn test_a_quote_inside_a_longer_word_is_not_still_shown() {
        let mut notes = Notes::new([
            quoted("Priya Sharma", "before", "https://example.test"),
            quoted("Priya Sharman", "after", "https://example.test"),
        ]);
        notes.supersede("https://example.test", "after", "Name: Priya Sharman");
        assert_eq!(texts(&notes), ["Priya Sharman"]);
    }

    #[test]
    fn test_a_read_that_evidenced_nothing_retires_nothing() {
        let mut notes = Notes::new([quoted("SelectedDate: 31/10/2026", "before", "https://example.test")]);
        notes.supersede("https://example.test", "after", "SelectedDate: 28/11/2026");
        assert!(notes.answered());
    }

    #[test]
    fn test_updated_page_retires_vanished_context_quotes() {
        for still_shown in [false, true] {
            let old = context(quoted("Subtotal: GBP 34.50", "before", "https://example.test"));
            let fresh = quoted("Subtotal: GBP 43.25", "after", "https://example.test");
            let mut notes = Notes::new([old.clone(), fresh.clone()]);
            let shown = if still_shown { format!("{}\n{}", fresh.text, old.text) } else { fresh.text.clone() };
            notes.supersede("https://example.test", "after", &shown);
            assert_eq!(notes.current_evidence(None).contains_key(&fact_id(&old)), still_shown);
            assert!(notes.current_evidence(None).contains_key(&fact_id(&fresh)));
            notes.add(old.clone());
            assert!(notes.current_evidence(None).contains_key(&fact_id(&old)));
        }
    }

    #[test]
    fn test_notes_deduplicate_spans_and_keep_each_answering_claim() {
        let read =
            |text: &str, sha: &str, start: i64| Fact::read(text, span(sha, start, start + 4), FactReader::JevChoice);
        let mut notes = Notes::default();
        assert!(notes.add(read("First fact", "capture", 0).answering()));
        assert!(!notes.add(read("Same span, another claim", "capture", 0).answering()));
        assert!(notes.answered());
        assert_eq!(notes.facts()[0].text, "First fact\nSame span, another claim");
        assert!(!notes.add(read("First fact", "capture", 0).answering()));
        assert_eq!(notes.facts()[0].text, "First fact\nSame span, another claim");
        assert!(notes.add(read("Another capture", "different", 0)));
        assert!(notes.add(read("Another span", "capture", 10)));
        assert_eq!(notes.evidence().len(), 3);
    }

    #[test]
    fn test_render_reports_omissions_and_never_slices_a_citation() {
        let first = Fact::read("A long cited fact", span("capture", 0, 4), FactReader::JevChoice);
        let second = Fact::read("A second cited fact", span("second", 0, 4), FactReader::JevChoice);
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
            ["A", "B", "C"].iter().map(|n| Fact::read(n, span(n, 0, 4), FactReader::JevChoice)).collect();
        let mut notes = Notes::new(records.clone());
        let ids: Vec<String> = notes.evidence().keys().cloned().collect();
        let (a, b, c) = (&ids[0], &ids[1], &ids[2]);
        let winner = Fact { answers: true, text: "A wins".into(), basis: vec![b.clone()], ..records[0].clone() };
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
            assert!(notes.add(Fact::read("fact", evidence, FactReader::JevChoice)));
        }
        assert_eq!(notes.facts().len(), 2);
    }

    #[test]
    fn a_fact_with_neither_evidence_nor_basis_is_refused() {
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
