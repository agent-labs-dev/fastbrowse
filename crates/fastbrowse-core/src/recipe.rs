//! Recipes: the steps a completed run took, kept by what each control means rather than where it sat, so a later run
//! can reuse them after a live model decision.
//!
//! A step names its page by its full address and its control by role, label, context and the attributes a page keeps
//! across loads. Control ids are per document and never stored. A resolved secret is never stored either: a field
//! filled from a secret keeps only the secret's name, and text typed into a sensitive field any other way is not kept.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::models::Operation;
use crate::page::Control;
use crate::schema::{Field, ModelTy, Schema, Ty};

pub const RECIPE_VERSION: u32 = 1;

/// Operations a recipe can carry out again. A drag, upload, dialog, tab switch or opened address depends on run state
/// a recipe does not hold, so it is recorded in its place but left to the agent on replay. Select choices are
/// decided live because the requested option may depend on current page evidence.
pub const REPLAYABLE: &[Operation] = &[
    Operation::Click,
    Operation::Enter,
    Operation::Fill,
    Operation::Press,
    Operation::Hover,
    Operation::Scroll,
    Operation::ScrollUp,
    Operation::Back,
    Operation::Escape,
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub version: u32,
    pub task: String,
    #[serde(default)]
    pub start: Option<String>,
    pub steps: Vec<RecipeStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipePage {
    pub origin: String,
    pub path: String,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub fragment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeTarget {
    pub role: String,
    pub label: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub input_type: Option<String>,
    #[serde(default)]
    pub input_name: Option<String>,
    #[serde(default)]
    pub frame_origin: Option<String>,
}

/// Where a fill or press value comes from: typed as written, a caller input by name, or a stored secret by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RecipeText {
    Literal(String),
    Live,
    Input(String),
    Secret(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeStep {
    pub operation: Operation,
    pub page: RecipePage,
    #[serde(default)]
    pub target: Option<RecipeTarget>,
    #[serde(default)]
    pub text: Option<RecipeText>,
    /// The option a select step chose.
    #[serde(default)]
    pub option: Option<String>,
    /// Whether the original run read evidence before this step. Live reads use Jev's current page judgement.
    #[serde(default)]
    pub read_first: bool,
    /// False when the step cannot be carried out again from the recipe alone; the agent decides it on replay.
    #[serde(default = "yes")]
    pub replay: bool,
}

impl Recipe {
    /// Validate the same document before accepting an upload or opening a recipe file.
    pub fn from_json(text: &str) -> Result<Recipe, String> {
        let recipe: Recipe = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if recipe.version != RECIPE_VERSION {
            return Err(format!("recipe version {}; this fastbrowse reads version {RECIPE_VERSION}", recipe.version));
        }
        if recipe.start.as_deref().is_some_and(|start| !crate::navigation::valid_address(start)) {
            return Err("recipe start must be a credential-free http or https URL with a usable host and port".into());
        }
        Ok(recipe)
    }

    /// Read a recipe file, refusing one written in a format this version does not know.
    pub fn load(path: &std::path::Path) -> Result<Recipe, String> {
        let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::from_json(&text).map_err(|error| format!("{} is not a recipe: {error}", path.display()))
    }

    /// Refuse an unwritable destination before a browser run can change the site.
    pub fn check_destination(path: &std::path::Path) -> std::io::Result<()> {
        temporary(path).map(drop)
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let mut text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        Self::from_json(&text).map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        text.push('\n');
        let mut file = temporary(path)?;
        use std::io::Write;
        file.write_all(text.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

/// The file contract, shared by embedders and client generators.
pub fn schema() -> serde_json::Value {
    crate::schema::json_schema(&Recipe::ty())
}

impl Schema for Recipe {
    fn ty() -> Ty {
        recipe_model(
            "Recipe",
            vec![
                Field::required("version", Ty::Literal(vec![RECIPE_VERSION.into()])),
                Field::required("task", Ty::Str),
                Field::default("start", Ty::optional(Ty::Str), serde_json::Value::Null).doc(
                    "A credential-free HTTP(S) address. Validate its URL semantics with validate_recipe before use.",
                ),
                Field::required("steps", Ty::list(RecipeStep::ty())),
            ],
        )
    }
}

impl Schema for RecipePage {
    fn ty() -> Ty {
        let mut fields = vec![Field::required("origin", Ty::Str), Field::required("path", Ty::Str)];
        fields.extend(optional_text(&["query", "fragment"]));
        recipe_model("RecipePage", fields)
    }
}

impl Schema for RecipeTarget {
    fn ty() -> Ty {
        let mut fields = vec![Field::required("role", Ty::Str), Field::required("label", Ty::Str)];
        fields.extend(optional_text(&["context", "href", "input_type", "input_name", "frame_origin"]));
        recipe_model("RecipeTarget", fields)
    }
}

impl Schema for RecipeText {
    fn ty() -> Ty {
        Ty::Union(vec![
            Ty::literal("live"),
            ModelTy::frozen("RecipeLive", vec![Field::required("live", Ty::Null)]).ty(),
            ModelTy::frozen("RecipeLiteral", vec![Field::required("literal", Ty::Str)]).ty(),
            ModelTy::frozen("RecipeInput", vec![Field::required("input", Ty::Str)]).ty(),
            ModelTy::frozen("RecipeSecret", vec![Field::required("secret", Ty::Str)]).ty(),
        ])
    }
}

impl Schema for RecipeStep {
    fn ty() -> Ty {
        recipe_model(
            "RecipeStep",
            vec![
                Field::required("operation", Operation::ty()),
                Field::required("page", RecipePage::ty()),
                Field::default("target", Ty::optional(RecipeTarget::ty()), serde_json::Value::Null),
                Field::default("text", Ty::optional(RecipeText::ty()), serde_json::Value::Null),
                Field::default("option", Ty::optional(Ty::Str), serde_json::Value::Null),
                Field::default("read_first", Ty::Bool, false),
                Field::default("replay", Ty::Bool, true),
            ],
        )
    }
}

fn recipe_model(name: &str, fields: Vec<Field>) -> Ty {
    // Serde also reads structs as positional arrays, with defaults for omitted trailing fields.
    let required = fields.iter().rposition(|field| field.default.is_none()).map_or(0, |index| index + 1);
    let sequence = Ty::Tuple(fields.iter().map(|field| field.ty.clone()).collect()).constrained("minItems", required);
    Ty::Union(vec![ModelTy::frozen(name, fields).ty(), sequence])
}

fn optional_text(names: &[&str]) -> Vec<Field> {
    names.iter().map(|name| Field::default(name, Ty::optional(Ty::Str), serde_json::Value::Null)).collect()
}

fn temporary(path: &std::path::Path) -> std::io::Result<tempfile::NamedTempFile> {
    if path.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "the recipe destination is a directory"));
    }
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    tempfile::NamedTempFile::new_in(parent)
}

fn yes() -> bool {
    true
}

impl RecipeStep {
    pub fn replayable(&self) -> bool {
        self.replay
            && REPLAYABLE.contains(&self.operation)
            && (!self.operation.is_targeted() || self.target.is_some())
            && (!matches!(self.operation, Operation::Fill | Operation::Press)
                || self.text.as_ref().is_some_and(|text| !matches!(text, RecipeText::Live)))
    }
}

impl RecipePage {
    pub fn of(url: &str) -> Option<RecipePage> {
        let parsed = Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        Some(RecipePage {
            origin: parsed.origin().ascii_serialization(),
            path: parsed.path().to_string(),
            query: parsed.query().map(str::to_owned),
            fragment: parsed.fragment().map(str::to_owned),
        })
    }
}

impl RecipePage {
    /// A different URL may name a different record, even when its controls have the same labels.
    pub fn matches(&self, other: &RecipePage) -> bool {
        self == other
    }
}

impl RecipeTarget {
    pub fn of(control: &Control, page_url: &str) -> RecipeTarget {
        RecipeTarget {
            role: control.role.clone(),
            label: control.label.clone(),
            context: control.context.clone(),
            href: control.href.as_deref().and_then(|href| resolve(page_url, href)),
            input_type: control.input_type.clone(),
            input_name: control.input_name.clone(),
            frame_origin: control.frame_origin.clone(),
        }
    }

    /// Whether `control` on the page at `page_url` is the control this target names. Every kept attribute must agree,
    /// as `act_on_twin` requires of a replacement node; a link must lead to the same resolved address.
    pub fn matches(&self, control: &Control, page_url: &str) -> bool {
        self.role == control.role
            && self.label == control.label
            && self.context == control.context
            && self.input_type == control.input_type
            && self.input_name == control.input_name
            && self.frame_origin == control.frame_origin
            && self.href == control.href.as_deref().and_then(|href| resolve(page_url, href))
    }
}

/// The one control on the page the target names, or None when none or several do: a guess between twins is the
/// choice a model is for.
pub fn find<'a>(target: &RecipeTarget, controls: &'a [Control], page_url: &str) -> Option<&'a Control> {
    let mut matching = controls.iter().filter(|control| target.matches(control, page_url));
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

fn resolve(base: &str, href: &str) -> Option<String> {
    let base = Url::parse(base).ok()?;
    let mut url = base.join(href).ok()?;
    url.set_fragment(None);
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(id: &str, role: &str, label: &str) -> Control {
        Control::new(id, role, label, [])
    }

    #[test]
    fn a_unique_control_matches_whatever_its_id() {
        let target = RecipeTarget::of(&control("a:1", "button", "Add to cart"), "https://shop.test/p/pico");
        let page = [control("b:7", "button", "Add to cart"), control("b:8", "button", "Checkout")];
        assert_eq!(find(&target, &page, "https://shop.test/p/pico").map(|c| c.id.as_str()), Some("b:7"));
    }

    #[test]
    fn twins_are_left_to_the_model() {
        let target = RecipeTarget::of(&control("a:1", "button", "Add to cart"), "https://shop.test/");
        let page = [control("b:1", "button", "Add to cart"), control("b:2", "button", "Add to cart")];
        assert!(find(&target, &page, "https://shop.test/").is_none());
    }

    #[test]
    fn a_relative_link_matches_its_resolved_address() {
        let link = Control { href: Some("/p/pico".into()), ..control("a:1", "link", "Pico") };
        let target = RecipeTarget::of(&link, "https://shop.test/search?q=pico");
        assert_eq!(target.href.as_deref(), Some("https://shop.test/p/pico"));
        let absolute = Control { href: Some("https://shop.test/p/pico#top".into()), ..control("b:1", "link", "Pico") };
        assert!(target.matches(&absolute, "https://shop.test/other"));
        let elsewhere = Control { href: Some("/p/bme280".into()), ..control("b:2", "link", "Pico") };
        assert!(!target.matches(&elsewhere, "https://shop.test/"));
    }

    #[test]
    fn page_identity_includes_query_fragment_and_generated_segments() {
        let page = RecipePage::of("https://shop.test/search?q=pico#results").unwrap();
        for other in [
            "https://shop.test/search?q=other#results",
            "https://shop.test/search?q=pico#other",
            "https://other.test/search?q=pico#results",
        ] {
            assert!(!page.matches(&RecipePage::of(other).unwrap()));
        }
        assert!(
            !RecipePage::of("https://shop.test/orders/3f2a9c1e77b04d4e")
                .unwrap()
                .matches(&RecipePage::of("https://shop.test/orders/0b9d1c2e11114aaa").unwrap())
        );
        assert!(RecipePage::of("about:blank").is_none());
    }

    #[test]
    fn a_step_missing_what_it_needs_is_not_replayed() {
        let page = RecipePage::of("https://shop.test/").unwrap();
        let step = RecipeStep {
            operation: Operation::Fill,
            page,
            target: Some(RecipeTarget::of(&control("a:1", "textbox", "Email"), "https://shop.test/")),
            text: None,
            option: None,
            read_first: false,
            replay: true,
        };
        assert!(!step.replayable());
        let filled = RecipeStep { text: Some(RecipeText::Input("email".into())), ..step.clone() };
        assert!(filled.replayable());
        assert!(!RecipeStep { replay: false, ..filled.clone() }.replayable());
        assert!(!RecipeStep { operation: Operation::Upload, ..filled }.replayable());
    }

    #[test]
    fn replacing_a_recipe_is_atomic_and_a_failed_save_preserves_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipe.json");
        let original = Recipe { version: RECIPE_VERSION, task: "original".into(), start: None, steps: vec![] };
        Recipe::check_destination(&path).unwrap();
        assert!(!path.exists());
        assert!(Recipe::check_destination(dir.path()).is_err());
        assert!(Recipe::check_destination(&dir.path().join("missing/recipe.json")).is_err());
        original.save(&path).unwrap();
        let replacement = Recipe { task: "replacement".into(), ..original };
        replacement.save(&path).unwrap();
        assert_eq!(Recipe::load(&path).unwrap(), replacement);
        assert!(replacement.save(&dir.path().join("missing/recipe.json")).is_err());
        assert_eq!(Recipe::load(&path).unwrap(), replacement);
    }

    #[test]
    fn recipe_starts_are_web_addresses() {
        for start in ["file:///etc/passwd", "javascript:alert(1)", "data:text/html,local", "not a URL"] {
            let value = serde_json::json!({"version": 1, "task": "t", "start": start, "steps": []});
            assert!(Recipe::from_json(&value.to_string()).is_err(), "accepted {start}");
        }
        for start in ["https://example.com/", "http://localhost:8080/", "HTTPS://example.com/"] {
            let value = serde_json::json!({"version": 1, "task": "t", "start": start, "steps": []});
            assert!(Recipe::from_json(&value.to_string()).is_ok(), "refused {start}");
        }
    }

    #[test]
    fn text_sources_round_trip() {
        let text = RecipeText::Secret("password".into());
        let json = serde_json::to_string(&text).unwrap();
        assert_eq!(json, r#"{"secret":"password"}"#);
        assert_eq!(serde_json::from_str::<RecipeText>(&json).unwrap(), text);
    }
}
