//! Scheduled internal tasks (`jarvis tasks`) — housekeeping Jarvis runs on its
//! own: checking its own dependencies, SQLite maintenance, screenshot cleanup.
//!
//! Difference from runbooks (phase D): a runbook is a USER shell script that
//! went through approval and fingerprint pinning (neither mic nor timer is
//! trusted with it). A task, by contrast, is a BUILT-IN Jarvis function —
//! trusted code, no approval needed. That's why task definitions live in code
//! (`registry`), not the DB; the DB only holds run history (`task_runs`,
//! read back for status/digest and the last run that drives `due`) and any
//! user override of schedule/enabled state (`state`).
//!
//! Safety invariant: a scheduled task NEVER installs a dependency itself or
//! changes the system outside its own data. `deps` only DETECTS and reports
//! with an exact remediation command — unattended auto-`cargo install`/
//! downloads would bypass the whole "nothing unapproved" model. The only
//! writes a task may make: cleaning up old screenshots (retention) and
//! maintaining its own DB — both idempotent.

use crate::config::{Config, Paths};
use crate::store::db;
use crate::util;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

// ---------- schedule ----------

/// A task's run schedule: `manual` (only by hand), `every <duration>`
/// (interval since last start — good for checks), or `daily@HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    Manual,
    Every { secs: u64 },
    Daily { hour: u32, min: u32 },
}

pub fn parse_schedule(spec: &str) -> Result<Schedule> {
    let s = spec.trim();
    if s.eq_ignore_ascii_case("manual") {
        return Ok(Schedule::Manual);
    }
    if let Some(rest) = s.strip_prefix("every ") {
        let secs = crate::config::parse_duration_spec(rest.trim())?;
        // sub-minute makes no sense for the scheduler (tick is per-minute) and risks thrashing
        anyhow::ensure!(secs >= 60, "plán „{spec}“ — interval musí být aspoň 60 s (např. every 6h)");
        return Ok(Schedule::Every { secs });
    }
    if let Some(t) = s.strip_prefix("daily@") {
        let (h, m) = t
            .split_once(':')
            .with_context(|| format!("plán „{spec}“ — čekám daily@HH:MM"))?;
        let (hour, min): (u32, u32) = (
            h.parse().with_context(|| format!("neplatná hodina v „{spec}“"))?,
            m.parse().with_context(|| format!("neplatná minuta v „{spec}“"))?,
        );
        anyhow::ensure!(hour <= 23 && min <= 59, "plán „{spec}“ — hodina 0–23, minuta 0–59");
        return Ok(Schedule::Daily { hour, min });
    }
    bail!("neznámý plán „{spec}“ — podporuji `manual`, `every <trvání>` nebo `daily@HH:MM`")
}

/// Unix ts of today's HH:MM in local time; None in a DST gap (the task is
/// skipped that day — better than running at the wrong hour).
fn daily_threshold_ts(now: chrono::DateTime<chrono::Local>, hour: u32, min: u32) -> Option<i64> {
    use chrono::TimeZone as _;
    let t = now.date_naive().and_hms_opt(hour, min, 0)?;
    match chrono::Local.from_local_datetime(&t) {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        chrono::LocalResult::Ambiguous(dt, _) => Some(dt.timestamp()),
        chrono::LocalResult::None => None,
    }
}

/// Is the task due? `every`: never ran, or the interval elapsed since the
/// last start. `daily`: past today's HH:MM, if today's attempt hasn't
/// happened yet (even a failed one counts — no retry storms; missed days are
/// caught up the next day).
fn due(schedule: Schedule, now: chrono::DateTime<chrono::Local>, last_started: Option<i64>) -> bool {
    match schedule {
        Schedule::Manual => false,
        Schedule::Every { secs } => match last_started {
            None => true,
            Some(l) => now.timestamp() - l >= secs as i64,
        },
        Schedule::Daily { hour, min } => match daily_threshold_ts(now, hour, min) {
            Some(threshold) => now.timestamp() >= threshold && last_started.is_none_or(|l| l < threshold),
            None => false,
        },
    }
}

// ---------- registry of built-in tasks ----------

