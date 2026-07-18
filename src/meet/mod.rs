//! `jarvis meet <URL>` — Jarvis se připojí do Google Meet jako samostatný
//! hlasový účastník (architektura B1):
//!
//! ```text
//! Jarvis TTS ─► mic_sink ─.monitor─► remap mic_source ─► Chrome (Meet uplink)
//! Meet downlink ─► Chrome ─► ear_sink ─.monitor─► whisper STT ─► converse
//! ```
//!
//! Orchestrace: virtuální audio → dedikovaný Chrome → vizuální připojení →
//! obousměrný audio bridge (reuse `listen` + `converse`, jen s přesměrovaným
//! zařízením a sinkem) → po odchodu shrnutí schůzky. Zdroj i Chrome se uklidí
//! i při pádu (Drop guardy).

pub mod audio;
pub mod browser;

use crate::config::{Config, Paths};
use crate::store::db;
use crate::{listen, util};
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{info, warn};

/// Nastaví se ze signálu (Ctrl-C / SIGTERM) i z hlídače Chrome; bridge ho čte.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Klon configu přesměrovaný na virtuální zařízení hovoru + zapnutá konverzace.
fn meet_config(cfg: &Config, va: &audio::VirtualAudio) -> Config {
    let mut m = cfg.clone();
    m.listen.enabled = true;
    m.listen.device = va.ear_monitor(); // Jarvis slyší ostatní účastníky
    m.speak.sink = va.mic_sink().to_string(); // Jarvisova řeč jde do hovoru
    m.converse.enabled = true; // wake-word odpovědi do hovoru
    m
}

