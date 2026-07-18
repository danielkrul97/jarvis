use crate::config::{self, Config, Paths};
use crate::store::db;
use crate::util;
use anyhow::{bail, Result};
use rusqlite::OptionalExtension;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn check_line(ok: bool, label: &str, detail: &str) {
    let mark = if ok { "✓" } else { "✗" };
    if detail.is_empty() {
        println!("{mark} {label}");
    } else {
        println!("{mark} {label} — {detail}");
    }
}

pub fn doctor(paths: &Paths, cfg: &Config, live: bool) -> Result<()> {
    let mut problems = 0usize;
    let mut check = |ok: bool, label: &str, detail: String| {
        check_line(ok, label, &detail);
        if !ok {
            problems += 1;
        }
    };

    // config
    if paths.config_file.exists() {
        check(true, "config", format!("{}", paths.config_file.display()));
    } else {
        check(true, "config", "soubor neexistuje, používám defaults".into());
    }

    // DISPLAY + X spojení
    match std::env::var("DISPLAY") {
        Ok(d) if !d.is_empty() => {
            check(true, "DISPLAY", d.clone());
            match crate::capture::x11::X11::connect() {
                Ok(x) => {
                    let (w, h) = x.geometry();
                    check(true, "X11 spojení", format!("root {w}x{h}, idle detekce: {}",
                        if x.has_screensaver() { "ano (MIT-SCREEN-SAVER)" } else { "NE — idle se ignoruje" }));
                }
                Err(e) => check(false, "X11 spojení", format!("{e:#}")),
            }
        }
        _ => check(false, "DISPLAY", "není nastaveno — capture nemůže běžet".into()),
    }

    // ovládání oken (wm): EWMH + XTest pro syntetický vstup
    match crate::wm::Wm::connect(cfg.wm.key_delay_ms) {
        Ok(w) => match w.ensure_xtest() {
            Ok(()) => {
                let n = w.windows().map(|v| v.len()).unwrap_or(0);
                check(
                    true,
                    "ovládání oken (wm)",
                    format!(
                        "XTest OK, {n} oken; converse agent: {}",
                        if cfg.wm.enabled { "povolen" } else { "vypnut ([wm] enabled=false)" }
                    ),
                );
            }
            Err(e) => check(false, "ovládání oken (wm)", format!("{e:#}")),
        },
        Err(e) => check(false, "ovládání oken (wm)", format!("{e:#}")),
    }

    // claude CLI
    match Command::new("claude").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            check(true, "claude CLI", v);
        }
        Ok(out) => check(false, "claude CLI", format!("exit {:?}", out.status.code())),
        Err(e) => check(false, "claude CLI", format!("nenalezen: {e}")),
    }

    // SendGrid klíč + práva souboru
    match config::sendgrid_key(paths) {
        Ok(_) => {
            let perms_ok = std::fs::metadata(&paths.secrets_file)
                .map(|m| m.permissions().mode() & 0o077 == 0)
                .unwrap_or(true); // klíč může být jen v env
            check(
                perms_ok,
                "SendGrid klíč",
                if perms_ok {
                    "nalezen".into()
                } else {
                    format!("{} je čitelný pro ostatní — chmod 600", paths.secrets_file.display())
                },
            );
        }
        Err(e) => check(false, "SendGrid klíč", format!("{e:#}")),
    }

    // runbooky (fáze D): spustitelnost + kanál pro schvalování na dálku
    if cfg.runbooks.enabled {
        let conn_check = db::open(&paths.db_path);
        match conn_check {
            Ok(conn) => {
                let n = crate::runbook::all(&conn).map(|v| v.len()).unwrap_or(0);
                let pending = crate::runbook::pending_proposals(&conn).map(|v| v.len()).unwrap_or(0);
                check(
                    true,
                    "runbooky",
                    format!(
                        "{n} schválených, {pending} návrhů čeká; hlasové spouštění: {}",
                        if cfg.runbooks.voice_run { "povoleno" } else { "vypnuto" }
                    ),
                );
            }
            Err(e) => check(false, "runbooky", format!("DB nejde otevřít: {e:#}")),
        }
        if cfg.runbooks.telegram_approve {
            match config::telegram_keys(paths) {
                Ok(_) => check(true, "Telegram (schvalování)", "klíče nalezeny".into()),
                Err(e) => check(false, "Telegram (schvalování)", format!("{e:#}")),
            }
        }
    }

    // Twilio (SMS)
    if cfg.sms.enabled {
        match config::twilio_keys(paths) {
            Ok(_) => check(
                true,
                "Twilio klíče (SMS)",
                format!("nalezeny; from {}, výchozí příjemce {}", cfg.sms.from, cfg.sms.to),
            ),
            Err(e) => check(false, "Twilio klíče (SMS)", format!("{e:#}")),
        }
    } else {
        check(true, "SMS", "vypnuty v configu".into());
    }

    // DB
    match db::open(&paths.db_path) {
        Ok(conn) => {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0))?;
            check(true, "databáze", format!("{} ({n} vzorků)", paths.db_path.display()));
        }
        Err(e) => check(false, "databáze", format!("{e:#}")),
    }

    // disk
    let (bytes, files) = util::dir_size(&paths.shots_dir);
    check(true, "screenshoty", format!("{files} souborů, {}", util::human_bytes(bytes)));

    // poslech
    if cfg.listen.enabled {
        let l = &cfg.listen;
        check(
            true,
            "STT engine",
            match l.engine.as_str() {
                "whisper" => "whisper (lokální)".into(),
                "elevenlabs" => format!("ElevenLabs Scribe ({}), bez fallbacku", l.scribe_model),
                _ => format!("auto: Scribe ({}) → whisper fallback", l.scribe_model),
            },
        );
        // ElevenLabs klíč pro Scribe (engine auto/elevenlabs)
        if l.engine != "whisper" {
            match config::elevenlabs_key(paths) {
                Ok(_) => check(true, "ElevenLabs klíč (Scribe)", "nalezen".into()),
                Err(e) => check(false, "ElevenLabs klíč (Scribe)", format!("{e:#}")),
            }
        }
        // whisper model: nutný pro engine "whisper", fallback pro "auto"
        if l.engine != "elevenlabs" {
            let model = l.resolve_model_path(paths);
            let label = if l.engine == "whisper" { "whisper model" } else { "whisper model (fallback)" };
            match std::fs::metadata(&model) {
                Ok(m) => check(
                    true,
                    label,
                    format!("{} ({})", model.display(), util::human_bytes(m.len())),
                ),
                Err(_) => check(
                    false,
                    label,
                    format!("{} chybí — `jarvis listen --download-model`", model.display()),
                ),
            }
        }
        let audio_tool = ["parec", "arecord"]
            .iter()
            .find(|b| Command::new(*b).arg("--version").output().is_ok());
        check(
            audio_tool.is_some(),
            "audio nástroj",
            audio_tool
                .map(|b| (*b).to_string())
                .unwrap_or_else(|| "chybí parec i arecord (pulseaudio-utils / alsa-utils)".into()),
        );
        // zámek obrazovky: mic démon se pauzuje, když je aktivní screensaver
        if cfg.listen.pause_when_locked {
            match crate::screen::probe() {
                crate::screen::Lock::Active => {
                    check(true, "zámek obrazovky", "detekce OK — teď UZAMČENO (poslech pauzuje)".into())
                }
                crate::screen::Lock::Inactive => check(
                    true,
                    "zámek obrazovky",
                    "detekce OK, teď odemčeno — při zámku se poslech pozastaví".into(),
                ),
                crate::screen::Lock::Unknown(why) => check(
                    true,
                    "zámek obrazovky",
                    format!("⚠ stav nezjistím ({why}) — poslech se při zámku NEpozastaví (fail-open)"),
                ),
            }
        } else {
            check(true, "zámek obrazovky", "pause_when_locked=false — poslech běží i při zámku".into());
        }
    } else {
        check(true, "poslech", "vypnut v configu".into());
    }

    // hlas (TTS): ElevenLabs a/nebo lokální piper podle engine
    if cfg.speak.enabled {
        if cfg.speak.engine != "piper" {
            match config::elevenlabs_key(paths) {
                Ok(_) => check(true, "ElevenLabs klíč", "nalezen".into()),
                Err(e) => check(false, "ElevenLabs klíč", format!("{e:#}")),
            }
        }
        if cfg.speak.engine != "elevenlabs" {
            let piper_ok = Command::new(&cfg.speak.piper_bin)
                .arg("--help")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            check(
                piper_ok,
                "piper (lokální TTS)",
                if piper_ok {
                    cfg.speak.piper_bin.clone()
                } else {
                    format!("'{}' nefunguje — `pip3 install --user piper-tts`", cfg.speak.piper_bin)
                },
            );
            let model = crate::speak::piper::model_path(paths, &cfg.speak);
            match std::fs::metadata(&model) {
                Ok(m) => check(
                    true,
                    "piper hlas",
                    format!("{} ({})", model.display(), util::human_bytes(m.len())),
                ),
                Err(_) => check(
                    false,
                    "piper hlas",
                    format!("{} chybí — `jarvis say --download-model`", model.display()),
                ),
            }
        }
        match crate::speak::detect_player(&cfg.speak.player) {
            Some(p) => check(true, "audio přehrávač", p),
            None => check(
                false,
                "audio přehrávač",
                "chybí ffplay/mpv/ffmpeg+paplay — nainstaluj, nebo nastav speak.player".into(),
            ),
        }
        if !cfg.speak.sink.is_empty() {
            let ok = crate::speak::sink_available(&cfg.speak.sink);
            check(
                ok,
                "audio sink (AEC reference)",
                if ok {
                    cfg.speak.sink.clone()
                } else {
                    format!(
                        "'{}' neexistuje — zkontroluj sink_name u module-echo-cancel \
                         v ~/.config/pulse/default.pa (řeč jde zatím na výchozí výstup)",
                        cfg.speak.sink
                    )
                },
            );
        }
    } else {
        check(true, "hlas", "vypnut v configu".into());
    }

    if live {
        match config::sendgrid_key(paths) {
            Ok(key) => match crate::mail::sendgrid::sandbox_check(&cfg.email, &key) {
                Ok(()) => check(true, "SendGrid (sandbox send)", "klíč i odesílatel OK".into()),
                Err(e) => check(false, "SendGrid (sandbox send)", format!("{e:#}")),
            },
            Err(_) => check(false, "SendGrid (sandbox send)", "bez klíče nelze ověřit".into()),
        }
        match crate::pipeline::claude::ping(&cfg.analysis.model) {
            Ok(v) => check(true, "claude -p (ping)", v),
            Err(e) => check(false, "claude -p (ping)", format!("{e:#}")),
        }
        // kredity zajímají TTS (speak) i Scribe STT (listen) — sdílí účet
        let uses_elevenlabs = (cfg.speak.enabled && cfg.speak.engine != "piper")
            || (cfg.listen.enabled && cfg.listen.engine != "whisper");
        if uses_elevenlabs {
            match config::elevenlabs_key(paths) {
                Ok(key) => match crate::speak::tts::credits(&key) {
                    Ok(crate::speak::tts::Credits::Known { used, limit }) => {
                        let left = limit.saturating_sub(used);
                        check(
                            left > 0,
                            "ElevenLabs kredity",
                            format!("{left} zbývá ({used}/{limit} použito)"),
                        );
                    }
                    Ok(crate::speak::tts::Credits::NoPermission) => check(
                        true,
                        "ElevenLabs kredity",
                        "klíč platný; je scoped bez user_read, zůstatek nevidím".into(),
                    ),
                    Err(e) => check(false, "ElevenLabs kredity", format!("{e:#}")),
                },
                Err(_) => check(false, "ElevenLabs kredity", "bez klíče nelze ověřit".into()),
            }
        }
        if cfg.sms.enabled {
            match config::twilio_keys(paths) {
                Ok((sid, token)) => match crate::sms::balance(&sid, &token) {
                    Ok(b) => check(true, "Twilio zůstatek", b),
                    Err(e) => check(false, "Twilio zůstatek", format!("{e:#}")),
                },
                Err(_) => check(false, "Twilio zůstatek", "bez klíčů nelze ověřit".into()),
            }
        }
        if cfg.listen.enabled {
            // 3 s: webrtc zdroj dává novému klientovi první ~2 s nuly (warm-up)
            match crate::listen::audio::probe_level(&cfg.listen.device, 3.0) {
                Ok((dbfs, peak)) => {
                    let has_signal = peak >= 3;
                    check(
                        has_signal,
                        "mikrofon (3 s poslech)",
                        if has_signal {
                            format!("úroveň {dbfs:.0} dBFS, peak {peak}")
                        } else {
                            format!("digitální ticho (peak {peak}) — odpojen/mute?")
                        },
                    );
                }
                Err(e) => check(false, "mikrofon (1,5 s poslech)", format!("{e:#}")),
            }
        }
    }

    if problems > 0 {
        bail!("doctor našel {problems} problém(ů)");
    }
    println!("Vše v pořádku.");
    Ok(())
}

