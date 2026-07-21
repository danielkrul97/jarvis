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
    pub listen: ListenCfg,
    pub speak: SpeakCfg,
    pub converse: ConverseCfg,
    pub wm: WmCfg,
    pub meet: MeetCfg,
    pub sms: SmsCfg,
    pub runbooks: RunbooksCfg,
    pub memory: MemoryCfg,
    pub proactive: ProactiveCfg,
    pub tasks: TasksCfg,
    pub improve: ImproveCfg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TasksCfg {
    /// Scheduled internal jobs (`jarvis tasks`): Jarvis manages its own
    /// housekeeping — dependency checks (binaries, models, keys, disk),
    /// DB maintenance, screenshot cleanup. Unlike runbooks these aren't
    /// user scripts (no approval needed): built-in, trusted functions run
    /// on a schedule. Disabled = `tasks run-due` and the `jarvis run` loop
    /// schedule nothing; manual `jarvis tasks run <name>` still works.
    pub enabled: bool,
    /// When a scheduled task finds a problem (missing dependency, low disk
    /// space) it reports to Telegram (TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID
    /// in secrets.env) so you learn about broken dependencies away from your
    /// desk too. No keys = logged only. Safety invariant: the task only
    /// INFORMS, never installs or changes the system itself — the remediation
    /// command is included in the message.
    pub notify_telegram: bool,
    /// Truncate stored run output (DB shouldn't hold megabytes of logs).
    pub max_output_chars: usize,
    /// Dependency check (`deps`) warns when `data_dir` has less than this
    /// many MB free (models and screenshots grow; a full disk breaks capture
    /// and STT).
    pub min_disk_free_mb: u64,
}

impl Default for TasksCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            notify_telegram: false,
            max_output_chars: 4000,
            min_disk_free_mb: 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryCfg {
    /// Long-term memory: hybrid search over past conversations/utterances
    /// (FTS5 on `conversations`/`utterances`) adds relevant snippets from
    /// the past to the conversation prompt — not just the last few exchanges
    /// by recency. false = falls back to pure recency behavior.
    pub enabled: bool,
    /// Gap (s) that ends a "session": follow-up context (last
    /// `converse.max_context_exchanges` exchanges) is only pulled into the
    /// prompt while the gap between exchanges stays under this — so mornings
    /// don't start with the tail of last night's conversation. 0 = no limit
    /// (previous behavior).
    pub session_gap_s: u64,
    /// How many relevant past snippets to retrieve and add to the prompt
    /// (on top of follow-up context). 0 = retrieval disabled (recency only).
    pub retrieve_k: usize,
    /// Length cap per attached snippet (chars) — bounds context tokens.
    pub snippet_max_chars: usize,
    /// Phase 2: nightly consolidation — `claude -p` extracts PERSISTENT facts
    /// about the user from the day's conversations/utterances (preferences,
    /// relationships, recurring tasks, profile) and stores them in semantic
    /// memory (`memory_facts`). false = facts are only added manually
    /// (`jarvis memory add`).
    pub consolidate: bool,
    /// Consolidation runs in the `jarvis run` loop after this local hour
    /// (once a day) — a quiet slot so it doesn't delay dialog or analysis.
    pub consolidate_hour: u8,
    /// Model for fact extraction (cheap is fine — it's text summarization).
    pub consolidate_model: String,
    /// How many facts (pinned profile + relevant retrieved) get added to the
    /// conversation prompt. 0 = no facts added to the prompt.
    pub facts_in_prompt: usize,
    /// Half-life of fact importance (days): older UNpinned facts lose
    /// salience and get pruned below threshold. 0 = no decay (facts live
    /// forever).
    pub fact_half_life_days: u64,
    /// Phase 3: dense vector embeddings (local e5 model via onnxruntime) +
    /// fusion with FTS5 (RRF). Catches synonyms/paraphrases the lexical
    /// layer misses. Activated once the index is populated: `jarvis memory
    /// embed`. Without embeddings (or without onnxruntime), retrieval
    /// silently falls back to FTS only.
    pub vectors: bool,
    /// Embedding model name (multilingual-e5-small = 384 dim, Czech OK).
    /// Used to name the model folder and tag `embeddings.model`.
    pub embed_model: String,
    /// Path to `libonnxruntime.so` (ort load-dynamic). Empty = autodetect
    /// (ORT_DYLIB_PATH env, then pip onnxruntime in ~/.local). Reuses the
    /// existing CPU onnxruntime — nothing extra downloaded or compiled.
    pub onnxruntime_lib: String,
}

impl Default for MemoryCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            session_gap_s: 1800,
            retrieve_k: 4,
            snippet_max_chars: 200,
            consolidate: true,
            consolidate_hour: 4,
            consolidate_model: "claude-haiku-4-5-20251001".into(),
            facts_in_prompt: 8,
            fact_half_life_days: 60,
            vectors: true,
            embed_model: "multilingual-e5-small".into(),
            onnxruntime_lib: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProactiveCfg {
    /// Proactive layer ("nervous system"): from observations (patterns,
    /// runbook runs, utterances) Jarvis offers timely action on its own —
    /// out loud if you're at the desk, otherwise via Telegram. Biased toward
    /// silence: better to stay quiet than interrupt. false = Jarvis stays
    /// purely reactive (only responds to wake address). Safety invariant:
    /// a nudge NEVER runs anything unapproved — it may only inform, offer to
    /// run an ALREADY approved runbook, or offer to generate a proposal
    /// (which still goes through the existing approval flow).
    pub enabled: bool,
    /// How often (s) the nudge loop in `jarvis run` evaluates the situation.
    pub tick_s: u64,
    /// Quiet hours [from, to) local time when Jarvis doesn't interrupt
    /// (wraps past midnight when from > to). from == to = no quiet window.
    pub quiet_from: u8,
    pub quiet_to: u8,
    /// Hard cap on nudges per day (across all detectors) — a safeguard
    /// against nagging. 0 = nudge effectively disabled.
    pub daily_max: u32,
    /// Minimum gap (min) between nudges of the same kind and subject (dedup
    /// key) — something already offered isn't repeated right away.
    pub cooldown_min: u64,
    /// Idle (s) below this threshold = you're at the desk → nudge out loud;
    /// above threshold (or no fresh sample) → Telegram, if enabled.
    pub at_desk_idle_s: u64,
    /// Model for the skeptical classifier (Tier 2 gate: worth interrupting
    /// now?).
    pub model: String,
    /// Both the classifier and actions respect the daily cap
    /// (analysis.daily_budget_usd). false = the cap doesn't stop nudges
    /// (spend is still tracked).
    pub respect_budget: bool,
    /// Detector: a pattern crossed its occurrence threshold and has no
    /// proposal yet → offer to generate an automation. Deterministic, no
    /// classifier.
    pub detect_pattern_ready: bool,
    /// Minimum occurrence count for a pattern to be worth offering.
    pub pattern_min_occurrences: i64,
    /// Detector: an approved runbook keeps failing → offer to show the
    /// log/disable it. Deterministic, no classifier.
    pub detect_runbook_failing: bool,
    /// How many consecutive runbook runs must fail before it speaks up.
    pub runbook_fail_streak: usize,
    /// Detector: an utterance contained an open commitment ("I'll send…",
    /// "I'll write…") → remind after a while. Coarse local filter, confirmed
    /// by the classifier. Experimental: enable only after the kill-gate
    /// (`jarvis nudge-eval`).
    pub detect_commitment: bool,
    /// Remote confirmation: a nudge sent to Telegram carries an action, and
    /// "yes N" / "no N" from a verified chat executes/discards it (reuses
    /// the approval loop). In v1 a voice nudge is informational only (you
    /// state the action in dialog).
    pub telegram_confirm: bool,
}

