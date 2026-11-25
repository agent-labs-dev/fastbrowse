//! Reading more of a list or a page in one turn: the pages a next-page link leads through, and the lines or controls a
//! windowed page left out. Each tool's result reaches the model as a note in its next turn, and nothing it reads is cut.

use std::collections::HashSet;

use fastbrowse_core::models::Status;
use fastbrowse_core::page::{Observation, pager_link};

use super::Agent;
use super::render::{line, page_text};
use super::state::{RunState, Step};
use crate::memory::address;

impl Agent {
    /// Follows `id`, a next-page link, and each next page's own, reading every page into the evidence a done's quotes and
    /// tallies are checked against. The model chose to stop at `max_pages` when it gave one; otherwise the list's last
    /// page, the one with no next-page link, ends it. A next page the run already reached ends it too, so a pager that
    /// loops reads each page once.
    pub(super) async fn read_pages(
        &self,
        state: &mut RunState,
        observation: &Observation,
        id: &str,
        max_pages: Option<u64>,
    ) -> Step {
        state.chat.queue.clear();
        let named = id;
        let id = state.chat.id_of(&observation.document_key, id);
        let Some(link) = observation.controls.iter().find(|control| control.id == id) else {
            state.chat.notices.push(format!("read_pages was refused: there is no control [{named}] on the page."));
            return self.count_progress(state, false, observation, None.into(), Status::Stuck).await;
        };
        if !pager_link(link) {
            state.chat.notices.push(format!(
                "read_pages was refused: [{named}] is not a link its page marks as the next page of a list; use one whose \
                 line says it leads to the next page of a list."
            ));
            return self.count_progress(state, false, observation, None.into(), Status::Stuck).await;
        }
        let mut reached: HashSet<String> = HashSet::from([address(&observation.url)]);
        let mut read: Vec<String> = Vec::new();
        let mut here = observation.clone();
        let mut next = link.clone();
        let ended = loop {
            if max_pages.is_some_and(|max| read.len() as u64 >= max) {
                break false;
            }
            let Some(url) = next.href.as_deref().and_then(|href| url::Url::parse(&here.url).ok()?.join(href).ok())
            else {
                break true;
            };
            if !reached.insert(address(url.as_str())) {
                break true;
            }
            self.open_by_code(state, &here, url.to_string()).await?;
            let landed = self.observe().await?;
            self.arrive(state, &landed).await?;
            reached.insert(address(&landed.url));
            let capture = self.capture().await?;
            if !state.chat.shown.iter().any(|shown| shown.sha256 == capture.sha256) {
                state.chat.shown.push(capture);
            }
            // The page's records are in the evidence whole, so it is read: counted unread, finishing reopened every
            // page the model had paged through.
            state.read_urls.insert(url.to_string());
            state.read_urls.insert(landed.url.clone());
            read.push(landed.url.clone());
            here = landed;
            match here.controls.iter().find(|control| pager_link(control)) {
                Some(control) => next = control.clone(),
                None => break true,
            }
        };
        let listed = read.iter().map(|url| format!("  {url}")).collect::<Vec<_>>().join("\n");
        let end = if ended {
            "the last page read has no further next-page link, so the list ended there"
        } else {
            "you stopped at max_pages; the last page read links to a further page"
        };
        state.chat.notices.push(format!(
            "read_pages read {} more page(s) of the list, {end}:\n{listed}\nTheir text is held for done: a count, \
             total, minimum or maximum cites them by address in `pages`, and read_text(page, from, lines) reads any \
             lines of one. The page below is the last one read.",
            read.len()
        ));
        self.count_progress(state, !read.is_empty(), &here, None.into(), Status::Stuck).await
    }

    /// Shows `lines` lines of a page's text from line `from` (1 is the first): the page in front of the model, or a page
    /// it was shown before, by address.
    pub(super) async fn read_text(
        &self,
        state: &mut RunState,
        observation: &Observation,
        page: Option<&str>,
        from: u64,
        lines: u64,
    ) -> Step {
        state.chat.queue.clear();
        let (url, text) = match page {
            None => (observation.url.clone(), page_text(&self.capture().await?)),
            Some(wanted) => match state.chat.shown.iter().rev().find(|shown| address(&shown.url) == address(wanted)) {
                Some(shown) => (shown.url.clone(), page_text(shown)),
                None => {
                    state.chat.notices.push(format!(
                        "read_text was refused: no page shown to you in this run has the address {wanted:?}."
                    ));
                    return self.count_progress(state, false, observation, None.into(), Status::Stuck).await;
                }
            },
        };
        let all: Vec<&str> = text.lines().collect();
        let first = from.max(1);
        if first as usize > all.len() {
            state.chat.notices.push(format!("read_text was refused: the page has {} lines.", all.len()));
            return self.count_progress(state, false, observation, None.into(), Status::Stuck).await;
        }
        let last = ((first - 1).saturating_add(lines) as usize).min(all.len());
        let new = state.chat.read_ranges.insert((address(&url), first, last as u64));
        state.chat.notices.push(format!(
            "read_text shows lines {first} to {last} of {} of {url}:\n{}",
            all.len(),
            all[first as usize - 1..last].join("\n")
        ));
        self.count_progress(state, new, observation, None.into(), Status::Stuck).await
    }

    /// Lists `count` of the controls outside the viewport, from the `from`th (1 is the first), under the numbers the
    /// model names them by.
    pub(super) async fn read_controls(
        &self,
        state: &mut RunState,
        observation: &Observation,
        from: u64,
        count: u64,
    ) -> Step {
        state.chat.queue.clear();
        let outside: Vec<_> = observation.controls.iter().filter(|control| control.offscreen).collect();
        let first = from.max(1) as usize;
        if first > outside.len() {
            state
                .chat
                .notices
                .push(format!("read_controls was refused: {} controls are outside the viewport.", outside.len()));
            return self.count_progress(state, false, observation, None.into(), Status::Stuck).await;
        }
        let last = (first - 1).saturating_add(count as usize).min(outside.len());
        let mut out =
            format!("read_controls shows controls {first} to {last} of {} outside the viewport:\n", outside.len());
        for control in &outside[first - 1..last] {
            let mut shown = (*control).clone();
            shown.id = state.chat.number(&observation.document_key, &control.id);
            out += &line(&shown, false);
            out.push('\n');
        }
        let new = state.chat.read_ranges.insert((format!("controls {}", observation.url), first as u64, last as u64));
        state.chat.notices.push(out);
        self.count_progress(state, new, observation, None.into(), Status::Stuck).await
    }
}
