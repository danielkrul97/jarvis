use crate::config::{Config, Paths};
use crate::store::db;
use crate::util;
use anyhow::Result;
use chrono::Timelike;
use std::time::Duration;
use tracing::{error, info, warn};

/// Fallback bez systemd: capture ve vlákně + interní plánovač
/// (hodinová analýza, denní digest) v hlavní smyčce.
pub fn run_all(paths: &Paths, cfg: &Config) -> Result<()> {
    info!("jarvis run: capture + plánovač v jednom procesu");
    // exkluzivita proti systemd službě / druhé instanci; zámek držíme celý běh
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
    loop {
        let now = util::now_ts();

        if now - last_analyze >= 3600 {
            last_analyze = now;
            if let Err(e) = crate::pipeline::analyze::run(paths, cfg, &conn, false, None) {
                warn!("hodinová analýza selhala: {e:#}");
            }
        }

        // runbooky à 5 min: plánované běhy + vyřízení vzdálených schválení
        // (stejná perioda jako jarvis-runbooks.timer v systemd režimu)
        if cfg.runbooks.enabled && now - last_runbooks >= 300 {
            last_runbooks = now;
            crate::runbook::tick(paths, cfg, &conn);
        }

        // digest: po digest hodině, dokud dnešek není odeslán (idempotence přes
        // DB status); neúspěch → další pokus nejdřív za hodinu
        let today = util::today_local().format("%Y-%m-%d").to_string();
        let hour = chrono::Local::now().hour() as u8;
        if hour >= cfg.digest.hour && now - last_digest_attempt >= 3600 {
            let sent = db::digest_row(&conn, &today)
                .ok()
                .flatten()
                .map(|(_, _, status)| status == "sent")
                .unwrap_or(false);
            if !sent {
                last_digest_attempt = now;
                match crate::digest::build::build(paths, cfg, &conn, util::today_local(), true)
                    .and_then(|_| crate::digest::send_stored(paths, cfg, &conn, &today, false))
                {
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
