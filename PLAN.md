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
| Tajemství | SendGrid API key uložen v `~/.config/jarvis/secrets.env` (0600), **nikdy v repu ani v tomto souboru** |

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
`SENDGRID_API_KEY=…`. Načítá se jako env (systemd `EnvironmentFile=%h/.config/jarvis/secrets.env`,
při ručním spuštění si ho binárka načte sama). V repu nikdy; `.gitignore` pro jistotu
dostane `*.env`, `secrets*`.

### 3.5 CLI

| Příkaz | Účel |
|---|---|
| `jarvis capture` | démon — snímání (foreground, loguje přes `tracing`) |
| `jarvis analyze [--dry-run]` | hodinová extrakce (dry-run vypíše prompt a vybrané snímky, nevolá API) |
| `jarvis digest [--date D] [--send\|--dry-run]` | složí digest; `--dry-run` uloží HTML do souboru k náhledu |
| `jarvis send-test` | pošle testovací e-mail (ověření SendGrid setupu) |
| `jarvis pause 30m` / `resume` | soukromí — dočasné vypnutí snímání |
| `jarvis status` | stav: poslední snímek, počty, dnešní útrata, fronta |
| `jarvis doctor` | prereq check: DISPLAY, claude CLI + auth, API key, verifikace odesílatele (test call), místo na disku |
| `jarvis purge [--older-than 7d]` | ruční retence |
| `jarvis install-units` | vygeneruje a nainstaluje systemd user units |

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
hourly_summaries(id, period_start, period_end, json, -- výstup analyze (JSON kontrakt)
        model, cost_usd, degraded BOOL)
daily_digests(id, date, markdown, html, status,      -- pending|sent|failed
        sendgrid_msg_id, sent_at)
patterns(id, key, description, evidence, occurrences,-- detekované rutiny (fáze B+)
        first_seen, last_seen,
        status)                                      -- candidate|proposed|approved|automated|dismissed
proposals(id, pattern_id, kind, path, created_at)    -- vygenerované automatizace (skript/skill/timer)
costs(id, ts, component, model, tokens_in, tokens_out, usd)
state(key, value)                                    -- watermarky, pause deadline
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
    └── patterns.rs          # fáze B (skeleton od začátku)
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
- **C — Návrhy artefaktů**: pro schválené vzory Jarvis přes `claude -p` rovnou vygeneruje
  automatizaci — shell/Rust skript, systemd timer, Claude Code skill či workflow — do
  `~/.local/share/jarvis/proposals/`, odkaz v digestu. Instalace = moje schválení.
- **D — Řízená exekuce**: schválené runbooky spouští Jarvis sám (timer/event), výsledky
  reportuje v digestu. Vše s vnějšími efekty zůstává approval-gated; rozšíření kanálů
  (Telegram bot už je v prostředí) pro schvalování „na dálku".

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
