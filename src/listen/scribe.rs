//! ElevenLabs Scribe (Speech-to-Text) client — a cloud alternative to local
//! whisper (`stt.rs`). Moves transcription off CPU/GPU to the cloud at
//! ~$0.22/h of audio. Sync (ureq) like the rest of the project; the
//! multipart body is built by hand (the project deliberately avoids extra
//! dependencies — even the WAV parser is homegrown). API errors are
//! translated into Czech messages with a hint (user-facing).

use crate::config::ListenCfg;
use crate::listen::audio;
use crate::listen::stt::Transcript;
use crate::util;
use anyhow::{anyhow, Result};
use std::time::Duration;

const API_BASE: &str = "https://api.elevenlabs.io";
/// Scribe requires ≥100 ms of audio; a shorter utterance is padded with silence (16 kHz).
const MIN_SAMPLES: usize = 1_600;

/// Transcribes an utterance (PCM 16 kHz mono) via Scribe. None = transcript
/// had no speech. Retries once on 429/5xx/transport; 4xx is fatal immediately
/// (same as `tts.rs`).
pub fn transcribe(api_key: &str, cfg: &ListenCfg, samples: &[i16]) -> Result<Option<Transcript>> {
    if samples.is_empty() {
        return Ok(None);
    }
    // pad to 100 ms, otherwise the API returns 400 for too-short a clip
    let padded;
    let samples = if samples.len() < MIN_SAMPLES {
        padded = {
            let mut v = samples.to_vec();
            v.resize(MIN_SAMPLES, 0);
            v
        };
        &padded[..]
    } else {
        samples
    };

    let wav = audio::encode_wav_mono_16k(samples);
    let boundary = format!("----jarvisScribe{:016x}", fnv1a(&wav));
    let body = build_multipart(&boundary, cfg, &wav);
    let content_type = format!("multipart/form-data; boundary={boundary}");
    let url = format!("{API_BASE}/v1/speech-to-text");

    let mut last = String::new();
    for (i, delay_s) in [0u64, 3].iter().enumerate() {
        if *delay_s > 0 {
            std::thread::sleep(Duration::from_secs(*delay_s));
        }
        let resp = ureq::post(&url)
            .set("xi-api-key", api_key)
            .set("Content-Type", &content_type)
            .timeout(Duration::from_secs(120))
            .send_bytes(&body);
        match resp {
            Ok(r) => {
                let v: serde_json::Value =
                    r.into_json().map_err(|e| anyhow!("neplatný JSON ze Scribe: {e}"))?;
                return Ok(parse_transcript(&v));
            }
            Err(ureq::Error::Status(code, r)) => {
                let body_text = r.into_string().unwrap_or_default();
                if code == 429 || code >= 500 {
                    last = format!("HTTP {code}: {}", util::truncate_chars(body_text.trim(), 200));
                    tracing::warn!("Scribe pokus {}/2: {last}", i + 1);
                    continue;
                }
                return Err(api_error(code, &body_text));
            }
            Err(e) => {
                last = format!("transport: {e}");
                tracing::warn!("Scribe pokus {}/2: {last}", i + 1);
            }
        }
    }
    Err(anyhow!("Scribe: vyčerpány pokusy, poslední chyba: {last}"))
}

/// Builds the multipart/form-data body: model_id, control flags, and the WAV file.
fn build_multipart(boundary: &str, cfg: &ListenCfg, wav: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(wav.len() + 512);
    let mut field = |name: &str, value: &str| {
        b.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        b.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        b.extend_from_slice(value.as_bytes());
        b.extend_from_slice(b"\r\n");
    };
    field("model_id", &cfg.scribe_model);
    // forcing the language saves latency and autodetection errors; "auto" = leave it to the API
    if cfg.language != "auto" {
        field("language_code", &cfg.language);
    }
    // for the assistant we want clean text: no "(laughs)" tags, no diarization
    field("tag_audio_events", "false");
    field("diarize", "false");
    field("timestamps_granularity", "word");
    // keyterm biasing for proper names (repeated field = List[str] server-side)
    for kt in &cfg.scribe_keyterms {
        field("keyterms", kt);
    }
    // the file goes last
    b.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    b.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"utterance.wav\"\r\n",
    );
    b.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    b.extend_from_slice(wav);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

