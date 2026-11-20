//! Durable request usage history for the Rust gateway.
//!
//! This store has its own schema and must not silently migrate an upstream
//! TypeScript database. It records only usage actually reported by a provider.
use std::{path::Path, sync::Mutex, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, Days, Local, NaiveDate, SecondsFormat, TimeZone, Utc};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const APPLICATION_ID: i64 = 0x43505255; // "CPRU"
const SCHEMA_VERSION: i64 = 1;
// Fail explicitly rather than silently truncating an abnormally large daily report.
const MAX_DAILY_BUCKETS: usize = 3660;

/// One upstream-reported usage observation. All-zero tokens without reported AIU are ignored.
/// Times are milliseconds; token counts are nonnegative integers.
#[derive(Clone, Debug, Default)]
pub struct UsageEvent {
    pub model: String,
    pub endpoint: String,
    pub session_id: String,
    pub trace_id: String,
    pub request_id: Option<String>,
    pub user_agent: String,
    /// Zero means use the current time.
    pub created_at_ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub total_tokens: i64,
    pub total_nano_aiu: Option<i64>,
    pub duration_ms: Option<i64>,
    pub first_token_ms: Option<i64>,
    pub request_outcome: Option<String>,
}

pub struct UsageStore {
    db: Mutex<Connection>,
}

fn iso(ms: i64) -> Result<String> {
    let dt = Utc
        .timestamp_millis_opt(ms)
        .single()
        .ok_or_else(|| anyhow!("invalid timestamp"))?;
    Ok(dt.to_rfc3339_opts(SecondsFormat::Millis, true))
}

