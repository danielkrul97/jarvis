use crate::config::{Config, Paths};
use crate::store::db;
use crate::util;
use anyhow::Result;
use chrono::Timelike;
use std::time::Duration;
use tracing::{error, info, warn};

/// Fallback without systemd: capture in a thread + internal scheduler
/// (hourly analysis, daily digest) in the main loop.
pub fn run_all(paths: &Paths, cfg: &Config) -> Result<()> {
    info!("jarvis run: capture + plánovač v jednom procesu");
    // exclusivity against the systemd service / a second instance; hold the lock for the whole run
    let _lock = crate::capture::acquire_lock(paths)?;
    {
        let paths = paths.clone();
        let cfg = cfg.clone();
        std::thread::spawn(move || loop {
            if let Err(e) = crate::capture::run_capture(&paths, &cfg) {
                error!("capture spadl: {e:#} — restart za 10 s");
                std::thread::sleep(Duration::from_secs(10));
            }
        });
    }

    if cfg.listen.enabled {
        let model = cfg.listen.resolve_model_path(paths);
        if model.exists() {
            let paths = paths.clone();
            let cfg = cfg.clone();
            std::thread::spawn(move || loop {
                if let Err(e) = crate::listen::run_listen(&paths, &cfg, false) {
                    error!("poslech spadl: {e:#} — restart za 10 s");
                    std::thread::sleep(Duration::from_secs(10));
                }
            });
        } else {
            warn!(
                "poslech se nespustí: model {} chybí — `jarvis listen --download-model`",
                model.display()
            );
        }
    }

    let conn = db::open(&paths.db_path)?;
    let mut last_analyze: i64 = 0;
    let mut last_digest_attempt: i64 = 0;
    let mut last_runbooks: i64 = 0;
    let mut last_tasks: i64 = 0;
    let mut last_nudge: i64 = 0;
    loop {
        let now = util::now_ts();

        if now - last_analyze >= 3600 {
            last_analyze = now;
            if let Err(e) = crate::pipeline::analyze::run(paths, cfg, &conn, false, None) {
                warn!("hodinová analýza selhala: {e:#}");
            }
        }

        // memory consolidation: once a day after memory.consolidate_hour (quiet time).
        // A DB marker holds idempotency; the watermark inside run() handles the data window.
        if cfg.memory.enabled && cfg.memory.consolidate {
            let hour = chrono::Local::now().hour() as u8;
            let today = util::today_local().format("%Y-%m-%d").to_string();
            let done = db::state_get(&conn, "memory_consolidate_date")
                .ok()
                .flatten()
                .is_some_and(|d| d == today);
            if hour >= cfg.memory.consolidate_hour && !done {
                match crate::memory::consolidate::run(paths, cfg, &conn, None, false) {
                    Ok(n) => {
                        info!("noční konsolidace paměti: {n} faktů");
                        let _ = db::state_set(&conn, "memory_consolidate_date", &today);
                    }
                    Err(e) => warn!("konsolidace paměti selhala: {e:#}"),
                }
            }
        }

        // runbooks every 5 min: scheduled runs + handling remote approvals
        // (same period as jarvis-runbooks.timer in systemd mode)
        if cfg.runbooks.enabled && now - last_runbooks >= 300 {
            last_runbooks = now;
            crate::runbook::tick(paths, cfg, &conn);
        }

        // scheduled internal tasks every 5 min: dependency self-management and maintenance
        // (same period as jarvis-tasks.timer in systemd mode). Its own due
        // schedule is inside (deps every 24h, cleanup/maintenance daily) — this is just
        // the scheduler tick, not the interval of the tasks themselves.
        if cfg.tasks.enabled && now - last_tasks >= 300 {
            last_tasks = now;
            crate::tasks::tick(paths, cfg, &conn);
        }

        // proactive layer: detection → nudge (+ remote confirmations "ano N").
        // Its own cadence (tick_s), separate from runbooks. A disabled layer is a no-op.
        if cfg.proactive.enabled && now - last_nudge >= cfg.proactive.tick_s as i64 {
            last_nudge = now;
            crate::nudge::tick(paths, cfg, &conn);
        }

        // digest: after the digest hour, until today's is sent (idempotency via
        // DB status); on failure, the next attempt is no sooner than an hour later
        let today = util::today_local().format("%Y-%m-%d").to_string();
        let hour = chrono::Local::now().hour() as u8;
        if hour >= cfg.digest.hour && now - last_digest_attempt >= 3600 {
            let row = db::digest_row(&conn, &today).ok().flatten();
            let sent = row.as_ref().map(|(_, _, status)| status == "sent").unwrap_or(false);
            if !sent {
                last_digest_attempt = now;
                // digest already rendered in the DB (earlier attempt) → just resend it.
                // build() calls Claude (cost + time) and synchronously blocks this
                // loop (and thus runbooks/nudge/analysis) for up to analysis.timeout_s;
                // must not re-run every hour during a SendGrid outage.
                let res = if row.is_some() {
                    crate::digest::send_stored(paths, cfg, &conn, &today, false)
                } else {
                    crate::digest::build::build(paths, cfg, &conn, util::today_local(), true)
                        .and_then(|_| crate::digest::send_stored(paths, cfg, &conn, &today, false))
                };
                match res {
                    Ok(true) => {
                        info!("denní digest {today} odeslán");
                        crate::speak::announce(paths, cfg, crate::speak::DIGEST_ANNOUNCEMENT);
                    }
                    Ok(false) => info!("denní digest {today} odeslal souběžný proces"),
                    Err(e) => warn!("digest {today} selhal: {e:#} — zkusím za hodinu"),
                }
            }
        }

        std::thread::sleep(Duration::from_secs(60));
    }
}
