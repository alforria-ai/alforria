//! `state/prompt.rs` — the prompt editor model (M8.6): port of
//! `component/prompt/index.tsx` (shell mode, submit pipeline, paste +
//! virtual-text extmarks) plus `prompt/history.tsx`, `prompt/stash.tsx`,
//! `prompt/part.ts`, `prompt/display.ts` and the model half of
//! `prompt/autocomplete.tsx`. The buffer lives in
//! [`crate::ui::textarea::Textarea`]; this module owns everything
//! around it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::state::route::{PromptMode, Route};
use crate::state::{App, Toast, ToastVariant};
use crate::ui::textarea::Textarea;

// ------------------------------------------------------------- part inputs

/// A prompt part input (`PromptInfo["parts"][number]`,
/// `prompt/history.tsx:11-26`) — the `Omit<Part, …>` input shapes.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptPart {
    Text {
        text: String,
        synthetic: Option<bool>,
        source: Option<TextSource>,
    },
    File {
        mime: String,
        filename: Option<String>,
        url: String,
        source: Option<FileSource>,
    },
    Agent {
        name: String,
        source: Option<AgentSource>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextSource {
    pub start: usize,
    pub end: usize,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileSource {
    pub kind: Option<String>,
    pub path: Option<String>,
    pub text: Option<TextSource>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentSource {
    pub start: usize,
    pub end: usize,
    pub value: String,
}

fn span_to_value(span_start: usize, span_end: usize, value: &str) -> Value {
    json!({ "start": span_start, "end": span_end, "value": value })
}

fn span_from_value(value: &Value) -> Option<(usize, usize, String)> {
    Some((
        value.get("start")?.as_u64()? as usize,
        value.get("end")?.as_u64()? as usize,
        value.get("value")?.as_str()?.to_string(),
    ))
}

impl PromptPart {
    /// The TS JSON shape (byte-compatible with the parts the TS TUI
    /// persists into history/stash and submits).
    pub fn to_value(&self) -> Value {
        match self {
            PromptPart::Text {
                text,
                synthetic,
                source,
            } => {
                let mut object = serde_json::Map::new();
                object.insert("type".into(), json!("text"));
                object.insert("text".into(), json!(text));
                if let Some(synthetic) = synthetic {
                    object.insert("synthetic".into(), json!(synthetic));
                }
                if let Some(source) = source {
                    object.insert(
                        "source".into(),
                        json!({ "text": span_to_value(source.start, source.end, &source.value) }),
                    );
                }
                Value::Object(object)
            }
            PromptPart::File {
                mime,
                filename,
                url,
                source,
            } => {
                let mut object = serde_json::Map::new();
                object.insert("type".into(), json!("file"));
                object.insert("mime".into(), json!(mime));
                if let Some(filename) = filename {
                    object.insert("filename".into(), json!(filename));
                }
                object.insert("url".into(), json!(url));
                if let Some(source) = source {
                    let mut source_object = serde_json::Map::new();
                    if let Some(kind) = &source.kind {
                        source_object.insert("type".into(), json!(kind));
                    }
                    if let Some(path) = &source.path {
                        source_object.insert("path".into(), json!(path));
                    }
                    if let Some(text) = &source.text {
                        source_object.insert(
                            "text".into(),
                            span_to_value(text.start, text.end, &text.value),
                        );
                    }
                    object.insert("source".into(), Value::Object(source_object));
                }
                Value::Object(object)
            }
            PromptPart::Agent { name, source } => {
                let mut object = serde_json::Map::new();
                object.insert("type".into(), json!("agent"));
                object.insert("name".into(), json!(name));
                if let Some(source) = source {
                    object.insert(
                        "source".into(),
                        span_to_value(source.start, source.end, &source.value),
                    );
                }
                Value::Object(object)
            }
        }
    }

    pub fn from_value(value: &Value) -> Option<PromptPart> {
        match value.get("type")?.as_str()? {
            "text" => Some(PromptPart::Text {
                text: value.get("text")?.as_str()?.to_string(),
                synthetic: value.get("synthetic").and_then(Value::as_bool),
                source: value
                    .get("source")
                    .and_then(|source| source.get("text"))
                    .and_then(|source| {
                        span_from_value(source).map(|(start, end, value)| TextSource {
                            start,
                            end,
                            value,
                        })
                    }),
            }),
            "file" => Some(PromptPart::File {
                mime: value.get("mime")?.as_str()?.to_string(),
                filename: value
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                url: value.get("url")?.as_str()?.to_string(),
                source: value.get("source").map(|source| FileSource {
                    kind: source
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    path: source
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    text: source
                        .get("text")
                        .and_then(span_from_value)
                        .map(|(start, end, value)| TextSource { start, end, value }),
                }),
            }),
            "agent" => Some(PromptPart::Agent {
                name: value.get("name")?.as_str()?.to_string(),
                source: value.get("source").and_then(|source| {
                    span_from_value(source).map(|(start, end, value)| AgentSource {
                        start,
                        end,
                        value,
                    })
                }),
            }),
            _ => None,
        }
    }

    /// The `(start, end)` span this part tracks in the buffer, if any.
    fn span(&self) -> Option<(usize, usize)> {
        self.span_value().map(|(start, end, _)| (start, end))
    }

    /// The span plus the virtual text value.
    fn span_value(&self) -> Option<(usize, usize, String)> {
        match self {
            PromptPart::Text { source, .. } => source
                .as_ref()
                .map(|source| (source.start, source.end, source.value.clone())),
            PromptPart::File { source, .. } => source
                .as_ref()
                .and_then(|source| source.text.as_ref())
                .map(|text| (text.start, text.end, text.value.clone())),
            PromptPart::Agent { source, .. } => source
                .as_ref()
                .map(|source| (source.start, source.end, source.value.clone())),
        }
    }

    fn set_span(&mut self, span: (usize, usize)) {
        match self {
            PromptPart::Text {
                source: Some(source),
                ..
            } => {
                source.start = span.0;
                source.end = span.1;
            }
            PromptPart::File {
                source: Some(source),
                ..
            } => {
                if let Some(text) = &mut source.text {
                    text.start = span.0;
                    text.end = span.1;
                }
            }
            PromptPart::Agent {
                source: Some(source),
                ..
            } => {
                source.start = span.0;
                source.end = span.1;
            }
            _ => {}
        }
    }
}

/// One history entry (`PromptInfo`, `prompt/history.tsx:9-26`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromptEntry {
    pub input: String,
    pub mode: Option<PromptMode>,
    pub parts: Vec<PromptPart>,
}

impl PromptEntry {
    fn to_value(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("input".into(), json!(self.input));
        object.insert(
            "parts".into(),
            Value::Array(self.parts.iter().map(PromptPart::to_value).collect()),
        );
        if let Some(mode) = self.mode {
            object.insert(
                "mode".into(),
                json!(if mode == PromptMode::Shell {
                    "shell"
                } else {
                    "normal"
                }),
            );
        }
        Value::Object(object)
    }

    fn from_value(value: &Value) -> Option<PromptEntry> {
        Some(PromptEntry {
            input: value.get("input")?.as_str()?.to_string(),
            mode: match value.get("mode").and_then(Value::as_str) {
                Some("shell") => Some(PromptMode::Shell),
                Some("normal") => Some(PromptMode::Normal),
                _ => None,
            },
            parts: value
                .get("parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(PromptPart::from_value)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        })
    }
}

// ------------------------------------------------------------- display ops

/// `expandTrackedPastedText` (`prompt/part.ts:25-33`): replace the
/// virtual-text ranges with real content, back to front.
pub fn expand_tracked_pasted_text(text: &str, ranges: Vec<(usize, usize, String)>) -> String {
    let mut ranges = ranges;
    ranges.sort_by_key(|range| std::cmp::Reverse(range.0));
    let mut result: Vec<char> = text.chars().collect();
    for (start, end, replacement) in ranges {
        if start > end || end > result.len() {
            continue;
        }
        let replacement: Vec<char> = replacement.chars().collect();
        result.splice(start..end, replacement);
    }
    result.into_iter().collect()
}

/// `expandPastedTextPlaceholders` (`prompt/part.ts:12-18`): replace
/// each tracked text part's virtual value with its real text.
pub fn expand_pasted_text_placeholders(text: &str, parts: &[PromptPart]) -> String {
    let mut result = text.to_string();
    for part in parts {
        if let PromptPart::Text {
            source: Some(source),
            text,
            ..
        } = part
        {
            result = result.replace(&source.value, text);
        }
    }
    result
}

// ---------------------------------------------------------------- history

pub const MAX_HISTORY_ENTRIES: usize = 50;

/// `parsePromptHistory` (`prompt/history.tsx:40-52`).
pub fn parse_prompt_history(text: &str) -> Vec<PromptEntry> {
    text.split('\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|line| PromptEntry::from_value(&line))
        .take(MAX_HISTORY_ENTRIES)
        .collect()
}

/// `usePromptHistory` (`prompt/history.tsx:54-103`) — one entry per
/// line in `<state>/prompt-history.jsonl`.
#[derive(Debug)]
pub struct History {
    index: i64,
    entries: Vec<PromptEntry>,
    file: Option<PathBuf>,
}

impl History {
    pub fn new(state_dir: Option<&Path>) -> History {
        let mut history = History {
            index: 0,
            entries: Vec::new(),
            file: None,
        };
        if let Some(dir) = state_dir {
            history.file = Some(dir.join("prompt-history.jsonl"));
            let text = history
                .file
                .as_ref()
                .and_then(|file| std::fs::read_to_string(file).ok())
                .unwrap_or_default();
            history.entries = parse_prompt_history(&text);
            // Rewrite valid entries to self-heal corruption and
            // enforce the limit (`history.tsx:75-81`).
            let entries = std::mem::take(&mut history.entries);
            history.write_all(&entries);
            history.entries = entries;
        }
        history
    }

    fn lookup(&self, index: i64) -> Option<&PromptEntry> {
        if index >= 0 {
            self.entries.get(index as usize)
        } else {
            let from_end = index.unsigned_abs() as usize;
            self.entries
                .len()
                .checked_sub(from_end)
                .and_then(|index| self.entries.get(index))
        }
    }

    /// `move(direction, input)` (`history.tsx:60-73`).
    pub fn move_direction(&mut self, direction: i32, input: &str) -> Option<PromptEntry> {
        if self.entries.is_empty() {
            return None;
        }
        let current = self.lookup(self.index)?;
        if current.input != input && !input.is_empty() {
            return None;
        }
        let next = self.index + direction as i64;
        if next.unsigned_abs() > self.entries.len() as u64 {
            return None;
        }
        if next > 0 {
            return None;
        }
        self.index = next;
        if self.index == 0 {
            return Some(PromptEntry::default());
        }
        self.lookup(self.index).cloned()
    }

    /// `append(item)` (`history.tsx:75-101`).
    pub fn append(&mut self, entry: PromptEntry) {
        if let Some(last) = self.entries.last() {
            if *last == entry {
                self.index = 0;
                return;
            }
        }
        self.entries.push(entry);
        let mut trimmed = false;
        if self.entries.len() > MAX_HISTORY_ENTRIES {
            let split = self.entries.len() - MAX_HISTORY_ENTRIES;
            self.entries.drain(0..split);
            trimmed = true;
        }
        self.index = 0;
        let entries = std::mem::take(&mut self.entries);
        if trimmed {
            self.write_all(&entries);
        } else if let Some(last) = entries.last() {
            self.append_line(&last.to_value());
        }
        self.entries = entries;
    }

    fn append_line(&self, value: &Value) {
        let Some(file) = &self.file else {
            return;
        };
        if let Ok(mut handle) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
        {
            use std::io::Write as _;
            let _ = writeln!(handle, "{value}");
        }
    }

    fn write_all(&self, entries: &[PromptEntry]) {
        let Some(file) = &self.file else {
            return;
        };
        let text = entries
            .iter()
            .map(|entry| entry.to_value().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.is_empty() {
            text
        } else {
            format!("{text}\n")
        };
        let _ = std::fs::write(file, text);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ------------------------------------------------------------------ stash

pub const MAX_STASH_ENTRIES: usize = 50;

#[derive(Debug, Clone, PartialEq)]
pub struct StashEntry {
    pub entry: PromptEntry,
    pub timestamp: u64,
}

impl StashEntry {
    fn to_value(&self) -> Value {
        let mut value = self.entry.to_value();
        if let Some(object) = value.as_object_mut() {
            object.insert("timestamp".into(), json!(self.timestamp));
        }
        value
    }
}

/// `parsePromptStash` (`prompt/stash.tsx:29-43`).
pub fn parse_prompt_stash(text: &str) -> Vec<StashEntry> {
    text.split('\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|line| {
            Some(StashEntry {
                entry: PromptEntry::from_value(&line)?,
                timestamp: line.get("timestamp").and_then(Value::as_u64)?,
            })
        })
        .take(MAX_STASH_ENTRIES)
        .collect()
}

/// `usePromptStash` (`prompt/stash.tsx`) — `<state>/prompt-stash.jsonl`.
#[derive(Debug)]
pub struct Stash {
    entries: Vec<StashEntry>,
    file: Option<PathBuf>,
}

impl Stash {
    pub fn new(state_dir: Option<&Path>) -> Stash {
        let mut stash = Stash {
            entries: Vec::new(),
            file: None,
        };
        if let Some(dir) = state_dir {
            stash.file = Some(dir.join("prompt-stash.jsonl"));
            let text = stash
                .file
                .as_ref()
                .and_then(|file| std::fs::read_to_string(file).ok())
                .unwrap_or_default();
            stash.entries = parse_prompt_stash(&text);
        }
        stash
    }

    pub fn list(&self) -> &[StashEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(&mut self, entry: PromptEntry) {
        self.entries.push(StashEntry {
            entry,
            timestamp: current_millis(),
        });
        let mut trimmed = false;
        if self.entries.len() > MAX_STASH_ENTRIES {
            let split = self.entries.len() - MAX_STASH_ENTRIES;
            self.entries.drain(0..split);
            trimmed = true;
        }
        if trimmed {
            self.write_all();
        } else if let Some(entry) = self.entries.last() {
            let value = entry.to_value();
            self.append_line(&value);
        }
    }

    pub fn pop(&mut self) -> Option<StashEntry> {
        let entry = self.entries.pop();
        self.write_all();
        entry
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
            self.write_all();
        }
    }

    fn append_line(&self, value: &Value) {
        let Some(file) = &self.file else {
            return;
        };
        if let Ok(mut handle) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
        {
            use std::io::Write as _;
            let _ = writeln!(handle, "{value}");
        }
    }

    fn write_all(&self) {
        let Some(file) = &self.file else {
            return;
        };
        let text = self
            .entries
            .iter()
            .map(|entry| entry.to_value().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.is_empty() {
            text
        } else {
            format!("{text}\n")
        };
        let _ = std::fs::write(file, text);
    }
}

fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------- autocomplete

/// One autocomplete row (`AutocompleteOption`).
#[derive(Debug, Clone, PartialEq)]
pub struct AutocompleteOption {
    pub display: String,
    pub description: Option<String>,
    /// The agent name for `@`-mentions (inserted as an `agent` part).
    pub agent: Option<String>,
    /// A server command name for `/` completions.
    pub command: Option<String>,
}

/// The model half of `prompt/autocomplete.tsx`.
#[derive(Debug, Default)]
pub struct Autocomplete {
    /// `false | "@" | "/"`.
    pub visible: Option<char>,
    /// Char offset of the trigger character.
    pub index: usize,
    pub selected: usize,
    pub options: Vec<AutocompleteOption>,
}

/// `mentionTriggerIndex` (`prompt/display.ts:56-71`).
pub fn mention_trigger_index(value: &str, offset: usize) -> Option<usize> {
    let chars: Vec<char> = value.chars().collect();
    let text: String = chars.iter().take(offset).collect();
    let text_chars: Vec<char> = text.chars().collect();
    let index = text_chars.iter().rposition(|char| *char == '@')?;
    let before = if index == 0 {
        None
    } else {
        text_chars.get(index - 1)
    };
    let query: String = text_chars[index..].iter().collect();
    if (before.is_none() || before.is_some_and(|c| c.is_whitespace()))
        && !query.chars().any(char::is_whitespace)
    {
        Some(index)
    } else {
        None
    }
}

/// `removeLineRange` (`prompt/autocomplete.tsx:27-30`).
fn remove_line_range(input: &str) -> String {
    match input.rfind('#') {
        Some(index) => input[..index].to_string(),
        None => input.to_string(),
    }
}

fn text_range(value: &str, start: usize, end: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    let start = start.min(chars.len());
    let end = end.clamp(start, chars.len());
    chars[start..end].iter().collect()
}

impl Autocomplete {
    pub fn visible(&self) -> bool {
        self.visible.is_some()
    }

    /// The search string — text between the trigger and the cursor
    /// (`filter`, `autocomplete.tsx:146-152`).
    fn search(&self, value: &str) -> String {
        if self.visible.is_none() {
            return String::new();
        }
        let cursor = value.chars().count();
        text_range(value, self.index + 1, cursor.max(self.index + 1))
    }

    /// `onInput` (`autocomplete.tsx:676-708`) — hides when the trigger
    /// is invalidated, shows on `/` at position 0 or an `@` mention.
    fn on_input(&mut self, value: &str, cursor: usize) {
        if self.visible.is_some() {
            let between = text_range(value, self.index, cursor);
            if cursor <= self.index
                || between.chars().any(char::is_whitespace)
                || (self.visible == Some('/') && slash_not_sole(value))
            {
                self.visible = None;
                self.selected = 0;
            }
            return;
        }
        if cursor == 0 {
            return;
        }
        let before: String = value.chars().take(cursor).collect();
        if value.starts_with('/') && !before.chars().any(char::is_whitespace) {
            self.show('/', 0);
            return;
        }
        if let Some(index) = mention_trigger_index(value, cursor) {
            self.show('@', index);
        }
    }

    fn show(&mut self, visible: char, index: usize) {
        self.visible = Some(visible);
        self.index = index;
        self.selected = 0;
    }

    fn move_selection(&mut self, direction: i32) {
        if self.options.is_empty() {
            return;
        }
        let mut selected = self.selected as i64 + direction as i64;
        if selected < 0 {
            selected = self.options.len() as i64 - 1;
        }
        if selected >= self.options.len() as i64 {
            selected = 0;
        }
        self.selected = selected as usize;
    }
}

/// `(store.visible === "/" && value.match(/^\S+\s+\S+\s*$/))` —
/// "/<command>" must be the sole content.
fn slash_not_sole(value: &str) -> bool {
    match value.strip_prefix('/') {
        Some(rest) => {
            let trimmed = rest.trim_end();
            trimmed.contains(' ') && trimmed.split_whitespace().count() > 1
        }
        None => false,
    }
}

// ------------------------------------------------------------- prompt state

/// Everything `component/prompt/index.tsx` owns.
#[derive(Debug)]
pub struct PromptState {
    pub textarea: Textarea,
    pub mode: PromptMode,
    pub parts: Vec<PromptPart>,
    pub extmark_to_part: BTreeMap<u64, usize>,
    pub history: History,
    pub stash: Stash,
    pub autocomplete: Autocomplete,
    /// The `submitting` double-submit guard (`prompt/index.tsx:930-945`).
    pub submitting: bool,
    /// The placeholder roll (`store.placeholder`).
    pub placeholder: u32,
}

impl Default for PromptState {
    fn default() -> PromptState {
        PromptState::new(None)
    }
}

impl PromptState {
    pub fn new(state_dir: Option<&Path>) -> PromptState {
        PromptState {
            textarea: Textarea::new(),
            mode: PromptMode::Normal,
            parts: Vec::new(),
            extmark_to_part: BTreeMap::new(),
            history: History::new(state_dir),
            stash: Stash::new(state_dir),
            autocomplete: Autocomplete::default(),
            submitting: false,
            placeholder: 0,
        }
    }

    pub fn input(&self) -> &str {
        self.textarea.text()
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.is_empty() && self.parts.is_empty()
    }

    pub fn set_mode(&mut self, mode: PromptMode) {
        self.mode = mode;
        self.placeholder = self.placeholder.wrapping_add(1);
    }

    /// `ref.set(prompt)` (`prompt/index.tsx:595-600`).
    pub fn set_entry(&mut self, entry: &PromptEntry) {
        self.textarea.set_text(&entry.input);
        self.parts = entry.parts.clone();
        self.mode = entry.mode.unwrap_or(PromptMode::Normal);
        self.restore_extmarks_from_parts();
        self.textarea.buffer_end(false);
    }

    /// `reset()` (`prompt/index.tsx:601-609`).
    pub fn reset(&mut self) {
        self.textarea.clear();
        self.textarea.clear_extmarks();
        self.parts.clear();
        self.extmark_to_part.clear();
        self.autocomplete.visible = None;
    }

    /// `restoreExtmarksFromParts` (`prompt/index.tsx:658-700`).
    pub fn restore_extmarks_from_parts(&mut self) {
        self.textarea.clear_extmarks();
        self.extmark_to_part.clear();
        for (index, part) in self.parts.iter().enumerate() {
            let Some((start, end)) = part.span() else {
                continue;
            };
            let id = self.textarea.create_extmark(start, end);
            self.extmark_to_part.insert(id, index);
        }
    }

    /// `syncExtmarksWithPromptParts` (`prompt/index.tsx:702-734`) —
    /// parts live in extmark order; parts whose extmark was destroyed
    /// are dropped.
    pub fn sync_extmarks_with_prompt_parts(&mut self) {
        let old_map = std::mem::take(&mut self.extmark_to_part);
        let parts = std::mem::take(&mut self.parts);
        let mut new_parts: Vec<PromptPart> = Vec::new();
        let mut new_map = BTreeMap::new();
        for mark in self.textarea.extmarks() {
            let Some(part_index) = old_map.get(&mark.id) else {
                continue;
            };
            let Some(part) = parts.get(*part_index) else {
                continue;
            };
            let mut part = part.clone();
            part.set_span((mark.start, mark.end));
            new_map.insert(mark.id, new_parts.len());
            new_parts.push(part);
        }
        self.extmark_to_part = new_map;
        self.parts = new_parts;
    }

    /// The `expandTrackedPastedText` input ranges — extmarks mapped to
    /// text parts (`prompt/index.tsx:1026-1034`).
    fn tracked_ranges(&self) -> Vec<(usize, usize, String)> {
        let mut ranges = Vec::new();
        for mark in self.textarea.extmarks() {
            let Some(part_index) = self.extmark_to_part.get(&mark.id) else {
                continue;
            };
            if let Some(PromptPart::Text { text, .. }) = self.parts.get(*part_index) {
                ranges.push((mark.start, mark.end, text.clone()));
            }
        }
        ranges
    }
}

// ------------------------------------------------------------------ paste

/// `pastedFilepath` (`prompt/index.tsx:78-87`).
pub fn pasted_filepath(value: &str) -> String {
    let raw = trim_wrapping_quotes(value);
    file_url_to_path(&raw).unwrap_or_else(|| raw.replace('\\', ""))
}

fn trim_wrapping_quotes(value: &str) -> String {
    let mut chars: Vec<char> = value.chars().collect();
    while matches!(chars.first(), Some('\'') | Some('"')) {
        chars.remove(0);
    }
    while matches!(chars.last(), Some('\'') | Some('"')) {
        chars.pop();
    }
    chars.into_iter().collect()
}

/// `fileURLToPath` for the `file://` unwrap (`prompt/index.tsx:81-84`).
fn file_url_to_path(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("file://")?;
    if rest.starts_with("localhost") {
        return percent_decode(rest.strip_prefix("localhost")?);
    }
    percent_decode(rest)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16)?;
            let low = (bytes[index + 2] as char).to_digit(16)?;
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The `readLocalAttachment` mime map (`prompt/local-attachment.ts:33-42`).
pub fn attachment_mime(path: &str) -> Option<&'static str> {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)?;
    Some(match extension.as_str() {
        "avif" => "image/avif",
        "gif" => "image/gif",
        "jpeg" | "jpg" => "image/jpeg",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// `readLocalAttachment` (`prompt/local-attachment.ts:45-48`).
enum LocalAttachment {
    Text(String),
    Binary { mime: String, content: Vec<u8> },
}

fn read_local_attachment(file: &str) -> Option<LocalAttachment> {
    let mime = attachment_mime(file)?;
    if mime == "image/svg+xml" {
        return std::fs::read_to_string(file)
            .ok()
            .filter(|content| !content.is_empty())
            .map(LocalAttachment::Text);
    }
    std::fs::read(file)
        .ok()
        .filter(|content| !content.is_empty())
        .map(|content| LocalAttachment::Binary {
            mime: mime.to_string(),
            content,
        })
}

/// `pasteInputText` (`prompt/index.tsx:1183-1222`): filepath → local
/// attachment → paste summary → raw insert.
pub fn paste_input_text(app: &mut App, text: &str) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let pasted = normalized.trim().to_string();
    let filepath = pasted_filepath(&pasted);
    let is_url = filepath.starts_with("http://") || filepath.starts_with("https://");
    if !is_url {
        if let Some(attachment) = read_local_attachment(&filepath) {
            match attachment {
                LocalAttachment::Text(content) => {
                    let filename = basename(&filepath).unwrap_or_else(|| "image".to_string());
                    paste_text(app, &content, &format!("[SVG: {filename}]"));
                    return;
                }
                LocalAttachment::Binary { mime, content } => {
                    let filename = basename(&filepath);
                    paste_attachment(
                        app,
                        &Attachment {
                            filename,
                            filepath: Some(filepath),
                            mime,
                            content,
                        },
                    );
                    return;
                }
            }
        }
    }

    let line_count = pasted.matches('\n').count() + 1;
    let paste_summary = app
        .state
        .sync
        .config
        .get("experimental")
        .and_then(|experimental| experimental.get("disable_paste_summary"))
        .and_then(Value::as_bool)
        .map(|disabled| !disabled)
        .unwrap_or(true);
    let paste_summary = app
        .state
        .kv
        .get(
            crate::state::kv::keys::PASTE_SUMMARY_ENABLED,
            json!(paste_summary),
        )
        .as_bool()
        .unwrap_or(paste_summary);
    if (line_count >= 3 || pasted.chars().count() > 150) && paste_summary {
        paste_text(app, &pasted, &format!("[Pasted ~{line_count} lines]"));
        return;
    }

    app.ui.prompt.textarea.insert_text(&normalized);
    after_content_change(app);
}

fn basename(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// `pasteText` (`prompt/index.tsx:1149-1181`) — insert `virtual_text`
/// as a virtual-text extmark backed by a real text part.
pub fn paste_text(app: &mut App, text: &str, virtual_text: &str) {
    let start = app.ui.prompt.textarea.cursor();
    let end = start + virtual_text.chars().count();
    app.ui
        .prompt
        .textarea
        .insert_text(&format!("{virtual_text} "));
    let id = app.ui.prompt.textarea.create_extmark(start, end);
    let part_index = app.ui.prompt.parts.len();
    app.ui.prompt.parts.push(PromptPart::Text {
        text: text.to_string(),
        synthetic: None,
        source: Some(TextSource {
            start,
            end,
            value: virtual_text.to_string(),
        }),
    });
    app.ui.prompt.extmark_to_part.insert(id, part_index);
}

/// `pasteAttachment` input (`prompt/index.tsx:1224-1270`).
pub struct Attachment {
    pub filename: Option<String>,
    pub filepath: Option<String>,
    pub mime: String,
    pub content: Vec<u8>,
}

/// `pasteAttachment` (`prompt/index.tsx:1224-1270`): `[Image N]` /
/// `[PDF N]` inline tokens + a `FilePart` with a data URL.
pub fn paste_attachment(app: &mut App, file: &Attachment) {
    let start = app.ui.prompt.textarea.cursor();
    let pdf = file.mime == "application/pdf";
    let count = app
        .ui
        .prompt
        .parts
        .iter()
        .filter(|part| match part {
            PromptPart::File { mime, .. } => {
                if pdf {
                    mime == "application/pdf"
                } else {
                    mime.starts_with("image/")
                }
            }
            _ => false,
        })
        .count();
    let slot = count + 1;
    let virtual_text = if pdf {
        format!("[PDF {slot}]")
    } else {
        format!("[Image {slot}]")
    };
    let end = start + virtual_text.chars().count();
    let text_to_insert = format!("{virtual_text} ");
    app.ui.prompt.textarea.insert_text(&text_to_insert);
    let id = app.ui.prompt.textarea.create_extmark(start, end);
    let part = PromptPart::File {
        mime: file.mime.clone(),
        filename: file.filename.clone(),
        url: format!("data:{};base64,{}", file.mime, base64_encode(&file.content)),
        source: Some(FileSource {
            kind: Some("file".to_string()),
            path: Some(
                file.filepath
                    .clone()
                    .or_else(|| file.filename.clone())
                    .unwrap_or_default(),
            ),
            text: Some(TextSource {
                start,
                end,
                value: virtual_text,
            }),
        }),
    };
    let part_index = app.ui.prompt.parts.len();
    app.ui.prompt.parts.push(part);
    app.ui.prompt.extmark_to_part.insert(id, part_index);
}

/// Standard base64 (RFC 4648) — no new dependency for attachments.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let number = (bytes[0] as u32) << 16 | (bytes[1] as u32) << 8 | bytes[2] as u32;
        out.push(TABLE[(number >> 18 & 0x3f) as usize] as char);
        out.push(TABLE[(number >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(number >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(number & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ----------------------------------------------------------------- submit

/// The dispatch half of a submit (`prompt/index.tsx:1059-1121`).
#[derive(Debug, Clone, PartialEq)]
pub enum SubmitDispatch {
    Shell {
        command: String,
    },
    Command {
        command: String,
        arguments: String,
        parts: Vec<Value>,
    },
    Prompt {
        parts: Vec<Value>,
    },
}

/// Everything [`execute_submit`] needs beyond the app state.
#[derive(Debug, Clone, PartialEq)]
pub struct SubmitPayload {
    /// `None` on the home route — create first.
    pub session_id: Option<String>,
    pub agent: String,
    pub model: crate::state::local::ModelRef,
    pub variant: Option<String>,
    pub dispatch: SubmitDispatch,
    /// History append + clear + navigate, applied once a home-route
    /// `session.create` succeeded.
    pub post_submit: Option<PromptEntry>,
}

/// The submit pipeline (`prompt/index.tsx:947-1147`). Returns the
/// [`crate::state::Effect::PromptSubmit`] for the runtime executor.
pub fn submit(app: &mut App) -> Vec<crate::state::Effect> {
    use crate::state::Effect;
    if app.ui.prompt.submitting {
        return Vec::new();
    }
    if app.ui.prompt.autocomplete.visible() {
        return Vec::new();
    }
    // `props.disabled` (`routes/session/index.tsx:241`): the prompt is
    // disabled while a permission or question prompt is open.
    if !app.state.sync.permission.is_empty() || !app.state.sync.question.is_empty() {
        return Vec::new();
    }
    let input = app.ui.prompt.input().to_string();
    if input.is_empty() {
        return Vec::new();
    }
    let Some(agent) = app
        .state
        .local
        .agent_current(&app.state.sync)
        .and_then(|agent| agent.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Vec::new();
    };
    let trimmed = input.trim();
    if trimmed == "exit" || trimmed == "quit" || trimmed == ":q" {
        app.exit(None);
        return Vec::new();
    }
    let Some(model) = app
        .state
        .local
        .model_current(&app.state.sync, &app.state.args)
    else {
        app.show_toast(Toast {
            title: None,
            variant: ToastVariant::Warning,
            message: "Connect a provider to send prompts".to_string(),
            duration_ms: 3000,
        });
        if app.state.sync.provider.is_empty() {
            let _ = crate::ui::dialogs::open(app, crate::state::PendingDialog::ProviderConnect);
        }
        return Vec::new();
    };
    // The workspace-status guard (`prompt/index.tsx:974-987`).
    if let Route::Session { session_id, .. } = &app.state.route.data {
        if let Some(workspace) = app
            .state
            .sync
            .session(session_id)
            .and_then(|session| session.workspace_id.clone())
        {
            let status = app
                .state
                .project
                .workspace
                .status
                .get(&workspace)
                .map(String::as_str)
                .unwrap_or("error");
            if status != "connected" {
                let _ = crate::ui::dialogs::open(
                    app,
                    crate::state::PendingDialog::WorkspaceUnavailable,
                );
                return Vec::new();
            }
        }
    }

    let entry = PromptEntry {
        input,
        mode: Some(app.ui.prompt.mode),
        parts: app.ui.prompt.parts.clone(),
    };
    let input_text = expand_tracked_pasted_text(&entry.input, app.ui.prompt.tracked_ranges());
    let non_text: Vec<PromptPart> = app
        .ui
        .prompt
        .parts
        .iter()
        .filter(|part| !matches!(part, PromptPart::Text { .. }))
        .cloned()
        .collect();

    let first_line = input_text.split('\n').next().unwrap_or_default();
    let first_word = first_line.split(' ').next().unwrap_or_default();
    let server_command = first_word.strip_prefix('/').filter(|name| {
        app.state
            .sync
            .command
            .iter()
            .any(|command| command.get("name").and_then(Value::as_str) == Some(*name))
    });

    let dispatch = if app.ui.prompt.mode == PromptMode::Shell {
        SubmitDispatch::Shell {
            command: input_text,
        }
    } else if input_text.starts_with('/') && server_command.is_some() {
        SubmitDispatch::Command {
            command: server_command.expect("checked above").to_string(),
            arguments: command_arguments(&input_text),
            parts: non_text
                .iter()
                .filter(|part| matches!(part, PromptPart::File { .. }))
                .map(PromptPart::to_value)
                .collect(),
        }
    } else {
        let mut parts = vec![json!({ "type": "text", "text": input_text })];
        parts.extend(non_text.iter().map(PromptPart::to_value));
        SubmitDispatch::Prompt { parts }
    };

    let payload = SubmitPayload {
        session_id: session_id(app),
        agent,
        model,
        variant: app
            .state
            .local
            .variant_current(&app.state.sync, &app.state.args),
        dispatch,
        post_submit: None,
    };

    if payload.session_id.is_some() {
        // `history.append` + clear run synchronously on the session
        // route — the SDK call is fire-and-forget.
        app.ui.prompt.history.append(entry);
        app.ui.prompt.reset();
        return vec![Effect::PromptSubmit {
            payload: Box::new(payload),
        }];
    }

    // Home route: `session.create` must succeed first; the state
    // changes ride the effect (`prompt/index.tsx:1000-1023, 1122-1144`).
    let mut payload = payload;
    payload.post_submit = Some(entry);
    app.ui.prompt.submitting = true;
    vec![Effect::PromptSubmit {
        payload: Box::new(payload),
    }]
}

/// The argument-preserving split of `/command` submissions
/// (`prompt/index.tsx:1076-1081`).
fn command_arguments(input_text: &str) -> String {
    let first_line_end = input_text.find('\n');
    let first_line = match first_line_end {
        Some(index) => &input_text[..index],
        None => input_text,
    };
    let rest_of_input = first_line_end.map(|index| &input_text[index + 1..]);
    let first_line_args = match first_line.split_once(' ') {
        Some((_, rest)) => rest,
        None => "",
    };
    let mut arguments = first_line_args.to_string();
    if let Some(rest) = rest_of_input {
        if !rest.is_empty() {
            arguments.push('\n');
            arguments.push_str(rest);
        }
    }
    arguments
}

fn session_id(app: &App) -> Option<String> {
    match &app.state.route.data {
        Route::Session { session_id, .. } => Some(session_id.clone()),
        _ => None,
    }
}

/// The runtime half of the submit pipeline — `session.create` when on
/// the home route, then the dispatch, then the deferred post-submit
/// state changes. Executed by the effect loop against the server seam.
///
/// The HTTP awaits run **without** holding the app lock: a turn that
/// blocks on a permission/question reply must not stop the message
/// pump (spec §2.1 — the driver executor runs each effect as a task).
pub async fn run_submit(
    app: &std::sync::Arc<tokio::sync::Mutex<App>>,
    api: &dyn crate::transport::api::ServerApi,
    mut payload: SubmitPayload,
) {
    use crate::transport::api::{
        Location, ProviderModel, SessionCommand, SessionCreate, SessionPrompt, SessionShell,
    };
    let session_id = match &payload.session_id {
        Some(session_id) => session_id.clone(),
        None => {
            let create = api
                .session_create(
                    &Location::default(),
                    SessionCreate {
                        agent: Some(payload.agent.clone()),
                        model: Some(crate::transport::api::SessionCreateModel {
                            id: payload.model.model_id.clone(),
                            provider_id: payload.model.provider_id.clone(),
                            variant: payload.variant.clone(),
                        }),
                        title: None,
                        parent_id: None,
                    },
                )
                .await;
            match create {
                Ok(session) => session.id,
                Err(_) => {
                    let mut app = app.lock().await;
                    app.show_toast(Toast {
                        title: None,
                        variant: ToastVariant::Error,
                        message: "Creating a session failed. Open console for more details."
                            .to_string(),
                        duration_ms: 5000,
                    });
                    app.ui.prompt.submitting = false;
                    return;
                }
            }
        }
    };
    // The home-route state changes (`prompt/index.tsx:1122-1144`) —
    // `history.append`, the input reset and the navigate are independent
    // of the prompt response (TS navigates from a `setTimeout` while the
    // prompt call is fire-and-forget). They must run BEFORE the dispatch:
    // the prompt HTTP call blocks on permission/question replies, and
    // the permission/question prompts only render on the session route.
    {
        let mut app = app.lock().await;
        if let Some(entry) = payload.post_submit.take() {
            app.ui.prompt.history.append(entry);
            app.ui.prompt.reset();
            app.state.route.navigate(Route::Session {
                session_id: session_id.clone(),
                prompt: None,
            });
            app.ui.prompt.submitting = false;
        }
    }

    let model = ProviderModel {
        provider_id: payload.model.provider_id.clone(),
        model_id: payload.model.model_id.clone(),
    };
    let loc = Location::default();
    match &payload.dispatch {
        SubmitDispatch::Shell { command } => {
            let _ = api
                .session_shell(
                    &loc,
                    &session_id,
                    SessionShell {
                        command: command.clone(),
                        agent: payload.agent.clone(),
                        model: Some(model),
                    },
                )
                .await;
            let mut app = app.lock().await;
            app.ui.prompt.mode = PromptMode::Normal;
        }
        SubmitDispatch::Command {
            command,
            arguments,
            parts,
        } => {
            let _ = api
                .session_command(
                    &loc,
                    &session_id,
                    SessionCommand {
                        command: command.clone(),
                        arguments: arguments.clone(),
                        agent: Some(payload.agent.clone()),
                        model: Some(format!(
                            "{}/{}",
                            payload.model.provider_id, payload.model.model_id
                        )),
                        variant: payload.variant.clone(),
                        parts: parts.clone(),
                    },
                )
                .await;
        }
        SubmitDispatch::Prompt { parts } => {
            let result = api
                .session_prompt(
                    &loc,
                    &session_id,
                    SessionPrompt {
                        agent: Some(payload.agent.clone()),
                        provider_id: Some(payload.model.provider_id.clone()),
                        model_id: Some(payload.model.model_id.clone()),
                        model: Some(model),
                        variant: payload.variant.clone(),
                        parts: parts.clone(),
                    },
                )
                .await;
            if let Err(error) = result {
                let mut app = app.lock().await;
                app.show_toast(Toast {
                    title: Some("Failed to send prompt".to_string()),
                    variant: ToastVariant::Error,
                    message: format!("{error:#}"),
                    duration_ms: 5000,
                });
            }
        }
    }
}

// ------------------------------------------------------------- key input

/// `DRAFT_RETENTION_MIN_CHARS` (`prompt/index.tsx:104`).
const DRAFT_RETENTION_MIN_CHARS: usize = 20;

/// `clearPrompt` (`prompt/index.tsx:1272-1286`) — the `prompt.clear`
/// command: drafts ≥ 20 chars (or with parts) go to history first.
/// TS `.length` counts UTF-16 code units.
pub fn clear_prompt(app: &mut App) {
    if app.ui.prompt.input().trim().encode_utf16().count() >= DRAFT_RETENTION_MIN_CHARS
        || !app.ui.prompt.parts.is_empty()
    {
        app.ui.prompt.history.append(PromptEntry {
            input: app.ui.prompt.input().to_string(),
            mode: Some(app.ui.prompt.mode),
            parts: app.ui.prompt.parts.clone(),
        });
    }
    app.ui.prompt.reset();
}

fn after_content_change(app: &mut App) {
    app.ui.prompt.sync_extmarks_with_prompt_parts();
    let value = app.ui.prompt.input().to_string();
    let cursor = app.ui.prompt.textarea.cursor();
    app.ui.prompt.autocomplete.on_input(&value, cursor);
    rebuild_options(app);
}

fn rebuild_options(app: &mut App) {
    let Some(visible) = app.ui.prompt.autocomplete.visible else {
        app.ui.prompt.autocomplete.options.clear();
        return;
    };
    let value = app.ui.prompt.input().to_owned();
    let cursor = app.ui.prompt.textarea.cursor();
    let auto = &mut app.ui.prompt.autocomplete;
    let search = auto.search(&value);
    let mut options: Vec<AutocompleteOption> = Vec::new();
    match visible {
        '/' => {
            for command in crate::command::slash_commands(app) {
                let Some(name) = command.slash_name else {
                    continue;
                };
                options.push(AutocompleteOption {
                    display: format!("/{name}"),
                    description: None,
                    agent: None,
                    command: None,
                });
            }
            for command in &app.state.sync.command {
                let Some(name) = command.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if command.get("source").and_then(Value::as_str) == Some("skill") {
                    continue;
                }
                let label = if command.get("source").and_then(Value::as_str) == Some("mcp") {
                    ":mcp"
                } else {
                    ""
                };
                options.push(AutocompleteOption {
                    display: format!("/{name}{label}"),
                    description: command
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    agent: None,
                    command: Some(name.to_string()),
                });
            }
        }
        '@' => {
            for agent in &app.state.sync.agent {
                if agent.get("hidden").and_then(Value::as_bool) == Some(true)
                    || agent.get("mode").and_then(Value::as_str) == Some("primary")
                {
                    continue;
                }
                let Some(name) = agent.get("name").and_then(Value::as_str) else {
                    continue;
                };
                options.push(AutocompleteOption {
                    display: format!("@{name}"),
                    description: None,
                    agent: Some(name.to_string()),
                    command: None,
                });
            }
        }
        _ => {}
    }
    options.sort_by(|a, b| a.display.cmp(&b.display));
    options.dedup_by(|_, _duplicate| false);
    let options = if search.is_empty() {
        options
    } else {
        let search = remove_line_range(&search);
        let mut matched: Vec<(f64, AutocompleteOption)> = options
            .into_iter()
            .filter_map(|option| {
                let target = remove_line_range(&option.display);
                let score = fuzzy_score(&target.to_ascii_lowercase(), &search.to_ascii_lowercase());
                score.map(|score| (score * start_bonus(&target, &search), option))
            })
            .collect();
        matched.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.display.cmp(&b.1.display))
        });
        matched
            .into_iter()
            .map(|(_, option)| option)
            .take(10)
            .collect()
    };
    app.ui.prompt.autocomplete.options = options;
    app.ui.prompt.autocomplete.selected = 0;
    let _ = cursor;
}

fn start_bonus(target: &str, search: &str) -> f64 {
    if target.starts_with(search) {
        2.0
    } else {
        1.0
    }
}

/// A minimal fuzzy subsequence score (the fuzzysort crate is an
/// escalation; a contiguous match scores highest).
fn fuzzy_score(target: &str, search: &str) -> Option<f64> {
    let mut index = 0;
    let mut score = 0.0;
    let mut last: Option<usize> = None;
    for char in search.chars() {
        let found = target[index..].find(char)?;
        score += if found == 0 { 3.0 } else { 1.0 };
        if last == Some(index + found) {
            score += 2.0;
        }
        last = Some(index + found);
        index += found + char.len_utf8();
    }
    Some(score)
}

/// The visible-keys interception while the autocomplete is open
/// (`prompt/autocomplete.tsx:581-641`).
pub fn autocomplete_key(app: &mut App, key: &crossterm::event::KeyEvent) -> bool {
    if !app.ui.prompt.autocomplete.visible() || !app.ui.prompt_focused {
        return false;
    }
    let matches = |keybind: &str| app.keymap.matches(keybind, key);
    if matches("prompt.autocomplete.prev") {
        app.ui.prompt.autocomplete.move_selection(-1);
        return true;
    }
    if matches("prompt.autocomplete.next") {
        app.ui.prompt.autocomplete.move_selection(1);
        return true;
    }
    if matches("prompt.autocomplete.hide") {
        hide_autocomplete(app);
        return true;
    }
    if matches("prompt.autocomplete.select") || matches("prompt.autocomplete.complete") {
        select_autocomplete(app);
        return true;
    }
    false
}

/// `hide()` (`autocomplete.tsx:650-661`) — for `/`, clear the slash
/// text.
pub fn hide_autocomplete(app: &mut App) {
    let visible = app.ui.prompt.autocomplete.visible;
    if visible == Some('/') {
        let text = app.ui.prompt.input().to_string();
        let cursor = app.ui.prompt.textarea.cursor();
        if !text.ends_with(' ') && text.starts_with('/') {
            app.ui.prompt.textarea.delete_range(0, cursor);
        }
    }
    app.ui.prompt.autocomplete.visible = None;
    app.ui.prompt.autocomplete.selected = 0;
    app.ui.prompt.autocomplete.options.clear();
}

/// `select()` (`autocomplete.tsx:553-558` + `insertPart`).
fn select_autocomplete(app: &mut App) {
    let Some(option) = app
        .ui
        .prompt
        .autocomplete
        .options
        .get(app.ui.prompt.autocomplete.selected)
        .cloned()
    else {
        return;
    };
    hide_autocomplete(app);
    if let Some(command) = option.command {
        let text = format!("/{command} ");
        app.ui.prompt.textarea.set_text(&text);
        app.ui.prompt.parts.clear();
        app.ui.prompt.extmark_to_part.clear();
        return;
    }
    if let Some(agent) = option.agent {
        insert_agent_part(app, &agent);
    }
}

/// `insertPart` for agents (`autocomplete.tsx:172-240`).
fn insert_agent_part(app: &mut App, name: &str) {
    let cursor = app.ui.prompt.textarea.cursor();
    let char_after = app.ui.prompt.textarea.char_at(cursor);
    let needs_space = char_after != Some(' ');
    let append = format!("@{name}{}", if needs_space { " " } else { "" });
    let index = app.ui.prompt.autocomplete.index;
    app.ui.prompt.textarea.delete_range(index, cursor);
    app.ui.prompt.textarea.insert_text(&append);
    let end = index + name.chars().count() + 1;
    let id = app.ui.prompt.textarea.create_extmark(index, end);
    let part_index = app.ui.prompt.parts.len();
    app.ui.prompt.parts.push(PromptPart::Agent {
        name: name.to_string(),
        source: Some(AgentSource {
            start: index,
            end,
            value: format!("@{name}"),
        }),
    });
    app.ui.prompt.extmark_to_part.insert(id, part_index);
}

/// `prompt.history.previous/next` via the up/down keys at the buffer
/// boundaries (`prompt/index.tsx:862-928`).
fn history_move(app: &mut App, direction: i32) {
    let input = app.ui.prompt.input().to_string();
    let Some(item) = app.ui.prompt.history.move_direction(direction, &input) else {
        return;
    };
    let mode = item.mode;
    app.ui.prompt.set_entry(&item);
    if mode.is_none() {
        app.ui.prompt.mode = PromptMode::Normal;
    }
}

/// `$EDITOR` content application (`prompt/index.tsx:424-514`): update
/// part positions from surviving virtual texts, drop parts whose
/// virtual text was deleted.
pub fn apply_editor_content(app: &mut App, normalized: &str) {
    let updated: Vec<PromptPart> = app
        .ui
        .prompt
        .parts
        .iter()
        .filter(|part| !matches!(part, PromptPart::Text { .. }))
        .filter_map(|part| {
            let Some((_, _, value)) = part.span_value() else {
                return Some(part.clone());
            };
            let start = normalized.find(&value)?;
            let mut part = part.clone();
            part.set_span((start, start + value.chars().count()));
            Some(part)
        })
        .collect();
    let mode = app.ui.prompt.mode;
    app.ui.prompt.reset();
    app.ui.prompt.textarea.set_text(normalized);
    app.ui.prompt.parts = updated;
    app.ui.prompt.mode = mode;
    app.ui.prompt.restore_extmarks_from_parts();
    app.ui.prompt.textarea.buffer_end(false);
}

/// `tui.prompt.append` (`prompt/index.tsx:237-248`).
pub fn prompt_append(app: &mut App, text: &str) {
    if !app.ui.prompt_focused {
        return;
    }
    app.ui.prompt.textarea.insert_text(text);
    app.ui.prompt.textarea.buffer_end(false);
    after_content_change(app);
}

// ------------------------------------------------- editing command dispatch

/// The managed-textarea layer (`keymap.tsx:136-173`): the `input.*`
/// command set plus the prompt commands that mutate the editor.
/// Returns `Some(effects)` when the command was consumed here.
pub fn handle_command(app: &mut App, name: &str) -> bool {
    if !app.ui.prompt_focused {
        return false;
    }
    let editing = matches!(
        name,
        "prompt.clear"
            | "input.newline"
            | "input.move.left"
            | "input.move.right"
            | "input.move.up"
            | "input.move.down"
            | "input.select.left"
            | "input.select.right"
            | "input.select.up"
            | "input.select.down"
            | "input.line.home"
            | "input.line.end"
            | "input.select.line.home"
            | "input.select.line.end"
            | "input.visual.line.home"
            | "input.visual.line.end"
            | "input.select.visual.line.home"
            | "input.select.visual.line.end"
            | "input.buffer.home"
            | "input.buffer.end"
            | "input.select.buffer.home"
            | "input.select.buffer.end"
            | "input.delete.line"
            | "input.delete.to.line.end"
            | "input.delete.to.line.start"
            | "input.backspace"
            | "input.delete"
            | "input.undo"
            | "input.redo"
            | "input.word.forward"
            | "input.word.backward"
            | "input.select.word.forward"
            | "input.select.word.backward"
            | "input.delete.word.forward"
            | "input.delete.word.backward"
            | "input.select.all"
            | "session.interrupt"
    );
    if !editing {
        return false;
    }
    let cursor_moved = matches!(
        name,
        "input.move.left"
            | "input.move.right"
            | "input.move.up"
            | "input.move.down"
            | "input.select.left"
            | "input.select.right"
            | "input.select.up"
            | "input.select.down"
    );
    match name {
        "prompt.clear" => clear_prompt(app),
        "input.newline" => app.ui.prompt.textarea.insert_text("\n"),
        "input.move.left" => app.ui.prompt.textarea.move_left(false),
        "input.move.right" => app.ui.prompt.textarea.move_right(false),
        "input.move.up" => {
            if app.ui.prompt.textarea.cursor() == 0 {
                history_move(app, -1);
            } else {
                app.ui.prompt.textarea.move_up(false);
            }
        }
        "input.move.down" => {
            if app.ui.prompt.textarea.cursor() == app.ui.prompt.textarea.char_count() {
                history_move(app, 1);
            } else {
                app.ui.prompt.textarea.move_down(false);
            }
        }
        "input.select.left" => app.ui.prompt.textarea.move_left(true),
        "input.select.right" => app.ui.prompt.textarea.move_right(true),
        "input.select.up" => app.ui.prompt.textarea.move_up(true),
        "input.select.down" => app.ui.prompt.textarea.move_down(true),
        "input.line.home" | "input.visual.line.home" => app.ui.prompt.textarea.line_home(false),
        "input.line.end" | "input.visual.line.end" => app.ui.prompt.textarea.line_end(false),
        "input.select.line.home" | "input.select.visual.line.home" => {
            app.ui.prompt.textarea.line_home(true)
        }
        "input.select.line.end" | "input.select.visual.line.end" => {
            app.ui.prompt.textarea.line_end(true)
        }
        "input.buffer.home" => app.ui.prompt.textarea.buffer_home(false),
        "input.buffer.end" => app.ui.prompt.textarea.buffer_end(false),
        "input.select.buffer.home" => app.ui.prompt.textarea.buffer_home(true),
        "input.select.buffer.end" => app.ui.prompt.textarea.buffer_end(true),
        "input.delete.line" => app.ui.prompt.textarea.delete_line(),
        "input.delete.to.line.end" => app.ui.prompt.textarea.delete_to_line_end(),
        "input.delete.to.line.start" => app.ui.prompt.textarea.delete_to_line_start(),
        "input.backspace" => {
            if app.ui.prompt.mode == PromptMode::Shell && app.ui.prompt.textarea.cursor() == 0 {
                app.ui.prompt.set_mode(PromptMode::Normal);
            } else {
                app.ui.prompt.textarea.backspace();
            }
        }
        "input.delete" => app.ui.prompt.textarea.delete(),
        "input.undo" => app.ui.prompt.textarea.undo(),
        "input.redo" => app.ui.prompt.textarea.redo(),
        "input.word.forward" => app.ui.prompt.textarea.word_forward(false),
        "input.word.backward" => app.ui.prompt.textarea.word_backward(false),
        "input.select.word.forward" => app.ui.prompt.textarea.word_forward(true),
        "input.select.word.backward" => app.ui.prompt.textarea.word_backward(true),
        "input.delete.word.forward" => app.ui.prompt.textarea.delete_word_forward(),
        "input.delete.word.backward" => app.ui.prompt.textarea.delete_word_backward(),
        "input.select.all" => app.ui.prompt.textarea.select_all(),
        "session.interrupt" => {
            // The interrupt command exits shell mode first
            // (`prompt/index.tsx:396-421`); otherwise it falls through
            // to the command's own double-press abort.
            if app.ui.prompt.mode == PromptMode::Shell {
                app.ui.prompt.set_mode(PromptMode::Normal);
                return true;
            }
            return false;
        }
        _ => return false,
    }
    if !cursor_moved {
        after_content_change(app);
    }
    true
}

/// Unbound key input: plain characters insert into the buffer; the
/// inline bindings — `!` shell-mode entry, escape shell-mode exit —
/// live here (`prompt/index.tsx:816-860`).
pub fn text_input(app: &mut App, key: &crossterm::event::KeyEvent) -> bool {
    if !app.ui.prompt_focused || !app.ui.dialogs.is_empty() {
        return false;
    }
    use crossterm::event::{KeyCode, KeyModifiers};
    if key.code == KeyCode::Esc && app.ui.prompt.mode == PromptMode::Shell {
        app.ui.prompt.set_mode(PromptMode::Normal);
        return true;
    }
    let KeyCode::Char(char) = key.code else {
        return false;
    };
    if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
        return false;
    }
    if char == '!'
        && app.ui.prompt.mode == PromptMode::Normal
        && app.ui.prompt.textarea.cursor() == 0
        && !app.ui.prompt.autocomplete.visible()
    {
        app.ui.prompt.set_mode(PromptMode::Shell);
        return true;
    }
    app.ui.prompt.textarea.insert_text(&char.to_string());
    after_content_change(app);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tracked_pasted_text_replaces_back_to_front() {
        let result = expand_tracked_pasted_text(
            "x [Pasted ~3 lines] y [Image 1] ",
            vec![
                (2, 19, "first\nsecond".to_string()),
                (22, 31, "<image-bytes>".to_string()),
            ],
        );
        assert_eq!(result, "x first\nsecond y <image-bytes> ");
    }

    #[test]
    fn expands_pasted_text_placeholders() {
        let parts = vec![PromptPart::Text {
            text: "real content".into(),
            synthetic: None,
            source: Some(TextSource {
                start: 0,
                end: 17,
                value: "[Pasted ~3 lines]".into(),
            }),
        }];
        assert_eq!(
            expand_pasted_text_placeholders("hello [Pasted ~3 lines] ", &parts),
            "hello real content "
        );
    }

    #[test]
    fn part_json_round_trip() {
        let part = PromptPart::File {
            mime: "image/png".into(),
            filename: Some("shot.png".into()),
            url: "data:image/png;base64,AAA".into(),
            source: Some(FileSource {
                kind: Some("file".into()),
                path: Some("/tmp/shot.png".into()),
                text: Some(TextSource {
                    start: 0,
                    end: 8,
                    value: "[Image 1]".into(),
                }),
            }),
        };
        let value = part.to_value();
        // JSON object member order is not wire-significant — compare
        // parsed values.
        assert_eq!(
            value,
            serde_json::from_str::<serde_json::Value>(
                r#"{"type":"file","mime":"image/png","filename":"shot.png","url":"data:image/png;base64,AAA","source":{"type":"file","path":"/tmp/shot.png","text":{"start":0,"end":8,"value":"[Image 1]"}}}"#
            )
            .unwrap()
        );
        assert_eq!(PromptPart::from_value(&value), Some(part));
    }

    #[test]
    fn pasted_filepath_strips_quotes_and_file_urls() {
        assert_eq!(pasted_filepath("\"/a b/c.txt\""), "/a b/c.txt");
        assert_eq!(pasted_filepath("file:///tmp/x%20y.md"), "/tmp/x y.md");
        assert_eq!(pasted_filepath("hello\\ world"), "hello world");
        assert_eq!(
            pasted_filepath("https://example.com"),
            "https://example.com"
        );
    }

    #[test]
    fn mention_trigger_requires_space_before() {
        assert_eq!(mention_trigger_index("hi @fo", 6), Some(3));
        assert_eq!(mention_trigger_index("a@fo", 4), None, "no space before");
        assert_eq!(mention_trigger_index("@fo bar", 8), None, "space in query");
    }
}
