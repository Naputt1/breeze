//! Deep merge for fog configuration values.
//!
//! The merge is path-aware: most values merge generically, but two array paths
//! get fog-specific rules (`dnsmasq.domains` unions, `scripts.<name>.service`
//! merges by entry name). Everything else keeps the Phase 0 semantics: objects
//! merge recursively, arrays and scalars are replaced by the overlay.

use serde_json::{Map, Value};

/// Merge `overlay` on top of `base`, applying the fog array rules.
///
/// * Objects merge recursively; keys only present in one side are kept.
/// * Arrays are replaced by the overlay, except at `dnsmasq.domains` (set
///   union) and `scripts.<name>.service` (merge entries by `name`).
/// * Every other value, including `null`, is replaced by the overlay.
pub fn deep_merge(base: &Value, overlay: &Value) -> Value {
    merge_at(base, overlay, &[])
}

/// Merge with the JSON path (from the config root) of `base`/`overlay`.
fn merge_at(base: &Value, overlay: &Value, path: &[&str]) -> Value {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => merge_objects(base, overlay, path),
        (Value::Array(base), Value::Array(overlay)) => merge_arrays(path, base, overlay),
        _ => overlay.clone(),
    }
}

fn merge_objects(base: &Map<String, Value>, overlay: &Map<String, Value>, path: &[&str]) -> Value {
    let mut merged = base.clone();

    for (key, overlay_value) in overlay {
        let mut child = path.to_vec();
        child.push(key.as_str());

        let value = match merged.get(key) {
            Some(base_value) => merge_at(base_value, overlay_value, &child),
            None => merge_new(overlay_value, &child),
        };
        merged.insert(key.clone(), value);
    }

    Value::Object(merged)
}

/// Value for a key only present in the overlay. Array paths with special rules
/// still run through the array merger so unions also deduplicate fresh keys.
fn merge_new(value: &Value, path: &[&str]) -> Value {
    match value {
        Value::Object(object) => merge_objects(&Map::new(), object, path),
        Value::Array(items) if is_domains_path(path) || is_service_path(path) => {
            merge_arrays(path, &[], items)
        }
        _ => value.clone(),
    }
}

/// Merge arrays for the JSON path of the array itself.
///
/// * `dnsmasq.domains`: set union, base order first, then new overlay entries.
/// * `scripts.<name>.service`: entries are merged by their `name` field.
/// * everything else: the overlay array replaces the base array.
fn merge_arrays(path: &[&str], base: &[Value], overlay: &[Value]) -> Value {
    if is_domains_path(path) {
        let mut merged = base.to_vec();
        for item in overlay {
            if !merged.contains(item) {
                merged.push(item.clone());
            }
        }
        Value::Array(merged)
    } else if is_service_path(path) {
        merge_service_entries(path, base, overlay)
    } else {
        Value::Array(overlay.to_vec())
    }
}

/// Merge `scripts.<name>.service` arrays: an overlay entry whose `name` matches
/// a base entry is deep-merged into that entry (overlay wins); unnamed entries
/// and names absent from the base are appended in overlay order.
fn merge_service_entries(path: &[&str], base: &[Value], overlay: &[Value]) -> Value {
    let mut merged = base.to_vec();

    for entry in overlay {
        let name = entry.get("name").and_then(Value::as_str);

        let index = name.and_then(|name| {
            merged
                .iter()
                .position(|existing| existing.get("name").and_then(Value::as_str) == Some(name))
        });

        match index {
            Some(index) => {
                let updated = merge_at(&merged[index], entry, path);
                merged[index] = updated;
            }
            None => merged.push(entry.clone()),
        }
    }

    Value::Array(merged)
}

fn is_domains_path(path: &[&str]) -> bool {
    matches!(path, ["dnsmasq", "domains"])
}

