//! ElevenLabs API client (ureq, sync — like the rest of the project). Covers
//! only what Jarvis needs: speech synthesis, listing account voices, credit
//! status. API error responses are translated into Czech messages with hints,
//! because a typical scoped key only allows `text_to_speech` and everything else returns 401.

use crate::config::SpeakCfg;
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::io::Read;
use std::time::Duration;

const API_BASE: &str = "https://api.elevenlabs.io";
/// A TTS response is audio; more than 100 MB means something went wrong.
const MAX_AUDIO_BYTES: u64 = 100 * 1024 * 1024;

/// Only *_v2_5 models can force the language via `language_code`;
/// multilingual_v2 rejects it (HTTP 400) and detects the language from the text.
pub fn supports_language_code(model_id: &str) -> bool {
    model_id.ends_with("_v2_5")
}

/// TTS request body per config.
fn payload(cfg: &SpeakCfg, text: &str) -> serde_json::Value {
    // f32 → f64 drags in binary noise (0.95 → 0.9499999…); API gets 3 decimals
    let r3 = |x: f32| (f64::from(x) * 1000.0).round() / 1000.0;
    let mut v = json!({
        "text": text,
        "model_id": cfg.model_id,
        "voice_settings": {
            "stability": r3(cfg.stability),
            "similarity_boost": r3(cfg.similarity_boost),
            "style": r3(cfg.style),
            "use_speaker_boost": cfg.speaker_boost,
            "speed": r3(cfg.speed),
        }
    });
    if cfg.language != "auto" && supports_language_code(&cfg.model_id) {
        v["language_code"] = json!(cfg.language);
    }
    v
}

/// Extracts (status, message) from the error JSON `{"detail": {"status": …,
/// "message": …}}`; `detail` is sometimes a bare string instead.
fn error_detail(body: &str) -> (String, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let d = &v["detail"];
    if let Some(s) = d.as_str() {
        return (String::new(), s.to_string());
    }
    let status = d["status"]
        .as_str()
        .or_else(|| d["code"].as_str())
        .unwrap_or_default()
        .to_string();
    let msg = d["message"].as_str().unwrap_or_default().to_string();
    (status, msg)
}

/// Converts an API HTTP error into a readable Czech error message with a hint.
fn api_error(http: u16, body: &str) -> anyhow::Error {
    let (status, msg) = error_detail(body);
    let hint = match status.as_str() {
        "quota_exceeded" => "\n  → kredity ElevenLabs jsou vyčerpané; počkej na obnovu měsíční kvóty, nebo dobij",
        "missing_permissions" => "\n  → scoped API klíč tuhle operaci nemá povolenou — rozšiř klíč v ElevenLabs dashboardu",
        "invalid_api_key" | "invalid_authorization_header" | "needs_authorization_header" => {
            "\n  → neplatný API klíč — zkontroluj ELEVENLABS_API_KEY v ~/.config/jarvis/secrets.env"
        }
        "voice_not_found" => "\n  → hlas s tímhle ID neexistuje nebo není v účtu — uprav speak.voice_id",
        "model_not_found" => "\n  → model neexistuje — uprav speak.model_id (např. eleven_multilingual_v2)",
        _ => "",
    };
    let detail = if msg.is_empty() { util::truncate_chars(body.trim(), 300) } else { msg };
    let status_part = if status.is_empty() { String::new() } else { format!(" [{status}]") };
    anyhow!("ElevenLabs HTTP {http}{status_part}: {detail}{hint}")
}

