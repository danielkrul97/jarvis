use crate::config::{Config, Paths};
use crate::patterns;
use crate::pipeline::claude::{self, ClaudeOutcome, ClaudeRequest};
use crate::pipeline::{segment, select};
use crate::store::db;
use crate::util;
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};

/// JSON contract for the hourly summary (stored in hourly_summaries.json).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HourlyJson {
    pub activities: Vec<Activity>,
    pub projects: Vec<String>,
    pub notable: Vec<String>,
    pub automation_hints: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Activity {
    pub start: String,
    pub end: String,
    pub project: String,
    pub what: String,
    pub app: String,
}

/// Processes the window from the watermark (or the last N hours) in hourly chunks.
pub fn run(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    dry_run: bool,
    window_hours: Option<u64>,
) -> Result<()> {
    // -5 s: the capture tick takes ts before the insert — a window ending exactly at `now`
    // could permanently skip a sample committed moments later
    let now = util::now_ts() - 5;
    let (start, clamp_catchup) = match window_hours {
        Some(h) => (now - (h * 3600) as i64, false),
        None => (db::state_get_i64(conn, "analyze_watermark")?.unwrap_or(now - 3600), true),
    };
    // the catch-up cap only applies to the watermark (a daemon off for a week
    // must not flood us with hourly chunks); explicit --window-hours is a
    // deliberate user choice, so we honor it in full instead of silently clamping it
    let mut chunk_start = if clamp_catchup { start.max(now - 48 * 3600) } else { start };
    if chunk_start >= now {
        info!("analyze: žádné nové období ke zpracování");
        return Ok(());
    }
    while chunk_start < now {
        let chunk_end = (chunk_start + 3600).min(now);
        process_chunk(paths, cfg, conn, chunk_start, chunk_end, dry_run)?;
        if !dry_run {
            // the watermark only moves forward — a rerun via --window-hours
            // must not put an already-processed period back in the queue
            let current = db::state_get_i64(conn, "analyze_watermark")?.unwrap_or(0);
            if chunk_end > current {
                db::state_set(conn, "analyze_watermark", &chunk_end.to_string())?;
            }
        }
        chunk_start = chunk_end;
    }
    if !dry_run {
        // hourly delivery safety net: undelivered digests (SendGrid outage etc.)
        crate::digest::retry_pending(paths, cfg, conn);
    }
    Ok(())
}

fn process_chunk(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    from: i64,
    to: i64,
    dry_run: bool,
) -> Result<()> {
    let samples = db::samples_between(conn, from, to)?;
    let idle_ms = (cfg.capture.idle_threshold_s * 1000) as i64;
    let segs = segment::segment(&samples, cfg.capture.meta_interval_s as i64, idle_ms);
    if segs.is_empty() {
        info!("okno {} – {}: žádná aktivita", util::fmt_hm(from), util::fmt_hm(to));
        return Ok(());
    }

    let timeline = segment::render_timeline(&segs, 25);
    let (day_start, _) = util::day_bounds_local(util::today_local())?;
    let spent = db::cost_since(conn, day_start)?;
    let budget_ok = spent < cfg.analysis.daily_budget_usd;
    let frames = if cfg.analysis.send_images && budget_ok {
        select::select_frames(&segs, &paths.data_dir, cfg.analysis.max_images_per_run)
    } else {
        Vec::new()
    };
    let prompt = build_prompt(&timeline, &frames, from, to);

    if dry_run {
        println!("─── okno {} – {} ───", util::fmt_local(from), util::fmt_local(to));
        println!("{timeline}");
        println!("vybrané snímky ({}):", frames.len());
        for f in &frames {
            println!("  {f}");
        }
        println!(
            "prompt {} znaků; dnešní útrata {spent:.4}/{:.2} USD{}",
            prompt.chars().count(),
            cfg.analysis.daily_budget_usd,
            if budget_ok { "" } else { " — POZOR: rozpočet vyčerpán, běželo by lokálně" }
        );
        return Ok(());
    }

    if !budget_ok {
        warn!(
            "denní rozpočet vyčerpán ({spent:.2}/{:.2} USD) — ukládám lokální souhrn bez Claude",
            cfg.analysis.daily_budget_usd
        );
        let json = degraded_summary(&segs);
        store_summary(conn, from, to, &json, "", 0.0, true)?;
        return Ok(());
    }

    match call_claude_hourly(cfg, paths, conn, &prompt) {
        Ok((json, outcome)) => {
            patterns::record_hints(conn, &json.automation_hints)?;
            store_summary(conn, from, to, &json, &cfg.analysis.model, outcome.cost_usd, false)?;
            info!(
                "souhrn {} – {}: {} aktivit, {} hintů, {:.4} USD",
                util::fmt_hm(from),
                util::fmt_hm(to),
                json.activities.len(),
                json.automation_hints.len(),
                outcome.cost_usd
            );
        }
        Err(e) => {
            warn!("claude analýza selhala: {e:#} — ukládám lokální souhrn z titulků");
            let json = degraded_summary(&segs);
            store_summary(conn, from, to, &json, "", 0.0, true)?;
        }
    }
    Ok(())
}

