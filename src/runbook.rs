//! Phase D: controlled execution of approved automations (runbooks).
//!
//! Lifecycle: pattern (phase B) → proposal in proposals/ (phase C) → **human
//! approval** → runbook → run manually (`runbook run`, voice too), or on a
//! schedule (`runbook run-due` from the 5-min timer; schedule `daily@HH:MM`).
//! Every run is written to `runbook_runs` (read back for status and digest).
//!
//! Security model: approval is a conscious decision — CLI `approve` requires
//! an interactive terminal (neither the agent nor the timer have a TTY);
//! remote approval works only from a verified Telegram chat (see telegram.rs).
//! Voice may only RUN already-approved runbooks; approval is never entrusted
//! to the microphone (anyone in the room could "sign" it on the owner's
//! behalf). The script runs under a hard timeout (SIGKILL to the whole
//! process group) and under a lock — a runbook never runs twice concurrently.

use crate::config::{Config, Paths};
use crate::util;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// An approved runbook (join of runbooks × proposals; artifact = path).
#[derive(Debug, Clone)]
pub struct Runbook {
    pub id: i64,
    pub proposal_id: i64,
    pub pattern_id: Option<i64>,
    pub name: String,
    pub schedule: String,
    pub enabled: bool,
    pub approved_at: i64,
    pub approved_via: String,
    pub kind: String,
    pub path: String,
    /// SHA-256 of the artifact, pinned at approval time; verified before execution.
    /// Empty = runbook approved before the integrity phase (verification skipped).
    pub artifact_sha256: String,
}

/// One run record (a runbook_runs row).
#[derive(Debug, Clone)]
pub struct RunRow {
    pub runbook_id: i64,
    pub name: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// None = killed by timeout (or jarvis crashed mid-run).
    pub exit_code: Option<i64>,
    pub trigger: String,
    pub output: String,
}

impl RunRow {
    pub fn ok(&self) -> bool {
        self.exit_code == Some(0)
    }
}

// ---------- schedule ----------

/// Run schedule: `manual` (run only manually/by voice) or `daily@HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    Manual,
    Daily { hour: u32, min: u32 },
}

pub fn parse_schedule(spec: &str) -> Result<Schedule> {
    let s = spec.trim();
    if s.eq_ignore_ascii_case("manual") {
        return Ok(Schedule::Manual);
    }
    if let Some(t) = s.strip_prefix("daily@") {
        let (h, m) = t
            .split_once(':')
            .with_context(|| format!("plán „{spec}“ — čekám daily@HH:MM"))?;
        let (hour, min): (u32, u32) = (
            h.parse().with_context(|| format!("neplatná hodina v „{spec}“"))?,
            m.parse().with_context(|| format!("neplatná minuta v „{spec}“"))?,
        );
        ensure!(hour <= 23 && min <= 59, "plán „{spec}“ — hodina 0–23, minuta 0–59");
        return Ok(Schedule::Daily { hour, min });
    }
    bail!("neznámý plán „{spec}“ — podporuji `manual` nebo `daily@HH:MM`")
}

/// Unix ts of today's HH:MM in local zone; None inside a DST gap (that day
/// the runbook is simply skipped — better than running at the wrong hour).
fn daily_threshold_ts(now: chrono::DateTime<chrono::Local>, hour: u32, min: u32) -> Option<i64> {
    use chrono::TimeZone as _;
    let t = now.date_naive().and_hms_opt(hour, min, 0)?;
    match chrono::Local.from_local_datetime(&t) {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        chrono::LocalResult::Ambiguous(dt, _) => Some(dt.timestamp()),
        chrono::LocalResult::None => None,
    }
}

/// Is the runbook due? Daily schedule: past today's HH:MM, if today's
/// attempt hasn't run yet (even a failed attempt counts — no retry storms;
/// missed days are naturally caught up by the timer's Persistent=true on
/// the next day).
fn due(
    schedule: Schedule,
    now: chrono::DateTime<chrono::Local>,
    last_started: Option<i64>,
) -> bool {
    match schedule {
        Schedule::Manual => false,
        Schedule::Daily { hour, min } => match daily_threshold_ts(now, hour, min) {
            Some(threshold) => {
                now.timestamp() >= threshold && last_started.is_none_or(|l| l < threshold)
            }
            None => false,
        },
    }
}

// ---------- DB ----------

const RB_COLS: &str = "r.id, r.proposal_id, r.pattern_id, r.name, r.schedule, r.enabled,
                       r.approved_at, r.approved_via, p.kind, p.path, r.artifact_sha256";

fn row_to_runbook(r: &rusqlite::Row) -> rusqlite::Result<Runbook> {
    Ok(Runbook {
        id: r.get(0)?,
        proposal_id: r.get(1)?,
        pattern_id: r.get(2)?,
        name: r.get(3)?,
        schedule: r.get(4)?,
        enabled: r.get::<_, i64>(5)? != 0,
        approved_at: r.get(6)?,
        approved_via: r.get(7)?,
        kind: r.get(8)?,
        path: r.get(9)?,
        artifact_sha256: r.get(10)?,
    })
}

pub fn all(conn: &Connection) -> Result<Vec<Runbook>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RB_COLS} FROM runbooks r JOIN proposals p ON p.id = r.proposal_id
         ORDER BY r.id"
    ))?;
    let rows = stmt.query_map([], row_to_runbook)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Runbook>> {
    conn.query_row(
        &format!(
            "SELECT {RB_COLS} FROM runbooks r JOIN proposals p ON p.id = r.proposal_id
             WHERE r.id = ?1"
        ),
        params![id],
        row_to_runbook,
    )
    .optional()
    .map_err(Into::into)
}

