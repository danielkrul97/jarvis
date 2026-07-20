//! Proactive layer (the "nervous system") — Jarvis offers timely action on its
//! own, from observation. Until now everything is reactive: it only speaks
//! when addressed, or in batch (digest, proposal announcement). Here,
//! observation (patterns, runbook runs, utterances) becomes a realtime nudge:
//! spoken at the desk, otherwise via Telegram.
//!
//! **Safety invariant**: nudge NEVER runs anything unapproved. It may only
//!   - inform (no action),
//!   - offer to run an ALREADY-approved runbook (reuses `runbook::run_one`),
//!   - offer to generate a proposal (`patterns::propose` → still goes through approve).
//!
//! **Bias to silence**: interrupting is a costlier mistake than occasionally
//! not offering. Two kinds of detectors: deterministic (pattern crossed
//! threshold, runbook repeatedly failing) deliver straight through local
//! gates; fuzzy (a commitment in speech) is additionally judged by a
//! skeptical classifier (Tier 2, same philosophy as open-ear converse). The
//! layer defaults to disabled and has a tunable kill-gate (`jarvis nudge-eval`).
//!
//! Layers are pure and testable (detectors, gates, channel selection, confirm
//! parser); DB/API/voice are just a thin shell around them (`tick`, `deliver`).

use crate::config::{Config, Paths};
use crate::pipeline::claude;
use crate::store::db;
use crate::{screen, speak, util};
use anyhow::{Context, Result};
use chrono::Timelike;
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};

// nudge kinds (= nudges.kind column)
pub const KIND_PATTERN_READY: &str = "pattern_ready";
pub const KIND_RUNBOOK_FAILING: &str = "runbook_failing";
pub const KIND_COMMITMENT: &str = "commitment";

// action kinds (= nudges.action_kind column)
pub const ACT_INFORM: &str = "inform";
pub const ACT_RUN_RUNBOOK: &str = "run_runbook";
pub const ACT_PROPOSE: &str = "propose";

/// Coarse local commitment markers in speech (fuzzy detector; classifier confirms).
const COMMIT_MARKERS: &[&str] = &[
    "pošlu", "pošli", "napíšu", "napíš", "udělám", "zavolám", "odpovím", "dodělám",
    "připomeň", "nezapomeň", "zařídím", "vyřídím", "domluvím", "objednám",
];

// ---------- candidates and detectors (pure) ----------

/// A signal that PASSED a detector, but not yet the gates/classifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub kind: &'static str,
    /// Cooldown key — the same subject won't repeat right away.
    pub dedup_key: String,
    /// Human-readable evidence (goes into both the nudge and the classifier).
    pub evidence: String,
    pub action_kind: &'static str,
    /// runbook id/name or pattern id ("" for inform).
    pub action_ref: String,
    /// Needs the Tier-2 classifier (fuzzy signals), or deliver directly?
    pub needs_classifier: bool,
}

/// A pattern ready to offer (from the patterns table via `patterns::top`).
#[derive(Debug, Clone)]
pub struct PatternSig {
    pub id: i64,
    pub description: String,
    pub occurrences: i64,
    pub status: String,
}

/// One runbook run (from `runbook::recent_runs`, newest first).
#[derive(Debug, Clone)]
pub struct RunSig {
    pub runbook_id: i64,
    pub name: String,
    /// Finished run? (unfinished = running/crashed → doesn't count toward the streak).
    pub finished: bool,
    /// exit == 0 (timeout and nonzero exit both count as false).
    pub ok: bool,
}

/// An utterance from the last hour (from `utterances_between`).
#[derive(Debug, Clone)]
pub struct UttSig {
    pub text: String,
}

/// Detector: pattern crossed the occurrence threshold and STILL has no
/// proposal (status candidate) → offer to generate automation. Deterministic
/// (no classifier). `proposed` is skipped — the approval path already informs
/// about those.
pub fn detect_pattern_ready(pats: &[PatternSig], min_occ: i64) -> Vec<Candidate> {
    pats.iter()
        .filter(|p| p.status == "candidate" && p.occurrences >= min_occ)
        .map(|p| Candidate {
            kind: KIND_PATTERN_READY,
            dedup_key: p.id.to_string(),
            evidence: format!(
                "opakovaně dělám ručně: {} (už {}×)",
                p.description.trim(),
                p.occurrences
            ),
            action_kind: ACT_PROPOSE,
            action_ref: p.id.to_string(),
            needs_classifier: false,
        })
        .collect()
}

