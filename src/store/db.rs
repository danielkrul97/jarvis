use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS samples(
  id INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL,
  wm_class TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL DEFAULT '',
  desktop INTEGER,
  idle_ms INTEGER NOT NULL DEFAULT 0,
  shot_path TEXT,
  phash INTEGER
);
CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples(ts);

CREATE TABLE IF NOT EXISTS hourly_summaries(
  id INTEGER PRIMARY KEY,
  period_start INTEGER NOT NULL,
  period_end INTEGER NOT NULL,
  json TEXT NOT NULL,
  model TEXT NOT NULL DEFAULT '',
  cost_usd REAL NOT NULL DEFAULT 0,
  degraded INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_summaries_period ON hourly_summaries(period_start);

CREATE TABLE IF NOT EXISTS daily_digests(
  id INTEGER PRIMARY KEY,
  date TEXT NOT NULL UNIQUE,
  markdown TEXT NOT NULL,
  html TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending',
  sendgrid_msg_id TEXT,
  sent_at INTEGER
);

CREATE TABLE IF NOT EXISTS patterns(
  id INTEGER PRIMARY KEY,
  key TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL,
  evidence TEXT NOT NULL DEFAULT '[]',
  occurrences INTEGER NOT NULL DEFAULT 1,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'candidate'
);

CREATE TABLE IF NOT EXISTS proposals(
  id INTEGER PRIMARY KEY,
  pattern_id INTEGER REFERENCES patterns(id),
  kind TEXT NOT NULL,
  path TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS costs(
  id INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL,
  component TEXT NOT NULL,
  model TEXT NOT NULL DEFAULT '',
  tokens_in INTEGER NOT NULL DEFAULT 0,
  tokens_out INTEGER NOT NULL DEFAULT 0,
  usd REAL NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_costs_ts ON costs(ts);

CREATE TABLE IF NOT EXISTS state(
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
    // journal_mode pragma returns a row, so query_row instead of pragma_update
    let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // SQLite FKs must be enabled per-connection (and outside a transaction) —
    // otherwise REFERENCES in runbooks/runbook_runs are just declarations, not enforced
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(conn)?;
    Ok(())
}

/// Does the table have this column? (Idempotence for ALTER … ADD COLUMN migrations —
/// ADD COLUMN has no IF NOT EXISTS, so a partial rerun would otherwise fail.)
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let found = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == column);
    Ok(found)
}

pub fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(SCHEMA_V1).context("migrace v1 selhala")?;
        conn.pragma_update(None, "user_version", 1)?;
    }
    if version < 2 {
        // one period = one summary (reruns via --window-hours overwrite it)
        conn.execute_batch(
            "DELETE FROM hourly_summaries WHERE id NOT IN
               (SELECT MAX(id) FROM hourly_summaries GROUP BY period_start);
             CREATE UNIQUE INDEX IF NOT EXISTS idx_summaries_start
               ON hourly_summaries(period_start);",
        )
        .context("migrace v2 selhala")?;
        conn.pragma_update(None, "user_version", 2)?;
    }
    if version < 3 {
        // mic listening: utterance transcripts (audio is never stored)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS utterances(
               id INTEGER PRIMARY KEY,
               ts_start INTEGER NOT NULL,
               ts_end INTEGER NOT NULL,
               text TEXT NOT NULL,
               lang TEXT NOT NULL DEFAULT '',
               conf REAL NOT NULL DEFAULT 0,
               source TEXT NOT NULL DEFAULT 'mic'
             );
             CREATE INDEX IF NOT EXISTS idx_utterances_ts ON utterances(ts_start);",
        )
        .context("migrace v3 selhala")?;
        conn.pragma_update(None, "user_version", 3)?;
    }
    if version < 4 {
        // voice dialog: question from mic → Claude's answer (spoken aloud)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversations(
               id INTEGER PRIMARY KEY,
               ts INTEGER NOT NULL,
               question TEXT NOT NULL,
               answer TEXT NOT NULL,
               model TEXT NOT NULL DEFAULT '',
               cost_usd REAL NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_conversations_ts ON conversations(ts);",
        )
        .context("migrace v4 selhala")?;
        conn.pragma_update(None, "user_version", 4)?;
    }
    if version < 5 {
        // phase D: an approved proposal becomes a runbook; every run is logged
        // (read-back for digest/status; the last run drives the run-due scheduler)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runbooks(
               id INTEGER PRIMARY KEY,
               proposal_id INTEGER NOT NULL UNIQUE REFERENCES proposals(id),
               pattern_id INTEGER REFERENCES patterns(id),
               name TEXT NOT NULL,
               schedule TEXT NOT NULL DEFAULT 'manual',
               enabled INTEGER NOT NULL DEFAULT 1,
               approved_at INTEGER NOT NULL,
               approved_via TEXT NOT NULL DEFAULT 'cli'
             );
             CREATE TABLE IF NOT EXISTS runbook_runs(
               id INTEGER PRIMARY KEY,
               runbook_id INTEGER NOT NULL REFERENCES runbooks(id),
               started_at INTEGER NOT NULL,
               finished_at INTEGER,
               exit_code INTEGER,
               trigger TEXT NOT NULL DEFAULT 'cli',
               output TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_runbook_runs_started ON runbook_runs(started_at);",
        )
        .context("migrace v5 selhala")?;
        conn.pragma_update(None, "user_version", 5)?;
    }
    if version < 6 {
        // approval integrity: the artifact's hash is fixed at approve time and
        // checked before every execution — once approved, the script can't be
        // swapped unnoticed (otherwise timer/voice would run changed content
        // without re-approval). ADD COLUMN has no IF NOT EXISTS → guard via
        // column_exists (idempotence).
        if !column_exists(conn, "runbooks", "artifact_sha256")? {
            conn.execute(
                "ALTER TABLE runbooks ADD COLUMN artifact_sha256 TEXT NOT NULL DEFAULT ''",
                [],
            )
            .context("migrace v6 selhala")?;
        }
        conn.pragma_update(None, "user_version", 6)?;
    }
    if version < 7 {
        // Long-term memory — hybrid retrieval: FTS5 index over conversation and
        // utterance text. External-content tables (content stays in conversations/
        // utterances, the index holds only the inverted list) + triggers for sync,
        // plus a one-off backfill of existing rows. `remove_diacritics 2` folds
        // Czech diacritics, so a query for "pocasi" also matches "počasí".
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS conversations_fts USING fts5(
               question, answer,
               content='conversations', content_rowid='id',
               tokenize='unicode61 remove_diacritics 2'
             );
             INSERT INTO conversations_fts(rowid, question, answer)
               SELECT id, question, answer FROM conversations;
             CREATE TRIGGER IF NOT EXISTS conversations_ai AFTER INSERT ON conversations BEGIN
               INSERT INTO conversations_fts(rowid, question, answer)
                 VALUES (new.id, new.question, new.answer);
             END;
             CREATE TRIGGER IF NOT EXISTS conversations_ad AFTER DELETE ON conversations BEGIN
               INSERT INTO conversations_fts(conversations_fts, rowid, question, answer)
                 VALUES('delete', old.id, old.question, old.answer);
             END;
             CREATE TRIGGER IF NOT EXISTS conversations_au AFTER UPDATE ON conversations BEGIN
               INSERT INTO conversations_fts(conversations_fts, rowid, question, answer)
                 VALUES('delete', old.id, old.question, old.answer);
               INSERT INTO conversations_fts(rowid, question, answer)
                 VALUES (new.id, new.question, new.answer);
             END;
             CREATE VIRTUAL TABLE IF NOT EXISTS utterances_fts USING fts5(
               text,
               content='utterances', content_rowid='id',
               tokenize='unicode61 remove_diacritics 2'
             );
             INSERT INTO utterances_fts(rowid, text)
               SELECT id, text FROM utterances;
             CREATE TRIGGER IF NOT EXISTS utterances_ai AFTER INSERT ON utterances BEGIN
               INSERT INTO utterances_fts(rowid, text) VALUES (new.id, new.text);
             END;
             CREATE TRIGGER IF NOT EXISTS utterances_ad AFTER DELETE ON utterances BEGIN
               INSERT INTO utterances_fts(utterances_fts, rowid, text)
                 VALUES('delete', old.id, old.text);
             END;
             CREATE TRIGGER IF NOT EXISTS utterances_au AFTER UPDATE ON utterances BEGIN
               INSERT INTO utterances_fts(utterances_fts, rowid, text)
                 VALUES('delete', old.id, old.text);
               INSERT INTO utterances_fts(rowid, text) VALUES (new.id, new.text);
             END;",
        )
        .context("migrace v7 selhala")?;
        conn.pragma_update(None, "user_version", 7)?;
    }
    if version < 8 {
        // Semantic memory (phase 2): distilled PERMANENT facts about the boss, not a
        // raw log. `superseded_by` keeps an audit trail — a contradiction doesn't
        // delete the old fact, just shadows it with a newer one. Active fact =
        // superseded_by IS NULL. FTS5 over (subject,text) for retrieval, same as
        // conversations.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_facts(
               id INTEGER PRIMARY KEY,
               kind TEXT NOT NULL DEFAULT 'fact',
               subject TEXT NOT NULL DEFAULT '',
               text TEXT NOT NULL,
               confidence REAL NOT NULL DEFAULT 0.7,
               salience REAL NOT NULL DEFAULT 1.0,
               pinned INTEGER NOT NULL DEFAULT 0,
               source TEXT NOT NULL DEFAULT '',
               first_seen INTEGER NOT NULL,
               last_seen INTEGER NOT NULL,
               superseded_by INTEGER REFERENCES memory_facts(id)
             );
             CREATE INDEX IF NOT EXISTS idx_facts_active
               ON memory_facts(superseded_by, pinned, salience);
             CREATE VIRTUAL TABLE IF NOT EXISTS memory_facts_fts USING fts5(
               subject, text,
               content='memory_facts', content_rowid='id',
               tokenize='unicode61 remove_diacritics 2'
             );
             CREATE TRIGGER IF NOT EXISTS memory_facts_ai AFTER INSERT ON memory_facts BEGIN
               INSERT INTO memory_facts_fts(rowid, subject, text)
                 VALUES (new.id, new.subject, new.text);
             END;
             CREATE TRIGGER IF NOT EXISTS memory_facts_ad AFTER DELETE ON memory_facts BEGIN
               INSERT INTO memory_facts_fts(memory_facts_fts, rowid, subject, text)
                 VALUES('delete', old.id, old.subject, old.text);
             END;
             CREATE TRIGGER IF NOT EXISTS memory_facts_au AFTER UPDATE ON memory_facts BEGIN
               INSERT INTO memory_facts_fts(memory_facts_fts, rowid, subject, text)
                 VALUES('delete', old.id, old.subject, old.text);
               INSERT INTO memory_facts_fts(rowid, subject, text)
                 VALUES (new.id, new.subject, new.text);
             END;",
        )
        .context("migrace v8 selhala")?;
        conn.pragma_update(None, "user_version", 8)?;
    }
    if version < 9 {
        // Dense embeddings (phase 3): one vector per row (fact/conversation/utterance),
        // stored as a BLOB of little-endian f32s. KNN is brute-forced in Rust (the
        // corpus is small — sqlite-vec/ANN would be overkill here). A cache, not
        // truth: a missing/stale embedding just means "backfill via `memory embed`".
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS embeddings(
               source TEXT NOT NULL,      -- 'fact' | 'conversation' | 'utterance'
               ref_id INTEGER NOT NULL,
               model TEXT NOT NULL,
               dim INTEGER NOT NULL,
               vec BLOB NOT NULL,
               PRIMARY KEY(source, ref_id)
             ) WITHOUT ROWID;",
        )
        .context("migrace v9 selhala")?;
        conn.pragma_update(None, "user_version", 9)?;
    }
    if version < 10 {
        // Proactive layer: nudges Jarvis speaks/sends on its own initiative. A row is
        // only created once a nudge is actually delivered → the table is the source
        // of truth for both the daily cap and cooldown (no counter in `state` that
        // could drift from reality). status holds the lifecycle: offered → confirmed
        // → done|failed | dismissed | expired. action_ref = runbook/pattern.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS nudges(
               id INTEGER PRIMARY KEY,
               ts INTEGER NOT NULL,
               kind TEXT NOT NULL,
               dedup_key TEXT NOT NULL DEFAULT '',
               evidence TEXT NOT NULL DEFAULT '',
               action_kind TEXT NOT NULL DEFAULT 'inform',
               action_ref TEXT NOT NULL DEFAULT '',
               channel TEXT NOT NULL DEFAULT '',
               status TEXT NOT NULL DEFAULT 'offered',
               decided_at INTEGER,
               outcome TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_nudges_ts ON nudges(ts);
             CREATE INDEX IF NOT EXISTS idx_nudges_dedup ON nudges(kind, dedup_key);",
        )
        .context("migrace v10 selhala")?;
        conn.pragma_update(None, "user_version", 10)?;
    }
    if version < 11 {
        // Partial index for retention purge: `samples` rows are never deleted (only
        // shot_path gets NULLed), so the table grows forever. Purge looks for
        // `shot_path IS NOT NULL AND ts < cutoff`; without this index the scan
        // grows with the whole history, not just rows that still have a screenshot.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_samples_shot
               ON samples(ts) WHERE shot_path IS NOT NULL;",
        )
        .context("migrace v11 selhala")?;
        conn.pragma_update(None, "user_version", 11)?;
    }
    if version < 12 {
        // Scheduled internal tasks (`jarvis tasks`): housekeeping Jarvis runs on its
        // own — checking its own dependencies, DB maintenance, screenshot cleanup.
        // Unlike runbooks these are NOT user scripts: they're built-in Jarvis
        // functions, so they don't go through approval. Task definitions live in
        // code (registry in tasks.rs); this table is just the run history —
        // read-back for status/digest, and the last run drives the scheduler's
        // `due` check.
        // `ok`: NULL = never finished (process crash), 1 = success, 0 = failed.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS task_runs(
               id INTEGER PRIMARY KEY,
               task TEXT NOT NULL,
               started_at INTEGER NOT NULL,
               finished_at INTEGER,
               ok INTEGER,
               trigger TEXT NOT NULL DEFAULT 'timer',
               output TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_task_runs_started ON task_runs(started_at);
             CREATE INDEX IF NOT EXISTS idx_task_runs_task ON task_runs(task, started_at);",
        )
        .context("migrace v12 selhala")?;
        conn.pragma_update(None, "user_version", 12)?;
    }
    if version < 13 {
        // Self-improvement ledger (`jarvis improve`): Jarvis develops its own
        // code on isolated git branches and records every attempt here. Git is
        // the source of truth for the CODE (branch + commit); this table is the
        // state machine / index over it. status: queued → drafting → tested →
        // proposed → approved → merged → deployed (or failed | dismissed |
        // rolled_back). diff_sha256 is pinned at proposal and re-verified
        // TOCTOU-safe before merge — same integrity model as runbook artifacts.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS improvements(
               id INTEGER PRIMARY KEY,
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL,
               source TEXT NOT NULL DEFAULT 'directed',
               title TEXT NOT NULL DEFAULT '',
               spec TEXT NOT NULL DEFAULT '',
               branch TEXT NOT NULL DEFAULT '',
               base_commit TEXT NOT NULL DEFAULT '',
               head_commit TEXT NOT NULL DEFAULT '',
               status TEXT NOT NULL DEFAULT 'queued',
               envelope TEXT NOT NULL DEFAULT '',
               diff_stat TEXT NOT NULL DEFAULT '',
               diff_sha256 TEXT NOT NULL DEFAULT '',
               tests_passed INTEGER,
               test_output TEXT NOT NULL DEFAULT '',
               cost_usd REAL NOT NULL DEFAULT 0,
               tokens_in INTEGER NOT NULL DEFAULT 0,
               tokens_out INTEGER NOT NULL DEFAULT 0,
               approved_at INTEGER,
               approved_via TEXT NOT NULL DEFAULT '',
               merged_at INTEGER,
               deployed_at INTEGER,
               note TEXT NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_improvements_status
               ON improvements(status, created_at);",
        )
        .context("migrace v13 selhala")?;
        conn.pragma_update(None, "user_version", 13)?;
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

