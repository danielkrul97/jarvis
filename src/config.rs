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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunbooksCfg {
    /// Fáze D: spouštění schválených runbooků (timer/hlas/CLI). Vypnuto =
    /// run-due nic nespouští a hlasový agent runbooky nevidí; schvalování
    /// a CLI `jarvis runbook run` fungují dál.
    pub enabled: bool,
    /// Hlasový agent smí `jarvis runbook run` (jen už schválené runbooky;
    /// schvalovat hlasem nejde nikdy — mikrofonu se nevěří).
    pub voice_run: bool,
    /// Tvrdý strop běhu skriptu; po vypršení SIGKILL celé process group.
    pub timeout_s: u64,
    /// Ořez uloženého výstupu běhu (DB nemá držet megabajty logů).
    pub max_output_chars: usize,
    /// Nový návrh automatizace ohlásit SMS (vyžaduje zapnuté [sms]).
    pub notify_sms: bool,
    /// Schvalování na dálku přes Telegram bot (TELEGRAM_BOT_TOKEN +
    /// TELEGRAM_CHAT_ID v secrets.env); run-due vyřizuje „schval N“ /
    /// „zamítni N“ z ověřeného chatu a nové návrhy tam ohlašuje.
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
    /// SMS kanál (Twilio). Vypnuto = `jarvis sms` odmítne a agent SMS nevidí.
    pub enabled: bool,
    /// Odesílatel: Messaging Service SID (`MG…`), E.164 číslo, nebo
    /// alfanumerický sender (max 11 znaků; příjemce nemůže odpovědět).
    pub from: String,
    /// Výchozí příjemce v E.164 (+420…) — typicky vlastní mobil.
    pub to: String,
    /// Pojistka délky (SMS se účtují po segmentech ~70 znaků s diakritikou).
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
    /// Konverzační agent smí ovládat okna/klávesnici/myš (Bash omezený na
    /// `jarvis wm …`). CLI `jarvis wm` funguje nezávisle na tomto přepínači.
    pub enabled: bool,
    /// Rozestup syntetických kláves (XTest) v ms.
    pub key_delay_ms: u64,
    /// Programy, které smí `jarvis wm spawn` spouštět mimo interaktivní
    /// terminál (hlasový agent, timery). Porovnává se přesně: holé jméno
    /// = binárka v PATH, absolutní cesta = konkrétní soubor. Prázdný
    /// seznam = spawn mimo TTY odmítne všechno; z terminálu funguje vždy.
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
    /// `jarvis meet <URL>` — Jarvis se připojí do Google Meet jako samostatný
    /// účastník (vlastní Chrome, virtuální mikrofon + reproduktor). false =
    /// příkaz odmítne běžet.
    pub enabled: bool,
    /// Binárka prohlížeče (jméno v PATH nebo absolutní cesta). Chrome/Chromium
    /// (WebRTC + spolehlivý výběr audio zařízení přes PULSE_SINK/PULSE_SOURCE).
    pub chrome_bin: String,
    /// Jméno, pod kterým Jarvis vystupuje v hovoru (vyplní se do pole „Your name").
    pub display_name: String,
    /// PulseAudio null-sink, do kterého míří Jarvisova řeč; jeho `.monitor`
    /// přemapovaný na `mic_source` slouží jako mikrofon do hovoru.
    pub mic_sink: String,
    /// PulseAudio remap-source (z `mic_sink`.monitor) — tohle Chrome vybere
    /// jako mikrofon (getUserMedia).
    pub mic_source: String,
    /// PulseAudio null-sink, kam Chrome hraje zvuk hovoru; jeho `.monitor`
    /// poslouchá STT (Jarvis slyší ostatní účastníky).
    pub ear_sink: String,
    /// Adresář profilu dedikovaného Chrome; prázdné = `<data_dir>/meet-profile`.
    pub profile_dir: String,
    /// Model pro vizuálního join-agenta (screenshot → klik). Prázdné = default CLI.
    pub join_model: String,
    /// Strop, jak dlouho join-agent zkouší připojení (vč. čekání na admit), v s.
    pub join_timeout_s: u64,
    /// Strop kol vizuálního join-agenta (screenshot → akce → ověření).
    pub join_max_turns: u32,
    /// Průběžně přepisovat celý hovor do DB (utterances, source=meet).
    pub transcribe: bool,
    /// Po skončení hovoru vygenerovat a odeslat shrnutí schůzky.
    pub summary: bool,
    /// Kam poslat shrnutí: "email" | "telegram" | "both" | "none".
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
    /// Když je aktivní zámek/šetřič obrazovky (XFCE screensaver, dotaz přes
    /// D-Bus `org.xfce.ScreenSaver.GetActive`), mic démon zvuk zahazuje a nic
    /// nepřepisuje — stejné soukromí jako `jarvis pause`. Netýká se `jarvis
    /// meet` (hovor se přepisuje dál). Fail-open: nejde-li stav zjistit, běží.
    pub pause_when_locked: bool,
    /// STT engine: "auto" = ElevenLabs Scribe (cloud), při chybě lokální
    /// whisper; "elevenlabs" = jen Scribe, bez fallbacku; "whisper" = jen
    /// lokálně (zdarma, nic neopouští stroj, ale náročné na CPU/GPU). V "auto"
    /// se whisper model načítá až při prvním fallbacku (líně) — dokud Scribe
    /// funguje, těžký model vůbec nezatíží stroj.
    pub engine: String,
    /// Model ElevenLabs Scribe: "scribe_v1" (stabilní, 99 jazyků vč. češtiny)
    /// nebo "scribe_v2". Účtuje se po délce audia (~0,22 $/h).
    pub scribe_model: String,
    /// Keyterm biasing pro Scribe: vlastní jména, která má rozpoznat přesně
    /// (bez toho slyší „Jarvisi" jako „Já vysí" a oslovení nezabere). Obdoba
    /// whisperového `hint`. Prázdné = neposílat (+20 % k ceně, když vyplněné).
    pub scribe_keyterms: Vec<String>,
    /// PulseAudio source (`pactl list sources short`); prázdné = výchozí mikrofon.
    pub device: String,
    /// Název ggml modelu bez `ggml-`/`.bin` — stáhne `jarvis listen --download-model`.
    pub model: String,
    /// Explicitní cesta k .bin souboru; přebíjí `model`.
    pub model_path: String,
    /// "auto" = detekce jazyka per promluva, jinak ISO kód ("cs", "en", …).
    pub language: String,
    /// Slovníková nápověda whisperu (initial prompt) — vlastní jména,
    /// která jinak komolí („Jarvisi" → „Jarysy"). Prázdné = bez nápovědy.
    pub hint: String,
    /// 0 = auto (polovina jader, max 8).
    pub threads: usize,
    pub min_speech_ms: u64,
    pub silence_ms: u64,
    pub max_utterance_s: u64,
    /// Citlivost VAD: práh = šumové dno × tento násobek (menší = citlivější).
    pub vad_speech_mult: f32,
}