/// Detector: an approved runbook's last `streak` FINISHED runs are all
/// failures → inform (no auto-run). Deterministic.
pub fn detect_runbook_failing(runs: &[RunSig], streak: usize) -> Vec<Candidate> {
    // per runbook, collect ok-flags of finished runs in input order (newest first)
    let mut by_rb: BTreeMap<i64, (&str, Vec<bool>)> = BTreeMap::new();
    for r in runs {
        if !r.finished {
            continue;
        }
        by_rb.entry(r.runbook_id).or_insert((r.name.as_str(), Vec::new())).1.push(r.ok);
    }
    let mut out = Vec::new();
    for (rbid, (name, oks)) in by_rb {
        if oks.len() >= streak && oks.iter().take(streak).all(|ok| !ok) {
            out.push(Candidate {
                kind: KIND_RUNBOOK_FAILING,
                dedup_key: rbid.to_string(),
                evidence: format!("automatizace „{name}“ opakovaně selhává ({streak}× v řadě)"),
                action_kind: ACT_INFORM,
                action_ref: String::new(),
                needs_classifier: false,
            });
        }
    }
    out
}

/// Detector (fuzzy): utterance contains a commitment marker and is long
/// enough → CANDIDATE for a reminder. The classifier makes the real call
/// (needs_classifier). Dedups within the batch so the same sentence doesn't
/// land twice.
pub fn detect_commitment(utts: &[UttSig], min_words: usize) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for u in utts {
        let text = u.text.trim();
        if text.split_whitespace().count() < min_words {
            continue;
        }
        let low = text.to_lowercase();
        if !COMMIT_MARKERS.iter().any(|m| low.contains(m)) {
            continue;
        }
        let key: String = low.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
        if !seen.insert(key.clone()) {
            continue;
        }
        out.push(Candidate {
            kind: KIND_COMMITMENT,
            dedup_key: key,
            evidence: text.to_string(),
            action_kind: ACT_INFORM,
            action_ref: String::new(),
            needs_classifier: true,
        });
    }
    out
}

// ---------- local gates (pure) ----------

/// Is the hour within the quiet window [from, to)? from == to = no window;
/// from > to = window wraps midnight (e.g. 22 → 8 = quiet 22:00–07:59).
pub fn in_quiet_hours(hour: u8, from: u8, to: u8) -> bool {
    if from == to {
        false
    } else if from < to {
        (from..to).contains(&hour)
    } else {
        hour >= from || hour < to
    }
}

/// Has the cooldown elapsed since the last nudge for this subject? None = never yet = OK.
pub fn cooldown_ok(last_ts: Option<i64>, now: i64, cooldown_s: i64) -> bool {
    match last_ts {
        None => true,
        Some(t) => now - t >= cooldown_s,
    }
}

// ---------- channel selection (pure) ----------

/// How to deliver the nudge. An action is offered only when there's a safe
/// confirmation path (Telegram yes/no); otherwise it DEGRADES to a plain
/// inform (nothing runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub deliver: bool,
    /// "voice" | "telegram" | ""
    pub channel: &'static str,
    pub speak: bool,
    pub telegram: bool,
    /// Effective action after degradation (may fall back to inform).
    pub effective_action: &'static str,
}

/// Channel selection and possible action degradation:
/// - action + Telegram → Telegram (yes/no); also speaks a heads-up if at the desk,
/// - otherwise inform: spoken at the desk, Telegram remotely, nowhere else = don't deliver.
pub fn plan_delivery(action_kind: &str, voice_ok: bool, tg_ok: bool) -> Plan {
    let eff = match action_kind {
        ACT_RUN_RUNBOOK => ACT_RUN_RUNBOOK,
        ACT_PROPOSE => ACT_PROPOSE,
        _ => ACT_INFORM,
    };
    let actionable = eff == ACT_RUN_RUNBOOK || eff == ACT_PROPOSE;
    if actionable && tg_ok {
        return Plan {
            deliver: true,
            channel: "telegram",
            speak: voice_ok,
            telegram: true,
            effective_action: eff,
        };
    }
    // not an action, or no safe confirmation path → just inform
    if voice_ok {
        return Plan { deliver: true, channel: "voice", speak: true, telegram: false, effective_action: ACT_INFORM };
    }
    if tg_ok {
        return Plan { deliver: true, channel: "telegram", speak: false, telegram: true, effective_action: ACT_INFORM };
    }
    Plan { deliver: false, channel: "", speak: false, telegram: false, effective_action: ACT_INFORM }
}

// ---------- remote confirmation (pure parser) ----------

/// Folds Czech diacritics to ASCII (ď→d, ě→e, …) so confirmation verbs
/// ("udělej", "proveď", "nedělej") match regardless of diacritics/transcription.
fn fold_diacritics(c: char) -> char {
    match c {
        'á' => 'a', 'č' => 'c', 'ď' => 'd', 'é' | 'ě' => 'e', 'í' => 'i', 'ň' => 'n',
        'ó' => 'o', 'ř' => 'r', 'š' => 's', 'ť' => 't', 'ú' | 'ů' => 'u', 'ý' => 'y', 'ž' => 'z',
        other => other,
    }
}

