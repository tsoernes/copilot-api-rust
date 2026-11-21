//! Presentation of the account-specific model catalog. Prices and spend are
//! deliberately separate; this module never infers a price from usage.
use super::catalog::{CatalogModel, PriceTier, SortKey, reference_price};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use serde_json::Value;

pub(super) struct CatalogView<'a> {
    /// Already sorted by the controller. `None` means not loaded or failed.
    pub models: Option<&'a [CatalogModel]>,
    pub summary: Option<&'a Value>,
    pub selected: usize,
    pub sort_label: &'a str,
    pub sort_key: SortKey,
    pub exact_counts: bool,
    pub filter: &'a str,
    pub filter_prompt: &'a str,
    /// Physical detail lines, controlled independently of model selection.
    pub detail_scroll: u16,
    pub status: &'a str,
}

fn clean_bounded(value: &str, limit: usize) -> String {
    let mut chars = value.chars().filter(|c| {
        !c.is_control()
            && !matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{206f}')
    });
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

fn clean(value: &str) -> String {
    clean_bounded(value, 256)
}

fn label(value: Option<&str>) -> String {
    value
        .map(clean)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "—".into())
}

fn abbreviate(value: &str, width: usize) -> String {
    let budget = if value.is_ascii() { width } else { width / 2 };
    if value.chars().count() <= budget {
        return value.to_owned();
    }
    if budget == 0 {
        return String::new();
    }
    let mut result: String = value.chars().take(budget.saturating_sub(1)).collect();
    result.push('…');
    result
}

fn number(value: Option<f64>) -> String {
    value
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into())
}

/// A present provider tier with no published output quote means the displayed
/// highest known output price may not be the highest across all tiers.
fn partial_output_reference(model: &CatalogModel) -> bool {
    reference_price(model, SortKey::Output).is_some()
        && [
            model.default_prices.as_ref(),
            model.long_context_prices.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|tier| {
            tier.verified
                .as_ref()
                .and_then(|quote| quote.output)
                .filter(|value| value.is_finite() && *value >= 0.0)
                .is_none()
        })
}

fn grouped(value: u128) -> String {
    let s = value.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}

// Match the monitor's count toggle; currency and provider price values do not
// use this formatter, and approximation is always explicitly marked.
fn limit(value: Option<u64>, exact: bool) -> String {
    let Some(value) = value else {
        return "—".into();
    };
    let n = u128::from(value);
    if exact || n < 1000 {
        return grouped(n);
    }
    const UNITS: &[(u128, &str)] = &[
        (1_000, "K"),
        (1_000_000, "M"),
        (1_000_000_000, "B"),
        (1_000_000_000_000, "T"),
    ];
    let mut index = UNITS
        .iter()
        .rposition(|(divisor, _)| n >= *divisor)
        .unwrap_or(0);
    loop {
        let (divisor, suffix) = UNITS[index];
        let scaled = (n * 100 + divisor / 2) / divisor;
        if scaled >= 100_000 && index + 1 < UNITS.len() {
            index += 1;
            continue;
        }
        let approx = if n * 100 == scaled * divisor { "" } else { "~" };
        let fraction = match scaled % 100 {
            0 => String::new(),
            d if d % 10 == 0 => format!(".{}", d / 10),
            d => format!(".{d:02}"),
        };
        return format!("{approx}{}{fraction}{suffix}", grouped(scaled / 100));
    }
}

/// Decimal integer conversion keeps even one nano-AIU visible; null != zero.
pub(super) fn aiu(value: Option<&Value>) -> String {
    let Some(nanos) = value.and_then(Value::as_u64) else {
        return "—".into();
    };
    let whole = nanos / 1_000_000_000;
    let rem = nanos % 1_000_000_000;
    if rem == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{}", format!("{rem:09}").trim_end_matches('0'))
    }
}

/// Do not guess from names, aliases, empty IDs or duplicate IDs.
fn exact_model<'a>(catalog: Option<&'a [CatalogModel]>, usage: &Value) -> Option<&'a CatalogModel> {
    let id = usage
        .get("model")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let mut matching = catalog?.iter().filter(|model| model.id == id);
    let found = matching.next()?;
    if matching.next().is_some() {
        None
    } else {
        Some(found)
    }
}

