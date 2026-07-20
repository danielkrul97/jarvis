//! Memory consolidation (phase 2): `claude -p` extracts PERMANENT facts about
//! the master from the day's conversations and utterances, and stores them in
//! semantic memory (`memory_facts`). Episodic layer → semantic layer, once
//! during quiet time (nightly tick in `jarvis run`, or manually via
//! `jarvis memory consolidate`).
//!
//! The model does extraction; Rust deterministically handles DEDUP and
//! supersede (testable without the API): identical wording → just confirm
//! (touch), same topic with revised wording → supersede the old fact with the
//! new one, otherwise insert new.

use crate::config::{Config, Paths};
use crate::pipeline::claude::{self, ClaudeRequest};
use crate::store::db::{self, Fact};
use crate::{memory, util};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;
use rusqlite::Connection;
use tracing::{info, warn};

/// JSON contract for extraction (what the model returns).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Extracted {
    facts: Vec<ExtractedFact>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ExtractedFact {
    kind: String,
    subject: String,
    text: String,
    confidence: f64,
}

impl Default for ExtractedFact {
    fn default() -> Self {
        Self { kind: "fact".into(), subject: String::new(), text: String::new(), confidence: 0.7 }
    }
}

/// Allowed fact kinds; anything else falls back to "fact".
const KINDS: &[&str] = &["profile", "preference", "fact", "relationship", "task"];
/// Threshold on shared-token ratio (Jaccard) above which a fact with the same
/// topic is treated as an update (supersede) rather than a new one.
const SUPERSEDE_JACCARD: f64 = 0.5;
/// Effective salience below this threshold = the fact gets pruned on decay.
const PRUNE_FLOOR: f64 = 0.2;
/// Cap on prompt material (most recent), so a long window doesn't blow up tokens.
const MAX_CONVS: usize = 300;
const MAX_UTTS: usize = 600;

