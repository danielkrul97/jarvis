//! Hlasový dialog: promluva s oslovením („Jarvisi, …") → Claude s kontextem
//! z DB → odpověď nahlas přes speak (s piper zálohou).
//!
//! Worker běží ve vlákně listen démona a čte frontu promluv, které prošly
//! wake-word filtrem — STT smyčku nikdy neblokuje. Vlastní řeč Jarvise se
//! nefiltruje jen echo-cancelem: promluvy překrývající okno vlastní řeči se
//! zahazují (guard proti slyšení sebe sama). Náklady jdou do `costs`
//! (component `converse`) a sdílejí denní strop `analysis.daily_budget_usd`.

use crate::config::{Config, ConverseCfg, Paths};
use crate::pipeline::claude;
use crate::speak;
use crate::store::db;
use crate::util;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::sync::atomic::{AtomicI64, Ordering};
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Hlasová odpověď, když je denní rozpočet vyčerpaný (nevolá Claude).
pub const BUDGET_REPLY: &str = "Omlouvám se, pane, denní rozpočet na umělou inteligenci \
                                je vyčerpán. Pokračovat mohu zítra.";
/// Hlasová odpověď na neočekávanou chybu (detaily jdou do logu).
pub const ERROR_REPLY: &str = "Omlouvám se, pane, něco se pokazilo. Detaily jsou v logu.";

/// Promluva, která prošla wake-word filtrem a čeká na odpověď.
pub struct Job {
    pub text: String,
    pub started_at: i64,
    /// Jak promluva prošla triage (wake / follow-up / kandidát na klasifikátor).
    pub trigger: Trigger,
}

/// Wake-word matcher odolný proti chybám přepisu. Whisper reálně komolí
/// („Jarvisi" → „Javi si"), proto se text normalizuje (malá písmena, bez
/// diakritiky, bez mezer a interpunkce) a kmen se hledá i s tolerancí
/// 1 editační chyby (zapnutelné `wake_fuzzy`).
pub struct WakeWords {
    stems: Vec<Vec<char>>,
    fuzzy: bool,
    /// Normalizovaný whisper hint: promluva, která s ním sdílí dlouhý
    /// souvislý úsek, je skoro jistě halucinace nápovědy, ne oslovení.
    hint: Vec<char>,
}

/// Práh echo-guardu: delší společný úsek než nejdelší reálné jméno
/// („jarvisi" = 7) — skutečné oslovení se s hintem takhle dlouho nepotká.
const HINT_ECHO_MIN: usize = 10;

/// Normalizace pro matching: malá písmena, česká diakritika složená na
/// ASCII, vše mimo [a-z0-9] zahozeno (i mezery — „Jar visi" drží pohromadě).
fn normalize(text: &str) -> Vec<char> {
    text.chars()
        .flat_map(|c| c.to_lowercase())
        .filter_map(|c| match c {
            'á' => Some('a'),
            'č' => Some('c'),
            'ď' => Some('d'),
            'é' | 'ě' => Some('e'),
            'í' => Some('i'),
            'ň' => Some('n'),
            'ó' => Some('o'),
            'ř' => Some('r'),
            'š' => Some('s'),
            'ť' => Some('t'),
            'ú' | 'ů' => Some('u'),
            'ý' => Some('y'),
            'ž' => Some('z'),
            c if c.is_ascii_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

/// Levenshteinova vzdálenost (kmeny ≤ 30 znaků, okna ~stejně — DP stačí).
fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Délka nejdelšího společného souvislého úseku (substring, ne subsekvence).
fn longest_common_run(a: &[char], b: &[char]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut best = 0;
    let mut prev = vec![0usize; b.len() + 1];
    for ca in a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            if ca == cb {
                cur[j + 1] = prev[j] + 1;
                best = best.max(cur[j + 1]);
            }
        }
        prev = cur;
    }
    best
}

impl WakeWords {
    pub fn new(stems: &[String], fuzzy: bool, hint: &str) -> Result<Self> {
        let stems: Vec<Vec<char>> = stems.iter().map(|s| normalize(s)).collect();
        if stems.iter().any(|s| s.len() < 3) {
            anyhow::bail!("wake word po normalizaci kratší než 3 znaky");
        }
        Ok(Self { stems, fuzzy, hint: normalize(hint) })
    }

    /// Vypadá promluva jako halucinovaný whisper hint? (obsahuje jméno, ale je
    /// to jen opsaná nápověda, ne oslovení). Sdílený guard: `matches` i open-ear
    /// brána ho musí použít, jinak by hint budil dialog bez wake-wordu.
    pub fn hint_echo(&self, text: &str) -> bool {
        !self.hint.is_empty() && longest_common_run(&normalize(text), &self.hint) >= HINT_ECHO_MIN
    }

    pub fn matches(&self, text: &str) -> bool {
        // echo-guard: whisper na šumu občas opíše samotný hint — a ten
        // obsahuje jméno, takže by falešně budil konverzaci
        if self.hint_echo(text) {
            return false;
        }
        let t = normalize(text);
        self.stems.iter().any(|stem| {
            let n = stem.len();
            // přesná podsekvence (levné, chytá i „JARVISI." a „Jar visi")
            if t.windows(n).any(|w| w == stem.as_slice()) {
                return true;
            }
            if !self.fuzzy {
                return false;
            }
            // okna délky n-1, n, n+1 s tolerancí 1 edit chyby („javisi")
            for len in [n - 1, n, n + 1] {
                if len == 0 || len > t.len() {
                    continue;
                }
                if t.windows(len).any(|w| levenshtein(stem, w) <= 1) {
                    return true;
                }
            }
            false
        })
    }
}

/// Režim odpovídání bez wake-wordu (viz `converse.open_ear`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenEarMode {
    /// Jen na oslovení jménem (dnešní chování).
    Off,
    /// Krátké okno po Jarvisově odpovědi, kdy jméno není potřeba (Tier 1).
    Followup,
    /// Každou věrohodnou promluvu posoudí klasifikátor ve workeru (Tier 2).
    Always,
}

/// Parametry open-ear brány, odvozené z configu (čisté, ať jde `triage` testovat).
#[derive(Debug, Clone, Copy)]
pub struct OpenEar {
    pub mode: OpenEarMode,
    /// Délka follow-up okna v sekundách.
    pub window_s: i64,
    /// Minimální počet slov, aby promluva bez jména vůbec kandidovala.
    pub min_words: usize,
}

impl OpenEar {
    pub fn from_cfg(c: &ConverseCfg) -> Self {
        let mode = match c.open_ear.as_str() {
            "followup" => OpenEarMode::Followup,
            "always" => OpenEarMode::Always,
            _ => OpenEarMode::Off,
        };
        Self { mode, window_s: c.followup_window_s as i64, min_words: c.open_ear_min_words }
    }
}

/// Jak promluva míří na Jarvise — nese ji `Job` z STT vlákna do workeru.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Oslovení jménem — worker vždy odpoví.
    Wake,
    /// Bez jména, ale ve follow-up okně po odpovědi — worker odpoví (Tier 1).
    Followup,
    /// Bez jména, režim „always" — o odpovědi rozhodne až klasifikátor (Tier 2).
    Candidate,
}