/// Finds a runbook by number, or by part of its name (case-insensitive);
/// multiple matches = error with the list (the voice agent must ask back, not guess).
pub fn resolve(conn: &Connection, query: &str) -> Result<Runbook> {
    if let Ok(id) = query.trim().parse::<i64>() {
        return get(conn, id)?.with_context(|| format!("runbook #{id} neexistuje (viz list)"));
    }
    let q = query.to_lowercase();
    let matches: Vec<Runbook> =
        all(conn)?.into_iter().filter(|r| r.name.to_lowercase().contains(&q)).collect();
    match matches.len() {
        0 => bail!("žádný runbook neodpovídá „{query}“ (viz `jarvis runbook list`)"),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => {
            let names: Vec<String> =
                matches.iter().map(|r| format!("#{} {}", r.id, r.name)).collect();
            bail!("„{query}“ odpovídá víc runbookům: {} — upřesni", names.join(", "))
        }
    }
}

/// Proposals awaiting a decision: have a file, no runbook yet, and the
/// pattern is `proposed`. Returns (proposal_id, kind, path, pattern description).
pub fn pending_proposals(conn: &Connection) -> Result<Vec<(i64, String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.kind, p.path, COALESCE(pat.description, '')
         FROM proposals p
         LEFT JOIN runbooks r ON r.proposal_id = p.id
         LEFT JOIN patterns pat ON pat.id = p.pattern_id
         WHERE r.id IS NULL AND COALESCE(pat.status, 'proposed') = 'proposed'
         ORDER BY p.id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn last_started(conn: &Connection, runbook_id: i64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(started_at) FROM runbook_runs WHERE runbook_id = ?1",
        params![runbook_id],
        |r| r.get::<_, Option<i64>>(0),
    )
    .map_err(Into::into)
}

/// Runs in the interval [from, to) — for the digest.
pub fn runs_between(conn: &Connection, from: i64, to: i64) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(
        "SELECT rr.runbook_id, r.name, rr.started_at, rr.finished_at, rr.exit_code,
                rr.trigger, rr.output
         FROM runbook_runs rr JOIN runbooks r ON r.id = rr.runbook_id
         WHERE rr.started_at >= ?1 AND rr.started_at < ?2
         ORDER BY rr.started_at",
    )?;
    let rows = stmt
        .query_map(params![from, to], row_to_run)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn recent_runs(conn: &Connection, limit: usize) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(
        "SELECT rr.runbook_id, r.name, rr.started_at, rr.finished_at, rr.exit_code,
                rr.trigger, rr.output
         FROM runbook_runs rr JOIN runbooks r ON r.id = rr.runbook_id
         ORDER BY rr.started_at DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], row_to_run)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn row_to_run(r: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        runbook_id: r.get(0)?,
        name: r.get(1)?,
        started_at: r.get(2)?,
        finished_at: r.get(3)?,
        exit_code: r.get(4)?,
        trigger: r.get(5)?,
        output: r.get(6)?,
    })
}

// ---------- approval ----------

/// Runnable artifact kinds. We can run shell scripts; systemd-timer/skill
/// artifacts are installed manually per install_hint — a runbook for them
/// doesn't make sense.
fn runnable(kind: &str, path: &str) -> bool {
    kind == "shell-script" || path.ends_with(".sh")
}

/// Core of approval (no TTY gate here — the CLI calls that; the Telegram
/// path has its own sender verification). Pattern moves to `approved`, or
/// to `automated` if scheduled.
pub fn approve(
    conn: &Connection,
    proposal_id: i64,
    schedule_spec: &str,
    name: Option<&str>,
    via: &str,
) -> Result<Runbook> {
    let schedule = parse_schedule(schedule_spec)?;
    let (pattern_id, kind, path): (Option<i64>, String, String) = conn
        .query_row(
            "SELECT pattern_id, kind, path FROM proposals WHERE id = ?1",
            params![proposal_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .with_context(|| format!("návrh #{proposal_id} neexistuje (viz `jarvis runbook pending`)"))?;
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM runbooks WHERE proposal_id = ?1",
            params![proposal_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        bail!("návrh #{proposal_id} už je schválený jako runbook #{id}");
    }
    ensure!(
        runnable(&kind, &path),
        "návrh #{proposal_id} je druhu „{kind}“ — spouštět umím jen shell skripty; \
         tenhle artefakt nasaď ručně podle install_hint z `jarvis propose`"
    );
    ensure!(
        Path::new(&path).is_file(),
        "artefakt {path} neexistuje — vygeneruj návrh znovu (`jarvis propose`)"
    );
    // set executable bit for good measure (we run via bash, but manual runs benefit)
    let _ = std::fs::set_permissions(&path, {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(0o755)
    });
    // content hash = the core of approval: execution verifies it before running,
    // so after approve the script can no longer be swapped unnoticed (see run_one)
    let artifact_sha256 = util::sha256_hex(
        &std::fs::read(&path).with_context(|| format!("nelze číst artefakt {path}"))?,
    );
    let name = match name {
        Some(n) => n.trim().to_string(),
        None => Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("runbook-{proposal_id}")),
    };
    ensure!(!name.is_empty(), "název runbooku nesmí být prázdný");
    conn.execute(
        "INSERT INTO runbooks(proposal_id, pattern_id, name, schedule, enabled,
                              approved_at, approved_via, artifact_sha256)
         VALUES(?1, ?2, ?3, ?4, 1, ?5, ?6, ?7)",
        params![
            proposal_id,
            pattern_id,
            name,
            schedule_spec.trim(),
            util::now_ts(),
            via,
            artifact_sha256
        ],
    )?;
    let id = conn.last_insert_rowid();
    if let Some(pid) = pattern_id {
        let status = if schedule == Schedule::Manual { "approved" } else { "automated" };
        crate::patterns::set_status(conn, pid, status)?;
    }
    info!("runbook #{id} „{name}“ schválen ({via}), plán {schedule_spec}");
    get(conn, id)?.context("runbook po INSERTu nedohledán")
}

