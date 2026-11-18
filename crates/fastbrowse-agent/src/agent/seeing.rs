//! Clicks placed by what Jev sees in the viewport screenshot rather than by the DOM: a covered control the hit test
//! refused, and a place inside a drawn surface, which has no elements to target.

use fastbrowse_clients::validation::jev_purpose;
use fastbrowse_core::decision::{
    Answer, BoxQuestion, DecisionError, Edges, Images, JEV_IMAGE_SIDE, NoulQuestion, Question, jev_takes_png,
};
use fastbrowse_core::models::{CostComponent, JevPurpose, StepOutcome, UNTRUSTED};
use fastbrowse_core::page::{ActResult, Control, Observation};
use fastbrowse_core::py::text;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::Agent;
use super::state::{RunState, Step};
use crate::policy::{Look, chosen_step, element, keys, screenshot_reference, step_question, task_steps};

const SHOWN: &str = "Is the control described in `control` shown in `viewport_screenshot` with nothing drawn over it, \
                     such as a dialog, banner or menu, so that a click on it would press that control?";
const WHERE: &str = "The control described in `control`, in `viewport_screenshot`.";

const DRAWN_SHOWN: &str = "The page element described in `surface` is drawn: it shows its content as pixels, with no \
                           elements inside it. Is the next step of `task` one click on something shown inside that \
                           surface in `viewport_screenshot`, with that thing visible and nothing drawn over it?";
const DRAWN_STEP: &str = "Which sentence or clause of `task` is the click on that thing for? Choose the task's own \
                          wording.";
const REDRAWN: &str = "`before` is the viewport just before the last click toward `task`, and `after` is the viewport \
                       now. Does `after` show the change that click was meant to make?";
const DRAWN_WHERE: &str = "The thing inside the surface described in `surface` that the next step of `task` must \
                           click, in `viewport_screenshot`.";
const DRAWN_ZOOMED: &str = "The thing inside the surface described in `surface` that the next step of `task` must \
                            click, in `zoomed_view`, an enlarged part of that surface.";

/// One located click: what Jev is shown and asked, and the names its events go under.
struct Sighting<'a> {
    /// What Jev is told besides the screenshot, which `click_sighted` takes and adds as `viewport_screenshot`.
    state: Value,
    shown: &'a str,
    yes: &'a str,
    no: &'a str,
    place: &'a str,
    /// `place` asked of `zoomed_view`, a part of the screenshot around the first answer, to place the click finer.
    zoomed: Option<&'a str>,
    /// The control whose redraw voids the answer.
    target: &'a Control,
    /// Whether the click must land on `target`. A covered control's point is on its cover by definition.
    on_target: bool,
    /// The task's sentences, one of which the click is for. Empty for a covered control, which names itself.
    steps: Vec<String>,
    event: &'a str,
}

