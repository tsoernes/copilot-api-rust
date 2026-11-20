//! Presentation only: input, refresh and networking are owned by the caller.
use chrono::{Local, TimeZone};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table, Tabs, Wrap},
};
use serde_json::Value;

pub struct View<'a> {
    pub endpoint: &'a str,
    pub period: &'a str,
    pub tab: usize,
    pub summary: Option<&'a Value>,
    pub daily: Option<&'a Value>,
    pub events: Option<&'a Value>,
    pub quota: Option<&'a Value>,
    pub status: &'a str,
    pub page: usize,
    pub model_offset: usize,
    pub event_offset: usize,
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

fn number(value: Option<&Value>) -> String {
    let Some(value) = value.and_then(Value::as_i64) else {
        return "—".into();
    };
    let s = value.to_string();
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i).is_multiple_of(3) && *c != '-' && chars[0] != '-' {
            out.push(',');
        }
        out.push(*c);
    }
    out
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
            Constraint::Min(3),
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
            "  {}   {}",
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
    let tab = view.tab.min(3);
    frame.render_widget(
        Tabs::new(["Overview", "Quota", "Models", "Requests"])
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
        _ => events(frame, parts[2], view),
    }
    let footer = if let Some(error) = view.error {
        vec![
            Line::styled(
                format!(" {}", clean(error)),
                Style::default().fg(Color::Red),
            ),
            Line::styled(
                " 1–4/Tab views  p period  r refresh  space pause  ↑/↓ scroll  ←/→ page  q quit",
                Style::default().fg(MUTED),
            ),
        ]
    } else {
        vec![
            Line::styled(
                " 1–4/Tab views  p period  r refresh  space pause  ↑/↓ scroll  ←/→ page  q quit",
                Style::default().fg(MUTED),
            ),
            Line::styled(
                " Usage is upstream-reported; cost is estimated, not a bill.",
                Style::default().fg(MUTED),
            ),
        ]
    };
    frame.render_widget(Paragraph::new(footer), parts[3]);
}

