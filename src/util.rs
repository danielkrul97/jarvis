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

/// (start, end) epoch of the local day; end is exclusive (midnight the next day).
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

/// Relative location of a screenshot under shots/: (date subdirectory, file name).
pub fn shot_rel_path(ts: i64) -> (String, String) {
    match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => (
            dt.format("%Y-%m-%d").to_string(),
            dt.format("%H%M%S").to_string() + ".jpg",
        ),
        chrono::LocalResult::None => ("unknown".into(), format!("{ts}.jpg")),
    }
}

/// Recursive directory size (bytes, file count). Read errors are skipped.
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

/// Downloads a URL to `target`: streamed, atomic via `.part`, with
/// Content-Length verification and progress logging. An existing file is not re-downloaded.
pub fn download(url: &str, target: &std::path::Path) -> Result<()> {
    use std::io::{Read, Write};
    use tracing::info;
    if target.exists() {
        info!("soubor už existuje: {}", target.display());
        return Ok(());
    }
    info!("stahuji {url}");
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(3600))
        .call()
        .with_context(|| format!("stažení {url} selhalo"))?;
    let total: Option<u64> = resp.header("content-length").and_then(|v| v.parse().ok());
    let mut part_os = target.as_os_str().to_owned();
    part_os.push(".part");
    let part = std::path::PathBuf::from(part_os);
    let file = std::fs::File::create(&part)
        .with_context(|| format!("nelze vytvořit {}", part.display()))?;
    let mut out = std::io::BufWriter::new(file);
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 1 << 20];
    let (mut done, mut last_log) = (0u64, 0u64);
    loop {
        let n = reader.read(&mut buf).context("čtení odpovědi selhalo")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("zápis souboru selhal")?;
        done += n as u64;
        if done - last_log >= 128 * 1024 * 1024 {
            last_log = done;
            info!(
                "… staženo {} / {}",
                human_bytes(done),
                total.map(human_bytes).unwrap_or_else(|| "?".into())
            );
        }
    }
    out.flush().context("flush souboru selhal")?;
    drop(out);
    if let Some(t) = total {
        if done != t {
            let _ = std::fs::remove_file(&part);
            anyhow::bail!("stažení nekompletní ({done} z {t} B) — zkus znovu");
        }
    }
    std::fs::rename(&part, target)
        .with_context(|| format!("nelze přejmenovat {} → {}", part.display(), target.display()))?;
    info!("uloženo: {} ({})", target.display(), human_bytes(done));
    Ok(())
}

/// Truncates a string to max_chars characters, with an ellipsis.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// SHA-256 (FIPS 180-4). Its only use is hashing a runbook artifact at
/// approval time (verified before every execution); a dependency would be
/// overkill — same rationale as the hand-rolled base64 in sms.rs.
#[allow(clippy::needless_range_loop)]
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (dst, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *dst = dst.wrapping_add(v);
        }
    }
    h.iter().map(|w| format!("{w:08x}")).collect()
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
    fn sha256_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // canonical 56B vector — length forces a second padding block
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(500), "500 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
