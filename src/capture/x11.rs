use crate::x11util;
use anyhow::{Context, Result};
use image::RgbImage;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::screensaver::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{Atom, ConnectionExt as _, ImageFormat, ImageOrder, Window};
use x11rb::rust_connection::RustConnection;

pub struct X11 {
    conn: RustConnection,
    root: Window,
    width: u16,
    height: u16,
    lsb: bool,
    has_screensaver: bool,
    /// (depth, bits_per_pixel) from setup — for correctly interpreting GetImage
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

    /// Cheap round-trip that surfaces connection errors — metadata queries
    /// mask errors (a window can legitimately disappear), this catches a
    /// dead connection right away.
    pub fn probe(&self) -> Result<()> {
        self.conn
            .get_input_focus()
            .context("X spojení: request selhal")?
            .reply()
            .context("X spojení: reply selhal")?;
        Ok(())
    }

    /// ms since the last user input; 0 if it can't be determined.
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

    /// Active window metadata. Errors (window disappeared, etc.) return
    /// empty values, not Err — only a lost connection is fatal, and that's
    /// caught by capture_screen or intern in connect().
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

    fn get_prop_u32(&self, window: Window, prop: Atom) -> Option<u32> {
        x11util::get_prop_u32(&self.conn, window, prop)
    }

    fn get_cardinal(&self, window: Window, prop: Atom) -> Option<i64> {
        self.get_prop_u32(window, prop).map(i64::from)
    }

    fn window_title(&self, win: Window) -> String {
        x11util::window_title(&self.conn, win, self.atom_net_wm_name)
    }

    fn window_class(&self, win: Window) -> String {
        x11util::window_class(&self.conn, win)
    }

    /// The full root window (with one monitor = the whole screen) as RGB.
    pub fn capture_screen(&self) -> Result<RgbImage> {
        // root dimensions are read live: resolution changes at runtime
        // (monitor hotplug, xrandr). A cache from `connect()` would, after
        // the root grows, leave GetImage requesting only the original
        // (smaller) region — still inside the new root, so it wouldn't
        // error, just silently crop the shot. GetGeometry = 1 roundtrip/frame.
        let geom = self
            .conn
            .get_geometry(self.root)
            .context("GetGeometry request selhal")?
            .reply()
            .context("GetGeometry reply selhal")?;
        let (w, h) = (geom.width, geom.height);
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, u32::MAX)
            .context("GetImage request selhal")?
            .reply()
            .context("GetImage reply selhal")?;
        x11util::zpixmap_to_rgb(&reply.data, reply.depth, w, h, self.lsb, &self.pixmap_formats)
    }
}
