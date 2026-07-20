//! Jarvis's long-term memory — hybrid retrieval over the past.
//!
//! Step 1 (phase 1): episodic memory. Alongside the recent exchanges, the
//! conversational prompt gets relevant snippets from earlier conversations
//! and utterances, found via the FTS5 index (migration v7). Follow-up context
//! is further scoped to the current "session" — mornings don't start with
//! yesterday's tail.
//!
//! Retrieval is best-effort: any error (DB, invalid FTS expression) is logged
//! and returns empty — memory must never take down the dialog.

pub mod consolidate;
pub mod embed;
pub mod vector;

use crate::config::{Config, MemoryCfg, Paths};
use crate::store::db;
use crate::util;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{info, warn};

/// RRF fusion damping constant (lexical + vector ranking). 60 is the common choice.
const RRF_K: f64 = 60.0;
/// Source tags for namespacing ids across conversations and utterances during fusion.
const SRC_CONV: u8 = 0;
const SRC_UTT: u8 = 1;

/// Resident cache of the embedding matrix per source. Reloading the whole
/// matrix from the DB + deserializing BLOBs is expensive on the hot voice path
/// (every turn calls `recall`) and grows with history. Invalidated via a cheap
/// signature (count, max ref_id): changes on embedding insert/delete, so a
/// reload only happens after a write, not on every query. Per-process; a
/// concurrent write from another process shows up as a signature change (at
/// most one extra reload).
struct EmbCacheEntry {
    sig: (i64, i64),
    items: Arc<Vec<(i64, Vec<f32>)>>,
}
static EMB_CACHE: OnceLock<Mutex<HashMap<String, EmbCacheEntry>>> = OnceLock::new();

fn embeddings_cached(conn: &Connection, source: &str) -> Result<Arc<Vec<(i64, Vec<f32>)>>> {
    let sig = db::embeddings_signature(conn, source)?;
    let cache = EMB_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = guard.get(source) {
            if e.sig == sig {
                return Ok(e.items.clone());
            }
        }
    }
    // miss / stale signature → load the matrix once and cache it
    let items = Arc::new(db::embeddings_for_source(conn, source)?);
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(source.to_string(), EmbCacheEntry { sig, items: items.clone() });
    Ok(items)
}

/// `jarvis memory …` — long-term memory management.
#[derive(clap::Subcommand)]
pub enum MemoryCmd {
    /// List facts (default: active only; --all includes superseded)
    List {
        #[arg(long)]
        all: bool,
    },
    /// Full-text search over active facts
    Search {
        /// Query; multiple words are joined
        query: Vec<String>,
    },
    /// Manually add a fact to memory
    Add {
        /// Fact text (a single statement)
        text: Vec<String>,
        /// Kind: profile | preference | fact | relationship | task
        #[arg(long, default_value = "fact")]
        kind: String,
        /// Short topic or name (optional)
        #[arg(long, default_value = "")]
        subject: String,
        /// Pin (profile — never forgotten or decayed)
        #[arg(long)]
        pin: bool,
    },
    /// Permanently forget a fact by id (see `memory list`)
    Forget {
        id: i64,
    },
    /// Download the embedding model (once) and backfill the vector index
    /// (facts + conversations + utterances). Turns on semantic search.
    Embed {
        /// Recompute existing embeddings too (otherwise only missing ones)
        #[arg(long)]
        all: bool,
    },
    /// Run consolidation now (extract facts from conversations and utterances)
    Consolidate {
        /// Process the last period (e.g. 24h, 7d) instead of from the watermark
        #[arg(long)]
        since: Option<String>,
        /// Just print the prompt and material, don't call the model
        #[arg(long)]
        dry_run: bool,
    },
}

