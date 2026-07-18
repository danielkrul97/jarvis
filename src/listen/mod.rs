//! Poslech mikrofonu: near-realtime přepis řeči (STT).
//!
//! Tok: parec/arecord → rámce 30 ms → VAD (ucelené promluvy) → whisper
//! (vlastní vlákno, aby VAD držel krok s realtime tokem) → SQLite
//! `utterances`. Audio se NIKDY neukládá na disk — ven jde jen text; při
//! `jarvis pause` se zvuk zahazuje rovnou z RAM a VAD se resetuje.

pub mod audio;
pub mod scribe;
pub mod stt;
pub mod vad;

use crate::config::{self, Config, ListenCfg, Paths};
use crate::converse;
use crate::screen;
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

/// Exkluzivní flock proti dvojímu poslechu (systemd služba × `jarvis run`
/// × ruční `jarvis listen`). Zámek drží vrácený File.
fn acquire_lock(paths: &Paths) -> Result<std::fs::File> {
    let path = paths.data_dir.join("listen.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("nelze otevřít {}", path.display()))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!(
            "poslech už běží v jiném procesu — `systemctl --user status jarvis-listen` \
             (nebo jiný `jarvis listen`/`jarvis run`)"
        );
    }
    Ok(f)
}

fn vad_config(cfg: &Config) -> VadConfig {
    VadConfig {
        min_speech_ms: cfg.listen.min_speech_ms,
        silence_ms: cfg.listen.silence_ms,
        max_utterance_ms: cfg.listen.max_utterance_s * 1000,
        speech_mult: cfg.listen.vad_speech_mult,
    }
}

/// Po chybě Scribe zůstane „auto" na lokálním whisperu tohle dlouho, než
/// cloud zkusí znovu — jinak by mrtvý klíč/síť platily failnutý round-trip
/// (u transport chyby i 3 s retry) na každé promluvě.
const SCRIBE_COOLDOWN: Duration = Duration::from_secs(60);

/// STT engine pro jednu promluvu. Volí se dle `listen.engine`.
enum Transcriber {
    /// Jen lokální whisper (načtený eagerly).
    Whisper(stt::Stt),
    /// Jen ElevenLabs Scribe, bez fallbacku.
    Scribe { key: String, listen: ListenCfg },
    /// Scribe napřed, whisper jako líný fallback (načte se až je potřeba).
    Auto(AutoStt),
}

/// „auto": Scribe s líným whisper fallbackem a cooldownem po chybě.
struct AutoStt {
    key: String,
    listen: ListenCfg,
    /// Cesta k whisper modelu pro líné načtení (rozbaleno při konstrukci).
    model_path: PathBuf,
    /// None dokud fallback poprvé nezasáhne — pak drží načtený model.
    whisper: Option<stt::Stt>,
    /// Dokdy jedeme lokálně (Scribe je dočasně mrtvý); None = zkoušet Scribe.
    scribe_down_until: Option<Instant>,
}

