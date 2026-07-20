//! Self-improvement layer — Jarvis develops and improves its OWN source code,
//! and tracks every change in git. The reasoning engine already exists
//! (`pipeline::claude` is headless Claude Code); this layer wraps it in gates
//! so a program that can rewrite itself never does so unsafely.
//!
//! **Safety invariant** (the whole point of this module): improve NEVER lands
//! or deploys anything unapproved. Autonomously it may only:
//!   - draft a change on an ISOLATED git branch (never `main`, never the
//!     running binary) — fully reversible, touches nothing live,
//!   - run the test suite on that branch (`cargo test` — a check that can fail).
//! Merging to `main` requires human approval (TTY typed-token, same gate as
//! `runbook approve`, or a verified Telegram numeric confirm). The exact diff's
//! sha256 is pinned at approval and re-verified before the merge (TOCTOU-safe,
//! the `runbook::run_one` model). Rebuilding + restarting the live binary is a
//! further, separately-gated step (`deploy_enabled`) with a smoke-test and
//! automatic rollback. The layer ships dark (`enabled = false`).
//!
//! **Why green tests aren't enough**: an agent could make tests pass by
//! weakening them, or could edit its own gates. Two structural guards, applied
//! before anything is offered: the test-integrity check (test count may only
//! grow) and the safety-critical-path classifier (a diff touching gate code is
//! always escalated to manual review, even under auto-merge).
//!
//! Layout mirrors `nudge.rs`: pure, unit-tested logic (envelope classification,
//! guards, branch naming, confirm parsing) with a thin DB/git/API shell on top.
//!
//! NOTE: this module is built phase by phase. Phase 1 lands the full data model
//! and pure logic (with tests); the lifecycle shell that consumes them lands in
//! phases 2–6. The scaffold allow below is removed once the shell is complete.

#![allow(dead_code)]

use crate::config::{Config, ImproveCfg, Paths};
use crate::pipeline::claude;
use crate::store::db;
use crate::util;
use anyhow::{bail, ensure, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tracing::info;

/// Tools handed to the codegen agent inside the worktree: it may read, edit,
/// write, and build/test — but NOT run git (Jarvis owns every git operation and
/// the gates) and NOT `cargo add`/`install`/`publish` (deps are gate-critical).
const DRAFT_TOOLS: &str = "Read,Edit,Write,Bash(cargo build:*),Bash(cargo test:*),\
    Bash(cargo check:*),Bash(cargo fmt:*),Bash(cargo clippy:*)";

// improvement sources (= improvements.source column)
pub const SRC_DIRECTED: &str = "directed"; // "Jarvisi, teach yourself X"
pub const SRC_FAILING_TEST: &str = "failing_test";
pub const SRC_CLIPPY: &str = "clippy";
pub const SRC_PLAN_ITEM: &str = "plan_item";
pub const SRC_RUNBOOK_FIX: &str = "runbook_fix";

// lifecycle statuses (= improvements.status column)
pub const STATUS_QUEUED: &str = "queued"; // task accepted, not drafted yet
pub const STATUS_DRAFTING: &str = "drafting"; // codegen running on a branch
pub const STATUS_TESTED: &str = "tested"; // branch built + tested green
pub const STATUS_PROPOSED: &str = "proposed"; // diff pinned, awaiting approval
pub const STATUS_APPROVED: &str = "approved"; // human approved, pre-merge
pub const STATUS_MERGED: &str = "merged"; // landed on main
pub const STATUS_DEPLOYED: &str = "deployed"; // live binary rebuilt + restarted
pub const STATUS_FAILED: &str = "failed"; // tests red / codegen error
pub const STATUS_DISMISSED: &str = "dismissed"; // abandoned by human
pub const STATUS_ROLLED_BACK: &str = "rolled_back"; // deploy smoke failed, reverted

// change envelope (from diff path classification)
pub const ENV_SAFE: &str = "safe"; // docs-only — eligible for auto-merge (if enabled)
pub const ENV_FEATURE: &str = "feature"; // ordinary code — always needs approval
pub const ENV_GATE_CRITICAL: &str = "gate_critical"; // touches safety/gate code — never auto-merges

/// Repo-relative paths whose modification is ALWAYS escalated to manual review,
/// even under `auto_merge_safe`. These define the gates, the build, and the
/// dependency set — the pieces a self-editing agent must never quietly change.
const SAFETY_CRITICAL: &[&str] = &[
    "src/config.rs",      // gate defaults + validation
    "src/runbook.rs",     // confirm_at_keyboard, pin-and-verify, execution
    "src/improve.rs",     // this module — the self-improvement gates themselves
    "src/units.rs",       // systemd units + deploy wiring
    "src/main.rs",        // command dispatch + SIGPIPE handling
    "src/telegram.rs",    // remote approval channel
    "src/kill.rs",        // stop/kill
    "Cargo.toml",         // dependencies = supply chain
    "Cargo.lock",         //   "
    ".cargo/config.toml", // build wiring / rpath
];

// ---------- pure logic (unit-tested) ----------

/// Does this changed-path set touch any safety-critical file? Comparison is on
/// the normalized repo-relative path (leading "./" stripped).
pub fn touches_safety_critical(changed: &[String]) -> bool {
    changed.iter().any(|p| {
        let p = p.trim().trim_start_matches("./");
        SAFETY_CRITICAL.contains(&p) || p.starts_with(".github/")
    })
}

/// Classifies a change by its touched paths into an approval envelope.
/// Empty (nothing changed, or paths unknown) → feature = the safe default
/// (needs review). Any critical path → gate_critical. All-docs → safe.
pub fn classify_envelope(changed: &[String]) -> &'static str {
    if changed.is_empty() {
        return ENV_FEATURE;
    }
    if touches_safety_critical(changed) {
        return ENV_GATE_CRITICAL;
    }
    let all_docs = changed
        .iter()
        .all(|p| p.trim().to_ascii_lowercase().ends_with(".md"));
    if all_docs {
        ENV_SAFE
    } else {
        ENV_FEATURE
    }
}

/// Test-integrity guard against gamed-green tests: the branch must not have
/// FEWER tests than the base. Tests may be added, never removed to make things
/// pass. (Phase 3 additionally diffs assertions; this is the coarse gate.)
pub fn test_integrity_ok(base_test_count: i64, head_test_count: i64) -> bool {
    head_test_count >= base_test_count
}

/// Branch name for an improvement: `<prefix>/<id>-<slug>`.
pub fn branch_name(prefix: &str, id: i64, title: &str) -> String {
    let slug = util::slugify(title, 40);
    if slug.is_empty() {
        format!("{prefix}/{id}")
    } else {
        format!("{prefix}/{id}-{slug}")
    }
}

