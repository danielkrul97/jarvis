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
    /// None/empty = default claude CLI model
    pub model: Option<&'a str>,
    pub cwd: &'a Path,
    pub allowed_tools: &'a str,
    pub max_turns: u32,
    pub timeout: Duration,
}

/// Headless call to `claude -p --output-format json`. The prompt goes via stdin,
/// stdout/stderr are read by threads (otherwise the process would block on a full pipe).
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
        // drop stdin → EOF for claude
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

// ---------- resident process (conversation) ----------

/// Pre-warmed `claude -p --input-format stream-json`: the process lives across
/// multiple questions, each question = one JSONL line on stdin, the answer = a
/// `result` event on stdout. Saves ~2 s of CLI startup per exchange, and the session
/// holds conversational memory. Empirically 2026-07-17: 1st question 3.0 s incl.
/// spawn, 2nd question 2.2 s (pure API, haiku).
pub struct Warm {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<StreamMsg>,
    exchanges: usize,
    last_used: Instant,
}

impl Warm {
    /// `allowed_tools`/`max_turns` are set by the caller (converse: Read only, or
    /// with [wm] also Bash restricted to `jarvis wm`). max_turns applies per message,
    /// not per session (verified).
    pub fn spawn(model: &str, cwd: &Path, allowed_tools: &str, max_turns: u32) -> Result<Self> {
        let mut cmd = Command::new("claude");
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose", // required by stream-json output
            "--include-partial-messages", // incremental text_delta for speech streaming
            "--allowed-tools",
            allowed_tools,
            "--max-turns",
            &max_turns.to_string(),
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }
        let mut child = cmd.spawn().context("nelze spustit warm `claude` (je v PATH?)")?;
        let stdin = child.stdin.take().context("warm claude bez stdin")?;
        let stdout = child.stdout.take().context("warm claude bez stdout")?;
        let stderr = child.stderr.take().context("warm claude bez stderr")?;

        // reader: parses stream events, forwards only `result`; EOF ends
        // the thread and dropping tx closes the channel (ask then returns Disconnected)
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(msg) = parse_stream_line(&v) {
                    if tx.send(msg).is_err() {
                        break; // owner is gone
                    }
                }
            }
        });
        // stderr must be drained, otherwise a full pipe would stall it; content goes to the log only
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::BufReader::new(stderr), &mut s);
            let s = s.trim();
            if !s.is_empty() {
                tracing::debug!("warm claude stderr: {}", util::truncate_chars(s, 400));
            }
        });
        Ok(Self { child, stdin, rx, exchanges: 0, last_used: Instant::now() })
    }

    /// A stale process should be discarded and replaced with a fresh one.
    pub fn stale(&self, max_exchanges: usize, idle_s: u64) -> bool {
        self.exchanges >= max_exchanges || self.last_used.elapsed().as_secs() > idle_s
    }

    /// Asks a question and waits for the result. Any error = discard the process
    /// (after a timeout, a delayed result from an old question could still be sitting in the channel).
    pub fn ask(&mut self, prompt: &str, timeout: Duration) -> Result<ClaudeOutcome> {
        self.ask_streaming(prompt, timeout, |_| {})
    }

    /// Like `ask`, but incremental ANSWER text (excluding "thinking") streams token by
    /// token to `on_text` — the caller assembles sentences from it for streamed synthesis.
    /// Returns the final outcome (cost/tokens) from the `result` event.
    pub fn ask_streaming(
        &mut self,
        prompt: &str,
        timeout: Duration,
        mut on_text: impl FnMut(&str),
    ) -> Result<ClaudeOutcome> {
        if let Ok(Some(st)) = self.child.try_wait() {
            bail!("warm proces mezitím skončil ({st})");
        }
        // discard any stale leftover messages
        while self.rx.try_recv().is_ok() {}
        let msg = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}
        });
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .context("zápis otázky do warm procesu selhal")?;
        // we stream text_delta deltas; `result` closes out the exchange. The timeout applies
        // to the WHOLE answer (from question to result).
        let deadline = Instant::now() + timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                bail!("warm claude neodpověděl do {} s", timeout.as_secs());
            };
            match self.rx.recv_timeout(remaining) {
                Ok(StreamMsg::Delta(t)) => on_text(&t),
                Ok(StreamMsg::Done(Ok(o))) => {
                    self.exchanges += 1;
                    self.last_used = Instant::now();
                    return Ok(o);
                }
                Ok(StreamMsg::Done(Err(e))) => bail!("warm claude: {e}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    bail!("warm claude neodpověděl do {} s", timeout.as_secs())
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("warm proces skončil (stdout zavřen)")
                }
            }
        }
    }
}

