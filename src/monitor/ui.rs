//! Presentation only: input, refresh and networking are owned by the caller.
use super::{
    catalog::{CatalogModel, SortKey},
    catalog_ui,
};
use chrono::{Local, TimeZone};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Sparkline, Table, Tabs, Wrap},
};
use serde_json::Value;

pub struct View<'a> {
    pub endpoint: &'a str,
    pub period: &'a str,
    pub tab: usize,
    pub summary: Option<&'a Value>,
    pub daily: Option<&'a Value>,
    pub events: Option<&'a Value>,
    pub sessions: Option<&'a Value>,
    pub rates: Option<&'a Value>,
    pub session_rates: Option<&'a Value>,
    pub quota: Option<&'a Value>,
    pub status: &'a str,
    pub page: usize,
    pub model_offset: usize,
    pub catalog: Option<&'a [CatalogModel]>,
    pub show_model_detail: bool,
    pub model_detail_scroll: u16,
    pub catalog_selected: usize,
    pub catalog_sort_label: &'a str,
    pub catalog_sort_key: SortKey,
    pub catalog_filter: &'a str,
    pub catalog_filter_prompt: &'a str,
    pub catalog_detail_scroll: u16,
    pub catalog_status: &'a str,
    pub event_offset: usize,
    pub session_page: usize,
    pub session_offset: usize,
    pub show_event_detail: bool,
    pub detail_scroll: usize,
    pub confirm_billing: bool,
    pub exact_counts: bool,
    pub paused: bool,
    pub last_updated: &'a str,
    pub error: Option<&'a str>,
}

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

// Models, endpoints and failures come from outside the terminal. Never pass
// embedded control sequences or unbounded server strings to a terminal.
fn clean(value: &str) -> String {
    value
        .chars()
        .take(120)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn endpoint_label(value: &str) -> String {
    // Display only the origin: URL paths, userinfo, queries and fragments can
    // carry secrets and have no value in the status bar.
    match reqwest::Url::parse(value) {
        Ok(url) => {
            let host = url.host_str().unwrap_or("?");
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            clean(&format!(
                "{}://{}{}",
                url.scheme(),
                host,
                url.port().map(|p| format!(":{p}")).unwrap_or_default()
            ))
        }
        Err(_) => "invalid endpoint".into(),
    }
}

fn text(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(clean)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "—".into())
}

fn grouped_unsigned(value: u128) -> String {
    let s = value.to_string();
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c);
    }
    out
}

fn count(value: Option<&Value>, exact: bool) -> String {
    let Some(value) = value else {
        return "—".into();
    };
    let signed = if let Some(n) = value.as_i64() {
        i128::from(n)
    } else if let Some(n) = value.as_u64() {
        i128::from(n)
    } else {
        return "—".into();
    };
    let negative = signed < 0;
    let magnitude = signed.unsigned_abs();
    let sign = if negative { "-" } else { "" };
    if exact || magnitude < 1_000 {
        return format!("{sign}{}", grouped_unsigned(magnitude));
    }
    const UNITS: &[(u128, &str)] = &[
        (1_000, "K"),
        (1_000_000, "M"),
        (1_000_000_000, "B"),
        (1_000_000_000_000, "T"),
    ];
    let mut index = UNITS
        .iter()
        .rposition(|(divisor, _)| magnitude >= *divisor)
        .unwrap_or(0);
    loop {
        let (divisor, suffix) = UNITS[index];
        let scaled = (magnitude * 100 + divisor / 2) / divisor;
        if scaled >= 100_000 && index + 1 < UNITS.len() {
            index += 1;
            continue;
        }
        let approx = if magnitude * 100 == scaled * divisor {
            ""
        } else {
            "~"
        };
        let fraction = match scaled % 100 {
            0 => String::new(),
            n if n % 10 == 0 => format!(".{}", n / 10),
            n => format!(".{n:02}"),
        };
        return format!(
            "{approx}{sign}{}{fraction}{suffix}",
            grouped_unsigned(scaled / 100)
        );
    }
}

// Quota quantities deliberately use the exact form, not the count-mode toggle.
fn number(value: Option<&Value>) -> String {
    count(value, true)
}

fn cost(value: &Value) -> String {
    let Some(costs) = value.get("costs").and_then(Value::as_array) else {
        return "—".into();
    };
    let parts: Vec<_> = costs
        .iter()
        .filter_map(|c| {
            let amount = c
                .get("amount")
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite())?;
            let currency = c.get("currency").and_then(Value::as_str).map(clean)?;
            Some(if amount != 0.0 && amount.abs() < 0.0001 {
                format!("{currency} {amount:.9}")
            } else {
                format!("{currency} {amount:.4}")
            })
        })
        .collect();
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" / ")
    }
}

fn decimal(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| {
            // Avoid hundreds of digits for abnormal finite floating values.
            if n >= 1.0e12 {
                format!("{n:.2e}")
            } else {
                format!("{n:.2}")
            }
        })
        .unwrap_or_else(|| "—".into())
}

fn quota_quantity(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "—".into();
    };
    if value.as_i64().is_some() {
        return number(Some(value));
    }
    if let Some(integer) = value.as_u64() {
        return integer.to_string();
    }
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .map(|n| {
            let raw = if n.fract() == 0.0 && n.abs() < 1.0e15 {
                format!("{n:.0}")
            } else {
                n.to_string()
            };
            if let Some((whole, fraction)) = raw.split_once('.')
                && let Ok(integer) = whole.parse::<i64>()
            {
                if whole == "-0" {
                    format!("-0.{fraction}")
                } else {
                    format!("{}.{}", number(Some(&Value::from(integer))), fraction)
                }
            } else if let Ok(integer) = raw.parse::<i64>() {
                number(Some(&Value::from(integer)))
            } else {
                raw
            }
        })
        .unwrap_or_else(|| "—".into())
}

/// Rolling windows are reported event counts, not all HTTP requests or an
/// extrapolated rate. Preserve missing fields as unavailable.
fn rate_lines(rates: Option<&Value>, width: usize, exact: bool) -> Vec<String> {
    let Some(windows) = rates
        .and_then(|v| v.get("windows"))
        .and_then(Value::as_array)
    else {
        return vec!["Rolling reported counts: unavailable".into()];
    };
    let mut lines = Vec::new();
    for (id, default_label) in [
        ("minute", "1m"),
        ("hour", "1h"),
        ("day", "24h"),
        ("week", "7d"),
        ("month", "30d"),
    ] {
        let Some(window) = windows
            .iter()
            .find(|w| w.get("id").and_then(Value::as_str) == Some(id))
        else {
            lines.push(format!("{default_label}: —"));
            continue;
        };
        let label = match window.get("duration_seconds").and_then(Value::as_u64) {
            Some(60) => "1m".to_owned(),
            Some(3600) => "1h".to_owned(),
            Some(86400) => "24h".to_owned(),
            Some(604800) => "7d".to_owned(),
            Some(2592000) => "30d".to_owned(),
            Some(n) => format!("{n}s"),
            None => default_label.into(),
        };
        let totals = window.get("totals").unwrap_or(&Value::Null);
        let requests = count(totals.get("request_count"), exact);
        let tokens = count(totals.get("total_tokens"), exact);
        let estimated = cost(totals);
        let combined = format!("{label}: {requests} req / {tokens} tok · est {estimated}");
        if combined.chars().count() <= width {
            lines.push(combined);
        } else {
            lines.push(format!("{label} requests: {requests}"));
            lines.push(format!("  tokens: {tokens}"));
            lines.push(format!("  est: {estimated}"));
        }
    }
    lines
}

const RATE_TABLE_HEADERS: [&str; 4] = ["Window", "Requests", "Tokens", "Est. cost"];

fn rate_table_rows(rates: Option<&Value>, exact: bool) -> Option<Vec<[String; 4]>> {
    let windows = rates
        .and_then(|value| value.get("windows"))
        .and_then(Value::as_array)?;
    Some(
        [
            ("minute", "1m"),
            ("hour", "1h"),
            ("day", "24h"),
            ("week", "7d"),
            ("month", "30d"),
        ]
        .into_iter()
        .map(|(id, default_label)| {
            let Some(window) = windows
                .iter()
                .find(|window| window.get("id").and_then(Value::as_str) == Some(id))
            else {
                return [default_label.to_owned(), "—".into(), "—".into(), "—".into()];
            };
            let period = match window.get("duration_seconds").and_then(Value::as_u64) {
                Some(60) => "1m".to_owned(),
                Some(3600) => "1h".to_owned(),
                Some(86400) => "24h".to_owned(),
                Some(604800) => "7d".to_owned(),
                Some(2592000) => "30d".to_owned(),
                Some(seconds) => format!("{seconds}s"),
                None => default_label.to_owned(),
            };
            let totals = window.get("totals").unwrap_or(&Value::Null);
            [
                period,
                count(totals.get("request_count"), exact),
                count(totals.get("total_tokens"), exact),
                cost(totals),
            ]
        })
        .collect(),
    )
}

