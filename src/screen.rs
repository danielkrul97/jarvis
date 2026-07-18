//! Detekce zámku obrazovky (XFCE screensaver přes session D-Bus).
//!
//! Slouží poslechu: když je obrazovka uzamčená/zhaslá, ambientní mic démon
//! nic nepřepisuje (soukromí) — stejná cesta jako `jarvis pause`. Ptáme se
//! `org.xfce.ScreenSaver.GetActive`, což je jazykově nezávislé (na rozdíl od
//! lokalizovaného výstupu `xfce4-screensaver-command -q`).
//!
//! Fail-open: když stav nejde zjistit (screensaver neběží, chybí dbus-send,
//! timeout), bereme jako odemčeno — přechodná chyba D-Bus nesmí asistenta
//! natrvalo umlčet. V běžné XFCE session screensaver běží a dotaz je spolehlivý.

use std::path::Path;
use std::process::Command;
use tracing::debug;

/// Stav zámku obrazovky.
pub enum Lock {
    /// Screensaver/zámek je aktivní → mic démon pauzuje.
    Active,
    /// Odemčeno, screensaver neaktivní.
    Inactive,
    /// Stav nejde zjistit (služba neběží / chybí dbus-send / timeout) — poslech
    /// běží dál (fail-open). Nese krátký důvod pro `jarvis doctor`.
    Unknown(String),
}

/// Dotáže se xfce4-screensaveru přes `org.xfce.ScreenSaver.GetActive`.
pub fn probe() -> Lock {
    // Testovací pojistka: vynuť „uzamčeno" bez sahání na screensaver (ověření
    // cesty pauzy end-to-end, i přes `jarvis doctor`). V provozu nenastavovat.
    if matches!(std::env::var("JARVIS_FAKE_SCREEN_LOCKED").ok().as_deref(), Some("1") | Some("true")) {
        return Lock::Active;
    }
    let mut cmd = Command::new("dbus-send");
    cmd.args([
        "--session",
        // krátký strop: zdravá session bus odpoví v jednotkách ms; 500 ms
        // pojistí, že zaseknutá sběrnice neblokuje realtime smyčku (rámce
        // se mezitím hromadí ve 128-frame bufferu, ~3,8 s rezerva)
        "--reply-timeout=500",
        "--print-reply=literal",
        "--dest=org.xfce.ScreenSaver",
        "/org/xfce/ScreenSaver",
        "org.xfce.ScreenSaver.GetActive",
    ]);
    // Pod systemd user službou nemusí být DBUS_SESSION_BUS_ADDRESS v prostředí
    // (jarvis-listen.service ho explicitně nenastavuje) — dopočítej standardní
    // cestu k user busu, ať dotaz funguje i tam, nejen z terminálu.
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        if let Some(addr) = user_bus_address() {
            cmd.env("DBUS_SESSION_BUS_ADDRESS", addr);
        }
    }
    match cmd.output() {
        Ok(o) if o.status.success() => {
            if parse_active(&String::from_utf8_lossy(&o.stdout)) {
                Lock::Active
            } else {
                Lock::Inactive
            }
        }
        Ok(o) => {
            // typicky ServiceUnknown, když screensaver neběží
            let stderr = String::from_utf8_lossy(&o.stderr);
            let why = stderr.lines().next().unwrap_or("").trim();
            let why =
                if why.is_empty() { "xfce4-screensaver neodpovídá".to_string() } else { why.to_string() };
            debug!("stav zámku nezjištěn: {why}");
            Lock::Unknown(why)
        }
        Err(e) => Lock::Unknown(format!("dbus-send: {e}")),
    }
}

/// Je obrazovka uzamčená / screensaver aktivní? `Inactive` i `Unknown` → `false`
/// (fail-open: přechodná chyba D-Bus nesmí poslech umlčet).
pub fn locked() -> bool {
    matches!(probe(), Lock::Active)
}

/// Standardní adresa user session busu, když ji prostředí nemá:
/// `$XDG_RUNTIME_DIR/bus`, fallback `/run/user/<uid>/bus`. None = socket není.
fn user_bus_address() -> Option<String> {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = Path::new(&rt).join("bus");
        if p.exists() {
            return Some(format!("unix:path={}", p.display()));
        }
    }
    let uid = unsafe { libc::getuid() };
    let p = format!("/run/user/{uid}/bus");
    Path::new(&p).exists().then(|| format!("unix:path={p}"))
}

/// Vytáhne boolean z odpovědi `dbus-send --print-reply=literal` (řádek
/// `   boolean true` / `   boolean false`). Robustní i vůči plné (ne-literal)
/// odpovědi — bereme poslední token, hlavička žádné `true`/`false` neobsahuje.
fn parse_active(stdout: &str) -> bool {
    matches!(stdout.split_whitespace().last(), Some("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_literal_reply() {
        // přesně to, co vrací `dbus-send --print-reply=literal ... GetActive`
        assert!(parse_active("   boolean true\n"));
        assert!(!parse_active("   boolean false\n"));
    }

    #[test]
    fn parses_full_reply() {
        // ne-literal varianta: hlavička + hodnota na druhém řádku
        let full = "method return time=1.2 sender=:1.24 -> destination=:1.9 serial=63 reply_serial=2\n   boolean true\n";
        assert!(parse_active(full));
        let full_false = "method return time=1.2 sender=:1.24 -> destination=:1.9 serial=63 reply_serial=2\n   boolean false\n";
        assert!(!parse_active(full_false));
    }

    #[test]
    fn empty_or_garbage_is_not_active() {
        // prázdný stdout (chyba) ani nesmysl nesmí hlásit „uzamčeno" (fail-open)
        assert!(!parse_active(""));
        assert!(!parse_active("\n"));
        assert!(!parse_active("Error: something broke"));
    }
}
