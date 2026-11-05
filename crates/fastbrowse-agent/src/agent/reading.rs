//! Reading a page into the notes: Jev picks what on it answers the task, and that stands.

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
        // This content is read now, so the notes hold what the page shows.
        state.mark_read(observation);
        let mut capture = capture;
        if text::strip(&capture.text).is_empty() {
            // An empty page may still be rendering; only new content warrants a read.
            if !self.outwait(observation).await? {
                return Ok(false);
            }
            capture = self.capture().await?;
            if text::strip(&capture.text).is_empty() {
                return Ok(false);
            }
        }
        let known = read_sources(&state.notes);
        let (tokens, ledger) = (&self.config.tokens, state.ledger.clone());
        let PageRead { facts, compose } =
            read_page(&*self.jev, &capture, &state.task, tokens, Some(&ledger), &self.trace, shows).await?;
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
            state.unchanged = 0;
            state.recoveries = 0;
        }
        Ok(progressed)
    }
}