/// A task's body: returns a human summary (stored as the run's output). Err =
/// the task failed / found a problem (for checks, finding a problem is a
/// legitimate "failure").
pub type TaskFn = fn(&Paths, &Config, &Connection) -> Result<String>;

pub struct TaskDef {
    pub name: &'static str,
    pub default_schedule: &'static str,
    pub description: &'static str,
    pub run: TaskFn,
}

/// Source of truth for which tasks exist (not the DB — they're functions in
/// code). Adding/removing a task is a change here; the DB only holds history
/// and any schedule/enabled overrides.
pub fn registry() -> Vec<TaskDef> {
    vec![
        TaskDef {
            name: "deps",
            default_schedule: "every 24h",
            description: "Kontrola vlastních závislostí (binárky, modely, klíče, disk)",
            run: run_deps,
        },
        TaskDef {
            name: "purge-screenshots",
            default_schedule: "daily@04:15",
            description: "Úklid snímků starších než retention.screenshots_days",
            run: run_purge_screenshots,
        },
        TaskDef {
            name: "db-maintenance",
            default_schedule: "daily@04:20",
            description: "Údržba SQLite (WAL checkpoint + optimize)",
            run: run_db_maintenance,
        },
    ]
}

pub fn find(name: &str) -> Option<TaskDef> {
    registry().into_iter().find(|t| t.name == name.trim())
}

/// Finds a task by name; error includes the list of names (so CLI/voice can prompt).
fn resolve(name: &str) -> Result<TaskDef> {
    find(name).with_context(|| {
        let names: Vec<&str> = registry().iter().map(|t| t.name).collect();
        format!("úloha „{name}“ neexistuje — mám: {}", names.join(", "))
    })
}

// ---------- schedule/enabled overrides (state) ----------

fn sched_key(name: &str) -> String {
    format!("task:{name}:schedule")
}
fn enabled_key(name: &str) -> String {
    format!("task:{name}:enabled")
}

/// Effective schedule for a task: user override, else the registry default.
pub fn effective_schedule(conn: &Connection, def: &TaskDef) -> String {
    db::state_get(conn, &sched_key(def.name))
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| def.default_schedule.to_string())
}

/// Is the task enabled for the scheduler? Enabled by default; override "0" disables it.
pub fn is_enabled(conn: &Connection, def: &TaskDef) -> bool {
    match db::state_get(conn, &enabled_key(def.name)).ok().flatten() {
        Some(v) => v.trim() != "0",
        None => true,
    }
}

pub fn set_enabled(conn: &Connection, name: &str, enabled: bool) -> Result<()> {
    resolve(name)?;
    db::state_set(conn, &enabled_key(name), if enabled { "1" } else { "0" })
}

pub fn set_schedule(conn: &Connection, name: &str, spec: &str) -> Result<Schedule> {
    resolve(name)?;
    let sched = parse_schedule(spec)?;
    db::state_set(conn, &sched_key(name), spec.trim())?;
    Ok(sched)
}

// ---------- run history (task_runs) ----------

/// Record of one task run.
#[derive(Debug, Clone)]
pub struct TaskRun {
    pub task: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// None = didn't finish (process crash), Some(true) = OK, Some(false) = problem.
    pub ok: Option<bool>,
    pub trigger: String,
    pub output: String,
}

fn row_to_run(r: &rusqlite::Row) -> rusqlite::Result<TaskRun> {
    Ok(TaskRun {
        task: r.get(0)?,
        started_at: r.get(1)?,
        finished_at: r.get(2)?,
        ok: r.get::<_, Option<i64>>(3)?.map(|v| v != 0),
        trigger: r.get(4)?,
        output: r.get(5)?,
    })
}

const RUN_COLS: &str = "task, started_at, finished_at, ok, trigger, output";

fn last_started(conn: &Connection, name: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(started_at) FROM task_runs WHERE task = ?1",
        params![name],
        |r| r.get::<_, Option<i64>>(0),
    )
    .map_err(Into::into)
}

