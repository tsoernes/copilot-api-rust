//! Capture only numeric, provider-reported token subcounts. These are often
//! overlapping subsets; they must never be added to the normalized totals.
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) const MAX_DETAIL_FIELDS: usize = 32;
pub(crate) const MAX_DETAIL_KEY_LEN: usize = 96;
const ROOTS: &[&str] = &[
    "prompt_tokens_details",
    "completion_tokens_details",
    "input_tokens_details",
    "output_tokens_details",
    "cache_creation",
];

pub(crate) fn update(details: &mut BTreeMap<String, i64>, usage: &Value) {
    for root in ROOTS {
        if let Some(value) = usage.get(*root) {
            walk(details, root, value, 0);
        }
    }
}

fn walk(details: &mut BTreeMap<String, i64>, prefix: &str, value: &Value, depth: usize) {
    if depth >= 3 {
        return;
    }
    let Some(map) = value.as_object() else { return };
    for (key, value) in map {
        if key.is_empty()
            || key.len() > 64
            || !key
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        {
            continue;
        }
        let path = format!("{prefix}.{key}");
        if path.len() > MAX_DETAIL_KEY_LEN {
            continue;
        }
        if value.is_object() {
            walk(details, &path, value, depth + 1);
        } else if key.ends_with("_tokens")
            && let Some(n) = value.as_i64().filter(|n| *n >= 0)
            && (details.contains_key(&path) || details.len() < MAX_DETAIL_FIELDS)
        {
            details.insert(path, n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn captures_only_reported_numeric_nested_counts_and_merges_updates() {
        let mut d = BTreeMap::new();
        update(
            &mut d,
            &json!({"input_tokens_details":{"cached_tokens":7,"audio_tokens":0,"negative_tokens":-3,"fraction_tokens":1.5,"message":"private","numeric_string_tokens":"7"},"output_tokens_details":{"reasoning_tokens":5},"cache_creation":{"ephemeral_5m_input_tokens":9}}),
        );
        assert_eq!(d["input_tokens_details.cached_tokens"], 7);
        assert_eq!(d["input_tokens_details.audio_tokens"], 0);
        assert_eq!(d["output_tokens_details.reasoning_tokens"], 5);
        assert_eq!(d["cache_creation.ephemeral_5m_input_tokens"], 9);
        assert_eq!(d.len(), 4);
        update(
            &mut d,
            &json!({"output_tokens_details":{"reasoning_tokens":8,"audio_tokens":3}}),
        );
        assert_eq!(d["output_tokens_details.reasoning_tokens"], 8);
        assert_eq!(d["output_tokens_details.audio_tokens"], 3);
        assert_eq!(d["input_tokens_details.cached_tokens"], 7);
    }

    #[test]
    fn limits_fields_depth_and_key_shape() {
        let mut map = serde_json::Map::new();
        for i in 0..100 {
            map.insert(format!("detail_{i}_tokens"), json!(i));
        }
        map.insert("bad\u{1b}_tokens".into(), json!(5));
        map.insert("huge_tokens".into(), json!(u64::MAX));
        let mut d = BTreeMap::new();
        update(&mut d, &json!({"output_tokens_details":map}));
        assert_eq!(d.len(), MAX_DETAIL_FIELDS);
        assert!(
            d.keys()
                .all(|k| k.len() <= MAX_DETAIL_KEY_LEN && !k.contains('\u{1b}'))
        );
        let mut deep = BTreeMap::new();
        update(
            &mut deep,
            &json!({"input_tokens_details":{"a":{"b":{"c":{"lost_tokens":1},"kept_tokens":2}}}}),
        );
        assert_eq!(deep.get("input_tokens_details.a.b.kept_tokens"), Some(&2));
        assert!(!deep.contains_key("input_tokens_details.a.b.c.lost_tokens"));
    }
}