/// Dispatcher for `jarvis memory`.
pub fn cli(paths: &Paths, cfg: &Config, conn: &Connection, cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::List { all } => {
            let facts = if all { db::all_facts(conn)? } else { db::active_facts(conn)? };
            if facts.is_empty() {
                println!("Paměť je zatím prázdná. Naplní ji noční konsolidace nebo `jarvis memory add`.");
                return Ok(());
            }
            for f in facts {
                let pin = if f.pinned { "📌" } else { "  " };
                let sup = f.superseded_by.map(|i| format!("  (zastíněno #{i})")).unwrap_or_default();
                let subj = if f.subject.is_empty() { String::new() } else { format!(" [{}]", f.subject) };
                println!(
                    "#{:<4} {pin} {}{subj}  ({}, jistota {:.0} %, sal {:.2}){sup}",
                    f.id, f.text, f.kind, f.confidence * 100.0, f.salience
                );
            }
            Ok(())
        }
        MemoryCmd::Search { query } => {
            let q = query.join(" ");
            anyhow::ensure!(!q.trim().is_empty(), "zadej aspoň jedno hledané slovo");
            // hybrid: FTS + vectors (if the index is populated) — same path as the prompt
            let hits = hybrid_facts(conn, &cfg.memory, &cfg.converse.wake_words, &q, 20);
            if hits.is_empty() {
                println!("Nic k „{q}“ v paměti není.");
            }
            for f in hits {
                let subj = if f.subject.is_empty() { String::new() } else { format!(" [{}]", f.subject) };
                println!("#{:<4} {}{subj}  ({})", f.id, f.text, f.kind);
            }
            Ok(())
        }
        MemoryCmd::Add { text, kind, subject, pin } => {
            let text = text.join(" ");
            let text = text.trim();
            anyhow::ensure!(text.chars().count() >= 4, "text faktu je moc krátký");
            anyhow::ensure!(
                ["profile", "preference", "fact", "relationship", "task"].contains(&kind.as_str()),
                "kind musí být profile|preference|fact|relationship|task, je '{kind}'"
            );
            let id = db::insert_fact(conn, &kind, subject.trim(), text, 1.0, pin, "cli")?;
            println!("Zapamatováno jako #{id}{}.", if pin { " (připnuto)" } else { "" });
            Ok(())
        }
        MemoryCmd::Forget { id } => {
            if db::delete_fact(conn, id)? {
                db::delete_embedding(conn, "fact", id)?; // avoid orphaning the vector
                println!("Fakt #{id} zapomenut.");
            } else {
                println!("Fakt #{id} v paměti není.");
            }
            Ok(())
        }
        MemoryCmd::Embed { all } => {
            anyhow::ensure!(
                cfg.memory.vectors,
                "memory.vectors je vypnuté — zapni ho v config.toml (pak `jarvis memory embed`)"
            );
            println!("Ověřuji embedding model (poprvé se stahuje ~470 MB)…");
            embed::ensure_model(&cfg.memory)?;
            println!("Indexuji fakta, konverzace a promluvy (CPU)…");
            let n = embed_backfill(&cfg.memory, conn, all)?;
            println!(
                "Hotovo: {n} nových embeddingů, index má celkem {}. Sémantické hledání je aktivní.",
                db::embedding_count(conn)?
            );
            Ok(())
        }
        MemoryCmd::Consolidate { since, dry_run } => {
            let since_ts = match since {
                Some(spec) => {
                    let secs = crate::config::parse_duration_spec(&spec)?;
                    Some(util::now_ts() - secs as i64)
                }
                None => None,
            };
            let n = consolidate::run(paths, cfg, conn, since_ts, dry_run)?;
            if !dry_run {
                println!("Konsolidace hotová: {n} nových/aktualizovaných faktů.");
            }
            Ok(())
        }
    }
}

/// Short, truncated memory ready for the prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Snippet {
    /// When it was created (epoch seconds) — for ordering and possible dedup.
    pub ts: i64,
    /// Human-readable source kind for the prompt ("dřívější rozhovor" / "zaslechnuto").
    pub kind: &'static str,
    /// Finished snippet text (already assembled and truncated to `snippet_max_chars`).
    pub text: String,
}

