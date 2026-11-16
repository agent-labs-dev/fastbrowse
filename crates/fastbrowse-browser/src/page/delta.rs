//! Exact reconstruction of a frame's control rows, before the snapshot schema accepts them.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlDelta {
    base_read: u64,
    controls: Vec<Value>,
    guards: Map<String, Value>,
    retarget_keys: Map<String, Value>,
}

/// A second page handle can move the browser's baseline, so only the named document and read can supply rows.
pub(super) fn apply(kept: &Value, incoming: &Value) -> Option<Value> {
    let mut result = incoming.as_object()?.clone();
    let delta: ControlDelta = serde_json::from_value(result.remove("control_delta")?).ok()?;
    if kept.get("document_key")?.as_str()? != incoming.get("document_key")?.as_str()?
        || kept.get("read")?.as_u64()? != delta.base_read
        || incoming.get("read")?.as_u64()? <= delta.base_read
    {
        return None;
    }
    let previous: HashMap<i64, &Value> = kept
        .get("controls")?
        .as_array()?
        .iter()
        .map(|row| Some((row.get("id")?.as_i64()?, row)))
        .collect::<Option<_>>()?;
    let mut controls = Vec::with_capacity(delta.controls.len());
    let mut guards = Map::new();
    let mut retarget_keys = Map::new();
    let mut seen = HashSet::new();
    for row in delta.controls {
        let row = match row.as_i64() {
            Some(id) => (*previous.get(&id)?).clone(),
            None => row,
        };
        let id = row.get("id")?.as_i64()?;
        if !seen.insert(id) {
            return None;
        }
        let key = id.to_string();
        for (name, patch, output) in
            [("guards", &delta.guards, &mut guards), ("retarget_keys", &delta.retarget_keys, &mut retarget_keys)]
        {
            let value = patch.get(&key).or_else(|| kept.get(name)?.get(&key))?;
            if !value.is_null() && !value.is_string() {
                return None;
            }
            output.insert(key.clone(), value.clone());
        }
        controls.push(row);
    }
    result.insert("controls".into(), controls.into());
    result.insert("guards".into(), guards.into());
    result.insert("retarget_keys".into(), retarget_keys.into());
    Some(result.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kept() -> Value {
        json!({"document_key":"doc", "read":4, "modal":"Open", "controls":[
            {"id":1,"label":"One"}, {"id":2,"label":"Two"}, {"id":3,"label":"Three"}
        ], "guards":{"1":"g1","2":"g2","3":"g3"}, "retarget_keys":{"1":"r1","2":"r2","3":"r3"}})
    }

    fn patch() -> Value {
        json!({"document_key":"doc", "read":5, "title":"Current", "control_delta":{
            "base_read":4, "controls":[3,{"id":1,"label":"Changed"},{"id":4,"label":"New"}],
            "guards":{"1":"changed","4":null}, "retarget_keys":{"4":"r4"}
        }})
    }

    #[test]
    fn reconstructs_order_removal_new_rows_and_guard_changes_without_old_metadata() {
        assert_eq!(
            apply(&kept(), &patch()).unwrap(),
            json!({
                "document_key":"doc", "read":5, "title":"Current", "controls":[
                    {"id":3,"label":"Three"}, {"id":1,"label":"Changed"}, {"id":4,"label":"New"}
                ], "guards":{"3":"g3","1":"changed","4":null},
                "retarget_keys":{"3":"r3","1":"r1","4":"r4"}
            })
        );
        let mut empty = patch();
        empty["control_delta"]["controls"] = json!([]);
        assert_eq!(apply(&kept(), &empty).unwrap()["guards"], json!({}));
    }

    #[test]
    fn a_missing_base_row_or_guard_never_becomes_a_partial_snapshot() {
        for controls in [json!([99]), json!([1, 1]), json!([{"id":99}]), json!(["1"])] {
            let mut incoming = patch();
            incoming["control_delta"]["controls"] = controls;
            assert!(apply(&kept(), &incoming).is_none());
        }
        for (field, value) in [("document_key", json!("other")), ("read", json!(4))] {
            let mut incoming = patch();
            incoming[field] = value;
            assert!(apply(&kept(), &incoming).is_none());
        }
        let mut incoming = patch();
        incoming["control_delta"]["base_read"] = json!(3);
        assert!(apply(&kept(), &incoming).is_none());
    }
}