/// Extracts text, language, and a rough confidence from the Scribe response.
/// Empty text (Scribe "heard no speech") = None, same as whisper returns None.
fn parse_transcript(v: &serde_json::Value) -> Option<Transcript> {
    let text = v["text"].as_str().unwrap_or_default().trim().to_string();
    if text.is_empty() {
        return None;
    }
    let lang = v["language_code"].as_str().unwrap_or("?").to_string();
    let lang_prob = v["language_probability"].as_f64().unwrap_or(0.0) as f32;
    let conf = conf_from_words(&v["words"], lang_prob).clamp(0.0, 1.0);
    Some(Transcript { text, lang, conf })
}

/// Confidence 0-1: average of exp(logprob) over word tokens (comparable to
/// whisper's `conf`, i.e. average token probability). Without words
/// (granularity none / empty), falls back to `language_probability`.
fn conf_from_words(words: &serde_json::Value, lang_prob: f32) -> f32 {
    let mut sum = 0f64;
    let mut n = 0u32;
    if let Some(arr) = words.as_array() {
        for w in arr {
            if w["type"].as_str() == Some("word") {
                if let Some(lp) = w["logprob"].as_f64() {
                    sum += lp.min(0.0).exp(); // logprob ≤ 0 → exp ∈ (0,1]
                    n += 1;
                }
            }
        }
    }
    if n > 0 {
        (sum / f64::from(n)) as f32
    } else {
        lang_prob
    }
}

/// Extracts (status, message) from the error JSON
/// `{"detail": {"status": …, "message": …}}`; `detail` is sometimes a bare
/// string too. (Same schema as the TTS endpoint, but STT has its own set of
/// statuses/hints.)
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