fn rate_table_widths(rows: &[[String; 4]]) -> [u16; 4] {
    std::array::from_fn(|column| {
        let width = rows
            .iter()
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or_default()
            .max(RATE_TABLE_HEADERS[column].chars().count());
        u16::try_from(width).unwrap_or(u16::MAX)
    })
}

fn rate_table_total_width(rows: &[[String; 4]]) -> u16 {
    // Three one-cell column gaps plus the panel's left/right borders.
    rate_table_widths(rows)
        .into_iter()
        .fold(5u16, u16::saturating_add)
}

fn wrap_physical_lines(lines: Vec<String>, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for line in lines {
        // Generated punctuation is single-width. For unusual external Unicode
        // text, use a conservative half width to accommodate wide glyphs.
        let chunk = if line.chars().all(|c| c.is_ascii() || matches!(c, '—' | '·')) {
            width.max(1)
        } else {
            (width / 2).max(1)
        };
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            wrapped.push(String::new());
        }
        for part in chars.chunks(chunk) {
            wrapped.push(part.iter().collect());
        }
    }
    wrapped
}

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(Style::default().fg(MUTED))
}

pub fn render(frame: &mut Frame<'_>, view: &View<'_>) {
    let area = frame.area();
    if area.width < 42 || area.height < 10 {
        frame.render_widget(
            Paragraph::new("Terminal too small. Resize to at least 42 × 10.\nq: quit")
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    let mut header = vec![Line::from(vec![
        Span::styled(
            " COPILOT MONITOR ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {}  {}   {}",
            if view.exact_counts {
                "EXACT"
            } else {
                "K/M (~ rounded)"
            },
            endpoint_label(view.endpoint),
            clean(view.period)
        )),
    ])];
    let mode = if view.paused { "PAUSED" } else { "AUTO" };
    header.push(Line::from(format!(
        " {mode} · {} · updated {}",
        clean(view.status),
        clean(view.last_updated)
    )));
    frame.render_widget(Paragraph::new(header), parts[0]);
    let tab = view.tab.min(5);
    let labels = if area.width < 48 {
        ["1:O", "2:Q", "3:M", "4:R", "5:S", "6:C"]
    } else {
        [
            "Overview", "Quota", "Models", "Requests", "Sessions", "Catalog",
        ]
    };
    frame.render_widget(
        Tabs::new(labels)
            .block(panel("Views"))
            .select(tab)
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .divider("  "),
        parts[1],
    );
    match tab {
        0 => overview(frame, parts[2], view),
        1 => quota(frame, parts[2], view),
        2 => models(frame, parts[2], view),
        3 => events(frame, parts[2], view),
        4 => sessions(frame, parts[2], view),
        _ => catalog_ui::render(
            frame,
            parts[2],
            &catalog_ui::CatalogView {
                models: view.catalog,
                summary: view.summary,
                selected: view.catalog_selected,
                sort_label: view.catalog_sort_label,
                sort_key: view.catalog_sort_key,
                detail_scroll: view.catalog_detail_scroll,
                status: view.catalog_status,
                exact_counts: view.exact_counts,
                filter: view.catalog_filter,
                filter_prompt: view.catalog_filter_prompt,
            },
        ),
    }
    let controls = if view.tab == 1 {
        " 1–6 Tab  u units  b billing settings  p period  r refresh  space pause  q quit"
    } else if view.tab == 5 {
        " s sort  o order  u units  ↑/↓ model  Home/End  PgUp/PgDn details  r refresh  q quit"
    } else if view.tab == 2 && view.show_model_detail {
        " PgUp/PgDn scroll  d return  u units  q quit"
    } else if view.tab == 2 {
        " d pricing  ↑/↓ model  u units  p period  r refresh  q quit"
    } else if view.tab == 3 && view.show_event_detail {
        " ↑/↓ detail  u units  d return  ←/→ page  p period  r refresh  q quit"
    } else if view.tab == 3 {
        " 1–6 Tab  u units  p period  r refresh  space pause  ↑/↓ scroll  ←/→ page  d detail  q quit"
    } else {
        " 1–6 Tab  u units  p period  r refresh  space pause  ↑/↓ scroll  ←/→ page  q quit"
    };
    let footer = if let Some(error) = view.error {
        vec![
            Line::styled(
                format!(" {}", clean(error)),
                Style::default().fg(Color::Red),
            ),
            Line::styled(controls, Style::default().fg(MUTED)),
        ]
    } else {
        vec![
            Line::styled(controls, Style::default().fg(MUTED)),
            Line::styled(
                " ~ = rounded; counts are reported; costs are estimated, not bills.",
                Style::default().fg(MUTED),
            ),
        ]
    };
    frame.render_widget(Paragraph::new(footer), parts[3]);
    if view.confirm_billing {
        let width = area.width.min(82);
        let height = area.height.min(12);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new("Open GitHub Copilot billing settings?\ny open / n cancel\nSelection changes are made by you in GitHub; this gateway does not switch automatically.\nhttps://github.com/settings/copilot/features")
                .block(panel("Confirm browser action"))
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

fn overview(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let totals = view
        .summary
        .and_then(|s| s.get("totals"))
        .unwrap_or(&Value::Null);
    let rolling = rate_table_rows(view.rates, view.exact_counts);
    let rate_height = u16::try_from(
        rolling
            .as_ref()
            .map_or(3, |rows| rows.len().saturating_add(3)),
    )
    .unwrap_or(u16::MAX);
    let rate_too_wide = rolling
        .as_ref()
        .is_some_and(|rows| rate_table_total_width(rows) > area.width);
    if view.rates.is_some()
        && (rate_too_wide
            || area.height
                < 3u16
                    .saturating_add(4)
                    .saturating_add(rate_height)
                    .saturating_add(1))
    {
        frame.render_widget(
            Paragraph::new("Resize taller or wider to show overview and all rolling windows.")
                .block(panel("Overview"))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let parts = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(area);
    let cards = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(parts[0]);
    for (rect, label, value, color) in [
        (
            cards[0],
            "Requests",
            count(totals.get("request_count"), view.exact_counts),
            Color::Magenta,
        ),
        (
            cards[1],
            "Tokens",
            count(totals.get("total_tokens"), view.exact_counts),
            ACCENT,
        ),
        (
            cards[2],
            "Input / Output",
            format!(
                "{} / {}",
                count(totals.get("input_tokens"), view.exact_counts),
                count(totals.get("output_tokens"), view.exact_counts)
            ),
            Color::Green,
        ),
        (cards[3], "Estimated", cost(totals), Color::Yellow),
    ] {
        frame.render_widget(
            Paragraph::new(value)
                .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
                .block(panel(label)),
            rect,
        );
    }
    let body = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(rate_height),
        Constraint::Min(1),
    ])
    .split(parts[1]);
    let cache_table = Table::new(
        [Row::new([
            count(totals.get("cache_read_input_tokens"), view.exact_counts),
            count(totals.get("cache_creation_input_tokens"), view.exact_counts),
            count(totals.get("total_nano_aiu"), view.exact_counts),
        ])],
        [
            Constraint::Percentage(34),
            Constraint::Percentage(34),
            Constraint::Percentage(32),
        ],
    )
    .header(
        Row::new(["Cache read", "Cache write", "Nano-AIU"])
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
    )
    .column_spacing(1)
    .block(panel("Cache & Copilot"));
    frame.render_widget(cache_table, body[0]);
    if let Some(rows) = rolling {
        let widths = rate_table_widths(&rows);
        let table = Table::new(
            rows.into_iter().map(Row::new),
            widths.map(Constraint::Length),
        )
        .header(
            Row::new(RATE_TABLE_HEADERS)
                .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        )
        .column_spacing(1)
        .block(panel("Rolling reported counts · costs estimated"));
        frame.render_widget(table, body[1]);
    } else {
        frame.render_widget(
            Paragraph::new("Rolling reported counts: unavailable")
                .block(panel("Rolling reported counts · costs estimated")),
            body[1],
        );
    }
    let values: Vec<u64> = view
        .daily
        .and_then(|v| v.get("days"))
        .and_then(Value::as_array)
        .map(|days| {
            days.iter()
                .rev()
                .take(180)
                .rev()
                .filter_map(|day| day.pointer("/totals/total_tokens").and_then(Value::as_u64))
                .collect()
        })
        .unwrap_or_default();
    if values.is_empty() {
        frame.render_widget(
            Paragraph::new("No daily usage reported yet.").block(panel("Daily tokens")),
            body[2],
        );
    } else {
        frame.render_widget(
            Sparkline::default()
                .data(&values)
                .style(Style::default().fg(ACCENT))
                .block(panel("Daily tokens · oldest → newest")),
            body[2],
        );
    }
}

fn quota(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let Some(windows) = view
        .quota
        .and_then(|q| q.get("windows"))
        .and_then(Value::as_array)
    else {
        frame.render_widget(
            Paragraph::new("No quota data available. Check sign-in and connection.")
                .block(panel("Copilot quota"))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    };
    if windows.is_empty() {
        frame.render_widget(
            Paragraph::new("GitHub reported no quota windows.").block(panel("Copilot quota")),
            area,
        );
        return;
    }
    let block = panel("Copilot quota · GitHub reported");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width);
    let token_based = view
        .quota
        .and_then(|quota| quota.get("tokenBasedBilling"))
        .and_then(Value::as_bool)
        == Some(true);
    let mut groups = Vec::new();
    for window in windows {
        let unlimited = window
            .get("unlimited")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let used = window
            .get("usedPercent")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite());
        let name = text(window, "name");
        let reset = window
            .get("resetDate")
            .and_then(Value::as_str)
            .map(clean)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                window
                    .get("resetAt")
                    .and_then(Value::as_i64)
                    .and_then(|seconds| Local.timestamp_opt(seconds, 0).single())
                    .map(|date| date.format("%Y-%m-%d %H:%M").to_string())
            })
            .unwrap_or_else(|| "—".into());
        // The upstream compatibility projection calls these "requests" even
        // for token-based billing. Avoid asserting unverified unit semantics.
        let unit = if token_based {
            "units".to_owned()
        } else {
            window
                .get("unit")
                .and_then(Value::as_str)
                .map(clean)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "unit not reported".into())
        };
        let limit = if unlimited && window.get("limit").is_none_or(Value::is_null) {
            "Unlimited".to_owned()
        } else {
            quota_quantity(window.get("limit"))
        };
        let reported_quota = quota_quantity(window.get("reportedQuotaRemaining"));
        // GitHub reports an integer `remaining` plus a more precise
        // `quota_remaining`. Prefer the precise value in the single display
        // line rather than presenting both as if they were separate balances.
        let remaining = if reported_quota != "—" {
            reported_quota
        } else {
            quota_quantity(window.get("remaining"))
        };
        let lines = vec![
            format!("{name} · {unit} · reset: {reset}"),
            format!(
                "Used (derived): {} / Limit: {limit}",
                quota_quantity(window.get("used"))
            ),
            format!("Remaining: {remaining}"),
        ];
        let lines = wrap_physical_lines(lines, width);
        groups.push((lines, unlimited, used));
    }
    let compact_required = groups
        .iter()
        .map(|(lines, _, _)| lines.len().saturating_add(1))
        .sum::<usize>()
        + usize::from(token_based);
    let boxed_required = groups
        .iter()
        .map(|(lines, unlimited, used)| {
            let gauge_height = if !unlimited && used.is_some() { 3 } else { 1 };
            lines.len().saturating_add(gauge_height)
        })
        .sum::<usize>()
        + usize::from(token_based);
    let boxed_gauges = boxed_required <= usize::from(inner.height);
    if compact_required > usize::from(inner.height) {
        frame.render_widget(
            Paragraph::new("Resize taller or wider to show all quota counts and reset times.")
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }
    let mut constraints: Vec<_> = groups
        .iter()
        .flat_map(|(lines, unlimited, used)| {
            let gauge_height = if boxed_gauges && !unlimited && used.is_some() {
                3
            } else {
                1
            };
            [
                Constraint::Length(u16::try_from(lines.len()).unwrap_or(u16::MAX)),
                Constraint::Length(gauge_height),
            ]
        })
        .collect();
    if token_based {
        constraints.insert(0, Constraint::Length(1));
    }
    let areas = Layout::vertical(constraints).split(inner);
    if token_based {
        frame.render_widget(
            Paragraph::new("Token-based billing; unit unconfirmed."),
            areas[0],
        );
    }
    let start = usize::from(token_based);
    for (index, (lines, unlimited, used)) in groups.into_iter().enumerate() {
        frame.render_widget(
            Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()),
            areas[start + index * 2],
        );
        let gauge_area = areas[start + index * 2 + 1];
        if unlimited {
            frame.render_widget(Paragraph::new("Unlimited"), gauge_area);
        } else if let Some(used) = used {
            let clamped = used.clamp(0.0, 100.0);
            let color = if used >= 90.0 {
                Color::Red
            } else if used >= 75.0 {
                Color::Yellow
            } else {
                Color::Green
            };
            frame.render_widget(
                Gauge::default()
                    .block(if boxed_gauges {
                        panel("Usage")
                    } else {
                        Block::default()
                    })
                    .gauge_style(Style::default().fg(color))
                    .ratio(clamped / 100.0)
                    .label(format!("{used}% used")),
                gauge_area,
            );
        } else {
            frame.render_widget(Paragraph::new("Percentage: —"), gauge_area);
        }
    }
}

