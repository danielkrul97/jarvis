//! SMS via the Twilio Messages API. The sender is either a Messaging
//! Service (`MG…` SID — Jarvis's case, alphanumeric sender in the
//! service's pool), an E.164 number, or an alphanumeric sender directly.
//! Auth = Basic (account SID + token from secrets.env), request format is
//! form-urlencoded, response is JSON.
//!
//! Delivery is verified via read-back: after sending, message status is
//! polled (queued → sending → sent → delivered); `failed`/`undelivered` is
//! an error with the Twilio message. An alphanumeric sender is one-way —
//! SMS replies aren't possible.

use crate::config::SmsCfg;
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::time::{Duration, Instant};
use tracing::warn;

const API_BASE: &str = "https://api.twilio.com/2010-04-01/Accounts";

enum SendError {
    Retryable(String),
    Fatal(String),
}

/// Sends an SMS; retries with backoff on 429/5xx/transport errors. Returns the message SID.
pub fn send(cfg: &SmsCfg, account_sid: &str, token: &str, to: &str, body: &str) -> Result<String> {
    if !is_e164(to) {
        bail!("příjemce musí být v E.164 formátu (+420123456789), je „{to}“");
    }
    let chars = body.chars().count();
    if chars == 0 {
        bail!("prázdný text SMS");
    }
    if chars > cfg.max_chars {
        bail!("text má {chars} znaků, strop sms.max_chars je {}", cfg.max_chars);
    }
    let url = format!("{API_BASE}/{account_sid}/Messages.json");
    let auth = basic_auth(account_sid, token);
    let params = form_params(&cfg.from, to, body);
    let delays_s: [u64; 3] = [0, 2, 8];
    let mut last_err = String::new();
    for (i, delay) in delays_s.iter().enumerate() {
        if *delay > 0 {
            std::thread::sleep(Duration::from_secs(*delay));
        }
        match try_send(&url, &auth, &params) {
            Ok(sid) => return Ok(sid),
            Err(SendError::Fatal(e)) => return Err(anyhow!("Twilio: {e}")),
            Err(SendError::Retryable(e)) => {
                warn!("Twilio pokus {}/{}: {e}", i + 1, delays_s.len());
                last_err = e;
            }
        }
    }
    Err(anyhow!("Twilio: vyčerpány pokusy, poslední chyba: {last_err}"))
}

fn try_send(url: &str, auth: &str, params: &[(&str, &str)]) -> Result<String, SendError> {
    let resp = ureq::post(url)
        .set("Authorization", auth)
        .timeout(Duration::from_secs(30))
        .send_form(params);
    match resp {
        Ok(r) => {
            let v: Value = r
                .into_json()
                .map_err(|e| SendError::Fatal(format!("nečitelná odpověď: {e}")))?;
            v["sid"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| SendError::Fatal(format!("odpověď bez SID: {v}")))
        }
        Err(ureq::Error::Status(code, r)) => {
            let body_text = r.into_string().unwrap_or_default();
            let (tw_code, tw_msg) = parse_twilio_error(&body_text);
            let hint = match tw_code {
                20003 => " (neplatné TWILIO_ACCOUNT_SID/TWILIO_AUTH_TOKEN?)",
                21211 => " (neplatné číslo příjemce)",
                21608 => " (trial účet smí posílat jen na ověřená čísla)",
                21606 | 21659 => " (odesílatel/From není schopný SMS pro tuto destinaci)",
                // 21612/21703 also occur in practice when SMS Geographic
                // Permissions are disabled for the destination country
                // (console → Messaging → Settings → Geo permissions) —
                // the API can't configure this
                21612 => " (kombinace To/From nejde — zkontroluj Geo permissions pro zemi příjemce)",
                21703 => " (Messaging Service nemá pro destinaci žádného použitelného odesílatele — pool, nebo Geo permissions)",
                63038 => " (vyčerpán denní limit zpráv účtu)",
                _ => "",
            };
            let msg = format!(
                "HTTP {code}, Twilio {tw_code}{hint}: {}",
                util::truncate_chars(&tw_msg, 300)
            );
            if code == 429 || code >= 500 {
                Err(SendError::Retryable(msg))
            } else {
                Err(SendError::Fatal(msg))
            }
        }
        Err(e) => Err(SendError::Retryable(format!("transport: {e}"))),
    }
}

