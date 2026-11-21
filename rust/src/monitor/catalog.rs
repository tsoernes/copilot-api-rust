//! Read-only projection of the Copilot model catalog.
//!
//! The provider's `token_prices` units are not documented. Keep those numbers
//! separate from independently corroborated credits-per-million quotations.
use std::{cmp::Ordering, collections::HashMap};

use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct VerifiedQuote {
    /// Published credits per million tokens, with matching model and tier.
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct PriceTier {
    /// `None` includes a provider-reported zero or invalid batch size.
    pub batch_size: Option<u64>,
    pub max_prompt_tokens: Option<u64>,
    /// Raw provider numbers; never label them as credits without corroboration.
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_write_1h: Option<f64>,
    /// Populated only by an independent matching published price source.
    pub verified: Option<VerifiedQuote>,
}

#[derive(Clone, Debug, Default)]
pub struct ModelMessage {
    pub code: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct CatalogModel {
    /// Exact provider ID used for joining usage and matching published prices.
    /// Always sanitize this field before rendering it in a terminal.
    pub id: String,
    pub name: String,
    pub vendor: Option<String>,
    /// Untouched name and vendor only when safe, complete, and unique in the
    /// catalog. Never match reference prices against sanitized display text.
    pub reference_identity: Option<(String, String)>,
    pub picker_enabled: Option<bool>,
    pub policy_state: Option<String>,
    pub model_type: Option<String>,
    pub family: Option<String>,
    pub tokenizer: Option<String>,
    pub preview: Option<bool>,
    pub version: Option<String>,
    pub supported_endpoints: Option<Vec<String>>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    /// Boolean fields explicitly reported under capabilities.supports.
    pub capability_flags: Vec<(String, bool)>,
    /// Non-boolean supports fields as bounded, sanitized JSON display text.
    pub capability_values: Vec<(String, String)>,
    /// Other numeric limits, preserved as decimal JSON text rather than f64.
    pub extra_limits: Vec<(String, String)>,
    pub info_messages: Vec<ModelMessage>,
    pub warning_messages: Vec<ModelMessage>,
    pub max_context_tokens: Option<u64>,
    pub max_prompt_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub billing_multiplier: Option<f64>,
    pub preferred_tier: Option<String>,
    pub default_prices: Option<PriceTier>,
    pub long_context_prices: Option<PriceTier>,
}

const MAX_DISPLAY_CHARS: usize = 120;
const MAX_DISPLAY_ITEMS: usize = 64;

fn unsafe_character(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{206f}')
}

fn reference_identity(model: &Value) -> Option<(String, String)> {
    let name = model.get("name")?.as_str()?;
    let vendor = model.get("vendor")?.as_str()?;
    let valid = |s: &str| {
        !s.is_empty() && s.chars().count() <= MAX_DISPLAY_CHARS && !s.chars().any(unsafe_character)
    };
    (valid(name) && valid(vendor)).then(|| (name.to_owned(), vendor.to_owned()))
}

fn safe_display(value: &str) -> String {
    let mut chars = value.chars();
    let mut result: String = chars
        .by_ref()
        .take(MAX_DISPLAY_CHARS)
        .map(|c| if unsafe_character(c) { ' ' } else { c })
        .collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(safe_display)
}

fn text_list(value: Option<&Value>) -> Option<Vec<String>> {
    Some(
        value?
            .as_array()?
            .iter()
            .take(MAX_DISPLAY_ITEMS)
            .filter_map(|v| text(Some(v)))
            .collect(),
    )
}

fn messages(value: Option<&Value>) -> Vec<ModelMessage> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_DISPLAY_ITEMS)
        .filter_map(|item| {
            Some(ModelMessage {
                code: text(item.get("code")),
                message: text(item.get("message"))?,
            })
        })
        .collect()
}

