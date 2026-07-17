use crate::config::{self, Config, Paths};
use crate::store::db;
use crate::util;
use anyhow::{bail, Result};
use rusqlite::OptionalExtension;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn check_line(ok: bool, label: &str, detail: &str) {
    let mark = if ok { "✓" } else { "✗" };
    if detail.is_empty() {
        println!("{mark} {label}");
    } else {
        println!("{mark} {label} — {detail}");
    }
}

pub fn doctor(paths: &Paths, cfg: &Config, live: bool) -> Result<()> {
    let mut problems = 0usize;
    let mut check = |ok: bool, label: &str, detail: String| {
        check_line(ok, label, &detail);
        if !ok {
            problems += 1;
        }
    };

    // config
    if paths.config_file.exists() {
        check(true, "config", format!("{}", paths.config_file.display()));
    } else {
        check(true, "config", "soubor neexistuje, používám defaults".into());
    }

    // DISPLAY + X spojení
    match std::env::var("DISPLAY") {
        Ok(d) if !d.is_empty() => {
            check(true, "DISPLAY", d.clone());
            match crate::capture::x11::X11::connect() {
                Ok(x) => {
                    let (w, h) = x.geometry();
                    check(true, "X11 spojení", format!("root {w}x{h}, idle detekce: {}",
                        if x.has_screensaver() { "ano (MIT-SCREEN-SAVER)" } else { "NE — idle se ignoruje" }));
                }
                Err(e) => check(false, "X11 spojení", format!("{e:#}")),
            }
        }
        _ => check(false, "DISPLAY", "není nastaveno — capture nemůže běžet".into()),
    }

    // claude CLI
    match Command::new("claude").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            check(true, "claude CLI", v);
        }
        Ok(out) => check(false, "claude CLI", format!("exit {:?}", out.status.code())),
        Err(e) => check(false, "claude CLI", format!("nenalezen: {e}")),
    }

    // SendGrid klíč + práva souboru
    match config::sendgrid_key(paths) {
        Ok(_) => {
            let perms_ok = std::fs::metadata(&paths.secrets_file)
                .map(|m| m.permissions().mode() & 0o077 == 0)
                .unwrap_or(true); // klíč může být jen v env
            check(
                perms_ok,
                "SendGrid klíč",
                if perms_ok {
                    "nalezen".into()
                } else {
                    format!("{} je čitelný pro ostatní — chmod 600", paths.secrets_file.display())
                },
            );
        }
        Err(e) => check(false, "SendGrid klíč", format!("{e:#}")),
    }

    // DB
    match db::open(&paths.db_path) {
        Ok(conn) => {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0))?;
            check(true, "databáze", format!("{} ({n} vzorků)", paths.db_path.display()));
        }
        Err(e) => check(false, "databáze", format!("{e:#}")),
    }

    // disk
    let (bytes, files) = util::dir_size(&paths.shots_dir);
    check(true, "screenshoty", format!("{files} souborů, {}", util::human_bytes(bytes)));

    if live {
        match config::sendgrid_key(paths) {
            Ok(key) => match crate::mail::sendgrid::sandbox_check(&cfg.email, &key) {
                Ok(()) => check(true, "SendGrid (sandbox send)", "klíč i odesílatel OK".into()),
                Err(e) => check(false, "SendGrid (sandbox send)", format!("{e:#}")),
            },
            Err(_) => check(false, "SendGrid (sandbox send)", "bez klíče nelze ověřit".into()),
        }
        match crate::pipeline::claude::ping(&cfg.analysis.model) {
            Ok(v) => check(true, "claude -p (ping)", v),
            Err(e) => check(false, "claude -p (ping)", format!("{e:#}")),
        }
    }

    if problems > 0 {
        bail!("doctor našel {problems} problém(ů)");
    }
    println!("Vše v pořádku.");
    Ok(())
}

pub fn status(paths: &Paths, cfg: &Config) -> Result<()> {
    let conn = db::open(&paths.db_path)?;
    let now = util::now_ts();
    let (day_start, _) = util::day_bounds_local(util::today_local())?;

    println!("Jarvis status — {}", util::fmt_local(now));

    match db::pause_until(&conn, now)? {
        Some(t) => println!("  snímání:        POZASTAVENO do {}", util::fmt_local(t)),
        None => println!("  snímání:        aktivní (pokud běží démon)"),
    }

    let last: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT ts, wm_class, title FROM samples ORDER BY ts DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match last {
        Some((ts, class, title)) => {
            let age = now - ts;
            println!("  poslední vzorek: {} ({age} s zpět) — {class}: {title}", util::fmt_local(ts));
            if age > 120 {
                println!("                  ⚠ démon zřejmě neběží (vzorek starší 2 min)");
            }
        }
        None => println!("  poslední vzorek: žádný — capture ještě neběžel"),
    }

    let samples_today: i64 = conn.query_row(
        "SELECT COUNT(*) FROM samples WHERE ts >= ?1",
        [day_start],
        |r| r.get(0),
    )?;
    let shots_today: i64 = conn.query_row(
        "SELECT COUNT(*) FROM samples WHERE ts >= ?1 AND shot_path IS NOT NULL",
        [day_start],
        |r| r.get(0),
    )?;
    println!("  dnes:           {samples_today} vzorků, {shots_today} screenshotů");

    let last_summary: Option<i64> = conn
        .query_row(
            "SELECT period_end FROM hourly_summaries ORDER BY period_end DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match last_summary {
        Some(ts) => println!("  analýza:        naposledy do {}", util::fmt_local(ts)),
        None => println!("  analýza:        zatím žádná"),
    }

    let today = util::today_local().format("%Y-%m-%d").to_string();
    let digest: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT status, sent_at FROM daily_digests WHERE date = ?1",
            [&today],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match digest {
        Some((status, Some(ts))) => println!("  digest dnes:    {status} ({})", util::fmt_local(ts)),
        Some((status, None)) => println!("  digest dnes:    {status}"),
        None => println!("  digest dnes:    zatím nevytvořen (plán: {}:00)", cfg.digest.hour),
    }

    let cost_today = db::cost_since(&conn, day_start)?;
    println!("  dnešní útrata:  {cost_today:.4} USD (strop {:.2} USD)", cfg.analysis.daily_budget_usd);

    let (bytes, files) = util::dir_size(&paths.shots_dir);
    println!("  úložiště:       {files} snímků, {}", util::human_bytes(bytes));
    Ok(())
}
