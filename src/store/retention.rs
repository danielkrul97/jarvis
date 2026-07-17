use crate::util;
use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;
use tracing::warn;

/// Smaže soubory screenshotů starších než `older_than_secs` a vyNULLuje
/// jejich shot_path (metadata vzorků zůstávají). Vrací počet odstraněných.
pub fn purge(conn: &Connection, data_dir: &Path, older_than_secs: i64) -> Result<usize> {
    let cutoff = util::now_ts() - older_than_secs;
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, shot_path FROM samples WHERE shot_path IS NOT NULL AND ts < ?1",
        )?;
        let collected: Vec<(i64, String)> = stmt
            .query_map(params![cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        collected
    };

    let mut removed = 0usize;
    for (id, rel) in rows {
        // obrana proti path traversal — mažeme jen uvnitř shots/
        if rel.starts_with('/') || !rel.starts_with("shots/") || rel.split('/').any(|c| c == "..")
        {
            warn!("podezřelá cesta snímku v DB, přeskakuji: {rel}");
            continue;
        }
        let path = data_dir.join(&rel);
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => removed += 1,
            Err(e) => {
                // nejde smazat → necháme shot_path, zkusí se příště
                warn!("nelze smazat {}: {e}", path.display());
                continue;
            }
        }
        conn.execute("UPDATE samples SET shot_path = NULL WHERE id = ?1", params![id])?;
    }

    // odstraň prázdné denní adresáře (remove_dir maže jen prázdné)
    if let Ok(entries) = std::fs::read_dir(data_dir.join("shots")) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_dir(entry.path());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db;

    #[test]
    fn purge_removes_old_keeps_new() {
        let conn = db::test_conn();
        let tmp = std::env::temp_dir().join(format!("jarvis-ret-test-{}", std::process::id()));
        let day_dir = tmp.join("shots/2020-01-01");
        std::fs::create_dir_all(&day_dir).unwrap();

        let old_rel = "shots/2020-01-01/old.jpg";
        let new_rel = "shots/2020-01-01/new.jpg";
        std::fs::write(tmp.join(old_rel), b"x").unwrap();
        std::fs::write(tmp.join(new_rel), b"x").unwrap();

        let now = util::now_ts();
        db::insert_sample(&conn, now - 10 * 86400, "a", "t", None, 0, Some(old_rel), Some(1)).unwrap();
        db::insert_sample(&conn, now, "a", "t", None, 0, Some(new_rel), Some(2)).unwrap();

        let removed = purge(&conn, &tmp, 7 * 86400).unwrap();
        assert_eq!(removed, 1);
        assert!(!tmp.join(old_rel).exists());
        assert!(tmp.join(new_rel).exists());

        let rows = db::samples_between(&conn, 0, now + 1).unwrap();
        assert!(rows[0].shot_path.is_none());
        assert_eq!(rows[1].shot_path.as_deref(), Some(new_rel));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn purge_skips_traversal_paths() {
        let conn = db::test_conn();
        let tmp = std::env::temp_dir().join(format!("jarvis-ret-test3-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("shots")).unwrap();
        let victim = tmp.join("victim.txt");
        std::fs::write(&victim, "nesmaž mě").unwrap();
        let now = util::now_ts();
        for evil in ["shots/../victim.txt", "/etc/passwd", "victim.txt"] {
            db::insert_sample(&conn, now - 10 * 86400, "a", "t", None, 0, Some(evil), None)
                .unwrap();
        }
        assert_eq!(purge(&conn, &tmp, 7 * 86400).unwrap(), 0);
        assert!(victim.exists());
        // podezřelé cesty zůstávají v DB jako stopa, nic se nemaže
        let rows = db::samples_between(&conn, 0, now).unwrap();
        assert!(rows.iter().all(|s| s.shot_path.is_some()));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn purge_missing_file_still_clears_path() {
        let conn = db::test_conn();
        let tmp = std::env::temp_dir().join(format!("jarvis-ret-test2-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("shots")).unwrap();
        let now = util::now_ts();
        db::insert_sample(&conn, now - 10 * 86400, "a", "t", None, 0, Some("shots/gone.jpg"), None).unwrap();
        assert_eq!(purge(&conn, &tmp, 7 * 86400).unwrap(), 1);
        assert!(db::samples_between(&conn, 0, now).unwrap()[0].shot_path.is_none());
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