/// "ano 5" / "ne 5" → (run?, nudge_id). Same as runbooks: no number means
/// NOTHING — bare "ano" must never trigger anything. Tolerates diacritics and punctuation.
pub fn parse_confirm(text: &str) -> Option<(bool, i64)> {
    let t: String = text.to_lowercase().chars().map(fold_diacritics).collect();
    let mut tokens = t.split(|c: char| !c.is_ascii_alphanumeric()).filter(|s| !s.is_empty());
    let yes = match tokens.next()? {
        "ano" | "jo" | "ok" | "udelej" | "proved" => true,
        "ne" | "nedelej" | "zahod" => false,
        _ => return None,
    };
    Some((yes, tokens.next()?.parse().ok()?))
}

// ---------- Tier-2 classifier (skeptical, biased to NO) ----------

/// Classifier prompt: should Jarvis interrupt NOW with this nudge? The bias
/// to NO is baked into both the instruction and the verdict parsing.
fn build_gate_prompt(kind: &str, evidence: &str) -> String {
    format!(
        "Jsi tichý asistent, který se ozve JEN když to má jasnou hodnotu. Rozhoduješ \
         jedinou věc: má teď Jarvis vyrušit pána touhle nabídkou, nebo mlčet? Vyrušit \
         je dražší chyba než občas nenabídnout — když váháš, mlč.\n\
         Řekni ANO jen když je podnět konkrétní, aktuální a akceschopný (pán by ocenil, \
         že se ozveš právě teď).\n\
         Řekni NE, když je vágní, nejspíš už vyřízený, jen přečtený/myšlený nahlas, \
         útržek řeči, nebo bys jím jen rušil.\n\n\
         Druh nabídky: {kind}\n\
         Podnět: „{evidence}“\n\
         Odpověz VÝHRADNĚ jedním slovem (ANO/NE):"
    )
}

/// Verdict: true only on a clear "ANO"; anything else = false (bias to silence).
fn parse_gate_verdict(reply: &str) -> bool {
    let norm: String = reply.trim().to_lowercase().chars().filter(|c| c.is_alphabetic()).collect();
    norm.starts_with("ano")
}

/// One classification (no DB): builds the prompt, calls the model, returns
/// (interrupt?, outcome for cost logging). Errors propagate.
fn classify_worth_raw(paths: &Paths, cfg: &Config, kind: &str, evidence: &str) -> Result<(bool, claude::ClaudeOutcome)> {
    let outcome = claude::run(&claude::ClaudeRequest {
        prompt: build_gate_prompt(kind, evidence),
        model: Some(&cfg.proactive.model),
        cwd: &paths.data_dir,
        allowed_tools: "Read",
        max_turns: 1,
        timeout: Duration::from_secs(60),
    })?;
    Ok((parse_gate_verdict(&outcome.text), outcome))
}

/// Worker path: classifies and logs cost (component "nudge-gate").
/// Error = false (stay silent).
fn classify_worth(paths: &Paths, cfg: &Config, conn: &Connection, c: &Candidate) -> bool {
    match classify_worth_raw(paths, cfg, c.kind, &c.evidence) {
        Ok((worth, outcome)) => {
            if let Err(e) = db::insert_cost(
                conn, util::now_ts(), "nudge-gate", &cfg.proactive.model,
                outcome.tokens_in, outcome.tokens_out, outcome.cost_usd,
            ) {
                warn!("nudge: zápis nákladu klasifikátoru selhal: {e:#}");
            }
            debug!("nudge klasifikátor: „{}“ → {} ({:.4} USD)", c.evidence, if worth { "ANO" } else { "NE" }, outcome.cost_usd);
            worth
        }
        Err(e) => {
            warn!("nudge klasifikátor selhal — mlčím: {e:#}");
            false
        }
    }
}

// ---------- nudge texts ----------

fn telegram_text(evidence: &str, effective_action: &str, id: i64) -> String {
    match effective_action {
        ACT_RUN_RUNBOOK => format!("Jarvis: {evidence}\n\nSpustit? odpověz „ano {id}“ / „ne {id}“"),
        ACT_PROPOSE => format!("Jarvis: {evidence}\n\nMám z toho udělat automatizaci? „ano {id}“ / „ne {id}“"),
        _ => format!("Jarvis: {evidence}"),
    }
}

fn voice_text(kind: &str, evidence: &str, effective_action: &str, also_telegram: bool) -> String {
    let tail = if also_telegram && effective_action != ACT_INFORM {
        " Potvrzení jsem poslal na Telegram."
    } else {
        ""
    };
    match kind {
        KIND_PATTERN_READY => format!("Pane, {evidence}. Mám z toho udělat automatizaci?{tail}"),
        KIND_RUNBOOK_FAILING => format!("Pane, {evidence}. Mrkněte na to, prosím."),
        KIND_COMMITMENT => format!("Pane, připomínám: {evidence}."),
        _ => format!("Pane, {evidence}.{tail}"),
    }
}

