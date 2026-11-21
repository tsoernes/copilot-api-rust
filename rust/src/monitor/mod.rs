mod catalog;
mod catalog_ui;
mod data;
mod pricing_reference;
mod ui;

use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use serde_json::Value;
use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::MissedTickBehavior,
};
use tracing_subscriber::fmt::MakeWriter;

use catalog::{CatalogModel, SortKey, SortOrder};
use data::{MonitorClient, UsageSnapshot};

fn sort_models(models: &mut [CatalogModel], key: SortKey, order: SortOrder) {
    if key == SortKey::Output && order == SortOrder::Descending {
        catalog::sort_catalog(models);
    } else {
        catalog::sort_catalog_by(models, key, order);
    }
}

const PERIODS: &[&str] = &[
    "today",
    "this_week",
    "last_7_days",
    "this_month",
    "last_30_days",
    "lifetime",
];
const PAGE_SIZE: usize = 20;

/// Keep server errors visible without printing into the alternate screen.
#[derive(Clone, Default)]
pub struct MemoryLog(Arc<Mutex<VecDeque<String>>>);
pub struct LogWriter {
    data: Vec<u8>,
    log: MemoryLog,
}
impl<'a> MakeWriter<'a> for MemoryLog {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            data: Vec::new(),
            log: self.clone(),
        }
    }
}
impl Write for LogWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let take = data.len().min(1024usize.saturating_sub(self.data.len()));
        self.data.extend_from_slice(&data[..take]);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for LogWriter {
    fn drop(&mut self) {
        let line: String = String::from_utf8_lossy(&self.data)
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        if !line.is_empty()
            && let Ok(mut log) = self.log.0.lock()
        {
            if log.len() >= 16 {
                log.pop_front();
            }
            log.push_back(line);
        }
    }
}
impl MemoryLog {
    fn latest(&self) -> Option<String> {
        self.0.lock().ok().and_then(|log| log.back().cloned())
    }
}

