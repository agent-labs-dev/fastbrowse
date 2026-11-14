//! A list read stays bound to the list and tabs Jev chose, while each page uses the normal loop's checks.

use fastbrowse_core::navigation::same_address;
use fastbrowse_core::page::Observation;
use fastbrowse_core::safety::origin_of;

use super::state::{RunState, read_key};

pub(super) struct ListWalk {
    start: String,
    tabs: Vec<(String, bool)>,
    next: String,
}

pub(super) fn offered(state: &RunState, observation: &Observation) -> bool {
    state.extent.walks() && state.in_range(&observation.url) && has_pager(observation)
}

pub(super) fn has_pager(observation: &Observation) -> bool {
    observation.dialog.is_none()
        && observation.modal.is_none()
        && observation.response_status.is_none_or(|status| status < 400)
        && observation.controls.iter().any(|control| {
            fastbrowse_core::page::pager_link(control)
                && control.href.as_ref().is_some_and(|href| {
                    url::Url::parse(&observation.url)
                        .ok()
                        .and_then(|here| here.join(href).ok().map(|next| next.origin() == here.origin()))
                        .unwrap_or(false)
                })
        })
}

impl ListWalk {
    pub fn new(state: &RunState, observation: &Observation, next: String) -> Option<Self> {
        Some(Self {
            start: state.pager_start.clone()?,
            tabs: observation.tabs.iter().map(|tab| (tab.id.clone(), tab.active)).collect(),
            next,
        })
    }

    pub fn continues(&self, state: &RunState, observation: &Observation) -> bool {
        let reached = same_address(&observation.url, &self.next)
            || state.arrived_by.as_ref().is_some_and(|(landed, requested)| {
                *landed == read_key(observation) && same_address(requested, &self.next)
            });
        reached
            && state.pager_start.as_deref() == Some(&self.start)
            && origin_of(&observation.url) == origin_of(&self.start)
            && state.in_range(&observation.url)
            && observation.dialog.is_none()
            && observation.modal.is_none()
            && observation.response_status.is_none_or(|status| status < 400)
            && self.tabs.len() == observation.tabs.len()
            && self.tabs.iter().zip(&observation.tabs).all(|((id, active), tab)| *id == tab.id && *active == tab.active)
    }
}