/// A short human title from a free-form spec (first line, trimmed to 72 chars).
pub fn title_from_spec(spec: &str) -> String {
    let first = spec.lines().next().unwrap_or("").trim();
    util::truncate_chars(first, 72)
}

fn is_valid_source(s: &str) -> bool {
    matches!(
        s,
        SRC_DIRECTED | SRC_FAILING_TEST | SRC_CLIPPY | SRC_PLAN_ITEM | SRC_RUNBOOK_FIX
    )
}

/// Remote/typed confirmation "ano N" / "ne N". As with runbooks and nudges, a
/// bare "ano" carries NO id and must never approve anything — the number is
/// mandatory. Tolerant of case and surrounding punctuation.
pub fn parse_confirm(text: &str) -> Option<(bool, i64)> {
    // fold diacritics so "zahoď"/"schvaľ" match their ASCII verb forms
    let t: String = text
        .chars()
        .flat_map(char::to_lowercase)
        .map(util::fold_ascii)
        .collect();
    let mut tokens = t
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty());
    let yes = match tokens.next()? {
        "ano" | "jo" | "ok" | "schval" | "merge" => true,
        "ne" | "zahod" | "zamitni" => false,
        _ => return None,
    };
    Some((yes, tokens.next()?.parse().ok()?))
}

// ---------- repo location ----------

/// Absolute path of the source repository. Config `improve.repo_dir` wins;
/// otherwise the compile-time manifest dir (the path this very binary was built
/// from — for a self-built Jarvis that IS the repo). The returned dir is not
/// guaranteed to exist; callers that mutate it verify `.git` first.
pub fn repo_root(cfg: &Config) -> PathBuf {
    let configured = cfg.improve.repo_dir.trim();
    if !configured.is_empty() {
        PathBuf::from(configured)
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }
}

// ---------- CLI ----------

