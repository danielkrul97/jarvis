use crate::config::{Config, Paths};
use crate::pipeline::claude::{self, ClaudeRequest};
use crate::store::db;
use crate::util;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Pattern {
    pub id: i64,
    pub description: String,
    pub occurrences: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub status: String,
}

/// Stores automation_hints from the hourly analysis; the same (normalized)
/// hint bumps occurrences — repetition is a signal for automation.
pub fn record_hints(conn: &Connection, hints: &[String]) -> Result<()> {
    let now = crate::util::now_ts();
    for h in hints {
        let desc = h.trim();
        // hints that are too short are noise
        if desc.chars().count() < 8 {
            continue;
        }
        let key = normalize_key(desc);
        conn.execute(
            "INSERT INTO patterns(key, description, occurrences, first_seen, last_seen)
             VALUES(?1, ?2, 1, ?3, ?3)
             ON CONFLICT(key) DO UPDATE SET
               occurrences = occurrences + 1,
               last_seen = ?3,
               description = ?2",
            params![key, desc, now],
        )?;
    }
    Ok(())
}

pub fn normalize_key(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

fn row_to_pattern(r: &rusqlite::Row) -> rusqlite::Result<Pattern> {
    Ok(Pattern {
        id: r.get(0)?,
        description: r.get(1)?,
        occurrences: r.get(2)?,
        first_seen: r.get(3)?,
        last_seen: r.get(4)?,
        status: r.get(5)?,
    })
}

const COLS: &str = "id, description, occurrences, first_seen, last_seen, status";

/// Patterns with at least min_occurrences (for the digest's Automation opportunities section).
pub fn top(conn: &Connection, min_occurrences: i64, limit: usize) -> Result<Vec<Pattern>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM patterns
         WHERE occurrences >= ?1 AND status IN ('candidate','proposed')
         ORDER BY occurrences DESC, last_seen DESC LIMIT ?2"
    ))?;
    let rows: Vec<Pattern> = stmt
        .query_map(params![min_occurrences, limit as i64], row_to_pattern)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn all(conn: &Connection) -> Result<Vec<Pattern>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM patterns ORDER BY occurrences DESC, last_seen DESC"
    ))?;
    let rows: Vec<Pattern> = stmt
        .query_map([], row_to_pattern)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Pattern>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM patterns WHERE id = ?1"),
        params![id],
        row_to_pattern,
    )
    .optional()
    .map_err(Into::into)
}

/// Most frequent candidate pattern (for `jarvis propose` with no argument).
pub fn best_candidate(conn: &Connection) -> Result<Option<Pattern>> {
    conn.query_row(
        &format!(
            "SELECT {COLS} FROM patterns WHERE status = 'candidate'
             ORDER BY occurrences DESC, last_seen DESC LIMIT 1"
        ),
        [],
        row_to_pattern,
    )
    .optional()
    .map_err(Into::into)
}

pub fn set_status(conn: &Connection, id: i64, status: &str) -> Result<()> {
    conn.execute("UPDATE patterns SET status = ?2 WHERE id = ?1", params![id, status])?;
    Ok(())
}

// ---------- phase C: generating automation proposals ----------

#[derive(Debug, Deserialize)]
struct ProposalJson {
    #[serde(default)]
    filename: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    install_hint: String,
}

pub fn print_list(conn: &Connection) -> Result<()> {
    let pats = all(conn)?;
    if pats.is_empty() {
        println!("Zatím žádné detekované vzory — nech Jarvise pár dní pozorovat.");
        return Ok(());
    }
    println!("{:>4}  {:>4}  {:<10}  {:<10}  popis", "id", "×", "status", "naposledy");
    for p in pats {
        println!(
            "{:>4}  {:>4}  {:<10}  {:<10}  {}",
            p.id,
            p.occurrences,
            p.status,
            util::fmt_local(p.last_seen).split(' ').next().unwrap_or(""),
            util::truncate_chars(&p.description, 90)
        );
    }
    Ok(())
}

