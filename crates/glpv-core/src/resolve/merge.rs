//! GitLab's configuration merge: deep-merge for hashes, replacement for
//! everything else (arrays included). Spans are never synthesised, so every
//! surviving leaf keeps the span of the file that actually wrote it.

use glpv_yaml::{Kind, Node};

/// `over` wins. Map ∪ Map merges key-wise (base key order kept, new keys
/// appended); any other combination takes `over` wholesale.
pub fn merge(base: Node, over: Node) -> Node {
    match (base, over) {
        (
            Node {
                kind: Kind::Map(mut base_map),
                ..
            },
            Node {
                kind: Kind::Map(over_map),
                span,
                anchor,
                alias_at,
            },
        ) => {
            for (key, over_entry) in over_map.entries {
                match base_map.entries.shift_remove_full(&key) {
                    Some((idx, _, base_entry)) => {
                        let merged = merge(base_entry.value, over_entry.value);
                        let entry = glpv_yaml::Entry {
                            key: over_entry.key,
                            key_span: over_entry.key_span,
                            value: merged,
                        };
                        base_map
                            .entries
                            .shift_insert(idx.min(base_map.entries.len()), key, entry);
                    }
                    None => {
                        base_map.entries.insert(key, over_entry);
                    }
                }
            }
            Node {
                kind: Kind::Map(base_map),
                span,
                anchor,
                alias_at,
            }
        }
        (_, over) => over,
    }
}

/// The `extends` variant of [`merge`]: an explicit `null` in `over` *removes*
/// the key from the result instead of setting it to null.
pub fn merge_extends(base: Node, over: Node) -> Node {
    match (base, over) {
        (
            Node {
                kind: Kind::Map(mut base_map),
                ..
            },
            Node {
                kind: Kind::Map(over_map),
                span,
                anchor,
                alias_at,
            },
        ) => {
            for (key, over_entry) in over_map.entries {
                if over_entry.value.is_null() {
                    base_map.entries.shift_remove(&key);
                    continue;
                }
                match base_map.entries.shift_remove_full(&key) {
                    Some((idx, _, base_entry)) => {
                        let merged = merge_extends(base_entry.value, over_entry.value);
                        let entry = glpv_yaml::Entry {
                            key: over_entry.key,
                            key_span: over_entry.key_span,
                            value: merged,
                        };
                        base_map
                            .entries
                            .shift_insert(idx.min(base_map.entries.len()), key, entry);
                    }
                    None => {
                        base_map.entries.insert(key, over_entry);
                    }
                }
            }
            Node {
                kind: Kind::Map(base_map),
                span,
                anchor,
                alias_at,
            }
        }
        (_, over) => over,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glpv_yaml::{FileId, parse};

    fn n(text: &str, file: u32) -> Node {
        parse(FileId(file), text).unwrap().0.remove(0).root.unwrap()
    }

    #[test]
    fn maps_deep_merge_arrays_replace() {
        let base = n("a:\n  x: 1\n  y: [1, 2]\nkeep: true\n", 0);
        let over = n("a:\n  y: [9]\n  z: 3\nnew: 1\n", 1);
        let m = merge(base, over);
        let a = m.get("a").unwrap();
        assert_eq!(a.get("x").unwrap().as_int(), Some(1));
        assert_eq!(a.get("y").unwrap().as_seq().unwrap().len(), 1);
        assert_eq!(a.get("z").unwrap().as_int(), Some(3));
        assert!(m.get("keep").unwrap().as_bool().unwrap());
        // Key order: base keys first, new keys appended.
        let keys: Vec<&str> = m.as_map().unwrap().iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["a", "keep", "new"]);
    }

    #[test]
    fn winning_leaves_keep_their_file() {
        let base = n("k: base\nonly_base: 1\n", 0);
        let over = n("k: over\n", 7);
        let m = merge(base, over);
        assert_eq!(m.get("k").unwrap().span.file.0, 7);
        assert_eq!(m.get("only_base").unwrap().span.file.0, 0);
    }

    #[test]
    fn extends_null_removes() {
        let base = n("variables:\n  A: 1\ncache: {k: v}\n", 0);
        let over = n("variables: null\ncache: {k2: v2}\n", 1);
        let m = merge_extends(base, over);
        assert!(m.get("variables").is_none());
        assert!(m.get("cache").unwrap().get("k").is_some());
    }
}
