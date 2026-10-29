//! A covered control found in the viewport screenshot rather than by the DOM hit test.

use fastbrowse_core::decision::{Answer, BoxQuestion, DecisionError, Images, NoulQuestion, Question};
use fastbrowse_core::models::{CostComponent, UNTRUSTED};
use fastbrowse_core::page::{ActResult, Control};
use indexmap::IndexMap;
use serde_json::{Map, json};

use super::Agent;
use super::state::{RunState, Step};
use crate::policy::{element, screenshot_reference};

const SHOWN: &str = "Is the control described in `control` shown in `viewport_screenshot` with nothing drawn over it, \
                     such as a dialog, banner or menu, so that a click on it would press that control?";
const WHERE: &str = "The control described in `control`, in `viewport_screenshot`.";

impl Agent {
    /// The hit test refused the click: another element is topmost at every point it tried. Jev, shown the viewport,
    /// says whether the control is plainly there to press and where, and the click lands at the centre of its box when
    /// both answers reach the bar an action's target is held to. None leaves the click refused.
    pub(super) async fn click_where_seen(&self, state: &mut RunState, target: &Control) -> Step<Option<ActResult>> {
        let Some(png) = self.viewport().await? else { return Ok(None) };
        let reference = screenshot_reference(&png);
        let seen = json!({"task": state.task, "control": element(target, false), "viewport_screenshot": reference});
        let questions: IndexMap<String, Question> = IndexMap::from([
            (
                "shown".to_string(),
                NoulQuestion::new(
                    format!("{UNTRUSTED}\n{SHOWN}"),
                    Some("The control is visible and nothing covers it."),
                    Some("It is hidden, covered or absent."),
                )
                .into(),
            ),
            (
                "where".to_string(),
                BoxQuestion { kind: Default::default(), instructions: format!("{UNTRUSTED}\n{WHERE}") }.into(),
            ),
        ]);
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let evaluation = match self.jev.evaluate_images(&seen, &questions, &Images::from([(reference, png)])).await {
            Ok(evaluation) => evaluation,
            Err(error @ DecisionError::Budget(_)) => return Err(error.into()),
            // Box is experimental at TypeSafe; a refused question leaves the click as the hit test left it.
            Err(error) => {
                self.event("covered_box_failed", || Map::from_iter([("error".into(), error.to_string().into())]));
                return Ok(None);
            }
        };
        state.ledger.record([evaluation.cost.clone()])?;
        let (Some(Answer::Noul(shown)), Some(Answer::Box(found))) =
            (evaluation.answers.get("shown"), evaluation.answers.get("where"))
        else {
            return Ok(None);
        };
        let bar = self.config.thresholds.recover_below;
        if shown.probability < bar || found.confidence.is_some_and(|confidence| confidence < bar) {
            return Ok(None);
        }
        let (x, y) = found.edges.centre();
        Ok(Some(self.page.click_viewport(x, y).await?))
    }
}
