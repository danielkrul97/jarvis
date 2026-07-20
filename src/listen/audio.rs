//! Audio source for listening: subprocess `parec` (native PulseAudio/PipeWire),
//! fallback `arecord` (ALSA via the pulse plugin). Both deliver raw PCM
//! s16le 16 kHz mono on stdout — resampling is done by the audio server,
//! better than we'd do it ourselves. The subprocess pattern is established
//! elsewhere in the project (`claude -p`); a dead source is handled by the
//! restart loop in `listen::run_listen`. Also includes a WAV reader + linear
//! resampler for `--wav` mode and tests.

use crate::listen::vad::{FRAME_MS, FRAME_SAMPLES};
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// A running audio subprocess; killed on drop (otherwise it'd run forever).
pub struct Source {
    child: Child,
    pub name: &'static str,
}

impl Source {
    pub fn stdout(&mut self) -> Result<std::process::ChildStdout> {
        self.child.stdout.take().context("stdout zdroje už byl odebrán")
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawns the first available source. `device` = PulseAudio source / ALSA
/// device; empty = default microphone.
pub fn spawn_source(device: &str) -> Result<Source> {
    // The configured device must exist in PulseAudio. Otherwise
    // `parec --device=X` silently falls back to the DEFAULT source = the
    // physical mic — and for `jarvis meet` (device = ear_sink.monitor) that
    // means Jarvis hears the user directly from the mic instead of the call
    // audio (it would even reply after being muted in Meet). Better to fail;
    // the restart loop retries once the device exists. None = couldn't verify
    // (pactl missing/failed) → proceed as before, so we don't break a plain
    // ALSA setup.
    if !device.is_empty() && pulse_source_exists(device) == Some(false) {
        bail!(
            "audio zdroj '{device}' v PulseAudiu neexistuje — nespouštím parec \
             (zákaz tichého fallbacku na výchozí mikrofon)"
        );
    }
    let mut parec = Command::new("parec");
    parec.args(["--format=s16le", "--rate=16000", "--channels=1", "--latency-msec=100"]);
    if !device.is_empty() {
        parec.arg(format!("--device={device}"));
    }
    match spawn(parec) {
        Ok(child) => return Ok(Source { child, name: "parec" }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::from(e).context("parec nejde spustit")),
    }
    let mut arecord = Command::new("arecord");
    arecord.args(["-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"]);
    if !device.is_empty() {
        arecord.args(["-D", device]);
    }
    match spawn(arecord) {
        Ok(child) => Ok(Source { child, name: "arecord" }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("nenalezen parec ani arecord — nainstaluj pulseaudio-utils nebo alsa-utils")
        }
        Err(e) => Err(anyhow::Error::from(e).context("arecord nejde spustit")),
    }
}

fn spawn(mut cmd: Command) -> std::io::Result<Child> {
    // stderr goes to the journal — PulseAudio errors should be visible
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()
}

/// Does a PulseAudio source with this name exist? `None` = couldn't tell
/// (pactl missing or failed) — the caller treats that as "unverified", not
/// "doesn't exist". `list short` is unlocalized (tab-separated: index, name, …).
fn pulse_source_exists(device: &str) -> Option<bool> {
    let out = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(["list", "short", "sources"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(short_list_has_name(&String::from_utf8_lossy(&out.stdout), device))
}

/// Is the configured PulseAudio source definitely missing? For self-healing in
/// the listening loop: heal only fires on `Some(false)` (source confirmed
/// gone). Empty `device` (default mic) and `None` (couldn't verify — pactl
/// missing) both → `false`, i.e. no heal (same fail-open as `spawn_source`).
pub fn source_missing(device: &str) -> bool {
    !device.is_empty() && pulse_source_exists(device) == Some(false)
}

/// `pactl list short sources` → 2nd column (name) of the source being sought.
/// Tab-separated, unlocalized. Matches the whole name, not a substring.
fn short_list_has_name(list_short: &str, name: &str) -> bool {
    list_short.lines().any(|l| l.split('\t').nth(1) == Some(name))
}

/// Reads one frame (`FRAME_SAMPLES` samples). Ok(None) = end of source.
/// `scratch` must be `FRAME_SAMPLES * 2` bytes (owned by the caller, no allocations).
pub fn read_frame(r: &mut impl Read, scratch: &mut [u8]) -> Result<Option<Vec<i16>>> {
    debug_assert_eq!(scratch.len(), FRAME_SAMPLES * 2);
    match r.read_exact(scratch) {
        Ok(()) => Ok(Some(bytes_to_i16(scratch))),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e).context("čtení audio streamu selhalo"),
    }
}

pub fn bytes_to_i16(bytes: &[u8]) -> Vec<i16> {
    bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

/// Diagnostics (`doctor --live`): reads a few seconds, returns (RMS in dBFS, peak).
pub fn probe_level(device: &str, secs: f32) -> Result<(f32, i16)> {
    let mut src = spawn_source(device)?;
    let mut out = src.stdout()?;
    let mut scratch = vec![0u8; FRAME_SAMPLES * 2];
    let frames = (secs * 1000.0 / FRAME_MS as f32) as usize;
    let (mut sumsq, mut n, mut peak) = (0f64, 0usize, 0i16);
    for _ in 0..frames {
        match read_frame(&mut out, &mut scratch)? {
            None => break,
            Some(frame) => {
                for s in frame {
                    sumsq += f64::from(s) * f64::from(s);
                    peak = peak.max(s.saturating_abs());
                }
                n += FRAME_SAMPLES;
            }
        }
    }
    if n == 0 {
        bail!("audio zdroj ({}) nedodal žádná data", src.name);
    }
    let rms = (sumsq / n as f64).sqrt() / 32768.0;
    Ok((20.0 * (rms.max(1e-9)).log10() as f32, peak))
}

// ---------- WAV (--wav mode and tests) ----------

/// Loads a WAV (PCM16 only) and converts to 16 kHz mono.
pub fn read_wav_mono_16k(path: &Path) -> Result<Vec<i16>> {
    let raw = std::fs::read(path).with_context(|| format!("nelze číst {}", path.display()))?;
    let (samples, channels, rate) = parse_wav(&raw)?;
    let mono = downmix(&samples, channels);
    Ok(resample_linear(&mono, rate, 16_000))
}

/// Minimal RIFF parser: fmt + data, skips other chunks (including odd
/// padding per the RIFF spec).
fn parse_wav(raw: &[u8]) -> Result<(Vec<i16>, u16, u32)> {
    if raw.len() < 12 || &raw[0..4] != b"RIFF" || &raw[8..12] != b"WAVE" {
        bail!("není WAV soubor (chybí RIFF/WAVE hlavička)");
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= raw.len() {
        let id: [u8; 4] = raw[pos..pos + 4].try_into().unwrap();
        let size = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        let end = pos.saturating_add(size).min(raw.len());
        match &id {
            // `pos + 16 <= raw.len()`: a truncated file declaring fmt ≥ 16 B
            // but not actually supplying those bytes would otherwise panic on
            // an OOB slice in u16_at(14)
            b"fmt " if size >= 16 && pos + 16 <= raw.len() => {
                let u16_at = |o: usize| u16::from_le_bytes(raw[pos + o..pos + o + 2].try_into().unwrap());
                let u32_at = |o: usize| u32::from_le_bytes(raw[pos + o..pos + o + 4].try_into().unwrap());
                fmt = Some((u16_at(0), u16_at(2), u32_at(4), u16_at(14)));
            }
            b"data" => data = Some(&raw[pos..end]),
            _ => {}
        }
        pos = end + (size & 1);
    }
    let (format, channels, rate, bits) = fmt.context("WAV bez fmt chunku")?;
    if format != 1 || bits != 16 {
        bail!(
            "podporuji jen PCM16 WAV (tenhle má formát {format}, {bits} bit) — převod: \
             ffmpeg -i vstup -ar 16000 -ac 1 -c:a pcm_s16le vystup.wav"
        );
    }
    if channels == 0 || rate == 0 {
        bail!("WAV s nesmyslným fmt (kanály {channels}, rate {rate})");
    }
    let data = data.context("WAV bez data chunku")?;
    Ok((bytes_to_i16(&data[..data.len() & !1]), channels, rate))
}

/// Averages channels → mono.
pub fn downmix(samples: &[i16], channels: u16) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks_exact(channels as usize)
        .map(|fr| (fr.iter().map(|&s| i32::from(s)).sum::<i32>() / i32::from(channels)) as i16)
        .collect()
}

/// Linear interpolation — good enough for CLI/tests; live listening is
/// resampled by the audio server (speex resampler).
pub fn resample_linear(input: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let out_len = (input.len() as u64 * u64::from(to_rate) / u64::from(from_rate)) as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * f64::from(from_rate) / f64::from(to_rate);
            let idx = pos as usize;
            let frac = pos - idx as f64;
            let a = f64::from(input[idx.min(input.len() - 1)]);
            let b = f64::from(input[(idx + 1).min(input.len() - 1)]);
            (a + (b - a) * frac) as i16
        })
        .collect()
}

/// Wraps PCM16 mono 16 kHz samples into a canonical WAV (RIFF/fmt/data).
/// Scribe (ElevenLabs STT) wants a file, not bare PCM; 44 B header + data.
pub fn encode_wav_mono_16k(samples: &[i16]) -> Vec<u8> {
    const RATE: u32 = 16_000;
    let data_len = samples.len() as u32 * 2;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes()); // size of the rest of the file
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk length
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate = rate × channels × 2 B
    w.extend_from_slice(&2u16.to_le_bytes()); // block align = channels × 2 B
    w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_to_i16_little_endian() {
        assert_eq!(bytes_to_i16(&[0x00, 0x00, 0xFF, 0x7F, 0x00, 0x80]), vec![0, 32767, -32768]);
    }

    #[test]
    fn downmix_averages_channels() {
        assert_eq!(downmix(&[100, 200, -50, 50], 2), vec![150, 0]);
        assert_eq!(downmix(&[7, 8], 1), vec![7, 8]);
    }

    #[test]
    fn resample_preserves_tone() {
        // 440 Hz sine, 1 s @ 44100 → @16000; verify frequency via zero crossings
        let src: Vec<i16> = (0..44_100)
            .map(|i| (10_000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44_100.0).sin()) as i16)
            .collect();
        let out = resample_linear(&src, 44_100, 16_000);
        assert!((out.len() as i64 - 16_000).abs() <= 2, "délka {}", out.len());
        let crossings = out.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        // 440 Hz ≈ 880 zero crossings per second
        assert!((830..=930).contains(&crossings), "průchodů nulou: {crossings}");
    }

