//! Reading a page into the notes: Jev picks what on it answers the task, and that stands.

use fastbrowse_core::effects::state_key;
use fastbrowse_core::page::{Capture, Observation};
use fastbrowse_core::py::text;
use serde_json::Map;

use super::Agent;
use super::helpers::read_sources;
use super::state::{RunState, Step};
use crate::memory::address;
use crate::retrieval::{PageRead, read_page};

impl Agent {
    /// Return whether the read added evidence, since only new evidence makes a read progress.
    pub(super) async fn read(
        &self,
        state: &mut RunState,
        capture: Capture,
        observation: &Observation,
        shows: bool,
    ) -> Step<bool> {
        // What a failed page shows, an empty result included, is not what the task asked to be told.
        if state.page_failed {
            return Ok(false);
        }
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
        let read =
            read_page(&*self.jev, &capture, &state.task, &state.dates, tokens, Some(&ledger), &self.trace, shows)
                .await?;
        Ok(self.end_read(state, capture, observation, tries, known, read))
    }

    /// A read whose Jev call runs beside `work`, which must not depend on the notes the read adds: a click's gates see
    /// only the task, the origin and the history. The read's result is applied before `work`'s is returned, so what
    /// follows sees the page as read, as it would after a read made first. None when the read needs the browser
    /// again (an empty capture waits for the page) or would not run, and the caller reads first instead.
    pub(super) async fn read_beside<T>(
        &self,
        state: &mut RunState,
        capture: Capture,
        observation: &Observation,
        shows: bool,
        work: impl AsyncFnOnce(&mut RunState) -> T,
    ) -> Step<Option<T>> {
        if state.page_failed || text::strip(&capture.text).is_empty() {
            return Ok(None);
        }
        let tries = state.begin_read(observation);
        let known = read_sources(&state.notes);
        let (task, dates, ledger) = (state.task.clone(), state.dates.clone(), state.ledger.clone());
        let tokens = &self.config.tokens;
        let reading = read_page(&*self.jev, &capture, &task, &dates, tokens, Some(&ledger), &self.trace, shows);
        let (read, done) = tokio::join!(self.trace.phase("read", reading), work(state));
        self.end_read(state, capture, observation, tries, known, read?);
        Ok(Some(done))
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
        state.record_read(&capture, facts, compose);
        // A timer changes the capture hash, so only a new source quote is progress.
        // The same page read again in a browser after a fetch differs in small print, which is not new evidence.
        let again = state.fetched.remove(&capture.url);
        let progressed = !again && read_sources(&state.notes).iter().any(|key| !known.contains(key));
        self.event("read", || {
            Map::from_iter([
                ("step".into(), state.steps.len().into()),
                ("url".into(), capture.url.clone().into()),
                ("title".into(), capture.title.clone().into()),
                ("sha256".into(), capture.sha256.clone().into()),
                ("text".into(), capture.text.clone().into()),
                ("chars".into(), text::len(&capture.text).into()),
                ("facts_added".into(), added.into()),
            ])
        });
        if progressed {
            state.evidence_reads += 1;
            // The page read is on screen as the evidence comes in, so an action that leaves it as it was has nothing
            // read since: a hover after a read was judged a return after new evidence, and so progress.
            state.reached.insert(state_key(observation), state.evidence_reads);
            state.unchanged = 0;
            state.recoveries = 0;
        }
        progressed
    }
}
