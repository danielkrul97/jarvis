use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA_V1: &str = r#"
CREATE TABLE samples(
  id INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL,
  wm_class TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL DEFAULT '',
  desktop INTEGER,
  idle_ms INTEGER NOT NULL DEFAULT 0,
  shot_path TEXT,
  phash INTEGER
);
CREATE INDEX idx_samples_ts ON samples(ts);

CREATE TABLE hourly_summaries(
  id INTEGER PRIMARY KEY,
  period_start INTEGER NOT NULL,
  period_end INTEGER NOT NULL,
  json TEXT NOT NULL,
  model TEXT NOT NULL DEFAULT '',
  cost_usd REAL NOT NULL DEFAULT 0,
  degraded INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_summaries_period ON hourly_summaries(period_start);

CREATE TABLE daily_digests(
  id INTEGER PRIMARY KEY,
  date TEXT NOT NULL UNIQUE,
  markdown TEXT NOT NULL,
  html TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending',
  sendgrid_msg_id TEXT,
  sent_at INTEGER
);

CREATE TABLE patterns(
  id INTEGER PRIMARY KEY,
  key TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL,
  evidence TEXT NOT NULL DEFAULT '[]',
  occurrences INTEGER NOT NULL DEFAULT 1,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'candidate'
);

CREATE TABLE proposals(
  id INTEGER PRIMARY KEY,
  pattern_id INTEGER REFERENCES patterns(id),
  kind TEXT NOT NULL,
  path TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE costs(
  id INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL,
  component TEXT NOT NULL,
  model TEXT NOT NULL DEFAULT '',
  tokens_in INTEGER NOT NULL DEFAULT 0,
  tokens_out INTEGER NOT NULL DEFAULT 0,
  usd REAL NOT NULL DEFAULT 0
);
CREATE INDEX idx_costs_ts ON costs(ts);

CREATE TABLE state(
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("nelze otevřít DB {}", path.display()))?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    // journal_mode pragma vrací řádek, proto query_row místo pragma_update
    let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(conn)?;
    Ok(())
}

pub fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(SCHEMA_V1).context("migrace v1 selhala")?;
        conn.pragma_update(None, "user_version", 1)?;
    }
    if version < 2 {
        // jedno období = jeden souhrn (reruns přes --window-hours přepisují)
        conn.execute_batch(
            "DELETE FROM hourly_summaries WHERE id NOT IN
               (SELECT MAX(id) FROM hourly_summaries GROUP BY period_start);
             CREATE UNIQUE INDEX IF NOT EXISTS idx_summaries_start
               ON hourly_summaries(period_start);",
        )
        .context("migrace v2 selhala")?;
        conn.pragma_update(None, "user_version", 2)?;
    }
    Ok(())
}

// ---------- state ----------

pub fn state_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM state WHERE key=?1", params![key], |r| r.get(0))
        .optional()
        .map_err(Into::into)
}

pub fn state_get_i64(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(state_get(conn, key)?.and_then(|v| v.parse().ok()))
}