#[derive(clap::Subcommand, Debug)]
pub enum ImproveCmd {
    /// Queue a directed improvement ("teach yourself X") into the git-tracked ledger
    Queue {
        /// Free-form task, e.g. "přidej si schopnost číst RSS a hlásit novinky"
        spec: Vec<String>,
        /// Origin tag (default: directed)
        #[arg(long)]
        source: Option<String>,
    },
    /// List the improvement ledger (newest first)
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one improvement (ledger row + diff once drafted)
    Show { id: i64 },
    /// Abandon a queued/drafted improvement (branch is kept for inspection)
    Dismiss { id: i64 },
    /// Draft: branch off main, let Claude write the change, Jarvis commits it  [phase 2]
    Draft {
        /// Improvement id from the ledger (default: the oldest queued one)
        id: Option<i64>,
        /// Show the plan and prompt only — no branch, no API, no writes
        #[arg(long)]
        dry_run: bool,
    },
    /// Failable gate: build + test the branch; reject on red or weakened tests  [phase 3]
    Test { id: i64 },
    /// Offer a green change for approval — pins the diff's sha256  [phase 4]
    Propose { id: i64 },
    /// Approve + merge to main (TTY typed-token or verified Telegram)  [phase 4]
    Approve { id: i64 },
    /// Deploy a merged change: rebuild, smoke-test, swap with rollback, restart  [phase 6]
    Deploy { id: i64 },
    /// One scheduler tick (queued → draft → test → propose); --dry-run just previews  [phase 5]
    Tick {
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn cli(paths: &Paths, cfg: &Config, conn: &rusqlite::Connection, cmd: ImproveCmd) -> Result<()> {
    match cmd {
        ImproveCmd::Queue { spec, source } => {
            let spec = spec.join(" ").trim().to_string();
            ensure!(
                !spec.is_empty(),
                "zadej popis: jarvis improve queue \"přidej si schopnost X\""
            );
            let source = source.unwrap_or_else(|| SRC_DIRECTED.to_string());
            ensure!(
                is_valid_source(&source),
                "neznámý zdroj '{source}' (directed|failing_test|clippy|plan_item|runbook_fix)"
            );
            let title = title_from_spec(&spec);
            let id = db::insert_improvement(conn, util::now_ts(), &source, &title, &spec)?;
            println!("✓ vylepšení #{id} zařazeno do ledgeru (stav: {STATUS_QUEUED})");
            println!("  „{title}“");
            println!("  další krok (fáze 2): jarvis improve draft {id}");
            Ok(())
        }
        ImproveCmd::List { limit } => {
            let rows = db::improvements_recent(conn, limit)?;
            if rows.is_empty() {
                println!("Ledger je prázdný. Zařaď vylepšení: jarvis improve queue \"…\"");
                return Ok(());
            }
            println!("{:>4}  {:<11} {:<40} {}", "id", "stav", "název", "větev");
            for r in &rows {
                println!("{}", fmt_improvement(r));
            }
            Ok(())
        }
        ImproveCmd::Show { id } => {
            let r = db::improvement_by_id(conn, id)?
                .with_context(|| format!("vylepšení #{id} neexistuje"))?;
            print_improvement(&r);
            // once drafted, show the diffstat against the base (review aid)
            if !r.base_commit.is_empty() && !r.head_commit.is_empty() {
                if let Ok(stat) = git(&repo_root(cfg), &["diff", "--stat", &r.base_commit, &r.head_commit]) {
                    let stat = stat.trim();
                    if !stat.is_empty() {
                        println!("  ── diff --stat (base → head) ──");
                        for line in stat.lines() {
                            println!("  {line}");
                        }
                    }
                }
            }
            Ok(())
        }
        ImproveCmd::Dismiss { id } => {
            let r = db::improvement_by_id(conn, id)?
                .with_context(|| format!("vylepšení #{id} neexistuje"))?;
            ensure!(
                !matches!(r.status.as_str(), STATUS_MERGED | STATUS_DEPLOYED),
                "vylepšení #{id} už je {} — merge/deploy nejde vzít zpět tudy (řeš přes git)",
                r.status
            );
            db::set_improvement_status(conn, id, STATUS_DISMISSED, "ručně zahozeno")?;
            // drop the isolated worktree; keep the branch for inspection
            cleanup_worktree(&repo_root(cfg), &worktree_path(paths, id));
            let tail = if r.branch.is_empty() {
                String::new()
            } else {
                format!(" (větev {} zůstává k inspekci)", r.branch)
            };
            println!("✓ vylepšení #{id} zahozeno{tail}");
            Ok(())
        }
        ImproveCmd::Tick { dry_run } => {
            if dry_run {
                run_dry(cfg, conn)
            } else {
                bail!("automatická smyčka je fáze 5 — zatím spouštěj kroky ručně (draft → test → propose → approve)")
            }
        }
        ImproveCmd::Draft { id, dry_run } => draft(paths, cfg, conn, id, dry_run),
        ImproveCmd::Test { id } => test_cmd(paths, cfg, conn, id),
        ImproveCmd::Propose { id } => propose(cfg, conn, id),
        ImproveCmd::Approve { id } => approve(paths, cfg, conn, id),
        ImproveCmd::Deploy { .. } => not_yet(6, "deploy (rebuild + smoke + swap + restart)"),
    }
}

fn not_yet(phase: u8, what: &str) -> Result<()> {
    bail!("„{what}“ je fáze {phase} — v tomto buildu ještě neimplementováno (ship-dark scaffold)")
}

/// `jarvis improve tick --dry-run`: config envelope + ledger tally + repo
/// readiness. Read-only, no API — a real check against the live DB.
pub fn run_dry(cfg: &Config, conn: &rusqlite::Connection) -> Result<()> {
    let im = &cfg.improve;
    println!(
        "Sebe-vývoj: enabled={} | self-source={} | auto-merge-safe={} | deploy={}",
        im.enabled, im.allow_self_source, im.auto_merge_safe, im.deploy_enabled
    );
    println!(
        "  model={} | max_turns={} | timeout={}s | repair={} | rozpočet={:.2} USD/den | strop={}/den",
        if im.model.is_empty() { "<default CLI>" } else { &im.model },
        im.max_turns,
        im.timeout_s,
        im.repair_attempts,
        im.daily_budget_usd,
        im.daily_max
    );
    let root = repo_root(cfg);
    let git_ok = root.join(".git").exists();
    println!(
        "  repo: {} {}",
        root.display(),
        if git_ok { "(git ✓)" } else { "(⚠ není git repo)" }
    );

    let rows = db::improvements_recent(conn, 500)?;
    if rows.is_empty() {
        println!("Ledger: prázdný.");
    } else {
        let mut by: BTreeMap<&str, u32> = BTreeMap::new();
        for r in &rows {
            *by.entry(r.status.as_str()).or_insert(0) += 1;
        }
        let parts: Vec<String> = by.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("Ledger ({} celkem): {}", rows.len(), parts.join(", "));
    }
    if !im.enabled {
        println!("\n⚠ vrstva je ship-dark (enabled=false) — `tick` bez --dry-run nic neudělá.");
    }
    Ok(())
}

fn fmt_improvement(r: &db::ImprovementRow) -> String {
    let branch = if r.branch.is_empty() { "—" } else { &r.branch };
    format!(
        "{:>4}  {:<11} {:<40} {}",
        r.id,
        r.status,
        util::truncate_chars(&r.title, 40),
        branch
    )
}

fn print_improvement(r: &db::ImprovementRow) {
    println!("#{} „{}“", r.id, r.title);
    println!("  stav:     {}", r.status);
    println!("  zdroj:    {}", r.source);
    println!("  založeno: {}", util::fmt_local(r.created_at));
    if !r.branch.is_empty() {
        println!("  větev:    {}", r.branch);
    }
    if !r.base_commit.is_empty() {
        let base: String = r.base_commit.chars().take(12).collect();
        let head: String = r.head_commit.chars().take(12).collect();
        println!("  commit:   base {base} → head {}", if head.is_empty() { "—" } else { &head });
    }
    if !r.envelope.is_empty() {
        println!("  obálka:   {}", r.envelope);
    }
    if let Some(passed) = r.tests_passed {
        println!("  testy:    {}", if passed { "✓ zeleno" } else { "✗ červeno" });
    }
    if !r.diff_stat.is_empty() {
        println!("  diff:     {}", r.diff_stat);
    }
    if !r.diff_sha256.is_empty() {
        let short: String = r.diff_sha256.chars().take(12).collect();
        println!("  otisk:    {short}…");
    }
    if r.cost_usd > 0.0 {
        println!("  náklad:   {:.4} USD ({} in / {} out)", r.cost_usd, r.tokens_in, r.tokens_out);
    }
    if r.approved_at.is_some() {
        println!(
            "  schváleno: {} ({})",
            r.approved_at.map(util::fmt_local).unwrap_or_default(),
            r.approved_via
        );
    }
    if !r.spec.is_empty() {
        println!("  ── zadání ──");
        for line in r.spec.lines().take(20) {
            println!("  {line}");
        }
    }
    if !r.note.is_empty() {
        println!("  pozn.:    {}", r.note);
    }
}

// ---------- subprocess runner (git + cargo) ----------

struct CmdOut {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// Drains a child pipe to a String on its own thread (a full pipe must never
/// deadlock the wait loop). None pipe → empty.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    match pipe {
        Some(mut p) => {
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = p.read_to_string(&mut s);
                let _ = tx.send(s);
            });
        }
        None => {
            let _ = tx.send(String::new());
        }
    }
    rx
}

/// Runs a child in its OWN process group with a hard timeout (SIGKILL to the
/// whole group — cargo spawns rustc children). Threaded drain, bounded join.
/// Modeled on `runbook::run_one`.
fn run_capture(
    program: &str,
    args: &[&str],
    dir: &Path,
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Result<CmdOut> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().with_context(|| format!("nelze spustit {program}"))?;
    let out_rx = drain(child.stdout.take());
    let err_rx = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(_) => {
                unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let grace = Duration::from_secs(5);
    let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
    Ok(CmdOut { code: status.and_then(|s| s.code()), stdout, stderr, timed_out })
}

// ---------- git plumbing (Jarvis owns every git op) ----------

/// Runs a git command in `dir`, returning stdout; errors carry stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = run_capture("git", args, dir, &[], Duration::from_secs(120))?;
    if out.timed_out {
        bail!("git {} vypršel (120 s)", args.join(" "));
    }
    if out.code != Some(0) {
        bail!(
            "git {} selhalo (exit {:?}): {}",
            args.join(" "),
            out.code,
            util::truncate_chars(out.stderr.trim(), 500)
        );
    }
    Ok(out.stdout)
}

fn git_head(repo: &Path, rev: &str) -> Result<String> {
    Ok(git(repo, &["rev-parse", rev])?.trim().to_string())
}

fn worktree_is_clean(dir: &Path) -> Result<bool> {
    Ok(git(dir, &["status", "--porcelain"])?.trim().is_empty())
}

