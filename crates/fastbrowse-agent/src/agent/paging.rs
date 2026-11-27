//! Reading more of a list or a page in one turn: the pages a next-page link leads through, and the lines or controls a
//! windowed page left out. Each tool's result reaches the model as a note in its next turn, and nothing it reads is cut.

use std::collections::HashSet;

use fastbrowse_core::models::Status;
use fastbrowse_core::page::{Observation, pager_link};

use super::Agent;
use super::driver::{Read, read_key};
use super::render::{line, page_text};
use super::state::{Flow, RunState, Step};
use crate::memory::{address, latest_page};

/// Why `read_pages` stopped.
enum End {
    Last,
    MaxPages,
    Looped,
    Refused(String),
    OtherSite(String),
}

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
        let home = url::Url::parse(&observation.url).ok().map(|url| url.origin());
        let mut reached: HashSet<String> = HashSet::from([address(&observation.url)]);
        // The page the list starts on is read whole too, as the pages after it are: the model saw it windowed to its
        // viewport when it was long, and a count over the list needs its records as much as the rest.
        state.chat.keep(self.capture().await?);
        state.read_urls.insert(observation.url.clone());
        let mut read: Vec<String> = vec![observation.url.clone()];
        let mut here = observation.clone();
        let mut next = link.clone();
        let end = loop {
            if max_pages.is_some_and(|max| read.len() as u64 > max) {
                break End::MaxPages;
            }
            // A step the model takes is checked against the run's limits before it starts; a tool that takes many is
            // checked before each.
            state.ledger.check(0.0)?;
            if let Some(stop) = super::out_of_browsing_time(state) {
                return Err(Flow::Stop(stop));
            }
            // Resolved against the address as the browser has it: the masked one hides a credential in the base.
            let base = self.raw().map_or_else(|| here.url.clone(), |raw| raw.url);
            let Some(url) = next.href.as_deref().and_then(|href| url::Url::parse(&base).ok()?.join(href).ok()) else {
                break End::Last;
            };
            if Some(url.origin()) != home {
                break End::OtherSite(url.to_string());
            }
            if !reached.insert(address(url.as_str())) {
                break End::Last;
            }
            if !self.open_by_code(state, &here, url.to_string()).await? {
                break End::Refused(url.to_string());
            }
            let landed = self.observe().await?;
            // A next page that redirects to one already read adds nothing, and the list has gone round.
            if !reached.insert(address(&landed.url)) && address(&landed.url) != address(url.as_str()) {
                here = landed;
                break End::Looped;
            }
            if Some(url::Url::parse(&landed.url).map(|landed| landed.origin()).ok()).flatten() != home {
                let url = landed.url.clone();
                here = landed;
                break End::OtherSite(url);
            }
            let last = !landed.controls.iter().any(pager_link) || max_pages.is_some_and(|max| read.len() as u64 >= max);
            // The loop that drives the model takes in the page it is left on, so the last one is not taken in twice.
            if !last {
                self.arrive(state, &landed).await?;
            }
            state.chat.keep(self.capture().await?);
            // The page's records are in the evidence whole, so it is read: counted unread, finishing reopened every
            // page the model had paged through.
            state.read_urls.insert(url.to_string());
            state.read_urls.insert(landed.url.clone());
            read.push(landed.url.clone());
            here = landed;
            match here.controls.iter().find(|control| pager_link(control)) {
                Some(control) => next = control.clone(),
                None => break End::Last,
            }
        };
        let listed = read.iter().map(|url| format!("  {url}")).collect::<Vec<_>>().join("\n");
        let why = match &end {
            End::Last => "the last page read has no further next-page link, so the list ended there".to_string(),
            End::MaxPages => "you stopped at max_pages; the last page read links to a further page".to_string(),
            End::Looped => "a next page led back to a page already read, so the list ended there".to_string(),
            End::Refused(url) => {
                format!("{url} was not opened (a rule of the run refused it), so the pages after it are unread")
            }
            End::OtherSite(url) => format!("the next page, {url}, is on another site and was not read"),
        };
        // Told to read each page before naming a record from it, the driver ranked the books by hand with no tally, and
        // its answer left out two of the cheapest. Told instead that code works a tally out, it wrote "book and price"
        // as tally.written and named no book: code checks the value or record the driver works out, so it reads them.
        state.chat.notices.push(format!(
            "read_pages read the page you were on and {} more page(s) of the list, {why}:\n{listed}\nTheir text is held \
             for done. A count, total, minimum or maximum over the list's records, or a place after one such as the \
             third cheapest, is a tally: set tally and name these pages in `pages`, and code checks the value or \
             record you write against every record on them. Only the last page read is shown below, so you have not \
             seen the text of the other pages listed: read_text(page, from, lines) reads any lines of one, to work \
             out what a tally writes or to quote a record.",
            read.len() - 1
        ));
        self.count_progress(state, read.len() > 1, &here, None.into(), Status::Stuck).await
    }

    /// Shows `lines` lines of a page's text from line `from` (1 is the first): the page in front of the model or a page
    /// it was shown before, by address. With `find`, the lines that hold it, each under its line number, from the
    /// `from`th such line: a page of twelve thousand lines names no section the model can jump to, and it paged
    /// through it by guessed offsets for seven turns.
    pub(super) async fn read_text(
        &self,
        state: &mut RunState,
        observation: &Observation,
        page: Option<&str>,
        from: u64,
        lines: u64,
        find: Option<&str>,
    ) -> Step {
        state.chat.queue.clear();
        let capture = match page {
            // The page as the model was shown it, unless it has changed since: each read captured it again.
            None if state.chat.planned_on.as_ref() == Some(&read_key(observation))
                && state.chat.shown.last().is_some_and(|shown| shown.url == observation.url) =>
            {
                state.chat.shown.last().cloned().expect("a page was shown")
            }
            None => self.capture().await?,
            Some(wanted) => {
                let held = state.chat.shown.iter().map(|shown| shown.url.as_str());
                match latest_page(held, wanted) {
                    Some(at) => state.chat.shown[at].clone(),
                    None => {
                        state.chat.notices.push(format!(
                            "read_text was refused: no page shown to you in this run has the address {wanted:?}."
                        ));
                        return self.count_progress(state, false, observation, None.into(), Status::Stuck).await;
                    }
                }
            }
        };
        let text = page_text(&capture);
        let all: Vec<&str> = text.lines().collect();
        // The lines wanted, as indexes into the page: the page's lines in order, or those that hold `find`.
        let (wanted, noun, place) = match find.map(str::trim).filter(|find| !find.is_empty()) {
            Some(find) => {
                let needle = find.to_lowercase();
                let at: Vec<usize> = (0..all.len()).filter(|at| all[*at].to_lowercase().contains(&needle)).collect();
                (at, "matches", format!(" of {} for {find:?}", capture.url))
            }
            None => ((0..all.len()).collect(), "lines", format!(" of {}", capture.url)),
        };
        let first = from.max(1);
        if lines == 0 || first as usize > wanted.len() {
            let why = match (lines, noun) {
                (0, _) => "it names no lines".to_string(),
                (_, "matches") => format!("the page has {} lines holding it", wanted.len()),
                _ => format!("the page has {} lines", all.len()),
            };
            state.chat.notices.push(format!("read_text was refused: {why}."));
            return self.count_progress(state, false, observation, None.into(), Status::Stuck).await;
        }
        let last = ((first - 1).saturating_add(lines) as usize).min(wanted.len());
        let shown = &wanted[first as usize - 1..last];
        // Progress is a line of this page text the model had not read, not a different request: the same lines of a
        // page that was redrawn are new, and a range inside one read before is not.
        let seen = state.chat.read_lines.entry((address(&capture.url), capture.sha256.clone())).or_default();
        let before = seen.len();
        seen.extend(shown.iter().map(|at| at + 1));
        let new = seen.len() > before;
        // A match is given under its line number, which `from` and `lines` of a plain read name.
        let lines = shown
            .iter()
            .map(|at| if noun == "matches" { format!("{}: {}", at + 1, all[*at]) } else { all[*at].to_string() })
            .collect();
        state.chat.reads.push(Read {
            tool: "read_text",
            noun,
            first: first as usize,
            total: wanted.len(),
            place,
            lines,
        });
        self.count_progress(state, new, observation, None.into(), Status::Stuck).await
    }

    /// Lists `count` of the controls further than a screen from the viewport, from the `from`th (1 is the first), under
    /// the numbers the model names them by.
    pub(super) async fn read_controls(
        &self,
        state: &mut RunState,
        observation: &Observation,
        from: u64,
        count: u64,
    ) -> Step {
        state.chat.queue.clear();
        // Marked as the page's own controls are, so an offscreen link says whether it leads to a page already read.
        let mut marked = observation.clone();
        state.mark_links(&mut marked);
        let outside: Vec<_> = marked.controls.iter().filter(|control| !super::render::in_window(control)).collect();
        let first = from.max(1) as usize;
        if count == 0 || first > outside.len() {
            let why = if count == 0 {
                "it names no controls".to_string()
            } else {
                format!(
                    "from counts the {total} controls further than a screen from the viewport, 1 to {total}",
                    total = outside.len()
                )
            };
            state.chat.notices.push(format!("read_controls was refused: {why}."));
            return self.count_progress(state, false, observation, None.into(), Status::Stuck).await;
        }
        let last = (first - 1).saturating_add(count as usize).min(outside.len());
        let mut lines = Vec::new();
        let mut new = false;
        for control in &outside[first - 1..last] {
            let mut shown = (*control).clone();
            shown.id = state.chat.number(&observation.document_key, &control.id);
            lines.push(line(&shown, false).trim_end().to_string());
            new |= state.chat.read_controls.insert((observation.document_key.clone(), control.id.clone()));
        }
        state.chat.reads.push(Read {
            tool: "read_controls",
            noun: "controls",
            first,
            total: outside.len(),
            place: " further than a screen from the viewport".into(),
            lines,
        });
        self.count_progress(state, new, observation, None.into(), Status::Stuck).await
    }
}