impl Default for ListenCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            pause_when_locked: true,
            // Scribe je default: whisper turbo je na běžném CPU/GPU náročný
            // (RTF 1–4 bez GPU), Scribe přesune přepis do cloudu za ~0,22 $/h.
            // Bez ElevenLabs klíče "auto" tiše spadne na lokální whisper.
            engine: "auto".into(),
            scribe_model: "scribe_v1".into(),
            scribe_keyterms: vec!["Jarvis".into(), "Jarvisi".into()],
            device: String::new(),
            // turbo na GPU (CUDA build): RTF ~0.2–0.6, nejlepší čeština.
            // CPU fallback pro turbo nestíhá (RTF 1–4) — bez GPU přepni na
            // "small-q5_1" (RTF ~0.2–0.8 na CPU). Viz PLAN §3.7 (2026-07-17).
            model: "large-v3-turbo-q5_0".into(),
            model_path: String::new(),
            // pinnutý jazyk: autodetekce stojí celý encode navíc (i na GPU
            // zdvoj- až ztrojnásobí čas krátkých promluv)
            language: "cs".into(),
            // jméno asistenta whisper nezná — bez nápovědy ho komolí.
            // Slovníkový styl schválně: na šumu/hudbě whisper hint občas
            // halucinuje do přepisu a věta znějící jako oslovení by falešně
            // budila konverzaci (echo-guard viz converse::WakeWords).
            hint: "Slovník: Jarvis, Jarvisi, ElevenLabs, digest.".into(),
            threads: 0,
            min_speech_ms: 300,
            silence_ms: 700,
            max_utterance_s: 28,
            // 2.0: na tichém mikrofonu (SNR ~14 dB) násobek 3 sekal věty
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
    /// "auto" = ElevenLabs, při chybě lokální piper; "piper" = jen lokálně
    /// (zdarma, nic neopouští stroj); "elevenlabs" = jen API, bez fallbacku.
    pub engine: String,
    /// ElevenLabs voice_id. Premade hlasy fungují i se scoped klíčem
    /// bez `voices_read`; hlasy z Voice Library je nutné nejdřív přidat
    /// do účtu (web) a sem vložit jejich ID.
    pub voice_id: String,
    pub model_id: String,
    /// ISO kód ("cs"); vynucení umí jen *_v2_5 modely, multilingual_v2
    /// jazyk pozná z textu. "auto" = nikdy neposílat language_code.
    pub language: String,
    /// Jen mp3_* — přehrávání i cache s kontejnerovým formátem počítají.
    pub output_format: String,
    /// 0–1; nižší = expresivnější přednes.
    pub stability: f32,
    /// 0–1; věrnost původnímu hlasu.
    pub similarity_boost: f32,
    /// 0–1; u češtiny držet nízko, vyšší hodnoty deformují výslovnost.
    pub style: f32,
    pub speaker_boost: bool,
    /// 0.7–1.2; Brumbál nespěchá.
    pub speed: f32,
    /// Přehrávač + argumenty; prázdné = auto (ffplay → mpv → ffmpeg+paplay).
    pub player: String,
    /// PulseAudio sink pro Jarvisovu řeč. Nastavený na sink echo-cancel
    /// modulu (sink_name v default.pa) dává AEC far-end referenci —
    /// mikrofon pak Jarvisův hlas odečte a neslyší sám sebe. Prázdné =
    /// výchozí výstup; neexistující sink = warn + výchozí výstup.
    pub sink: String,
    /// Po odeslání denního digestu ho Jarvis ohlásí nahlas.
    pub announce_digest: bool,
    /// Stejný text se stejným nastavením se generuje jen jednou (kredity).
    pub cache: bool,
    /// Pojistka proti spálení kreditů: 1 znak = 1 kredit (multilingual_v2).
    pub max_chars: usize,
    /// Binárka lokálního TTS (`pip3 install --user piper-tts`).
    pub piper_bin: String,
    /// Hlas z rhasspy/piper-voices; stáhne `jarvis say --download-model`.
    pub piper_voice: String,
}