impl UsageStore {
    /// Open a Rust-owned DB. A nonempty foreign DB or unknown schema is rejected.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).context("creating usage database directory")?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let created = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path);
            if let Err(e) = created
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(e).context("creating usage database");
            }
        }
        let db = Connection::open(path).context("opening usage database")?;
        Self::initialize(db)
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(mut db: Connection) -> Result<Self> {
        db.busy_timeout(Duration::from_secs(5))?;
        let app_id: i64 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let existing: i64 = db.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if existing == 0 && app_id == 0 && version == 0 {
            let tx = db.transaction()?;
            tx.execute_batch("CREATE TABLE rust_usage_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT, created_at_ms INTEGER NOT NULL,
                trace_id TEXT NOT NULL, session_id TEXT NOT NULL, request_id TEXT,
                user_agent TEXT NOT NULL, endpoint TEXT NOT NULL, model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                cache_read_input_tokens INTEGER NOT NULL, cache_creation_input_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL, total_nano_aiu INTEGER,
                duration_ms INTEGER, first_token_ms INTEGER, request_outcome TEXT
            );
            CREATE INDEX rust_usage_time ON rust_usage_events(created_at_ms, id);
            CREATE INDEX rust_usage_request ON rust_usage_events(request_id);
            PRAGMA application_id=1129337429; PRAGMA user_version=1;")?;
            tx.commit()?;
        } else if app_id != APPLICATION_ID || version != SCHEMA_VERSION {
            bail!("usage database is not a supported Rust usage database; refusing to modify it");
        }
        // Validate expected columns before accepting a purportedly compatible DB.
        db.prepare("SELECT id, created_at_ms, trace_id, session_id, request_id, user_agent, endpoint, model, input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, total_tokens, total_nano_aiu, duration_ms, first_token_ms, request_outcome FROM rust_usage_events LIMIT 0")?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| anyhow!("usage database lock poisoned"))
    }

    /// Persist one observation; a DB error is returned rather than silently dropping usage.
    pub fn record(&self, mut event: UsageEvent) -> Result<()> {
        let numbers = [
            event.input_tokens,
            event.output_tokens,
            event.cache_read_input_tokens,
            event.cache_creation_input_tokens,
            event.total_tokens,
        ];
        if numbers.iter().any(|&n| n < 0)
            || event.total_nano_aiu.is_some_and(|n| n < 0)
            || event.duration_ms.is_some_and(|n| n < 0)
            || event.first_token_ms.is_some_and(|n| n < 0)
        {
            bail!("usage values must be nonnegative");
        }
        if numbers.iter().all(|&n| n == 0) && event.total_nano_aiu.is_none() {
            return Ok(());
        }
        if event.created_at_ms == 0 {
            event.created_at_ms = Utc::now().timestamp_millis();
        }
        iso(event.created_at_ms)?;
        if event.model.trim().is_empty() {
            event.model = "unknown".into();
        }
        event.user_agent = event.user_agent.chars().take(64).collect();
        let db = self.lock()?;
        db.execute("INSERT INTO rust_usage_events (created_at_ms,trace_id,session_id,request_id,user_agent,endpoint,model,input_tokens,output_tokens,cache_read_input_tokens,cache_creation_input_tokens,total_tokens,total_nano_aiu,duration_ms,first_token_ms,request_outcome) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![event.created_at_ms,event.trace_id,event.session_id,event.request_id,event.user_agent,event.endpoint,event.model,event.input_tokens,event.output_tokens,event.cache_read_input_tokens,event.cache_creation_input_tokens,event.total_tokens,event.total_nano_aiu,event.duration_ms,event.first_token_ms,event.request_outcome])?;
        Ok(())
    }

    pub fn summary(&self, period: &str) -> Result<Value> {
        let db = self.lock()?;
        let (period, start, end) = range(&db, period)?;
        Ok(
            json!({"period":period,"range":range_json(start,end)?,"totals":totals(&db,start,end)?,"byModel":models(&db,start,end)?}),
        )
    }

    pub fn daily(&self, period: &str) -> Result<Value> {
        let db = self.lock()?;
        let (period, start, end) = range(&db, period)?;
        let mut days = Vec::new();
        if start < end {
            let mut date = Local
                .timestamp_millis_opt(start)
                .single()
                .ok_or_else(|| anyhow!("invalid range"))?
                .date_naive();
            loop {
                let day_start = midnight(date)?;
                if day_start >= end {
                    break;
                }
                if days.len() >= MAX_DAILY_BUCKETS {
                    bail!("daily range exceeds 3660 days; request a shorter period");
                }
                let next_date = date
                    .checked_add_days(Days::new(1))
                    .ok_or_else(|| anyhow!("date overflow"))?;
                let day_end = midnight(next_date)?;
                let first = day_start.max(start);
                let last = day_end.min(end);
                if first < last {
                    days.push(json!({"date":date.format("%Y-%m-%d").to_string(),"start_ms":first,"end_ms":last,"totals":totals(&db,first,last)?,"byModel":models(&db,first,last)?}));
                }
                date = next_date;
            }
        }
        Ok(
            json!({"period":period,"range":range_json(start,end)?,"totals":totals(&db,start,end)?,"byModel":models(&db,start,end)?,"days":days}),
        )
    }

    pub fn events(&self, period: &str, page: usize, page_size: usize) -> Result<Value> {
        let db = self.lock()?;
        let (period, start, end) = range(&db, period)?;
        let (page, size, offset) = page_args(page, page_size);
        let total: i64 = db.query_row(
            "SELECT count(*) FROM rust_usage_events WHERE created_at_ms>=? AND created_at_ms<?",
            params![start, end],
            |r| r.get(0),
        )?;
        let mut stmt = db.prepare("SELECT id,created_at_ms,trace_id,session_id,request_id,user_agent,endpoint,model,input_tokens,output_tokens,cache_read_input_tokens,cache_creation_input_tokens,total_tokens,total_nano_aiu,duration_ms,first_token_ms,request_outcome,(SELECT count(*) FROM rust_usage_events AS siblings WHERE siblings.request_id=rust_usage_events.request_id) FROM rust_usage_events WHERE created_at_ms>=? AND created_at_ms<? ORDER BY created_at_ms DESC,id DESC LIMIT ? OFFSET ?")?;
        let rows = stmt.query_map(params![start,end,size,offset], |r| {
            let created_at_ms: i64=r.get(1)?;
            let output: i64=r.get(9)?;
            let duration: Option<i64>=r.get(14)?;
            let first: Option<i64>=r.get(15)?;
            let outcome: Option<String>=r.get(16)?;
            let siblings: i64=r.get(17)?;
            let aiu: Option<i64> = r.get(13)?;
            let cost = aiu.map(|n| {
                let nanos = n / 100 + i64::from(n % 100 >= 50);
                json!({"currency":"USD", "source":"copilot_aiu", "total_cost_nanos":nanos, "amount":nanos as f64 / 1_000_000_000.0})
            });
            let speed=if outcome.as_deref()==Some("completed") && siblings==1 && output>0 {
                duration.zip(first).and_then(|(d,f)| if d>f {Some(output as f64*1000.0/(d-f) as f64)} else {None})
            } else {None};
            Ok(json!({"id":r.get::<_,i64>(0)?,"created_at_ms":created_at_ms,"created_at_utc":iso(created_at_ms).ok(),"trace_id":r.get::<_,String>(2)?,"session_id":r.get::<_,String>(3)?,"request_id":r.get::<_,Option<String>>(4)?,"user_agent":r.get::<_,String>(5)?,"endpoint":r.get::<_,String>(6)?,"model":r.get::<_,String>(7)?,"input_tokens":r.get::<_,i64>(8)?,"output_tokens":output,"cache_read_input_tokens":r.get::<_,i64>(10)?,"cache_creation_input_tokens":r.get::<_,i64>(11)?,"total_tokens":r.get::<_,i64>(12)?,"total_nano_aiu":aiu,"duration_ms":duration,"first_token_ms":first,"request_outcome":outcome,"output_tokens_per_second":speed,"api_key_id":null,"api_key_name":null,"source":"copilot","provider_name":null,"user_id":"","cost":cost}))
        })?;
        let items: Vec<Value> = rows.collect::<rusqlite::Result<_>>()?;
        page_json(items, page, size, total, period, start, end)
    }

    pub fn sessions(&self, period: &str, page: usize, page_size: usize) -> Result<Value> {
        let db = self.lock()?;
        let (period, start, end) = range(&db, period)?;
        let (page, size, offset) = page_args(page, page_size);
        let total: i64 = db.query_row("SELECT count(DISTINCT session_id) FROM rust_usage_events WHERE created_at_ms>=? AND created_at_ms<?",params![start,end],|r|r.get(0))?;
        let sql = format!(
            "SELECT session_id,{} FROM rust_usage_events WHERE created_at_ms>=? AND created_at_ms<? GROUP BY session_id ORDER BY sum(total_tokens) DESC,session_id ASC LIMIT ? OFFSET ?",
            totals_sql()
        );
        let mut stmt = db.prepare(&sql)?;
        let items = stmt
            .query_map(params![start, end, size, offset], |r| {
                let session_id: String = r.get(0)?;
                let mut item = totals_from_row(r, 1)?;
                item["session_id"] = json!(session_id);
                Ok(item)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        page_json(items, page, size, total, period, start, end)
    }
}

fn parse_period(period: &str) -> &'static str {
    match period {
        "today" | "day" => "today",
        "this_week" => "this_week",
        "last_7_days" | "week" => "last_7_days",
        "this_month" => "this_month",
        "last_30_days" | "month" => "last_30_days",
        "lifetime" => "lifetime",
        _ => "today",
    }
}

fn midnight(date: NaiveDate) -> Result<i64> {
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("invalid local date"))?;
    // Some zones historically skip midnight. Find the first valid instant of that date.
    for minute in 0..=180 {
        let candidate = naive + chrono::Duration::minutes(minute);
        if let Some(dt) = Local.from_local_datetime(&candidate).earliest() {
            return Ok(dt.timestamp_millis());
        }
    }
    bail!("cannot resolve local date")
}

