# Jarvis

Osobní asistent ve stylu filmového Jarvise: sleduje, na čem na počítači pracuješ
(X11), průběžně to analyzuje přes Claude Code CLI a jednou denně pošle e-mailem
digest s doporučeními. Dlouhodobý cíl: detekovat opakované rutiny a **automatizovat
tvoji práci** (`jarvis propose`). Architektura a rozhodnutí: viz [PLAN.md](PLAN.md).

## Jak to funguje

1. **capture** (démon) — každých 10 s metadata aktivního okna (titulek, třída,
   idle), každých 60 s screenshot (dedup přes dHash, blacklist citlivých oken,
   pauza na povel). Vše lokálně: SQLite + JPEG v `~/.local/share/jarvis`.
1. **listen** (démon) — poslouchá mikrofon a near-realtime přepisuje řeč:
   `parec` (16 kHz mono) → energetický VAD s adaptivním prahem vyřízne
   promluvy → STT → text do tabulky `utterances`, ~1 s po dořečení. STT engine
   volí `listen.engine`: **`auto`** (default) použije ElevenLabs **Scribe**
   (cloud, ~0,22 $/h) a při chybě/bez klíče spadne na lokální whisper.cpp
   (`whisper-rs`, model `large-v3-turbo-q5_0`, CUDA na GTX 1650); v `auto` se
   whisper model načte až při prvním fallbacku (líně), takže dokud Scribe jede,
   těžký model stroj vůbec nezatíží. `elevenlabs` = jen Scribe, `whisper` = jen
   lokálně (nic neopouští počítač; bez GPU `model = "small-q5_1"`). Zvuk se
   nikdy neukládá na disk — ven jde (u Scribe) jen krátký WAV promluvy k přepisu
   a zpět text; u whisperu neopouští audio počítač vůbec.
1. **speak** — Jarvis mluví: ElevenLabs TTS (`eleven_multilingual_v2`, česky,
   hlas laděný do brumbálovského vypravěče) s **lokální zálohou piper**
   (`cs_CZ-jirka-medium`, neuronový TTS na CPU, ~1,5 s syntéza) — když API
   selže (kvóta, síť, klíč), Jarvis neoněmí, přepne se sám (`engine = "auto"`;
   `"piper"` = jen lokálně zdarma). Cache vygenerovaných frází (stejná věta =
   1 kredit jednou), přehrání přes `ffplay`/`mpv`/`paplay`. `jarvis say "…"`
   na povel; po odeslání digestu ho Jarvis sám ohlásí nahlas
   (`announce_digest`). Spotřeba znaků se eviduje v tabulce `costs`.
1. **converse** — hlasový dialog: řekni „Jarvisi, …“ a Jarvis odpoví nahlas.
   Oslovená promluva jde s kontextem (čas, aktivní okno, poslední výměny)
   do Clauda (haiku, sdílí denní rozpočet) a odpověď se přečte přes speak.
   Okamžité „Ano, pane?“ potvrdí oslovení, než se odpověď vymyslí. Textem:
   `jarvis converse "Jarvisi, kolik…"`. Výměny se ukládají do `conversations`.
   Volitelně umí odpovědět i bez jména (`converse.open_ear`): „followup“ = po
   odpovědi krátké okno na navazující otázku, „always“ = skeptický klasifikátor
   posoudí každou větu, jestli mířila na Jarvise (experiment; default „off“ =
   jen na oslovení). Ladí se přes `jarvis converse-eval` (kill-gate).