/// Capture pause: returns the epoch until which it's paused (if in the future).
pub fn pause_until(conn: &Connection, now: i64) -> Result<Option<i64>> {
    Ok(state_get_i64(conn, "pause_until")?.filter(|&t| t > now))
}

// ---------- nudges (proactive layer) ----------

/// One proactive nudge. A row is only created once Jarvis delivers it (speaks
/// it aloud / sends via Telegram), so this table also serves as the source of
/// truth for the daily cap and cooldown. Some fields (dedup_key, channel) are
/// only written/read in SQL — nothing in Rust reads them yet (see `UtteranceRow`).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct NudgeRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub dedup_key: String,
    pub evidence: String,
    /// inform | run_runbook | propose
    pub action_kind: String,
    /// runbook id/name or pattern id ("" for inform)
    pub action_ref: String,
    /// voice | telegram
    pub channel: String,
    /// offered | confirmed | done | failed | dismissed | expired
    pub status: String,
}

const NUDGE_COLS: &str =
    "id, ts, kind, dedup_key, evidence, action_kind, action_ref, channel, status";

fn nudge_from_row(r: &rusqlite::Row) -> rusqlite::Result<NudgeRow> {
    Ok(NudgeRow {
        id: r.get(0)?,
        ts: r.get(1)?,
        kind: r.get(2)?,
        dedup_key: r.get(3)?,
        evidence: r.get(4)?,
        action_kind: r.get(5)?,
        action_ref: r.get(6)?,
        channel: r.get(7)?,
        status: r.get(8)?,
    })
}