fn range(db: &Connection, value: &str) -> Result<(&'static str, i64, i64)> {
    let period = parse_period(value);
    let now = Local::now();
    let end = now
        .timestamp_millis()
        .checked_add(1)
        .ok_or_else(|| anyhow!("timestamp overflow"))?;
    if period == "lifetime" {
        let start: Option<i64> = db.query_row(
            "SELECT min(created_at_ms) FROM rust_usage_events WHERE created_at_ms<?",
            [end],
            |r| r.get(0),
        )?;
        return Ok((period, start.unwrap_or(end), end));
    }
    let date = now.date_naive();
    let start_date = match period {
        "this_week" => {
            date.checked_sub_days(Days::new(date.weekday().num_days_from_monday() as u64))
        }
        "last_7_days" => date.checked_sub_days(Days::new(6)),
        "this_month" => date.with_day(1),
        "last_30_days" => date.checked_sub_days(Days::new(29)),
        _ => Some(date),
    }
    .ok_or_else(|| anyhow!("invalid date"))?;
    Ok((period, midnight(start_date)?, end))
}

fn range_json(start: i64, end: i64) -> Result<Value> {
    Ok(json!({"start_ms":start,"end_ms":end,"start_utc":iso(start)?,"end_utc":iso(end)?}))
}

