//! `jarvis kill` — hard emergency stop of the whole daemon. Two things, both best-effort:
//!   1. stop the systemd user units (services hold the daemons, timers fire oneshots),
//!   2. send SIGTERM to running foreground processes (`jarvis run/capture/listen/meet`).
//!
//! Deletes nothing, approves nothing, leaves no persistent flag — just shuts down
//! whatever is running now. `stop`, not `disable`, so it comes back after a reboot
//! (or `jarvis run`). This process (`jarvis kill`) never kills itself.

use anyhow::Result;
use std::process::Command;
use std::time::Duration;
use tracing::debug;

/// Is this argv a long-running jarvis daemon? Returns the subcommand name. Pure,
/// testable: basename argv[0] == "jarvis" and argv[1] is a daemon. Short-lived
/// commands (status, say, kill…) and foreign processes return None.
fn daemon_subcommand(args: &[&str]) -> Option<&'static str> {
    let argv0 = args.first()?;
    let base = argv0.rsplit('/').next().unwrap_or(argv0);
    if base != "jarvis" {
        return None;
    }
    match *args.get(1)? {
        "run" => Some("run"),
        "capture" => Some("capture"),
        "listen" => Some("listen"),
        "meet" => Some("meet"),
        _ => None,
    }
}

/// Scans /proc for running jarvis daemons (run/capture/listen/meet) other than us.
/// Returns (pid, subcommand name). Unavailable /proc → empty.
fn running_daemons() -> Vec<(i32, &'static str)> {
    let me = std::process::id() as i32;
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue; // process vanished meanwhile / no permission
        };
        let args: Vec<&str> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| std::str::from_utf8(s).ok())
            .collect();
        if let Some(sub) = daemon_subcommand(&args) {
            out.push((pid, sub));
        }
    }
    out
}

/// Which of `names` are active now. `systemctl --user is-active a b c` prints one
/// status per line in the same order; exit != 0 (something inactive) is ignored and
/// we read stdout. systemctl without a user bus → empty/garbage output → nothing.
fn active_units(names: &[&'static str]) -> Vec<&'static str> {
    let Ok(out) = Command::new("systemctl")
        .arg("--user")
        .arg("is-active")
        .args(names)
        .output()
    else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    names
        .iter()
        .zip(stdout.lines())
        .filter(|(_, st)| st.trim() == "active")
        .map(|(n, _)| *n)
        .collect()
}

/// Stops active systemd user units. If systemctl is missing (non-systemd machine)
/// or nothing runs, just reports it. Best-effort — errors are only printed.
fn stop_units() {
    // systemctl may not exist at all → skip silently
    if Command::new("systemctl").arg("--version").output().is_err() {
        println!("Systemd: systemctl nedostupné — units přeskakuji.");
        return;
    }
    let names = crate::units::unit_names();
    let active = active_units(&names);
    if active.is_empty() {
        println!("Systemd units: žádné aktivní.");
        return;
    }
    let mut args = vec!["--user", "stop"];
    args.extend(active.iter().copied());
    match Command::new("systemctl").args(&args).output() {
        Ok(o) if o.status.success() => {
            println!("Zastaveno {} units: {}", active.len(), active.join(", "));
        }
        Ok(o) => eprintln!(
            "systemctl stop skončil chybou: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("systemctl stop nelze spustit: {e}"),
    }
}

/// `jarvis kill`: hard stop. `no_units` = leave systemd alone, processes only.
/// `force` = wait after SIGTERM, then SIGKILL whatever is left.
pub fn run(no_units: bool, force: bool) -> Result<()> {
    if !no_units {
        stop_units();
    }

    let daemons = running_daemons();
    if daemons.is_empty() {
        println!("Foreground procesy (run/capture/listen/meet): žádné.");
        return Ok(());
    }
    for (pid, sub) in &daemons {
        // SIGTERM: let the daemon release its lock and flush the DB
        let rc = unsafe { libc::kill(*pid, libc::SIGTERM) };
        if rc == 0 {
            println!("SIGTERM → jarvis {sub} (pid {pid})");
        } else {
            eprintln!(
                "SIGTERM na pid {pid} selhal: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    if force {
        // brief grace period for cleanup, then finish off whatever didn't respond
        std::thread::sleep(Duration::from_secs(3));
        for (pid, sub) in &daemons {
            if unsafe { libc::kill(*pid, 0) } == 0 {
                // kill(pid, 0) == 0 → process still alive → SIGKILL
                let rc = unsafe { libc::kill(*pid, libc::SIGKILL) };
                if rc == 0 {
                    println!("SIGKILL → jarvis {sub} (pid {pid}) (nereagoval na SIGTERM)");
                }
            } else {
                debug!("jarvis {sub} (pid {pid}) po SIGTERM skončil");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_subcommand_matches_longrunners_only() {
        // installed binary and dev build, with extra args too
        assert_eq!(daemon_subcommand(&["/home/u/.cargo/bin/jarvis", "run"]), Some("run"));
        assert_eq!(daemon_subcommand(&["target/debug/jarvis", "capture"]), Some("capture"));
        assert_eq!(daemon_subcommand(&["jarvis", "listen", "--print-only"]), Some("listen"));
        assert_eq!(daemon_subcommand(&["jarvis", "meet", "https://meet…"]), Some("meet"));
        // short-lived commands are NOT killed (especially our own `kill`)
        assert_eq!(daemon_subcommand(&["jarvis", "kill"]), None);
        assert_eq!(daemon_subcommand(&["jarvis", "status"]), None);
        assert_eq!(daemon_subcommand(&["jarvis", "say", "run"]), None);
        assert_eq!(daemon_subcommand(&["jarvis"]), None);
        // a foreign process with the same subcommand in argv isn't matched
        assert_eq!(daemon_subcommand(&["/usr/bin/vim", "run"]), None);
        assert_eq!(daemon_subcommand(&["/opt/jarvisctl", "run"]), None);
        assert_eq!(daemon_subcommand(&[]), None);
    }
}
