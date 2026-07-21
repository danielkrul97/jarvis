use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// Generates the content of systemd user units. `exec` = absolute path to the binary,
/// `display`/`xauthority` are baked in from the current session (capture needs them).
pub fn unit_files(
    exec: &str,
    display: &str,
    xauthority: Option<&str>,
    digest_hour: u8,
    consolidate_hour: u8,
) -> Vec<(&'static str, String)> {
    let xauth_line = xauthority
        .map(|x| format!("Environment=XAUTHORITY={x}\n"))
        .unwrap_or_default();
    vec![
        (
            "jarvis-capture.service",
            format!(
                "[Unit]\n\
                 Description=Jarvis — snímání X11 aktivity\n\
                 After=graphical-session.target\n\n\
                 [Service]\n\
                 ExecStart={exec} capture\n\
                 Restart=always\n\
                 RestartSec=5\n\
                 Environment=DISPLAY={display}\n\
                 {xauth_line}\
                 EnvironmentFile=-%h/.config/jarvis/secrets.env\n\n\
                 [Install]\n\
                 WantedBy=default.target\n"
            ),
        ),
        (
            "jarvis-listen.service",
            format!(
                "[Unit]\n\
                 Description=Jarvis — poslech mikrofonu (near-realtime STT)\n\
                 After=graphical-session.target pulseaudio.service pipewire-pulse.service\n\n\
                 [Service]\n\
                 ExecStart={exec} listen\n\
                 Restart=always\n\
                 RestartSec=5\n\
                 EnvironmentFile=-%h/.config/jarvis/secrets.env\n\n\
                 [Install]\n\
                 WantedBy=default.target\n"
            ),
        ),
        (
            "jarvis-analyze.service",
            format!(
                "[Unit]\n\
                 Description=Jarvis — hodinová extrakce aktivity\n\n\
                 [Service]\n\
                 Type=oneshot\n\
                 ExecStart={exec} analyze\n\
                 EnvironmentFile=-%h/.config/jarvis/secrets.env\n"
            ),
        ),
        (
            "jarvis-analyze.timer",
            "[Unit]\n\
             Description=Jarvis — hodinová extrakce (timer)\n\n\
             [Timer]\n\
             OnCalendar=hourly\n\
             Persistent=true\n\
             RandomizedDelaySec=120\n\n\
             [Install]\n\
             WantedBy=timers.target\n"
                .to_string(),
        ),
        (
            "jarvis-digest.service",
            format!(
                "[Unit]\n\
                 Description=Jarvis — denní digest e-mailem\n\n\
                 [Service]\n\
                 Type=oneshot\n\
                 ExecStart={exec} digest --send\n\
                 EnvironmentFile=-%h/.config/jarvis/secrets.env\n"
            ),
        ),
        (
            "jarvis-digest.timer",
            format!(
                "[Unit]\n\
                 Description=Jarvis — denní digest (timer)\n\n\
                 [Timer]\n\
                 OnCalendar=*-*-* {digest_hour:02}:00:00\n\
                 Persistent=true\n\n\
                 [Install]\n\
                 WantedBy=timers.target\n"
            ),
        ),
        (
            "jarvis-runbooks.service",
            format!(
                "[Unit]\n\
                 Description=Jarvis — schválené automatizace (run-due)\n\n\
                 [Service]\n\
                 Type=oneshot\n\
                 ExecStart={exec} runbook run-due\n\
                 Environment=DISPLAY={display}\n\
                 {xauth_line}\
                 EnvironmentFile=-%h/.config/jarvis/secrets.env\n"
            ),
        ),
        (
            "jarvis-runbooks.timer",
            "[Unit]\n\
             Description=Jarvis — schválené automatizace (timer à 5 min)\n\n\
             [Timer]\n\
             OnCalendar=*:0/5\n\
             Persistent=true\n\
             RandomizedDelaySec=15\n\n\
             [Install]\n\
             WantedBy=timers.target\n"
                .to_string(),
        ),
        (
            "jarvis-memory.service",
            format!(
                "[Unit]\n\
                 Description=Jarvis — noční konsolidace paměti (fakta + embeddingy)\n\n\
                 [Service]\n\
                 Type=oneshot\n\
                 ExecStart={exec} memory consolidate\n\
                 EnvironmentFile=-%h/.config/jarvis/secrets.env\n"
            ),
        ),
        (
            "jarvis-memory.timer",
            format!(
                "[Unit]\n\
                 Description=Jarvis — konsolidace paměti (denní timer)\n\n\
                 [Timer]\n\
                 OnCalendar=*-*-* {consolidate_hour:02}:30:00\n\
                 Persistent=true\n\
                 RandomizedDelaySec=300\n\n\
                 [Install]\n\
                 WantedBy=timers.target\n"
            ),
        ),
        (
            "jarvis-tasks.service",
            format!(
                "[Unit]\n\
                 Description=Jarvis — plánované interní úlohy (samospráva závislostí, údržba)\n\n\
                 [Service]\n\
                 Type=oneshot\n\
                 ExecStart={exec} tasks run-due\n\
                 EnvironmentFile=-%h/.config/jarvis/secrets.env\n"
            ),
        ),
        (
            // hourly tick: run-due itself handles each task's due schedule (deps every 24h,
            // cleanup and maintenance daily) — the timer just "pings" often enough.
            // Persistent catches up a missed window after the machine wakes.
            "jarvis-tasks.timer",
            "[Unit]\n\
             Description=Jarvis — plánované úlohy (timer, hodinová otočka)\n\n\
             [Timer]\n\
             OnCalendar=hourly\n\
             Persistent=true\n\
             RandomizedDelaySec=180\n\n\
             [Install]\n\
             WantedBy=timers.target\n"
                .to_string(),
        ),
        (
            "jarvis-improve.service",
            format!(
                "[Unit]\n\
                 Description=Jarvis — sebe-vývoj (draft → test → propose, oznámení e-mailem)\n\n\
                 [Service]\n\
                 Type=oneshot\n\
                 ExecStart={exec} improve tick\n\
                 Environment=PATH=%h/.cargo/bin:%h/.local/bin:/usr/local/bin:/usr/bin:/bin\n\
                 EnvironmentFile=-%h/.config/jarvis/secrets.env\n"
            ),
        ),
        (
            // several times a day; the auto-deploy (restart) is deferred while
            // the user is at the desk (improve::deploy_pending_when_away), so
            // mid-day ticks merge but don't interrupt the voice assistant.
            // Enabled only when [improve] enabled=true (ship dark).
            "jarvis-improve.timer",
            "[Unit]\n\
             Description=Jarvis — sebe-vývoj (několikrát denně)\n\n\
             [Timer]\n\
             OnCalendar=*-*-* 00/4:15:00\n\
             Persistent=true\n\
             RandomizedDelaySec=300\n\n\
             [Install]\n\
             WantedBy=timers.target\n"
                .to_string(),
        ),
    ]
}