/// Raw status query — only network/parse, does NOT interpret
/// failed/undelivered (the caller handles that). Returns (status, price,
/// error_code, error_message).
fn fetch_status(
    account_sid: &str,
    token: &str,
    msg_sid: &str,
) -> Result<(String, Option<f64>, i64, String)> {
    let url = format!("{API_BASE}/{account_sid}/Messages/{msg_sid}.json");
    let v: Value = ureq::get(&url)
        .set("Authorization", &basic_auth(account_sid, token))
        .timeout(Duration::from_secs(15))
        .call()
        .context("dotaz na stav zprávy selhal")?
        .into_json()
        .context("nečitelná odpověď na stav zprávy")?;
    let status = v["status"].as_str().unwrap_or("unknown").to_string();
    let code = v["error_code"].as_i64().unwrap_or(0);
    let emsg = v["error_message"].as_str().unwrap_or("bez detailu").to_string();
    // price comes back negative ("-0.0831") and delayed
    let price = v["price"].as_str().and_then(|p| p.parse::<f64>().ok()).map(f64::abs);
    Ok((status, price, code, emsg))
}

/// Error for a definitively undelivered message (failed/undelivered), with a hint.
fn delivery_error(status: &str, code: i64, emsg: &str) -> anyhow::Error {
    let hint = match code {
        21612 | 21703 => {
            " — nejčastější příčina: vypnuté SMS Geographic Permissions pro zemi \
             příjemce (Twilio konzole → Messaging → Settings → Geo permissions)"
        }
        30003..=30008 => " — problém na straně operátora/telefonu příjemce",
        _ => "",
    };
    anyhow!("zpráva {status}: Twilio {code} — {emsg}{hint}")
}

/// Polls status until `delivered` or timeout (then returns the last status
/// reached — `sent` is a normal end state for alphanumeric senders).
/// `failed`/`undelivered` = Err.
pub fn wait_final(
    account_sid: &str,
    token: &str,
    msg_sid: &str,
    timeout: Duration,
) -> Result<(String, Option<f64>)> {
    let deadline = Instant::now() + timeout;
    let mut last_ok: Option<(String, Option<f64>)> = None;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        match fetch_status(account_sid, token, msg_sid) {
            Ok((status, price, code, emsg)) => {
                if status == "failed" || status == "undelivered" {
                    return Err(delivery_error(&status, code, &emsg));
                }
                last_ok = Some((status.clone(), price));
                if status == "delivered" || Instant::now() >= deadline {
                    return Ok((status, price));
                }
            }
            // a transient polling error must not fail an ALREADY-SENT SMS:
            // log it and keep retrying until the deadline, only then return
            // the last known status (or an error, but only if we never got
            // a status at all)
            Err(e) => {
                warn!("stav SMS zatím nezjištěn, zkusím znovu: {e:#}");
                if Instant::now() >= deadline {
                    return match last_ok {
                        Some(s) => Ok(s),
                        None => Err(e.context("stav SMS se nepodařilo ověřit do timeoutu")),
                    };
                }
            }
        }
    }
}

/// Account balance for `doctor --live`.
pub fn balance(account_sid: &str, token: &str) -> Result<String> {
    let url = format!("{API_BASE}/{account_sid}/Balance.json");
    let v: Value = ureq::get(&url)
        .set("Authorization", &basic_auth(account_sid, token))
        .timeout(Duration::from_secs(15))
        .call()
        .context("dotaz na zůstatek selhal (klíče?)")?
        .into_json()
        .context("nečitelná odpověď na zůstatek")?;
    Ok(format!(
        "{} {}",
        v["balance"].as_str().unwrap_or("?"),
        v["currency"].as_str().unwrap_or("")
    ))
}

// ---------- pure helpers (unit-tested) ----------

