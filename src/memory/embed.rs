//! Local embeddings (phase 3): multilingual-e5-small via onnxruntime (ort
//! load-dynamic — reuses the existing pip library, nothing extra to download
//! or compile). All CPU; at this scale (1 sentence/query) it's negligible
//! next to a Claude call.
//!
//! Best-effort: if the model or onnxruntime is missing, `embed_query` returns
//! None and retrieval silently falls back to FTS. The model downloads once via
//! `jarvis memory embed` (rustls `util::download`, no openssl/hf-hub).

use crate::config::MemoryCfg;
use crate::util;
use anyhow::{Context, Result};
use ndarray::Array2;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Value;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tokenizers::Tokenizer;
use tracing::warn;

/// Model + tokenizer URL for the Xenova onnx export of the given model. The
/// path only differs by name between models, so it's derived from
/// `cfg.embed_model` — that makes the config choice actually take effect
/// (previously e5-small was hardcoded for download, but the config's model
/// tag was stored in the DB → mismatched). `embed_model` is restricted in
/// `validate()` to [A-Za-z0-9._-], so nothing can be smuggled into the URL.
/// The default `multilingual-e5-small` yields exactly the original e5-small URL.
fn model_urls(cfg: &MemoryCfg) -> (String, String) {
    let base = format!("https://huggingface.co/Xenova/{}/resolve/main", cfg.embed_model);
    (format!("{base}/onnx/model.onnx"), format!("{base}/tokenizer.json"))
}

/// The resident model stays loaded for the process's whole lifetime (onnx
/// load ~0.5 s + ~470 MB RAM). Only set after a successful load; failure isn't
/// cached (the next attempt after `memory embed` succeeds without restarting the daemon).
static EMBEDDER: OnceLock<Mutex<Embedder>> = OnceLock::new();

struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    wants_token_type: bool,
}

/// Model directory (derived from $HOME like `Paths`, so retrieval doesn't have
/// to thread `Paths` through the whole call chain). None = no $HOME.
fn model_dir(cfg: &MemoryCfg) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local/share/jarvis/models")
            .join(format!("embed-{}", cfg.embed_model)),
    )
}

/// Are the model files downloaded?
pub fn model_present(cfg: &MemoryCfg) -> bool {
    let Some(d) = model_dir(cfg) else { return false };
    d.join("model.onnx").exists() && d.join("tokenizer.json").exists()
}

/// Downloads the model + tokenizer (rustls via `util::download`) if missing.
pub fn ensure_model(cfg: &MemoryCfg) -> Result<()> {
    let d = model_dir(cfg).context("nelze určit složku modelu (chybí $HOME)")?;
    std::fs::create_dir_all(&d).with_context(|| format!("nelze vytvořit {}", d.display()))?;
    let (model_url, tokenizer_url) = model_urls(cfg);
    util::download(&model_url, &d.join("model.onnx"))?;
    util::download(&tokenizer_url, &d.join("tokenizer.json"))?;
    Ok(())
}