1. **sms** — Jarvis umí poslat SMS přes Twilio (`jarvis sms "text"`): jde na
   tvoje číslo (`sms.to`), čeká na doručenku a útratu eviduje v `costs`.
   Hlasový agent to smí taky („Jarvisi, pošli mi SMS…"); cizímu číslu jen
   s výslovně nadiktovaným `--to`. Klíče v `secrets.env`
   (`TWILIO_ACCOUNT_SID`, `TWILIO_AUTH_TOKEN`), kanál se zapíná `[sms]` v configu.
1. **wm** — ruce Jarvise: ovládání oken (fokus, zavření, maximalizace, posun),
   klávesnice (píše i plnou českou diakritiku — XTest s remapem keysymů),
   myši a screenshoty, vše nativně přes X11 (EWMH + XTest, žádný xdotool).
   `jarvis wm …` z CLI; hlasový agent má tytéž příkazy povolené přes Bash
   (`[wm] enabled`) a stav obrazovky si ověřuje screenshotem (vision) —
   „Jarvisi, přepni na Signal a napiš Tomášovi…“ tak skutečně provede.
2. **analyze** (hodinově) — segmentace časové osy z titulků, výběr ≤8
   reprezentativních snímků, `claude -p` (vision, JSON kontrakt) → hodinový
   souhrn. Denní rozpočet (`daily_budget_usd`), při vyčerpání titulky-only.
3. **digest** (denně v 19:00) — agregace dne, Claude vygeneruje Markdown digest
   (přehled, rozložení času, postřehy, doporučení, automatizační příležitosti),
   render do HTML a odeslání přes SendGrid. Nedoručený digest se doesílá hodinově.
4. **propose** — z opakovaných vzorů (`automation_hints`) generuje konkrétní
   automatizační artefakty do `~/.local/share/jarvis/proposals/`. Nic se
   neinstaluje samo.

## Instalace

```bash
cargo install --path .
jarvis listen --download-model   # whisper model (~574 MB, jednorázově)
jarvis say --download-model      # piper hlas pro lokální TTS zálohu (~60 MB)
jarvis doctor            # kontrola prostředí (X11, claude CLI, SendGrid klíč, DB, mikrofon)
jarvis install-units     # systemd user units: capture + listen + hodinový a denní timer
```

Prerekvizity:
- X11 session (Wayland není podporován), `claude` CLI přihlášené.
- `~/.config/jarvis/secrets.env` s `SENDGRID_API_KEY=...` a `ELEVENLABS_API_KEY=...`
  (chmod 600). ElevenLabs klíči stačí scope `text_to_speech`; s právy
  `voices_read`/`user_read` navíc funguje `say --list-voices` a kontrola kreditů
  v `doctor --live`.
- Verifikovaný odesílatel v SendGrid (Single Sender Verification); `email.from`
  v configu musí být verifikovaná adresa.
- Pro poslech: PulseAudio/PipeWire s mikrofonem (`parec`, případně `arecord`).
- Pro hlas: přehrávač `ffplay` (ffmpeg), `mpv`, nebo `ffmpeg`+`paplay`;
  lokální záloha `pip3 install --user piper-tts` + `jarvis say --download-model`.

### Build

`whisper-rs` kompiluje whisper.cpp s CUDA — potřebuje `cmake`, `libclang`
a CUDA toolkit. Vše je na tomto stroji user-space bez sudo:

```bash
pip3 install --user cmake libclang
~/.local/opt/micromamba/micromamba create -p ~/.local/opt/cuda-env \
  -c nvidia -c conda-forge cuda-nvcc=12.6 cuda-cudart-dev=12.6 \
  libcublas-dev=12.6 cuda-cccl=12.6
```

Build je najde přes `[env]` + rpath v [.cargo/config.toml](.cargo/config.toml);
po přeinstalaci systému stačí zopakovat a případně upravit cesty tam. Binárka
má CUDA cesty zapečené (rpath) — žádné LD_LIBRARY_PATH není potřeba. Když GPU
za běhu není dostupné, whisper spadne na CPU backend (pak přepni model na
`small-q5_1`, turbo na CPU nestíhá).

## Konfigurace

`~/.config/jarvis/config.toml` — všechny hodnoty volitelné, defaults viz
[config.example.toml](config.example.toml). Tajemství jen v `secrets.env`, nikdy
v configu ani repu.

## Příkazy

| Příkaz | Účel |
|---|---|
| `jarvis capture` | snímací démon (foreground; v produkci přes systemd) |
| `jarvis listen [--print-only]` | poslech mikrofonu, near-realtime přepis do DB |
| `jarvis listen --download-model` | stáhne whisper model z configu |
| `jarvis listen --wav soubor.wav` | prožene WAV celou pipeline (test/ladění, bez zápisu) |
| `jarvis say "text" [--voice ID] [--out f] [--no-cache] [--local]` | řekne text nahlas (`--local` = vynuť piper) |
| `jarvis say --download-model` | stáhne piper hlas pro lokální zálohu |
| `jarvis say --list-voices` | hlasy v účtu (vyžaduje klíč s `voices_read`) |
| `jarvis converse "otázka" [--mute]` | zeptej se Jarvise textem (dialog bez mikrofonu) |
| `jarvis converse-eval <jsonl> \| --from-db N` | open-ear kill-gate: `--from-db` vypíše šablonu korpusu, se souborem vyhodnotí false-accept/recall |
| `jarvis wm <akce>` | okna/klávesnice/myš: `list`, `focus`, `spawn`, `type`, `key`, `click`, `screenshot`, `close`, `maximize`, `wait`… |
| `jarvis sms "text" [--to +420…] [--no-wait]` | SMS přes Twilio, čeká na doručenku (klíče v secrets.env) |
| `jarvis run` | fallback bez systemd: capture + poslech + plánovač v jednom procesu |
| `jarvis analyze [--dry-run] [--window-hours N]` | hodinová extrakce |
| `jarvis digest [--date D] [--send\|--dry-run]` | denní digest (dry-run uloží HTML náhled) |
| `jarvis send-test` | testovací e-mail přes SendGrid |
| `jarvis pause 30m` / `jarvis resume` | soukromí: dočasně nesnímat |
| `jarvis status` | poslední vzorek, dnešní útrata, stav digestu |
| `jarvis doctor [--live]` | prerekvizity; `--live` = SendGrid sandbox + claude ping |
| `jarvis purge [--older-than 7d]` | ruční smazání starých snímků |
| `jarvis install-units [--print]` | instalace/výpis systemd user units |
| `jarvis propose [--list] [--pattern ID]` | návrhy automatizací z detekovaných vzorů |
| `jarvis runbook pending` | návrhy čekající na schválení |
| `jarvis runbook approve <id> [--schedule daily@HH:MM]` | schválí návrh ke spouštění (chce terminál; na dálku Telegram) |
| `jarvis runbook run <id\|název>` / `list` / `runs` / `show` | spustí/vypíše schválené automatizace (run umí i hlasový agent) |
| `jarvis runbook enable/disable/schedule/dismiss` | správa runbooků |

## Soukromí

- Data zůstávají lokálně (`~/.local/share/jarvis`, 0700). Stroj opouštějí jen
  zmenšené snímky do Anthropic API (vypnutelné `send_images = false` → analýza
  jen z titulků) a finální digest e-mailem.
- Blacklist oken (hesla, banking, anonymní režim) — snímek se vůbec nepořídí
  a neukládá se ani titulek. `jarvis pause` na schůzky. Idle se nesnímá.
- **Poslech**: audio se nikdy neukládá na disk ani neposílá ven — přepis běží
  lokálně (whisper.cpp na CPU) a ukládá se jen text. `jarvis pause` zahazuje
  zvuk rovnou z paměti; `enabled = false` v `[listen]` poslech úplně vypne.
- **Hlas**: text předaný `jarvis say` (a hláška o digestu) se posílá do
  ElevenLabs API — do hlasového výstupu nedávej nic citlivého. Vygenerované
  audio zůstává v lokální cache (`tts-cache/`, 0700); `[speak] enabled = false`
  kanál vypne. Lokální záloha piper nic nikam neposílá — `engine = "piper"`
  dává plně offline hlas (a `say --local` jednorázově).
- **Konverzace**: oslovená promluva + titulek aktivního okna + poslední
  výměny jdou do Claude API (stejně jako analýza). Bez oslovení se do Clauda
  neposílá nic — pokud není zapnutý `converse.open_ear` (pak jde do Clauda
  i promluva bez jména, která projde open-ear bránou); `jarvis pause` umlčí
  i dialog (zvuk se zahazuje);
  `[converse] enabled = false` dialog vypne. Vlastní řeč Jarvis neslyší:
  hraje ji skrz sink echo-cancel modulu (`speak.sink`) a AEC ji od
  mikrofonu odečte — do logu promluv padá jen skutečné okolí.
- **SMS**: text zprávy jde do Twilio API (a operátorům po cestě) — necitlivý
  obsah. Odesílatel je alfanumerický („Olvano") → příjemce nemůže odpovědět.
  Agentovi je cizí příjemce dovolen jen s výslovně nadiktovaným číslem;
  `[sms] enabled = false` kanál vypne úplně.
- **Ovládání oken**: s `[wm] enabled` může hlasový agent psát do oken a klikat
  (Bash je omezený vzorem na `jarvis wm …`, nic jiného spustit nejde; prompt
  vyžaduje ověření cílového okna před psaním a stop při nejednoznačném cíli).
  Screenshoty z `jarvis wm screenshot` si agent čte lokálně nástrojem Read —
  do API jde snímek jen jako součást agentovy konverzace, stejná cesta jako
  `analyze`. Kdo tomu nevěří, dá `[wm] enabled = false` — hlasová větev
  zmizí a `jarvis wm` zůstane jen jako ruční CLI.
- Retence snímků 7 dní (konfig.), textové souhrny a přepisy zůstávají.

## Provoz a řešení potíží

- Proti dvojímu snímání drží capture flock (`~/.local/share/jarvis/capture.lock`) —
  `jarvis run` a `jarvis capture` odmítnou start, když už běží systemd služba.
  Poslech má vlastní zámek (`listen.lock`) a chová se stejně.
- Poslech neběží / nic nepřepisuje? `jarvis status` (tep démona, počty promluv,
  hlídač ticha), `journalctl --user -u jarvis-listen -f`, `jarvis doctor --live`
  (změří reálnou úroveň mikrofonu), `jarvis listen --print-only` na ruční test.
- Dvojímu odeslání digestu brání atomický claim v DB; doručení je přesto
  **at-least-once** — při výpadku sítě uprostřed odeslání může vzácně dorazit
  digest dvakrát (vědomý kompromis, SendGrid nededuplikuje).
- `systemctl --user list-timers 'jarvis-*'` — tikají timery?
- `journalctl --user -u jarvis-capture -f` — log démona.
- Po změně `digest.hour` znovu `jarvis install-units`.
- Capture po rebootu nevidí X? Units zapékají DISPLAY/XAUTHORITY z instalace;
  pokud se ti mění XAUTHORITY, přidej do autostartu
  `systemctl --user import-environment DISPLAY XAUTHORITY && systemctl --user restart jarvis-capture`.
- `jarvis doctor --live` ověří celou cestu (SendGrid sandbox send, claude ping).
