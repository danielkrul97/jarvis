# Jarvis — plán implementace

Osobní asistent ve stylu filmového Jarvise: Rust démon pravidelně snímá X11 obrazovku,
zjišťuje, na čem právě pracuji, ukládá data lokálně, extrahuje informace přes pipeline
napojenou na Claude Code CLI a jednou denně posílá formátovaný e-mail s reportem
a doporučeními přes SendGrid na `dankrul.krul@gmail.com`.

**North star: automatizovat moji práci.** Denní report není cíl, ale první fáze —
sběrná vrstva a pipeline jsou navržené tak, aby z pozorování šlo detekovat opakované
rutiny a postupně je předávat automatizacím (fáze A–D, viz §9).

---

## 1. Ověřené prostředí (2026-07-17)

| Věc | Stav |
|---|---|
| Repo | `/mnt/data/Projects/jarvis`, branch `main` bez commitů (pracujeme v mainu, bez commitů) |
| Rust | cargo/rustc 1.94.0 |
| Claude Code CLI | 2.1.212 (`claude` v PATH) |
| Session | X11, `DISPLAY=:0.0`, 1 monitor 1920×1080 (HDMI-0) |
| Tajemství | SendGrid + ElevenLabs API klíče v `~/.config/jarvis/secrets.env` (0600), **nikdy v repu ani v tomto souboru** |

Předpoklad: pouze X11 (Wayland mimo rozsah, viz rizika).

---

## 2. Architektura

```
             ┌────────────────────────── jarvis (jeden binární program, subcommandy) ──────────────────────────┐
             │                                                                                                 │
 X11 server  │  [capture daemon]         [analyze — každou hodinu]        [digest — denně 19:00]               │
 ┌────────┐  │  every 10 s: metadata     1. vyber ≤8 reprezentativních    1. agreguj denní timeline + souhrny  │
 │ DISPLAY │─┼─▶ aktivní okno, idle      snímků za poslední hodinu        2. claude -p → Markdown digest       │
 │  :0.0   │  │  every 60 s: screenshot  2. claude -p (vision, JSON)  ──▶ 3. Markdown → HTML šablona           │
 └────────┘  │  (dedup, blacklist)       3. souhrn → SQLite               4. SendGrid v3 → e-mail              │
             │        │                        ▲                               ▲                               │
             │        ▼                        │                               │                               │
             │  ~/.local/share/jarvis/   ──────┴───────────────────────────────┘                               │
             │  ├─ jarvis.db  (SQLite: samples, summaries, digests, patterns, costs)                           │
             │  └─ shots/YYYY-MM-DD/*.webp  (retence 7 dní)                                                    │
             └─────────────────────────────────────────────────────────────────────────────────────────────────┘
                        spouštění: systemd --user (capture.service + analyze.timer + digest.timer)
```

Klíčové rozhodnutí: **levný vysokofrekvenční signál (titulky oken) + řídký drahý signál
(screenshoty přes vision)**. Titulky oken (browser tab, soubor v editoru, terminál) nesou
většinu informace „na čem pracuji" zadarmo; screenshoty je jen obohacují. Tím se řeší
náklady i objem dat.

---

## 3. Komponenty

### 3.1 Capture daemon (`jarvis capture`)

- **Metadata každých 10 s** přes `x11rb` (EWMH): `_NET_ACTIVE_WINDOW` → `_NET_WM_NAME`
  (fallback `WM_NAME`), `WM_CLASS`, číslo plochy. Idle čas přes XScreenSaver extension
  (`XScreenSaverQueryInfo`).
- **Screenshot každých 60 s** přes `xcap`, ale jen když:
  - idle < 120 s (uživatel je u počítače),
  - aktivní okno neprochází blacklistem (regex na `WM_CLASS`/titulek — výchozí:
    KeePass/Bitwarden, anonymní okna prohlížeče, banking),
  - perceptual hash (`img_hash`, dHash) se liší od posledního uloženého snímku
    (Hamming > 6) — čtu-li 10 minut jeden dokument, uloží se 1 snímek, ne 10,
  - neexistuje pauza (`jarvis pause 30m` → flag soubor s deadlinem).
- **Uložení**: downscale na max 1568 px delší strany (limit Anthropic vision, plné 1080p
  se ani nemusí moc zmenšovat), WebP q85 (fallback JPEG q90, pokud bude s `webp` crate
  friction), ~100–200 KB/snímek. Řádek do `samples` s cestou, phashem, titulkem, idle.
- Robustnost: pád X spojení → reconnect s backoff; žádný `unwrap` na X datech
  (titulky můžou být ne-UTF8, okno může zmizet mezi dotazy).

### 3.2 Extrakční pipeline (`jarvis analyze`, každou hodinu)

1. Načti `samples` od poslední watermarky (tabulka `state`).
2. Segmentuj podle normalizovaného (`wm_class`, titulek) — souvislé bloky činnosti;
   spočítej minuty na segment (to je deterministická časová osa dne, bez tokenů).
3. Vyber ≤8 reprezentativních snímků pokrývajících nejdelší/nejrozmanitější segmenty.
4. Zavolej Claude Code CLI headless:

   ```bash
   cd ~/.local/share/jarvis && claude -p "$(cat prompt.txt)" \
     --output-format json --allowed-tools Read \
     --model claude-haiku-4-5-20251001 --max-turns 12
   ```

   Prompt: přečti uvedené snímky (absolutní cesty) + přiložená timeline titulků,
   vrať **striktní JSON**:

   ```json
   {
     "activities": [{"start": "…", "end": "…", "project": "…", "what": "…", "app": "…"}],
     "projects": ["jarvis", "…"],
     "notable": ["rozepsaný e-mail X nedokončen", "…"],
     "automation_hints": ["ručně kopíruje data z A do B", "…"]
   }
   ```

5. Parsuj `.result` (JSON výstup CLI obsahuje i `total_cost_usd` a usage — ukládat do
   `costs`), ulož do `hourly_summaries`. Retry 1× při nevalidním JSON; při opakovaném
   selhání ulož fallback souhrn jen z titulků (pipeline nikdy neblokuje digest).
6. **Rozpočtová pojistka**: konfigurovatelný denní strop (default 1 USD / 40 volání);
   po překročení se analýza degraduje na titulky-only.

### 3.3 Denní digest (`jarvis digest --send`, 19:00 Europe/Prague)

1. Agreguj den: minuty per projekt/aplikace (z titulků), hodinové souhrny,
   `automation_hints`, srovnání s předchozími dny (co pokračuje, co je nové).
2. `claude -p` (default model session, tj. silnější než haiku) → Markdown se sekcemi:
   **Přehled dne · Na čem jsi pracoval · Rozložení času · Postřehy (fokus, přepínání
   kontextu) · Nedokončené věci · Doporučení na zítřek · Automatizační příležitosti**.
3. Markdown → HTML přes `pulldown-cmark` do jednoduché šablony s inline CSS
   (tmavě/světle neutrální, tabulka časů, čitelné na mobilu) + plaintext alternativa.