pub fn recent_runs(conn: &Connection, limit: usize) -> Result<Vec<TaskRun>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {RUN_COLS} FROM task_runs ORDER BY started_at DESC LIMIT ?1"))?;
    let rows = stmt
        .query_map(params![limit as i64], row_to_run)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Runs within [from, to) — for the digest.
pub fn runs_between(conn: &Connection, from: i64, to: i64) -> Result<Vec<TaskRun>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RUN_COLS} FROM task_runs WHERE started_at >= ?1 AND started_at < ?2
         ORDER BY started_at"
    ))?;
    let rows = stmt
        .query_map(params![from, to], row_to_run)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// ---------- execution ----------

/// Runs a task (a built-in function) with an flock lock against concurrency
/// (timer vs. the `jarvis run` loop). The run row is written right at start
/// (NULL finished_at = running / "didn't finish" after a crash) and completed
/// at the end. Returns a TaskRun even when the task ended with a problem —
/// an infrastructure error (lock/DB) returns Err.
pub fn run_one(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    def: &TaskDef,
    trigger: &str,
) -> Result<TaskRun> {
    // per-task flock: a second concurrent run of the same task is rejected outright
    let lock_path = paths.data_dir.join(format!("task-{}.lock", def.name));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("nelze otevřít {}", lock_path.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("úloha „{}“ právě běží — nespouštím podruhé", def.name);
    }

    let started = util::now_ts();
    info!("úloha „{}“ startuje ({trigger})", def.name);
    conn.execute(
        "INSERT INTO task_runs(task, started_at, trigger) VALUES(?1, ?2, ?3)",
        params![def.name, started, trigger],
    )
    .context("zápis řádku běhu úlohy selhal")?;
    let run_id = conn.last_insert_rowid();

    let outcome = (def.run)(paths, cfg, conn);
    let finished = util::now_ts();
    let (ok, raw) = match &outcome {
        Ok(msg) => (true, msg.clone()),
        Err(e) => (false, format!("{e:#}")),
    };
    let output = util::truncate_chars(raw.trim(), cfg.tasks.max_output_chars);
    conn.execute(
        "UPDATE task_runs SET finished_at = ?2, ok = ?3, output = ?4 WHERE id = ?1",
        params![run_id, finished, ok as i64, output],
    )?;

    match &outcome {
        Ok(_) => info!("úloha „{}“ doběhla OK ({} s)", def.name, finished - started),
        Err(e) => warn!("úloha „{}“ narazila na problém: {e:#}", def.name),
    }
    // problem → optional Telegram notice (best-effort; informational only)
    if !ok && cfg.tasks.notify_telegram {
        notify_problem(paths, def.name, &output);
    }
    Ok(TaskRun {
        task: def.name.into(),
        started_at: started,
        finished_at: Some(finished),
        ok: Some(ok),
        trigger: trigger.into(),
        output,
    })
}

/// Walks enabled scheduled tasks and runs the ones that are due. One task's
/// error must not stop the others; returns the completed runs.
pub fn run_due(paths: &Paths, cfg: &Config, conn: &Connection) -> Result<Vec<TaskRun>> {
    if !cfg.tasks.enabled {
        return Ok(Vec::new());
    }
    let now = chrono::Local::now();
    let mut results = Vec::new();
    for def in registry() {
        if !is_enabled(conn, &def) {
            continue;
        }
        let schedule = match parse_schedule(&effective_schedule(conn, &def)) {
            Ok(s) => s,
            Err(e) => {
                warn!("úloha „{}“ má neplatný plán: {e:#}", def.name);
                continue;
            }
        };
        if !due(schedule, now, last_started(conn, def.name)?) {
            continue;
        }
        match run_one(paths, cfg, conn, &def, "timer") {
            Ok(row) => results.push(row),
            Err(e) => warn!("plánovaná úloha „{}“ selhala: {e:#}", def.name),
        }
    }
    Ok(results)
}

