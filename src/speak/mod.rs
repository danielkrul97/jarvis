//! Hlas Jarvise: ElevenLabs TTS s lokálním piper fallbackem + cache + přehrání.
//!
//! Tok: text → engine dle configu ("auto" = ElevenLabs, při chybě piper) →
//! cache (klíč FNV-1a z textu, hlasu a nastavení; mp3 z API, wav z piperu)
//! → ~/.local/share/jarvis/tts-cache/ → přehrávač (subprocess, jako parec
//! u poslechu). Kredity: stejná věta se generuje jednou, spotřeba znaků se
//! eviduje v `costs`; piper je zdarma a nic neposílá ven.

pub mod piper;
pub mod tts;

use crate::config::{self, Config, Paths, SpeakCfg};
use crate::store::db;
use crate::util;
use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::{debug, info, warn};

/// Řekne text nahlas. Cache-hit nevolá API (0 kreditů).
pub fn say(
    paths: &Paths,
    cfg: &Config,
    text: &str,
    voice_override: Option<&str>,
    use_cache: bool,
    force_local: bool,
) -> Result<()> {
    let audio = synth(paths, cfg, text, voice_override, use_cache, force_local)?;
    play(&cfg.speak, &audio)
}

/// Vygeneruje (nebo z cache vytáhne) audio a vrátí cestu k souboru v cache
/// (mp3 z ElevenLabs, wav z piperu).
pub fn synth(
    paths: &Paths,
    cfg: &Config,
    text: &str,
    voice_override: Option<&str>,
    use_cache: bool,
    force_local: bool,
) -> Result<PathBuf> {
    synth_impl(paths, cfg, text, voice_override, use_cache, force_local, None)
}

/// Jádro `synth`; `cost_conn` = existující DB spojení pro evidenci TTS
/// spotřeby. Streamovaná řeč sdílí jedno spojení pro celou odpověď místo
/// otevírání nového na každou větu (viz `say_once_on`). None = otevřít vlastní.
fn synth_impl(
    paths: &Paths,
    cfg: &Config,
    text: &str,
    voice_override: Option<&str>,
    use_cache: bool,
    force_local: bool,
    cost_conn: Option<&Connection>,
) -> Result<PathBuf> {
    let s = &cfg.speak;
    if !s.enabled {
        bail!("hlas je vypnutý v configu ([speak] enabled = false)");
    }
    let text = text.trim();
    if text.is_empty() {
        bail!("prázdný text — není co říct");
    }
    let chars = text.chars().count();
    if chars > s.max_chars {
        bail!(
            "text má {chars} znaků, strop speak.max_chars je {} (1 znak = 1 kredit ElevenLabs)",
            s.max_chars
        );
    }
    // explicitní --voice je záměr slyšet konkrétní ElevenLabs hlas —
    // tichá záměna za piper by mátla (A/B testy hlasů)
    let engine = if force_local {
        "piper"
    } else if voice_override.is_some() {
        "elevenlabs"
    } else {
        s.engine.as_str()
    };
    match engine {
        "piper" => synth_piper(paths, s, text, use_cache),
        "elevenlabs" => synth_elevenlabs(paths, cfg, text, voice_override, use_cache, chars, cost_conn),
        _ => synth_elevenlabs(paths, cfg, text, voice_override, use_cache, chars, cost_conn).or_else(|e| {
            warn!("ElevenLabs selhal — přepínám na lokální piper: {e:#}");
            synth_piper(paths, s, text, use_cache)
        }),
    }
}

fn synth_elevenlabs(
    paths: &Paths,
    cfg: &Config,
    text: &str,
    voice_override: Option<&str>,
    use_cache: bool,
    chars: usize,
    cost_conn: Option<&Connection>,
) -> Result<PathBuf> {
    let s = &cfg.speak;
    let voice = voice_override.unwrap_or(&s.voice_id);
    let path = paths.tts_cache_dir.join(format!("{:016x}.mp3", cache_key(s, voice, text)));
    if use_cache && s.cache && path.exists() {
        debug!("TTS cache hit: {}", path.display());
        return Ok(path);
    }

    let key = config::elevenlabs_key(paths)?;
    let t0 = std::time::Instant::now();
    let audio = tts::synthesize(&key, s, voice, text)?;
    info!(
        "TTS: {chars} znaků → {} za {:.1} s",
        util::human_bytes(audio.len() as u64),
        t0.elapsed().as_secs_f32()
    );
    // atomicky přes .part — nedokončený soubor nesmí otrávit cache
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &audio).with_context(|| format!("nelze zapsat {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("nelze přejmenovat {} → {}", tmp.display(), path.display()))?;
    // evidence spotřeby: kredit = znak; cena v USD závisí na tarifu → 0.0.
    // `cost_conn` recykluje spojení volajícího; None otevře vlastní.
    let now = util::now_ts();
    let record = |c: &Connection| db::insert_cost(c, now, "tts", &s.model_id, chars as i64, 0, 0.0);
    let recorded = match cost_conn {
        Some(c) => record(c),
        None => db::open(&paths.db_path).and_then(|c| record(&c)),
    };
    if let Err(e) = recorded {
        warn!("evidence TTS spotřeby selhala: {e:#}");
    }
    Ok(path)
}

/// Lokální syntéza piperem; stejné cache schéma jako ElevenLabs (wav).
fn synth_piper(paths: &Paths, s: &SpeakCfg, text: &str, use_cache: bool) -> Result<PathBuf> {
    let path = paths.tts_cache_dir.join(format!("{:016x}.wav", piper_cache_key(s, text)));
    if use_cache && s.cache && path.exists() {
        debug!("TTS cache hit (piper): {}", path.display());
        return Ok(path);
    }
    let t0 = std::time::Instant::now();
    let tmp = path.with_extension("part");
    piper::synthesize(paths, s, text, &tmp)?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("nelze přejmenovat {} → {}", tmp.display(), path.display()))?;
    info!(
        "piper TTS: {} znaků za {:.1} s",
        text.chars().count(),
        t0.elapsed().as_secs_f32()
    );
    Ok(path)
}

