mod capture;
mod config;
mod converse;
mod digest;
mod listen;
mod mail;
mod meet;
mod patterns;
mod pipeline;
mod run;
mod runbook;
mod screen;
mod sms;
mod speak;
mod status;
mod store;
mod telegram;
mod units;
mod util;
mod wm;
mod x11util;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "jarvis",
    version,
    about = "Osobní Jarvis — sleduje práci na X11, extrahuje aktivitu a posílá denní digest e-mailem"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Snímací démon (foreground)
    Capture,
    /// Poslech mikrofonu: near-realtime přepis řeči do databáze (foreground)
    Listen {
        /// Přepisy jen vypisuj, nezapisuj do DB
        #[arg(long)]
        print_only: bool,
        /// Prožeň WAV soubor pipeline (VAD + STT) místo mikrofonu a skonči
        #[arg(long)]
        wav: Option<String>,
        /// Jednorázově přebij listen.engine ("auto", "elevenlabs", "whisper")
        #[arg(long)]
        engine: Option<String>,
        /// Jednorázově přebij listen.model z configu (benchmark/ladění)
        #[arg(long)]
        model: Option<String>,
        /// Jednorázově přebij listen.language ("cs", "en", "auto")
        #[arg(long)]
        language: Option<String>,
        /// Jednorázově přebij listen.device (PulseAudio source)
        #[arg(long)]
        device: Option<String>,
        /// Stáhni nakonfigurovaný whisper model a skonči
        #[arg(long)]
        download_model: bool,
    },
    /// Řekne text nahlas (ElevenLabs TTS, česky); bez textu čte stdin
    Say {
        /// Text k přečtení (víc argumentů se spojí mezerou)
        text: Vec<String>,
        /// Jednorázově přebij speak.voice_id (jen ElevenLabs, bez fallbacku)
        #[arg(long)]
        voice: Option<String>,
        /// Ulož audio do souboru místo přehrání (ElevenLabs: mp3, piper: wav)
        #[arg(long)]
        out: Option<String>,
        /// Vynech cache — vždy syntetizuj znovu (u ElevenLabs spálí kredity)
        #[arg(long)]
        no_cache: bool,
        /// Vynuť lokální syntézu (piper) — ElevenLabs se vůbec nevolá
        #[arg(long, conflicts_with = "voice")]
        local: bool,
        /// Stáhni piper hlas z configu (speak.piper_voice) a skonči
        #[arg(long)]
        download_model: bool,
        /// Vypiš hlasy v účtu a skonči (vyžaduje klíč s voices_read)
        #[arg(long)]
        list_voices: bool,
    },
    /// Zeptej se Jarvise textem (stejná smyčka jako hlasový dialog, bez mikrofonu)
    Converse {
        /// Otázka; bez zadání čte stdin
        text: Vec<String>,
        /// Jen vypiš odpověď, nemluv
        #[arg(long)]
        mute: bool,
    },
    /// Kill-gate open-ear klasifikátoru: olabelovaný JSONL korpus → confusion matrix
    ConverseEval {
        /// JSONL korpus {"text","label"} (label = directed|human|background)
        file: Option<std::path::PathBuf>,
        /// Místo vyhodnocení vypiš šablonu korpusu z posledních N mic promluv
        #[arg(long, value_name = "N")]
        from_db: Option<usize>,
    },
    /// Připojí Jarvise do Google Meet jako hlasového účastníka (foreground; Ctrl-C ukončí)
    Meet {
        /// URL hovoru, např. https://meet.google.com/abc-defg-hij
        url: String,
    },
    /// Vše v jednom procesu: capture + poslech + hodinová analýza + denní digest (bez systemd)
    Run,
    /// Extrakce aktivity za uplynulé období (spouštět každou hodinu)
    Analyze {
        /// Jen vypiš, co by se analyzovalo — bez volání Claude a bez zápisu
        #[arg(long)]
        dry_run: bool,
        /// Zpracuj posledních N hodin místo od watermarky
        #[arg(long)]
        window_hours: Option<u64>,
    },
    /// Sestaví (a případně odešle) denní digest
    Digest {
        /// Datum YYYY-MM-DD (výchozí dnes)
        #[arg(long)]
        date: Option<String>,
        /// Odešli e-mailem přes SendGrid
        #[arg(long)]
        send: bool,
        /// Ulož HTML k náhledu, nic neposílej
        #[arg(long)]
        dry_run: bool,
    },
    /// Pošle testovací e-mail (ověření SendGrid)
    SendTest,
    /// Pošle SMS přes Twilio (výchozí příjemce sms.to z configu)
    Sms {
        /// Text zprávy; bez zadání čte stdin
        text: Vec<String>,
        /// Příjemce v E.164 (+420…); přebíjí sms.to
        #[arg(long)]
        to: Option<String>,
        /// Nečekat na doručenku — jen odeslat a vypsat SID
        #[arg(long)]
        no_wait: bool,
    },
    /// Pozastaví snímání, např. `jarvis pause 30m`
    Pause { duration: String },
    /// Obnoví snímání
    Resume,
    /// Stav: poslední vzorek, dnešní útrata, digest…
    Status,
    /// Kontrola prostředí a prerekvizit
    Doctor {
        /// Živé kontroly (SendGrid sandbox send, claude ping) — stojí pár tokenů
        #[arg(long)]
        live: bool,
    },
    /// Smaže screenshoty starší než zadané období
    Purge {
        #[arg(long, default_value = "7d")]
        older_than: String,
    },
    /// Nainstaluje a aktivuje systemd user units
    InstallUnits {
        /// Jen vypiš obsah units, nic neinstaluj
        #[arg(long)]
        print: bool,
    },
    /// Ovládání oken, klávesnice a myši (X11) — používá i hlasový Jarvis
    Wm {
        #[command(subcommand)]
        cmd: wm::WmCmd,
    },
    /// Automatizační vzory: výpis a generování návrhů
    Propose {
        /// ID vzoru (viz --list); bez zadání vezme nejčastější kandidátní vzor
        #[arg(long)]
        pattern: Option<i64>,
        /// Vypiš detekované vzory
        #[arg(long)]
        list: bool,
    },
    /// Schválené automatizace (fáze D): schvalování, spouštění, historie
    Runbook {
        #[command(subcommand)]
        cmd: runbook::RunbookCmd,
    },
}