/// Rozhodne, zda a jak promluva míří na Jarvise; `None` = zahodit hned v STT
/// vlákně (nic se neposílá workerovi). `now` = začátek promluvy (epocha s),
/// `speech_end` = kdy Jarvis naposledy domluvil (0 = ještě nikdy).
///
/// Wake-word funguje vždy a nezávisle na `open_ear`. Bez jména se promluva
/// posuzuje jen v režimu Followup/Always: musí projít hint-echo guardem
/// i minimem slov, pak rozhoduje follow-up okno. Promluva překrývající vlastní
/// řeč (echo) se v „always" zahazuje, ať se neplatí klasifikátor na ozvěnu.
pub fn triage(
    wake: &WakeWords,
    ear: &OpenEar,
    text: &str,
    now: i64,
    speech_end: i64,
) -> Option<Trigger> {
    if wake.matches(text) {
        return Some(Trigger::Wake);
    }
    if ear.mode == OpenEarMode::Off {
        return None;
    }
    // halucinovaný hint nebudí ani open-ear (stejný guard jako `matches`)
    if wake.hint_echo(text) {
        return None;
    }
    // „ehm", „jo" — moc krátké, ať nezaplavují workera ani klasifikátor
    if text.split_whitespace().count() < ear.min_words {
        return None;
    }
    let in_window = speech_end > 0 && (1..=ear.window_s).contains(&(now - speech_end));
    // promluva nepřekrývá vlastní řeč (jinak = echo)
    let not_echo = speech_end == 0 || now - speech_end > 0;
    match ear.mode {
        OpenEarMode::Off => None, // ošetřeno výše
        OpenEarMode::Followup => in_window.then_some(Trigger::Followup),
        OpenEarMode::Always if in_window => Some(Trigger::Followup),
        OpenEarMode::Always if not_echo => Some(Trigger::Candidate),
        OpenEarMode::Always => None,
    }
}

/// Skládá tokenové delty odpovědi do vět pro streamovanou syntézu. Emituje
/// ucelený úsek, jakmile narazí na koncovku věty (. ! ? …) NÁSLEDOVANOU mezerou
/// (aby decimál/zkratka uvnitř věty neřezala), nebo na nový řádek. Pojistka:
/// nad `CHUNK_MAX_HOLD` znaků bez koncovky ustřihne na poslední mezeře, ať se
/// řeč nerozjede pozdě u dlouhého souvětí.
struct SpeechChunker {
    buf: String,
}

const CHUNK_MAX_HOLD: usize = 240;

impl SpeechChunker {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Přidá kus textu a vrátí věty, které se tím uzavřely (často žádné).
    fn push(&mut self, delta: &str) -> Vec<String> {
        self.buf.push_str(delta);
        let mut out = Vec::new();
        while let Some(s) = self.take_ready() {
            if !s.is_empty() {
                out.push(s);
            }
        }
        out
    }

    /// Zbytek po skončení streamu (poslední věta bez koncové interpunkce).
    fn flush(&mut self) -> Option<String> {
        let s = self.buf.trim().to_string();
        self.buf.clear();
        (!s.is_empty()).then_some(s)
    }

    /// Odebere z bufferu nejbližší ucelenou větu, nebo None (počkat na víc dat).
    fn take_ready(&mut self) -> Option<String> {
        let mut cut: Option<usize> = None; // byte index konce úseku (za koncovkou)
        let mut it = self.buf.char_indices().peekable();
        while let Some((idx, c)) = it.next() {
            let end = idx + c.len_utf8();
            if c == '\n' {
                cut = Some(end);
                break;
            }
            if matches!(c, '.' | '!' | '?' | '…') {
                // koncovka věty jen když ji následuje mezera; konec bufferu =
                // počkej (může to být decimál/zkratka nebo věta ještě pokračuje)
                if matches!(it.peek(), Some(&(_, next)) if next.is_whitespace()) {
                    cut = Some(end);
                    break;
                }
            }
        }
        // pojistka proti pozdnímu startu: dlouhý běh bez koncovky ustřihni na mezeře
        if cut.is_none() && self.buf.chars().count() > CHUNK_MAX_HOLD {
            cut = self.buf.rfind(char::is_whitespace).map(|p| p + 1);
        }
        let b = cut?;
        let sentence = self.buf[..b].trim().to_string();
        self.buf = self.buf[b..].trim_start().to_string();
        Some(sentence)
    }
}

/// Přehrávací fronta pro streamovanou řeč: vlastní vlákno syntetizuje a přehrává
/// věty popořadě, hlavní vlákno je jen posílá (neblokuje) — první věta tak mluví,
/// zatímco model generuje další. Ack jde frontou jako první (překryv s Claudem).
struct SpeechPlayer {
    tx: Option<mpsc::Sender<(String, bool)>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SpeechPlayer {
    fn start(paths: &Paths, cfg: &Config) -> Self {
        let (tx, rx) = mpsc::channel::<(String, bool)>();
        let paths = paths.clone();
        let cfg = cfg.clone();
        let handle = std::thread::spawn(move || {
            // jedno DB spojení pro celou řeč — evidence TTS spotřeby se pak
            // nezakládá znovu pro každou větu (viz speak::say_once_on)
            let cost_conn = db::open(&paths.db_path).ok();
            for (text, cached) in rx {
                // cached = fixní fráze (ack) drží se v cache; věty odpovědi
                // jednorázově (say_once maže po přehrání)
                let r = if cached {
                    speak::say(&paths, &cfg, &text, None, true, false)
                } else if let Some(c) = &cost_conn {
                    speak::say_once_on(&paths, &cfg, c, &text)
                } else {
                    speak::say_once(&paths, &cfg, &text)
                };
                if let Err(e) = r {
                    warn!("streaming TTS: věta se nepřehrála: {e:#}");
                }
            }
        });
        Self { tx: Some(tx), handle: Some(handle) }
    }

    /// Zařadí větu do fronty (neblokuje). `cached` = fixní fráze (ack).
    fn say(&self, text: String, cached: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((text, cached));
        }
    }

    /// Zavře frontu a počká, až doznějí všechny zařazené věty.
    fn finish(mut self) {
        self.tx.take(); // drop senderu → přehrávací smyčka po vyprázdnění skončí
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Worker: čte frontu a odpovídá. Končí, až když druhá strana kanálu zmizí.
pub fn worker_loop(paths: &Paths, cfg: &Config, rx: mpsc::Receiver<Job>, speech_end: Arc<AtomicI64>) {
    let conn = match db::open(&paths.db_path) {
        Ok(c) => c,
        Err(e) => {
            warn!("konverzace: nelze otevřít DB — worker končí: {e:#}");
            return;
        }
    };
    // rezidentní mozek: první otázka pak jede bez CLI startu
    let mut warm: Option<claude::Warm> = None;
    if cfg.converse.warm {
        let (tools, turns) = agent_caps(cfg);
        match claude::Warm::spawn(&cfg.converse.model, &paths.data_dir, &tools, turns) {
            Ok(w) => {
                info!("konverzační mozek předehřát ({})", cfg.converse.model);
                warm = Some(w);
            }
            Err(e) => warn!("předehřátí mozku selhalo — pojede cold: {e:#}"),
        }
    }
    // `speech_end` (konec posledního mluvení Jarvise, unix ts) je sdílený s STT
    // vláknem: slouží zároveň jako echo-guard i jako práh follow-up okna.
    while let Ok(job) = rx.recv() {
        if job.started_at <= speech_end.load(Ordering::Relaxed) + 1 {
            debug!("konverzace: promluva překrývá vlastní řeč — ignoruji: {}", job.text);
            continue;
        }
        match job.trigger {
            Trigger::Wake | Trigger::Followup => {
                if let Err(e) = respond(paths, cfg, &conn, &job.text, &mut warm, &speech_end) {
                    warn!("konverzace selhala: {e:#}");
                    speak_tracked(paths, cfg, ERROR_REPLY, true, &speech_end);
                }
            }
            Trigger::Candidate => {
                // Tier 2: rozhodne skeptický klasifikátor. Při vyčerpaném
                // rozpočtu mlčíme — kandidáta neoslovujeme hláškou o rozpočtu,
                // nebyli jsme osloveni.
                if cfg.converse.respect_budget && over_budget(cfg, &conn).unwrap_or(false) {
                    debug!("konverzace: open-ear kandidát a vyčerpaný rozpočet — mlčím: {}", job.text);
                } else if is_device_directed(paths, cfg, &conn, &job.text) {
                    if let Err(e) = respond(paths, cfg, &conn, &job.text, &mut warm, &speech_end) {
                        warn!("konverzace selhala: {e:#}");
                        speak_tracked(paths, cfg, ERROR_REPLY, true, &speech_end);
                    }
                } else {
                    debug!("konverzace: open-ear — promluva nemířila na mě, mlčím: {}", job.text);
                }
            }
        }
    }
}

/// Náhodně vybere ack z listu. `seed` je zdroj náhody z volajícího (viz
/// `random_seed`) — oddělený, ať funkce zůstane čistá a testovatelná: seed
/// určuje index (`seed % počet`). Prázdné položky i prázdný list = None.
fn pick_ack(acks: &[String], seed: u64) -> Option<&str> {
    let nonempty: Vec<&str> = acks.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if nonempty.is_empty() {
        return None;
    }
    Some(nonempty[(seed % nonempty.len() as u64) as usize])
}

/// Náhodný seed pro výběr acku bez extra závislosti (žádný `rand`): nanosekundy
/// od epochy proprané hasherem, ať i časy pár ms od sebe dají rozházené hodnoty.
fn random_seed() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos())
        .hash(&mut h);
    h.finish()
}

