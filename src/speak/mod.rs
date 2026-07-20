//! Jarvis's voice: ElevenLabs TTS with local piper fallback + cache + playback.
//!
//! Flow: text → engine per config ("auto" = ElevenLabs, piper on failure) →
//! cache (FNV-1a key from text, voice, settings; mp3 from API, wav from piper)
//! → ~/.local/share/jarvis/tts-cache/ → player (subprocess, like parec for
//! listening). Credits: the same sentence is generated once; character usage
//! is recorded in `costs`. piper is free and sends nothing out.

pub mod piper;
pub mod tts;

use crate::config::{self, Config, Paths, SpeakCfg};
use crate::store::db;
use crate::util;
use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Shared playback control for barge-in. The mic/STT thread can interrupt
/// speech currently playing: `barge_in()` sets "stop", and the playback
/// thread sees it in its poll loop (`play_killable`) within ~40 ms and kills
/// the subprocess. `speaking` = a reply is currently playing (the mic loop
/// watches it for voice-onset). The playback thread kills its OWN child —
/// no `Child` shared across threads (just two atomics).
/// Window (s) for remembering Jarvis's own recent speech, for echo detection.
const SELF_ECHO_WINDOW_S: i64 = 12;
/// Minimum fraction of an utterance's tokens matching a spoken phrase for it to count as an echo.
const SELF_ECHO_TOKEN_RATIO: f32 = 0.6;
/// Minimum fraction of an utterance's length covered by a contiguous common
/// substring with a spoken phrase (for short garbled leaks where tokens don't
/// match exactly: "K službám" → "K službě").
const SELF_ECHO_LCS_RATIO: f32 = 0.7;

#[derive(Debug, Default)]
pub struct SpeechControl {
    speaking: AtomicBool,
    interrupt: AtomicBool,
    /// What Jarvis recently said (unix ts, normalized tokens): acks, fillers,
    /// and reply sentences. Used to detect Jarvis's own speech leaking back
    /// into the mic through imperfect AEC (self-echo). Shared between the
    /// speech thread (writes) and the STT thread (reads via `is_self_echo`).
    recent: Mutex<VecDeque<(i64, Vec<String>)>>,
}

impl SpeechControl {
    /// Start of a new reply: clears any previous interrupt and marks speaking.
    pub fn begin(&self) {
        self.interrupt.store(false, Ordering::SeqCst);
        self.speaking.store(true, Ordering::SeqCst);
    }
    /// End of a reply (finished naturally / interrupted).
    pub fn end(&self) {
        self.speaking.store(false, Ordering::SeqCst);
    }
    /// Requests immediate interruption of speech currently playing.
    pub fn barge_in(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
    }
    pub fn is_speaking(&self) -> bool {
        self.speaking.load(Ordering::SeqCst)
    }
    pub fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }

    /// Records a just-spoken phrase (ack, filler, reply sentence) into the
    /// self-speech window. Called from the speech thread right before synthesis/playback.
    pub fn record_spoken(&self, text: &str) {
        let toks = echo_tokens(text);
        if toks.is_empty() {
            return;
        }
        let now = util::now_ts();
        if let Ok(mut g) = self.recent.lock() {
            g.push_back((now, toks));
            while g.front().is_some_and(|&(ts, _)| now - ts > SELF_ECHO_WINDOW_S) {
                g.pop_front();
            }
            while g.len() > 64 {
                g.pop_front(); // guard against unbounded growth
            }
        }
    }

    /// Is the transcript `text` (at time `at`) an echo of Jarvis's own recent
    /// speech? AEC can leak garbled text, so this is fuzzy: (1) token overlap
    /// for longer leaks, (2) longest common substring for short garbled
    /// phrases. Short utterances (< 2 tokens, < 6 chars) are never flagged,
    /// so a user's "ano"/"jo" never passes as an echo. Wake-word addressing
    /// is filtered separately by the caller (Jarvis never says its own name in a reply).
    pub fn is_self_echo(&self, text: &str, at: i64) -> bool {
        let toks = echo_tokens(text);
        if toks.is_empty() {
            return false;
        }
        let joined: String = toks.concat();
        let joined_len = joined.chars().count();
        let g = match self.recent.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        for (ts, spoken) in g.iter() {
            if (at - ts).abs() > SELF_ECHO_WINDOW_S {
                continue;
            }
            // 1) token overlap (>= 2 matching tokens, so short words don't match by accident)
            let matched = toks.iter().filter(|&t| spoken.contains(t)).count();
            if matched >= 2 && matched as f32 / toks.len() as f32 >= SELF_ECHO_TOKEN_RATIO {
                return true;
            }
            // 2) longest common substring (short garbled phrases)
            if joined_len >= 6 {
                let sp: String = spoken.concat();
                let lcs = longest_common_substring_len(&joined, &sp);
                if lcs as f32 / joined_len as f32 >= SELF_ECHO_LCS_RATIO {
                    return true;
                }
            }
        }
        false
    }
}

