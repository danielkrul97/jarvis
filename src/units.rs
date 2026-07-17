use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// Vygeneruje obsah systemd user units. `exec` = absolutní cesta k binárce,
/// `display`/`xauthority` se zapékají z aktuální session (capture je potřebuje).
pub fn unit_files(
    exec: &str,
    display: &str,
    xauthority: Option<&str>,
    digest_hour: u8,
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
    let units = unit_files(&exec, &display, xauthority.as_deref(), cfg.digest.hour);

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
    systemctl(&[
        "enable",
        "--now",
        "jarvis-capture.service",
        "jarvis-analyze.timer",
        "jarvis-digest.timer",
    ])?;
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
        let units = unit_files("/usr/bin/jarvis", ":0.0", Some("/home/u/.Xauthority"), 19);
        assert_eq!(units.len(), 5);
        let capture = &units[0].1;
        assert!(capture.contains("ExecStart=/usr/bin/jarvis capture"));
        assert!(capture.contains("Environment=DISPLAY=:0.0"));
        assert!(capture.contains("XAUTHORITY=/home/u/.Xauthority"));
        assert!(capture.contains("Restart=always"));
        let digest_timer = &units[4].1;
        assert!(digest_timer.contains("OnCalendar=*-*-* 19:00:00"));
        assert!(digest_timer.contains("Persistent=true"));
    }

    #[test]
    fn units_without_xauthority() {
        let units = unit_files("/x", ":0", None, 7);
        assert!(!units[0].1.contains("XAUTHORITY"));
        assert!(units[4].1.contains("07:00:00"));
    }
}
