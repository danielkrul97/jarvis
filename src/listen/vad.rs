//! Energy-based VAD (voice activity detection) with an adaptive noise threshold.
//!
//! Deliberately simple and fully testable: frame RMS is compared against a
//! multiple of a floating noise floor. An utterance starts on a loud frame
//! (with a pre-roll so the first word's onset isn't cut off), ends after
//! `silence_ms` of silence, and speech that runs too long is split at
//! `max_utterance_ms` (whisper handles a max 30s window). Known limitation:
//! sudden sustained noise (fan, music) passes through to STT and only gets
//! filtered by whisper's no-speech threshold — the upgrade path to Silero VAD
//! is in PLAN.md.

use std::collections::VecDeque;

pub const SAMPLE_RATE: usize = 16_000;
pub const FRAME_MS: u64 = 30;
pub const FRAME_SAMPLES: usize = SAMPLE_RATE * FRAME_MS as usize / 1000; // 480

/// Speech threshold = max(noise floor × speech_mult, MIN_THRESHOLD); RMS on
/// a [-1, 1] scale. speech_mult lives in config (`listen.vad_speech_mult`) —
/// on a quiet mic with low SNR (measured 2026-07-17: speech only ~14 dB above
/// noise) a multiplier of 3 chopped sentences into fragments; default is
/// therefore 2.0.
const MIN_THRESHOLD: f32 = 0.0045;
/// EMA of the noise floor (updated only on silent frames; τ ≈ 0.6 s).
const FLOOR_ALPHA: f32 = 0.05;
const FLOOR_INIT: f32 = 0.003;
/// How much audio before the first loud frame to keep (word onset).
const PREROLL_MS: u64 = 300;
/// Overlap when force-splitting long speech — so a word isn't cut exactly at the split.
const SPLIT_CARRY_MS: u64 = 200;
/// How much trailing silence to keep in an utterance; the rest is trimmed (saves STT).
const TAIL_SILENCE_MS: u64 = 150;

#[derive(Debug, Clone)]
pub struct VadConfig {
    pub min_speech_ms: u64,
    pub silence_ms: u64,
    pub max_utterance_ms: u64,
    /// Multiple of the noise floor above which a frame counts as "speech".
    pub speech_mult: f32,
}

/// A complete utterance: PCM 16 kHz mono + start/end epochs (seconds).
#[derive(Debug)]
pub struct Utterance {
    pub samples: Vec<i16>,
    pub started_at: i64,
    pub ended_at: i64,
}

enum State {
    Idle,
    Active { buf: Vec<i16>, voiced_ms: u64, trailing_ms: u64, started_at: i64 },
}

pub struct Vad {
    cfg: VadConfig,
    floor: f32,
    preroll: VecDeque<i16>,
    state: State,
}

fn rms(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    ((sum / frame.len() as f64).sqrt() / 32768.0) as f32
}

const fn ms_to_samples(ms: u64) -> usize {
    SAMPLE_RATE * ms as usize / 1000
}

impl Vad {
    pub fn new(cfg: VadConfig) -> Self {
        Self {
            cfg,
            floor: FLOOR_INIT,
            preroll: VecDeque::with_capacity(ms_to_samples(PREROLL_MS)),
            state: State::Idle,
        }
    }

    /// The threshold is public for diagnostics (doctor --live).
    pub fn threshold(&self) -> f32 {
        (self.floor * self.cfg.speech_mult).max(MIN_THRESHOLD)
    }

    /// Accumulated speech duration in the current utterance (ms); 0 = not
    /// speaking right now. The mic loop uses it to detect voice-onset during
    /// Jarvis's speech (barge-in).
    pub fn active_voiced_ms(&self) -> u64 {
        match &self.state {
            State::Active { voiced_ms, .. } => *voiced_ms,
            State::Idle => 0,
        }
    }

    /// Epoch (s) when the running utterance started, if one is active — used
    /// to pair an acoustic barge-in with the utterance that triggered it (see
    /// handle_utterance).
    pub fn active_started_at(&self) -> Option<i64> {
        match &self.state {
            State::Active { started_at, .. } => Some(*started_at),
            State::Idle => None,
        }
    }

