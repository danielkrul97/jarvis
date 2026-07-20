use crate::store::db::Sample;
use crate::util;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Segment {
    pub wm_class: String,
    pub title: String,
    pub start: i64,
    /// exclusive end = ts of the last sample + meta_interval
    pub end: i64,
    pub samples: usize,
    /// (ts, relative path) of screenshots taken during the segment
    pub shots: Vec<(i64, String)>,
}

impl Segment {
    pub fn duration_s(&self) -> i64 {
        self.end - self.start
    }
}

/// Contiguous blocks of the same activity (window class + normalized title).
/// Idle samples are skipped; a gap > 3× the interval ends the segment (suspend, outage).
pub fn segment(samples: &[Sample], meta_interval_s: i64, idle_threshold_ms: i64) -> Vec<Segment> {
    let gap = meta_interval_s * 3;
    let mut out: Vec<Segment> = Vec::new();
    for s in samples.iter().filter(|s| s.idle_ms < idle_threshold_ms) {
        let title = normalize_title(&s.title);
        let cont = out.last().is_some_and(|seg| {
            let last_ts = seg.end - meta_interval_s;
            seg.wm_class == s.wm_class && seg.title == title && s.ts - last_ts <= gap
        });
        if cont {
            let seg = out.last_mut().unwrap();
            seg.end = s.ts + meta_interval_s;
            seg.samples += 1;
            if let Some(p) = &s.shot_path {
                seg.shots.push((s.ts, p.clone()));
            }
        } else {
            out.push(Segment {
                wm_class: s.wm_class.clone(),
                title,
                start: s.ts,
                end: s.ts + meta_interval_s,
                samples: 1,
                shots: s.shot_path.iter().map(|p| (s.ts, p.clone())).collect(),
            });
        }
    }
    out
}

/// Title normalization: strip change markers ("● ", "* "), collapse whitespace.
pub fn normalize_title(t: &str) -> String {
    let t = t.trim().trim_start_matches("● ").trim_start_matches("* ");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Chronological timeline for the prompt; above max_lines, keeps the longest segments.
pub fn render_timeline(segments: &[Segment], max_lines: usize) -> String {
    let mut segs: Vec<&Segment> = segments.iter().collect();
    if segs.len() > max_lines {
        segs.sort_by_key(|s| std::cmp::Reverse(s.duration_s()));
        segs.truncate(max_lines);
        segs.sort_by_key(|s| s.start);
    }
    segs.iter()
        .map(|s| {
            format!(
                "{}–{} ({} min) [{}] {}",
                util::fmt_hm(s.start),
                util::fmt_hm(s.end),
                (s.duration_s() + 30) / 60,
                s.wm_class,
                util::truncate_chars(&s.title, 100)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Sum of active seconds by window class, descending.
pub fn seconds_by_class(segments: &[Segment]) -> Vec<(String, i64)> {
    let mut map: HashMap<&str, i64> = HashMap::new();
    for s in segments {
        *map.entry(s.wm_class.as_str()).or_default() += s.duration_s();
    }
    let mut v: Vec<(String, i64)> = map.into_iter().map(|(k, s)| (k.to_string(), s)).collect();
    v.sort_by_key(|(_, s)| std::cmp::Reverse(*s));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64, class: &str, title: &str, idle_ms: i64, shot: Option<&str>) -> Sample {
        Sample {
            ts,
            wm_class: class.into(),
            title: title.into(),
            idle_ms,
            shot_path: shot.map(Into::into),
        }
    }

    #[test]
    fn merges_consecutive_same_activity() {
        let samples = vec![
            sample(0, "firefox", "Docs", 0, None),
            sample(10, "firefox", "● Docs", 0, Some("shots/a.jpg")),
            sample(20, "firefox", "Docs", 0, None),
            sample(30, "vim", "main.rs", 0, None),
        ];
        let segs = segment(&samples, 10, 120_000);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].samples, 3);
        assert_eq!(segs[0].duration_s(), 30);
        assert_eq!(segs[0].shots.len(), 1);
        assert_eq!(segs[1].wm_class, "vim");
    }

    #[test]
    fn gap_splits_segment() {
        let samples = vec![
            sample(0, "firefox", "Docs", 0, None),
            sample(10, "firefox", "Docs", 0, None),
            // gap of 100 s (> 3×10)
            sample(110, "firefox", "Docs", 0, None),
        ];
        let segs = segment(&samples, 10, 120_000);
        assert_eq!(segs.len(), 2);
    }

    #[test]
    fn idle_samples_are_skipped() {
        let samples = vec![
            sample(0, "firefox", "Docs", 0, None),
            sample(10, "firefox", "Docs", 500_000, None),
            sample(20, "firefox", "Docs", 0, None),
        ];
        let segs = segment(&samples, 10, 120_000);
        // idle sample dropped, but the 20 s gap <= 30 s → still one segment
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].samples, 2);
    }

    #[test]
    fn timeline_caps_lines_keeps_chronology() {
        let samples: Vec<Sample> = (0..40)
            .map(|i| sample(i64::from(i) * 10, "app", &format!("task {i}"), 0, None))
            .collect();
        let segs = segment(&samples, 10, 120_000);
        assert_eq!(segs.len(), 40);
        let tl = render_timeline(&segs, 5);
        assert_eq!(tl.lines().count(), 5);
    }

    #[test]
    fn seconds_by_class_sums_and_sorts() {
        let samples = vec![
            sample(0, "vim", "a", 0, None),
            sample(10, "vim", "a", 0, None),
            sample(20, "firefox", "b", 0, None),
        ];
        let segs = segment(&samples, 10, 120_000);
        let by_class = seconds_by_class(&segs);
        assert_eq!(by_class[0], ("vim".to_string(), 20));
        assert_eq!(by_class[1], ("firefox".to_string(), 10));
    }
}