fn is_service_path(path: &[&str]) -> bool {
    path.len() == 3 && path[0] == "scripts" && path[2] == "service"
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

    #[test]
    fn fog_rules_are_applied_by_json_path() {
        struct Case {
            name: &'static str,
            base: Value,
            overlay: Value,
            expected: Value,
        }

        let cases = vec![
            Case {
                name: "ports merge as a map with overlay winning",
                base: json!({ "ports": { "web": 8080, "db": 5432 } }),
                overlay: json!({ "ports": { "web": 9090 } }),
                expected: json!({ "ports": { "web": 9090, "db": 5432 } }),
            },
            Case {
                name: "dnsmasq.domains unions, base order first, deduplicated",
                base: json!({ "dnsmasq": { "domains": ["a.local", "b.local"] } }),
                overlay: json!({ "dnsmasq": { "domains": ["b.local", "c.local", "c.local"] } }),
                expected: json!({ "dnsmasq": { "domains": ["a.local", "b.local", "c.local"] } }),
            },
            Case {
                name: "native_routes is replaced",
                base: json!({ "native_routes": [{ "host": "a" }, { "host": "b" }] }),
                overlay: json!({ "native_routes": [{ "host": "c" }] }),
                expected: json!({ "native_routes": [{ "host": "c" }] }),
            },
            Case {
                name: "service entries merge by name and unknown names append",
                base: json!({
                    "scripts": {
                        "dev": {
                            "service": [
                                { "name": "a", "port": 3000, "watch": true },
                                { "name": "b", "port": 3001 }
                            ]
                        }
                    }
                }),
                overlay: json!({
                    "scripts": {
                        "dev": {
                            "service": [
                                { "name": "a", "port": 4000 },
                                { "name": "c", "port": 4002 }
                            ]
                        }
                    }
                }),
                expected: json!({
                    "scripts": {
                        "dev": {
                            "service": [
                                { "name": "a", "port": 4000, "watch": true },
                                { "name": "b", "port": 3001 },
                                { "name": "c", "port": 4002 }
                            ]
                        }
                    }
                }),
            },
            Case {
                name: "unnamed service entries append",
                base: json!({ "scripts": { "dev": { "service": [{ "name": "a", "port": 3000 }] } } }),
                overlay: json!({
                    "scripts": { "dev": { "service": [{ "port": 5000 }, { "name": "a", "port": 4000 }] } }
                }),
                expected: json!({
                    "scripts": {
                        "dev": {
                            "service": [
                                { "name": "a", "port": 4000 },
                                { "port": 5000 }
                            ]
                        }
                    }
                }),
            },
            Case {
                name: "scripts.dev.env merges as a map",
                base: json!({ "scripts": { "dev": { "env": { "A": "1", "B": "2" } } } }),
                overlay: json!({ "scripts": { "dev": { "env": { "B": "3", "C": "4" } } } }),
                expected: json!({ "scripts": { "dev": { "env": { "A": "1", "B": "3", "C": "4" } } } }),
            },
            Case {
                name: "proxy merges but proxy.routes is replaced",
                base: json!({
                    "scripts": {
                        "dev": {
                            "proxy": { "enabled": true, "routes": [{ "path": "/a" }, { "path": "/b" }] }
                        }
                    }
                }),
                overlay: json!({
                    "scripts": {
                        "dev": {
                            "proxy": { "routes": [{ "path": "/c" }] }
                        }
                    }
                }),
                expected: json!({
                    "scripts": {
                        "dev": {
                            "proxy": { "enabled": true, "routes": [{ "path": "/c" }] }
                        }
                    }
                }),
            },
            Case {
                name: "concurrent, reuse and share are replaced",
                base: json!({ "scripts": { "dev": { "concurrent": 2, "reuse": true, "share": false } } }),
                overlay: json!({ "scripts": { "dev": { "concurrent": 4, "reuse": false, "share": true } } }),
                expected: json!({ "scripts": { "dev": { "concurrent": 4, "reuse": false, "share": true } } }),
            },
            Case {
                name: "unknown nested keys deep merge with arrays replaced",
                base: json!({
                    "custom": { "list": [1, 2], "obj": { "a": 1, "keep": true } }
                }),
                overlay: json!({
                    "custom": { "list": [3], "obj": { "b": 2 }, "added": "yes" }
                }),
                expected: json!({
                    "custom": {
                        "list": [3],
                        "obj": { "a": 1, "keep": true, "b": 2 },
                        "added": "yes"
                    }
                }),
            },
            Case {
                name: "domains outside dnsmasq is replaced",
                base: json!({ "other": { "domains": ["a", "b"] } }),
                overlay: json!({ "other": { "domains": ["c"] } }),
                expected: json!({ "other": { "domains": ["c"] } }),
            },
            Case {
                name: "service outside scripts is replaced",
                base: json!({ "other": { "service": [{ "name": "a", "port": 1 }] } }),
                overlay: json!({ "other": { "service": [{ "name": "a", "port": 2 }] } }),
                expected: json!({ "other": { "service": [{ "name": "a", "port": 2 }] } }),
            },
            Case {
                name: "fresh dnsmasq.domains is deduplicated",
                base: json!({}),
                overlay: json!({ "dnsmasq": { "domains": ["a", "a", "b"] } }),
                expected: json!({ "dnsmasq": { "domains": ["a", "b"] } }),
            },
        ];

        for case in cases {
            assert_eq!(
                deep_merge(&case.base, &case.overlay),
                case.expected,
                "case failed: {}",
                case.name
            );
        }
    }

    #[test]
    fn layers_merge_in_order() {
        let base = json!({ "ports": { "web": 8080 } });
        let shared = json!({ "ports": { "web": 8081, "db": 5432 } });
        let branch = json!({ "ports": { "web": 8082 } });
        let worktree = json!({ "ports": { "web": 8083, "api": 9000 } });

        let merged = deep_merge(&deep_merge(&deep_merge(&base, &shared), &branch), &worktree);

        assert_eq!(
            merged,
            json!({ "ports": { "web": 8083, "db": 5432, "api": 9000 } })
        );
    }
}
