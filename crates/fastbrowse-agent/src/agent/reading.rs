//! Reading a page into the notes: Jev picks what on it answers the task, and that stands.

use fastbrowse_core::effects::state_key;
use fastbrowse_core::page::{Capture, Observation};
use fastbrowse_core::py::text;
use serde_json::Map;

use super::Agent;
use super::helpers::read_sources;
use super::state::{FailureCause, RunState, Step, tab_key};
use fastbrowse_core::models::{Evidence, FactReader};

use crate::memory::{Fact, address};
use crate::retrieval::{PageRead, read_page};

/// What a done's claims cite: their `quotes`, the quotes of a count, total or extreme (`ranging`), and the `pages` such
/// a claim names its records are on.
pub(super) struct Cited<'a> {
    pub quotes: &'a [String],
    pub ranging: &'a [String],
    pub pages: &'a [String],
    /// The latest screenshot the driver was shown of the page in front of it, which a quote no page text holds was read off.
    pub pictured: Option<&'a [u8]>,
}

impl Agent {
    /// Return whether the read added evidence, since only new evidence makes a read progress.
    pub(super) async fn read(
        &self,
        state: &mut RunState,
        capture: Capture,
        observation: &Observation,
        shows: bool,
    ) -> Step<bool> {
        // A page counts as read once a read of it finished, so the notes hold what it shows, or after a second try that
        // did not. The try is counted before anything is awaited, so one that errors has still used it up.
        let tries = state.begin_read(observation);
        let mut capture = capture;
        if text::strip(&capture.text).is_empty() {
            // An empty page may still be rendering; only new content warrants a read.
            if !self.outwait(observation, super::browsing_deadline(&state.ledger, state.compose)).await? {
                state.end_read(observation, false, tries);
                return Ok(false);
            }
            capture = self.capture().await?;
            if text::strip(&capture.text).is_empty() {
                state.end_read(observation, false, tries);
                return Ok(false);
            }
        }
        let known = read_sources(&state.notes);
        let (tokens, ledger) = (&self.config.tokens, state.ledger.clone());
        let read = read_page(
            &*self.jev,
            &capture,
            &self.model_task(state),
            &state.dates,
            tokens,
            Some(&ledger),
            &self.trace,
            shows,
            self.reached_by(state, observation).as_deref(),
        )
        .await?;
        Ok(self.end_read(state, capture, observation, tries, known, read))
    }