pub fn state_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO state(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn state_del(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM state WHERE key=?1", params![key])?;
    Ok(())
}

/// Pauza snímání: vrací epochu, do kdy je pozastaveno (pokud je v budoucnu).
pub fn pause_until(conn: &Connection, now: i64) -> Result<Option<i64>> {
    Ok(state_get_i64(conn, "pause_until")?.filter(|&t| t > now))
}

// ---------- samples ----------

/// Vzorek pro čtení pipeline — nese jen sloupce, které konzumenti čtou
/// (desktop a phash se jen zapisují; id řeší SQL přímo).
#[derive(Debug, Clone)]
pub struct Sample {
    pub ts: i64,
    pub wm_class: String,
    pub title: String,
    pub idle_ms: i64,
    pub shot_path: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn insert_sample(
    conn: &Connection,
    ts: i64,
    wm_class: &str,
    title: &str,
    desktop: Option<i64>,
    idle_ms: i64,
    shot_path: Option<&str>,
    phash: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO samples(ts, wm_class, title, desktop, idle_ms, shot_path, phash)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![ts, wm_class, title, desktop, idle_ms, shot_path, phash],
    )?;
    Ok(())
}

pub fn samples_between(conn: &Connection, from: i64, to: i64) -> Result<Vec<Sample>> {
    let mut stmt = conn.prepare(
        "SELECT ts, wm_class, title, idle_ms, shot_path
         FROM samples WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
    )?;
    let rows = stmt
        .query_map(params![from, to], |r| {
            Ok(Sample {
                ts: r.get(0)?,
                wm_class: r.get(1)?,
                title: r.get(2)?,
                idle_ms: r.get(3)?,
                shot_path: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------- hourly summaries ----------

#[derive(Debug, Clone)]
pub struct SummaryRow {
    pub json: String,
    pub degraded: bool,
}

pub fn insert_hourly_summary(
    conn: &Connection,
    period_start: i64,
    period_end: i64,
    json: &str,
    model: &str,
    cost_usd: f64,
    degraded: bool,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO hourly_summaries(period_start, period_end, json, model, cost_usd, degraded)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![period_start, period_end, json, model, cost_usd, degraded as i64],
    )?;
    Ok(())
}

pub fn summaries_between(conn: &Connection, from: i64, to: i64) -> Result<Vec<SummaryRow>> {
    let mut stmt = conn.prepare(
        "SELECT json, degraded FROM hourly_summaries
         WHERE period_start >= ?1 AND period_start < ?2 ORDER BY period_start",
    )?;
    let rows: Vec<SummaryRow> = stmt
        .query_map(params![from, to], |r| {
            Ok(SummaryRow { json: r.get(0)?, degraded: r.get::<_, i64>(1)? != 0 })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// ---------- costs ----------

pub fn insert_cost(
    conn: &Connection,
    ts: i64,
    component: &str,
    model: &str,
    tokens_in: i64,
    tokens_out: i64,
    usd: f64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO costs(ts, component, model, tokens_in, tokens_out, usd)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![ts, component, model, tokens_in, tokens_out, usd],
    )?;
    Ok(())
}

pub fn cost_since(conn: &Connection, since_ts: i64) -> Result<f64> {
    conn.query_row(
        "SELECT COALESCE(SUM(usd), 0) FROM costs WHERE ts >= ?1",
        params![since_ts],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

pub fn cost_between(conn: &Connection, from: i64, to: i64) -> Result<f64> {
    conn.query_row(
        "SELECT COALESCE(SUM(usd), 0) FROM costs WHERE ts >= ?1 AND ts < ?2",
        params![from, to],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

// ---------- daily digests ----------

/// Vloží/aktualizuje obsah digestu; status existujícího řádku se nemění
/// (odeslaný zůstává odeslaný, pending zůstává pending).
pub fn upsert_digest(conn: &Connection, date: &str, markdown: &str, html: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO daily_digests(date, markdown, html, status) VALUES(?1, ?2, ?3, 'pending')
         ON CONFLICT(date) DO UPDATE SET markdown = excluded.markdown, html = excluded.html",
        params![date, markdown, html],
    )?;
    Ok(())
}

pub fn digest_row(conn: &Connection, date: &str) -> Result<Option<(String, String, String)>> {
    conn.query_row(
        "SELECT markdown, html, status FROM daily_digests WHERE date = ?1",
        params![date],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .map_err(Into::into)
}

pub fn mark_digest_sent(conn: &Connection, date: &str, msg_id: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE daily_digests SET status='sent', sendgrid_msg_id=?2, sent_at=?3 WHERE date=?1",
        params![date, msg_id, crate::util::now_ts()],
    )?;
    Ok(())
}

/// Atomický claim před odesláním — brání dvojímu odeslání při souběhu
/// digest timeru a hodinového retry (analyze). `allow_resend` (explicitní
/// `digest --send`) smí převzít i řádek se statusem `sent`.
/// Zaseknutý claim (`sending` starší hodiny — pád procesu) jde převzít vždy.
pub fn claim_digest(conn: &Connection, date: &str, allow_resend: bool) -> Result<bool> {
    let now = crate::util::now_ts();
    let stale = now - 3600;
    let n = if allow_resend {
        conn.execute(
            "UPDATE daily_digests SET status='sending', sent_at=?2
             WHERE date=?1 AND (status<>'sending' OR COALESCE(sent_at,0) < ?3)",
            params![date, now, stale],
        )?
    } else {
        conn.execute(
            "UPDATE daily_digests SET status='sending', sent_at=?2
             WHERE date=?1 AND (status='pending'
                                OR (status='sending' AND COALESCE(sent_at,0) < ?3))",
            params![date, now, stale],
        )?
    };
    Ok(n == 1)
}

/// Vrácení claimu po neúspěšném odeslání — digest zůstává k doeslání.
pub fn unclaim_digest(conn: &Connection, date: &str) -> Result<()> {
    conn.execute(
        "UPDATE daily_digests SET status='pending', sent_at=NULL
         WHERE date=?1 AND status='sending'",
        params![date],
    )?;
    Ok(())
}

/// Digesty čekající na doručení: pending + zaseknuté `sending` starší hodiny.
pub fn pending_digest_dates(conn: &Connection) -> Result<Vec<String>> {
    let stale = crate::util::now_ts() - 3600;
    let mut stmt = conn.prepare(
        "SELECT date FROM daily_digests
         WHERE status='pending' OR (status='sending' AND COALESCE(sent_at,0) < ?1)
         ORDER BY date",
    )?;
    let rows: Vec<String> = stmt
        .query_map(params![stale], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

#[cfg(test)]
pub fn test_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent() {
        let conn = test_conn();
        migrate(&conn).unwrap(); // druhé volání nesmí spadnout
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 2);
    }

    #[test]
    fn state_roundtrip() {
        let conn = test_conn();
        assert!(state_get(&conn, "x").unwrap().is_none());
        state_set(&conn, "x", "1").unwrap();
        state_set(&conn, "x", "2").unwrap();
        assert_eq!(state_get(&conn, "x").unwrap().as_deref(), Some("2"));
        state_del(&conn, "x").unwrap();
        assert!(state_get(&conn, "x").unwrap().is_none());
    }

    #[test]
    fn pause_logic() {
        let conn = test_conn();
        assert!(pause_until(&conn, 100).unwrap().is_none());
        state_set(&conn, "pause_until", "200").unwrap();
        assert_eq!(pause_until(&conn, 100).unwrap(), Some(200));
        assert!(pause_until(&conn, 300).unwrap().is_none());
    }

    #[test]
    fn samples_roundtrip() {
        let conn = test_conn();
        insert_sample(&conn, 10, "firefox", "Docs", Some(1), 500, None, None).unwrap();
        insert_sample(&conn, 20, "alacritty", "vim", None, 0, Some("shots/x.jpg"), Some(42)).unwrap();
        let rows = samples_between(&conn, 0, 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].wm_class, "firefox");
        assert_eq!(rows[1].shot_path.as_deref(), Some("shots/x.jpg"));
        // konec intervalu je exkluzivní
        assert_eq!(samples_between(&conn, 15, 20).unwrap().len(), 0);
        assert_eq!(samples_between(&conn, 15, 21).unwrap().len(), 1);
    }

    #[test]
    fn digest_upsert_preserves_sent_status() {
        let conn = test_conn();
        upsert_digest(&conn, "2026-07-17", "md1", "html1").unwrap();
        assert_eq!(pending_digest_dates(&conn).unwrap(), vec!["2026-07-17"]);
        mark_digest_sent(&conn, "2026-07-17", Some("msg-1")).unwrap();
        assert!(pending_digest_dates(&conn).unwrap().is_empty());
        // rebuild obsahu nesmí shodit status zpět na pending
        upsert_digest(&conn, "2026-07-17", "md2", "html2").unwrap();
        let (md, _, status) = digest_row(&conn, "2026-07-17").unwrap().unwrap();
        assert_eq!(md, "md2");
        assert_eq!(status, "sent");
    }

    #[test]
    fn digest_claim_prevents_double_send() {
        let conn = test_conn();
        upsert_digest(&conn, "2026-07-17", "md", "html").unwrap();
        assert!(claim_digest(&conn, "2026-07-17", false).unwrap());
        // souběžný proces claim nedostane
        assert!(!claim_digest(&conn, "2026-07-17", false).unwrap());
        // po vrácení claimu jde znovu
        unclaim_digest(&conn, "2026-07-17").unwrap();
        assert!(claim_digest(&conn, "2026-07-17", false).unwrap());
        mark_digest_sent(&conn, "2026-07-17", None).unwrap();
        // odeslaný: bez force ne, s force (explicitní resend) ano
        assert!(!claim_digest(&conn, "2026-07-17", false).unwrap());
        assert!(claim_digest(&conn, "2026-07-17", true).unwrap());
    }

    #[test]
    fn pending_dates_include_stale_sending() {
        let conn = test_conn();
        upsert_digest(&conn, "2026-07-16", "md", "html").unwrap();
        assert!(claim_digest(&conn, "2026-07-16", false).unwrap());
        // čerstvě claimnutý není ve frontě
        assert!(pending_digest_dates(&conn).unwrap().is_empty());
        // zaseknutý claim (pád procesu) se po hodině vrací do fronty
        conn.execute("UPDATE daily_digests SET sent_at = 1000 WHERE date='2026-07-16'", [])
            .unwrap();
        assert_eq!(pending_digest_dates(&conn).unwrap(), vec!["2026-07-16"]);
        assert!(claim_digest(&conn, "2026-07-16", false).unwrap());
    }

    #[test]
    fn summary_replaces_same_period() {
        let conn = test_conn();
        insert_hourly_summary(&conn, 100, 200, "a", "m", 0.0, false).unwrap();
        insert_hourly_summary(&conn, 100, 200, "b", "m", 0.1, true).unwrap();
        let rows = summaries_between(&conn, 0, 1000).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].json, "b");
        assert!(rows[0].degraded);
    }

    #[test]
    fn costs_sum() {
        let conn = test_conn();
        insert_cost(&conn, 10, "analyze", "haiku", 100, 20, 0.01).unwrap();
        insert_cost(&conn, 20, "digest", "sonnet", 200, 50, 0.05).unwrap();
        let total = cost_since(&conn, 0).unwrap();
        assert!((total - 0.06).abs() < 1e-9);
        assert!((cost_since(&conn, 15).unwrap() - 0.05).abs() < 1e-9);
    }
}
