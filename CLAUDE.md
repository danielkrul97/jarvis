# CLAUDE.md

Jarvis — a Rust X11 watcher + full voice assistant (STT → Claude → TTS), proactive
nudges, runbooks, and a daily email digest. Jarvis speaks **Czech** to the user.

## Code comments

- **Write all code comments in English** (`//`, `///`, `//!`). No Czech in comments.
- **Be brief.** Explain *why*, not *what* the code obviously does — no line-by-line
  narration, no restating the signature. Keep the rationale and edge-case notes;
  drop the padding.
- **User-facing Czech strings stay Czech** — they are data, not comments: TTS
  replies and prompts, wake-words, acknowledgements, and Czech test fixtures
  (e.g. `assert_eq!(..., "třetí")`). A comment may quote a Czech token when it
  explains language logic (e.g. `// doesn't start with "ano"`).
