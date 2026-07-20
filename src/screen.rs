//! Screen lock detection (XFCE screensaver via session D-Bus).
//!
//! Used by listening: when the screen is locked/off, the ambient mic daemon
//! transcribes nothing (privacy) — same path as `jarvis pause`. We query
//! `org.xfce.ScreenSaver.GetActive`, which is locale-independent (unlike the
//! localized output of `xfce4-screensaver-command -q`).
//!
//! Fail-open: when the state can't be determined (screensaver not running,
//! dbus-send missing, timeout), we treat it as unlocked — a transient D-Bus
//! error must not permanently silence the assistant. In a normal XFCE session
//! the screensaver runs and the query is reliable.

use std::path::Path;
use std::process::Command;
use tracing::debug;

/// Screen lock state.
pub enum Lock {
    /// Screensaver/lock is active → mic daemon pauses.
    Active,
    /// Unlocked, screensaver inactive.
    Inactive,
    /// State can't be determined (service not running / dbus-send missing /
    /// timeout) — listening keeps running (fail-open). Carries a short reason for `jarvis doctor`.
    Unknown(String),
}

/// Queries xfce4-screensaver via `org.xfce.ScreenSaver.GetActive`.
pub fn probe() -> Lock {
    // Test hook: force "locked" without touching the screensaver (verifies
    // the pause path end-to-end, including via `jarvis doctor`). Don't set in production.
    if matches!(std::env::var("JARVIS_FAKE_SCREEN_LOCKED").ok().as_deref(), Some("1") | Some("true")) {
        return Lock::Active;
    }
    let mut cmd = Command::new("dbus-send");
    cmd.args([
        "--session",
        // short cap: a healthy session bus replies within single-digit ms;
        // 500 ms ensures a stuck bus doesn't block the realtime loop (frames
        // pile up meanwhile in the 128-frame buffer, ~3.8 s of headroom)
        "--reply-timeout=500",
        "--print-reply=literal",
        "--dest=org.xfce.ScreenSaver",
        "/org/xfce/ScreenSaver",
        "org.xfce.ScreenSaver.GetActive",
    ]);
    // Under a systemd user service, DBUS_SESSION_BUS_ADDRESS may be absent
    // from the environment (jarvis-listen.service doesn't set it explicitly)
    // — derive the standard user-bus path, so the query works there too, not just from a terminal.
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
            // typically ServiceUnknown, when the screensaver isn't running
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

/// Is the screen locked / screensaver active? Both `Inactive` and `Unknown` → `false`
/// (fail-open: a transient D-Bus error must not silence listening).
pub fn locked() -> bool {
    matches!(probe(), Lock::Active)
}

/// Standard user session bus address, when the environment lacks it:
/// `$XDG_RUNTIME_DIR/bus`, fallback `/run/user/<uid>/bus`. None = no socket.
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

/// Extracts a boolean from a `dbus-send --print-reply=literal` reply (line
/// `   boolean true` / `   boolean false`). Also robust against a full
/// (non-literal) reply — we take the last token, since the header never contains `true`/`false`.
fn parse_active(stdout: &str) -> bool {
    matches!(stdout.split_whitespace().last(), Some("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_literal_reply() {
        // exactly what `dbus-send --print-reply=literal ... GetActive` returns
        assert!(parse_active("   boolean true\n"));
        assert!(!parse_active("   boolean false\n"));
    }

    #[test]
    fn parses_full_reply() {
        // non-literal variant: header + value on the second line
        let full = "method return time=1.2 sender=:1.24 -> destination=:1.9 serial=63 reply_serial=2\n   boolean true\n";
        assert!(parse_active(full));
        let full_false = "method return time=1.2 sender=:1.24 -> destination=:1.9 serial=63 reply_serial=2\n   boolean false\n";
        assert!(!parse_active(full_false));
    }

    #[test]
    fn empty_or_garbage_is_not_active() {
        // neither empty stdout (error) nor garbage may report "locked" (fail-open)
        assert!(!parse_active(""));
        assert!(!parse_active("\n"));
        assert!(!parse_active("Error: something broke"));
    }
}
