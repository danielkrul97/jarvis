//! Remote runbook approval via Telegram bot (PLAN §9, phase D).
//!
//! Bot API `getUpdates` without long-polling — polled by the scheduler's
//! 5-min tick (`runbook run-due` / `jarvis run`), so it works behind NAT
//! without a webhook. Trust model: the bot is Daniel's own (token in
//! secrets.env) and listens to a SINGLE chat (`TELEGRAM_CHAT_ID` = the
//! owner's private chat with the bot); messages from elsewhere are ignored
//! and logged. Approve = "schval N", reject = "zamítni N". The bot
//! announces new proposals itself.

use crate::config::{self, Config, Paths};
use crate::store::db;
use crate::util;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, info, warn};

const API: &str = "https://api.telegram.org";

/// ureq error WITHOUT the URL — Telegram's URL contains `bot<token>`, and
/// ureq puts it in the Display impl for both Status and Transport errors,
/// so `{e:#}` in a log would otherwise leak the token. We keep only the
/// status code/error kind and (for Status) the response body, which
/// doesn't contain the token.
fn ureq_err(what: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            anyhow::anyhow!("Telegram {what}: HTTP {code} {}", util::truncate_chars(&body, 200))
        }
        ureq::Error::Transport(t) => {
            anyhow::anyhow!("Telegram {what}: spojení selhalo ({:?})", t.kind())
        }
    }
}

