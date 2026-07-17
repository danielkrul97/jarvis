use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub capture: CaptureCfg,
    pub analysis: AnalysisCfg,
    pub digest: DigestCfg,
    pub email: EmailCfg,
    pub retention: RetentionCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureCfg {
    pub meta_interval_s: u64,
    pub shot_interval_s: u64,
    pub idle_threshold_s: u64,
    pub max_dimension: u32,
    pub phash_min_distance: u32,
    pub blacklist_class: Vec<String>,
    pub blacklist_title: Vec<String>,
}

impl Default for CaptureCfg {
    fn default() -> Self {
        Self {
            meta_interval_s: 10,
            shot_interval_s: 60,
            idle_threshold_s: 120,
            max_dimension: 1568,
            phash_min_distance: 7,
            blacklist_class: vec![
                "(?i)keepass".into(),
                "(?i)bitwarden".into(),
                "(?i)1password".into(),
            ],
            blacklist_title: vec![
                "(?i)anonymní".into(),
                "(?i)incognito".into(),
                "(?i)private browsing".into(),
                "(?i)soukromé prohlížení".into(),
                "(?i)bank".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalysisCfg {
    pub max_images_per_run: usize,
    pub model: String,
    pub daily_budget_usd: f64,
    pub send_images: bool,
    pub timeout_s: u64,
}

impl Default for AnalysisCfg {
    fn default() -> Self {
        Self {
            max_images_per_run: 8,
            model: "claude-haiku-4-5-20251001".into(),
            daily_budget_usd: 1.0,
            send_images: true,
            timeout_s: 600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DigestCfg {
    pub hour: u8,
    pub model: String,
}

impl Default for DigestCfg {
    fn default() -> Self {
        Self { hour: 19, model: String::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmailCfg {
    pub to: String,
    pub from: String,
    pub from_name: String,
    pub subject_prefix: String,
}

impl Default for EmailCfg {
    fn default() -> Self {
        Self {
            to: "dankrul.krul@gmail.com".into(),
            from: "dankrul.krul@gmail.com".into(),
            from_name: "Jarvis".into(),
            subject_prefix: "Jarvis digest".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionCfg {
    pub screenshots_days: u64,
}

impl Default for RetentionCfg {
    fn default() -> Self {
        Self { screenshots_days: 7 }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let cfg: Config = if paths.config_file.exists() {
            let text = fs::read_to_string(&paths.config_file)
                .with_context(|| format!("nelze číst {}", paths.config_file.display()))?;
            toml::from_str(&text)
                .with_context(|| format!("neplatný config {}", paths.config_file.display()))?
        } else {
            Config::default()
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.digest.hour > 23 {
            bail!("digest.hour musí být 0–23, je {}", self.digest.hour);
        }
        if self.capture.meta_interval_s == 0 || self.capture.shot_interval_s == 0 {
            bail!("intervaly snímání musí být >= 1 s");
        }
        if self.capture.max_dimension < 256 {
            bail!("capture.max_dimension musí být >= 256");
        }
        Blacklist::new(&self.capture)?;
        Ok(())
    }
}

pub struct Blacklist {
    class: Vec<Regex>,
    title: Vec<Regex>,
}

impl Blacklist {
    pub fn new(cfg: &CaptureCfg) -> Result<Self> {
        let compile = |patterns: &[String], what: &str| -> Result<Vec<Regex>> {
            patterns
                .iter()
                .map(|p| Regex::new(p).with_context(|| format!("neplatný regex v {what}: {p}")))
                .collect()
        };
        Ok(Self {
            class: compile(&cfg.blacklist_class, "blacklist_class")?,
            title: compile(&cfg.blacklist_title, "blacklist_title")?,
        })
    }

    pub fn matches(&self, wm_class: &str, title: &str) -> bool {
        self.class.iter().any(|r| r.is_match(wm_class))
            || self.title.iter().any(|r| r.is_match(title))
    }
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub secrets_file: PathBuf,
    pub data_dir: PathBuf,
    pub shots_dir: PathBuf,
    pub proposals_dir: PathBuf,
    pub db_path: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let home = PathBuf::from(std::env::var_os("HOME").context("chybí $HOME")?);
        let config_dir = home.join(".config/jarvis");
        let data_dir = home.join(".local/share/jarvis");
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            secrets_file: config_dir.join("secrets.env"),
            shots_dir: data_dir.join("shots"),
            proposals_dir: data_dir.join("proposals"),
            db_path: data_dir.join("jarvis.db"),
            config_dir,
            data_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.data_dir, &self.shots_dir, &self.proposals_dir] {
            fs::create_dir_all(dir).with_context(|| format!("nelze vytvořit {}", dir.display()))?;
        }
        // data i config drží citlivá data — jen pro uživatele
        for dir in [&self.config_dir, &self.data_dir] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("nelze nastavit práva {}", dir.display()))?;
        }
        Ok(())
    }
}

/// SendGrid API klíč: env SENDGRID_API_KEY má přednost, jinak secrets.env.
pub fn sendgrid_key(paths: &Paths) -> Result<String> {
    if let Ok(k) = std::env::var("SENDGRID_API_KEY") {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Ok(k);
        }
    }
    let text = fs::read_to_string(&paths.secrets_file).with_context(|| {
        format!(
            "SENDGRID_API_KEY není v env a nelze číst {}",
            paths.secrets_file.display()
        )
    })?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix("SENDGRID_API_KEY=") {
            let v = v.trim().trim_matches('"').to_string();
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    bail!(
        "SENDGRID_API_KEY nenalezen v {} ani v prostředí",
        paths.secrets_file.display()
    )
}

/// Parsuje "30m", "2h", "7d", "45s" nebo holé sekundy na sekundy.
pub fn parse_duration_spec(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("prázdné trvání");
    }
    let (num, mult) = match s.chars().last().unwrap() {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        c if c.is_ascii_digit() => (s, 1),
        c => bail!("neznámá jednotka '{c}' v trvání '{s}' (podporuji s/m/h/d)"),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("neplatné trvání '{s}'"))?;
    Ok(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.email.to, "dankrul.krul@gmail.com");
        assert_eq!(cfg.digest.hour, 19);
        assert_eq!(cfg.retention.screenshots_days, 7);
    }

    #[test]
    fn duration_spec() {
        assert_eq!(parse_duration_spec("30m").unwrap(), 1800);
        assert_eq!(parse_duration_spec("2h").unwrap(), 7200);
        assert_eq!(parse_duration_spec("7d").unwrap(), 604800);
        assert_eq!(parse_duration_spec("45s").unwrap(), 45);
        assert_eq!(parse_duration_spec("90").unwrap(), 90);
        assert!(parse_duration_spec("x").is_err());
        assert!(parse_duration_spec("").is_err());
        assert!(parse_duration_spec("5w").is_err());
    }

    #[test]
    fn blacklist_matching() {
        let cfg = CaptureCfg::default();
        let bl = Blacklist::new(&cfg).unwrap();
        assert!(bl.matches("KeePassXC", "moje hesla"));
        assert!(bl.matches("firefox", "Mozilla Firefox (Anonymní prohlížení)"));
        assert!(bl.matches("chromium", "Incognito — tab"));
        assert!(!bl.matches("firefox", "Rust dokumentace"));
        assert!(!bl.matches("Alacritty", "vim PLAN.md"));
    }
}