/// Memory context for a single conversational prompt.
#[derive(Debug, Default)]
pub struct Recall {
    /// Follow-up context: exchanges from the current session, chronological (oldest first).
    pub recent: Vec<(String, String)>,
    /// Relevant snippets from the past outside the current session (hybrid retrieval).
    pub relevant: Vec<Snippet>,
    /// Semantic facts about the master: pinned profile + relevant retrieved (phase 2).
    pub facts: Vec<db::Fact>,
}

/// Assembles memory context for a question: follow-up (session) + retrieved
/// snippets + semantic facts. Never errors — DB/FTS failures resolve to empty
/// (best-effort), so the dialog never fails because of memory.
pub fn recall(conn: &Connection, cfg: &Config, question: &str) -> Recall {
    let recent = recent_context(conn, cfg);
    // exchanges already shown as follow-up context must not repeat in "retrieved"
    let session_oldest = session_oldest_ts(conn, cfg, &recent);
    let (relevant, facts) = if cfg.memory.enabled {
        (
            relevant(conn, &cfg.memory, &cfg.converse.wake_words, question, session_oldest),
            gather_facts(conn, &cfg.memory, &cfg.converse.wake_words, question),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    Recall { recent, relevant, facts }
}

/// Facts for the prompt: pinned profile takes priority (always), remaining
/// slots get filled by relevant retrieved facts. Cap = `facts_in_prompt`. Best-effort.
fn gather_facts(
    conn: &Connection,
    cfg: &MemoryCfg,
    wake_words: &[String],
    question: &str,
) -> Vec<db::Fact> {
    let cap = cfg.facts_in_prompt;
    if cap == 0 {
        return Vec::new();
    }
    let mut out: Vec<db::Fact> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    match db::pinned_facts(conn) {
        Ok(pinned) => {
            for f in pinned {
                if out.len() >= cap {
                    break;
                }
                if seen.insert(f.id) {
                    out.push(f);
                }
            }
        }
        Err(e) => warn!("paměť: čtení připnutých faktů selhalo: {e:#}"),
    }
    if out.len() >= cap {
        return out;
    }
    // hybrid retrieval for the remaining slots (FTS + vectors via RRF)
    for f in hybrid_facts(conn, cfg, wake_words, question, cap) {
        if out.len() >= cap {
            break;
        }
        if seen.insert(f.id) {
            out.push(f);
        }
    }
    out
}

/// Hybrid FACT search: FTS (lexical) + vectors (semantic) fused via RRF,
/// sorted best-first. Shared by `gather_facts` (for the prompt) and the CLI
/// `memory search`. Degrades to plain FTS without an embedding model.
pub fn hybrid_facts(
    conn: &Connection,
    cfg: &MemoryCfg,
    wake_words: &[String],
    question: &str,
    cap: usize,
) -> Vec<db::Fact> {
    let mut id_to_fact: HashMap<i64, db::Fact> = HashMap::new();
    let mut fts_ids: Vec<i64> = Vec::new();
    if let Some(query) = build_match_query(question, wake_words) {
        match db::search_facts(conn, &query, cap * 2) {
            Ok(hits) => {
                for f in hits {
                    fts_ids.push(f.id);
                    id_to_fact.insert(f.id, f);
                }
            }
            Err(e) => warn!("paměť: FTS hledání ve faktech selhalo: {e:#}"),
        }
    }
    let mut vec_ids: Vec<i64> = Vec::new();
    if cfg.vectors {
        if let Some(qv) = embed::embed_query(cfg, question) {
            if let Ok(items) = embeddings_cached(conn, "fact") {
                for id in vector::knn(&qv, &items, cap * 2) {
                    if !id_to_fact.contains_key(&id) {
                        match db::fact_by_id(conn, id) {
                            Ok(Some(f)) => {
                                id_to_fact.insert(id, f);
                            }
                            _ => continue, // superseded/deleted fact (orphaned embedding)
                        }
                    }
                    vec_ids.push(id);
                }
            }
        }
    }
    let mut out = Vec::new();
    for id in vector::rrf(&[fts_ids, vec_ids], RRF_K) {
        if out.len() >= cap {
            break;
        }
        if let Some(f) = id_to_fact.remove(&id) {
            out.push(f);
        }
    }
    out
}

/// Backfills missing embeddings (facts + conversations + mic utterances) into
/// the index. `full` = recompute existing ones too. Orphaned embeddings of
/// non-superseded facts get cleaned up. Returns the count of newly indexed
/// rows. Verify/download the model first.
pub fn embed_backfill(cfg: &MemoryCfg, conn: &Connection, full: bool) -> Result<usize> {
    let model = &cfg.embed_model;
    let mut done = 0;

    // facts
    let facts = db::active_facts(conn)?;
    let active_ids: HashSet<i64> = facts.iter().map(|f| f.id).collect();
    let have = if full { HashSet::new() } else { db::embedded_ref_ids(conn, "fact")? };
    let todo: Vec<&db::Fact> = facts.iter().filter(|f| !have.contains(&f.id)).collect();
    if !todo.is_empty() {
        let texts: Vec<String> = todo.iter().map(|f| f.text.clone()).collect();
        let vecs = embed::embed_passages(cfg, &texts)?;
        for (f, v) in todo.iter().zip(vecs) {
            db::upsert_embedding(conn, "fact", f.id, model, &v)?;
            done += 1;
        }
    }
    // clean up embeddings of facts that are no longer active
    for id in db::embedded_ref_ids(conn, "fact")? {
        if !active_ids.contains(&id) {
            db::delete_embedding(conn, "fact", id)?;
        }
    }

    // conversations (the whole exchange gets embedded: question + answer)
    let convs = db::all_conversations(conn)?;
    let have = if full { HashSet::new() } else { db::embedded_ref_ids(conn, "conversation")? };
    done += embed_rows(cfg, conn, "conversation", convs.into_iter()
        .filter(|(id, _, _)| !have.contains(id))
        .map(|(id, q, a)| (id, format!("{q} {a}")))
        .collect())?;

    // mic utterances
    let utts = db::all_mic_utterances(conn)?;
    let have = if full { HashSet::new() } else { db::embedded_ref_ids(conn, "utterance")? };
    done += embed_rows(cfg, conn, "utterance", utts.into_iter()
        .filter(|(id, _)| !have.contains(id))
        .collect())?;

    Ok(done)
}

/// Indexes a batch of (ref_id, text) for the given source. Skips empty texts.
fn embed_rows(cfg: &MemoryCfg, conn: &Connection, source: &str, rows: Vec<(i64, String)>) -> Result<usize> {
    let rows: Vec<(i64, String)> = rows.into_iter().filter(|(_, t)| !t.trim().is_empty()).collect();
    if rows.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = rows.iter().map(|(_, t)| t.clone()).collect();
    let vecs = embed::embed_passages(cfg, &texts)?;
    for ((id, _), v) in rows.iter().zip(vecs) {
        db::upsert_embedding(conn, source, *id, &cfg.embed_model, &v)?;
    }
    info!("embed: {} × {source}", rows.len());
    Ok(rows.len())
}

/// Follow-up context scoped to the current session. Without memory enabled
/// (or session_gap_s == 0) it's simply the last N exchanges, as before.
fn recent_context(conn: &Connection, cfg: &Config) -> Vec<(String, String)> {
    let limit = cfg.converse.max_context_exchanges;
    if limit == 0 {
        return Vec::new();
    }
    let gap = if cfg.memory.enabled { cfg.memory.session_gap_s as i64 } else { 0 };
    match db::recent_conversations_ts(conn, limit) {
        Ok(rows) => session_window(&rows, gap, util::now_ts()),
        Err(e) => {
            warn!("paměť: čtení navazujícího kontextu selhalo: {e:#}");
            Vec::new()
        }
    }
}

/// Oldest ts among exchanges included in the follow-up context — a cutoff so
/// retrieval doesn't offer it again as "retrieved". None = no recent context.
fn session_oldest_ts(conn: &Connection, cfg: &Config, recent: &[(String, String)]) -> Option<i64> {
    if recent.is_empty() {
        return None;
    }
    // recent is derived from the same session_window; its length is enough
    // to look up the matching ts from the DB (the most recent `recent.len()` exchanges).
    let rows = db::recent_conversations_ts(conn, cfg.converse.max_context_exchanges).ok()?;
    rows.iter().take(recent.len()).map(|(ts, _, _)| *ts).min()
}

/// From exchanges sorted newest-first (ts DESC), picks only those from the
/// session in progress: walking back from `now` until the gap between
/// consecutive exchanges (or between `now` and the latest exchange) exceeds
/// `gap_s`. Result is chronological (oldest first), ready for the prompt.
/// `gap_s == 0` = no limit (everything).
pub fn session_window(
    rows_desc: &[(i64, String, String)],
    gap_s: i64,
    now: i64,
) -> Vec<(String, String)> {
    if gap_s == 0 {
        return rows_desc.iter().rev().map(|(_, q, a)| (q.clone(), a.clone())).collect();
    }
    let mut out = Vec::new();
    let mut prev_ts = now;
    for (ts, q, a) in rows_desc {
        if prev_ts - ts > gap_s {
            break; // session boundary
        }
        out.push((q.clone(), a.clone()));
        prev_ts = *ts;
    }
    out.reverse();
    out
}

/// Should an utterance be excluded from "zaslechnuto" (overheard) snippets? We
/// exclude live interaction: (1) utterances from the current session
/// (`ts ≥ session_oldest`) — that's an ongoing dialog, not "earlier context";
/// (2) an utterance matching the current question — the master's own speech,
/// which the listen daemon writes to `utterances` BEFORE the brain processes
/// it. Without this, the query would come back into the prompt as "overheard
/// earlier", the brain would read it as an echo (a duplicated, already-heard
/// transcript) and REJECT the live query. Compared via `fold_terms`
/// (diacritics/punctuation/case stripped), so "Jarvisi, jaký je dnes den?" ==
/// "jarvisi jaky je dnes den". Conversations get the same session filter
/// directly in `relevant`.
fn is_live_self_utterance(
    text: &str,
    question: &str,
    ts: i64,
    session_oldest: Option<i64>,
) -> bool {
    session_oldest.is_some_and(|cut| ts >= cut) || fold_terms(text) == fold_terms(question)
}

/// Hybrid retrieval: FTS5 (lexical) + dense vectors (semantic), merged via
/// RRF, across both conversations and utterances. `session_oldest` = cutoff
/// above which conversations aren't taken (already in follow-up context).
/// Silently degrades to plain FTS without an embedding model. Best-effort —
/// errors → empty.
pub fn relevant(
    conn: &Connection,
    cfg: &MemoryCfg,
    wake_words: &[String],
    question: &str,
    session_oldest: Option<i64>,
) -> Vec<Snippet> {
    if cfg.retrieve_k == 0 {
        return Vec::new();
    }
    let k = cfg.retrieve_k;
    let max = cfg.snippet_max_chars;
    // candidate: (source, id) -> (ts, finished snippet text)
    let mut cand: HashMap<(u8, i64), (i64, String)> = HashMap::new();
    let (mut fts_conv, mut fts_utt) = (Vec::new(), Vec::new());
    let (mut vec_conv, mut vec_utt) = (Vec::new(), Vec::new());

    // 1) lexical (FTS) — only when terms remain from the question
    if let Some(query) = build_match_query(question, wake_words) {
        match db::search_conversations(conn, &query, k * 2) {
            Ok(rows) => {
                for (id, ts, q, a, _) in rows {
                    let a = a.unwrap_or_default();
                    let text = util::truncate_chars(&format!("Pán: {q} → Jarvis: {a}"), max);
                    cand.insert((SRC_CONV, id), (ts, text));
                    fts_conv.push((SRC_CONV, id));
                }
            }
            Err(e) => warn!("paměť: FTS konverzace selhalo: {e:#}"),
        }
        match db::search_utterances(conn, &query, k * 2) {
            Ok(rows) => {
                for (id, ts, t, _, _) in rows {
                    cand.insert((SRC_UTT, id), (ts, util::truncate_chars(t.trim(), max)));
                    fts_utt.push((SRC_UTT, id));
                }
            }
            Err(e) => warn!("paměť: FTS promluvy selhalo: {e:#}"),
        }
    }

    // 2) semantic (vectors) — works even for a question without "searchable" words
    if cfg.vectors {
        if let Some(qv) = embed::embed_query(cfg, question) {
            for (src, source, is_conv) in
                [(SRC_CONV, "conversation", true), (SRC_UTT, "utterance", false)]
            {
                let Ok(items) = embeddings_cached(conn, source) else { continue };
                for id in vector::knn(&qv, &items, k * 2) {
                    let key = (src, id);
                    if !cand.contains_key(&key) {
                        let fetched = if is_conv {
                            db::conversation_by_id(conn, id).ok().flatten().map(|(ts, q, a)| {
                                (ts, util::truncate_chars(&format!("Pán: {q} → Jarvis: {a}"), max))
                            })
                        } else {
                            db::utterance_by_id(conn, id)
                                .ok()
                                .flatten()
                                .map(|(ts, t)| (ts, util::truncate_chars(t.trim(), max)))
                        };
                        let Some(entry) = fetched else { continue };
                        cand.insert(key, entry);
                    }
                    if is_conv {
                        vec_conv.push(key);
                    } else {
                        vec_utt.push(key);
                    }
                }
            }
        }
    }

    // 3) RRF fusion separately per source, then collect alternately (both get room)
    let fused_conv = vector::rrf(&[fts_conv, vec_conv], RRF_K);
    let fused_utt = vector::rrf(&[fts_utt, vec_utt], RRF_K);
    let mut out: Vec<Snippet> = Vec::new();
    let (mut ci, mut ui) = (fused_conv.into_iter(), fused_utt.into_iter());
    loop {
        if out.len() >= k {
            break;
        }
        let mut progressed = false;
        if let Some(key) = ci.next() {
            progressed = true;
            if let Some((ts, text)) = cand.get(&key) {
                // exchanges from the current session are already in the follow-up context
                if !session_oldest.is_some_and(|cut| *ts >= cut) {
                    out.push(Snippet { ts: *ts, kind: "dřívější rozhovor", text: text.clone() });
                }
            }
        }
        if out.len() >= k {
            break;
        }
        if let Some(key) = ui.next() {
            progressed = true;
            if let Some((ts, text)) = cand.get(&key) {
                // don't mirror the master's own live speech back as "overheard earlier"
                if !is_live_self_utterance(text, question, *ts, session_oldest) {
                    out.push(Snippet { ts: *ts, kind: "zaslechnuto", text: text.clone() });
                }
            }
        }
        if !progressed {
            break;
        }
    }
    out
}

/// Czech + a few English stopwords (already folded to ASCII, lowercase). Short
/// words (< 2 chars) are dropped separately; names/terms (Tomáš, smlouva) stay.
const STOPWORDS: &[&str] = &[
    "a", "i", "o", "u", "v", "s", "k", "z", "na", "do", "od", "po", "za", "ze", "se", "si", "je",
    "to", "ta", "ten", "ty", "co", "ze", "ale", "nebo", "jak", "kdy", "kde", "kdo", "proc", "pro",
    "me", "mi", "my", "ty", "vy", "on", "ona", "ono", "byl", "byla", "bylo", "bude", "jsem", "jste",
    "the", "and", "for", "you",
];

/// Builds an FTS5 MATCH expression from the question: splits into words,
/// folds diacritics, drops stopwords, wake address (wake words + "pane") and
/// short words, joins the rest via OR as quoted terms. None = nothing left to search.
pub fn build_match_query(question: &str, wake_words: &[String]) -> Option<String> {
    let wake: HashSet<String> = wake_words.iter().map(|w| fold_word(w)).collect();
    let mut seen = HashSet::new();
    let terms: Vec<String> = fold_terms(question)
        .into_iter()
        .filter(|t| t.chars().count() >= 2)
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .filter(|t| !wake.contains(t) && t != "pane" && t != "pan")
        .filter(|t| seen.insert(t.clone()))
        .take(12) // cap for absurdly long utterances (and FTS query size)
        .collect();
    if terms.is_empty() {
        return None;
    }
    Some(terms.iter().map(|t| fts_token(t)).collect::<Vec<_>>().join(" OR "))
}

/// Folded term → FTS5 token. Longer words go as a stem prefix (`"smlou"*`) to
/// match Czech inflected forms too (smlouva/smlouvu/smlouvou) — unicode61
/// doesn't stem, so without this lexical retrieval would miss a lot on Czech.
/// Short words (≤ 4 chars) go exact (a stem would be too generic). Always
/// quoted → safe against FTS5 keywords (OR/AND/NOT/NEAR).
fn fts_token(term: &str) -> String {
    let len = term.chars().count();
    if len <= 4 {
        format!("\"{term}\"")
    } else {
        // stem = strip ~the case ending, but keep at least 4 chars (precision)
        let plen = std::cmp::max(4, len - 3);
        let stem: String = term.chars().take(plen).collect();
        format!("\"{stem}\"*")
    }
}

/// Normalized text tokens (folded, lowercase) — shared with consolidation for
/// fact dedup, to match what goes into FTS.
pub(crate) fn tokens(text: &str) -> Vec<String> {
    fold_terms(text)
}

/// Splits text into words (boundary = anything non-alphanumeric) and folds each.
fn fold_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(fold_word)
        .collect()
}

/// Lowercase + Czech diacritics folded to ASCII (same map as FTS
/// `remove_diacritics`, so terms match the index). Other characters pass through.
fn fold_word(w: &str) -> String {
    w.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'á' => 'a',
            'č' => 'c',
            'ď' => 'd',
            'é' | 'ě' => 'e',
            'í' => 'i',
            'ň' => 'n',
            'ó' => 'o',
            'ř' => 'r',
            'š' => 's',
            'ť' => 't',
            'ú' | 'ů' => 'u',
            'ý' => 'y',
            'ž' => 'z',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_query_strips_wake_stopwords_and_folds() {
        let wake = vec!["jarvisi".to_string(), "jarvise".to_string()];
        // "Jarvisi" (wake), "kdy" (stopword), "mám" (>=2, kept) …
        let q = build_match_query("Jarvisi, kdy mám tu schůzku s Tomášem?", &wake).unwrap();
        // folded; longer words as stem prefix, short ones exact; OR-joined;
        // without wake address and stopwords
        assert!(q.contains("\"schu\"*"), "schůzku → prefix na kmen; q = {q}");
        assert!(q.contains("\"toma\"*"), "Tomášem → prefix na kmen; q = {q}");
        assert!(q.contains("\"mam\""), "mám (3 znaky) → přesně; q = {q}");
        assert!(!q.contains("jarvis"));
        assert!(!q.contains("kdy"));
        assert!(q.contains(" OR "));
    }

    #[test]
    fn match_query_none_when_only_stopwords_and_wake() {
        let wake = vec!["jarvisi".to_string()];
        assert!(build_match_query("Jarvisi, a co to je, pane?", &wake).is_none());
        assert!(build_match_query("", &wake).is_none());
    }

    #[test]
    fn match_query_dedups_terms() {
        // dedup after folding; "smlouva" (7 chars) goes as a stem prefix
        let q = build_match_query("smlouva smlouva SMLOUVA", &[]).unwrap();
        assert_eq!(q, "\"smlo\"*");
    }

    #[test]
    fn fts_token_prefix_vs_exact() {
        assert_eq!(fts_token("auto"), "\"auto\""); // ≤4 → exact
        assert_eq!(fts_token("mam"), "\"mam\"");
        assert_eq!(fts_token("smlouvou"), "\"smlou\"*"); // 8 → stem 5
        assert_eq!(fts_token("hodinu"), "\"hodi\"*"); // 6 → stem 4 (min)
    }

    #[test]
    fn session_window_breaks_on_gap() {
        // ts DESC; now = 1000, gap = 60 s
        let rows = vec![
            (990, "q3".to_string(), "a3".to_string()), // 10 s before now → in session
            (950, "q2".to_string(), "a2".to_string()), // 40 s before q3 → in session
            (800, "q1".to_string(), "a1".to_string()), // 150 s before q2 → past the boundary
        ];
        let out = session_window(&rows, 60, 1000);
        // only q2,q3 and chronological (oldest first)
        assert_eq!(out, vec![("q2".into(), "a2".into()), ("q3".into(), "a3".into())]);
    }

    #[test]
    fn session_window_empty_when_last_exchange_stale() {
        // last exchange 1 h ago, gap 30 min → new session, nothing carries over
        let rows = vec![(1000, "q".to_string(), "a".to_string())];
        assert!(session_window(&rows, 1800, 1000 + 3600).is_empty());
    }

    #[test]
    fn session_window_zero_gap_returns_all() {
        let rows = vec![
            (990, "q2".to_string(), "a2".to_string()),
            (100, "q1".to_string(), "a1".to_string()),
        ];
        let out = session_window(&rows, 0, 1000);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "q1"); // chronological
    }

    // --- "zaslechnuto" must not mirror the master's own live speech (self-poisoning) ---

    #[test]
    fn live_self_utterance_dropped_when_echoes_question() {
        // master's speech written to utterances == current question → exclude, even without a session
        assert!(is_live_self_utterance(
            "Jarvisi, jaký je dnes den?",
            "Jarvisi, jaký je dnes den?",
            500,
            None
        ));
        // diacritics / case / punctuation don't matter (fold_terms)
        assert!(is_live_self_utterance(
            "jarvisi jaky je DNES den",
            "Jarvisi, jaký je dnes den?",
            500,
            None
        ));
    }

    #[test]
    fn live_self_utterance_dropped_from_current_session() {
        // ts within the current session (>= cutoff) = ongoing dialog → exclude, even if different text
        assert!(is_live_self_utterance("úplně jiná věta", "jaký je dnes den", 1000, Some(900)));
    }

    #[test]
    fn old_unrelated_utterance_kept() {
        // old (before the session) and different text = legitimate earlier context → keep
        assert!(!is_live_self_utterance("kdy mi jede vlak", "jaký je dnes den", 100, Some(900)));
        // no current session and different text → keep
        assert!(!is_live_self_utterance("kdy mi jede vlak", "jaký je dnes den", 100, None));
    }

    // DB-backed integration over real FTS: `relevant` must not return the master's
    // own live utterance (== the question the listen daemon wrote to `utterances`
    // BEFORE the brain processed it) as "zaslechnuto" — otherwise the brain rejects the live query.
    #[test]
    fn relevant_excludes_own_live_utterance_via_fts() {
        let conn = db::test_conn();
        let q = "Jarvisi, jaký je dnes den?";
        // master's question just written to utterances (ts "now")
        db::insert_utterance(&conn, 10_000, 10_000, q, "cs", 0.96, "mic").unwrap();
        // older, but relevant to the question utterance = legitimate "zaslechnuto"
        db::insert_utterance(&conn, 100, 100, "Jaký je dnes stav integrace, nevíš?", "cs", 0.9, "mic")
            .unwrap();
        let cfg = MemoryCfg { vectors: false, ..MemoryCfg::default() };
        let wake = vec!["jarvisi".to_string()];
        let snips = relevant(&conn, &cfg, &wake, q, None);
        // the own live utterance (matching the question) must NOT be returned
        assert!(
            !snips.iter().any(|s| fold_terms(&s.text) == fold_terms(q)),
            "vlastní živá promluva prosákla zpět jako zaslechnuto: {snips:?}"
        );
        // but a legitimate older relevant utterance is returned (fix must not over-filter)
        assert!(
            snips.iter().any(|s| s.text.contains("integrace")),
            "legitimní dřívější promluva se ztratila: {snips:?}"
        );
    }
}