impl AutoStt {
    /// Líné načtení whisperu; první fallback zaplatí načtení modelu, dál 0.
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
        if !cooling {
            match scribe::transcribe(&self.key, &self.listen, samples) {
                Ok(r) => {
                    self.scribe_down_until = None; // Scribe zase žije
                    return Ok(r);
                }
                Err(e) => {
                    warn!(
                        "Scribe selhal — {} s jedu lokálně na whisperu: {e:#}",
                        SCRIBE_COOLDOWN.as_secs()
                    );
                    self.scribe_down_until = Some(Instant::now() + SCRIBE_COOLDOWN);
                }
            }
        }
        self.whisper()?.transcribe(samples)
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

/// Načte lokální whisper model (eager). Sdílí ho `whisper` engine i „auto"
/// fallback.
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

/// Sestaví STT engine dle `listen.engine`. „auto" bez ElevenLabs klíče tiše
/// spadne na lokální whisper.
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

/// Oslovené promluvy tečou z STT vlákna do konverzačního workera.
struct ConvoHook {
    wake: converse::WakeWords,
    open_ear: converse::OpenEar,
    /// Sdílené s workerem: kdy Jarvis naposledy domluvil (echo-guard + práh
    /// follow-up okna). Worker zapisuje, STT vlákno tady čte.
    speech_end: Arc<AtomicI64>,
    tx: mpsc::SyncSender<converse::Job>,
}

/// Démon: poslouchá, dokud ho něco nezabije. Chybu vrací nahoru — volající
/// (systemd / `jarvis run`) restartuje.
pub fn run_listen(paths: &Paths, cfg: &Config, print_only: bool) -> Result<()> {
    run_listen_ex(paths, cfg, print_only, "mic", None)
}

/// Jádro poslechu s parametry navíc: `source` je štítek promluv v DB
/// (`"mic"` pro klasický poslech, `"meet"` pro hovor) a `stop` volitelně
/// ukončí smyčku zvenčí (`jarvis meet` po odchodu z hovoru). Klasický démon
/// volá `run_listen` (source `"mic"`, bez stopu).
pub fn run_listen_ex(
    paths: &Paths,
    cfg: &Config,
    print_only: bool,
    source: &str,
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let _lock = acquire_lock(paths)?;
    let source = source.to_string();
    // Zámek obrazovky pozastavuje jen ambientní mic démona (source "mic"), ne
    // meet bridge — při hovoru nechceme přepis utnout kvůli lock screenu.
    let lock_guard = cfg.listen.pause_when_locked && source == "mic";
    let mut engine = load_transcriber(paths, cfg)?;
    let conn = db::open(&paths.db_path)?;
    info!(
        "poslech běží: VAD ticho {} ms, max promluva {} s{}",
        cfg.listen.silence_ms,
        cfg.listen.max_utterance_s,
        if print_only { " (print-only, bez zápisu do DB)" } else { "" }
    );

    // rámce: 128 × 30 ms ≈ 3,8 s rezerva; promluvy: 8 čekajících na přepis
    let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<i16>>(128);
    let (utt_tx, utt_rx) = mpsc::sync_channel::<Utterance>(8);
    let device = cfg.listen.device.clone();

    // hlasový dialog: fronta oslovených promluv → worker (Claude + řeč)
    let (convo_hook, convo_rx) = if cfg.converse.enabled && !print_only {
        let (tx, rx) = mpsc::sync_channel::<converse::Job>(4);
        let wake = converse::WakeWords::new(
            &cfg.converse.wake_words,
            cfg.converse.wake_fuzzy,
            &cfg.listen.hint,
        )?;
        let open_ear = converse::OpenEar::from_cfg(&cfg.converse);
        // sdílený mezi STT vláknem (čte pro follow-up okno) a workerem (zapisuje)
        let speech_end = Arc::new(AtomicI64::new(0));
        info!(
            "konverzace aktivní — oslovení: {} (open_ear: {})",
            cfg.converse.wake_words.join(", "),
            cfg.converse.open_ear
        );
        let hook = ConvoHook { wake, open_ear, speech_end: Arc::clone(&speech_end), tx };
        (Some(hook), Some((rx, speech_end)))
    } else {
        (None, None)
    };

    std::thread::scope(|s| -> Result<()> {
        // 1) čtečka zvuku: subprocess s restartem a backoffem
        s.spawn(move || audio_reader_loop(&device, frame_tx));

        // 2) přepis: whisper běží déle než realtime tok rámců → vlastní
        //    vlákno; vlastní i konverzační hook (drop tx ukončí workera)
        let stt_conn = db::open(&paths.db_path)?;
        s.spawn(move || {
            while let Ok(u) = utt_rx.recv() {
                handle_utterance(&mut engine, &stt_conn, &u, print_only, &source, convo_hook.as_ref());
            }
        });

        // 3) konverzační worker: Claude i TTS trvají sekundy → mimo STT vlákno
        if let Some((rx, speech_end)) = convo_rx {
            s.spawn(move || converse::worker_loop(paths, cfg, rx, speech_end));
        }

        // 4) hlavní smyčka: VAD + pauza + heartbeat + hlídač mrtvého mikrofonu
        let mut v = Vad::new(vad_config(cfg));
        let mut paused = false;
        let mut locked_prev = false;
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
            // pauza ~1× za s; heartbeat ~1× za minutu
            if frames % 33 == 1 {
                let now = util::now_ts();
                let timer_paused = db::pause_until(&conn, now)?.is_some();
                // zámek obrazovky (stejné soukromí jako `jarvis pause`) forkuje
                // dbus-send → neptáme se každou sekundu, ale ~1× za 3 s; mezi
                // dotazy platí poslední známý stav. Mic se po zamčení obrazovky
                // ztiší nejpozději do ~3 s.
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
                paused = timer_paused || locked_prev;
                if paused {
                    v.reset();
                }
                if frames % (33 * 60) == 1 {
                    db::state_set(&conn, "listen_alive_ts", &now.to_string())?;
                }
            }
            if paused {
                continue; // soukromí: zvuk se zahazuje, nic se neukládá
            }
            window_peak = window_peak.max(frame.iter().map(|s| s.saturating_abs()).max().unwrap_or(0));
            if let Some(u) = v.push_frame(&frame, util::now_ts()) {
                if utt_tx.try_send(u).is_err() {
                    warn!(
                        "přepis nestíhá — promluva zahozena (Scribe: pomalá síť; \
                         whisper: zvaž rychlejší model listen.model = \"small-q5_1\")"
                    );
                }
            }
            // 2 minuty čistého digitálního ticha = mikrofon nejspíš nic nedodává
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

/// Čte rámce ze subprocessu a posílá je hlavní smyčce; padlý zdroj restartuje
/// s exponenciálním backoffem. Končí, až když druhá strana kanálu zmizí.
fn audio_reader_loop(device: &str, tx: mpsc::SyncSender<Vec<i16>>) {
    let mut backoff = 1u64;
    loop {
        match audio::spawn_source(device) {
            Ok(mut src) => {
                let name = src.name;
                match src.stdout() {
                    Ok(mut out) => {
                        info!("audio zdroj: {name}");
                        let started = std::time::Instant::now();
                        let mut scratch = vec![0u8; FRAME_SAMPLES * 2];
                        loop {
                            match audio::read_frame(&mut out, &mut scratch) {
                                Ok(Some(frame)) => {
                                    if tx.send(frame).is_err() {
                                        return; // hlavní smyčka skončila
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
                        // zdroj chvíli zdravě běžel → příští pád řešíme svižně
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

/// Přepíše promluvu a uloží/vypíše výsledek; oslovené promluvy předá
/// konverzačnímu workeru. Chyby jen loguje — jedna vadná promluva nesmí
/// položit démona.
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
                if let Some(trigger) =
                    converse::triage(&h.wake, &h.open_ear, &t.text, u.started_at, speech_end)
                {
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
        Ok(None) => debug!("promluva bez řeči ({dur:.1} s) — zahozena"),
        Err(e) => warn!("přepis selhal: {e:#}"),
    }
}

/// Prožene WAV soubor celou pipeline (VAD + STT) jako by šel z mikrofonu.
/// Nic nezapisuje do DB — slouží k testům a ladění.
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

/// `jarvis listen --download-model`: stáhne model z configu.
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
