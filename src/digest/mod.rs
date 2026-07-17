pub mod build;
pub mod render;

use crate::config::{self, Config, Paths};
use crate::mail::sendgrid;
use crate::store::db;
use crate::util;
use anyhow::{anyhow, Result};
use chrono::Timelike;
use rusqlite::Connection;
use tracing::{info, warn};

/// Odešle digest uložený v DB pro dané datum. Atomický claim brání dvojímu
/// odeslání při souběhu digest timeru s hodinovým retry. `force` (explicitní
/// `digest --send`) smí přeposlat i už odeslaný digest. Vrací true = odesláno.
pub fn send_stored(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    date: &str,
    force: bool,
) -> Result<bool> {
    let (md, html, _) = db::digest_row(conn, date)?
        .ok_or_else(|| anyhow!("digest pro {date} v DB neexistuje"))?;
    if !db::claim_digest(conn, date, force)? {
        info!("digest {date} přeskočen — právě ho odesílá jiný proces, nebo už je odeslaný");
        return Ok(false);
    }
    let key = config::sendgrid_key(paths)?;
    let subject = format!("{} — {date}", cfg.email.subject_prefix);
    match sendgrid::send(&cfg.email, &key, &subject, &md, &html) {
        Ok(msg_id) => {
            db::mark_digest_sent(conn, date, msg_id.as_deref())?;
            info!("digest {date} odeslán na {}", cfg.email.to);
            Ok(true)
        }
        Err(e) => {
            // claim vrátit, aby digest zůstal k doeslání
            let _ = db::unclaim_digest(conn, date);
            Err(e)
        }
    }
}

/// Doeslání nedoručených digestů — volá se po každé hodinové analýze.
/// Dnešní digest se posílá až po digest hodině, budoucí data nikdy.
pub fn retry_pending(paths: &Paths, cfg: &Config, conn: &Connection) {
    let today = util::today_local().format("%Y-%m-%d").to_string();
    let now_hour = chrono::Local::now().hour() as u8;
    let dates = match db::pending_digest_dates(conn) {
        Ok(d) => d,
        Err(e) => {
            warn!("nelze načíst pending digesty: {e:#}");
            return;
        }
    };
    for date in dates {
        if date > today || (date == today && now_hour < cfg.digest.hour) {
            continue;
        }
        if let Err(e) = send_stored(paths, cfg, conn, &date, false) {
            warn!("doeslání digestu {date} selhalo: {e:#}");
        }
    }
}