/// Finds `libonnxruntime.so*` — from config, else ORT_DYLIB_PATH, else the pip
/// onnxruntime under ~/.local/lib/python3.*/site-packages/onnxruntime/capi/.
fn find_onnxruntime(cfg: &MemoryCfg) -> Option<PathBuf> {
    let configured = cfg.onnxruntime_lib.trim();
    if !configured.is_empty() {
        return Some(PathBuf::from(configured));
    }
    if let Some(env) = std::env::var_os("ORT_DYLIB_PATH") {
        return Some(PathBuf::from(env));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let lib = home.join(".local/lib");
    let pyver = std::fs::read_dir(&lib).ok()?.flatten().find_map(|e| {
        let name = e.file_name();
        name.to_str().filter(|n| n.starts_with("python3")).map(|_| e.path())
    })?;
    let capi = pyver.join("site-packages/onnxruntime/capi");
    std::fs::read_dir(&capi).ok()?.flatten().find_map(|e| {
        let name = e.file_name();
        name.to_str().filter(|n| n.starts_with("libonnxruntime.so")).map(|_| e.path())
    })
}

impl Embedder {
    fn load(cfg: &MemoryCfg) -> Result<Self> {
        let lib = find_onnxruntime(cfg).context(
            "nenašel jsem libonnxruntime.so (nastav memory.onnxruntime_lib nebo nainstaluj pip onnxruntime)",
        )?;
        anyhow::ensure!(lib.exists(), "onnxruntime knihovna neexistuje: {}", lib.display());
        // ort load-dynamic reads ORT_DYLIB_PATH on the first call
        std::env::set_var("ORT_DYLIB_PATH", &lib);
        let d = model_dir(cfg).context("chybí $HOME")?;
        let tokenizer = Tokenizer::from_file(d.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("tokenizer: {e}"))?;
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .commit_from_file(d.join("model.onnx"))
            .context("načtení onnx modelu selhalo")?;
        let wants_token_type = session.inputs.iter().any(|i| i.name == "token_type_ids");
        Ok(Self { session, tokenizer, wants_token_type })
    }

    /// Embedding of one text: tokenize → onnx → mean-pool (masked) → L2
    /// normalize. The caller prefixes the text ("query: "/"passage: ").
    fn embed_one(&mut self, text: &str) -> Result<Vec<f32>> {
        let enc = self.tokenizer.encode(text, true).map_err(|e| anyhow::anyhow!("{e}"))?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&x| x as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&x| x as i64).collect();
        let len = ids.len();
        let ids_a = Array2::from_shape_vec((1, len), ids)?;
        let mask_a = Array2::from_shape_vec((1, len), mask.clone())?;
        let outputs = if self.wants_token_type {
            let tt = Array2::<i64>::zeros((1, len));
            self.session.run(ort::inputs![
                "input_ids" => Value::from_array(ids_a)?,
                "attention_mask" => Value::from_array(mask_a)?,
                "token_type_ids" => Value::from_array(tt)?,
            ])?
        } else {
            self.session.run(ort::inputs![
                "input_ids" => Value::from_array(ids_a)?,
                "attention_mask" => Value::from_array(mask_a)?,
            ])?
        };
        // last_hidden_state [1, len, hidden]
        let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
        let hidden = shape[shape.len() - 1] as usize;
        let mut v = vec![0f32; hidden];
        let mut cnt = 0f32;
        for i in 0..len {
            if mask[i] == 0 {
                continue;
            }
            cnt += 1.0;
            for h in 0..hidden {
                v[h] += data[i * hidden + h];
            }
        }
        for x in &mut v {
            *x /= cnt.max(1.0);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        for x in &mut v {
            *x /= norm;
        }
        Ok(v)
    }

    /// Embeds a whole batch at once: tokenize + pad to the longest in the
    /// batch, one onnx inference (instead of N with batch size 1), then masked
    /// mean-pool + L2 normalize per row. attention_mask zeroes out padding
    /// both in the model's attention and in the pool, so the result matches
    /// `embed_one` (up to float noise). Returns vectors in input order. The
    /// caller prefixes the texts.
    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encs = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let b = encs.len();
        let maxlen = encs.iter().map(|e| e.get_ids().len()).max().unwrap_or(0).max(1);
        let mut ids = Array2::<i64>::zeros((b, maxlen));
        let mut mask = Array2::<i64>::zeros((b, maxlen));
        for (r, enc) in encs.iter().enumerate() {
            let e_ids = enc.get_ids();
            let e_mask = enc.get_attention_mask();
            for c in 0..e_ids.len() {
                ids[[r, c]] = e_ids[c] as i64;
                mask[[r, c]] = e_mask[c] as i64;
            }
        }
        let outputs = if self.wants_token_type {
            let tt = Array2::<i64>::zeros((b, maxlen));
            self.session.run(ort::inputs![
                "input_ids" => Value::from_array(ids)?,
                "attention_mask" => Value::from_array(mask.clone())?,
                "token_type_ids" => Value::from_array(tt)?,
            ])?
        } else {
            self.session.run(ort::inputs![
                "input_ids" => Value::from_array(ids)?,
                "attention_mask" => Value::from_array(mask.clone())?,
            ])?
        };
        // last_hidden_state [B, L, hidden]
        let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
        let hidden = shape[shape.len() - 1] as usize;
        let seqlen = shape[shape.len() - 2] as usize;
        let mut out = Vec::with_capacity(b);
        for r in 0..b {
            let mut v = vec![0f32; hidden];
            let mut cnt = 0f32;
            for c in 0..seqlen {
                if mask[[r, c]] == 0 {
                    continue;
                }
                cnt += 1.0;
                let base = (r * seqlen + c) * hidden;
                for h in 0..hidden {
                    v[h] += data[base + h];
                }
            }
            for x in &mut v {
                *x /= cnt.max(1.0);
            }
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
            for x in &mut v {
                *x /= norm;
            }
            out.push(v);
        }
        Ok(out)
    }
}

/// Returns the resident embedder, or None (model not downloaded / onnxruntime
/// missing). Failure isn't cached — the next call succeeds once the model is available.
fn embedder(cfg: &MemoryCfg) -> Option<&'static Mutex<Embedder>> {
    if let Some(e) = EMBEDDER.get() {
        return Some(e);
    }
    if !model_present(cfg) {
        return None;
    }
    match Embedder::load(cfg) {
        Ok(e) => {
            let _ = EMBEDDER.set(Mutex::new(e)); // concurrent loser drops it, get() below wins
            EMBEDDER.get()
        }
        Err(e) => {
            warn!("embeddingy nedostupné (retrieval jede jen na FTS): {e:#}");
            None
        }
    }
}

/// Embeds a query (prefix "query: "). None = embeddings unavailable → FTS-only.
pub fn embed_query(cfg: &MemoryCfg, text: &str) -> Option<Vec<f32>> {
    let m = embedder(cfg)?;
    let mut g = m.lock().ok()?;
    match g.embed_one(&format!("query: {text}")) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("embed dotazu selhal: {e:#}");
            None
        }
    }
}

/// Embeds passages (prefix "passage: ") for filling the index. Errors
/// propagate — backfill needs to know about the problem.
pub fn embed_passages(cfg: &MemoryCfg, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let m = embedder(cfg)
        .context("embedder nedostupný — stáhni model (`jarvis memory embed`) a ověř onnxruntime")?;
    let mut g = m.lock().map_err(|_| anyhow::anyhow!("embedder mutex otráven"))?;
    // in chunks: one inference per chunk instead of per text
    const BATCH: usize = 32;
    let mut out = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(BATCH) {
        let prefixed: Vec<String> = chunk.iter().map(|t| format!("passage: {t}")).collect();
        let refs: Vec<&str> = prefixed.iter().map(|s| s.as_str()).collect();
        out.extend(g.embed_batch(&refs)?);
    }
    Ok(out)
}