// ---------- gathering signals + delivery (shell) ----------

/// Collects candidates from all enabled detectors, in priority order
/// (failing runbook > ready pattern > commitment in speech).
fn gather(cfg: &Config, conn: &Connection) -> Vec<Candidate> {
    let pr = &cfg.proactive;
    let mut out = Vec::new();

    if pr.detect_runbook_failing {
        match crate::runbook::recent_runs(conn, 60) {
            Ok(rows) => {
                let sigs: Vec<RunSig> = rows
                    .iter()
                    .map(|r| RunSig {
                        runbook_id: r.runbook_id,
                        name: r.name.clone(),
                        finished: r.finished_at.is_some(),
                        ok: r.ok(),
                    })
                    .collect();
                out.extend(detect_runbook_failing(&sigs, pr.runbook_fail_streak));
            }
            Err(e) => warn!("nudge: čtení běhů runbooků selhalo: {e:#}"),
        }
    }
    if pr.detect_pattern_ready {
        match crate::patterns::top(conn, pr.pattern_min_occurrences, 10) {
            Ok(pats) => {
                let sigs: Vec<PatternSig> = pats
                    .iter()
                    .map(|p| PatternSig {
                        id: p.id,
                        description: p.description.clone(),
                        occurrences: p.occurrences,
                        status: p.status.clone(),
                    })
                    .collect();
                out.extend(detect_pattern_ready(&sigs, pr.pattern_min_occurrences));
            }
            Err(e) => warn!("nudge: čtení vzorů selhalo: {e:#}"),
        }
    }
    if pr.detect_commitment {
        let now = util::now_ts();
        match db::utterances_between(conn, now - 3600, now + 1) {
            Ok(utts) => {
                let sigs: Vec<UttSig> = utts.iter().map(|u| UttSig { text: u.text.clone() }).collect();
                out.extend(detect_commitment(&sigs, 3));
            }
            Err(e) => warn!("nudge: čtení promluv selhalo: {e:#}"),
        }
    }
    out
}