impl Default for SpeakCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: "auto".into(),
            // „George" — premade, teplý hlubší britský vypravěč; přes
            // multilingual_v2 mluví česky a z premade hlasů je Brumbálovi
            // nejblíž. Vlastní volba: `jarvis say --list-voices` / web.
            voice_id: "JBFqnCBsd6RMkjVDRZzb".into(),
            // nejlepší čeština; rychlejší a levnější je eleven_flash_v2_5
            model_id: "eleven_multilingual_v2".into(),
            language: "cs".into(),
            output_format: "mp3_44100_128".into(),
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
            // jediný kvalitní český hlas v piper-voices (mužský, medium)
            piper_voice: "cs_CZ-jirka-medium".into(),
        }
    }
}

/// Deserializace, která přijme jeden řetězec i pole řetězců → Vec<String>
/// (zpětná kompatibilita: `ack = "Ano, pane?"` i `ack = ["…", "…"]`).
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
    /// Hlasový dialog: promluva s oslovením → Claude → odpověď nahlas.
    pub enabled: bool,
    /// Kmeny oslovení (case-insensitive, bez diakritiky a mezer). Vokativ
    /// („jarvisi") netriggeruje řeč O projektu; volnější: přidej "jarvis".
    pub wake_words: Vec<String>,
    /// Tolerance 1 editační chyby přepisu („Javi si" ≈ „Jarvisi"). Cena:
    /// občas chytne i skloňované „jarvis" v běžné řeči.
    pub wake_fuzzy: bool,
    /// Odpovídání bez wake-wordu (addressee detection). "off" = jen na
    /// oslovení jménem (výchozí, dnešní chování). "followup" = po Jarvisově
    /// odpovědi je krátké okno, kdy navazující promluva jméno nepotřebuje
    /// (Tier 1). "always" = každou věrohodnou promluvu posoudí skeptický
    /// klasifikátor, jestli mířila na Jarvise (Tier 2, experimentální — zapni
    /// až po kill-gate, viz PLAN §3.9).
    pub open_ear: String,
    /// Jak dlouho po Jarvisově řeči drží follow-up okno (s). Krátké schválně:
    /// čím delší, tím větší riziko skočení do řeči, když se mezitím otočíš
    /// na člověka.
    pub followup_window_s: u64,
    /// Minimální počet slov promluvy bez wake-wordu, aby vůbec byla kandidátem
    /// (odfiltruje „ehm", „jo" — ta se nemají posílat workerovi ani klasifikátoru).
    pub open_ear_min_words: usize,
    /// Model pro odpovědi; rychlost > síla (mluvený dialog).
    pub model: String,
    /// Okamžitá reakce na oslovení, než Claude vymyslí odpověď; vybírá se z listu
    /// náhodně kvůli pestrosti. Přijme i jediný řetězec. "" nebo [] = vypnuto.
    #[serde(deserialize_with = "string_or_seq")]
    pub ack: Vec<String>,
    /// Kolik minulých výměn se přikládá pro navazující otázky.
    pub max_context_exchanges: usize,
    /// Strop kol agenta, když má povolené nástroje ([wm] enabled) — akce
    /// s okny potřebují víc otoček (příkaz → screenshot → ověření). Bez
    /// nástrojů se vždy používá 1.
    pub max_turns: u32,
    pub timeout_s: u64,
    /// false = denní strop (analysis.daily_budget_usd) konverzace neblokuje;
    /// útrata se dál eviduje a je vidět ve `status` a digestu.
    pub respect_budget: bool,
    /// Rezidentní claude proces (stream-json): odpověď bez CLI startu
    /// (~2 s úspora). Při chybě automatický fallback na jednorázový spawn.
    pub warm: bool,
    /// Po tolika výměnách se proces recykluje — session akumuluje kontext
    /// a input tokeny (cena) by rostly donekonečna.
    pub warm_max_exchanges: usize,
    /// Recyklace po nečinnosti (čerstvá session ráno místo včerejší).
    pub warm_idle_s: u64,
    /// Konverzační agent smí hledat na webu (WebSearch/WebFetch) — aktuální
    /// informace: počasí, zprávy, kurzy, cokoli po datu znalostí modelu.
    /// Web search se účtuje přes Anthropic (~0,01 $/dotaz). false = mozek
    /// jede jen z natrénovaných znalostí a aktuality přizná, že nemá.
    pub web: bool,
}

