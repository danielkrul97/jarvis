//! Ovládání oken, klávesnice a myši (X11): EWMH client messages pro okna,
//! XTest pro syntetický vstup, GetImage pro screenshoty. CLI `jarvis wm …`
//! slouží člověku i konverzačnímu agentovi (Bash omezený na `jarvis wm`).
//!
//! Psaní textu: znak → keysym (Latin-1 přímo, jinak 0x0100_0000+kódový bod
//! dle X11 konvence). Keysym, který v aktuálním rozložení chybí (česká
//! diakritika na US layoutu, emoji…), se dočasně namapuje na volný keycode
//! a po dopsání se mapa obnoví — stejný trik používá xdotool.

use crate::config::{Config, Paths};
use crate::x11util;
use anyhow::{bail, ensure, Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, ImageFormat,
    ImageOrder, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const XK_RETURN: u32 = 0xff0d;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;
const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;

/// Pauza po přemapování keycode — klienti (hlavně Chromium/Electron) musí
/// stihnout zpracovat MappingNotify, než přijde fake stisk.
const REMAP_SETTLE_MS: u64 = 30;

#[derive(Debug, Clone)]
pub struct WinMeta {
    pub id: Window,
    pub desktop: Option<u32>,
    pub class: String,
    pub title: String,
}

impl WinMeta {
    fn desktop_label(&self) -> String {
        match self.desktop {
            Some(u32::MAX) => "*".into(),
            Some(d) => d.to_string(),
            None => "-".into(),
        }
    }
}

impl std::fmt::Display for WinMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:07x}  {} — {}", self.id, self.class, self.title)
    }
}

struct Atoms {
    client_list: Atom,
    client_list_stacking: Atom,
    active_window: Atom,
    wm_name: Atom,
    wm_desktop: Atom,
    close_window: Atom,
    wm_state: Atom,
    max_vert: Atom,
    max_horz: Atom,
    fullscreen: Atom,
    wm_change_state: Atom,
}

pub struct Wm {
    conn: RustConnection,
    root: Window,
    width: u16,
    height: u16,
    lsb: bool,
    pixmap_formats: Vec<(u8, u8)>,
    a: Atoms,
    key_delay: Duration,
}

impl Wm {
    pub fn connect(key_delay_ms: u64) -> Result<Self> {
        let (conn, screen_num) =
            x11rb::connect(None).context("nelze se připojit k X serveru (DISPLAY?)")?;
        let (root, width, height, lsb, pixmap_formats) = {
            let setup = conn.setup();
            let screen = &setup.roots[screen_num];
            (
                screen.root,
                screen.width_in_pixels,
                screen.height_in_pixels,
                setup.image_byte_order == ImageOrder::LSB_FIRST,
                setup
                    .pixmap_formats
                    .iter()
                    .map(|f| (f.depth, f.bits_per_pixel))
                    .collect::<Vec<_>>(),
            )
        };
        let intern = |name: &[u8]| -> Result<Atom> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        let a = Atoms {
            client_list: intern(b"_NET_CLIENT_LIST")?,
            client_list_stacking: intern(b"_NET_CLIENT_LIST_STACKING")?,
            active_window: intern(b"_NET_ACTIVE_WINDOW")?,
            wm_name: intern(b"_NET_WM_NAME")?,
            wm_desktop: intern(b"_NET_WM_DESKTOP")?,
            close_window: intern(b"_NET_CLOSE_WINDOW")?,
            wm_state: intern(b"_NET_WM_STATE")?,
            max_vert: intern(b"_NET_WM_STATE_MAXIMIZED_VERT")?,
            max_horz: intern(b"_NET_WM_STATE_MAXIMIZED_HORZ")?,
            fullscreen: intern(b"_NET_WM_STATE_FULLSCREEN")?,
            wm_change_state: intern(b"WM_CHANGE_STATE")?,
        };
        Ok(Self {
            conn,
            root,
            width,
            height,
            lsb,
            pixmap_formats,
            a,
            key_delay: Duration::from_millis(key_delay_ms),
        })
    }

    /// Ověří dostupnost XTest — bez něj nejde syntetický vstup.
    pub fn ensure_xtest(&self) -> Result<()> {
        self.conn
            .xtest_get_version(2, 2)
            .context("XTest request selhal")?
            .reply()
            .context("X server nemá XTest extension — nelze posílat klávesy/myš")?;
        Ok(())
    }