/// Generates an automation artifact for a pattern (default: most frequent
/// candidate) into proposals/, marks the pattern `proposed`, and announces it
/// on the configured channels (Telegram/SMS — remote approval).
pub fn propose(paths: &Paths, cfg: &Config, conn: &Connection, id: Option<i64>) -> Result<()> {
    let pat = match id {
        Some(i) => get(conn, i)?.with_context(|| format!("vzor #{i} neexistuje (viz --list)"))?,
        None => best_candidate(conn)?
            .context("žádný kandidátní vzor — nech Jarvise pár dní pozorovat (viz --list)")?,
    };
    println!("Generuji návrh automatizace pro vzor #{}: {}", pat.id, pat.description);

    let prompt = format!(
        "Jsi Jarvis, můj osobní asistent pro automatizaci práce. V mé denní činnosti \
         jsem opakovaně pozoroval tento ruční vzor:\n\n\
         „{desc}“ (viděno {n}×, poprvé {first}, naposledy {last})\n\n\
         Prostředí: Linux, X11, systemd user units, Claude Code CLI (`claude -p`), \
         bash, Rust. Navrhni JEDEN konkrétní, bezpečný a idempotentní automatizační \
         artefakt, který tento ruční krok odstraní nebo výrazně zkrátí.\n\n\
         Odpověz POUZE validním JSON objektem (žádné ``` ploty):\n\
         {{\n\
         \x20 \"filename\": \"nazev-souboru.sh\",\n\
         \x20 \"kind\": \"shell-script|systemd-timer|claude-skill|other\",\n\
         \x20 \"description\": \"1–2 věty česky, co artefakt dělá\",\n\
         \x20 \"content\": \"kompletní obsah souboru, okomentovaný, česky\",\n\
         \x20 \"install_hint\": \"přesné kroky nasazení, česky\"\n\
         }}",
        desc = pat.description,
        n = pat.occurrences,
        first = util::fmt_local(pat.first_seen),
        last = util::fmt_local(pat.last_seen),
    );
    let outcome = claude::run(&ClaudeRequest {
        prompt,
        model: None, // quality over cost; default claude CLI model
        cwd: &paths.data_dir,
        allowed_tools: "Read",
        max_turns: 3,
        timeout: Duration::from_secs(600),
    })?;
    db::insert_cost(
        conn,
        util::now_ts(),
        "propose",
        "default",
        outcome.tokens_in,
        outcome.tokens_out,
        outcome.cost_usd,
    )?;
    let p: ProposalJson = serde_json::from_str(claude::extract_json(&outcome.text)?)
        .context("odpověď neodpovídá kontraktu návrhu")?;
    if p.content.trim().is_empty() {
        anyhow::bail!("návrh má prázdný obsah");
    }

    let fname = sanitize_filename(&p.filename);
    let path = paths.proposals_dir.join(format!("{}-{}", pat.id, fname));
    std::fs::write(&path, &p.content)
        .with_context(|| format!("nelze zapsat {}", path.display()))?;
    if fname.ends_with(".sh") {
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    conn.execute(
        "INSERT INTO proposals(pattern_id, kind, path, created_at) VALUES(?1, ?2, ?3, ?4)",
        params![pat.id, p.kind, path.display().to_string(), util::now_ts()],
    )?;
    let proposal_id = conn.last_insert_rowid();
    set_status(conn, pat.id, "proposed")?;
    crate::runbook::announce_proposal(paths, cfg, conn, proposal_id, &p.kind, &pat.description);

    println!("✓ Návrh #{proposal_id} uložen: {}", path.display());
    println!("  Typ:       {}", p.kind);
    println!("  Popis:     {}", p.description);
    println!("  Nasazení:  {}", p.install_hint);
    println!("  (Nic se neinstaluje ani nespouští samo — rozhodnutí je na tobě.)");
    if p.kind == "shell-script" || fname.ends_with(".sh") {
        println!("  Ke spouštění schválíš: jarvis runbook approve {proposal_id}");
    }
    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(80)
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() { "automation.txt".into() } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db;

    #[test]
    fn normalize_collapses_case_and_whitespace() {
        assert_eq!(normalize_key("  Ruční   KOPÍROVÁNÍ dat  "), "ruční kopírování dat");
        assert_eq!(normalize_key(&"x".repeat(500)).chars().count(), 120);
    }

    #[test]
    fn record_hints_dedupes_and_counts() {
        let conn = db::test_conn();
        record_hints(&conn, &["Ruční kopírování dat z A do B".into()]).unwrap();
        record_hints(&conn, &["ruční  kopírování dat z a do b".into()]).unwrap();
        record_hints(&conn, &["krátké".into()]).unwrap(); // noise — discarded
        let pats = all(&conn).unwrap();
        assert_eq!(pats.len(), 1);
        assert_eq!(pats[0].occurrences, 2);
        assert_eq!(pats[0].status, "candidate");
    }

    #[test]
    fn top_filters_by_occurrences_and_status() {
        let conn = db::test_conn();
        record_hints(&conn, &["opakovaný ruční import CSV".into()]).unwrap();
        record_hints(&conn, &["opakovaný ruční import CSV".into()]).unwrap();
        record_hints(&conn, &["jednorázová věc, dlouhý popis".into()]).unwrap();
        assert_eq!(top(&conn, 2, 10).unwrap().len(), 1);
        let id = all(&conn).unwrap()[0].id;
        set_status(&conn, id, "dismissed").unwrap();
        assert_eq!(top(&conn, 2, 10).unwrap().len(), 0);
        assert_eq!(get(&conn, id).unwrap().unwrap().status, "dismissed");
    }

    #[test]
    fn filename_sanitization() {
        assert_eq!(sanitize_filename("check-scraper.sh"), "check-scraper.sh");
        assert_eq!(sanitize_filename("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_filename("há čky.sh"), "hky.sh");
        assert_eq!(sanitize_filename(""), "automation.txt");
        assert_eq!(sanitize_filename("..."), "automation.txt");
    }

    #[test]
    fn best_candidate_prefers_most_frequent() {
        let conn = db::test_conn();
        record_hints(&conn, &["vzor A dlouhý popis".into()]).unwrap();
        for _ in 0..3 {
            record_hints(&conn, &["vzor B dlouhý popis".into()]).unwrap();
        }
        assert_eq!(best_candidate(&conn).unwrap().unwrap().description, "vzor B dlouhý popis");
    }
}
