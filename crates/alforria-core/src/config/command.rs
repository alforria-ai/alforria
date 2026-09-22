//! Command Markdown discovery — port of `packages/opencode/src/config/command.ts`.
//!
//! `load(dir)` scans `{command,commands}/**/*.md` under `dir`. Frontmatter
//! parse failures skip the file silently (TS `.catch(() => undefined)`); a
//! schema decode failure is a fatal `InvalidError`.

use std::path::Path;

use serde_json::{Map, Value};

use crate::config::agent::{parse_markdown, relative_name, scan_markdown};
use crate::config::schema::CommandInfo;
use crate::CoreError;

/// `ConfigCommand.load(dir)` — `{name, ...data, template: content.trim()}`.
pub fn load_commands(dir: &Path) -> Result<Value, CoreError> {
    let mut result = Map::new();
    for path in scan_markdown(dir, &["command/**/*.md", "commands/**/*.md"]) {
        let Some(md) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| parse_markdown(&text))
        else {
            continue;
        };
        let name = relative_name(&path, dir, &["command/", "commands/"]);
        let mut config = Map::new();
        config.insert("name".to_owned(), Value::String(name.clone()));
        config.extend(md.data);
        config.insert(
            "template".to_owned(),
            Value::String(md.content.trim().to_owned()),
        );
        let command: CommandInfo = serde_json::from_value(Value::Object(config))
            .map_err(|err| CoreError::invalid(&path, err.to_string()))?;
        let value = serde_json::to_value(&command)
            .map_err(|err| CoreError::invalid(&path, err.to_string()))?;
        result.insert(name, value);
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "alforria-core-m33-command-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn commands_discovered_with_trimmed_template() {
        let dir = temp_dir();
        write(
            &dir.join("command").join("deploy.md"),
            "---\ndescription: ship it\nagent: build\n---\n  Deploy!  \n",
        );
        write(
            &dir.join("commands").join("nested").join("gen.md"),
            "---\n---\nGenerate.\n",
        );
        let commands = load_commands(&dir).unwrap();
        let deploy = commands.get("deploy").unwrap();
        assert_eq!(deploy["description"], json!("ship it"));
        assert_eq!(deploy["agent"], json!("build"));
        assert_eq!(deploy["template"], json!("Deploy!"));
        assert_eq!(commands["nested/gen"]["template"], json!("Generate."));
    }

    #[test]
    fn command_decode_failure_is_fatal() {
        let dir = temp_dir();
        // subtask must be a boolean.
        write(
            &dir.join("command").join("bad.md"),
            "---\nsubtask: 5\n---\nbody\n",
        );
        let err = load_commands(&dir).unwrap_err();
        assert!(matches!(err, CoreError::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn command_frontmatter_failure_is_skipped() {
        let dir = temp_dir();
        write(
            &dir.join("command").join("bad.md"),
            "---\nkey: [unclosed\n---\nbody\n",
        );
        write(&dir.join("command").join("good.md"), "---\n---\nbody\n");
        let commands = load_commands(&dir).unwrap();
        assert!(commands.get("bad").is_none());
        assert!(commands.get("good").is_some());
    }

    #[test]
    fn markdown_files_without_frontmatter_are_bodies() {
        let dir = temp_dir();
        write(&dir.join("command").join("plain.md"), "Just a template.\n");
        let commands = load_commands(&dir).unwrap();
        assert_eq!(commands["plain"]["template"], json!("Just a template."));
    }
}