pub(super) fn reference_output(catalog: Option<&[CatalogModel]>, usage: &Value) -> String {
    number(
        exact_model(catalog, usage)
            .and_then(|model| model.default_prices.as_ref())
            .and_then(|tier| tier.verified.as_ref())
            .and_then(|quote| quote.output),
    )
}

fn usage_detail_lines(
    usage: &Value,
    catalog: Option<&[CatalogModel]>,
    exact_counts: bool,
) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Model ID: {}",
            label(usage.get("model").and_then(Value::as_str))
        ),
        format!(
            "Reported AIU spent: {} (may be partial)",
            aiu(usage.get("total_nano_aiu"))
        ),
        "GitHub reference prices are not actual billed rates or spend.".into(),
    ];
    if let Some(model) = exact_model(catalog, usage) {
        tier_lines(
            "Default",
            model.default_prices.as_ref(),
            &mut lines,
            exact_counts,
        );
        tier_lines(
            "Long context",
            model.long_context_prices.as_ref(),
            &mut lines,
            exact_counts,
        );
    } else {
        lines.push(
            "Reference prices unavailable: catalog missing, unmatched or ambiguous ID.".into(),
        );
    }
    lines
}

pub(super) fn usage_detail_line_count(
    usage: &Value,
    catalog: Option<&[CatalogModel]>,
    width: u16,
    exact_counts: bool,
) -> usize {
    wrap_lines(
        usage_detail_lines(usage, catalog, exact_counts),
        usize::from(width.saturating_sub(2)),
    )
    .len()
}

pub(super) fn render_usage_details(
    frame: &mut Frame<'_>,
    area: Rect,
    usage: &Value,
    catalog: Option<&[CatalogModel]>,
    scroll: u16,
    exact_counts: bool,
) {
    let lines = wrap_lines(
        usage_detail_lines(usage, catalog, exact_counts),
        usize::from(area.width.saturating_sub(2)),
    );
    let maximum = lines
        .len()
        .saturating_sub(usize::from(area.height.saturating_sub(2)));
    let lines = lines
        .into_iter()
        .skip(usize::from(scroll).min(maximum))
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(panel("Pricing · d return · PgUp/PgDn")),
        area,
    );
}

fn tier_lines(name: &str, tier: Option<&PriceTier>, lines: &mut Vec<String>, exact_counts: bool) {
    let Some(tier) = tier else {
        lines.push(format!("{name} GitHub reference credits / 1M: —"));
        lines.push(format!("{name} raw provider prices: —"));
        return;
    };
    if let Some(quote) = &tier.verified {
        lines.push(format!("{name} GitHub reference credits / 1M:"));
        lines.push(format!(
            "  input {} / output {}",
            number(quote.input),
            number(quote.output)
        ));
        lines.push(format!(
            "  cache read {} / write {}",
            number(quote.cache_read),
            number(quote.cache_write)
        ));
        let source = clean_bounded(&quote.source, 4096);
        lines.push(format!(
            "  source: {}",
            if source.is_empty() { "—" } else { &source }
        ));
    } else {
        lines.push(format!("{name} GitHub reference credits / 1M: —"));
    }
    if let Some(batch) = tier.batch_size.filter(|batch| *batch > 0) {
        lines.push(format!(
            "{name} raw provider units / batch {batch} (unverified):"
        ));
        lines.push(format!(
            "  input {} / output {}",
            number(tier.input),
            number(tier.output)
        ));
        lines.push(format!(
            "  cache read {} / write {}",
            number(tier.cache_read),
            number(tier.cache_write)
        ));
        lines.push(format!("  cache write 1h {}", number(tier.cache_write_1h)));
    } else {
        lines.push(format!("{name} raw prices unavailable (batch unknown)"));
    }
    if let Some(max_prompt) = tier.max_prompt_tokens {
        lines.push(format!(
            "  tier max prompt: {} tokens",
            limit(Some(max_prompt), exact_counts)
        ));
    }
}