    /// Builds WAV bytes: optional foreign chunk with odd length before the
    /// data (tests alignment), then fmt and data.
    fn build_wav(channels: u16, rate: u32, samples: &[i16], odd_chunk: bool) -> Vec<u8> {
        let mut w: Vec<u8> = Vec::new();
        w.extend(b"RIFF");
        w.extend([0u8; 4]); // size filled in at the end
        w.extend(b"WAVE");
        if odd_chunk {
            w.extend(b"LIST");
            w.extend(3u32.to_le_bytes());
            w.extend(b"abc"); // odd length → 1 B padding
            w.push(0);
        }
        w.extend(b"fmt ");
        w.extend(16u32.to_le_bytes());
        w.extend(1u16.to_le_bytes()); // PCM
        w.extend(channels.to_le_bytes());
        w.extend(rate.to_le_bytes());
        w.extend((rate * u32::from(channels) * 2).to_le_bytes()); // byte rate
        w.extend((channels * 2).to_le_bytes()); // block align
        w.extend(16u16.to_le_bytes()); // bits
        w.extend(b"data");
        w.extend((samples.len() as u32 * 2).to_le_bytes());
        for s in samples {
            w.extend(s.to_le_bytes());
        }
        let size = (w.len() - 8) as u32;
        w[4..8].copy_from_slice(&size.to_le_bytes());
        w
    }

