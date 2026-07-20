//! Wrapper around whisper.cpp (whisper-rs): model loading, transcribing a
//! single utterance, anti-hallucination filters, and ggml model downloads.

use crate::listen::vad::SAMPLE_RATE;
use crate::util;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, info};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub struct Stt {
    // ctx holds the model; state references it via Arc internally, but we
    // keep ctx around for ownership clarity
    _ctx: WhisperContext,
    state: WhisperState,
    language: Option<String>,
    threads: i32,
    /// Whisper's initial prompt: a dictionary hint for proper names.
    hint: Option<String>,
}

#[derive(Debug)]
pub struct Transcript {
    pub text: String,
    pub lang: String,
    /// Average token probability 0-1 (rough transcript confidence).
    pub conf: f32,
}

/// Phrases whisper typically hallucinates on silence/music (Czech + English).
/// Only short, low-confidence transcripts get dropped — see `is_hallucination`.
const HALLUCINATIONS: &[&str] = &[
    "titulky vytvořil",
    "titulky vytvořila",
    "děkuji za zhlédnutí",
    "děkujeme za zhlédnutí",
    "thanks for watching",
    "thank you for watching",
    "subtitles by",
    "podpořte kanál",
];

pub fn is_hallucination(text: &str, conf: f32) -> bool {
    // Threshold 0.92: in live use (2026-07-17) whisper hallucinated "Titulky
    // vytvořil …" ("Subtitles by …") on keyboard noise/room tone with conf
    // 0.80-0.87 — confidence means nothing for these phrases. Short text +
    // known phrase = almost certainly noise.
    if text.chars().count() > 80 {
        return false;
    }
    let t = text.to_lowercase();
    // A bare "konec" ("end") is a classic Czech hallucination on breath/noise
    // (seen live 6x in one day, conf 0.45-0.78; threshold 0.85 with margin).
    if conf < 0.85 && t.trim_matches(|c: char| c.is_ascii_punctuation() || c == ' ') == "konec" {
        return true;
    }
    conf < 0.92 && HALLUCINATIONS.iter().any(|h| t.contains(h))
}