/// Two rounds: claude call + parse; invalid JSON or a CLI error → one retry.
/// Cost is billed per attempt (even a failed parse still cost tokens).
fn call_claude_hourly(
    cfg: &Config,
    paths: &Paths,
    conn: &Connection,
    prompt: &str,
) -> Result<(HourlyJson, ClaudeOutcome)> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=2 {
        let outcome = match claude::run(&ClaudeRequest {
            prompt: prompt.to_string(),
            model: Some(&cfg.analysis.model),
            cwd: &paths.data_dir,
            allowed_tools: "Read",
            max_turns: 16,
            timeout: Duration::from_secs(cfg.analysis.timeout_s),
        }) {
            Ok(o) => o,
            Err(e) => {
                warn!("pokus {attempt}/2: claude CLI selhal: {e:#}");
                last_err = Some(e);
                continue;
            }
        };
        db::insert_cost(
            conn,
            util::now_ts(),
            "analyze",
            &cfg.analysis.model,
            outcome.tokens_in,
            outcome.tokens_out,
            outcome.cost_usd,
        )?;
        match claude::extract_json(&outcome.text)
            .and_then(|s| serde_json::from_str::<HourlyJson>(s).map_err(Into::into))
        {
            Ok(json) => return Ok((json, outcome)),
            Err(e) => {
                warn!("pokus {attempt}/2: odpověď není validní JSON kontrakt: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("claude analýza selhala")))
}

fn build_prompt(timeline: &str, frames: &[String], from: i64, to: i64) -> String {
    let mut p = format!(
        "Jsi Jarvis, můj osobní pracovní asistent. Analyzuješ, na čem jsem právě \
         pracoval na svém počítači.\n\n\
         Období: {} – {} (lokální čas).\n\n\
         Časová osa aktivních oken (z titulků, deterministická):\n{timeline}\n",
        util::fmt_local(from),
        util::fmt_local(to)
    );
    if !frames.is_empty() {
        p.push_str("\nScreenshoty z tohoto období — přečti KAŽDÝ nástrojem Read (cesty jsou relativní k pracovnímu adresáři):\n");
        for f in frames {
            p.push_str(&format!("- {f}\n"));
        }
    }
    p.push_str(
        "\nÚkol: popiš, na čem jsem pracoval. Odpověz POUZE validním JSON objektem \
         (žádný další text, žádné ``` ploty) přesně v tomto tvaru:\n\
         {\n\
         \x20 \"activities\": [{\"start\": \"HH:MM\", \"end\": \"HH:MM\", \"project\": \"název projektu\", \"what\": \"co konkrétně se dělo\", \"app\": \"aplikace\"}],\n\
         \x20 \"projects\": [\"seznam projektů, na kterých se pracovalo\"],\n\
         \x20 \"notable\": [\"nedokončené věci, chyby na obrazovce, pozoruhodnosti\"],\n\
         \x20 \"automation_hints\": [\"konkrétní opakované ruční činnosti, které by šly zautomatizovat (jen pokud je reálně vidíš)\"]\n\
         }\n\
         Piš česky, stručně a konkrétně — používej názvy souborů, projektů a webů, \
         které vidíš na screenshotech a v titulcích.",
    );
    p
}

/// Fallback without Claude: activities deterministically taken from the longest segments.
pub fn degraded_summary(segs: &[segment::Segment]) -> HourlyJson {
    let mut ordered: Vec<&segment::Segment> = segs.iter().collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.duration_s()));
    let activities: Vec<Activity> = ordered
        .iter()
        .take(10)
        .map(|s| Activity {
            start: util::fmt_hm(s.start),
            end: util::fmt_hm(s.end),
            project: String::new(),
            what: util::truncate_chars(&s.title, 120),
            app: s.wm_class.clone(),
        })
        .collect();
    HourlyJson { activities, ..Default::default() }
}

fn store_summary(
    conn: &Connection,
    from: i64,
    to: i64,
    json: &HourlyJson,
    model: &str,
    cost: f64,
    degraded: bool,
) -> Result<()> {
    let text = serde_json::to_string(json)?;
    db::insert_hourly_summary(conn, from, to, &text, model, cost, degraded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db::Sample;

    fn sample(ts: i64, class: &str, title: &str) -> Sample {
        Sample {
            ts,
            wm_class: class.into(),
            title: title.into(),
            idle_ms: 0,
            shot_path: None,
        }
    }

    #[test]
    fn degraded_summary_from_segments() {
        let samples: Vec<Sample> = (0..12).map(|i| sample(i * 10, "vim", "main.rs")).collect();
        let segs = segment::segment(&samples, 10, 120_000);
        let json = degraded_summary(&segs);
        assert_eq!(json.activities.len(), 1);
        assert_eq!(json.activities[0].app, "vim");
        assert!(json.automation_hints.is_empty());
    }

    #[test]
    fn hourly_json_tolerates_missing_fields() {
        let json: HourlyJson = serde_json::from_str(r#"{"projects":["x"]}"#).unwrap();
        assert_eq!(json.projects, vec!["x"]);
        assert!(json.activities.is_empty());
    }

    #[test]
    fn prompt_contains_contract_and_frames() {
        let p = build_prompt("timeline", &["shots/a.jpg".into()], 0, 3600);
        assert!(p.contains("automation_hints"));
        assert!(p.contains("shots/a.jpg"));
        assert!(p.contains("Read"));
    }
}
