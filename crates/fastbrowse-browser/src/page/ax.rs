//! Typed, generation-bound accessibility state used by incremental snapshot reads.

use std::collections::{HashMap, HashSet};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AxNode {
    pub node_id: String,
    pub backend_dom_node_id: Option<i64>,
    #[serde(default)] pub role: Option<Value>,
    #[serde(default)] pub name: Option<Value>,
    #[serde(default)] pub value: Option<Value>,
    #[serde(default)] pub properties: Vec<Value>,
    #[serde(default)] pub child_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct AxIndex {
    pub generation: u64,
    pub document_key: String,
    nodes: HashMap<String, AxNode>,
    by_backend_id: HashMap<i64, String>,
    dirty_backend_ids: HashSet<i64>,
}

impl AxIndex {
    pub(crate) fn replace(&mut self, document_key: String, generation: u64, nodes: Vec<AxNode>) {
        self.document_key = document_key;
        self.generation = generation;
        self.nodes.clear(); self.by_backend_id.clear(); self.dirty_backend_ids.clear();
        for node in nodes {
            if let Some(backend_id) = node.backend_dom_node_id { self.by_backend_id.insert(backend_id, node.node_id.clone()); }
            self.nodes.insert(node.node_id.clone(), node);
        }
    }
    #[allow(dead_code)]
    pub(crate) fn invalidate_backend(&mut self, backend_id: i64, generation: u64) { self.generation = generation; self.dirty_backend_ids.insert(backend_id); }
    #[allow(dead_code)]
    pub(crate) fn node_for_backend(&self, backend_id: i64) -> Option<&AxNode> { self.by_backend_id.get(&backend_id).and_then(|id| self.nodes.get(id)) }
    #[allow(dead_code)]
    pub(crate) fn dirty(&self) -> impl Iterator<Item = i64> + '_ { self.dirty_backend_ids.iter().copied() }
}

#[cfg(test)]
mod tests {
    use super::{AxIndex, AxNode};
    fn node(id: &str, backend: i64) -> AxNode { AxNode { node_id: id.into(), backend_dom_node_id: Some(backend), role: None, name: None, value: None, properties: vec![], child_ids: vec![] } }
    #[test]
    fn baseline_binds_backend_ids_and_replaces_documents() {
        let mut index = AxIndex::default(); index.replace("doc-a".into(), 3, vec![node("ax-a", 11)]);
        assert_eq!(index.node_for_backend(11).map(|n| n.node_id.as_str()), Some("ax-a"));
        index.replace("doc-b".into(), 4, vec![node("ax-b", 22)]);
        assert_eq!(index.node_for_backend(11), None); assert_eq!(index.node_for_backend(22).map(|n| n.node_id.as_str()), Some("ax-b"));
    }
    #[test]
    fn invalidation_marks_only_changed_backend_nodes() {
        let mut index = AxIndex::default(); index.replace("doc".into(), 1, vec![node("ax-a", 11), node("ax-b", 12)]); index.invalidate_backend(12, 2);
        assert_eq!(index.generation, 2); assert_eq!(index.dirty().collect::<Vec<_>>(), vec![12]); assert!(index.node_for_backend(11).is_some());
    }
}