/// Runs consolidation from the watermark (or `since_override`) to now. Returns
/// the count of new/updated facts. `dry_run` only prints the prompt and material.
pub fn run(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    since_override: Option<i64>,
    dry_run: bool,
) -> Result<usize> {
    let now = util::now_ts();
    let since = since_override.unwrap_or_else(|| {
        db::state_get_i64(conn, "memory_watermark").ok().flatten().unwrap_or(now - 86_400)
    });
    // The watermark advances ONLY on the automatic path (watermark to now).
    // A manual `--since Xh` is a one-off window override; if it advanced the
    // watermark to `now`, the span between the old watermark and the
    // `--since` window would never get processed by any automatic run →
    // silent loss of memory consolidation.
    let advance_watermark = since_override.is_none();
    let mut convs = db::conversations_between(conn, since, now)?;
    // mic utterances only: meet/wav transcripts are about other people's
    // meetings, not about the master
    let mut utts: Vec<(i64, String)> = db::utterances_between(conn, since, now)?
        .into_iter()
        .map(|u| (u.ts_start, u.text))
        .collect();

    // cap on prompt size (tokens): a daily run is small, but a long downtime
    // or `--since 90d` would blow up the prompt. We keep the MOST RECENT and log what's dropped.
    if convs.len() > MAX_CONVS {
        let drop = convs.len() - MAX_CONVS;
        warn!("konsolidace: {drop} nejstarších konverzací nad strop {MAX_CONVS} — vynechávám");
        convs.drain(0..drop);
    }
    if utts.len() > MAX_UTTS {
        let drop = utts.len() - MAX_UTTS;
        warn!("konsolidace: {drop} nejstarších promluv nad strop {MAX_UTTS} — vynechávám");
        utts.drain(0..drop);
    }

    if convs.is_empty() && utts.is_empty() {
        info!("konsolidace: od {} nic nového", util::fmt_local(since));
        if !dry_run && advance_watermark {
            db::state_set(conn, "memory_watermark", &now.to_string())?;
        }
        return Ok(0);
    }

    let existing = db::active_facts(conn)?;
    let prompt = build_prompt(&existing, &convs, &utts);

    if dry_run {
        println!("─── konsolidace {} – {} ───", util::fmt_local(since), util::fmt_local(now));
        println!("materiál: {} konverzací, {} promluv, {} známých faktů", convs.len(), utts.len(), existing.len());
        println!("prompt {} znaků\n", prompt.chars().count());
        println!("{prompt}");
        return Ok(0);
    }

    // budget shared with analysis/conversation; if exhausted, don't advance the
    // watermark, so the material is retried the next day once the cap resets
    if cfg.converse.respect_budget && crate::converse::over_budget(cfg, conn)? {
        warn!("konsolidace: denní rozpočet vyčerpán — přeskakuji (zkusím zítra)");
        return Ok(0);
    }

    let extracted = call_claude(cfg, paths, conn, &prompt, now)?;
    let applied = apply(conn, &extracted.facts, now)?;
    let pruned = db::prune_faded_facts(conn, now, cfg.memory.fact_half_life_days, PRUNE_FLOOR)?;
    if advance_watermark {
        db::state_set(conn, "memory_watermark", &now.to_string())?;
    }

    // keep the vector index fresh: consolidation is nightly "heavy maintenance",
    // so it's allowed to download the model on first run too (util::download
    // skips existing files → the 470 MB hit happens only once). This turns on
    // vectors on its own, without a manual `memory embed`. Best-effort: a
    // download/embed error just logs — the facts still stand.
    if cfg.memory.vectors {
        match memory::embed::ensure_model(&cfg.memory) {
            Ok(()) => match memory::embed_backfill(&cfg.memory, conn, false) {
                Ok(n) if n > 0 => info!("konsolidace: doplněno {n} embeddingů"),
                Ok(_) => {}
                Err(e) => warn!("konsolidace: doembedování selhalo: {e:#}"),
            },
            Err(e) => warn!("konsolidace: model embeddingů nedostupný ({e:#}) — vektory přeskakuji"),
        }
    }
    info!(
        "konsolidace: {applied} nových/aktualizovaných faktů z {} útržků, {pruned} prořezáno",
        convs.len() + utts.len()
    );
    Ok(applied)
}