impl Stt {
    pub fn load(model_path: &Path, language: &str, threads_cfg: usize, hint: &str) -> Result<Self> {
        // route whisper.cpp/ggml logs → tracing (otherwise they'd spam stderr on every load)
        static LOG_HOOKS: std::sync::Once = std::sync::Once::new();
        LOG_HOOKS.call_once(whisper_rs::install_logging_hooks);

        let threads = if threads_cfg == 0 {
            (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8) / 2).clamp(2, 8)
        } else {
            threads_cfg.min(64)
        } as i32;
        let path = model_path.to_str().context("cesta k modelu není platné UTF-8")?;
        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .with_context(|| format!("nelze načíst whisper model {path}"))?;
        let state = ctx.create_state().context("vytvoření whisper state selhalo")?;
        info!(
            "whisper model načten: {} ({} vláken, jazyk {})",
            model_path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
            threads,
            if language == "auto" { "autodetekce" } else { language },
        );
        Ok(Self {
            _ctx: ctx,
            state,
            language: (language != "auto").then(|| language.to_string()),
            threads,
            hint: {
                let h = hint.trim();
                (!h.is_empty()).then(|| h.to_string())
            },
        })
    }

    /// Transcribes an utterance (PCM 16 kHz mono). None = no speech in it.
    pub fn transcribe(&mut self, samples: &[i16]) -> Result<Option<Transcript>> {
        // whisper complains on input < ~1 s — pad with silence
        const MIN_SAMPLES: usize = SAMPLE_RATE + SAMPLE_RATE / 5;
        let mut audio: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32768.0).collect();
        if audio.len() < MIN_SAMPLES {
            audio.resize(MIN_SAMPLES, 0.0);
        }

        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        p.set_language(self.language.as_deref()); // None = autodetect
        p.set_translate(false);
        p.set_no_context(true); // utterances are independent; prevents hallucination carryover
        if let Some(h) = &self.hint {
            p.set_initial_prompt(h); // dictionary: proper names ("Jarvisi")
        }
        p.set_suppress_blank(true);
        p.set_suppress_nst(true); // non-speech tokens (♪ …)
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        // A short utterance doesn't need the full 30s encoder context — the
        // main CPU saving (same trick as whisper.cpp's stream example). Floor
        // 512: a smaller context caused text to repeat in the transcript.
        let secs = audio.len() as f32 / SAMPLE_RATE as f32;
        if secs < 29.0 {
            p.set_audio_ctx(((secs / 30.0 * 1500.0) as i32 + 128).clamp(512, 1500));
        }

        self.state.full(p, &audio).context("whisper_full selhal")?;

        let n = self.state.full_n_segments();
        let mut text = String::new();
        let (mut conf_sum, mut conf_w) = (0f64, 0f64);
        for i in 0..n {
            let Some(seg) = self.state.get_segment(i) else { continue };
            let seg_text = match seg.to_str_lossy() {
                Ok(t) => t.trim().to_string(),
                Err(_) => continue,
            };
            if seg_text.is_empty() {
                continue;
            }
            let nt = seg.n_tokens();
            let avg_p = if nt > 0 {
                (0..nt)
                    .filter_map(|j| seg.get_token(j))
                    .map(|t| f64::from(t.token_probability()))
                    .sum::<f64>()
                    / f64::from(nt)
            } else {
                0.0
            };
            // classic combo: the segment is most likely "transcribed silence"
            if seg.no_speech_probability() > 0.8 && avg_p < 0.35 {
                debug!(
                    "segment zahozen (no_speech {:.2}, p {:.2}): {seg_text}",
                    seg.no_speech_probability(),
                    avg_p
                );
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&seg_text);
            conf_sum += avg_p * f64::from(nt);
            conf_w += f64::from(nt);
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(None);
        }
        let conf = if conf_w > 0.0 { (conf_sum / conf_w) as f32 } else { 0.0 };
        let lang = whisper_rs::get_lang_str(self.state.full_lang_id_from_state())
            .unwrap_or("?")
            .to_string();
        if is_hallucination(&text, conf) {
            debug!("halucinace zahozena (p {conf:.2}): {text}");
            return Ok(None);
        }
        Ok(Some(Transcript { text, lang, conf }))
    }
}

// ---------- model downloads ----------

const MODEL_BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Downloads a ggml model (e.g. "large-v3-turbo-q5_0") into `models_dir`.
/// Atomic via .part; an existing file is not re-downloaded.
pub fn download_model(models_dir: &Path, name: &str) -> Result<PathBuf> {
    let target = models_dir.join(format!("ggml-{name}.bin"));
    util::download(&format!("{MODEL_BASE_URL}/ggml-{name}.bin"), &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hallucination_filter() {
        // short + known phrase → hallucination even at high confidence
        // (actually measured p 0.87 on keyboard noise)
        assert!(is_hallucination("Titulky vytvořil DimSum Team", 0.3));
        assert!(is_hallucination("Titulky vytvořil Jirka Kováč", 0.87));
        assert!(is_hallucination("Děkuji za zhlédnutí!", 0.5));
        assert!(is_hallucination("thanks for watching", 0.2));
        // an extremely confident transcript is not dropped
        assert!(!is_hallucination("Děkuji za zhlédnutí", 0.95));
        // long text is not dropped (real speech about subtitles)
        let long = "Dneska jsem řešil, jak se generují titulky. Titulky vytvořil \
                    nástroj, který jsme ladili celé odpoledne a výsledek je fajn.";
        assert!(!is_hallucination(long, 0.3));
        // ordinary Czech sentence
        assert!(!is_hallucination("pojď se podívat na ten pull request", 0.4));
        // bare "konec" = hallucination on noise; kept when confident or in a sentence
        assert!(is_hallucination("Konec.", 0.75));
        assert!(is_hallucination(" konec ", 0.45));
        assert!(!is_hallucination("Konec.", 0.9));
        assert!(!is_hallucination("a to je konec porady", 0.5));
    }
}
