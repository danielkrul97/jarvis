//! Microphone listening: near-realtime speech transcription (STT).
//!
//! Flow: parec/arecord → 30 ms frames → VAD (complete utterances) → whisper
//! (own thread, so VAD keeps up with the realtime stream) → SQLite
//! `utterances`. Audio is NEVER written to disk — only text leaves; on
//! `jarvis pause` audio is dropped straight from RAM and VAD resets.

pub mod audio;
pub mod scribe;
pub mod stt;
pub mod vad;

use crate::config::{self, Config, ListenCfg, Paths};
use crate::converse;
use crate::screen;
use crate::speak;
use crate::store::db;
use crate::util;
use anyhow::{bail, Context, Result};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use vad::{Utterance, Vad, VadConfig, FRAME_SAMPLES, SAMPLE_RATE};

/// Exclusive flock against double listening. `name` = "listen.lock" (ambient
/// mic daemon: systemd × `jarvis run` × manual `jarvis listen`) or "meet.lock"
/// (call). Separate locks on purpose: `jarvis meet` and the mic daemon must
/// NOT contend for one lock (meet used to grab listen.lock and the service
/// restarted 388× in vain) — instead the mic daemon just goes quiet during a
/// call (see meet heartbeat in the main loop). The returned File holds the lock.
fn acquire_lock(paths: &Paths, name: &str) -> Result<std::fs::File> {
    let path = paths.data_dir.join(name);
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("nelze otevřít {}", path.display()))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!(
            "už běží v jiném procesu (zámek {name}) — `systemctl --user status jarvis-listen`, \
             nebo jiný `jarvis listen`/`jarvis run`/`jarvis meet`"
        );
    }
    Ok(f)
}

/// DB key: `jarvis meet` (meet bridge) uses it to signal an active call. The
/// ambient mic daemon goes quiet accordingly, so it doesn't answer the same
/// utterance twice (once from the call via the meet bridge, once directly from
/// the mic). Value = epoch until which the heartbeat is valid; meet renews it
/// ~1x/s. TTL gives self-healing — if meet crashes without cleanup, the
/// heartbeat expires and the mic wakes itself up.
pub(crate) const MEET_ACTIVE_KEY: &str = "meet_active_until";
const MEET_ACTIVE_TTL: i64 = 10;

fn vad_config(cfg: &Config) -> VadConfig {
    VadConfig {
        min_speech_ms: cfg.listen.min_speech_ms,
        silence_ms: cfg.listen.silence_ms,
        max_utterance_ms: cfg.listen.max_utterance_s * 1000,
        speech_mult: cfg.listen.vad_speech_mult,
    }
}

/// After a Scribe error, "auto" stays on local whisper this long before
/// retrying the cloud — otherwise a dead key/network would pay for a failed
/// round-trip (plus the 3 s transport-error retry) on every utterance.
const SCRIBE_COOLDOWN: Duration = Duration::from_secs(60);

/// STT engine for one utterance. Chosen by `listen.engine`.
enum Transcriber {
    /// Local whisper only (loaded eagerly).
    Whisper(stt::Stt),
    /// ElevenLabs Scribe only, no fallback.
    Scribe { key: String, listen: ListenCfg },
    /// Scribe first, whisper as a lazy fallback (loaded only when needed).
    Auto(AutoStt),
}

/// "auto": Scribe with a lazy whisper fallback and an error cooldown.
struct AutoStt {
    key: String,
    listen: ListenCfg,
    /// Path to the whisper model for lazy loading (resolved at construction).
    model_path: PathBuf,
    /// None until the fallback first kicks in — then holds the loaded model.
    whisper: Option<stt::Stt>,
    /// Until when we run locally (Scribe is temporarily down); None = try Scribe.
    scribe_down_until: Option<Instant>,
}

