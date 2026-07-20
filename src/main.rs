mod capture;
mod config;
mod converse;
mod digest;
mod improve;
mod kill;
mod listen;
mod mail;
mod meet;
mod memory;
mod nudge;
mod patterns;
mod pipeline;
mod run;
mod runbook;
mod screen;
mod sms;
mod speak;
mod status;
mod store;
mod tasks;
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
    /// Capture daemon (foreground)
    Capture,
    /// Microphone listener: near-realtime speech transcription to the DB (foreground)
    Listen {
        /// Only print transcripts, don't write to DB
        #[arg(long)]
        print_only: bool,
        /// Run a WAV file through the pipeline (VAD + STT) instead of the mic, then exit
        #[arg(long)]
        wav: Option<String>,
        /// One-off override of listen.engine ("auto", "elevenlabs", "whisper")
        #[arg(long)]
        engine: Option<String>,
        /// One-off override of listen.model from config (benchmarking/tuning)
        #[arg(long)]
        model: Option<String>,
        /// One-off override of listen.language ("cs", "en", "auto")
        #[arg(long)]
        language: Option<String>,
        /// One-off override of listen.device (PulseAudio source)
        #[arg(long)]
        device: Option<String>,
        /// Download the configured whisper model, then exit
        #[arg(long)]
        download_model: bool,
    },
    /// Speaks text aloud (ElevenLabs TTS, Czech); reads stdin if no text given
    Say {
        /// Text to read aloud (multiple args are joined with a space)
        text: Vec<String>,
        /// One-off override of speak.voice_id (ElevenLabs only, no fallback)
        #[arg(long)]
        voice: Option<String>,
        /// Save audio to a file instead of playing it (ElevenLabs: mp3, piper: wav)
        #[arg(long)]
        out: Option<String>,
        /// Skip the cache — always resynthesize (burns credits on ElevenLabs)
        #[arg(long)]
        no_cache: bool,
        /// Force local synthesis (piper) — ElevenLabs is never called
        #[arg(long, conflicts_with = "voice")]
        local: bool,
        /// Download the piper voice from config (speak.piper_voice), then exit
        #[arg(long)]
        download_model: bool,
        /// List voices on the account, then exit (requires a key with voices_read)
        #[arg(long)]
        list_voices: bool,
    },
    /// Ask Jarvis via text (same loop as the voice dialog, no mic)
    Converse {
        /// Question; reads stdin if not given
        text: Vec<String>,
        /// Just print the answer, don't speak
        #[arg(long)]
        mute: bool,
    },
    /// Kill-gate for the open-ear classifier: labeled JSONL corpus → confusion matrix
    ConverseEval {
        /// JSONL corpus {"text","label"} (label = directed|human|background)
        file: Option<std::path::PathBuf>,
        /// Instead of evaluating, print a corpus template from the last N mic utterances
        #[arg(long, value_name = "N")]
        from_db: Option<usize>,
    },
    /// Joins Jarvis into Google Meet as a voice participant (foreground; Ctrl-C to quit)
    Meet {
        /// Call URL, e.g. https://meet.google.com/abc-defg-hij
        url: String,
    },
    /// Everything in one process: capture + listen + hourly analysis + daily digest (no systemd)
    Run,
    /// Extracts activity for the elapsed period (run hourly)
    Analyze {
        /// Just print what would be analyzed — no Claude call, no writes
        #[arg(long)]
        dry_run: bool,
        /// Process the last N hours instead of from the watermark
        #[arg(long)]
        window_hours: Option<u64>,
    },
    /// Builds (and optionally sends) the daily digest
    Digest {
        /// Date YYYY-MM-DD (default: today)
        #[arg(long)]
        date: Option<String>,
        /// Send by email via SendGrid (respects an already-sent digest)
        #[arg(long)]
        send: bool,
        /// Resend even a digest already sent today (otherwise a sent one is skipped)
        #[arg(long, requires = "send")]
        resend: bool,
        /// Save HTML for preview, send nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Sends a test email (verifies SendGrid)
    SendTest,
    /// Sends an SMS via Twilio (default recipient sms.to from config)
    Sms {
        /// Message text; reads stdin if not given
        text: Vec<String>,
        /// Recipient in E.164 (+420…); overrides sms.to
        #[arg(long)]
        to: Option<String>,
        /// Don't wait for the delivery receipt — just send and print the SID
        #[arg(long)]
        no_wait: bool,
    },
    /// Pauses capture, e.g. `jarvis pause 30m`
    Pause { duration: String },
    /// Resumes capture
    Resume,
    /// Emergency hard stop: stops systemd units and sends SIGTERM to running daemons
    Kill {
        /// Don't touch systemd units, just signal foreground processes
        #[arg(long)]
        no_units: bool,
        /// After SIGTERM, wait and SIGKILL unresponsive processes
        #[arg(long)]
        force: bool,
    },
    /// Status: last sample, today's spend, digest…
    Status,
    /// Checks environment and prerequisites
    Doctor {
        /// Live checks (SendGrid sandbox send, claude ping) — costs a few tokens
        #[arg(long)]
        live: bool,
    },
    /// Deletes screenshots older than the given period
    Purge {
        #[arg(long, default_value = "7d")]
        older_than: String,
    },
    /// Installs and activates systemd user units
    InstallUnits {
        /// Just print the unit contents, install nothing
        #[arg(long)]
        print: bool,
    },
    /// Window, keyboard, and mouse control (X11) — also used by voice Jarvis
    Wm {
        #[command(subcommand)]
        cmd: wm::WmCmd,
    },
    /// Automation patterns: listing and generating proposals
    Propose {
        /// Pattern ID (see --list); defaults to the most frequent candidate pattern
        #[arg(long)]
        pattern: Option<i64>,
        /// List detected patterns
        #[arg(long)]
        list: bool,
    },
    /// Approved automations (phase D): approval, execution, history
    Runbook {
        #[command(subcommand)]
        cmd: runbook::RunbookCmd,
    },
    /// Scheduled internal tasks: dependency self-management and maintenance (list, run, schedule)
    Tasks {
        #[command(subcommand)]
        cmd: tasks::TasksCmd,
    },
    /// Long-term memory: facts about the master (list, search, add, consolidate)
    Memory {
        #[command(subcommand)]
        cmd: memory::MemoryCmd,
    },
    /// Proactive layer: one tick (detection → nudge); --dry-run just prints
    Nudge {
        /// Just show what the layer would do now — no API, writes, speech, or Telegram
        #[arg(long)]
        dry_run: bool,
    },
    /// Kill-gate for the proactive classifier: labeled JSONL → false-interrupt rate
    NudgeEval {
        /// JSONL corpus {"evidence","label"} (label = worth|noise)
        file: Option<std::path::PathBuf>,
        /// Instead of evaluating, print a corpus template from the last N mic utterances
        #[arg(long, value_name = "N")]
        from_db: Option<usize>,
    },
    /// Kill-gate for the reprompt gate (clean, free): labeled JSONL → false-reject rate
    RepromptEval {
        /// JSONL corpus {"text","label"} (label = real|junk)
        file: Option<std::path::PathBuf>,
        /// Instead of evaluating, print a template from the last N real questions (conversations)
        #[arg(long, value_name = "N")]
        from_db: Option<usize>,
    },
    /// Self-improvement: Jarvis develops and improves its own code, tracked in git
    Improve {
        #[command(subcommand)]
        cmd: improve::ImproveCmd,
    },
}

