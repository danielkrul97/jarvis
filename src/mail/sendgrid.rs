use crate::config::EmailCfg;
use crate::util;
use anyhow::{anyhow, Result};
use serde_json::json;
use std::time::Duration;
use tracing::warn;

const API_URL: &str = "https://api.sendgrid.com/v3/mail/send";

enum SendError {
    Retryable(String),
    Fatal(String),
}

fn payload(
    cfg: &EmailCfg,
    subject: &str,
    text: &str,
    html: &str,
    sandbox: bool,
) -> serde_json::Value {
    let mut v = json!({
        "personalizations": [{"to": [{"email": cfg.to}]}],
        "from": {"email": cfg.from, "name": cfg.from_name},
        "subject": subject,
        // SendGrid vyžaduje text/plain před text/html
        "content": [
            {"type": "text/plain", "value": text},
            {"type": "text/html", "value": html}
        ]
    });
    if sandbox {
        v["mail_settings"] = json!({"sandbox_mode": {"enable": true}});
    }
    v
}

/// Odešle e-mail; retry s backoffem na 429/5xx/transport chyby.
/// Vrací SendGrid X-Message-Id, pokud ho server poslal.
pub fn send(
    cfg: &EmailCfg,
    api_key: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> Result<Option<String>> {
    send_inner(cfg, api_key, subject, text, html, false)
}

/// Sandbox mód: SendGrid požadavek zvaliduje (klíč, odesílatel), ale nic neodešle.
pub fn sandbox_check(cfg: &EmailCfg, api_key: &str) -> Result<()> {
    send_inner(cfg, api_key, "Jarvis sandbox check", "test", "<p>test</p>", true).map(|_| ())
}

fn send_inner(
    cfg: &EmailCfg,
    api_key: &str,
    subject: &str,
    text: &str,
    html: &str,
    sandbox: bool,
) -> Result<Option<String>> {
    let body = payload(cfg, subject, text, html, sandbox);
    let delays_s: [u64; 4] = [0, 2, 8, 30];
    let mut last_err = String::new();
    for (i, delay) in delays_s.iter().enumerate() {
        if *delay > 0 {
            std::thread::sleep(Duration::from_secs(*delay));
        }
        match try_send(&body, api_key) {
            Ok(msg_id) => return Ok(msg_id),
            Err(SendError::Fatal(e)) => return Err(anyhow!("SendGrid: {e}")),
            Err(SendError::Retryable(e)) => {
                warn!("SendGrid pokus {}/{}: {e}", i + 1, delays_s.len());
                last_err = e;
            }
        }
    }
    Err(anyhow!("SendGrid: vyčerpány pokusy, poslední chyba: {last_err}"))
}

fn try_send(body: &serde_json::Value, api_key: &str) -> Result<Option<String>, SendError> {
    let resp = ureq::post(API_URL)
        .set("Authorization", &format!("Bearer {api_key}"))
        .timeout(Duration::from_secs(30))
        .send_json(body.clone());
    match resp {
        Ok(r) => Ok(r.header("X-Message-Id").map(str::to_string)),
        Err(ureq::Error::Status(code, r)) => {
            let body_text = r.into_string().unwrap_or_default();
            let hint = match code {
                401 => " (neplatný API klíč?)",
                403 => " (odesílatel zřejmě není verifikovaný — SendGrid Single Sender Verification)",
                _ => "",
            };
            let msg = format!("HTTP {code}{hint}: {}", util::truncate_chars(body_text.trim(), 300));
            if code == 429 || code >= 500 {
                Err(SendError::Retryable(msg))
            } else {
                Err(SendError::Fatal(msg))
            }
        }
        Err(e) => Err(SendError::Retryable(format!("transport: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EmailCfg {
        EmailCfg::default()
    }

    #[test]
    fn payload_shape_plain_before_html() {
        let v = payload(&cfg(), "Subj", "plain", "<p>html</p>", false);
        let content = v["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text/plain");
        assert_eq!(content[1]["type"], "text/html");
        assert_eq!(v["personalizations"][0]["to"][0]["email"], "dankrul.krul@gmail.com");
        assert!(v.get("mail_settings").is_none());
    }

    #[test]
    fn sandbox_flag_present() {
        let v = payload(&cfg(), "s", "t", "h", true);
        assert_eq!(v["mail_settings"]["sandbox_mode"]["enable"], true);
    }
}