impl AutoStt {
    /// Lazy-loads whisper; the first fallback pays for loading, then it's free.
    fn whisper(&mut self) -> Result<&mut stt::Stt> {
        if self.whisper.is_none() {
            info!("načítám lokální whisper (fallback za Scribe): {}", self.model_path.display());
            let w = stt::Stt::load(
                &self.model_path,
                &self.listen.language,
                self.listen.threads,
                &self.listen.hint,
            )?;
            self.whisper = Some(w);
        }
        Ok(self.whisper.as_mut().expect("whisper právě načten"))
    }

    fn transcribe(&mut self, samples: &[i16]) -> Result<Option<stt::Transcript>> {
        let cooling = self.scribe_down_until.is_some_and(|t| Instant::now() < t);
        if cooling {
            // Scribe is in cooldown → go straight to whisper (the fallback
            // was already verified when we entered cooldown)
            return self.whisper()?.transcribe(samples);
        }
        let scribe_err = match scribe::transcribe(&self.key, &self.listen, samples) {
            Ok(r) => {
                self.scribe_down_until = None; // Scribe is alive again
                return Ok(r);
            }
            Err(e) => e,
        };
        // We try the fallback IMMEDIATELY and only arm the cooldown if whisper
        // actually worked. If cooldown armed blindly (whisper unavailable —
        // model not downloaded, which "auto" only warns about), 60 s would
        // skip Scribe and drop EVERY utterance, even if the cloud recovered
        // meanwhile. Without the cooldown, the next utterance retries Scribe →
        // a transient error costs one utterance, not a minute.
        match self.whisper().and_then(|w| w.transcribe(samples)) {
            Ok(r) => {
                warn!(
                    "Scribe selhal — {} s jedu lokálně na whisperu: {scribe_err:#}",
                    SCRIBE_COOLDOWN.as_secs()
                );
                self.scribe_down_until = Some(Instant::now() + SCRIBE_COOLDOWN);
                Ok(r)
            }
            Err(whisper_err) => {
                warn!(
                    "Scribe i whisper fallback selhaly (bez cooldownu, příště zkusím \
                     Scribe): scribe={scribe_err:#}; whisper={whisper_err:#}"
                );
                Err(scribe_err)
            }
        }
    }
}

impl Transcriber {
    fn transcribe(&mut self, samples: &[i16]) -> Result<Option<stt::Transcript>> {
        match self {
            Transcriber::Whisper(w) => w.transcribe(samples),
            Transcriber::Scribe { key, listen } => scribe::transcribe(key, listen, samples),
            Transcriber::Auto(a) => a.transcribe(samples),
        }
    }
}

/// Loads the local whisper model (eager). Shared by the `whisper` engine and
/// the "auto" fallback.
fn load_whisper(paths: &Paths, l: &ListenCfg) -> Result<stt::Stt> {
    let model = l.resolve_model_path(paths);
    if !model.exists() {
        bail!(
            "whisper model {} neexistuje — stáhni ho: `jarvis listen --download-model`",
            model.display()
        );
    }
    stt::Stt::load(&model, &l.language, l.threads, &l.hint)
}

/// Builds the STT engine per `listen.engine`. "auto" without an ElevenLabs
/// key silently falls back to local whisper.
fn load_transcriber(paths: &Paths, cfg: &Config) -> Result<Transcriber> {
    let l = &cfg.listen;
    match l.engine.as_str() {
        "whisper" => Ok(Transcriber::Whisper(load_whisper(paths, l)?)),
        "elevenlabs" => {
            let key = config::elevenlabs_key(paths).context(
                "listen.engine = \"elevenlabs\" vyžaduje ELEVENLABS_API_KEY v ~/.config/jarvis/secrets.env",
            )?;
            info!("STT: ElevenLabs Scribe ({}), bez lokálního fallbacku", l.scribe_model);
            Ok(Transcriber::Scribe { key, listen: l.clone() })
        }
        // "auto"
        _ => match config::elevenlabs_key(paths) {
            Ok(key) => {
                let model_path = l.resolve_model_path(paths);
                if !model_path.exists() {
                    warn!(
                        "whisper model {} chybí — fallback za Scribe nebude fungovat \
                         (stáhni `jarvis listen --download-model`)",
                        model_path.display()
                    );
                }
                info!("STT: ElevenLabs Scribe ({}) s lokálním whisper fallbackem", l.scribe_model);
                Ok(Transcriber::Auto(AutoStt {
                    key,
                    listen: l.clone(),
                    model_path,
                    whisper: None,
                    scribe_down_until: None,
                }))
            }
            Err(e) => {
                warn!("ELEVENLABS_API_KEY chybí ({e:#}) — STT jede lokálně na whisperu");
                Ok(Transcriber::Whisper(load_whisper(paths, l)?))
            }
        },
    }
}