    /// Drops any in-progress state (capture paused).
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.preroll.clear();
    }

    /// Accepts one frame (`FRAME_SAMPLES` samples) and optionally returns a
    /// completed utterance. `now_ts` = frame arrival epoch in seconds.
    pub fn push_frame(&mut self, frame: &[i16], now_ts: i64) -> Option<Utterance> {
        let loud = rms(frame) >= self.threshold();
        match &mut self.state {
            State::Idle => {
                if loud {
                    let mut buf: Vec<i16> = self.preroll.iter().copied().collect();
                    let preroll_ms = (buf.len() * 1000 / SAMPLE_RATE) as u64;
                    self.preroll.clear();
                    buf.extend_from_slice(frame);
                    self.state = State::Active {
                        buf,
                        voiced_ms: FRAME_MS,
                        trailing_ms: 0,
                        started_at: now_ts - ((preroll_ms + FRAME_MS) / 1000) as i64,
                    };
                } else {
                    // the noise floor is only learned from silence
                    self.floor += FLOOR_ALPHA * (rms(frame) - self.floor);
                    self.preroll.extend(frame.iter().copied());
                    while self.preroll.len() > ms_to_samples(PREROLL_MS) {
                        self.preroll.pop_front();
                    }
                }
                None
            }
            State::Active { buf, voiced_ms, trailing_ms, started_at } => {
                buf.extend_from_slice(frame);
                if loud {
                    *voiced_ms += FRAME_MS;
                    *trailing_ms = 0;
                } else {
                    *trailing_ms += FRAME_MS;
                    // silence inside/after speech may also update the floor
                    // (slowly rising noise must not keep Active forever)
                    self.floor += FLOOR_ALPHA * (rms(frame) - self.floor);
                }

                if *trailing_ms >= self.cfg.silence_ms {
                    let voiced = *voiced_ms;
                    let trailing = *trailing_ms;
                    let started = *started_at;
                    let mut samples = std::mem::take(buf);
                    self.state = State::Idle;
                    if voiced < self.cfg.min_speech_ms {
                        return None; // click, tap — drop it
                    }
                    let cut_ms = trailing.saturating_sub(TAIL_SILENCE_MS);
                    samples.truncate(samples.len().saturating_sub(ms_to_samples(cut_ms)));
                    return Some(Utterance {
                        samples,
                        started_at: started,
                        ended_at: now_ts - (cut_ms / 1000) as i64,
                    });
                }

                let buf_ms = (buf.len() * 1000 / SAMPLE_RATE) as u64;
                if buf_ms >= self.cfg.max_utterance_ms {
                    // force-split long speech; carry a tail slice into the next
                    // utterance so the cut doesn't slice a word too hard
                    let carry_len = ms_to_samples(SPLIT_CARRY_MS).min(buf.len());
                    let carry: Vec<i16> = buf[buf.len() - carry_len..].to_vec();
                    // carry is the onset of the NEXT utterance — trim it off the
                    // one being sent, so the same 200 ms isn't transcribed twice
                    // (end of N == start of N+1)
                    let mut samples = std::mem::take(buf);
                    samples.truncate(samples.len() - carry_len);
                    let started = *started_at;
                    let voiced = *voiced_ms;
                    self.state = State::Active {
                        buf: carry,
                        voiced_ms: FRAME_MS, // continuing speech will prove itself via later frames
                        trailing_ms: 0,
                        started_at: now_ts,
                    };
                    if voiced >= self.cfg.min_speech_ms {
                        return Some(Utterance {
                            samples,
                            started_at: started,
                            ended_at: now_ts,
                        });
                    }
                    return None;
                }
                None
            }
        }
    }

    /// Closes an in-progress utterance (end of a WAV file; the daemon never calls flush).
    pub fn flush(&mut self, now_ts: i64) -> Option<Utterance> {
        let state = std::mem::replace(&mut self.state, State::Idle);
        if let State::Active { buf, voiced_ms, started_at, .. } = state {
            if voiced_ms >= self.cfg.min_speech_ms {
                return Some(Utterance { samples: buf, started_at, ended_at: now_ts });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VadConfig {
        VadConfig { min_speech_ms: 300, silence_ms: 700, max_utterance_ms: 28_000, speech_mult: 2.0 }
    }

    fn sine(ms: u64, amp: f32) -> Vec<i16> {
        (0..ms_to_samples(ms))
            .map(|i| {
                (amp * 32768.0
                    * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SAMPLE_RATE as f32).sin())
                    as i16
            })
            .collect()
    }

    /// Deterministic weak noise (LCG) — realistic mic "silence".
    fn noise(ms: u64, amp: i16, seed: &mut u64) -> Vec<i16> {
        (0..ms_to_samples(ms))
            .map(|_| {
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((*seed >> 33) as i64 % (2 * amp as i64 + 1) - amp as i64) as i16
            })
            .collect()
    }

    /// Feeds a signal through the VAD frame by frame with a ticking epoch; returns utterances.
    fn feed(vad: &mut Vad, signal: &[i16], t_ms: &mut u64) -> Vec<Utterance> {
        let mut out = Vec::new();
        for frame in signal.chunks(FRAME_SAMPLES) {
            if frame.len() < FRAME_SAMPLES {
                break;
            }
            let now = 1_000_000 + (*t_ms / 1000) as i64;
            if let Some(u) = vad.push_frame(frame, now) {
                out.push(u);
            }
            *t_ms += FRAME_MS;
        }
        out
    }

    #[test]
    fn silence_yields_nothing() {
        let mut vad = Vad::new(cfg());
        let mut seed = 7;
        let mut t = 0;
        let utts = feed(&mut vad, &noise(3000, 60, &mut seed), &mut t);
        assert!(utts.is_empty());
        assert!(vad.flush(1_000_003).is_none());
        // the floor adapted to the noise (RMS of noise amp 60 ≈ 0.001)
        assert!(vad.threshold() >= MIN_THRESHOLD);
    }

    #[test]
    fn detects_single_utterance_with_preroll_and_tail() {
        let mut vad = Vad::new(cfg());
        let mut seed = 42;
        let mut t = 0;
        let mut signal = noise(1000, 60, &mut seed);
        signal.extend(sine(600, 0.25));
        signal.extend(noise(1500, 60, &mut seed));
        let utts = feed(&mut vad, &signal, &mut t);
        assert_eq!(utts.len(), 1, "čekám přesně jednu promluvu");
        let u = &utts[0];
        // preroll(300) + speech(600) + kept silence tail(~150) ± frames
        let ms = u.samples.len() * 1000 / SAMPLE_RATE;
        assert!((900..=1400).contains(&ms), "délka {ms} ms mimo očekávání");
        // start ≈ epoch 1_000_000 + ~0.7 s (speech starts at t=1.0 s, preroll 0.3 s)
        assert!((1_000_000..=1_000_001).contains(&u.started_at), "start {}", u.started_at);
        assert!(u.ended_at >= u.started_at);
    }

    #[test]
    fn short_click_is_dropped() {
        let mut vad = Vad::new(cfg());
        let mut seed = 3;
        let mut t = 0;
        let mut signal = noise(1000, 60, &mut seed);
        signal.extend(sine(60, 0.4)); // 60 ms click
        signal.extend(noise(1500, 60, &mut seed));
        let utts = feed(&mut vad, &signal, &mut t);
        assert!(utts.is_empty(), "kliknutí nemá být promluva");
    }

    #[test]
    fn long_speech_splits_at_max_len() {
        let mut vad = Vad::new(cfg());
        let mut seed = 9;
        let mut t = 0;
        let mut signal = noise(1000, 60, &mut seed);
        signal.extend(sine(65_000, 0.25)); // 65 s of continuous "speech"
        let utts = feed(&mut vad, &signal, &mut t);
        assert!(utts.len() >= 2, "65 s řeči se má rozdělit, mám {}", utts.len());
        for u in &utts {
            let ms = u.samples.len() * 1000 / SAMPLE_RATE;
            assert!(ms <= 28_500, "kus {ms} ms přesahuje max okno");
        }
        // the remainder arrives via flush
        assert!(vad.flush(2_000_000).is_some());
    }

    #[test]
    fn slowly_rising_noise_never_triggers() {
        let mut vad = Vad::new(cfg());
        let mut t = 0;
        let mut all = Vec::new();
        // noise amplitude rises 60→600 over 60 s — the floor must keep up
        let mut seed = 11;
        for step in 0..60 {
            all.extend(noise(1000, 60 + step * 9, &mut seed));
        }
        let utts = feed(&mut vad, &all, &mut t);
        assert!(utts.is_empty(), "plíživý hluk nemá spouštět promluvy");
    }

    #[test]
    fn reset_drops_partial_utterance() {
        let mut vad = Vad::new(cfg());
        let mut t = 0;
        let mut seed = 5;
        feed(&mut vad, &noise(1000, 60, &mut seed), &mut t);
        feed(&mut vad, &sine(500, 0.25), &mut t); // speech running, not yet ended
        vad.reset();
        assert!(vad.flush(1_000_100).is_none(), "po resetu nesmí nic zbýt");
    }
}