fn details(
    model: &CatalogModel,
    summary: Option<&Value>,
    catalog: &[CatalogModel],
    exact_counts: bool,
) -> Vec<String> {
    let matches = catalog.iter().filter(|item| item.id == model.id).count();
    let usage = if model.id.is_empty() || matches != 1 {
        None
    } else {
        summary
            .and_then(|s| s.get("byModel"))
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row.get("model").and_then(Value::as_str) == Some(model.id.as_str()))
            })
    };
    let mut lines = vec![
        format!("ID: {}", label(Some(&model.id))),
        format!("Name: {}", label(Some(&model.name))),
        format!("Vendor: {}", label(model.vendor.as_deref())),
        format!("Type: {}", label(model.model_type.as_deref())),
        format!(
            "Picker: {} / policy: {}",
            match model.picker_enabled {
                Some(true) => "enabled",
                Some(false) => "disabled",
                None => "—",
            },
            label(model.policy_state.as_deref())
        ),
        format!(
            "Context {} / prompt {} / output {} tokens",
            limit(model.max_context_tokens, exact_counts),
            limit(model.max_prompt_tokens, exact_counts),
            limit(model.max_output_tokens, exact_counts)
        ),
        format!("Billing multiplier: {}", number(model.billing_multiplier)),
        format!("Preferred tier: {}", label(model.preferred_tier.as_deref())),
        format!(
            "Reported AIU spent: {} (may be partial)",
            aiu(usage.and_then(|r| r.get("total_nano_aiu")))
        ),
        "GitHub reference prices are not actual billed rates.".into(),
    ];
    if matches > 1 {
        lines.push("AIU unavailable: duplicate catalog ID.".into());
    }
    if partial_output_reference(model) {
        lines.push("* Highest published output may be partial: another reported tier has no published output reference.".into());
    }
    tier_lines(
        "Default",
        model.default_prices.as_ref(),
        &mut lines,
        exact_counts,
    );
    tier_lines(
        "Long context",
        model.long_context_prices.as_ref(),
        &mut lines,
        exact_counts,
    );
    lines.push(format!(
        "Version: {} / preview: {}",
        label(model.version.as_deref()),
        match model.preview {
            Some(true) => "yes",
            Some(false) => "no",
            None => "—",
        }
    ));
    lines.push(format!("Family: {}", label(model.family.as_deref())));
    lines.push(format!("Tokenizer: {}", label(model.tokenizer.as_deref())));
    for (name, values) in [
        ("Endpoints", model.supported_endpoints.as_ref()),
        ("Input modalities", model.input_modalities.as_ref()),
        ("Output modalities", model.output_modalities.as_ref()),
    ] {
        lines.push(format!(
            "{name}: {}",
            values
                .map(|items| items
                    .iter()
                    .map(|s| clean(s))
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_else(|| "—".into())
        ));
    }
    for (name, value) in &model.capability_flags {
        lines.push(format!(
            "Supports {}: {}",
            clean(name),
            if *value { "yes" } else { "no" }
        ));
    }
    for (name, value) in model.capability_values.iter().take(64) {
        lines.push(format!("Supports {}: {}", clean(name), clean(value)));
    }
    for (name, value) in model.extra_limits.iter().take(64) {
        let rendered = value
            .parse::<u64>()
            .ok()
            .map(|n| limit(Some(n), exact_counts))
            .unwrap_or_else(|| clean(value));
        lines.push(format!("Limit {}: {}", clean(name), rendered));
    }
    for (kind, messages) in [
        ("Info", &model.info_messages),
        ("Warning", &model.warning_messages),
    ] {
        for message in messages {
            lines.push(format!(
                "{kind} {}: {}",
                label(message.code.as_deref()),
                clean(&message.message)
            ));
        }
    }
    lines
}

// Render generated text in bounded physical lines. A half-width budget for
// non-ASCII is conservative for wide Unicode, so metadata cannot clip.
fn wrap_lines(lines: Vec<String>, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in lines {
        let chunk = if line.is_ascii() {
            width.max(1)
        } else {
            (width / 2).max(1)
        };
        let chars: Vec<char> = line.chars().collect();
        for part in chars.chunks(chunk) {
            result.push(part.iter().collect());
        }
        if chars.is_empty() {
            result.push(String::new());
        }
    }
    result
}

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", clean(title)))
        .border_style(Style::default().fg(Color::DarkGray))
}

/// Number of wrapped detail lines, for the controller to clamp scrolling.
pub(super) fn detail_line_count(
    model: &CatalogModel,
    summary: Option<&Value>,
    catalog: &[CatalogModel],
    width: u16,
    exact_counts: bool,
) -> usize {
    wrap_lines(
        details(model, summary, catalog, exact_counts),
        usize::from(width.saturating_sub(2)),
    )
    .len()
}