/// Tokenization for echo comparison: lowercase, Czech diacritics folded to
/// ASCII, anything outside [a-z0-9] is a separator (spaces, punctuation).
fn echo_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        let f = fold_echo(c);
        if f.is_ascii_alphanumeric() {
            cur.push(f);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Folds Czech diacritics to ASCII (á→a, č→c, …) for echo comparison.
fn fold_echo(c: char) -> char {
    match c {
        'á' => 'a',
        'č' => 'c',
        'ď' => 'd',
        'é' | 'ě' => 'e',
        'í' => 'i',
        'ň' => 'n',
        'ó' => 'o',
        'ř' => 'r',
        'š' => 's',
        'ť' => 't',
        'ú' | 'ů' => 'u',
        'ý' => 'y',
        'ž' => 'z',
        _ => c,
    }
}

/// Length of the longest contiguous common character substring of two
/// strings (DP, O(n·m); utterances and phrases are short). Used to detect garbled echoes.
fn longest_common_substring_len(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut best = 0;
    for &ca in &a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, &cb) in b.iter().enumerate() {
            if ca == cb {
                cur[j + 1] = prev[j] + 1;
                best = best.max(cur[j + 1]);
            }
        }
        prev = cur;
    }
    best
}

/// Speaks text aloud. A cache hit skips the API call (0 credits).
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

/// Generates (or pulls from cache) audio and returns the path to the cached
/// file (mp3 from ElevenLabs, wav from piper).
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