/// Speech synthesis: returns audio bytes (format per `cfg.output_format`).
/// Retries once on 429/5xx/transport errors; 4xx is fatal immediately.
pub fn synthesize(api_key: &str, cfg: &SpeakCfg, voice_id: &str, text: &str) -> Result<Vec<u8>> {
    let url = format!(
        "{API_BASE}/v1/text-to-speech/{voice_id}?output_format={}",
        cfg.output_format
    );
    let body = payload(cfg, text);
    let mut last = String::new();
    for (i, delay_s) in [0u64, 3].iter().enumerate() {
        if *delay_s > 0 {
            std::thread::sleep(Duration::from_secs(*delay_s));
        }
        let resp = ureq::post(&url)
            .set("xi-api-key", api_key)
            .timeout(Duration::from_secs(120))
            .send_json(body.clone());
        match resp {
            Ok(r) => {
                let mut buf = Vec::new();
                r.into_reader()
                    .take(MAX_AUDIO_BYTES)
                    .read_to_end(&mut buf)
                    .context("čtení audio odpovědi selhalo")?;
                if buf.is_empty() {
                    bail!("ElevenLabs vrátil prázdné audio");
                }
                return Ok(buf);
            }
            Err(ureq::Error::Status(code, r)) => {
                let body_text = r.into_string().unwrap_or_default();
                if code == 429 || code >= 500 {
                    last = format!("HTTP {code}: {}", util::truncate_chars(body_text.trim(), 200));
                    tracing::warn!("ElevenLabs pokus {}/2: {last}", i + 1);
                    continue;
                }
                return Err(api_error(code, &body_text));
            }
            Err(e) => {
                last = format!("transport: {e}");
                tracing::warn!("ElevenLabs pokus {}/2: {last}", i + 1);
            }
        }
    }
    Err(anyhow!("ElevenLabs: vyčerpány pokusy, poslední chyba: {last}"))
}

/// Like `synthesize`, but returns a STREAM of audio bytes from the `/stream`
/// endpoint — the player plays it as it arrives (speech starts after the
/// first chunk, not the whole mp3). Retry once on 429/5xx/transport happens
/// BEFORE consuming the body; 4xx is fatal immediately. Body format is the same as `synthesize`.
pub fn synthesize_stream(
    api_key: &str,
    cfg: &SpeakCfg,
    voice_id: &str,
    text: &str,
) -> Result<Box<dyn Read + Send>> {
    let url = format!(
        "{API_BASE}/v1/text-to-speech/{voice_id}/stream?output_format={}",
        cfg.output_format
    );
    let body = payload(cfg, text);
    let mut last = String::new();
    for (i, delay_s) in [0u64, 3].iter().enumerate() {
        if *delay_s > 0 {
            std::thread::sleep(Duration::from_secs(*delay_s));
        }
        match ureq::post(&url)
            .set("xi-api-key", api_key)
            .timeout(Duration::from_secs(120))
            .send_json(body.clone())
        {
            Ok(r) => return Ok(Box::new(r.into_reader())),
            Err(ureq::Error::Status(code, r)) => {
                let body_text = r.into_string().unwrap_or_default();
                if code == 429 || code >= 500 {
                    last = format!("HTTP {code}: {}", util::truncate_chars(body_text.trim(), 200));
                    tracing::warn!("ElevenLabs stream pokus {}/2: {last}", i + 1);
                    continue;
                }
                return Err(api_error(code, &body_text));
            }
            Err(e) => {
                last = format!("transport: {e}");
                tracing::warn!("ElevenLabs stream pokus {}/2: {last}", i + 1);
            }
        }
    }
    Err(anyhow!("ElevenLabs stream: vyčerpány pokusy, poslední chyba: {last}"))
}

pub struct VoiceInfo {
    pub id: String,
    pub name: String,
    pub category: String,
    pub labels: String,
}

/// Voices available in the account (`jarvis say --list-voices`). Requires a
/// key with `voices_read` permission — a scoped TTS key gets an explanatory error.
pub fn list_voices(api_key: &str) -> Result<Vec<VoiceInfo>> {
    let resp = ureq::get(&format!("{API_BASE}/v1/voices"))
        .set("xi-api-key", api_key)
        .timeout(Duration::from_secs(30))
        .call();
    let r = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            return Err(api_error(code, &r.into_string().unwrap_or_default()));
        }
        Err(e) => return Err(anyhow!("transport: {e}")),
    };
    let v: serde_json::Value = r.into_json().context("neplatný JSON z /v1/voices")?;
    let voices = v["voices"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| VoiceInfo {
                    id: x["voice_id"].as_str().unwrap_or_default().to_string(),
                    name: x["name"].as_str().unwrap_or_default().to_string(),
                    category: x["category"].as_str().unwrap_or_default().to_string(),
                    labels: x["labels"]
                        .as_object()
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, val)| val.as_str().map(|s| format!("{k}={s}")))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(voices)
}

