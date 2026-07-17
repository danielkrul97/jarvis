use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, TimeZone};

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn fmt_local(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        chrono::LocalResult::None => format!("ts:{ts}"),
    }
}

pub fn fmt_hm(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            dt.format("%H:%M").to_string()
        }
        chrono::LocalResult::None => format!("ts:{ts}"),
    }
}

pub fn today_local() -> NaiveDate {
    Local::now().date_naive()
}

/// (start, end) epochy lokálního dne; end je exkluzivní (půlnoc dalšího dne).
pub fn day_bounds_local(date: NaiveDate) -> Result<(i64, i64)> {
    let start_naive = date.and_hms_opt(0, 0, 0).context("neplatné datum")?;
    let next = date.succ_opt().context("datum mimo rozsah")?;
    let end_naive = next.and_hms_opt(0, 0, 0).context("neplatné datum")?;
    let start = Local
        .from_local_datetime(&start_naive)
        .earliest()
        .context("nelze určit začátek dne (DST)")?;
    let end = Local
        .from_local_datetime(&end_naive)
        .earliest()
        .context("nelze určit konec dne (DST)")?;
    Ok((start.timestamp(), end.timestamp()))
}

/// Relativní umístění screenshotu v shots/: (podadresář data, název souboru).
pub fn shot_rel_path(ts: i64) -> (String, String) {
    match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => (
            dt.format("%Y-%m-%d").to_string(),
            dt.format("%H%M%S").to_string() + ".jpg",
        ),
        chrono::LocalResult::None => ("unknown".into(), format!("{ts}.jpg")),
    }
}

/// Velikost adresáře rekurzivně (bytes, počet souborů). Chyby čtení přeskakuje.
pub fn dir_size(path: &std::path::Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            let (b, f) = dir_size(&entry.path());
            bytes += b;
            files += f;
        } else {
            bytes += meta.len();
            files += 1;
        }
    }
    (bytes, files)
}

/// Ořízne řetězec na max_chars znaků, s výpustkou.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_bounds_are_24h() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();
        let (start, end) = day_bounds_local(d).unwrap();
        assert_eq!(end - start, 86400);
    }

    #[test]
    fn shot_path_shape() {
        let (dir, file) = shot_rel_path(1_752_000_000);
        assert_eq!(dir.len(), 10); // YYYY-MM-DD
        assert!(file.ends_with(".jpg"));
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(500), "500 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