/// Jedna výměna vč. hlasu: rozpočet → ack → Claude → odpověď nahlas.
fn respond(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    question: &str,
    warm: &mut Option<claude::Warm>,
    last_speech_end: &AtomicI64,
) -> Result<()> {
    info!("konverzace: „{question}“");
    if cfg.converse.respect_budget && over_budget(cfg, conn)? {
        info!("konverzace: denní rozpočet vyčerpán — odpovídám bez Clauda");
        speak_tracked(paths, cfg, BUDGET_REPLY, true, last_speech_end);
        return Ok(());
    }
    let ack = pick_ack(&cfg.converse.ack, random_seed()).unwrap_or("").to_string();
    // Streamovaná cesta (potřebuje warm proces): ack hraje hned a maskuje
    // „myšlení" modelu, věty se syntetizují průběžně. Selhání warm → cold cesta.
    if cfg.converse.warm && warm.is_some() {
        let w = warm.as_mut().expect("warm ověřen podmínkou");
        match respond_streaming(paths, cfg, conn, question, w, &ack, last_speech_end) {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!("streaming odpověď selhala ({e:#}) — cold fallback");
                *warm = None; // mrtvý warm zahodit, fallback rozjede čerstvý
            }
        }
    }
    // Blokující cesta: cold spawn (warm vypnutý/None) nebo fallback po selhání.
    if !ack.is_empty() {
        // fixní fráze s cache: od druhého použití zazní okamžitě a zdarma
        speak_tracked(paths, cfg, &ack, true, last_speech_end);
    }
    let answer = exchange(paths, cfg, conn, question, warm)?;
    speak_tracked(paths, cfg, &answer, false, last_speech_end);
    Ok(())
}

/// Streamovaná odpověď: ack se přehraje hned (maskuje „myšlení" a generování),
/// věty odpovědi se syntetizují a přehrávají průběžně, jak je model vydává —
/// čas do prvního slova padá z „celý Sonnet + celé TTS" na „první věta". Vyžaduje
/// warm proces. `last_speech_end` se posune až po doznění poslední věty (echo-guard
/// i follow-up okno počítají od konce Jarvisovy řeči).
fn respond_streaming(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    question: &str,
    warm: &mut claude::Warm,
    ack: &str,
    last_speech_end: &AtomicI64,
) -> Result<()> {
    let player = SpeechPlayer::start(paths, cfg);
    if !ack.is_empty() {
        player.say(ack.to_string(), true); // hraje, zatímco Claude přemýšlí/generuje
    }
    let res = exchange_streaming(cfg, conn, question, warm, |s| player.say(s.to_string(), false));
    player.finish(); // počkej, až doznějí všechny zařazené věty (i při chybě)
    last_speech_end.store(util::now_ts(), Ordering::Relaxed);
    res.map(|_| ())
}

/// CLI cesta `jarvis converse`: streamovaná výměna přes warm proces (stejně jako
/// hlasový démon). `mute` = věty jen tiskni s časovou značkou (test streamingu
/// a endpointingu bez zvuku), jinak je průběžně říkej. Bez warm procesu spadne
/// na jednorázový `exchange`.
pub fn converse_cli(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    question: &str,
    mute: bool,
) -> Result<String> {
    let (tools, turns) = agent_caps(cfg);
    let mut warm = claude::Warm::spawn(&cfg.converse.model, &paths.data_dir, &tools, turns).ok();
    if let Some(w) = warm.as_mut() {
        let t0 = std::time::Instant::now();
        let player = (!mute && cfg.speak.enabled).then(|| SpeechPlayer::start(paths, cfg));
        let answer = exchange_streaming(cfg, conn, question, w, |s| match &player {
            Some(p) => p.say(s.to_string(), false),
            None => println!("[{:>4.1}s] {s}", t0.elapsed().as_secs_f32()),
        });
        if let Some(p) = player {
            p.finish();
        }
        match answer {
            Ok(a) => return Ok(a),
            Err(e) => warn!("streaming converse selhal — cold fallback: {e:#}"),
        }
    }
    exchange(paths, cfg, conn, question, &mut None)
}

/// Překročený denní strop? (sdílený s analýzou — součet za celý den)
pub fn over_budget(cfg: &Config, conn: &Connection) -> Result<bool> {
    let (day_start, _) = util::day_bounds_local(util::today_local())?;
    Ok(db::cost_since(conn, day_start)? >= cfg.analysis.daily_budget_usd)
}

/// Jádro výměny bez hlasu: prompt s kontextem → Claude → log do DB.
/// Vrací odpověď připravenou pro řeč. `warm` = rezidentní proces (worker);
/// `&mut None` = vždy cold spawn (CLI `jarvis converse`).
pub fn exchange(
    paths: &Paths,
    cfg: &Config,
    conn: &Connection,
    question: &str,
    warm: &mut Option<claude::Warm>,
) -> Result<String> {
    let prompt = build_prompt(cfg, conn, question)?;
    let outcome = ask_claude(paths, cfg, &prompt, warm)?;
    Ok(record_exchange(cfg, conn, question, &outcome))
}

/// Streamovaná výměna: text odpovědi jde po VĚTÁCH do `on_sentence`, jak ho model
/// generuje (pro průběžnou syntézu). Vyžaduje warm proces (delty). Vrací celou
/// odpověď (jako `exchange`); náklad i text se evidují po přijetí `result`.
pub fn exchange_streaming(
    cfg: &Config,
    conn: &Connection,
    question: &str,
    warm: &mut claude::Warm,
    mut on_sentence: impl FnMut(&str),
) -> Result<String> {
    let prompt = build_prompt(cfg, conn, question)?;
    let mut chunker = SpeechChunker::new();
    let outcome = warm.ask_streaming(&prompt, Duration::from_secs(cfg.converse.timeout_s), |delta| {
        for s in chunker.push(delta) {
            on_sentence(&s);
        }
    })?;
    if let Some(rest) = chunker.flush() {
        on_sentence(&rest);
    }
    Ok(record_exchange(cfg, conn, question, &outcome))
}

/// Zaeviduje výměnu (náklad + text do DB, log) a vrátí odpověď připravenou pro
/// řeč. Sdílí blokující `exchange` i streamovaný běh.
fn record_exchange(
    cfg: &Config,
    conn: &Connection,
    question: &str,
    outcome: &claude::ClaudeOutcome,
) -> String {
    let c = &cfg.converse;
    let answer = normalize_for_speech(&outcome.text, cfg.speak.max_chars);
    let now = util::now_ts();
    if let Err(e) = db::insert_cost(
        conn, now, "converse", &c.model, outcome.tokens_in, outcome.tokens_out, outcome.cost_usd,
    )
    .and_then(|()| db::insert_conversation(conn, now, question, &answer, &c.model, outcome.cost_usd))
    {
        warn!("konverzace: zápis do DB selhal: {e:#}");
    }
    info!("konverzace: odpověď ({:.4} USD): {}", outcome.cost_usd, answer);
    answer
}