fn models(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let rows = view
        .summary
        .and_then(|s| s.get("byModel"))
        .and_then(Value::as_array);
    let Some(rows) = rows else {
        frame.render_widget(
            Paragraph::new("Waiting for usage data…").block(panel("Models")),
            area,
        );
        return;
    };
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No models recorded for this period.").block(panel("Models")),
            area,
        );
        return;
    }
    let offset = view.model_offset.min(rows.len().saturating_sub(1));
    if view.show_model_detail {
        catalog_ui::render_usage_details(
            frame,
            area,
            &rows[offset],
            view.catalog,
            view.model_detail_scroll,
            view.exact_counts,
        );
        return;
    }
    let columns = [
        ("Requests", "request_count"),
        ("Input", "input_tokens"),
        ("Output", "output_tokens"),
        ("Cache read", "cache_read_input_tokens"),
        ("Cache write", "cache_creation_input_tokens"),
        ("Total", "total_tokens"),
    ];
    let widths: Vec<usize> = columns
        .iter()
        .map(|(label, key)| {
            rows.iter()
                .map(|m| count(m.get(*key), view.exact_counts).len())
                .max()
                .unwrap_or(0)
                .max(label.len())
        })
        .collect();
    let cost_width = rows
        .iter()
        .map(cost)
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("Est. cost".len());
    let ref_width = rows
        .iter()
        .map(|m| catalog_ui::reference_output(view.catalog, m).len())
        .max()
        .unwrap_or(0)
        .max("GH default out cr/1M".len());
    let aiu_width = rows
        .iter()
        .map(|m| catalog_ui::aiu(m.get("total_nano_aiu")).len())
        .max()
        .unwrap_or(0)
        .max("AIU (partial?)".len());
    let ttft_width = rows
        .iter()
        .map(|m| decimal(m.get("average_first_token_ms")).len())
        .max()
        .unwrap_or(0)
        .max("Avg TTFT ms".len());
    let speed_width = rows
        .iter()
        .map(|m| decimal(m.get("output_tokens_per_second")).len())
        .max()
        .unwrap_or(0)
        .max("Output tok/s".len());
    let required = 12usize
        + widths.iter().sum::<usize>()
        + cost_width
        + ref_width
        + aiu_width
        + ttft_width
        + speed_width
        + 11
        + 2;
    let wide = usize::from(area.width) >= required;
    let visible = if wide {
        usize::from(area.height.saturating_sub(3))
    } else {
        1
    };
    let shown = visible.min(rows.len().saturating_sub(offset));
    let title = if shown == 0 {
        "Model breakdown".into()
    } else {
        format!(
            "Model breakdown · {}–{} / {} · d pricing",
            offset + 1,
            offset + shown,
            rows.len()
        )
    };
    if !wide {
        let model = &rows[offset];
        let mut lines = vec![format!("Model: {}", text(model, "model"))];
        if area.width >= 80 {
            for pair in columns.chunks(2) {
                lines.push(
                    pair.iter()
                        .map(|(label, key)| {
                            format!("{label}: {}", count(model.get(*key), view.exact_counts))
                        })
                        .collect::<Vec<_>>()
                        .join("    "),
                );
            }
        } else {
            lines.extend(columns.iter().map(|(label, key)| {
                format!("{label}: {}", count(model.get(*key), view.exact_counts))
            }));
        }
        lines.push(format!("Estimated cost: {}", cost(model)));
        let price = format!(
            "GH default out cr/1M: {}",
            catalog_ui::reference_output(view.catalog, model)
        );
        let aiu = format!(
            "AIU (partial?): {}",
            catalog_ui::aiu(model.get("total_nano_aiu"))
        );
        let timing = format!(
            "Avg TTFT ms: {}",
            decimal(model.get("average_first_token_ms"))
        );
        let speed = format!(
            "Output tok/s: {}",
            decimal(model.get("output_tokens_per_second"))
        );
        if area.width >= 80 {
            lines.push(format!("{price}    {aiu}"));
            lines.push(format!("{timing}    {speed}"));
        } else {
            lines.extend([price, aiu, timing, speed]);
        }
        let lines = wrap_physical_lines(lines, usize::from(area.width.saturating_sub(2)));
        if lines.len() > usize::from(area.height.saturating_sub(2)) {
            frame.render_widget(
                Paragraph::new("Resize taller or wider to show all model statistics.")
                    .block(panel(&title))
                    .wrap(Wrap { trim: true }),
                area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
                    .block(panel(&title)),
                area,
            );
        }
        return;
    }
    let items = rows
        .iter()
        .skip(offset)
        .take(visible)
        .enumerate()
        .map(|(index, m)| {
            let mut cells = vec![Cell::from(if index == 0 {
                format!("> {}", text(m, "model"))
            } else {
                text(m, "model")
            })];
            cells.extend(
                columns
                    .iter()
                    .map(|(_, key)| Cell::from(count(m.get(*key), view.exact_counts))),
            );
            cells.push(Cell::from(cost(m)));
            cells.push(Cell::from(catalog_ui::reference_output(view.catalog, m)));
            cells.push(Cell::from(catalog_ui::aiu(m.get("total_nano_aiu"))));
            cells.push(Cell::from(decimal(m.get("average_first_token_ms"))));
            cells.push(Cell::from(decimal(m.get("output_tokens_per_second"))));
            Row::new(cells).style(if index == 0 {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            })
        });
    let mut constraints = vec![Constraint::Min(12)];
    constraints.extend(
        widths
            .iter()
            .map(|w| Constraint::Length(u16::try_from(*w).unwrap_or(u16::MAX))),
    );
    constraints.extend(
        [cost_width, ref_width, aiu_width, ttft_width, speed_width]
            .into_iter()
            .map(|w| Constraint::Length(u16::try_from(w).unwrap_or(u16::MAX))),
    );
    let table = Table::new(items, constraints)
        .header(
            Row::new([
                "Model",
                "Requests",
                "Input",
                "Output",
                "Cache read",
                "Cache write",
                "Total",
                "Est. cost",
                "GH default out cr/1M",
                "AIU (partial?)",
                "Avg TTFT ms",
                "Output tok/s",
            ])
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        )
        .column_spacing(1)
        .block(panel(&title));
    frame.render_widget(table, area);
}