/// Records a delivered nudge; returns its id (for remote "ano N" / "ne N").
#[allow(clippy::too_many_arguments)]
pub fn insert_nudge(
    conn: &Connection,
    ts: i64,
    kind: &str,
    dedup_key: &str,
    evidence: &str,
    action_kind: &str,
    action_ref: &str,
    channel: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO nudges(ts, kind, dedup_key, evidence, action_kind, action_ref, channel, status)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'offered')",
        params![ts, kind, dedup_key, evidence, action_kind, action_ref, channel],
    )?;
    Ok(conn.last_insert_rowid())
}

/// How many nudges fired since `since` (epoch) — input for the daily cap.
pub fn nudge_count_since(conn: &Connection, since: i64) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM nudges WHERE ts >= ?1", params![since], |r| r.get(0))
        .map_err(Into::into)
}

/// When a nudge of the same kind and subject last fired (cooldown). MAX over
/// an empty set = NULL → None. None = never yet.
pub fn last_nudge_ts(conn: &Connection, kind: &str, dedup_key: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(ts) FROM nudges WHERE kind=?1 AND dedup_key=?2",
        params![kind, dedup_key],
        |r| r.get::<_, Option<i64>>(0),
    )
    .map_err(Into::into)
}

/// Nudge by id (for remote confirmation "ano N").
pub fn nudge_by_id(conn: &Connection, id: i64) -> Result<Option<NudgeRow>> {
    conn.query_row(&format!("SELECT {NUDGE_COLS} FROM nudges WHERE id=?1"), params![id], nudge_from_row)
        .optional()
        .map_err(Into::into)
}

