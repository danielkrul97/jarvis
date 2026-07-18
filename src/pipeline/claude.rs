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

// ---------- rezidentní proces (konverzace) ----------

/// Předehřátý `claude -p --input-format stream-json`: proces žije přes víc
/// otázek, každá otázka = jeden JSONL řádek na stdin, odpověď = `result`
/// event na stdout. Ušetří ~2 s CLI startu na výměnu a session drží
/// konverzační paměť. Empiricky 2026-07-17: 1. otázka 3,0 s vč. spawnu,
/// 2. otázka 2,2 s (čisté API, haiku).
pub struct Warm {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<StreamMsg>,
    exchanges: usize,
    last_used: Instant,
}

impl Warm {
    /// `allowed_tools`/`max_turns` určuje volající (converse: jen Read, nebo
    /// s [wm] i Bash omezený na `jarvis wm`). max_turns platí per zpráva,
    /// ne per session (ověřeno).
    pub fn spawn(model: &str, cwd: &Path, allowed_tools: &str, max_turns: u32) -> Result<Self> {
        let mut cmd = Command::new("claude");
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose", // stream-json výstup ho vyžaduje
            "--include-partial-messages", // průběžné text_delta pro streamování řeči
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

        // čtečka: parsuje stream eventy, dál posílá jen `result`; EOF ukončí
        // vlákno a drop tx zavře kanál (ask pak vrací Disconnected)
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(msg) = parse_stream_line(&v) {
                    if tx.send(msg).is_err() {
                        break; // majitel zanikl
                    }
                }
            }
        });
        // stderr nutno odsávat, jinak se plná roura zasekne; obsah jen do logu
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

    /// Vyčpělý proces se má zahodit a nahradit čerstvým.
    pub fn stale(&self, max_exchanges: usize, idle_s: u64) -> bool {
        self.exchanges >= max_exchanges || self.last_used.elapsed().as_secs() > idle_s
    }

    /// Položí otázku a čeká na výsledek. Jakákoli chyba = proces zahodit
    /// (po timeoutu by v kanálu mohl ležet opožděný výsledek staré otázky).
    pub fn ask(&mut self, prompt: &str, timeout: Duration) -> Result<ClaudeOutcome> {
        self.ask_streaming(prompt, timeout, |_| {})
    }

    /// Jako `ask`, ale průběžný text ODPOVĚDI (bez „myšlení") jde po tokenech do
    /// `on_text` — volající z něj skládá věty pro streamovanou syntézu. Vrací
    /// finální outcome (náklad/tokeny) z `result` eventu.
    pub fn ask_streaming(
        &mut self,
        prompt: &str,
        timeout: Duration,
        mut on_text: impl FnMut(&str),
    ) -> Result<ClaudeOutcome> {
        if let Ok(Some(st)) = self.child.try_wait() {
            bail!("warm proces mezitím skončil ({st})");
        }
        // zahodit případné opožděné zprávy z minula
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
        // delty text_delta streamujeme; `result` výměnu uzavře. Timeout platí
        // na CELOU odpověď (od otázky po result).
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

/// `result` event stream-json výstupu má stejná pole jako -p json obálka.
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

/// Zpráva z reader vlákna warm procesu: buď kus textu odpovědi (průběžně), nebo
/// finální výsledek (`result` event s nákladem/tokeny).
enum StreamMsg {
    Delta(String),
    Done(Result<ClaudeOutcome, String>),
}

/// Ze stream-json řádku vytáhne buď `text_delta` (průběžný text ODPOVĚDI — ne
/// „myšlení", to jede jako `thinking_delta`), nebo finální `result`. Ostatní
/// eventy (system, tool-use, message-level…) ignoruje.
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
    fn outcome_from_stream_result_event() {
        // tvar result eventu stream-json výstupu (shodný s -p json obálkou)
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
        // průběžný text odpovědi (přesný tvar zachycený z claude 2026-07-18)
        let d: Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Nem"}}}"#,
        )
        .unwrap();
        assert!(matches!(parse_stream_line(&d), Some(StreamMsg::Delta(t)) if t == "Nem"));
        // „myšlení" se do řeči nesmí dostat
        let think: Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}}"#,
        )
        .unwrap();
        assert!(parse_stream_line(&think).is_none());
        // start bloku i ostatní eventy = None
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
