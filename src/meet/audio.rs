//! Virtual PulseAudio devices for `jarvis meet`. Jarvis joins the call as a
//! separate participant with its own mic and speaker — both virtual, so its
//! audio doesn't mix with the user's hardware mic:
//!
//! ```text
//! Jarvis TTS ─paplay─► mic_sink (null) ─.monitor─► remap ► mic_source ─► Chrome getUserMedia (uplink)
//! Meet downlink ─► Chrome output ─► ear_sink (null) ─.monitor─► parec ► whisper STT
//! ```
//!
//! Chrome binds to these devices via `PULSE_SINK`/`PULSE_SOURCE` env vars
//! (verified: env alone is enough, no manual `move-*` needed).
//!
//! NOTE: `pactl list` (long form) is localized (Czech labels) — so all
//! queries use `LC_ALL=C` and `list short` (tab-separated, not localized).
//! Devices are ephemeral: created on call entry and removed on exit (even
//! via `Drop`).

use anyhow::{bail, Context, Result};
use std::process::Command;
use tracing::{debug, info, warn};

/// Set of virtual devices held for the call's duration. `Drop` cleans them up.
pub struct VirtualAudio {
    mic_sink: String,
    mic_source: String,
    ear_sink: String,
    torn_down: bool,
}

impl VirtualAudio {
    /// Idempotently creates two null-sinks + a remap-source. Any stale
    /// modules with the same names are removed first; after loading,
    /// verifies the devices actually exist (self-check — load-module can
    /// fail silently).
    pub fn ensure(mic_sink: &str, mic_source: &str, ear_sink: &str) -> Result<Self> {
        let mut va = Self {
            mic_sink: mic_sink.to_string(),
            mic_source: mic_source.to_string(),
            ear_sink: ear_sink.to_string(),
            torn_down: true, // nothing to clean up until load succeeds
        };
        // 1) clean up any leftovers (from a previous crashed run)
        va.unload_all();

        // 2) load: null-sinks first, remap-source (depends on mic_sink.monitor) after
        load_module(&[
            "module-null-sink",
            &format!("sink_name={mic_sink}"),
            "sink_properties=device.description=JarvisMicSink",
        ])
        .context("nelze vytvořit mic_sink (null-sink)")?;
        load_module(&[
            "module-null-sink",
            &format!("sink_name={ear_sink}"),
            "sink_properties=device.description=JarvisEarSink",
        ])
        .context("nelze vytvořit ear_sink (null-sink)")?;
        load_module(&[
            "module-remap-source",
            &format!("source_name={mic_source}"),
            &format!("master={mic_sink}.monitor"),
            "source_properties=device.description=JarvisMic",
        ])
        .context("nelze vytvořit mic_source (remap-source)")?;
        va.torn_down = false;

        // 3) read-back: devices must actually exist
        if !sink_exists(mic_sink)? {
            va.unload_all();
            bail!("mic_sink '{mic_sink}' po load-module neexistuje");
        }
        if !sink_exists(ear_sink)? {
            va.unload_all();
            bail!("ear_sink '{ear_sink}' po load-module neexistuje");
        }
        if !source_exists(mic_source)? {
            va.unload_all();
            bail!("mic_source '{mic_source}' po load-module neexistuje");
        }
        va.torn_down = false;
        info!("virtuální audio připraveno: mic_sink={mic_sink} mic_source={mic_source} ear_sink={ear_sink}");
        Ok(va)
    }

    /// PulseAudio source that STT listens on (call audio = other participants).
    pub fn ear_monitor(&self) -> String {
        format!("{}.monitor", self.ear_sink)
    }
    /// PulseAudio sink Jarvis's speech is played into (→ the call's mic).
    pub fn mic_sink(&self) -> &str {
        &self.mic_sink
    }
    /// PulseAudio source Chrome picks as its microphone.
    pub fn mic_source(&self) -> &str {
        &self.mic_source
    }
    /// PulseAudio sink Chrome plays call audio into (PULSE_SINK for Chrome).
    pub fn ear_sink(&self) -> &str {
        &self.ear_sink
    }

    /// Removes all devices. Idempotent — a no-op after the first call.
    pub fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.unload_all();
        self.torn_down = true;
        info!("virtuální audio odstraněno");
    }

    /// Removes modules owning our devices (regardless of `torn_down`).
    /// Order: remap-source first (depends on mic_sink.monitor), then null-sinks.
    fn unload_all(&self) {
        for needle in [
            format!("source_name={}", self.mic_source),
            format!("sink_name={}", self.mic_sink),
            format!("sink_name={}", self.ear_sink),
        ] {
            match module_indices(&needle) {
                Ok(idxs) => {
                    for idx in idxs {
                        if let Err(e) = unload_module(idx) {
                            warn!("unload-module {idx} ({needle}) selhal: {e:#}");
                        } else {
                            debug!("unload-module {idx} ({needle})");
                        }
                    }
                }
                Err(e) => warn!("výpis modulů pro '{needle}' selhal: {e:#}"),
            }
        }
    }
}