4. Odeslání SendGrid v3:

   ```
   POST https://api.sendgrid.com/v3/mail/send
   Authorization: Bearer $SENDGRID_API_KEY
   {"personalizations":[{"to":[{"email":"dankrul.krul@gmail.com"}]}],
    "from":{"email":"<VERIFIKOVANÝ ODESÍLATEL>","name":"Jarvis"},
    "subject":"Jarvis digest — 2026-07-17",
    "content":[{"type":"text/plain","value":"…"},{"type":"text/html","value":"…"}]}
   ```

   Očekávaný výsledek 202. Retry 3× s exponenciálním backoffem; při selhání digest
   zůstává v DB se stavem `pending` a další běh (`OnCalendar` má `Persistent=true`,
   plus hodinový retry) ho doručí. Stav a SendGrid message-id do `daily_digests`.
5. Digest se generuje i při málo datech („dnes skoro nic nenasnímáno — Jarvis běžel Xh").

**Prerekvizita (ruční, jednorázová)**: v SendGrid dashboardu ověřit odesílatele
(Single Sender Verification — může být přímo gmail adresa), jinak API vrací 403.

### 3.7 Poslech mikrofonu (`jarvis listen`) — hlasový kanál, krok 1

Cíl: Jarvis near-realtime „slyší", co se u počítače děje (schůzky, diktování,
poznámky nahlas) — další sběrný kanál vedle X11. Krok 1 = spolehlivý přepis do
DB; napojení na hodinovou analýzu a digest je další krok (čtecí API
`utterances_between` už existuje).

**Pipeline** (vše lokálně, sync + vlákna, bez tokio):

```
parec (PulseAudio, s16le 16 kHz mono; fallback arecord)   [vlákno čtečky, restart+backoff]
  → rámce 30 ms → energetický VAD s adaptivním šumovým prahem
    (pre-roll 300 ms, konec po silence_ms, split po max_utterance_s)  [hlavní smyčka]
  → whisper.cpp přes whisper-rs (CPU; audio_ctx trim ≈ 4–6× rychlejší
    na krátkých promluvách; anti-halucinační filtry no_speech×logprob
    + blacklist frází)                                     [STT vlákno, fronta 8]
  → utterances(ts_start, ts_end, text, lang, conf, source)
```

Rozhodnutí a proč:
- **parec subprocess, ne in-process capture** — audio server resampluje na
  16 kHz kvalitněji než my, formát je garantovaný, subprocess pattern je
  v projektu zavedený (`claude -p`). Pád zdroje řeší respawn s backoffem.
- **Vlastní energy-VAD** — plně unit-testovatelný, bez dalšího modelu. Známá
  mez: trvalý hluk (větrák/hudba) propadne do STT a zabije ho až whisper
  no-speech filtr. Upgrade path: Silero VAD (whisper.cpp ho umí,
  `whisper-rs` exponuje `whisper_vad.rs` + ~2MB model).
- **Model default `large-v3-turbo-q5_0` + `language="cs"`, inference na GPU**
  (GTX 1650, CUDA build). Empiricky 2026-07-17:
  - CPU (i5-12400F, 6 vláken): turbo RTF 0.85–4 ✗; small-q5_1 RTF 0.22–0.77 ✓
    → na CPU je obhajitelný jen small.
  - GPU: turbo RTF **0.17–0.57** ✓ (~6× proti CPU), přepis ~1 s po dořečení;
    VRAM ~0.6 GB z 4 GB. → default turbo (nejlepší čeština).
  - Autodetekce jazyka stojí celý encode navíc i na GPU (krátké promluvy
    RTF 1.1–1.9 ✗) → pin "cs". Bez GPU: `model="small-q5_1"` jedním řádkem.
  - Česká e2e verifikace: piper TTS věta → pipeline → všechna klíčová slova ✓
    (turbo si na syntetickém hlasu vymyslel krátký ocas — artefakt TTS +
    flush bez trimu ticha ve `--wav` režimu; živé promluvy končí trimem).
  - CUDA toolkit je user-space (micromamba, bez sudo), build wiring
    v `.cargo/config.toml` (CUDACXX/CUDAToolkit_ROOT/CUDAARCHS=75 + rpath —
    binárka najde libcudart/libcublas bez LD_LIBRARY_PATH). Pád GPU za běhu
    → whisper.cpp sám spadne na CPU backend (pomalejší, ale funkční).
  - **2026-07-17 večer: mikrofon vyměněn za USB HyperX SoloCast** — front
    jack umřel (-84 dBFS); `module-echo-cancel` přepnut na
    `source_master=alsa_input.usb-HP__Inc_HyperX_SoloCast-00.analog-stereo`
    (v `~/.config/pulse/default.pa` i za běhu), `jarvis_denoised` název
    zůstal → config beze změny. Pozn.: AGC po výměně chvíli „warm-upuje"
    (první ~sekundy nuly). Přidán `listen.hint` (whisper initial prompt)
    na vlastní jména.
  - **Ladění na reálném mikrofonu (front jack, SNR jen ~14 dB)**: VAD práh
    snížen na floor×2 (`vad_speech_mult`, config) — násobek 3 sekal věty;
    Front Mic Boost na 30 dB; a hlavně **webrtc noise suppression + AGC**
    přes `module-echo-cancel` (persistentně v `~/.config/pulse/default.pa`,
    POZOR: `source_master` explicitně, jinak se naváže na monitor = nuly;
    `listen.device = "jarvis_denoised"`). Anti-halucinační filtr rozšířen
    z živých dat: fráze „titulky vytvořil…" padají do conf 0.92 (reálně
    chodily s 0.87) a samostatné „konec" do conf 0.85 (6× za den na ruchu).
- **Soukromí**: audio nikdy na disk, jen text; `pause` zahazuje zvuk z RAM
  a resetuje VAD; `[listen] enabled=false` kanál úplně vypne. Hlídač
  digitálního ticha (2 min peak<3) hlásí mrtvý mikrofon do logu i `status`.