/// At the desk? = a fresh sample exists and idle is below the threshold. No
/// sample (capture not running) = treat as "not at desk" → prefer Telegram
/// over speaking into an empty room.
fn at_desk(cfg: &Config, conn: &Connection, now: i64) -> bool {
    let row: Option<(i64, i64)> = conn
        .query_row("SELECT ts, idle_ms FROM samples ORDER BY ts DESC LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()
        .ok()
        .flatten();
    match row {
        Some((ts, idle_ms)) => {
            let fresh = now - ts <= (cfg.capture.meta_interval_s as i64) * 3 + 5;
            fresh && idle_ms < (cfg.proactive.at_desk_idle_s as i64) * 1000
        }
        None => false,
    }
}

/// Delivers ONE nudge over the chosen channel and writes a row to `nudges`.
fn deliver(paths: &Paths, cfg: &Config, conn: &Connection, c: &Candidate, now: i64) {
    let desk = at_desk(cfg, conn, now);
    let voice_ok = cfg.speak.enabled && desk;
    let tg_ok = cfg.proactive.telegram_confirm && crate::config::telegram_keys(paths).is_ok();
    let plan = plan_delivery(c.action_kind, voice_ok, tg_ok);
    if !plan.deliver {
        debug!("nudge [{}]: žádný kanál (u stolu={desk}, tg={tg_ok}) — nedoručuji: {}", c.kind, c.evidence);
        return;
    }
    let action_ref = if plan.effective_action == ACT_INFORM { "" } else { c.action_ref.as_str() };
    let id = match db::insert_nudge(
        conn, now, c.kind, &c.dedup_key, &c.evidence, plan.effective_action, action_ref, plan.channel,
    ) {
        Ok(id) => id,
        Err(e) => {
            warn!("nudge: zápis nabídky selhal: {e:#}");
            return;
        }
    };
    if plan.telegram {
        if let Ok((token, chat_id)) = crate::config::telegram_keys(paths) {
            let msg = telegram_text(&c.evidence, plan.effective_action, id);
            if let Err(e) = crate::telegram::send_message(&token, &chat_id, &msg) {
                warn!("nudge: telegram se neodeslal: {e:#}");
            }
        }
    }
    if plan.speak {
        let phrase = voice_text(c.kind, &c.evidence, plan.effective_action, plan.telegram);
        if let Err(e) = speak::say_once(paths, cfg, &phrase) {
            warn!("nudge: hlas selhal: {e:#}");
        }
    }
    info!("nudge [{}] #{id} → {} ({}): {}", c.kind, plan.channel, plan.effective_action, c.evidence);
}

// ---------- orchestrator ----------

/// One tick of the proactive layer. Handles remote confirmations (shared
/// Telegram stream), then — only if the layer is enabled and nothing mutes
/// it — gathers signals and delivers AT MOST ONE nudge (stay calm). All
/// errors are just logged.
pub fn tick(paths: &Paths, cfg: &Config, conn: &Connection) {
    if !cfg.proactive.enabled {
        return;
    }
    // remote confirmations "ano N"/"ne N" (unified poll — see telegram::process_approvals)
    if cfg.proactive.telegram_confirm {
        crate::telegram::process_approvals(paths, cfg, conn);
    }
    let now = util::now_ts();
    let _ = db::state_set(conn, "nudge_alive_ts", &now.to_string());

    // global mutes: pause, locked screen, quiet hours, daily cap
    if matches!(db::pause_until(conn, now), Ok(Some(_))) {
        return;
    }
    if matches!(screen::probe(), screen::Lock::Active) {
        return;
    }
    let hour = chrono::Local::now().hour() as u8;
    if in_quiet_hours(hour, cfg.proactive.quiet_from, cfg.proactive.quiet_to) {
        return;
    }
    let day_start = util::day_bounds_local(util::today_local()).map(|(s, _)| s).unwrap_or(0);
    if db::nudge_count_since(conn, day_start).unwrap_or(0) as u32 >= cfg.proactive.daily_max {
        return;
    }

    let cooldown_s = (cfg.proactive.cooldown_min as i64) * 60;
    for c in gather(cfg, conn) {
        // cooldown per (kind, subject)
        let last = db::last_nudge_ts(conn, c.kind, &c.dedup_key).ok().flatten();
        if !cooldown_ok(last, now, cooldown_s) {
            continue;
        }
        // fuzzy signals must pass the classifier; stay silent when budget is exhausted
        if c.needs_classifier {
            if cfg.proactive.respect_budget && crate::converse::over_budget(cfg, conn).unwrap_or(false) {
                debug!("nudge: rozpočet vyčerpán — kandidáta neklasifikuji: {}", c.evidence);
                continue;
            }
            if !classify_worth(paths, cfg, conn, &c) {
                continue;
            }
        }
        deliver(paths, cfg, conn, &c, now);
        return; // at most one nudge per tick (stay calm)
    }
}

/// `jarvis nudge --dry-run`: what the layer would do right now — no API, no
/// writes, no speech or Telegram. A real failable check against the live DB.
pub fn run_dry(paths: &Paths, cfg: &Config, conn: &Connection) -> Result<()> {
    let pr = &cfg.proactive;
    let now = util::now_ts();
    let hour = chrono::Local::now().hour() as u8;
    let desk = at_desk(cfg, conn, now);
    let tg_ok = pr.telegram_confirm && crate::config::telegram_keys(paths).is_ok();
    println!("Proaktivní vrstva: enabled={}, u stolu={desk}, telegram={tg_ok}", pr.enabled);
    let mut muted = Vec::new();
    if matches!(db::pause_until(conn, now), Ok(Some(_))) {
        muted.push("pauza");
    }
    if matches!(screen::probe(), screen::Lock::Active) {
        muted.push("zamčená obrazovka");
    }
    if in_quiet_hours(hour, pr.quiet_from, pr.quiet_to) {
        muted.push("klidové hodiny");
    }
    let day_start = util::day_bounds_local(util::today_local()).map(|(s, _)| s).unwrap_or(0);
    let today = db::nudge_count_since(conn, day_start).unwrap_or(0);
    if today as u32 >= pr.daily_max {
        muted.push("denní strop");
    }
    println!("Dnes nabídek: {today}/{}{}", pr.daily_max, if muted.is_empty() { String::new() } else { format!("; teď by MLČEL: {}", muted.join(", ")) });

    let cands = gather(cfg, conn);
    if cands.is_empty() {
        println!("Kandidáti: žádní.");
        return Ok(());
    }
    let cooldown_s = (pr.cooldown_min as i64) * 60;
    println!("Kandidáti ({}):", cands.len());
    for c in &cands {
        let last = db::last_nudge_ts(conn, c.kind, &c.dedup_key).ok().flatten();
        let cd = cooldown_ok(last, now, cooldown_s);
        let plan = plan_delivery(c.action_kind, cfg.speak.enabled && desk, tg_ok);
        println!(
            "  [{}] {}\n      akce={}→{} ref={} klasifikátor={} cooldown_ok={} kanál={}",
            c.kind, c.evidence, c.action_kind, plan.effective_action, c.action_ref, c.needs_classifier, cd,
            if plan.deliver { plan.channel } else { "—(nedoručí)" }
        );
    }
    Ok(())
}

/// Remote confirmation of a nudge (called from telegram::process_approvals).
/// Returns the reply text for the chat. Idempotent: won't re-run an already
/// resolved nudge.
pub fn confirm_remote(paths: &Paths, cfg: &Config, conn: &Connection, id: i64, yes: bool) -> String {
    let Some(n) = db::nudge_by_id(conn, id).ok().flatten() else {
        return format!("Neznámá nabídka #{id}.");
    };
    if n.status != "offered" {
        return format!("Nabídka #{id} už je vyřízená ({}).", n.status);
    }
    if !yes {
        let _ = db::set_nudge_status(conn, id, "dismissed", "uživatel zamítl");
        return format!("Dobře, nabídku #{id} nechávám být.");
    }
    match n.action_kind.as_str() {
        ACT_RUN_RUNBOOK => match crate::runbook::resolve(conn, &n.action_ref) {
            Ok(rb) => match crate::runbook::run_one(paths, cfg, conn, &rb, "nudge") {
                Ok(run) if run.ok() => {
                    let _ = db::set_nudge_status(conn, id, "done", "runbook OK");
                    format!("✓ Runbook „{}“ doběhl v pořádku.", rb.name)
                }
                Ok(run) => {
                    let _ = db::set_nudge_status(conn, id, "failed", "runbook exit != 0");
                    format!("✗ Runbook „{}“ skončil chybou (exit {:?}).", rb.name, run.exit_code)
                }
                Err(e) => {
                    let _ = db::set_nudge_status(conn, id, "failed", &format!("{e:#}"));
                    format!("✗ Spuštění runbooku selhalo: {e:#}")
                }
            },
            Err(e) => {
                let _ = db::set_nudge_status(conn, id, "failed", &format!("{e:#}"));
                format!("✗ Runbook nenalezen: {e:#}")
            }
        },
        ACT_PROPOSE => {
            let pid: Option<i64> = n.action_ref.parse().ok();
            match crate::patterns::propose(paths, cfg, conn, pid) {
                Ok(()) => {
                    let _ = db::set_nudge_status(conn, id, "done", "návrh vygenerován");
                    "✓ Návrh automatizace vygenerován — detail: „jarvis runbook pending“ (schválení pořád na tobě).".into()
                }
                Err(e) => {
                    let _ = db::set_nudge_status(conn, id, "failed", &format!("{e:#}"));
                    format!("✗ Generování návrhu selhalo: {e:#}")
                }
            }
        }
        _ => {
            let _ = db::set_nudge_status(conn, id, "done", "bez akce");
            format!("Nabídka #{id} nemá co provést.")
        }
    }
}

// ---------- kill-gate (jarvis nudge-eval) ----------

/// Kill-gate tally: how many "worth" signals the classifier caught (recall)
/// and — the main metric — how often it said ANO on "noise" (false-interrupt
/// = unnecessary interruption). Design bias: keep false-interrupt as low as
/// possible even at the cost of recall.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct EvalTally {
    pub worth_total: u32,
    pub worth_hit: u32,
    pub noise_total: u32,
    pub noise_accept: u32,
    pub skipped: u32,
}