/// Řekne jednorázový text: bez cache lookup a soubor se po přehrání smaže —
/// konverzační odpovědi se neopakují, cache by jimi jen rostla.
pub fn say_once(paths: &Paths, cfg: &Config, text: &str) -> Result<()> {
    let audio = synth(paths, cfg, text, None, false, false)?;
    let res = play(&cfg.speak, &audio);
    let _ = std::fs::remove_file(&audio);
    res
}

/// Jako `say_once`, ale evidenci TTS spotřeby zapíše přes DODANÉ spojení —
/// streamované přehrávací vlákno tak drží jedno DB spojení pro celou odpověď
/// místo otevírání nového na každou větu.
pub fn say_once_on(paths: &Paths, cfg: &Config, conn: &Connection, text: &str) -> Result<()> {
    let audio = synth_impl(paths, cfg, text, None, false, false, Some(conn))?;
    let res = play(&cfg.speak, &audio);
    let _ = std::fs::remove_file(&audio);
    res
}

/// Jedna sdílená fráze pro obě doručovací cesty (systemd timer i `jarvis
/// run`) — stejný text = jedna položka v cache = kredity jen jednou.
pub const DIGEST_ANNOUNCEMENT: &str =
    "Dobrý večer, pane. Denní přehled je hotov a právě odletěl do vaší e-mailové schránky.";

/// Ohláška z démona (digest apod.): chyba hlas nesmí položit smyčku,
/// jen se zaloguje.
pub fn announce(paths: &Paths, cfg: &Config, text: &str) {
    if !cfg.speak.enabled || !cfg.speak.announce_digest {
        return;
    }
    if let Err(e) = say(paths, cfg, text, None, true, false) {
        warn!("hlasová ohláška selhala: {e:#}");
    }
}

/// Existuje PulseAudio sink daného jména? (pactl chybí/selže → false)
pub fn sink_available(name: &str) -> bool {
    Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.split('\t').nth(1) == Some(name))
        })
        .unwrap_or(false)
}

/// Cílový sink pro přehrávání: nakonfigurovaný a existující, jinak výchozí.
/// PULSE_SINK na neexistující sink tvrdě selže (ověřeno) — proto kontrola.
fn resolve_sink(s: &SpeakCfg) -> Option<&str> {
    if s.sink.is_empty() {
        return None;
    }
    if sink_available(&s.sink) {
        Some(&s.sink)
    } else {
        warn!(
            "speak.sink '{}' v PulseAudio neexistuje — hraju na výchozí výstup \
             (AEC bez reference, Jarvis se může slyšet)",
            s.sink
        );
        None
    }
}

/// Přehraje audio soubor. Prázdný `s.player` = auto-detekce
/// (ffplay → mpv → ffmpeg+paplay); jinak "binárka argumenty…" + cesta.
/// `s.sink` směruje řeč přes PULSE_SINK (echo-cancel far-end).
pub fn play(s: &SpeakCfg, path: &Path) -> Result<()> {
    let sink = resolve_sink(s);
    let cmd = |bin: &str| {
        let mut c = Command::new(bin);
        if let Some(v) = sink {
            c.env("PULSE_SINK", v);
        }
        c
    };
    let player_cfg = s.player.trim();
    if !player_cfg.is_empty() {
        let mut it = player_cfg.split_whitespace();
        let bin = it.next().unwrap();
        let st = cmd(bin)
            .args(it)
            .arg(path)
            .status()
            .with_context(|| format!("přehrávač '{bin}' nejde spustit"))?;
        if !st.success() {
            bail!("přehrávač '{bin}' skončil s {st}");
        }
        return Ok(());
    }
    let candidates: [(&str, &[&str]); 2] = [
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "error"]),
        ("mpv", &["--no-video", "--really-quiet"]),
    ];
    for (bin, args) in candidates {
        match cmd(bin).args(args).arg(path).status() {
            Ok(st) if st.success() => return Ok(()),
            Ok(st) => bail!("{bin} skončil s {st}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(anyhow::Error::from(e).context(format!("{bin} nejde spustit"))),
        }
    }
    play_via_ffmpeg_paplay(path, sink)
}