    /// Round-trip: server zpracoval všechny předchozí requesty.
    fn sync(&self) -> Result<()> {
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    fn meta(&self, win: Window) -> WinMeta {
        WinMeta {
            id: win,
            desktop: x11util::get_prop_u32(&self.conn, win, self.a.wm_desktop),
            class: x11util::window_class(&self.conn, win),
            title: x11util::window_title(&self.conn, win, self.a.wm_name),
        }
    }

    pub fn windows(&self) -> Result<Vec<WinMeta>> {
        let ids = x11util::get_prop_windows(&self.conn, self.root, self.a.client_list);
        ensure!(
            !ids.is_empty(),
            "_NET_CLIENT_LIST je prázdný — window manager nepodporuje EWMH?"
        );
        Ok(ids.into_iter().map(|w| self.meta(w)).collect())
    }

    pub fn active_id(&self) -> Option<Window> {
        x11util::get_prop_u32(&self.conn, self.root, self.a.active_window).filter(|&w| w != 0)
    }

    pub fn active(&self) -> Option<WinMeta> {
        self.active_id().map(|w| self.meta(w))
    }

    /// Najde okno: "0xID", jinak podřetězec třídy (přednost) nebo titulku,
    /// case-insensitive; při shodě víc oken vyhrává nejvýš položené.
    pub fn resolve(&self, query: &str) -> Result<WinMeta> {
        let wins = self.windows()?;
        if let Some(hex) = query.strip_prefix("0x").or_else(|| query.strip_prefix("0X")) {
            let id = u32::from_str_radix(hex, 16)
                .with_context(|| format!("'{query}' není hex ID okna"))?;
            ensure!(wins.iter().any(|w| w.id == id), "okno {query} není v seznamu oken");
            return Ok(self.meta(id));
        }
        let stacking =
            x11util::get_prop_windows(&self.conn, self.root, self.a.client_list_stacking);
        match pick_window(&wins, &stacking, query) {
            Some(i) => Ok(wins[i].clone()),
            None => {
                let list = wins
                    .iter()
                    .map(|w| format!("  {w}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!("žádné okno neodpovídá „{query}“. Otevřená okna:\n{list}")
            }
        }
    }

    fn client_message(&self, win: Window, type_: Atom, data: [u32; 5]) -> Result<()> {
        let ev = ClientMessageEvent::new(32, win, type_, data);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            ev,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Aktivuje okno a počká na read-back přes _NET_ACTIVE_WINDOW (až 2 s).
    pub fn activate(&self, win: Window) -> Result<WinMeta> {
        // source indication 2 = pager/uživatelská akce — WM ji respektuje
        self.client_message(win, self.a.active_window, [2, 0, 0, 0, 0])?;
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(80));
            if self.active_id() == Some(win) {
                return Ok(self.meta(win));
            }
        }
        let now = self
            .active()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "žádné".into());
        bail!("okno se neaktivovalo do 2 s (aktivní je: {now})")
    }

    pub fn close(&self, win: Window) -> Result<()> {
        self.client_message(win, self.a.close_window, [0, 2, 0, 0, 0])
    }

    pub fn minimize(&self, win: Window) -> Result<()> {
        // ICCCM WM_CHANGE_STATE → IconicState (3)
        self.client_message(win, self.a.wm_change_state, [3, 0, 0, 0, 0])
    }

    pub fn set_maximized(&self, win: Window, on: bool) -> Result<()> {
        let action = u32::from(on);
        self.client_message(
            win,
            self.a.wm_state,
            [action, self.a.max_vert, self.a.max_horz, 2, 0],
        )
    }

    pub fn set_fullscreen(&self, win: Window, on: bool) -> Result<()> {
        let action = u32::from(on);
        self.client_message(win, self.a.wm_state, [action, self.a.fullscreen, 0, 2, 0])
    }

    pub fn move_window(&self, win: Window, x: i32, y: i32) -> Result<()> {
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().x(x).y(y))?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn resize_window(&self, win: Window, w: u32, h: u32) -> Result<()> {
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().width(w).height(h))?;
        self.conn.flush()?;
        Ok(())
    }