/// Names of all managed user units — for `jarvis kill` and diagnostics, without
/// needing to know exec/DISPLAY (unlike `unit_files`). Kept in sync
/// with `unit_files` (checked by test `unit_names_match_unit_files`).
pub fn unit_names() -> Vec<&'static str> {
    vec![
        "jarvis-capture.service",
        "jarvis-listen.service",
        "jarvis-analyze.service",
        "jarvis-analyze.timer",
        "jarvis-digest.service",
        "jarvis-digest.timer",
        "jarvis-runbooks.service",
        "jarvis-runbooks.timer",
        "jarvis-memory.service",
        "jarvis-memory.timer",
        "jarvis-tasks.service",
        "jarvis-tasks.timer",
        "jarvis-improve.service",
        "jarvis-improve.timer",
    ]
}

pub fn install(cfg: &Config, print_only: bool) -> Result<()> {
    let exec = std::env::current_exe()
        .context("nelze zjistit cestu k binárce")?
        .display()
        .to_string();
    if !print_only && exec.contains("/target/") {
        bail!(
            "binárka běží z {exec} — pro trvalý provoz nejdřív `cargo install --path .` \
             a spusť `jarvis install-units` z ~/.cargo/bin/jarvis (jinak units umřou s dalším buildem)"
        );
    }
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let xauthority = std::env::var("XAUTHORITY").ok();
    let units =
        unit_files(&exec, &display, xauthority.as_deref(), cfg.digest.hour, cfg.memory.consolidate_hour);

    if print_only {
        for (name, content) in &units {
            println!("─── {name} ───\n{content}");
        }
        return Ok(());
    }

    let home = PathBuf::from(std::env::var_os("HOME").context("chybí $HOME")?);
    let dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&dir).with_context(|| format!("nelze vytvořit {}", dir.display()))?;
    for (name, content) in &units {
        std::fs::write(dir.join(name), content)
            .with_context(|| format!("nelze zapsat {name}"))?;
        println!("✓ zapsán {name}");
    }
    systemctl(&["daemon-reload"])?;
    let mut enable = vec![
        "enable",
        "--now",
        "jarvis-capture.service",
        "jarvis-analyze.timer",
        "jarvis-digest.timer",
    ];
    if cfg.listen.enabled {
        enable.push("jarvis-listen.service");
    }
    if cfg.runbooks.enabled {
        enable.push("jarvis-runbooks.timer");
    }
    if cfg.memory.enabled && cfg.memory.consolidate {
        enable.push("jarvis-memory.timer");
    }
    if cfg.tasks.enabled {
        enable.push("jarvis-tasks.timer");
    }
    if cfg.improve.enabled {
        enable.push("jarvis-improve.timer");
    }
    systemctl(&enable)?;
    if !cfg.listen.enabled {
        // listen disabled in config → stop any previously enabled service;
        // best effort (the unit may not exist)
        let _ = systemctl(&["disable", "--now", "jarvis-listen.service"]);
    }
    if !cfg.runbooks.enabled {
        let _ = systemctl(&["disable", "--now", "jarvis-runbooks.timer"]);
    }
    if !(cfg.memory.enabled && cfg.memory.consolidate) {
        let _ = systemctl(&["disable", "--now", "jarvis-memory.timer"]);
    }
    if !cfg.tasks.enabled {
        let _ = systemctl(&["disable", "--now", "jarvis-tasks.timer"]);
    }
    if !cfg.improve.enabled {
        let _ = systemctl(&["disable", "--now", "jarvis-improve.timer"]);
    }
    println!("Units aktivní. Kontrola: systemctl --user list-timers 'jarvis-*'");
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("systemctl --user nelze spustit")?;
    if !out.status.success() {
        bail!(
            "systemctl --user {} selhalo: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_contain_essentials() {
        let units = unit_files("/usr/bin/jarvis", ":0.0", Some("/home/u/.Xauthority"), 19, 4);
        assert_eq!(units.len(), 14);
        let capture = &units[0].1;
        assert!(capture.contains("ExecStart=/usr/bin/jarvis capture"));
        assert!(capture.contains("Environment=DISPLAY=:0.0"));
        assert!(capture.contains("XAUTHORITY=/home/u/.Xauthority"));
        assert!(capture.contains("Restart=always"));
        let listen = &units[1].1;
        assert!(listen.contains("ExecStart=/usr/bin/jarvis listen"));
        assert!(listen.contains("Restart=always"));
        let digest_timer = &units[5].1;
        assert!(digest_timer.contains("OnCalendar=*-*-* 19:00:00"));
        assert!(digest_timer.contains("Persistent=true"));
        // runbooks: scripts may move windows → the service needs an X environment
        let runbooks = &units[6].1;
        assert!(runbooks.contains("ExecStart=/usr/bin/jarvis runbook run-due"));
        assert!(runbooks.contains("Environment=DISPLAY=:0.0"));
        assert!(runbooks.contains("XAUTHORITY=/home/u/.Xauthority"));
        let runbooks_timer = &units[7].1;
        assert!(runbooks_timer.contains("OnCalendar=*:0/5"));
        assert!(runbooks_timer.contains("Persistent=true"));
        // memory: nightly consolidation of facts + embeddings (no X, just claude+DB)
        let memory_svc = &units[8].1;
        assert!(memory_svc.contains("ExecStart=/usr/bin/jarvis memory consolidate"));
        assert!(memory_svc.contains("Type=oneshot"));
        let memory_timer = &units[9].1;
        assert!(memory_timer.contains("OnCalendar=*-*-* 04:30:00")); // consolidate_hour=4
        assert!(memory_timer.contains("Persistent=true"));
        // scheduled tasks: oneshot run-due + hourly tick (the due schedule is internal)
        let tasks_svc = &units[10].1;
        assert!(tasks_svc.contains("ExecStart=/usr/bin/jarvis tasks run-due"));
        assert!(tasks_svc.contains("Type=oneshot"));
        let tasks_timer = &units[11].1;
        assert!(tasks_timer.contains("OnCalendar=hourly"));
        assert!(tasks_timer.contains("Persistent=true"));
    }

    #[test]
    fn unit_names_match_unit_files() {
        // kill relies on `unit_names` covering exactly what `unit_files`
        // generates (and in the same order) — otherwise kill would miss a unit
        let from_files: Vec<&str> =
            unit_files("/x", ":0", None, 19, 4).iter().map(|(n, _)| *n).collect();
        assert_eq!(from_files, unit_names());
    }

    #[test]
    fn units_without_xauthority() {
        let units = unit_files("/x", ":0", None, 7, 3);
        assert!(!units[0].1.contains("XAUTHORITY"));
        assert!(!units[6].1.contains("XAUTHORITY"));
        assert!(units[5].1.contains("07:00:00"));
        assert!(units[9].1.contains("03:30:00")); // consolidate_hour=3
    }
}
