use crate::config::{Config, Paths};
use crate::patterns::{self, Pattern};
use crate::pipeline::analyze::HourlyJson;
use crate::pipeline::claude::{self, ClaudeRequest};
use crate::pipeline::segment::{self, Segment};
use crate::store::db;
use crate::util;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::Connection;
use std::time::Duration;
use tracing::{info, warn};

pub struct DayData {
    pub date: NaiveDate,
    pub segments: Vec<Segment>,
    pub summaries: Vec<HourlyJson>,
    pub degraded_count: usize,
    pub patterns: Vec<Pattern>,
    pub runbook_runs: Vec<crate::runbook::RunRow>,
    pub pending_proposals: usize,
    pub cost_usd: f64,
}

/// Sestaví digest pro daný den a vrátí (markdown, html). S `persist` ho uloží
/// do DB (status pending → doručovací smyčka ho odešle); dry-run persist
/// nesmí, jinak by se náhled později odeslal sám.
pub fn build(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    date: NaiveDate,
    persist: bool,
) -> Result<(String, String)> {
    let data = collect(cfg, conn, date)?;
    let mut md = generate_markdown(paths, cfg, conn, &data);
    md.push_str(&format!(
        "\n\n---\n*Náklady Jarvise za den: {:.2} USD*\n",
        data.cost_usd
    ));
    let html = super::render::render_email(&md, date);
    if persist {
        db::upsert_digest(conn, &date.format("%Y-%m-%d").to_string(), &md, &html)?;
    }
    Ok((md, html))
}

fn collect(cfg: &Config, conn: &Connection, date: NaiveDate) -> Result<DayData> {
    let (from, to) = util::day_bounds_local(date)?;
    let samples = db::samples_between(conn, from, to)?;
    let idle_ms = (cfg.capture.idle_threshold_s * 1000) as i64;
    let segments = segment::segment(&samples, cfg.capture.meta_interval_s as i64, idle_ms);
    let summary_rows = db::summaries_between(conn, from, to)?;
    let degraded_count = summary_rows.iter().filter(|r| r.degraded).count();
    let summaries: Vec<HourlyJson> = summary_rows
        .iter()
        .filter_map(|r| serde_json::from_str(&r.json).ok())
        .collect();
    let pats = patterns::top(conn, 2, 8)?;
    let runbook_runs = crate::runbook::runs_between(conn, from, to)?;
    let pending_proposals = crate::runbook::pending_proposals(conn)?.len();
    let cost_usd = db::cost_between(conn, from, to)?;
    Ok(DayData {
        date,
        segments,
        summaries,
        degraded_count,
        patterns: pats,
        runbook_runs,
        pending_proposals,
        cost_usd,
    })
}

fn generate_markdown(paths: &Paths, cfg: &Config, conn: &Connection, data: &DayData) -> String {
    if data.segments.is_empty() && data.summaries.is_empty() {
        return format!(
            "# Jarvis digest — {}\n\nDnes jsem nezaznamenal žádnou aktivitu. \
             Buď byl klid, nebo capture démon neběžel (`jarvis status` napoví).",
            data.date.format("%Y-%m-%d")
        );
    }
    let base = deterministic_markdown(data);
    match generate_via_claude(paths, cfg, conn, data) {
        Ok(md) => md,
        Err(e) => {
            warn!("digest přes Claude selhal: {e:#} — posílám deterministický fallback");
            base
        }
    }
}

fn generate_via_claude(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    data: &DayData,
) -> Result<String> {
    let prompt = build_prompt(data)?;
    let model = if cfg.digest.model.is_empty() { None } else { Some(cfg.digest.model.as_str()) };
    let outcome = claude::run(&ClaudeRequest {
        prompt,
        model,
        cwd: &paths.data_dir,
        allowed_tools: "Read",
        max_turns: 3,
        timeout: Duration::from_secs(cfg.analysis.timeout_s),
    })?;
    db::insert_cost(
        conn,
        util::now_ts(),
        "digest",
        if cfg.digest.model.is_empty() { "default" } else { &cfg.digest.model },
        outcome.tokens_in,
        outcome.tokens_out,
        outcome.cost_usd,
    )?;
    let md = strip_fences(outcome.text.trim());
    if !md.starts_with('#') {
        anyhow::bail!("odpověď nevypadá jako Markdown digest");
    }
    info!("digest vygenerován ({} znaků, {:.4} USD)", md.chars().count(), outcome.cost_usd);
    Ok(md.to_string())
}