/// Warm cesta s úklidem vyčpělých procesů; každá chyba = zahodit proces
/// a spadnout na jednorázový spawn — hlas nesmí zůstat němý kvůli mozku.
fn ask_claude(
    paths: &Paths,
    cfg: &Config,
    prompt: &str,
    warm: &mut Option<claude::Warm>,
) -> Result<claude::ClaudeOutcome> {
    let c = &cfg.converse;
    let (tools, turns) = agent_caps(cfg);
    if c.warm {
        if warm.as_ref().is_some_and(|w| w.stale(c.warm_max_exchanges, c.warm_idle_s)) {
            debug!("konverzace: warm proces vyčpěl — recykluji");
            *warm = None;
        }
        if warm.is_none() {
            match claude::Warm::spawn(&c.model, &paths.data_dir, &tools, turns) {
                Ok(w) => *warm = Some(w),
                Err(e) => warn!("warm spawn selhal — cold fallback: {e:#}"),
            }
        }
        if let Some(w) = warm.as_mut() {
            match w.ask(prompt, Duration::from_secs(c.timeout_s)) {
                Ok(o) => return Ok(o),
                Err(e) => {
                    warn!("warm mozek selhal — cold fallback: {e:#}");
                    *warm = None;
                }
            }
        }
    }
    claude::run(&claude::ClaudeRequest {
        prompt: prompt.to_string(),
        model: Some(&c.model),
        cwd: &paths.data_dir,
        allowed_tools: &tools,
        max_turns: turns,
        timeout: Duration::from_secs(c.timeout_s),
    })
}

/// Nástroje konverzačního agenta: s povoleným [wm] dostane Bash omezený na
/// `jarvis wm …` (okna), s [sms] na `jarvis sms …`, s [runbooks] na čtení
/// a SPOUŠTĚNÍ schválených runbooků (schvalování hlasem neexistuje — proto
/// se `jarvis runbook approve` do allowlistu nikdy nedává). S jakýmkoli
/// nástrojem navíc i víc kol na akci + ověření; jinak Read a jedno kolo.
fn agent_caps(cfg: &Config) -> (String, u32) {
    let mut tools = vec!["Read"];
    if cfg.converse.web {
        tools.push("WebSearch");
        tools.push("WebFetch");
    }
    if cfg.wm.enabled {
        tools.push("Bash(jarvis wm:*)");
    }
    if cfg.sms.enabled {
        tools.push("Bash(jarvis sms:*)");
    }
    if cfg.runbooks.enabled && cfg.runbooks.voice_run {
        tools.push("Bash(jarvis runbook list)");
        tools.push("Bash(jarvis runbook pending)");
        tools.push("Bash(jarvis runbook show:*)");
        tools.push("Bash(jarvis runbook runs:*)");
        tools.push("Bash(jarvis runbook run:*)");
    }
    if tools.len() == 1 {
        ("Read".into(), 1)
    } else {
        (tools.join(","), cfg.converse.max_turns)
    }
}

/// Promluví a posune echo-guard okno. `cached` = fixní fráze (drží se
/// v cache), jinak jednorázová odpověď (soubor se po přehrání maže).
fn speak_tracked(paths: &Paths, cfg: &Config, text: &str, cached: bool, last_speech_end: &AtomicI64) {
    let res = if cached {
        speak::say(paths, cfg, text, None, true, false)
    } else {
        speak::say_once(paths, cfg, text)
    };
    if let Err(e) = res {
        warn!("konverzace: hlas selhal: {e:#}");
    }
    last_speech_end.store(util::now_ts(), Ordering::Relaxed);
}

/// Jednořádkový popis aktivního okna („třída — titulek"), jen když je čerstvé
/// (≤ 2 min; jinak uživatel nejspíš není u počítače). Sdílí ho prompt
/// konverzace i open-ear klasifikátor. DB chyba = None (okno je jen dekorace).
fn active_window_line(conn: &Connection, now: i64) -> Option<String> {
    let active: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT ts, wm_class, title FROM samples ORDER BY ts DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .ok()
        .flatten();
    active.and_then(|(ts, class, title)| (now - ts <= 120).then(|| format!("{class} — {title}")))
}

/// Prompt skeptického open-ear klasifikátoru: jediné rozhodnutí ANO/NE, jestli
/// promluva mířila na Jarvise. Bias do NE je v instrukci i v parsování verdiktu.
fn build_gate_prompt(text: &str, active_window: Option<&str>) -> String {
    let screen = active_window.map(|w| format!("Na obrazovce je teď: {w}\n")).unwrap_or_default();
    format!(
        "Rozhoduješ jedinou věc: mířila tahle promluva na hlasového asistenta Jarvise, \
         nebo ne? Přepis je z mikrofonu v místnosti, kde se běžně mluví i s jinými lidmi \
         a je slyšet pozadí (televize, telefon). Odpověz VÝHRADNĚ jedním slovem: ANO nebo NE.\n\n\
         Řekni ANO jen když je to jasně dotaz nebo povel pro asistenta (např. „kolik je hodin“, \
         „zhasni monitor“, „napiš Tomášovi“, „jaké bude počasí“).\n\
         Řekni NE, když:\n\
         - mluvíš k jinému člověku (oslovení jiným jménem, „půjdeme“, „řekni mu“, „podej mi“),\n\
         - je to útržek konverzace, čtení nahlas, myšlení nahlas nebo zvuk z pozadí,\n\
         - nedává to jako povel ani dotaz smysl,\n\
         - si nejsi jistý.\n\
         Když váháš, řekni NE.\n\n\
         {screen}\
         Promluva: „{text}“\n\
         Odpověz jedním slovem (ANO/NE):"
    )
}

/// Verdikt klasifikátoru: true jen na jasné „ANO"; cokoli jiného (NE, prázdné,
/// žvást) = false. Bias do ticha — radši nemluvit než skočit do cizí řeči.
fn parse_gate_verdict(reply: &str) -> bool {
    normalize(reply).starts_with(&['a', 'n', 'o'])
}

/// Zeptá se klasifikátoru, jestli promluva bez wake-wordu mířila na Jarvise
/// (open-ear „always"). Volá se jen na kandidáty po levných lokálních filtrech,
/// ne na každou promluvu. Náklad → `costs` (component „converse-gate").
/// Jakákoli chyba = false (radši mlčet).
/// Jedna klasifikace open-ear (bez DB): sestaví prompt, zavolá model a vrátí
/// (mířilo na Jarvise?, outcome kvůli evidenci nákladu). `active_window` předá
/// volající. Chyba se propaguje — worker ji překlopí na „mlčet", eval ji vyhodí.
fn classify_directed(
    paths: &Paths,
    cfg: &Config,
    text: &str,
    active_window: Option<&str>,
) -> Result<(bool, claude::ClaudeOutcome)> {
    let outcome = claude::run(&claude::ClaudeRequest {
        prompt: build_gate_prompt(text, active_window),
        model: Some(&cfg.converse.model),
        cwd: &paths.data_dir,
        allowed_tools: "Read",
        max_turns: 1,
        timeout: Duration::from_secs(cfg.converse.timeout_s),
    })?;
    Ok((parse_gate_verdict(&outcome.text), outcome))
}

