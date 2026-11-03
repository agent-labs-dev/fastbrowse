//! Recipe upload validation and client generation use the same contract as the file loader.

use std::path::Path;

use fastbrowse_core::recipe::{Recipe, schema};

#[test]
fn the_recipe_file_schema_is_current() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/recipe.schema.json");
    let written = serde_json::to_string_pretty(&schema()).unwrap() + "\n";
    if std::env::var("FASTBROWSE_UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::write(&path, &written).unwrap();
    }
    assert_eq!(std::fs::read_to_string(path).unwrap_or_default(), written);
}

#[test]
fn uploads_and_files_refuse_the_same_invalid_documents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recipe.json");
    for text in [
        r#"{"version":2,"task":"Review","steps":[]}"#,
        r#"{"version":1,"task":"Review","steps":[],"extra":true}"#,
        r#"{"version":1,"task":"Review","steps":[{"operation":"invent","page":{}}]}"#,
    ] {
        std::fs::write(&path, text).unwrap();
        assert!(Recipe::from_json(text).is_err());
        assert!(Recipe::load(&path).is_err());
    }
}

#[test]
fn recipe_starts_follow_navigation_policy_when_loaded_or_saved() {
    let dir = tempfile::tempdir().unwrap();
    for (index, start) in [
        "https://example.com/",
        "https:example.com",
        "ht\ttps://example.com",
        " https://example.com/",
        "https://user:password@example.com/",
        "https://example.com:0/",
        "https:",
        "file:///tmp/local.html",
        "data:text/plain,hello",
    ]
    .iter()
    .enumerate()
    {
        let expected = fastbrowse_core::navigation::valid_address(start);
        let value = serde_json::json!({"version": 1, "task": "Review", "start": start, "steps": []});
        assert_eq!(Recipe::from_json(&value.to_string()).is_ok(), expected, "parse {start:?}");
        let recipe: Recipe = serde_json::from_value(value).unwrap();
        let path = dir.path().join(format!("{index}.json"));
        assert_eq!(recipe.save(&path).is_ok(), expected, "save {start:?}");
        assert_eq!(path.exists(), expected);
    }
}