impl Drop for VirtualAudio {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Runs `pactl` with `LC_ALL=C` and returns stdout (errors on nonzero exit).
fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .with_context(|| format!("nelze spustit pactl {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "pactl {} selhalo: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn load_module(module_args: &[&str]) -> Result<()> {
    let mut args = vec!["load-module"];
    args.extend_from_slice(module_args);
    pactl(&args).map(|_| ())
}

fn unload_module(index: u32) -> Result<()> {
    let idx = index.to_string();
    pactl(&["unload-module", &idx]).map(|_| ())
}

/// Indices of modules whose args contain the given token (e.g.
/// `sink_name=jarvis_mic_sink`). The token is matched as a whole, not as a
/// substring — otherwise `source_name=jarvis_mic` would also match
/// `…=jarvis_mic_sink`.
fn module_indices(needle: &str) -> Result<Vec<u32>> {
    let out = pactl(&["list", "short", "modules"])?;
    Ok(parse_module_indices(&out, needle))
}

fn parse_module_indices(list_short: &str, needle: &str) -> Vec<u32> {
    list_short
        .lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let idx: u32 = cols.next()?.trim().parse().ok()?;
            let _name = cols.next()?;
            let args = cols.next().unwrap_or("");
            if args.split_whitespace().any(|tok| tok == needle) {
                Some(idx)
            } else {
                None
            }
        })
        .collect()
}

fn sink_exists(name: &str) -> Result<bool> {
    let out = pactl(&["list", "short", "sinks"])?;
    Ok(short_list_has_name(&out, name))
}

fn source_exists(name: &str) -> Result<bool> {
    let out = pactl(&["list", "short", "sources"])?;
    Ok(short_list_has_name(&out, name))
}

/// `pactl list short {sinks,sources}` → column 1 (index), column 2 (name).
fn short_list_has_name(list_short: &str, name: &str) -> bool {
    list_short.lines().any(|line| {
        let mut cols = line.split('\t');
        cols.next(); // index column
        cols.next().map(str::trim) == Some(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULES: &str = "\
25\tmodule-echo-cancel\taec_method=webrtc source_master=alsa_input.usb source_name=jarvis_denoised sink_name=jarvis_out
33\tmodule-null-sink\tsink_name=jarvis_mic_sink sink_properties=device.description=JarvisMicSink
34\tmodule-null-sink\tsink_name=jarvis_ear_sink sink_properties=device.description=JarvisEarSink
35\tmodule-remap-source\tsource_name=jarvis_mic master=jarvis_mic_sink.monitor source_properties=device.description=JarvisMic
40\tmodule-stream-restore\t";

    #[test]
    fn module_index_matches_exact_token_not_substring() {
        // sink_name=jarvis_mic_sink must match only the null-sink (line 33),
        // NOT the remap-source, which has jarvis_mic_sink only in master=…monitor
        assert_eq!(parse_module_indices(MODULES, "sink_name=jarvis_mic_sink"), vec![33]);
        assert_eq!(parse_module_indices(MODULES, "sink_name=jarvis_ear_sink"), vec![34]);
        // source_name=jarvis_mic (substring jarvis_mic_sink) matches ONLY remap (35)
        assert_eq!(parse_module_indices(MODULES, "source_name=jarvis_mic"), vec![35]);
    }

    #[test]
    fn module_index_empty_when_absent() {
        assert!(parse_module_indices(MODULES, "sink_name=neexistuje").is_empty());
        assert!(parse_module_indices("", "sink_name=x").is_empty());
    }

    #[test]
    #[ignore = "živý pactl: vytvoří a smaže reálná PulseAudio zařízení"]
    fn live_ensure_and_teardown() {
        let mut va =
            VirtualAudio::ensure("jarvis_t_mic_sink", "jarvis_t_mic", "jarvis_t_ear_sink").unwrap();
        assert!(sink_exists("jarvis_t_mic_sink").unwrap(), "mic_sink má existovat");
        assert!(sink_exists("jarvis_t_ear_sink").unwrap(), "ear_sink má existovat");
        assert!(source_exists("jarvis_t_mic").unwrap(), "mic_source má existovat");
        // idempotence: a second ensure must not fail or duplicate
        let va2 =
            VirtualAudio::ensure("jarvis_t_mic_sink", "jarvis_t_mic", "jarvis_t_ear_sink").unwrap();
        assert_eq!(module_indices("sink_name=jarvis_t_mic_sink").unwrap().len(), 1);
        drop(va2);
        va.teardown();
        assert!(!sink_exists("jarvis_t_mic_sink").unwrap(), "mic_sink má být pryč");
        assert!(!sink_exists("jarvis_t_ear_sink").unwrap(), "ear_sink má být pryč");
        assert!(!source_exists("jarvis_t_mic").unwrap(), "mic_source má být pryč");
    }

    #[test]
    fn short_list_name_lookup() {
        let sinks = "0\talsa_output.pci\tmodule-alsa-card.c\ts16le 2ch\tRUNNING\n\
                     3\tjarvis_mic_sink\tmodule-null-sink.c\ts16le 2ch\tIDLE";
        assert!(short_list_has_name(sinks, "jarvis_mic_sink"));
        assert!(short_list_has_name(sinks, "alsa_output.pci"));
        assert!(!short_list_has_name(sinks, "jarvis_ear_sink"));
        // must not match a substring of the name
        assert!(!short_list_has_name(sinks, "jarvis_mic"));
    }
}
