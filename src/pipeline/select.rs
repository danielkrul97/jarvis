use crate::pipeline::segment::Segment;
use std::path::Path;

/// Representative frames: segments sorted by duration, taking the middle
/// screenshot from each until the cap is reached. Returns paths relative to data_dir
/// (existing files only).
pub fn select_frames(segments: &[Segment], data_dir: &Path, cap: usize) -> Vec<String> {
    let mut ordered: Vec<&Segment> = segments.iter().filter(|s| !s.shots.is_empty()).collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.duration_s()));
    let mut out = Vec::new();
    for seg in ordered {
        if out.len() >= cap {
            break;
        }
        let (_, rel) = &seg.shots[seg.shots.len() / 2];
        if data_dir.join(rel).exists() && !out.contains(rel) {
            out.push(rel.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::segment::Segment;

    fn seg(dur: i64, shots: &[&str]) -> Segment {
        Segment {
            wm_class: "app".into(),
            title: "t".into(),
            start: 0,
            end: dur,
            samples: 1,
            shots: shots.iter().map(|s| (0, s.to_string())).collect(),
        }
    }

    #[test]
    fn selects_by_duration_caps_and_skips_missing() {
        let tmp = std::env::temp_dir().join(format!("jarvis-sel-test-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("shots")).unwrap();
        for name in ["a.jpg", "b.jpg", "c.jpg"] {
            std::fs::write(tmp.join("shots").join(name), b"x").unwrap();
        }
        let segments = vec![
            seg(100, &["shots/a.jpg"]),
            seg(300, &["shots/b.jpg"]),
            seg(200, &["shots/missing.jpg"]),
            seg(50, &["shots/c.jpg"]),
            seg(10, &[]),
        ];
        let frames = select_frames(&segments, &tmp, 2);
        // longest (b) first, missing skipped, cap 2 → b + a
        assert_eq!(frames, vec!["shots/b.jpg".to_string(), "shots/a.jpg".to_string()]);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn picks_middle_shot() {
        let tmp = std::env::temp_dir().join(format!("jarvis-sel-test2-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("shots")).unwrap();
        for name in ["1.jpg", "2.jpg", "3.jpg"] {
            std::fs::write(tmp.join("shots").join(name), b"x").unwrap();
        }
        let segments = vec![seg(100, &["shots/1.jpg", "shots/2.jpg", "shots/3.jpg"])];
        assert_eq!(select_frames(&segments, &tmp, 5), vec!["shots/2.jpg".to_string()]);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