/// Sends a message to the chat; returns errors (the caller logs them —
/// notifications are best-effort, but we want approval replies visible in
/// the log).
pub fn send_message(token: &str, chat_id: &str, text: &str) -> Result<()> {
    let resp = ureq::post(&format!("{API}/bot{token}/sendMessage"))
        .timeout(Duration::from_secs(15))
        .send_form(&[("chat_id", chat_id), ("text", text)])
        .map_err(|e| ureq_err("sendMessage", e))?;
    let v: Value = resp.into_json().context("nečitelná odpověď sendMessage")?;
    anyhow::ensure!(
        v["ok"].as_bool().unwrap_or(false),
        "Telegram odmítl zprávu: {}",
        util::truncate_chars(&v.to_string(), 200)
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Update {
    pub update_id: i64,
    pub chat_id: Option<i64>,
    pub text: Option<String>,
}

fn fetch_updates(token: &str, offset: i64) -> Result<Vec<Update>> {
    let resp = ureq::get(&format!("{API}/bot{token}/getUpdates"))
        .query("offset", &offset.to_string())
        .query("timeout", "0")
        .query("allowed_updates", r#"["message"]"#)
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| ureq_err("getUpdates", e))?;
    let v: Value = resp.into_json().context("nečitelná odpověď getUpdates")?;
    anyhow::ensure!(
        v["ok"].as_bool().unwrap_or(false),
        "getUpdates vrátil chybu: {}",
        util::truncate_chars(&v.to_string(), 200)
    );
    Ok(parse_updates(&v))
}

pub(crate) fn parse_updates(v: &Value) -> Vec<Update> {
    v["result"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|u| {
                    Some(Update {
                        update_id: u["update_id"].as_i64()?,
                        chat_id: u["message"]["chat"]["id"].as_i64(),
                        text: u["message"]["text"].as_str().map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// "schval 3" / "zamítni 3" → (approve?, proposal_id). Tolerates case,
/// diacritics in "zamítni", the slash form "/schval_3", and "schval #3".
/// Without a proposal number, nothing — a bare "ano" must never approve
/// anything.
pub(crate) fn parse_command(text: &str) -> Option<(bool, i64)> {
    let t = text.to_lowercase().replace('í', "i");
    let mut tokens = t.split(|c: char| !c.is_ascii_alphanumeric()).filter(|s| !s.is_empty());
    let approve = match tokens.next()? {
        "schval" | "schvalit" => true,
        "zamitni" | "zamitnout" => false,
        _ => return None,
    };
    Some((approve, tokens.next()?.parse().ok()?))
}

/// Announces a new proposal to the chat (best-effort; off/unconfigured = silent).
pub fn notify_proposal(paths: &Paths, cfg: &Config, proposal_id: i64, kind: &str, desc: &str) {
    if !cfg.runbooks.telegram_approve {
        return;
    }
    let Ok((token, chat_id)) = config::telegram_keys(paths) else {
        debug!("telegram: klíče nejsou v secrets.env — návrh #{proposal_id} neohlašuji");
        return;
    };
    let text = format!(
        "Jarvis: nový návrh automatizace #{proposal_id} [{kind}]\n{}\n\n\
         Schválit: odpověz „schval {proposal_id}“\n\
         Zamítnout: „zamítni {proposal_id}“\n\
         Detail na stroji: jarvis runbook pending",
        util::truncate_chars(desc, 300)
    );
    if let Err(e) = send_message(&token, &chat_id, &text) {
        warn!("telegram: ohláška návrhu #{proposal_id} selhala: {e:#}");
    }
}

/// Processes pending approval messages. Called from the scheduler's tick;
/// all errors are just logged (the scheduler must not crash over a network
/// issue). The offset advances even past unintelligible messages — nothing
/// gets processed twice.
pub fn process_approvals(paths: &Paths, cfg: &Config, conn: &Connection) {
    // One getUpdates stream carries two command families: runbook approvals
    // ("schval N"/"zamítni N") and proactive confirmations ("ano N"/"ne N").
    // Polling only runs when at least one of them has remote work to do.
    let nudge_confirm = cfg.proactive.enabled && cfg.proactive.telegram_confirm;
    if !cfg.runbooks.telegram_approve && !nudge_confirm {
        return;
    }
    let Ok((token, chat_id)) = config::telegram_keys(paths) else {
        debug!("telegram: klíče nejsou v secrets.env — schvalování na dálku spí");
        return;
    };
    let offset = match db::state_get_i64(conn, "telegram_offset") {
        Ok(v) => v.unwrap_or(0),
        Err(e) => {
            warn!("telegram: nelze číst offset: {e:#}");
            return;
        }
    };
    let updates = match fetch_updates(&token, offset) {
        Ok(u) => u,
        Err(e) => {
            warn!("telegram: {e:#}");
            return;
        }
    };
    let mut new_offset = offset;
    for u in &updates {
        new_offset = new_offset.max(u.update_id + 1);
        let Some(text) = u.text.as_deref() else { continue };
        if u.chat_id.map(|c| c.to_string()) != Some(chat_id.clone()) {
            warn!(
                "telegram: zpráva z neověřeného chatu {:?} — ignoruji: {}",
                u.chat_id,
                util::truncate_chars(text, 80)
            );
            continue;
        }
        let reply = if let Some((approve, proposal_id)) = parse_command(text) {
            if !cfg.runbooks.telegram_approve {
                debug!("telegram: schvalování runbooků vypnuté — ignoruji: {}", util::truncate_chars(text, 80));
                continue;
            }
            if approve {
                match crate::runbook::approve(conn, proposal_id, "manual", None, "telegram") {
                    Ok(rb) => {
                        info!("telegram: návrh #{proposal_id} schválen → runbook #{}", rb.id);
                        format!(
                            "✓ Runbook #{} „{}“ schválen (plán manual — spustí se \
                             ručně nebo hlasem; denní plán: jarvis runbook schedule \
                             {} daily@HH:MM)",
                            rb.id, rb.name, rb.id
                        )
                    }
                    Err(e) => format!("✗ Schválení #{proposal_id} selhalo: {e:#}"),
                }
            } else {
                match crate::runbook::dismiss(conn, proposal_id) {
                    Ok(_) => {
                        info!("telegram: návrh #{proposal_id} zamítnut");
                        format!("✓ Návrh #{proposal_id} zamítnut.")
                    }
                    Err(e) => format!("✗ Zamítnutí #{proposal_id} selhalo: {e:#}"),
                }
            }
        } else if let Some((yes, nudge_id)) = crate::nudge::parse_confirm(text) {
            if !nudge_confirm {
                debug!("telegram: proaktivní potvrzení vypnuté — ignoruji: {}", util::truncate_chars(text, 80));
                continue;
            }
            info!("telegram: proaktivní potvrzení #{nudge_id} = {}", if yes { "ano" } else { "ne" });
            crate::nudge::confirm_remote(paths, cfg, conn, nudge_id, yes)
        } else {
            debug!("telegram: nerozumím zprávě: {}", util::truncate_chars(text, 80));
            continue;
        };
        if let Err(e) = send_message(&token, &chat_id, &reply) {
            warn!("telegram: odpověď se neodeslala: {e:#}");
        }
    }
    if new_offset != offset {
        if let Err(e) = db::state_set(conn, "telegram_offset", &new_offset.to_string()) {
            warn!("telegram: nelze uložit offset: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parsing_accepts_human_variants() {
        assert_eq!(parse_command("schval 3"), Some((true, 3)));
        assert_eq!(parse_command("SCHVAL 3"), Some((true, 3)));
        assert_eq!(parse_command("Schválit 12"), None); // diacritics in the stem don't match — á≠a
        assert_eq!(parse_command("schvalit 12"), Some((true, 12)));
        assert_eq!(parse_command("/schval_7"), Some((true, 7)));
        assert_eq!(parse_command("schval #4"), Some((true, 4)));
        assert_eq!(parse_command("zamítni 5"), Some((false, 5)));
        assert_eq!(parse_command("zamitni 5"), Some((false, 5)));
        assert_eq!(parse_command("Zamítnout 9"), Some((false, 9)));
        // no number, foreign words, empty → nothing
        assert_eq!(parse_command("schval"), None);
        assert_eq!(parse_command("ano"), None);
        assert_eq!(parse_command("ahoj jak je"), None);
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("schval abc"), None);
    }

    #[test]
    fn updates_parse_from_bot_api_shape() {
        let v: Value = serde_json::from_str(
            r#"{"ok":true,"result":[
                {"update_id":10,"message":{"chat":{"id":42},"text":"schval 1"}},
                {"update_id":11,"message":{"chat":{"id":99},"text":"schval 2"}},
                {"update_id":12,"edited_message":{"chat":{"id":42}}}
            ]}"#,
        )
        .unwrap();
        let ups = parse_updates(&v);
        assert_eq!(ups.len(), 3);
        assert_eq!(ups[0].update_id, 10);
        assert_eq!(ups[0].chat_id, Some(42));
        assert_eq!(ups[0].text.as_deref(), Some("schval 1"));
        assert_eq!(ups[1].chat_id, Some(99));
        // update without a message → empty, but update_id still counts (offset!)
        assert_eq!(ups[2].update_id, 12);
        assert_eq!(ups[2].chat_id, None);
        assert_eq!(ups[2].text, None);
    }
}
