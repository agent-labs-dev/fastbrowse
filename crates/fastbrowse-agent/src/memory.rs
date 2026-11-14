//! Small evidence-backed memory with stable citation ids and explicit truncation.
//!
//! Every map beside the facts is an `IndexMap`, so iteration follows insertion order, and each set is only ever asked
//! for membership or sorted first.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

use fastbrowse_core::citations::{TableBlock, table_blocks};
use fastbrowse_core::models::{Evidence, FactReader};
use fastbrowse_core::page::{BlockKind, Capture};
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

/// A text's words in order, lowercased. A currency sign or unit stays on its amount, since "£12.29" and "$12.29"
/// differ and so do "5%" and "5‰", and so does a minus sign that starts one, since "-$10.00" and "$10.00" differ.
fn words(text: &str) -> Vec<String> {
    const CURRENCY: &[char] = &['£', '$', '€', '¥', '₹', '₩', '₽', '¢', '₺', '₪'];
    const UNITS: &[char] = &['%', '‰', '‱', '°'];
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let kept = |index: usize| {
        let c = chars[index];
        let signs_amount = matches!(c, '-' | '\u{2212}')
            && index.checked_sub(1).is_none_or(|before| !chars[before].is_alphanumeric())
            && chars.get(index + 1).is_some_and(|next| next.is_ascii_digit() || CURRENCY.contains(next));
        c.is_alphanumeric() || c == '.' || CURRENCY.contains(&c) || UNITS.contains(&c) || signs_amount
    };
    let mut words = Vec::new();
    let mut word = String::new();
    for index in 0..=chars.len() {
        if index < chars.len() && kept(index) {
            word.push(if chars[index] == '\u{2212}' { '-' } else { chars[index] });
            continue;
        }
        let trimmed = word.trim_matches('.');
        if !trimmed.is_empty() {
            words.push(trimmed.to_string());
        }
        word.clear();
    }
    words
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

/// Whether `now` was captured after `before`; false when either capture time has no fixed offset.
fn read_later(now: &Evidence, before: &Evidence) -> bool {
    now.captured_at.timestamp().zip(before.captured_at.timestamp()).is_some_and(|(now, before)| now > before)
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

type CaptureIdentity = (String, String, DateTime);

#[derive(Clone)]
struct SourceBlock {
    source_id: String,
    start: i64,
    end: i64,
    kind: BlockKind,
    whole_record: bool,
    /// The span of each link's text inside the block.
    links: Vec<(i64, i64)>,
}

#[derive(Clone, Default)]
pub struct Notes {
    facts: IndexMap<String, Fact>,
    /// The facts standing as the task's answer: a later read or a reopened count takes one out, and it stays as history.
    answering: IndexSet<String>,
    retired: HashSet<String>,
    pages: IndexMap<CaptureIdentity, CapturedPage>,
    /// How many of the run's steps that could change the site had been taken when each capture was read.
    steps_taken: HashMap<CaptureIdentity, usize>,
    /// A table's structure belongs to its capture, even when another page renders identical text.
    tables: HashMap<CaptureIdentity, Arc<[TableBlock]>>,
    /// A fragment keeps its captured block type, so a heading is not mistaken for the record it names.
    source_blocks: HashMap<CaptureIdentity, Arc<[SourceBlock]>>,
    /// Where code blanked a secret value in each capture's text.
    masked: HashMap<CaptureIdentity, Arc<[(i64, i64)]>>,
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

    /// Remember a capture read once `steps_taken` of the run's steps that could change the site were taken.
    #[doc(hidden)]
    pub fn remember_capture(&mut self, capture: &Capture, steps_taken: usize) {
        let key = (capture.sha256.clone(), capture.url.clone(), capture.captured_at);
        self.steps_taken.entry(key.clone()).or_insert(steps_taken);
        // A title can change while the quoted body does not; recapturing must not rename older evidence.
        self.pages
            .entry(key.clone())
            .or_insert_with(|| CapturedPage { url: capture.url.clone(), title: capture.title.clone() });
        let mut sources: HashMap<CaptureIdentity, Vec<SourceBlock>> = HashMap::new();
        for block in &capture.blocks {
            let url = block.source_url.as_ref().unwrap_or(&capture.url);
            let identity = (capture.sha256.clone(), url.clone(), capture.captured_at);
            sources.entry(identity).or_default().push(SourceBlock {
                source_id: block.source_id.clone(),
                start: block.start,
                end: block.end,
                kind: block.kind,
                whole_record: block.whole_record,
                links: block.links.iter().map(|link| (link.start, link.end)).collect(),
            });
        }
        for (identity, blocks) in sources {
            self.tables.entry(identity.clone()).or_insert_with(|| table_blocks(capture).into());
            self.masked.entry(identity.clone()).or_insert_with(|| capture.masked.clone().into());
            self.source_blocks.entry(identity).or_insert_with(|| blocks.into());
        }
    }

    /// Whether code blanked a secret value inside `evidence`'s quote, so a `•` in it is a mask, not page text.
    pub fn masks_secret(&self, evidence: &Evidence) -> bool {
        let key = (evidence.capture_sha256.clone(), evidence.url.clone(), evidence.captured_at);
        self.masked
            .get(&key)
            .is_some_and(|spans| spans.iter().any(|&(start, end)| start < evidence.end && evidence.start < end))
    }

    /// Whether `evidence` quotes one whole record block of a remembered capture: every part of a card or comment the
    /// page rendered, so what it does not show, that record does not show on the page.
    pub fn whole_record(&self, evidence: &Evidence) -> bool {
        self.source_block(evidence)
            .is_some_and(|block| block.whole_record && block.start == evidence.start && block.end == evidence.end)
    }

    /// The type of the element the quote is: a link when it lies within one link's text, such as a list item that is
    /// only a link, otherwise its uniquely containing block's. Unknown provenance must not invent a page structure.
    pub fn source_kind(&self, evidence: &Evidence) -> Option<BlockKind> {
        let block = self.source_block(evidence)?;
        let link = block.links.iter().any(|&(start, end)| start <= evidence.start && evidence.end <= end);
        Some(if link { BlockKind::Link } else { block.kind })
    }

    /// Whether `evidence` quotes the entire text of the element `source_kind` names: all of one link's text when it
    /// lies within a link, otherwise all of its block's. A heading that ends in "..." is then the page's own heading,
    /// not a cut from a longer one.
    pub fn whole_element(&self, evidence: &Evidence) -> bool {
        let Some(block) = self.source_block(evidence) else { return false };
        let link = block.links.iter().find(|&&(start, end)| start <= evidence.start && evidence.end <= end);
        let (start, end) = link.copied().unwrap_or((block.start, block.end));
        (start, end) == (evidence.start, evidence.end)
    }

    fn source_block(&self, evidence: &Evidence) -> Option<&SourceBlock> {
        let key = (evidence.capture_sha256.clone(), evidence.url.clone(), evidence.captured_at);
        let blocks = self.source_blocks.get(&key)?;
        let mut matching = blocks.iter().filter(|block| {
            block.source_id == evidence.source_id && block.start <= evidence.start && evidence.end <= block.end
        });
        let block = matching.next()?;
        matching.next().is_none().then_some(block)
    }

    /// The table blocks of the capture `evidence` was cut from; none when that capture was never remembered.
    pub fn table_blocks(&self, evidence: &Evidence) -> &[TableBlock] {
        let key = (evidence.capture_sha256.clone(), evidence.url.clone(), evidence.captured_at);
        self.tables.get(&key).map_or(&[], |tables| tables)
    }

    /// How many of the run's steps that could change the site had been taken when `evidence` was read; None for a
    /// capture never remembered, such as a screenshot reading.
    ///
    /// A page read before a step that changed it shows the state before that step: a settings form read before its
    /// description was replaced was reported as still holding the old text, beside the public page read after the
    /// save, and the answer that mixed the two was rejected (#934).
    pub fn steps_taken(&self, evidence: &Evidence) -> Option<usize> {
        self.steps_taken.get(&(evidence.capture_sha256.clone(), evidence.url.clone(), evidence.captured_at)).copied()
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

    /// Whether a later read replaced this fact, while retaining it as evidence about the earlier state.
    pub(crate) fn superseded(&self, key: &str) -> bool {
        self.retired.contains(key)
    }

    /// Keep only the facts `keep` accepts; the pages and tables read stay, since other facts may cite them.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&Fact) -> bool) {
        self.facts.retain(|_, fact| keep(fact));
        let facts = &self.facts;
        self.answering.retain(|key| facts.contains_key(key));
        self.retired.retain(|key| facts.contains_key(key));
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
            // The same span of the same page text read again stands as of its latest read. Kept as of its first, a
            // sorted list read again after the run had left it and come back held its top record only as the page
            // stood before those steps, and the composer named the next record as the list's first (#1082).
            let evidence = match (&previous.evidence, &fact.evidence) {
                (Some(before), Some(now)) if read_later(now, before) => Some(now.clone()),
                _ => kept.evidence.clone(),
            };
            let merged = Fact { basis, text, evidence, ..kept.clone() };
            self.facts.insert(key, merged);
            return false;
        }
        self.facts.insert(key, fact);
        true
    }

    /// A value read from the page's pixels: its quote is the screenshot's own text, marked as not literal page text,
    /// and its digest names the image a citation points at. A page drawn as a picture answers only this way.
    pub fn add_image_read(&mut self, text: &str, url: &str, captured_at: DateTime, png: &[u8]) -> bool {
        let source = crate::policy::screenshot_reference(png);
        let evidence = Evidence {
            source_id: source.clone(),
            url: url.into(),
            frame_id: None,
            captured_at,
            capture_sha256: source,
            start: 0,
            end: text::len(text) as i64,
            quote: format!("read from the screenshot: {text}"),
            complete_source: false,
            control_context: None,
            rendered_text: false,
            heading_path: Vec::new(),
            capture_artifact: None,
            links: Vec::new(),
        };
        self.add(Fact {
            answers: true,
            text: text.into(),
            evidence: Some(evidence),
            basis: Vec::new(),
            reader: FactReader::LlmImage,
        })
    }

    /// Whether every word and number of `text` is already in a current text note read from `url`.
    ///
    /// A screenshot reading that only repeats what the page's text layer said adds nothing the notes lack, yet it
    /// reads one viewport as the whole answer: on a listing whose covers made the page count as drawn, such a reading
    /// named the dearer book in view, and the composer took it over the cheaper one the text notes held (#799).
    pub fn repeats_page_text(&self, text: &str, url: &str) -> bool {
        let page = address(url);
        let said = words(text);
        // One note has to say it, word for word in one run: words scattered over several notes, reordered, or picked
        // out with others between, can state what none of them does, as "Alice beat Bob" from "Alice beat Carol" and
        // "Bob beat Alice", or "Basic plan $10.00" from "Basic plan $20.00; Premium plan $10.00".
        let in_order = |note: &[String]| note.windows(said.len()).any(|run| run == said.as_slice());
        !said.is_empty()
            && self
                .facts
                .iter()
                .filter(|(key, fact)| self.current(key) && fact.reader != FactReader::LlmImage)
                .filter(|(_, fact)| fact.evidence.as_ref().is_some_and(|evidence| address(&evidence.url) == page))
                .any(|(_, fact)| in_order(&words(&fact.text)))
    }

    /// Whether the notes hold a fact standing as the task's answer.
    pub fn answered(&self) -> bool {
        !self.answering.is_empty()
    }

    /// Whether this fact stands as the task's answer.
    pub fn answers(&self, key: &str) -> bool {
        self.answering.contains(key)
    }

    /// Once a read of `url` adds evidence, earlier reads of that address whose quote the page no longer shows stop
    /// supporting it, and stay as context.
    ///
    /// After a date picker's choice was corrected from 31/10 to 28/11, a new read quoted the new date, but the answer
    /// still quoted the old one, since both facts stood. A product page that swaps its name and price together must not
    /// answer with the old name beside the new price either, so a vanished value is retired whatever it was read for.
    pub fn supersede(&mut self, url: &str, sha256: &str, text: &str) {
        // Synthesis keeps source quotes as context; retired history must not stand in for a fresh read.
        let fresh: Vec<&Evidence> = self
            .facts
            .iter()
            .filter(|(key, _)| self.current(key))
            .filter_map(|(_, fact)| fact.evidence.as_ref())
            .filter(|evidence| evidence.capture_sha256 == sha256)
            .collect();
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
        let cut = |count: usize| -> String {
            let body = render(&ordered[..count]);
            let marker = format!("[{} facts omitted]", ordered.len() - count);
            if body.is_empty() { marker } else { format!("{body}\n{marker}") }
        };
        // Each fact kept adds a line and shortens the marker by at most a digit, so the size only grows with the
        // count: no count past the largest that fits can fit, and the scan below starts there. Scanning down from
        // every fact re-rendered all of them per fact dropped, and a checkout run's 1,800 notes took over three
        // minutes a step.
        let (mut fits, mut over) = (0, ordered.len());
        while over - fits > 1 {
            let middle = (fits + over) / 2;
            if size(&cut(middle)) <= budget { fits = middle } else { over = middle }
        }
        for count in (0..=fits).rev() {
            let kept: HashSet<String> = keys[..count].iter().map(|key| key.to_string()).collect();
            if preserve_answer && required.iter().any(|key| !kept.contains(key)) {
                return Err(AgentError::NotesTooLarge(format!(
                    "The answer's evidence exceeds the {max_chars} character notes budget"
                )));
            }
            if ordered[..count].iter().any(|(_, fact)| fact.basis.iter().any(|key| !kept.contains(key))) {
                continue;
            }
            let result = cut(count);
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

    /// A checkout run's 1,800 notes, over the budget, took over three minutes to cut each step: every count from
    /// the top was rendered whole. The cut is the same; it took 17s here before, and well under 1s now.
    #[test]
    fn a_minus_sign_on_an_amount_is_part_of_it() {
        let url = "https://example.test/balance";
        let notes = Notes::new([quoted("Account balance $10.00", "capture", url)]);
        assert!(notes.repeats_page_text("Account balance $10.00", url));
        assert!(!notes.repeats_page_text("Account balance -$10.00", url));
        assert!(!notes.repeats_page_text("Account balance \u{2212}10.00", url));
        assert!(!notes.repeats_page_text("Account balance 10.00%", url));
        let rate = Notes::new([quoted("Annual interest rate 5%", "capture", url)]);
        assert!(rate.repeats_page_text("Annual interest rate 5%", url));
        assert!(!rate.repeats_page_text("Annual interest rate 5\u{2030}", url));
        // A hyphen between words is still a break, as before.
        assert_eq!(words("well-known -5 a-1"), ["well", "known", "-5", "a", "1"]);
    }

    #[test]
    fn notes_far_over_budget_are_cut_quickly() {
        let mut notes = Notes::default();
        for i in 0..1800 {
            let quote =
                format!("Product {i} line with a price of £{i}.50 incl. VAT and its reviews {}", "x".repeat(120));
            notes.add(context(quoted(&quote, &format!("{i:064}"), &format!("https://shop.test/page/{}", i / 90))));
        }
        let budget = (dumped_len(&notes.render(i64::MAX / 2, true, true).unwrap()) * 2 / 3) as i64;
        let started = std::time::Instant::now();
        let cut = notes.render(budget, true, true).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
        assert!(dumped_len(&cut) as i64 <= budget && cut.ends_with("facts omitted]"));
    }

    fn context(fact: Fact) -> Fact {
        Fact { answers: false, ..fact }
    }

    fn texts(notes: &Notes) -> Vec<String> {
        notes.supporting().into_iter().map(|(_, fact)| fact.text).collect()
    }

    #[test]
    fn an_image_reading_cites_the_screenshot_it_was_read_from() {
        // A drawn page's value is in its pixels, so the fact names the screenshot's digest and is marked as not
        // literal page text; a citation built from it links the page instead of a text fragment.
        let mut notes = Notes::default();
        let at = DateTime::new(2026, 1, 1, 0, 0, 0).utc();
        assert!(notes.add_image_read("premium 3.459", "https://example.test/map", at, b"a-png"));
        assert!(notes.answered());
        let fact = notes.facts()[0];
        assert_eq!(serde_json::to_value(fact.reader).unwrap(), "llm_image");
        let evidence = fact.evidence.as_ref().expect("the screenshot is the evidence");
        assert_eq!(evidence.url, "https://example.test/map");
        assert_eq!(evidence.source_id, evidence.capture_sha256);
        assert!(evidence.capture_sha256.starts_with("screenshot-"));
        assert_eq!(evidence.quote, "read from the screenshot: premium 3.459");
        assert!(!evidence.rendered_text);
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
    fn a_synthesis_read_retires_vanished_quotes_at_the_same_address() {
        for answering in [false, true] {
            for (url, history, retained) in [
                ("https://example.test", false, false),
                ("https://example.test", true, true),
                ("https://example.test/other", false, true),
            ] {
                let mut old = quoted("Subtotal: GBP 34.50", "before", "https://example.test");
                old.answers = answering;
                let fresh = context(quoted("Subtotal: GBP 43.25", "after", url));
                let mut notes = Notes::new([old.clone(), fresh.clone()]);
                let shown = if history { format!("{}\n{}", old.text, fresh.text) } else { fresh.text.clone() };
                notes.supersede(url, "after", &shown);
                assert_eq!(notes.current(&fact_id(&old)), retained);
                assert!(notes.current(&fact_id(&fresh)));
                assert_eq!(notes.answered(), answering && retained);
                assert_eq!(notes.facts().len(), 2);
            }
        }
    }

    #[test]
    fn revisiting_a_retired_capture_without_evidence_keeps_current_facts() {
        let url = "https://example.test";
        let old = quoted("Subtotal: GBP 34.50", "before", url);
        let fresh = context(quoted("Subtotal: GBP 43.25", "after", url));
        let mut notes = Notes::new([old.clone(), fresh.clone()]);
        notes.supersede(url, "after", &fresh.text);
        assert!(!notes.current(&fact_id(&old)));
        assert!(notes.current(&fact_id(&fresh)));
        notes.supersede(url, "before", &old.text);
        assert!(notes.current(&fact_id(&fresh)));
        assert!(!notes.current(&fact_id(&old)));
        notes.add(old.clone());
        notes.supersede(url, "before", &old.text);
        assert!(notes.current(&fact_id(&old)));
        assert!(!notes.current(&fact_id(&fresh)));
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

    /// #1082: a price-sorted list was read, left for its next page and read again unchanged on coming back. Its top
    /// record kept the first read's step, so the notes showed the list after the return without it.
    #[test]
    fn a_span_read_again_stands_as_of_its_latest_read() {
        use crate::retrieval::testing::capture;

        let first = capture(&[(BlockKind::ListItem, "Drum Roaster - GBP 210.00")]);
        let again = Capture { captured_at: DateTime::new(2026, 1, 1, 0, 5, 0).utc(), ..first.clone() };
        let read = |captured: &Capture| {
            let quote = Evidence {
                capture_sha256: captured.sha256.clone(),
                captured_at: captured.captured_at,
                ..span("", 0, 12)
            };
            Fact::read("Drum Roaster", quote, FactReader::JevChoice)
        };
        let mut notes = Notes::default();
        notes.remember_capture(&first, 1);
        assert!(notes.add(read(&first).answering()));
        notes.remember_capture(&again, 3);
        assert!(!notes.add(read(&again)));
        let kept = notes.facts()[0];
        assert_eq!(notes.steps_taken(kept.evidence.as_ref().unwrap()), Some(3));
        assert!(notes.answered(), "the read again keeps the answer the first read gave");
        // An earlier capture added after a later one does not move the span back.
        assert!(!notes.add(read(&first)));
        assert_eq!(notes.steps_taken(notes.facts()[0].evidence.as_ref().unwrap()), Some(3));
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