/// Updates a nudge's status (+ decision time and text outcome).
pub fn set_nudge_status(conn: &Connection, id: i64, status: &str, outcome: &str) -> Result<()> {
    conn.execute(
        "UPDATE nudges SET status=?2, outcome=?3, decided_at=?4 WHERE id=?1",
        params![id, status, outcome, crate::util::now_ts()],
    )?;
    Ok(())
}

// ---------- improvements (self-improvement ledger) ----------

/// One self-improvement attempt. Git holds the actual code (branch/commit);
/// this row is the state machine over it. Several fields are written by later
/// lifecycle steps and only read in some phases → allow dead_code like NudgeRow.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ImprovementRow {
    pub id: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// directed | failing_test | clippy | plan_item | runbook_fix
    pub source: String,
    pub title: String,
    pub spec: String,
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    /// queued | drafting | tested | proposed | approved | merged | deployed | failed | dismissed | rolled_back
    pub status: String,
    /// safe | feature | gate_critical
    pub envelope: String,
    pub diff_stat: String,
    pub diff_sha256: String,
    /// None = not run yet, Some(true/false) = green/red
    pub tests_passed: Option<bool>,
    pub test_output: String,
    pub cost_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub approved_at: Option<i64>,
    pub approved_via: String,
    pub merged_at: Option<i64>,
    pub deployed_at: Option<i64>,
    pub note: String,
}

const IMPR_COLS: &str = "id, created_at, updated_at, source, title, spec, branch, \
    base_commit, head_commit, status, envelope, diff_stat, diff_sha256, tests_passed, \
    test_output, cost_usd, tokens_in, tokens_out, approved_at, approved_via, merged_at, \
    deployed_at, note";

fn improvement_from_row(r: &rusqlite::Row) -> rusqlite::Result<ImprovementRow> {
    Ok(ImprovementRow {
        id: r.get(0)?,
        created_at: r.get(1)?,
        updated_at: r.get(2)?,
        source: r.get(3)?,
        title: r.get(4)?,
        spec: r.get(5)?,
        branch: r.get(6)?,
        base_commit: r.get(7)?,
        head_commit: r.get(8)?,
        status: r.get(9)?,
        envelope: r.get(10)?,
        diff_stat: r.get(11)?,
        diff_sha256: r.get(12)?,
        tests_passed: r.get::<_, Option<i64>>(13)?.map(|v| v != 0),
        test_output: r.get(14)?,
        cost_usd: r.get(15)?,
        tokens_in: r.get(16)?,
        tokens_out: r.get(17)?,
        approved_at: r.get(18)?,
        approved_via: r.get(19)?,
        merged_at: r.get(20)?,
        deployed_at: r.get(21)?,
        note: r.get(22)?,
    })
}