    /// Read `quotes` as the facts that answer the task, each off `capture` or else the latest of `earlier`, the pages
    /// the driver was shown before, returning the note each was read into, or else those no page holds verbatim. The driver saw the page and names what
    /// answers; a Jev read picks one passage, and kept "Date: 12/04/2026" from a page whose date, day and month were
    /// three blocks, so done was refused with the answer on screen.
    ///
    /// The page each of `ranging` is read off, the quotes of a count, total or extreme, is kept whole as well: code works
    /// one out from every record read, Jev judging which are in its scope, so a record the driver left unquoted is still
    /// counted. Read only as quoted, the driver's two rows of a four-row table were the total. `pages`, by address, are
    /// the pages such a claim names its records are on, kept whole the same way. Errs with the quotes no page holds and
    /// the pages the driver was never shown.
    pub(super) fn read_quoted(
        &self,
        state: &mut RunState,
        capture: Capture,
        earlier: &[Capture],
        observation: &Observation,
        cited: Cited<'_>,
    ) -> std::result::Result<Vec<String>, (Vec<String>, Vec<String>)> {
        let Cited { quotes, ranging, pages, pictured } = cited;
        let mut missing = Vec::new();
        // The pages kept whole: the current one, and the earlier ones by their place in `earlier`.
        let (mut whole_current, mut whole_before) = (false, std::collections::BTreeSet::new());
        let mut unshown = Vec::new();
        for page in pages {
            if address(page) == address(&capture.url) {
                whole_current = true;
            } else if let Some(at) = earlier.iter().rposition(|shown| address(&shown.url) == address(page)) {
                whole_before.insert(at);
            } else {
                unshown.push(page.clone());
            }
        }
        // The note each quote is read into, in the quotes' order, for the claims that cite them.
        let mut ids = Vec::new();
        let mut current = Vec::new();
        let mut before: Vec<(usize, Vec<Fact>)> = Vec::new();
        // The fact is the evidence's text, which for a table row is the row under its header.
        let fact = |evidence: Evidence| Fact::read(&evidence.quote.clone(), evidence, FactReader::LlmQuote).answering();
        for quote in quotes {
            if let Some(evidence) = crate::retrieval::quoted(&capture, quote) {
                whole_current |= ranging.contains(quote);
                let fact = fact(evidence);
                ids.push(crate::memory::fact_id(&fact));
                current.push(fact);
            } else if let Some((at, evidence)) = earlier
                .iter()
                .enumerate()
                .rev()
                .find_map(|(at, page)| crate::retrieval::quoted(page, quote).map(|evidence| (at, evidence)))
            {
                if ranging.contains(quote) {
                    whole_before.insert(at);
                }
                let fact = fact(evidence);
                ids.push(crate::memory::fact_id(&fact));
                match before.iter_mut().find(|(page, _)| *page == at) {
                    Some((_, facts)) => facts.push(fact),
                    None => before.push((at, vec![fact])),
                }
            } else if let Some(png) = pictured {
                // A chart's bars and a product drawn as an image are in no text layer: the driver read the value off the
                // screenshot it took, as the invariant allows, and the evidence says so.
                let fact = Fact::read_from_image(&self.mask(quote), &capture.url, observation.captured_at, png);
                ids.push(crate::memory::fact_id(&fact));
                current.push(fact);
            } else {
                missing.push(quote.clone());
            }
        }
        if !missing.is_empty() || !unshown.is_empty() {
            return Err((missing, unshown));
        }
        // Every page of the list the driver was shown whole holds records a count, total or extreme ranges over, quoted
        // or not: read only as quoted, a page whose records the driver missed was taken as read and left out. Only a
        // claim citing a page of that list ranges over it: a shop's front page, the first page of every book, was
        // kept whole for the priciest book in a one-page category opened from it, and the run was sent through the front
        // list's next pages one by one.
        // The pages kept whole so far are those a count, total or extreme cites: a quote another claim took off a list
        // page ranges over nothing.
        let on_list = |url: &str| state.pager_ordinals.contains_key(&address(url));
        let cites_list =
            (whole_current && on_list(&capture.url)) || whole_before.iter().any(|at| on_list(&earlier[*at].url));
        if cites_list {
            let listed = |page: &Capture| on_list(&page.url) && state.read_urls.contains(&page.url);
            whole_current |= listed(&capture);
            let mut list_pages = std::collections::HashSet::from([address(&capture.url)]);
            for (at, page) in earlier.iter().enumerate().rev() {
                if listed(page) && list_pages.insert(address(&page.url)) {
                    whole_before.insert(at);
                }
            }
        }
        // Several quotes are parts of one answer, written up together, as is one a count, total or extreme ranges over:
        // a single quote picked as the whole answer ended a whole-list count on its first page.
        let compose = quotes.len() > 1 || !ranging.is_empty() || !pages.is_empty();
        // Known before the earlier pages are recorded: counted after them, an answer quoting only pages read before
        // added no evidence, and a done that gathered it renewed no budget.
        let known = read_sources(&state.notes);
        // A page kept whole with none of its quotes cited is recorded too: unrecorded, its capture was unknown to the
        // notes, so no quote card on it was proved a whole record and Jev counted the footer "Made with love by Zyte"
        // as one quote, which left the count by author unworked.
        for at in &whole_before {
            if !before.iter().any(|(page, _)| page == at) {
                before.push((*at, Vec::new()));
            }
        }
        before.sort_by_key(|(at, _)| *at);
        // A page kept whole is read with its quotes, as one read: added after it, a done citing only `pages` read no
        // facts, so it renewed no budget, kept no capture artifact and traced none of the records it counted.
        for (at, facts) in &mut before {
            if whole_before.contains(at) {
                facts.extend(crate::retrieval::whole_page_facts(&earlier[*at]));
            }
        }
        if whole_current {
            current.extend(crate::retrieval::whole_page_facts(&capture));
        }
        for (at, facts) in before {
            let page = &earlier[at];
            self.seen().read_captures.entry(page.sha256.clone()).or_insert_with(|| page.text.clone());
            self.record_read(state, page, facts, compose);
        }
        let tries = state.begin_read(observation);
        self.end_read(state, capture, observation, tries, known, PageRead { facts: current, compose, finished: true });
        state.compose |= compose;
        Ok(ids)
    }