/// One scheduler tick — shared by the systemd timer (`tasks run-due`) and the
/// built-in `jarvis run` scheduler. Logs errors and keeps going.
pub fn tick(paths: &Paths, cfg: &Config, conn: &Connection) -> Vec<TaskRun> {
    match run_due(paths, cfg, conn) {
        Ok(rows) => {
            if !rows.is_empty() {
                info!("plánovač úloh: dokončeno {} úloh(a)", rows.len());
            }
            rows
        }
        Err(e) => {
            warn!("plánovač úloh selhal: {e:#}");
            Vec::new()
        }
    }
}

/// Reports a scheduled task's problem to Telegram (best-effort). Informs
/// only — the remediation command is part of the message, a human takes the action.
fn notify_problem(paths: &Paths, task: &str, detail: &str) {
    let Ok((token, chat_id)) = crate::config::telegram_keys(paths) else {
        warn!("úloha „{task}“: je co ohlásit, ale Telegram klíče nejsou v secrets.env");
        return;
    };
    let text = format!(
        "Jarvis: plánovaná úloha „{task}“ našla problém:\n{}",
        util::truncate_chars(detail, 500)
    );
    match crate::telegram::send_message(&token, &chat_id, &text) {
        Ok(()) => info!("úloha „{task}“: problém ohlášen na Telegram"),
        Err(e) => warn!("úloha „{task}“: Telegram hláška selhala: {e:#}"),
    }
}

// ---------- task bodies ----------

/// Collector for dependency-check findings. `problem` = a dependency missing
/// per the current config (counts as failure); `warn` = an on-demand-only
/// dependency (missing, but doesn't block normal operation).
#[derive(Default)]
struct Findings {
    lines: Vec<String>,
    problems: usize,
}

impl Findings {
    fn ok(&mut self, label: &str, detail: impl Into<String>) {
        self.lines.push(format!("✓ {label} — {}", detail.into()));
    }
    fn warn(&mut self, label: &str, detail: impl Into<String>) {
        self.lines.push(format!("⚠ {label} — {}", detail.into()));
    }
    fn problem(&mut self, label: &str, detail: impl Into<String>) {
        self.lines.push(format!("✗ {label} — {}", detail.into()));
        self.problems += 1;
    }
}

/// Finds a binary: an absolute/relative path is checked directly, a bare name
/// is looked up in $PATH. No process spawned — just file existence (dependency presence).
fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = Path::new(bin);
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file())
}

/// Free space (bytes) on the volume holding `path` — statvfs, no extra dependency.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0 {
        Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
    } else {
        None
    }
}