/// Deterministická kostra: vždy k dispozici, slouží i jako fallback e-mail.
pub fn deterministic_markdown(data: &DayData) -> String {
    let mut md = format!("# Jarvis digest — {}\n", data.date.format("%Y-%m-%d"));

    let total_s: i64 = data.segments.iter().map(Segment::duration_s).sum();
    if let (Some(first), Some(last)) = (data.segments.first(), data.segments.last()) {
        md.push_str(&format!(
            "\n## Přehled dne\nAktivní čas u počítače: **{}** ({} – {}).\n",
            fmt_minutes(total_s),
            util::fmt_hm(first.start),
            util::fmt_hm(last.end)
        ));
    }

    md.push_str("\n## Rozložení času\n\n| Aplikace | Čas |\n|---|---|\n");
    for (class, secs) in segment::seconds_by_class(&data.segments).into_iter().take(10) {
        let name = if class.is_empty() { "(neznámé)".to_string() } else { class };
        md.push_str(&format!("| {} | {} |\n", name, fmt_minutes(secs)));
    }

    let activities: Vec<String> = data
        .summaries
        .iter()
        .flat_map(|s| s.activities.iter())
        .map(|a| {
            format!(
                "- {}–{} **{}**: {}",
                a.start,
                a.end,
                if a.project.is_empty() { &a.app } else { &a.project },
                a.what
            )
        })
        .collect();
    if !activities.is_empty() {
        md.push_str("\n## Na čem jsi pracoval\n");
        md.push_str(&activities.join("\n"));
        md.push('\n');
    }

    let notable: Vec<String> = data
        .summaries
        .iter()
        .flat_map(|s| s.notable.iter())
        .map(|n| format!("- {n}"))
        .collect();
    if !notable.is_empty() {
        md.push_str("\n## Nedokončené věci a postřehy\n");
        md.push_str(&notable.join("\n"));
        md.push('\n');
    }

    if !data.patterns.is_empty() {
        md.push_str("\n## Automatizační příležitosti\n");
        for p in &data.patterns {
            md.push_str(&format!("- {} *(viděno {}×)*\n", p.description, p.occurrences));
        }
        md.push_str("\nNávrh automatizace vygeneruje `jarvis propose`.\n");
    }

    if !data.runbook_runs.is_empty() || data.pending_proposals > 0 {
        md.push_str("\n## Automatizace (runbooky)\n");
        for r in &data.runbook_runs {
            let state = match (r.finished_at, r.exit_code) {
                (Some(_), Some(0)) => "✓".to_string(),
                (Some(_), Some(c)) => format!("✗ exit {c}"),
                (Some(_), None) => "✗ timeout".to_string(),
                (None, _) => "⚠ nedoběhl".to_string(),
            };
            md.push_str(&format!(
                "- {} **{}** ({}) {}\n",
                util::fmt_hm(r.started_at),
                r.name,
                r.trigger,
                state
            ));
        }
        if data.pending_proposals > 0 {
            md.push_str(&format!(
                "\n{} návrh(y) čekají na schválení — `jarvis runbook pending`.\n",
                data.pending_proposals
            ));
        }
    }

    if data.degraded_count > 0 {
        md.push_str(&format!(
            "\n> {} hodinových souhrnů běželo bez Claude (rozpočet/chyba) — jen z titulků oken.\n",
            data.degraded_count
        ));
    }
    md
}

fn build_prompt(data: &DayData) -> Result<String> {
    let timeline = segment::render_timeline(&data.segments, 40);
    let by_class: Vec<String> = segment::seconds_by_class(&data.segments)
        .into_iter()
        .take(10)
        .map(|(c, s)| format!("{c}: {}", fmt_minutes(s)))
        .collect();
    let summaries_json = serde_json::to_string(&data.summaries).context("serializace souhrnů")?;
    let patterns_txt: Vec<String> = data
        .patterns
        .iter()
        .map(|p| format!("- {} (viděno {}×, id {})", p.description, p.occurrences, p.id))
        .collect();
    let runbook_txt: Vec<String> = data
        .runbook_runs
        .iter()
        .map(|r| {
            let state = match (r.finished_at, r.exit_code) {
                (Some(_), Some(0)) => "OK".to_string(),
                (Some(_), Some(c)) => format!("selhal (exit {c})"),
                (Some(_), None) => "zabit timeoutem".to_string(),
                (None, _) => "nedoběhl".to_string(),
            };
            format!("- {} „{}“ ({}): {state}", util::fmt_hm(r.started_at), r.name, r.trigger)
        })
        .collect();

    Ok(format!(
        "Jsi Jarvis, můj osobní pracovní asistent (mluvíš česky, věcně, přátelsky, \
         bez patosu). Sestav můj denní e-mailový digest za {date}.\n\n\
         PODKLADY\n\
         Časová osa oken:\n{timeline}\n\n\
         Čas podle aplikací: {by_class}\n\n\
         Hodinové souhrny (JSON z průběžné analýzy):\n{summaries_json}\n\n\
         Opakované vzory vhodné k automatizaci:\n{patterns}\n\n\
         Běhy schválených automatizací (runbooků) — {pending} návrhů čeká na \
         schválení:\n{runbooks}\n\n\
         ÚKOL\n\
         Vrať POUZE Markdown (žádné ``` ploty, žádný text okolo), začni řádkem \
         `# Jarvis digest — {date}`. Sekce (vynech prázdné):\n\
         ## Přehled dne — 2–3 věty, co byl hlavní tah dne\n\
         ## Na čem jsi pracoval — odrážky s časy, projekty, konkréty\n\
         ## Rozložení času — Markdown tabulka | Aplikace/Projekt | Čas |\n\
         ## Postřehy — fokus vs. přepínání kontextu, vzorce, co stálo za povšimnutí\n\
         ## Nedokončené věci — co zůstalo rozdělané (z `notable`)\n\
         ## Doporučení na zítřek — max 3 konkrétní, akční doporučení\n\
         ## Automatizační příležitosti — z opakovaných vzorů; u každého přidej \
         `(jarvis propose --pattern ID)`\n\
         ## Automatizace (runbooky) — co běželo samo a jak dopadlo; selhání \
         zmiň výrazně; když čekají návrhy na schválení, připomeň \
         `jarvis runbook pending`\n\
         Buď konkrétní (názvy souborů, projektů, čísla). Žádné vymýšlení — jen co je v podkladech.",
        date = data.date.format("%Y-%m-%d"),
        timeline = timeline,
        by_class = by_class.join(", "),
        summaries_json = summaries_json,
        patterns = if patterns_txt.is_empty() { "(žádné)".to_string() } else { patterns_txt.join("\n") },
        pending = data.pending_proposals,
        runbooks = if runbook_txt.is_empty() { "(žádné)".to_string() } else { runbook_txt.join("\n") },
    ))
}

