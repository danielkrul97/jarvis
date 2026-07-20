//! Realtime STT: ElevenLabs `scribe_v2_realtime` over a WebSocket. Audio is
//! streamed while the user speaks (base64 PCM `input_audio_chunk` messages)
//! and the transcript comes back ~150 ms after the final `commit` — instead of
//! POSTing the whole utterance after endpointing (batch `scribe.rs`).
//!
//! Sync like the rest of the project (tungstenite, blocking). The socket is
//! driven from ONE thread: push audio frames as they arrive (writes only),
//! then `commit` and drain reads until the committed transcript. TLS reuses
//! the rustls already pulled in by ureq. Privacy unchanged: the caller streams
//! only frames the local VAD marks as speech, never continuous audio.

use crate::config::{Config, ListenCfg, Paths};
use crate::listen::stt::Transcript;
use crate::listen::vad::{Vad, SAMPLE_RATE};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::Connection;
use std::collections::VecDeque;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// The only realtime Scribe model today; batch models (scribe_v1/v2) don't
/// speak this WebSocket protocol.
const REALTIME_MODEL: &str = "scribe_v2_realtime";
const HOST: &str = "api.elevenlabs.io";
/// Per-read socket timeout while draining the commit response — bounds a single
/// blocking `read()` so the outer deadline stays responsive.
const READ_TIMEOUT: Duration = Duration::from_secs(1);

pub struct RealtimeStt {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
}

// ---------- pure helpers (unit-tested) ----------