    fn end_read(
        &self,
        state: &mut RunState,
        capture: Capture,
        observation: &Observation,
        tries: u8,
        known: std::collections::BTreeSet<(String, Option<String>, String)>,
        read: PageRead,
    ) -> bool {
        let PageRead { facts, compose, finished } = read;
        state.end_read(observation, finished, tries);
        let added = facts.len();
        // Jev's latest word on a read stands: one fact that alone answers the task needs nothing written around it.
        if added > 0 {
            // Two picks that differ are not one answer, and only writing them up can say which stands.
            let differs = facts
                .iter()
                .find(|fact| fact.answers)
                .is_some_and(|new| state.notes.facts().iter().any(|fact| fact.answers && fact.text != new.text));
            // Jev picks from one page knowing only the task, so its pick cannot speak for another page an earlier read
            // kept to compose from: counting a site's quotes, the author page's "Quotes by: GoodReads.com" was returned
            // verbatim over the list page read whole.
            let page = address(&capture.url);
            let elsewhere = state.compose
                && state.notes.facts().iter().any(|fact| {
                    !fact.answers && fact.evidence.as_ref().is_some_and(|evidence| address(&evidence.url) != page)
                });
            state.compose = compose || differs || elsewhere;
        }
        if added > 0 {
            self.seen().read_captures.entry(capture.sha256.clone()).or_insert_with(|| capture.text.clone());
        }
        self.record_read(state, &capture, facts, compose);
        // A timer changes the capture hash, so only a new source quote is progress.
        // The same page read again in a browser after a fetch differs in small print, which is not new evidence.
        let again = state.fetched.remove(&capture.url);
        let progressed = !again && read_sources(&state.notes).iter().any(|key| !known.contains(key));
        if progressed {
            state.evidence_reads += 1;
            // The page read is on screen as the evidence comes in, so an action that leaves it as it was has nothing
            // read since: a hover after a read was judged a return after new evidence, and so progress.
            state.reached.insert(state_key(observation), state.evidence_reads);
            state.shown.insert(tab_key(observation), state.evidence_reads);
            // Evidence from a page the run knows reports the site failed renews no recovery: each done quoting the error
            // panel again reset the count that ends such a run as unavailable, and a driver rewording it never ended.
            let failing = state
                .site_failure
                .as_ref()
                .is_some_and(|failure| failure.url == observation.url && failure.cause == FailureCause::Reported);
            if !failing {
                state.renew_stall();
                state.recoveries = 0;
            }
        }
        progressed
    }

    /// Record `facts` read off `capture` into the notes, and trace them: the trace keeps the typed capture and each
    /// fact's evidence, so `Notes::record` rebuilds these notes from it. A page quoted from the pages shown before was
    /// recorded untraced, and a replay lost what the answer cited.
    fn record_read(&self, state: &mut RunState, capture: &Capture, facts: Vec<Fact>, compose: bool) {
        let added = facts.len();
        let read_facts = facts.clone();
        let steps_taken = state.record_read(capture, facts, compose);
        self.event("read", || {
            Map::from_iter([
                ("step".into(), state.steps.len().into()),
                ("url".into(), capture.url.clone().into()),
                ("title".into(), capture.title.clone().into()),
                ("sha256".into(), capture.sha256.clone().into()),
                ("chars".into(), text::len(&capture.text).into()),
                ("facts_added".into(), added.into()),
                ("capture".into(), serde_json::to_value(capture).unwrap_or_default()),
                ("facts".into(), serde_json::to_value(&read_facts).unwrap_or_default()),
                ("steps_taken".into(), steps_taken.into()),
                ("compose".into(), compose.into()),
            ])
        });
    }
}
