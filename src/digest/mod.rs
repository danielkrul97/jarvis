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

/// Sends the digest stored in the DB for the given date. An atomic claim
/// prevents double-send when the digest timer races the hourly retry.
/// `force` (explicit `digest --send`) may resend an already-sent digest.
/// Returns true = sent.
pub fn send_stored(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    date: &str,
    force: bool,
) -> Result<bool> {
    let (md, html, prev_status) = db::digest_row(conn, date)?
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
            // revert the claim to the state BEFORE the claim: normal send →
            // pending (stays queued for redelivery), forced resend of an
            // already-sent digest → back to sent (a failed resend must not
            // resurrect the digest into the queue and send it twice)
            let _ = db::unclaim_digest(conn, date, &prev_status);
            Err(e)
        }
    }
}

/// Redelivers undelivered digests — called after every hourly analysis.
/// Today's digest is sent only after the digest hour; future dates never.
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
