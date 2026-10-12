//! `prompt/frecency.tsx` — `<state>/frecency.jsonl`: how often and how
//! recently each file was mentioned. Same file and line shape as TS, so
//! both TUIs share it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// `MAX_FRECENCY_ENTRIES`.
pub const MAX_FRECENCY_ENTRIES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    pub frequency: u64,
    pub last_open: u64,
}

#[derive(Debug, Default)]
pub struct Frecency {
    data: BTreeMap<String, Entry>,
    file: Option<PathBuf>,
}

/// `parseFrecency`: the last line per path wins; newest 1000 by
/// `lastOpen`.
pub fn parse_frecency(text: &str) -> Vec<(String, Entry)> {
    let mut latest: BTreeMap<String, Entry> = BTreeMap::new();
    for line in text.split('\n').filter(|line| !line.is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let (Some(path), Some(frequency), Some(last_open)) = (
            value.get("path").and_then(Value::as_str),
            value.get("frequency").and_then(Value::as_u64),
            value.get("lastOpen").and_then(Value::as_u64),
        ) else {
            continue;
        };
        latest.insert(
            path.to_string(),
            Entry {
                frequency,
                last_open,
            },
        );
    }
    let mut entries: Vec<(String, Entry)> = latest.into_iter().collect();
    entries.sort_by(|a, b| b.1.last_open.cmp(&a.1.last_open));
    entries.truncate(MAX_FRECENCY_ENTRIES);
    entries
}

fn line(path: &str, entry: &Entry) -> String {
    json!({"path": path, "frequency": entry.frequency, "lastOpen": entry.last_open}).to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Frecency {
    pub fn load(state_dir: Option<&Path>) -> Frecency {
        let Some(dir) = state_dir else {
            return Frecency::default();
        };
        let file = dir.join("frecency.jsonl");
        let entries = parse_frecency(&std::fs::read_to_string(&file).unwrap_or_default());
        if !entries.is_empty() {
            let text = entries
                .iter()
                .map(|(path, entry)| line(path, entry))
                .collect::<Vec<_>>()
                .join("\n");
            let _ = std::fs::write(&file, text + "\n");
        }
        Frecency {
            data: entries.into_iter().collect(),
            file: Some(file),
        }
    }

    /// `calculateFrecency`: frequency decayed by days since last open.
    pub fn score(&self, absolute: &str, now: u64) -> f64 {
        self.data.get(absolute).map_or(0.0, |entry| {
            entry.frequency as f64
                / (1.0 + now.saturating_sub(entry.last_open) as f64 / 86_400_000.0)
        })
    }

    pub fn get(&self, absolute: &str) -> Option<Entry> {
        self.data.get(absolute).copied()
    }

    /// `updateFrecency(path)`: one more open, now; appended, compacted
    /// past the cap.
    pub fn update(&mut self, absolute: &str) {
        let entry = Entry {
            frequency: self.data.get(absolute).map_or(0, |entry| entry.frequency) + 1,
            last_open: now_ms(),
        };
        self.data.insert(absolute.to_string(), entry);
        let Some(file) = &self.file else {
            return;
        };
        if self.data.len() <= MAX_FRECENCY_ENTRIES {
            use std::io::Write as _;
            if let Ok(mut handle) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file)
            {
                let _ = writeln!(handle, "{}", line(absolute, &entry));
            }
            return;
        }
        let mut sorted: Vec<(String, Entry)> = std::mem::take(&mut self.data).into_iter().collect();
        sorted.sort_by(|a, b| b.1.last_open.cmp(&a.1.last_open));
        sorted.truncate(MAX_FRECENCY_ENTRIES);
        let text = sorted
            .iter()
            .map(|(path, entry)| line(path, entry))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(file, text + "\n");
        self.data = sorted.into_iter().collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_line_per_path_wins_and_updates_append() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("frecency.jsonl"),
            "{\"path\":\"/a\",\"frequency\":1,\"lastOpen\":10}\nbad\n{\"path\":\"/a\",\"frequency\":3,\"lastOpen\":20}\n",
        )
        .unwrap();
        let mut frecency = Frecency::load(Some(dir.path()));
        assert_eq!(
            frecency.get("/a"),
            Some(Entry {
                frequency: 3,
                last_open: 20
            })
        );
        frecency.update("/a");
        assert_eq!(frecency.get("/a").unwrap().frequency, 4);
        let reloaded = Frecency::load(Some(dir.path()));
        assert_eq!(reloaded.get("/a").unwrap().frequency, 4);
        assert!(reloaded.score("/a", now_ms()) > 3.9);
    }
}