pub(super) fn render(frame: &mut Frame<'_>, area: Rect, view: &CatalogView<'_>) {
    let Some(models) = view.models else {
        frame.render_widget(
            Paragraph::new(format!("Catalog unavailable: {}", clean(view.status)))
                .block(panel("Catalog")),
            area,
        );
        return;
    };
    if models.is_empty() {
        frame.render_widget(
            Paragraph::new(if view.filter.is_empty() {
                "No models returned by Copilot."
            } else {
                "No models match this filter."
            })
            .block(panel("Catalog")),
            area,
        );
        return;
    }
    if area.height < 7 || area.width < 20 {
        frame.render_widget(
            Paragraph::new("Resize to show catalog models.").block(panel("Catalog")),
            area,
        );
        return;
    }
    let selected = view.selected.min(models.len() - 1);
    let list_height = (area.height / 3).clamp(5, 10);
    let sections =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(3)]).split(area);
    let visible = usize::from(sections[0].height.saturating_sub(4));
    let start = selected.saturating_sub(visible.saturating_sub(1));
    let title = format!("Catalog · {} / {}", selected + 1, models.len());
    let metric = match view.sort_key {
        SortKey::Input => "Input: GitHub ref credits/1M",
        SortKey::Output | SortKey::Name => "Max published output cr/1M (* partial)",
        SortKey::LongContextOutput => "Long-context output ref cr/1M",
        SortKey::CacheRead => "Cache read: GitHub ref credits/1M",
        SortKey::CacheWrite => "Cache write: GitHub ref credits/1M",
    };
    let filter_line = clean(view.filter_prompt);
    let mut rows = vec![
        Line::from(format!(
            "Sort: {} · {}",
            clean(view.sort_label),
            clean(view.status)
        )),
        Line::from(filter_line),
        Line::from(metric),
    ];
    rows.extend(
        models
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, m)| {
                let marker = if i == selected { "> " } else { "  " };
                let key = match view.sort_key {
                    SortKey::Name => SortKey::Output,
                    other => other,
                };
                let mut price = number(reference_price(m, key));
                if key == SortKey::Output && partial_output_reference(m) {
                    price.push('*');
                }
                let inner_width = usize::from(sections[0].width.saturating_sub(2));
                let price = if price.chars().count() > inner_width.saturating_sub(7) {
                    "…".into()
                } else {
                    price
                };
                let id_width = inner_width.saturating_sub(5 + price.chars().count());
                let id = abbreviate(&label(Some(&m.id)), id_width);
                Line::styled(
                    format!("{marker}{id} | {price}"),
                    if i == selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                )
            }),
    );
    frame.render_widget(Paragraph::new(rows).block(panel(&title)), sections[0]);
    let lines = wrap_lines(
        details(&models[selected], view.summary, models, view.exact_counts),
        usize::from(sections[1].width.saturating_sub(2)),
    );
    let max_scroll = lines
        .len()
        .saturating_sub(usize::from(sections[1].height.saturating_sub(2)));
    let scroll = usize::from(view.detail_scroll).min(max_scroll);
    let visible_lines = lines
        .into_iter()
        .skip(scroll)
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible_lines).block(panel("Details · PgUp/PgDn")),
        sections[1],
    );
}

#[cfg(test)]
mod tests {
    use super::super::catalog::VerifiedQuote;
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    #[test]
    fn terminal_labels_strip_control_and_bidirectional_overrides() {
        assert_eq!(clean("safe\u{1b}[31m\u{202e}name"), "safe[31mname");
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        let b = terminal.backend().buffer();
        (0..b.area.height)
            .map(|y| {
                let row = (0..b.area.width)
                    .map(|x| b[(x, y)].symbol())
                    .collect::<String>();
                format!("{row}\n")
            })
            .collect()
    }