fn changed_files(worktree: &Path, base: &str) -> Result<Vec<String>> {
    Ok(git(worktree, &["diff", "--name-only", base, "HEAD"])?
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Counts test attributes in the tree's Rust sources — input to the
/// test-integrity guard (a shrinking count = tests removed to force green).
fn count_test_attrs(dir: &Path) -> i64 {
    let script = "grep -rIhE '#\\[(test|tokio::test)\\]' src 2>/dev/null | wc -l";
    match run_capture("bash", &["-c", script], dir, &[], Duration::from_secs(60)) {
        Ok(o) => o.stdout.trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Commits all changes in the worktree under Jarvis's machine identity, so
/// `git log --author=<author_name>` cleanly separates self-authored changes.
fn commit_all(worktree: &Path, im: &ImproveCfg, msg: &str) -> Result<String> {
    git(worktree, &["add", "-A"])?;
    let name = format!("user.name={}", im.author_name);
    let email = format!("user.email={}", im.author_email);
    git(worktree, &["-c", name.as_str(), "-c", email.as_str(), "commit", "--no-verify", "-m", msg])?;
    git_head(worktree, "HEAD")
}

fn worktree_path(paths: &Paths, id: i64) -> PathBuf {
    paths.data_dir.join("improve").join(format!("wt-{id}"))
}

/// Removes the worktree dir and prunes git's registry (branch is kept for
/// inspection). Best-effort — never fails the caller.
fn cleanup_worktree(repo: &Path, wt: &Path) {
    if wt.exists() {
        let wt_s = wt.to_string_lossy();
        let _ = git(repo, &["worktree", "remove", "--force", wt_s.as_ref()]);
        let _ = std::fs::remove_dir_all(wt);
    }
    let _ = git(repo, &["worktree", "prune"]);
}

/// Single-draft lock (flock), same idea as per-runbook locks.
fn acquire_lock(paths: &Paths) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let path = paths.data_dir.join("improve.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("nelze otevřít {}", path.display()))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("jiný improve běh právě probíhá — nespouštím podruhé");
    }
    Ok(f)
}

// ---------- codegen prompt + result parsing (pure) ----------

/// The instructions handed to the codegen agent. Bakes in the house rules
/// (English comments), the test-integrity constraint, gate-file caution, and a
/// strict JSON result contract.
fn build_draft_prompt(spec: &str, title: &str) -> String {
    format!(
        "You are Jarvis working on your OWN source code — a Rust project (an X11 \
watcher + voice assistant). You are on a fresh, isolated git branch; make the \
change described below, safely and completely.\n\n\
TASK: {spec}\n\n\
HOUSE RULES (from CLAUDE.md):\n\
- Write all code comments in English; be brief — explain WHY, not what the code \
obviously does. User-facing Czech strings stay Czech (they are data).\n\
- Match the surrounding code's style, naming, and idioms.\n\n\
REQUIREMENTS:\n\
- Keep the change MINIMAL and focused on the task; do not refactor unrelated code.\n\
- Add or extend unit tests for the change, then run `cargo test` until green.\n\
- NEVER weaken, delete, or #[ignore] existing tests to make things pass. The total \
test count must not go down — that is checked and will reject your work.\n\
- Reuse existing patterns/helpers (util::, the config/db conventions) over new \
dependencies. Do NOT add crates (`cargo add` is unavailable).\n\
- Be cautious around gate/safety files (config.rs defaults, runbook.rs, improve.rs, \
units.rs, main.rs, telegram.rs). Touch them only if the task truly requires it, and \
say so in your summary if you do.\n\
- You have Read/Edit/Write and cargo (build/test/check/fmt/clippy). You do NOT have \
git — do not attempt commits; committing is handled for you.\n\n\
WHEN DONE, output ONE JSON object and nothing after it:\n\
{{\"summary\": \"one line: what you changed\", \"files_changed\": [\"src/...\"], \
\"tests_added\": <int>, \"touched_gate_files\": <bool>}}\n\n\
Title: {title}"
    )
}

#[derive(Debug, Default)]
struct DraftResult {
    summary: String,
    files_changed: Vec<String>,
    tests_added: i64,
    touched_gate_files: bool,
}

/// Best-effort parse of the agent's JSON result (context only — Jarvis computes
/// the authoritative diff/envelope from git regardless).
fn parse_draft_result(text: &str) -> Option<DraftResult> {
    let json = claude::extract_json(text).ok()?;
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(DraftResult {
        summary: v["summary"].as_str().unwrap_or_default().to_string(),
        files_changed: v["files_changed"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        tests_added: v["tests_added"].as_i64().unwrap_or(0),
        touched_gate_files: v["touched_gate_files"].as_bool().unwrap_or(false),
    })
}

/// Repair-turn prompt: the previous attempt's `cargo test` was red. Hand the
/// agent the failing output and ask for the SMALLEST fix, same guardrails.
fn build_repair_prompt(spec: &str, gate_output: &str) -> String {
    format!(
        "Your previous attempt did NOT pass — `cargo test` failed with the output below. \
Fix it with the SMALLEST change; keep what already works, don't restart from scratch.\n\n\
ORIGINAL TASK: {spec}\n\n\
CARGO TEST OUTPUT (tail):\n{}\n\n\
Same rules: NEVER weaken, delete, or #[ignore] tests to go green (the test count must \
not drop); reuse existing helpers; do not add crates; you have Read/Edit/Write and \
cargo but NOT git. Make cargo test pass, then output ONE JSON object:\n\
{{\"summary\": \"what you fixed\", \"files_changed\": [\"src/...\"], \"tests_added\": <int>, \"touched_gate_files\": <bool>}}",
        util::truncate_chars(gate_output.trim(), 3000)
    )
}

/// Total self-improvement spend today (components `improve%`) — budget input.
fn improve_spent_today(conn: &rusqlite::Connection) -> f64 {
    let day_start = util::day_bounds_local(util::today_local()).map(|(s, _)| s).unwrap_or(0);
    db::cost_since_like(conn, "improve%", day_start).unwrap_or(0.0)
}

fn draft_commit_message(id: i64, title: &str, result: Option<&DraftResult>) -> String {
    let body = result
        .map(|r| r.summary.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("Automated self-improvement drafted by Jarvis.");
    format!("{}\n\n{}\n\nJarvis-Improvement: {}\n", util::truncate_chars(title, 72), body, id)
}

/// Parses `cargo test` output: (all suites ok?, total tests that ran). Handles
/// multiple `test result:` lines (unit + integration + doctests). None if no
/// summary line is present (e.g. a build failure before any test ran).
fn parse_test_summary(output: &str) -> Option<(bool, i64)> {
    let mut seen = false;
    let mut ok = true;
    let mut total = 0i64;
    for line in output.lines() {
        let Some(rest) = line.trim().strip_prefix("test result:") else {
            continue;
        };
        seen = true;
        let rest = rest.trim_start();
        if !rest.starts_with("ok") {
            ok = false;
        }
        // "... ok. 269 passed; 0 failed; ..." → number right before "passed"
        if let Some(before) = rest.split("passed").next() {
            if let Some(n) = before.split_whitespace().last().and_then(|t| t.parse::<i64>().ok()) {
                total += n;
            }
        }
    }
    seen.then_some((ok, total))
}

// ---------- gate + draft/test commands (shell) ----------

struct GateOutcome {
    passed: bool,
    output: String,
}

/// Failable gate: `cargo test` on the worktree. Shares the main repo's target
/// dir so the compile is incremental (warm cache), not a cold CUDA rebuild.
fn run_gate(worktree: &Path, repo: &Path, im: &ImproveCfg) -> Result<GateOutcome> {
    let target = repo.join("target");
    let target_s = target.to_string_lossy();
    let envs = [("CARGO_TARGET_DIR", target_s.as_ref())];
    let timeout = Duration::from_secs(im.timeout_s.max(600));
    info!("improve: cargo test na větvi ({})", worktree.display());
    let test = run_capture("cargo", &["test", "--quiet"], worktree, &envs, timeout)?;
    let combined = format!("{}\n{}", test.stdout, test.stderr);
    let passed = !test.timed_out && test.code == Some(0);
    let mut output = util::truncate_chars(combined.trim(), 6000);
    if test.timed_out {
        output.push_str(&format!("\n[jarvis] cargo test zabit po {} s", timeout.as_secs()));
    }
    Ok(GateOutcome { passed, output })
}

/// `jarvis improve draft [id] [--dry-run]`: branch off committed main, run
/// codegen in an isolated worktree, commit under the machine identity, then run
/// the integrity + build/test gate. Ship-dark: the live path needs `enabled`.
fn draft(
    paths: &Paths,
    cfg: &Config,
    conn: &rusqlite::Connection,
    id: Option<i64>,
    dry_run: bool,
) -> Result<()> {
    let im = &cfg.improve;
    let imp = match id {
        Some(i) => db::improvement_by_id(conn, i)?.with_context(|| format!("vylepšení #{i} neexistuje"))?,
        None => db::oldest_queued_improvement(conn)?
            .context("žádné queued vylepšení; zadej: jarvis improve queue \"…\"")?,
    };
    let repo = repo_root(cfg);
    let branch = branch_name(&im.branch_prefix, imp.id, &imp.title);
    let wt = worktree_path(paths, imp.id);
    let prompt = build_draft_prompt(&imp.spec, &imp.title);

    if dry_run {
        println!("── improve draft #{} (dry-run — nic se nespustí) ──", imp.id);
        println!("stav:      {}", imp.status);
        println!("repo:      {} {}", repo.display(), if repo.join(".git").exists() { "(git ✓)" } else { "(⚠ není git)" });
        println!("větev:     {branch}");
        println!("worktree:  {}", wt.display());
        println!("model:     {}", if im.model.is_empty() { "<default CLI>" } else { &im.model });
        println!("nástroje:  {DRAFT_TOOLS}");
        println!("max_turns: {} | timeout: {}s | enabled: {}", im.max_turns, im.timeout_s, im.enabled);
        println!("\n── prompt pro codegen ──\n{prompt}");
        return Ok(());
    }

    ensure!(
        im.enabled,
        "[improve] enabled=false — ostrý draft je vypnutý (ship-dark). Náhled: jarvis improve draft {} --dry-run",
        imp.id
    );
    ensure!(
        matches!(imp.status.as_str(), STATUS_QUEUED | STATUS_FAILED),
        "vylepšení #{} je ve stavu '{}' — draft dělá jen 'queued'/'failed'",
        imp.id,
        imp.status
    );
    ensure!(repo.join(".git").exists(), "repo {} nemá .git — nastav [improve] repo_dir", repo.display());
    let base = git_head(&repo, "main").context("v repu není větev 'main' (nebo git selhal)")?;

    let _lock = acquire_lock(paths)?;
    // fresh worktree at COMMITTED main HEAD — never the dirty live tree
    cleanup_worktree(&repo, &wt);
    let _ = git(&repo, &["branch", "-D", &branch]); // clear a stale branch from a prior failed draft
    if let Some(parent) = wt.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("nelze vytvořit {}", parent.display()))?;
    }
    db::set_improvement_status(conn, imp.id, STATUS_DRAFTING, "codegen běží")?;
    let wt_arg = wt.to_str().context("cesta worktree není UTF-8")?;
    git(&repo, &["worktree", "add", "-b", &branch, wt_arg, &base])
        .with_context(|| format!("worktree add selhalo pro větev {branch}"))?;

    let base_tests = count_test_attrs(&wt);
    info!("improve #{}: codegen na {branch} (base {base_tests} testů, {})", imp.id, short(&base));

    // The codegen agent runs cargo during its edit→test loop; point it at the
    // main repo's warm target dir so those builds are incremental, not a cold
    // CUDA rebuild per turn. (This CLI command is a one-shot process, so a
    // process-wide env var is fine; the gate sets the same dir explicitly.)
    std::env::set_var("CARGO_TARGET_DIR", repo.join("target"));

    let model = if im.model.is_empty() { None } else { Some(im.model.as_str()) };
    let max_attempts = 1 + im.repair_attempts; // initial draft + up to N repairs
    let mut total_cost = 0.0;
    let mut summary = String::new();
    let mut last_gate: Option<GateOutcome> = None;
    let mut head_short = String::new();
    let mut envelope = ENV_FEATURE;
    let mut diff_stat = String::new();
    let mut head_tests = base_tests;

    for attempt in 0..max_attempts {
        // repairs cost money too — stop before spending past the daily budget
        if attempt > 0 {
            let spent = improve_spent_today(conn);
            if spent >= im.daily_budget_usd {
                info!("improve #{}: rozpočet vyčerpán ({spent:.2}/{:.2} USD) — už neopravuji", imp.id, im.daily_budget_usd);
                break;
            }
        }
        let prompt = match &last_gate {
            None => build_draft_prompt(&imp.spec, &imp.title),
            Some(g) => build_repair_prompt(&imp.spec, &g.output),
        };
        let outcome = match claude::run(&claude::ClaudeRequest {
            prompt,
            model,
            cwd: &wt,
            allowed_tools: DRAFT_TOOLS,
            max_turns: im.max_turns,
            timeout: Duration::from_secs(im.timeout_s),
        }) {
            Ok(o) => o,
            Err(e) => {
                db::update_improvement_tests(conn, imp.id, None, &util::truncate_chars(&format!("{e:#}"), 2000), STATUS_FAILED)?;
                if attempt == 0 {
                    cleanup_worktree(&repo, &wt); // nothing worth keeping
                }
                return Err(e).context("codegen selhal");
            }
        };
        total_cost += outcome.cost_usd;
        db::add_improvement_cost(conn, imp.id, outcome.cost_usd, outcome.tokens_in, outcome.tokens_out)?;
        let _ = db::insert_cost(
            conn,
            util::now_ts(),
            if attempt == 0 { "improve" } else { "improve-repair" },
            &im.model,
            outcome.tokens_in,
            outcome.tokens_out,
            outcome.cost_usd,
        );
        let result = parse_draft_result(&outcome.text);
        if let Some(r) = &result {
            if !r.summary.is_empty() {
                summary = r.summary.clone();
            }
        }

        if worktree_is_clean(&wt)? {
            if attempt == 0 {
                db::update_improvement_tests(conn, imp.id, None, "agent neprovedl žádnou změnu", STATUS_FAILED)?;
                cleanup_worktree(&repo, &wt);
                bail!("codegen neprovedl žádnou změnu (#{} → failed)", imp.id);
            }
            info!("improve #{}: oprava nic nezměnila — končím s poslední červenou", imp.id);
            break;
        }

        let head = commit_all(&wt, im, &draft_commit_message(imp.id, &imp.title, result.as_ref()))?;
        head_short = short(&head).to_string();
        let files = changed_files(&wt, &base)?;
        envelope = classify_envelope(&files);
        diff_stat = git(&wt, &["diff", "--shortstat", &base, "HEAD"])?.trim().to_string();
        db::update_improvement_draft(conn, imp.id, &branch, &base, &head, envelope, &diff_stat, STATUS_DRAFTING)?;
        info!("improve #{}: commit {head_short} ({} souborů, obálka {envelope}, pokus {})", imp.id, files.len(), attempt + 1);

        // integrity guard: the suite must not shrink
        head_tests = count_test_attrs(&wt);
        if !test_integrity_ok(base_tests, head_tests) {
            let note = format!("test-integrity FAIL: base {base_tests} → head {head_tests} (testy nesmí ubývat)");
            db::update_improvement_tests(conn, imp.id, Some(false), &note, STATUS_FAILED)?;
            bail!("#{}: {note} — zamítnuto (větev {branch} zůstává k inspekci)", imp.id);
        }

        // failable gate: build + test
        let gate = run_gate(&wt, &repo, im)?;
        if gate.passed {
            db::update_improvement_tests(conn, imp.id, Some(true), &gate.output, STATUS_TESTED)?;
            println!("── improve draft #{} ──", imp.id);
            println!("větev:   {branch}");
            println!("commit:  {head_short}");
            println!("obálka:  {envelope}");
            println!("diff:    {diff_stat}");
            let repaired = if attempt > 0 { format!(" (po {} opravě/ách)", attempt) } else { String::new() };
            println!("testy:   base {base_tests} → head {head_tests}; brána ✓ ZELENÁ{repaired}");
            if !summary.is_empty() {
                println!("shrnutí: {}", util::truncate_chars(&summary, 200));
            }
            println!("náklad:  {total_cost:.4} USD");
            println!("\n✓ připraveno: jarvis improve show {} → propose/approve", imp.id);
            return Ok(());
        }
        db::update_improvement_tests(conn, imp.id, Some(false), &gate.output, STATUS_FAILED)?;
        last_gate = Some(gate);
        if attempt + 1 < max_attempts {
            info!("improve #{}: brána ČERVENÁ — pokus o opravu ({}/{})", imp.id, attempt + 1, im.repair_attempts);
        }
    }

    // exhausted attempts / budget / no-op repair → failed; keep branch for inspection
    println!("── improve draft #{} ──", imp.id);
    println!("větev:   {branch}");
    if !head_short.is_empty() {
        println!("commit:  {head_short}  (obálka {envelope}, {diff_stat})");
    }
    println!("testy:   base {base_tests} → head {head_tests}; brána ✗ ČERVENÁ (vyčerpáno {max_attempts} pokusů)");
    println!("náklad:  {total_cost:.4} USD");
    println!("\n✗ nezvládnuto → failed. Log: jarvis improve show {}", imp.id);
    bail!("brána červená po {max_attempts} pokusech (#{} → failed)", imp.id);
}

/// `jarvis improve test <id>`: re-run the failable gate on an already-drafted
/// branch's worktree.
fn test_cmd(paths: &Paths, cfg: &Config, conn: &rusqlite::Connection, id: i64) -> Result<()> {
    let imp = db::improvement_by_id(conn, id)?.with_context(|| format!("vylepšení #{id} neexistuje"))?;
    ensure!(!imp.branch.is_empty(), "vylepšení #{id} ještě nebylo draftnuto (žádná větev)");
    let repo = repo_root(cfg);
    let wt = worktree_path(paths, id);
    ensure!(wt.exists(), "worktree #{id} chybí ({}) — nejdřív `jarvis improve draft {id}`", wt.display());
    let _lock = acquire_lock(paths)?;
    let gate = run_gate(&wt, &repo, &cfg.improve)?;
    let status = if gate.passed { STATUS_TESTED } else { STATUS_FAILED };
    db::update_improvement_tests(conn, id, Some(gate.passed), &gate.output, status)?;
    println!("testy #{id} ({}): brána {}", imp.branch, if gate.passed { "✓ ZELENÁ" } else { "✗ ČERVENÁ" });
    if !gate.passed {
        bail!("brána červená — log: jarvis improve show {id}");
    }
    Ok(())
}

// ---------- propose / approve / merge (phase 4) ----------

/// First 12 chars of a sha (display), safe for short strings.
fn short(sha: &str) -> &str {
    &sha[..12.min(sha.len())]
}

/// `jarvis improve propose <id>`: pin the green change's diff hash and offer it
/// for approval. Operates on the stored base/head commits — no worktree needed.
fn propose(cfg: &Config, conn: &rusqlite::Connection, id: i64) -> Result<()> {
    let imp = db::improvement_by_id(conn, id)?.with_context(|| format!("vylepšení #{id} neexistuje"))?;
    ensure!(imp.status == STATUS_TESTED, "navrhnout jde jen otestované (tested) — #{id} je '{}'", imp.status);
    ensure!(imp.tests_passed == Some(true), "#{id} nemá zelené testy — nejdřív projdi bránou");
    ensure!(
        !imp.base_commit.is_empty() && !imp.head_commit.is_empty(),
        "#{id} nemá commity (draft neproběhl?)"
    );
    let repo = repo_root(cfg);
    let diff = git(&repo, &["diff", &imp.base_commit, &imp.head_commit])?;
    ensure!(!diff.trim().is_empty(), "prázdný diff — nic k nabídnutí (#{id})");
    let sha = util::sha256_hex(diff.as_bytes());
    let files: Vec<String> = git(&repo, &["diff", "--name-only", &imp.base_commit, &imp.head_commit])?
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let envelope = classify_envelope(&files);
    db::set_improvement_proposed(conn, id, &sha, envelope)?;
    println!("── návrh #{id}: {} ──", imp.title);
    println!("větev:    {}", imp.branch);
    let warn = if envelope == ENV_GATE_CRITICAL {
        "  ⚠ sahá na bezpečnostní/gate soubory — nutná pečlivá revize"
    } else {
        ""
    };
    println!("obálka:   {envelope}{warn}");
    println!("diff:     {}", imp.diff_stat);
    println!("otisk:    {}… (zapíchnuto; před merge se ověří)", short(&sha));
    println!("revize:   git -C {} diff {} {}", repo.display(), short(&imp.base_commit), short(&imp.head_commit));
    println!("schválit: jarvis improve approve {id}   (u klávesnice — opíšeš číslo)");
    Ok(())
}

/// `jarvis improve approve <id>`: the human gate. Re-verifies no drift, the
/// pinned diff hash (TOCTOU), and a clean main tree, requires a typed-token
/// confirmation, then fast-forwards main to the Jarvis-authored commit.
fn approve(paths: &Paths, cfg: &Config, conn: &rusqlite::Connection, id: i64) -> Result<()> {
    let imp = db::improvement_by_id(conn, id)?.with_context(|| format!("vylepšení #{id} neexistuje"))?;
    ensure!(imp.status == STATUS_PROPOSED, "schválit jde jen navržené (proposed) — #{id} je '{}'", imp.status);
    ensure!(imp.tests_passed == Some(true), "#{id} nemá zelené testy");
    let repo = repo_root(cfg);
    ensure!(repo.join(".git").exists(), "repo {} nemá .git", repo.display());
    // no drift: main must still be exactly where the branch was cut from
    let main_head = git_head(&repo, "main")?;
    ensure!(
        main_head == imp.base_commit,
        "main se pohnul od návrhu (base {} → nyní {}) — #{id} je potřeba re-draftnout",
        short(&imp.base_commit),
        short(&main_head)
    );
    // TOCTOU: the diff must still hash to the pinned value
    let diff = git(&repo, &["diff", &imp.base_commit, &imp.head_commit])?;
    let sha = util::sha256_hex(diff.as_bytes());
    ensure!(
        sha == imp.diff_sha256,
        "diff #{id} se od návrhu ZMĚNIL (otisk nesedí) — z bezpečnosti NEMERGUJI"
    );
    // never merge over uncommitted work in the live tree
    ensure!(
        worktree_is_clean(&repo)?,
        "main má necommitnuté změny — ukliď/commitni je před merge #{id}"
    );

    if imp.envelope == ENV_GATE_CRITICAL {
        println!("⚠⚠ #{id} sahá na BEZPEČNOSTNÍ/GATE soubory. Zkontroluj diff:");
        println!("   git -C {} diff {} {}", repo.display(), short(&imp.base_commit), short(&imp.head_commit));
    }
    crate::runbook::confirm_at_keyboard(
        &format!("Schválit a MERGNOUT vylepšení #{id} do main? Opiš číslo pro potvrzení: "),
        &id.to_string(),
    )?;
    db::set_improvement_approved(conn, id, "cli")?;

    // fast-forward: the branch is main + the improvement commit, so main advances
    // to the Jarvis-authored commit with no merge commit and no tree churn
    git(&repo, &["merge", "--ff-only", &imp.branch])
        .with_context(|| format!("merge --ff-only {} selhal — #{id} zůstává approved, řeš ručně", imp.branch))?;
    db::set_improvement_merged(conn, id)?;
    cleanup_worktree(&repo, &worktree_path(paths, id));
    let _ = git(&repo, &["branch", "-d", &imp.branch]);
    let new_head = git_head(&repo, "main").unwrap_or_default();
    println!("✓ #{id} smergnuto do main (HEAD {})", short(&new_head));
    println!("  commit autor: {} <{}>", cfg.improve.author_name, cfg.improve.author_email);
    if cfg.improve.deploy_enabled {
        println!("  nasazení: jarvis improve deploy {id}  (fáze 6: rebuild + smoke + restart)");
    } else {
        println!("  nasazení ručně: cargo install --path . --force  (deploy_enabled=false)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_docs_only_is_safe() {
        assert_eq!(classify_envelope(&["README.md".into(), "PLAN.md".into()]), ENV_SAFE);
    }

    #[test]
    fn envelope_ordinary_code_is_feature() {
        assert_eq!(classify_envelope(&["src/digest/build.rs".into()]), ENV_FEATURE);
        // a mix of docs and code is NOT safe
        assert_eq!(
            classify_envelope(&["README.md".into(), "src/digest/build.rs".into()]),
            ENV_FEATURE
        );
    }

    #[test]
    fn envelope_gate_code_is_critical() {
        // every safety-critical file must classify as gate_critical, even alone
        for f in SAFETY_CRITICAL {
            assert_eq!(
                classify_envelope(&[(*f).to_string()]),
                ENV_GATE_CRITICAL,
                "{f} musí být gate_critical"
            );
        }
        // ...and dominates a mixed diff (docs + gate code = still critical)
        assert_eq!(
            classify_envelope(&["README.md".into(), "src/config.rs".into()]),
            ENV_GATE_CRITICAL
        );
        // CI config is critical too
        assert_eq!(classify_envelope(&[".github/workflows/ci.yml".into()]), ENV_GATE_CRITICAL);
    }

    #[test]
    fn envelope_empty_defaults_to_review() {
        assert_eq!(classify_envelope(&[]), ENV_FEATURE);
    }

    #[test]
    fn safety_critical_normalizes_leading_dotslash() {
        assert!(touches_safety_critical(&["./src/config.rs".into()]));
        assert!(!touches_safety_critical(&["src/patterns.rs".into()]));
    }

    #[test]
    fn test_integrity_rejects_shrinking_suite() {
        assert!(test_integrity_ok(260, 262), "přidané testy jsou OK");
        assert!(test_integrity_ok(260, 260), "stejný počet je OK");
        assert!(!test_integrity_ok(260, 259), "ubrané testy = podvod → zamítni");
    }

    #[test]
    fn branch_name_slugs_the_title() {
        assert_eq!(
            branch_name("jarvis/improve", 7, "Přidej RSS čtečku novinek!"),
            "jarvis/improve/7-pridej-rss-ctecku-novinek"
        );
        // empty/punctuation-only title degrades to just the id
        assert_eq!(branch_name("jarvis/improve", 9, "!!!"), "jarvis/improve/9");
    }

    #[test]
    fn confirm_requires_a_number() {
        assert_eq!(parse_confirm("ano 5"), Some((true, 5)));
        assert_eq!(parse_confirm("schval 12"), Some((true, 12)));
        assert_eq!(parse_confirm("ne 5"), Some((false, 5)));
        assert_eq!(parse_confirm("zahoď 7"), Some((false, 7)));
        // bare yes/no carries no id → must do nothing
        assert_eq!(parse_confirm("ano"), None);
        assert_eq!(parse_confirm("ne"), None);
        assert_eq!(parse_confirm("možná"), None);
        assert_eq!(parse_confirm(""), None);
    }

    #[test]
    fn title_from_spec_takes_first_line() {
        assert_eq!(title_from_spec("Přidej RSS\ndruhý řádek"), "Přidej RSS");
        assert_eq!(title_from_spec("  oříznu okraje  "), "oříznu okraje");
    }

    #[test]
    fn parse_test_summary_sums_and_detects_failure() {
        let ok = "running 3 tests\n\
                  test result: ok. 269 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 21s\n\
                  Doc-tests jarvis\n\
                  test result: ok. 2 passed; 0 failed; 0 ignored";
        assert_eq!(parse_test_summary(ok), Some((true, 271)));
        let bad = "test result: FAILED. 267 passed; 2 failed; 1 ignored; 0 measured";
        assert_eq!(parse_test_summary(bad), Some((false, 267)));
        // mixed suites: overall false, counts still sum
        let mixed = "test result: ok. 10 passed; 0 failed\ntest result: FAILED. 5 passed; 1 failed";
        assert_eq!(parse_test_summary(mixed), Some((false, 15)));
        // a build failure has no summary line at all
        assert_eq!(parse_test_summary("error[E0432]: unresolved import\nerror: could not compile"), None);
    }

    #[test]
    fn parse_draft_result_reads_json_with_fences() {
        let text = "Hotovo.\n```json\n{\"summary\":\"add RSS reader\",\"files_changed\":[\"src/rss.rs\"],\"tests_added\":3,\"touched_gate_files\":false}\n```";
        let r = parse_draft_result(text).unwrap();
        assert_eq!(r.summary, "add RSS reader");
        assert_eq!(r.files_changed, vec!["src/rss.rs".to_string()]);
        assert_eq!(r.tests_added, 3);
        assert!(!r.touched_gate_files);
        assert!(parse_draft_result("žádný json").is_none());
    }

    #[test]
    fn draft_prompt_carries_the_guardrails() {
        let p = build_draft_prompt("add an RSS reader", "RSS");
        assert!(p.contains("add an RSS reader"), "the task");
        assert!(p.contains("test count must not go down"), "integrity constraint");
        assert!(p.contains("do NOT have"), "no-git constraint");
        assert!(p.contains("JSON"), "result contract");
        assert!(p.contains("English"), "house rules");
    }

    #[test]
    fn repair_prompt_carries_task_and_failure() {
        let p = build_repair_prompt("add mask_secret", "test result: FAILED. 1 failed\nassertion `left == right` failed");
        assert!(p.contains("add mask_secret"), "original task");
        assert!(p.contains("assertion `left == right`"), "the failure output");
        assert!(p.contains("did NOT pass"));
        assert!(p.contains("test count must not drop"), "integrity still enforced on repair");
    }

    #[test]
    fn commit_message_has_improvement_trailer() {
        let r = DraftResult { summary: "add RSS".into(), ..Default::default() };
        let m = draft_commit_message(7, "Add RSS reader", Some(&r));
        assert!(m.starts_with("Add RSS reader\n"));
        assert!(m.contains("add RSS"));
        assert!(m.contains("Jarvis-Improvement: 7"));
        // no agent result → default body, trailer still present
        let m2 = draft_commit_message(9, "Title", None);
        assert!(m2.contains("Automated self-improvement"));
        assert!(m2.contains("Jarvis-Improvement: 9"));
    }

    #[test]
    fn short_sha_is_safe() {
        assert_eq!(short("6c03c741fdf6abaf"), "6c03c741fdf6");
        assert_eq!(short("abc"), "abc"); // shorter than 12 → whole string
        assert_eq!(short(""), "");
    }

    /// Hermetic end-to-end check of the phase-4 git mechanics: `git()` runner,
    /// machine-identity commit, diff-hash pinning (TOCTOU baseline), and the
    /// fast-forward merge — in a throwaway repo, no spend, no TTY. Skips cleanly
    /// if `git` is unavailable.
    #[test]
    fn git_diff_pin_and_ff_merge_roundtrip() {
        let dir = std::env::temp_dir().join(format!("jarvis-impr-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if git(&dir, &["init", "-q", "-b", "main"]).is_err() {
            return; // no git in this environment → skip
        }
        let g = |args: &[&str]| git(&dir, args).unwrap();
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-qm", "init"]);
        let base = git_head(&dir, "main").unwrap();

        // branch + a machine-identity commit
        g(&["checkout", "-q", "-b", "feat"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        g(&["-c", "user.name=Jarvis", "-c", "user.email=jarvis@localhost", "commit", "-qaam", "change"]);
        let head = git_head(&dir, "feat").unwrap();
        assert_ne!(base, head);
        assert!(g(&["log", "-1", "--format=%an <%ae>"]).contains("Jarvis <jarvis@localhost>"));

        // pin the diff, confirm it is stable
        let pin = crate::util::sha256_hex(g(&["diff", &base, &head]).as_bytes());
        assert_eq!(pin, crate::util::sha256_hex(g(&["diff", &base, &head]).as_bytes()));

        // no drift → fast-forward main to the change
        g(&["checkout", "-q", "main"]);
        g(&["merge", "--ff-only", "feat"]);
        assert_eq!(git_head(&dir, "main").unwrap(), head, "main fast-forwarded");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\ntwo\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