/// Rejects a proposal: pattern → `dismissed`; the file in proposals/ stays.
pub fn dismiss(conn: &Connection, proposal_id: i64) -> Result<String> {
    let (pattern_id, path): (Option<i64>, String) = conn
        .query_row(
            "SELECT pattern_id, path FROM proposals WHERE id = ?1",
            params![proposal_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .with_context(|| format!("návrh #{proposal_id} neexistuje"))?;
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM runbooks WHERE proposal_id = ?1",
            params![proposal_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        bail!("návrh #{proposal_id} už běží jako runbook #{id} — nejdřív `runbook disable {id}`");
    }
    if let Some(pid) = pattern_id {
        crate::patterns::set_status(conn, pid, "dismissed")?;
    }
    Ok(path)
}

/// Re-pins the artifact hash of an approved runbook — after a CONSCIOUS edit
/// of the script (otherwise run_one refuses to run the changed content).
/// Security-equivalent to approve (pinning new content), so the CLI requires
/// keyboard confirmation too.
pub fn repin(conn: &Connection, id: i64) -> Result<Runbook> {
    let rb = get(conn, id)?.with_context(|| format!("runbook #{id} neexistuje"))?;
    ensure!(Path::new(&rb.path).is_file(), "artefakt {} neexistuje", rb.path);
    let hash = util::sha256_hex(
        &std::fs::read(&rb.path).with_context(|| format!("nelze číst artefakt {}", rb.path))?,
    );
    conn.execute(
        "UPDATE runbooks SET artifact_sha256 = ?2 WHERE id = ?1",
        params![id, hash],
    )?;
    info!("runbook #{id} — otisk artefaktu znovu zafixován");
    get(conn, id)?.context("runbook po repinu nedohledán")
}

pub fn set_enabled(conn: &Connection, id: i64, enabled: bool) -> Result<()> {
    let n = conn.execute(
        "UPDATE runbooks SET enabled = ?2 WHERE id = ?1",
        params![id, enabled as i64],
    )?;
    ensure!(n == 1, "runbook #{id} neexistuje");
    Ok(())
}

pub fn set_schedule(conn: &Connection, id: i64, spec: &str) -> Result<Schedule> {
    let schedule = parse_schedule(spec)?;
    let n = conn.execute(
        "UPDATE runbooks SET schedule = ?2 WHERE id = ?1",
        params![id, spec.trim()],
    )?;
    ensure!(n == 1, "runbook #{id} neexistuje");
    Ok(schedule)
}

// ---------- execution ----------

/// The two pipes are read by threads (a full pipe would block the script). We
/// read to EOF but only store the first `cap` bytes — the rest is discarded.
/// If we closed the pipe after `cap` bytes (an earlier `take(cap)`), a chatty
/// script would get EPIPE on its next write and die mid-work just because of
/// output length. The result travels over a channel so reading can be bounded
/// by a timeout: a detached child (setsid, self-daemonized) keeps the pipe
/// open even after killpg, so `read` would never see EOF and a plain `join`
/// would block the whole scheduler.
fn drain(
    stream: Option<impl std::io::Read + Send + 'static>,
    cap: usize,
) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stream {
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if buf.len() < cap {
                            let room = cap - buf.len();
                            buf.extend_from_slice(&chunk[..n.min(room)]);
                        }
                        // keep reading past cap, just discard → producer never sees EPIPE
                    }
                    Err(_) => break,
                }
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// Writes content to a file with 0600 permissions (owner only) — the runnable
/// copy of the runbook, which no one else can tamper with between
/// verification and execution.
fn write_private(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(content)?;
    Ok(())
}

/// Deletes the temp file on drop (the runnable runbook copy, once the run is
/// over, regardless of which path the function returns through).
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs a runbook: `bash <artifact>` in its own process group, with a hard
/// timeout (SIGKILL to the whole group) and an flock lock against concurrent
/// runs. The run row is written right at the start (NULL finished_at = still
/// running; "didn't finish" if jarvis crashes) and completed at the end.
pub fn run_one(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    rb: &Runbook,
    trigger: &str,
) -> Result<RunRow> {
    ensure!(rb.enabled, "runbook #{} „{}“ je vypnutý (`runbook enable {}`)", rb.id, rb.name, rb.id);
    ensure!(
        Path::new(&rb.path).is_file(),
        "artefakt {} zmizel — zamítni runbook a vygeneruj návrh znovu",
        rb.path
    );
    // Read the artifact into memory ONCE, and verify the hash and (below) run
    // it from those same bytes via a private copy. Otherwise bash would open
    // the file a second time, independent of our hash → the window between
    // verification and execution could be used to swap the content (TOCTOU)
    // and bypass the pinned hash.
    let content =
        std::fs::read(&rb.path).with_context(|| format!("nelze číst artefakt {}", rb.path))?;
    // integrity: content must match the hash pinned at approval. Empty hash =
    // runbook from before the integrity phase → verification isn't possible, just warn.
    if rb.artifact_sha256.is_empty() {
        warn!(
            "runbook #{} nemá zafixovaný otisk (schválen před fází integrity) — \
             spouštím bez ověření; přepni přes `jarvis runbook repin {}`",
            rb.id, rb.id
        );
    } else {
        let now_hash = util::sha256_hex(&content);
        ensure!(
            now_hash == rb.artifact_sha256,
            "artefakt {} se od schválení změnil (otisk nesedí) — z bezpečnosti NESPOUŠTÍM. \
             Pokud je změna záměrná, znovu zafixuj: `jarvis runbook repin {}`",
            rb.path,
            rb.id
        );
    }
    // per-runbook lock: a second run of the same runbook doesn't wait — it's
    // rejected outright (the timer comes back in 5 minutes; voice can report it)
    let lock_path = paths.data_dir.join(format!("runbook-{}.lock", rb.id));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("nelze otevřít {}", lock_path.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("runbook #{} „{}“ právě běží — nespouštím podruhé", rb.id, rb.name);
    }

    let started = util::now_ts();
    info!("runbook #{} „{}“ startuje ({trigger})", rb.id, rb.name);

    // run a verified COPY (0600 in our data_dir, only we write to it): the hash
    // and the execution both come from the same bytes, so the inode can't be
    // swapped in between. The RAII guard deletes the copy once the run is done
    // (even on any error path).
    let run_path = paths.data_dir.join(format!("runbook-{}.run.sh", rb.id));
    write_private(&run_path, &content)
        .with_context(|| format!("nelze připravit spouštěnou kopii {}", run_path.display()))?;
    let _run_guard = TempFile(run_path.clone());

    use std::os::unix::process::CommandExt;
    // spawn the process first: if spawn fails, nothing ran → no phantom run row
    let mut child = std::process::Command::new("bash")
        .arg(&run_path)
        .current_dir(&paths.data_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("nelze spustit bash {}", run_path.display()))?;

    // only now write the run row (NULL finished_at = running; "didn't finish"
    // if jarvis crashes); if the write fails, clean up the child so it doesn't
    // become an orphan or leak a thread
    let run_id = match conn.execute(
        "INSERT INTO runbook_runs(runbook_id, started_at, trigger) VALUES(?1, ?2, ?3)",
        params![rb.id, started, trigger],
    ) {
        Ok(_) => conn.last_insert_rowid(),
        Err(e) => {
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
            let _ = child.wait();
            return Err(e).context("zápis řádku běhu runbooku selhal");
        }
    };

    // pipe cap: comfortably above max_output_chars (UTF-8 up to 4 B/char plus
    // slack), so truncation is still governed by the character limit, but memory stays bounded
    let cap = (cfg.runbooks.max_output_chars as usize)
        .saturating_mul(4)
        .saturating_add(4096);
    let out = drain(child.stdout.take(), cap);
    let err = drain(child.stderr.take(), cap);

    let timeout = Duration::from_secs(cfg.runbooks.timeout_s);
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut wait_err: Option<String> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(e) => {
                // a wait error must not leave the process running or threads hanging
                wait_err = Some(e.to_string());
                unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            // whole group — the script may have spawned children
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(300));
    };

    // bounded join: once the process ends, give the drain a short grace period.
    // If it doesn't arrive (a detached child keeps the pipe open even after
    // killpg), abandon the thread and move on — better to lose some output
    // than to hang the whole scheduler.
    let drain_grace = Duration::from_secs(5);
    let stdout = out.recv_timeout(drain_grace).unwrap_or_default();
    let stderr = err.recv_timeout(drain_grace).unwrap_or_default();
    let mut output = stdout;
    if !stderr.trim().is_empty() {
        output.push_str("\n[stderr]\n");
        output.push_str(&stderr);
    }
    if timed_out {
        output.push_str(&format!(
            "\n[jarvis] zabit po timeoutu {} s (runbooks.timeout_s)",
            cfg.runbooks.timeout_s
        ));
    }
    if let Some(e) = &wait_err {
        output.push_str(&format!("\n[jarvis] chyba při čekání na proces: {e}"));
    }
    let output = util::truncate_chars(output.trim(), cfg.runbooks.max_output_chars);
    let exit_code: Option<i64> = status.and_then(|st| st.code().map(i64::from));
    let finished = util::now_ts();
    conn.execute(
        "UPDATE runbook_runs SET finished_at = ?2, exit_code = ?3, output = ?4 WHERE id = ?1",
        params![run_id, finished, exit_code, output],
    )?;
    let row = RunRow {
        runbook_id: rb.id,
        name: rb.name.clone(),
        started_at: started,
        finished_at: Some(finished),
        exit_code,
        trigger: trigger.into(),
        output,
    };
    match exit_code {
        Some(0) => info!("runbook #{} doběhl OK ({} s)", rb.id, finished - started),
        Some(c) => warn!("runbook #{} skončil s kódem {c}", rb.id),
        None => warn!("runbook #{} zabit (timeout {} s)", rb.id, cfg.runbooks.timeout_s),
    }
    Ok(row)
}