/// Build physical lines once for consistent rendering and scroll bounds. The
/// detail object is deliberately never summed: provider fields may overlap.
fn event_detail_lines(event: &Value, exact: bool) -> Vec<String> {
    let mut lines = vec![
        format!("Model: {}", text(event, "model")),
        format!("Input: {}", count(event.get("input_tokens"), exact)),
        format!("Output: {}", count(event.get("output_tokens"), exact)),
        format!(
            "Cache read: {}",
            count(event.get("cache_read_input_tokens"), exact)
        ),
        format!(
            "Cache write: {}",
            count(event.get("cache_creation_input_tokens"), exact)
        ),
        format!("Total: {}", count(event.get("total_tokens"), exact)),
        "".into(),
        "Provider-reported subcounts:".into(),
        "Overlapping; do not sum.".into(),
    ];
    let mut details: Vec<_> = event
        .get("token_details")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter_map(|(key, value)| {
            let count = value.as_i64().filter(|n| *n >= 0)?;
            Some((clean(key), count))
        })
        .collect();
    details.sort_by(|a, b| a.0.cmp(&b.0));
    if details.is_empty() {
        lines.push("Not reported (including older rows).".into());
    } else {
        let omitted = details.len().saturating_sub(256);
        for (name, value) in details.into_iter().take(256) {
            // Names are normally ASCII field paths. Non-ASCII names use half
            // the minimum content width to accommodate double-width glyphs.
            let width = if name.is_ascii() { 38 } else { 19 };
            let chars: Vec<char> = name.chars().collect();
            if chars.is_empty() {
                lines.push("(unnamed field)".into());
            }
            for chunk in chars.chunks(width) {
                lines.push(chunk.iter().collect());
            }
            lines.push(format!("  {}", count(Some(&Value::from(value)), exact)));
        }
        if omitted > 0 {
            lines.push(format!("{omitted} additional fields not shown"));
        }
    }
    lines
}

/// Number of rendered detail lines for clamping keyboard scrolling.
pub(crate) fn event_detail_line_count(event: &Value) -> usize {
    event_detail_lines(event, false).len()
}