struct Screen {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}
impl Screen {
    fn enter() -> Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("monitor requires an interactive terminal on stdin and stdout");
        }
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut output = io::stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen) {
            let _ = execute!(output, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error).context("enter alternate screen");
        }
        match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(error).context("initialize terminal")
            }
        }
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Default)]
struct App {
    tab: usize,
    period: usize,
    page: usize,
    model_offset: usize,
    show_model_detail: bool,
    model_detail_scroll: u16,
    selected_model_id: Option<String>,
    event_offset: usize,
    session_page: usize,
    session_offset: usize,
    show_event_detail: bool,
    detail_scroll: usize,
    selected_event_id: Option<i64>,
    confirm_billing: bool,
    notice: Option<String>,
    exact_counts: bool,
    paused: bool,
    summary: Option<Value>,
    daily: Option<Value>,
    events: Option<Value>,
    sessions: Option<Value>,
    rates: Option<Value>,
    session_rates: Option<Value>,
    quota: Option<Value>,
    catalog: Option<Vec<CatalogModel>>,
    catalog_selected: usize,
    catalog_filter: String,
    catalog_filter_active: bool,
    catalog_detail_scroll: u16,
    catalog_sort_key: SortKey,
    catalog_sort_order: SortOrder,
    catalog_updated: Option<Instant>,
    catalog_error: Option<String>,
    usage_updated: Option<Instant>,
    quota_updated: Option<Instant>,
    usage_error: Option<String>,
    quota_error: Option<String>,
}
impl App {
    fn period(&self) -> &'static str {
        PERIODS[self.period]
    }
    fn clear_usage(&mut self) {
        self.summary = None;
        self.daily = None;
        self.events = None;
        self.sessions = None;
        self.session_rates = None;
        self.usage_updated = None;
        self.show_model_detail = false;
        self.model_detail_scroll = 0;
        self.selected_model_id = None;
    }
    fn apply_usage(&mut self, snapshot: UsageSnapshot) {
        let mut failures = Vec::new();
        let selected_session = self.selected_session();
        for (name, result, destination) in [
            ("summary", snapshot.summary, &mut self.summary),
            ("daily", snapshot.daily, &mut self.daily),
            ("events", snapshot.events, &mut self.events),
            ("sessions", snapshot.sessions, &mut self.sessions),
            ("rates", snapshot.rates, &mut self.rates),
        ] {
            match result {
                Ok(value) => *destination = Some(value),
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
        if let Some(result) = snapshot.session_rates {
            match result {
                Ok(value) => self.session_rates = Some(value),
                Err(error) => {
                    self.session_rates = None;
                    failures.push(format!("session rates: {error}"));
                }
            }
        } else {
            self.session_rates = None;
        }
        if let Some(sessions) = self
            .sessions
            .as_ref()
            .and_then(|v| v.get("items"))
            .and_then(Value::as_array)
        {
            if let Some(id) = selected_session
                && let Some(index) = sessions.iter().position(|session| {
                    session.get("session_id").and_then(Value::as_str) == Some(id.as_str())
                })
            {
                self.session_offset = index;
            }
            self.session_offset = self.session_offset.min(sessions.len().saturating_sub(1));
        }
        if let Some(models) = self
            .summary
            .as_ref()
            .and_then(|v| v.get("byModel"))
            .and_then(Value::as_array)
        {
            if self.show_model_detail
                && let Some(id) = self.selected_model_id.as_deref()
            {
                if let Some(index) = models
                    .iter()
                    .position(|model| model.get("model").and_then(Value::as_str) == Some(id))
                {
                    self.model_offset = index;
                } else {
                    self.show_model_detail = false;
                    self.model_detail_scroll = 0;
                    self.selected_model_id = None;
                }
            }
            self.model_offset = self.model_offset.min(models.len().saturating_sub(1));
        }
        if let Some(events) = self
            .events
            .as_ref()
            .and_then(|v| v.get("items"))
            .and_then(Value::as_array)
        {
            if self.show_event_detail
                && let Some(id) = self.selected_event_id
            {
                if let Some(index) = events
                    .iter()
                    .position(|event| event.get("id").and_then(Value::as_i64) == Some(id))
                {
                    self.event_offset = index;
                } else {
                    self.show_event_detail = false;
                    self.detail_scroll = 0;
                    self.selected_event_id = None;
                }
            }
            self.event_offset = self.event_offset.min(events.len().saturating_sub(1));
        }
        if failures.is_empty() {
            self.usage_updated = Some(Instant::now());
            self.usage_error = None;
        } else {
            self.usage_error = Some(format!("STALE / unavailable: {}", failures.join("; ")));
        }
    }
    fn sort_catalog(&mut self) {
        let selected = self
            .filtered_catalog()
            .get(self.catalog_selected)
            .map(|m| m.id.clone());
        let query = self.catalog_filter.trim().to_lowercase();
        if let Some(models) = &mut self.catalog {
            sort_models(models, self.catalog_sort_key, self.catalog_sort_order);
            self.catalog_selected = selected
                .as_ref()
                .and_then(|id| {
                    models
                        .iter()
                        .filter(|model| {
                            query.is_empty()
                                || model.id.to_lowercase().contains(&query)
                                || model.name.to_lowercase().contains(&query)
                                || model
                                    .vendor
                                    .as_deref()
                                    .is_some_and(|vendor| vendor.to_lowercase().contains(&query))
                        })
                        .position(|model| &model.id == id)
                })
                .unwrap_or(0);
        }
        self.catalog_detail_scroll = 0;
    }
    fn filtered_catalog(&self) -> Vec<CatalogModel> {
        let query = self.catalog_filter.trim().to_lowercase();
        self.catalog
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|model| {
                query.is_empty()
                    || model.id.to_lowercase().contains(&query)
                    || model.name.to_lowercase().contains(&query)
                    || model
                        .vendor
                        .as_deref()
                        .is_some_and(|vendor| vendor.to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    }
    fn apply_catalog(&mut self, result: std::result::Result<Value, String>) {
        let selected = self
            .filtered_catalog()
            .get(self.catalog_selected)
            .map(|m| m.id.clone());
        match result.and_then(|value| catalog::parse_catalog(&value).map_err(str::to_owned)) {
            Ok(mut models) => {
                pricing_reference::annotate_official_prices(&mut models);
                sort_models(&mut models, self.catalog_sort_key, self.catalog_sort_order);
                self.catalog = Some(models);
                self.catalog_selected = selected
                    .as_ref()
                    .and_then(|id| {
                        self.filtered_catalog()
                            .iter()
                            .position(|model| &model.id == id)
                    })
                    .unwrap_or(0);
                self.catalog_updated = Some(Instant::now());
                self.catalog_error = None;
            }
            Err(error) => {
                self.catalog_error = Some(format!("Catalog stale / unavailable: {error}"));
                if let Some(models) = &mut self.catalog {
                    pricing_reference::annotate_official_prices(models);
                }
                self.sort_catalog();
            }
        }
    }
    fn catalog_sort_label(&self) -> String {
        let key = match self.catalog_sort_key {
            SortKey::Output => "output",
            SortKey::Input => "input",
            SortKey::CacheRead => "cache read",
            SortKey::CacheWrite => "cache write",
            SortKey::Name => "name",
        };
        let order = match self.catalog_sort_order {
            SortOrder::Ascending => "asc",
            SortOrder::Descending => "desc",
        };
        match self.catalog_sort_key {
            SortKey::Name => format!("{key} {order} · unknown last"),
            SortKey::Output => format!("highest published {key} {order} · unknown last"),
            _ => format!("default {key} {order} · unknown last"),
        }
    }
    fn selected_session(&self) -> Option<String> {
        self.sessions
            .as_ref()
            .and_then(|v| v.get("items"))
            .and_then(Value::as_array)
            .and_then(|items| items.get(self.session_offset))
            .and_then(|v| v.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
    fn apply_quota(&mut self, result: std::result::Result<Value, String>) {
        match result {
            Ok(value) => {
                self.quota = Some(value);
                self.quota_updated = Some(Instant::now());
                self.quota_error = None;
            }
            Err(error) => self.quota_error = Some(format!("Quota stale / unavailable: {error}")),
        }
    }
    fn key_in_terminal(&mut self, key: KeyEvent, width: u16, height: u16) -> Action {
        // Never allow an unseen billing confirmation to open a browser.
        if width < 42 || height < 10 {
            if self.confirm_billing {
                self.confirm_billing = false;
            }
            if key.code == KeyCode::Char('b') {
                return Action::None;
            }
        }
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let step = height.saturating_sub(12).max(1);
            let maximum = if self.tab == 5 {
                let filtered = self.filtered_catalog();
                filtered
                    .get(self.catalog_selected)
                    .map(|model| {
                        catalog_ui::detail_line_count(
                            model,
                            self.summary.as_ref(),
                            &filtered,
                            width,
                            self.exact_counts,
                        )
                    })
                    .unwrap_or(0)
            } else if self.tab == 2 && self.show_model_detail {
                self.summary
                    .as_ref()
                    .and_then(|v| v.get("byModel"))
                    .and_then(Value::as_array)
                    .and_then(|rows| rows.get(self.model_offset))
                    .map(|usage| {
                        catalog_ui::usage_detail_line_count(
                            usage,
                            self.catalog.as_deref(),
                            width,
                            self.exact_counts,
                        )
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            if maximum > 0 {
                let scroll = if self.tab == 5 {
                    &mut self.catalog_detail_scroll
                } else {
                    &mut self.model_detail_scroll
                };
                *scroll = match key.code {
                    KeyCode::PageUp => scroll.saturating_sub(step),
                    KeyCode::PageDown => scroll
                        .saturating_add(step)
                        .min(u16::try_from(maximum.saturating_sub(1)).unwrap_or(u16::MAX)),
                    _ => *scroll,
                };
                return Action::None;
            }
        }
        self.key(key)
    }
    fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if self.catalog_filter_active && self.tab == 5 {
            match key.code {
                KeyCode::Enter => {
                    self.catalog_filter_active = false;
                    self.catalog_selected = 0;
                    self.catalog_detail_scroll = 0;
                    return Action::None;
                }
                KeyCode::Esc => {
                    self.catalog_filter_active = false;
                    self.catalog_filter.clear();
                    self.catalog_selected = 0;
                    self.catalog_detail_scroll = 0;
                    return Action::None;
                }
                KeyCode::Backspace => {
                    self.catalog_filter.pop();
                    self.catalog_selected = 0;
                    self.catalog_detail_scroll = 0;
                    return Action::None;
                }
                KeyCode::Char(c) if !c.is_control() && self.catalog_filter.chars().count() < 64 => {
                    self.catalog_filter.push(c);
                    self.catalog_selected = 0;
                    self.catalog_detail_scroll = 0;
                    return Action::None;
                }
                _ => return Action::None,
            }
        }
        if self.confirm_billing {
            return match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.confirm_billing = false;
                    Action::OpenBilling
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.confirm_billing = false;
                    Action::None
                }
                KeyCode::Char('q') => Action::Quit,
                _ => Action::None,
            };
        }
        if self.tab == 2 && self.show_model_detail && key.code == KeyCode::Esc {
            self.show_model_detail = false;
            self.model_detail_scroll = 0;
            self.selected_model_id = None;
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('1') => {
                self.tab = 0;
                Action::None
            }
            KeyCode::Char('2') => {
                self.tab = 1;
                Action::None
            }
            KeyCode::Char('3') => {
                self.tab = 2;
                Action::None
            }
            KeyCode::Char('4') => {
                self.tab = 3;
                Action::None
            }
            KeyCode::Char('5') => {
                self.tab = 4;
                Action::None
            }
            KeyCode::Char('6') => {
                self.tab = 5;
                Action::None
            }
            KeyCode::Tab => {
                self.tab = (self.tab + 1) % 6;
                Action::None
            }
            KeyCode::BackTab => {
                self.tab = (self.tab + 5) % 6;
                Action::None
            }
            KeyCode::Char('p') => {
                self.period = (self.period + 1) % PERIODS.len();
                self.page = 1;
                self.model_offset = 0;
                self.event_offset = 0;
                self.session_page = 1;
                self.session_offset = 0;
                self.show_event_detail = false;
                self.detail_scroll = 0;
                self.selected_event_id = None;
                self.clear_usage();
                Action::Usage
            }
            KeyCode::Char('r') => Action::Both,
            KeyCode::Char('u') => {
                self.exact_counts = !self.exact_counts;
                Action::None
            }
            KeyCode::Char('b') if self.tab == 1 => {
                self.confirm_billing = true;
                Action::None
            }
            KeyCode::Char('d') if self.tab == 3 => {
                if self.show_event_detail {
                    self.show_event_detail = false;
                    self.selected_event_id = None;
                } else if let Some(event) = self
                    .events
                    .as_ref()
                    .and_then(|v| v.get("items"))
                    .and_then(Value::as_array)
                    .and_then(|events| events.get(self.event_offset))
                {
                    self.show_event_detail = true;
                    self.selected_event_id = event.get("id").and_then(Value::as_i64);
                }
                self.detail_scroll = 0;
                Action::None
            }
            KeyCode::Char('d') if self.tab == 2 => {
                if self.show_model_detail {
                    self.show_model_detail = false;
                    self.selected_model_id = None;
                } else if let Some(model) = self
                    .summary
                    .as_ref()
                    .and_then(|v| v.get("byModel"))
                    .and_then(Value::as_array)
                    .and_then(|rows| rows.get(self.model_offset))
                {
                    self.show_model_detail = true;
                    self.selected_model_id = model
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                self.model_detail_scroll = 0;
                Action::None
            }
            KeyCode::Char('s') if self.tab == 5 => {
                self.catalog_sort_key = match self.catalog_sort_key {
                    SortKey::Output => SortKey::Input,
                    SortKey::Input => SortKey::CacheRead,
                    SortKey::CacheRead => SortKey::CacheWrite,
                    SortKey::CacheWrite => SortKey::Name,
                    SortKey::Name => SortKey::Output,
                };
                self.sort_catalog();
                self.catalog_selected = 0;
                Action::None
            }
            KeyCode::Char('/') if self.tab == 5 => {
                self.catalog_filter_active = true;
                Action::None
            }
            KeyCode::Char('o') if self.tab == 5 => {
                self.catalog_sort_order = match self.catalog_sort_order {
                    SortOrder::Descending => SortOrder::Ascending,
                    SortOrder::Ascending => SortOrder::Descending,
                };
                self.sort_catalog();
                self.catalog_selected = 0;
                Action::None
            }
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                if self.paused {
                    Action::None
                } else {
                    Action::Both
                }
            }
            KeyCode::Up | KeyCode::Char('k') if self.tab == 2 && !self.show_model_detail => {
                self.model_offset = self.model_offset.saturating_sub(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') if self.tab == 4 && self.session_offset > 0 => {
                self.session_offset -= 1;
                self.session_rates = None;
                Action::Usage
            }
            KeyCode::Down | KeyCode::Char('j') if self.tab == 4 => {
                let count = self
                    .sessions
                    .as_ref()
                    .and_then(|v| v.get("items"))
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                if self.session_offset + 1 < count {
                    self.session_offset += 1;
                    self.session_rates = None;
                    Action::Usage
                } else {
                    Action::None
                }
            }
            KeyCode::Up | KeyCode::Char('k') if self.tab == 3 && self.show_event_detail => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') if self.tab == 3 && self.show_event_detail => {
                if let Some(event) = self
                    .events
                    .as_ref()
                    .and_then(|v| v.get("items"))
                    .and_then(Value::as_array)
                    .and_then(|events| events.get(self.event_offset))
                {
                    let maximum = ui::event_detail_line_count(event).saturating_sub(1);
                    self.detail_scroll = self.detail_scroll.saturating_add(1).min(maximum);
                }
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') if self.tab == 3 => {
                self.event_offset = self.event_offset.saturating_sub(1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') if self.tab == 3 => {
                let count = self
                    .events
                    .as_ref()
                    .and_then(|v| v.get("items"))
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                if self.event_offset + 1 < count {
                    self.event_offset += 1;
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') if self.tab == 2 && !self.show_model_detail => {
                let count = self
                    .summary
                    .as_ref()
                    .and_then(|v| v.get("byModel"))
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                if self.model_offset + 1 < count {
                    self.model_offset += 1;
                }
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') if self.tab == 5 => {
                self.catalog_selected = self.catalog_selected.saturating_sub(1);
                self.catalog_detail_scroll = 0;
                Action::None
            }
            KeyCode::Home if self.tab == 5 => {
                self.catalog_selected = 0;
                self.catalog_detail_scroll = 0;
                Action::None
            }
            KeyCode::End if self.tab == 5 => {
                self.catalog_selected = self.filtered_catalog().len().saturating_sub(1);
                self.catalog_detail_scroll = 0;
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') if self.tab == 5 => {
                let count = self.filtered_catalog().len();
                if self.catalog_selected + 1 < count {
                    self.catalog_selected += 1;
                    self.catalog_detail_scroll = 0;
                }
                Action::None
            }
            KeyCode::Left if self.tab == 3 && self.page > 1 => {
                self.page -= 1;
                self.event_offset = 0;
                self.show_event_detail = false;
                self.detail_scroll = 0;
                self.selected_event_id = None;
                self.events = None;
                Action::Usage
            }
            KeyCode::Right if self.tab == 3 => {
                let pages = self
                    .events
                    .as_ref()
                    .and_then(|v| v.get("total_pages"))
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                if (self.page as u64) < pages {
                    self.page += 1;
                    self.event_offset = 0;
                    self.show_event_detail = false;
                    self.detail_scroll = 0;
                    self.selected_event_id = None;
                    self.events = None;
                    Action::Usage
                } else {
                    Action::None
                }
            }
            KeyCode::Left if self.tab == 4 && self.session_page > 1 => {
                self.session_page -= 1;
                self.session_offset = 0;
                self.sessions = None;
                self.session_rates = None;
                Action::Usage
            }
            KeyCode::Right if self.tab == 4 => {
                let pages = self
                    .sessions
                    .as_ref()
                    .and_then(|v| v.get("total_pages"))
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                if (self.session_page as u64) < pages {
                    self.session_page += 1;
                    self.session_offset = 0;
                    self.sessions = None;
                    self.session_rates = None;
                    Action::Usage
                } else {
                    Action::None
                }
            }
            _ => Action::None,
        }
    }
}
#[derive(PartialEq, Eq)]
enum Action {
    None,
    Quit,
    Usage,
    Both,
    OpenBilling,
}
enum Update {
    Usage {
        period: usize,
        page: usize,
        session_page: usize,
        selected_session: Option<String>,
        result: Box<UsageSnapshot>,
    },
    Quota(std::result::Result<Value, String>),
    Catalog(std::result::Result<Value, String>),
}

fn launch_usage(
    client: &MonitorClient,
    app: &App,
    tx: &mpsc::Sender<Update>,
    tasks: &mut JoinSet<()>,
) {
    let client = client.clone();
    let tx = tx.clone();
    let period = app.period;
    let page = app.page;
    let session_page = app.session_page;
    let selected_session = app.selected_session();
    tasks.spawn(async move {
        let result = client
            .fetch_usage(
                PERIODS[period],
                page,
                session_page,
                PAGE_SIZE,
                selected_session.as_deref(),
            )
            .await;
        let _ = tx
            .send(Update::Usage {
                period,
                page,
                session_page,
                selected_session,
                result: Box::new(result),
            })
            .await;
    });
}
fn launch_quota(client: &MonitorClient, tx: &mpsc::Sender<Update>, tasks: &mut JoinSet<()>) {
    let client = client.clone();
    let tx = tx.clone();
    tasks.spawn(async move {
        let result = client.fetch_quota().await;
        let _ = tx.send(Update::Quota(result)).await;
    });
}

fn launch_catalog(client: &MonitorClient, tx: &mpsc::Sender<Update>, tasks: &mut JoinSet<()>) {
    let client = client.clone();
    let tx = tx.clone();
    tasks.spawn(async move {
        let result = client.fetch_models().await;
        let _ = tx.send(Update::Catalog(result)).await;
    });
}

/// Run a read-only dashboard while the caller owns the server lifecycle.
pub async fn run(
    endpoint: &str,
    api_key: Option<String>,
    server: &mut JoinHandle<io::Result<()>>,
    logs: MemoryLog,
) -> Result<()> {
    let client = MonitorClient::new(endpoint, api_key)?;
    let mut screen = Screen::enter()?;
    let mut app = App {
        page: 1,
        session_page: 1,
        ..App::default()
    };
    let mut input = EventStream::new();
    let (tx, mut rx) = mpsc::channel::<Update>(4);
    let mut tasks = JoinSet::new();
    let mut usage_pending = false;
    let mut quota_pending = false;
    let mut catalog_pending = false;
    let mut usage_requeue = false;
    let mut quota_requeue = false;
    let mut catalog_requeue = false;
    let mut usage_interval = tokio::time::interval(Duration::from_secs(5));
    let mut quota_interval = tokio::time::interval(Duration::from_secs(60));
    let mut catalog_interval = tokio::time::interval(Duration::from_secs(60));
    usage_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    quota_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    catalog_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let termination = async move {
        terminate.recv().await;
    };
    #[cfg(not(unix))]
    let termination = std::future::pending::<()>();
    tokio::pin!(termination);
    loop {
        let age = app
            .usage_updated
            .map(|t| format!("{}s ago", t.elapsed().as_secs()))
            .unwrap_or_else(|| "never".into());
        let quota_age = app
            .quota_updated
            .map(|t| format!("{}s ago", t.elapsed().as_secs()))
            .unwrap_or_else(|| "not loaded".into());
        let status = app
            .notice
            .clone()
            .unwrap_or_else(|| format!("server running · quota {quota_age}"));
        let catalog_status = match (&app.catalog_error, app.catalog_updated) {
            (Some(error), _) => format!(
                "{error} · price reference {}",
                pricing_reference::reference_status()
            ),
            (None, Some(updated)) => format!(
                "updated {}s ago · price reference {}",
                updated.elapsed().as_secs(),
                pricing_reference::reference_status()
            ),
            (None, None) => format!(
                "loading · price reference {}",
                pricing_reference::reference_status()
            ),
        };
        let catalog_sort_label = app.catalog_sort_label();
        let filtered_catalog = app.filtered_catalog();
        let catalog_rows = app.catalog.as_ref().map(|_| filtered_catalog.as_slice());
        let catalog_filter_prompt = if app.catalog_filter_active {
            format!("Filter /{}▏ · Enter keep · Esc cancel", app.catalog_filter)
        } else if app.catalog_filter.is_empty() {
            "Filter: / to search model name, ID, or vendor".into()
        } else {
            format!("Filter /{} · / edit · Esc clear", app.catalog_filter)
        };
        let error = app
            .usage_error
            .clone()
            .or_else(|| app.quota_error.clone())
            .or_else(|| app.catalog_error.clone())
            .or_else(|| logs.latest());
        screen.terminal.draw(|frame| {
            ui::render(
                frame,
                &ui::View {
                    endpoint,
                    period: app.period(),
                    tab: app.tab,
                    summary: app.summary.as_ref(),
                    daily: app.daily.as_ref(),
                    events: app.events.as_ref(),
                    sessions: app.sessions.as_ref(),
                    rates: app.rates.as_ref(),
                    session_rates: app.session_rates.as_ref(),
                    quota: app.quota.as_ref(),
                    status: &status,
                    page: app.page,
                    model_offset: app.model_offset,
                    catalog: catalog_rows,
                    show_model_detail: app.show_model_detail,
                    model_detail_scroll: app.model_detail_scroll,
                    catalog_selected: app.catalog_selected,
                    catalog_sort_label: &catalog_sort_label,
                    catalog_sort_key: app.catalog_sort_key,
                    catalog_detail_scroll: app.catalog_detail_scroll,
                    catalog_status: &catalog_status,
                    catalog_filter: &app.catalog_filter,
                    catalog_filter_prompt: &catalog_filter_prompt,
                    event_offset: app.event_offset,
                    session_page: app.session_page,
                    session_offset: app.session_offset,
                    show_event_detail: app.show_event_detail,
                    detail_scroll: app.detail_scroll,
                    confirm_billing: app.confirm_billing,
                    exact_counts: app.exact_counts,
                    paused: app.paused,
                    last_updated: &age,
                    error: error.as_deref(),
                },
            )
        })?;
        tokio::select! {
            event = input.next() => {
                let action = match event { Some(Ok(Event::Key(key))) => { let size = screen.terminal.size()?; app.key_in_terminal(key, size.width, size.height) }, Some(Ok(Event::Resize(width, height))) => { if width < 42 || height < 10 { app.confirm_billing = false; } Action::None }, Some(Ok(_)) => Action::None, Some(Err(error)) => return Err(error).context("reading terminal input"), None => break };
                if action == Action::Quit { break; }
                if action == Action::OpenBilling {
                    match std::process::Command::new("xdg-open").arg("https://github.com/settings/copilot/features")
                        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
                        Ok(mut child) => {
                            std::thread::spawn(move || { let _ = child.wait(); });
                            app.notice = Some("Requested browser: github.com/settings/copilot/features (change payer there)".into());
                        }
                        Err(_) => app.notice = Some("Browser unavailable; visit github.com/settings/copilot/features".into()),
                    }
                }
                if matches!(action, Action::Usage | Action::Both) {
                    if usage_pending { usage_requeue = true; } else { launch_usage(&client, &app, &tx, &mut tasks); usage_pending = true; }
                }
                if action == Action::Both {
                    if quota_pending { quota_requeue = true; } else { launch_quota(&client, &tx, &mut tasks); quota_pending = true; }
                    if catalog_pending { catalog_requeue = true; } else { launch_catalog(&client, &tx, &mut tasks); catalog_pending = true; }
                }
            }
            _ = usage_interval.tick() => if !app.paused && !usage_pending { launch_usage(&client, &app, &tx, &mut tasks); usage_pending = true; },
            _ = quota_interval.tick() => if !app.paused && !quota_pending { launch_quota(&client, &tx, &mut tasks); quota_pending = true; },
            _ = catalog_interval.tick() => if !app.paused && !catalog_pending { launch_catalog(&client, &tx, &mut tasks); catalog_pending = true; },
            Some(update) = rx.recv() => match update {
                Update::Usage { period, page, session_page, selected_session, result } => {
                    usage_pending = false;
                    if period == app.period && page == app.page && session_page == app.session_page {
                        app.apply_usage(*result);
                        if app.selected_session().is_some() && app.selected_session() != selected_session { usage_requeue = true; }
                    }
                    if usage_requeue { usage_requeue = false; launch_usage(&client, &app, &tx, &mut tasks); usage_pending = true; }
                },
                Update::Quota(result) => {
                    quota_pending = false; app.apply_quota(result);
                    if quota_requeue { quota_requeue = false; launch_quota(&client, &tx, &mut tasks); quota_pending = true; }
                },
                Update::Catalog(result) => {
                    catalog_pending = false; app.apply_catalog(result);
                    if catalog_requeue { catalog_requeue = false; launch_catalog(&client, &tx, &mut tasks); catalog_pending = true; }
                },
            },
            Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                if joined.is_err() { app.usage_error = Some("Monitoring task failed".into()); usage_pending = false; quota_pending = false; catalog_pending = false; }
            },
            result = &mut *server => return match result { Ok(Ok(())) => Err(anyhow::anyhow!("server stopped unexpectedly")), Ok(Err(error)) => Err(error).context("server failed"), Err(error) => Err(error).context("server task failed") },
            _ = &mut termination => break,
        }
    }
    tasks.abort_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_navigate_without_leaking_old_period_data() {
        let mut app = App {
            page: 1,
            summary: Some(serde_json::json!({"totals":{}})),
            ..App::default()
        };
        assert!(app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)) == Action::Usage);
        assert_eq!(app.period(), "this_week");
        assert!(app.summary.is_none());
        app.key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        app.events = Some(serde_json::json!({"total_pages":2}));
        assert!(app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)) == Action::Usage);
        assert_eq!(app.page, 2);
        assert!(app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)) == Action::Quit);
        assert!(app.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)) == Action::Quit);
    }

    #[test]
    fn pause_refresh_scroll_and_stale_status() {
        let mut app = App {
            page: 1,
            tab: 2,
            summary: Some(serde_json::json!({"byModel":[{"model":"a"},{"model":"b"}]})),
            ..App::default()
        };
        assert!(app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)) == Action::None);
        assert_eq!(app.model_offset, 1);
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.model_offset, 1);
        app.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.model_offset, 0);
        assert!(app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)) == Action::None);
        assert!(app.paused);
        assert!(app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)) == Action::Both);
        assert!(app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)) == Action::Both);
        assert!(!app.paused);
        assert!(app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)) == Action::None);
        assert!(app.exact_counts);
        assert!(app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)) == Action::None);
        assert!(!app.exact_counts);
        app.apply_quota(Ok(serde_json::json!({"windows":[]})));
        assert!(app.quota_updated.is_some());
        app.apply_quota(Err("network error".into()));
        assert!(app.quota_error.as_deref().unwrap().contains("stale"));
        assert!(app.quota.is_some());
        app.apply_usage(UsageSnapshot {
            summary: Err("unavailable".into()),
            daily: Ok(serde_json::json!({})),
            events: Ok(serde_json::json!({})),
            sessions: Ok(serde_json::json!({})),
            rates: Ok(serde_json::json!({})),
            session_rates: None,
        });
        assert!(app.usage_error.as_deref().unwrap().contains("STALE"));
        assert!(app.summary.is_some());
    }

    #[test]
    fn billing_settings_require_visible_explicit_confirmation() {
        let mut app = App {
            page: 1,
            tab: 1,
            ..App::default()
        };
        assert!(
            app.key_in_terminal(
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
                41,
                10
            ) == Action::None
        );
        assert!(!app.confirm_billing);
        app.key_in_terminal(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            42,
            10,
        );
        assert!(app.confirm_billing);
        assert!(
            app.key_in_terminal(
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                41,
                10
            ) == Action::None
        );
        assert!(!app.confirm_billing);
        app.key_in_terminal(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            80,
            24,
        );
        assert!(
            app.key_in_terminal(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                80,
                24
            ) == Action::None
        );
        assert!(!app.confirm_billing);
        app.key_in_terminal(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            80,
            24,
        );
        assert!(
            app.key_in_terminal(
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                80,
                24
            ) == Action::OpenBilling
        );
        assert!(!app.confirm_billing);
    }

    #[test]
    fn memory_log_is_bounded_and_removes_terminal_control() {
        let logs = MemoryLog::default();
        for i in 0..30 {
            let mut writer = logs.make_writer();
            write!(writer, "bad\u{1b}[2J-{i}").unwrap();
        }
        let entries = logs.0.lock().unwrap();
        assert_eq!(entries.len(), 16);
        assert!(!entries.back().unwrap().contains('\u{1b}'));
        assert!(entries.back().unwrap().ends_with("-29"));
    }

    #[test]
    fn navigation_covers_tabs_and_request_page_scroll() {
        let mut app = App {
            page: 2,
            tab: 3,
            events: Some(serde_json::json!({"total_pages":3,"items":[{},{}]})),
            ..App::default()
        };
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.event_offset, 1);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.event_offset, 0);
        assert!(app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)) == Action::Usage);
        assert_eq!(app.page, 1);
        app.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.tab, 1);
        app.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(app.tab, 1);
        app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.tab, 1);
        assert!(app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)) == Action::Quit);
        app.apply_usage(UsageSnapshot {
            summary: Ok(serde_json::json!({"byModel":[]})),
            daily: Ok(serde_json::json!({})),
            events: Ok(serde_json::json!({"items":[]})),
            sessions: Ok(serde_json::json!({})),
            rates: Ok(serde_json::json!({})),
            session_rates: None,
        });
        assert!(app.usage_updated.is_some());
        assert!(app.usage_error.is_none());
    }

    #[test]
    fn request_details_keep_selection_across_reordered_refreshes() {
        let mut app = App {
            page: 1,
            tab: 3,
            events: Some(
                serde_json::json!({"items":[{"id":11,"model":"a"},{"id":12,"model":"b"}]}),
            ),
            ..App::default()
        };
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(app.show_event_detail);
        assert_eq!(app.selected_event_id, Some(12));
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.detail_scroll, 1);
        let refreshed = UsageSnapshot {
            summary: Ok(serde_json::json!({})),
            daily: Ok(serde_json::json!({})),
            events: Ok(serde_json::json!({"items":[{"id":99},{"id":11},{"id":12}]})),
            sessions: Ok(serde_json::json!({})),
            rates: Ok(serde_json::json!({})),
            session_rates: None,
        };
        app.apply_usage(refreshed);
        assert_eq!(app.event_offset, 2);
        assert!(app.show_event_detail);
        app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(!app.show_event_detail);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn session_navigation_is_independent_and_preserves_selection() {
        let mut app = App {
            page: 2,
            session_page: 1,
            tab: 4,
            sessions: Some(
                serde_json::json!({"total_pages":2,"items":[{"session_id":"a"},{"session_id":"b"}]}),
            ),
            ..App::default()
        };
        assert!(app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)) == Action::Usage);
        assert_eq!(app.selected_session().as_deref(), Some("b"));
        app.apply_usage(UsageSnapshot {
            summary: Ok(serde_json::json!({})), daily: Ok(serde_json::json!({})),
            events: Ok(serde_json::json!({})),
            sessions: Ok(serde_json::json!({"total_pages":2,"items":[{"session_id":"b"},{"session_id":"a"}]})),
            rates: Ok(serde_json::json!({"windows":[]})), session_rates: Some(Ok(serde_json::json!({"session_id":"a","windows":[]}))),
        });
        assert_eq!(app.selected_session().as_deref(), Some("b"));
        assert_eq!(app.session_offset, 0);
        assert!(app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)) == Action::Usage);
        assert_eq!(app.session_page, 2);
        assert_eq!(app.page, 2);
        assert!(app.session_rates.is_none());
        app.key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.tab, 5);
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.tab, 0);
    }

    #[test]
    fn catalog_sort_controls_preserve_selection_and_hide_unknown_prices() {
        use catalog::{PriceTier, VerifiedQuote};
        let model = |id: &str, input: Option<f64>, output: Option<f64>| CatalogModel {
            id: id.into(),
            name: id.into(),
            default_prices: Some(PriceTier {
                verified: Some(VerifiedQuote {
                    input,
                    output,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut app = App {
            tab: 5,
            catalog: Some(vec![
                model("low", Some(1.0), Some(2.0)),
                model("high", Some(4.0), Some(10.0)),
                model("unknown", None, None),
            ]),
            ..Default::default()
        };
        assert_eq!(
            app.catalog_sort_label(),
            "highest published output desc · unknown last"
        );
        app.sort_catalog();
        assert_eq!(app.catalog.as_ref().unwrap()[0].id, "high");
        // The pre-sort selection remains the same ID.
        assert_eq!(
            app.catalog.as_ref().unwrap()[app.catalog_selected].id,
            "low"
        );
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.catalog_sort_key, SortKey::Input);
        assert_eq!(app.catalog_selected, 0);
        app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert_eq!(app.catalog_sort_order, SortOrder::Ascending);
        let ids: Vec<_> = app
            .catalog
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, ["low", "high", "unknown"]);
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.catalog_selected, 2);
        app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.catalog_selected, 0);
        app.apply_catalog(Err("offline".into()));
        assert_eq!(app.catalog.as_ref().unwrap().len(), 3);
        assert!(app.catalog_error.as_deref().unwrap().contains("offline"));
        assert!(app.key(KeyEvent::new(KeyCode::Char('6'), KeyModifiers::NONE)) == Action::None);
        assert_eq!(app.tab, 5);
    }

    #[test]
    fn catalog_search_filters_name_id_and_vendor_with_edit_cancel() {
        let model = |id: &str, name: &str, vendor: &str| CatalogModel {
            id: id.into(),
            name: name.into(),
            vendor: Some(vendor.into()),
            ..Default::default()
        };
        let mut app = App {
            tab: 5,
            catalog: Some(vec![
                model("gpt-thing", "GPT-Model", "OpenAI"),
                model("claude-thing", "Claude", "Anthropic"),
                model("vendor-model", "Other", "Special Vendor"),
            ]),
            ..Default::default()
        };
        app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.catalog_filter_active);
        for key in ['c', 'L', 'A', 'U', 'D', 'E'] {
            app.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        let filtered = app.filtered_catalog();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "claude-thing");
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.catalog_filter_active);
        assert_eq!(app.catalog_filter, "cLAUDE");
        app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.catalog_filter.is_empty());
        assert_eq!(app.filtered_catalog().len(), 3);
        app.catalog_filter = "special vendor".into();
        assert_eq!(app.filtered_catalog()[0].id, "vendor-model");
    }

    #[test]
    fn model_pricing_detail_returns_without_quitting() {
        let mut app = App {
            tab: 2,
            summary: Some(
                serde_json::json!({"byModel":[{"model":"example","total_nano_aiu":123}]}),
            ),
            ..Default::default()
        };
        assert!(app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)) == Action::None);
        assert!(app.show_model_detail);
        assert_eq!(app.selected_model_id.as_deref(), Some("example"));
        app.key_in_terminal(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 80, 24);
        assert!(app.model_detail_scroll > 0);
        assert!(app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)) == Action::None);
        assert!(!app.show_model_detail);
    }

    #[tokio::test]
    async fn refresh_tasks_use_expected_period_page_and_send_updates() {
        use axum::{Router, routing::get};
        let api = Router::new().fallback(get(|| async { axum::Json(serde_json::json!({})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
        let client = MonitorClient::new(&format!("http://{addr}"), None).unwrap();
        let app = App {
            period: 2,
            page: 2,
            ..App::default()
        };
        let (tx, mut rx) = mpsc::channel(4);
        let mut tasks = JoinSet::new();
        launch_usage(&client, &app, &tx, &mut tasks);
        launch_quota(&client, &tx, &mut tasks);
        launch_catalog(&client, &tx, &mut tasks);
        let mut saw_usage = false;
        let mut saw_quota = false;
        let mut saw_catalog = false;
        for _ in 0..3 {
            match tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                Update::Usage {
                    period,
                    page,
                    result,
                    ..
                } => {
                    assert_eq!((period, page), (2, 2));
                    assert!(result.summary.is_ok());
                    saw_usage = true;
                }
                Update::Quota(result) => {
                    assert!(result.is_ok());
                    saw_quota = true;
                }
                Update::Catalog(result) => {
                    assert!(result.is_ok());
                    saw_catalog = true;
                }
            }
        }
        assert!(saw_usage && saw_quota && saw_catalog);
        while tasks.join_next().await.is_some() {}
        server.abort();
    }
}