    fn fixture() -> Vec<CatalogModel> {
        vec![
            CatalogModel {
                id: "model-a".into(),
                name: "Example".into(),
                vendor: Some("Vendor".into()),
                default_prices: Some(PriceTier {
                    batch_size: Some(1_000_000),
                    input: Some(1.0),
                    output: Some(5.0),
                    verified: Some(VerifiedQuote {
                        input: Some(2.0),
                        output: Some(10.0),
                        cache_read: Some(0.5),
                        cache_write: Some(3.0),
                        source: "official reference".into(),
                    }),
                    ..Default::default()
                }),
                long_context_prices: Some(PriceTier {
                    batch_size: None,
                    input: Some(0.0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            CatalogModel {
                id: "model-b".into(),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn spend_is_an_exact_id_join_and_nanos_are_not_rounded_away() {
        let models = fixture();
        let summary = json!({"byModel":[{"model":"model-a","total_nano_aiu":1},{"model":"model-b","total_nano_aiu":null},{"model":"model-a-alias","total_nano_aiu":1000000000}]});
        let a = details(&models[0], Some(&summary), &models, true).join("\n");
        assert!(
            a.contains("Reported AIU spent: 0.000000001 (may be partial)"),
            "{a}"
        );
        assert!(a.contains("Default GitHub reference credits / 1M:"), "{a}");
        assert!(
            a.contains("Default raw provider units / batch 1000000 (unverified)"),
            "{a}"
        );
        assert!(
            a.contains("Long context raw prices unavailable (batch unknown)"),
            "{a}"
        );
        assert!(
            !a.contains("Long context raw provider units / batch —"),
            "{a}"
        );
        assert!(
            details(&models[1], Some(&summary), &models, true)
                .join("\n")
                .contains("Reported AIU spent: —")
        );
        assert_eq!(aiu(Some(&json!(1000000000))), "1");
        assert_eq!(aiu(Some(&json!(null))), "—");
        assert_eq!(aiu(Some(&json!(-1))), "—");
        let duplicate = vec![models[0].clone(), models[0].clone()];
        let ambiguous = details(&duplicate[0], Some(&summary), &duplicate, true).join("\n");
        assert!(ambiguous.contains("Reported AIU spent: —"), "{ambiguous}");
        assert!(ambiguous.contains("duplicate catalog ID"), "{ambiguous}");
    }

    #[test]
    fn invalid_batch_never_displays_raw_zero_as_a_price() {
        for batch in [None, Some(0)] {
            let tier = PriceTier {
                batch_size: batch,
                input: Some(0.0),
                output: Some(0.0),
                cache_read: Some(0.0),
                ..Default::default()
            };
            let mut lines = Vec::new();
            tier_lines("Default", Some(&tier), &mut lines, true);
            let display = lines.join("\n");
            assert!(
                display.contains("raw prices unavailable (batch unknown)"),
                "{display}"
            );
            assert!(!display.contains("input 0"), "{display}");
            assert!(!display.contains("output 0"), "{display}");
        }
        let tier = PriceTier {
            batch_size: Some(1_000_000),
            input: Some(0.0),
            ..Default::default()
        };
        let mut lines = Vec::new();
        tier_lines("Default", Some(&tier), &mut lines, true);
        assert!(lines.join("\n").contains("input 0"));
    }

    #[test]
    fn reference_source_retains_long_promotion_footnote() {
        let source = format!("{} promo ends 2026-12-01", "x".repeat(350));
        let tier = PriceTier {
            verified: Some(VerifiedQuote {
                source,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut lines = Vec::new();
        tier_lines("Default", Some(&tier), &mut lines, true);
        assert!(lines.join("\n").contains("promo ends 2026-12-01"));
    }

    #[test]
    fn list_price_and_label_follow_the_selected_sort_metric() {
        let models = fixture();
        for (sort_key, label, expected) in [
            (
                SortKey::Output,
                "Max published output cr/1M (* partial)",
                "model-a | 10*",
            ),
            (
                SortKey::LongContextOutput,
                "Long-context output ref cr/1M",
                "model-a | —",
            ),
            (
                SortKey::Input,
                "Input: GitHub ref credits/1M",
                "model-a | 2",
            ),
            (
                SortKey::CacheRead,
                "Cache read: GitHub ref credits/1M",
                "model-a | 0.5",
            ),
            (
                SortKey::CacheWrite,
                "Cache write: GitHub ref credits/1M",
                "model-a | 3",
            ),
            (
                SortKey::Name,
                "Max published output cr/1M (* partial)",
                "model-a | 10*",
            ),
        ] {
            for (width, height) in [(178, 46), (42, 24)] {
                let v = CatalogView {
                    models: Some(&models),
                    summary: None,
                    selected: 0,
                    sort_label: "selected",
                    filter: "",
                    filter_prompt: "Filter (press /)",
                    sort_key,
                    exact_counts: true,
                    detail_scroll: 0,
                    status: "ok",
                };
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| render(f, f.area(), &v)).unwrap();
                let display = text(&terminal);
                assert!(display.contains(label), "{display}");
                assert!(display.contains(expected), "{display}");
                assert!(display.contains("model-b | —"), "{display}");
            }
        }
    }

    #[test]
    fn output_list_uses_highest_published_tier_and_marks_partial() {
        use super::super::catalog::{SortOrder, sort_catalog_by};
        let quote = |output| {
            Some(VerifiedQuote {
                output: Some(output),
                ..Default::default()
            })
        };
        let tier = |quote| {
            Some(PriceTier {
                verified: quote,
                ..Default::default()
            })
        };
        let full = CatalogModel {
            id: "full".into(),
            default_prices: tier(quote(10.0)),
            long_context_prices: tier(quote(20.0)),
            ..Default::default()
        };
        let partial = CatalogModel {
            id: "partial".into(),
            default_prices: tier(quote(5.0)),
            long_context_prices: tier(None),
            ..Default::default()
        };
        let unknown = CatalogModel {
            id: "unknown".into(),
            default_prices: tier(None),
            ..Default::default()
        };
        assert!(!partial_output_reference(&full));
        assert!(partial_output_reference(&partial));
        assert!(!partial_output_reference(&unknown));
        let mut models = vec![unknown, partial, full];
        sort_catalog_by(&mut models, SortKey::Output, SortOrder::Descending);
        assert!(
            details(&models[1], None, &models, true)
                .join("\n")
                .contains("another reported tier has no published output reference")
        );
        for (width, height) in [(178, 46), (42, 24)] {
            let v = CatalogView {
                models: Some(&models),
                summary: None,
                selected: 0,
                sort_label: "output desc",
                filter: "",
                filter_prompt: "Filter (press /)",
                sort_key: SortKey::Output,
                exact_counts: true,
                detail_scroll: 0,
                status: "ok",
            };
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, f.area(), &v)).unwrap();
            let display = text(&terminal);
            assert!(display.contains("Max published output"), "{display}");
            let first = display.find("full | 20").expect("highest verified tier");
            let second = display.find("partial | 5*").expect("partial marker");
            let last = display.find("unknown | —").expect("unknown last");
            assert!(first < second && second < last, "{display}");
        }
    }

    #[test]
    fn compact_context_and_tier_limits_do_not_change_monetary_prices() {
        let mut models = fixture();
        models[0].max_context_tokens = Some(1_234_567);
        models[0].max_prompt_tokens = Some(1_000_000);
        models[0].default_prices.as_mut().unwrap().max_prompt_tokens = Some(1_234_567);
        let compact = details(&models[0], None, &models, false).join("\n");
        assert!(compact.contains("Context ~1.23M / prompt 1M"), "{compact}");
        assert!(
            compact.contains("tier max prompt: ~1.23M tokens"),
            "{compact}"
        );
        assert!(compact.contains("output 10"), "{compact}");
        let exact = details(&models[0], None, &models, true).join("\n");
        assert!(
            exact.contains("Context 1,234,567 / prompt 1,000,000"),
            "{exact}"
        );
        assert!(
            exact.contains("tier max prompt: 1,234,567 tokens"),
            "{exact}"
        );
        assert!(exact.contains("output 10"), "{exact}");
    }

    #[test]
    fn extra_capabilities_and_limits_are_sanitized_and_visible() {
        let mut models = fixture();
        models[0].family = Some("family\u{1b}[31m".into());
        models[0].tokenizer = Some("tokenizer".into());
        models[0].capability_values =
            vec![("reasoning_effort".into(), "[\"low\",\"high\"]".into())];
        models[0].extra_limits = vec![
            ("max_images".into(), "1234567".into()),
            ("ratio".into(), "1.25".into()),
        ];
        let exact = details(&models[0], None, &models, true).join("\n");
        assert!(exact.contains("Family: family[31m"), "{exact}");
        assert!(!exact.contains('\u{1b}'));
        assert!(exact.contains("Tokenizer: tokenizer"), "{exact}");
        assert!(
            exact.contains("Supports reasoning_effort: [\"low\",\"high\"]"),
            "{exact}"
        );
        assert!(exact.contains("Limit max_images: 1,234,567"), "{exact}");
        assert!(exact.contains("Limit ratio: 1.25"), "{exact}");
        let compact = details(&models[0], None, &models, false).join("\n");
        assert!(compact.contains("Limit max_images: ~1.23M"), "{compact}");
        for (width, height) in [(178, 46), (42, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut view = CatalogView {
                models: Some(&models),
                summary: None,
                selected: 0,
                sort_label: "output desc",
                filter: "",
                filter_prompt: "Filter (press /)",
                sort_key: SortKey::Output,
                exact_counts: true,
                detail_scroll: 0,
                status: "ok",
            };
            let mut seen = String::new();
            for scroll in 0..detail_line_count(&models[0], None, &models, width, true) {
                view.detail_scroll = scroll as u16;
                terminal.draw(|f| render(f, f.area(), &view)).unwrap();
                seen.push_str(&text(&terminal));
            }
            for value in [
                "Family: family[31m",
                "Tokenizer: tokenizer",
                "reasoning_effort",
                "Limit max_images",
            ] {
                assert!(
                    seen.contains(value),
                    "missing {value} at {width}x{height}: {seen}"
                );
            }
        }
    }

    #[test]
    fn catalog_is_navigable_and_details_are_reachable_at_typical_sizes() {
        let models = fixture();
        let summary = json!({"byModel":[{"model":"model-a","total_nano_aiu":1}]});
        for (width, height) in [(178, 46), (80, 24), (42, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut view = CatalogView {
                models: Some(&models),
                summary: Some(&summary),
                selected: 0,
                sort_label: "output desc",
                filter: "",
                filter_prompt: "Filter (press /)",
                sort_key: SortKey::Output,
                exact_counts: true,
                detail_scroll: 0,
                status: "ok",
            };
            terminal.draw(|f| render(f, f.area(), &view)).unwrap();
            let display = text(&terminal);
            assert!(display.contains("model-a | 10"), "{display}");
            assert!(display.contains("model-b | —"), "{display}");
            assert!(display.contains("Max published output"), "{display}");
            let mut seen = String::new();
            for scroll in 0..detail_line_count(&models[0], Some(&summary), &models, width, true) {
                view.detail_scroll = scroll as u16;
                terminal.draw(|f| render(f, f.area(), &view)).unwrap();
                seen.push_str(&text(&terminal));
            }
            for value in [
                "0.000000001",
                "reference credits",
                "official reference",
                "unverified",
                "Long context",
            ] {
                assert!(
                    seen.contains(value),
                    "missing {value} at {width}x{height}: {seen}"
                );
            }
            view.selected = 1;
            view.detail_scroll = 0;
            terminal.draw(|f| render(f, f.area(), &view)).unwrap();
            assert!(text(&terminal).contains("ID: model-b"));
        }
    }

    #[test]
    fn empty_missing_and_too_small_are_explicit() {
        let mut terminal = Terminal::new(TestBackend::new(42, 12)).unwrap();
        let mut view = CatalogView {
            models: None,
            summary: None,
            selected: 0,
            sort_label: "output desc",
            filter: "",
            filter_prompt: "Filter (press /)",
            sort_key: SortKey::Output,
            exact_counts: true,
            detail_scroll: 0,
            status: "offline",
        };
        terminal.draw(|f| render(f, f.area(), &view)).unwrap();
        assert!(text(&terminal).contains("unavailable"));
        view.models = Some(&[]);
        terminal.draw(|f| render(f, f.area(), &view)).unwrap();
        assert!(text(&terminal).contains("No models"));
        let models = fixture();
        view.models = Some(&models);
        let mut tiny = Terminal::new(TestBackend::new(15, 5)).unwrap();
        tiny.draw(|f| render(f, f.area(), &view)).unwrap();
        assert!(text(&tiny).contains("Resize"));
    }

    #[test]
    fn filter_prompt_and_empty_match_are_visible() {
        let models = fixture();
        let mut view = CatalogView {
            models: Some(&models),
            summary: None,
            selected: 0,
            sort_label: "output desc",
            sort_key: SortKey::Output,
            exact_counts: true,
            filter: "claude",
            filter_prompt: "Filter /claude▏ · Enter keep · Esc cancel",
            detail_scroll: 0,
            status: "ok",
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render(f, f.area(), &view)).unwrap();
        assert!(text(&terminal).contains("Filter /claude"));
        view.models = Some(&[]);
        terminal.draw(|f| render(f, f.area(), &view)).unwrap();
        assert!(text(&terminal).contains("No models match this filter"));
    }
}
