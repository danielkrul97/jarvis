# Jarvis

Osobní asistent ve stylu filmového Jarvise: sleduje, na čem na počítači pracuješ
(X11), průběžně to analyzuje přes Claude Code CLI a jednou denně pošle e-mailem
digest s doporučeními. Dlouhodobý cíl: detekovat opakované rutiny a **automatizovat
tvoji práci** (`jarvis propose`). Architektura a rozhodnutí: viz [PLAN.md](PLAN.md).

## Jak to funguje

1. **capture** (démon) — každých 10 s metadata aktivního okna (titulek, třída,
   idle), každých 60 s screenshot (dedup přes dHash, blacklist citlivých oken,
   pauza na povel). Vše lokálně: SQLite + JPEG v `~/.local/share/jarvis`.
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
jarvis doctor            # kontrola prostředí (X11, claude CLI, SendGrid klíč, DB)
jarvis install-units     # systemd user units: capture + hodinový a denní timer
```

Prerekvizity:
- X11 session (Wayland není podporován), `claude` CLI přihlášené.
- `~/.config/jarvis/secrets.env` s `SENDGRID_API_KEY=...` (chmod 600).
- Verifikovaný odesílatel v SendGrid (Single Sender Verification); `email.from`
  v configu musí být verifikovaná adresa.

## Konfigurace

`~/.config/jarvis/config.toml` — všechny hodnoty volitelné, defaults viz
[config.example.toml](config.example.toml). Tajemství jen v `secrets.env`, nikdy
v configu ani repu.

## Příkazy

| Příkaz | Účel |
|---|---|
| `jarvis capture` | snímací démon (foreground; v produkci přes systemd) |
| `jarvis run` | fallback bez systemd: capture + plánovač v jednom procesu |
| `jarvis analyze [--dry-run] [--window-hours N]` | hodinová extrakce |
| `jarvis digest [--date D] [--send\|--dry-run]` | denní digest (dry-run uloží HTML náhled) |
| `jarvis send-test` | testovací e-mail přes SendGrid |
| `jarvis pause 30m` / `jarvis resume` | soukromí: dočasně nesnímat |
| `jarvis status` | poslední vzorek, dnešní útrata, stav digestu |
| `jarvis doctor [--live]` | prerekvizity; `--live` = SendGrid sandbox + claude ping |
| `jarvis purge [--older-than 7d]` | ruční smazání starých snímků |
| `jarvis install-units [--print]` | instalace/výpis systemd user units |
| `jarvis propose [--list] [--pattern ID]` | návrhy automatizací z detekovaných vzorů |

## Soukromí

- Data zůstávají lokálně (`~/.local/share/jarvis`, 0700). Stroj opouštějí jen
  zmenšené snímky do Anthropic API (vypnutelné `send_images = false` → analýza
  jen z titulků) a finální digest e-mailem.
- Blacklist oken (hesla, banking, anonymní režim) — snímek se vůbec nepořídí
  a neukládá se ani titulek. `jarvis pause` na schůzky. Idle se nesnímá.
- Retence snímků 7 dní (konfig.), textové souhrny zůstávají.

## Provoz a řešení potíží

- Proti dvojímu snímání drží capture flock (`~/.local/share/jarvis/capture.lock`) —
  `jarvis run` a `jarvis capture` odmítnou start, když už běží systemd služba.
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