/// A located click left unmade because the page is no longer the one the screenshot shows: the step after it looks
/// again rather than ending the run.
fn moved(why: &str) -> ActResult {
    ActResult {
        outcome: StepOutcome::Stale,
        failure: None,
        page_changed: false,
        detail: Some(why.into()),
        moved: false,
    }
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
        let sighting = Sighting {
            state: json!({"task": self.model_task(state), "control": element(target)}),
            shown: SHOWN,
            yes: "The control is visible and nothing covers it.",
            no: "It is hidden, covered or absent.",
            place: WHERE,
            // A control is boxed whole, as large as the page draws it.
            zoomed: None,
            target,
            on_target: false,
            steps: Vec::new(),
            event: "covered",
        };
        self.click_sighted(state, observation, sighting).await
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
        let sighting = Sighting {
            state: json!({"task": self.model_task(state), "surface": element(surface)}),
            shown: DRAWN_SHOWN,
            yes: "The step is one click on something visible inside the surface.",
            no: "The step needs typing, dragging or another action, or what it needs is hidden, covered or absent.",
            place: DRAWN_WHERE,
            zoomed: Some(DRAWN_ZOOMED),
            target: surface,
            // A point Jev placed outside the surface would press an element no decision chose.
            on_target: true,
            steps: task_steps(&state.task),
            event: "drawn",
        };
        self.click_sighted(state, observation, sighting).await
    }

    /// The click lands at the centre of Jev's box when both answers reach the bar an action's target is held to, and
    /// the page is still the one `observation` and the screenshot show.
    async fn click_sighted(
        &self,
        state: &mut RunState,
        observation: &Observation,
        sighting: Sighting<'_>,
    ) -> Step<Option<ActResult>> {
        let Sighting { state: mut seen, shown, yes, no, place, zoomed, target, on_target, steps, event } = sighting;
        let mut questions: IndexMap<String, Question> = IndexMap::from([
            ("shown".to_string(), NoulQuestion::new(format!("{UNTRUSTED}\n{shown}"), Some(yes), Some(no)).into()),
            (
                "where".to_string(),
                BoxQuestion { kind: Default::default(), instructions: format!("{UNTRUSTED}\n{place}") }.into(),
            ),
        ]);
        if let Some(question) = step_question(&steps, DRAWN_STEP) {
            questions.insert(keys::STEP.into(), question);
        }
        // The target is measured before the screenshot, so a viewport resized after it shows as a moved target: a
        // cloud browser's viewport grew from 524 to 568 px between the screenshot and a later measure, and the click
        // landed below the pin Jev had boxed.
        let placed = if on_target { self.page.edges(&target.id).await? } else { None };
        let Some(png) = self.viewport().await? else { return Ok(None) };
        let reference = screenshot_reference(&png);
        seen["viewport_screenshot"] = reference.clone().into();
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
        let (x, y) = match zoomed {
            Some(zoomed) => self.placed_finer(state, &seen, zoomed, &png, &found.edges, event).await?,
            None => found.edges.centre(),
        };
        // The box is a point in the screenshot's page: on another document, tab or redrawn control it presses whatever
        // is there now, which the gate on this target never authorized.
        if self.page.document_changed(observation).await?
            || self.page.redrawn(observation, 0.0, Some(&target.id)).await?
        {
            return Ok(Some(moved("the page changed after the screenshot")));
        }
        // Coordinates belong to the pixels Jev saw; scrolling and layout can change without semantic guards changing.
        // A drawn surface paints itself, as a map fading its tiles in or a playing video does, so its pixels differ
        // after any question; a point inside it keeps its meaning while the surface stays where it was. A covered
        // control's point is on its cover, which only the pixels show.
        let unmoved = if on_target {
            self.page.edges(&target.id).await? == placed
        } else {
            self.viewport().await?.is_some_and(|current| current == png)
        };
        if !unmoved {
            self.event(&format!("{event}_box_changed"), Map::new);
            return Ok(Some(moved("the page moved after the screenshot")));
        }
        let mut result = self.page.click_viewport(x, y, on_target.then_some(target.id.as_str())).await?;
        // Jev found what the step needs, but not on the surface the step chose: Leaflet's red circle, boxed where it
        // is, lies outside the marker image the step had picked. The step aimed at the wrong element, which the next
        // step can correct, so it fails rather than ending the run as an action the surface cannot take.
        if on_target && result.outcome == StepOutcome::Covered {
            self.event(&format!("{event}_box_outside"), Map::new);
            let outside =
                format!("what the step needs is shown outside {}, so nothing was clicked", text::repr(&target.label));
            return Ok(Some(ActResult { outcome: StepOutcome::Failed, detail: Some(outside), ..result }));
        }
        // The history entry shows what the box held, before the click and after it: told only that the canvas was
        // clicked and its picture changed, the step after the Library pin's click could not see which pin it was, and
        // clicked the map again.
        let before = place_card(&png, &found.edges);
        state.sighted = chosen_step(&evaluation, &steps);
        // A click inside a drawn surface is judged by its pixels even when the page also added text: a map's
        // "Directions ready." is no control, so a page state keyed by its controls read the right pin as no progress,
        // and the map was withheld from the steps after it.
        let after =
            if on_target || !result.page_changed { self.judge_by_pixels(state, png, &mut result).await? } else { None };
        state.look =
            before.map(|before| Look { before, after: after.and_then(|after| place_card(&after, &found.edges)) });
        Ok(Some(result))
    }

    /// The point to click for `found`, a box in `png`. Box edges come in hundredths of the image, about 13 CSS px of a
    /// 1265 px viewport, and one hundredth to the left of OpenLayers' 28 px pin was a click on the map beside it. The
    /// same question asked of the largest square of the screenshot around the box places it about twice as finely
    /// (TypeSafe's own advice for a small target). Its answer counts when it clears the bar and falls inside `found`,
    /// the same thing found again; otherwise, or on a square screenshot, which no square crop narrows, the click goes
    /// to the centre of `found`.
    async fn placed_finer(
        &self,
        state: &mut RunState,
        seen: &Value,
        zoomed: &str,
        png: &[u8],
        found: &Edges,
        event: &str,
    ) -> Step<(f64, f64)> {
        let centre = found.centre();
        let Some(image) = rgba(png) else { return Ok(centre) };
        let (width, height) = (image.width(), image.height());
        let side = width.min(height);
        if side == width.max(height) {
            return Ok(centre);
        }
        let start =
            |at: f64, total: u32| (at * total as f64 - side as f64 / 2.0).round().clamp(0.0, (total - side) as f64);
        let (left, top) = (start(centre.0, width), start(centre.1, height));
        let mut part = Vec::new();
        let square = image::imageops::crop_imm(&image, left as u32, top as u32, side, side).to_image();
        if square.write_to(&mut std::io::Cursor::new(&mut part), image::ImageFormat::Png).is_err()
            || !jev_takes_png(&part)
        {
            return Ok(centre);
        }
        let reference = screenshot_reference(&part);
        let mut seen = seen.clone();
        if let Some(seen) = seen.as_object_mut() {
            seen.remove("viewport_screenshot");
            seen.insert("zoomed_view".into(), reference.clone().into());
        }
        let questions: IndexMap<String, Question> = IndexMap::from([(
            "where".to_string(),
            BoxQuestion { kind: Default::default(), instructions: format!("{UNTRUSTED}\n{zoomed}") }.into(),
        )]);
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let evaluation = match jev_purpose(
            JevPurpose::Locate,
            self.jev.evaluate_images(&seen, &questions, &Images::from([(reference, part)])),
        )
        .await
        {
            Ok(evaluation) => evaluation,
            Err(error @ DecisionError::Budget(_)) => return Err(error.into()),
            Err(_) => return Ok(centre),
        };
        state.ledger.record([evaluation.cost.clone()])?;
        let Some(Answer::Box(fine)) = evaluation.answers.get("where") else { return Ok(centre) };
        let (x, y) = fine.edges.centre();
        let point = ((left + x * side as f64) / width as f64, (top + y * side as f64) / height as f64);
        let agrees = (found.left..=found.right).contains(&point.0) && (found.top..=found.bottom).contains(&point.1);
        if fine.confidence.is_some_and(|confidence| confidence < fastbrowse_core::decision::EVEN_ODDS) || !agrees {
            self.event(&format!("{event}_finer_refused"), Map::new);
            return Ok(centre);
        }
        Ok(point)
    }

    /// Judges an executed click by the frames before and after it, for a click whose effect is in pixels: one that
    /// changed no element is reported as a page change when the picture changed as it meant, and one that also added
    /// text says so, since the text alone does not show which pin a map selected. Returns the frame after the click.
    pub(super) async fn judge_by_pixels(
        &self,
        state: &mut RunState,
        before: Vec<u8>,
        result: &mut ActResult,
    ) -> Step<Option<Vec<u8>>> {
        if result.outcome != StepOutcome::Executed {
            return Ok(None);
        }
        let Some(after) = self.viewport().await? else {
            state.drawn_changed = None;
            return Ok(None);
        };
        state.drawn_changed = self.changed_as_meant(state, before, &after).await?;
        if state.drawn_changed == Some(true) {
            const MEANT: &str = "the picture on the page changed as the click meant";
            result.page_changed = true;
            result.detail = Some(match result.detail.take() {
                Some(detail) => format!("{detail}, and {MEANT}"),
                None => MEANT.to_string(),
            });
        }
        Ok(Some(after))
    }

    /// Whether a click that changed no element changed the pixels as it meant to. A frame byte for byte the same as
    /// `before` changed nothing, which needs no question. None when Jev cannot answer.
    async fn changed_as_meant(&self, state: &mut RunState, before: Vec<u8>, after: &[u8]) -> Step<Option<bool>> {
        if after == before {
            return Ok(Some(false));
        }
        let (was, now) = (screenshot_reference(&before), screenshot_reference(after));
        let seen = json!({"task": self.model_task(state), "before": was, "after": now});
        let questions: IndexMap<String, Question> = IndexMap::from([(
            "changed".to_string(),
            NoulQuestion::new(
                format!("{UNTRUSTED}\n{REDRAWN}"),
                Some("It shows the change the click was meant to make."),
                Some("It shows no change, or a different one."),
            )
            .into(),
        )]);
        state.ledger.reserve(CostComponent::Jev, 0.0, 1)?;
        let images = Images::from([(was, before), (now, after.to_vec())]);
        let evaluation =
            match jev_purpose(JevPurpose::Verify, self.jev.evaluate_images(&seen, &questions, &images)).await {
                Ok(evaluation) => evaluation,
                Err(error @ DecisionError::Budget(_)) => return Err(error.into()),
                Err(error) => {
                    self.event("redrawn_failed", || Map::from_iter([("error".into(), error.to_string().into())]));
                    return Ok(None);
                }
            };
        state.ledger.record([evaluation.cost.clone()])?;
        let Some(Answer::Noul(changed)) = evaluation.answers.get("changed") else { return Ok(None) };
        Ok(Some(changed.probability >= fastbrowse_core::decision::EVEN_ODDS))
    }
}

