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
    control_backend_ids: HashMap<String, i64>,
}

impl AxIndex {
    pub(crate) fn replace(&mut self, document_key: String, generation: u64, nodes: Vec<AxNode>) {
        self.document_key = document_key;
        self.generation = generation;
        self.nodes.clear(); self.by_backend_id.clear(); self.dirty_backend_ids.clear();
        self.control_backend_ids.clear();
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
    pub(crate) fn bind_control(&mut self, control_id: String, backend_id: i64) { self.control_backend_ids.insert(control_id, backend_id); }
    pub(crate) fn backend_for_control(&self, control_id: &str) -> Option<i64> { self.control_backend_ids.get(control_id).copied() }
    /// Merge one complete changed branch. Callers must supply the branch's current nodes, including child order;
    /// an incomplete `getPartialAXTree` response must be rejected before calling this method.
    pub(crate) fn merge_branch(&mut self, backend_id: i64, nodes: Vec<AxNode>, generation: u64) -> bool {
        let Some(root_id) = self.by_backend_id.get(&backend_id).cloned() else { return false };
        if !self.branch_complete(backend_id, &nodes) { return false }
        let mut previous_ids = HashSet::from([root_id.clone()]);
        let mut pending = self.nodes.get(&root_id).map(|node| node.child_ids.clone()).unwrap_or_default();
        while let Some(id) = pending.pop() {
            if !previous_ids.insert(id.clone()) { continue }
            if let Some(node) = self.nodes.get(&id) { pending.extend(node.child_ids.iter().cloned()); }
        }
        let replacement_ids: HashSet<String> = nodes.iter().map(|node| node.node_id.clone()).collect();
        for node in nodes {
            if let Some(old) = self.nodes.insert(node.node_id.clone(), node.clone()) {
                if let Some(old_backend) = old.backend_dom_node_id { self.by_backend_id.remove(&old_backend); }
            }
            if let Some(node_backend) = node.backend_dom_node_id { self.by_backend_id.insert(node_backend, node.node_id.clone()); }
        }
        for removed in previous_ids.difference(&replacement_ids) {
            if let Some(old) = self.nodes.remove(removed) {
                if let Some(old_backend) = old.backend_dom_node_id { self.by_backend_id.remove(&old_backend); }
            }
        }
        self.generation = generation;
        self.dirty_backend_ids.remove(&backend_id);
        true
    }

    pub(crate) fn branch_complete(&self, backend_id: i64, nodes: &[AxNode]) -> bool {
        let Some(root_id) = self.by_backend_id.get(&backend_id) else { return false };
        let mut supplied = HashMap::new();
        for node in nodes { if supplied.insert(node.node_id.as_str(), node).is_some() { return false; } }
        let Some(root) = nodes.iter().find(|node| &node.node_id == root_id) else { return false };
        let mut reachable = HashSet::from([root_id.clone()]);
        let mut pending = root.child_ids.clone();
        while let Some(id) = pending.pop() {
            if !reachable.insert(id.clone()) { return false; }
            let Some(node) = supplied.get(id.as_str()) else { return false };
            pending.extend(node.child_ids.iter().cloned());
        }
        reachable.len() == supplied.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{AxIndex, AxNode};
    fn node(id: &str, backend: i64) -> AxNode { AxNode { node_id: id.into(), backend_dom_node_id: Some(backend), role: None, name: None, value: None, properties: vec![], child_ids: vec![] } }
    #[test]
    fn baseline_binds_backend_ids_and_replaces_documents() {
        let mut index = AxIndex::default(); index.replace("doc-a".into(), 3, vec![node("ax-a", 11)]);
        index.bind_control("control-a".into(), 11);
        assert_eq!(index.backend_for_control("control-a"), Some(11));
        assert_eq!(index.node_for_backend(11).map(|n| n.node_id.as_str()), Some("ax-a"));
        index.replace("doc-b".into(), 4, vec![node("ax-b", 22)]);
        assert_eq!(index.node_for_backend(11), None); assert_eq!(index.node_for_backend(22).map(|n| n.node_id.as_str()), Some("ax-b"));
    }
    #[test]
    fn invalidation_marks_only_changed_backend_nodes() {
        let mut index = AxIndex::default(); index.replace("doc".into(), 1, vec![node("ax-a", 11), node("ax-b", 12)]); index.invalidate_backend(12, 2);
        assert_eq!(index.generation, 2); assert_eq!(index.dirty().collect::<Vec<_>>(), vec![12]); assert!(index.node_for_backend(11).is_some());
    }

    #[test]
    fn partial_branch_replaces_values_and_preserves_explicit_child_order() {
        let mut index = AxIndex::default();
        let mut root = node("root", 10); root.child_ids = vec!["a".into(), "b".into()];
        index.replace("doc".into(), 1, vec![root, node("a", 11), node("b", 12)]);
        let mut changed = node("root", 10); changed.child_ids = vec!["b".into(), "a".into()];
        assert!(index.merge_branch(10, vec![changed, node("b", 12), node("a", 11)], 2));
        assert_eq!(index.generation, 2);
        assert_eq!(index.node_for_backend(10).map(|node| node.child_ids.clone()), Some(vec!["b".into(), "a".into()]));
        let mut removed = node("root", 10); removed.child_ids = vec!["a".into()];
        assert!(index.merge_branch(10, vec![removed, node("a", 11)], 3));
        assert_eq!(index.node_for_backend(12), None);
        let mut incomplete = node("root", 10); incomplete.child_ids = vec!["a".into()];
        assert!(!index.branch_complete(10, &[incomplete]));
    }
}
