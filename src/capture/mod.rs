pub mod dedup;
pub mod x11;

use crate::config::{Blacklist, Config, Paths};
use crate::store::{db, retention};
use crate::util;
use anyhow::{bail, Context, Result};
use image::imageops::{self, FilterType};
use image::RgbImage;
use rusqlite::Connection;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Exclusive flock against double capture (systemd service × manual `capture`
/// × `jarvis run`). The lock is held by the returned File — the caller must
/// keep it alive.
pub fn acquire_lock(paths: &Paths) -> Result<std::fs::File> {
    let path = paths.data_dir.join("capture.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("nelze otevřít {}", path.display()))?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!(
            "capture už běží v jiném procesu — `systemctl --user status jarvis-capture` \
             (nebo jiný `jarvis capture`/`jarvis run`)"
        );
    }
    Ok(f)
}

/// Capture daemon: runs in a loop until something kills it (systemd, Ctrl-C).
pub fn run_capture(paths: &Paths, cfg: &Config) -> Result<()> {
    let bl = Blacklist::new(&cfg.capture)?;
    let conn = db::open(&paths.db_path)?;
    let mut x = x11::X11::connect()?;
    let (w, h) = x.geometry();
    info!(
        "capture běží: {w}x{h}, metadata à {} s, screenshot à {} s, idle práh {} s",
        cfg.capture.meta_interval_s, cfg.capture.shot_interval_s, cfg.capture.idle_threshold_s
    );

    let mut last_shot_try: i64 = 0;
    let mut last_phash: Option<u64> =
        db::state_get(&conn, "last_phash")?.and_then(|v| v.parse().ok());

    loop {
        let tick = Instant::now();
        if let Err(e) = step(&x, &conn, cfg, &bl, paths, &mut last_shot_try, &mut last_phash) {
            warn!("tick selhal: {e:#} — zkouším obnovit X spojení");
            std::thread::sleep(Duration::from_secs(3));
            match x11::X11::connect() {
                Ok(nx) => {
                    x = nx;
                    info!("X11 spojení obnoveno");
                }
                Err(e2) => warn!("reconnect selhal: {e2:#}"),
            }
        }
        housekeeping(&conn, paths, cfg);
        let interval = Duration::from_secs(cfg.capture.meta_interval_s);
        let elapsed = tick.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

fn step(
    x: &x11::X11,
    conn: &Connection,
    cfg: &Config,
    bl: &Blacklist,
    paths: &Paths,
    last_shot_try: &mut i64,
    last_phash: &mut Option<u64>,
) -> Result<()> {
    let now = util::now_ts();
    // Privacy: during pause, NOTHING gets stored — not even window titles.
    if db::pause_until(conn, now)?.is_some() {
        return Ok(());
    }
    // Cheap connection probe — otherwise a dead X connection (metadata
    // errors are masked) would only surface at the next screenshot.
    x.probe()?;

    let info = x.active_window_info();
    let idle_ms = x.idle_ms() as i64;
    let blacklisted = bl.matches(&info.wm_class, &info.title);
    // For blacklisted windows we don't store even the title — just the window class.
    let title = if blacklisted { "[blacklisted]".to_string() } else { info.title };

    let mut shot_path: Option<String> = None;
    let mut phash: Option<i64> = None;

    let due = now - *last_shot_try >= cfg.capture.shot_interval_s as i64;
    let user_active = idle_ms < (cfg.capture.idle_threshold_s * 1000) as i64;
    if due && user_active && !blacklisted {
        *last_shot_try = now;
        let img = x.capture_screen()?;
        let img = downscale(img, cfg.capture.max_dimension);
        let hash = dedup::dhash(&img);
        let dist = last_phash.map(|p| dedup::hamming(p, hash)).unwrap_or(u32::MAX);
        if dist >= cfg.capture.phash_min_distance {
            let rel = save_shot(&img, paths, now)?;
            *last_phash = Some(hash);
            db::state_set(conn, "last_phash", &hash.to_string())?;
            debug!("screenshot {rel} (hamming {dist})");
            shot_path = Some(rel);
            phash = Some(hash as i64);
        } else {
            debug!("screenshot přeskočen — obrazovka se nezměnila (hamming {dist})");
        }
    }

    db::insert_sample(
        conn,
        now,
        &info.wm_class,
        &title,
        info.desktop,
        idle_ms,
        shot_path.as_deref(),
        phash,
    )?;
    Ok(())
}

/// Downscales the image so the longer side is at most `max_dim`.
fn downscale(img: RgbImage, max_dim: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    let longer = w.max(h);
    if longer <= max_dim {
        return img;
    }
    let scale = f64::from(max_dim) / f64::from(longer);
    let nw = (f64::from(w) * scale).round().max(1.0) as u32;
    let nh = (f64::from(h) * scale).round().max(1.0) as u32;
    imageops::resize(&img, nw, nh, FilterType::Triangle)
}

/// Saves a JPEG to shots/YYYY-MM-DD/HHMMSS.jpg; returns the path relative to data_dir.
fn save_shot(img: &RgbImage, paths: &Paths, ts: i64) -> Result<String> {
    let (day, file) = util::shot_rel_path(ts);
    let dir = paths.shots_dir.join(&day);
    std::fs::create_dir_all(&dir).with_context(|| format!("nelze vytvořit {}", dir.display()))?;
    let path = dir.join(&file);
    let f = std::fs::File::create(&path)
        .with_context(|| format!("nelze vytvořit {}", path.display()))?;
    let mut writer = std::io::BufWriter::new(f);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 90)
        .encode_image(img)
        .with_context(|| format!("JPEG encode selhal pro {}", path.display()))?;
    Ok(format!("shots/{day}/{file}"))
}

/// Hourly housekeeping (retention) — errors are just logged, the daemon keeps running.
fn housekeeping(conn: &Connection, paths: &Paths, cfg: &Config) {
    let now = util::now_ts();
    let last = db::state_get_i64(conn, "last_purge").ok().flatten().unwrap_or(0);
    if now - last < 3600 {
        return;
    }
    match retention::purge(
        conn,
        &paths.data_dir,
        (cfg.retention.screenshots_days * 86400) as i64,
    ) {
        Ok(0) => {}
        Ok(n) => info!("retence: odstraněno {n} starých snímků"),
        Err(e) => warn!("retence selhala: {e:#}"),
    }
    if let Err(e) = db::state_set(conn, "last_purge", &now.to_string()) {
        warn!("nelze zapsat last_purge: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::downscale;
    use image::RgbImage;

    #[test]
    fn downscale_caps_longer_side() {
        let img = RgbImage::new(1920, 1080);
        let out = downscale(img, 1568);
        assert_eq!(out.dimensions().0, 1568);
        assert_eq!(out.dimensions().1, 882);
    }

    #[test]
    fn downscale_keeps_small_images() {
        let img = RgbImage::new(800, 600);
        assert_eq!(downscale(img, 1568).dimensions(), (800, 600));
    }
}
