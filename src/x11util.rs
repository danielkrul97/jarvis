//! Sdílené nízkoúrovňové X11 helpery pro capture (snímání) a wm (ovládání
//! oken). Čtení vlastností nikdy nepanikaří — okno může zmizet mezi dotazy,
//! titulky můžou být ne-UTF8.

use anyhow::{bail, Context, Result};
use image::RgbImage;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, Window};

/// UTF-8 lossy, bez řídicích znaků, oříznuto na max_chars.
pub fn sanitize(bytes: &[u8], max_chars: usize) -> String {
    let s: String = String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect();
    s.trim().to_string()
}

pub fn get_prop_bytes<C: Connection>(conn: &C, window: Window, prop: Atom) -> Option<Vec<u8>> {
    let reply = conn
        .get_property(false, window, prop, AtomEnum::ANY, 0, 4096)
        .ok()?
        .reply()
        .ok()?;
    Some(reply.value)
}

pub fn get_prop_u32<C: Connection>(conn: &C, window: Window, prop: Atom) -> Option<u32> {
    let reply = conn
        .get_property(false, window, prop, AtomEnum::ANY, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    reply.value32().and_then(|mut it| it.next())
}

/// Seznam oken z vlastnosti typu WINDOW[] (např. _NET_CLIENT_LIST).
pub fn get_prop_windows<C: Connection>(conn: &C, window: Window, prop: Atom) -> Vec<Window> {
    conn.get_property(false, window, prop, AtomEnum::ANY, 0, 4096)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|it| it.collect()))
        .unwrap_or_default()
}

/// _NET_WM_NAME (UTF-8), fallback WM_NAME.
pub fn window_title<C: Connection>(conn: &C, win: Window, atom_net_wm_name: Atom) -> String {
    let bytes = get_prop_bytes(conn, win, atom_net_wm_name)
        .filter(|b| !b.is_empty())
        .or_else(|| get_prop_bytes(conn, win, AtomEnum::WM_NAME.into()))
        .unwrap_or_default();
    sanitize(&bytes, 500)
}

/// WM_CLASS = "instance\0class\0" — bereme class, fallback instance.
pub fn window_class<C: Connection>(conn: &C, win: Window) -> String {
    let bytes = get_prop_bytes(conn, win, AtomEnum::WM_CLASS.into()).unwrap_or_default();
    let parts: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| sanitize(p, 120))
        .collect();
    parts.get(1).or_else(|| parts.first()).cloned().unwrap_or_default()
}

/// Převod dat GetImage (Z_PIXMAP, depth 24/32) na RGB obraz.
/// `pixmap_formats` = (depth, bits_per_pixel) ze setupu spojení.
pub fn zpixmap_to_rgb(
    data: &[u8],
    depth: u8,
    w: u16,
    h: u16,
    lsb: bool,
    pixmap_formats: &[(u8, u8)],
) -> Result<RgbImage> {
    if depth != 24 && depth != 32 {
        bail!("nepodporovaná hloubka obrazu: {depth} (čekám 24/32)");
    }
    let (w_us, h_us) = (usize::from(w), usize::from(h));
    if h_us == 0 || data.len() < h_us {
        bail!("GetImage vrátil prázdná data");
    }
    let stride = data.len() / h_us;
    // bits_per_pixel ze setupu; fallback na odvození ze stride
    let bytes_px = pixmap_formats
        .iter()
        .find(|(d, _)| *d == depth)
        .map(|(_, bpp)| usize::from(*bpp) / 8)
        .unwrap_or(stride / w_us);
    if bytes_px != 3 && bytes_px != 4 {
        bail!("neočekávaný formát pixelu: {bytes_px} B/px (depth {depth})");
    }
    if stride < w_us * bytes_px {
        bail!("GetImage: řádek kratší než šířka ({stride} < {})", w_us * bytes_px);
    }
    let mut out = Vec::with_capacity(w_us * h_us * 3);
    for y in 0..h_us {
        let row = &data[y * stride..y * stride + w_us * bytes_px];
        for x in 0..w_us {
            let i = x * bytes_px;
            let (r, g, b) = if lsb {
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

/// Uloží RGB obraz jako JPEG q90 (stejný formát jako capture snímky).
pub fn save_jpeg(img: &RgbImage, path: &std::path::Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("nelze vytvořit {}", dir.display()))?;
    }
    let f = std::fs::File::create(path)
        .with_context(|| format!("nelze vytvořit {}", path.display()))?;
    let mut writer = std::io::BufWriter::new(f);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 90)
        .encode_image(img)
        .with_context(|| format!("JPEG encode selhal pro {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn zpixmap_rejects_bad_depth_and_empty() {
        assert!(zpixmap_to_rgb(&[0u8; 16], 16, 2, 2, true, &[]).is_err());
        assert!(zpixmap_to_rgb(&[], 24, 2, 2, true, &[(24, 32)]).is_err());
    }

    #[test]
    fn zpixmap_converts_bgrx_lsb() {
        // 1×1, BGRX little-endian: B=1 G=2 R=3
        let img = zpixmap_to_rgb(&[1, 2, 3, 0], 24, 1, 1, true, &[(24, 32)]).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [3, 2, 1]);
    }
}