/// Walks the enabled scheduled runbooks and runs the ones that are due.
/// One failure must not stop the others; returns the completed runs.
pub fn run_due(paths: &Paths, cfg: &Config, conn: &Connection) -> Result<Vec<RunRow>> {
    if !cfg.runbooks.enabled {
        return Ok(Vec::new());
    }
    let now = chrono::Local::now();
    let mut results = Vec::new();
    for rb in all(conn)? {
        if !rb.enabled {
            continue;
        }
        let schedule = match parse_schedule(&rb.schedule) {
            Ok(s) => s,
            Err(e) => {
                warn!("runbook #{} má neplatný plán „{}“: {e:#}", rb.id, rb.schedule);
                continue;
            }
        };
        if !due(schedule, now, last_started(conn, rb.id)?) {
            continue;
        }
        match run_one(paths, cfg, conn, &rb, "timer") {
            Ok(row) => results.push(row),
            Err(e) => warn!("plánovaný runbook #{} selhal: {e:#}", rb.id),
        }
    }
    Ok(results)
}

/// Announces a new proposal on remote-approval channels (Telegram, SMS).
/// Best effort: a failed announcement must not derail proposal generation —
/// the proposal is always visible in the digest and in `jarvis runbook pending`.
pub fn announce_proposal(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    proposal_id: i64,
    kind: &str,
    desc: &str,
) {
    crate::telegram::notify_proposal(paths, cfg, proposal_id, kind, desc);
    if !(cfg.runbooks.notify_sms && cfg.sms.enabled) {
        return;
    }
    let text = format!(
        "Jarvis: novy navrh automatizace #{proposal_id} [{kind}]: {}. Schvaleni: \
         jarvis runbook approve {proposal_id} (nebo Telegram).",
        util::truncate_chars(desc, 200)
    );
    let (sid, token) = match crate::config::twilio_keys(paths) {
        Ok(k) => k,
        Err(e) => {
            warn!("SMS ohláška návrhu: chybí Twilio klíče: {e:#}");
            return;
        }
    };
    match crate::sms::send(&cfg.sms, &sid, &token, &cfg.sms.to, &text) {
        Ok(msg_sid) => {
            info!("SMS ohláška návrhu #{proposal_id} odeslána ({msg_sid})");
            let chars = text.chars().count() as i64;
            if let Err(e) =
                crate::store::db::insert_cost(conn, util::now_ts(), "sms", "twilio", chars, 0, 0.0)
            {
                warn!("zápis útraty SMS selhal: {e:#}");
            }
        }
        Err(e) => warn!("SMS ohláška návrhu #{proposal_id} selhala: {e:#}"),
    }
}

