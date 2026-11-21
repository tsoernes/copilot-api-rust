//! Public GitHub documentation reference, independent of provider wire prices.
//! Regenerate with `bun scripts/refresh-copilot-pricing.mjs` and review the diff.
use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;

use super::catalog::{CatalogModel, PriceTier, VerifiedQuote};

const SNAPSHOT: &str = include_str!("pricing_reference_snapshot.json");
// Older snapshots are not suitable for a current price sort.
const MAX_REFERENCE_AGE_DAYS: i64 = 30;

#[derive(Debug, Deserialize)]
struct Reference {
    source: String,
    retrieved_at: DateTime<Utc>,
    unit: String,
    usd_per_ai_credit: f64,
    rows: Vec<Row>,
}

#[derive(Debug, Deserialize)]
struct Row {
    vendor: String,
    model: String,
    tier: String,
    threshold: String,
    input_usd_per_million: Option<f64>,
    output_usd_per_million: Option<f64>,
    cached_input_usd_per_million: Option<f64>,
    cache_write_usd_per_million: Option<f64>,
    footnotes: Vec<String>,
}

fn reference() -> Option<&'static Reference> {
    static REFERENCE: OnceLock<Option<Reference>> = OnceLock::new();
    REFERENCE
        .get_or_init(|| serde_json::from_str(SNAPSHOT).ok())
        .as_ref()
}

fn normalized(s: &str) -> String {
    s.split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn status_at(reference: &Reference, now: DateTime<Utc>) -> &'static str {
    if reference.source
        != "https://docs.github.com/en/copilot/reference/copilot-billing/models-and-pricing"
        || reference.rows.is_empty()
        || reference.unit != "USD per million tokens"
        || reference.usd_per_ai_credit != 0.01
        || now < reference.retrieved_at
    {
        "invalid"
    } else if now - reference.retrieved_at > Duration::days(MAX_REFERENCE_AGE_DAYS) {
        "stale"
    } else {
        "fresh"
    }
}

/// A fresh snapshot is still a dated public reference, not an account quote.
pub(super) fn reference_status() -> &'static str {
    reference().map_or("invalid", |r| status_at(r, Utc::now()))
}

// Only recognize the documentation's explicit promotional-pricing footnote
// pattern. An unknown condition or ambiguous date is never assumed to be safe.
// Expire at UTC start of the named date for both "until" and "through" since
// GitHub does not specify a timezone. The dated snapshot supplies the start.
fn notes_current(notes: &[String], now: DateTime<Utc>) -> bool {
    notes.iter().all(|note| {
        if !note.contains(" is available at the promotional pricing of ") {
            return false;
        }
        let matches: Vec<_> = [" until ", " through "]
            .into_iter()
            .flat_map(|prefix| {
                note.match_indices(prefix)
                    .map(move |(pos, _)| pos + prefix.len())
            })
            .collect();
        let [start] = matches.as_slice() else {
            return false;
        };
        let Some(date_text) = note[*start..].split('.').next() else {
            return false;
        };
        let Ok(date) = NaiveDate::parse_from_str(date_text.trim(), "%B %d, %Y") else {
            return false;
        };
        now.date_naive() < date
    })
}

fn threshold_agrees(row: &Row, tier: &PriceTier) -> bool {
    if row.threshold == "Not applicable" || tier.max_prompt_tokens.is_none() {
        return true;
    }
    let (op, value) = match row.threshold.split_once(' ') {
        Some(pair) => pair,
        None => return false,
    };
    let Some(number) = value
        .strip_suffix('K')
        .and_then(|v| v.parse::<u64>().ok())
        .and_then(|v| v.checked_mul(1_000))
    else {
        return false;
    };
    let Some(max) = tier.max_prompt_tokens else {
        return true;
    };
    match op {
        "≤" if row.tier == "Default" => max == number,
        ">" if row.tier == "Long context" => max > number,
        _ => false,
    }
}

fn annotate_with_reference(models: &mut [CatalogModel], reference: &Reference, now: DateTime<Utc>) {
    // Duplicate provider names could denote distinct variants: never guess which
    // exact provider ID corresponds to a single documentation entry.
    let mut identities: HashMap<(String, String), usize> = HashMap::new();
    for model in models.iter() {
        if let Some((name, vendor)) = &model.reference_identity {
            *identities
                .entry((normalized(vendor), normalized(name)))
                .or_default() += 1;
        }
    }
    for model in models.iter_mut() {
        for tier in [&mut model.default_prices, &mut model.long_context_prices]
            .into_iter()
            .flatten()
        {
            tier.verified = None;
        }
        if status_at(reference, now) != "fresh" || model.id.is_empty() {
            continue;
        }
        let Some((name, vendor)) = &model.reference_identity else {
            continue;
        };
        let key = (normalized(vendor), normalized(name));
        if identities.get(&key) != Some(&1) {
            continue;
        }
        for (tier_name, target) in [
            ("Default", &mut model.default_prices),
            ("Long context", &mut model.long_context_prices),
        ] {
            let mut matching = reference.rows.iter().filter(|row| {
                normalized(&row.vendor) == key.0
                    && normalized(&row.model) == key.1
                    && row.tier == tier_name
            });
            let Some(row) = matching.next() else { continue };
            if matching.next().is_some() || !notes_current(&row.footnotes, now) {
                continue;
            }
            // Validate finite nonnegative numbers even for a hand-edited snapshot.
            let values = [
                row.input_usd_per_million,
                row.output_usd_per_million,
                row.cached_input_usd_per_million,
                row.cache_write_usd_per_million,
            ];
            if values
                .into_iter()
                .flatten()
                .any(|v| !v.is_finite() || v < 0.0)
            {
                continue;
            }
            if target
                .as_ref()
                .is_some_and(|tier| !threshold_agrees(row, tier))
            {
                continue;
            }
            let note = if row.footnotes.is_empty() {
                String::new()
            } else {
                format!("; {}", row.footnotes.join("; "))
            };
            let quote = VerifiedQuote {
                input: row.input_usd_per_million.map(|v| v * 100.0),
                output: row.output_usd_per_million.map(|v| v * 100.0),
                cache_read: row.cached_input_usd_per_million.map(|v| v * 100.0),
                cache_write: row.cache_write_usd_per_million.map(|v| v * 100.0),
                source: format!(
                    "GitHub published reference {} ({}, {}; {}{})",
                    reference.retrieved_at.date_naive(),
                    tier_name,
                    row.threshold,
                    reference.source,
                    note
                ),
            };
            target.get_or_insert_with(PriceTier::default).verified = Some(quote);
        }
    }
}