/// Poslední záchrana: ffmpeg dekóduje mp3 na raw PCM, paplay ho pošle
/// do PulseAudio (stejná rodina nástrojů jako parec u poslechu).
fn play_via_ffmpeg_paplay(path: &Path, sink: Option<&str>) -> Result<()> {
    let spawn_err = |e: std::io::Error, what: &str| -> anyhow::Error {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "žádný přehrávač: nenalezen ffplay, mpv ani {what} — nainstaluj ffmpeg, \
                 mpv nebo nastav speak.player"
            )
        } else {
            anyhow::Error::from(e).context(format!("{what} nejde spustit"))
        }
    };
    let mut dec = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "s16le", "-ar", "44100", "-ac", "2", "-"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_err(e, "ffmpeg"))?;
    let dec_out = dec.stdout.take().context("ffmpeg bez stdout")?;
    let mut pa_cmd = Command::new("paplay");
    if let Some(v) = sink {
        pa_cmd.env("PULSE_SINK", v);
    }
    let pa = pa_cmd
        .args(["--raw", "--format=s16le", "--rate=44100", "--channels=2"])
        .stdin(Stdio::from(dec_out))
        .status();
    let dec_st = dec.wait().context("čekání na ffmpeg selhalo")?;
    let pa_st = pa.map_err(|e| spawn_err(e, "paplay"))?;
    if !dec_st.success() {
        bail!("ffmpeg dekódování skončilo s {dec_st}");
    }
    if !pa_st.success() {
        bail!("paplay skončil s {pa_st}");
    }
    Ok(())
}

/// Pro `doctor`: který přehrávač je k dispozici.
pub fn detect_player(player_cfg: &str) -> Option<String> {
    let have = |bin: &str| Command::new(bin).arg("-version").output().is_ok();
    if !player_cfg.trim().is_empty() {
        let bin = player_cfg.split_whitespace().next().unwrap_or_default();
        return have(bin).then(|| format!("{bin} (z configu)"));
    }
    if have("ffplay") {
        return Some("ffplay".into());
    }
    if have("mpv") {
        return Some("mpv".into());
    }
    if have("ffmpeg") && have("paplay") {
        return Some("ffmpeg + paplay".into());
    }
    None
}

/// Stabilní klíč cache: hlas + model + formát + jazyk + voice_settings + text.
/// FNV-1a 64 — deterministické napříč běhy (DefaultHasher to negarantuje).
fn cache_key(s: &SpeakCfg, voice: &str, text: &str) -> u64 {
    let sig = format!(
        "{voice}\x1f{}\x1f{}\x1f{}\x1f{:.3}\x1f{:.3}\x1f{:.3}\x1f{}\x1f{:.3}\x1f{text}",
        s.model_id,
        s.output_format,
        s.language,
        s.stability,
        s.similarity_boost,
        s.style,
        s.speaker_boost,
        s.speed,
    );
    fnv1a(sig.as_bytes())
}

/// Klíč cache pro piper: engine + hlas + tempo + text. Záměrně jiný prostor
/// než ElevenLabs klíče (a jiná přípona), aby se enginy nikdy nepomíchaly.
fn piper_cache_key(s: &SpeakCfg, text: &str) -> u64 {
    let sig = format!("piper\x1f{}\x1f{:.3}\x1f{text}", s.piper_voice, 1.0 / s.speed);
    fnv1a(sig.as_bytes())
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

    #[test]
    fn fnv1a_known_vectors() {
        // ověřené konstanty FNV-1a 64 (offset basis pro "", referenční "a")
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn cache_key_stable_and_sensitive() {
        let s = SpeakCfg::default();
        let a = cache_key(&s, "voiceA", "Dobrý večer, pane.");
        assert_eq!(a, cache_key(&s, "voiceA", "Dobrý večer, pane."), "klíč musí být deterministický");
        assert_ne!(a, cache_key(&s, "voiceB", "Dobrý večer, pane."), "jiný hlas = jiný klíč");
        assert_ne!(a, cache_key(&s, "voiceA", "Dobrý večer."), "jiný text = jiný klíč");
        let slower = SpeakCfg { speed: 0.8, ..SpeakCfg::default() };
        assert_ne!(a, cache_key(&slower, "voiceA", "Dobrý večer, pane."), "jiné nastavení = jiný klíč");
    }

    #[test]
    fn detect_player_custom_missing_binary() {
        assert!(detect_player("neexistujici-prehravac-xyz --flag").is_none());
    }

    #[test]
    fn piper_and_elevenlabs_keys_never_collide() {
        let s = SpeakCfg::default();
        let text = "Dobrý večer, pane.";
        assert_ne!(piper_cache_key(&s, text), cache_key(&s, &s.voice_id, text));
        // jiné tempo = jiná piper položka (length-scale mění výstup)
        let slower = SpeakCfg { speed: 0.8, ..SpeakCfg::default() };
        assert_ne!(piper_cache_key(&s, text), piper_cache_key(&slower, text));
        assert_eq!(piper_cache_key(&s, text), piper_cache_key(&SpeakCfg::default(), text));
    }
}