/// Converts a Scribe HTTP error into a readable Czech error with a hint (user-facing).
fn api_error(http: u16, body: &str) -> anyhow::Error {
    let (status, msg) = error_detail(body);
    let hint = match status.as_str() {
        "quota_exceeded" => "\n  → kredity ElevenLabs jsou vyčerpané; dobij, nebo přepni listen.engine = \"whisper\"",
        "missing_permissions" => "\n  → scoped API klíč nemá povolený speech_to_text — rozšiř klíč v ElevenLabs dashboardu",
        "invalid_api_key" | "invalid_authorization_header" | "needs_authorization_header" => {
            "\n  → neplatný API klíč — zkontroluj ELEVENLABS_API_KEY v ~/.config/jarvis/secrets.env"
        }
        "model_not_found" => "\n  → model neexistuje — uprav listen.scribe_model (scribe_v1 | scribe_v2)",
        _ => "",
    };
    let detail = if msg.is_empty() { util::truncate_chars(body.trim(), 300) } else { msg };
    let status_part = if status.is_empty() { String::new() } else { format!(" [{status}]") };
    anyhow!("Scribe HTTP {http}{status_part}: {detail}{hint}")
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ListenCfg;

    fn cfg() -> ListenCfg {
        ListenCfg { scribe_model: "scribe_v1".into(), language: "cs".into(), ..ListenCfg::default() }
    }

    #[test]
    fn multipart_has_fields_file_and_terminator() {
        let wav = audio::encode_wav_mono_16k(&[1, 2, 3, 4]);
        let boundary = "----test";
        let body = build_multipart(boundary, &cfg(), &wav);
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("--\r\n"), "chybí ukončení částí");
        assert!(s.contains("name=\"model_id\"\r\n\r\nscribe_v1\r\n"), "model_id: {s}");
        assert!(s.contains("name=\"language_code\"\r\n\r\ncs\r\n"), "language_code");
        // keyterm biasing as repeated fields
        assert!(s.contains("name=\"keyterms\"\r\n\r\nJarvis\r\n"), "keyterm Jarvis");
        assert!(s.contains("name=\"keyterms\"\r\n\r\nJarvisi\r\n"), "keyterm Jarvisi");
        assert!(s.contains("filename=\"utterance.wav\""), "název souboru");
        assert!(s.contains("Content-Type: audio/wav"), "typ souboru");
        // closing boundary exactly once, at the end
        assert!(s.trim_end().ends_with("------test--"), "uzavírací boundary: {s}");
        // binary WAV is inside the body
        assert!(body.windows(4).any(|w| w == b"RIFF"), "WAV data chybí");
    }

    #[test]
    fn multipart_auto_language_omits_code() {
        let c = ListenCfg { language: "auto".into(), ..cfg() };
        let body = build_multipart("----t", &c, &[0, 1, 2]);
        let s = String::from_utf8_lossy(&body);
        assert!(!s.contains("language_code"), "auto nemá posílat language_code");
    }

    #[test]
    fn multipart_empty_keyterms_omits_field() {
        let c = ListenCfg { scribe_keyterms: vec![], ..cfg() };
        let body = build_multipart("----t", &c, &[0, 1, 2]);
        let s = String::from_utf8_lossy(&body);
        assert!(!s.contains("keyterms"), "prázdné keyterms se nemají posílat");
    }

    #[test]
    fn conf_averages_exp_logprob_over_words() {
        // two words: logprob 0 → 1.0, logprob ln(0.5) → 0.5; average 0.75
        let v = serde_json::json!({
            "words": [
                {"type": "word", "logprob": 0.0},
                {"type": "spacing", "logprob": -9.0},
                {"type": "word", "logprob": (0.5f64).ln()},
            ]
        });
        let c = conf_from_words(&v["words"], 0.1);
        assert!((c - 0.75).abs() < 1e-4, "conf {c}");
    }

    #[test]
    fn conf_falls_back_to_language_probability() {
        let v = serde_json::json!({ "words": [] });
        assert!((conf_from_words(&v["words"], 0.42) - 0.42).abs() < 1e-6);
    }

    #[test]
    fn parse_real_response_shape() {
        // response shape per api-reference/speech-to-text/convert
        let v = serde_json::json!({
            "language_code": "cs",
            "language_probability": 0.98,
            "text": "  Jarvisi, jaké je počasí?  ",
            "words": [
                {"text": "Jarvisi", "type": "word", "logprob": -0.1, "start": 0.0, "end": 0.5},
                {"text": " ", "type": "spacing", "logprob": 0.0},
                {"text": "počasí", "type": "word", "logprob": -0.2, "start": 0.6, "end": 1.0}
            ]
        });
        let t = parse_transcript(&v).expect("má být přepis");
        assert_eq!(t.text, "Jarvisi, jaké je počasí?");
        assert_eq!(t.lang, "cs");
        assert!(t.conf > 0.7 && t.conf <= 1.0, "conf {}", t.conf);
    }

    #[test]
    fn parse_empty_text_is_none() {
        let v = serde_json::json!({ "language_code": "cs", "text": "   ", "words": [] });
        assert!(parse_transcript(&v).is_none());
    }

    #[test]
    fn api_error_hints_are_actionable() {
        let quota = r#"{"detail":{"status":"quota_exceeded","message":"0 credits"}}"#;
        let e = api_error(401, quota).to_string();
        assert!(e.contains("vyčerpané") && e.contains("whisper"), "{e}");
        let model = r#"{"detail":{"status":"model_not_found","message":"nope"}}"#;
        let e = api_error(404, model).to_string();
        assert!(e.contains("listen.scribe_model"), "{e}");
        let perms = r#"{"detail":{"status":"missing_permissions","message":"x"}}"#;
        let e = api_error(401, perms).to_string();
        assert!(e.contains("speech_to_text"), "{e}");
    }

    #[test]
    fn error_detail_bare_string_and_garbage() {
        assert_eq!(error_detail(r#"{"detail":"Not found"}"#), (String::new(), "Not found".into()));
        assert_eq!(error_detail("<html>502</html>"), (String::new(), String::new()));
    }
}
