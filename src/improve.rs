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
//!
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

use crate::config::{Config, ImproveCfg, Paths};
use crate::pipeline::claude;
use crate::store::db;
use crate::util;
use anyhow::{bail, ensure, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Tools handed to the codegen agent inside the worktree: it may read, edit,
/// write, and build/test — but NOT run git (Jarvis owns every git operation and
/// the gates) and NOT `cargo add`/`install`/`publish` (deps are gate-critical).
// Only fast `cargo check`/`fmt` — NOT the full `cargo test` (a slow full-crate +
// CUDA compile on this codebase; the agent ran it repeatedly and hit max_turns).
// The authoritative full test suite is Jarvis's gate; a red gate feeds the
// failure back for repair.
const DRAFT_TOOLS: &str = "Read,Edit,Write,Bash(cargo check:*),Bash(cargo fmt:*)";

// improvement sources (= improvements.source column)
pub const SRC_DIRECTED: &str = "directed"; // "Jarvisi, teach yourself X"
pub const SRC_FAILING_TEST: &str = "failing_test";
pub const SRC_CLIPPY: &str = "clippy";
pub const SRC_IDEATED: &str = "ideated"; // Jarvis decided this one itself (ideation)
pub const SRC_PLAN_ITEM: &str = "plan_item";
pub const SRC_RUNBOOK_FIX: &str = "runbook_fix";

// lifecycle statuses (= improvements.status column)
pub const STATUS_QUEUED: &str = "queued"; // task accepted, not drafted yet
pub const STATUS_DRAFTING: &str = "drafting"; // codegen running on a branch
pub const STATUS_TESTED: &str = "tested"; // branch built + tested green
pub const STATUS_PROPOSED: &str = "proposed"; // diff pinned, awaiting approval
pub const STATUS_MERGED: &str = "merged"; // landed on main (db writes the 'approved' state directly)
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
        SRC_DIRECTED | SRC_FAILING_TEST | SRC_CLIPPY | SRC_IDEATED | SRC_PLAN_ITEM | SRC_RUNBOOK_FIX
    )
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
    Deploy {
        id: i64,
        /// Show the plan only — no build, no swap, no restart
        #[arg(long)]
        dry_run: bool,
    },
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
            println!("{:>4}  {:<11} {:<40} větev", "id", "stav", "název");
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
                tick(paths, cfg, conn)
            }
        }
        ImproveCmd::Draft { id, dry_run } => draft(paths, cfg, conn, id, dry_run),
        ImproveCmd::Test { id } => test_cmd(paths, cfg, conn, id),
        ImproveCmd::Propose { id } => propose(cfg, conn, id),
        ImproveCmd::Approve { id } => approve(paths, cfg, conn, id),
        ImproveCmd::Deploy { id, dry_run } => deploy(paths, cfg, conn, id, dry_run),
    }
}