/// Wake-addressed utterances flow from the STT thread to the conversation worker.
struct ConvoHook {
    wake: converse::WakeWords,
    open_ear: converse::OpenEar,
    /// Shared with the worker: when Jarvis last finished speaking (echo-guard +
    /// follow-up window threshold). Worker writes, STT thread reads here.
    speech_end: Arc<AtomicI64>,
    tx: mpsc::SyncSender<converse::Job>,
    /// Barge-in: the STT thread interrupts Jarvis's speech on a wake address
    /// (Wake) — echo-safe even without AEC. Shared with the worker's playback thread.
    control: Arc<speak::SpeechControl>,
    barge_in: bool,
    /// Acoustic barge-in (main loop) stores the `started_at` of the utterance
    /// that interrupted speech here; the STT thread then treats it as directed
    /// (even without the name).
    barge_start: Arc<AtomicI64>,
}

/// Barge-in control for the main VAD loop (acoustic interruption during speech).
struct BargeCtl {
    control: Arc<speak::SpeechControl>,
    barge_start: Arc<AtomicI64>,
    /// The mic has an echo-cancel reference (speak.sink) → Jarvis doesn't hear
    /// itself, so voice-onset during speech is a real user. Without AEC, silent
    /// (acoustic) barge-in is disabled (it would trigger on Jarvis's own echo).
    aec: bool,
    enabled: bool,
    min_ms: u64,
}

/// Daemon: listens until something kills it. Returns errors upward — the
/// caller (systemd / `jarvis run`) restarts.
pub fn run_listen(paths: &Paths, cfg: &Config, print_only: bool) -> Result<()> {
    run_listen_ex(paths, cfg, print_only, "mic", None)
}