pub fn status(paths: &Paths, cfg: &Config) -> Result<()> {
    let conn = db::open(&paths.db_path)?;
    let now = util::now_ts();
    let (day_start, _) = util::day_bounds_local(util::today_local())?;

    println!("Jarvis status — {}", util::fmt_local(now));

    match db::pause_until(&conn, now)? {
        Some(t) => println!("  snímání:        POZASTAVENO do {}", util::fmt_local(t)),
        None => println!("  snímání:        aktivní (pokud běží démon)"),
    }

    let last: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT ts, wm_class, title FROM samples ORDER BY ts DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match last {
        Some((ts, class, title)) => {
            let age = now - ts;
            println!("  poslední vzorek: {} ({age} s zpět) — {class}: {title}", util::fmt_local(ts));
            if age > 120 {
                println!("                  ⚠ démon zřejmě neběží (vzorek starší 2 min)");
            }
        }
        None => println!("  poslední vzorek: žádný — capture ještě neběžel"),
    }

    let samples_today: i64 = conn.query_row(
        "SELECT COUNT(*) FROM samples WHERE ts >= ?1",
        [day_start],
        |r| r.get(0),
    )?;
    let shots_today: i64 = conn.query_row(
        "SELECT COUNT(*) FROM samples WHERE ts >= ?1 AND shot_path IS NOT NULL",
        [day_start],
        |r| r.get(0),
    )?;
    println!("  dnes:           {samples_today} vzorků, {shots_today} screenshotů");

    if !cfg.listen.enabled {
        println!("  poslech:        vypnut v configu");
    } else if !cfg.listen.resolve_model_path(paths).exists() {
        println!("  poslech:        model chybí — `jarvis listen --download-model`");
    } else {
        let alive = db::state_get_i64(&conn, "listen_alive_ts")?
            .map(|t| now - t <= 150)
            .unwrap_or(false);
        let utt_today = db::utterance_count_since(&conn, day_start)?;
        println!(
            "  poslech:        {}; dnes {utt_today} promluv",
            if alive { "běží" } else { "neběží (démon nehlásí tep)" }
        );
        if db::state_get(&conn, "listen_silent")?.is_some() {
            println!("                  ⚠ mikrofon nedodává signál (digitální ticho)");
        }
        match db::last_utterance(&conn)? {
            Some((ts, text)) => println!(
                "  poslední řeč:   {} — „{}“",
                util::fmt_local(ts),
                util::truncate_chars(&text, 60)
            ),
            None => println!("  poslední řeč:   zatím žádná"),
        }
    }

    if cfg.speak.enabled {
        let (bytes, files) = util::dir_size(&paths.tts_cache_dir);
        let engine = match cfg.speak.engine.as_str() {
            "piper" => format!("lokální piper ({})", cfg.speak.piper_voice),
            "elevenlabs" => format!("ElevenLabs ({})", cfg.speak.voice_id),
            _ => format!("ElevenLabs ({}) + piper záloha", cfg.speak.voice_id),
        };
        println!(
            "  hlas:           zapnut — {engine}, cache {files} frází ({})",
            util::human_bytes(bytes)
        );
    } else {
        println!("  hlas:           vypnut v configu");
    }

    if cfg.converse.enabled {
        let convos_today = db::conversation_count_since(&conn, day_start)?;
        println!(
            "  konverzace:     zapnuta — oslovení „{}“; dnes {convos_today} výměn",
            cfg.converse.wake_words.join("“ / „")
        );
    } else {
        println!("  konverzace:     vypnuta v configu");
    }

    println!(
        "  ovládání oken:  {}",
        if cfg.wm.enabled {
            "zapnuto — hlasový agent smí `jarvis wm` (okna, klávesnice, myš)"
        } else {
            "pro agenta vypnuto (CLI `jarvis wm` funguje vždy)"
        }
    );

    if cfg.sms.enabled {
        println!("  sms:            zapnuty — {} → {}", cfg.sms.from, cfg.sms.to);
    } else {
        println!("  sms:            vypnuty v configu");
    }

    if cfg.runbooks.enabled {
        let rbs = crate::runbook::all(&conn)?;
        let active = rbs.iter().filter(|r| r.enabled).count();
        let pending = crate::runbook::pending_proposals(&conn)?.len();
        let last_run = crate::runbook::recent_runs(&conn, 1)?.into_iter().next();
        let last_txt = match last_run {
            Some(r) => format!(
                "poslední běh {} „{}“ {}",
                util::fmt_local(r.started_at),
                r.name,
                match (r.finished_at, r.exit_code) {
                    (Some(_), Some(0)) => "✓".into(),
                    (Some(_), Some(c)) => format!("✗ exit {c}"),
                    (Some(_), None) => "✗ timeout".into(),
                    (None, _) => "⚠ nedoběhl".into(),
                }
            ),
            None => "zatím žádný běh".into(),
        };
        println!(
            "  runbooky:       {active} aktivních ({} celkem), {pending} návrhů čeká; {last_txt}",
            rbs.len()
        );
        if cfg.runbooks.telegram_approve {
            println!(
                "                  vzdálené schvalování: Telegram {}",
                if config::telegram_keys(paths).is_ok() {
                    "nakonfigurován"
                } else {
                    "⚠ chybí TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID v secrets.env"
                }
            );
        }
    } else {
        println!("  runbooky:       vypnuty v configu");
    }

    let last_summary: Option<i64> = conn
        .query_row(
            "SELECT period_end FROM hourly_summaries ORDER BY period_end DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match last_summary {
        Some(ts) => println!("  analýza:        naposledy do {}", util::fmt_local(ts)),
        None => println!("  analýza:        zatím žádná"),
    }

    let today = util::today_local().format("%Y-%m-%d").to_string();
    let digest: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT status, sent_at FROM daily_digests WHERE date = ?1",
            [&today],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match digest {
        Some((status, Some(ts))) => println!("  digest dnes:    {status} ({})", util::fmt_local(ts)),
        Some((status, None)) => println!("  digest dnes:    {status}"),
        None => println!("  digest dnes:    zatím nevytvořen (plán: {}:00)", cfg.digest.hour),
    }

    let cost_today = db::cost_since(&conn, day_start)?;
    println!("  dnešní útrata:  {cost_today:.4} USD (strop {:.2} USD)", cfg.analysis.daily_budget_usd);

    let (bytes, files) = util::dir_size(&paths.shots_dir);
    println!("  úložiště:       {files} snímků, {}", util::human_bytes(bytes));
    Ok(())
}
