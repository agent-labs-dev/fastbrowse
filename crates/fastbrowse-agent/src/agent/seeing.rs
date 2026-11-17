//! Clicks placed by what Jev sees in the viewport screenshot rather than by the DOM: a covered control the hit test
//! refused, and a place inside a drawn surface, which has no elements to target.

use fastbrowse_clients::validation::jev_purpose;
use fastbrowse_core::decision::{Answer, BoxQuestion, DecisionError, Images, NoulQuestion, Question};
use fastbrowse_core::models::{CostComponent, JevPurpose, UNTRUSTED};
use fastbrowse_core::page::{ActResult, Control, Observation};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::Agent;
use super::state::{RunState, Step};
use crate::policy::{element, screenshot_reference};

const SHOWN: &str = "Is the control described in `control` shown in `viewport_screenshot` with nothing drawn over it, \
                     such as a dialog, banner or menu, so that a click on it would press that control?";
const WHERE: &str = "The control described in `control`, in `viewport_screenshot`.";

const DRAWN_SHOWN: &str = "The page element described in `surface` is drawn: it shows its content as pixels, with no \
                           elements inside it. Is the next step of `task` one click on something shown inside that \
                           surface in `viewport_screenshot`, with that thing visible and nothing drawn over it?";
const DRAWN_WHERE: &str = "The thing inside the surface described in `surface` that the next step of `task` must \
                           click, in `viewport_screenshot`.";

/// One located click: what Jev is shown and asked, and the names its events go under.
struct Sighting<'a> {
    state: Value,
    shown: &'a str,
    yes: &'a str,
    no: &'a str,
    place: &'a str,
    /// The control whose redraw voids the answer.
    target: &'a Control,
    /// Whether the click must land on `target`. A covered control's point is on its cover by definition.
    on_target: bool,
    event: &'a str,
}

impl Agent {
    /// The hit test refused the click: another element is topmost at every point it tried. Jev, shown the viewport,
    /// says whether the control is plainly there to press and where. None leaves the click refused.
    pub(super) async fn click_where_seen(
        &self,
        state: &mut RunState,
        observation: &Observation,
        target: &Control,
    ) -> Step<Option<ActResult>> {
        let Some(png) = self.viewport().await? else { return Ok(None) };
        let sighting = Sighting {
            state: json!({"task": self.model_task(state), "control": element(target), "viewport_screenshot": screenshot_reference(&png)}),
            shown: SHOWN,
            yes: "The control is visible and nothing covers it.",
            no: "It is hidden, covered or absent.",
            place: WHERE,
            target,
            on_target: false,
            event: "covered",
        };
        self.click_sighted(state, observation, png, sighting).await
    }

    /// The task's next step is inside a drawn surface: a map, chart or canvas app shows what it needs as pixels, with
    /// no element to target. Jev says whether that step is one click on something it sees there, and where. None means
    /// the run cannot make the step.
    pub(super) async fn click_inside_drawn(
        &self,
        state: &mut RunState,
        observation: &Observation,
        surface: &Control,
    ) -> Step<Option<ActResult>> {
        let Some(png) = self.viewport().await? else { return Ok(None) };
        let sighting = Sighting {
            state: json!({"task": self.model_task(state), "surface": element(surface), "viewport_screenshot": screenshot_reference(&png)}),
            shown: DRAWN_SHOWN,
            yes: "The step is one click on something visible inside the surface.",
            no: "The step needs typing, dragging or another action, or what it needs is hidden, covered or absent.",
            place: DRAWN_WHERE,
            target: surface,
            // A point Jev placed outside the surface would press an element no decision chose.
            on_target: true,
            event: "drawn",
        };
        self.click_sighted(state, observation, png, sighting).await
    }

    /// The click lands at the centre of Jev's box when both answers reach the bar an action's target is held to, and
    /// the page is still the one `observation` and the screenshot show.
    async fn click_sighted(
        &self,
        state: &mut RunState,
        observation: &Observation,
        png: Vec<u8>,
        sighting: Sighting<'_>,
    ) -> Step<Option<ActResult>> {
        let Sighting { state: seen, shown, yes, no, place, target, on_target, event } = sighting;
        let questions: IndexMap<String, Question> = IndexMap::from([
            ("shown".to_string(), NoulQuestion::new(format!("{UNTRUSTED}\n{shown}"), Some(yes), Some(no)).into()),
            (
                "where".to_string(),
                BoxQuestion { kind: Default::default(), instructions: format!("{UNTRUSTED}\n{place}") }.into(),
            ),
        ]);
        let reference = screenshot_reference(&png);
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let evaluation = match jev_purpose(
            JevPurpose::Locate,
            self.jev.evaluate_images(&seen, &questions, &Images::from([(reference, png.clone())])),
        )
        .await
        {
            Ok(evaluation) => evaluation,
            Err(error @ DecisionError::Budget(_)) => return Err(error.into()),
            // Box is experimental at TypeSafe; a refused question leaves the click unmade.
            Err(error) => {
                self.event(&format!("{event}_box_failed"), || {
                    Map::from_iter([("error".into(), error.to_string().into())])
                });
                return Ok(None);
            }
        };
        state.ledger.record([evaluation.cost.clone()])?;
        let (Some(Answer::Noul(shown)), Some(Answer::Box(found))) =
            (evaluation.answers.get("shown"), evaluation.answers.get("where"))
        else {
            return Ok(None);
        };
        let bar = fastbrowse_core::decision::EVEN_ODDS;
        if shown.probability < bar || found.confidence.is_some_and(|confidence| confidence < bar) {
            return Ok(None);
        }
        // The box is a point in the screenshot's page: on another document, tab or redrawn control it presses whatever
        // is there now, which the gate on this target never authorized.
        if self.page.document_changed(observation).await?
            || self.page.redrawn(observation, 0.0, Some(&target.id)).await?
        {
            return Ok(None);
        }
        // Coordinates belong to the pixels Jev saw; scrolling and layout can change without semantic guards changing.
        let Some(current) = self.viewport().await? else { return Ok(None) };
        if current != png {
            self.event(&format!("{event}_box_changed"), Map::new);
            return Ok(None);
        }
        let (x, y) = found.edges.centre();
        Ok(Some(self.page.click_viewport(x, y, on_target.then_some(target.id.as_str())).await?))
    }
}