/// Standard base64 (with padding) — hand-rolled to avoid a dependency, like the
/// project's WAV/multipart encoders.
fn base64_encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { A[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// Percent-encode a URL query value (keyterms may contain spaces/diacritics).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Builds the `wss://…/realtime` URL with query params. Manual commit (local
/// VAD decides endpoints), 16 kHz PCM, word timestamps (for a confidence like
/// batch Scribe), language pinned unless "auto", keyterm biasing.
fn build_url(cfg: &ListenCfg) -> String {
    let mut url = format!(
        "wss://{HOST}/v1/speech-to-text/realtime?model_id={REALTIME_MODEL}\
         &audio_format=pcm_16000&commit_strategy=manual&include_timestamps=true"
    );
    if cfg.language != "auto" {
        url.push_str(&format!("&language_code={}", percent_encode(&cfg.language)));
    }
    for kt in &cfg.scribe_keyterms {
        url.push_str(&format!("&keyterms={}", percent_encode(kt)));
    }
    url
}

/// One `input_audio_chunk` JSON frame (serde escapes the base64 safely).
fn chunk_message(audio_b64: &str, commit: bool) -> String {
    serde_json::json!({
        "message_type": "input_audio_chunk",
        "audio_base_64": audio_b64,
        "commit": commit,
        "sample_rate": 16000,
    })
    .to_string()
}

/// Confidence 0–1: average of exp(logprob) over word tokens (comparable to
/// whisper/batch-Scribe `conf`). Without words, falls back to `lang_prob`.
fn conf_from_words(words: &serde_json::Value, lang_prob: f32) -> f32 {
    let mut sum = 0f64;
    let mut n = 0u32;
    if let Some(arr) = words.as_array() {
        for w in arr {
            if w["type"].as_str() == Some("word") {
                if let Some(lp) = w["logprob"].as_f64() {
                    sum += lp.min(0.0).exp();
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

/// A parsed server message. `Committed(None)` = the server heard no speech.
enum RtEvent {
    Partial,
    Committed(Option<Transcript>),
    Failed(String),
    Ignored,
}

fn parse_message(text: &str) -> RtEvent {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return RtEvent::Ignored;
    };
    match v["message_type"].as_str().unwrap_or_default() {
        "partial_transcript" => RtEvent::Partial,
        "committed_transcript" | "committed_transcript_with_timestamps" => {
            let t = v["text"].as_str().unwrap_or_default().trim().to_string();
            if t.is_empty() {
                return RtEvent::Committed(None);
            }
            let lang = v["language_code"].as_str().unwrap_or("?").to_string();
            let lang_prob = v["language_probability"].as_f64().unwrap_or(0.9) as f32;
            let conf = conf_from_words(&v["words"], lang_prob).clamp(0.0, 1.0);
            RtEvent::Committed(Some(Transcript { text: t, lang, conf }))
        }
        "error" | "auth_error" | "quota_exceeded" | "rate_limited" | "input_error" => {
            let msg = v["error"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .unwrap_or("realtime error");
            RtEvent::Failed(msg.to_string())
        }
        _ => RtEvent::Ignored, // session_started, timestamps-only, entities, …
    }
}

fn ws_err(op: &str, e: tungstenite::Error) -> anyhow::Error {
    anyhow!("realtime WS {op}: {e}")
}

/// True for a socket read timeout (no data within `READ_TIMEOUT`) — not a real
/// failure, just "keep waiting until the outer deadline".
fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(
        e,
        tungstenite::Error::Io(io)
            if io.kind() == std::io::ErrorKind::WouldBlock
                || io.kind() == std::io::ErrorKind::TimedOut
    )
}

fn set_read_timeout(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>, dur: Duration) {
    let sock: Option<&TcpStream> = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::Rustls(s) => Some(&s.sock),
        _ => None,
    };
    if let Some(s) = sock {
        let _ = s.set_read_timeout(Some(dur));
    }
}

impl RealtimeStt {
    /// Opens the WebSocket (bounded TCP connect, TLS + WS handshake, xi-api-key
    /// header). Fails fast so the caller can fall back to batch Scribe/whisper.
    pub fn connect(key: &str, cfg: &ListenCfg, connect_timeout: Duration) -> Result<Self> {
        let url = build_url(cfg);
        let mut req = url.as_str().into_client_request().context("neplatná realtime URL")?;
        req.headers_mut().insert(
            "xi-api-key",
            HeaderValue::from_str(key).context("neplatný API klíč do WS headeru")?,
        );
        let addr = (HOST, 443)
            .to_socket_addrs()
            .with_context(|| format!("DNS {HOST} selhalo"))?
            .next()
            .with_context(|| format!("{HOST} se nepřeložilo"))?;
        let stream = TcpStream::connect_timeout(&addr, connect_timeout)
            .with_context(|| format!("TCP připojení k {addr} selhalo"))?;
        // covers the TLS + WS handshake reads; tightened after connect
        let _ = stream.set_read_timeout(Some(connect_timeout));
        let (mut ws, _resp) = tungstenite::client_tls(req, stream)
            .map_err(|e| anyhow!("realtime WS handshake: {e}"))?;
        set_read_timeout(&mut ws, READ_TIMEOUT);
        Ok(Self { ws })
    }

    /// Streams one batch of samples (PCM 16 kHz mono) — write only, no commit.
    pub fn push_samples(&mut self, samples: &[i16]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let msg = chunk_message(&base64_encode(&bytes), false);
        self.ws.write(Message::Text(msg)).map_err(|e| ws_err("send audio", e))?;
        self.ws.flush().map_err(|e| ws_err("flush", e))?;
        Ok(())
    }

    /// Commits the utterance and waits for the committed transcript. `None` =
    /// server heard no speech. Bounded by `timeout`; a hung server → Err (the
    /// caller falls back to batch on the buffered samples).
    pub fn commit(&mut self, timeout: Duration) -> Result<Option<Transcript>> {
        self.ws
            .write(Message::Text(chunk_message("", true)))
            .map_err(|e| ws_err("send commit", e))?;
        self.ws.flush().map_err(|e| ws_err("send commit", e))?;
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                bail!("realtime: commit se nedočkal transkriptu do {} s", timeout.as_secs());
            }
            match self.ws.read() {
                Ok(Message::Text(t)) => match parse_message(t.as_str()) {
                    RtEvent::Committed(opt) => return Ok(opt),
                    RtEvent::Failed(e) => bail!("realtime server: {e}"),
                    RtEvent::Partial | RtEvent::Ignored => continue,
                },
                Ok(Message::Close(_)) => bail!("realtime: server zavřel spojení před commitem"),
                Ok(_) => continue, // ping/pong/binary handled by tungstenite
                Err(e) if is_timeout(&e) => continue, // keep waiting until deadline
                Err(e) => return Err(ws_err("read", e)),
            }
        }
    }
}

impl Drop for RealtimeStt {
    fn drop(&mut self) {
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }
}

// ---------- streaming worker ----------

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(6);
/// Rolling pre-roll kept while idle, so the word onset before the first loud
/// frame is streamed too (mirrors the VAD's own pre-roll into the utterance).
const PREROLL_MS: usize = 300;

/// Realtime STT worker (`listen.engine = "realtime"`). Runs its OWN VAD over the
/// teed mic frames: opens a WebSocket at speech onset, streams frames as they
/// arrive, and commits at endpoint — so the transcript is ready ~150 ms after
/// the user stops instead of a full batch round-trip. Any WS failure (no key,
/// connect/stream/commit error) falls back to `fallback` (batch Scribe →
/// whisper) on the buffered utterance, so a dead network never silences Jarvis.
/// Privacy is unchanged: the caller tees only non-paused frames, and audio is
/// sent to the cloud only while this VAD marks speech.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_worker(
    paths: &Paths,
    cfg: &Config,
    rx: mpsc::Receiver<Vec<i16>>,
    fallback: &mut super::Transcriber,
    conn: &Connection,
    print_only: bool,
    source: &str,
    convo: Option<&super::ConvoHook>,
) {
    let key = crate::config::elevenlabs_key(paths).ok();
    if key.is_none() {
        warn!("realtime STT: chybí ELEVENLABS_API_KEY — jedu jen na batch/whisper fallbacku");
    } else {
        info!("STT: realtime scribe_v2_realtime přes WebSocket (fallback: batch Scribe → whisper)");
    }
    let mut vad = Vad::new(super::vad_config(cfg));
    let preroll_cap = SAMPLE_RATE * PREROLL_MS / 1000;
    let mut preroll: VecDeque<i16> = VecDeque::with_capacity(preroll_cap);
    let mut session: Option<RealtimeStt> = None;

    while let Ok(frame) = rx.recv() {
        let now = crate::util::now_ts();
        let was_active = vad.active_started_at().is_some();
        let utt = vad.push_frame(&frame, now);
        let active_now = vad.active_started_at().is_some();

        if !was_active && active_now {
            // onset: open the WS (if we have a key) and flush pre-roll + this frame
            session = key.as_deref().and_then(|k| {
                match RealtimeStt::connect(k, &cfg.listen, CONNECT_TIMEOUT) {
                    Ok(mut s) => {
                        let pre: Vec<i16> = preroll.iter().copied().collect();
                        match s.push_samples(&pre).and_then(|()| s.push_samples(&frame)) {
                            Ok(()) => Some(s),
                            Err(e) => {
                                debug!("realtime stream (onset) selhal ({e:#}) — batch fallback");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        debug!("realtime connect selhal ({e:#}) — batch fallback pro tuhle promluvu");
                        None
                    }
                }
            });
            preroll.clear();
        } else if was_active && active_now {
            // ongoing speech → keep streaming
            if let Some(s) = &mut session {
                if let Err(e) = s.push_samples(&frame) {
                    debug!("realtime stream selhal ({e:#}) — batch fallback");
                    session = None;
                }
            }
        } else {
            // idle: roll the pre-roll ring so the onset isn't lost at connect time
            preroll.extend(frame.iter().copied());
            while preroll.len() > preroll_cap {
                preroll.pop_front();
            }
        }

        if let Some(u) = utt {
            // endpoint: commit the WS session, or batch-fallback on the buffered samples
            let dur = u.samples.len() as f32 / SAMPLE_RATE as f32;
            let t0 = Instant::now();
            let transcript = match session.take() {
                Some(mut s) => s.commit(COMMIT_TIMEOUT).unwrap_or_else(|e| {
                    warn!("realtime commit selhal — batch fallback: {e:#}");
                    fallback.transcribe(&u.samples).unwrap_or_default()
                }),
                None => fallback.transcribe(&u.samples).unwrap_or_else(|e| {
                    warn!("realtime fallback (batch) selhal: {e:#}");
                    None
                }),
            };
            match transcript {
                Some(t) => super::deliver_transcript(
                    conn,
                    &t,
                    u.started_at,
                    u.ended_at,
                    dur,
                    t0.elapsed().as_secs_f32(),
                    print_only,
                    source,
                    convo,
                ),
                None => {
                    // no speech — consume any pending barge candidate so it doesn't hang
                    if let Some(h) = convo {
                        h.barge_start.store(0, Ordering::Relaxed);
                    }
                    debug!("realtime: promluva bez řeči ({dur:.1} s)");
                }
            }
        } else if was_active && !active_now {
            // speech aborted (too short / VAD dropped it) → drop the WS session
            session = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn pcm_roundtrip_shape() {
        // i16 LE → base64 of the exact bytes
        let samples: Vec<i16> = vec![0, 1, -1, 256];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(bytes, vec![0, 0, 1, 0, 255, 255, 0, 1]);
        assert_eq!(base64_encode(&bytes), "AAABAP//AAE=");
    }

    #[test]
    fn percent_encode_diacritics_and_space() {
        assert_eq!(percent_encode("Jarvis"), "Jarvis");
        assert_eq!(percent_encode("Tomáš Messing"), "Tom%C3%A1%C5%A1%20Messing");
    }

    #[test]
    fn url_has_required_params() {
        let cfg = ListenCfg {
            language: "cs".into(),
            scribe_keyterms: vec!["Jarvis".into(), "Tomáš".into()],
            ..ListenCfg::default()
        };
        let u = build_url(&cfg);
        assert!(u.starts_with("wss://api.elevenlabs.io/v1/speech-to-text/realtime?"));
        assert!(u.contains("model_id=scribe_v2_realtime"));
        assert!(u.contains("audio_format=pcm_16000"));
        assert!(u.contains("commit_strategy=manual"));
        assert!(u.contains("language_code=cs"));
        assert!(u.contains("keyterms=Jarvis"));
        assert!(u.contains("keyterms=Tom%C3%A1%C5%A1"));
    }

    #[test]
    fn url_auto_language_omits_code() {
        let cfg = ListenCfg { language: "auto".into(), scribe_keyterms: vec![], ..ListenCfg::default() };
        let u = build_url(&cfg);
        assert!(!u.contains("language_code"));
        assert!(!u.contains("keyterms="));
    }

    #[test]
    fn chunk_message_shapes() {
        let m = chunk_message("QUJD", false);
        assert!(m.contains("\"message_type\":\"input_audio_chunk\""));
        assert!(m.contains("\"audio_base_64\":\"QUJD\""));
        assert!(m.contains("\"commit\":false"));
        assert!(m.contains("\"sample_rate\":16000"));
        assert!(chunk_message("", true).contains("\"commit\":true"));
    }

    #[test]
    fn parse_committed_with_timestamps() {
        let msg = serde_json::json!({
            "message_type": "committed_transcript_with_timestamps",
            "text": "  Jarvisi, kolik je hodin?  ",
            "language_code": "cs",
            "words": [
                {"text": "Jarvisi", "type": "word", "logprob": -0.1},
                {"text": " ", "type": "spacing", "logprob": 0.0},
                {"text": "hodin", "type": "word", "logprob": -0.2}
            ]
        })
        .to_string();
        match parse_message(&msg) {
            RtEvent::Committed(Some(t)) => {
                assert_eq!(t.text, "Jarvisi, kolik je hodin?");
                assert_eq!(t.lang, "cs");
                assert!(t.conf > 0.7 && t.conf <= 1.0, "conf {}", t.conf);
            }
            _ => panic!("čekám Committed(Some)"),
        }
    }

    #[test]
    fn parse_committed_empty_is_none() {
        let msg = r#"{"message_type":"committed_transcript","text":"   "}"#;
        assert!(matches!(parse_message(msg), RtEvent::Committed(None)));
    }

    #[test]
    fn parse_partial_and_error_and_ignored() {
        assert!(matches!(
            parse_message(r#"{"message_type":"partial_transcript","text":"Jar"}"#),
            RtEvent::Partial
        ));
        assert!(matches!(
            parse_message(r#"{"message_type":"session_started","session_id":"x"}"#),
            RtEvent::Ignored
        ));
        match parse_message(r#"{"message_type":"quota_exceeded","error":"no credits"}"#) {
            RtEvent::Failed(e) => assert!(e.contains("no credits")),
            _ => panic!("čekám Failed"),
        }
        assert!(matches!(parse_message("not json"), RtEvent::Ignored));
    }
}
