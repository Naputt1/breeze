//! Deep merge for fog configuration values.

use serde_json::{Map, Value};

/// Merge `overlay` on top of `base`.
///
/// * Objects merge recursively; keys only present in one side are kept.
/// * Arrays are replaced by the overlay (see [`merge_arrays`] for the hook a
///   later phase extends).
/// * Every other value, including `null`, is replaced by the overlay.
#[allow(dead_code)] // Wired into the fog command in a later phase.
pub fn deep_merge(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => merge_objects(base, overlay),
        _ => overlay.clone(),
    }
}

fn merge_objects(base: &Map<String, Value>, overlay: &Map<String, Value>) -> Value {
    let mut merged = base.clone();

    for (key, overlay_value) in overlay {
        let value = match merged.get(key) {
            Some(base_value) => merge_value(key, base_value, overlay_value),
            None => overlay_value.clone(),
        };
        merged.insert(key.clone(), value);
    }

    Value::Object(merged)
}

fn merge_value(key: &str, base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => merge_objects(base, overlay),
        (Value::Array(base), Value::Array(overlay)) => merge_arrays(key, base, overlay),
        _ => overlay.clone(),
    }
}

/// Merge arrays for a given key.
///
/// Phase 0 semantics: the overlay array replaces the base array wholesale.
/// A later phase turns this into key-based merging for
/// `scripts.<name>.service` entries (matched on their `name` field); this
/// function is the single hook for that change.
fn merge_arrays(_key: &str, _base: &[Value], overlay: &[Value]) -> Value {
    Value::Array(overlay.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deep_merges_objects() {
        let base = json!({
            "a": 1,
            "b": { "c": 2, "d": 3 }
        });
        let overlay = json!({
            "b": { "d": 4, "e": 5 },
            "f": 6
        });

        let merged = deep_merge(&base, &overlay);

        assert_eq!(
            merged,
            json!({
                "a": 1,
                "b": { "c": 2, "d": 4, "e": 5 },
                "f": 6
            })
        );
    }

    #[test]
    fn overlay_replaces_scalar() {
        let merged = deep_merge(
            &json!({ "name": "base", "port": 3000 }),
            &json!({ "name": "overlay", "port": 4000 }),
        );

        assert_eq!(merged, json!({ "name": "overlay", "port": 4000 }));
    }

    #[test]
    fn merges_deeply_nested_objects() {
        let base = json!({
            "level1": { "level2": { "keep": true, "change": "base" } }
        });
        let overlay = json!({
            "level1": { "level2": { "change": "overlay", "add": [1, 2] } }
        });

        let merged = deep_merge(&base, &overlay);

        assert_eq!(
            merged,
            json!({
                "level1": { "level2": { "keep": true, "change": "overlay", "add": [1, 2] } }
            })
        );
    }

    #[test]
    fn null_overlay_replaces_value() {
        let merged = deep_merge(
            &json!({ "a": 1, "b": { "nested": true } }),
            &json!({ "a": null, "b": null }),
        );

        assert_eq!(merged, json!({ "a": null, "b": null }));
        assert_eq!(
            deep_merge(&json!(null), &json!({ "a": 1 })),
            json!({ "a": 1 })
        );
    }

    #[test]
    fn arrays_are_replaced() {
        let base = json!({
            "list": [1, 2, 3],
            "nested": { "list": ["a", "b"] }
        });
        let overlay = json!({
            "list": [4],
            "nested": { "list": [] }
        });

        let merged = deep_merge(&base, &overlay);

        assert_eq!(
            merged,
            json!({
                "list": [4],
                "nested": { "list": [] }
            })
        );
    }

    #[test]
    fn overlay_wins_on_type_change() {
        let merged = deep_merge(
            &json!({ "value": { "object": true } }),
            &json!({ "value": ["array"] }),
        );

        assert_eq!(merged, json!({ "value": ["array"] }));
    }
}
