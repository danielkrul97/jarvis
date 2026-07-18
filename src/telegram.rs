//! Schvalování runbooků na dálku přes Telegram bota (PLAN §9, fáze D).
//!
//! Bot API `getUpdates` bez čekání — polluje ho 5min otočka plánovače
//! (`runbook run-due` / `jarvis run`), takže funguje za NAT bez webhooku.
//! Model důvěry: bot je Danielův vlastní (token v secrets.env) a poslouchá
//! se JEDINÝ chat (`TELEGRAM_CHAT_ID` = soukromý chat pána s botem);
//! zprávy odjinud se ignorují a logují. Schválení = „schval N“, zamítnutí
//! = „zamítni N“. Nové návrhy bot ohlašuje sám.

use crate::config::{self, Config, Paths};
use crate::store::db;
use crate::util;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::time::Duration;
use tracing::{debug, info, warn};

const API: &str = "https://api.telegram.org";

/// Chyba ureq BEZ URL — ta u Telegramu obsahuje `bot<token>` a ureq ji dává
/// do Display u Status i Transport chyb, takže `{e:#}` v logu jinak vypíše
/// token. Bereme jen stavový kód / druh chyby a (u Status) tělo odpovědi,
/// které token neobsahuje.
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

/// Pošle zprávu do chatu; chyby vrací (volající loguje — notifikace jsou
/// best-effort, schvalovací odpovědi chceme vidět v logu).
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

/// „schval 3“ / „zamítni 3“ → (approve?, proposal_id). Toleruje velikost
/// písmen, diakritiku v „zamítni“, lomítkový tvar „/schval_3“ i „schval #3“.
/// Bez čísla návrhu nic — samotné „ano“ nesmí nikdy nic schválit.
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

/// Ohlásí nový návrh do chatu (best-effort; vypnuto/nenakonfigurováno = ticho).
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

/// Vyřídí čekající schvalovací zprávy. Volá se z otočky plánovače; všechny
/// chyby jen loguje (plánovač nesmí spadnout kvůli síti). Offset se posouvá
/// i přes nesrozumitelné zprávy — nic se nevyřizuje dvakrát.
pub fn process_approvals(paths: &Paths, cfg: &Config, conn: &Connection) {
    if !cfg.runbooks.telegram_approve {
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
        let Some((approve, proposal_id)) = parse_command(text) else {
            debug!("telegram: nerozumím zprávě: {}", util::truncate_chars(text, 80));
            continue;
        };
        let reply = if approve {
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
        assert_eq!(parse_command("Schválit 12"), None); // diakritika v kmeni ne — á≠a
        assert_eq!(parse_command("schvalit 12"), Some((true, 12)));
        assert_eq!(parse_command("/schval_7"), Some((true, 7)));
        assert_eq!(parse_command("schval #4"), Some((true, 4)));
        assert_eq!(parse_command("zamítni 5"), Some((false, 5)));
        assert_eq!(parse_command("zamitni 5"), Some((false, 5)));
        assert_eq!(parse_command("Zamítnout 9"), Some((false, 9)));
        // bez čísla, cizí slova, prázdno → nic
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
        // update bez message → prázdné, ale update_id se počítá (offset!)
        assert_eq!(ups[2].update_id, 12);
        assert_eq!(ups[2].chat_id, None);
        assert_eq!(ups[2].text, None);
    }
}