impl Default for ProactiveCfg {
    fn default() -> Self {
        Self {
            // ship dark: enabled manually only after the kill-gate is verified
            enabled: false,
            tick_s: 120,
            quiet_from: 22,
            quiet_to: 8,
            daily_max: 8,
            cooldown_min: 90,
            at_desk_idle_s: 120,
            model: "claude-haiku-4-5-20251001".into(),
            respect_budget: true,
            // deterministic detectors: safe to enable right away
            detect_pattern_ready: true,
            pattern_min_occurrences: 3,
            detect_runbook_failing: true,
            runbook_fail_streak: 2,
            // needs classifier + kill-gate → disabled by default
            detect_commitment: false,
            telegram_confirm: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImproveCfg {
    /// Self-improvement layer: Jarvis writes and tests changes to its OWN
    /// source on isolated git branches. Safety invariant: it NEVER merges or
    /// deploys anything unapproved — autonomously it only drafts on a branch
    /// and runs the tests there; merging to main needs your approval (TTY or
    /// verified Telegram), and rebuilding the live binary is a further gated
    /// step. false = the whole loop is off (ship dark).
    pub enabled: bool,
    /// Also mine improvement tasks from Jarvis's own signals (failing tests,
    /// clippy, repeatedly-failing runbooks), not just directed "teach yourself
    /// X" requests. false = directed tasks only.
    pub allow_self_source: bool,
    /// Envelope B: auto-merge a GREEN change when its diff is in the safe class
    /// (docs-only). Anything touching code — and always anything touching
    /// gate/build/dependency files — still needs your approval regardless.
    pub auto_merge_safe: bool,
    /// Envelope B+: also auto-merge GREEN ordinary-code changes (feature class),
    /// informing you after the fact rather than asking. Gate/safety-critical
    /// files (config gate defaults, runbook.rs, improve.rs, units.rs, main.rs,
    /// telegram.rs, Cargo.*, .cargo, .github) ALWAYS still need your review — a
    /// self-editing agent must never rewrite its own gates unreviewed.
    pub auto_merge_code: bool,
    /// Auto-merge size cap: a GREEN change above this many changed files, or this
    /// many changed lines (insertions+deletions), goes to human review even if it
    /// is otherwise auto-merge-eligible. A big change has a big blast radius, so a
    /// human always sees it regardless of the flags above.
    pub auto_merge_max_files: usize,
    pub auto_merge_max_lines: usize,
    /// Staged (plan-then-build) mode: a task is first decomposed into at most this
    /// many small, independently-built steps; 1 step = an ordinary single draft.
    pub plan_max_steps: usize,
    /// Envelope C: after a merge, rebuild the binary, smoke-test it, hot-swap
    /// with a .prev rollback, and restart the daemons. false = merge only; you
    /// run `cargo install` yourself (today's behaviour).
    pub deploy_enabled: bool,
    /// Codegen model ("" = the strongest default CLI model — self-editing wants
    /// the best reasoning, unlike the cheap haiku classifiers).
    pub model: String,
    /// Max tool-use turns for the codegen edit loop (edit → build → test → fix).
    pub max_turns: u32,
    /// Hard timeout (s) for one codegen run.
    pub timeout_s: u64,
    /// How many times the agent may retry after red tests before giving up.
    pub repair_attempts: u32,
    /// Daily spend guard for the self-improvement loop (its own budget, on top
    /// of tracking every call in `costs`).
    pub daily_budget_usd: f64,
    /// Max improvement attempts started per day (a nagging/runaway guard).
    pub daily_max: u32,
    /// Branch name prefix for drafts (`<prefix>/<id>-<slug>`).
    pub branch_prefix: String,
    /// git author identity for machine-written commits, so `git log` cleanly
    /// separates self-authored changes from yours.
    pub author_name: String,
    pub author_email: String,
    /// Remote approval of a proposed change via a verified Telegram chat.
    pub telegram_approve: bool,
    /// Source repository path. Empty = the path this binary was built from
    /// (compile-time manifest dir), which for a self-built Jarvis IS the repo.
    pub repo_dir: String,
}

impl Default for ImproveCfg {
    fn default() -> Self {
        Self {
            // ship dark: the entire self-editing loop is off until explicitly enabled
            enabled: false,
            allow_self_source: false,
            auto_merge_safe: false,
            auto_merge_code: false,
            auto_merge_max_files: 3,
            auto_merge_max_lines: 150,
            plan_max_steps: 6,
            deploy_enabled: false,
            model: String::new(),
            max_turns: 40,
            timeout_s: 1800,
            repair_attempts: 2,
            daily_budget_usd: 5.0,
            daily_max: 3,
            branch_prefix: "jarvis/improve".into(),
            author_name: "Jarvis".into(),
            author_email: "jarvis@localhost".into(),
            telegram_approve: false,
            repo_dir: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunbooksCfg {
    /// Phase D: running approved runbooks (timer/voice/CLI). Disabled =
    /// run-due runs nothing and the voice agent doesn't see runbooks;
    /// approval and CLI `jarvis runbook run` still work.
    pub enabled: bool,
    /// Voice agent may `jarvis runbook run` (only already-approved runbooks;
    /// approving by voice is never allowed — the mic isn't trusted).
    pub voice_run: bool,
    /// Hard cap on script runtime; SIGKILLs the whole process group on
    /// expiry.
    pub timeout_s: u64,
    /// Truncate stored run output (DB shouldn't hold megabytes of logs).
    pub max_output_chars: usize,
    /// Report new automation proposals via SMS (requires [sms] enabled).
    pub notify_sms: bool,
    /// Remote approval via Telegram bot (TELEGRAM_BOT_TOKEN +
    /// TELEGRAM_CHAT_ID in secrets.env); run-due handles "approve N" /
    /// "reject N" from a verified chat and reports new proposals there.
    pub telegram_approve: bool,
}

impl Default for RunbooksCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            voice_run: true,
            timeout_s: 600,
            max_output_chars: 4000,
            notify_sms: false,
            telegram_approve: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SmsCfg {
    /// SMS channel (Twilio). Disabled = `jarvis sms` refuses and the agent
    /// doesn't see SMS.
    pub enabled: bool,
    /// Sender: Messaging Service SID (`MG…`), E.164 number, or an
    /// alphanumeric sender (max 11 chars; recipient can't reply).
    pub from: String,
    /// Default recipient in E.164 (+420…) — typically your own mobile.
    pub to: String,
    /// Length guard (SMS is billed per ~70-char segment with diacritics).
    pub max_chars: usize,
}

impl Default for SmsCfg {
    fn default() -> Self {
        Self { enabled: false, from: String::new(), to: String::new(), max_chars: 480 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WmCfg {
    /// Conversation agent may control windows/keyboard/mouse (Bash
    /// restricted to `jarvis wm …`). CLI `jarvis wm` works independent of
    /// this switch.
    pub enabled: bool,
    /// Delay between synthetic keys (XTest), ms.
    pub key_delay_ms: u64,
    /// Programs `jarvis wm spawn` may launch outside an interactive
    /// terminal (voice agent, timers). Matched exactly: a bare name = a
    /// binary in PATH, an absolute path = a specific file. Empty list =
    /// spawn outside a TTY refuses everything; from a terminal it always
    /// works.
    pub spawn_allowed: Vec<String>,
}

impl Default for WmCfg {
    fn default() -> Self {
        Self { enabled: true, key_delay_ms: 12, spawn_allowed: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MeetCfg {
    /// `jarvis meet <URL>` — Jarvis joins a Google Meet as an independent
    /// participant (dedicated Chrome, virtual mic + speaker). false = the
    /// command refuses to run.
    pub enabled: bool,
    /// Browser binary (name in PATH or absolute path). Chrome/Chromium
    /// (WebRTC + reliable audio device selection via PULSE_SINK/PULSE_SOURCE).
    pub chrome_bin: String,
    /// Name Jarvis appears under in the call (fills the "Your name" field).
    pub display_name: String,
    /// PulseAudio null-sink that Jarvis's speech is routed to; its
    /// `.monitor`, remapped to `mic_source`, serves as the mic into the call.
    pub mic_sink: String,
    /// PulseAudio remap-source (from `mic_sink`.monitor) — what Chrome picks
    /// as its microphone (getUserMedia).
    pub mic_source: String,
    /// PulseAudio null-sink that Chrome plays call audio into; its
    /// `.monitor` is what STT listens on (Jarvis hears other participants).
    pub ear_sink: String,
    /// Dedicated Chrome profile directory; empty = `<data_dir>/meet-profile`.
    pub profile_dir: String,
    /// Model for the visual join agent (screenshot → click). Empty = CLI
    /// default.
    pub join_model: String,
    /// Cap on how long the join agent tries to connect (incl. waiting to be
    /// admitted), in s.
    pub join_timeout_s: u64,
    /// Cap on visual join-agent turns (screenshot → action → verify).
    pub join_max_turns: u32,
    /// Continuously transcribe the whole call into the DB (utterances,
    /// source=meet).
    pub transcribe: bool,
    /// Generate and send a meeting summary after the call ends.
    pub summary: bool,
    /// Where to send the summary: "email" | "telegram" | "both" | "none".
    pub summary_to: String,
}

impl Default for MeetCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            chrome_bin: "google-chrome".into(),
            display_name: "Jarvis".into(),
            mic_sink: "jarvis_mic_sink".into(),
            mic_source: "jarvis_mic".into(),
            ear_sink: "jarvis_ear_sink".into(),
            profile_dir: String::new(),
            join_model: String::new(),
            join_timeout_s: 180,
            join_max_turns: 20,
            transcribe: true,
            summary: true,
            summary_to: "email".into(),
        }
    }
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListenCfg {
    pub enabled: bool,
    /// When the screen lock/saver is active (XFCE screensaver, checked via
    /// D-Bus `org.xfce.ScreenSaver.GetActive`), the mic daemon discards
    /// audio and transcribes nothing — same privacy as `jarvis pause`.
    /// Doesn't apply to `jarvis meet` (the call keeps transcribing). Fail
    /// open: if state can't be determined, it runs.
    pub pause_when_locked: bool,
    /// STT engine: "auto" = ElevenLabs Scribe (cloud), falls back to local
    /// whisper on error; "elevenlabs" = Scribe only, no fallback; "whisper"
    /// = local only (free, nothing leaves the machine, but CPU/GPU heavy);
    /// "realtime" = ElevenLabs scribe_v2_realtime over WebSocket — audio is
    /// streamed while you speak (transcript ready ~150 ms after you stop),
    /// falling back to batch Scribe → whisper on error.
    /// In "auto" the whisper model loads lazily on first fallback — as long
    /// as Scribe works, the heavy model never touches the machine.
    pub engine: String,
    /// ElevenLabs Scribe model: "scribe_v2" (newer, lowest WER — better
    /// comprehension) or "scribe_v1". Billed by audio duration (~$0.22/h).
    pub scribe_model: String,
    /// Keyterm biasing for Scribe: proper names it should recognize
    /// accurately (without it, "Jarvisi" gets heard as garbage and the wake
    /// address misses). Analogous to whisper's `hint`. Empty = don't send
    /// (+20% cost when populated).
    pub scribe_keyterms: Vec<String>,
    /// PulseAudio source (`pactl list sources short`); empty = default mic.
    pub device: String,
    /// Self-healing: shell command run when `device` is MISSING from
    /// PulseAudio. Typically reloads `module-echo-cancel`, which breaks when
    /// the USB mic disconnects on sleep (source_master disappears → so does
    /// sink `jarvis_out` and source `jarvis_denoised` → listening goes silent
    /// and `jarvis_out` is also missing for TTS). The listen loop calls the
    /// command and tries to restore the source instead of waiting forever;
    /// rate-limited to ~15s to avoid piling up duplicate modules. Empty =
    /// disabled (previous behavior). Runs via `sh -c`, result only logged.
    pub device_heal_cmd: String,
    /// ggml model name without `ggml-`/`.bin` — downloaded by `jarvis listen
    /// --download-model`.
    pub model: String,
    /// Explicit path to the .bin file; overrides `model`.
    pub model_path: String,
    /// "auto" = per-utterance language detection, otherwise an ISO code
    /// ("cs", "en", …).
    pub language: String,
    /// Whisper dictionary hint (initial prompt) — proper names it would
    /// otherwise mangle ("Jarvisi" → garbage). Empty = no hint.
    pub hint: String,
    /// 0 = auto (half the cores, max 8).
    pub threads: usize,
    pub min_speech_ms: u64,
    pub silence_ms: u64,
    pub max_utterance_s: u64,
    /// VAD sensitivity: threshold = noise floor × this multiplier (lower =
    /// more sensitive).
    pub vad_speech_mult: f32,
}

impl Default for ListenCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            pause_when_locked: true,
            // Scribe is the default: whisper turbo is heavy on typical CPU/GPU
            // (RTF 1-4 without GPU); Scribe moves transcription to the cloud
            // at ~$0.22/h. Without an ElevenLabs key, "auto" silently falls
            // back to local whisper.
            engine: "auto".into(),
            // scribe_v2: newer Scribe with the lowest WER (better Czech comprehension)
            scribe_model: "scribe_v2".into(),
            scribe_keyterms: vec!["Jarvis".into(), "Jarvisi".into()],
            device: String::new(),
            device_heal_cmd: String::new(),
            // turbo on GPU (CUDA build): RTF ~0.2-0.6, best Czech accuracy.
            // CPU fallback for turbo can't keep up (RTF 1-4) — without a GPU
            // switch to "small-q5_1" (RTF ~0.2-0.8 on CPU). See PLAN §3.7 (2026-07-17).
            model: "large-v3-turbo-q5_0".into(),
            model_path: String::new(),
            // pinned language: autodetection costs a full extra encode pass
            // (on GPU too — doubles to triples the time for short utterances)
            language: "cs".into(),
            // whisper doesn't know the assistant's name — mangles it without
            // a hint. Dictionary style is deliberate: on noise/music whisper's
            // hint sometimes hallucinates into the transcript, and a phrase
            // that sounds like a wake address would falsely wake the
            // conversation (echo-guard, see converse::WakeWords).
            hint: "Slovník: Jarvis, Jarvisi, ElevenLabs, digest.".into(),
            threads: 0,
            min_speech_ms: 300,
            // 480: tradeoff between responsiveness and cutting off a sentence
            // mid-breath (700 added ~220ms of dead time per turn). Below
            // ~400 risks clipping.
            silence_ms: 480,
            max_utterance_s: 28,
            // 2.0: on a quiet mic (SNR ~14 dB) a multiplier of 3 clipped sentences
            vad_speech_mult: 2.0,
        }
    }
}

impl ListenCfg {
    pub fn resolve_model_path(&self, paths: &Paths) -> PathBuf {
        if !self.model_path.is_empty() {
            PathBuf::from(&self.model_path)
        } else {
            paths.models_dir.join(format!("ggml-{}.bin", self.model))
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpeakCfg {
    pub enabled: bool,
    /// "auto" = ElevenLabs, falls back to local piper on error; "piper" =
    /// local only (free, nothing leaves the machine); "elevenlabs" = API
    /// only, no fallback.
    pub engine: String,
    /// ElevenLabs voice_id. Premade voices work even with a scoped key
    /// lacking `voices_read`; Voice Library voices must first be added to
    /// the account (web) and their ID pasted here.
    pub voice_id: String,
    pub model_id: String,
    /// ISO code ("cs"); only *_v2_5 models can force it, multilingual_v2
    /// detects language from the text. "auto" = never send language_code.
    pub language: String,
    /// mp3_* only — both playback and the cache assume a container format.
    pub output_format: String,
    /// Stream ElevenLabs audio straight into the player (speech starts after
    /// the first chunk, not the whole mp3) — one-shot replies only; ack/cache
    /// still go through a file. Requires ffplay/mpv (stdin); otherwise/on
    /// error → buffered.
    pub stream: bool,
    /// 0-1; lower = more expressive delivery.
    pub stability: f32,
    /// 0-1; fidelity to the original voice.
    pub similarity_boost: f32,
    /// 0-1; keep low for Czech — higher values distort pronunciation.
    pub style: f32,
    pub speaker_boost: bool,
    /// 0.7-1.2; Brumbál isn't in a hurry.
    pub speed: f32,
    /// Player binary + args; empty = auto (ffplay → mpv → ffmpeg+paplay).
    pub player: String,
    /// PulseAudio sink for Jarvis's speech. Set to the echo-cancel module's
    /// sink (sink_name in default.pa) to give AEC a far-end reference — the
    /// mic then subtracts Jarvis's own voice and doesn't hear itself. Empty
    /// = default output; a nonexistent sink = warn + default output.
    pub sink: String,
    /// Announce the daily digest out loud after sending it.
    pub announce_digest: bool,
    /// The same text with the same settings is only generated once (saves
    /// credits).
    pub cache: bool,
    /// Guard against burning credits: 1 char = 1 credit (multilingual_v2).
    pub max_chars: usize,
    /// Local TTS binary (`pip3 install --user piper-tts`).
    pub piper_bin: String,
    /// Voice from rhasspy/piper-voices; downloaded by `jarvis say
    /// --download-model`.
    pub piper_voice: String,
}

impl Default for SpeakCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: "auto".into(),
            // "George" — premade, warm deeper British narrator; speaks Czech
            // via multilingual_v2 and is the closest premade voice to
            // Brumbál. To pick your own: `jarvis say --list-voices` / web.
            voice_id: "JBFqnCBsd6RMkjVDRZzb".into(),
            // flash_v2_5: an order of magnitude lower latency (critical path
            // for every sentence) and can force language_code=cs (see
            // tts::supports_language_code); the warmest/highest-quality
            // Czech is eleven_multilingual_v2.
            model_id: "eleven_flash_v2_5".into(),
            language: "cs".into(),
            output_format: "mp3_44100_128".into(),
            stream: true,
            stability: 0.5,
            similarity_boost: 0.75,
            style: 0.0,
            speaker_boost: true,
            speed: 0.95,
            player: String::new(),
            sink: String::new(),
            announce_digest: true,
            cache: true,
            max_chars: 2500,
            piper_bin: "piper".into(),
            // the only decent Czech voice in piper-voices (male, medium)
            piper_voice: "cs_CZ-jirka-medium".into(),
        }
    }
}

/// Deserializes either a single string or an array of strings into
/// Vec<String> (backward compat: `ack = "Ano, pane?"` and `ack = ["…", "…"]`
/// both work).
fn string_or_seq<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConverseCfg {
    /// Voice dialog: an utterance with a wake address → Claude → spoken reply.
    pub enabled: bool,
    /// Wake-address stems (case-insensitive, no diacritics/spaces). The
    /// vocative ("jarvisi") doesn't trigger on speech ABOUT the project;
    /// looser: add "jarvis".
    pub wake_words: Vec<String>,
    /// Tolerates 1 transcription edit distance ("Javi si" ≈ "Jarvisi").
    /// Cost: occasionally also catches inflected "jarvis" in ordinary speech.
    pub wake_fuzzy: bool,
    /// Responding without a wake word (addressee detection). "off" = only on
    /// wake address by name (default, current behavior). "followup" = after
    /// Jarvis speaks there's a short window where the next utterance doesn't
    /// need the name (Tier 1). "always" = every plausible utterance is
    /// judged by a skeptical classifier for whether it was addressed to
    /// Jarvis (Tier 2, experimental — enable only after the kill-gate, see
    /// PLAN §3.9).
    pub open_ear: String,
    /// How long the follow-up window stays open after Jarvis speaks (s).
    /// Deliberately short: the longer it is, the higher the risk of butting
    /// in when you've turned to talk to a person instead.
    pub followup_window_s: u64,
    /// Minimum word count for an utterance without a wake word to even be a
    /// candidate (filters out "uh", "yeah" — those shouldn't go to the
    /// worker or the classifier).
    pub open_ear_min_words: usize,
    /// Model for replies; speed > strength (spoken dialog).
    pub model: String,
    /// Model for the skeptical gate (addressee classifier for open-ear and
    /// follow-up): runs BEFORE the expensive `model`, on every candidate
    /// without a wake-by-name, so it should be cheap and fast (haiku). Empty
    /// = falls back to `model`.
    pub gate_model: String,
    /// Immediate reaction to a wake address while Claude composes the reply;
    /// picked randomly from the list for variety. Accepts a single string
    /// too. "" or [] = disabled.
    #[serde(deserialize_with = "string_or_seq")]
    pub ack: Vec<String>,
    /// How many past exchanges are attached for follow-up questions.
    pub max_context_exchanges: usize,
    /// Cap on agent turns when tools are enabled ([wm] enabled) — window
    /// actions need more round trips (command → screenshot → verify).
    /// Without tools it's always 1.
    pub max_turns: u32,
    pub timeout_s: u64,
    /// false = the daily cap (analysis.daily_budget_usd) doesn't block
    /// conversation; spend is still tracked and visible in `status` and the
    /// digest.
    pub respect_budget: bool,
    /// Resident claude process (stream-json): replies without a CLI startup
    /// (~2s saved). Automatic fallback to a one-shot spawn on error.
    pub warm: bool,
    /// The process is recycled after this many exchanges — the session
    /// accumulates context and input tokens (cost) would otherwise grow
    /// unbounded.
    pub warm_max_exchanges: usize,
    /// Recycle after idle time (fresh session in the morning instead of
    /// yesterday's).
    pub warm_idle_s: u64,
    /// Conversation agent may search the web (WebSearch/WebFetch) — current
    /// info: weather, news, rates, anything past the model's knowledge
    /// cutoff. Web search is billed through Anthropic (~$0.01/query). false
    /// = the brain runs on trained knowledge only and admits when it lacks
    /// current info.
    pub web: bool,
    /// Barge-in: speak, and Jarvis stops talking. "Jarvisi …" always
    /// interrupts (echo-safe). Interruption without a wake word (voice onset
    /// during speech) only works with AEC — speak.sink set to an
    /// echo-cancel sink; without it Jarvis would trip over its own echo, so
    /// the silent variant is disabled. false = speech isn't interruptible.
    pub barge_in: bool,
    /// How many ms of continuous speech trigger acoustic barge-in (AEC
    /// only). Lower = more responsive but more sensitive to short
    /// noises/coughs.
    pub barge_in_ms: u64,
    /// Filler for long waits: once this many seconds have passed since the
    /// ack and the first sentence of the reply still hasn't started
    /// (typically an agentic wm/web action or a slow model), Jarvis inserts
    /// a reassuring filler ("Ještě chvilku, pane…") and repeats it at the
    /// same cadence until the reply starts. 0 = disabled. Cached phrases
    /// (cheap); barge-in and the start of the reply both cut the filler off.
    pub filler_after_s: u64,
    /// Filler phrases (randomized, for variety). [] or filler_after_s=0 =
    /// disabled.
    #[serde(deserialize_with = "string_or_seq")]
    pub filler: Vec<String>,
    /// When a wake address isn't followed by a real question (just the name
    /// / name + filler), Jarvis asks for a repeat and skips calling Claude
    /// entirely (saves money and latency). Random from the list; [] = gate
    /// disabled (noise goes to Claude as before).
    #[serde(deserialize_with = "string_or_seq")]
    pub reprompt: Vec<String>,
    /// Reprompt only fires when the substantive word count after the wake
    /// address is LESS than this (1 = only when nothing is left) — biased
    /// toward NOT rejecting a real question.
    pub reprompt_min_words: usize,
    /// On the first exchange of a "session" (after a long gap / overnight),
    /// Jarvis greets by time of day instead of the ack ("Dobré ráno,
    /// pane."). false = always just the ack.
    pub greeting: bool,
    /// How long a gap since the last conversation triggers a greeting (s).
    /// Default 4h.
    pub greeting_gap_s: u64,
    /// "Jarvisi dobrou noc" → Jarvis says goodbye and skips calling Claude.
    /// Random from the list; [] = disabled. Only catches clear farewells,
    /// not questions containing them.
    #[serde(deserialize_with = "string_or_seq")]
    pub farewell: Vec<String>,
}

impl Default for ConverseCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            wake_words: vec!["jarvisi".into(), "jarvise".into()],
            wake_fuzzy: true,
            open_ear: "followup".into(),
            followup_window_s: 12,
            open_ear_min_words: 2,
            model: "claude-haiku-4-5-20251001".into(),
            gate_model: "claude-haiku-4-5-20251001".into(),
            ack: vec![
                "Ano, pane?".into(),
                "Poslouchám, pane.".into(),
                "K službám, pane.".into(),
                "Copak, pane?".into(),
                "Prosím, pane?".into(),
                "Přejete si, pane?".into(),
                "Jsem tu, pane.".into(),
                "Poslouchám.".into(),
                "K vašim službám, pane.".into(),
                "Nuže, pane?".into(),
                "Jak mohu posloužit, pane?".into(),
                "Slyším vás, pane.".into(),
                "Tady jsem, pane.".into(),
                "Pozorně poslouchám, pane.".into(),
                "Co pro vás mohu udělat, pane?".into(),
                "Zajisté, pane.".into(),
                "Rád pomohu, pane.".into(),
                "Vždy k službám, pane.".into(),
                // interjections / shorter fillers — sometimes read more like "thinking"
                "Hmm…".into(),
                "Moment, pane…".into(),
                "Vteřinku…".into(),
                "Okamžik, dívám se…".into(),
            ],
            max_context_exchanges: 3,
            max_turns: 12,
            timeout_s: 90,
            respect_budget: true,
            warm: true,
            warm_max_exchanges: 10,
            warm_idle_s: 900,
            web: true,
            barge_in: true,
            barge_in_ms: 250,
            filler_after_s: 7,
            filler: vec![
                "Ještě chvilku, pane…".into(),
                "Okamžik, pane…".into(),
                "Už na tom pracuji…".into(),
                "Ještě moment…".into(),
                "Hned to bude, pane…".into(),
            ],
            reprompt: vec![
                "Promiňte, pane, nerozuměl jsem. Zopakujete to?".into(),
                "Neslyšel jsem dobře, pane — ještě jednou, prosím?".into(),
                "Ano, pane? Nezachytil jsem, co si přejete.".into(),
                "Prosím, pane?".into(),
            ],
            reprompt_min_words: 1,
            greeting: true,
            greeting_gap_s: 14400, // 4h = "new session"
            farewell: vec![
                "Dobrou noc, pane. Odpočiňte si.".into(),
                "Nashledanou, pane.".into(),
                "Mějte se, pane.".into(),
            ],
        }
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

    pub(crate) fn validate(&self) -> Result<()> {
        if self.digest.hour > 23 {
            bail!("digest.hour musí být 0–23, je {}", self.digest.hour);
        }
        if self.capture.meta_interval_s == 0 || self.capture.shot_interval_s == 0 {
            bail!("intervaly snímání musí být >= 1 s");
        }
        if self.capture.max_dimension < 256 {
            bail!("capture.max_dimension musí být >= 256");
        }
        // `contains` is false for NaN/inf too → a single range check covers
        // them. Without this check, a negative cap would permanently block
        // AI (converse only returns BUDGET_REPLY, analysis runs degraded)
        // while NaN would instead disable the cap entirely.
        if !(0.0..=1000.0).contains(&self.analysis.daily_budget_usd) {
            bail!(
                "analysis.daily_budget_usd musí být 0–1000 USD (konečné číslo), je {}",
                self.analysis.daily_budget_usd
            );
        }
        // 0 would make the hourly cleanup delete all screenshots (cutoff =
        // now); the upper bound keeps `screenshots_days * 86400` clear of i64
        // overflow.
        if !(1..=3650).contains(&self.retention.screenshots_days) {
            bail!(
                "retention.screenshots_days musí být 1–3650, je {}",
                self.retention.screenshots_days
            );
        }
        let l = &self.listen;
        if l.model.is_empty() && l.model_path.is_empty() {
            bail!("listen.model nebo listen.model_path musí být vyplněné");
        }
        if !l.model.is_empty()
            && !l.model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            bail!("listen.model smí obsahovat jen [A-Za-z0-9._-], je '{}'", l.model);
        }
        let lang_ok = l.language == "auto"
            || ((2..=3).contains(&l.language.len())
                && l.language.chars().all(|c| c.is_ascii_lowercase()));
        if !lang_ok {
            bail!("listen.language musí být 'auto' nebo ISO kód (cs, en, …), je '{}'", l.language);
        }
        if !(5..=28).contains(&l.max_utterance_s) {
            bail!("listen.max_utterance_s musí být 5–28 (whisper okno je 30 s), je {}", l.max_utterance_s);
        }
        if !(60..=5000).contains(&l.min_speech_ms) {
            bail!("listen.min_speech_ms musí být 60–5000, je {}", l.min_speech_ms);
        }
        if !(200..=5000).contains(&l.silence_ms) {
            bail!("listen.silence_ms musí být 200–5000, je {}", l.silence_ms);
        }
        if l.threads > 64 {
            bail!("listen.threads musí být 0–64, je {}", l.threads);
        }
        if l.hint.chars().count() > 200 {
            bail!("listen.hint je moc dlouhý ({} znaků, max 200)", l.hint.chars().count());
        }
        if !(1.2..=10.0).contains(&l.vad_speech_mult) {
            bail!("listen.vad_speech_mult musí být 1.2–10, je {}", l.vad_speech_mult);
        }
        if !matches!(l.engine.as_str(), "auto" | "elevenlabs" | "whisper" | "realtime") {
            bail!(
                "listen.engine musí být auto | elevenlabs | whisper | realtime, je '{}'",
                l.engine
            );
        }
        if l.scribe_model.is_empty()
            || !l.scribe_model.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            bail!("listen.scribe_model smí obsahovat jen [a-z0-9_], je '{}'", l.scribe_model);
        }
        if l.scribe_keyterms.len() > 100 {
            bail!("listen.scribe_keyterms: max 100 termů (je {})", l.scribe_keyterms.len());
        }
        for kt in &l.scribe_keyterms {
            let n = kt.chars().count();
            if !(1..=50).contains(&n) {
                bail!("listen.scribe_keyterms: každý term musí být 1–50 znaků, je '{kt}' ({n})");
            }
            if kt.contains(['<', '>', '{', '}', '[', ']', '\\']) {
                bail!("listen.scribe_keyterms: term nesmí obsahovat <>{{}}[]\\ — je '{kt}'");
            }
        }
        let s = &self.speak;
        if !matches!(s.engine.as_str(), "auto" | "elevenlabs" | "piper") {
            bail!("speak.engine musí být auto | elevenlabs | piper, je '{}'", s.engine);
        }
        if s.piper_bin.trim().is_empty() {
            bail!("speak.piper_bin nesmí být prázdné (default \"piper\")");
        }
        if s.piper_voice.is_empty()
            || !s
                .piper_voice
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            bail!("speak.piper_voice smí obsahovat jen [A-Za-z0-9._-], je '{}'", s.piper_voice);
        }
        if s.voice_id.is_empty()
            || s.voice_id.len() > 64
            || !s.voice_id.chars().all(|c| c.is_ascii_alphanumeric())
        {
            bail!("speak.voice_id musí být alfanumerické ID ElevenLabs hlasu, je '{}'", s.voice_id);
        }
        if s.model_id.is_empty()
            || !s.model_id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            bail!("speak.model_id smí obsahovat jen [a-z0-9_], je '{}'", s.model_id);
        }
        let lang_ok = s.language == "auto"
            || ((2..=3).contains(&s.language.len())
                && s.language.chars().all(|c| c.is_ascii_lowercase()));
        if !lang_ok {
            bail!("speak.language musí být 'auto' nebo ISO kód (cs, en, …), je '{}'", s.language);
        }
        if !s.output_format.starts_with("mp3_")
            || !s.output_format.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            bail!(
                "speak.output_format: podporuji jen mp3_* (např. mp3_44100_128), je '{}'",
                s.output_format
            );
        }
        for (name, v) in [
            ("stability", s.stability),
            ("similarity_boost", s.similarity_boost),
            ("style", s.style),
        ] {
            if !(0.0..=1.0).contains(&v) {
                bail!("speak.{name} musí být 0–1, je {v}");
            }
        }
        if !(0.7..=1.2).contains(&s.speed) {
            bail!("speak.speed musí být 0.7–1.2 (limit ElevenLabs), je {}", s.speed);
        }
        if !s.sink.is_empty()
            && !s.sink.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            bail!("speak.sink smí obsahovat jen [A-Za-z0-9._-], je '{}'", s.sink);
        }
        if !(1..=10_000).contains(&s.max_chars) {
            bail!("speak.max_chars musí být 1–10000 (limit requestu ElevenLabs), je {}", s.max_chars);
        }
        let c = &self.converse;
        if c.wake_words.is_empty() {
            bail!("converse.wake_words nesmí být prázdné (např. [\"jarvisi\"])");
        }
        for w in &c.wake_words {
            let w = w.trim();
            if !(3..=30).contains(&w.chars().count()) || !w.chars().all(char::is_alphanumeric) {
                bail!("converse.wake_words: kmen musí být 3–30 alfanumerických znaků, je '{w}'");
            }
        }
        if c.model.trim().is_empty() {
            bail!("converse.model nesmí být prázdný");
        }
        match c.open_ear.as_str() {
            "off" | "followup" | "always" => {}
            other => bail!(
                "converse.open_ear musí být \"off\", \"followup\" nebo \"always\", je '{other}'"
            ),
        }
        if !(3..=120).contains(&c.followup_window_s) {
            bail!("converse.followup_window_s musí být 3–120, je {}", c.followup_window_s);
        }
        if !(1..=20).contains(&c.open_ear_min_words) {
            bail!("converse.open_ear_min_words musí být 1–20, je {}", c.open_ear_min_words);
        }
        if !(60..=2000).contains(&c.barge_in_ms) {
            bail!("converse.barge_in_ms musí být 60–2000, je {}", c.barge_in_ms);
        }
        if c.filler_after_s != 0 && !(3..=120).contains(&c.filler_after_s) {
            bail!("converse.filler_after_s musí být 0 (vypnuto) nebo 3–120, je {}", c.filler_after_s);
        }
        if !(1..=5).contains(&c.reprompt_min_words) {
            bail!("converse.reprompt_min_words musí být 1–5, je {}", c.reprompt_min_words);
        }
        if !(60..=86_400).contains(&c.greeting_gap_s) {
            bail!("converse.greeting_gap_s musí být 60–86400, je {}", c.greeting_gap_s);
        }
        if !(10..=600).contains(&c.timeout_s) {
            bail!("converse.timeout_s musí být 10–600, je {}", c.timeout_s);
        }
        if c.max_context_exchanges > 20 {
            bail!("converse.max_context_exchanges musí být 0–20, je {}", c.max_context_exchanges);
        }
        if !(1..=100).contains(&c.warm_max_exchanges) {
            bail!("converse.warm_max_exchanges musí být 1–100, je {}", c.warm_max_exchanges);
        }
        if !(60..=86_400).contains(&c.warm_idle_s) {
            bail!("converse.warm_idle_s musí být 60–86400, je {}", c.warm_idle_s);
        }
        if !(1..=40).contains(&c.max_turns) {
            bail!("converse.max_turns musí být 1–40, je {}", c.max_turns);
        }
        if self.wm.key_delay_ms > 500 {
            bail!("wm.key_delay_ms musí být 0–500, je {}", self.wm.key_delay_ms);
        }
        for p in &self.wm.spawn_allowed {
            if p.trim().is_empty() || p.chars().any(|c| c.is_whitespace() || c.is_control()) {
                bail!(
                    "wm.spawn_allowed: položka musí být jméno binárky nebo absolutní \
                     cesta bez mezer, je '{p}'"
                );
            }
        }
        let rb = &self.runbooks;
        if !(10..=7200).contains(&rb.timeout_s) {
            bail!("runbooks.timeout_s musí být 10–7200, je {}", rb.timeout_s);
        }
        if !(200..=100_000).contains(&rb.max_output_chars) {
            bail!("runbooks.max_output_chars musí být 200–100000, je {}", rb.max_output_chars);
        }
        let tk = &self.tasks;
        if !(200..=100_000).contains(&tk.max_output_chars) {
            bail!("tasks.max_output_chars musí být 200–100000, je {}", tk.max_output_chars);
        }
        if tk.min_disk_free_mb > 1_000_000 {
            bail!("tasks.min_disk_free_mb musí být 0–1000000 (max ~1 TB), je {}", tk.min_disk_free_mb);
        }
        let mem = &self.memory;
        if mem.retrieve_k > 50 {
            bail!("memory.retrieve_k musí být 0–50, je {}", mem.retrieve_k);
        }
        if mem.session_gap_s > 86_400 {
            bail!("memory.session_gap_s musí být 0–86400, je {}", mem.session_gap_s);
        }
        if !(20..=2000).contains(&mem.snippet_max_chars) {
            bail!("memory.snippet_max_chars musí být 20–2000, je {}", mem.snippet_max_chars);
        }
        if mem.consolidate_hour > 23 {
            bail!("memory.consolidate_hour musí být 0–23, je {}", mem.consolidate_hour);
        }
        if mem.consolidate_model.trim().is_empty() {
            bail!("memory.consolidate_model nesmí být prázdný");
        }
        if mem.facts_in_prompt > 50 {
            bail!("memory.facts_in_prompt musí být 0–50, je {}", mem.facts_in_prompt);
        }
        if mem.fact_half_life_days > 3650 {
            bail!("memory.fact_half_life_days musí být 0–3650, je {}", mem.fact_half_life_days);
        }
        if mem.embed_model.is_empty()
            || !mem.embed_model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            bail!("memory.embed_model smí obsahovat jen [A-Za-z0-9._-], je '{}'", mem.embed_model);
        }
        let sm = &self.sms;
        if sm.enabled {
            let from_ok = crate::sms::is_messaging_sid(&sm.from)
                || crate::sms::is_e164(&sm.from)
                || crate::sms::is_alpha_sender(&sm.from);
            if !from_ok {
                bail!(
                    "sms.from musí být Messaging Service SID (MG…), E.164 číslo (+420…) \
                     nebo alfanumerický sender (max 11 znaků), je '{}'",
                    sm.from
                );
            }
            if !crate::sms::is_e164(&sm.to) {
                bail!("sms.to musí být E.164 číslo (+420123456789), je '{}'", sm.to);
            }
            if !(1..=1600).contains(&sm.max_chars) {
                bail!("sms.max_chars musí být 1–1600 (limit Twilio), je {}", sm.max_chars);
            }
        }
        let m = &self.meet;
        if m.enabled {
            if m.chrome_bin.trim().is_empty()
                || m.chrome_bin.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                bail!("meet.chrome_bin musí být jméno binárky nebo cesta bez mezer, je '{}'", m.chrome_bin);
            }
            let name = m.display_name.trim();
            if name.is_empty() || name.chars().count() > 60 {
                bail!("meet.display_name musí být 1–60 znaků, je '{}'", m.display_name);
            }
            let dev_ok = |s: &str| {
                !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            };
            for (field, val) in
                [("mic_sink", &m.mic_sink), ("mic_source", &m.mic_source), ("ear_sink", &m.ear_sink)]
            {
                if !dev_ok(val) {
                    bail!("meet.{field} smí obsahovat jen [A-Za-z0-9._-] a nesmí být prázdné, je '{val}'");
                }
            }
            if m.mic_sink == m.ear_sink {
                bail!("meet.mic_sink a meet.ear_sink musí být různé názvy");
            }
            if !(30..=1800).contains(&m.join_timeout_s) {
                bail!("meet.join_timeout_s musí být 30–1800, je {}", m.join_timeout_s);
            }
            if !(1..=60).contains(&m.join_max_turns) {
                bail!("meet.join_max_turns musí být 1–60, je {}", m.join_max_turns);
            }
            if !matches!(m.summary_to.as_str(), "email" | "telegram" | "both" | "none") {
                bail!("meet.summary_to musí být email | telegram | both | none, je '{}'", m.summary_to);
            }
        }
        let pr = &self.proactive;
        if !(20..=3600).contains(&pr.tick_s) {
            bail!("proactive.tick_s musí být 20–3600, je {}", pr.tick_s);
        }
        if pr.quiet_from > 23 || pr.quiet_to > 23 {
            bail!("proactive.quiet_from/quiet_to musí být 0–23 (je {}/{})", pr.quiet_from, pr.quiet_to);
        }
        if pr.daily_max > 100 {
            bail!("proactive.daily_max musí být 0–100, je {}", pr.daily_max);
        }
        if pr.cooldown_min > 1440 {
            bail!("proactive.cooldown_min musí být 0–1440 (max den), je {}", pr.cooldown_min);
        }
        if !(10..=3600).contains(&pr.at_desk_idle_s) {
            bail!("proactive.at_desk_idle_s musí být 10–3600, je {}", pr.at_desk_idle_s);
        }
        if pr.model.trim().is_empty() {
            bail!("proactive.model nesmí být prázdný");
        }
        if !(1..=1000).contains(&pr.pattern_min_occurrences) {
            bail!("proactive.pattern_min_occurrences musí být 1–1000, je {}", pr.pattern_min_occurrences);
        }
        if !(1..=20).contains(&pr.runbook_fail_streak) {
            bail!("proactive.runbook_fail_streak musí být 1–20, je {}", pr.runbook_fail_streak);
        }
        let im = &self.improve;
        if !(1..=200).contains(&im.max_turns) {
            bail!("improve.max_turns musí být 1–200, je {}", im.max_turns);
        }
        if !(60..=7200).contains(&im.timeout_s) {
            bail!("improve.timeout_s musí být 60–7200, je {}", im.timeout_s);
        }
        if im.repair_attempts > 10 {
            bail!("improve.repair_attempts musí být 0–10, je {}", im.repair_attempts);
        }
        if !(0.0..=100.0).contains(&im.daily_budget_usd) {
            bail!("improve.daily_budget_usd musí být 0–100, je {}", im.daily_budget_usd);
        }
        if im.daily_max > 50 {
            bail!("improve.daily_max musí být 0–50, je {}", im.daily_max);
        }
        if im.branch_prefix.is_empty()
            || !im
                .branch_prefix
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | '_' | '-'))
        {
            bail!(
                "improve.branch_prefix smí obsahovat jen [a-z0-9/_-] a nesmí být prázdný, je '{}'",
                im.branch_prefix
            );
        }
        if im.author_name.trim().is_empty() {
            bail!("improve.author_name nesmí být prázdný");
        }
        if !im.author_email.contains('@') {
            bail!("improve.author_email musí vypadat jako e-mail, je '{}'", im.author_email);
        }
        if !im.model.is_empty()
            && !im.model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            bail!("improve.model smí obsahovat jen [A-Za-z0-9._-], je '{}'", im.model);
        }
        if !im.repo_dir.is_empty() && !std::path::Path::new(&im.repo_dir).is_absolute() {
            bail!("improve.repo_dir musí být absolutní cesta, je '{}'", im.repo_dir);
        }
        if !(1..=500).contains(&im.auto_merge_max_files) {
            bail!("improve.auto_merge_max_files musí být 1–500, je {}", im.auto_merge_max_files);
        }
        if !(1..=100_000).contains(&im.auto_merge_max_lines) {
            bail!("improve.auto_merge_max_lines musí být 1–100000, je {}", im.auto_merge_max_lines);
        }
        if !(1..=20).contains(&im.plan_max_steps) {
            bail!("improve.plan_max_steps musí být 1–20, je {}", im.plan_max_steps);
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
    pub models_dir: PathBuf,
    pub tts_cache_dir: PathBuf,
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
            models_dir: data_dir.join("models"),
            tts_cache_dir: data_dir.join("tts-cache"),
            db_path: data_dir.join("jarvis.db"),
            config_dir,
            data_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.config_dir,
            &self.data_dir,
            &self.shots_dir,
            &self.proposals_dir,
            &self.models_dir,
            &self.tts_cache_dir,
        ] {
            fs::create_dir_all(dir).with_context(|| format!("nelze vytvořit {}", dir.display()))?;
        }
        // both data and config hold sensitive data — user-only permissions
        for dir in [&self.config_dir, &self.data_dir] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("nelze nastavit práva {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Secret lookup: env var `name` takes priority, else a `name=…` line in secrets.env.
fn secret(paths: &Paths, name: &str) -> Result<String> {
    if let Ok(k) = std::env::var(name) {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Ok(k);
        }
    }
    let text = fs::read_to_string(&paths.secrets_file).with_context(|| {
        format!(
            "{name} není v env a nelze číst {}",
            paths.secrets_file.display()
        )
    })?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(v) = line.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            let v = v.trim().trim_matches('"').to_string();
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    bail!(
        "{name} nenalezen v {} ani v prostředí",
        paths.secrets_file.display()
    )
}