/// Draws `OUTLINE` around `edges`, fractions of a viewport `size`, on `card`, cut from that viewport at `origin`. The
/// line sits just outside the element, so none of its own pixels are covered.
fn outline(card: &mut image::RgbaImage, edges: &Edges, origin: (u32, u32), size: (u32, u32)) {
    const WIDTH: i64 = 3;
    let at = |fraction: f64, total: u32, from: u32| (fraction * total as f64).round() as i64 - from as i64;
    let (left, right) = (at(edges.left, size.0, origin.0) - 1, at(edges.right, size.0, origin.0));
    let (top, bottom) = (at(edges.top, size.1, origin.1) - 1, at(edges.bottom, size.1, origin.1));
    let (w, h) = (card.width() as i64, card.height() as i64);
    for y in (top - WIDTH + 1).max(0)..(bottom + WIDTH).min(h) {
        for x in (left - WIDTH + 1).max(0)..(right + WIDTH).min(w) {
            if x <= left || x >= right || y <= top || y >= bottom {
                card.put_pixel(x as u32, y as u32, OUTLINE);
            }
        }
    }
}

/// TypeSafe's shortest image side, in pixels.
const JEV_IMAGE_MIN_SIDE: u32 = 280;

/// The colour a card's own element is outlined in, named to Jev wherever it reads a card.
const OUTLINE: image::Rgba<u8> = image::Rgba([255, 0, 255, 255]);