/// Listening core with extra params: `source` labels utterances in the DB
/// (`"mic"` for regular listening, `"meet"` for a call), and `stop` optionally
/// ends the loop from outside (`jarvis meet` after leaving the call). The
/// regular daemon calls `run_listen` (source `"mic"`, no stop).
pub fn run_listen_ex(
    paths: &Paths,
    cfg: &Config,
    print_only: bool,
    source: &str,
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    // the call and the ambient mic have separate locks — they don't contend
    // (coordination happens via the meet heartbeat below, not mutual killing)
    let _lock = acquire_lock(paths, if source == "meet" { "meet.lock" } else { "listen.lock" })?;
    let is_meet = source == "meet";
    let source = source.to_string();
    // The screen lock only pauses the ambient mic daemon (source "mic"), not
    // the meet bridge — during a call we don't want transcription cut by the
    // lock screen.
    let lock_guard = cfg.listen.pause_when_locked && source == "mic";
    let mut engine = load_transcriber(paths, cfg)?;
    let conn = db::open(&paths.db_path)?;
    info!(
        "poslech běží: VAD ticho {} ms, max promluva {} s{}",
        cfg.listen.silence_ms,
        cfg.listen.max_utterance_s,
        if print_only { " (print-only, bez zápisu do DB)" } else { "" }
    );

    // frames: 128 × 30 ms ≈ 3.8 s of buffer; utterances: 8 pending transcription
    let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<i16>>(128);
    let (utt_tx, utt_rx) = mpsc::sync_channel::<Utterance>(8);
    let device = cfg.listen.device.clone();
    let device_heal_cmd = cfg.listen.device_heal_cmd.clone();

    // voice dialog: queue of wake-addressed utterances → worker (Claude + speech)
    let (convo_hook, convo_rx, barge) = if cfg.converse.enabled && !print_only {
        let (tx, rx) = mpsc::sync_channel::<converse::Job>(4);
        let wake = converse::WakeWords::new(
            &cfg.converse.wake_words,
            cfg.converse.wake_fuzzy,
            &cfg.listen.hint,
        )?;
        let open_ear = converse::OpenEar::from_cfg(&cfg.converse);
        // shared between the STT thread (reads for the follow-up window) and the worker (writes)
        let speech_end = Arc::new(AtomicI64::new(0));
        // barge-in: control is shared by the STT thread, the worker, and the
        // playback thread; the main loop stores in barge_start the utterance
        // that acoustically interrupted.
        let control = Arc::new(speak::SpeechControl::default());
        let barge_start = Arc::new(AtomicI64::new(0));
        // AEC = mic has an echo-cancel reference (speak.sink) → Jarvis doesn't
        // hear itself, so voice-onset during speech is the user; without AEC,
        // wake barge-in only.
        let aec = !cfg.speak.sink.is_empty() && speak::sink_available(&cfg.speak.sink);
        info!(
            "konverzace aktivní — oslovení: {} (open_ear: {})",
            cfg.converse.wake_words.join(", "),
            cfg.converse.open_ear
        );
        if cfg.converse.barge_in {
            if aec {
                info!("barge-in zapnut (akustické přerušení přes AEC sink '{}')", cfg.speak.sink);
            } else {
                info!(
                    "barge-in jen na oslovení jménem — bez AEC (nastav speak.sink na \
                     echo-cancel sink pro tiché přerušení řeči)"
                );
            }
        }
        let hook = ConvoHook {
            wake,
            open_ear,
            speech_end: Arc::clone(&speech_end),
            tx,
            control: Arc::clone(&control),
            barge_in: cfg.converse.barge_in,
            barge_start: Arc::clone(&barge_start),
        };
        let barge = BargeCtl {
            control: Arc::clone(&control),
            barge_start,
            aec,
            enabled: cfg.converse.barge_in,
            min_ms: cfg.converse.barge_in_ms,
        };
        (Some(hook), Some((rx, speech_end, control, aec)), Some(barge))
    } else {
        (None, None, None)
    };

    std::thread::scope(|s| -> Result<()> {
        // 1) audio reader: subprocess with restart and backoff
        s.spawn(move || audio_reader_loop(&device, &device_heal_cmd, frame_tx));

        // 2) transcription: whisper runs longer than the realtime frame
        //    stream → own thread; also owns the conversation hook (dropping tx ends the worker)
        let stt_conn = db::open(&paths.db_path)?;
        s.spawn(move || {
            while let Ok(u) = utt_rx.recv() {
                handle_utterance(&mut engine, &stt_conn, &u, print_only, &source, convo_hook.as_ref());
            }
        });

        // 3) conversation worker: Claude and TTS take seconds → off the STT thread
        if let Some((rx, speech_end, control, aec)) = convo_rx {
            s.spawn(move || converse::worker_loop(paths, cfg, rx, speech_end, control, aec));
        }

        // 4) main loop: VAD + pause + heartbeat + dead-mic watchdog
        let mut v = Vad::new(vad_config(cfg));
        let mut paused = false;
        let mut locked_prev = false;
        let mut meet_prev = false;
        let mut frames: u64 = 0;
        let mut window_peak: i16 = 0;
        let mut silent_warned = false;
        loop {
            if stop.is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed)) {
                info!("stop signál — poslech končí");
                break;
            }
            let frame = match frame_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(f) => f,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    warn!("10 s bez audio dat — zdroj se nejspíš restartuje");
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("audio čtečka skončila"),
            };
            frames += 1;
            // pause check ~1x/s; heartbeat ~1x/min
            if frames % 33 == 1 {
                let now = util::now_ts();
                let timer_paused = db::pause_until(&conn, now)?.is_some();
                // the screen lock check (same privacy guarantee as `jarvis
                // pause`) forks dbus-send → we don't ask every second, only
                // ~1x/3s; the last known state holds between checks. The mic
                // goes quiet within ~3 s of the screen locking.
                if lock_guard && frames % (33 * 3) == 1 {
                    let locked = screen::locked();
                    if locked != locked_prev {
                        if locked {
                            info!("obrazovka uzamčena — mic démon pozastaven (soukromí)");
                        } else {
                            info!("obrazovka odemčena — poslech pokračuje");
                        }
                        locked_prev = locked;
                    }
                }
                // call coordination: the meet bridge holds the heartbeat, the
                // ambient mic daemon goes quiet accordingly (otherwise a double
                // reply — from the call and directly from the mic). The meet
                // daemon never mutes itself.
                let mut meet_active = false;
                if is_meet {
                    db::state_set(&conn, MEET_ACTIVE_KEY, &(now + MEET_ACTIVE_TTL).to_string())?;
                } else {
                    meet_active =
                        db::state_get_i64(&conn, MEET_ACTIVE_KEY)?.is_some_and(|t| t > now);
                    if meet_active != meet_prev {
                        if meet_active {
                            info!("probíhá hovor (jarvis meet) — mic démon ztišen, ať neodpovídá dvakrát");
                        } else {
                            info!("hovor skončil — mic démon pokračuje");
                        }
                        meet_prev = meet_active;
                    }
                }
                paused = timer_paused || locked_prev || meet_active;
                if paused {
                    v.reset();
                }
                if frames % (33 * 60) == 1 {
                    db::state_set(&conn, "listen_alive_ts", &now.to_string())?;
                }
            }
            if paused {
                continue; // privacy: audio is dropped, nothing gets saved
            }
            window_peak = window_peak.max(frame.iter().map(|s| s.saturating_abs()).max().unwrap_or(0));
            let utt = v.push_frame(&frame, util::now_ts());
            // acoustic barge-in is only a CANDIDATE: voice during Jarvis's
            // speech could be its own echo leaking through imperfect AEC. So
            // we do NOT interrupt speech immediately — the candidate is either
            // confirmed by the transcript in handle_utterance (real user → cuts
            // speech and is treated as directed) or dropped (self-echo → Jarvis
            // keeps talking). We store only the utterance start, and only once
            // (barge_start == 0), so the transcript step can consume it.
            if let Some(b) = barge.as_ref() {
                if b.enabled
                    && b.aec
                    && b.control.is_speaking()
                    && !b.control.interrupted()
                    && v.active_voiced_ms() >= b.min_ms
                    && b.barge_start.load(Ordering::Relaxed) == 0
                {
                    if let Some(start) = v.active_started_at() {
                        b.barge_start.store(start, Ordering::Relaxed);
                        debug!(
                            "barge-in kandidát ({} ms) — čekám na potvrzení přepisem",
                            v.active_voiced_ms()
                        );
                    }
                }
            }
            if let Some(u) = utt {
                if utt_tx.try_send(u).is_err() {
                    warn!(
                        "přepis nestíhá — promluva zahozena (Scribe: pomalá síť; \
                         whisper: zvaž rychlejší model listen.model = \"small-q5_1\")"
                    );
                }
            }
            // 2 minutes of pure digital silence = mic likely producing nothing
            if frames % 4000 == 0 {
                if window_peak < 3 {
                    if !silent_warned {
                        warn!("žádný signál z mikrofonu (2 min digitální ticho) — odpojen/mute?");
                        silent_warned = true;
                    }
                    db::state_set(&conn, "listen_silent", "1")?;
                } else {
                    silent_warned = false;
                    db::state_del(&conn, "listen_silent")?;
                }
                window_peak = 0;
            }
        }
        Ok(())
    })
}

