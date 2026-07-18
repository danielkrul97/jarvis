//! Wrapper nad whisper.cpp (whisper-rs): načtení modelu, přepis jedné
//! promluvy, filtry proti halucinacím a stahování ggml modelů.

use crate::listen::vad::SAMPLE_RATE;
use crate::util;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, info};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub struct Stt {
    // ctx drží model; state na něj odkazuje přes Arc, ale ctx si necháváme
    // pro čitelnost vlastnictví
    _ctx: WhisperContext,
    state: WhisperState,
    language: Option<String>,
    threads: i32,
    /// Initial prompt whisperu: slovníková nápověda na vlastní jména.
    hint: Option<String>,
}

#[derive(Debug)]
pub struct Transcript {
    pub text: String,
    pub lang: String,
    /// Průměrná pravděpodobnost tokenů 0–1 (hrubá jistota přepisu).
    pub conf: f32,
}

/// Fráze, které whisper typicky halucinuje na tichu/hudbě (č. + angl.).
/// Zahazují se jen krátké a málo jisté přepisy — viz `is_hallucination`.
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
    // Práh 0.92: v živém provozu (2026-07-17) whisper halucinoval „Titulky
    // vytvořil …" na klávesnici/ruchy s conf 0.80–0.87 — jistota u těchto
    // frází nic neznamená. Krátký text + známá fráze = skoro jistě ruch.
    if text.chars().count() > 80 {
        return false;
    }
    let t = text.to_lowercase();
    // Samostatné „konec" je klasická česká halucinace na dech/ruch
    // (živě 6× během dne, conf 0.45–0.78; práh 0.85 s rezervou).
    if conf < 0.85 && t.trim_matches(|c: char| c.is_ascii_punctuation() || c == ' ') == "konec" {
        return true;
    }
    conf < 0.92 && HALLUCINATIONS.iter().any(|h| t.contains(h))
}

impl Stt {
    pub fn load(model_path: &Path, language: &str, threads_cfg: usize, hint: &str) -> Result<Self> {
        // logy whisper.cpp/ggml → tracing (jinak špiní stderr při každém load)
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

    /// Přepíše promluvu (PCM 16 kHz mono). None = nebyla v ní řeč.
    pub fn transcribe(&mut self, samples: &[i16]) -> Result<Option<Transcript>> {
        // whisper si na vstupu < ~1 s stěžuje — doplnit tichem
        const MIN_SAMPLES: usize = SAMPLE_RATE + SAMPLE_RATE / 5;
        let mut audio: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32768.0).collect();
        if audio.len() < MIN_SAMPLES {
            audio.resize(MIN_SAMPLES, 0.0);
        }

        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        p.set_language(self.language.as_deref()); // None = autodetekce
        p.set_translate(false);
        p.set_no_context(true); // promluvy jsou nezávislé; brání přenosu halucinací
        if let Some(h) = &self.hint {
            p.set_initial_prompt(h); // slovník: vlastní jména („Jarvisi")
        }
        p.set_suppress_blank(true);
        p.set_suppress_nst(true); // ne-řečové tokeny (♪ …)
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        // Krátká promluva nepotřebuje plný 30s encoder kontext — hlavní úspora
        // CPU (stejný trik používá whisper.cpp stream example). Floor 512:
        // menší kontext způsoboval opakování textu v přepisu.
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
            // klasická kombinace: segment je nejspíš „přepsané ticho"
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

// ---------- stahování modelů ----------

const MODEL_BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Stáhne ggml model (např. "large-v3-turbo-q5_0") do `models_dir`.
/// Atomicky přes .part; existující soubor se nestahuje znovu.
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
        // krátké + známá fráze → halucinace i při vysoké confidence
        // (reálně naměřeno p 0.87 na zvuku klávesnice)
        assert!(is_hallucination("Titulky vytvořil DimSum Team", 0.3));
        assert!(is_hallucination("Titulky vytvořil Jirka Kováč", 0.87));
        assert!(is_hallucination("Děkuji za zhlédnutí!", 0.5));
        assert!(is_hallucination("thanks for watching", 0.2));
        // extrémně jistý přepis se nezahazuje
        assert!(!is_hallucination("Děkuji za zhlédnutí", 0.95));
        // dlouhý text se nezahazuje (reálná řeč o titulcích)
        let long = "Dneska jsem řešil, jak se generují titulky. Titulky vytvořil \
                    nástroj, který jsme ladili celé odpoledne a výsledek je fajn.";
        assert!(!is_hallucination(long, 0.3));
        // běžná česká věta
        assert!(!is_hallucination("pojď se podívat na ten pull request", 0.4));
        // samostatné „konec" = halucinace na ruch; jisté nebo ve větě zůstává
        assert!(is_hallucination("Konec.", 0.75));
        assert!(is_hallucination(" konec ", 0.45));
        assert!(!is_hallucination("Konec.", 0.9));
        assert!(!is_hallucination("a to je konec porady", 0.5));
    }
}
