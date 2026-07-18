//! Dedikovaný Chrome pro Google Meet + vizuální připojení do hovoru.
//!
//! Chrome běží ve vlastním profilu (izolace od uživatelova prohlížeče) a je
//! přes `PULSE_SINK`/`PULSE_SOURCE` napevno navázaný na Jarvisova virtuální
//! zařízení (ověřeno v P1: samotné env stačí). Připojení do hovoru řídí
//! vizuální agent — stejný osvědčený vzor jako converse (`jarvis wm`
//! screenshot → Read → klik), ale s ABSOLUTNÍ cestou k běžící binárce, aby
//! nezáleželo na PATH.

use crate::config::{Config, Paths};
use crate::pipeline::claude::{self, ClaudeRequest};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tracing::info;

/// Běžící dedikovaný Chrome; `Drop` ho zabije (jinak by zůstal v hovoru).
pub struct Chrome {
    child: Child,
    #[allow(dead_code)]
    profile: PathBuf,
}

impl Chrome {
    /// true, pokud Chrome mezitím skončil (zavřené okno / konec hovoru).
    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        self.kill();
    }
}

fn profile_dir(paths: &Paths, cfg: &Config) -> PathBuf {
    if cfg.meet.profile_dir.is_empty() {
        paths.data_dir.join("meet-profile")
    } else {
        PathBuf::from(&cfg.meet.profile_dir)
    }
}

/// Spustí dedikovaný Chrome navázaný na virtuální audio a otevře Meet URL.
/// Mikrofon se auto-povolí (`--use-fake-ui-for-media-stream`); video stroj
/// nemá, takže kamera je vypnutá sama.
pub fn launch(
    paths: &Paths,
    cfg: &Config,
    url: &str,
    mic_source: &str,
    ear_sink: &str,
) -> Result<Chrome> {
    let profile = profile_dir(paths, cfg);
    std::fs::create_dir_all(&profile)
        .with_context(|| format!("nelze vytvořit profil {}", profile.display()))?;
    let (out, err) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.data_dir.join("meet-chrome.log"))
    {
        Ok(f) => match f.try_clone() {
            Ok(f2) => (Stdio::from(f), Stdio::from(f2)),
            Err(_) => (Stdio::null(), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    let mut cmd = Command::new(&cfg.meet.chrome_bin);
    cmd.env("PULSE_SINK", ear_sink)
        .env("PULSE_SOURCE", mic_source)
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-session-crashed-bubble",
            "--disable-features=Translate,MediaRouter",
            "--use-fake-ui-for-media-stream", // auto-grant mikrofonu (reálné virtuální zařízení)
            "--autoplay-policy=no-user-gesture-required",
            "--start-maximized",
        ])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    let child = cmd
        .spawn()
        .with_context(|| format!("nelze spustit prohlížeč '{}'", cfg.meet.chrome_bin))?;
    info!("Chrome spuštěn (profil {}), otevírám {url}", profile.display());
    Ok(Chrome { child, profile })
}

/// Výsledek pokusu o připojení.
pub struct JoinResult {
    pub joined: bool,
    pub note: String,
}

#[derive(serde::Deserialize)]
struct JoinJson {
    #[serde(default)]
    joined: bool,
    #[serde(default)]
    note: String,
}

/// Vizuálně připojí Jarvise do hovoru: agent screenshotuje, čte, klikne
/// „Ask to join"/„Join now", vyplní jméno, počká na admit a ověří, že je
/// v hovoru. Vrací JSON kontrakt {joined, note}.
pub fn join(paths: &Paths, cfg: &Config) -> Result<JoinResult> {
    // Chrome potřebuje chvíli na načtení pre-join stránky Meetu
    std::thread::sleep(Duration::from_secs(6));
    let exe = std::env::current_exe().context("nelze zjistit cestu k jarvis binárce")?;
    let exe_s = exe.display().to_string();
    let tools = format!("Read,Bash({exe_s} wm:*)");
    let model = (!cfg.meet.join_model.is_empty()).then_some(cfg.meet.join_model.as_str());
    let outcome = claude::run(&ClaudeRequest {
        prompt: join_prompt(&exe_s, &cfg.meet.display_name),
        model,
        cwd: &paths.data_dir,
        allowed_tools: &tools,
        max_turns: cfg.meet.join_max_turns,
        timeout: Duration::from_secs(cfg.meet.join_timeout_s),
    })
    .context("vizuální join-agent selhal")?;
    Ok(parse_join(&outcome.text))
}

fn parse_join(text: &str) -> JoinResult {
    if let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) {
        if a <= b {
            if let Ok(j) = serde_json::from_str::<JoinJson>(&text[a..=b]) {
                return JoinResult { joined: j.joined, note: j.note };
            }
        }
    }
    // fallback: bez validního JSON považuj za neúspěch, ale nes s sebou text
    JoinResult {
        joined: false,
        note: format!("agent nevrátil validní JSON: {}", text.chars().take(200).collect::<String>()),
    }
}

fn join_prompt(exe: &str, display_name: &str) -> String {
    format!(
        r#"Jsi ovládací agent. Máš připojit uživatele do probíhajícího Google Meet hovoru,
který už je otevřený v okně Chrome. Ovládáš obrazovku výhradně příkazy `{exe} wm ...`
(Bash) a obrázky si prohlížíš nástrojem Read.

POSTUP (opakuj smyčku screenshot -> akce -> ověření):
1. Udělej screenshot: spusť `{exe} wm screenshot` — vypíše cestu k JPG. Ten JPG si
   otevři nástrojem Read a popiš, co vidíš. Rozlišení obrazovky je 1920x1080;
   souřadnice pro klikání ber přímo z obrázku v těchto pixelech.
2. Najdi předvstupní obrazovku Google Meet (okno Chrome, titulek obsahuje "Meet").
   Pokud se ještě načítá, počkej (`sleep 3`) a screenshotni znovu.
3. Je-li vidět textové pole "Your name" / "Vaše jméno" (připojení jako host),
   klikni do něj (`{exe} wm click X Y`) a napiš jméno: `{exe} wm type "{display_name}"`.
4. Mikrofon musí být ZAPNUTÝ (Jarvis přes něj mluví). Je-li tlačítko mikrofonu
   přeškrtnuté/červené (muted), klikni na něj a odmutuj. Kameru neřeš — stroj nemá
   webkameru, video je vypnuté.
5. Klikni na tlačítko připojení: "Ask to join" / "Join now" / "Požádat o připojení"
   / "Připojit se".
6. Po kliknutí screenshotni znovu. Jsi-li na čekací obrazovce ("Asking to join..."),
   počkej na vpuštění hostitelem (`sleep 10`, screenshot) a opakuj, dokud nezmizí
   (max do vyčerpání kol).
7. Připojení je hotové, když vidíš ovládací lištu hovoru (tlačítko pro zavěšení,
   ikony mikrofonu/prezentace/chatu, případně dlaždice účastníků).

Zásady: před psaním vždy klikni do cílového pole (píše se do zaměřeného okna).
Když si nejsi jistý stavem, udělej nový screenshot. Nevymýšlej si — jednej podle
toho, co reálně vidíš na obrázku.

Až skončíš (úspěšně nebo ne), vrať POSLEDNÍM řádkem POUZE striktní JSON:
{{"joined": true, "note": "stručně stav, např. v hovoru se 2 účastníky"}}
nebo {{"joined": false, "note": "proč se nepovedlo, např. čekání na admit vypršelo"}}"#
    )
}