/// Checks Jarvis's own dependencies against the CURRENT config: only reports
/// what this setup actually needs (e.g. the whisper model only when listening
/// doesn't run purely through Scribe). Offline and cheap (no network or API).
/// Findings are stored as the run's output (read-back); a problem = Err → Telegram.
fn run_deps(paths: &Paths, cfg: &Config, conn: &Connection) -> Result<String> {
    let mut f = Findings::default();

    // claude CLI — the brain behind analysis, conversation, and the digest
    match which("claude") {
        Some(p) => f.ok("claude CLI", p.display().to_string()),
        None => f.problem("claude CLI", "není v PATH — nainstaluj Claude Code CLI"),
    }
    // bash — runbooks run via `bash <script>`
    match which("bash") {
        Some(p) => f.ok("bash", p.display().to_string()),
        None => f.problem("bash", "není v PATH — runbooky nepůjde spustit"),
    }
    // SendGrid key — daily digest by email
    match crate::config::sendgrid_key(paths) {
        Ok(_) => f.ok("SendGrid klíč", "nalezen"),
        Err(_) => f.problem(
            "SendGrid klíč",
            "chybí v secrets.env (SENDGRID_API_KEY) — digest se neodešle",
        ),
    }
    // whisper model — primary STT (engine whisper) or fallback (engine auto)
    if cfg.listen.enabled && cfg.listen.engine != "elevenlabs" {
        let model = cfg.listen.resolve_model_path(paths);
        if model.exists() {
            f.ok("whisper model", model.display().to_string());
        } else {
            f.problem(
                "whisper model",
                format!("{} chybí — `jarvis listen --download-model`", model.display()),
            );
        }
    }
    // ElevenLabs key — Scribe STT and/or TTS
    let needs_eleven = (cfg.listen.enabled && cfg.listen.engine != "whisper")
        || (cfg.speak.enabled && cfg.speak.engine != "piper");
    if needs_eleven {
        match crate::config::elevenlabs_key(paths) {
            Ok(_) => f.ok("ElevenLabs klíč", "nalezen"),
            Err(_) => f.problem("ElevenLabs klíč", "chybí v secrets.env (ELEVENLABS_API_KEY)"),
        }
    }
    // piper (local TTS) — required only when it's the sole speech engine
    if cfg.speak.enabled && cfg.speak.engine == "piper" {
        match which(&cfg.speak.piper_bin) {
            Some(_) => {
                let voice = crate::speak::piper::model_path(paths, &cfg.speak);
                if voice.exists() {
                    f.ok("piper hlas", voice.display().to_string());
                } else {
                    f.problem(
                        "piper hlas",
                        format!("{} chybí — `jarvis say --download-model`", voice.display()),
                    );
                }
            }
            None => f.problem(
                "piper",
                format!("'{}' není v PATH — `pip3 install --user piper-tts`", cfg.speak.piper_bin),
            ),
        }
    }
    // audio player — TTS needs something to play sound with
    if cfg.speak.enabled {
        match crate::speak::detect_player(&cfg.speak.player) {
            Some(p) => f.ok("audio přehrávač", p),
            None => f.problem("audio přehrávač", "chybí ffplay/mpv/ffmpeg+paplay"),
        }
    }
    // browser for Meet — only needed on-demand (`jarvis meet`), hence a warning
    if cfg.meet.enabled {
        match which(&cfg.meet.chrome_bin) {
            Some(_) => f.ok("prohlížeč (Meet)", cfg.meet.chrome_bin.clone()),
            None => f.warn(
                "prohlížeč (Meet)",
                format!("'{}' není v PATH — `jarvis meet` nepůjde", cfg.meet.chrome_bin),
            ),
        }
    }
    // free disk space (data_dir) — models and screenshots grow; full disk breaks capture and STT
    match free_bytes(&paths.data_dir) {
        Some(bytes) => {
            let min = cfg.tasks.min_disk_free_mb.saturating_mul(1024 * 1024);
            if cfg.tasks.min_disk_free_mb > 0 && bytes < min {
                f.problem(
                    "místo na disku",
                    format!("jen {} volných (práh {} MB)", util::human_bytes(bytes), cfg.tasks.min_disk_free_mb),
                );
            } else {
                f.ok("místo na disku", format!("{} volných", util::human_bytes(bytes)));
            }
        }
        None => f.warn("místo na disku", "statvfs selhal — nezjištěno"),
    }
    // database — we have an open connection, so it's available; add the corpus size
    let samples: i64 =
        conn.query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0)).unwrap_or(-1);
    f.ok("databáze", format!("dostupná ({samples} vzorků)"));

    let report = f.lines.join("\n");
    if f.problems > 0 {
        bail!("{report}\n\n{} problém(ů) závislostí — viz výše", f.problems);
    }
    Ok(report)
}

/// Cleans up screenshots older than retention.screenshots_days (wraps retention).
fn run_purge_screenshots(paths: &Paths, cfg: &Config, conn: &Connection) -> Result<String> {
    let secs = (cfg.retention.screenshots_days as i64).saturating_mul(86_400);
    let n = crate::store::retention::purge(conn, &paths.data_dir, secs)?;
    Ok(format!(
        "odstraněno {n} snímků starších než {} dní",
        cfg.retention.screenshots_days
    ))
}

/// SQLite maintenance: WAL checkpoint (so the -wal file doesn't grow) +
/// optimize (query planner stats). Both idempotent; doesn't block concurrent reads.
fn run_db_maintenance(_paths: &Paths, _cfg: &Config, conn: &Connection) -> Result<String> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;")
        .context("údržba SQLite selhala")?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let bytes = (page_count.saturating_mul(page_size)).max(0) as u64;
    Ok(format!("WAL checkpoint + optimize OK; DB {}", util::human_bytes(bytes)))
}

// ---------- CLI ----------