fn parse_twilio_error(body: &str) -> (i64, String) {
    serde_json::from_str::<Value>(body)
        .map(|v| {
            (
                v["code"].as_i64().unwrap_or(0),
                v["message"].as_str().unwrap_or(body).to_string(),
            )
        })
        .unwrap_or((0, body.to_string()))
}

/// Form params: a Messaging Service SID (`MG…`) goes into
/// MessagingServiceSid, anything else (E.164, alphanumeric sender) into From.
pub(crate) fn form_params<'a>(from: &'a str, to: &'a str, body: &'a str) -> Vec<(&'static str, &'a str)> {
    let sender_key = if is_messaging_sid(from) { "MessagingServiceSid" } else { "From" };
    vec![("To", to), ("Body", body), (sender_key, from)]
}

pub(crate) fn is_e164(s: &str) -> bool {
    let b = s.as_bytes();
    (9..=16).contains(&s.len())
        && b[0] == b'+'
        && b[1] != b'0'
        && b[1..].iter().all(u8::is_ascii_digit)
}

pub(crate) fn is_messaging_sid(s: &str) -> bool {
    s.len() == 34 && s.starts_with("MG") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Alphanumeric sender: 1-11 chars [A-Za-z0-9 ], at least one letter.
pub(crate) fn is_alpha_sender(s: &str) -> bool {
    (1..=11).contains(&s.chars().count())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ')
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

fn basic_auth(user: &str, pass: &str) -> String {
    format!("Basic {}", b64(format!("{user}:{pass}").as_bytes()))
}

/// RFC 4648 base64 (standard alphabet, with padding) — the only use in the
/// project is Basic auth, so a dependency would be overkill.
pub(crate) fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b1 = chunk[0];
        let b2 = chunk.get(1).copied().unwrap_or(0);
        let b3 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b1) << 16) | (u32::from(b2) << 8) | u32::from(b3);
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_rfc4648_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64(b"AC123:token"), "QUMxMjM6dG9rZW4=");
    }

    #[test]
    fn e164_validation() {
        assert!(is_e164("+420733606016"));
        assert!(is_e164("+15005550006"));
        assert!(!is_e164("420733606016")); // missing +
        assert!(!is_e164("+0420733606016")); // zero right after +
        assert!(!is_e164("+420 733 606 016")); // spaces
        assert!(!is_e164("+42")); // too short
        assert!(!is_e164("+123456789012345678")); // too long
    }

    #[test]
    fn messaging_sid_and_alpha_sender() {
        assert!(is_messaging_sid("MG0123456789abcdef0123456789abcdef"));
        assert!(!is_messaging_sid("ACxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")); // AC = Account SID, not Messaging
        assert!(!is_messaging_sid("MG3d99")); // short
        assert!(is_alpha_sender("Olvano"));
        assert!(is_alpha_sender("Jarvis 24"));
        assert!(!is_alpha_sender("123456")); // no letter
        assert!(!is_alpha_sender("MocDlouhySender")); // >11
        assert!(!is_alpha_sender("no-reply")); // hyphen
    }

    #[test]
    fn form_params_pick_sender_field() {
        let p = form_params("MG0123456789abcdef0123456789abcdef", "+4201", "ahoj");
        assert!(p.contains(&("MessagingServiceSid", "MG0123456789abcdef0123456789abcdef")));
        assert!(!p.iter().any(|(k, _)| *k == "From"));
        let p = form_params("+420999888777", "+4201", "ahoj");
        assert!(p.contains(&("From", "+420999888777")));
        let p = form_params("Olvano", "+4201", "ahoj");
        assert!(p.contains(&("From", "Olvano")));
        assert_eq!(p[0], ("To", "+4201"));
        assert_eq!(p[1], ("Body", "ahoj"));
    }

    #[test]
    fn twilio_error_parsing() {
        let (code, msg) =
            parse_twilio_error(r#"{"code": 21608, "message": "The number is unverified", "status": 400}"#);
        assert_eq!(code, 21608);
        assert!(msg.contains("unverified"));
        let (code, msg) = parse_twilio_error("not json");
        assert_eq!(code, 0);
        assert_eq!(msg, "not json");
    }
}