impl EvalTally {
    fn record(&mut self, label: &str, surfaced: bool) {
        match label {
            "worth" => {
                self.worth_total += 1;
                self.worth_hit += u32::from(surfaced);
            }
            "noise" => {
                self.noise_total += 1;
                self.noise_accept += u32::from(surfaced);
            }
            _ => self.skipped += 1,
        }
    }
    pub fn recall(&self) -> f64 {
        if self.worth_total == 0 { 0.0 } else { f64::from(self.worth_hit) / f64::from(self.worth_total) }
    }
    /// Share of "noise" the classifier incorrectly said ANO to (unnecessary
    /// interruption). The key kill-gate metric; target < ~2-3%.
    pub fn false_interrupt_rate(&self) -> f64 {
        if self.noise_total == 0 { 0.0 } else { f64::from(self.noise_accept) / f64::from(self.noise_total) }
    }
}

/// Kill-gate: labeled JSONL (`{"evidence","label"[,"kind"]}`, label =
/// worth|noise) → the real classifier → confusion matrix + recall +
/// false-interrupt rate. Real spend (runs with your key, cost gets logged).
pub fn eval(paths: &Paths, cfg: &Config, file: &Path) -> Result<()> {
    let body = std::fs::read_to_string(file).with_context(|| format!("nelze číst korpus {}", file.display()))?;
    let conn = db::open(&paths.db_path)?;
    let mut tally = EvalTally::default();
    let mut cost = 0.0;
    for (i, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("řádek {}: neplatný JSON", i + 1))?;
        let evidence = row["evidence"].as_str().unwrap_or_default();
        let label = row["label"].as_str().unwrap_or_default();
        let kind = row["kind"].as_str().unwrap_or(KIND_COMMITMENT);
        if evidence.is_empty() || label.is_empty() {
            eprintln!("řádek {}: chybí evidence/label — přeskakuji", i + 1);
            continue;
        }
        let (surfaced, outcome) = classify_worth_raw(paths, cfg, kind, evidence)?;
        cost += outcome.cost_usd;
        let _ = db::insert_cost(&conn, util::now_ts(), "nudge-gate", &cfg.proactive.model, outcome.tokens_in, outcome.tokens_out, outcome.cost_usd);
        tally.record(label, surfaced);
        let mark = match (label, surfaced) {
            ("worth", true) | ("noise", false) => "ok  ",
            ("worth", false) => "MISS",
            _ => "RUŠÍ", // noise + ANO = unnecessary interruption
        };
        println!("{mark} [{label:^6}→{}] {evidence}", if surfaced { "ANO" } else { "NE " });
    }
    println!("\n── nudge kill-gate ──");
    println!("worth:  {}/{} chyceno   (recall {:.0} %)", tally.worth_hit, tally.worth_total, tally.recall() * 100.0);
    println!(
        "noise:  {}/{} vyrušení  (false-interrupt {:.1} %)  ← klíčová metrika",
        tally.noise_accept, tally.noise_total, tally.false_interrupt_rate() * 100.0
    );
    if tally.skipped > 0 {
        println!("přeskočeno: {} (neznámý label)", tally.skipped);
    }
    println!("náklad: {cost:.4} USD");
    println!("\nZapnout detect_commitment má smysl, jen když je false-interrupt hodně nízko (cíl < 2–3 %).");
    Ok(())
}