fn nonnegative(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn price_tier(value: Option<&Value>, batch_size: Option<u64>) -> Option<PriceTier> {
    let value = value?.as_object()?;
    Some(PriceTier {
        batch_size,
        max_prompt_tokens: value.get("max_prompt_tokens").and_then(Value::as_u64),
        input: nonnegative(value.get("input_price")),
        output: nonnegative(value.get("output_price")),
        cache_read: nonnegative(value.get("cache_read_price")),
        cache_write: nonnegative(value.get("cache_write_price")),
        cache_write_1h: nonnegative(value.get("cache_write_1h_price")),
        verified: None,
    })
}

/// Keep every array entry, even an entry with missing metadata or malformed prices.
/// An invalid response is an error, not an empty catalog.
pub fn parse_catalog(value: &Value) -> Result<Vec<CatalogModel>, &'static str> {
    let models = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Invalid model catalog")?;
    let mut result: Vec<CatalogModel> = models
        .iter()
        .map(|model| {
            let capabilities = model.get("capabilities");
            let limits = capabilities.and_then(|cap| cap.get("limits"));
            let billing = model.get("billing");
            let prices = billing.and_then(|billing| billing.get("token_prices"));
            let batch_size = prices
                .and_then(|prices| prices.get("batch_size"))
                .and_then(Value::as_u64)
                .filter(|size| *size > 0);
            CatalogModel {
                id: model
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                name: text(model.get("name")).unwrap_or_default(),
                vendor: text(model.get("vendor")),
                reference_identity: reference_identity(model),
                picker_enabled: model.get("model_picker_enabled").and_then(Value::as_bool),
                policy_state: text(model.get("policy").and_then(|policy| policy.get("state"))),
                model_type: text(capabilities.and_then(|cap| cap.get("type"))),
                family: text(capabilities.and_then(|cap| cap.get("family"))),
                tokenizer: text(capabilities.and_then(|cap| cap.get("tokenizer"))),
                preview: model.get("preview").and_then(Value::as_bool),
                version: text(model.get("version")),
                supported_endpoints: text_list(model.get("supported_endpoints")),
                input_modalities: text_list(
                    model
                        .get("input_modalities")
                        .or_else(|| capabilities.and_then(|cap| cap.get("input_modalities")))
                        .or_else(|| model.get("modalities").and_then(|m| m.get("input")))
                        .or_else(|| {
                            capabilities
                                .and_then(|cap| cap.get("modalities"))
                                .and_then(|m| m.get("input"))
                        }),
                ),
                output_modalities: text_list(
                    model
                        .get("output_modalities")
                        .or_else(|| capabilities.and_then(|cap| cap.get("output_modalities")))
                        .or_else(|| model.get("modalities").and_then(|m| m.get("output")))
                        .or_else(|| {
                            capabilities
                                .and_then(|cap| cap.get("modalities"))
                                .and_then(|m| m.get("output"))
                        }),
                ),
                capability_flags: capabilities
                    .and_then(|cap| cap.get("supports"))
                    .and_then(Value::as_object)
                    .into_iter()
                    .flat_map(|supports| supports.iter())
                    .filter_map(|(name, value)| Some((safe_display(name), value.as_bool()?)))
                    .take(MAX_DISPLAY_ITEMS)
                    .collect(),
                capability_values: capabilities
                    .and_then(|cap| cap.get("supports"))
                    .and_then(Value::as_object)
                    .into_iter()
                    .flat_map(|supports| supports.iter())
                    .filter(|(_, value)| !value.is_boolean())
                    .map(|(name, value)| (safe_display(name), safe_display(&value.to_string())))
                    .take(MAX_DISPLAY_ITEMS)
                    .collect(),
                extra_limits: limits
                    .and_then(Value::as_object)
                    .into_iter()
                    .flat_map(|limits| limits.iter())
                    .filter(|(name, value)| {
                        !matches!(
                            name.as_str(),
                            "max_context_window_tokens" | "max_prompt_tokens" | "max_output_tokens"
                        ) && value.is_number()
                    })
                    .map(|(name, value)| (safe_display(name), safe_display(&value.to_string())))
                    .take(MAX_DISPLAY_ITEMS)
                    .collect(),
                info_messages: messages(model.get("info_messages")),
                warning_messages: messages(model.get("warning_messages")),
                max_context_tokens: limits
                    .and_then(|limits| limits.get("max_context_window_tokens"))
                    .and_then(Value::as_u64),
                max_prompt_tokens: limits
                    .and_then(|limits| limits.get("max_prompt_tokens"))
                    .and_then(Value::as_u64),
                max_output_tokens: limits
                    .and_then(|limits| limits.get("max_output_tokens"))
                    .and_then(Value::as_u64),
                billing_multiplier: nonnegative(
                    billing.and_then(|billing| billing.get("multiplier")),
                ),
                preferred_tier: text(prices.and_then(|prices| prices.get("preferred_tier"))),
                default_prices: price_tier(
                    prices.and_then(|prices| prices.get("default")),
                    batch_size,
                ),
                long_context_prices: price_tier(
                    prices.and_then(|prices| prices.get("long_context")),
                    batch_size,
                ),
            }
        })
        .collect();
    let mut counts = HashMap::new();
    for identity in result.iter().filter_map(|m| m.reference_identity.as_ref()) {
        *counts.entry(identity.clone()).or_insert(0_usize) += 1;
    }
    for model in &mut result {
        if model
            .reference_identity
            .as_ref()
            .is_some_and(|identity| counts[identity] > 1)
        {
            model.reference_identity = None;
        }
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Output,
    LongContextOutput,
    Input,
    CacheRead,
    CacheWrite,
    Name,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortOrder {
    #[default]
    Descending,
    Ascending,
}

/// Highest available verified output tier descending; unverified last.
pub fn sort_catalog(models: &mut [CatalogModel]) {
    sort_catalog_by(models, SortKey::default(), SortOrder::default());
}

/// Reference credits/1M used for sorting. Output is the highest available
/// published default or long-context tier. Availability may be partial, and
/// this number is not necessarily the price billed for any actual request.
/// Other price keys use the published default tier. Never use raw provider
/// prices as a fallback; a missing, negative, or non-finite value is unknown.
pub fn reference_price(model: &CatalogModel, key: SortKey) -> Option<f64> {
    fn quote(tier: Option<&PriceTier>) -> Option<&VerifiedQuote> {
        tier.and_then(|tier| tier.verified.as_ref())
    }
    let valid = |value: Option<f64>| value.filter(|v| v.is_finite() && *v >= 0.0);
    if key == SortKey::Output {
        return [
            model.default_prices.as_ref(),
            model.long_context_prices.as_ref(),
        ]
        .into_iter()
        .filter_map(|tier| valid(quote(tier).and_then(|q| q.output)))
        .max_by(f64::total_cmp);
    }
    if key == SortKey::LongContextOutput {
        return valid(quote(model.long_context_prices.as_ref()).and_then(|quote| quote.output));
    }
    let default = quote(model.default_prices.as_ref())?;
    valid(match key {
        SortKey::Input => default.input,
        SortKey::CacheRead => default.cache_read,
        SortKey::CacheWrite => default.cache_write,
        SortKey::Output | SortKey::LongContextOutput | SortKey::Name => None,
    })
}

/// Only independently confirmed prices can be sorted numerically. Missing or
/// unverified values sort last in both directions. Name/ID break all ties.
pub fn sort_catalog_by(models: &mut [CatalogModel], key: SortKey, order: SortOrder) {
    models.sort_by(|a, b| {
        let compare_present = |ordering: Ordering| match order {
            SortOrder::Ascending => ordering,
            SortOrder::Descending => ordering.reverse(),
        };
        let primary = if key == SortKey::Name {
            let a_name = (!a.name.is_empty()).then_some(a.name.as_str());
            let b_name = (!b.name.is_empty()).then_some(b.name.as_str());
            match (a_name, b_name) {
                (Some(a), Some(b)) => compare_present(a.cmp(b)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        } else {
            match (reference_price(a, key), reference_price(b, key)) {
                (Some(a), Some(b)) => compare_present(a.total_cmp(&b)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        };
        primary
            .then_with(|| {
                let a_label = if a.name.is_empty() { &a.id } else { &a.name };
                let b_label = if b.name.is_empty() { &b.id } else { &b.name };
                a_label.cmp(b_label)
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_catalog_and_raw_tiers_without_assuming_credit_units() {
        let catalog = parse_catalog(&json!({"data":[
            {"id":"a","name":"A","vendor":"vendor","model_picker_enabled":false,
             "policy":{"state":"disabled"},"capabilities":{"type":"chat","limits":{"max_prompt_tokens":10,"max_output_tokens":4}},
             "billing":{"multiplier":0,"token_prices":{"batch_size":1000000,"preferred_tier":"default",
                "default":{"input_price":10,"output_price":50,"cache_read_price":1,"cache_write_price":12.5,"cache_write_1h_price":25,"max_prompt_tokens":10},
                "long_context":{"input_price":20,"output_price":100,"max_prompt_tokens":20}}}},
            {"id":"missing"}, null
        ]})).unwrap();
        assert_eq!(catalog.len(), 3);
        let a = &catalog[0];
        assert_eq!(a.picker_enabled, Some(false));
        assert_eq!(a.policy_state.as_deref(), Some("disabled"));
        assert_eq!(a.max_prompt_tokens, Some(10));
        assert_eq!(a.billing_multiplier, Some(0.0));
        let price = a.default_prices.as_ref().unwrap();
        assert_eq!(price.batch_size, Some(1_000_000));
        assert_eq!(price.input, Some(10.0));
        assert_eq!(price.cache_write_1h, Some(25.0));
        assert!(price.verified.is_none());
        assert_eq!(a.long_context_prices.as_ref().unwrap().output, Some(100.0));
        assert!(catalog[1].default_prices.is_none());
        assert!(parse_catalog(&json!({"data":{}})).is_err());
    }

    #[test]
    fn zero_batch_is_unknown_not_free_and_invalid_values_are_missing() {
        let catalog = parse_catalog(&json!({"data":[{"id":"a","billing":{"token_prices":{
            "batch_size":0,"default":{"input_price":0,"output_price":-1,"cache_read_price":"1"}}}},
            {"id":"b","billing":{"token_prices":{"batch_size":1000000,"default":{"output_price":0}}}}]})).unwrap();
        let a = catalog[0].default_prices.as_ref().unwrap();
        assert_eq!(a.batch_size, None);
        assert_eq!(a.input, Some(0.0)); // raw only; batch validity decides usability
        assert_eq!(a.output, None);
        assert_eq!(a.cache_read, None);
        assert_eq!(
            catalog[1].default_prices.as_ref().unwrap().output,
            Some(0.0)
        );
    }

    #[test]
    fn sorts_verified_then_unverified_and_unknown_by_name_and_id() {
        let mut catalog = parse_catalog(&json!({"data":[
            {"id":"low","name":"Zulu","billing":{"token_prices":{"batch_size":1000000,"default":{"output_price":9999}}}},
            {"id":"unknown","billing":{"token_prices":{"batch_size":0,"default":{"output_price":999}}}},
            {"id":"high","name":"Alpha","billing":{"token_prices":{"batch_size":1000000,"default":{"output_price":1}}}},
            {"id":"other-batch","billing":{"token_prices":{"batch_size":100,"default":{"output_price":9999}}}},
            {"id":"verified","billing":{"token_prices":{"batch_size":1000000,"default":{"output_price":1}}}}
        ]})).unwrap();
        catalog[4].default_prices.as_mut().unwrap().verified = Some(VerifiedQuote {
            output: Some(0.5),
            source: "published reference".into(),
            ..Default::default()
        });
        sort_catalog(&mut catalog);
        assert_eq!(
            catalog.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["verified", "high", "low", "other-batch", "unknown"]
        );
    }

    #[test]
    fn captures_bounded_safe_display_information_without_changing_id() {
        let long = "x".repeat(130);
        let catalog = parse_catalog(&json!({"data":[{
            "id":"bad\u{1b}[0m-id","name":"name\u{1b}[0m", "version":long,
            "preview":true,"supported_endpoints":["/responses", 3, "bad\u{7}"],
            "input_modalities":["text","image"],"output_modalities":["text"],
            "info_messages":[{"code":"i","message":"hello\nworld"}],
            "warning_messages":[{"message":"warning"}],
            "capabilities":{"supports":{"streaming":true,"vision":false,"reasoning_effort":["high"]}}
        }]})).unwrap();
        let m = &catalog[0];
        assert_eq!(m.id, "bad\u{1b}[0m-id");
        assert_eq!(m.name, "name [0m");
        assert_eq!(m.version.as_ref().unwrap().chars().count(), 121);
        assert_eq!(m.preview, Some(true));
        assert_eq!(
            m.supported_endpoints.as_ref().unwrap(),
            &["/responses", "bad "]
        );
        assert_eq!(m.input_modalities.as_ref().unwrap(), &["text", "image"]);
        assert_eq!(m.output_modalities.as_ref().unwrap(), &["text"]);
        assert!(m.capability_flags.contains(&("streaming".into(), true)));
        assert!(m.capability_flags.contains(&("vision".into(), false)));
        assert_eq!(m.info_messages[0].message, "hello world");
        assert_eq!(m.warning_messages[0].message, "warning");
    }

    #[test]
    fn preserves_nonboolean_capabilities_and_extra_numeric_limits() {
        let large = "a".repeat(200);
        let catalog = parse_catalog(&json!({"data":[{"id":"m", "capabilities":{
            "family":"family\u{1b}","tokenizer":"tokenizer",
            "limits":{"max_prompt_tokens":10,"max_output_tokens":4,
                "max_context_window_tokens":20,"max_inputs":18446744073709551615_u64,
                "fractional_limit":1.5,"not_numeric":"3"},
            "supports":{"vision":false,"reasoning_effort":["low","high"],
                "min_thinking_budget":10,"long_value":large}
        }}]}))
        .unwrap();
        let model = &catalog[0];
        assert_eq!(model.family.as_deref(), Some("family "));
        assert_eq!(model.tokenizer.as_deref(), Some("tokenizer"));
        assert_eq!(model.capability_flags, [("vision".into(), false)]);
        assert!(
            model
                .capability_values
                .contains(&("reasoning_effort".into(), "[\"low\",\"high\"]".into()))
        );
        assert!(
            model
                .capability_values
                .contains(&("min_thinking_budget".into(), "10".into()))
        );
        let long = &model
            .capability_values
            .iter()
            .find(|(k, _)| k == "long_value")
            .unwrap()
            .1;
        assert_eq!(long.chars().count(), 121);
        assert!(long.ends_with('…'));
        assert!(
            model
                .extra_limits
                .contains(&("max_inputs".into(), "18446744073709551615".into()))
        );
        assert!(
            model
                .extra_limits
                .contains(&("fractional_limit".into(), "1.5".into()))
        );
        assert_eq!(model.extra_limits.len(), 2);
    }

    #[test]
    fn reference_identity_requires_safe_untouched_unique_name_and_vendor() {
        let long = "x".repeat(121);
        let catalog = parse_catalog(&json!({"data":[
            {"id":"a","name":"Model A","vendor":"Vendor"},
            {"id":"b","name":"Model\nA","vendor":"Vendor"},
            {"id":"c","name":"Model\u{202e} A","vendor":"Vendor"},
            {"id":"d","name":long,"vendor":"Vendor"},
            {"id":"e","name":"Duplicate","vendor":"Vendor"},
            {"id":"f","name":"Duplicate","vendor":"Vendor"},
            {"id":"g","name":"Model G","vendor":"Vendor\u{2066}"},
            {"id":"h","name":"Model H"},
            {"id":"i","name":"","vendor":"Vendor"}
        ]}))
        .unwrap();
        assert_eq!(
            catalog[0].reference_identity,
            Some(("Model A".into(), "Vendor".into()))
        );
        for model in &catalog[1..] {
            assert!(model.reference_identity.is_none(), "{}", model.id);
        }
        assert_eq!(catalog[1].name, "Model A"); // display collision is not a match
        assert!(!catalog[2].name.contains('\u{202e}'));
    }

    #[test]
    fn selectable_sorts_use_only_verified_prices_and_keep_unknown_last() {
        let make = |id: &str, name: &str, price: Option<VerifiedQuote>| CatalogModel {
            id: id.into(),
            name: name.into(),
            default_prices: Some(PriceTier {
                verified: price.clone(),
                output: Some(9999.0),
                ..Default::default()
            }),
            long_context_prices: Some(PriceTier {
                verified: price,
                ..Default::default()
            }),
            ..Default::default()
        };
        let quote = |input, output, cache_read, cache_write| {
            Some(VerifiedQuote {
                input: Some(input),
                output: Some(output),
                cache_read: Some(cache_read),
                cache_write: Some(cache_write),
                source: "reference".into(),
            })
        };
        let items = vec![
            make("missing", "Unknown", None),
            make("b", "B", quote(1.0, 4.0, 2.0, 8.0)),
            make("a", "A", quote(2.0, 3.0, 4.0, 1.0)),
        ];
        for (key, ascending) in [
            (SortKey::Input, ["b", "a", "missing"]),
            (SortKey::Output, ["a", "b", "missing"]),
            (SortKey::LongContextOutput, ["a", "b", "missing"]),
            (SortKey::CacheRead, ["b", "a", "missing"]),
            (SortKey::CacheWrite, ["a", "b", "missing"]),
        ] {
            let mut rows = items.clone();
            sort_catalog_by(&mut rows, key, SortOrder::Ascending);
            assert_eq!(
                rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                ascending
            );
            sort_catalog_by(&mut rows, key, SortOrder::Descending);
            assert_eq!(
                rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                [ascending[1], ascending[0], "missing"]
            );
        }
        let mut rows = items;
        sort_catalog_by(&mut rows, SortKey::Name, SortOrder::Ascending);
        assert_eq!(
            rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "missing"]
        );
        sort_catalog_by(&mut rows, SortKey::Name, SortOrder::Descending);
        assert_eq!(
            rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["missing", "b", "a"]
        );
    }

    #[test]
    fn sorting_ties_are_stable_by_name_then_id() {
        let mut rows = vec![
            CatalogModel {
                id: "z".into(),
                name: "same".into(),
                ..Default::default()
            },
            CatalogModel {
                id: "a".into(),
                name: "same".into(),
                ..Default::default()
            },
        ];
        for order in [SortOrder::Ascending, SortOrder::Descending] {
            sort_catalog_by(&mut rows, SortKey::Output, order);
            assert_eq!(
                rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                ["a", "z"]
            );
        }
    }

    #[test]
    fn output_reference_uses_highest_available_verified_tier_without_raw_fallback() {
        let tier = |output: Option<f64>, raw: Option<f64>| PriceTier {
            output: raw,
            verified: Some(VerifiedQuote {
                input: Some(3.0),
                output,
                cache_read: Some(2.0),
                cache_write: Some(1.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut m = CatalogModel {
            default_prices: Some(tier(Some(4.0), Some(999.0))),
            long_context_prices: Some(tier(Some(8.0), Some(1000.0))),
            ..Default::default()
        };
        assert_eq!(reference_price(&m, SortKey::Output), Some(8.0));
        assert_eq!(reference_price(&m, SortKey::LongContextOutput), Some(8.0));
        assert_eq!(reference_price(&m, SortKey::Input), Some(3.0));
        assert_eq!(reference_price(&m, SortKey::CacheRead), Some(2.0));
        assert_eq!(reference_price(&m, SortKey::CacheWrite), Some(1.0));
        assert_eq!(reference_price(&m, SortKey::Name), None);

        m.default_prices
            .as_mut()
            .unwrap()
            .verified
            .as_mut()
            .unwrap()
            .output = Some(12.0);
        assert_eq!(reference_price(&m, SortKey::Output), Some(12.0));
        assert_eq!(reference_price(&m, SortKey::LongContextOutput), Some(8.0));
        m.default_prices.as_mut().unwrap().verified = None;
        assert_eq!(reference_price(&m, SortKey::Output), Some(8.0));
        assert_eq!(reference_price(&m, SortKey::Input), None); // long tier is not the default
        assert_eq!(reference_price(&m, SortKey::LongContextOutput), Some(8.0)); // exact long tier only
        m.long_context_prices.as_mut().unwrap().verified = None;
        assert_eq!(reference_price(&m, SortKey::Output), None); // raw values are not prices
        assert_eq!(reference_price(&m, SortKey::LongContextOutput), None);

        m.default_prices = Some(tier(Some(f64::NAN), None));
        m.long_context_prices = Some(tier(Some(0.0), None));
        assert_eq!(reference_price(&m, SortKey::Output), Some(0.0));
        m.long_context_prices
            .as_mut()
            .unwrap()
            .verified
            .as_mut()
            .unwrap()
            .output = Some(f64::INFINITY);
        assert_eq!(reference_price(&m, SortKey::Output), None);
        m.default_prices
            .as_mut()
            .unwrap()
            .verified
            .as_mut()
            .unwrap()
            .output = Some(-1.0);
        assert_eq!(reference_price(&m, SortKey::Output), None);
    }

    #[test]
    fn output_sort_uses_highest_verified_tier_and_keeps_unknown_last() {
        let tier = |value: Option<f64>| {
            Some(PriceTier {
                output: Some(999.0),
                verified: value.map(|output| VerifiedQuote {
                    output: Some(output),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        let mut rows = vec![
            CatalogModel {
                id: "unknown".into(),
                default_prices: tier(None),
                ..Default::default()
            },
            CatalogModel {
                id: "default-only".into(),
                default_prices: tier(Some(3.0)),
                ..Default::default()
            },
            CatalogModel {
                id: "long-only".into(),
                long_context_prices: tier(Some(8.0)),
                ..Default::default()
            },
            CatalogModel {
                id: "both".into(),
                default_prices: tier(Some(4.0)),
                long_context_prices: tier(Some(10.0)),
                ..Default::default()
            },
            CatalogModel {
                id: "zero".into(),
                default_prices: tier(Some(0.0)),
                ..Default::default()
            },
        ];
        sort_catalog(&mut rows);
        assert_eq!(
            rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["both", "long-only", "default-only", "zero", "unknown"]
        );
        sort_catalog_by(&mut rows, SortKey::Output, SortOrder::Ascending);
        assert_eq!(
            rows.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["zero", "default-only", "long-only", "both", "unknown"]
        );
    }
}