impl Default for ConverseCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            wake_words: vec!["jarvisi".into(), "jarvise".into()],
            wake_fuzzy: true,
            open_ear: "off".into(),
            followup_window_s: 12,
            open_ear_min_words: 2,
            model: "claude-haiku-4-5-20251001".into(),
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
            ],
            max_context_exchanges: 3,
            max_turns: 12,
            timeout_s: 90,
            respect_budget: true,
            warm: true,
            warm_max_exchanges: 10,
            warm_idle_s: 900,
            web: true,
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
        // `contains` je false i pro NaN/inf → jediná mez pokryje i je. Bez
        // kontroly by záporný strop trvale blokoval AI (converse vrací jen
        // BUDGET_REPLY, analýza jede degradovaně) a NaN by strop naopak vypnul.
        if !(0.0..=1000.0).contains(&self.analysis.daily_budget_usd) {
            bail!(
                "analysis.daily_budget_usd musí být 0–1000 USD (konečné číslo), je {}",
                self.analysis.daily_budget_usd
            );
        }
        // 0 by v hodinovém úklidu smazalo úplně všechny snímky (cutoff = teď);
        // horní mez drží `screenshots_days * 86400` mimo overflow i64.
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
        if !matches!(l.engine.as_str(), "auto" | "elevenlabs" | "whisper") {
            bail!("listen.engine musí být auto | elevenlabs | whisper, je '{}'", l.engine);
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
        // data i config drží citlivá data — jen pro uživatele
        for dir in [&self.config_dir, &self.data_dir] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("nelze nastavit práva {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Tajemství: env proměnná `name` má přednost, jinak řádek `name=…` v secrets.env.
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

/// (account SID, auth token) pro Twilio SMS.
pub fn twilio_keys(paths: &Paths) -> Result<(String, String)> {
    Ok((secret(paths, "TWILIO_ACCOUNT_SID")?, secret(paths, "TWILIO_AUTH_TOKEN")?))
}

/// (bot token, chat id) pro schvalování runbooků přes Telegram.
pub fn telegram_keys(paths: &Paths) -> Result<(String, String)> {
    Ok((secret(paths, "TELEGRAM_BOT_TOKEN")?, secret(paths, "TELEGRAM_CHAT_ID")?))
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
    // checked: absurdní vstup („1000000000000000d") by jinak přetekl u64
    // (debug panika / release wrap → nesmyslná pauza, i záporná po `+ now_ts`)
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
        // zpětná kompatibilita: jediný řetězec → jednoprvkový list
        let one: Config = toml::from_str("[converse]\nack = \"Jistě?\"\n").unwrap();
        assert_eq!(one.converse.ack, vec!["Jistě?".to_string()]);
        one.validate().unwrap();
        // list frází
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
        // platné režimy projdou
        for m in ["off", "followup", "always"] {
            let mut cfg = Config::default();
            cfg.converse.open_ear = m.into();
            assert!(cfg.validate().is_ok(), "režim {m} má být platný");
        }
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.email.to, "dankrul.krul@gmail.com");
        assert_eq!(cfg.digest.hour, 19);
        assert_eq!(cfg.retention.screenshots_days, 7);
        // čeština je default celého asistenta
        assert_eq!(cfg.listen.language, "cs");
        assert_eq!(cfg.speak.language, "cs");
        assert_eq!(cfg.speak.model_id, "eleven_multilingual_v2");
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
        // env by přebilo soubor — pro test musí být čisté
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