fn strip_fences(text: &str) -> &str {
    let t = text.trim();
    let t = t.strip_prefix("```markdown").or_else(|| t.strip_prefix("```md")).or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim()
}

fn fmt_minutes(secs: i64) -> String {
    let mins = (secs + 30) / 60;
    if mins >= 60 {
        format!("{} h {} min", mins / 60, mins % 60)
    } else {
        format!("{mins} min")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::analyze::Activity;

    fn seg(start: i64, end: i64, class: &str, title: &str) -> Segment {
        Segment {
            wm_class: class.into(),
            title: title.into(),
            start,
            end,
            samples: 1,
            shots: vec![],
        }
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 17).unwrap()
    }

    #[test]
    fn deterministic_md_has_sections() {
        let data = DayData {
            date: day(),
            segments: vec![seg(0, 3600, "vim", "main.rs"), seg(3600, 5400, "firefox", "docs")],
            summaries: vec![HourlyJson {
                activities: vec![Activity {
                    start: "10:00".into(),
                    end: "11:00".into(),
                    project: "jarvis".into(),
                    what: "psaní kódu".into(),
                    app: "vim".into(),
                }],
                notable: vec!["rozdělaný PLAN.md".into()],
                ..Default::default()
            }],
            degraded_count: 1,
            patterns: vec![],
            runbook_runs: vec![
                crate::runbook::RunRow {
                    runbook_id: 1,
                    name: "ranní sync".into(),
                    started_at: 3600,
                    finished_at: Some(3660),
                    exit_code: Some(0),
                    trigger: "timer".into(),
                    output: String::new(),
                },
                crate::runbook::RunRow {
                    runbook_id: 2,
                    name: "zlobivý".into(),
                    started_at: 7200,
                    finished_at: Some(7300),
                    exit_code: None,
                    trigger: "voice".into(),
                    output: String::new(),
                },
            ],
            pending_proposals: 2,
            cost_usd: 0.05,
        };
        let md = deterministic_markdown(&data);
        assert!(md.contains("# Jarvis digest — 2026-07-17"));
        assert!(md.contains("## Rozložení času"));
        assert!(md.contains("## Automatizace (runbooky)"));
        assert!(md.contains("ranní sync"));
        assert!(md.contains("✗ timeout"));
        assert!(md.contains("2 návrh(y) čekají na schválení"));
        assert!(md.contains("| vim | 1 h 0 min |"));
        assert!(md.contains("psaní kódu"));
        assert!(md.contains("rozdělaný PLAN.md"));
        assert!(md.contains("bez Claude"));
    }

    #[test]
    fn empty_day_message() {
        let data = DayData {
            date: day(),
            segments: vec![],
            summaries: vec![],
            degraded_count: 0,
            patterns: vec![],
            runbook_runs: vec![],
            pending_proposals: 0,
            cost_usd: 0.0,
        };
        let md = deterministic_markdown(&data);
        assert!(md.contains("# Jarvis digest"));
        assert!(!md.contains("## Automatizace (runbooky)"));
    }

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```markdown\n# X\n```"), "# X");
        assert_eq!(strip_fences("# X"), "# X");
    }

    #[test]
    fn minutes_formatting() {
        assert_eq!(fmt_minutes(90), "2 min");
        assert_eq!(fmt_minutes(3600), "1 h 0 min");
        assert_eq!(fmt_minutes(5400), "1 h 30 min");
    }
}