    #[test]
    fn parse_wav_stereo_with_odd_chunk() {
        let samples: Vec<i16> = vec![10, 20, 30, 40, 50, 60];
        let bytes = build_wav(2, 44_100, &samples, true);
        let (parsed, ch, rate) = parse_wav(&bytes).unwrap();
        assert_eq!((ch, rate), (2, 44_100));
        assert_eq!(parsed, samples);
    }

    #[test]
    fn parse_wav_rejects_non_pcm() {
        let mut bytes = build_wav(1, 16_000, &[1, 2, 3], false);
        // overwrite audio_format to 3 (IEEE float) — offset: 12 (RIFF hdr) + 8 (fmt hdr)
        bytes[20] = 3;
        let err = parse_wav(&bytes).unwrap_err().to_string();
        assert!(err.contains("PCM16"), "{err}");
    }

    #[test]
    fn parse_wav_rejects_garbage() {
        assert!(parse_wav(b"tohle neni wav ani nahodou").is_err());
    }

    #[test]
    fn parse_wav_rejects_truncated_fmt_without_panic() {
        // fmt chunk promises 16 B, but the file ends right after its header —
        // must not panic on an OOB slice, just fail cleanly
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend(b"RIFF");
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(b"WAVE");
        bytes.extend(b"fmt ");
        bytes.extend(16u32.to_le_bytes()); // declares 16 B, supplies none
        let err = parse_wav(&bytes).unwrap_err().to_string();
        assert!(err.contains("fmt"), "{err}");
    }

    #[test]
    fn encode_wav_roundtrips_through_parser() {
        let samples: Vec<i16> = vec![0, 100, -100, 32767, -32768, 7, -3];
        let bytes = encode_wav_mono_16k(&samples);
        // canonical 44 B header + 2 B/sample
        assert_eq!(bytes.len(), 44 + samples.len() * 2);
        let (parsed, ch, rate) = parse_wav(&bytes).unwrap();
        assert_eq!((ch, rate), (1, 16_000));
        assert_eq!(parsed, samples);
    }

    #[test]
    fn source_name_lookup_is_exact() {
        let sources = "0\talsa_input.usb-HP__Inc_HyperX_SoloCast-00.analog-stereo\tmodule-alsa-card.c\ts16le 2ch 44100Hz\tRUNNING\n\
                       4\tjarvis_ear_sink.monitor\tmodule-null-sink.c\ts16le 2ch\tIDLE";
        assert!(short_list_has_name(sources, "jarvis_ear_sink.monitor"));
        assert!(short_list_has_name(sources, "alsa_input.usb-HP__Inc_HyperX_SoloCast-00.analog-stereo"));
        // missing source (typically ear_sink.monitor after meet crashes) → false → spawn_source bails
        assert!(!short_list_has_name(sources, "jarvis_ear_sink"));
        assert!(!short_list_has_name(sources, "neexistuje"));
        assert!(!short_list_has_name("", "cokoli"));
    }

    #[test]
    fn encode_wav_empty_is_valid_header() {
        let bytes = encode_wav_mono_16k(&[]);
        let (parsed, ch, rate) = parse_wav(&bytes).unwrap();
        assert_eq!((ch, rate), (1, 16_000));
        assert!(parsed.is_empty());
    }
}
