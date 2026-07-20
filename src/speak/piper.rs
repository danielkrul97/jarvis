//! Local TTS fallback: piper (neural CPU synthesis, voice cs_CZ-jirka-medium).
//! Subprocess pattern like parec/ffplay; nothing leaves the machine and
//! synthesis is free — hence it serves as a fallback when ElevenLabs fails
//! (quota, network, key), or as the sole engine (`speak.engine="piper"`).

use crate::config::{Paths, SpeakCfg};
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const VOICES_BASE_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main";

pub fn model_path(paths: &Paths, s: &SpeakCfg) -> PathBuf {
    paths.models_dir.join(format!("{}.onnx", s.piper_voice))
}

/// "cs_CZ-jirka-medium" → "cs/cs_CZ/jirka/medium" (directory in the voices repo).
fn voice_repo_dir(voice: &str) -> Result<String> {
    let parts: Vec<&str> = voice.split('-').collect();
    if parts.len() < 3 {
        bail!(
            "speak.piper_voice čekám ve tvaru <locale>-<jméno>-<kvalita> \
             (např. cs_CZ-jirka-medium), je '{voice}'"
        );
    }
    let locale = parts[0];
    let quality = parts[parts.len() - 1];
    let name = parts[1..parts.len() - 1].join("-");
    let lang = locale.split('_').next().unwrap_or(locale);
    Ok(format!("{lang}/{locale}/{name}/{quality}"))
}

/// Downloads a voice (.onnx + .onnx.json) from rhasspy/piper-voices into models_dir.
pub fn download_voice(paths: &Paths, s: &SpeakCfg) -> Result<PathBuf> {
    let dir = voice_repo_dir(&s.piper_voice)?;
    for ext in [".onnx", ".onnx.json"] {
        let file = format!("{}{ext}", s.piper_voice);
        util::download(
            &format!("{VOICES_BASE_URL}/{dir}/{file}"),
            &paths.models_dir.join(&file),
        )?;
    }
    Ok(model_path(paths, s))
}

/// Synthesizes text into WAV file `out`. Text is collapsed to a single line —
/// piper treats each stdin line as a separate utterance.
pub fn synthesize(paths: &Paths, s: &SpeakCfg, text: &str, out: &Path) -> Result<()> {
    let model = model_path(paths, s);
    if !model.exists() {
        bail!(
            "piper hlas {} chybí — stáhni ho: `jarvis say --download-model`",
            model.display()
        );
    }
    let config = paths.models_dir.join(format!("{}.onnx.json", s.piper_voice));
    // ElevenLabs speed (higher = faster) ↔ piper length-scale (higher = slower)
    let length_scale = 1.0 / s.speed;
    let mut child = Command::new(&s.piper_bin)
        .arg("-m")
        .arg(&model)
        .arg("-c")
        .arg(&config)
        .arg("-f")
        .arg(out)
        .args(["--length-scale", &format!("{length_scale:.3}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "piper ('{}') není nainstalovaný — `pip3 install --user piper-tts`",
                    s.piper_bin
                )
            } else {
                anyhow::Error::from(e).context(format!("piper ('{}') nejde spustit", s.piper_bin))
            }
        })?;
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    child
        .stdin
        .take()
        .context("piper bez stdin")?
        .write_all(line.as_bytes())
        .context("zápis textu do piperu selhal")?;
    // drop stdin (end of the statement above) = EOF → piper synthesizes and exits
    let res = child.wait_with_output().context("čekání na piper selhalo")?;
    if !res.status.success() {
        let err = String::from_utf8_lossy(&res.stderr);
        bail!("piper skončil s {}: {}", res.status, util::truncate_chars(err.trim(), 300));
    }
    if !out.exists() {
        bail!("piper nevytvořil {}", out.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_repo_dir_shapes() {
        assert_eq!(voice_repo_dir("cs_CZ-jirka-medium").unwrap(), "cs/cs_CZ/jirka/medium");
        assert_eq!(voice_repo_dir("en_US-lessac-high").unwrap(), "en/en_US/lessac/high");
        // multi-word voice name stays together
        assert_eq!(voice_repo_dir("en_US-hfc-male-medium").unwrap(), "en/en_US/hfc-male/medium");
        assert!(voice_repo_dir("cs_CZ").is_err());
        assert!(voice_repo_dir("").is_err());
    }
}