/// Two rounds: claude call + parse; invalid JSON or an error → one retry.
/// Cost is recorded in `costs` (component "memory") for each attempt.
fn call_claude(
    cfg: &Config,
    paths: &Paths,
    conn: &Connection,
    prompt: &str,
    now: i64,
) -> Result<Extracted> {
    let model = &cfg.memory.consolidate_model;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=2 {
        let outcome = match claude::run(&ClaudeRequest {
            prompt: prompt.to_string(),
            model: Some(model),
            cwd: &paths.data_dir,
            allowed_tools: "Read",
            max_turns: 1,
            timeout: Duration::from_secs(cfg.analysis.timeout_s),
        }) {
            Ok(o) => o,
            Err(e) => {
                warn!("konsolidace pokus {attempt}/2: claude selhal: {e:#}");
                last_err = Some(e);
                continue;
            }
        };
        db::insert_cost(conn, now, "memory", model, outcome.tokens_in, outcome.tokens_out, outcome.cost_usd)?;
        match claude::extract_json(&outcome.text)
            .and_then(|s| serde_json::from_str::<Extracted>(s).map_err(Into::into))
        {
            Ok(ex) => return Ok(ex),
            Err(e) => {
                warn!("konsolidace pokus {attempt}/2: nevalidní JSON kontrakt: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("konsolidace selhala")))
}

/// Applies extracted facts via deterministic dedup. Active facts are loaded
/// ONCE and token sets precomputed (previously this did a full SELECT +
/// re-tokenization of all facts for EACH extracted fact → O(F·N)). Intra-batch
/// dedup is kept by projecting newly inserted/superseded facts straight back
/// into the in-memory list (a second identical one in the batch = touch the
/// first inserted).
fn apply(conn: &Connection, facts: &[ExtractedFact], _now: i64) -> Result<usize> {
    let mut known: Vec<FactTokens> =
        db::active_facts(conn)?.iter().map(FactTokens::from_fact).collect();
    let mut applied = 0;
    for nf in facts {
        let text = nf.text.trim();
        // discard a too-short/empty statement (hallucination, stray fragment)
        if text.chars().count() < 4 {
            continue;
        }
        let kind = if KINDS.contains(&nf.kind.as_str()) { nf.kind.as_str() } else { "fact" };
        let subject = nf.subject.trim();
        let conf = nf.confidence.clamp(0.0, 1.0);
        match decide(text, subject, &known) {
            Decision::Touch(id) => db::touch_fact(conn, id)?,
            Decision::Insert => {
                let nid = db::insert_fact(conn, kind, subject, text, conf, false, "consolidate")?;
                known.push(FactTokens::new(nid, text, subject));
                applied += 1;
            }
            Decision::Supersede(old) => {
                let nid = db::insert_fact(conn, kind, subject, text, conf, false, "consolidate")?;
                db::supersede_fact(conn, old, nid)?;
                known.retain(|f| f.id != old); // old one is no longer active
                known.push(FactTokens::new(nid, text, subject));
                applied += 1;
            }
        }
    }
    Ok(applied)
}

/// Precomputed tokens of an active fact for dedup — so `tokens()` isn't called
/// again for every compared pair.
struct FactTokens {
    id: i64,
    text_joined: String,    // tokens(text).join(" ")  — for wording match
    subject_joined: String, // tokens(subject).join(" ") — for topic match
    text_tokens: Vec<String>, // tokens(text) — for Jaccard
}

impl FactTokens {
    fn new(id: i64, text: &str, subject: &str) -> Self {
        let text_tokens = memory::tokens(text);
        FactTokens {
            id,
            text_joined: text_tokens.join(" "),
            subject_joined: memory::tokens(subject).join(" "),
            text_tokens,
        }
    }
    fn from_fact(f: &Fact) -> Self {
        Self::new(f.id, &f.text, &f.subject)
    }
}

/// Dedup decision for a single extracted fact.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Insert a new fact.
    Insert,
    /// Identical wording already exists → just confirm it (touch this id).
    Touch(i64),
    /// Same topic, revised wording → supersede the old fact (id) with the new one.
    Supersede(i64),
}

/// Deterministic dedup: identical wording → Touch; same non-empty topic with
/// large enough token overlap → Supersede; otherwise Insert. `known` has
/// tokens precomputed (see `FactTokens`), so no tokenizing happens here.
fn decide(new_text: &str, new_subject: &str, known: &[FactTokens]) -> Decision {
    let nt = memory::tokens(new_text);
    let nt_joined = nt.join(" ");
    // 1) identical wording (after folding) → just confirm
    if let Some(f) = known.iter().find(|f| f.text_joined == nt_joined) {
        return Decision::Touch(f.id);
    }
    // 2) same topic + large overlap → update (supersede the most similar one)
    let ns = memory::tokens(new_subject).join(" ");
    if !ns.is_empty() {
        let nt_set: HashSet<&String> = nt.iter().collect();
        let best = known
            .iter()
            .filter(|f| f.subject_joined == ns)
            .map(|f| (f, jaccard(&nt_set, &f.text_tokens)))
            .filter(|(_, j)| *j >= SUPERSEDE_JACCARD)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((f, _)) = best {
            return Decision::Supersede(f.id);
        }
    }
    Decision::Insert
}

/// Jaccard similarity of token sets: |∩| / |∪|.
fn jaccard(a: &HashSet<&String>, b_tokens: &[String]) -> f64 {
    if a.is_empty() && b_tokens.is_empty() {
        return 1.0;
    }
    let b: HashSet<&String> = b_tokens.iter().collect();
    let inter = a.intersection(&b).count();
    let union = a.union(&b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

fn build_prompt(existing: &[Fact], convs: &[(i64, String, String)], utts: &[(i64, String)]) -> String {
    let mut known = String::new();
    for f in existing.iter().take(60) {
        let subj = if f.subject.is_empty() { String::new() } else { format!("/{}", f.subject) };
        known.push_str(&format!("- [{}{}] {}\n", f.kind, subj, f.text));
    }
    if known.is_empty() {
        known.push_str("(zatím nic)\n");
    }
    let mut material = String::new();
    for (_, q, a) in convs {
        material.push_str(&format!("[rozhovor] Pán: {q} | Jarvis: {a}\n"));
    }
    for (_, t) in utts {
        let t = t.trim();
        if !t.is_empty() {
            material.push_str(&format!("[zaslechnuto] {t}\n"));
        }
    }
    format!(
        "Jsi dlouhodobá paměť hlasového asistenta Jarvise. Z útržků níže (mé dotazy \
         Jarvisovi, jeho odpovědi a přepisy toho, co jsem řekl nahlas) vytáhni TRVALÁ \
         fakta o mně (pánovi) a mém světě: osobní profil, preference, vztahy k lidem, \
         opakující se úkoly a plány.\n\n\
         NEEXTRAHUJ pomíjivé věci: aktuální čas, počasí, dnešní stav, pozdravy, testovací \
         věty, nejasné útržky přepisu. Fakt, který už znám, neopakuj — LEDAže se změnil, \
         pak napiš aktuální verzi (stejné téma v poli \"subject\").\n\n\
         Co už o pánovi vím:\n{known}\n\
         Nové útržky ke zpracování:\n{material}\n\
         Vrať POUZE validní JSON (žádný další text, žádné ``` ploty) přesně v tomto tvaru:\n\
         {{\"facts\":[{{\"kind\":\"profile|preference|fact|relationship|task\",\"subject\":\"krátké téma nebo jméno\",\"text\":\"jedno oznamovací tvrzení česky\",\"confidence\":0.0-1.0}}]}}\n\
         Když nic trvalého nevidíš, vrať {{\"facts\":[]}}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(id: i64, subject: &str, text: &str) -> Fact {
        Fact {
            id,
            kind: "fact".into(),
            subject: subject.into(),
            text: text.into(),
            confidence: 0.8,
            salience: 1.0,
            pinned: false,
            last_seen: 0,
            superseded_by: None,
        }
    }

    /// Precompute tokens (decide takes `&[FactTokens]`, not `&[Fact]`).
    fn tok(fs: &[Fact]) -> Vec<FactTokens> {
        fs.iter().map(FactTokens::from_fact).collect()
    }

    #[test]
    fn decide_touch_on_identical_wording() {
        let existing = tok(&[fact(1, "káva", "Pán pije kávu bez cukru.")]);
        // identical wording (different punctuation/case) → confirm, don't duplicate
        assert_eq!(decide("pán PIJE kávu bez cukru", "káva", &existing), Decision::Touch(1));
    }

    #[test]
    fn decide_supersede_on_same_subject_changed_wording() {
        let existing = tok(&[fact(7, "bydliště", "Pán bydlí v Brně.")]);
        // same topic, large enough overlap, but a changed detail → update
        assert_eq!(
            decide("Pán bydlí v Praze.", "bydliště", &existing),
            Decision::Supersede(7)
        );
    }

    #[test]
    fn decide_insert_when_new_or_low_overlap() {
        let existing = tok(&[fact(1, "káva", "Pán pije kávu bez cukru.")]);
        // completely different topic → new fact
        assert_eq!(decide("Pán má rád jazz.", "hudba", &existing), Decision::Insert);
        // same topic, but small overlap (a different fact about coffee) → prefer new, not supersede
        assert_eq!(
            decide("Kávovar je rozbitý od pondělí.", "káva", &existing),
            Decision::Insert
        );
    }

    #[test]
    fn jaccard_basic() {
        let a_tokens = memory::tokens("pán bydlí v praze");
        let a: HashSet<&String> = a_tokens.iter().collect();
        assert!((jaccard(&a, &memory::tokens("pán bydlí v praze")) - 1.0).abs() < 1e-9);
        assert_eq!(jaccard(&a, &memory::tokens("úplně jiná věta slova")), 0.0);
    }

    #[test]
    fn extracted_json_contract_parses_and_defaults() {
        let ex: Extracted = serde_json::from_str(
            r#"{"facts":[{"kind":"preference","subject":"káva","text":"Pán pije espresso.","confidence":0.9},
                        {"text":"Jen text bez zbytku."}]}"#,
        )
        .unwrap();
        assert_eq!(ex.facts.len(), 2);
        assert_eq!(ex.facts[0].kind, "preference");
        // missing field gets the default (kind=fact, confidence=0.7)
        assert_eq!(ex.facts[1].kind, "fact");
        assert!((ex.facts[1].confidence - 0.7).abs() < 1e-9);
        // empty contract
        let empty: Extracted = serde_json::from_str(r#"{"facts":[]}"#).unwrap();
        assert!(empty.facts.is_empty());
    }

    fn ef(kind: &str, subject: &str, text: &str, conf: f64) -> ExtractedFact {
        ExtractedFact { kind: kind.into(), subject: subject.into(), text: text.into(), confidence: conf }
    }

    #[test]
    fn apply_inserts_dedups_and_supersedes_against_real_db() {
        let conn = db::test_conn();
        // 1) new facts get inserted
        let n = apply(
            &conn,
            &[ef("preference", "káva", "Pán pije espresso.", 0.9), ef("profile", "", "Pán mluví česky.", 0.95)],
            0,
        )
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(db::active_facts(&conn).unwrap().len(), 2);
        // 2) identical wording a second time → touch, no new, no duplicate
        let n = apply(&conn, &[ef("preference", "káva", "Pán pije ESPRESSO.", 0.9)], 0).unwrap();
        assert_eq!(n, 0);
        assert_eq!(db::active_facts(&conn).unwrap().len(), 2);
        // 3) same topic, changed wording → supersede (still 2 active, old one disappears)
        let n = apply(&conn, &[ef("preference", "káva", "Pán pije espresso s mlékem.", 0.9)], 0).unwrap();
        assert_eq!(n, 1);
        let active = db::active_facts(&conn).unwrap();
        assert_eq!(active.len(), 2);
        assert!(active.iter().any(|x| x.text.contains("s mlékem")));
        assert!(!active.iter().any(|x| x.text == "Pán pije espresso."));
        // 4) too-short/empty text gets discarded (transcript hallucination)
        assert_eq!(apply(&conn, &[ef("fact", "", "ok", 0.5)], 0).unwrap(), 0);
        assert_eq!(apply(&conn, &[ef("fact", "", "   ", 0.5)], 0).unwrap(), 0);
        // 5) unknown kind falls back to "fact"
        apply(&conn, &[ef("nesmysl", "", "Pán má rád hory.", 0.7)], 0).unwrap();
        assert!(db::active_facts(&conn).unwrap().iter().any(|x| x.kind == "fact" && x.text.contains("hory")));
    }

    #[test]
    fn build_prompt_has_contract_material_and_known() {
        let existing = vec![fact(1, "káva", "Pán pije kávu bez cukru.")];
        let convs = vec![(10i64, "Kolik je hodin?".to_string(), "Pět, pane.".to_string())];
        let utts = vec![(20i64, "musím zavolat Tomášovi".to_string())];
        let p = build_prompt(&existing, &convs, &utts);
        assert!(p.contains("\"facts\""));
        assert!(p.contains("Pán pije kávu bez cukru")); // known facts in context
        assert!(p.contains("Tomášovi")); // material
        assert!(p.contains("NEEXTRAHUJ"));
    }
}
