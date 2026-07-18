//! Energetický VAD (voice activity detection) s adaptivním šumovým prahem.
//!
//! Záměrně jednoduchý a plně testovatelný: RMS rámce se porovnává s násobkem
//! plovoucího šumového dna. Promluva začíná hlasitým rámcem (s pre-rollem, aby
//! se neusekl náběh prvního slova), končí po `silence_ms` ticha a příliš dlouhá
//! řeč se dělí po `max_utterance_ms` (whisper zpracovává max 30s okno).
//! Známá mez: náhlý trvalý hluk (větrák, hudba) projde do STT a odfiltruje ho
//! až no-speech práh whisperu — upgrade path na Silero VAD je v PLAN.md.

use std::collections::VecDeque;

pub const SAMPLE_RATE: usize = 16_000;
pub const FRAME_MS: u64 = 30;
pub const FRAME_SAMPLES: usize = SAMPLE_RATE * FRAME_MS as usize / 1000; // 480

/// Práh řeči = max(šumové dno × speech_mult, MIN_THRESHOLD); RMS na škále [-1, 1].
/// speech_mult je v configu (`listen.vad_speech_mult`) — na tichém mikrofonu
/// s malým SNR (naměřeno 2026-07-17: řeč jen ~14 dB nad šumem) sekal násobek 3
/// věty na fragmenty; default je proto 2.0.
const MIN_THRESHOLD: f32 = 0.0045;
/// EMA šumového dna (aktualizuje se jen na tichých rámcích; τ ≈ 0,6 s).
const FLOOR_ALPHA: f32 = 0.05;
const FLOOR_INIT: f32 = 0.003;
/// Kolik zvuku před prvním hlasitým rámcem se přibalí (náběh slova).
const PREROLL_MS: u64 = 300;
/// Překryv při nuceném dělení dlouhé řeči — ať se neusekne slovo přesně v řezu.
const SPLIT_CARRY_MS: u64 = 200;
/// Kolik koncového ticha v promluvě nechat; zbytek se ořízne (šetří STT).
const TAIL_SILENCE_MS: u64 = 150;

#[derive(Debug, Clone)]
pub struct VadConfig {
    pub min_speech_ms: u64,
    pub silence_ms: u64,
    pub max_utterance_ms: u64,
    /// Násobek šumového dna, od kterého je rámec „řeč".
    pub speech_mult: f32,
}

/// Ucelená promluva: PCM 16 kHz mono + epochy začátku/konce (sekundy).
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

    /// Práh je veřejný kvůli diagnostice (doctor --live).
    pub fn threshold(&self) -> f32 {
        (self.floor * self.cfg.speech_mult).max(MIN_THRESHOLD)
    }

    /// Zahodí rozpracovaný stav (pauza snímání).
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.preroll.clear();
    }

    /// Přijme jeden rámec (`FRAME_SAMPLES` vzorků) a případně vrátí
    /// dokončenou promluvu. `now_ts` = epocha příchodu rámce v sekundách.
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
                    // šumové dno se učí jen z ticha
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
                    // ticho uvnitř/za řečí smí dno aktualizovat taky (pomalu
                    // rostoucí hluk nesmí držet Active navěky)
                    self.floor += FLOOR_ALPHA * (rms(frame) - self.floor);
                }

                if *trailing_ms >= self.cfg.silence_ms {
                    let voiced = *voiced_ms;
                    let trailing = *trailing_ms;
                    let started = *started_at;
                    let mut samples = std::mem::take(buf);
                    self.state = State::Idle;
                    if voiced < self.cfg.min_speech_ms {
                        return None; // kliknutí, ťuknutí — zahodit
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
                    // nucený řez dlouhé řeči; kousek konce si přeneseme do
                    // další promluvy, ať řez nepůlí slovo tak tvrdě
                    let carry_len = ms_to_samples(SPLIT_CARRY_MS).min(buf.len());
                    let carry: Vec<i16> = buf[buf.len() - carry_len..].to_vec();
                    let samples = std::mem::take(buf);
                    let started = *started_at;
                    let voiced = *voiced_ms;
                    self.state = State::Active {
                        buf: carry,
                        voiced_ms: FRAME_MS, // pokračující řeč se prokáže dalšími rámci
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

    /// Uzavře rozpracovanou promluvu (konec WAV souboru; démon flush nevolá).
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

    /// Deterministický slabý šum (LCG) — realistické „ticho" mikrofonu.
    fn noise(ms: u64, amp: i16, seed: &mut u64) -> Vec<i16> {
        (0..ms_to_samples(ms))
            .map(|_| {
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((*seed >> 33) as i64 % (2 * amp as i64 + 1) - amp as i64) as i16
            })
            .collect()
    }

    /// Prožene signál VAD po rámcích s tikající epochou; vrací promluvy.
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
        // dno se přizpůsobilo šumu (RMS šumu amp 60 ≈ 0.001)
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
        // preroll(300) + řeč(600) + ponechaný ocas ticha(~150) ± rámce
        let ms = u.samples.len() * 1000 / SAMPLE_RATE;
        assert!((900..=1400).contains(&ms), "délka {ms} ms mimo očekávání");
        // začátek ≈ epocha 1_000_000 + ~0.7 s (řeč začíná v t=1.0 s, preroll 0.3 s)
        assert!((1_000_000..=1_000_001).contains(&u.started_at), "start {}", u.started_at);
        assert!(u.ended_at >= u.started_at);
    }

    #[test]
    fn short_click_is_dropped() {
        let mut vad = Vad::new(cfg());
        let mut seed = 3;
        let mut t = 0;
        let mut signal = noise(1000, 60, &mut seed);
        signal.extend(sine(60, 0.4)); // kliknutí 60 ms
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
        signal.extend(sine(65_000, 0.25)); // 65 s nepřetržité „řeči"
        let utts = feed(&mut vad, &signal, &mut t);
        assert!(utts.len() >= 2, "65 s řeči se má rozdělit, mám {}", utts.len());
        for u in &utts {
            let ms = u.samples.len() * 1000 / SAMPLE_RATE;
            assert!(ms <= 28_500, "kus {ms} ms přesahuje max okno");
        }
        // zbytek doteče při flush
        assert!(vad.flush(2_000_000).is_some());
    }

    #[test]
    fn slowly_rising_noise_never_triggers() {
        let mut vad = Vad::new(cfg());
        let mut t = 0;
        let mut all = Vec::new();
        // amplituda šumu roste 60→600 během 60 s — dno musí stíhat
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
        feed(&mut vad, &sine(500, 0.25), &mut t); // řeč běží, neukončená
        vad.reset();
        assert!(vad.flush(1_000_100).is_none(), "po resetu nesmí nic zbýt");
    }
}