// ---------- email notifications (send-only channel) ----------

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Sends a plain notification via SendGrid (the improve layer's one-way channel;
/// approval stays at the keyboard). Errors propagate — callers treat it as
/// best-effort so a mail hiccup never blocks the pipeline.
fn email_notify(paths: &Paths, cfg: &Config, subject: &str, body_md: &str) -> Result<()> {
    let key = crate::config::sendgrid_key(paths)?;
    let html = format!(
        "<div style=\"font-family:sans-serif;max-width:640px\"><pre style=\"white-space:pre-wrap;\
         font-family:inherit\">{}</pre></div>",
        html_escape(body_md)
    );
    crate::mail::sendgrid::send(&cfg.email, &key, subject, body_md, &html)
        .context("SendGrid odeslání selhalo")?;
    Ok(())
}

/// Emails a ready-for-approval proposal. The mail carries the review command and
/// the approve command, but is explicit that approval happens at the keyboard.
fn notify_proposed(paths: &Paths, cfg: &Config, imp: &db::ImprovementRow) {
    let repo = repo_root(cfg);
    let subject = format!("Jarvis: návrh vylepšení #{} — {}", imp.id, util::truncate_chars(&imp.title, 60));
    let gate_warn = if imp.envelope == ENV_GATE_CRITICAL {
        "\n⚠ Sahá na bezpečnostní/gate soubory — pečlivá revize!"
    } else {
        ""
    };
    let body = format!(
        "Jarvis navrhuje vylepšení VLASTNÍHO kódu (napsal a otestoval si ho sám, testy zeleno).\n\n\
         #{}  {}\n\
         obálka: {}{}\n\
         diff:   {}\n\
         otisk:  {}…\n\n\
         Revize:   git -C {} diff {} {}\n\
         Schválit: jarvis improve approve {}   (jen u klávesnice — tenhle mail je oznámení, ne brána)\n\
         Zahodit:  jarvis improve dismiss {}\n",
        imp.id,
        imp.title,
        imp.envelope,
        gate_warn,
        imp.diff_stat,
        short(&imp.diff_sha256),
        repo.display(),
        short(&imp.base_commit),
        short(&imp.head_commit),
        imp.id,
        imp.id,
    );
    match email_notify(paths, cfg, &subject, &body) {
        Ok(()) => info!("improve: návrh #{} odeslán e-mailem", imp.id),
        Err(e) => warn!("improve: e-mail o návrhu #{} selhal (jen oznámení): {e:#}", imp.id),
    }
}

// ---------- self-source (opt-in) ----------

/// Compact snapshot of Jarvis's runtime state for the ideation prompt: the
/// north-star from PLAN.md plus what's failing / recurring (runbooks, patterns).
fn gather_signals(cfg: &Config, conn: &rusqlite::Connection) -> String {
    let mut s = String::new();
    if let Ok(plan) = std::fs::read_to_string(repo_root(cfg).join("PLAN.md")) {
        s.push_str("PLAN.md (výňatek — north-star):\n");
        s.push_str(&util::truncate_chars(&plan, 1400));
        s.push('\n');
    }
    if let Ok(runs) = crate::runbook::recent_runs(conn, 40) {
        let failing: Vec<String> = runs
            .iter()
            .filter(|r| r.finished_at.is_some() && !r.ok())
            .map(|r| r.name.clone())
            .collect();
        if !failing.is_empty() {
            s.push_str(&format!("\nOpakovaně padající runbooky: {}\n", failing.join(", ")));
        }
    }
    if let Ok(pats) = crate::patterns::top(conn, 2, 5) {
        if !pats.is_empty() {
            s.push_str("\nDetekované rutiny (kandidáti na automatizaci):\n");
            for p in &pats {
                s.push_str(&format!("- {} ({}×)\n", util::truncate_chars(&p.description, 80), p.occurrences));
            }
        }
    }
    if s.is_empty() {
        s.push_str("(žádné zvláštní provozní signály)\n");
    }
    s
}

/// The ideation prompt: Jarvis reads its own code + state and decides the ONE
/// highest-leverage, bounded, safe improvement to make to itself.
fn build_ideation_prompt(signals: &str) -> String {
    format!(
        "You are Jarvis, an autonomous assistant that improves its OWN Rust codebase (an \
X11 watcher + voice assistant). North-star: automate Daniel's work (see PLAN.md). You \
may Read your own source to understand yourself. Decide the SINGLE highest-leverage \
improvement to make to yourself right now.\n\n\
Your current runtime state:\n{signals}\n\n\
The improvement you pick MUST be:\n\
- concrete, bounded, and safe — small and self-contained (ideally one or two files),\n\
- genuinely useful (toward the north-star, or your robustness / quality / coverage),\n\
- FULLY covered by new unit tests — it will be auto-tested and may be auto-merged and \
deployed WITHOUT a human first, so it must be low-risk and well tested,\n\
- NOT a change to gate/safety files (config gate defaults, runbook.rs, improve.rs, \
units.rs, main.rs, telegram.rs) — those always need human review, so don't pick them.\n\
Prefer fixing a real weakness you find, hardening an edge case, or a small missing \
capability. Avoid big speculative features and risky refactors.\n\n\
Explore the code as needed, then output ONE JSON object and nothing after it:\n\
{{\"title\": \"short title\", \"spec\": \"precise, self-contained task for a coding \
agent: what to change, in which file(s), and which unit tests to add\", \"rationale\": \
\"one line: why this is high-leverage now\"}}"
    )
}

/// Autonomous ideation: Jarvis reads its code + state and queues the improvement
/// it decides on. Returns the new id, or None if it declined / errored.
fn ideate(cfg: &Config, conn: &rusqlite::Connection) -> Option<i64> {
    let im = &cfg.improve;
    let repo = repo_root(cfg);
    let outcome = match claude::run(&claude::ClaudeRequest {
        prompt: build_ideation_prompt(&gather_signals(cfg, conn)),
        model: if im.model.is_empty() { None } else { Some(im.model.as_str()) },
        cwd: &repo,
        allowed_tools: "Read",
        max_turns: 20,
        timeout: Duration::from_secs(600),
    }) {
        Ok(o) => o,
        Err(e) => {
            warn!("improve: ideace selhala: {e:#}");
            return None;
        }
    };
    let _ = db::insert_cost(conn, util::now_ts(), "improve-ideate", &im.model, outcome.tokens_in, outcome.tokens_out, outcome.cost_usd);
    let v: serde_json::Value = serde_json::from_str(claude::extract_json(&outcome.text).ok()?).ok()?;
    let spec_core = v["spec"].as_str().unwrap_or_default().trim();
    if spec_core.is_empty() {
        return None;
    }
    let rationale = v["rationale"].as_str().unwrap_or_default().trim();
    let spec = if rationale.is_empty() {
        spec_core.to_string()
    } else {
        format!("{spec_core}\n\n(Proč teď: {rationale})")
    };
    let title = match v["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => title_from_spec(&spec),
    };
    match db::insert_improvement(conn, util::now_ts(), SRC_IDEATED, &title, &spec) {
        Ok(id) => {
            info!("improve: sám jsem se rozhodl vylepšit #{id}: {title}");
            Some(id)
        }
        Err(e) => {
            warn!("improve: zápis ideovaného úkolu selhal: {e:#}");
            None
        }
    }
}

/// Self-source (opt-in): when the queue is empty, DECIDE what to improve.
/// Primary = autonomous ideation (Jarvis reads itself and chooses); fallback =
/// clippy warnings. Returns how many tasks were queued.
fn self_source(cfg: &Config, conn: &rusqlite::Connection) -> i64 {
    if ideate(cfg, conn).is_some() {
        return 1;
    }
    self_source_clippy(cfg, conn)
}

/// Fallback self-source: queue fix tasks from clippy warnings on main (deduped,
/// capped). Used only when ideation produced nothing.
fn self_source_clippy(cfg: &Config, conn: &rusqlite::Connection) -> i64 {
    let repo = repo_root(cfg);
    let out = match run_capture(
        "cargo",
        &["clippy", "--quiet", "--message-format", "short"],
        &repo,
        &[],
        Duration::from_secs(1200),
    ) {
        Ok(o) => o,
        Err(_) => return 0,
    };
    let mut queued = 0;
    let mut seen = std::collections::HashSet::new();
    for line in out.stderr.lines().chain(out.stdout.lines()) {
        let Some(idx) = line.find(": warning: ") else {
            continue;
        };
        let loc = line[..idx].trim();
        let msg = line[idx + ": warning: ".len()..].trim();
        if msg.is_empty() || !loc.contains(".rs:") || !seen.insert(loc.to_string()) {
            continue;
        }
        let spec = format!(
            "Fix this clippy warning without changing behaviour, and keep all tests green:\n\
             {loc}: {msg}\nTouch only the file involved."
        );
        let title = format!("clippy: {}", util::truncate_chars(msg, 56));
        if db::insert_improvement(conn, util::now_ts(), SRC_CLIPPY, &title, &spec).is_ok() {
            queued += 1;
        }
        if queued >= 3 {
            break;
        }
    }
    queued
}

// ---------- autonomous tick (timer-driven) ----------

/// Is the user likely at the keyboard now? (fresh sample + low idle.) Used to
/// DEFER the auto-deploy restart while Daniel is actively using Jarvis — the
/// change still merges to git; the rebuild + restart waits until he steps away.
fn user_present(cfg: &Config, conn: &rusqlite::Connection) -> bool {
    let now = util::now_ts();
    let row: Option<(i64, i64)> = conn
        .query_row("SELECT ts, idle_ms FROM samples ORDER BY ts DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok();
    match row {
        Some((ts, idle_ms)) => {
            let fresh = now - ts <= (cfg.capture.meta_interval_s as i64) * 3 + 5;
            fresh && idle_ms < 300_000 // < 5 min idle = present at the desk
        }
        None => false, // no fresh sample (capture off / away) → safe to deploy
    }
}

/// Deploys merged-but-not-deployed improvements, but only while the user is away
/// — one rebuild puts all of main's new commits live. Called at the top of each
/// tick, so daytime merges deploy as soon as Daniel steps away.
fn deploy_pending_when_away(paths: &Paths, cfg: &Config, conn: &rusqlite::Connection) {
    if !cfg.improve.deploy_enabled || user_present(cfg, conn) {
        return;
    }
    let pending = match db::merged_improvement_ids(conn) {
        Ok(p) if !p.is_empty() => p,
        _ => return,
    };
    info!("improve: nikdo u stolu → nasazuji {} smergnutých vylepšení", pending.len());
    match deploy(paths, cfg, conn, pending[0], false) {
        Ok(()) => {
            for id in &pending[1..] {
                let _ = db::set_improvement_deployed(conn, *id); // one rebuild = all of main live
            }
        }
        Err(e) => warn!("improve: odložený deploy selhal: {e:#}"),
    }
}

/// One autonomous pass (from the `jarvis-improve` timer): deploy any merged work
/// if the user is away, then — under the daily caps — draft the next task
/// (self-sourcing / ideating if the queue is empty), and on green auto-merge (+
/// deploy when away) or e-mail for review. At most one improvement per tick. All
/// errors are just logged — a timer must never abort.
pub fn tick(paths: &Paths, cfg: &Config, conn: &rusqlite::Connection) -> Result<()> {
    let im = &cfg.improve;
    if !im.enabled {
        info!("improve tick: [improve] enabled=false — nic nedělám");
        return Ok(());
    }
    // first, land any merges that were deferred while Daniel was at the desk
    deploy_pending_when_away(paths, cfg, conn);
    let day_start = util::day_bounds_local(util::today_local()).map(|(s, _)| s).unwrap_or(0);
    if db::improvement_attempts_since(conn, day_start).unwrap_or(0) >= im.daily_max as i64 {
        info!("improve tick: denní strop {} pokusů dosažen", im.daily_max);
        return Ok(());
    }
    if improve_spent_today(conn) >= im.daily_budget_usd {
        info!("improve tick: denní rozpočet {:.2} USD vyčerpán", im.daily_budget_usd);
        return Ok(());
    }
    if im.allow_self_source && db::oldest_queued_improvement(conn)?.is_none() {
        let n = self_source(cfg, conn);
        if n > 0 {
            info!("improve tick: self-source zařadil {n} úkolů");
        }
    }
    let Some(imp) = db::oldest_queued_improvement(conn)? else {
        info!("improve tick: fronta prázdná — žádná práce");
        return Ok(());
    };
    info!("improve tick: draftuji #{} „{}“", imp.id, imp.title);
    match draft(paths, cfg, conn, Some(imp.id), false) {
        Ok(()) => {
            if let Err(e) = propose(cfg, conn, imp.id) {
                warn!("improve tick: propose #{} selhal: {e:#}", imp.id);
                return Ok(());
            }
            let Ok(Some(p)) = db::improvement_by_id(conn, imp.id) else {
                return Ok(());
            };
            let (nf, nl) = parse_diff_size(&p.diff_stat);
            let small = is_small_enough(nf, nl, im.auto_merge_max_files, im.auto_merge_max_lines);
            if should_auto_merge(im.auto_merge_safe, im.auto_merge_code, &p.envelope) && small {
                info!("improve tick: #{} (obálka {}, {nf} souborů/{nl} řádků) → auto-merge, jen informuju", p.id, p.envelope);
                let _ = db::set_improvement_approved(conn, p.id, "auto");
                match merge_improvement(paths, cfg, conn, &p) {
                    Ok(_) => {
                        if im.deploy_enabled {
                            if user_present(cfg, conn) {
                                info!("improve tick: #{} smergnuto; u stolu → deploy odložen (nasadí se, až budeš pryč)", p.id);
                            } else if let Err(e) = deploy(paths, cfg, conn, p.id, false) {
                                warn!("improve tick: auto-deploy #{} selhal: {e:#}", p.id);
                            }
                        }

                        if let Ok(Some(done)) = db::improvement_by_id(conn, p.id) {
                            let acted = match done.status.as_str() {
                                STATUS_DEPLOYED => "smergnuto + NASAZENO (rebuild + restart)",
                                STATUS_MERGED => "smergnuto do main (deploy čeká / vypnut)",
                                other => other,
                            };
                            let subj = format!("Jarvis se sám vylepšil: #{} — {}", p.id, util::truncate_chars(&p.title, 50));
                            let body = format!(
                                "Sám jsem se rozhodl a provedl vylepšení VLASTNÍHO kódu — jen tě informuju.\n\n\
                                 #{}  {}\nzdroj: {}   obálka: {}\nstav: {}\n\n\
                                 Diff je v gitu (autor Jarvis). Kdyby se nelíbilo: `git -C {} revert <commit>` + přebuild.\n",
                                p.id, p.title, p.source, p.envelope, acted, repo_root(cfg).display()
                            );
                            let _ = email_notify(paths, cfg, &subj, &body);
                        }
                    }
                    Err(e) => {
                        warn!("improve tick: auto-merge #{} selhal ({e:#}) — nechávám na tobě", p.id);
                        notify_proposed(paths, cfg, &p);
                    }
                }
            } else {
                if should_auto_merge(im.auto_merge_safe, im.auto_merge_code, &p.envelope) && !small {
                    info!("improve tick: #{} je velká ({nf} souborů / {nl} řádků > strop {}/{}) → k tvému schválení", p.id, im.auto_merge_max_files, im.auto_merge_max_lines);
                }
                // gate-critical, too big, or flags off → human review by e-mail
                notify_proposed(paths, cfg, &p);
            }
        }
        Err(e) => info!("improve tick: draft #{} neprošel bránou: {e:#}", imp.id),
    }
    Ok(())
}

/// `jarvis improve tick --dry-run`: config envelope + ledger tally + repo
/// readiness. Read-only, no API — a real check against the live DB.
pub fn run_dry(cfg: &Config, conn: &rusqlite::Connection) -> Result<()> {
    let im = &cfg.improve;
    println!(
        "Sebe-vývoj: enabled={} | self-source={} | auto-merge: docs={}/kód={} | deploy={}",
        im.enabled, im.allow_self_source, im.auto_merge_safe, im.auto_merge_code, im.deploy_enabled
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
- You have Read/Edit/Write and `cargo check` (fast compile check) + `cargo fmt`. You do \
NOT have the full test suite (it is slow) — write tests but do NOT run `cargo test`; the \
full suite is run for you afterward and any failure comes back for you to fix. No git.\n\
- Work EFFICIENTLY — limited turn budget. Read ONLY the file(s) you will change (grep for \
what you need; don't read the whole codebase). Make all edits, then `cargo check` once.\n\n\
WHEN DONE, output ONE JSON object and nothing after it:\n\
{{\"summary\": \"one line: what you changed\", \"files_changed\": [\"src/...\"], \
\"tests_added\": <int>, \"touched_gate_files\": <bool>}}\n\n\
Title: {title}"
    )
}

#[derive(Debug, Default)]
struct DraftResult {
    summary: String,
}

/// Best-effort parse of the agent's JSON result — only the one-line summary is
/// consumed (Jarvis computes the authoritative diff/envelope/tests from git).
fn parse_draft_result(text: &str) -> Option<DraftResult> {
    let json = claude::extract_json(text).ok()?;
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(DraftResult {
        summary: v["summary"].as_str().unwrap_or_default().to_string(),
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
not drop); reuse existing helpers; do not add crates; you have Read/Edit/Write and fast \
`cargo check` (NOT the full suite) but NOT git. Fix the SPECIFIC failure above \
efficiently; the full suite is re-run for you. Then output ONE JSON object:\n\
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
    let test = run_capture("cargo", &["test"], worktree, &envs, timeout)?;
    let combined = format!("{}\n{}", test.stdout, test.stderr);
    let passed = !test.timed_out && test.code == Some(0);
    let mut output = util::truncate_chars(combined.trim(), 6000);
    if let Some((ok, n)) = parse_test_summary(&combined) {
        output.push_str(&format!("\n[jarvis] cargo test: {n} testů ({})", if ok { "ok" } else { "SELHALO" }));
    }
    if test.timed_out {
        output.push_str(&format!("\n[jarvis] cargo test zabit po {} s", timeout.as_secs()));
    }
    Ok(GateOutcome { passed, output })
}

/// `jarvis improve draft [id] [--dry-run]`: branch off committed main, run
/// codegen in an isolated worktree, commit under the machine identity, then run
/// the integrity + build/test gate. Ship-dark: the live path needs `enabled`.
// ---------- staged (plan-then-build) mode ----------

#[derive(Debug, Clone)]
struct PlanStep {
    title: String,
    spec: String,
}

/// Planner prompt: decompose a task into the fewest ordered, independently-built
/// steps (one step if it's already small).
fn build_plan_prompt(spec: &str, max_steps: usize) -> String {
    format!(
        "You are Jarvis planning a change to your OWN Rust codebase (an X11 watcher + \
voice assistant). Decompose the TASK into the FEWEST ordered steps — each a small, \
self-contained, testable unit that leaves the code COMPILING. If the task is already \
small, return exactly ONE step. Use AT MOST {max_steps} steps. You may Read the code \
to plan well.\n\n\
TASK: {spec}\n\n\
Output ONE JSON object and nothing after it:\n\
{{\"steps\": [{{\"title\": \"short step title\", \"spec\": \"precise instructions for \
this step: what to change, in which file(s), and which unit tests to add\"}}]}}"
    )
}

/// Per-step build prompt for staged mode (earlier steps are already applied).
fn build_step_prompt(overall_title: &str, i: usize, total: usize, step: &PlanStep) -> String {
    format!(
        "You are Jarvis implementing your OWN Rust codebase — STEP {i} OF {total} of a \
larger change. Overall goal: \"{overall_title}\". Earlier steps are ALREADY applied in \
the working tree; do ONLY this step.\n\n\
STEP {i}/{total} — {}:\n{}\n\n\
House rules: English comments (brief — WHY, not what); match the surrounding style; add \
unit tests for what you add; NEVER weaken, delete, or #[ignore] existing tests (the test \
count must not drop); reuse existing helpers; do not add crates unless essential; you \
have Read/Edit/Write and fast `cargo check` (NOT the slow full test suite — it is run for \
you afterward) but NOT git. Read only what you need; work within your turn budget. Leave \
the code COMPILING at the end of this step. When done, output ONE JSON object: \
{{\"summary\": \"what this step did\"}}",
        step.title, step.spec
    )
}

/// Parses the planner's JSON into ordered steps (capped). Empty/garbage → None.
fn parse_plan(text: &str, max_steps: usize) -> Option<Vec<PlanStep>> {
    let v: serde_json::Value = serde_json::from_str(claude::extract_json(text).ok()?).ok()?;
    let steps: Vec<PlanStep> = v["steps"]
        .as_array()?
        .iter()
        .filter_map(|s| {
            let spec = s["spec"].as_str().unwrap_or_default().trim();
            if spec.is_empty() {
                return None;
            }
            let title = match s["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                Some(t) => t.to_string(),
                None => title_from_spec(spec),
            };
            Some(PlanStep { title, spec: spec.to_string() })
        })
        .take(max_steps)
        .collect();
    (!steps.is_empty()).then_some(steps)
}

/// Plans the task into steps. A single-step plan (or any failure) = one ordinary
/// draft over the whole spec. Costs one planner call for multi-step tasks.
fn plan_task(
    cfg: &Config,
    conn: &rusqlite::Connection,
    imp: &db::ImprovementRow,
    total_cost: &mut f64,
) -> Vec<PlanStep> {
    let im = &cfg.improve;
    let single = vec![PlanStep { title: util::truncate_chars(&imp.title, 60), spec: imp.spec.clone() }];
    if im.plan_max_steps <= 1 {
        return single;
    }
    let outcome = match claude::run(&claude::ClaudeRequest {
        prompt: build_plan_prompt(&imp.spec, im.plan_max_steps),
        model: if im.model.is_empty() { None } else { Some(im.model.as_str()) },
        cwd: &repo_root(cfg),
        allowed_tools: "Read",
        max_turns: 12,
        timeout: Duration::from_secs(600),
    }) {
        Ok(o) => o,
        Err(e) => {
            warn!("improve: plán selhal ({e:#}) — dělám jedním draftem");
            return single;
        }
    };
    *total_cost += outcome.cost_usd;
    let _ = db::insert_cost(conn, util::now_ts(), "improve-plan", &im.model, outcome.tokens_in, outcome.tokens_out, outcome.cost_usd);
    parse_plan(&outcome.text, im.plan_max_steps).unwrap_or(single)
}

fn notify_staged_start(paths: &Paths, cfg: &Config, imp: &db::ImprovementRow, steps: &[PlanStep]) {
    let mut plan = String::new();
    for (i, s) in steps.iter().enumerate() {
        plan.push_str(&format!("  {}. {}\n", i + 1, util::truncate_chars(&s.title, 80)));
    }
    let subj = format!("Jarvis začal velký úkol #{} — {} kroků", imp.id, steps.len());
    let body = format!(
        "Rozhodl jsem se na větším vylepšení a rozložil ho na {} kroků. Budu tě informovat průběžně.\n\n\
         #{}  {}\nzdroj: {}\n\nPlán:\n{}",
        steps.len(),
        imp.id,
        imp.title,
        imp.source,
        plan
    );
    let _ = email_notify(paths, cfg, &subj, &body);
}

fn notify_staged_step(paths: &Paths, cfg: &Config, imp: &db::ImprovementRow, i: usize, total: usize, step: &PlanStep) {
    let subj = format!("Jarvis #{}: krok {}/{} — {}", imp.id, i, total, util::truncate_chars(&step.title, 50));
    let body = format!("Velký úkol #{} „{}“:\nkrok {}/{}: {}\n", imp.id, imp.title, i, total, step.title);
    let _ = email_notify(paths, cfg, &subj, &body);
}

fn notify_staged_failed(paths: &Paths, cfg: &Config, imp: &db::ImprovementRow, i: usize, total: usize, reason: &str) {
    let subj = format!("Jarvis: velký úkol #{} SELHAL (krok {}/{})", imp.id, i, total);
    let body = format!(
        "#{}  {}\nselhalo u kroku {}/{}: {}\n\nVětev zůstává k inspekci: jarvis improve show {}\n",
        imp.id,
        imp.title,
        i,
        total,
        util::truncate_chars(reason, 500),
        imp.id
    );
    let _ = email_notify(paths, cfg, &subj, &body);
}

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

    if dry_run {
        println!("── improve draft #{} (dry-run — nic se nespustí) ──", imp.id);
        println!("stav:      {}", imp.status);
        println!("repo:      {} {}", repo.display(), if repo.join(".git").exists() { "(git ✓)" } else { "(⚠ není git)" });
        println!("větev:     {branch}");
        println!("worktree:  {}", wt.display());
        println!("model:     {}", if im.model.is_empty() { "<default CLI>" } else { &im.model });
        println!("nástroje:  {DRAFT_TOOLS}");
        println!("max_turns: {} | timeout: {}s | plan_max_steps: {} | enabled: {}", im.max_turns, im.timeout_s, im.plan_max_steps, im.enabled);
        println!("(úkol se nejdřív naplánuje: 1 krok = běžný draft, víc = staged build)");
        println!("\n── prompt (base draft) ──\n{}", build_draft_prompt(&imp.spec, &imp.title));
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
    let mut total_cost = 0.0;
    let mut summary = String::new();

    // 1) PLAN the task into steps (1 = ordinary draft, >1 = staged build)
    let steps = plan_task(cfg, conn, &imp, &mut total_cost);
    let staged = steps.len() > 1;
    info!(
        "improve #{}: {} — {} krok(ů), base {base_tests} testů",
        imp.id,
        if staged { "STAGED" } else { "draft" },
        steps.len()
    );
    if staged {
        notify_staged_start(paths, cfg, &imp, &steps);
    }

    // 2) BUILD each step, accumulating in the worktree with a checkpoint commit
    for (i, step) in steps.iter().enumerate() {
        if improve_spent_today(conn) >= im.daily_budget_usd {
            warn!("improve #{}: rozpočet vyčerpán u kroku {}/{} — stavím jen co mám", imp.id, i + 1, steps.len());
            break;
        }
        if staged {
            info!("improve #{}: krok {}/{}: {}", imp.id, i + 1, steps.len(), step.title);
            notify_staged_step(paths, cfg, &imp, i + 1, steps.len(), step);
        }
        let prompt = if staged {
            build_step_prompt(&imp.title, i + 1, steps.len(), step)
        } else {
            build_draft_prompt(&imp.spec, &imp.title)
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
                if i == 0 && worktree_is_clean(&wt).unwrap_or(true) {
                    cleanup_worktree(&repo, &wt); // nothing worth keeping
                }
                if staged {
                    notify_staged_failed(paths, cfg, &imp, i + 1, steps.len(), &format!("codegen: {e:#}"));
                }
                return Err(e).context("codegen selhal");
            }
        };
        total_cost += outcome.cost_usd;
        db::add_improvement_cost(conn, imp.id, outcome.cost_usd, outcome.tokens_in, outcome.tokens_out)?;
        let _ = db::insert_cost(conn, util::now_ts(), if staged { "improve-step" } else { "improve" }, &im.model, outcome.tokens_in, outcome.tokens_out, outcome.cost_usd);
        if let Some(r) = parse_draft_result(&outcome.text) {
            if !r.summary.is_empty() {
                summary = r.summary;
            }
        }
        // checkpoint this step (earlier steps stay committed even if a later one fails)
        if !worktree_is_clean(&wt)? {
            let msg = if staged {
                format!("{}: krok {}/{} — {}\n\nJarvis-Improvement: {}\n", util::truncate_chars(&imp.title, 60), i + 1, steps.len(), step.title, imp.id)
            } else {
                draft_commit_message(imp.id, &imp.title, None)
            };
            commit_all(&wt, im, &msg)?;
        }
    }

    // 3) did anything actually change? then record metadata + integrity-guard
    let files = changed_files(&wt, &base)?;
    if files.is_empty() {
        db::update_improvement_tests(conn, imp.id, None, "agent neprovedl žádnou změnu", STATUS_FAILED)?;
        cleanup_worktree(&repo, &wt);
        bail!("codegen neprovedl žádnou změnu (#{} → failed)", imp.id);
    }
    let mut head = git_head(&wt, "HEAD")?;
    let mut envelope = classify_envelope(&files);
    let mut diff_stat = git(&wt, &["diff", "--shortstat", &base, "HEAD"])?.trim().to_string();
    db::update_improvement_draft(conn, imp.id, &branch, &base, &head, envelope, &diff_stat, STATUS_DRAFTING)?;
    let mut head_tests = count_test_attrs(&wt);
    if !test_integrity_ok(base_tests, head_tests) {
        let note = format!("test-integrity FAIL: base {base_tests} → head {head_tests} (testy nesmí ubývat)");
        db::update_improvement_tests(conn, imp.id, Some(false), &note, STATUS_FAILED)?;
        if staged {
            notify_staged_failed(paths, cfg, &imp, steps.len(), steps.len(), &note);
        }
        bail!("#{}: {note} — zamítnuto (větev {branch} zůstává k inspekci)", imp.id);
    }

    // 4) authoritative gate on the WHOLE change + bounded self-repair
    let max_gate_attempts = 1 + im.repair_attempts;
    for attempt in 0..max_gate_attempts {
        let gate = run_gate(&wt, &repo, im)?;
        if gate.passed {
            db::update_improvement_tests(conn, imp.id, Some(true), &gate.output, STATUS_TESTED)?;
            println!("── improve draft #{} ──", imp.id);
            println!("větev:   {branch}");
            println!("commit:  {}", short(&head));
            println!("obálka:  {envelope}");
            println!("diff:    {diff_stat}");
            println!("kroky:   {} | testy: base {base_tests} → head {head_tests}; brána ✓ ZELENÁ", steps.len());
            if !summary.is_empty() {
                println!("shrnutí: {}", util::truncate_chars(&summary, 200));
            }
            println!("náklad:  {total_cost:.4} USD");
            println!("\n✓ připraveno: jarvis improve show {} → propose/approve", imp.id);
            return Ok(());
        }
        db::update_improvement_tests(conn, imp.id, Some(false), &gate.output, STATUS_FAILED)?;
        if attempt + 1 >= max_gate_attempts || improve_spent_today(conn) >= im.daily_budget_usd {
            let reason = format!("brána červená ({} pokus/ů; limit oprav nebo rozpočtu)", attempt + 1);
            println!("── improve draft #{} ── ✗ {reason}. Log: jarvis improve show {}", imp.id, imp.id);
            if staged {
                notify_staged_failed(paths, cfg, &imp, steps.len(), steps.len(), &reason);
            }
            bail!("{reason} (#{} → failed)", imp.id);
        }
        info!("improve #{}: brána ČERVENÁ — oprava {}/{}", imp.id, attempt + 1, im.repair_attempts);
        let outcome = match claude::run(&claude::ClaudeRequest {
            prompt: build_repair_prompt(&imp.spec, &gate.output),
            model,
            cwd: &wt,
            allowed_tools: DRAFT_TOOLS,
            max_turns: im.max_turns,
            timeout: Duration::from_secs(im.timeout_s),
        }) {
            Ok(o) => o,
            Err(e) => {
                warn!("improve #{}: oprava selhala: {e:#}", imp.id);
                break;
            }
        };
        total_cost += outcome.cost_usd;
        db::add_improvement_cost(conn, imp.id, outcome.cost_usd, outcome.tokens_in, outcome.tokens_out)?;
        let _ = db::insert_cost(conn, util::now_ts(), "improve-repair", &im.model, outcome.tokens_in, outcome.tokens_out, outcome.cost_usd);
        if worktree_is_clean(&wt)? {
            break; // repair changed nothing → give up
        }
        commit_all(&wt, im, &format!("{}: oprava\n\nJarvis-Improvement: {}\n", util::truncate_chars(&imp.title, 60), imp.id))?;
        let files = changed_files(&wt, &base)?;
        head = git_head(&wt, "HEAD")?;
        envelope = classify_envelope(&files);
        diff_stat = git(&wt, &["diff", "--shortstat", &base, "HEAD"])?.trim().to_string();
        db::update_improvement_draft(conn, imp.id, &branch, &base, &head, envelope, &diff_stat, STATUS_DRAFTING)?;
        head_tests = count_test_attrs(&wt);
        if !test_integrity_ok(base_tests, head_tests) {
            let note = format!("test-integrity FAIL po opravě: {base_tests} → {head_tests}");
            db::update_improvement_tests(conn, imp.id, Some(false), &note, STATUS_FAILED)?;
            bail!("#{}: {note}", imp.id);
        }
    }
    let reason = "brána červená (oprava nezabrala)";
    if staged {
        notify_staged_failed(paths, cfg, &imp, steps.len(), steps.len(), reason);
    }
    bail!("{reason} (#{} → failed)", imp.id);
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
    // merge_improvement re-verifies drift/TOCTOU/clean-tree again — the tree
    // could have changed while the human was typing the confirmation
    let new_head = merge_improvement(paths, cfg, conn, &imp)?;
    println!("✓ #{id} smergnuto do main (HEAD {})", short(&new_head));
    println!("  commit autor: {} <{}>", cfg.improve.author_name, cfg.improve.author_email);
    finish_or_deploy(paths, cfg, conn, &imp)
}

// ---------- merge + deploy (phase 4/6 shared) ----------

/// Re-verifies (no drift, pinned diff, clean tree) then fast-forwards main to
/// the improvement's commit and records the merge + cleanup. Authorization is
/// the CALLER's job (TTY confirm in `approve`, or safe-class auto-merge in
/// `tick`). Runs the TOCTOU re-check itself, so it is safe after any delay.
fn merge_improvement(
    paths: &Paths,
    cfg: &Config,
    conn: &rusqlite::Connection,
    imp: &db::ImprovementRow,
) -> Result<String> {
    let repo = repo_root(cfg);
    let main_head = git_head(&repo, "main")?;
    ensure!(main_head == imp.base_commit, "main se pohnul od návrhu (#{}) — nutný re-draft", imp.id);
    let diff = git(&repo, &["diff", &imp.base_commit, &imp.head_commit])?;
    ensure!(
        util::sha256_hex(diff.as_bytes()) == imp.diff_sha256,
        "diff #{} se změnil (otisk nesedí) — NEMERGUJI",
        imp.id
    );
    // fast-forward the `main` REF without checking it out — improve never touches
    // the user's working tree or whatever branch they have checked out.
    git(&repo, &["fetch", ".", &format!("{}:main", imp.branch)])
        .with_context(|| format!("ff main←{} selhal (drift/ne-ff?) — #{} zůstává, řeš ručně", imp.branch, imp.id))?;
    db::set_improvement_merged(conn, imp.id)?;
    cleanup_worktree(&repo, &worktree_path(paths, imp.id));
    let _ = git(&repo, &["branch", "-d", &imp.branch]);
    git_head(&repo, "main")
}

/// Should `tick` auto-merge this green change without asking? Docs (safe) when
/// auto_merge_safe; ordinary code (feature) when auto_merge_code; gate-critical
/// NEVER — a self-editing agent must not rewrite its own gates unreviewed, so
/// those always wait for a human even with both flags on.
pub fn should_auto_merge(auto_merge_safe: bool, auto_merge_code: bool, envelope: &str) -> bool {
    match envelope {
        ENV_GATE_CRITICAL => false,
        ENV_SAFE => auto_merge_safe,
        _ => auto_merge_code,
    }
}

/// Parses `git diff --shortstat` ("N files changed, X insertions(+), Y
/// deletions(-)") into (files, lines-changed). Missing parts count as 0.
fn parse_diff_size(diff_stat: &str) -> (usize, usize) {
    let num_before = |kw: &str| -> usize {
        diff_stat
            .split(kw)
            .next()
            .and_then(|pre| pre.split_whitespace().last())
            .and_then(|t| t.parse().ok())
            .unwrap_or(0)
    };
    (num_before("file"), num_before("insertion") + num_before("deletion"))
}

/// Auto-merge size cap: a change within BOTH limits may auto-merge; anything
/// bigger has too large a blast radius → human review even if otherwise eligible.
fn is_small_enough(files: usize, lines: usize, max_files: usize, max_lines: usize) -> bool {
    files <= max_files && lines <= max_lines
}

/// After a merge: auto-deploy when enabled, otherwise print the manual step.
fn finish_or_deploy(paths: &Paths, cfg: &Config, conn: &rusqlite::Connection, imp: &db::ImprovementRow) -> Result<()> {
    if cfg.improve.deploy_enabled {
        println!("  deploy_enabled=true → nasazuji…");
        deploy(paths, cfg, conn, imp.id, false)
    } else {
        println!("  nasazení ručně: cargo install --path . --force  (deploy_enabled=false)");
        Ok(())
    }
}

/// Where `cargo install` places the binary (CARGO_HOME/bin, else ~/.cargo/bin).
fn install_path() -> PathBuf {
    if let Some(ch) = std::env::var_os("CARGO_HOME") {
        return PathBuf::from(ch).join("bin").join("jarvis");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".cargo/bin/jarvis")
}

/// Smoke-tests a freshly built binary WITHOUT touching the daemons: it must
/// report a version and survive a real config-load + DB-open (`improve tick
/// --dry-run`). Enough to catch a broken build before we restart anything.
fn smoke_test(bin: &Path) -> Result<()> {
    let bin_s = bin.to_str().context("binárka: cesta není UTF-8")?;
    let tmp = std::env::temp_dir();
    let v = run_capture(bin_s, &["--version"], &tmp, &[], Duration::from_secs(30))?;
    ensure!(
        v.code == Some(0) && v.stdout.to_lowercase().contains("jarvis"),
        "`{bin_s} --version` neproběhl (exit {:?})",
        v.code
    );
    let t = run_capture(bin_s, &["improve", "tick", "--dry-run"], &tmp, &[], Duration::from_secs(60))?;
    ensure!(t.code == Some(0), "`improve tick --dry-run` na nové binárce selhal (exit {:?})", t.code);
    Ok(())
}

/// After a restart, are the daemons that should be running actually active?
/// Checks capture (always on) and listen (if enabled). A crash-on-boot from a
/// bad deploy shows up here → rollback.
fn daemons_healthy(cfg: &Config) -> bool {
    let mut svcs = vec!["jarvis-capture.service"];
    if cfg.listen.enabled {
        svcs.push("jarvis-listen.service");
    }
    svcs.iter().all(|svc| {
        matches!(
            run_capture("systemctl", &["--user", "is-active", svc], &std::env::temp_dir(), &[], Duration::from_secs(15)),
            Ok(o) if o.stdout.trim() == "active"
        )
    })
}

fn notify_deploy(paths: &Paths, cfg: &Config, imp: &db::ImprovementRow, ok: bool, detail: &str) {
    let subject = if ok {
        format!("Jarvis: nasazeno vylepšení #{} — {}", imp.id, util::truncate_chars(&imp.title, 50))
    } else {
        format!("Jarvis: deploy #{} VRÁCEN zpět", imp.id)
    };
    let head = if ok { "✓ NASAZENO (rebuild + smoke + restart)" } else { "✗ VRÁCENO na .prev" };
    let body = format!("#{}  {}\n{head}\n{detail}\n", imp.id, imp.title);
    if let Err(e) = email_notify(paths, cfg, &subject, &body) {
        warn!("improve: e-mail o deploy #{} selhal: {e:#}", imp.id);
    }
}

/// `jarvis improve deploy <id>`: rebuild + install the merged change, smoke-test
/// the new binary, and only if it passes reinstall units + restart the daemons.
/// On ANY failure the previous binary is restored from `.prev`. Gated by
/// deploy_enabled; the highest-stakes step, so every path fails safe.
fn deploy(paths: &Paths, cfg: &Config, conn: &rusqlite::Connection, id: i64, dry_run: bool) -> Result<()> {
    let im = &cfg.improve;
    let imp = db::improvement_by_id(conn, id)?.with_context(|| format!("vylepšení #{id} neexistuje"))?;
    let repo = repo_root(cfg);
    let bin = install_path();
    let prev = bin.with_extension("prev");
    let bin_s = bin.to_string_lossy().to_string();

    if dry_run {
        println!("── improve deploy #{id} (dry-run — nic se nespustí) ──");
        println!("stav:          {}", imp.status);
        println!("deploy_enabled: {}", im.deploy_enabled);
        println!("binárka:       {}", bin.display());
        println!("záloha:        {}", prev.display());
        println!("1) build:      cargo install z čerstvého worktree na main (--force)");
        println!("2) smoke:      {bin_s} --version  +  improve tick --dry-run");
        println!("3) units+restart: {bin_s} install-units  +  systemctl --user restart jarvis-capture/listen");
        println!("rollback:      smoke/health FAIL → obnovit {} → restart", prev.display());
        return Ok(());
    }

    ensure!(im.deploy_enabled, "[improve] deploy_enabled=false — self-deploy je vypnutý");
    ensure!(imp.status == STATUS_MERGED, "deploy jde jen na 'merged' (#{id} je '{}')", imp.status);
    ensure!(bin.parent().map(|p| p.exists()).unwrap_or(false), "cíl instalace {} neexistuje", bin.display());

    // 1. back up the current (running) binary — the process keeps its old inode
    if bin.exists() {
        std::fs::copy(&bin, &prev)
            .with_context(|| format!("záloha {} → {} selhala", bin.display(), prev.display()))?;
        info!("deploy #{id}: záloha do {}", prev.display());
    }

    // 2. build + install release from a FRESH detached worktree on main — never
    // the user's current working tree / branch (which may hold unrelated WIP).
    let build_wt = paths.data_dir.join("improve").join("deploy-main");
    cleanup_worktree(&repo, &build_wt);
    let build_wt_s = build_wt.to_str().context("build worktree: cesta není UTF-8")?;
    git(&repo, &["worktree", "add", "--detach", build_wt_s, "main"])
        .context("worktree na main pro build selhal")?;
    info!("deploy #{id}: cargo install z {build_wt_s} (main, release — chvíli to trvá)");
    let inst = run_capture("cargo", &["install", "--path", build_wt_s, "--force"], &build_wt, &[], Duration::from_secs(im.timeout_s.max(2400)))?;
    cleanup_worktree(&repo, &build_wt);
    if inst.timed_out || inst.code != Some(0) {
        db::set_improvement_status(conn, id, STATUS_MERGED, "cargo install selhal (binárka nezměněna)")?;
        bail!("cargo install selhal — stará binárka zůstává:\n{}", util::truncate_chars(inst.stderr.trim(), 1200));
    }

    // 3. smoke-test the NEW binary before touching any daemon
    if let Err(e) = smoke_test(&bin) {
        if prev.exists() {
            let _ = std::fs::copy(&prev, &bin);
        }
        db::set_improvement_status(conn, id, STATUS_ROLLED_BACK, &format!("smoke selhal: {e:#}"))?;
        notify_deploy(paths, cfg, &imp, false, &format!("smoke-test selhal: {e:#}"));
        bail!("smoke-test nové binárky selhal — vráceno na .prev: {e:#}");
    }

    // 4. reinstall units (path/features may have changed) + restart daemons
    let _ = run_capture(&bin_s, &["install-units"], &repo, &[], Duration::from_secs(60));
    let _ = run_capture(
        "systemctl",
        &["--user", "restart", "jarvis-capture.service", "jarvis-listen.service"],
        &repo,
        &[],
        Duration::from_secs(60),
    );

    // 5. let services settle, then health-check; roll back if a daemon didn't
    // come up on the new binary
    std::thread::sleep(Duration::from_secs(4));
    if !daemons_healthy(cfg) {
        if prev.exists() {
            let _ = std::fs::copy(&prev, &bin);
        }
        let _ = run_capture("systemctl", &["--user", "restart", "jarvis-capture.service"], &repo, &[], Duration::from_secs(60));
        db::set_improvement_status(conn, id, STATUS_ROLLED_BACK, "démon po deploy nenaběhl, vráceno")?;
        notify_deploy(paths, cfg, &imp, false, "démon po restartu nebyl 'active' — vráceno na .prev");
        bail!("démon po deploy nenaběhl — vráceno na .prev");
    }

    db::set_improvement_deployed(conn, id)?;
    notify_deploy(paths, cfg, &imp, true, &format!("HEAD {}", short(&imp.head_commit)));
    println!("✓ #{id} nasazeno: binárka přebuilděna, smoke OK, démon restartován (.prev = rollback).");
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
    fn parse_draft_result_reads_summary_from_json() {
        let text = "Hotovo.\n```json\n{\"summary\":\"add RSS reader\",\"tests_added\":3}\n```";
        assert_eq!(parse_draft_result(text).unwrap().summary, "add RSS reader");
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
    fn auto_merge_policy_matrix() {
        // docs (safe): governed by auto_merge_safe
        assert!(should_auto_merge(true, false, ENV_SAFE));
        assert!(!should_auto_merge(false, false, ENV_SAFE));
        // ordinary code (feature): governed by auto_merge_code
        assert!(should_auto_merge(false, true, ENV_FEATURE), "auto_merge_code → code auto-merges");
        assert!(!should_auto_merge(true, false, ENV_FEATURE), "no code flag → human");
        // gate-critical: NEVER, even with both flags on
        assert!(!should_auto_merge(true, true, ENV_GATE_CRITICAL), "gate code never auto-merges");
    }

    #[test]
    fn diff_size_parsing_and_cap() {
        assert_eq!(parse_diff_size(" 3 files changed, 45 insertions(+), 12 deletions(-)"), (3, 57));
        assert_eq!(parse_diff_size(" 1 file changed, 2 insertions(+)"), (1, 2));
        assert_eq!(parse_diff_size(" 1 file changed, 5 deletions(-)"), (1, 5));
        assert_eq!(parse_diff_size(""), (0, 0));
        assert!(is_small_enough(3, 150, 3, 150), "on the limit = still small");
        assert!(!is_small_enough(4, 10, 3, 150), "too many files → human");
        assert!(!is_small_enough(1, 200, 3, 150), "too many lines → human");
    }

    #[test]
    fn parse_plan_reads_steps_capped() {
        let text = "Plán:\n```json\n{\"steps\":[{\"title\":\"A\",\"spec\":\"do A\"},{\"title\":\"B\",\"spec\":\"do B\"},{\"title\":\"C\",\"spec\":\"do C\"}]}\n```";
        let steps = parse_plan(text, 2).unwrap();
        assert_eq!(steps.len(), 2, "capped at max_steps");
        assert_eq!(steps[0].title, "A");
        assert_eq!(steps[0].spec, "do A");
        // empty-spec steps dropped; garbage → None
        assert!(parse_plan("{\"steps\":[{\"title\":\"x\",\"spec\":\"\"}]}", 5).is_none());
        assert!(parse_plan("no json", 5).is_none());
        // a titleless step falls back to a title derived from its spec
        let one = parse_plan("{\"steps\":[{\"spec\":\"add mask helper\"}]}", 5).unwrap();
        assert_eq!(one[0].title, "add mask helper");
    }

    #[test]
    fn ideation_prompt_constrains_scope() {
        let p = build_ideation_prompt("Padající runbooky: záloha");
        assert!(p.contains("Padající runbooky: záloha"), "signals are included");
        assert!(p.contains("highest-leverage"));
        assert!(p.contains("gate/safety files"), "steers away from gate code");
        assert!(p.contains("auto-merged") && p.contains("deployed"), "warns it may auto-land");
        assert!(p.contains("JSON"));
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
        let r = DraftResult { summary: "add RSS".into() };
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
