mod capture;
mod config;
mod digest;
mod mail;
mod patterns;
mod pipeline;
mod run;
mod status;
mod store;
mod units;
mod util;

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
    /// Vše v jednom procesu: capture + hodinová analýza + denní digest (bez systemd)
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
    /// Automatizační vzory: výpis a generování návrhů
    Propose {
        /// ID vzoru (viz --list); bez zadání vezme nejčastější kandidátní vzor
        #[arg(long)]
        pattern: Option<i64>,
        /// Vypiš detekované vzory
        #[arg(long)]
        list: bool,
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
        Cmd::Run => run::run_all(&paths, &cfg),
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
        Cmd::InstallUnits { print } => units::install(&cfg, print),
        Cmd::Propose { pattern, list } => {
            let conn = store::db::open(&paths.db_path)?;
            if list {
                patterns::print_list(&conn)
            } else {
                patterns::propose(&paths, &conn, pattern)
            }
        }
    }
}