/// Worker cesta: klasifikuje kandidáta a zaeviduje náklad. Chyba = false
/// (mlčet). Volá se jen po levných lokálních filtrech, ne na každou promluvu.
fn is_device_directed(paths: &Paths, cfg: &Config, conn: &Connection, text: &str) -> bool {
    let now = util::now_ts();
    let aw = active_window_line(conn, now);
    match classify_directed(paths, cfg, text, aw.as_deref()) {
        Ok((directed, outcome)) => {
            if let Err(e) = db::insert_cost(
                conn,
                now,
                "converse-gate",
                &cfg.converse.model,
                outcome.tokens_in,
                outcome.tokens_out,
                outcome.cost_usd,
            ) {
                warn!("open-ear: zápis nákladu selhal: {e:#}");
            }
            debug!(
                "open-ear klasifikátor: „{text}“ → {} ({:.4} USD)",
                if directed { "ANO" } else { "NE" },
                outcome.cost_usd
            );
            directed
        }
        Err(e) => {
            warn!("open-ear klasifikátor selhal — mlčím: {e:#}");
            false
        }
    }
}

fn build_prompt(cfg: &Config, conn: &Connection, question: &str) -> Result<String> {
    let now = util::now_ts();
    let mut ctx = format!("Čas: {}\n", util::fmt_local(now));
    if let Some(w) = active_window_line(conn, now) {
        ctx.push_str(&format!("Aktivní okno na obrazovce: {w}\n"));
    }
    let recent = db::recent_conversations(conn, cfg.converse.max_context_exchanges)?;
    if !recent.is_empty() {
        ctx.push_str("Předchozí výměny (nejstarší první):\n");
        for (q, a) in recent {
            ctx.push_str(&format!("  Pán: {q}\n  Jarvis: {a}\n"));
        }
    }
    let mut tools_help = String::new();
    if cfg.converse.web {
        tools_help.push_str(WEB_TOOLS_PROMPT);
    }
    if cfg.wm.enabled {
        tools_help.push_str(WM_TOOLS_PROMPT);
    }
    if cfg.sms.enabled {
        tools_help.push_str(SMS_TOOLS_PROMPT);
    }
    if cfg.runbooks.enabled && cfg.runbooks.voice_run {
        tools_help.push_str(RUNBOOK_TOOLS_PROMPT);
    }
    Ok(format!(
        "Jsi Jarvis, můj osobní hlasový asistent. Mluvíš VÝHRADNĚ česky, vykáš mi \
         a oslovuješ mě „pane“. Jsi věcný a pohotový, s decentním suchým humorem.\n\
         Tvoje odpověď se PŘEČTE NAHLAS syntézou řeči: žádné odrážky, žádný markdown, \
         žádná emoji; čísla, jednotky a zkratky piš tak, jak se vyslovují. Odpovídej \
         stručně — jedna až tři věty, pokud výslovně nežádám víc.\n\
         Přepis mé řeči dělá stroj a občas ji zkomolí. Když je přepis zjevně \
         nesmyslný nebo nejde poznat, na co se ptám, NEODPOVÍDEJ naslepo a nevykládej \
         obecné fráze — krátce řekni, že jsi nerozuměl, a popros o zopakování. \
         Zdvojené věty ber jako jednu. Kontext obrazovky používej, JEN když s otázkou \
         souvisí — nekomentuj ho sám od sebe.\n{tools_help}\n\
         Kontext:\n{ctx}\n\
         Právě jsem řekl (automatický přepis z mikrofonu): „{question}“\n\n\
         Odpověz pouze textem odpovědi, nic jiného."
    ))
}

/// Návod na web — přikládá se, jen když je converse.web = true (agent má
/// povolené WebSearch/WebFetch).
const WEB_TOOLS_PROMPT: &str = "\
Na AKTUÁLNÍ informace (počasí, zprávy, kurzy, výsledky, cokoli po datu tvých \
znalostí) použij nástroj WebSearch, případně WebFetch na konkrétní stránku — \
nehádej a nevymýšlej si čísla. Hledej stručně, výsledek shrň jednou až dvěma \
větami tak, jak se řekne nahlas (žádné odkazy, URL ani citace). Když web nic \
užitečného nevrátí, přiznej to a neodpovídej naslepo.\n";

/// Návod na ovládání oken pro agenta — přikládá se, jen když je [wm] enabled
/// (a agent má povolený Bash omezený na `jarvis wm`).
const WM_TOOLS_PROMPT: &str = "\
Umíš ovládat počítač: nástroj Bash máš povolený VÝHRADNĚ pro příkazy `jarvis wm …`:\n\
  jarvis wm list | active | focus <okno> | close <okno> | minimize <okno> |\n\
  maximize [--off] <okno> | fullscreen [--off] <okno> | move <okno> X Y |\n\
  resize <okno> ŠÍŘKA VÝŠKA | wait [--timeout-s N] <okno> |\n\
  spawn [--window <okno>] <program> [argumenty…] — spustí aplikaci |\n\
  type [--window <okno>] [--enter] \"text\" | key <zkratka…> (ctrl+f, Return, alt+F4) |\n\
  click X Y [--button 3] [--double] | pointer X Y | screenshot [--window <okno>]\n\
<okno> = část třídy/titulku okna, nebo 0xID z listu; focus vypíše read-back toho, \
co je teď aktivní. screenshot vypíše cestu k JPG — prohlédni si ho nástrojem Read, \
kdykoli si nejsi jistý stavem obrazovky nebo kam kliknout.\n\
Aplikaci, která neběží, spusť přes spawn (smí jen programy povolené v konfiguraci; \
když spawn program odmítne, řekni pánovi, že si ho musí přidat do wm.spawn_allowed \
— neobcházej to). Když aplikace už běží, použij focus, ne spawn.\n\
Když pán žádá akci s okny/aplikacemi, PROVEĎ ji těmito příkazy (žádné vymýšlení, \
že to nejde). Než někam napíšeš text, VŽDY ověř, že je aktivní správné okno \
(focus/active, případně screenshot). Pokud akce může něco odeslat či smazat \
a cíl není jednoznačný, radši se zastav a řekni, co ti chybí. Výsledek akce \
na závěr shrň jednou větou.\n";

/// Návod na runbooky — přikládá se, jen když je [runbooks] enabled
/// a voice_run. Schvalování v allowlistu záměrně není.
const RUNBOOK_TOOLS_PROMPT: &str = "\
Umíš spouštět SCHVÁLENÉ automatizace (runbooky): Bash máš povolený pro\n\
  jarvis runbook list | pending | show <id> | runs | run <id|část názvu> --trigger voice\n\
`run` spouštěj jen na výslovnou žádost pána; po doběhu shrň výsledek jednou \
větou (exit 0 = úspěch, jinak řekni, co selhalo). Schválit ani zamítnout \
návrh hlasem NEJDE — to pán dělá sám (`jarvis runbook approve` v terminálu, \
nebo Telegram); když o to požádá, řekni mu to.\n";

/// Návod na SMS — přikládá se, jen když je [sms] enabled.
const SMS_TOOLS_PROMPT: &str = "\
Umíš poslat SMS: Bash příkaz `jarvis sms \"text\"` pošle zprávu pánovi na jeho \
mobil (výchozí příjemce z konfigurace; na SMS z tohoto kanálu nejde odpovědět). \
Jinému příjemci JEN když pán výslovně nadiktoval číslo: `jarvis sms --to \
+420123456789 \"text\"` — jinak --to nepoužívej. Příkaz čeká na doručenku \
a vypíše stav; text drž krátký, diakritika je v pořádku.\n";

/// Úprava odpovědi pro TTS: jeden řádek, ořez na limit řeči.
fn normalize_for_speech(text: &str, max_chars: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    util::truncate_chars(&joined, max_chars)
}

/// Vyhodnocení open-ear kill-gate: kolik directed promluv model chytil (recall)
/// a — hlavní metrika — jak často „skočil do řeči" na human/background (false
/// accept). Bias návrhu: false accept se drží co nejníž i za cenu recall.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct EvalTally {
    pub directed_total: u32,
    pub directed_hit: u32,
    pub other_total: u32,
    pub other_accept: u32,
    pub skipped: u32,
}