pub fn run_meet(paths: &Paths, cfg: &Config, url: &str) -> Result<()> {
    if !cfg.meet.enabled {
        bail!("meet je vypnuté — nastav [meet] enabled = true");
    }
    if !url.contains("meet.google.com") {
        warn!("URL nevypadá jako Google Meet (pokračuji přesto): {url}");
    }
    let session_start = util::now_ts();

    // 1) virtuální audio (Drop uklidí i při pádu níž)
    let va = audio::VirtualAudio::ensure(&cfg.meet.mic_sink, &cfg.meet.mic_source, &cfg.meet.ear_sink)
        .context("příprava virtuálního audia selhala")?;

    // 2)+3) dedikovaný Chrome + vizuální připojení do hovoru.
    // JARVIS_MEET_NO_BROWSER = neřídit prohlížeč (audio bridge na už běžící
    // hovor / testování); jinak Jarvis spustí vlastní Chrome a připojí se sám.
    let manage_browser = std::env::var_os("JARVIS_MEET_NO_BROWSER").is_none();
    let mut chrome: Option<browser::Chrome> = if manage_browser {
        let chrome = browser::launch(paths, cfg, url, va.mic_source(), va.ear_sink())
            .context("spuštění Chrome selhalo")?;
        let jr = browser::join(paths, cfg).context("připojení do hovoru selhalo")?;
        if !jr.joined {
            bail!("nepřipojil jsem se do hovoru: {}", jr.note); // chrome se zabije přes Drop
        }
        info!("✅ v hovoru: {}", jr.note);
        Some(chrome)
    } else {
        warn!("JARVIS_MEET_NO_BROWSER — Chrome neřídím; předpokládám běžící hovor s audiem na virtuálních zařízeních");
        None
    };

    // 4) bridge: reuse listen (STT z ear monitoru) + converse (odpovědi do mic sinku)
    let mcfg = meet_config(cfg, &va);
    STOP.store(false, Ordering::SeqCst);
    // Ctrl-C / SIGTERM → korektní odchod (shrnutí + úklid), ne tvrdé zabití
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        // zápis do mrtvého warm-claude procesu nesmí zabít démona
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    // pozdrav do hovoru (a zároveň okamžitý self-check cesty TTS → mikrofon)
    let _ = crate::speak::say(
        paths,
        &mcfg,
        "Dobrý den, u hovoru je Jarvis. Kdykoli mě oslovte slovem Jarvisi.",
        None,
        true,
        false,
    );

    info!("bridge běží — Ctrl-C nebo zavření okna ukončí hovor");
    let bridge_res: Result<()> = std::thread::scope(|s| {
        let bridge = s.spawn(|| listen::run_listen_ex(paths, &mcfg, false, "meet", Some(&STOP)));
        // hlavní vlákno hlídá stav: signál / zavřený Chrome / konec bridge
        loop {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            if let Some(c) = chrome.as_mut() {
                if c.exited() {
                    info!("Chrome skončil (zavřené okno / konec hovoru) — ukončuji");
                    STOP.store(true, Ordering::SeqCst);
                    break;
                }
            }
            if bridge.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        STOP.store(true, Ordering::SeqCst); // ať bridge dojede, i když skončil Chrome
        bridge.join().map_err(|_| anyhow::anyhow!("bridge vlákno spadlo (panic)"))?
    });
    if let Err(e) = &bridge_res {
        warn!("bridge skončil chybou: {e:#}");
    }

    // 5) odchod z hovoru + úklid (chrome kill přes Drop; audio přes Drop)
    if let Some(c) = chrome.as_mut() {
        c.kill();
    }
    info!("odešel jsem z hovoru");

    // 6) poshovorové shrnutí
    if cfg.meet.summary && cfg.meet.summary_to != "none" {
        if let Err(e) = summarize_and_deliver(paths, cfg, session_start) {
            warn!("shrnutí schůzky selhalo: {e:#}");
        }
    }
    drop(va); // explicitní teardown zařízení (jinak by proběhl na konci scope)
    bridge_res
}

/// Sestaví přepis hovoru (utterances source=meet od začátku session), nechá
/// Claude vygenerovat shrnutí a rozešle ho dle `meet.summary_to`.
fn summarize_and_deliver(paths: &Paths, cfg: &Config, session_start: i64) -> Result<()> {
    let conn = db::open(&paths.db_path)?;
    let rows = db::utterances_between(&conn, session_start, util::now_ts() + 1)?;
    if rows.is_empty() {
        info!("žádný přepis hovoru — shrnutí přeskočeno");
        return Ok(());
    }
    // přepis s časovými značkami; strop na délku promptu
    let mut transcript = String::new();
    for r in &rows {
        transcript.push_str(&format!("[{}] {}\n", util::fmt_hm(r.ts_start), r.text.trim()));
    }
    const MAX: usize = 20_000;
    if transcript.len() > MAX {
        // posuň řez na nejbližší hranici znaku — bajtový offset by jinak padl
        // doprostřed vícebajtového UTF-8 znaku (český přepis je jich plný)
        // a `&transcript[cut..]` by panikl
        let mut cut = transcript.len() - MAX;
        while !transcript.is_char_boundary(cut) {
            cut += 1;
        }
        transcript = format!("(…začátek oříznut…)\n{}", &transcript[cut..]);
    }

    let prompt = format!(
        "Toto je přepis hlasové schůzky (Google Meet), kterého se Jarvis účastnil. \
         Shrň ji česky ve formátu Markdown se sekcemi: **Účel/téma**, **Klíčové body**, \
         **Rozhodnutí**, **Akční body** (kdo/co, pokud zaznělo), **Otevřené otázky**. \
         Buď věcný, nevymýšlej si nic, co v přepisu není. Pokud je přepis útržkovitý, \
         řekni to. Vrať jen Markdown, nic navíc.\n\n=== PŘEPIS ===\n{transcript}"
    );
    let model = (!cfg.digest.model.is_empty()).then_some(cfg.digest.model.as_str());
    let outcome = crate::pipeline::claude::run(&crate::pipeline::claude::ClaudeRequest {
        prompt,
        model,
        cwd: &paths.data_dir,
        allowed_tools: "Read",
        max_turns: 1,
        timeout: Duration::from_secs(180),
    })
    .context("Claude shrnutí selhalo")?;
    let markdown = outcome.text.trim().to_string();
    if markdown.is_empty() {
        bail!("Claude vrátil prázdné shrnutí");
    }
    // eviduj útratu
    let _ = db::insert_cost(
        &conn,
        util::now_ts(),
        "meet-summary",
        cfg.digest.model.as_str(),
        outcome.tokens_in,
        outcome.tokens_out,
        outcome.cost_usd,
    );

    let date = util::fmt_local(session_start);
    let subject = format!("Jarvis — shrnutí schůzky {date}");
    deliver(paths, cfg, &subject, &markdown)?;
    info!("shrnutí schůzky rozesláno ({} promluv)", rows.len());
    Ok(())
}

fn deliver(paths: &Paths, cfg: &Config, subject: &str, markdown: &str) -> Result<()> {
    let to = cfg.meet.summary_to.as_str();
    let mut errs = Vec::new();
    if matches!(to, "email" | "both") {
        if let Err(e) = deliver_email(paths, cfg, subject, markdown) {
            errs.push(format!("email: {e:#}"));
        }
    }
    if matches!(to, "telegram" | "both") {
        if let Err(e) = deliver_telegram(paths, subject, markdown) {
            errs.push(format!("telegram: {e:#}"));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        bail!("doručení shrnutí selhalo — {}", errs.join("; "))
    }
}

fn deliver_email(paths: &Paths, cfg: &Config, subject: &str, markdown: &str) -> Result<()> {
    let key = crate::config::sendgrid_key(paths)?;
    let html = format!(
        "<div style=\"font-family:sans-serif;max-width:640px\"><pre style=\"white-space:pre-wrap;\
         font-family:inherit\">{}</pre></div>",
        html_escape(markdown)
    );
    crate::mail::sendgrid::send(&cfg.email, &key, subject, markdown, &html)
        .context("SendGrid odeslání selhalo")?;
    Ok(())
}

fn deliver_telegram(paths: &Paths, subject: &str, markdown: &str) -> Result<()> {
    let (token, chat_id) = crate::config::telegram_keys(paths)?;
    crate::telegram::send_message(&token, &chat_id, &format!("*{subject}*\n\n{markdown}"))
        .context("Telegram odeslání selhalo")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
