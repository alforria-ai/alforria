//! `util/locale.ts` + the small display helpers the session renderers
//! need (`util/collapse-tool-output.ts`, `runtime.abbreviateHome`,
//! `context/path-format.tsx`, `util/tool-display.ts`).

/// `Locale.duration` (`util/locale.ts:39-58`).
pub fn duration(input: i64) -> String {
    if input < 1000 {
        return format!("{input}ms");
    }
    if input < 60000 {
        return format!("{:.1}s", input as f64 / 1000.0);
    }
    if input < 3600000 {
        let minutes = input / 60000;
        let seconds = (input % 60000) / 1000;
        return format!("{minutes}m {seconds}s");
    }
    if input < 86400000 {
        let hours = input / 3600000;
        let minutes = (input % 3600000) / 60000;
        return format!("{hours}h {minutes}m");
    }
    let days = input / 86400000;
    let hours = (input % 86400000) / 3600000;
    format!("{days}d {hours}h")
}

/// `Locale.titlecase` (`util/locale.ts:1-3`): `/\b\w/g` — uppercase
/// every word character at the start of a word (`[a-zA-Z0-9_]` runs).
pub fn titlecase(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut previous_word = false;
    for char in input.chars() {
        let word = char.is_ascii_alphanumeric() || char == '_';
        if word && !previous_word {
            out.extend(char.to_uppercase());
        } else {
            out.push(char);
        }
        previous_word = word;
    }
    out
}

/// `Locale.truncate` (`util/locale.ts:61-64`) — char-indexed with a
/// trailing `…` (same as `crate::app::truncate`).
pub fn truncate(input: &str, len: usize) -> String {
    if input.chars().count() <= len {
        return input.to_string();
    }
    let prefix: String = input.chars().take(len.saturating_sub(1)).collect();
    format!("{prefix}…")
}

/// `Locale.time` / `todayTimeOrDateTime` (`util/locale.ts:7-35`) — the
/// TS formats are locale-dependent; this port renders UTC (`HH:MM` /
/// `HH:MM · YYYY-MM-DD`).
pub fn today_time_or_date_time(input: i64) -> String {
    let days = input.div_euclid(86_400_000);
    let millis = input.rem_euclid(86_400_000) as u64;
    let (hour, minute) = (millis / 3_600_000, (millis % 3_600_000) / 60_000);
    let (year, month, day) = civil_from_days(days);
    let time = format!("{hour:02}:{minute:02}");
    if days == current_utc_day() {
        time
    } else {
        format!("{time} · {year:04}-{month:02}-{day:02}")
    }
}

pub fn current_utc_day() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    now.div_euclid(86_400_000)
}

/// Days since the unix epoch → `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// `abbreviateHome` (`runtime.tsx:3-10`).
pub fn abbreviate_home(input: &str, home: &str) -> String {
    if home.is_empty() {
        return input.to_string();
    }
    let relative = relative_path(home, input);
    match relative {
        None => input.to_string(),
        Some(path) if path.is_empty() => "~".to_string(),
        Some(path) => {
            if path == ".." || path.starts_with("../") || path.starts_with('/') {
                input.to_string()
            } else {
                format!("~/{path}")
            }
        }
    }
}

/// `path.relative(from, to)` — returns `None` when the result would
/// start escaping (`..`) or when `to` is absolute.
fn relative_path(from: &str, to: &str) -> Option<String> {
    if to.starts_with('/') && !from.starts_with('/') {
        return None;
    }
    let mut from: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let to_abs = to.starts_with('/');
    let mut target: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    if !to_abs {
        // path.resolve(base, input) — resolve relative inputs against base.
        from.extend(target);
        target = from.clone();
    }
    let mut common = 0;
    while common < from.len() && common < target.len() && from[common] == target[common] {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..from.len() {
        parts.push("..".to_string());
    }
    for part in &target[common..] {
        parts.push(part.to_string());
    }
    Some(parts.join("/"))
}

/// `formatPath` (`context/path-format.tsx:26-40`): resolve against the
/// base directory, return the path relative to it — `..`-escapes fall
/// back to `abbreviateHome`.
pub fn format_path(input: Option<&str>, base: &str, home: &str) -> String {
    let Some(input) = input else {
        return String::new();
    };
    if input.is_empty() {
        return String::new();
    }
    let absolute = if input.starts_with('/') {
        input.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), input)
    };
    let relative = relative_path(base, &absolute).unwrap_or_default();
    if relative.is_empty() {
        return ".".to_string();
    }
    if relative != ".." && !relative.starts_with("../") && !relative.starts_with('/') {
        return relative.replace('\\', "/");
    }
    abbreviate_home(&absolute, home)
}

/// `collapseToolOutput` (`util/collapse-tool-output.ts`).
pub fn collapse_tool_output(output: &str, max_lines: usize, max_chars: usize) -> Collapse {
    let lines: Vec<&str> = output.split('\n').collect();
    let chars = output.chars().count();
    if lines.len() <= max_lines && chars <= max_chars {
        return Collapse {
            output: output.to_string(),
            overflow: false,
        };
    }
    let preview: String = lines[..max_lines.min(lines.len())].join("\n");
    if preview.chars().count() > max_chars {
        let keep = max_chars.saturating_sub(1);
        let truncated: String = preview.chars().take(keep).collect();
        return Collapse {
            output: format!("{truncated}…"),
            overflow: true,
        };
    }
    let mut collapsed = lines[..max_lines.min(lines.len())].to_vec();
    collapsed.push("…");
    Collapse {
        output: collapsed.join("\n"),
        overflow: true,
    }
}