impl EvalTally {
    fn record(&mut self, label: &str, directed: bool) {
        match label {
            "directed" => {
                self.directed_total += 1;
                self.directed_hit += u32::from(directed);
            }
            "human" | "background" => {
                self.other_total += 1;
                self.other_accept += u32::from(directed);
            }
            _ => self.skipped += 1,
        }
    }
    /// Podíl directed promluv, které model správně chytil (0–1).
    pub fn recall(&self) -> f64 {
        if self.directed_total == 0 {
            0.0
        } else {
            f64::from(self.directed_hit) / f64::from(self.directed_total)
        }
    }
    /// Podíl human/background promluv, na které model chybně odpověděl (0–1) —
    /// „skočení do cizí řeči". Klíčová metrika kill-gate; cíl < ~2–3 %.
    pub fn false_accept_rate(&self) -> f64 {
        if self.other_total == 0 {
            0.0
        } else {
            f64::from(self.other_accept) / f64::from(self.other_total)
        }
    }
}

/// Kill-gate: prožene olabelovaný JSONL korpus (`{"text","label"[,"screen"]}`,
/// label = directed|human|background) skutečným klasifikátorem a vypíše
/// confusion matrix + recall + false-accept rate. Reálná API spend — běží
/// s tvým klíčem, útrata se eviduje do `costs` (component „converse-gate").
pub fn eval_open_ear(paths: &Paths, cfg: &Config, file: &Path) -> Result<()> {
    let body = std::fs::read_to_string(file)
        .with_context(|| format!("nelze číst korpus {}", file.display()))?;
    let conn = db::open(&paths.db_path)?;
    let mut tally = EvalTally::default();
    let mut cost = 0.0;
    for (i, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: serde_json::Value =
            serde_json::from_str(line).with_context(|| format!("řádek {}: neplatný JSON", i + 1))?;
        let text = row["text"].as_str().unwrap_or_default();
        let label = row["label"].as_str().unwrap_or_default();
        if text.is_empty() || label.is_empty() {
            eprintln!("řádek {}: chybí text/label — přeskakuji", i + 1);
            continue;
        }
        let (directed, outcome) = classify_directed(paths, cfg, text, row["screen"].as_str())?;
        cost += outcome.cost_usd;
        let _ = db::insert_cost(
            &conn,
            util::now_ts(),
            "converse-gate",
            &cfg.converse.model,
            outcome.tokens_in,
            outcome.tokens_out,
            outcome.cost_usd,
        );
        tally.record(label, directed);
        let mark = match (label, directed) {
            ("directed", true) | ("human", false) | ("background", false) => "ok  ",
            ("directed", false) => "MISS",
            _ => "BUTT", // human/background + ANO = skočení do cizí řeči
        };
        println!("{mark} [{label:^10}→{}] {text}", if directed { "ANO" } else { "NE " });
    }
    println!("\n── open-ear kill-gate ──");
    println!(
        "directed:  {}/{} chyceno   (recall {:.0} %)",
        tally.directed_hit,
        tally.directed_total,
        tally.recall() * 100.0
    );
    println!(
        "human/bg:  {}/{} skočení   (false-accept {:.1} %)  ← klíčová metrika",
        tally.other_accept,
        tally.other_total,
        tally.false_accept_rate() * 100.0
    );
    if tally.skipped > 0 {
        println!("přeskočeno: {} (neznámý label)", tally.skipped);
    }
    println!("náklad:    {cost:.4} USD");
    println!("\nZapnout „always“ má smysl, jen když je false-accept hodně nízko (cíl < 2–3 %).");
    Ok(())
}

