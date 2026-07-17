use anyhow::{bail, Context, Result};
use image::RgbImage;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::screensaver::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, Window};
use x11rb::rust_connection::RustConnection;

pub struct X11 {
    conn: RustConnection,
    root: Window,
    width: u16,
    height: u16,
    lsb: bool,
    has_screensaver: bool,
    /// (depth, bits_per_pixel) ze setupu — pro správnou interpretaci GetImage
    pixmap_formats: Vec<(u8, u8)>,
    atom_net_active_window: Atom,
    atom_net_wm_name: Atom,
    atom_net_current_desktop: Atom,
}

#[derive(Debug, Default, Clone)]
pub struct WindowInfo {
    pub wm_class: String,
    pub title: String,
    pub desktop: Option<i64>,
}

impl X11 {
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) =
            x11rb::connect(None).context("nelze se připojit k X serveru (DISPLAY?)")?;
        let (lsb, root, width, height, pixmap_formats) = {
            let setup = conn.setup();
            let screen = &setup.roots[screen_num];
            (
                setup.image_byte_order == ImageOrder::LSB_FIRST,
                screen.root,
                screen.width_in_pixels,
                screen.height_in_pixels,
                setup
                    .pixmap_formats
                    .iter()
                    .map(|f| (f.depth, f.bits_per_pixel))
                    .collect::<Vec<_>>(),
            )
        };
        let has_screensaver = conn
            .extension_information(screensaver::X11_EXTENSION_NAME)?
            .is_some();
        let intern = |name: &[u8]| -> Result<Atom> {
            Ok(conn.intern_atom(false, name)?.reply()?.atom)
        };
        let atom_net_active_window = intern(b"_NET_ACTIVE_WINDOW")?;
        let atom_net_wm_name = intern(b"_NET_WM_NAME")?;
        let atom_net_current_desktop = intern(b"_NET_CURRENT_DESKTOP")?;
        Ok(Self {
            conn,
            root,
            width,
            height,
            lsb,
            has_screensaver,
            pixmap_formats,
            atom_net_active_window,
            atom_net_wm_name,
            atom_net_current_desktop,
        })
    }

    pub fn geometry(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn has_screensaver(&self) -> bool {
        self.has_screensaver
    }

    /// Levný round-trip, který propaguje chybu spojení — metadata dotazy chyby
    /// maskují (okno může legitimně zmizet), tohle odhalí mrtvé spojení hned.
    pub fn probe(&self) -> Result<()> {
        self.conn
            .get_input_focus()
            .context("X spojení: request selhal")?
            .reply()
            .context("X spojení: reply selhal")?;
        Ok(())
    }

    /// ms od posledního vstupu uživatele; 0 pokud nelze zjistit.
    pub fn idle_ms(&self) -> u64 {
        if !self.has_screensaver {
            return 0;
        }
        self.conn
            .screensaver_query_info(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| u64::from(r.ms_since_user_input))
            .unwrap_or(0)
    }

    /// Metadata aktivního okna. Chyby (okno zmizelo apod.) vrací prázdné hodnoty,
    /// ne Err — fatální je jen ztráta spojení, kterou zachytí volání capture_screen
    /// nebo intern v connect().
    pub fn active_window_info(&self) -> WindowInfo {
        let desktop = self.get_cardinal(self.root, self.atom_net_current_desktop);
        let win = self
            .get_prop_u32(self.root, self.atom_net_active_window)
            .unwrap_or(0);
        if win == 0 {
            return WindowInfo { desktop, ..Default::default() };
        }
        let title = self.window_title(win);
        let wm_class = self.window_class(win);
        WindowInfo { wm_class, title, desktop }
    }

    fn get_prop_bytes(&self, window: Window, prop: Atom) -> Option<Vec<u8>> {
        let reply = self
            .conn
            .get_property(false, window, prop, AtomEnum::ANY, 0, 4096)
            .ok()?
            .reply()
            .ok()?;
        Some(reply.value)
    }

    fn get_prop_u32(&self, window: Window, prop: Atom) -> Option<u32> {
        let reply = self
            .conn
            .get_property(false, window, prop, AtomEnum::ANY, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        reply.value32().and_then(|mut it| it.next())
    }

    fn get_cardinal(&self, window: Window, prop: Atom) -> Option<i64> {
        self.get_prop_u32(window, prop).map(i64::from)
    }

    fn window_title(&self, win: Window) -> String {
        let bytes = self
            .get_prop_bytes(win, self.atom_net_wm_name)
            .filter(|b| !b.is_empty())
            .or_else(|| self.get_prop_bytes(win, AtomEnum::WM_NAME.into()))
            .unwrap_or_default();
        sanitize(&bytes, 500)
    }

    fn window_class(&self, win: Window) -> String {
        let bytes = self
            .get_prop_bytes(win, AtomEnum::WM_CLASS.into())
            .unwrap_or_default();
        // WM_CLASS = "instance\0class\0" — bereme class, fallback instance
        let parts: Vec<String> = bytes
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| sanitize(p, 120))
            .collect();
        parts.get(1).or_else(|| parts.first()).cloned().unwrap_or_default()
    }

    /// Celý root (u jednoho monitoru = celá obrazovka) jako RGB.
    pub fn capture_screen(&self) -> Result<RgbImage> {
        let (w, h) = (self.width, self.height);
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, u32::MAX)
            .context("GetImage request selhal")?
            .reply()
            .context("GetImage reply selhal")?;
        if reply.depth != 24 && reply.depth != 32 {
            bail!("nepodporovaná hloubka obrazu: {} (čekám 24/32)", reply.depth);
        }
        let (w_us, h_us) = (usize::from(w), usize::from(h));
        let data = reply.data;
        if h_us == 0 || data.len() < h_us {
            bail!("GetImage vrátil prázdná data");
        }
        let stride = data.len() / h_us;
        // bits_per_pixel ze setupu; fallback na odvození ze stride
        let bytes_px = self
            .pixmap_formats
            .iter()
            .find(|(d, _)| *d == reply.depth)
            .map(|(_, bpp)| usize::from(*bpp) / 8)
            .unwrap_or(stride / w_us);
        if bytes_px != 3 && bytes_px != 4 {
            bail!("neočekávaný formát pixelu: {bytes_px} B/px (depth {})", reply.depth);
        }
        if stride < w_us * bytes_px {
            bail!("GetImage: řádek kratší než šířka ({stride} < {})", w_us * bytes_px);
        }
        let mut out = Vec::with_capacity(w_us * h_us * 3);
        for y in 0..h_us {
            let row = &data[y * stride..y * stride + w_us * bytes_px];
            for x in 0..w_us {
                let i = x * bytes_px;
                let (r, g, b) = if self.lsb {
                    (row[i + 2], row[i + 1], row[i]) // BGR(X)
                } else if bytes_px == 4 {
                    (row[i + 1], row[i + 2], row[i + 3]) // XRGB
                } else {
                    (row[i], row[i + 1], row[i + 2]) // RGB
                };
                out.push(r);
                out.push(g);
                out.push(b);
            }
        }
        RgbImage::from_raw(u32::from(w), u32::from(h), out)
            .context("nelze složit RGB obraz z X dat")
    }
}

/// UTF-8 lossy, bez řídicích znaků, oříznuto na max_chars.
fn sanitize(bytes: &[u8], max_chars: usize) -> String {
    let s: String = String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect();
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitize_strips_controls_and_trims() {
        // tab i newline jsou řídicí znaky, filtrují se bez náhrady
        assert_eq!(sanitize(b"  vim\tPLAN.md\n", 100), "vimPLAN.md");
    }

    #[test]
    fn sanitize_handles_invalid_utf8() {
        let s = sanitize(&[0x66, 0x6f, 0xff, 0x6f], 100);
        assert!(s.starts_with("fo"));
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "á".repeat(1000);
        assert_eq!(sanitize(long.as_bytes(), 10).chars().count(), 10);
    }
}