/// Attach dated public documentation reference prices; never interpret raw
/// provider values as currency. Stale or ambiguous references remain unknown.
pub fn annotate_official_prices(models: &mut [CatalogModel]) {
    if let Some(reference) = reference() {
        annotate_with_reference(models, reference, Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model(id: &str, name: &str, vendor: &str) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            name: name.into(),
            vendor: Some(vendor.into()),
            reference_identity: Some((name.into(), vendor.into())),
            ..Default::default()
        }
    }
    fn now(reference: &Reference) -> DateTime<Utc> {
        reference.retrieved_at + Duration::days(1)
    }
    #[test]
    fn exact_documented_name_and_vendor_get_reference_not_raw_rates() {
        let r = reference().unwrap();
        let mut models = vec![model("provider-id", "gpt-5.3-codex", "OPENAI")];
        annotate_with_reference(&mut models, r, now(r));
        let q = models[0]
            .default_prices
            .as_ref()
            .unwrap()
            .verified
            .as_ref()
            .unwrap();
        assert_eq!(q.input, Some(175.0));
        assert_eq!(q.output, Some(1400.0));
        assert!(q.source.contains("GitHub published reference"));
    }
    #[test]
    fn variant_wrong_vendor_duplicates_and_stale_fail_closed() {
        let r = reference().unwrap();
        let mut models = vec![
            model("a", "GPT-5.3-Codex", "OpenAI"),
            model("b", "GPT-5.3-Codex", "OpenAI"),
            model("c", "GPT-5.3-Codex (fast)", "OpenAI"),
            model("d", "GPT-5.3-Codex", "Anthropic"),
        ];
        annotate_with_reference(&mut models, r, now(r));
        assert!(models.iter().all(|m| m.default_prices.is_none()));
        let mut models = vec![model("a", "GPT-5.3-Codex", "OpenAI")];
        annotate_with_reference(&mut models, r, r.retrieved_at + Duration::days(31));
        assert!(models[0].default_prices.is_none());
    }
    #[test]
    fn tier_threshold_and_promotions_are_fail_closed() {
        let r = reference().unwrap();
        let mut m = model("a", "Claude Haiku 5.5", "Anthropic");
        m.default_prices = Some(PriceTier {
            max_prompt_tokens: Some(999),
            ..Default::default()
        });
        annotate_with_reference(std::slice::from_mut(&mut m), r, now(r));
        assert!(m.default_prices.as_ref().unwrap().verified.is_none());
        let mut promo = model("b", "Gemini 4 Argon", "Google");
        annotate_with_reference(std::slice::from_mut(&mut promo), r, now(r));
        let quote = promo
            .default_prices
            .as_ref()
            .unwrap()
            .verified
            .as_ref()
            .unwrap();
        assert!(quote.source.contains("until December 1, 2026"));
        let midnight = DateTime::parse_from_rfc3339("2026-12-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let prefix = "Model is available at the promotional pricing of $1 per 1M";
        for word in ["until", "through"] {
            let note = format!("{prefix} {word} December 1, 2026.");
            assert!(notes_current(
                std::slice::from_ref(&note),
                midnight - Duration::seconds(1)
            ));
            assert!(!notes_current(&[note], midnight));
        }
        assert!(!notes_current(&["Unspecified promotion".into()], now(r)));
        assert!(!notes_current(
            &[format!("{prefix} until unknown.")],
            now(r)
        ));
        assert!(!notes_current(
            &[format!(
                "{prefix} until December 1, 2026 through December 2, 2026."
            )],
            now(r)
        ));
    }
    #[test]
    fn display_values_never_substitute_for_validated_identity() {
        let r = reference().unwrap();
        let mut m = model("a", "GPT-5.3-Codex", "OpenAI");
        m.reference_identity = None;
        annotate_with_reference(std::slice::from_mut(&mut m), r, now(r));
        assert!(m.default_prices.is_none());
        m.reference_identity = Some(("GPT-5.3-Codex (fast)".into(), "OpenAI".into()));
        annotate_with_reference(std::slice::from_mut(&mut m), r, now(r));
        assert!(m.default_prices.is_none());
    }
    #[test]
    fn reference_status_identifies_stale_and_future() {
        let r = reference().unwrap();
        assert_eq!(status_at(r, now(r)), "fresh");
        assert_eq!(status_at(r, r.retrieved_at + Duration::days(31)), "stale");
        assert_eq!(
            status_at(r, r.retrieved_at - Duration::seconds(1)),
            "invalid"
        );
    }
}
