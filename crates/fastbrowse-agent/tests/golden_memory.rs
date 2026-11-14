//! `shows`, the whole-word quote check, on what was recorded.

mod common;

use common::*;
use fastbrowse_agent::memory::shows;
use serde_json::json;

#[test]
fn whole_word_quotes_match() {
    let golden = golden("memory.json");
    for case in golden["shows"].as_array().unwrap() {
        let shown = shows(case["text"].as_str().unwrap(), case["quote"].as_str().unwrap());
        assert_eq!(json!(shown), case["shows"], "{case}");
    }
}