/// Vypíše posledních `n` mic promluv jako JSONL šablonu kill-gate korpusu
/// (`{"text","label":""}`); olabeluj `label` na directed|human|background
/// a pusť `jarvis converse-eval <soubor>`.
pub fn eval_scaffold(paths: &Paths, n: usize) -> Result<()> {
    let conn = db::open(&paths.db_path)?;
    for t in db::recent_utterance_texts(&conn, n)? {
        println!("{}", serde_json::json!({ "text": t, "label": "" }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn wake_words_exact_and_normalized() {
        let cfg = Config::default().converse;
        let w = WakeWords::new(&cfg.wake_words, false, "").unwrap();
        assert!(w.matches("Jarvisi, kolik je hodin?"));
        assert!(w.matches("hej JARVISI!"));
        assert!(w.matches("Jarvise, slyšíš mě?"));
        assert!(w.matches("Jár visi, haló")); // normalizace slepí mezery a diakritiku
        // bez fuzzy: nominativ ani komolení nespouští
        assert!(!w.matches("teď ladím jarvis listen a padá mi to"));
        assert!(!w.matches("Javi si slyšíš mě"));
    }

    #[test]
    fn wake_words_fuzzy_catches_real_whisper_mangling() {
        let cfg = Config::default().converse;
        let w = WakeWords::new(&cfg.wake_words, true, "").unwrap();
        // reálný přepis z journalu 2026-07-17: „Jarvisi" → „Javi si"
        assert!(w.matches("Javi si slyšíš mě. Odpovězd mi jednou krátkou větou."));
        assert!(w.matches("Jarvis, kolik je hodin?")); // nominativ ≈ vzdálenost 1
        assert!(w.matches("Džarvisi, haló"));
        // běžná slova nesmí spouštět (vzdálenost ≥ 2)
        assert!(!w.matches("auto je v servisu, závist nikam nevede"));
        assert!(!w.matches("ECHO napsalo o motoristech, motoristé naplní podtávku"));
        // vědomý trade-off fuzzy režimu: skloňované „jarvis" v běžné řeči chytne
        assert!(w.matches("teď ladím jarvis listen"));
    }

    fn wake_default() -> WakeWords {
        let cfg = Config::default();
        WakeWords::new(&cfg.converse.wake_words, true, "").unwrap()
    }

    #[test]
    fn triage_wake_answers_in_every_mode() {
        let wake = wake_default();
        for mode in [OpenEarMode::Off, OpenEarMode::Followup, OpenEarMode::Always] {
            let ear = OpenEar { mode, window_s: 12, min_words: 2 };
            assert_eq!(
                triage(&wake, &ear, "Jarvisi, kolik je hodin?", 1000, 0),
                Some(Trigger::Wake),
                "wake musí fungovat i v režimu {mode:?}"
            );
        }
    }

    #[test]
    fn triage_off_ignores_non_wake() {
        let wake = wake_default();
        let ear = OpenEar { mode: OpenEarMode::Off, window_s: 12, min_words: 2 };
        assert_eq!(triage(&wake, &ear, "kolik je hodin", 1000, 995), None);
    }

    #[test]
    fn triage_followup_only_inside_window() {
        let wake = wake_default();
        let ear = OpenEar { mode: OpenEarMode::Followup, window_s: 12, min_words: 2 };
        // 5 s po Jarvisově řeči → follow-up
        assert_eq!(triage(&wake, &ear, "a co zítra", 1005, 1000), Some(Trigger::Followup));
        // 20 s po → okno zavřené
        assert_eq!(triage(&wake, &ear, "a co zítra", 1020, 1000), None);
        // Jarvis ještě nikdy nemluvil → žádné okno
        assert_eq!(triage(&wake, &ear, "a co zítra", 1005, 0), None);
        // přesně na hraně okna (12 s) ještě platí, o sekundu dál už ne
        assert_eq!(triage(&wake, &ear, "a co zítra", 1012, 1000), Some(Trigger::Followup));
        assert_eq!(triage(&wake, &ear, "a co zítra", 1013, 1000), None);
    }

    #[test]
    fn triage_min_words_filters_fillers() {
        let wake = wake_default();
        let ear = OpenEar { mode: OpenEarMode::Followup, window_s: 12, min_words: 2 };
        // jednoslovné „díky" v okně → nekandiduje (okno se přirozeně zavře)
        assert_eq!(triage(&wake, &ear, "díky", 1005, 1000), None);
        assert_eq!(triage(&wake, &ear, "a dost", 1005, 1000), Some(Trigger::Followup));
    }

    #[test]
    fn triage_always_candidate_outside_window_followup_inside() {
        let wake = wake_default();
        let ear = OpenEar { mode: OpenEarMode::Always, window_s: 12, min_words: 2 };
        // mimo okno → kandidát na klasifikátor
        assert_eq!(triage(&wake, &ear, "zhasni monitor", 5000, 1000), Some(Trigger::Candidate));
        // v okně → levnější follow-up (bez klasifikátoru)
        assert_eq!(triage(&wake, &ear, "zhasni monitor", 1005, 1000), Some(Trigger::Followup));
        // překrytí vlastní řeči (echo) → nic, ať se neplatí klasifikátor na ozvěnu
        assert_eq!(triage(&wake, &ear, "zhasni monitor", 1000, 1000), None);
        // Jarvis nikdy nemluvil → rovnou kandidát
        assert_eq!(triage(&wake, &ear, "zhasni monitor", 5000, 0), Some(Trigger::Candidate));
    }

    #[test]
    fn triage_hint_echo_never_triggers_open_ear() {
        let cfg = Config::default();
        let wake = WakeWords::new(&cfg.converse.wake_words, true, &cfg.listen.hint).unwrap();
        let ear = OpenEar { mode: OpenEarMode::Always, window_s: 12, min_words: 2 };
        // halucinace hintu (dlouhý společný úsek se slovníkem) nebudí ani open-ear
        assert_eq!(
            triage(&wake, &ear, "No a pak slovník Jarvis, Jarvisi hraje dál.", 5000, 1000),
            None
        );
    }

    #[test]
    fn gate_verdict_defaults_to_no() {
        // jasné ANO (i s interpunkcí, whitespace, celou větou)
        assert!(parse_gate_verdict("ANO"));
        assert!(parse_gate_verdict("Ano."));
        assert!(parse_gate_verdict("  ano  \n"));
        assert!(parse_gate_verdict("Ano, mířilo to na tebe."));
        // vše ostatní = NE (bias do ticha)
        assert!(!parse_gate_verdict("NE"));
        assert!(!parse_gate_verdict("Ne, to bylo na někoho jiného."));
        assert!(!parse_gate_verdict("nevím"));
        assert!(!parse_gate_verdict(""));
        assert!(!parse_gate_verdict("???"));
        assert!(!parse_gate_verdict("nano")); // nezačíná na „ano"
    }

    #[test]
    fn gate_prompt_has_question_screen_and_bias() {
        let p = build_gate_prompt("zhasni monitor", Some("Signal — Tomáš"));
        assert!(p.contains("zhasni monitor"));
        assert!(p.contains("Signal — Tomáš"));
        assert!(p.contains("ANO") && p.contains("NE"));
        assert!(p.contains("Když váháš, řekni NE"));
        // bez aktivního okna se řádek o obrazovce nepřidá
        let p0 = build_gate_prompt("kolik je hodin", None);
        assert!(!p0.contains("Na obrazovce"));
        assert!(p0.contains("kolik je hodin"));
    }

    #[test]
    fn hint_echo_guard_blocks_hallucinated_hint_not_real_addressing() {
        let cfg = Config::default();
        let w = WakeWords::new(&cfg.converse.wake_words, true, &cfg.listen.hint).unwrap();
        // skutečná oslovení projdou (společný úsek s hintem = jen jméno, 7 < 10)
        assert!(w.matches("Jarvisi, slyšíš mě?"));
        assert!(w.matches("Jarvisi, kolik je hodin?"));
        // halucinace hintu na hudbě/šumu konverzaci nebudí — whisper 2026-07-17
        // reálně opisoval celé fráze nápovědy do přepisů
        assert!(!w.matches("No a pak slovník Jarvis, Jarvisi hraje dál."));
        assert!(!w.matches("Jarvisi, ElevenLabs, digest."));
        // guard vypnutý prázdným hintem
        let w0 = WakeWords::new(&cfg.converse.wake_words, true, "").unwrap();
        assert!(w0.matches("Jarvisi, ElevenLabs, digest."));
    }

    #[test]
    fn longest_common_run_basics() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(longest_common_run(&c("abcdef"), &c("xxcdexx")), 3);
        assert_eq!(longest_common_run(&c("jarvisi"), &c("slovnikjarvisjarvisi")), 7);
        assert_eq!(longest_common_run(&c(""), &c("abc")), 0);
        assert_eq!(longest_common_run(&c("abc"), &c("xyz")), 0);
    }

    #[test]
    fn levenshtein_basics() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(levenshtein(&c("jarvisi"), &c("jarvisi")), 0);
        assert_eq!(levenshtein(&c("jarvisi"), &c("javisi")), 1); // vypadlé r
        assert_eq!(levenshtein(&c("jarvisi"), &c("jarvis")), 1);
        assert_eq!(levenshtein(&c("jarvisi"), &c("servisu")), 3);
        assert_eq!(levenshtein(&c("abc"), &c("")), 3);
    }

    #[test]
    fn prompt_contains_question_and_context() {
        let conn = mem_db();
        db::insert_conversation(&conn, 100, "Kolik je hodin?", "Pět, pane.", "m", 0.0).unwrap();
        let cfg = Config::default();
        let p = build_prompt(&cfg, &conn, "A za hodinu?").unwrap();
        assert!(p.contains("„A za hodinu?“"));
        assert!(p.contains("Kolik je hodin?"));
        assert!(p.contains("Pět, pane."));
        assert!(p.contains("česky"));
        assert!(p.contains("NAHLAS"));
    }

    #[test]
    fn prompt_wm_tools_follow_config() {
        let conn = mem_db();
        let mut cfg = Config::default();
        cfg.wm.enabled = true;
        let p = build_prompt(&cfg, &conn, "přepni na signal").unwrap();
        assert!(p.contains("jarvis wm"));
        assert!(p.contains("screenshot"));
        assert!(p.contains("spawn"));
        assert!(p.contains("spawn_allowed"));
        cfg.wm.enabled = false;
        let p = build_prompt(&cfg, &conn, "přepni na signal").unwrap();
        assert!(!p.contains("jarvis wm"));
    }

    #[test]
    fn agent_caps_follow_tool_flags() {
        let mut cfg = Config::default();
        cfg.converse.max_turns = 7;
        cfg.converse.web = false; // web se testuje zvlášť
        cfg.wm.enabled = true;
        cfg.sms.enabled = false;
        cfg.runbooks.enabled = false;
        assert_eq!(agent_caps(&cfg), ("Read,Bash(jarvis wm:*)".to_string(), 7));
        cfg.sms.enabled = true;
        assert_eq!(
            agent_caps(&cfg),
            ("Read,Bash(jarvis wm:*),Bash(jarvis sms:*)".to_string(), 7)
        );
        cfg.wm.enabled = false;
        assert_eq!(agent_caps(&cfg), ("Read,Bash(jarvis sms:*)".to_string(), 7));
        cfg.sms.enabled = false;
        assert_eq!(agent_caps(&cfg), ("Read".to_string(), 1));
    }

    #[test]
    fn agent_caps_runbooks_run_only_never_approve() {
        let mut cfg = Config::default();
        cfg.converse.max_turns = 9;
        cfg.converse.web = false; // web se testuje zvlášť
        cfg.wm.enabled = false;
        cfg.sms.enabled = false;
        cfg.runbooks.enabled = true;
        cfg.runbooks.voice_run = true;
        let (tools, turns) = agent_caps(&cfg);
        assert_eq!(turns, 9);
        assert!(tools.contains("Bash(jarvis runbook run:*)"));
        assert!(tools.contains("Bash(jarvis runbook list)"));
        // schvalování hlasem nesmí existovat v žádné podobě
        assert!(!tools.contains("approve"));
        assert!(!tools.contains("dismiss"));
        assert!(!tools.contains("Bash(jarvis runbook:*)"));
        cfg.runbooks.voice_run = false;
        assert_eq!(agent_caps(&cfg), ("Read".to_string(), 1));
    }

    #[test]
    fn agent_caps_web_flag() {
        let mut cfg = Config::default();
        cfg.converse.max_turns = 5;
        cfg.wm.enabled = false;
        cfg.sms.enabled = false;
        cfg.runbooks.enabled = false;
        // web zapnutý (default): agent dostane WebSearch/WebFetch a víc kol
        cfg.converse.web = true;
        assert_eq!(agent_caps(&cfg), ("Read,WebSearch,WebFetch".to_string(), 5));
        // web vypnutý bez dalších nástrojů = jen Read, jedno kolo
        cfg.converse.web = false;
        assert_eq!(agent_caps(&cfg), ("Read".to_string(), 1));
    }

    #[test]
    fn prompt_web_hint_follows_config() {
        let conn = mem_db();
        let mut cfg = Config::default();
        cfg.converse.web = true;
        assert!(build_prompt(&cfg, &conn, "jaké je počasí").unwrap().contains("WebSearch"));
        cfg.converse.web = false;
        assert!(!build_prompt(&cfg, &conn, "jaké je počasí").unwrap().contains("WebSearch"));
    }

    #[test]
    fn prompt_runbook_tools_follow_config() {
        let conn = mem_db();
        let mut cfg = Config::default();
        cfg.runbooks.enabled = true;
        cfg.runbooks.voice_run = true;
        let p = build_prompt(&cfg, &conn, "spusť ranní sync").unwrap();
        assert!(p.contains("jarvis runbook"));
        assert!(p.contains("NEJDE"));
        cfg.runbooks.voice_run = false;
        let p = build_prompt(&cfg, &conn, "spusť ranní sync").unwrap();
        assert!(!p.contains("jarvis runbook"));
    }

    #[test]
    fn prompt_sms_tools_follow_config() {
        let conn = mem_db();
        let mut cfg = Config::default();
        cfg.sms.enabled = true;
        let p = build_prompt(&cfg, &conn, "pošli mi to smskou").unwrap();
        assert!(p.contains("jarvis sms"));
        cfg.sms.enabled = false;
        let p = build_prompt(&cfg, &conn, "pošli mi to smskou").unwrap();
        assert!(!p.contains("jarvis sms"));
    }

    #[test]
    fn conversations_roundtrip_and_order() {
        let conn = mem_db();
        for (i, q) in ["první", "druhá", "třetí"].iter().enumerate() {
            db::insert_conversation(&conn, 10 + i as i64, q, "odp", "m", 0.001).unwrap();
        }
        let recent = db::recent_conversations(&conn, 2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].0, "druhá"); // chronologicky, nejstarší první
        assert_eq!(recent[1].0, "třetí");
        assert_eq!(db::conversation_count_since(&conn, 11).unwrap(), 2);
    }

    #[test]
    fn budget_guard_uses_costs_table() {
        let conn = mem_db();
        let cfg = Config::default(); // strop 1.0 USD
        assert!(!over_budget(&cfg, &conn).unwrap());
        db::insert_cost(&conn, util::now_ts(), "analyze", "m", 0, 0, 1.5).unwrap();
        assert!(over_budget(&cfg, &conn).unwrap());
    }

    #[test]
    fn pick_ack_maps_seed_and_handles_empty() {
        let acks = vec![
            "Ano, pane?".to_string(),
            "Poslouchám, pane.".to_string(),
            "K službám, pane.".to_string(),
        ];
        // seed % počet → index (včetně wrapu)
        assert_eq!(pick_ack(&acks, 0), Some("Ano, pane?"));
        assert_eq!(pick_ack(&acks, 1), Some("Poslouchám, pane."));
        assert_eq!(pick_ack(&acks, 2), Some("K službám, pane."));
        assert_eq!(pick_ack(&acks, 3), Some("Ano, pane?")); // wrap
        assert_eq!(pick_ack(&acks, 7), Some("Poslouchám, pane.")); // 7 % 3 = 1
        // prázdné položky se přeskočí; index je do filtrovaného seznamu
        let mixed = vec!["".to_string(), "  ".to_string(), "Jediná".to_string()];
        assert_eq!(pick_ack(&mixed, 0), Some("Jediná"));
        assert_eq!(pick_ack(&mixed, 9), Some("Jediná"));
        // prázdný i celý prázdný list = ack vypnutý
        assert_eq!(pick_ack(&[], 0), None);
        assert_eq!(pick_ack(&["".to_string(), "  ".to_string()], 3), None);
        // jediná fráze → vždy ona
        assert_eq!(pick_ack(&["Jen já".to_string()], 42), Some("Jen já"));
    }

    #[test]
    fn eval_tally_recall_and_false_accept() {
        let mut t = EvalTally::default();
        t.record("directed", true); // hit
        t.record("directed", false); // miss
        t.record("human", true); // skočení do řeči
        t.record("human", false); // ok
        t.record("background", false); // ok
        t.record("nonsense", true); // neznámý label → skip
        assert_eq!(t.directed_total, 2);
        assert_eq!(t.directed_hit, 1);
        assert_eq!(t.other_total, 3);
        assert_eq!(t.other_accept, 1);
        assert_eq!(t.skipped, 1);
        assert!((t.recall() - 0.5).abs() < 1e-9);
        assert!((t.false_accept_rate() - 1.0 / 3.0).abs() < 1e-9);
        // prázdná tally nedělí nulou
        let e = EvalTally::default();
        assert_eq!(e.recall(), 0.0);
        assert_eq!(e.false_accept_rate(), 0.0);
    }

    #[test]
    fn normalize_flattens_and_truncates() {
        assert_eq!(normalize_for_speech("Ano,\npane.\n\n  Jistě.", 100), "Ano, pane. Jistě.");
        let long = "slovo ".repeat(100);
        assert!(normalize_for_speech(&long, 20).chars().count() <= 20);
    }

    #[test]
    fn chunker_emits_on_sentence_boundary_with_trailing_space() {
        let mut c = SpeechChunker::new();
        assert!(c.push("Je přibližně ").is_empty());
        assert!(c.push("čtvrt na pět").is_empty());
        // koncovka na konci bufferu ještě neřeže (věta může pokračovat)
        assert!(c.push(" odpoledne, pane.").is_empty());
        // až mezera za koncovkou uvolní hotovou větu
        assert_eq!(c.push(" A co"), vec!["Je přibližně čtvrt na pět odpoledne, pane."]);
        assert_eq!(c.flush().as_deref(), Some("A co"));
    }

    #[test]
    fn chunker_multiple_sentences_and_newline() {
        let mut c = SpeechChunker::new();
        // koncovky uvnitř textu se emitují hned; poslední „.“ je na konci → flush
        assert_eq!(
            c.push("Ano. Dnes bude jasno.\nTeplota kolem dvaceti."),
            vec!["Ano.", "Dnes bude jasno."]
        );
        assert_eq!(c.flush().as_deref(), Some("Teplota kolem dvaceti."));
    }

    #[test]
    fn chunker_does_not_split_decimals() {
        let mut c = SpeechChunker::new();
        assert!(c.push("Teplota je 20").is_empty());
        assert!(c.push(".").is_empty()); // konec bufferu → počkej (může být decimál)
        assert!(c.push("5 stupně.").is_empty()); // „20.5 ….“ koncovka zas na konci
        assert_eq!(c.flush().as_deref(), Some("Teplota je 20.5 stupně."));
    }

    #[test]
    fn chunker_overflow_guard_cuts_long_runon() {
        let mut c = SpeechChunker::new();
        // dlouhé souvětí bez koncovky (přes CHUNK_MAX_HOLD) se ustřihne na mezeře,
        // ať řeč nestartuje pozdě — nic se neztratí
        let long = "slovo ".repeat(60); // ~360 znaků, žádná koncovka věty
        assert!(!c.push(&long).is_empty(), "dlouhý běh se má ustřihnout, ne držet");
    }
}