    /// Čeká, až se objeví okno odpovídající dotazu.
    pub fn wait(&self, query: &str, timeout: Duration) -> Result<WinMeta> {
        let deadline = Instant::now() + timeout;
        loop {
            let wins = self.windows().unwrap_or_default();
            let stacking =
                x11util::get_prop_windows(&self.conn, self.root, self.a.client_list_stacking);
            if let Some(i) = pick_window(&wins, &stacking, query) {
                return Ok(wins[i].clone());
            }
            if Instant::now() >= deadline {
                bail!("okno „{query}“ se neobjevilo do {} s", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Čeká na okno, které v `before` ještě nebylo (čerstvě spuštěná
    /// aplikace). _NET_CLIENT_LIST override-redirect okna (notifikace,
    /// tooltips) neobsahuje, takže falešné poplachy jsou vzácné.
    pub fn wait_new(
        &self,
        before: &std::collections::HashSet<Window>,
        timeout: Duration,
    ) -> Result<WinMeta> {
        let deadline = Instant::now() + timeout;
        loop {
            let wins = self.windows().unwrap_or_default();
            if let Some(w) = wins.iter().find(|w| !before.contains(&w.id)) {
                return Ok(w.clone());
            }
            if Instant::now() >= deadline {
                bail!("nové okno se neobjevilo do {} s", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    // ---------- klávesnice ----------

    fn fetch_keymap(&self) -> Result<Keymap> {
        let (min, max) = {
            let setup = self.conn.setup();
            (setup.min_keycode, setup.max_keycode)
        };
        let reply = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)?
            .reply()
            .context("GetKeyboardMapping selhal")?;
        let per = usize::from(reply.keysyms_per_keycode);
        ensure!(per > 0 && !reply.keysyms.is_empty(), "prázdná mapa klávesnice");
        Ok(Keymap { min, per, syms: reply.keysyms, borrowed: None, bound_sym: None })
    }

    fn fake_key(&self, kc: u8, press: bool) -> Result<()> {
        let t = if press { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
        self.conn
            .xtest_fake_input(t, kc, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)?;
        Ok(())
    }

    /// Namapuje keysym na zapůjčený keycode (jednou, cache drží poslední sym).
    fn bind_spare(&self, km: &mut Keymap, sym: u32) -> Result<u8> {
        let kc = match km.borrowed {
            Some(kc) => kc,
            None => {
                let kc = km.spare().unwrap_or(km.max_keycode());
                km.borrowed = Some(kc);
                kc
            }
        };
        if km.bound_sym != Some(sym) {
            self.conn.change_keyboard_mapping(1, kc, 2, &[sym, sym])?;
            self.sync()?;
            std::thread::sleep(Duration::from_millis(REMAP_SETTLE_MS));
            km.bound_sym = Some(sym);
        }
        Ok(kc)
    }

    /// Vrátí zapůjčenému keycode původní keysymy.
    fn restore_borrowed(&self, km: &Keymap) -> Result<()> {
        if let Some(kc) = km.borrowed {
            let row = km.row(kc).to_vec();
            self.conn
                .change_keyboard_mapping(1, kc, row.len() as u8, &row)?;
            self.sync()?;
        }
        Ok(())
    }

    /// Napíše text do aktuálně fokusovaného okna. Vrací počet znaků.
    pub fn type_text(&self, text: &str) -> Result<usize> {
        self.ensure_xtest()?;
        let mut km = self.fetch_keymap()?;
        let shift = km
            .find_any(XK_SHIFT_L)
            .context("v mapě klávesnice chybí Shift")?;
        let mut typed = 0usize;
        let result = (|| -> Result<()> {
            for c in text.chars() {
                let sym = keysym_for_char(c);
                let (kc, need_shift) = match km.find(sym) {
                    Some(hit) => hit,
                    None => (self.bind_spare(&mut km, sym)?, false),
                };
                if need_shift {
                    self.fake_key(shift, true)?;
                }
                // uvolnění musí proběhnout i při chybě mezi press a release —
                // jinak zůstane klávesa/Shift logicky držená a kazí další vstup
                let press = self.fake_key(kc, true);
                let release = if press.is_ok() { self.fake_key(kc, false) } else { Ok(()) };
                if need_shift {
                    let _ = self.fake_key(shift, false);
                }
                press?;
                release?;
                self.conn.flush()?;
                std::thread::sleep(self.key_delay);
                typed += 1;
            }
            Ok(())
        })();
        let restore = self.restore_borrowed(&km);
        result?;
        restore?;
        Ok(typed)
    }

    /// Stiskne zkratku ("ctrl+f", "Return", "alt+F4", "ctrl++").
    pub fn key_combo(&self, combo: &str) -> Result<()> {
        self.ensure_xtest()?;
        let (mod_syms, key_sym) = parse_combo(combo)?;
        let mut km = self.fetch_keymap()?;
        let shift = km
            .find_any(XK_SHIFT_L)
            .context("v mapě klávesnice chybí Shift")?;
        let mut mod_kcs = Vec::new();
        for m in &mod_syms {
            let kc = km
                .find_any(*m)
                .with_context(|| format!("modifikátor (keysym 0x{m:x}) nemá keycode"))?;
            mod_kcs.push(kc);
        }
        let (kc, need_shift) = match km.find(key_sym) {
            Some(hit) => hit,
            None => (self.bind_spare(&mut km, key_sym)?, false),
        };
        if need_shift && !mod_syms.contains(&XK_SHIFT_L) {
            mod_kcs.push(shift);
        }
        let result = (|| -> Result<()> {
            for &mk in &mod_kcs {
                self.fake_key(mk, true)?;
            }
            self.fake_key(kc, true)?;
            self.fake_key(kc, false)?;
            for &mk in mod_kcs.iter().rev() {
                self.fake_key(mk, false)?;
            }
            self.conn.flush()?;
            std::thread::sleep(self.key_delay.max(Duration::from_millis(25)));
            Ok(())
        })();
        if result.is_err() {
            // po chybě uprostřed sekvence uvolni vše, co mohlo zůstat stisknuté;
            // release nestisknuté klávesy je v X neškodný no-op, zato zaseknutý
            // Ctrl/Shift by kazil další skutečný vstup uživatele
            let _ = self.fake_key(kc, false);
            for &mk in mod_kcs.iter().rev() {
                let _ = self.fake_key(mk, false);
            }
            let _ = self.conn.flush();
        }
        let restore = self.restore_borrowed(&km);
        result?;
        restore?;
        Ok(())
    }

    // ---------- myš ----------

    pub fn move_pointer(&self, x: i16, y: i16) -> Result<()> {
        self.ensure_xtest()?;
        // detail 0 = absolutní souřadnice na root okně
        self.conn
            .xtest_fake_input(MOTION_NOTIFY_EVENT, 0, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    pub fn click(&self, x: i16, y: i16, button: u8, double: bool) -> Result<()> {
        ensure!((1..=9).contains(&button), "button musí být 1–9 (1=levé, 2=prostřední, 3=pravé)");
        self.move_pointer(x, y)?;
        std::thread::sleep(Duration::from_millis(40));
        let presses = if double { 2 } else { 1 };
        for i in 0..presses {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(120));
            }
            self.conn.xtest_fake_input(
                BUTTON_PRESS_EVENT,
                button,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            )?;
            self.conn.flush()?;
            std::thread::sleep(Duration::from_millis(40));
            self.conn.xtest_fake_input(
                BUTTON_RELEASE_EVENT,
                button,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            )?;
            self.conn.flush()?;
        }
        Ok(())
    }

    // ---------- screenshot ----------

    /// Snímek celé obrazovky, nebo výřez okna (query). Uloží JPEG.
    pub fn screenshot(&self, window_query: Option<&str>, out: &std::path::Path) -> Result<()> {
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, self.width, self.height, u32::MAX)
            .context("GetImage request selhal")?
            .reply()
            .context("GetImage reply selhal")?;
        let img = x11util::zpixmap_to_rgb(
            &reply.data,
            reply.depth,
            self.width,
            self.height,
            self.lsb,
            &self.pixmap_formats,
        )?;
        let img = match window_query {
            None => img,
            Some(q) => {
                let m = self.resolve(q)?;
                let geo = self.conn.get_geometry(m.id)?.reply().context("GetGeometry selhal")?;
                let tr = self
                    .conn
                    .translate_coordinates(m.id, self.root, 0, 0)?
                    .reply()
                    .context("TranslateCoordinates selhal")?;
                let (x, y, w, h) = clamp_rect(
                    i32::from(tr.dst_x),
                    i32::from(tr.dst_y),
                    u32::from(geo.width),
                    u32::from(geo.height),
                    u32::from(self.width),
                    u32::from(self.height),
                )
                .with_context(|| format!("okno {m} je celé mimo obrazovku"))?;
                image::imageops::crop_imm(&img, x, y, w, h).to_image()
            }
        };
        x11util::save_jpeg(&img, out)
    }
}

// ---------- čisté helpery (unit-testované) ----------

struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
    /// keycode zapůjčený pro nemapované znaky (obnoví se po akci)
    borrowed: Option<u8>,
    /// keysym právě navázaný na zapůjčený keycode
    bound_sym: Option<u32>,
}

impl Keymap {
    fn count(&self) -> usize {
        self.syms.len() / self.per
    }

    fn max_keycode(&self) -> u8 {
        self.min + (self.count().saturating_sub(1)) as u8
    }

    fn row(&self, kc: u8) -> &[u32] {
        let i = usize::from(kc - self.min) * self.per;
        &self.syms[i..i + self.per]
    }

    /// (keycode, potřebuje Shift) — sloupec 0 bez Shiftu, sloupec 1 s ním.
    fn find(&self, sym: u32) -> Option<(u8, bool)> {
        for k in 0..self.count() {
            let kc = self.min + k as u8;
            let row = self.row(kc);
            if row[0] == sym {
                return Some((kc, false));
            }
            if self.per > 1 && row[1] == sym {
                return Some((kc, true));
            }
        }
        None
    }

    /// Keycode se symem v libovolném sloupci (modifikátory).
    fn find_any(&self, sym: u32) -> Option<u8> {
        (0..self.count())
            .map(|k| self.min + k as u8)
            .find(|&kc| self.row(kc).contains(&sym))
    }

    /// Nejvyšší keycode bez jediného keysymu — bezpečný k zapůjčení.
    fn spare(&self) -> Option<u8> {
        (0..self.count())
            .rev()
            .map(|k| self.min + k as u8)
            .find(|&kc| self.row(kc).iter().all(|&s| s == 0))
    }
}

/// Keysym znaku dle X11 konvence: řídicí klávesy jmenovitě, Latin-1 přímo,
/// ostatní Unicode = 0x0100_0000 + kódový bod.
pub(crate) fn keysym_for_char(c: char) -> u32 {
    match c {
        '\n' | '\r' => XK_RETURN,
        '\t' => 0xff09,
        c => {
            let cp = c as u32;
            if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
                cp
            } else {
                0x0100_0000 + cp
            }
        }
    }
}

/// Pojmenované klávesy pro `wm key` (case-insensitive) + F1–F24.
pub(crate) fn named_keysym(name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase();
    let sym = match n.as_str() {
        "return" | "enter" => XK_RETURN,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "prior" => 0xff55,
        "pagedown" | "next" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "space" => 0x20,
        "menu" => 0xff67,
        _ => {
            let num = n.strip_prefix('f')?.parse::<u32>().ok()?;
            if (1..=24).contains(&num) {
                return Some(0xffbe + num - 1);
            }
            return None;
        }
    };
    Some(sym)
}

/// "ctrl+shift+k" → (mod keysymy, keysym klávesy). "ctrl++" = Ctrl a '+'.
pub(crate) fn parse_combo(s: &str) -> Result<(Vec<u32>, u32)> {
    let s = s.trim();
    ensure!(!s.is_empty(), "prázdná zkratka");
    let (mod_tokens, key_token): (Vec<&str>, &str) = if s == "+" {
        (vec![], "+")
    } else if let Some(rest) = s.strip_suffix("++") {
        (rest.split('+').filter(|p| !p.is_empty()).collect(), "+")
    } else {
        let mut parts: Vec<&str> = s.split('+').collect();
        let key = parts.pop().unwrap_or_default();
        ensure!(!key.is_empty(), "neplatná zkratka „{s}“");
        (parts, key)
    };
    let mut mods = Vec::new();
    for m in mod_tokens {
        let sym = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => XK_CONTROL_L,
            "shift" => XK_SHIFT_L,
            "alt" => XK_ALT_L,
            "super" | "meta" | "win" | "cmd" => XK_SUPER_L,
            "altgr" => XK_ISO_LEVEL3_SHIFT,
            other => bail!("neznámý modifikátor „{other}“ (umím ctrl/shift/alt/super/altgr)"),
        };
        mods.push(sym);
    }
    let key_sym = if let Some(ks) = named_keysym(key_token) {
        ks
    } else {
        let mut chars = key_token.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => keysym_for_char(c),
            _ => bail!(
                "neznámá klávesa „{key_token}“ (jeden znak, Return, Tab, Esc, F1–F24, šipky…)"
            ),
        }
    };
    Ok((mods, key_sym))
}

/// Výběr okna: přesná třída > podřetězec třídy > podřetězec titulku;
/// při stejném ranku vyhrává okno nejvýš ve stacking order.
pub(crate) fn pick_window(wins: &[WinMeta], stacking: &[Window], query: &str) -> Option<usize> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return None;
    }
    let stack_pos = |id: Window| stacking.iter().position(|&w| w == id).unwrap_or(0);
    wins.iter()
        .enumerate()
        .filter_map(|(i, w)| {
            let class = w.class.to_lowercase();
            let title = w.title.to_lowercase();
            let rank = if class == q {
                0u8
            } else if class.contains(&q) {
                1
            } else if title.contains(&q) {
                2
            } else {
                return None;
            };
            Some((rank, std::cmp::Reverse(stack_pos(w.id)), i))
        })
        .min_by_key(|&(rank, pos, _)| (rank, pos))
        .map(|(_, _, i)| i)
}

/// Ořez obdélníku na obrazovku; None = celý mimo.
pub(crate) fn clamp_rect(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    screen_w: u32,
    screen_h: u32,
) -> Option<(u32, u32, u32, u32)> {
    let x1 = (x + w as i32).min(screen_w as i32);
    let y1 = (y + h as i32).min(screen_h as i32);
    let x0 = x.max(0);
    let y0 = y.max(0);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// Doslovné "\n" a "\t" v CLI argumentu na skutečné znaky.
pub(crate) fn unescape(text: &str) -> String {
    text.replace("\\n", "\n").replace("\\t", "\t")
}

// ---------- CLI ----------

#[derive(clap::Subcommand)]
pub enum WmCmd {
    /// Vypíše okna: ID, plocha, třída, titulek (* = aktivní)
    List,
    /// Vypíše aktivní okno
    Active,
    /// Aktivuje okno a ověří read-backem (query = část třídy/titulku nebo 0xID)
    Focus { query: String },
    /// Zdvořile zavře okno (_NET_CLOSE_WINDOW)
    Close { query: String },
    /// Minimalizuje okno
    Minimize { query: String },
    /// Maximalizuje okno (--off vrátí)
    Maximize {
        query: String,
        #[arg(long)]
        off: bool,
    },
    /// Přepne okno do fullscreenu (--off vrátí)
    Fullscreen {
        query: String,
        #[arg(long)]
        off: bool,
    },
    /// Přesune okno na souřadnice
    Move {
        query: String,
        #[arg(allow_negative_numbers = true)]
        x: i32,
        #[arg(allow_negative_numbers = true)]
        y: i32,
    },
    /// Změní velikost okna
    Resize { query: String, width: u32, height: u32 },
    /// Počká, až se objeví okno (např. po startu aplikace)
    Wait {
        query: String,
        #[arg(long, default_value_t = 10)]
        timeout_s: u64,
    },
    /// Spustí aplikaci (odpojeně) a počká na její okno. Mimo terminál jen
    /// programy z wm.spawn_allowed. Vlastní přepínače patří PŘED program:
    /// `jarvis wm spawn --window Signal signal-desktop`
    Spawn {
        /// Program (jméno v PATH nebo absolutní cesta) a jeho argumenty
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Okno, na které čekat (část třídy/titulku) — pro aplikace, které
        /// předávají běžící instanci; bez zadání se čeká na jakékoli NOVÉ okno
        #[arg(long)]
        window: Option<String>,
        /// Jak dlouho čekat na okno
        #[arg(long, default_value_t = 15)]
        timeout_s: u64,
        /// Jen spustit, na okno nečekat
        #[arg(long)]
        no_wait: bool,
    },
    /// Napíše text do aktivního okna (XTest; umí diakritiku, "\n" = Enter)
    Type {
        /// Text; víc argumentů se spojí mezerou
        text: Vec<String>,
        /// Nejdřív aktivovat toto okno (bezpečnější než psát naslepo)
        #[arg(long)]
        window: Option<String>,
        /// Po textu stisknout Enter
        #[arg(long)]
        enter: bool,
        /// Rozestup kláves v ms (default z configu wm.key_delay_ms)
        #[arg(long)]
        delay_ms: Option<u64>,
    },
    /// Stiskne zkratky, např. `key ctrl+f` nebo `key ctrl+a Delete`
    Key { combos: Vec<String> },
    /// Klikne myší na souřadnice (--button 3 = pravé, --double)
    Click {
        x: u16,
        y: u16,
        #[arg(long, default_value_t = 1)]
        button: u8,
        #[arg(long)]
        double: bool,
    },
    /// Přesune kurzor myši
    Pointer { x: u16, y: u16 },
    /// Uloží screenshot (celá obrazovka, s --window jen okno) a vypíše cestu
    Screenshot {
        #[arg(long)]
        window: Option<String>,
        /// Cílový soubor (default ~/.local/share/jarvis/wm-screenshot.jpg)
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

pub fn cli(paths: &Paths, cfg: &Config, cmd: WmCmd) -> Result<()> {
    let delay = match &cmd {
        WmCmd::Type { delay_ms: Some(d), .. } => *d,
        _ => cfg.wm.key_delay_ms,
    };
    let wm = Wm::connect(delay)?;
    match cmd {
        WmCmd::List => {
            let active = wm.active_id();
            for w in wm.windows()? {
                let mark = if active == Some(w.id) { '*' } else { ' ' };
                println!(
                    "{mark}0x{:07x}  d{:<2} {:<24} {}",
                    w.id,
                    w.desktop_label(),
                    w.class,
                    w.title
                );
            }
        }
        WmCmd::Active => match wm.active() {
            Some(m) => println!("{m}"),
            None => println!("žádné aktivní okno"),
        },
        WmCmd::Focus { query } => {
            let m = wm.resolve(&query)?;
            let m = wm.activate(m.id)?;
            println!("aktivní: {m}");
        }
        WmCmd::Close { query } => {
            let m = wm.resolve(&query)?;
            wm.close(m.id)?;
            println!("žádost o zavření odeslána: {m}");
        }
        WmCmd::Minimize { query } => {
            let m = wm.resolve(&query)?;
            wm.minimize(m.id)?;
            println!("minimalizováno: {m}");
        }
        WmCmd::Maximize { query, off } => {
            let m = wm.resolve(&query)?;
            wm.set_maximized(m.id, !off)?;
            println!("{}: {m}", if off { "maximalizace zrušena" } else { "maximalizováno" });
        }
        WmCmd::Fullscreen { query, off } => {
            let m = wm.resolve(&query)?;
            wm.set_fullscreen(m.id, !off)?;
            println!("{}: {m}", if off { "fullscreen zrušen" } else { "fullscreen" });
        }
        WmCmd::Move { query, x, y } => {
            let m = wm.resolve(&query)?;
            wm.move_window(m.id, x, y)?;
            println!("přesunuto na {x},{y}: {m}");
        }
        WmCmd::Resize { query, width, height } => {
            let m = wm.resolve(&query)?;
            wm.resize_window(m.id, width, height)?;
            println!("velikost {width}×{height}: {m}");
        }
        WmCmd::Wait { query, timeout_s } => {
            let m = wm.wait(&query, Duration::from_secs(timeout_s))?;
            println!("okno je tu: {m}");
        }
        WmCmd::Spawn { command, window, timeout_s, no_wait } => {
            let program = &command[0];
            ensure_spawn_allowed(program, &cfg.wm.spawn_allowed)?;
            let before: std::collections::HashSet<Window> =
                wm.windows()?.iter().map(|w| w.id).collect();
            let pid = spawn_detached(program, &command[1..], &paths.data_dir)?;
            println!("spuštěno: {} (pid {pid})", command.join(" "));
            if no_wait {
                return Ok(());
            }
            let timeout = Duration::from_secs(timeout_s);
            let found = match &window {
                Some(q) => wm.wait(q, timeout),
                None => wm.wait_new(&before, timeout),
            };
            match found {
                // aktivace best-effort: fullscreen aplikace umí fokus
                // odmítnout, ale okno tu je a program běží — to je úspěch
                Ok(m) => match wm.activate(m.id) {
                    Ok(m) => println!("aktivní: {m}"),
                    Err(e) => println!("okno je tu: {m} (aktivace selhala: {e})"),
                },
                // proces žije, jen okno nikde — pro agenta poctivá zpráva
                // místo chyby (single-instance aplikace předala běžící
                // instanci, nebo start trvá déle)
                Err(e) => println!(
                    "{e} — proces (pid {pid}) běží; aplikace možná předala \
                     běžící instanci, zkontroluj `jarvis wm list` nebo screenshot"
                ),
            }
        }
        WmCmd::Type { text, window, enter, .. } => {
            if let Some(q) = window {
                let m = wm.resolve(&q)?;
                wm.activate(m.id)?;
            }
            let text = unescape(&text.join(" "));
            ensure!(
                !text.is_empty() || enter,
                "prázdný text (bez --enter není co psát)"
            );
            let target = wm
                .active()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "neznámé okno".into());
            let n = wm.type_text(&text)?;
            if enter {
                wm.key_combo("Return")?;
            }
            println!(
                "napsáno {n} znaků{} do: {target}",
                if enter { " + Enter" } else { "" }
            );
        }
        WmCmd::Key { combos } => {
            ensure!(!combos.is_empty(), "žádná zkratka (např. `jarvis wm key ctrl+f`)");
            for c in &combos {
                wm.key_combo(c)?;
                println!("stisknuto: {c}");
            }
        }
        WmCmd::Click { x, y, button, double } => {
            wm.click(x as i16, y as i16, button, double)?;
            println!(
                "klik{} button {button} na {x},{y}",
                if double { " (dvojitý)" } else { "" }
            );
        }
        WmCmd::Pointer { x, y } => {
            wm.move_pointer(x as i16, y as i16)?;
            println!("kurzor na {x},{y}");
        }
        WmCmd::Screenshot { window, out } => {
            let out = match out {
                Some(p) => confine_to(&p, &paths.data_dir)?,
                None => paths.data_dir.join("wm-screenshot.jpg"),
            };
            wm.screenshot(window.as_deref(), &out)?;
            println!("{}", out.display());
        }
    }
    Ok(())
}

/// Neomezený spawn (mimo allowlist) je vědomé rozhodnutí člověka u klávesnice.
/// Samotné `is_terminal()` je ale děditelná ambientní autorita — pty agenta
/// nebo špatně nastavený unit ji zdědí a obešly by allowlist. Proto navíc
/// vyžadujeme explicitní `JARVIS_WM_UNRESTRICTED=1`, který si člověk nastaví
/// ve svém shellu (démon ani agent ho nemají). Bez něj platí allowlist vždy.
fn interactive_human() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::env::var_os("JARVIS_WM_UNRESTRICTED").is_some()
}

/// Mimo neomezený terminál (hlasový agent, timery, běžný shell) smí spawn jen
/// programy z wm.spawn_allowed — fail-closed.
fn ensure_spawn_allowed(program: &str, allowed: &[String]) -> Result<()> {
    if interactive_human() || spawn_permitted(program, allowed) {
        return Ok(());
    }
    bail!(
        "program „{program}“ není ve wm.spawn_allowed — přidej ho v \
         ~/.config/jarvis/config.toml (sekce [wm]), nebo z terminálu povol \
         neomezený spawn: export JARVIS_WM_UNRESTRICTED=1"
    )
}

/// `--out` screenshotu smí mířit jen dovnitř `base` (relativní cesta bez „..“
/// a bez absolutního kořene). Agent omezený na `jarvis wm` by jinak přes
/// `--out` zapsal JPEG bajty kamkoli (přepsal config, allowlist, cokoli
/// zapisovatelného).
fn confine_to(requested: &std::path::Path, base: &std::path::Path) -> Result<PathBuf> {
    use std::path::Component;
    for c in requested.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!(
                "--out smí být jen relativní cesta uvnitř {} (bez „/“ a „..“)",
                base.display()
            ),
        }
    }
    Ok(base.join(requested))
}

/// Přesná shoda s položkou seznamu: holé jméno povoluje jen holé jméno
/// (binárku najde PATH služby), absolutní cesta jen tu samou cestu. Žádné
/// basename triky — agent nesmí podvrhnout vlastní binárku mimo PATH.
/// Pozor: shoduje se jen program, ne argumenty — na allowlist patří jen
/// listové aplikace; program schopný spustit další příkaz (xterm -e, flatpak
/// run, env…) by allowlist obešel (viz config.example.toml).
fn spawn_permitted(program: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| a == program)
}