/// Credit status for `doctor --live`.
pub enum Credits {
    /// (used, monthly quota)
    Known { used: u64, limit: u64 },
    /// The key is valid but scoped without `user_read` — balance can't be read.
    NoPermission,
}

pub fn credits(api_key: &str) -> Result<Credits> {
    let resp = ureq::get(&format!("{API_BASE}/v1/user/subscription"))
        .set("xi-api-key", api_key)
        .timeout(Duration::from_secs(30))
        .call();
    match resp {
        Ok(r) => {
            let v: serde_json::Value = r.into_json().context("neplatný JSON subscription")?;
            Ok(Credits::Known {
                used: v["character_count"].as_u64().unwrap_or(0),
                limit: v["character_limit"].as_u64().unwrap_or(0),
            })
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            let (status, _) = error_detail(&body);
            if status == "missing_permissions" {
                return Ok(Credits::NoPermission);
            }
            Err(api_error(code, &body))
        }
        Err(e) => Err(anyhow!("transport: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SpeakCfg;

    #[test]
    fn payload_multilingual_v2_omits_language_code() {
        // explicitly multilingual_v2 (default is now flash_v2_5) — verifies that
        // a model without support doesn't force the language (would send HTTP 400)
        let cfg = SpeakCfg { model_id: "eleven_multilingual_v2".into(), ..SpeakCfg::default() };
        let v = payload(&cfg, "Dobrý den");
        assert_eq!(v["model_id"], "eleven_multilingual_v2");
        assert!(v.get("language_code").is_none());
        assert_eq!(v["voice_settings"]["speed"], 0.95);
        assert_eq!(v["voice_settings"]["use_speaker_boost"], true);
    }

    #[test]
    fn payload_default_flash_enforces_czech() {
        // the new default (flash_v2_5) supports and should force language_code=cs
        let v = payload(&SpeakCfg::default(), "Dobrý den");
        assert_eq!(v["model_id"], "eleven_flash_v2_5");
        assert_eq!(v["language_code"], "cs");
    }

    #[test]
    fn payload_v2_5_enforces_czech() {
        let cfg = SpeakCfg { model_id: "eleven_flash_v2_5".into(), ..SpeakCfg::default() };
        let v = payload(&cfg, "Dobrý den");
        assert_eq!(v["language_code"], "cs");
        // "auto" disables forcing even for models that support it
        let cfg = SpeakCfg {
            model_id: "eleven_flash_v2_5".into(),
            language: "auto".into(),
            ..SpeakCfg::default()
        };
        assert!(payload(&cfg, "x").get("language_code").is_none());
    }

    /// Real API responses captured 2026-07-17 while wiring up the key.
    #[test]
    fn error_detail_real_responses() {
        let quota = r#"{"detail":{"type":"invalid_request","code":"quota_exceeded","message":"This request exceeds your quota of 159644. You have 0 credits remaining, while 6 credits are required for this request.","status":"quota_exceeded","request_id":"9d1e89c6c5a9f0f49e232e0f7766bc8b"}}"#;
        let (status, msg) = error_detail(quota);
        assert_eq!(status, "quota_exceeded");
        assert!(msg.contains("0 credits remaining"));

        let perms = r#"{"detail":{"type":"authentication_error","code":"unauthorized","message":"The API key you used is missing the permission user_read to execute this operation.","status":"missing_permissions","request_id":"610a7ee3a2597511086bf79a90142308"}}"#;
        let (status, _) = error_detail(perms);
        assert_eq!(status, "missing_permissions");

        // detail as a bare string (some older endpoints return this)
        let (status, msg) = error_detail(r#"{"detail":"Not found"}"#);
        assert_eq!(status, "");
        assert_eq!(msg, "Not found");

        // non-JSON body must not panic
        let (status, msg) = error_detail("<html>gateway timeout</html>");
        assert_eq!(status, "");
        assert_eq!(msg, "");
    }

    #[test]
    fn api_error_hints() {
        let quota = r#"{"detail":{"status":"quota_exceeded","message":"0 credits"}}"#;
        let e = api_error(401, quota).to_string();
        assert!(e.contains("vyčerpané"), "{e}");
        let bad_voice = r#"{"detail":{"status":"voice_not_found","message":"nope"}}"#;
        let e = api_error(404, bad_voice).to_string();
        assert!(e.contains("speak.voice_id"), "{e}");
    }
}
