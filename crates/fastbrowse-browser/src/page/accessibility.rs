//! The page as the browser's accessibility tree, written as an indented outline for Jev.
//!
//! The flat page text loses structure: which lines are one list's items, which link is that list's pager, which row a
//! price sits in. Agents that read the accessibility tree (Playwright's snapshot, Browser Use, Stagehand) keep it, and
//! with it Jev finished whole-site counts it stopped short of when shown only the text.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};

use super::CdpPage;

/// The states a node carries that change what can be done with it or what it says.
const STATES: &[&str] =
    &["disabled", "focused", "checked", "expanded", "selected", "pressed", "level", "required", "invalid", "url"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    node_id: String,
    #[serde(default)]
    ignored: bool,
    role: Option<Named>,
    name: Option<Named>,
    #[serde(default)]
    properties: Vec<Property>,
    #[serde(default)]
    child_ids: Vec<String>,
}

#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    value: Value,
}

#[derive(Deserialize)]
struct Property {
    name: String,
    value: Named,
}

#[derive(Deserialize)]
struct Tree {
    nodes: Vec<Node>,
}

impl CdpPage {
    /// The active tab's accessibility tree as an outline; empty when the browser will not give one, since the page
    /// text and controls still describe the page without it.
    pub(super) async fn accessibility(&self) -> String {
        let Ok(session_id) = self.session.active_session_id() else { return String::new() };
        let Ok(reply) = self.session.send("Accessibility.getFullAXTree", json!({}), Some(&session_id)).await else {
            return String::new();
        };
        serde_json::from_value::<Tree>(reply).map(|tree| outline(&tree.nodes)).unwrap_or_default()
    }
}

/// One line per node that says something: `- role "name" [state=value]`, children indented under it. Ignored nodes,
/// unnamed wrappers and text repeating its parent's name are skipped, and their children move up a level.
fn outline(nodes: &[Node]) -> String {
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|node| (node.node_id.as_str(), node)).collect();
    let mut out = String::new();
    if let Some(root) = nodes.first() {
        write(root, &by_id, 0, "", &mut out);
    }
    out
}

fn write(node: &Node, by_id: &HashMap<&str, &Node>, depth: usize, parent_name: &str, out: &mut String) {
    let role = node.role.as_ref().and_then(|r| r.value.as_str()).unwrap_or_default();
    // A line box of text Chrome splits for layout: its StaticText parent already holds the words.
    if role == "InlineTextBox" {
        return;
    }
    let name = node.name.as_ref().and_then(|n| n.value.as_str()).unwrap_or_default().trim();
    let silent = node.ignored
        || (name.is_empty() && matches!(role, "" | "generic" | "none" | "GenericContainer" | "LineBreak"))
        || (role == "StaticText" && name == parent_name);
    let mut child_depth = depth;
    if !silent {
        out.push_str(&"  ".repeat(depth));
        out.push_str("- ");
        out.push_str(role);
        if !name.is_empty() {
            out.push(' ');
            out.push_str(&Value::from(name).to_string());
        }
        for property in node.properties.iter().filter(|p| STATES.contains(&p.name.as_str())) {
            match &property.value.value {
                Value::Bool(false) | Value::Null => {}
                Value::String(text) => out.push_str(&format!(" [{}={text}]", property.name)),
                other => out.push_str(&format!(" [{}={other}]", property.name)),
            }
        }
        out.push('\n');
        child_depth += 1;
    }
    let own_name = if silent { parent_name } else { name };
    for child in node.child_ids.iter().filter_map(|id| by_id.get(id.as_str())) {
        write(child, by_id, child_depth, own_name, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(nodes: Value) -> Vec<Node> {
        serde_json::from_value::<Tree>(json!({ "nodes": nodes })).unwrap().nodes
    }

    #[test]
    fn a_list_and_its_pager_keep_their_structure_and_wrappers_and_repeated_text_drop_out() {
        let nodes = tree(json!([
            {"nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Quotes"}, "childIds": ["2"]},
            {"nodeId": "2", "role": {"value": "generic"}, "name": {"value": ""}, "childIds": ["3", "6"]},
            {"nodeId": "3", "role": {"value": "list"}, "childIds": ["4"]},
            {"nodeId": "4", "role": {"value": "listitem"}, "childIds": ["5"]},
            {"nodeId": "5", "role": {"value": "link"}, "name": {"value": "Next \u{2192}"},
             "properties": [{"name": "url", "value": {"value": "https://q.test/page/2/"}},
                            {"name": "focused", "value": {"value": false}}],
             "childIds": ["7"]},
            {"nodeId": "6", "role": {"value": "button"}, "name": {"value": "Go"}, "ignored": true},
            {"nodeId": "7", "role": {"value": "StaticText"}, "name": {"value": "Next \u{2192}"}, "childIds": ["8"]},
            {"nodeId": "8", "role": {"value": "InlineTextBox"}, "name": {"value": "Next \u{2192}"}}
        ]));
        assert_eq!(
            outline(&nodes),
            "- RootWebArea \"Quotes\"\n  - list\n    - listitem\n      - link \"Next \u{2192}\" [url=https://q.test/page/2/]\n"
        );
    }
}