/// One scheduler tick — shared by the systemd timer (`runbook run-due`) and
/// the built-in scheduler in `jarvis run`. Logs errors and keeps going;
/// returns the completed runs (the CLI prints them).
pub fn tick(paths: &Paths, cfg: &Config, conn: &Connection) -> Vec<RunRow> {
    crate::telegram::process_approvals(paths, cfg, conn);
    match run_due(paths, cfg, conn) {
        Ok(rows) => {
            if !rows.is_empty() {
                info!("plánovač: dokončeno {} běh(ů)", rows.len());
            }
            rows
        }
        Err(e) => {
            warn!("plánovač runbooků selhal: {e:#}");
            Vec::new()
        }
    }
}

// ---------- CLI ----------

#[derive(clap::Subcommand)]
pub enum RunbookCmd {
    /// Proposals awaiting approval
    Pending,
    /// Approved runbooks
    List,
    /// Runbook detail: metadata, start of the artifact, recent runs
    Show { runbook: String },
    /// Approves a proposal → runbook (requires a terminal; schedule via --schedule daily@HH:MM)
    Approve {
        proposal_id: i64,
        /// manual = only manually/by voice; daily@HH:MM = daily at this time
        #[arg(long, default_value = "manual")]
        schedule: String,
        /// Human-readable name (default: artifact file name)
        #[arg(long)]
        name: Option<String>,
    },
    /// Rejects a proposal (pattern → dismissed; file on disk stays)
    Dismiss { proposal_id: i64 },
    /// Re-pins the artifact hash after a CONSCIOUS edit of an approved script
    Repin { runbook: String },
    /// Runs a runbook now (number or part of the name)
    Run {
        runbook: String,
        /// Run source, for the record (cli|voice)
        #[arg(long, default_value = "cli", hide = true)]
        trigger: String,
    },
    /// Runs runbooks that are due per schedule (called by the 5-min timer)
    RunDue,
    /// Run history
    Runs {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Enables a runbook
    Enable { runbook: String },
    /// Disables a runbook (both schedule and manual runs)
    Disable { runbook: String },
    /// Changes a runbook's schedule (manual | daily@HH:MM)
    Schedule { runbook: String, spec: String },
}

/// Conscious confirmation at the keyboard. `is_terminal()` alone is
/// inheritable ambient authority (an agent's pty or a misconfigured unit
/// would inherit it), so we additionally require the human to type back the
/// expected token. Automated paths (timer, voice, stdin=null) can't supply
/// it and so can't approve execution.
pub(crate) fn confirm_at_keyboard(prompt: &str, expect: &str) -> Result<()> {
    use std::io::{IsTerminal, Write};
    ensure!(
        std::io::stdin().is_terminal(),
        "tohle je vědomé rozhodnutí u klávesnice — spusť z terminálu (hlasový \
         agent ani timer nesmí); na dálku jde schválit z ověřeného Telegramu \
         ([runbooks] telegram_approve)"
    );
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("nelze přečíst potvrzení")?;
    ensure!(line.trim() == expect, "potvrzení nesedí — zrušeno");
    Ok(())
}

fn fmt_run(r: &RunRow) -> String {
    let state = match (r.finished_at, r.exit_code) {
        (None, _) => "běží/nedoběhl".into(),
        (Some(_), Some(0)) => "✓ OK".into(),
        (Some(_), Some(c)) => format!("✗ exit {c}"),
        (Some(_), None) => "✗ timeout".into(),
    };
    format!(
        "{}  #{:<3} {:<24} {:<9} {}",
        util::fmt_local(r.started_at),
        r.runbook_id,
        util::truncate_chars(&r.name, 24),
        r.trigger,
        state
    )
}

pub fn cli(paths: &Paths, cfg: &Config, conn: &Connection, cmd: RunbookCmd) -> Result<()> {
    match cmd {
        RunbookCmd::Pending => {
            let pend = pending_proposals(conn)?;
            if pend.is_empty() {
                println!(
                    "Žádné návrhy nečekají. Nové vygeneruje `jarvis propose` \
                     z detekovaných vzorů."
                );
                return Ok(());
            }
            for (id, kind, path, desc) in pend {
                println!("#{id}  [{kind}]  {}", util::truncate_chars(&desc, 80));
                println!("     {path}");
                println!("     schválit: jarvis runbook approve {id} [--schedule daily@HH:MM]");
            }
            Ok(())
        }
        RunbookCmd::List => {
            let rbs = all(conn)?;
            if rbs.is_empty() {
                println!("Žádné schválené runbooky (kandidáty ukáže `jarvis runbook pending`).");
                return Ok(());
            }
            println!("{:>4}  {:<24} {:<14} {:<8} poslední běh", "id", "název", "plán", "stav");
            for rb in rbs {
                let last = last_started(conn, rb.id)?
                    .map(util::fmt_local)
                    .unwrap_or_else(|| "—".into());
                println!(
                    "{:>4}  {:<24} {:<14} {:<8} {last}",
                    rb.id,
                    util::truncate_chars(&rb.name, 24),
                    rb.schedule,
                    if rb.enabled { "zapnutý" } else { "vypnutý" },
                );
            }
            Ok(())
        }
        RunbookCmd::Show { runbook } => {
            let rb = resolve(conn, &runbook)?;
            println!("#{} „{}“", rb.id, rb.name);
            println!(
                "  původ:     návrh #{}{}",
                rb.proposal_id,
                rb.pattern_id.map(|p| format!(" (vzor #{p})")).unwrap_or_default()
            );
            println!("  druh:      {}", rb.kind);
            println!("  artefakt:  {}", rb.path);
            println!("  plán:      {}", rb.schedule);
            println!("  stav:      {}", if rb.enabled { "zapnutý" } else { "vypnutý" });
            println!(
                "  schválen:  {} ({})",
                util::fmt_local(rb.approved_at),
                rb.approved_via
            );
            match std::fs::read_to_string(&rb.path) {
                Ok(content) => {
                    println!("  ── artefakt (začátek) ──");
                    for line in content.lines().take(40) {
                        println!("  {line}");
                    }
                    if content.lines().count() > 40 {
                        println!("  … (celý soubor: {})", rb.path);
                    }
                }
                Err(e) => println!("  ⚠ artefakt nejde přečíst: {e}"),
            }
            let runs = recent_runs(conn, 50)?;
            let mine: Vec<&RunRow> = runs.iter().filter(|r| r.runbook_id == rb.id).take(5).collect();
            if !mine.is_empty() {
                println!("  ── poslední běhy ──");
                for r in mine {
                    println!("  {}", fmt_run(r));
                }
            }
            Ok(())
        }
        RunbookCmd::Approve { proposal_id, schedule, name } => {
            confirm_at_keyboard(
                &format!(
                    "Schválit návrh #{proposal_id} k automatické exekuci? \
                     Opiš číslo návrhu pro potvrzení: "
                ),
                &proposal_id.to_string(),
            )?;
            let rb = approve(conn, proposal_id, &schedule, name.as_deref(), "cli")?;
            println!("✓ runbook #{} „{}“ schválen (plán {})", rb.id, rb.name, rb.schedule);
            if parse_schedule(&rb.schedule)? == Schedule::Manual {
                println!("  spustíš: jarvis runbook run {} (funguje i hlasem)", rb.id);
            } else {
                println!("  poběží automaticky; hned teď: jarvis runbook run {}", rb.id);
            }
            Ok(())
        }
        RunbookCmd::Dismiss { proposal_id } => {
            let path = dismiss(conn, proposal_id)?;
            println!("✓ návrh #{proposal_id} zamítnut (soubor zůstává: {path})");
            Ok(())
        }
        RunbookCmd::Repin { runbook } => {
            let rb = resolve(conn, &runbook)?;
            confirm_at_keyboard(
                &format!(
                    "Zafixovat otisk runbooku #{} „{}“ na AKTUÁLNÍ obsah {}? \
                     Opiš číslo runbooku pro potvrzení: ",
                    rb.id, rb.name, rb.path
                ),
                &rb.id.to_string(),
            )?;
            let rb = repin(conn, rb.id)?;
            let short: String = rb.artifact_sha256.chars().take(12).collect();
            println!("✓ runbook #{} „{}“ — otisk zafixován ({short}…)", rb.id, rb.name);
            Ok(())
        }
        RunbookCmd::Run { runbook, trigger } => {
            let trigger = if trigger == "voice" { "voice" } else { "cli" };
            let rb = resolve(conn, &runbook)?;
            let row = run_one(paths, cfg, conn, &rb, trigger)?;
            println!("{}", fmt_run(&row));
            if !row.output.is_empty() {
                println!("── výstup ──");
                println!("{}", row.output);
            }
            if !row.ok() {
                bail!("runbook „{}“ nedoběhl úspěšně", rb.name);
            }
            Ok(())
        }
        RunbookCmd::RunDue => {
            let rows = tick(paths, cfg, conn);
            if rows.is_empty() {
                println!("Nic není na řadě.");
            }
            for r in &rows {
                println!("{}", fmt_run(r));
            }
            Ok(())
        }
        RunbookCmd::Runs { limit } => {
            let runs = recent_runs(conn, limit)?;
            if runs.is_empty() {
                println!("Zatím žádné běhy.");
            }
            for r in runs.iter().rev() {
                println!("{}", fmt_run(r));
            }
            Ok(())
        }
        RunbookCmd::Enable { runbook } => {
            let rb = resolve(conn, &runbook)?;
            set_enabled(conn, rb.id, true)?;
            println!("✓ runbook #{} „{}“ zapnutý", rb.id, rb.name);
            Ok(())
        }
        RunbookCmd::Disable { runbook } => {
            let rb = resolve(conn, &runbook)?;
            set_enabled(conn, rb.id, false)?;
            println!("✓ runbook #{} „{}“ vypnutý", rb.id, rb.name);
            Ok(())
        }
        RunbookCmd::Schedule { runbook, spec } => {
            let rb = resolve(conn, &runbook)?;
            set_schedule(conn, rb.id, &spec)?;
            println!("✓ runbook #{} „{}“ — plán {}", rb.id, rb.name, spec.trim());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::db;

    /// Isolated environment: a real tempdir (flock + scripts need disk),
    /// in-memory DB with the schema.
    struct Env {
        paths: Paths,
        cfg: Config,
        conn: Connection,
        _tmp: TempDir,
    }

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn env() -> Env {
        let base = std::env::temp_dir().join(format!(
            "jarvis-runbook-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let proposals = base.join("proposals");
        std::fs::create_dir_all(&proposals).unwrap();
        let paths = Paths {
            config_dir: base.join("cfg"),
            config_file: base.join("cfg/config.toml"),
            secrets_file: base.join("cfg/secrets.env"),
            data_dir: base.clone(),
            shots_dir: base.join("shots"),
            proposals_dir: proposals,
            models_dir: base.join("models"),
            tts_cache_dir: base.join("tts"),
            db_path: base.join("jarvis.db"),
        };
        Env { paths, cfg: Config::default(), conn: db::test_conn(), _tmp: TempDir(base) }
    }

    /// Inserts a pattern + proposal with a script of the given content; returns proposal_id.
    fn seed_proposal(e: &Env, script: &str) -> i64 {
        crate::patterns::record_hints(&e.conn, &["opakovaný ruční krok X".into()]).unwrap();
        let pattern_id = e.conn.last_insert_rowid();
        let path = e.paths.proposals_dir.join(format!("{pattern_id}-test.sh"));
        std::fs::write(&path, script).unwrap();
        e.conn
            .execute(
                "INSERT INTO proposals(pattern_id, kind, path, created_at) VALUES(?1,?2,?3,?4)",
                params![pattern_id, "shell-script", path.display().to_string(), 1],
            )
            .unwrap();
        e.conn.last_insert_rowid()
    }

    #[test]
    fn schedule_parsing() {
        assert_eq!(parse_schedule("manual").unwrap(), Schedule::Manual);
        assert_eq!(parse_schedule(" MANUAL ").unwrap(), Schedule::Manual);
        assert_eq!(
            parse_schedule("daily@07:30").unwrap(),
            Schedule::Daily { hour: 7, min: 30 }
        );
        assert_eq!(parse_schedule("daily@0:5").unwrap(), Schedule::Daily { hour: 0, min: 5 });
        assert!(parse_schedule("daily@25:00").is_err());
        assert!(parse_schedule("daily@10:60").is_err());
        assert!(parse_schedule("daily@").is_err());
        assert!(parse_schedule("weekly@10:00").is_err());
        assert!(parse_schedule("").is_err());
    }

    #[test]
    fn due_logic_daily() {
        use chrono::TimeZone as _;
        let now = chrono::Local.with_ymd_and_hms(2026, 7, 17, 12, 0, 0).unwrap();
        let sched = Schedule::Daily { hour: 9, min: 0 };
        let nine = daily_threshold_ts(now, 9, 0).unwrap();
        // past deadline, never ran → due
        assert!(due(sched, now, None));
        // today's run (even an attempt) already happened → not due
        assert!(!due(sched, now, Some(nine + 60)));
        // yesterday's run → due
        assert!(due(sched, now, Some(nine - 86_400)));
        // before today's deadline → not due
        assert!(!due(Schedule::Daily { hour: 14, min: 0 }, now, None));
        // manual is never due
        assert!(!due(Schedule::Manual, now, Some(0)));
        assert!(!due(Schedule::Manual, now, None));
    }

    #[test]
    fn approve_creates_runbook_and_flips_pattern() {
        let e = env();
        let pid = seed_proposal(&e, "#!/bin/bash\necho ok\n");
        let rb = approve(&e.conn, pid, "manual", None, "test").unwrap();
        assert_eq!(rb.proposal_id, pid);
        assert!(rb.enabled);
        assert!(rb.name.contains("test"));
        let pat = crate::patterns::get(&e.conn, rb.pattern_id.unwrap()).unwrap().unwrap();
        assert_eq!(pat.status, "approved");
        // approving twice fails
        assert!(approve(&e.conn, pid, "manual", None, "test").is_err());
        // pending is now empty (pattern is not proposed and the runbook exists)
        assert!(pending_proposals(&e.conn).unwrap().is_empty());
    }

    #[test]
    fn approve_with_schedule_marks_automated() {
        let e = env();
        let pid = seed_proposal(&e, "echo ok\n");
        let rb = approve(&e.conn, pid, "daily@06:00", Some("ranní sync"), "test").unwrap();
        assert_eq!(rb.name, "ranní sync");
        assert_eq!(rb.schedule, "daily@06:00");
        let pat = crate::patterns::get(&e.conn, rb.pattern_id.unwrap()).unwrap().unwrap();
        assert_eq!(pat.status, "automated");
    }

    #[test]
    fn approve_rejects_missing_or_unrunnable() {
        let e = env();
        assert!(approve(&e.conn, 999, "manual", None, "test").is_err());
        // nonexistent file
        e.conn
            .execute(
                "INSERT INTO proposals(pattern_id, kind, path, created_at)
                 VALUES(NULL, 'shell-script', '/nonexistent/x.sh', 1)",
                [],
            )
            .unwrap();
        let missing = e.conn.last_insert_rowid();
        assert!(approve(&e.conn, missing, "manual", None, "test").is_err());
        // non-runnable kind
        e.conn
            .execute(
                "INSERT INTO proposals(pattern_id, kind, path, created_at)
                 VALUES(NULL, 'claude-skill', '/tmp/skill.md', 1)",
                [],
            )
            .unwrap();
        let skill = e.conn.last_insert_rowid();
        let err = approve(&e.conn, skill, "manual", None, "test").unwrap_err();
        assert!(err.to_string().contains("shell skripty"), "{err}");
        // invalid schedule
        let pid = seed_proposal(&e, "echo ok\n");
        assert!(approve(&e.conn, pid, "daily@99:00", None, "test").is_err());
    }

    #[test]
    fn run_one_captures_output_and_exit() {
        let e = env();
        let pid = seed_proposal(&e, "echo AHOJ; echo CHYBA >&2; exit 0\n");
        let rb = approve(&e.conn, pid, "manual", None, "test").unwrap();
        let row = run_one(&e.paths, &e.cfg, &e.conn, &rb, "cli").unwrap();
        assert_eq!(row.exit_code, Some(0));
        assert!(row.ok());
        assert!(row.output.contains("AHOJ"));
        assert!(row.output.contains("[stderr]"));
        assert!(row.output.contains("CHYBA"));
        // read-back from DB
        let runs = recent_runs(&e.conn, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].exit_code, Some(0));
        assert!(runs[0].finished_at.is_some());
    }

    #[test]
    fn run_one_refuses_tampered_artifact_until_repin() {
        let e = env();
        let pid = seed_proposal(&e, "echo original\n");
        let rb = approve(&e.conn, pid, "manual", None, "test").unwrap();
        // tamper with the content after approval (harmless text — runs after repin in the test)
        std::fs::write(&rb.path, "echo TAMPERED\n").unwrap();
        let rb = get(&e.conn, rb.id).unwrap().unwrap();
        let err = run_one(&e.paths, &e.cfg, &e.conn, &rb, "cli").unwrap_err();
        assert!(err.to_string().contains("otisk nesedí"), "{err}");
        // no run was recorded (rejected before execution)
        assert!(recent_runs(&e.conn, 10).unwrap().is_empty());
        // repin pins the new content → now it runs
        repin(&e.conn, rb.id).unwrap();
        let rb = get(&e.conn, rb.id).unwrap().unwrap();
        let row = run_one(&e.paths, &e.cfg, &e.conn, &rb, "cli").unwrap();
        assert!(row.output.contains("TAMPERED"));
    }

    #[test]
    fn approve_pins_artifact_hash() {
        let e = env();
        let pid = seed_proposal(&e, "echo ok\n");
        let rb = approve(&e.conn, pid, "manual", None, "test").unwrap();
        assert_eq!(rb.artifact_sha256, crate::util::sha256_hex(b"echo ok\n"));
    }

    #[test]
    fn run_one_records_failure_exit() {
        let e = env();
        let pid = seed_proposal(&e, "echo spadl jsem; exit 3\n");
        let rb = approve(&e.conn, pid, "manual", None, "test").unwrap();
        let row = run_one(&e.paths, &e.cfg, &e.conn, &rb, "cli").unwrap();
        assert_eq!(row.exit_code, Some(3));
        assert!(!row.ok());
    }

    #[test]
    fn run_one_kills_on_timeout() {
        let e = env();
        let mut cfg = e.cfg.clone();
        cfg.runbooks.timeout_s = 1; // below validation's minimum — validate() doesn't run here
        let pid = seed_proposal(&e, "sleep 300\n");
        let rb = approve(&e.conn, pid, "manual", None, "test").unwrap();
        let started = std::time::Instant::now();
        let row = run_one(&e.paths, &cfg, &e.conn, &rb, "cli").unwrap();
        assert!(started.elapsed() < Duration::from_secs(8), "timeout nezabral včas");
        assert_eq!(row.exit_code, None);
        assert!(row.output.contains("timeout"));
        assert!(!row.ok());
    }

    #[test]
    fn run_one_refuses_disabled() {
        let e = env();
        let pid = seed_proposal(&e, "echo ok\n");
        let rb = approve(&e.conn, pid, "manual", None, "test").unwrap();
        set_enabled(&e.conn, rb.id, false).unwrap();
        let rb = get(&e.conn, rb.id).unwrap().unwrap();
        assert!(run_one(&e.paths, &e.cfg, &e.conn, &rb, "cli").is_err());
    }

    #[test]
    fn run_due_runs_once_per_day() {
        let e = env();
        let pid = seed_proposal(&e, "echo planovany\n");
        // daily@00:00 is always already past deadline today
        approve(&e.conn, pid, "daily@00:00", None, "test").unwrap();
        let first = run_due(&e.paths, &e.cfg, &e.conn).unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].ok());
        // nothing a second time today
        let second = run_due(&e.paths, &e.cfg, &e.conn).unwrap();
        assert!(second.is_empty());
        // disabled runbooks → nothing, even if due
        let mut off = e.cfg.clone();
        off.runbooks.enabled = false;
        e.conn.execute("DELETE FROM runbook_runs", []).unwrap();
        assert!(run_due(&e.paths, &off, &e.conn).unwrap().is_empty());
    }

    #[test]
    fn resolve_by_id_name_and_ambiguity() {
        let e = env();
        let p1 = seed_proposal(&e, "echo a\n");
        approve(&e.conn, p1, "manual", Some("zaloha fotek"), "test").unwrap();
        let p2 = seed_proposal(&e, "echo b\n");
        approve(&e.conn, p2, "manual", Some("zaloha mailu"), "test").unwrap();
        assert_eq!(resolve(&e.conn, "1").unwrap().name, "zaloha fotek");
        assert_eq!(resolve(&e.conn, "MAILU").unwrap().name, "zaloha mailu");
        assert!(resolve(&e.conn, "zaloha").is_err()); // multiple matches
        assert!(resolve(&e.conn, "neexistuje").is_err());
        assert!(resolve(&e.conn, "99").is_err());
    }

    #[test]
    fn dismiss_flips_pattern_and_blocks_approved() {
        let e = env();
        let pid = seed_proposal(&e, "echo ok\n");
        dismiss(&e.conn, pid).unwrap();
        let pats = crate::patterns::all(&e.conn).unwrap();
        assert_eq!(pats[0].status, "dismissed");
        assert!(pending_proposals(&e.conn).unwrap().is_empty());
        // cannot dismiss an already-approved proposal
        let pid2 = seed_proposal(&e, "echo ok\n");
        approve(&e.conn, pid2, "manual", None, "test").unwrap();
        assert!(dismiss(&e.conn, pid2).is_err());
    }

    #[test]
    fn pending_lists_unhandled_proposals() {
        let e = env();
        let pid = seed_proposal(&e, "echo ok\n");
        // the propose flow sets the pattern status to proposed
        let pat_id: i64 = e
            .conn
            .query_row("SELECT pattern_id FROM proposals WHERE id=?1", params![pid], |r| {
                r.get(0)
            })
            .unwrap();
        crate::patterns::set_status(&e.conn, pat_id, "proposed").unwrap();
        let pend = pending_proposals(&e.conn).unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].0, pid);
        assert!(pend[0].3.contains("opakovaný"));
    }
}