/// Inserts a queued improvement; returns its id.
pub fn insert_improvement(
    conn: &Connection,
    now: i64,
    source: &str,
    title: &str,
    spec: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO improvements(created_at, updated_at, source, title, spec, status)
         VALUES(?1, ?1, ?2, ?3, ?4, 'queued')",
        params![now, source, title, spec],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn improvement_by_id(conn: &Connection, id: i64) -> Result<Option<ImprovementRow>> {
    conn.query_row(
        &format!("SELECT {IMPR_COLS} FROM improvements WHERE id=?1"),
        params![id],
        improvement_from_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Recent improvements, newest first.
pub fn improvements_recent(conn: &Connection, limit: usize) -> Result<Vec<ImprovementRow>> {
    let mut stmt = conn
        .prepare(&format!("SELECT {IMPR_COLS} FROM improvements ORDER BY id DESC LIMIT ?1"))?;
    let rows = stmt
        .query_map(params![limit as i64], improvement_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Updates status + note, bumping updated_at.
pub fn set_improvement_status(conn: &Connection, id: i64, status: &str, note: &str) -> Result<()> {
    conn.execute(
        "UPDATE improvements SET status=?2, note=?3, updated_at=?4 WHERE id=?1",
        params![id, status, note, crate::util::now_ts()],
    )?;
    Ok(())
}

/// The last `limit` nudges (listing in `status`).
pub fn recent_nudges(conn: &Connection, limit: usize) -> Result<Vec<NudgeRow>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {NUDGE_COLS} FROM nudges ORDER BY ts DESC LIMIT ?1"))?;
    let rows = stmt
        .query_map(params![limit as i64], nudge_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// ---------- samples ----------

/// A sample for the read pipeline — carries only the columns consumers read
/// (desktop and phash are write-only; id is handled directly in SQL).
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

// ---------- utterances (listening) ----------

/// An utterance transcript for reading. Only tests consume it so far — wiring
/// it into hourly analysis is the next step (PLAN §3.7), then `allow` goes away.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct UtteranceRow {
    pub ts_start: i64,
    pub ts_end: i64,
    pub text: String,
    pub lang: String,
    pub conf: f64,
}

pub fn insert_utterance(
    conn: &Connection,
    ts_start: i64,
    ts_end: i64,
    text: &str,
    lang: &str,
    conf: f64,
    source: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO utterances(ts_start, ts_end, text, lang, conf, source)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![ts_start, ts_end, text, lang, conf, source],
    )?;
    Ok(())
}

#[allow(dead_code)] // see UtteranceRow
pub fn utterances_between(conn: &Connection, from: i64, to: i64) -> Result<Vec<UtteranceRow>> {
    let mut stmt = conn.prepare(
        "SELECT ts_start, ts_end, text, lang, conf
         FROM utterances WHERE ts_start >= ?1 AND ts_start < ?2 ORDER BY ts_start",
    )?;
    let rows = stmt
        .query_map(params![from, to], |r| {
            Ok(UtteranceRow {
                ts_start: r.get(0)?,
                ts_end: r.get(1)?,
                text: r.get(2)?,
                lang: r.get(3)?,
                conf: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn last_utterance(conn: &Connection) -> Result<Option<(i64, String)>> {
    conn.query_row(
        "SELECT ts_start, text FROM utterances ORDER BY ts_start DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

/// Texts of the last `limit` mic utterances (newest first) — corpus template
/// for the open-ear kill-gate (`jarvis converse-eval --from-db`).
pub fn recent_utterance_texts(conn: &Connection, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT text FROM utterances WHERE source = 'mic' ORDER BY ts_start DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn utterance_count_since(conn: &Connection, since: i64) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM utterances WHERE ts_start >= ?1",
        params![since],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

// ---------- semantic memory: facts (migration v8) ----------

/// A distilled, durable claim about the boss/his world. Active = `superseded_by`
/// is None. `salience` is the base importance (rises with reuse, decays at
/// read-time based on `last_seen` age).
#[derive(Debug, Clone)]
pub struct Fact {
    pub id: i64,
    pub kind: String,
    pub subject: String,
    pub text: String,
    pub confidence: f64,
    pub salience: f64,
    pub pinned: bool,
    pub last_seen: i64,
    pub superseded_by: Option<i64>,
}

fn fact_from_row(r: &rusqlite::Row) -> rusqlite::Result<Fact> {
    Ok(Fact {
        id: r.get(0)?,
        kind: r.get(1)?,
        subject: r.get(2)?,
        text: r.get(3)?,
        confidence: r.get(4)?,
        salience: r.get(5)?,
        pinned: r.get::<_, i64>(6)? != 0,
        last_seen: r.get(7)?,
        superseded_by: r.get(8)?,
    })
}

const FACT_COLS: &str =
    "id, kind, subject, text, confidence, salience, pinned, last_seen, superseded_by";

/// Inserts a new fact (salience 1.0, first_seen == last_seen == now). Returns id.
#[allow(clippy::too_many_arguments)]
pub fn insert_fact(
    conn: &Connection,
    kind: &str,
    subject: &str,
    text: &str,
    confidence: f64,
    pinned: bool,
    source: &str,
) -> Result<i64> {
    let now = crate::util::now_ts();
    conn.execute(
        "INSERT INTO memory_facts(kind, subject, text, confidence, salience, pinned, source, first_seen, last_seen)
         VALUES(?1, ?2, ?3, ?4, 1.0, ?5, ?6, ?7, ?7)",
        params![kind, subject, text, confidence, pinned as i64, source, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Contradiction/update: shadows the old fact with a newer one (keeps the audit trail).
pub fn supersede_fact(conn: &Connection, old_id: i64, new_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE memory_facts SET superseded_by = ?2 WHERE id = ?1",
        params![old_id, new_id],
    )?;
    Ok(())
}

/// Reconfirm: a fact gets re-confirmed (via consolidation/use) → bump last_seen
/// and raise salience (capped at 2.0), so fresh, repeated facts don't decay away.
pub fn touch_fact(conn: &Connection, id: i64) -> Result<()> {
    let now = crate::util::now_ts();
    conn.execute(
        "UPDATE memory_facts SET last_seen = ?2, salience = MIN(2.0, salience + 0.5) WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

/// Permanently forgets a fact (CLI `memory forget`, decay prune). Returns whether anything was deleted.
pub fn delete_fact(conn: &Connection, id: i64) -> Result<bool> {
    // This fact may be the target of another row's superseded_by (the update
    // audit trail). FKs are on with no ON DELETE action, so a direct DELETE would
    // hit "FOREIGN KEY constraint failed" — breaking prune_faded and `memory
    // forget` for any fact that ever superseded something. We clear back-references
    // (the row this one shadowed becomes active again) and delete in one
    // transaction, so it's atomic.
    let tx = conn.unchecked_transaction()?;
    tx.execute("UPDATE memory_facts SET superseded_by = NULL WHERE superseded_by = ?1", params![id])?;
    let n = tx.execute("DELETE FROM memory_facts WHERE id = ?1", params![id])?;
    tx.commit()?;
    Ok(n > 0)
}

/// All active facts (not shadowed), most recently confirmed first. Input for
/// dedup during consolidation, and for listing.
pub fn active_facts(conn: &Connection) -> Result<Vec<Fact>> {
    let sql = format!(
        "SELECT {FACT_COLS} FROM memory_facts WHERE superseded_by IS NULL ORDER BY last_seen DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], fact_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// All facts including shadowed ones (CLI `memory list --all`), newest first.
pub fn all_facts(conn: &Connection) -> Result<Vec<Fact>> {
    let sql = format!("SELECT {FACT_COLS} FROM memory_facts ORDER BY id DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], fact_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Active fact by id (None = doesn't exist or is shadowed) — for filling in
/// vector hits that aren't in the FTS result.
pub fn fact_by_id(conn: &Connection, id: i64) -> Result<Option<Fact>> {
    let sql = format!(
        "SELECT {FACT_COLS} FROM memory_facts WHERE id = ?1 AND superseded_by IS NULL"
    );
    conn.query_row(&sql, params![id], fact_from_row).optional().map_err(Into::into)
}

/// Active pinned facts (profile) — always go into the prompt, ordered by salience.
pub fn pinned_facts(conn: &Connection) -> Result<Vec<Fact>> {
    let sql = format!(
        "SELECT {FACT_COLS} FROM memory_facts
         WHERE superseded_by IS NULL AND pinned = 1 ORDER BY salience DESC, last_seen DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], fact_from_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// FTS5 search over active facts (relevance via bm25, best first).
pub fn search_facts(conn: &Connection, match_query: &str, limit: usize) -> Result<Vec<Fact>> {
    // columns must be f.-qualified: memory_facts_fts also has subject/text
    let sql = format!(
        "SELECT {} FROM memory_facts_fts
         JOIN memory_facts f ON f.id = memory_facts_fts.rowid
         WHERE memory_facts_fts MATCH ?1 AND f.superseded_by IS NULL
         ORDER BY bm25(memory_facts_fts) LIMIT ?2",
        FACT_COLS.split(", ").map(|c| format!("f.{c}")).collect::<Vec<_>>().join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![match_query, limit as i64], fact_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Read-time decay: deletes active, NOT-pinned facts whose effective salience
/// (base × 0.5^(age/half-life)) has dropped below `floor`. `half_life_days == 0`
/// = no decay. Computed in Rust (no dependency on SQL `pow`).
pub fn prune_faded_facts(
    conn: &Connection,
    now: i64,
    half_life_days: u64,
    floor: f64,
) -> Result<usize> {
    if half_life_days == 0 {
        return Ok(0);
    }
    let hl = (half_life_days * 86_400) as f64;
    let mut pruned = 0;
    for f in active_facts(conn)? {
        if f.pinned {
            continue;
        }
        let eff = f.salience * 0.5f64.powf((now - f.last_seen).max(0) as f64 / hl);
        if eff < floor && delete_fact(conn, f.id)? {
            pruned += 1;
        }
    }
    Ok(pruned)
}

// ---------- dense embeddings (migration v9) ----------

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Stores/updates a row's embedding (`source` ∈ fact|conversation|utterance).
pub fn upsert_embedding(
    conn: &Connection,
    source: &str,
    ref_id: i64,
    model: &str,
    v: &[f32],
) -> Result<()> {
    conn.execute(
        "INSERT INTO embeddings(source, ref_id, model, dim, vec) VALUES(?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(source, ref_id) DO UPDATE SET model=excluded.model, dim=excluded.dim, vec=excluded.vec",
        params![source, ref_id, model, v.len() as i64, vec_to_blob(v)],
    )?;
    Ok(())
}

/// All embeddings for a given source as (ref_id, vector) — input for brute-force
/// KNN. Doesn't care whether the source is active; the caller handles joining to live rows.
pub fn embeddings_for_source(conn: &Connection, source: &str) -> Result<Vec<(i64, Vec<f32>)>> {
    let mut stmt = conn.prepare("SELECT ref_id, vec FROM embeddings WHERE source = ?1")?;
    let rows = stmt
        .query_map(params![source], |r| Ok((r.get::<_, i64>(0)?, blob_to_vec(&r.get::<_, Vec<u8>>(1)?))))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// A cheap fingerprint of a source's embeddings, for invalidating an in-memory
/// cache: (count, max ref_id). Both inserts and deletes change the fingerprint
/// (new embeddings get higher ref_ids), so the cache only needs reloading after
/// a write, not on every query. The UNIQUE(source, ref_id) index keeps this fast
/// (no full table scan).
pub fn embeddings_signature(conn: &Connection, source: &str) -> Result<(i64, i64)> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(ref_id), 0) FROM embeddings WHERE source = ?1",
        params![source],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(Into::into)
}

/// ref_ids that already have an embedding (incremental embedding skips these).
pub fn embedded_ref_ids(conn: &Connection, source: &str) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = conn.prepare("SELECT ref_id FROM embeddings WHERE source = ?1")?;
    let rows = stmt
        .query_map(params![source], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn embedding_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0)).map_err(Into::into)
}

pub fn delete_embedding(conn: &Connection, source: &str, ref_id: i64) -> Result<()> {
    conn.execute("DELETE FROM embeddings WHERE source = ?1 AND ref_id = ?2", params![source, ref_id])?;
    Ok(())
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

// ---------- conversations ----------

pub fn insert_conversation(
    conn: &Connection,
    ts: i64,
    question: &str,
    answer: &str,
    model: &str,
    cost_usd: f64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO conversations(ts, question, answer, model, cost_usd)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![ts, question, answer, model, cost_usd],
    )?;
    Ok(())
}

/// Conversations in the window [from, to), chronological — input for memory consolidation.
pub fn conversations_between(
    conn: &Connection,
    from: i64,
    to: i64,
) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT ts, question, answer FROM conversations WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
    )?;
    let rows = stmt
        .query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// The last `limit` exchanges with timestamps, newest first (ts DESC) — input
/// for follow-up context. The retrieval layer picks out just the current
/// session's exchanges (`memory::session_window`).
pub fn recent_conversations_ts(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT ts, question, answer FROM conversations ORDER BY ts DESC, id DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn conversation_count_since(conn: &Connection, since_ts: i64) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM conversations WHERE ts >= ?1",
        params![since_ts],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

// ---------- hybrid retrieval (FTS5, migration v7) ----------

/// A hit in the FTS5 index: `(id, ts, text_a, text_b, bm25_score)`. `id` is the
/// rowid (key for fusing with vectors). For conversations `text_a`=question,
/// `text_b`=answer; for utterances `text_a`=transcript, `text_b`=None. Lower
/// bm25 = better (rows already come sorted best-first).
pub type FtsHit = (i64, i64, String, Option<String>, f64);

/// FTS5 search over past conversations. `match_query` is a ready-made FTS5 MATCH
/// expression (the caller builds it from terms); an empty/invalid expression
/// returns an error the retrieval layer swallows (search is best-effort and must
/// never crash the dialog).
pub fn search_conversations(conn: &Connection, match_query: &str, limit: usize) -> Result<Vec<FtsHit>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.ts, c.question, c.answer, bm25(conversations_fts) AS rank
         FROM conversations_fts JOIN conversations c ON c.id = conversations_fts.rowid
         WHERE conversations_fts MATCH ?1 ORDER BY rank LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![match_query, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                Some(r.get::<_, String>(3)?),
                r.get::<_, f64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// FTS5 search over past mic utterances (source='mic'). Meet/wav transcripts
/// are excluded from retrieval — dialog context is about what was said in the
/// room, not what came up in someone else's meeting.
pub fn search_utterances(conn: &Connection, match_query: &str, limit: usize) -> Result<Vec<FtsHit>> {
    let mut stmt = conn.prepare(
        "SELECT u.id, u.ts_start, u.text, bm25(utterances_fts) AS rank
         FROM utterances_fts JOIN utterances u ON u.id = utterances_fts.rowid
         WHERE utterances_fts MATCH ?1 AND u.source = 'mic' ORDER BY rank LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![match_query, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, None, r.get::<_, f64>(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Conversation text by id (for filling in vector hits outside the FTS result).
pub fn conversation_by_id(conn: &Connection, id: i64) -> Result<Option<(i64, String, String)>> {
    conn.query_row(
        "SELECT ts, question, answer FROM conversations WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .map_err(Into::into)
}

/// Mic utterance text by id (for filling in vector hits outside the FTS result).
pub fn utterance_by_id(conn: &Connection, id: i64) -> Result<Option<(i64, String)>> {
    conn.query_row(
        "SELECT ts_start, text FROM utterances WHERE id = ?1 AND source = 'mic'",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

/// Texts of all mic utterances (id, text) for bulk embedding.
pub fn all_mic_utterances(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt =
        conn.prepare("SELECT id, text FROM utterances WHERE source = 'mic' ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Texts of all conversations (id, question, answer) for bulk embedding.
pub fn all_conversations(conn: &Connection) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare("SELECT id, question, answer FROM conversations ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
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

/// Inserts/updates digest content; an existing row's status is left unchanged
/// (sent stays sent, pending stays pending).
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

/// Atomic claim before sending — prevents a double send when the digest timer
/// and the hourly retry (analyze) race. `allow_resend` (an explicit
/// `digest --send`) may also claim a row with status `sent`.
/// A stuck claim (`sending` older than an hour — process crash) can always be reclaimed.
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

/// Releases the claim after a failed send — the digest goes back to the state
/// it had BEFORE the claim (`restore_to`). For a forced resend it's `sent`:
/// otherwise a failed resend would drop an already-delivered digest back to
/// `pending` and the hourly retry would send it again. For a normal send it's
/// `pending` (stays queued to be sent).
pub fn unclaim_digest(conn: &Connection, date: &str, restore_to: &str) -> Result<()> {
    conn.execute(
        "UPDATE daily_digests SET status=?2 WHERE date=?1 AND status='sending'",
        params![date, restore_to],
    )?;
    Ok(())
}

/// Digests awaiting delivery: pending + stuck `sending` older than an hour.
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
        migrate(&conn).unwrap(); // a second call must not fail
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 13);
    }

    #[test]
    fn nudges_roundtrip_count_and_cooldown() {
        let conn = test_conn();
        // fresh DB → no nudges
        assert_eq!(nudge_count_since(&conn, 0).unwrap(), 0);
        assert_eq!(last_nudge_ts(&conn, "pattern_ready", "k1").unwrap(), None);
        let id =
            insert_nudge(&conn, 1000, "pattern_ready", "k1", "3× ruční přenos A→B", "propose", "7", "voice")
                .unwrap();
        assert!(id > 0);
        // daily cap: count from a time before and after insertion
        assert_eq!(nudge_count_since(&conn, 0).unwrap(), 1);
        assert_eq!(nudge_count_since(&conn, 2000).unwrap(), 0);
        // cooldown only sees the last ts for a matching (kind, dedup_key)
        assert_eq!(last_nudge_ts(&conn, "pattern_ready", "k1").unwrap(), Some(1000));
        assert_eq!(last_nudge_ts(&conn, "pattern_ready", "jiný").unwrap(), None);
        assert_eq!(last_nudge_ts(&conn, "runbook_failing", "k1").unwrap(), None);
        // load + status transition
        let row = nudge_by_id(&conn, id).unwrap().unwrap();
        assert_eq!(row.action_kind, "propose");
        assert_eq!(row.action_ref, "7");
        assert_eq!(row.status, "offered");
        set_nudge_status(&conn, id, "done", "vygenerován návrh #5").unwrap();
        assert_eq!(nudge_by_id(&conn, id).unwrap().unwrap().status, "done");
        assert_eq!(recent_nudges(&conn, 10).unwrap().len(), 1);
        assert!(nudge_by_id(&conn, 999).unwrap().is_none());
    }

    #[test]
    fn fts_search_conversations_ranks_and_folds_diacritics() {
        let conn = test_conn();
        insert_conversation(&conn, 100, "Jaké bude zítra počasí v Praze?",
            "Zítra bude v Praze slunečno, pane.", "haiku", 0.01).unwrap();
        insert_conversation(&conn, 200, "Kolik je hodin?", "Je půl třetí, pane.", "haiku", 0.01).unwrap();
        // a query without diacritics finds the row with diacritics (index folded počasí→pocasi)
        let hits = search_conversations(&conn, "\"pocasi\" OR \"praze\"", 5).unwrap();
        assert!(!hits.is_empty());
        // tuple = (id, ts, question, answer, score)
        assert_eq!(hits[0].1, 100, "nejrelevantnější je řádek o počasí (ts)");
        assert_eq!(hits[0].3.as_deref(), Some("Zítra bude v Praze slunečno, pane."));
        // an unrelated term returns nothing
        assert!(search_conversations(&conn, "\"banan\"", 5).unwrap().is_empty());
    }

    #[test]
    fn fts_prefix_matches_czech_inflections() {
        let conn = test_conn();
        insert_conversation(&conn, 100, "musím podepsat smlouvu", "smlouva je hotová, pane", "m", 0.0)
            .unwrap();
        // a prefix query on the stem matches inflected forms (smlouvu and smlouva) —
        // this checks both FTS5 prefix syntax `"kmen"*` and Czech recall
        let hits = search_conversations(&conn, "\"smlou\"*", 5).unwrap();
        assert_eq!(hits.len(), 1);
        // the exact form "smlouvou" (instrumental case) wouldn't match either without a prefix
        assert!(search_conversations(&conn, "\"smlouvou\"", 5).unwrap().is_empty());
    }

    #[test]
    fn fts_reflects_delete_via_trigger() {
        let conn = test_conn();
        insert_conversation(&conn, 100, "test smlouva Tomáš", "ano pane", "haiku", 0.0).unwrap();
        assert_eq!(search_conversations(&conn, "\"smlouva\"", 5).unwrap().len(), 1);
        conn.execute("DELETE FROM conversations WHERE ts = 100", []).unwrap();
        // the conversations_ad trigger must keep the index consistent
        assert!(search_conversations(&conn, "\"smlouva\"", 5).unwrap().is_empty());
    }

    #[test]
    fn fts_search_utterances_mic_only() {
        let conn = test_conn();
        insert_utterance(&conn, 10, 15, "musím zavolat Tomášovi ohledně smlouvy", "cs", 0.9, "mic").unwrap();
        insert_utterance(&conn, 20, 25, "smlouva zazněla na schůzce", "cs", 0.9, "meet").unwrap();
        let hits = search_utterances(&conn, "\"smlouvy\" OR \"smlouva\"", 5).unwrap();
        // only the mic utterance; meet/someone else's meeting isn't pulled into dialog retrieval
        assert_eq!(hits.len(), 1);
        // tuple = (id, ts, text, None, score)
        assert_eq!(hits[0].1, 10, "ts_start");
        assert!(hits[0].2.contains("Tomášovi"));
    }

    #[test]
    fn utterances_roundtrip() {
        let conn = test_conn();
        insert_utterance(&conn, 100, 105, "ahoj světe", "cs", 0.92, "mic").unwrap();
        insert_utterance(&conn, 200, 203, "hello", "en", 0.85, "wav").unwrap();
        let rows = utterances_between(&conn, 0, 1000).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "ahoj světe");
        assert_eq!(rows[0].lang, "cs");
        assert!((rows[0].conf - 0.92).abs() < 1e-9);
        assert_eq!(last_utterance(&conn).unwrap().unwrap().0, 200);
        assert_eq!(utterance_count_since(&conn, 150).unwrap(), 1);
        // end of interval is exclusive
        assert_eq!(utterances_between(&conn, 100, 200).unwrap().len(), 1);
    }

    #[test]
    fn facts_crud_supersede_and_search() {
        let conn = test_conn();
        let a = insert_fact(&conn, "preference", "káva", "Pán pije kávu bez cukru.", 0.8, false, "cli")
            .unwrap();
        let _p = insert_fact(&conn, "profile", "", "Pán komunikuje česky.", 0.95, true, "cli").unwrap();
        // active = both; pinned = only the profile
        assert_eq!(active_facts(&conn).unwrap().len(), 2);
        assert_eq!(pinned_facts(&conn).unwrap().len(), 1);
        assert!(pinned_facts(&conn).unwrap()[0].text.contains("česky"));
        // FTS finds the fact by topic (even an inflected query via the stem)
        let hits = search_facts(&conn, "\"kav\"*", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a);
        // update: the new fact shadows the old one → the old one drops out of active
        let b = insert_fact(&conn, "preference", "káva", "Pán pije kávu s mlékem.", 0.9, false, "cli")
            .unwrap();
        supersede_fact(&conn, a, b).unwrap();
        let active = active_facts(&conn).unwrap();
        assert_eq!(active.len(), 2); // profile + the new coffee fact
        assert!(active.iter().all(|f| f.id != a));
        // FTS no longer returns the old fact (active filter), but returns the new one
        let hits = search_facts(&conn, "\"kav\"*", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, b);
    }

    #[test]
    fn facts_touch_bumps_salience_capped() {
        let conn = test_conn();
        let id = insert_fact(&conn, "fact", "", "něco", 0.7, false, "cli").unwrap();
        for _ in 0..10 {
            touch_fact(&conn, id).unwrap();
        }
        let f = &active_facts(&conn).unwrap()[0];
        assert!((f.salience - 2.0).abs() < 1e-9, "salience stropovaná na 2.0, je {}", f.salience);
    }

    #[test]
    fn facts_prune_fades_old_unpinned_not_pinned() {
        let conn = test_conn();
        let now = crate::util::now_ts();
        // old unpinned fact (last seen 2 half-lives ago) → effective
        // salience 1.0*0.25 = 0.25; gets pruned at floor 0.3
        let old = insert_fact(&conn, "fact", "", "starý zvyk", 0.5, false, "consolidate").unwrap();
        conn.execute("UPDATE memory_facts SET last_seen = ?2 WHERE id = ?1",
            params![old, now - 2 * 60 * 86_400]).unwrap();
        // an equally old pinned fact is NEVER pruned
        let pin = insert_fact(&conn, "profile", "", "pán je Daniel", 1.0, true, "cli").unwrap();
        conn.execute("UPDATE memory_facts SET last_seen = ?2 WHERE id = ?1",
            params![pin, now - 5 * 60 * 86_400]).unwrap();
        let pruned = prune_faded_facts(&conn, now, 60, 0.3).unwrap();
        assert_eq!(pruned, 1);
        let active = active_facts(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].pinned);
        // half_life_days == 0 = decay disabled (nothing gets deleted)
        assert_eq!(prune_faded_facts(&conn, now, 0, 0.3).unwrap(), 0);
    }

    #[test]
    fn embeddings_roundtrip_and_upsert() {
        let conn = test_conn();
        let v1 = vec![0.1f32, -0.2, 0.3, 0.4];
        upsert_embedding(&conn, "fact", 7, "e5", &v1).unwrap();
        upsert_embedding(&conn, "conversation", 3, "e5", &[1.0, 2.0]).unwrap();
        assert_eq!(embedding_count(&conn).unwrap(), 2);
        // blob roundtrip preserves f32 values exactly (bit-exact LE)
        let facts = embeddings_for_source(&conn, "fact").unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0], (7, v1.clone()));
        // upserting the same key overwrites the vector (doesn't duplicate)
        upsert_embedding(&conn, "fact", 7, "e5", &[9.0, 9.0]).unwrap();
        assert_eq!(embedding_count(&conn).unwrap(), 2);
        assert_eq!(embeddings_for_source(&conn, "fact").unwrap()[0].1, vec![9.0, 9.0]);
        // embedded_ref_ids + delete
        assert!(embedded_ref_ids(&conn, "fact").unwrap().contains(&7));
        delete_embedding(&conn, "fact", 7).unwrap();
        assert_eq!(embedding_count(&conn).unwrap(), 1);
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
        // end of interval is exclusive
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
        // rebuilding the content must not drop status back to pending
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
        // a concurrent process doesn't get the claim
        assert!(!claim_digest(&conn, "2026-07-17", false).unwrap());
        // after releasing the claim it works again (normal send → back to pending)
        unclaim_digest(&conn, "2026-07-17", "pending").unwrap();
        assert!(claim_digest(&conn, "2026-07-17", false).unwrap());
        mark_digest_sent(&conn, "2026-07-17", None).unwrap();
        // sent: without force no, with force (explicit resend) yes
        assert!(!claim_digest(&conn, "2026-07-17", false).unwrap());
        assert!(claim_digest(&conn, "2026-07-17", true).unwrap());
    }

    #[test]
    fn unclaim_restores_prior_status_not_pending() {
        let conn = test_conn();
        upsert_digest(&conn, "2026-07-17", "md", "html").unwrap();
        mark_digest_sent(&conn, "2026-07-17", None).unwrap();
        // a forced resend claims 'sent' → 'sending'
        assert!(claim_digest(&conn, "2026-07-17", true).unwrap());
        // send fails → revert to the ORIGINAL state 'sent', not 'pending'
        unclaim_digest(&conn, "2026-07-17", "sent").unwrap();
        let (_, _, status) = digest_row(&conn, "2026-07-17").unwrap().unwrap();
        assert_eq!(status, "sent", "selhaný resend nesmí vzkřísit odeslaný digest do fronty");
        assert!(pending_digest_dates(&conn).unwrap().is_empty());
    }

    #[test]
    fn pending_dates_include_stale_sending() {
        let conn = test_conn();
        upsert_digest(&conn, "2026-07-16", "md", "html").unwrap();
        assert!(claim_digest(&conn, "2026-07-16", false).unwrap());
        // freshly claimed isn't in the queue
        assert!(pending_digest_dates(&conn).unwrap().is_empty());
        // a stuck claim (process crash) returns to the queue after an hour
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