- **Observabilita**: heartbeat `listen_alive_ts` (status hlásí „neběží"),
  per-promluva log s jazykem, confidence a RTF; `doctor --live` měří reálnou
  úroveň mikrofonu; `listen --wav` = deterministický e2e test celé pipeline.

### 3.8 Hlas Jarvise (`jarvis say`) — hlasový kanál, krok 2 (TTS)

Cíl: Jarvis česky mluví — druhá půlka hlasového kanálu vedle poslechu (§3.7).
ElevenLabs API, hlas laděný k Brumbálovi, čeština default.

**Pipeline** (sync, bez nových závislostí — `ureq` + subprocess jako jinde):

```
text → engine (speak.engine):
  "auto"       ElevenLabs → při JAKÉKOLI chybě (kvóta/síť/klíč) lokální piper
  "elevenlabs" jen API (bez zálohy)     "piper" jen lokálně (zdarma, offline)
  → cache lookup (FNV-1a klíč: text+hlas+nastavení; oddělené prostory enginů)
  → [miss] POST /v1/text-to-speech/{voice_id} (mp3)  |  piper subprocess (wav)
  → ~/.local/share/jarvis/tts-cache/<klíč>.{mp3,wav} (atomicky přes .part)
  → přehrávač: ffplay → mpv → ffmpeg+paplay (subprocess, config speak.player)
```

Rozhodnutí a proč (2026-07-17):
- **`eleven_multilingual_v2`** — nejlepší kvalita češtiny; `language_code`
  neumí (vynucení jen u *_v2_5 modelů — klient to řeší sám), češtinu pozná
  z textu. Levnější/rychlejší alternativa jedním řádkem: `eleven_flash_v2_5`.
- **Hlas: „George" (premade `JBFqnCBsd6RMkjVDRZzb`)** — teplý hlubší britský
  vypravěč, z premade hlasů Brumbálovi nejblíž; `speed 0.95` (nespěchá),
  `style 0.0` (vyšší hodnoty deformují českou výslovnost). POZOR: dodaný
  API klíč je **scoped jen na `text_to_speech`** (bez `voices_read`/
  `voices_write`/`user_read`) → Voice Library nejde prohledat ani přidávat
  přes API; brumbálovštější hlas z knihovny se vybírá na webu (přidat do
  My Voices → ID do `speak.voice_id`). Premade hlasy fungují i se scoped
  klíčem.
- **Cache s deterministickým klíčem (FNV-1a 64)** — 1 znak = 1 kredit;
  opakovaná hláška (digest announce) se generuje jednou. `DefaultHasher`
  nejde použít (nestabilní napříč běhy). Spotřeba se loguje do `costs`
  (component `tts`, tokens_in = znaky).
- **Pojistky**: `max_chars 2500` strop na request; `enabled=false` kanál
  vypne; chyby ohlášky v `run` smyčce jen warn (hlas nesmí položit démona);
  jen `mp3_*` formáty (validace) — přehrávání i cache s kontejnerem počítají.
- **Integrace**: `jarvis say` CLI (+ `--out`, `--voice`, `--no-cache`,
  `--list-voices`); po odeslání digestu v `run` smyčce hlasová ohláška
  (`announce_digest`, default zap.); `doctor` kontroluje klíč + přehrávač,
  `doctor --live` stav kreditů (scoped klíč bez `user_read` → „zůstatek
  nevidím", platnost klíče se pozná z typu 401).
- **Lokální záloha: piper** (`piper-tts` 1.4.2 pip user-space, hlas
  `cs_CZ-jirka-medium` ~60 MB v models_dir, `say --download-model`; sdílený
  atomický downloader `util::download` s whisperem). Proč piper: neuronová
  kvalita na CPU (~1,5 s na větu, RTF « 1), jediný slušný český hlas mezi
  lokálními TTS, subprocess pattern. Speed se mapuje na `--length-scale`
  (= 1/speed); text na jeden řádek (piper bere řádek = promluva); stderr
  se ukazuje jen při chybě (onnxruntime spamuje GPU warningy). `--voice`
  vynucuje ElevenLabs bez fallbacku (explicitní A/B záměr), `--local`
  vynucuje piper. Známý trade-off: v "auto" režimu se při ležícím API
  platí ~1–3 s na neúspěšný pokus ElevenLabs při každé necachované frázi
  (žádný circuit breaker — stav se nedrží, oživne hned s kredity).
- **Stav při zapojení (2026-07-17)**: klíč platný, ale účet má **0 kreditů
  z kvóty 159 644** → ElevenLabs cesta e2e ověřena po quota chybu (reálný
  401 s českou nápovědou); **fallback ověřen ostře**: auto režim při 0
  kreditech přepnul a promluvil piperem (první reálná řeč Jarvise), cache
  hit bez re-syntézy, `--local` funguje. ElevenLabs syntéza proběhne po
  obnově kvóty: `jarvis say "Dobrý večer, pane."`.
- **Soukromí**: mluvený text odchází do ElevenLabs — do `say` nepatří nic
  citlivého; audio cache zůstává lokální (0700 adresář). Piper větev nic
  neposílá ven (plně offline hlas: `engine = "piper"`).

### 3.9 Hlasový dialog (`converse`) — hlasový kanál, krok 3

Cíl: „Jarvisi, …“ → odpověď nahlas. Spojuje poslech (§3.7), Claude pipeline
a hlas (§3.8) do dialogu.

**Tok** (worker ve vlákně listen démona, STT nikdy neblokuje):

```
utterance (whisper) → wake-word filtr (\b<kmen>, default vokativ „jarvisi/jarvise")
  → fronta (4) → worker: echo-guard (okno vlastní řeči ±1 s) → rozpočet?
  → ack „Ano, pane?" (cache, okamžité) → claude -p (haiku, max_turns 1,
    kontext: čas + aktivní okno + poslední 3 výměny z `conversations`)
  → normalizace pro řeč (1 řádek, ořez na speak.max_chars) → speak (say_once,
    soubor se po přehrání maže) → costs (component converse) + conversations
```

Rozhodnutí a proč (2026-07-17):
- **Wake word = vokativ** („jarvisi", „jarvise") + **fuzzy matching**:
  normalizace (lowercase, bez diakritiky a mezer) a tolerance 1 editační
  chyby (`wake_fuzzy`, default zap.) — whisper jméno komolil („Javi si").
  Druhá půlka řešení je `listen.hint` (whisper initial prompt se jménem
  „Jarvisi") — po nasazení přepisuje jméno správně i ze syntetického
  hlasu přes reproduktory. Trade-off fuzzy: občas chytne i skloňované
  „jarvis" v běžné řeči (vypnutelné).
- **Jarvis neslyší sám sebe — AEC s far-end referencí**: echo-cancel modul
  má pojmenovaný sink (`sink_name=jarvis_out` v default.pa) a `speak.sink`
  směruje VŠECHNU Jarvisovu řeč skrz něj (env `PULSE_SINK` na přehrávači;
  neexistující sink → warn + výchozí výstup, PULSE_SINK jinak tvrdě selže).
  Ověřeno kontrolovaným experimentem 2026-07-17: kontrolní fráze přes
  výchozí sink se přepsala, totéž oslovení přes jarvis_out z přepisu
  úplně zmizelo (a okolní zvuk prošel). Druhá obrana: **echo-guard
  časovým oknem** — promluvy začínající před koncem vlastní řeči +1 s
  se ve workeru zahazují.
- **Rozpočet sdílený s analýzou** (`analysis.daily_budget_usd`, součet
  z `costs`); po vyčerpání odpovídá fixní lokální frází bez Clauda.
  `respect_budget = false` blokaci vypne (útrata se dál jen eviduje) —
  Daniel má vypnuto (2026-07-17).
- **`jarvis converse "…" [--mute]`** = stejná výměna textem (test, skript);
  nevyžaduje wake word ani zapnutý converse.
- **Warm mozek** (`converse.warm`, default zap.): rezidentní `claude -p
  --input-format stream-json --output-format stream-json --verbose` proces
  ve workeru — otázka = JSONL řádek na stdin, odpověď = `result` event.
  Empiricky: cold spawn 4,1 s (zahřátá OS cache; 15–19 s jen úplně první
  běh dne), warm otázka 2,2 s (haiku, čisté API); e2e wake→odpověď 6,7 s
  vč. acku (sonnet). Držený stdin u plain `-p` NEJDE (3s stdin timeout);
  stream-json timeout nemá (proces přežil 80 s idle před 1. otázkou).
  `--max-turns 1` platí per zpráva. Session akumuluje kontext (= paměť
  zdarma, ale rostoucí input tokeny) → recyklace po `warm_max_exchanges`
  (10) nebo `warm_idle_s` (15 min). Jakákoli chyba → proces zahodit +
  cold fallback; POZOR na SIGPIPE: main.rs dává démonům (`listen`/`run`)
  SIG_IGN, jinak by zápis do mrtvého warm procesu zabil celý démon
  (CLI příkazy nechávají DFL kvůli `jarvis status | head`).
- **Hint echo-guard**: whisper na hlasité hudbě halucinoval text
  `listen.hint` do přepisů („…slyšíš? Jarvis odpovídá.") a 2× falešně
  spustil placenou konverzaci → hint přepsán na slovníkový styl
  („Slovník: Jarvis, Jarvisi, …" — nezní jako oslovení) a wake ignoruje
  promluvy sdílející s hintem souvislý úsek ≥ 10 znaků (jméno má 7 →
  reálné oslovení guard nikdy netrefí).
- **Ověřeno ostře**: `converse` CLI → skutečný haiku (0,014 USD/výměna)
  → česká odpověď v persóně (vykání, „pane", suchý humor) → piper nahlas;
  kontextová paměť potvrzena navazující otázkou; worker + wake-word +
  fronta + rozpočet pokryté unit testy (87 testů zeleno).

**Open-ear — odpovídání bez wake-wordu (2026-07-18, `converse.open_ear`, default „off"):**

Cíl: umět odpovědět i bez „Jarvisi…", ale neskákat do cizí konverzace. Je to
addressee detection — jeden stolní mikrofon principiálně neví, na koho mluvíš,
takže řešení je **vrstvená brána s biasem do ticha** (false-accept = skočení do
řeči je dražší chyba než false-reject = občas neodpovím). Wake-word zůstává 100%
override vždy a nezávisle na open_ear.

- **Soukromí/cena beze změny na STT**: každá promluva se přepisuje a ukládá do
  `utterances` už dnes (kvůli digestu). Open-ear mění jen KDY Jarvis promluví;
  přidává Claude volání na podmnožinu promluv, ne nový tok do STT.
- **Triage v STT vlákně** (`converse::triage`, čistá a testovaná), sémantika ve
  workeru. Tři vrstvy:
  - *Tier 0 — wake*: „Jarvisi…" → vždy odpoví (beze změny).
  - *Tier 1 — „followup"*: po Jarvisově odpovědi drží `followup_window_s` (12 s)
    okno, kdy navazující promluva jméno nepotřebuje. Nejbezpečnější (právě jsi
    mluvil na Jarvise); okno se obnoví každou odpovědí = víceotáčkový dialog.
    `speech_end` je **sdílený atomik** worker↔STT vlákno — slouží zároveň jako
    echo-guard i práh okna. Filtr `open_ear_min_words` zahodí „ehm/jo".
  - *Tier 2 — „always" (experiment)*: promluvu mimo okno posoudí **skeptický
    klasifikátor** (haiku, ANO/NE, bias do NE; náklad → costs „converse-gate").
    Volá se JEN na kandidáty po levných lokálních filtrech (hint-echo guard,
    min_words, ne echo vlastní řeči) — nikdy na každou promluvu, jinak
    rozpočtový oheň. Při vyčerpaném rozpočtu kandidát mlčí (neoznamuje „rozpočet
    vyčerpán" — nebyl osloven).
- **Kill-gate (empirická brána Tier 2)** — `jarvis converse-eval <jsonl>`:
  olabelovaný korpus reálných promluv (directed | human | background) →
  confusion matrix + recall + **false-accept rate**. Šablona z reálné DB:
  `--from-db N`. „always" se zapíná, až když je false-accept hodně nízko
  (cíl < 2–3 %), jinak recall nestojí za riziko skákání do řeči. Tier 1 gate
  nepotřebuje (nemá klasifikátor) — kryjí ho unit testy + ostrý běh.
- **Meze**: jeden mikrofon = žádný prostorový signál adresace, chyby jsou
  neredukovatelné. Diarizace přes VAD segmenty nespolehlivá (Scribe ID mluvčích
  nejsou stabilní napříč voláními). Follow-up může minout, když se hned po
  odpovědi otočíš na člověka (proto krátké okno). Default „off" = dnešní chování.

### 3.10 Ovládání oken (`jarvis wm`) — ruce Jarvise

Cíl: Jarvis umí hýbat s prostředím — okna, klávesnice, myš, screenshoty. Základ
pro budoucí automatizace (fáze C/D) a pro hlasové povely typu „Jarvisi, otevři
Signal a napiš Tomášovi…".

**Vrstvy** (`src/wm.rs`, sdílené X11 helpery v `src/x11util.rs`):

```
jarvis wm CLI: list | active | focus | close | minimize | maximize | fullscreen
               | move | resize | wait | spawn | type | key | click | pointer | screenshot
  ├─ okna: EWMH client messages (_NET_ACTIVE_WINDOW se source=2, _NET_CLOSE_WINDOW,
  │   _NET_WM_STATE, WM_CHANGE_STATE) + ConfigureWindow; výběr okna: přesná třída
  │   > podřetězec třídy > podřetězec titulku, remízy řeší stacking (topmost);
  │   focus čeká na read-back _NET_ACTIVE_WINDOW (fail ≠ tichý úspěch)
  ├─ klávesnice: XTest fake keys; znak → keysym (Latin-1 přímo, jinak
  │   0x0100_0000+codepoint); keysym mimo aktuální layout → dočasné zapůjčení
  │   volného keycodu přes ChangeKeyboardMapping + obnova (trik xdotool)
  │   → plná česká diakritika nezávisle na rozložení
  ├─ myš: XTest motion (absolutně) + buttony (levé/prostřední/pravé, dvojklik)
  └─ screenshot: GetImage root (celek) / crop dle geometrie okna, JPEG q90,
      konverze pixelů sdílená s capture
```

Rozhodnutí a proč (2026-07-17):
- **Nativní Rust místo xdotool/wmctrl subprocess** — žádná runtime závislost,
  všechna primitiva vracejí read-backy, jeden kód pro CLI i agenta; x11rb už
  v projektu je (jen +feature `xtest`).
- **Converse agent dostal ruce**: s `[wm] enabled` má konverzační claude
  `--allowed-tools "Read,Bash(jarvis wm:*)"` (prefixový vzor — nic jiného než
  `jarvis wm` spustit nejde) a `converse.max_turns` (12) kol na smyčku
  akce → screenshot → Read (vision) → další krok. Prompt: seznam příkazů,
  povinné ověření cílového okna před psaním, stop při nejednoznačném cíli,
  na závěr shrnutí jednou větou. Bez `[wm]` zůstává Read + 1 kolo jako dřív.
- **Bezpečnost**: type/key jdou do fokusovaného okna → `type --window` nejdřív
  aktivuje s ověřením; `[wm] enabled=false` hlasovou větev vypne (CLI zůstává,
  to spouští člověk). Trade-off vědomě přijatý: agent s klávesnicí je mocný —
  drží ho prompt + omezený Bash vzor + budget.
- **`wm spawn` (2026-07-17)**: aplikace, která neběží, se dá spustit —
  odpojeně (vlastní process group, výstup do spawn.log), detekce úspěchu
  = NOVÉ okno proti snímku před startem (`--window dotaz` pro single-instance
  aplikace, které předávají běžící instanci), aktivace best-effort (fullscreen
  okna umí fokus odmítnout — není to chyba spawnu). Mimo TTY (agent, timery)
  smí jen programy z `wm.spawn_allowed` (přesná shoda, holé jméno = PATH;
  žádné basename triky s podvrženou cestou); z terminálu bez omezení.
- **Ověřeno ostře (2026-07-17)**: gedit — česká věta s plnou diakritikou
  napsána znak-přesně (52 znaků, stavový řádek „Sl. 53"); dialog neuloženého
  dokumentu sestřelen klikem dle screenshotu; **Signal Desktop — otevřen 1:1
  chat Tomáš Messing (klik v sidebaru dle snímku), zpráva napsána, vizuální
  gate před Enterem (text v poli + správná hlavička), odesláno, read-back
  screenshot potvrdil bublinu s fajfkou**. Unit testy: keysymy, komba, výběr
  oken, ořez, gating promptu (102 testů zeleno).

Známé meze: čistě X11 (Wayland mimo rozsah, viz §10); klik podle souřadnic ze
screenshotu předpokládá, že se scéna mezitím nezměnila (agent má re-shootovat
při pochybnostech); AltGr znaky mimo core mapu jdou remapem (fungují, jen
o ~30 ms pomaleji na znak); typing do her/VM s grabem klávesnice negarantován.

### 3.11 SMS kanál (`jarvis sms`) — Twilio

Cíl: Jarvis umí poslat SMS („Jarvisi, až doběhne X, hoď mi SMS") — notifikační
kanál nezávislý na počítači, doplněk e-mailu a hlasu.

**Tok**: `jarvis sms "text" [--to +420…] [--no-wait]` → Twilio Messages API
(form-urlencoded POST, Basic auth vlastním base64 — žádná nová závislost;
retry 0/2/8 s na 429/5xx/transport) → **poll doručenky** (`GET Messages/{sid}`
à 2 s do `delivered`/timeout 30 s; `failed`/`undelivered` = chyba s Twilio
kódem a nápovědou) → útrata do `costs` (component `sms`, tokens_in = znaky,
usd = cena z API, chodí se zpožděním).

Rozhodnutí a proč (2026-07-17):
- **Odesílatel `MG…` Messaging Service** (`sms.from`): klient pozná MG SID
  a pošle `MessagingServiceSid` místo `From`; umí i E.164 číslo a alfanumerický
  sender (validace v configu). Služba „Olvano e-sign" má v poolu jen alpha
  sender **Olvano** → SMS jsou jednosměrné (nejde odpovědět).
- **Klíče ze stargate účtu** (`TWILIO_ACCOUNT_SID`/`TWILIO_AUTH_TOKEN`
  v secrets.env, zkopírováno z ~/Projects/stargate/.env); výchozí příjemce
  `sms.to = +420733606016` (verified caller ID účtu = Daniel).
- **Converse agent smí SMS**: s `[sms] enabled` přibude `Bash(jarvis sms:*)`
  do allowed-tools; prompt: default jde pánovi, cizímu číslu JEN s výslovně
  nadiktovaným `--to`. `enabled=false` kanál i agentní větev vypne.
- Pojistky: `sms.max_chars` (default 480 ≈ 3 segmenty), E.164 validace,
  doctor kontroluje klíče a `--live` zůstatek účtu (Balance API).
- **Stav ověření (2026-07-17): kód e2e hotový, ale účet zatím žádnou SMS
  nikdy nedoručil** — historie Messages je 100% failed (i červnové pokusy
  stargate: 21703/21612/21701 napříč alpha/US číslem/službou; účet je Full,
  balance 18,6 USD). Diagnóza: **SMS Geographic Permissions pro ČR vypnuté**
  — nastavuje se JEN v konzoli (Messaging → Settings → Geo permissions),
  API to neumí. Po zapnutí: `jarvis sms "test"` musí skončit `sent`/
  `delivered` (kill-gate zůstává otevřený, klient hlásí kódy s nápovědou).

Známé meze: alpha sender = jednosměrka (odpověď nedorazí); cena se do
`costs` propíše jen když ji Twilio stihne vrátit během poll okna; geo
permissions bez API = ruční krok v konzoli.

### 3.4 Konfigurace a tajemství

`~/.config/jarvis/config.toml` (vzor bude v repu jako `config.example.toml`):

```toml
[capture]
meta_interval_s = 10
shot_interval_s = 60
idle_threshold_s = 120
blacklist_class = ["(?i)keepass", "(?i)bitwarden"]
blacklist_title = ["(?i)anonymní", "(?i)incognito", "(?i)private browsing", "(?i)bank"]

[analysis]
cadence = "hourly"
max_images_per_run = 8
model = "claude-haiku-4-5-20251001"
daily_budget_usd = 1.0
send_images = true          # false = titulky-only režim (nic vizuálního neopouští stroj)

[digest]
hour = 19                    # Europe/Prague
model = ""                  # prázdné = default model claude CLI

[email]
to = "dankrul.krul@gmail.com"
from = "dankrul.krul@gmail.com"   # musí být verifikovaný v SendGrid
from_name = "Jarvis"

[retention]
screenshots_days = 7
summaries_days = 0           # 0 = navždy (text je maličký)
```

Tajemství **výhradně** v `~/.config/jarvis/secrets.env` (0600, adresář 0700):
`SENDGRID_API_KEY=…`, `ELEVENLABS_API_KEY=…`. Načítá se jako env (systemd
`EnvironmentFile=%h/.config/jarvis/secrets.env`, při ručním spuštění si je binárka
načte sama). V repu nikdy; `.gitignore` pro jistotu dostane `*.env`, `secrets*`.

### 3.5 CLI

| Příkaz | Účel |
|---|---|
| `jarvis capture` | démon — snímání (foreground, loguje přes `tracing`) |
| `jarvis listen [--print-only\|--wav F\|--download-model]` | démon — poslech mikrofonu (near-realtime STT) |
| `jarvis analyze [--dry-run]` | hodinová extrakce (dry-run vypíše prompt a vybrané snímky, nevolá API) |
| `jarvis digest [--date D] [--send\|--dry-run]` | složí digest; `--dry-run` uloží HTML do souboru k náhledu |
| `jarvis send-test` | pošle testovací e-mail (ověření SendGrid setupu) |
| `jarvis pause 30m` / `resume` | soukromí — dočasné vypnutí snímání |
| `jarvis status` | stav: poslední snímek, počty, dnešní útrata, fronta |
| `jarvis doctor` | prereq check: DISPLAY, claude CLI + auth, API key, verifikace odesílatele (test call), místo na disku |
| `jarvis purge [--older-than 7d]` | ruční retence |
| `jarvis install-units` | vygeneruje a nainstaluje systemd user units |
| `jarvis wm <akce>` | ovládání oken/klávesnice/myši + screenshot (EWMH/XTest) — ruce hlasového agenta |
| `jarvis sms "text" [--to +420…]` | SMS přes Twilio s čekáním na doručenku; smí i hlasový agent |

### 3.6 systemd user units (`systemd/` v repu, instaluje `install-units`)

- `jarvis-capture.service` — `ExecStart=%h/.cargo/bin/jarvis capture`, `Restart=always`,
  `Environment=DISPLAY=:0` (+ poznámka: po loginu `systemctl --user import-environment DISPLAY XAUTHORITY`).
- `jarvis-analyze.timer` — `OnCalendar=hourly`, `Persistent=true` → `jarvis analyze`.
- `jarvis-digest.timer` — `OnCalendar=*-*-* 19:00`, `Persistent=true` → `jarvis digest --send`.
- Všechny services: `EnvironmentFile=%h/.config/jarvis/secrets.env`.

Fallback bez systemd: `jarvis run` — vše v jednom procesu s interním plánovačem.

---

## 4. Datový model (SQLite, `rusqlite` bundled, migrace v kódu)

```sql
samples(id, ts, wm_class, title, desktop, idle_ms,
        shot_path NULL, phash NULL)                  -- 1 řádek / 10 s vzorek
utterances(id, ts_start, ts_end, text, lang, conf,   -- přepisy řeči (poslech);
        source)                                      -- audio se nikdy neukládá
hourly_summaries(id, period_start, period_end, json, -- výstup analyze (JSON kontrakt)
        model, cost_usd, degraded BOOL)
daily_digests(id, date, markdown, html, status,      -- pending|sent|failed
        sendgrid_msg_id, sent_at)
patterns(id, key, description, evidence, occurrences,-- detekované rutiny (fáze B+)
        first_seen, last_seen,
        status)                                      -- candidate|proposed|approved|automated|dismissed
proposals(id, pattern_id, kind, path, created_at)    -- vygenerované automatizace (skript/skill/timer)
runbooks(id, proposal_id UNIQUE, pattern_id, name,   -- schválené automatizace (fáze D)
        schedule,                                    -- manual | daily@HH:MM
        enabled, approved_at, approved_via)          -- cli | telegram
runbook_runs(id, runbook_id, started_at,             -- každý běh; finished_at NULL = běží
        finished_at, exit_code,                      -- exit NULL = timeout/kill
        trigger,                                     -- timer | cli | voice
        output)                                      -- ořezaný stdout+stderr
costs(id, ts, component, model, tokens_in, tokens_out, usd)
state(key, value)                                    -- watermarky, pause deadline, telegram_offset
```

Retence: denně smazat `shots/` starší než `retention.screenshots_days` + odpovídající
`samples.shot_path` → NULL (metadata zůstávají). DB zůstává malá (text).

---

## 5. Soukromí a bezpečnost

- Data jen lokálně (`~/.local/share/jarvis`, 0700); jediné, co opouští stroj, jsou
  zmenšené snímky do Anthropic API při `analyze` (vypnutelné `send_images=false`)
  a finální digest e-mailem.
- Blacklist tříd/titulků oken (hesla, banking, anonymní okna) — snímek se **vůbec nepořídí**.
- `jarvis pause` pro schůzky/citlivou práci; idle detekce brání snímání zamčené/opuštěné session.
- API klíč: mimo repo, 0600. **Doporučení: klíč byl vložen do chatu v plaintextu —
  po zprovoznění ho v SendGrid zrotovat** a nový uložit jen do `secrets.env`.
- Žádná telemetrie, žádné third-party služby kromě Anthropic + SendGrid.

---

## 6. Náklady (odhad)

Vision: ~(š×v)/750 tokenů → 1568×882 ≈ 1,8k tok/snímek. 8 snímků/h × 14 h ≈ 200k
input tok/den na Haiku ≈ **~0,25 USD/den**; digest (Sonnet, text-only) ≈ centy.
Pokud je `claude` přihlášen přes subscription, jde to z plánu. Pojistka: denní strop
v configu + evidence v `costs` + řádek „dnešní útrata" v digestu a `jarvis status`.

---

## 7. Struktura repa a závislosti

```
jarvis/
├── Cargo.toml
├── PLAN.md                  # tento dokument
├── config.example.toml
├── .gitignore               # target/, *.env, secrets*
├── systemd/*.{service,timer}
├── templates/email.html     # HTML šablona digestu (inline CSS)
└── src/
    ├── main.rs              # clap subcommandy, init tracing
    ├── config.rs            # TOML + secrets.env + validace
    ├── capture/{mod,x11,dedup}.rs
    ├── store/{mod,db,retention}.rs
    ├── pipeline/{mod,segment,select,claude,analyze}.rs   # claude.rs = wrapper na `claude -p`
    ├── digest/{mod,build,render}.rs
    ├── mail/sendgrid.rs
    ├── wm.rs                # ovládání oken/vstupu (EWMH + XTest) — `jarvis wm`, ruce agenta
    ├── x11util.rs           # sdílené X11 helpery (properties, ZPixmap→RGB, JPEG)
    ├── sms.rs               # SMS přes Twilio (Messages API, poll doručenky, vlastní base64)
    ├── patterns.rs          # fáze B+C: detekce vzorů + generování návrhů
    ├── runbook.rs           # fáze D: schvalování a exekuce runbooků (timeout, flock, runy v DB)
    └── telegram.rs          # schvalování na dálku (bot getUpdates poll, ověřený chat)
```

Závislosti: `clap` (derive), `tokio` (rt, process, time), `xcap`, `x11rb`
(+ screensaver feature), `image`, `webp`, `img_hash`, `rusqlite` (bundled), `reqwest`
(rustls-tls, json), `serde`/`serde_json`, `toml`, `directories`, `chrono`+`chrono-tz`,
`tracing`+`tracing-subscriber`, `anyhow`+`thiserror`, `pulldown-cmark`, `regex`.

---

## 8. Milníky

| M | Obsah | Akceptace / ověření |
|---|---|---|
| **M0** | Scaffold: cargo init, clap skelet, config, tracing, DB migrace, `doctor` | `cargo test` zelené; `jarvis doctor` reportuje reálný stav prostředí |
| **M1** | Capture: metadata + screenshoty, idle, dedup, blacklist, pause, retence | 10 min běhu → v DB správné tituly, dedup drží (statická obrazovka = 1 snímek), KeePass okno se nesnímá |
| **M2** | Pipeline: segmentace, výběr snímků, `claude -p` wrapper, JSON kontrakt, budget guard | `jarvis analyze --dry-run` ukáže smysluplný výběr; ostrý běh uloží validní souhrn + cost; nevalidní JSON → degradovaný fallback |
| **M3** | Digest + e-mail: agregace, generování, HTML render, SendGrid, retry | `jarvis digest --dry-run` → pěkné HTML v souboru; `send-test` → 202 a mail v inboxu; ostrý digest dorazí |
| **M4** | Provoz: systemd units, `install-units`, `status`, recovery (X reconnect, persistent timery), README | Přežije reboot: capture autostart, timery tikají, digest přijde bez zásahu |
| **M5** | Automatizace — viz §9 | první automation_hints v digestu → první schválený proposal běží |

Ověřování průběžně: unit testy na dedup/segmentaci/výběr snímků/render; integrační
dry-runy; ostré e2e na konci M3 (celý den nasnímat → večer přijde mail).
Odhad: M0–M1 ~1 den, M2 ~1 den, M3 ~0,5 dne, M4 ~0,5 dne.

---

## 9. Cesta k automatizaci práce (north star)

- **A — Pozorování (M1–M3)**: spolehlivá časová osa dne + denní report. Základ: bez
  dobrých dat nejde nic automatizovat.
- **B — Detekce vzorů (M5)**: nad `hourly_summaries.automation_hints` + deterministickou
  timeline detekovat opakované rutiny (stejná sekvence aplikací/úloh ≥3× týdně, ruční
  přenosy dat, opakované dotazy). Ukládat do `patterns`, v digestu sekce „Automatizační
  příležitosti" s odhadem ušetřeného času.
- **C — Návrhy artefaktů** *(hotové)*: pro vzory Jarvis přes `claude -p` vygeneruje
  automatizaci — shell skript, systemd timer, Claude Code skill — do
  `~/.local/share/jarvis/proposals/` (`jarvis propose`), odkaz v digestu; nový návrh
  se ohlašuje na Telegram/SMS (dle configu). Instalace/spouštění = moje schválení.
- **D — Řízená exekuce** *(hotové 2026-07-17)*: `jarvis runbook approve <návrh>` udělá
  z návrhu runbook (approve chce TTY — agent ani timer schválit nemůžou; na dálku
  z ověřeného Telegram chatu: „schval N“ / „zamítni N“, bot návrhy ohlašuje sám).
  Spouštění: ručně/hlasem (`jarvis runbook run`, hlasový agent má povolené jen
  run/list/show — nikdy approve) a plánovaně `schedule daily@HH:MM` přes
  `jarvis-runbooks.timer` (à 5 min, `runbook run-due`; `jarvis run` má stejnou otočku).
  Každý běh: flock proti souběhu, tvrdý timeout (SIGKILL process group), výstup
  ořezaný do DB (`runbook_runs`) → sekce „Automatizace (runbooky)“ v digestu,
  `jarvis runbook runs`, `jarvis status`. Bez schválení neběží nic.

---

## 10. Rizika a otevřené body (zvolené defaulty, lze změnit)

- **Wayland**: mimo rozsah; při přechodu na Wayland přestane fungovat capture (řešení
  přes xdg-desktop-portal až bude potřeba).
- **Headless `claude` v timeru**: používá uložený login — `doctor` to ověřuje; při
  vypršení auth pipeline degraduje na titulky-only a digest to ohlásí.
- **SendGrid**: nutná jednorázová verifikace odesílatele (jinak 403); klíč zrotovat.
- **Kvalita vision extrakce**: malý text může být nečitelný → titulková timeline je
  vždy záchranná síť; prompt ladit až nad reálnými daty (M2).
- Defaulty: snímek à 60 s · digest 19:00 Europe/Prague · retence snímků 7 dní ·
  Haiku na hodinovku, default model na digest · 1 monitor (multi-monitor: `xcap`
  umí všechny, zapneme až bude víc než HDMI-0).
- **Poslech**: energy-VAD neumí odlišit řeč od trvalého hluku (řeší až whisper
  no-speech filtr → zbytečné CPU) — upgrade na Silero VAD až to bude reálně
  vadit. Reproduktory nejsou odposlouchávané (jen mikrofon; schůzky v
  sluchátkách slyší jen moji stranu — monitor source je případné rozšíření).
  Build vyžaduje user-space cmake+libclang (pip), viz README/`.cargo/config.toml`.
- **Runbooky/Telegram**: schvalovací bot je vlastní (@BotFather); token + chat id
  v secrets.env (`TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`), poslouchá se výhradně
  nakonfigurovaný chat. Poll jede v 5min timeru → schválení na dálku má latenci
  do 5 minut. Skripty runbooků běží s právy uživatele bez sandboxu — proto je
  brána na schválení (přečti si artefakt: `jarvis runbook show/pending`).
- **Hlas (TTS)**: účet ElevenLabs měl při zapojení 0 kreditů → finální doladění
  hlasu (poslechový A/B test George vs. hlas z Voice Library) čeká na obnovu
  kvóty; do té doby mluví lokální piper záloha (funkční, ověřeno ostře).
  Klíč je scoped na `text_to_speech` — správa hlasů jen přes web;
  klíč byl vložen do chatu v plaintextu → **po zprovoznění zrotovat** (stejně
  jako SendGrid, viz §5). Digest ohláška mluví na stroji s audio výstupem
  (obě cesty: systemd timer i `jarvis run`).

---

## 11. Sebe-vývoj (`jarvis improve`) — Jarvis vyvíjí a zlepšuje vlastní kód

North star dotažený na vlastní zdroják: automatizaci (§9 C/D) rozšiřujeme až na
Jarvisův VLASTNÍ kód. Motor už existuje — `pipeline::claude::run` je headless
Claude Code (`cwd` + `allowed_tools` jsou parametry; dnes míří na `data_dir`
s read-only nástroji). Sebe-vývoj = otočit `cwd` na kořen repa, rozšířit
nástroje o editaci a obalit to STEJNÝMI branami, jaké drží runbooky a nudge.
Každé vylepšení = **jedna git větev + commit pod strojovou identitou** → auditní
stopa v gitu, přesně jak má být.

### 11.1 Životní cyklus (state machine)

```
queued   ── zadání „Jarvisi, nauč se X" / self-source (padající testy, clippy)
  ▼ draft
drafting ── větev jarvis/improve/<id>-<slug> z main (NIKDY v main)
            claude píše kód [cwd=repo, tools=Read,Edit,Write,Bash(cargo:*); git NE]
            Jarvis commitne pod „Jarvis <jarvis@localhost>" + trailer Jarvis-Improvement:<id>
  ▼ test   ❱ FAILABLE BRÁNA: cargo build + cargo test na větvi ❰ + test-integrity guard
tested   ── zeleno; červená / oslabené testy → failed
  ▼ propose (zapíchne sha256 diffu; klasifikace obálky)
proposed ── ohlášeno (digest / Telegram / hlas)
  ▼ approve  ⛔ confirm_at_keyboard (TTY-token) / ověřený Telegram „ano <id>"
approved ── re-verifikace zapíchnutého sha256 (TOCTOU) + base_commit == HEAD
  ▼ merge
merged   ── merge do main s trailerem (kód přistál v gitu)
  ▼ deploy  ⛔ deploy_enabled; cargo install → smoke (--version/doctor/self-test)
deployed ──   → swap binárky (.prev záloha) → systemctl restart; smoke FAIL → rolled_back
```

### 11.2 Bezpečnostní invariant

improve NIKDY sám nemergne ani nenasadí nic neschváleného. Autonomně smí jen
(a) zapsat změnu na IZOLOVANOU větev (nikdy main, nikdy živá binárka) — plně
vratné; (b) spustit tam testy. Merge do main = tvůj souhlas (TTY-token stejně
jako runbook approve, nebo ověřený Telegram s číslem). Diff je zapíchnutý
sha256 a před mergem se TOCTOU-bezpečně ověří (model `runbook::run_one`).
Rebuild + restart živé binárky je další, zvlášť hradlovaný krok
(`deploy_enabled`) se smoke-testem a automatickým rollbackem. Ship dark
(`enabled=false`).

Dvě strukturální pojistky (zelené testy nestačí — dají se ošidit):
- **test-integrity** (`improve::test_integrity_ok`): počet `#[test]` na větvi
  nesmí klesnout — testy se jen přidávají, nikdy neředí kvůli zelené.
- **gate-critical eskalace** (`improve::classify_envelope` → `gate_critical`):
  diff sahající na bezpečnostně-kritické soubory (config.rs, runbook.rs,
  improve.rs, units.rs, main.rs, telegram.rs, kill.rs, Cargo.*,
  .cargo/config.toml, .github/) jde VŽDY na ruční review, i pod auto-merge.

### 11.3 Obálka autonomie (volba 2026-07-20: cíl C)

- **A** — návrh + testy, merge schvaluješ ty (základ, „nic neschváleného").
- **B** — + auto-merge bezpečné třídy (jen docs; `auto_merge_safe`).
- **C** — plná smyčka vč. self-deploy (rebuild+smoke+swap+restart; `deploy_enabled`).

Vše ship-dark; C je cíl, ale každá nebezpečná hrana je za flagem `false`,
jištěná failable bránami (cargo test → smoke-test → rollback).

### 11.4 Datový model (DB v13)

`improvements(id, created_at, updated_at, source, title, spec, branch,
base_commit, head_commit, status, envelope, diff_stat, diff_sha256,
tests_passed, test_output, cost_usd, tokens_in, tokens_out, approved_at,
approved_via, merged_at, deployed_at, note)`. Git = pravda o KÓDU
(větev/commit); tabulka = state machine/index nad ním. `diff_sha256` =
pin-and-verify jako u runbook artefaktů.

### 11.5 CLI (`jarvis improve <sub>`)

| Příkaz | Účel | Fáze |
|---|---|---|
| `queue "…" [--source S]` | zařaď zadané vylepšení do ledgeru | ✅ 1 |
| `list` / `show <id>` / `dismiss <id>` | ledger + detail + zahození | ✅ 1 |
| `tick --dry-run` | náhled: config + ledger + repo, bez API/zápisu | ✅ 1 |
| `draft <id> [--dry-run]` | větev + codegen + commit | 2 |
| `test <id>` | failable brána build+test na větvi | 3 |
| `propose <id>` / `approve <id>` | zapíchnutí sha256 + TTY/Telegram merge | 4 |
| `tick` | automatická smyčka (calm, ≤1 akce/tik) | 5 |
| `deploy <id>` | rebuild + smoke + swap (.prev) + restart | 6 |

Konfigurace `[improve]` (vzor v `config.example.toml`), ship-dark
`enabled=false`, `deploy_enabled=false`, `allow_self_source=false`,
`auto_merge_safe=false`.

### 11.6 Stav a plán fází (2026-07-20)

- **Fáze 1 — substrát: HOTOVO a ověřeno.** Config `[improve]` (podle vzoru
  `ProactiveCfg`, ship dark), migrace DB **v13** + accessory (podle `NudgeRow`),
  `src/improve.rs` (čistá logika + brány), CLI queue/list/show/dismiss/
  tick-dry-run, `util::slugify`, `confirm_at_keyboard` povýšeno na `pub(crate)`
  ke sdílení. Ověření: `cargo test` **269 zelených** (+9 nových); ostrý
  read-back binárky proti IZOLOVANÉ DB (queue→list→show→dismiss→dry-run,
  `user_version=13`). Ship dark — nic se nespustí.
- **Fáze 2 — draft engine: HOTOVO a ověřeno.** `run_capture` (proces-group
  SIGKILL timeout jako `run_one`) + `git()` helper, izolovaný `git worktree`
  z committed main (NIKDY dirty tree), codegen `claude::run` (cwd=worktree,
  tools Read/Edit/Write/cargo — bez git), commit pod strojovou identitou +
  trailer, `draft --dry-run`, `test <id>`, flock proti souběhu. Ověřeno: 275
  testů + hermetický git-roundtrip test + ostrý git-worktree smoke (izolace od
  dirty tree, machine-identity commit). **První ostrý (placený) codegen
  spouštíš ty** (`enabled=false`).
- **Fáze 3 — failable brána: JÁDRO HOTOVO.** `cargo test` na větvi (sdílený
  target = teplý cache) + test-integrity guard (počet testů nesmí klesnout) +
  envelope klasifikace. Zbývá: omezená self-repair smyčka (retry při červené do
  `repair_attempts`) + jemnější assertion-level integrita.
- **Fáze 4 — propose→approve→merge: HOTOVO a ověřeno.** `propose` zapíchne
  sha256 diffu + klasifikuje obálku; `approve` re-verifikuje no-drift
  (base==main HEAD) + TOCTOU (otisk diffu) + čistý main tree, pak
  `confirm_at_keyboard` (TTY-token) → `merge --ff-only` (Jarvisův commit přistane
  na main) → cleanup worktree+větve. Ověřeno ostře proti reálnému repu (bez
  útraty, bez TTY): propose pin + VŠECHNY brány zamítly (dirty-tree, TOCTOU
  tamper, drift tamper, status), main HEAD nedotčen. Telegram approve (číslo
  z ověřeného chatu) je součást fáze 5.
- **Fáze 5 — reporting + plán:** sekce „Sebe-vývoj" v digestu (u `build.rs:189`),
  `improve tick` (≤1 akce/tik, jako nudge), volitelný `jarvis-improve.timer`.
- **Fáze 6 — self-deploy:** `cargo install` → smoke (`--version`/`doctor`/
  self-test) → swap `~/.cargo/bin/jarvis` (.prev záloha) → `systemctl --user
  restart` → post-deploy `doctor`; smoke/health FAIL → rollback `.prev`.
  POZOR: `units.rs` bere `current_exe()` a odmítá `/target/` build → deploy
  musí instalovat do `~/.cargo/bin` a teprve pak `install-units`.
  `enabled`/`deploy_enabled` zůstávají `false` do ověření.

**Ověřeno naživo (2026-07-20):** ostrý end-to-end draft na reálném úkolu
(`util::mask_secret`) — Claude napsal korektní, otestovaný Rust (char-based,
multibyte-safe, dobrý anglický komentář + test na dlouhý/8-znaků/prázdný/
multibyte vstup), Jarvis commitnul pod strojovou identitou, integrity 276→277,
brána `cargo test` **zeleno**, `propose` zapíchl otisk; reálný main NEDOTČEN,
artefakty uklizeny. Náklad **$1,08 / malý úkol** (nejsilnější model, ~5 min).
Pozn.: izolovaný HOME v testu odřízl rustup default toolchain i claude creds
(obojí žije pod reálným HOME → řešeno `RUSTUP_HOME`/`CARGO_HOME` + kopií creds);
v produkci (démon pod reálným HOME) tenhle artefakt nevzniká. Zbytkový gap:
neviděl jsem agentovu self-repair smyčku (v testu neměl funkční cargo, kód dal
správně napoprvé).

Resume: `src/improve.rs` má dočasný `#![allow(dead_code)]` scaffold (pryč po
fázi 6). Hotovo: **fáze 1, 2, 3-jádro, 4 + live e2e**. Zbývá: fáze 3-rest
(self-repair smyčka), **fáze 5** (`tick` + Telegram approve + digest sekce),
**fáze 6** (self-deploy: rebuild + smoke + swap + rollback + restart).