impl Drop for Warm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `result` event of stream-json output has the same fields as the -p json envelope.
fn outcome_from_stream(v: &Value) -> Result<ClaudeOutcome, String> {
    let text = v["result"].as_str().unwrap_or_default().to_string();
    if v["is_error"].as_bool().unwrap_or(false) {
        return Err(format!("claude ohlásil chybu: {}", util::truncate_chars(&text, 300)));
    }
    if text.is_empty() {
        return Err("prázdný result".into());
    }
    Ok(ClaudeOutcome {
        text,
        cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
        tokens_in: v["usage"]["input_tokens"].as_i64().unwrap_or(0),
        tokens_out: v["usage"]["output_tokens"].as_i64().unwrap_or(0),
    })
}

/// A message from the warm process's reader thread: either a chunk of answer
/// text (incremental), or the final result (`result` event with cost/tokens).
enum StreamMsg {
    Delta(String),
    Done(Result<ClaudeOutcome, String>),
}

/// Extracts from a stream-json line either `text_delta` (incremental ANSWER text — not
/// "thinking", which comes as `thinking_delta`), or the final `result`. Other
/// events (system, tool-use, message-level…) are ignored.
fn parse_stream_line(v: &Value) -> Option<StreamMsg> {
    match v["type"].as_str()? {
        "stream_event" => {
            let ev = &v["event"];
            if ev["type"] == "content_block_delta" && ev["delta"]["type"] == "text_delta" {
                let t = ev["delta"]["text"].as_str()?;
                (!t.is_empty()).then(|| StreamMsg::Delta(t.to_string()))
            } else {
                None
            }
        }
        "result" => Some(StreamMsg::Done(outcome_from_stream(v))),
        _ => None,
    }
}

/// Extracts a JSON object from the answer text (ignores ```json fences and surrounding text).
pub fn extract_json(text: &str) -> Result<&str> {
    let start = text.find('{').context("v odpovědi není JSON objekt")?;
    let end = text.rfind('}').context("v odpovědi není uzavřený JSON objekt")?;
    if end < start {
        bail!("poškozený JSON v odpovědi");
    }
    Ok(&text[start..=end])
}

/// Live test for doctor --live; cheap model, one round trip.
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
    fn outcome_from_stream_result_event() {
        // shape of the result event in stream-json output (same as the -p json envelope)
        let v: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Ano, pane.","total_cost_usd":0.045,"usage":{"input_tokens":700,"output_tokens":25}}"#,
        )
        .unwrap();
        let o = outcome_from_stream(&v).unwrap();
        assert_eq!(o.text, "Ano, pane.");
        assert_eq!(o.tokens_in, 700);
        let err: Value =
            serde_json::from_str(r#"{"type":"result","is_error":true,"result":"boom"}"#).unwrap();
        assert!(outcome_from_stream(&err).is_err());
        let empty: Value = serde_json::from_str(r#"{"type":"result","result":""}"#).unwrap();
        assert!(outcome_from_stream(&empty).is_err());
    }

    #[test]
    fn parse_stream_line_picks_text_delta_and_result() {
        // incremental answer text (exact shape captured from claude on 2026-07-18)
        let d: Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Nem"}}}"#,
        )
        .unwrap();
        assert!(matches!(parse_stream_line(&d), Some(StreamMsg::Delta(t)) if t == "Nem"));
        // "thinking" must not leak into speech
        let think: Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}}"#,
        )
        .unwrap();
        assert!(parse_stream_line(&think).is_none());
        // block start and other events = None
        let start: Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}}"#,
        )
        .unwrap();
        assert!(parse_stream_line(&start).is_none());
        let sys: Value = serde_json::from_str(r#"{"type":"system","subtype":"init"}"#).unwrap();
        assert!(parse_stream_line(&sys).is_none());
        // result → Done(Ok)
        let res: Value = serde_json::from_str(
            r#"{"type":"result","is_error":false,"result":"Ahoj.","total_cost_usd":0.01,"usage":{"input_tokens":5,"output_tokens":2}}"#,
        )
        .unwrap();
        assert!(matches!(parse_stream_line(&res), Some(StreamMsg::Done(Ok(o))) if o.text == "Ahoj."));
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