#[derive(clap::Subcommand)]
pub enum TasksCmd {
    /// List tasks, their schedules, and last run
    List,
    /// Task detail: schedule, state, recent runs and output
    Show { name: String },
    /// Runs the task now (even if disabled — this is a deliberate command)
    Run { name: String },
    /// Runs tasks that are due per schedule (called by the timer / `jarvis run`)
    RunDue,
    /// Run history
    Runs {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Enables the task for the scheduler
    Enable { name: String },
    /// Disables the task for the scheduler (manual `tasks run` still works)
    Disable { name: String },
    /// Changes the task's schedule (manual | every <duration> | daily@HH:MM)
    Schedule { name: String, spec: String },
}

fn fmt_run(r: &TaskRun) -> String {
    let state = match (r.finished_at, r.ok) {
        (None, _) => "běží/nedoběhl",
        (Some(_), Some(true)) => "✓ OK",
        (Some(_), Some(false)) => "✗ problém",
        (Some(_), None) => "✗",
    };
    format!(
        "{}  {:<18} {:<7} {}",
        util::fmt_local(r.started_at),
        util::truncate_chars(&r.task, 18),
        r.trigger,
        state
    )
}

pub fn cli(paths: &Paths, cfg: &Config, conn: &Connection, cmd: TasksCmd) -> Result<()> {
    match cmd {
        TasksCmd::List => {
            if !cfg.tasks.enabled {
                println!("Plánované úlohy jsou vypnuté ([tasks] enabled=false) — plánovač nic nespustí.");
                println!("Ruční spuštění funguje dál: `jarvis tasks run <name>`.\n");
            }
            println!("{:<18} {:<14} {:<8} poslední běh", "úloha", "plán", "stav");
            for def in registry() {
                let last = last_started(conn, def.name)?
                    .map(util::fmt_local)
                    .unwrap_or_else(|| "—".into());
                println!(
                    "{:<18} {:<14} {:<8} {last}",
                    def.name,
                    effective_schedule(conn, &def),
                    if is_enabled(conn, &def) { "zapnutá" } else { "vypnutá" },
                );
                println!("    {}", def.description);
            }
            Ok(())
        }
        TasksCmd::Show { name } => {
            let def = resolve(&name)?;
            println!("„{}“", def.name);
            println!("  popis:  {}", def.description);
            println!("  plán:   {} (default {})", effective_schedule(conn, &def), def.default_schedule);
            println!("  stav:   {}", if is_enabled(conn, &def) { "zapnutá" } else { "vypnutá" });
            let runs = recent_runs(conn, 200)?;
            let mine: Vec<&TaskRun> = runs.iter().filter(|r| r.task == def.name).take(5).collect();
            if mine.is_empty() {
                println!("  běhy:   zatím žádné");
            } else {
                println!("  ── poslední běhy ──");
                for r in &mine {
                    println!("  {}", fmt_run(r));
                }
                if let Some(last) = mine.first() {
                    if !last.output.is_empty() {
                        println!("  ── výstup posledního běhu ──");
                        for line in last.output.lines() {
                            println!("  {line}");
                        }
                    }
                }
            }
            Ok(())
        }
        TasksCmd::Run { name } => {
            let def = resolve(&name)?;
            let row = run_one(paths, cfg, conn, &def, "cli")?;
            println!("{}", fmt_run(&row));
            if !row.output.is_empty() {
                println!("── výstup ──");
                println!("{}", row.output);
            }
            if row.ok == Some(false) {
                bail!("úloha „{}“ narazila na problém (viz výše)", def.name);
            }
            Ok(())
        }
        TasksCmd::RunDue => {
            let rows = tick(paths, cfg, conn);
            if rows.is_empty() {
                println!("Nic není na řadě.");
            }
            for r in &rows {
                println!("{}", fmt_run(r));
            }
            Ok(())
        }
        TasksCmd::Runs { limit } => {
            let runs = recent_runs(conn, limit)?;
            if runs.is_empty() {
                println!("Zatím žádné běhy.");
            }
            for r in runs.iter().rev() {
                println!("{}", fmt_run(r));
            }
            Ok(())
        }
        TasksCmd::Enable { name } => {
            let def = resolve(&name)?;
            set_enabled(conn, def.name, true)?;
            println!("✓ úloha „{}“ zapnutá", def.name);
            Ok(())
        }
        TasksCmd::Disable { name } => {
            let def = resolve(&name)?;
            set_enabled(conn, def.name, false)?;
            println!("✓ úloha „{}“ vypnutá (ruční `tasks run` funguje dál)", def.name);
            Ok(())
        }
        TasksCmd::Schedule { name, spec } => {
            let def = resolve(&name)?;
            set_schedule(conn, def.name, &spec)?;
            println!("✓ úloha „{}“ — plán {}", def.name, spec.trim());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db;

    struct Env {
        paths: Paths,
        cfg: Config,
        conn: Connection,
        _tmp: TempDir,
    }

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn env() -> Env {
        let base = std::env::temp_dir().join(format!(
            "jarvis-tasks-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let paths = Paths {
            config_dir: base.join("cfg"),
            config_file: base.join("cfg/config.toml"),
            secrets_file: base.join("cfg/secrets.env"),
            data_dir: base.clone(),
            shots_dir: base.join("shots"),
            proposals_dir: base.join("proposals"),
            models_dir: base.join("models"),
            tts_cache_dir: base.join("tts"),
            db_path: base.join("jarvis.db"),
        };
        Env { paths, cfg: Config::default(), conn: db::test_conn(), _tmp: TempDir(base) }
    }

    #[test]
    fn schedule_parsing() {
        assert_eq!(parse_schedule("manual").unwrap(), Schedule::Manual);
        assert_eq!(parse_schedule(" MANUAL ").unwrap(), Schedule::Manual);
        assert_eq!(parse_schedule("every 24h").unwrap(), Schedule::Every { secs: 86_400 });
        assert_eq!(parse_schedule("every 30m").unwrap(), Schedule::Every { secs: 1800 });
        assert_eq!(parse_schedule("daily@04:15").unwrap(), Schedule::Daily { hour: 4, min: 15 });
        // sub-minute interval, missing duration, and unknown shapes are rejected
        assert!(parse_schedule("every 30s").is_err());
        assert!(parse_schedule("every").is_err());
        assert!(parse_schedule("every ").is_err());
        assert!(parse_schedule("daily@25:00").is_err());
        assert!(parse_schedule("weekly@10:00").is_err());
        assert!(parse_schedule("").is_err());
    }

    #[test]
    fn due_logic_every_and_daily() {
        use chrono::TimeZone as _;
        let now = chrono::Local.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap();
        // every: never ran → due; ran recently → no; ran long ago → yes
        let every = Schedule::Every { secs: 3600 };
        assert!(due(every, now, None));
        assert!(!due(every, now, Some(now.timestamp() - 100)));
        assert!(due(every, now, Some(now.timestamp() - 4000)));
        // daily past deadline, never ran → due; already attempted today → no
        let daily = Schedule::Daily { hour: 9, min: 0 };
        let nine = daily_threshold_ts(now, 9, 0).unwrap();
        assert!(due(daily, now, None));
        assert!(!due(daily, now, Some(nine + 60)));
        assert!(due(daily, now, Some(nine - 86_400)));
        // before today's deadline → no; manual never
        assert!(!due(Schedule::Daily { hour: 14, min: 0 }, now, None));
        assert!(!due(Schedule::Manual, now, None));
        assert!(!due(Schedule::Manual, now, Some(0)));
    }

    #[test]
    fn registry_is_consistent() {
        let reg = registry();
        assert!(!reg.is_empty());
        let mut names = std::collections::HashSet::new();
        for def in &reg {
            // name is a stable kebab-case id
            assert!(
                !def.name.is_empty()
                    && def.name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "neplatné jméno úlohy: {}",
                def.name
            );
            assert!(names.insert(def.name), "duplicitní jméno úlohy: {}", def.name);
            // default schedule must be valid
            parse_schedule(def.default_schedule)
                .unwrap_or_else(|e| panic!("úloha {} má neplatný default plán: {e:#}", def.name));
        }
        // the flagship self-managed dependency task is present
        assert!(find("deps").is_some());
    }

    #[test]
    fn run_one_records_and_reads_back() {
        let e = env();
        // purge on an empty data_dir deletes 0 screenshots → success, deterministic
        let def = find("purge-screenshots").unwrap();
        let row = run_one(&e.paths, &e.cfg, &e.conn, &def, "cli").unwrap();
        assert_eq!(row.ok, Some(true));
        assert!(row.finished_at.is_some());
        assert!(row.output.contains("odstraněno 0"));
        // read back from the DB
        let runs = recent_runs(&e.conn, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].task, "purge-screenshots");
        assert_eq!(runs[0].ok, Some(true));
    }

    #[test]
    fn db_maintenance_runs() {
        let e = env();
        let def = find("db-maintenance").unwrap();
        let row = run_one(&e.paths, &e.cfg, &e.conn, &def, "cli").unwrap();
        assert_eq!(row.ok, Some(true));
        assert!(row.output.contains("checkpoint"));
    }

    #[test]
    fn deps_runs_and_reports_checks() {
        let e = env();
        let def = find("deps").unwrap();
        // temp env has no SendGrid key → deps finds at least one problem (Err),
        // but the run gets recorded and the report lists the checked dependencies
        let row = run_one(&e.paths, &e.cfg, &e.conn, &def, "cli").unwrap();
        assert!(row.finished_at.is_some());
        assert!(row.output.contains("claude CLI"), "report: {}", row.output);
        assert!(row.output.contains("místo na disku"), "report: {}", row.output);
        assert!(row.output.contains("databáze"), "report: {}", row.output);
        // without keys/models in the temp HOME this ends in a problem
        assert_eq!(row.ok, Some(false));
        // the run row is in the DB
        assert_eq!(recent_runs(&e.conn, 5).unwrap()[0].task, "deps");
    }

    #[test]
    fn overrides_via_state() {
        let e = env();
        let def = find("deps").unwrap();
        // default: enabled, schedule from registry
        assert!(is_enabled(&e.conn, &def));
        assert_eq!(effective_schedule(&e.conn, &def), "every 24h");
        // disabling and schedule override both take effect
        set_enabled(&e.conn, "deps", false).unwrap();
        assert!(!is_enabled(&e.conn, &def));
        set_schedule(&e.conn, "deps", "daily@06:00").unwrap();
        assert_eq!(effective_schedule(&e.conn, &def), "daily@06:00");
        // an invalid schedule doesn't get saved; a nonexistent task fails
        assert!(set_schedule(&e.conn, "deps", "weekly@1:00").is_err());
        assert_eq!(effective_schedule(&e.conn, &def), "daily@06:00");
        assert!(set_enabled(&e.conn, "neexistuje", true).is_err());
    }

    #[test]
    fn run_due_respects_schedule_enabled_and_global_switch() {
        let e = env();
        // everything on manual → nothing is due
        for def in registry() {
            set_schedule(&e.conn, def.name, "manual").unwrap();
        }
        assert!(run_due(&e.paths, &e.cfg, &e.conn).unwrap().is_empty());
        // purge on daily@00:00 (always past deadline today) → runs once
        set_schedule(&e.conn, "purge-screenshots", "daily@00:00").unwrap();
        let first = run_due(&e.paths, &e.cfg, &e.conn).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].task, "purge-screenshots");
        // a second time the same day, nothing (last run is past today's threshold)
        assert!(run_due(&e.paths, &e.cfg, &e.conn).unwrap().is_empty());
        // a disabled task doesn't run, even if it's due
        e.conn.execute("DELETE FROM task_runs", []).unwrap();
        set_enabled(&e.conn, "purge-screenshots", false).unwrap();
        assert!(run_due(&e.paths, &e.cfg, &e.conn).unwrap().is_empty());
        // global switch [tasks] enabled=false → the scheduler stays quiet
        set_enabled(&e.conn, "purge-screenshots", true).unwrap();
        let mut off = e.cfg.clone();
        off.tasks.enabled = false;
        assert!(run_due(&e.paths, &off, &e.conn).unwrap().is_empty());
    }
}