/// Minimum gap between source self-healing attempts (s) — so a fast loop
/// restart doesn't stack duplicate modules when healing runs but the source
/// only comes back later (e.g. a USB mic not yet enumerated after wake).
const HEAL_MIN_INTERVAL_S: i64 = 15;

/// How often during capture to verify the configured source still exists (s).
/// PulseAudio's `module-rescue-streams` silently moves parec to the default
/// mic after the source disappears — parec doesn't die, so the restart loop
/// (and healing with it) would otherwise never fire. Hence we check the
/// source while parec is running too.
const SRC_CHECK_INTERVAL_S: u64 = 5;

/// Should self-healing run now? Pure function (testable): only when a command
/// is configured, the source is confirmed missing, and the rate-limit has elapsed.
fn heal_due(has_cmd: bool, source_missing: bool, now: i64, last_heal: i64) -> bool {
    has_cmd && source_missing && now - last_heal >= HEAL_MIN_INTERVAL_S
}

/// Runs the user command that recovers a missing audio source (typically
/// reloading `module-echo-cancel`). Via `sh -c`; result is only logged —
/// healing failure must not crash the listening loop (the source may still
/// come back another way).
fn run_heal_cmd(cmd: &str) {
    match std::process::Command::new("sh").arg("-c").arg(cmd).output() {
        Ok(out) if out.status.success() => {
            info!("self-healing audio zdroje: příkaz proběhl (exit 0)");
        }
        Ok(out) => warn!(
            "self-healing audio zdroje: příkaz skončil {} — {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => warn!("self-healing audio zdroje: příkaz nejde spustit: {e:#}"),
    }
}

/// Reads frames from the subprocess and sends them to the main loop; restarts
/// a dead source with exponential backoff. When the configured source
/// disappears entirely (typically `module-echo-cancel` after suspend with a
/// disconnected USB mic), tries `heal_cmd` (self-healing) instead of waiting
/// forever. Ends only when the other end of the channel is gone.
fn audio_reader_loop(device: &str, heal_cmd: &str, tx: mpsc::SyncSender<Vec<i16>>) {
    let mut backoff = 1u64;
    let mut last_heal = 0i64;
    loop {
        // self-healing: the configured source is confirmed missing → try to
        // recover it (reload the module) before spawning parec. The rate-limit
        // prevents stacking duplicate modules when the source comes back slowly
        // (mic after wake).
        if !heal_cmd.is_empty() {
            let now = util::now_ts();
            if heal_due(true, audio::source_missing(device), now, last_heal) {
                last_heal = now;
                warn!("audio zdroj '{device}' chybí — self-healing (reload): {heal_cmd}");
                run_heal_cmd(heal_cmd);
            }
        }
        match audio::spawn_source(device) {
            Ok(mut src) => {
                let name = src.name;
                match src.stdout() {
                    Ok(mut out) => {
                        info!("audio zdroj: {name}");
                        let started = std::time::Instant::now();
                        let mut scratch = vec![0u8; FRAME_SAMPLES * 2];
                        let mut last_src_check = std::time::Instant::now();
                        loop {
                            // parec is alive, but the source may have disappeared
                            // (echo-cancel died along with a suspended USB mic)
                            // and PulseAudio silently moved it to the default
                            // mic. Check periodically; if the source is gone,
                            // end parec (src.drop kills it) → outer loop → heal.
                            if !device.is_empty()
                                && last_src_check.elapsed() >= Duration::from_secs(SRC_CHECK_INTERVAL_S)
                            {
                                last_src_check = std::time::Instant::now();
                                if audio::source_missing(device) {
                                    warn!("audio zdroj '{device}' zmizel pod parcem (PA rescue na výchozí?) — restartuji zdroj");
                                    break;
                                }
                            }
                            match audio::read_frame(&mut out, &mut scratch) {
                                Ok(Some(frame)) => {
                                    if tx.send(frame).is_err() {
                                        return; // main loop ended
                                    }
                                }
                                Ok(None) => {
                                    warn!("audio zdroj {name} skončil (EOF)");
                                    break;
                                }
                                Err(e) => {
                                    warn!("čtení audia selhalo: {e:#}");
                                    break;
                                }
                            }
                        }
                        // source ran healthily for a while → handle the next crash swiftly
                        if started.elapsed() > Duration::from_secs(60) {
                            backoff = 1;
                        }
                    }
                    Err(e) => warn!("{e:#}"),
                }
            }
            Err(e) => warn!("audio zdroj nejde spustit: {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(backoff));
        backoff = (backoff * 2).min(60);
    }
}

/// Transcribes an utterance and saves/prints the result; wake-addressed
/// utterances get handed to the conversation worker. Errors are only logged —
/// one bad utterance must not crash the daemon.
fn handle_utterance(
    engine: &mut Transcriber,
    conn: &rusqlite::Connection,
    u: &Utterance,
    print_only: bool,
    source: &str,
    convo: Option<&ConvoHook>,
) {
    let dur = u.samples.len() as f32 / SAMPLE_RATE as f32;
    let t0 = std::time::Instant::now();
    match engine.transcribe(&u.samples) {
        Ok(Some(t)) => {
            let rtf = t0.elapsed().as_secs_f32() / dur.max(0.1);
            info!(
                "🗣 [{}] ({}, p {:.2}, {:.1} s, rtf {:.2}) {}",
                util::fmt_hm(u.started_at),
                t.lang,
                t.conf,
                dur,
                rtf,
                t.text
            );
            if !print_only {
                if let Err(e) = db::insert_utterance(
                    conn, u.started_at, u.ended_at, &t.text, &t.lang, f64::from(t.conf), source,
                ) {
                    warn!("zápis promluvy selhal: {e:#}");
                }
            }
            if let Some(h) = convo {
                let speech_end = h.speech_end.load(Ordering::Relaxed);
                // consume the pending acoustic barge candidate (energy during
                // speech waiting for transcript confirmation) — always, so it
                // doesn't stay hanging
                let bs = h.barge_start.swap(0, Ordering::Relaxed);
                let barged = h.barge_in && bs != 0 && (bs..=bs + 2).contains(&u.started_at);
                let triaged =
                    converse::triage(&h.wake, &h.open_ear, &t.text, u.started_at, speech_end);
                let is_wake = matches!(triaged, Some(converse::Trigger::Wake));
                // self-echo: Jarvis's own speech (ack/filler/reply) leaking back
                // into the mic through imperfect AEC. A wake address can't be an
                // echo (Jarvis never says its own name) → wake is exempted from
                // the filter; other self-speech is dropped: barge is NOT
                // confirmed (Jarvis keeps talking) and no false turn fires
                // (ends "conversations with itself").
                if !is_wake && h.control.is_self_echo(&t.text, u.started_at) {
                    debug!("konverzace: vlastní ozvěna (self-echo) — ignoruji: {}", t.text);
                } else {
                    // confirmed acoustic barge (real user) → cut speech now
                    if barged && h.control.is_speaking() {
                        h.control.barge_in();
                        info!("barge-in: uživatel mluví — přerušuji řeč");
                    }
                    // a barged utterance is directed even without the name (user cut into speech)
                    let trigger = if barged { Some(converse::Trigger::Wake) } else { triaged };
                    if let Some(trigger) = trigger {
                        // wake barge-in: "Jarvisi …" during speech interrupts (echo-safe even without AEC)
                        if h.barge_in && trigger == converse::Trigger::Wake && h.control.is_speaking() {
                            h.control.barge_in();
                        }
                        let job = converse::Job {
                            text: t.text.clone(),
                            started_at: u.started_at,
                            trigger,
                        };
                        if h.tx.try_send(job).is_err() {
                            warn!("konverzace: worker nestíhá — promluva zahozena: {}", t.text);
                        }
                    }
                }
            }
        }
        Ok(None) => {
            // even a speechless utterance (just noise) consumes a pending barge
            // candidate, so it doesn't linger into the next utterance
            if let Some(h) = convo {
                h.barge_start.store(0, Ordering::Relaxed);
            }
            debug!("promluva bez řeči ({dur:.1} s) — zahozena");
        }
        Err(e) => warn!("přepis selhal: {e:#}"),
    }
}

/// Runs a WAV file through the whole pipeline (VAD + STT) as if it came from
/// the mic. Writes nothing to the DB — for testing and debugging.
pub fn run_wav(paths: &Paths, cfg: &Config, wav: &Path) -> Result<()> {
    let mut engine = load_transcriber(paths, cfg)?;
    let samples = audio::read_wav_mono_16k(wav)?;
    info!(
        "WAV {}: {:.1} s @ 16 kHz mono",
        wav.display(),
        samples.len() as f32 / SAMPLE_RATE as f32
    );
    let conn = db::open(&paths.db_path)?;
    let mut v = Vad::new(vad_config(cfg));
    let start = util::now_ts();
    let mut processed = 0usize;
    let mut count = 0usize;
    for frame in samples.chunks(FRAME_SAMPLES) {
        if frame.len() < FRAME_SAMPLES {
            break;
        }
        processed += FRAME_SAMPLES;
        let now = start + (processed / SAMPLE_RATE) as i64;
        if let Some(u) = v.push_frame(frame, now) {
            count += 1;
            handle_utterance(&mut engine, &conn, &u, true, "wav", None);
        }
    }
    if let Some(u) = v.flush(start + (processed / SAMPLE_RATE) as i64) {
        count += 1;
        handle_utterance(&mut engine, &conn, &u, true, "wav", None);
    }
    if count == 0 {
        println!("VAD v souboru žádnou řeč nenašel.");
    }
    Ok(())
}

/// `jarvis listen --download-model`: downloads the model from config.
pub fn download(paths: &Paths, cfg: &Config) -> Result<()> {
    if !cfg.listen.model_path.is_empty() {
        bail!(
            "listen.model_path je nastavené ({}) — stahování řídí jen listen.model",
            cfg.listen.model_path
        );
    }
    let p = stt::download_model(&paths.models_dir, &cfg.listen.model)?;
    println!("Model připraven: {}", p.display());
    Ok(())
}

#[cfg(test)]
mod heal_tests {
    use super::*;

    #[test]
    fn no_heal_without_cmd() {
        // without `device_heal_cmd` heal never runs (same behavior as before)
        assert!(!heal_due(false, true, 1_000, 0));
    }

    #[test]
    fn no_heal_when_source_present() {
        // source exists → no heal, even with a command and after any amount of time
        assert!(!heal_due(true, false, 1_000_000, 0));
    }

    #[test]
    fn heals_when_missing_cmd_and_interval() {
        assert!(heal_due(true, true, HEAL_MIN_INTERVAL_S, 0));
    }

    #[test]
    fn rate_limited_against_module_stacking() {
        let last = 1_000;
        // too soon after the last attempt → no heal (otherwise modules would stack)
        assert!(!heal_due(true, true, last + HEAL_MIN_INTERVAL_S - 1, last));
        // once the rate-limit elapses → heal again
        assert!(heal_due(true, true, last + HEAL_MIN_INTERVAL_S, last));
    }
}