fn sessions(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let rows = view
        .sessions
        .and_then(|s| s.get("items"))
        .and_then(Value::as_array);
    let Some(rows) = rows else {
        frame.render_widget(
            Paragraph::new("Waiting for session usage…").block(panel("Sessions")),
            area,
        );
        return;
    };
    let total_pages = view
        .sessions
        .and_then(|s| s.get("total_pages"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let offset = view.session_offset.min(rows.len().saturating_sub(1));
    let mut title = format!("Sessions · page {} / {}", view.session_page, total_pages);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No reported usage sessions in this period.").block(panel(&title)),
            area,
        );
        return;
    }
    let selected = &rows[offset];
    // A previous fetch can complete after the selection changes. Never show
    // its data as if it belonged to the newly selected session.
    let selected_id = selected.get("session_id").and_then(Value::as_str);
    let scoped_rates = view.session_rates.filter(|r| {
        selected_id.is_some() && r.get("session_id").and_then(Value::as_str) == selected_id
    });
    let mut rolling = rate_lines(
        scoped_rates,
        usize::from(area.width.saturating_sub(2)),
        view.exact_counts,
    );
    if scoped_rates.is_none() {
        rolling = vec!["Session rates: unavailable".into()];
    }
    if selected
        .get("request_scoped_fallbacks")
        .and_then(Value::as_u64)
        .is_some_and(|count| count > 0)
    {
        rolling.insert(
            0,
            "Request-ID fallback; client session not established".into(),
        );
    }
    rolling = wrap_physical_lines(rolling, usize::from(area.width.saturating_sub(2)));
    let columns = [
        ("Requests", "request_count"),
        ("Input", "input_tokens"),
        ("Output", "output_tokens"),
        ("Cache read", "cache_read_input_tokens"),
        ("Cache write", "cache_creation_input_tokens"),
        ("Total", "total_tokens"),
    ];
    let widths: Vec<usize> = columns
        .iter()
        .map(|(label, key)| {
            rows.iter()
                .map(|row| count(row.get(*key), view.exact_counts).len())
                .max()
                .unwrap_or(0)
                .max(label.len())
        })
        .collect();
    let cost_width = rows
        .iter()
        .map(cost)
        .map(|s| s.len())
        .max()
        .unwrap_or(0)
        .max("Est. cost".len());
    let required = 16usize + widths.iter().sum::<usize>() + cost_width + 7 + 2;
    if usize::from(area.width) < required {
        title.push_str(&format!(" · {} / {}", offset + 1, rows.len()));
        let session = format!("Session: {}", text(selected, "session_id"));
        let mut lines: Vec<String> = Vec::new();
        // Wrap long IDs instead of silently showing the wrong/truncated session.
        let chunk = if session.is_ascii() {
            usize::from(area.width.saturating_sub(2)).max(1)
        } else {
            usize::from(area.width.saturating_sub(2)).max(1) / 2
        };
        let chars: Vec<char> = session.chars().collect();
        for part in chars.chunks(chunk.max(1)) {
            lines.push(part.iter().collect());
        }
        lines.extend(columns.iter().map(|(label, key)| {
            format!("{label}: {}", count(selected.get(*key), view.exact_counts))
        }));
        lines.push(format!("Estimated cost: {}", cost(selected)));
        lines.push("Rolling reported counts:".into());
        lines.extend(rolling);
        let lines = wrap_physical_lines(lines, usize::from(area.width.saturating_sub(2)));
        if lines.len() > usize::from(area.height.saturating_sub(2)) {
            frame.render_widget(
                Paragraph::new(
                    "Resize taller or wider to show all session counts and rolling windows.",
                )
                .block(panel(&title))
                .wrap(Wrap { trim: true }),
                area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
                    .block(panel(&title)),
                area,
            );
        }
        return;
    }
    let rates_height = u16::try_from(rolling.len().saturating_add(2)).unwrap_or(u16::MAX);
    if area.height < rates_height.saturating_add(5) {
        frame.render_widget(
            Paragraph::new("Resize taller to show session rows and rolling windows.")
                .block(panel(&title))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let layout =
        Layout::vertical([Constraint::Min(5), Constraint::Length(rates_height)]).split(area);
    let visible = usize::from(layout[0].height.saturating_sub(3));
    let shown = visible.min(rows.len().saturating_sub(offset));
    if shown > 0 {
        title.push_str(&format!(
            " · {}–{} / {}",
            offset + 1,
            offset + shown,
            rows.len()
        ));
    }
    let items = rows
        .iter()
        .skip(offset)
        .take(visible)
        .enumerate()
        .map(|(index, row)| {
            let mut cells = vec![Cell::from(if index == 0 {
                format!("> {}", text(row, "session_id"))
            } else {
                text(row, "session_id")
            })];
            cells.extend(
                columns
                    .iter()
                    .map(|(_, key)| Cell::from(count(row.get(*key), view.exact_counts))),
            );
            cells.push(Cell::from(cost(row)));
            Row::new(cells).style(if index == 0 {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            })
        });
    let mut constraints = vec![Constraint::Min(16)];
    constraints.extend(
        widths
            .iter()
            .map(|w| Constraint::Length(u16::try_from(*w).unwrap_or(u16::MAX))),
    );
    constraints.push(Constraint::Length(
        u16::try_from(cost_width).unwrap_or(u16::MAX),
    ));
    let table = Table::new(items, constraints)
        .header(
            Row::new([
                "Session",
                "Requests",
                "Input",
                "Output",
                "Cache read",
                "Cache write",
                "Total",
                "Est. cost",
            ])
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        )
        .column_spacing(1)
        .block(panel(&title));
    frame.render_widget(table, layout[0]);
    frame.render_widget(
        Paragraph::new(rolling.into_iter().map(Line::from).collect::<Vec<_>>())
            .block(panel("Selected session · rolling reported counts")),
        layout[1],
    );
}

fn events(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let rows = view
        .events
        .and_then(|e| e.get("items"))
        .and_then(Value::as_array);
    let Some(rows) = rows else {
        frame.render_widget(
            Paragraph::new("Waiting for request events…").block(panel("Requests")),
            area,
        );
        return;
    };
    let total_pages = view
        .events
        .and_then(|e| e.get("total_pages"))
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let mut title = format!("Requests · page {} / {}", view.page, total_pages);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No reported usage events on this page.").block(panel(&title)),
            area,
        );
        return;
    }
    let offset = view.event_offset.min(rows.len().saturating_sub(1));
    if view.show_event_detail {
        let lines = event_detail_lines(&rows[offset], view.exact_counts);
        let maximum = lines.len().saturating_sub(1).min(u16::MAX as usize);
        let scroll = u16::try_from(view.detail_scroll.min(maximum)).unwrap_or(u16::MAX);
        let detail_title = format!(
            "Request details · {} / {} · d: list",
            offset + 1,
            rows.len()
        );
        let detail = Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .block(panel(&detail_title))
            .scroll((scroll, 0));
        frame.render_widget(detail, area);
        return;
    }
    // Fit every numeric value in full, or switch to one vertically arranged
    // request. This prevents a narrow terminal from silently clipping counts.
    let columns = [
        ("Input", "input_tokens"),
        ("Output", "output_tokens"),
        ("Cache read", "cache_read_input_tokens"),
        ("Cache write", "cache_creation_input_tokens"),
        ("Total", "total_tokens"),
    ];
    let widths: Vec<u16> = columns
        .iter()
        .map(|(label, key)| {
            let length = rows
                .iter()
                .map(|e| count(e.get(*key), view.exact_counts).len())
                .max()
                .unwrap_or(0)
                .max(label.len());
            u16::try_from(length).unwrap_or(u16::MAX)
        })
        .collect();
    // Fixed metadata columns, numeric columns, eight separators and borders.
    let required = 14u16 + 12 + 10 + 10 + widths.iter().sum::<u16>() + 8 + 2;
    let wide = area.width >= required;
    let visible = if wide {
        usize::from(area.height.saturating_sub(3))
    } else {
        1
    };
    let shown = visible.min(rows.len().saturating_sub(offset));
    if shown > 0 {
        title.push_str(&format!(
            " · {}–{} / {}",
            offset + 1,
            offset + shown,
            rows.len()
        ));
    }
    if !wide {
        // Header, tabs and footer consume seven terminal rows; this area needs
        // borders and six lines to show metadata plus all five token values.
        if area.height < 8 {
            frame.render_widget(
                Paragraph::new("Resize to at least 42 × 15 to show request token details.")
                    .block(panel(&title))
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        let event = &rows[offset];
        let time = event
            .get("created_at_ms")
            .and_then(Value::as_i64)
            .and_then(|ms| Local.timestamp_millis_opt(ms).single())
            .map(|dt| dt.format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "—".into());
        let heading = format!(
            "{} · {} · {} · {}",
            text(event, "model"),
            time,
            text(event, "endpoint"),
            text(event, "request_outcome")
        );
        let mut lines = vec![Line::styled(heading, Style::default().fg(ACCENT))];
        lines.extend(columns.iter().map(|(label, key)| {
            Line::from(format!(
                "{label}: {}",
                count(event.get(*key), view.exact_counts)
            ))
        }));
        frame.render_widget(Paragraph::new(lines).block(panel(&title)), area);
        return;
    }
    let items = rows
        .iter()
        .skip(offset)
        .take(visible)
        .enumerate()
        .map(|(index, e)| {
            let time = e
                .get("created_at_ms")
                .and_then(Value::as_i64)
                .and_then(|ms| Local.timestamp_millis_opt(ms).single())
                .map(|dt| dt.format("%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "—".into());
            let outcome = text(e, "request_outcome");
            let color = match outcome.as_str() {
                "error" => Color::Red,
                "cancelled" | "incomplete" => Color::Yellow,
                _ => Color::Green,
            };
            Row::new(vec![
                Cell::from(time),
                Cell::from(if index == 0 {
                    format!("> {}", text(e, "model"))
                } else {
                    text(e, "model")
                }),
                Cell::from(text(e, "endpoint")),
                Cell::from(count(e.get("input_tokens"), view.exact_counts)),
                Cell::from(count(e.get("output_tokens"), view.exact_counts)),
                Cell::from(count(e.get("cache_read_input_tokens"), view.exact_counts)),
                Cell::from(count(
                    e.get("cache_creation_input_tokens"),
                    view.exact_counts,
                )),
                Cell::from(count(e.get("total_tokens"), view.exact_counts)),
                Cell::from(outcome).style(Style::default().fg(color)),
            ])
            .style(if index == 0 {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            })
        });
    let table = Table::new(
        items,
        [
            Constraint::Length(14),
            Constraint::Min(12),
            Constraint::Length(10),
            Constraint::Length(widths[0]),
            Constraint::Length(widths[1]),
            Constraint::Length(widths[2]),
            Constraint::Length(widths[3]),
            Constraint::Length(widths[4]),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new([
            "Local time",
            "Model",
            "Endpoint",
            "Input",
            "Output",
            "Cache read",
            "Cache write",
            "Total",
            "Outcome",
        ])
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
    )
    .column_spacing(1)
    .block(panel(&title));
    frame.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::super::catalog::{PriceTier, VerifiedQuote};
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    fn view<'a>(
        summary: &'a Value,
        daily: &'a Value,
        events: &'a Value,
        quota: &'a Value,
    ) -> View<'a> {
        View {
            endpoint: "http://secret:password@localhost:4141/path?api_key=bad",
            period: "today",
            tab: 0,
            summary: Some(summary),
            daily: Some(daily),
            events: Some(events),
            sessions: None,
            rates: None,
            session_rates: None,
            quota: Some(quota),
            status: "ok",
            page: 1,
            model_offset: 0,
            catalog: None,
            show_model_detail: false,
            model_detail_scroll: 0,
            catalog_selected: 0,
            catalog_sort_label: "output desc",
            catalog_sort_key: SortKey::Output,
            catalog_filter: "",
            catalog_filter_prompt: "Filter (press /)",
            catalog_detail_scroll: 0,
            catalog_status: "ok",
            event_offset: 0,
            session_page: 1,
            session_offset: 0,
            show_event_detail: false,
            detail_scroll: 0,
            confirm_billing: false,
            exact_counts: true,
            paused: false,
            last_updated: "now",
            error: None,
        }
    }
    fn render_text(terminal: &Terminal<TestBackend>) -> String {
        let b = terminal.backend().buffer();
        let mut result = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                result.push_str(b[(x, y)].symbol());
            }
            result.push('\n');
        }
        result
    }

    #[test]
    fn displays_metrics_tabs_and_hides_credentials() {
        let summary = json!({"totals":{"total_tokens":42,"request_count":3,"input_tokens":30,"output_tokens":12,"cache_read_input_tokens":4,"cache_creation_input_tokens":2,"total_nano_aiu":100,"costs":[{"currency":"USD","amount":0.01}]},"byModel":[{"model":"gpt-thing","request_count":3,"total_tokens":42}]});
        let daily = json!({"days":[{"totals":{"total_tokens":42}}]});
        let events = json!({"items":[{"model":"gpt-thing","endpoint":"responses","total_tokens":42,"request_outcome":"completed","created_at_ms":1795180800000i64}],"total_pages":2});
        let quota = json!({"windows":[{"name":"Premium","usedPercent":35.0}]});
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut v = view(&summary, &daily, &events, &quota);
        for (tab, expected) in [
            (0, "42"),
            (1, "Premium"),
            (2, "gpt-thing"),
            (3, "responses"),
        ] {
            v.tab = tab;
            terminal.draw(|f| render(f, &v)).unwrap();
            let text = render_text(&terminal);
            assert!(text.contains(expected), "missing {expected}: {text}");
            assert!(!text.contains("password"));
            assert!(!text.contains("api_key"));
        }
    }

    #[test]
    fn small_terminal_missing_data_and_control_chars_are_safe() {
        assert_eq!(clean("bad\u{1b}[31m\nvalue"), "bad [31m value");
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        let null = Value::Null;
        let mut v = view(&null, &null, &null, &null);
        v.summary = None;
        v.quota = None;
        v.events = None;
        v.daily = None;
        v.error = Some("bad\u{1b}[31m");
        for tab in 0..4 {
            v.tab = tab;
            terminal.draw(|f| render(f, &v)).unwrap();
        }
        assert!(render_text(&terminal).contains("Terminal too small"));
        let mut terminal = Terminal::new(TestBackend::new(55, 14)).unwrap();
        for tab in 0..4 {
            v.tab = tab;
            terminal.draw(|f| render(f, &v)).unwrap();
        }
        assert!(!render_text(&terminal).contains('\u{1b}'));
    }

    #[test]
    fn model_offset_scrolls_and_clamps() {
        let summary = json!({"byModel":[{"model":"first"},{"model":"second"},{"model":"third"}]});
        let null = Value::Null;
        let mut v = view(&summary, &null, &null, &null);
        v.tab = 2;
        v.model_offset = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("second"));
        assert!(!display.contains("first"));
        v.model_offset = usize::MAX;
        terminal.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&terminal).contains("third"));
    }

    #[test]
    fn model_breakdown_includes_cache_write_total_and_timing_without_clipping() {
        let null = Value::Null;
        let summary = json!({"byModel":[{"model":"example","request_count":11,"input_tokens":101,"output_tokens":202,"cache_read_input_tokens":303,"cache_creation_input_tokens":404,"total_tokens":1010,"average_first_token_ms":12.5,"output_tokens_per_second":8.75,"costs":[]} ]});
        let mut v = view(&summary, &null, &null, &null);
        v.tab = 2;
        for (width, height) in [(42, 24), (80, 24), (178, 46)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&terminal);
            for expected in [
                "example",
                "Requests",
                "Input",
                "Output",
                "Cache read",
                "Cache write",
                "Total",
                "Avg TTFT ms",
                "Output tok/s",
                "101",
                "202",
                "303",
                "404",
                "1,010",
                "12.50",
                "8.75",
            ] {
                assert!(
                    display.contains(expected),
                    "missing {expected} at {width}x{height}: {display}"
                );
            }
        }
    }

    #[test]
    fn model_breakdown_handles_large_numbers_and_missing_metrics() {
        let null = Value::Null;
        let summary = json!({"byModel":[{"model":"example","input_tokens":i64::MAX,"output_tokens":null,"cache_read_input_tokens":0,"cache_creation_input_tokens":7,"total_tokens":7}]});
        let mut v = view(&summary, &null, &null, &null);
        v.tab = 2;
        let mut terminal = Terminal::new(TestBackend::new(42, 24)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("9,223,372,036,854,775,807"), "{display}");
        assert!(display.contains("Output: —"), "{display}");
        assert!(display.contains("Avg TTFT ms: —"), "{display}");
        assert!(display.contains("Output tok/s: —"), "{display}");
        let mut short = Terminal::new(TestBackend::new(42, 12)).unwrap();
        short.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&short).contains("Resize taller"));
    }

    #[test]
    fn model_prices_and_spend_are_distinct_and_exactly_joined() {
        let null = Value::Null;
        let summary = json!({"byModel":[
            {"model":"model-a","total_nano_aiu":1,"costs":[{"currency":"USD","amount":0.01}]},
            {"model":"model-b","total_nano_aiu":null,"costs":[]}
        ]});
        let catalog = vec![CatalogModel {
            id: "model-a".into(),
            default_prices: Some(PriceTier {
                verified: Some(VerifiedQuote {
                    input: Some(2.0),
                    output: Some(10.0),
                    cache_read: Some(0.5),
                    cache_write: Some(3.0),
                    source: "GitHub reference 2026-11-20; promo applies".into(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let mut v = view(&summary, &null, &null, &null);
        v.tab = 2;
        v.catalog = Some(&catalog);
        for (width, height) in [(178, 46), (80, 24), (42, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&terminal);
            for value in [
                "model-a",
                "cr/1M",
                "AIU (partial?)",
                "0.000000001",
                "USD 0.0100",
            ] {
                assert!(
                    display.contains(value),
                    "missing {value} at {width}x{height}: {display}"
                );
            }
            v.show_model_detail = true;
            let mut seen = String::new();
            let count = catalog_ui::usage_detail_line_count(
                &summary["byModel"][0],
                Some(&catalog),
                width,
                v.exact_counts,
            );
            for scroll in 0..count {
                v.model_detail_scroll = scroll as u16;
                terminal.draw(|f| render(f, &v)).unwrap();
                seen.push_str(&render_text(&terminal));
            }
            for value in [
                "0.000000001",
                "reference credits",
                "output 10",
                "cache read 0.5",
                "GitHub reference",
            ] {
                assert!(
                    seen.contains(value),
                    "missing {value} at {width}x{height}: {seen}"
                );
            }
            v.show_model_detail = false;
            v.model_detail_scroll = 0;
        }
        assert_eq!(
            catalog_ui::reference_output(Some(&catalog), &summary["byModel"][1]),
            "—"
        );
        assert_eq!(
            catalog_ui::reference_output(None, &summary["byModel"][0]),
            "—"
        );
        let duplicate = vec![catalog[0].clone(), catalog[0].clone()];
        assert_eq!(
            catalog_ui::reference_output(Some(&duplicate), &summary["byModel"][0]),
            "—"
        );
    }

    #[test]
    fn sixth_tab_displays_catalog_and_its_controls() {
        let null = Value::Null;
        let catalog = [CatalogModel {
            id: "available-model".into(),
            ..Default::default()
        }];
        let mut v = view(&null, &null, &null, &null);
        v.tab = 5;
        v.catalog = Some(&catalog);
        for (width, height) in [(178, 46), (80, 24), (42, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&terminal);
            for expected in ["available-model", "s sort", "o order"] {
                assert!(display.contains(expected), "{width}x{height}: {display}");
            }
        }
    }

    #[test]
    fn event_offset_scrolls_and_clamps_within_page() {
        let null = Value::Null;
        let events = json!({"items":[{"model":"first"},{"model":"second"},{"model":"third"}],"total_pages":2});
        let mut v = view(&null, &null, &events, &null);
        v.tab = 3;
        v.page = 2;
        v.event_offset = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("second"));
        assert!(!display.contains("first"));
        assert!(display.contains("page 2 / 2"));
        v.event_offset = usize::MAX;
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("third"));
        assert!(display.contains("3–3 / 3"));
    }

    #[test]
    fn request_counts_are_separate_and_visible_in_wide_and_narrow_terminals() {
        let null = Value::Null;
        let events = json!({"items":[{"model":"example","endpoint":"responses","request_outcome":"completed","input_tokens":101,"output_tokens":202,"cache_read_input_tokens":303,"cache_creation_input_tokens":404,"total_tokens":1010}],"total_pages":1});
        let mut v = view(&null, &null, &events, &null);
        v.tab = 3;
        for (width, height) in [(42, 24), (80, 24), (120, 24), (160, 24), (178, 46)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&terminal);
            for expected in [
                "Input",
                "Output",
                "Cache read",
                "Cache write",
                "Total",
                "101",
                "202",
                "303",
                "404",
                "1,010",
            ] {
                assert!(
                    display.contains(expected),
                    "missing {expected} at width {width}: {display}"
                );
            }
        }
    }

    #[test]
    fn large_and_missing_values_are_not_silently_truncated_or_invented() {
        let null = Value::Null;
        let events = json!({"items":[{"model":"example","input_tokens":i64::MAX,"output_tokens":null,"cache_read_input_tokens":3,"cache_creation_input_tokens":4,"total_tokens":10}],"total_pages":1});
        let mut v = view(&null, &null, &events, &null);
        v.tab = 3;
        let mut terminal = Terminal::new(TestBackend::new(42, 20)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("9,223,372,036,854,775,807"), "{display}");
        assert!(display.contains("Output: —"), "{display}");
        let mut short = Terminal::new(TestBackend::new(42, 12)).unwrap();
        short.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&short).contains("Resize to at least 42"));
    }

    #[test]
    fn request_detail_shows_reported_subcounts_without_adding_them() {
        let null = Value::Null;
        let events = json!({"items":[
            {"model":"first"},
            {"model":"second","input_tokens":100,"output_tokens":20,"cache_read_input_tokens":10,"cache_creation_input_tokens":4,"total_tokens":134,
             "token_details":{"output_tokens_details.reasoning_tokens":7,"input_tokens_details.audio_tokens":3,"bad":-1}}
        ],"total_pages":1});
        let mut v = view(&null, &null, &events, &null);
        v.tab = 3;
        v.event_offset = 1;
        v.show_event_detail = true;
        let mut terminal = Terminal::new(TestBackend::new(178, 46)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        for expected in [
            "Model: second",
            "Input: 100",
            "Output: 20",
            "Cache read: 10",
            "Cache write: 4",
            "Total: 134",
            "Provider-reported subcounts",
            "Overlapping; do not sum",
            "output_tokens_details.reasoning_tokens",
            "input_tokens_details.audio_tokens",
        ] {
            assert!(display.contains(expected), "missing {expected}: {display}");
        }
        assert!(!display.contains("bad"));
        assert!(!display.contains("Model: first"));
    }

    #[test]
    fn request_detail_historic_missing_and_scroll_bounds_are_explicit() {
        let null = Value::Null;
        let historic = json!({"model":"old","input_tokens":null});
        let events = json!({"items":[historic.clone()],"total_pages":1});
        let mut v = view(&null, &null, &events, &null);
        v.tab = 3;
        v.show_event_detail = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("Input: —"), "{display}");
        assert!(
            display.contains("Not reported (including older rows)"),
            "{display}"
        );
        v.detail_scroll = usize::MAX;
        terminal.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&terminal).contains("Not reported (including older rows)"));
        assert_eq!(event_detail_line_count(&historic), 10);
    }

    #[test]
    fn detail_long_fields_and_values_are_bounded_on_small_terminals() {
        let event = json!({"token_details": {"an_extremely_long_provider_reported_token_field_name_that_must_be_wrapped":i64::MAX}});
        let lines = event_detail_lines(&event, true);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("9,223,372,036,854,775,807"))
        );
        assert!(lines.iter().all(|line| line.chars().count() <= 38));
        let unsafe_event = json!({"token_details":{"path\u{1b}[31m":2}});
        assert!(
            event_detail_lines(&unsafe_event, true)
                .iter()
                .all(|line| !line.contains('\u{1b}'))
        );
    }

    #[test]
    fn selected_request_is_marked_and_incomplete_is_yellow() {
        let null = Value::Null;
        let events = json!({"items":[{"model":"first"},{"model":"selected","request_outcome":"incomplete","total_tokens":3}],"total_pages":1});
        let mut v = view(&null, &null, &events, &null);
        v.tab = 3;
        v.event_offset = 1;
        let mut terminal = Terminal::new(TestBackend::new(178, 46)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("> selected"), "{display}");
        let (y, line) = display
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("incomplete"))
            .unwrap();
        let index = line.find("incomplete").unwrap();
        let x = line[..index].chars().count();
        let cell = &terminal.backend().buffer()[(x as u16, y as u16)];
        assert_eq!(cell.fg, Color::Yellow);
        assert_eq!(cell.bg, Color::DarkGray);
    }

    #[test]
    fn rolling_overview_displays_reported_counts_and_does_not_invent_missing_windows() {
        let null = Value::Null;
        let rates = json!({"windows":[
            {"id":"minute","duration_seconds":60,"totals":{"request_count":11,"total_tokens":111,"costs":[]}},
            {"id":"hour","duration_seconds":3600,"totals":{"request_count":22,"total_tokens":222,"costs":[]}}
        ]});
        let mut v = view(&null, &null, &null, &null);
        v.rates = Some(&rates);
        for (width, height) in [(42, 24), (178, 46)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&terminal);
            assert!(display.contains("Rolling reported counts"), "{display}");
            for header in RATE_TABLE_HEADERS {
                assert!(display.contains(header), "missing {header}: {display}");
            }
            let rows = rate_table_rows(Some(&rates), false).unwrap();
            assert_eq!(
                rows[0],
                ["1m".to_owned(), "11".into(), "111".into(), "—".into()]
            );
            assert_eq!(
                rows[1],
                ["1h".to_owned(), "22".into(), "222".into(), "—".into()]
            );
            assert_eq!(
                rows[2],
                ["24h".to_owned(), "—".into(), "—".into(), "—".into()]
            );
            assert_eq!(
                rows[3],
                ["7d".to_owned(), "—".into(), "—".into(), "—".into()]
            );
            assert_eq!(
                rows[4],
                ["30d".to_owned(), "—".into(), "—".into(), "—".into()]
            );
        }
    }

    #[test]
    fn rolling_week_and_month_are_visible_globally_and_per_session() {
        let null = Value::Null;
        let windows = json!([
            {"id":"minute","duration_seconds":60,"totals":{"request_count":1,"total_tokens":10,"costs":[{"currency":"USD","amount":0.01}]}},
            {"id":"hour","duration_seconds":3600,"totals":{"request_count":2,"total_tokens":20,"costs":[{"currency":"USD","amount":0.02}]}},
            {"id":"day","duration_seconds":86400,"totals":{"request_count":3,"total_tokens":30,"costs":[{"currency":"USD","amount":0.03}]}},
            {"id":"week","duration_seconds":604800,"totals":{"request_count":4,"total_tokens":40,"costs":[{"currency":"USD","amount":0.04}]}},
            {"id":"month","duration_seconds":2592000,"totals":{"request_count":5,"total_tokens":50,"costs":[{"currency":"USD","amount":0.05}]}}
        ]);
        let rates = json!({"windows": windows});
        let session_rates = json!({"session_id":"selected","windows":rates["windows"]});
        let sessions_data = json!({"items":[{"session_id":"selected","request_count":5,"input_tokens":20,"output_tokens":30,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"total_tokens":50,"costs":[]}],"total_pages":1});
        let mut v = view(&null, &null, &null, &null);
        v.rates = Some(&rates);
        v.sessions = Some(&sessions_data);
        v.session_rates = Some(&session_rates);
        for (width, height) in [(178, 46), (80, 24), (42, 24)] {
            for tab in [0, 4] {
                v.tab = tab;
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| render(f, &v)).unwrap();
                let display = render_text(&terminal);
                if tab == 0 {
                    for header in RATE_TABLE_HEADERS {
                        assert!(display.contains(header), "missing {header}: {display}");
                    }
                    let rows = rate_table_rows(Some(&rates), false).unwrap();
                    for (index, expected) in [
                        ["1m", "1", "10", "USD 0.0100"],
                        ["1h", "2", "20", "USD 0.0200"],
                        ["24h", "3", "30", "USD 0.0300"],
                        ["7d", "4", "40", "USD 0.0400"],
                        ["30d", "5", "50", "USD 0.0500"],
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        for column in 0..4 {
                            assert_eq!(
                                rows[index][column], expected[column],
                                "wrong table row {index}, column {column}"
                            );
                        }
                    }
                } else {
                    for expected in [
                        "1m: 1 req / 10 tok",
                        "1h: 2 req / 20 tok",
                        "24h: 3 req / 30 tok",
                        "7d: 4 req / 40 tok",
                        "30d: 5 req / 50 tok",
                        "USD 0.0400",
                        "USD 0.0500",
                    ] {
                        assert!(
                            display.contains(expected),
                            "missing {expected} in tab {tab} at {width}x{height}: {display}"
                        );
                    }
                }
            }
        }
        // Very small screens must explicitly request a resize rather than truncate.
        for tab in [0, 4] {
            v.tab = tab;
            let mut terminal = Terminal::new(TestBackend::new(42, 12)).unwrap();
            terminal.draw(|f| render(f, &v)).unwrap();
            assert!(render_text(&terminal).contains("Resize"));
        }
    }

    #[test]
    fn rolling_week_and_month_respect_count_units_and_missing_values() {
        let rates = json!({"windows":[
            {"id":"week","duration_seconds":604800,"totals":{"request_count":1200,"total_tokens":1234567}},
            {"id":"month","duration_seconds":2592000,"totals":{"request_count":null}}
        ]});
        let compact = rate_lines(Some(&rates), 100, false).join("\n");
        assert!(compact.contains("7d: 1.2K req / ~1.23M tok"), "{compact}");
        assert!(compact.contains("30d: — req / — tok"), "{compact}");
        let exact = rate_lines(Some(&rates), 100, true).join("\n");
        assert!(exact.contains("7d: 1,200 req / 1,234,567 tok"), "{exact}");
        assert!(exact.contains("30d: — req / — tok"), "{exact}");
    }

    #[test]
    fn sessions_display_breakdowns_and_only_the_selected_sessions_rates() {
        let null = Value::Null;
        let sessions_data = json!({"items":[
            {"session_id":"first","request_count":1,"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30,"cache_creation_input_tokens":40,"total_tokens":100,"costs":[]},
            {"session_id":"selected","request_count":2,"input_tokens":101,"output_tokens":202,"cache_read_input_tokens":303,"cache_creation_input_tokens":404,"total_tokens":1010,"costs":[]}
        ],"total_pages":2});
        let rates = json!({"session_id":"selected","windows":[
            {"id":"minute","duration_seconds":60,"totals":{"request_count":12,"total_tokens":120,"costs":[]}},
            {"id":"hour","duration_seconds":3600,"totals":{"request_count":23,"total_tokens":230,"costs":[]}},
            {"id":"day","duration_seconds":86400,"totals":{"request_count":34,"total_tokens":340,"costs":[]}}
        ]});
        let mut v = view(&null, &null, &null, &null);
        v.tab = 4;
        v.session_page = 2;
        v.session_offset = 1;
        v.sessions = Some(&sessions_data);
        v.session_rates = Some(&rates);
        for (width, height) in [(42, 30), (100, 30), (178, 46)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&terminal);
            for expected in [
                "Sessions",
                "page 2 / 2",
                "selected",
                "101",
                "202",
                "303",
                "404",
                "1,010",
                "12 req / 120 tok",
                "23 req / 230 tok",
                "34 req / 340 tok",
            ] {
                assert!(
                    display.contains(expected),
                    "missing {expected} at {width}x{height}: {display}"
                );
            }
        }
        let stale = json!({"session_id":"first","windows":[{"id":"minute","duration_seconds":60,"totals":{"request_count":9999,"total_tokens":9999}}]});
        v.session_rates = Some(&stale);
        let mut terminal = Terminal::new(TestBackend::new(178, 46)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("Session rates: unavailable"), "{display}");
        assert!(!display.contains("9,999"));
    }

    #[test]
    fn quota_counts_show_reported_values_without_inventing_units_or_remaining() {
        let null = Value::Null;
        let quota = json!({"tokenBasedBilling":true,"windows":[
            {"name":"Premium","unit":"requests","used":400.0,"limit":1000.0,"remaining":600.0,"reportedQuotaRemaining":600.25,"usedPercent":40.0,"resetDate":"2026-12-01"},
            {"name":"Chat","unit":"requests","unlimited":true},
            {"name":"Completions","unit":"requests","used":11.0,"limit":40.0,"usedPercent":27.5}
        ]});
        let mut v = view(&null, &null, &null, &quota);
        v.tab = 1;
        for (width, height) in [(42, 24), (80, 24), (178, 46)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&terminal);
            for expected in [
                "Token-based billing; unit unconfirmed",
                "Premium",
                "Used (derived): 400 / Limit: 1,000",
                "Remaining: 600.25",
                "reset: 2026-12-01",
                "40% used",
                "Chat",
                "Limit: Unlimited",
                "Completions",
                "Used (derived): 11 / Limit: 40",
                "Remaining: —",
            ] {
                assert!(
                    display.contains(expected),
                    "missing {expected} at {width}x{height}: {display}"
                );
            }
            if width == 178 {
                assert!(
                    display.contains("┌ Usage"),
                    "missing boxed gauge: {display}"
                );
            }
            assert!(
                !display.contains("quota_remaining:"),
                "duplicate balance: {display}"
            );
            assert!(
                !display.contains("requests"),
                "unsupported unit claim: {display}"
            );
            assert!(
                !display.contains("Remaining: 29"),
                "inferred remaining: {display}"
            );
        }
    }

    #[test]
    fn quota_preserves_other_reported_units_and_explicit_missing_values() {
        let null = Value::Null;
        let quota = json!({"windows":[{"name":"Premium","unit":"credits","remaining":12.5,"usedPercent":10.0}]});
        let mut v = view(&null, &null, &null, &quota);
        v.tab = 1;
        let mut terminal = Terminal::new(TestBackend::new(178, 46)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        for expected in [
            "Premium",
            "Premium · credits · reset: —",
            "Used (derived): — / Limit: —",
            "Remaining: 12.5",
            "reset: —",
            "10% used",
        ] {
            assert!(display.contains(expected), "missing {expected}: {display}");
        }
        assert_eq!(quota_quantity(Some(&json!(-0.25))), "-0.25");
        assert_eq!(quota_quantity(Some(&json!(600.25))), "600.25");
    }

    #[test]
    fn billing_action_requires_an_explicit_confirmation_prompt() {
        let null = Value::Null;
        let mut v = view(&null, &null, &null, &null);
        v.tab = 1;
        let mut wide = Terminal::new(TestBackend::new(178, 46)).unwrap();
        wide.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&wide).contains("b billing settings"));
        assert!(!render_text(&wide).contains("y open / n cancel"));
        v.confirm_billing = true;
        wide.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&wide);
        for expected in [
            "Open GitHub Copilot billing settings?",
            "y open / n cancel",
            "Selection changes are made by you in GitHub;",
            "this gateway does not switch",
            "automatically.",
            "https://github.com/settings/copilot/features",
        ] {
            assert!(display.contains(expected), "missing {expected}: {display}");
        }
        for height in [10, 12] {
            let mut narrow = Terminal::new(TestBackend::new(42, height)).unwrap();
            narrow.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&narrow);
            assert!(display.contains("y open / n cancel"), "{display}");
            assert!(
                display.contains("Selection changes are made by you"),
                "{display}"
            );
            assert!(
                display.contains("this gateway does not switch"),
                "{display}"
            );
            assert!(display.contains("automatically."), "{display}");
            assert!(
                display.contains("github.com/settings/copilot/feat"),
                "{display}"
            );
            assert!(display.contains("ures"), "{display}");
        }
    }

    #[test]
    fn count_units_are_exact_or_explicitly_approximate_without_float_precision_loss() {
        for (value, expected) in [
            (0i64, "0"),
            (999, "999"),
            (1000, "1K"),
            (1200, "1.2K"),
            (1230, "1.23K"),
            (1234, "~1.23K"),
            (999_995, "~1M"),
            (1_000_000, "1M"),
            (1_500_000, "1.5M"),
            (1_000_000_000, "1B"),
            (1_000_000_000_000, "1T"),
            (-1234, "~-1.23K"),
            (i64::MAX, "~9,223,372.04T"),
        ] {
            assert_eq!(count(Some(&json!(value)), false), expected, "{value}");
        }
        assert_eq!(
            count(Some(&json!(i64::MIN)), true),
            "-9,223,372,036,854,775,808"
        );
        assert_eq!(
            count(Some(&json!(u64::MAX)), true),
            "18,446,744,073,709,551,615"
        );
        assert_eq!(count(Some(&json!(u64::MAX)), false), "~18,446,744.07T");
        assert_eq!(count(None, false), "—");
        assert_eq!(count(Some(&json!(1.5)), false), "—");
        assert_eq!(quota_quantity(Some(&json!(144853.3))), "144,853.3");
    }

    #[test]
    fn count_toggle_reformats_all_usage_views_and_preserves_costs_and_quota() {
        let null = Value::Null;
        let totals = json!({"request_count":1200,"input_tokens":1234,"output_tokens":5678,"cache_read_input_tokens":1500,"cache_creation_input_tokens":4000,"total_tokens":11412,"total_nano_aiu":1234,"costs":[{"currency":"USD","amount":0.01}]});
        let summary = json!({"totals":totals,"byModel":[{"model":"example","request_count":1200,"input_tokens":1234,"output_tokens":5678,"cache_read_input_tokens":1500,"cache_creation_input_tokens":4000,"total_tokens":11412,"costs":[{"currency":"USD","amount":0.01}]}]});
        let events = json!({"items":[{"model":"example","input_tokens":1234,"output_tokens":5678,"cache_read_input_tokens":1500,"cache_creation_input_tokens":4000,"total_tokens":11412,"token_details":{"output_tokens_details.reasoning_tokens":1234}}],"total_pages":1});
        let sessions_data = json!({"items":[{"session_id":"session","request_count":1200,"input_tokens":1234,"output_tokens":5678,"cache_read_input_tokens":1500,"cache_creation_input_tokens":4000,"total_tokens":11412,"costs":[{"currency":"USD","amount":0.01}]}],"total_pages":1});
        let rates = json!({"windows":[{"id":"minute","duration_seconds":60,"totals":{"request_count":1200,"total_tokens":1234}},{"id":"hour","duration_seconds":3600,"totals":{}},{"id":"day","duration_seconds":86400,"totals":{}}]});
        let session_rates = json!({"session_id":"session","windows":rates["windows"]});
        let quota = json!({"tokenBasedBilling":true,"windows":[{"name":"Premium","remaining":144853.3,"reportedQuotaRemaining":144853.3}]});
        let mut v = view(&summary, &null, &events, &quota);
        v.sessions = Some(&sessions_data);
        v.rates = Some(&rates);
        v.session_rates = Some(&session_rates);
        let mut terminal = Terminal::new(TestBackend::new(178, 46)).unwrap();
        v.exact_counts = false;
        for tab in [0, 2, 3, 4] {
            v.tab = tab;
            terminal.draw(|f| render(f, &v)).unwrap();
            let display = render_text(&terminal);
            assert!(display.contains("~1.23K"), "tab {tab}: {display}");
            assert!(display.contains("u units"), "tab {tab}: {display}");
        }
        v.tab = 3;
        v.show_event_detail = true;
        terminal.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&terminal).contains("~1.23K"));
        v.exact_counts = true;
        terminal.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&terminal).contains("1,234"));
        v.tab = 0;
        v.show_event_detail = false;
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("1,234"));
        assert!(display.contains("USD 0.0100"));
        v.exact_counts = false;
        v.tab = 1;
        terminal.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&terminal).contains("144,853.3"));
        let mut narrow = Terminal::new(TestBackend::new(42, 24)).unwrap();
        narrow.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&narrow).contains("u units"));
    }

    #[test]
    fn session_request_id_fallback_is_not_mislabeled_as_known_session() {
        let null = Value::Null;
        let sessions_data = json!({"items":[{"session_id":"request-id", "request_scoped_fallbacks":1}],"total_pages":1});
        let mut v = view(&null, &null, &null, &null);
        v.tab = 4;
        v.sessions = Some(&sessions_data);
        let mut terminal = Terminal::new(TestBackend::new(178, 46)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        assert!(
            render_text(&terminal).contains("Request-ID fallback; client session not established")
        );
    }
}
