//! Reading a page into the notes: Jev picks what on it answers the task, and that stands.

use fastbrowse_core::page::{Capture, Observation};
use fastbrowse_core::py::text;
use serde_json::Map;

use super::Agent;
use super::helpers::read_sources;
use super::state::{RunState, Step};
use crate::retrieval::{PageRead, read_page};

impl Agent {
    /// Return whether the read added evidence, since only new evidence makes a read progress.
    pub(super) async fn read(&self, state: &mut RunState, capture: Capture, observation: &Observation) -> Step<bool> {
        // What a failed page shows, an empty result included, is not what the task asked to be told.
        if state.page_failed {
            return Ok(false);
        }
        // This content is read now, so the notes hold what the page shows.
        state.read_documents.insert(observation.document_key.clone(), false);
        state.read_pages.insert(observation.page_key.clone());
        state.read_urls.insert(observation.url.clone());
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
            read_page(&*self.jev, &capture, &state.task, tokens, Some(&ledger), &self.trace).await?;
        let added = facts.len();
        // Jev's latest word on a read stands: one fact that alone answers the task needs nothing written around it.
        if added > 0 {
            state.compose = compose;
        }
        state.notes.remember_capture(&capture);
        for fact in facts {
            state.notes.add(fact);
        }
        state.notes.supersede(&capture.url, &capture.sha256, &capture.text);
        // A timer changes the capture hash, so only a new source quote is progress.
        let progressed = read_sources(&state.notes).iter().any(|key| !known.contains(key));
        self.event("read", || {
            Map::from_iter([
                ("url".into(), capture.url.clone().into()),
                ("chars".into(), text::len(&capture.text).into()),
                ("facts_added".into(), added.into()),
            ])
        });
        if progressed {
            state.evidence_reads += 1;
        }
        Ok(progressed)
    }
}