/// Core of `synth`; `cost_conn` = an existing DB connection for recording TTS
/// usage. Streamed speech shares one connection for the whole reply instead
/// of opening a new one per sentence (see `say_streamed`). None = open its own.
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
    // explicit --voice means the user wants to hear a specific ElevenLabs
    // voice — silently swapping to piper would be confusing (voice A/B tests)
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
    // atomic via .part — an unfinished file must not poison the cache
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &audio).with_context(|| format!("nelze zapsat {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("nelze přejmenovat {} → {}", tmp.display(), path.display()))?;
    // usage recording: credit = char; USD price depends on plan → 0.0.
    // `cost_conn` reuses the caller's connection; None opens its own.
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

/// Local synthesis via piper; same cache scheme as ElevenLabs (wav).
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

/// Speaks one-off text: no cache lookup, and the file is deleted after
/// playback — conversational replies don't repeat, so caching them would just bloat the cache.
pub fn say_once(paths: &Paths, cfg: &Config, text: &str) -> Result<()> {
    let audio = synth(paths, cfg, text, None, false, false)?;
    let res = play(&cfg.speak, &audio);
    let _ = std::fs::remove_file(&audio);
    res
}

/// Audio source for the killable player: a finished file, or a stream of mp3
/// bytes on stdin (streamed reply).
enum PlayInput<'a> {
    File(&'a Path),
    Stream(Box<dyn Read + Send>),
}

/// Is the binary in PATH? (`bin -version`; NotFound → no). Matches `detect_player`.
fn binary_exists(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok()
}

/// Player able to read mp3 from stdin (ffplay > mpv), probed ONCE — the
/// `-version` probe is a fork+exec and must not sit on the per-sentence hot
/// path. None = no player found (streaming then falls back to buffer + ffmpeg/paplay).
fn stream_player() -> Option<&'static str> {
    static PLAYER: OnceLock<Option<&'static str>> = OnceLock::new();
    *PLAYER.get_or_init(|| {
        if binary_exists("ffplay") {
            Some("ffplay")
        } else if binary_exists("mpv") {
            Some("mpv")
        } else {
            None
        }
    })
}

/// Can this config stream? Only the ElevenLabs engine, auto-player (a custom
/// `player` is honored → buffer instead), and ffplay/mpv present (stdin-capable).
fn streaming_possible(s: &SpeakCfg) -> bool {
    matches!(s.engine.as_str(), "auto" | "elevenlabs")
        && s.player.trim().is_empty()
        && stream_player().is_some()
}

/// Speech ready to play. Separates synthesis/stream-opening from actual
/// playback, so the SpeechPlayer can prepare the NEXT sentence while the
/// current one plays (otherwise there's silence between sentences equal to
/// the next sentence's TTS latency).
pub enum Prepared {
    /// Finished file; `temp` = delete after playback (one-off reply).
    File { path: PathBuf, temp: bool },
    /// Open mp3 stream (ElevenLabs) — piped into the player as it arrives.
    Stream(Box<dyn Read + Send>),
}

/// Prepares speech: an ack is synthesized/cached to a file, a one-off reply
/// is streamed (ElevenLabs), otherwise via a temp file. Does NOT play — that's
/// `play_prepared`'s job, so the next sentence's prep can overlap with playback.
pub fn prepare_speech(
    paths: &Paths,
    cfg: &Config,
    cost_conn: Option<&Connection>,
    text: &str,
    cached: bool,
) -> Result<Prepared> {
    let s = &cfg.speak;
    if !s.enabled {
        bail!("hlas je vypnutý v configu ([speak] enabled = false)");
    }
    // ack: kept in cache, played from file
    if cached {
        let path = synth(paths, cfg, text, None, true, false)?;
        return Ok(Prepared::File { path, temp: false });
    }
    // one-off reply: try streaming first
    if s.stream && streaming_possible(s) {
        match stream_answer(paths, cfg, text, cost_conn) {
            Ok(Some(reader)) => return Ok(Prepared::Stream(reader)),
            Ok(None) => {} // engine isn't ElevenLabs → buffer below
            Err(e) => warn!("streaming TTS selhal — buffer/piper: {e:#}"),
        }
    }
    // buffer: one-off synthesis to a temp file
    let path = synth_impl(paths, cfg, text, None, false, false, cost_conn)?;
    Ok(Prepared::File { path, temp: true })
}

/// Plays prepared speech KILLABLY (barge-in via `control`). Deletes the temp
/// file after playback. Interruption mid-playback is not an error.
pub fn play_prepared(s: &SpeakCfg, prepared: Prepared, control: &SpeechControl) -> Result<()> {
    match prepared {
        Prepared::Stream(reader) => play_killable(s, PlayInput::Stream(reader), control),
        Prepared::File { path, temp } => {
            let res = play_killable(s, PlayInput::File(&path), control);
            if temp {
                let _ = std::fs::remove_file(&path);
            }
            res
        }
    }
}

/// Opens an ElevenLabs stream for a one-off reply and records usage (credit =
/// char, same as `synth_elevenlabs`). Returns a reader for the audio stream,
/// or None if the engine isn't ElevenLabs (→ buffer/piper). Error = fall back to buffer.
fn stream_answer(
    paths: &Paths,
    cfg: &Config,
    text: &str,
    cost_conn: Option<&Connection>,
) -> Result<Option<Box<dyn Read + Send>>> {
    let s = &cfg.speak;
    if !matches!(s.engine.as_str(), "auto" | "elevenlabs") {
        return Ok(None);
    }
    let text = text.trim();
    if text.is_empty() {
        bail!("prázdný text — není co říct");
    }
    let chars = text.chars().count();
    if chars > s.max_chars {
        bail!("text má {chars} znaků, strop speak.max_chars je {}", s.max_chars);
    }
    let key = config::elevenlabs_key(paths)?;
    let reader = tts::synthesize_stream(&key, s, &s.voice_id, text)?;
    let now = util::now_ts();
    let record = |c: &Connection| db::insert_cost(c, now, "tts", &s.model_id, chars as i64, 0, 0.0);
    let recorded = match cost_conn {
        Some(c) => record(c),
        None => db::open(&paths.db_path).and_then(|c| record(&c)),
    };
    if let Err(e) = recorded {
        warn!("evidence TTS spotřeby (stream) selhala: {e:#}");
    }
    Ok(Some(reader))
}

/// Plays audio killably: the mic/STT thread can interrupt playback via
/// `control` (barge-in). The player runs as a subprocess; this thread kills
/// its OWN child within ~40 ms of `interrupt` being set. `File` plays from a
/// file (custom/ffplay/mpv, else ffmpeg+paplay — this last resort can't be
/// interrupted), `Stream` pipes mp3 to stdin (ffplay/mpv). Interruption → Ok(()).
fn play_killable(s: &SpeakCfg, input: PlayInput, control: &SpeechControl) -> Result<()> {
    if control.interrupted() {
        return Ok(()); // barge-in arrived before we even started
    }
    let sink = resolve_sink(s);
    let with_sink = |bin: &str| {
        let mut c = Command::new(bin);
        if let Some(v) = sink {
            c.env("PULSE_SINK", v);
        }
        c
    };
    // build the player command (ffplay/mpv detection is cached — no fork per sentence)
    let mut cmd = match &input {
        PlayInput::File(path) => {
            let player_cfg = s.player.trim();
            if !player_cfg.is_empty() {
                let mut it = player_cfg.split_whitespace();
                let mut c = with_sink(it.next().unwrap());
                c.args(it).arg(path);
                c
            } else {
                match stream_player() {
                    Some("ffplay") => {
                        let mut c = with_sink("ffplay");
                        c.args(["-nodisp", "-autoexit", "-loglevel", "error"]).arg(path);
                        c
                    }
                    Some("mpv") => {
                        let mut c = with_sink("mpv");
                        c.args(["--no-video", "--really-quiet"]).arg(path);
                        c
                    }
                    // last resort: ffmpeg+paplay (blocking, not interruptible)
                    _ => return play_via_ffmpeg_paplay(path, sink),
                }
            }
        }
        PlayInput::Stream(_) => match stream_player() {
            Some("ffplay") => {
                let mut c = with_sink("ffplay");
                c.args(["-nodisp", "-autoexit", "-loglevel", "error", "-i", "-"]);
                c
            }
            Some("mpv") => {
                let mut c = with_sink("mpv");
                c.args(["--no-video", "--really-quiet", "-"]);
                c
            }
            _ => bail!("streaming: chybí ffplay i mpv pro přehrání ze stdin"),
        },
    };
    if matches!(input, PlayInput::Stream(_)) {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd.spawn().context("přehrávač nejde spustit")?;
    // pipe the stream into the player's stdin; dropping stdin = EOF. We
    // DETACH the thread — after barge-in (kill child) a join could hang on a
    // slow HTTP read; writing to a dead stdin ends it soon enough anyway, and
    // we don't care when it finishes.
    if let PlayInput::Stream(mut reader) = input {
        if let Some(mut si) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut reader, &mut si);
            });
        }
    }
    // poll: finished / interrupt / sleep. Kill is immediate, poll just
    // notices the end — 40 ms stop latency is inaudible. Interruption
    // (barge-in) kills the child and returns right away (not reported as a player error).
    loop {
        if let Some(status) = child.try_wait().context("čekání na přehrávač selhalo")? {
            if !status.success() {
                warn!("přehrávač skončil s {status}");
            }
            break;
        }
        if control.interrupted() {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Ok(())
}

/// One shared phrase for both delivery paths (systemd timer and `jarvis
/// run`) — same text = one cache entry = credits spent only once.
pub const DIGEST_ANNOUNCEMENT: &str =
    "Dobrý večer, pane. Denní přehled je hotov a právě odletěl do vaší e-mailové schránky.";

/// Announcement from the daemon (digest etc.): a voice error must not take
/// down the loop — it's just logged.
pub fn announce(paths: &Paths, cfg: &Config, text: &str) {
    if !cfg.speak.enabled || !cfg.speak.announce_digest {
        return;
    }
    if let Err(e) = say(paths, cfg, text, None, true, false) {
        warn!("hlasová ohláška selhala: {e:#}");
    }
}

/// Does a PulseAudio sink with this name exist? (pactl missing/fails → false)
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

/// Target sink for playback: configured and existing, else the default.
/// PULSE_SINK pointing at a nonexistent sink hard-fails (verified) — hence the check.
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

/// Plays an audio file. Empty `s.player` = auto-detect
/// (ffplay → mpv → ffmpeg+paplay); otherwise "binary args…" + path.
/// `s.sink` routes speech through PULSE_SINK (echo-cancel far-end).
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

/// Last resort: ffmpeg decodes mp3 to raw PCM, paplay sends it to PulseAudio
/// (same tool family as parec for listening).
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

/// For `doctor`: which player is available.
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

/// Stable cache key: voice + model + format + language + voice_settings + text.
/// FNV-1a 64 — deterministic across runs (DefaultHasher doesn't guarantee that).
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

/// Cache key for piper: engine + voice + rate + text. Deliberately a
/// different key space than ElevenLabs keys (and a different extension), so the engines never mix.
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
        // verified FNV-1a 64 constants (offset basis for "", reference for "a")
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
        // different rate = different piper entry (length-scale changes output)
        let slower = SpeakCfg { speed: 0.8, ..SpeakCfg::default() };
        assert_ne!(piper_cache_key(&s, text), piper_cache_key(&slower, text));
        assert_eq!(piper_cache_key(&s, text), piper_cache_key(&SpeakCfg::default(), text));
    }

    // --- self-echo (Jarvis hearing itself through imperfect AEC) ---

    #[test]
    fn self_echo_detects_own_ack() {
        let c = SpeechControl::default();
        let now = util::now_ts();
        c.record_spoken("Ano, pane?");
        assert!(c.is_self_echo("ano pane", now));
    }

    #[test]
    fn self_echo_detects_garbled_leak() {
        // real case from measurement: "Toto je zkušební věta" leaks as "Toto je skušení"
        let c = SpeechControl::default();
        let now = util::now_ts();
        c.record_spoken("Toto je zkušební věta pro měření potlačení ozvěny.");
        assert!(c.is_self_echo("Toto je skušení.", now));
    }

    #[test]
    fn self_echo_detects_short_butler_filler() {
        // from the journal: "K službám, pane." leaked and got transcribed as "K službě"
        let c = SpeechControl::default();
        let now = util::now_ts();
        c.record_spoken("K službám, pane.");
        assert!(c.is_self_echo("K službě", now));
    }

    #[test]
    fn self_echo_real_user_speech_passes() {
        let c = SpeechControl::default();
        let now = util::now_ts();
        c.record_spoken("Ano, pane, zajisté to zařídím.");
        assert!(!c.is_self_echo("jaké bude zítra počasí v Praze", now));
        // a user following up on Jarvis's action must not be swallowed as an echo
        c.record_spoken("Mám otevřít Firefox, pane?");
        assert!(!c.is_self_echo("ano otevři firefox", now));
    }

    #[test]
    fn self_echo_ignores_too_short_utterance() {
        let c = SpeechControl::default();
        let now = util::now_ts();
        c.record_spoken("Ano, pane?");
        // a single-word "ano" (min-words filters it out anyway) must not be flagged
        assert!(!c.is_self_echo("ano", now));
    }

    #[test]
    fn self_echo_expires_outside_window() {
        let c = SpeechControl::default();
        let now = util::now_ts();
        c.record_spoken("K vašim službám, pane.");
        assert!(c.is_self_echo("k vašim službám pane", now));
        assert!(!c.is_self_echo("k vašim službám pane", now + SELF_ECHO_WINDOW_S + 5));
    }

    #[test]
    fn lcs_len_basic() {
        assert_eq!(longest_common_substring_len("ksluzbe", "ksluzbampane"), 6); // "ksluzb"
        assert_eq!(longest_common_substring_len("abc", "xyz"), 0);
    }
}