/// Spustí program odpojeně: vlastní process group (signály Jarvise ho
/// nezabijí), stdin z /dev/null, výstup do spawn.log (ladění startů).
fn spawn_detached(program: &str, args: &[String], data_dir: &std::path::Path) -> Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let log_path = data_dir.join("spawn.log");
    // aplikace do logu sypou svůj stdout — nesmí růst donekonečna
    if std::fs::metadata(&log_path).map(|m| m.len() > 10_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&log_path);
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .ok()
        .and_then(|mut f| {
            use std::io::Write;
            let _ = writeln!(
                f,
                "--- {} spawn: {program} {}",
                crate::util::fmt_local(crate::util::now_ts()),
                args.join(" ")
            );
            f.try_clone().ok().map(|c| (f, c))
        });
    let (out, err) = match log {
        Some((f, c)) => (Stdio::from(f), Stdio::from(c)),
        None => (Stdio::null(), Stdio::null()),
    };
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow::anyhow!("program „{program}“ nenalezen v PATH")
            }
            _ => anyhow::anyhow!("nelze spustit „{program}“: {e}"),
        })?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_allowlist_exact_match_only() {
        let allowed = vec!["firefox".to_string(), "/opt/tools/backup".to_string()];
        assert!(spawn_permitted("firefox", &allowed));
        assert!(spawn_permitted("/opt/tools/backup", &allowed));
        // cesta ≠ holé jméno: podvržená binárka mimo PATH neprojde
        assert!(!spawn_permitted("/tmp/evil/firefox", &allowed));
        assert!(!spawn_permitted("fire", &allowed));
        assert!(!spawn_permitted("firefoxx", &allowed));
        assert!(!spawn_permitted("bash", &allowed));
        assert!(!spawn_permitted("cokoli", &[]));
    }

    #[test]
    fn spawn_denied_without_tty_and_allowlist() {
        // testy běží bez TTY → prázdný allowlist musí odmítnout
        assert!(ensure_spawn_allowed("xclock", &[]).is_err());
        assert!(ensure_spawn_allowed("xclock", &["xclock".to_string()]).is_ok());
    }

    #[test]
    fn keysym_ascii_latin1_unicode_and_controls() {
        assert_eq!(keysym_for_char('a'), 0x61);
        assert_eq!(keysym_for_char('A'), 0x41);
        assert_eq!(keysym_for_char(' '), 0x20);
        assert_eq!(keysym_for_char('á'), 0xe1); // Latin-1 přímo
        assert_eq!(keysym_for_char('š'), 0x0100_0161); // Unicode keysym
        assert_eq!(keysym_for_char('ř'), 0x0100_0159);
        assert_eq!(keysym_for_char('€'), 0x0100_20ac);
        assert_eq!(keysym_for_char('\n'), XK_RETURN);
        assert_eq!(keysym_for_char('\t'), 0xff09);
    }

    #[test]
    fn named_keys() {
        assert_eq!(named_keysym("Return"), Some(XK_RETURN));
        assert_eq!(named_keysym("enter"), Some(XK_RETURN));
        assert_eq!(named_keysym("ESC"), Some(0xff1b));
        assert_eq!(named_keysym("f1"), Some(0xffbe));
        assert_eq!(named_keysym("F12"), Some(0xffc9));
        assert_eq!(named_keysym("f25"), None);
        assert_eq!(named_keysym("nesmysl"), None);
    }

    #[test]
    fn combo_parsing() {
        assert_eq!(parse_combo("ctrl+f").unwrap(), (vec![XK_CONTROL_L], 0x66));
        assert_eq!(
            parse_combo("ctrl+shift+t").unwrap(),
            (vec![XK_CONTROL_L, XK_SHIFT_L], 0x74)
        );
        assert_eq!(parse_combo("Return").unwrap(), (vec![], XK_RETURN));
        assert_eq!(parse_combo("alt+F4").unwrap(), (vec![XK_ALT_L], 0xffc1));
        assert_eq!(parse_combo("ctrl++").unwrap(), (vec![XK_CONTROL_L], 0x2b));
        assert_eq!(parse_combo("+").unwrap(), (vec![], 0x2b));
        assert!(parse_combo("bogus+x").is_err());
        assert!(parse_combo("ctrl+dlouhé").is_err());
        assert!(parse_combo("").is_err());
    }

    fn synth_keymap() -> Keymap {
        // kc8: a/A, kc9: Return, kc10: volný, kc11: Shift_L, kc12: plus/1
        Keymap {
            min: 8,
            per: 2,
            syms: vec![
                0x61, 0x41, // 8
                XK_RETURN, 0, // 9
                0, 0, // 10
                XK_SHIFT_L, 0, // 11
                0x2b, 0x31, // 12
            ],
            borrowed: None,
            bound_sym: None,
        }
    }

    #[test]
    fn keymap_find_and_spare() {
        let km = synth_keymap();
        assert_eq!(km.find(0x61), Some((8, false)));
        assert_eq!(km.find(0x41), Some((8, true))); // A = Shift+a
        assert_eq!(km.find(XK_RETURN), Some((9, false)));
        assert_eq!(km.find(0x31), Some((12, true))); // 1 nad plusem (cz layout)
        assert_eq!(km.find(0x0100_0161), None); // š nemapované
        assert_eq!(km.find_any(XK_SHIFT_L), Some(11));
        assert_eq!(km.spare(), Some(10));
        assert_eq!(km.max_keycode(), 12);
    }

    fn w(id: u32, class: &str, title: &str) -> WinMeta {
        WinMeta { id, desktop: Some(0), class: class.into(), title: title.into() }
    }

    #[test]
    fn pick_prefers_class_then_title_then_stacking() {
        let wins = vec![
            w(1, "Navigator", "Signal blog — Firefox"),
            w(2, "Signal", "Signal"),
            w(3, "Signal", "Signal — druhé okno"),
            w(4, "Xfce4-terminal", "vim signal.rs"),
        ];
        // stacking zdola nahoru: 3 je výš než 2
        let stacking = vec![1, 2, 3, 4];
        // přesná třída vyhrává nad podřetězcem titulku, topmost vyhrává remízu
        assert_eq!(pick_window(&wins, &stacking, "signal"), Some(2));
        // podřetězec třídy
        assert_eq!(pick_window(&wins, &stacking, "terminal"), Some(3));
        // jen titulek
        assert_eq!(pick_window(&wins, &stacking, "firefox"), Some(0));
        assert_eq!(pick_window(&wins, &stacking, "nic-takového"), None);
        assert_eq!(pick_window(&wins, &stacking, ""), None);
    }

    #[test]
    fn clamp_rect_cases() {
        assert_eq!(clamp_rect(10, 10, 100, 50, 1920, 1080), Some((10, 10, 100, 50)));
        // přesah vpravo dole
        assert_eq!(clamp_rect(1900, 1060, 100, 100, 1920, 1080), Some((1900, 1060, 20, 20)));
        // záporný počátek
        assert_eq!(clamp_rect(-30, -20, 100, 100, 1920, 1080), Some((0, 0, 70, 80)));
        // úplně mimo
        assert_eq!(clamp_rect(2000, 0, 100, 100, 1920, 1080), None);
        assert_eq!(clamp_rect(0, -200, 100, 100, 1920, 1080), None);
    }

    #[test]
    fn unescape_translates_literals() {
        assert_eq!(unescape("ahoj\\nsvěte\\tkonec"), "ahoj\nsvěte\tkonec");
        assert_eq!(unescape("beze změny"), "beze změny");
    }
}