/// The `{output, overflow}` pair (`util/collapse-tool-output.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collapse {
    pub output: String,
    pub overflow: bool,
}

/// `input()` (`routes/session/index.tsx:2609-2616`): the
/// `[key=value, …]` tag of primitive input entries.
pub fn input_tag(input: &serde_json::Map<String, serde_json::Value>, omit: &[&str]) -> String {
    let entries: Vec<String> = input
        .iter()
        .filter(|(key, value)| {
            !omit.iter().any(|item| item == key)
                && (value.is_string() || value.is_number() || value.is_boolean())
        })
        .map(|(key, value)| match value {
            serde_json::Value::String(text) => format!("{key}={text}"),
            other => format!("{key}={other}"),
        })
        .collect();
    if entries.is_empty() {
        return String::new();
    }
    format!("[{}]", entries.join(", "))
}

/// `webSearchProviderLabel` (`util/tool-display.ts:1-5`).
pub fn web_search_provider_label(provider: &serde_json::Value) -> &'static str {
    match provider.as_str() {
        Some("parallel") => "Parallel Web Search",
        Some("exa") => "Exa Web Search",
        _ => "Web Search",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_matches_locale_ts() {
        assert_eq!(duration(500), "500ms");
        assert_eq!(duration(1500), "1.5s");
        assert_eq!(duration(61000), "1m 1s");
        assert_eq!(duration(3_660_000), "1h 1m");
        assert_eq!(duration(90_000_000), "1d 1h");
    }

    #[test]
    fn titlecase_uppercases_each_word() {
        assert_eq!(titlecase("hello world"), "Hello World");
        assert_eq!(titlecase("web-fetch tool"), "Web-Fetch Tool");
        assert_eq!(titlecase(""), "");
    }

    #[test]
    fn truncate_slices_characters() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn today_time_or_date_time_formats_utc() {
        // 2026-09-20T00:00:00Z in ms.
        let date = 1_789_222_400_000i64;
        let days = date.div_euclid(86_400_000);
        let text = today_time_or_date_time(date);
        // Today (whatever the clock says) is within a day of the stamp.
        assert!(
            (days - current_utc_day()).abs() <= 1 || text.contains(" · "),
            "{text}"
        );
        // A same-day stamp renders as plain time.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(!today_time_or_date_time(now).contains('·'));
    }

    #[test]
    fn abbreviate_home_matches_relative() {
        assert_eq!(abbreviate_home("/home/jon/x", "/home/jon"), "~/x");
        assert_eq!(abbreviate_home("/home/jon", "/home/jon"), "~");
        assert_eq!(abbreviate_home("/elsewhere/x", "/home/jon"), "/elsewhere/x");
        assert_eq!(abbreviate_home("/x", ""), "/x");
    }

    #[test]
    fn format_path_resolves_relative_to_base() {
        // `path.relative(base, resolve(base, input))`.
        assert_eq!(format_path(Some("/a/b/c.txt"), "/a/b", "/home"), "c.txt");
        assert_eq!(format_path(Some("/a/b"), "/a/b", "/home"), ".");
        assert_eq!(format_path(Some("c.txt"), "/a/b", "/home"), "c.txt");
        assert_eq!(
            format_path(Some("/home/j/c.txt"), "/a/b", "/home/j"),
            "~/c.txt"
        );
        assert_eq!(format_path(None, "/a/b", "/home"), "");
        assert_eq!(format_path(Some(""), "/a/b", "/home"), "");
    }

    #[test]
    fn collapse_output_limits_lines_and_chars() {
        assert_eq!(
            collapse_tool_output("a\nb", 10, 100),
            Collapse {
                output: "a\nb".into(),
                overflow: false
            }
        );
        let collapsed = collapse_tool_output("1\n2\n3\n4", 2, 100);
        assert!(collapsed.overflow);
        assert_eq!(collapsed.output, "1\n2\n…");
        let chars = collapse_tool_output("abcdefghij", 10, 5);
        assert!(chars.overflow);
        assert_eq!(chars.output, "abcd…");
    }

    #[test]
    fn input_tag_renders_primitives_only() {
        let input = serde_json::json!({
            "command": "ls",
            "count": 3,
            "flag": true,
            "filePath": "skip me",
            "nested": {"a": 1},
        });
        let map = input.as_object().unwrap();
        assert_eq!(
            input_tag(map, &["filePath"]),
            "[command=ls, count=3, flag=true]"
        );
        // Nested values are dropped — only primitives render; the omit
        // list filters on top.
        assert_eq!(
            input_tag(map, &[]),
            "[command=ls, count=3, filePath=skip me, flag=true]"
        );
        assert_eq!(
            input_tag(&serde_json::Map::new(), &[]),
            "",
            "empty input renders no tag"
        );
    }

    #[test]
    fn web_search_labels() {
        assert_eq!(
            web_search_provider_label(&serde_json::json!("parallel")),
            "Parallel Web Search"
        );
        assert_eq!(
            web_search_provider_label(&serde_json::json!("exa")),
            "Exa Web Search"
        );
        assert_eq!(
            web_search_provider_label(&serde_json::json!(null)),
            "Web Search"
        );
    }
}
