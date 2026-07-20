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

use crate::config::{Config, Paths};
use crate::store::db;
use crate::util;
use anyhow::{bail, ensure, Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

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

pub fn cli(_paths: &Paths, cfg: &Config, conn: &rusqlite::Connection, cmd: ImproveCmd) -> Result<()> {
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
        ImproveCmd::Draft { .. } => not_yet(2, "draft (větev + codegen + commit)"),
        ImproveCmd::Test { .. } => not_yet(3, "test (failable brána build+test)"),
        ImproveCmd::Propose { .. } => not_yet(4, "propose (zapíchnutí sha256 + nabídka)"),
        ImproveCmd::Approve { .. } => not_yet(4, "approve + merge (TTY/Telegram)"),
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
}