/// Each card cut out of `viewport`, a PNG, as a PNG Jev takes, with the card outlined in `OUTLINE` just outside its
/// edges. A card shorter or narrower than Jev's smallest image is widened about its centre, within the viewport, so
/// neighbouring icon buttons are cut alike: three of an editor's toolbar buttons came out as one image, and only the
/// outline says which each is. One Jev would refuse, or one that is the whole viewport, is left out.
pub(super) fn cards(viewport: &[u8], edges: &IndexMap<String, Edges>) -> IndexMap<String, Vec<u8>> {
    let Some(image) = rgba(viewport) else { return IndexMap::new() };
    edges.iter().filter_map(|(id, edges)| Some((id.clone(), cut(&image, edges, true)?))).collect()
}

/// The card of a place Jev's box found, cut as `cards` cuts one but with no outline: the box is Jev's estimate, not the
/// element's own edges, and can sit inside what it marks. A box narrower than the map's Library pin outlined the pin
/// itself, hiding that it had turned from red to blue, and the step after the right click put done at 0.5; with the
/// card unmarked, at 0.9.
pub(super) fn place_card(viewport: &[u8], edges: &Edges) -> Option<Vec<u8>> {
    cut(&rgba(viewport)?, edges, false)
}

fn rgba(png: &[u8]) -> Option<image::RgbaImage> {
    Some(image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.to_rgba8())
}

fn cut(image: &image::RgbaImage, edges: &Edges, outlined: bool) -> Option<Vec<u8>> {
    let span = |from: f64, to: f64, total: u32| {
        let total = total as f64;
        let length = ((to - from) * total).ceil().clamp(JEV_IMAGE_MIN_SIDE as f64, JEV_IMAGE_SIDE as f64).min(total);
        let start = ((from + to) / 2.0 * total - length / 2.0).clamp(0.0, total - length);
        (start.floor() as u32, length as u32)
    };
    let (x, width) = span(edges.left, edges.right, image.width());
    let (y, height) = span(edges.top, edges.bottom, image.height());
    // A card cut to the whole viewport is the viewport screenshot again, with its outline outside the picture, so it
    // tells no element apart. Each click on a Street View canvas kept one such card of about 2.2 MB, and thirteen of
    // them took the step's request over Jev's body limit (#1301).
    if (x, y, width, height) == (0, 0, image.width(), image.height()) {
        return None;
    }
    let mut card = image::imageops::crop_imm(image, x, y, width, height).to_image();
    if outlined {
        outline(&mut card, edges, (x, y), (image.width(), image.height()));
    }
    let mut png = Vec::new();
    card.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    jev_takes_png(&png).then_some(png)
}