fn totals_sql() -> &'static str {
    "COALESCE(sum(input_tokens),0),COALESCE(sum(output_tokens),0),COALESCE(sum(cache_read_input_tokens),0),COALESCE(sum(cache_creation_input_tokens),0),COALESCE(sum(total_tokens),0),sum(total_nano_aiu),count(*),sum(total_nano_aiu / 100 + CASE WHEN total_nano_aiu % 100 >= 50 THEN 1 ELSE 0 END)"
}

fn totals_from_row(r: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Value> {
    let cost: Option<i64> = r.get(offset + 7)?;
    let costs = cost.map(|nanos| vec![json!({"currency":"USD", "total_cost_nanos":nanos, "amount":nanos as f64 / 1_000_000_000.0})]).unwrap_or_default();
    Ok(
        json!({"input_tokens":r.get::<_,i64>(offset)?,"output_tokens":r.get::<_,i64>(offset+1)?,"cache_read_input_tokens":r.get::<_,i64>(offset+2)?,"cache_creation_input_tokens":r.get::<_,i64>(offset+3)?,"total_tokens":r.get::<_,i64>(offset+4)?,"total_nano_aiu":r.get::<_,Option<i64>>(offset+5)?,"request_count":r.get::<_,i64>(offset+6)?,"costs":costs}),
    )
}

fn totals(db: &Connection, start: i64, end: i64) -> Result<Value> {
    let sql = format!(
        "SELECT {} FROM rust_usage_events WHERE created_at_ms>=? AND created_at_ms<?",
        totals_sql()
    );
    Ok(db.query_row(&sql, params![start, end], |r| totals_from_row(r, 0))?)
}

fn models(db: &Connection, start: i64, end: i64) -> Result<Vec<Value>> {
    // Restrict timing to single-row request IDs; otherwise time cannot be attributed safely.
    let sql = format!(
        "SELECT model,{}, avg(CASE WHEN request_outcome='completed' AND request_id IS NOT NULL AND first_token_ms IS NOT NULL AND (SELECT count(*) FROM rust_usage_events s WHERE s.request_id=rust_usage_events.request_id)=1 THEN first_token_ms END), sum(CASE WHEN request_outcome='completed' AND request_id IS NOT NULL AND first_token_ms IS NOT NULL AND duration_ms>first_token_ms AND output_tokens>0 AND (SELECT count(*) FROM rust_usage_events s WHERE s.request_id=rust_usage_events.request_id)=1 THEN output_tokens ELSE 0 END), sum(CASE WHEN request_outcome='completed' AND request_id IS NOT NULL AND first_token_ms IS NOT NULL AND duration_ms>first_token_ms AND output_tokens>0 AND (SELECT count(*) FROM rust_usage_events s WHERE s.request_id=rust_usage_events.request_id)=1 THEN duration_ms-first_token_ms ELSE 0 END) FROM rust_usage_events WHERE created_at_ms>=? AND created_at_ms<? GROUP BY model ORDER BY sum(total_tokens) DESC,model ASC",
        totals_sql()
    );
    let mut stmt = db.prepare(&sql)?;
    let items = stmt
        .query_map(params![start, end], |r| {
            let mut value = totals_from_row(r, 1)?;
            let output: i64 = r.get(10)?;
            let duration: i64 = r.get(11)?;
            value["model"] = json!(r.get::<_, String>(0)?);
            value["average_first_token_ms"] = json!(r.get::<_, Option<f64>>(9)?);
            value["output_tokens_per_second"] = json!(if duration > 0 {
                Some(output as f64 * 1000.0 / duration as f64)
            } else {
                None
            });
            Ok(value)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(items)
}

fn page_args(page: usize, size: usize) -> (i64, i64, i64) {
    let page = page.max(1).min(i32::MAX as usize) as i64;
    let size = size.clamp(1, 100) as i64;
    (page, size, (page - 1) * size)
}

fn page_json(
    items: Vec<Value>,
    page: i64,
    size: i64,
    total: i64,
    period: &str,
    start: i64,
    end: i64,
) -> Result<Value> {
    Ok(
        json!({"items":items,"page":page,"page_size":size,"total":total,"total_pages":((total+size-1)/size).max(1),"period":period,"range":range_json(start,end)?}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(model: &str, session: &str, n: i64) -> UsageEvent {
        UsageEvent {
            model: model.into(),
            endpoint: "chat_completions".into(),
            session_id: session.into(),
            trace_id: "trace".into(),
            request_id: Some(format!("request-{n}")),
            input_tokens: n,
            output_tokens: 2,
            total_tokens: n + 2,
            duration_ms: Some(1100),
            first_token_ms: Some(100),
            request_outcome: Some("completed".into()),
            ..Default::default()
        }
    }

    #[test]
    fn aggregates_models_sessions_and_times() -> Result<()> {
        let store = UsageStore::in_memory()?;
        store.record(event("a", "one", 3))?;
        store.record(event("b", "two", 8))?;
        let summary = store.summary("today")?;
        assert_eq!(summary["period"], "today");
        assert_eq!(summary["totals"]["input_tokens"], 11);
        assert_eq!(summary["totals"]["output_tokens"], 4);
        assert_eq!(summary["totals"]["request_count"], 2);
        assert_eq!(summary["totals"]["total_nano_aiu"], Value::Null);
        assert_eq!(summary["totals"]["costs"], json!([]));
        assert_eq!(summary["byModel"][0]["model"], "b");
        assert_eq!(summary["byModel"][0]["average_first_token_ms"], 100.0);
        assert_eq!(summary["byModel"][0]["output_tokens_per_second"], 2.0);
        let sessions = store.sessions("day", 1, 20)?;
        assert_eq!(sessions["total"], 2);
        assert_eq!(sessions["items"][0]["session_id"], "two");
        let daily = store.daily("today")?;
        assert_eq!(daily["days"].as_array().unwrap().len(), 1);
        assert_eq!(daily["days"][0]["totals"]["total_tokens"], 15);
        Ok(())
    }

    #[test]
    fn events_are_paginated_capped_sorted_and_have_nullable_timing() -> Result<()> {
        let store = UsageStore::in_memory()?;
        let mut first = event("a", "s", 1);
        first.created_at_ms = Utc::now().timestamp_millis() - 5000;
        first.user_agent = "x".repeat(80);
        first.cache_read_input_tokens = 5;
        first.total_nano_aiu = Some(200);
        store.record(first)?;
        let mut second = event("b", "s", 2);
        second.created_at_ms = Utc::now().timestamp_millis() - 1000;
        second.request_outcome = Some("cancelled".into());
        store.record(second)?;
        let p = store.events("week", 1, 1)?;
        assert_eq!(p["total"], 2);
        assert_eq!(p["total_pages"], 2);
        assert_eq!(p["items"][0]["model"], "b");
        assert_eq!(p["items"][0]["output_tokens_per_second"], Value::Null);
        let p = store.events("week", 2, 1)?;
        assert_eq!(p["items"][0]["model"], "a");
        assert_eq!(p["items"][0]["total_nano_aiu"], 200);
        assert_eq!(p["items"][0]["user_agent"].as_str().unwrap().len(), 64);
        assert_eq!(store.events("week", 0, 999)?["page_size"], 100);
        assert_eq!(store.summary("week")?["totals"]["total_nano_aiu"], 200);
        assert_eq!(
            store.summary("week")?["totals"]["costs"][0]["total_cost_nanos"],
            2
        );
        assert_eq!(p["items"][0]["cost"]["source"], "copilot_aiu");
        Ok(())
    }

    #[test]
    fn unknown_and_empty_are_not_fabricated() -> Result<()> {
        let store = UsageStore::in_memory()?;
        store.record(UsageEvent::default())?;
        assert_eq!(store.summary("unknown")?["period"], "today");
        assert_eq!(store.summary("lifetime")?["totals"]["request_count"], 0);
        assert_eq!(store.daily("lifetime")?["days"], json!([]));
        assert_eq!(store.events("lifetime", 1, 20)?["total_pages"], 1);
        let bad = UsageEvent {
            input_tokens: -1,
            ..Default::default()
        };
        assert!(store.record(bad).is_err());
        store.record(UsageEvent {
            total_nano_aiu: Some(0),
            ..Default::default()
        })?;
        assert_eq!(
            store.summary("lifetime")?["totals"]["costs"][0]["total_cost_nanos"],
            0
        );
        Ok(())
    }

    #[test]
    fn reopen_persists_and_foreign_databases_are_untouched() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("copilot-rust-usage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir)?;
        let path = dir.join("rust.db");
        {
            let store = UsageStore::open(&path)?;
            store.record(event("a", "s", 42))?;
        }
        {
            let store = UsageStore::open(&path)?;
            assert_eq!(store.summary("lifetime")?["totals"]["input_tokens"], 42);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        let foreign = dir.join("upstream.db");
        {
            let db = Connection::open(&foreign)?;
            db.execute_batch("CREATE TABLE token_usage_events(id INTEGER PRIMARY KEY); INSERT INTO token_usage_events VALUES (123)")?;
        }
        assert!(UsageStore::open(&foreign).is_err());
        let db = Connection::open(&foreign)?;
        assert_eq!(
            db.query_row("SELECT count(*) FROM token_usage_events", [], |r| r
                .get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            db.pragma_query_value(None, "application_id", |r| r.get::<_, i64>(0))?,
            0
        );
        drop(db);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn duplicate_request_timing_is_not_falsely_attributed() -> Result<()> {
        let store = UsageStore::in_memory()?;
        let mut one = event("a", "s", 3);
        let mut two = event("b", "s", 7);
        one.request_id = Some("same".into());
        two.request_id = Some("same".into());
        store.record(one)?;
        store.record(two)?;
        let summary = store.summary("today")?;
        assert!(
            summary["byModel"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["average_first_token_ms"].is_null()
                    && v["output_tokens_per_second"].is_null())
        );
        assert!(
            store.events("today", 1, 20)?["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["output_tokens_per_second"].is_null())
        );
        Ok(())
    }
}