fn main() -> Result<()> {
    // CLI chování: `jarvis status | head` nesmí panikařit na broken pipe
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let paths = config::Paths::new()?;
    paths.ensure_dirs()?;
    let cfg = config::Config::load(&paths)?;

    match cli.cmd {
        Cmd::Capture => {
            let _lock = capture::acquire_lock(&paths)?;
            capture::run_capture(&paths, &cfg)
        }
        Cmd::Listen { print_only, wav, engine, model, language, device, download_model } => {
            let mut cfg = cfg;
            if let Some(e) = engine {
                cfg.listen.engine = e;
            }
            if let Some(m) = model {
                cfg.listen.model = m;
                cfg.listen.model_path = String::new();
            }
            if let Some(l) = language {
                cfg.listen.language = l;
            }
            if let Some(d) = device {
                cfg.listen.device = d;
            }
            cfg.validate()?; // přebité hodnoty (--engine/--language/--model) taky ověř
            if download_model {
                listen::download(&paths, &cfg)
            } else if let Some(w) = wav {
                listen::run_wav(&paths, &cfg, std::path::Path::new(&w))
            } else {
                // démon píše do stdin warm procesu — EPIPE nesmí zabít proces
                // (SIG_DFL výš je kvůli CLI rourám typu `jarvis status | head`)
                unsafe {
                    libc::signal(libc::SIGPIPE, libc::SIG_IGN);
                }
                listen::run_listen(&paths, &cfg, print_only)
            }
        }
        Cmd::Say { text, voice, out, no_cache, local, download_model, list_voices } => {
            if download_model {
                let p = speak::piper::download_voice(&paths, &cfg.speak)?;
                println!("Piper hlas připraven: {}", p.display());
                Ok(())
            } else if list_voices {
                let key = config::elevenlabs_key(&paths)?;
                let voices = speak::tts::list_voices(&key)?;
                if voices.is_empty() {
                    println!("V účtu nejsou žádné hlasy.");
                }
                for v in voices {
                    println!("{}  {:20}  [{}] {}", v.id, v.name, v.category, v.labels);
                }
                Ok(())
            } else {
                let text = if text.is_empty() {
                    use std::io::Read;
                    let mut s = String::new();
                    std::io::stdin()
                        .read_to_string(&mut s)
                        .context("čtení textu ze stdin selhalo")?;
                    s
                } else {
                    text.join(" ")
                };
                if let Some(out) = out {
                    let p = speak::synth(&paths, &cfg, &text, voice.as_deref(), !no_cache, local)?;
                    std::fs::copy(&p, &out)
                        .with_context(|| format!("nelze zapsat {out}"))?;
                    println!("Audio uloženo: {out}");
                    Ok(())
                } else {
                    speak::say(&paths, &cfg, &text, voice.as_deref(), !no_cache, local)
                }
            }
        }
        Cmd::Converse { text, mute } => {
            let question = if text.is_empty() {
                use std::io::Read;
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .context("čtení otázky ze stdin selhalo")?;
                s
            } else {
                text.join(" ")
            };
            let conn = store::db::open(&paths.db_path)?;
            if cfg.converse.respect_budget && converse::over_budget(&cfg, &conn)? {
                let answer = converse::BUDGET_REPLY;
                println!("{answer}");
                if !mute && cfg.speak.enabled {
                    if let Err(e) = speak::say_once(&paths, &cfg, answer) {
                        tracing::warn!("hlas selhal (odpověď je vypsaná výš): {e:#}");
                    }
                }
            } else {
                // stejná streamovaná cesta jako hlasový démon (warm + průběžná
                // syntéza po větách); --mute jen tiskne věty s časem místo mluvení
                let answer = converse::converse_cli(&paths, &cfg, &conn, question.trim(), mute)?;
                println!("{answer}");
            }
            Ok(())
        }
        Cmd::ConverseEval { file, from_db } => {
            if let Some(n) = from_db {
                converse::eval_scaffold(&paths, n)
            } else if let Some(f) = file {
                converse::eval_open_ear(&paths, &cfg, &f)
            } else {
                anyhow::bail!("zadej JSONL korpus, nebo --from-db N pro šablonu")
            }
        }
        Cmd::Meet { url } => meet::run_meet(&paths, &cfg, &url),
        Cmd::Run => {
            // stejný důvod jako u `listen`: warm claude v konverzačním workeru
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            }
            run::run_all(&paths, &cfg)
        }
        Cmd::Analyze { dry_run, window_hours } => {
            let conn = store::db::open(&paths.db_path)?;
            pipeline::analyze::run(&paths, &cfg, &conn, dry_run, window_hours)
        }
        Cmd::Digest { date, send, dry_run } => {
            let conn = store::db::open(&paths.db_path)?;
            let d = match date {
                Some(s) => chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                    .context("datum čekám ve tvaru YYYY-MM-DD")?,
                None => util::today_local(),
            };
            let date_str = d.format("%Y-%m-%d").to_string();
            let (md, html) = digest::build::build(&paths, &cfg, &conn, d, !dry_run)?;
            if dry_run {
                let out = paths.data_dir.join(format!("digest-{date_str}.html"));
                std::fs::write(&out, &html)?;
                println!("Náhled HTML: {} (nikam se neodesílá, do DB se neukládá)", out.display());
                println!("─── Markdown ───");
                println!("{md}");
            } else if send {
                if digest::send_stored(&paths, &cfg, &conn, &date_str, true)? {
                    println!("Digest {date_str} odeslán na {}.", cfg.email.to);
                    speak::announce(&paths, &cfg, speak::DIGEST_ANNOUNCEMENT);
                } else {
                    println!("Digest {date_str} právě odesílá jiný proces — neodesílám podruhé.");
                }
            } else {
                println!(
                    "Digest {date_str} sestaven a uložen. Odešle se automaticky po {}:00 \
                     hodinovou doručovací smyčkou, nebo hned: `jarvis digest --send`.",
                    cfg.digest.hour
                );
            }
            Ok(())
        }
        Cmd::SendTest => {
            let key = config::sendgrid_key(&paths)?;
            let today = util::today_local();
            let md = "# Jarvis — testovací e-mail\n\nPokud tohle čteš, SendGrid pipeline funguje. ✅\n";
            let html = digest::render::render_email(md, today);
            let msg_id = mail::sendgrid::send(
                &cfg.email,
                &key,
                "Jarvis — testovací e-mail",
                md,
                &html,
            )?;
            println!(
                "Odesláno na {} (X-Message-Id: {}). Zkontroluj inbox.",
                cfg.email.to,
                msg_id.unwrap_or_else(|| "—".into())
            );
            Ok(())
        }
        Cmd::Sms { text, to, no_wait } => {
            anyhow::ensure!(
                cfg.sms.enabled,
                "[sms] není zapnuto — doplň sekci [sms] do ~/.config/jarvis/config.toml"
            );
            let text = if text.is_empty() {
                use std::io::Read;
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .context("čtení textu ze stdin selhalo")?;
                s
            } else {
                text.join(" ")
            };
            let text = text.trim().to_string();
            let recipient = to.unwrap_or_else(|| cfg.sms.to.clone());
            let (sid, token) = config::twilio_keys(&paths)?;
            let msg_sid = sms::send(&cfg.sms, &sid, &token, &recipient, &text)?;
            let price = if no_wait {
                println!("SMS předána Twiliu (SID {msg_sid}), na doručenku nečekám.");
                None
            } else {
                let (status, price) =
                    sms::wait_final(&sid, &token, &msg_sid, std::time::Duration::from_secs(30))?;
                println!(
                    "SMS pro {recipient}: {status}{} (SID {msg_sid})",
                    price.map(|p| format!(", cena {p:.4} USD")).unwrap_or_default()
                );
                price
            };
            let conn = store::db::open(&paths.db_path)?;
            let chars = text.chars().count() as i64;
            if let Err(e) = store::db::insert_cost(
                &conn, util::now_ts(), "sms", "twilio", chars, 0, price.unwrap_or(0.0),
            ) {
                tracing::warn!("zápis útraty SMS selhal: {e:#}");
            }
            Ok(())
        }
        Cmd::Pause { duration } => {
            let secs = config::parse_duration_spec(&duration)?;
            let conn = store::db::open(&paths.db_path)?;
            let until = util::now_ts() + secs as i64;
            store::db::state_set(&conn, "pause_until", &until.to_string())?;
            println!("Snímání pozastaveno do {}.", util::fmt_local(until));
            Ok(())
        }
        Cmd::Resume => {
            let conn = store::db::open(&paths.db_path)?;
            store::db::state_del(&conn, "pause_until")?;
            println!("Snímání obnoveno.");
            Ok(())
        }
        Cmd::Status => status::status(&paths, &cfg),
        Cmd::Doctor { live } => status::doctor(&paths, &cfg, live),
        Cmd::Purge { older_than } => {
            let secs = config::parse_duration_spec(&older_than)?;
            let conn = store::db::open(&paths.db_path)?;
            let n = store::retention::purge(&conn, &paths.data_dir, secs as i64)?;
            println!("Odstraněno {n} snímků starších než {older_than}.");
            Ok(())
        }
        Cmd::Wm { cmd } => wm::cli(&paths, &cfg, cmd),
        Cmd::InstallUnits { print } => units::install(&cfg, print),
        Cmd::Propose { pattern, list } => {
            let conn = store::db::open(&paths.db_path)?;
            if list {
                patterns::print_list(&conn)
            } else {
                patterns::propose(&paths, &cfg, &conn, pattern)
            }
        }
        Cmd::Runbook { cmd } => {
            let conn = store::db::open(&paths.db_path)?;
            runbook::cli(&paths, &cfg, &conn, cmd)
        }
    }
}
