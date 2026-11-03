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