fn main() -> Result<()> {
    // CLI behavior: `jarvis status | head` must not panic on broken pipe
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
            cfg.validate()?; // also validate overridden values (--engine/--language/--model)
            if download_model {
                listen::download(&paths, &cfg)
            } else if let Some(w) = wav {
                listen::run_wav(&paths, &cfg, std::path::Path::new(&w))
            } else {
                // the daemon writes to the warm process's stdin — EPIPE must not kill it
                // (the SIG_DFL above is for CLI pipes like `jarvis status | head`)
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
                // same streamed path as the voice daemon (warm + incremental
                // synthesis per sentence); --mute just prints sentences with timing instead of speaking
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
            // same reason as `listen`: warm claude in the conversation worker
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            }
            run::run_all(&paths, &cfg)
        }
        Cmd::Analyze { dry_run, window_hours } => {
            let conn = store::db::open(&paths.db_path)?;
            pipeline::analyze::run(&paths, &cfg, &conn, dry_run, window_hours)
        }
        Cmd::Digest { date, send, resend, dry_run } => {
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
                // NOTE: scheduled send (systemd digest.timer, `digest --send`) MUST NOT
                // force — force bypasses idempotency via the `sent` status, and racing
                // with the hourly retry_pending would send the digest twice. Force only
                // on explicit `--resend` (a deliberate resend of an already-sent digest).
                if digest::send_stored(&paths, &cfg, &conn, &date_str, resend)? {
                    println!("Digest {date_str} odeslán na {}.", cfg.email.to);
                    speak::announce(&paths, &cfg, speak::DIGEST_ANNOUNCEMENT);
                } else {
                    println!(
                        "Digest {date_str} se neodeslal — už je odeslaný (přeposlat: \
                         `jarvis digest --send --resend`), nebo ho právě posílá jiný proces."
                    );
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
        Cmd::Kill { no_units, force } => kill::run(no_units, force),
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
        Cmd::Tasks { cmd } => {
            let conn = store::db::open(&paths.db_path)?;
            tasks::cli(&paths, &cfg, &conn, cmd)
        }
        Cmd::Memory { cmd } => {
            let conn = store::db::open(&paths.db_path)?;
            memory::cli(&paths, &cfg, &conn, cmd)
        }
        Cmd::Nudge { dry_run } => {
            let conn = store::db::open(&paths.db_path)?;
            if dry_run {
                nudge::run_dry(&paths, &cfg, &conn)
            } else {
                nudge::tick(&paths, &cfg, &conn);
                Ok(())
            }
        }
        Cmd::NudgeEval { file, from_db } => {
            if let Some(n) = from_db {
                nudge::eval_scaffold(&paths, n)
            } else if let Some(f) = file {
                nudge::eval(&paths, &cfg, &f)
            } else {
                anyhow::bail!("zadej JSONL korpus, nebo --from-db N pro šablonu")
            }
        }
        Cmd::RepromptEval { file, from_db } => {
            if let Some(n) = from_db {
                converse::eval_reprompt_scaffold(&paths, n)
            } else if let Some(f) = file {
                converse::eval_reprompt(&paths, &cfg, &f)
            } else {
                anyhow::bail!("zadej JSONL korpus, nebo --from-db N pro šablonu")
            }
        }
        Cmd::Improve { cmd } => {
            let conn = store::db::open(&paths.db_path)?;
            improve::cli(&paths, &cfg, &conn, cmd)
        }
    }
}