/// Prints the last `n` mic utterances as a JSONL kill-gate corpus template
/// (`{"evidence","kind":"commitment","label":""}`); label `label` as
/// worth|noise and run `jarvis nudge-eval <file>`.
pub fn eval_scaffold(paths: &Paths, n: usize) -> Result<()> {
    let conn = db::open(&paths.db_path)?;
    for t in db::recent_utterance_texts(&conn, n)? {
        println!("{}", serde_json::json!({ "evidence": t, "kind": KIND_COMMITMENT, "label": "" }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_ready_needs_threshold_and_candidate_status() {
        let pats = vec![
            PatternSig { id: 1, description: "kopíruju z A do B".into(), occurrences: 5, status: "candidate".into() },
            PatternSig { id: 2, description: "málo časté".into(), occurrences: 2, status: "candidate".into() },
            PatternSig { id: 3, description: "už navrženo".into(), occurrences: 9, status: "proposed".into() },
        ];
        let out = detect_pattern_ready(&pats, 3);
        assert_eq!(out.len(), 1, "jen #1: nad prahem a ještě candidate");
        assert_eq!(out[0].dedup_key, "1");
        assert_eq!(out[0].action_kind, ACT_PROPOSE);
        assert_eq!(out[0].action_ref, "1");
        assert!(!out[0].needs_classifier, "deterministický detektor neklasifikuje");
    }

    #[test]
    fn runbook_failing_needs_consecutive_finished_failures() {
        // newest first; #10 fails 2x in a row, #20's last run is OK, #30 has only 1 finished run
        let runs = vec![
            RunSig { runbook_id: 10, name: "záloha".into(), finished: true, ok: false },
            RunSig { runbook_id: 10, name: "záloha".into(), finished: true, ok: false },
            RunSig { runbook_id: 10, name: "záloha".into(), finished: true, ok: true },
            RunSig { runbook_id: 20, name: "sync".into(), finished: true, ok: true },
            RunSig { runbook_id: 20, name: "sync".into(), finished: true, ok: false },
            RunSig { runbook_id: 30, name: "report".into(), finished: false, ok: false }, // running/crashed
            RunSig { runbook_id: 30, name: "report".into(), finished: true, ok: false },
        ];
        let out = detect_runbook_failing(&runs, 2);
        assert_eq!(out.len(), 1, "jen #10 má 2 dokončené neúspěchy v řadě");
        assert_eq!(out[0].dedup_key, "10");
        assert_eq!(out[0].kind, KIND_RUNBOOK_FAILING);
        assert_eq!(out[0].action_kind, ACT_INFORM, "padající runbook se NEspouští, jen informuje");
        // unfinished run (#30) doesn't count toward the streak → only 1 finished failure → nothing
    }

    #[test]
    fn commitment_matches_marker_and_length() {
        let utts = vec![
            UttSig { text: "Pošlu Tomášovi ten soubor odpoledne".into() }, // marker + enough words
            UttSig { text: "no".into() },                                   // short
            UttSig { text: "dneska bylo hezky venku a tak".into() },        // no marker
            UttSig { text: "Pošlu Tomášovi ten soubor odpoledne".into() },  // duplicate
        ];
        let out = detect_commitment(&utts, 3);
        assert_eq!(out.len(), 1, "jeden unikátní závazek");
        assert_eq!(out[0].kind, KIND_COMMITMENT);
        assert!(out[0].needs_classifier, "fuzzy podnět MUSÍ přes klasifikátor");
    }

    #[test]
    fn quiet_hours_wrap_and_boundaries() {
        // wraps midnight 22 → 8
        assert!(in_quiet_hours(23, 22, 8));
        assert!(in_quiet_hours(0, 22, 8));
        assert!(in_quiet_hours(7, 22, 8));
        assert!(!in_quiet_hours(8, 22, 8)); // the `to` boundary is exclusive
        assert!(!in_quiet_hours(21, 22, 8));
        assert!(in_quiet_hours(22, 22, 8)); // the `from` boundary is inclusive
        // same day 8 → 22
        assert!(in_quiet_hours(10, 8, 22));
        assert!(!in_quiet_hours(23, 8, 22));
        // from == to = no window
        assert!(!in_quiet_hours(0, 0, 0));
        assert!(!in_quiet_hours(15, 9, 9));
    }

    #[test]
    fn cooldown_respects_window() {
        assert!(cooldown_ok(None, 1000, 300), "nikdy nenabídnuto → OK");
        assert!(!cooldown_ok(Some(900), 1000, 300), "před 100 s, cooldown 300 → ne");
        assert!(cooldown_ok(Some(700), 1000, 300), "přesně na hraně (300 s) → OK");
        assert!(cooldown_ok(Some(600), 1000, 300), "dávno → OK");
    }

    #[test]
    fn plan_delivery_matrix() {
        // action + telegram → telegram (+ voice heads-up at desk = voice_ok)
        let p = plan_delivery(ACT_PROPOSE, true, true);
        assert_eq!(p.channel, "telegram");
        assert!(p.telegram && p.speak);
        assert_eq!(p.effective_action, ACT_PROPOSE);
        // action + telegram, but not at desk (voice_ok=false) → telegram only
        let p = plan_delivery(ACT_RUN_RUNBOOK, false, true);
        assert!(p.telegram && !p.speak);
        assert_eq!(p.effective_action, ACT_RUN_RUNBOOK);
        // action WITHOUT telegram, at desk → degrades to spoken inform (nothing runs)
        let p = plan_delivery(ACT_PROPOSE, true, false);
        assert_eq!(p.channel, "voice");
        assert!(p.speak && !p.telegram);
        assert_eq!(p.effective_action, ACT_INFORM);
        // action WITHOUT telegram, not at desk → doesn't deliver (no safe path)
        let p = plan_delivery(ACT_PROPOSE, false, false);
        assert!(!p.deliver);
        // inform at desk → spoken
        let p = plan_delivery(ACT_INFORM, true, true);
        assert_eq!(p.channel, "voice");
        // inform remotely (voice_ok=false) → telegram
        let p = plan_delivery(ACT_INFORM, false, true);
        assert_eq!(p.channel, "telegram");
        assert_eq!(p.effective_action, ACT_INFORM);
    }

    #[test]
    fn confirm_parser_needs_number() {
        assert_eq!(parse_confirm("ano 5"), Some((true, 5)));
        assert_eq!(parse_confirm("Ano 12"), Some((true, 12)));
        assert_eq!(parse_confirm("jo 3"), Some((true, 3)));
        assert_eq!(parse_confirm("ne 5"), Some((false, 5)));
        assert_eq!(parse_confirm("ne 7"), Some((false, 7)));
        assert_eq!(parse_confirm("udělej 9"), Some((true, 9)));
        // no number or unrecognized words → nothing (bare "ano" must never trigger anything)
        assert_eq!(parse_confirm("ano"), None);
        assert_eq!(parse_confirm("ne"), None);
        assert_eq!(parse_confirm("schval 3"), None); // that's a runbook, not a nudge
        assert_eq!(parse_confirm("ahoj"), None);
        assert_eq!(parse_confirm(""), None);
    }

    #[test]
    fn gate_verdict_bias_to_silence() {
        assert!(parse_gate_verdict("ANO"));
        assert!(parse_gate_verdict("ano, určitě"));
        assert!(!parse_gate_verdict("NE"));
        assert!(!parse_gate_verdict("ne, radši mlč"));
        assert!(!parse_gate_verdict(""));
        assert!(!parse_gate_verdict("nevím, možná"), "cokoli nejasného = mlčet");
    }

    #[test]
    fn eval_tally_metrics() {
        let mut t = EvalTally::default();
        t.record("worth", true);
        t.record("worth", false);
        t.record("noise", false);
        t.record("noise", false);
        t.record("noise", true); // one unnecessary interruption
        t.record("junk", true); // unknown label
        assert_eq!(t.worth_total, 2);
        assert!((t.recall() - 0.5).abs() < 1e-9);
        assert_eq!(t.noise_total, 3);
        assert!((t.false_interrupt_rate() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(t.skipped, 1);
    }
}
