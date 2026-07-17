use crate::util;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct ClaudeOutcome {
    pub text: String,
    pub cost_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
}

pub struct ClaudeRequest<'a> {
    pub prompt: String,
    /// None/prázdný = výchozí model claude CLI
    pub model: Option<&'a str>,
    pub cwd: &'a Path,
    pub allowed_tools: &'a str,
    pub max_turns: u32,
    pub timeout: Duration,
}

/// Headless volání `claude -p --output-format json`. Prompt jde přes stdin,
/// stdout/stderr čtou vlákna (jinak by se proces zablokoval na plné rouře).
pub fn run(req: &ClaudeRequest) -> Result<ClaudeOutcome> {
    let mut cmd = Command::new("claude");
    cmd.arg("-p")
        .arg("--output-format")
        .arg("json")
        .arg("--allowed-tools")
        .arg(req.allowed_tools)
        .arg("--max-turns")
        .arg(req.max_turns.to_string())
        .current_dir(req.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(m) = req.model {
        if !m.is_empty() {
            cmd.arg("--model").arg(m);
        }
    }
    let mut child = cmd.spawn().context("nelze spustit `claude` (je v PATH?)")?;

    let mut stdin = child.stdin.take().context("chybí stdin dítěte")?;
    let prompt = req.prompt.clone();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(prompt.as_bytes());
        // drop stdin → EOF pro claude
    });
    let mut stdout = child.stdout.take().context("chybí stdout dítěte")?;
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let mut stderr = child.stderr.take().context("chybí stderr dítěte")?;
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + req.timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("claude CLI nedoběhl do {} s — zabit", req.timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let _ = writer.join();
    let stdout_s = out_reader.join().unwrap_or_default();
    let stderr_s = err_reader.join().unwrap_or_default();

    if !status.success() {
        bail!(
            "claude CLI exit {:?}: stderr: {} | stdout: {}",
            status.code(),
            util::truncate_chars(stderr_s.trim(), 400),
            util::truncate_chars(stdout_s.trim(), 400)
        );
    }
    parse_outcome(&stdout_s).with_context(|| {
        format!(
            "nečekaný výstup claude CLI: {}",
            util::truncate_chars(stdout_s.trim(), 300)
        )
    })
}

fn parse_outcome(stdout: &str) -> Result<ClaudeOutcome> {
    let v: Value = serde_json::from_str(stdout.trim()).context("výstup není JSON")?;
    let text = v["result"].as_str().unwrap_or_default().to_string();
    if v["is_error"].as_bool().unwrap_or(false) {
        bail!("claude ohlásil chybu: {}", util::truncate_chars(&text, 300));
    }
    if text.is_empty() {
        bail!("prázdný result");
    }
    Ok(ClaudeOutcome {
        text,
        cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
        tokens_in: v["usage"]["input_tokens"].as_i64().unwrap_or(0),
        tokens_out: v["usage"]["output_tokens"].as_i64().unwrap_or(0),
    })
}

/// Vytáhne JSON objekt z textu odpovědi (ignoruje ```json ploty a text okolo).
pub fn extract_json(text: &str) -> Result<&str> {
    let start = text.find('{').context("v odpovědi není JSON objekt")?;
    let end = text.rfind('}').context("v odpovědi není uzavřený JSON objekt")?;
    if end < start {
        bail!("poškozený JSON v odpovědi");
    }
    Ok(&text[start..=end])
}

/// Živý test pro doctor --live; levný model, jedna otočka.
pub fn ping(model: &str) -> Result<String> {
    let tmp = std::env::temp_dir();
    let outcome = run(&ClaudeRequest {
        prompt: "Odpověz přesně jedním slovem: pong".into(),
        model: Some(model),
        cwd: &tmp,
        allowed_tools: "Read",
        max_turns: 1,
        timeout: Duration::from_secs(120),
    })?;
    Ok(format!(
        "odpověď „{}“ ({:.4} USD)",
        util::truncate_chars(outcome.text.trim(), 40),
        outcome.cost_usd
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outcome_happy_path() {
        let raw = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"a\":1}","total_cost_usd":0.0123,"usage":{"input_tokens":100,"output_tokens":20}}"#;
        let o = parse_outcome(raw).unwrap();
        assert_eq!(o.text, "{\"a\":1}");
        assert!((o.cost_usd - 0.0123).abs() < 1e-9);
        assert_eq!(o.tokens_in, 100);
        assert_eq!(o.tokens_out, 20);
    }

    #[test]
    fn parse_outcome_error_flag() {
        let raw = r#"{"is_error":true,"result":"něco se pokazilo"}"#;
        assert!(parse_outcome(raw).is_err());
    }

    #[test]
    fn parse_outcome_garbage() {
        assert!(parse_outcome("not json").is_err());
        assert!(parse_outcome(r#"{"is_error":false,"result":""}"#).is_err());
    }

    #[test]
    fn extract_json_variants() {
        assert_eq!(extract_json(r#"{"a":1}"#).unwrap(), r#"{"a":1}"#);
        assert_eq!(
            extract_json("Tady je JSON:\n```json\n{\"a\":1}\n```\ndík").unwrap(),
            r#"{"a":1}"#
        );
        assert!(extract_json("žádný json").is_err());
    }
}