pub fn sendgrid_key(paths: &Paths) -> Result<String> {
    secret(paths, "SENDGRID_API_KEY")
}

pub fn elevenlabs_key(paths: &Paths) -> Result<String> {
    secret(paths, "ELEVENLABS_API_KEY")
}

/// (account SID, auth token) for Twilio SMS.
pub fn twilio_keys(paths: &Paths) -> Result<(String, String)> {
    Ok((secret(paths, "TWILIO_ACCOUNT_SID")?, secret(paths, "TWILIO_AUTH_TOKEN")?))
}

/// (bot token, chat id) for approving runbooks via Telegram.
pub fn telegram_keys(paths: &Paths) -> Result<(String, String)> {
    Ok((secret(paths, "TELEGRAM_BOT_TOKEN")?, secret(paths, "TELEGRAM_CHAT_ID")?))
}

/// Parses "30m", "2h", "7d", "45s", or bare seconds into seconds.
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
    // checked: an absurd input ("1000000000000000d") would otherwise
    // overflow u64 (debug panic / release wraparound → a nonsense pause,
    // even negative after `+ now_ts`)
    n.checked_mul(mult).with_context(|| format!("trvání '{s}' je mimo rozsah"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn ack_accepts_string_or_list() {
        // backward compat: a single string → a one-element list
        let one: Config = toml::from_str("[converse]\nack = \"Jistě?\"\n").unwrap();
        assert_eq!(one.converse.ack, vec!["Jistě?".to_string()]);
        one.validate().unwrap();
        // list of phrases
        let many: Config = toml::from_str("[converse]\nack = [\"A\", \"B\"]\n").unwrap();
        assert_eq!(many.converse.ack, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn open_ear_validation_rejects_bad_values() {
        let mut cfg = Config::default();
        cfg.converse.open_ear = "sometimes".into();
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.converse.followup_window_s = 0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.converse.open_ear_min_words = 0;
        assert!(cfg.validate().is_err());
        // valid modes pass
        for m in ["off", "followup", "always"] {
            let mut cfg = Config::default();
            cfg.converse.open_ear = m.into();
            assert!(cfg.validate().is_ok(), "režim {m} má být platný");
        }
    }

    #[test]
    fn memory_validation_rejects_bad_values() {
        let mut cfg = Config::default();
        cfg.memory.retrieve_k = 999;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.memory.snippet_max_chars = 0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.memory.session_gap_s = 999_999;
        assert!(cfg.validate().is_err());
        // boundary-valid values pass (0 = retrieval/limit disabled)
        let mut cfg = Config::default();
        cfg.memory.retrieve_k = 0;
        cfg.memory.session_gap_s = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn proactive_validation_rejects_bad_values() {
        let mut cfg = Config::default();
        cfg.proactive.tick_s = 5; // below minimum
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.proactive.quiet_from = 24; // outside 0-23
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.proactive.at_desk_idle_s = 0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.proactive.pattern_min_occurrences = 0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.proactive.runbook_fail_streak = 0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.proactive.model = "  ".into();
        assert!(cfg.validate().is_err());
        // both the default (disabled) and enabled with sane values pass
        assert!(Config::default().validate().is_ok());
        let mut cfg = Config::default();
        cfg.proactive.enabled = true;
        cfg.proactive.quiet_from = 0;
        cfg.proactive.quiet_to = 0; // equal = no quiet window
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn tasks_validation_rejects_bad_values() {
        let mut cfg = Config::default();
        cfg.tasks.max_output_chars = 10; // below minimum
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.tasks.max_output_chars = 200_000; // above maximum
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.tasks.min_disk_free_mb = 2_000_000; // above maximum
        assert!(cfg.validate().is_err());
        // default and boundary-valid values pass (0 MB = disk warning disabled)
        assert!(Config::default().validate().is_ok());
        let mut cfg = Config::default();
        cfg.tasks.min_disk_free_mb = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn converse_reprompt_greeting_validation_and_defaults() {
        let mut cfg = Config::default();
        cfg.converse.reprompt_min_words = 0; // below minimum
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.converse.reprompt_min_words = 9; // above maximum
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.converse.greeting_gap_s = 10; // gap too short
        assert!(cfg.validate().is_err());
        // sane defaults and non-empty lists
        let d = Config::default();
        assert!((1..=5).contains(&d.converse.reprompt_min_words));
        assert!(d.converse.greeting);
        assert!(!d.converse.reprompt.is_empty());
        assert!(!d.converse.farewell.is_empty());
        assert!(cfg_ok(&d));
        // reprompt/farewell also accept a single string (string_or_seq)
        let one: Config =
            toml::from_str("[converse]\nreprompt = \"Co, pane?\"\nfarewell = \"Ahoj.\"\n").unwrap();
        assert_eq!(one.converse.reprompt, vec!["Co, pane?".to_string()]);
        assert_eq!(one.converse.farewell, vec!["Ahoj.".to_string()]);
    }

    fn cfg_ok(c: &Config) -> bool {
        c.validate().is_ok()
    }

    #[test]
    fn converse_filler_validation_and_defaults() {
        let mut cfg = Config::default();
        cfg.converse.filler_after_s = 1; // >0 but below minimum 3
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.converse.filler_after_s = 200; // above maximum
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.converse.filler_after_s = 0; // disabled = valid
        assert!(cfg.validate().is_ok());
        // default has filler enabled with a sane value and a non-empty list
        let d = Config::default();
        assert!((3..=120).contains(&d.converse.filler_after_s));
        assert!(!d.converse.filler.is_empty());
        // default ack also includes interjections (…)
        assert!(d.converse.ack.iter().any(|a| a.contains('…')));
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.email.to, "dankrul.krul@gmail.com");
        assert_eq!(cfg.digest.hour, 19);
        assert_eq!(cfg.retention.screenshots_days, 7);
        // Czech is the default for the whole assistant
        assert_eq!(cfg.listen.language, "cs");
        assert_eq!(cfg.speak.language, "cs");
        // snappy defaults for a smooth conversation
        assert_eq!(cfg.speak.model_id, "eleven_flash_v2_5");
        assert!(cfg.speak.stream);
        assert_eq!(cfg.listen.silence_ms, 480);
        assert_eq!(cfg.converse.open_ear, "followup");
        assert!(cfg.converse.barge_in);
    }

    #[test]
    fn speak_validation_rejects_bad_values() {
        let mut cfg = Config::default();
        cfg.speak.stability = 1.5;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.speak.speed = 0.3;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.speak.voice_id = "../../etc/passwd".into();
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.speak.output_format = "pcm_44100".into();
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.speak.language = "czech".into();
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.speak.max_chars = 0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.speak.engine = "espeak".into();
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.speak.piper_voice = "../evil".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn secret_reads_any_key_from_env_file() {
        // env would override the file — must be clean for the test
        std::env::remove_var("SENDGRID_API_KEY");
        std::env::remove_var("ELEVENLABS_API_KEY");
        let dir = std::env::temp_dir().join(format!("jarvis-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("secrets.env");
        fs::write(&f, "# komentář\nSENDGRID_API_KEY=sg1\nELEVENLABS_API_KEY=\"el1\"\n").unwrap();
        let mut paths = Paths::new().unwrap();
        paths.secrets_file = f;
        assert_eq!(sendgrid_key(&paths).unwrap(), "sg1");
        assert_eq!(elevenlabs_key(&paths).unwrap(), "el1");
        let _ = fs::remove_dir_all(&dir);
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