fn overview(frame: &mut Frame<'_>, area: Rect, view: &View<'_>) {
    let Some(summary) = view.summary else {
        frame.render_widget(
            Paragraph::new("Waiting for usage data…").block(panel("Overview")),
            area,
        );
        return;
    };
    let totals = summary.get("totals").unwrap_or(&Value::Null);
    let parts = Layout::vertical([Constraint::Length(5), Constraint::Min(3)]).split(area);
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
            number(totals.get("request_count")),
            Color::Magenta,
        ),
        (
            cards[1],
            "Tokens",
            number(totals.get("total_tokens")),
            ACCENT,
        ),
        (
            cards[2],
            "Input / Output",
            format!(
                "{} / {}",
                number(totals.get("input_tokens")),
                number(totals.get("output_tokens"))
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
    let body = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).split(parts[1]);
    let detail = format!(
        " Cache read: {}    Cache write: {}    Nano-AIU: {}",
        number(totals.get("cache_read_input_tokens")),
        number(totals.get("cache_creation_input_tokens")),
        number(totals.get("total_nano_aiu"))
    );
    frame.render_widget(
        Paragraph::new(detail).block(panel("Cache & Copilot")),
        body[0],
    );
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
            body[1],
        );
    } else {
        frame.render_widget(
            Sparkline::default()
                .data(&values)
                .style(Style::default().fg(ACCENT))
                .block(panel("Daily tokens · oldest → newest")),
            body[1],
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
    let rows = Layout::vertical(
        windows
            .iter()
            .map(|_| Constraint::Length(3))
            .collect::<Vec<_>>(),
    )
    .split(inner);
    for (window, rect) in windows.iter().zip(rows.iter()) {
        if rect.height == 0 {
            continue;
        }
        let unlimited = window
            .get("unlimited")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let used = window
            .get("usedPercent")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite());
        let label = if unlimited {
            "Unlimited".to_owned()
        } else if let Some(used) = used {
            format!("{:.1}% used", used)
        } else {
            "Usage unavailable".into()
        };
        let name = text(window, "name");
        let reset = window
            .get("resetDate")
            .and_then(Value::as_str)
            .map(clean)
            .map(|d| format!("  reset {d}"))
            .unwrap_or_default();
        let title = format!("{name} · {label}{reset}");
        if unlimited || used.is_none() {
            frame.render_widget(Paragraph::new(title), *rect);
        } else {
            let used = used.unwrap_or(0.0).clamp(0.0, 100.0);
            let color = if used >= 90.0 {
                Color::Red
            } else if used >= 75.0 {
                Color::Yellow
            } else {
                Color::Green
            };
            frame.render_widget(
                Gauge::default()
                    .block(Block::default().title(title))
                    .gauge_style(Style::default().fg(color))
                    .ratio(used / 100.0)
                    .label(label),
                *rect,
            );
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
    let visible = usize::from(area.height.saturating_sub(3));
    let offset = view.model_offset.min(rows.len().saturating_sub(1));
    let shown = visible.min(rows.len().saturating_sub(offset));
    let title = if shown == 0 {
        "Model breakdown".into()
    } else {
        format!(
            "Model breakdown · {}–{} / {}",
            offset + 1,
            offset + shown,
            rows.len()
        )
    };
    let items = rows.iter().skip(offset).take(visible).map(|m| {
        Row::new(vec![
            Cell::from(text(m, "model")),
            Cell::from(number(m.get("request_count"))),
            Cell::from(number(m.get("input_tokens"))),
            Cell::from(number(m.get("output_tokens"))),
            Cell::from(number(m.get("cache_read_input_tokens"))),
            Cell::from(cost(m)),
        ])
    });
    let table = Table::new(
        items,
        [
            Constraint::Percentage(28),
            Constraint::Percentage(10),
            Constraint::Percentage(14),
            Constraint::Percentage(14),
            Constraint::Percentage(14),
            Constraint::Percentage(20),
        ],
    )
    .header(
        Row::new(["Model", "Req", "Input", "Output", "Cache", "Est. cost"])
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
    )
    .column_spacing(1)
    .block(panel(&title));
    frame.render_widget(table, area);
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
    let visible = usize::from(area.height.saturating_sub(3));
    let offset = view.event_offset.min(rows.len().saturating_sub(1));
    let shown = visible.min(rows.len().saturating_sub(offset));
    if shown > 0 {
        title.push_str(&format!(
            " · {}–{} / {}",
            offset + 1,
            offset + shown,
            rows.len()
        ));
    }
    let items = rows.iter().skip(offset).take(visible).map(|e| {
        let time = e
            .get("created_at_ms")
            .and_then(Value::as_i64)
            .and_then(|ms| Local.timestamp_millis_opt(ms).single())
            .map(|dt| dt.format("%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "—".into());
        let outcome = text(e, "request_outcome");
        let color = match outcome.as_str() {
            "error" => Color::Red,
            "cancelled" => Color::Yellow,
            _ => Color::Green,
        };
        Row::new(vec![
            Cell::from(time),
            Cell::from(text(e, "model")),
            Cell::from(text(e, "endpoint")),
            Cell::from(number(e.get("total_tokens"))),
            Cell::from(outcome).style(Style::default().fg(color)),
        ])
    });
    let table = Table::new(
        items,
        [
            Constraint::Length(14),
            Constraint::Percentage(32),
            Constraint::Percentage(27),
            Constraint::Percentage(20),
            Constraint::Percentage(21),
        ],
    )
    .header(
        Row::new(["Local time", "Model", "Endpoint", "Tokens", "Outcome"])
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
    )
    .column_spacing(1)
    .block(panel(&title));
    frame.render_widget(table, area);
}

#[cfg(test)]
mod tests {
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
            quota: Some(quota),
            status: "ok",
            page: 1,
            model_offset: 0,
            event_offset: 0,
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
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        terminal.draw(|f| render(f, &v)).unwrap();
        let display = render_text(&terminal);
        assert!(display.contains("second"));
        assert!(!display.contains("first"));
        v.model_offset = usize::MAX;
        terminal.draw(|f| render(f, &v)).unwrap();
        assert!(render_text(&terminal).contains("third"));
    }

    #[test]
    fn event_offset_scrolls_and_clamps_within_page() {
        let null = Value::Null;
        let events = json!({"items":[{"model":"first"},{"model":"second"},{"model":"third"}],"total_pages":2});
        let mut v = view(&null, &null, &events, &null);
        v.tab = 3;
        v.page = 2;
        v.event_offset = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
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
}
