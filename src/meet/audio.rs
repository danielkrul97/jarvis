//! Virtuální PulseAudio zařízení pro `jarvis meet`. Jarvis vystupuje v hovoru
//! jako samostatný účastník s vlastním mikrofonem a reproduktorem — oboje
//! virtuální, aby se jeho zvuk nemíchal s hardwarovým mikrofonem uživatele:
//!
//! ```text
//! Jarvis TTS ─paplay─► mic_sink (null) ─.monitor─► remap ► mic_source ─► Chrome getUserMedia (uplink)
//! Meet downlink ─► Chrome output ─► ear_sink (null) ─.monitor─► parec ► whisper STT
//! ```
//!
//! Chrome se na tato zařízení naváže přes `PULSE_SINK`/`PULSE_SOURCE` env
//! (ověřeno: samotné env stačí, ruční `move-*` není potřeba).
//!
//! POZOR: `pactl list` (dlouhá forma) je lokalizovaná (české popisky) — proto
//! všechny dotazy jedou s `LC_ALL=C` a přes `list short` (tab-separated,
//! ne-lokalizované). Zařízení jsou efemérní: vytvoří se při vstupu do hovoru
//! a při odchodu (i přes `Drop`) se zase odstraní.

use anyhow::{bail, Context, Result};
use std::process::Command;
use tracing::{debug, info, warn};

/// Sada virtuálních zařízení držená po dobu hovoru. `Drop` je uklidí.
pub struct VirtualAudio {
    mic_sink: String,
    mic_source: String,
    ear_sink: String,
    torn_down: bool,
}

impl VirtualAudio {
    /// Vytvoří (idempotentně) dva null-sinky + remap-source. Případné staré
    /// moduly se stejnými názvy se nejdřív odstraní, pak se po loadu ověří,
    /// že zařízení reálně existují (self-check — load-module může tiše selhat).
    pub fn ensure(mic_sink: &str, mic_source: &str, ear_sink: &str) -> Result<Self> {
        let mut va = Self {
            mic_sink: mic_sink.to_string(),
            mic_source: mic_source.to_string(),
            ear_sink: ear_sink.to_string(),
            torn_down: true, // dokud se úspěšně nenačte, nemáme co uklízet
        };
        // 1) úklid případných reziduí (po pádu předchozího běhu)
        va.unload_all();

        // 2) load: null-sinky nejdřív, remap-source (závisí na mic_sink.monitor) až potom
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

        // 3) read-back: zařízení musí reálně existovat
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

    /// PulseAudio source, který poslouchá STT (zvuk hovoru = ostatní účastníci).
    pub fn ear_monitor(&self) -> String {
        format!("{}.monitor", self.ear_sink)
    }
    /// PulseAudio sink, do kterého se přehrává Jarvisova řeč (→ mikrofon hovoru).
    pub fn mic_sink(&self) -> &str {
        &self.mic_sink
    }
    /// PulseAudio source, který Chrome vybere jako mikrofon.
    pub fn mic_source(&self) -> &str {
        &self.mic_source
    }
    /// PulseAudio sink, kam Chrome hraje zvuk hovoru (PULSE_SINK pro Chrome).
    pub fn ear_sink(&self) -> &str {
        &self.ear_sink
    }

    /// Odstraní všechna zařízení. Idempotentní — po prvním volání už nic nedělá.
    pub fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.unload_all();
        self.torn_down = true;
        info!("virtuální audio odstraněno");
    }

    /// Odstraní moduly vlastnící naše zařízení (bez ohledu na `torn_down`).
    /// Pořadí: nejdřív remap-source (závisí na mic_sink.monitor), pak null-sinky.
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

/// Spustí `pactl` s `LC_ALL=C` a vrátí stdout (chyba na nenulový exit).
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

/// Indexy modulů, jejichž argumenty obsahují daný token (např.
/// `sink_name=jarvis_mic_sink`). Token se porovnává celý, ne jako podřetězec —
/// jinak by `source_name=jarvis_mic` chytlo i `…=jarvis_mic_sink`.
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

/// `pactl list short {sinks,sources}` → sloupec 1 (index), sloupec 2 (název).
fn short_list_has_name(list_short: &str, name: &str) -> bool {
    list_short.lines().any(|line| {
        let mut cols = line.split('\t');
        cols.next(); // index
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
        // sink_name=jarvis_mic_sink smí trefit jen null-sink (řádek 33),
        // NE remap-source, který má jarvis_mic_sink jen v master=…monitor
        assert_eq!(parse_module_indices(MODULES, "sink_name=jarvis_mic_sink"), vec![33]);
        assert_eq!(parse_module_indices(MODULES, "sink_name=jarvis_ear_sink"), vec![34]);
        // source_name=jarvis_mic (substring jarvis_mic_sink) trefí JEN remap (35)
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
        // idempotence: druhé ensure nesmí spadnout ani duplikovat
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
        // nesmí matchovat podřetězec názvu
        assert!(!short_list_has_name(sinks, "jarvis_mic"));
    }
}
