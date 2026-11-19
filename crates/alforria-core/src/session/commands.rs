//! Built-in command defaults shared by the session engine and server.

use std::path::Path;

use crate::config::schema::CommandInfo;

pub const INITIALIZE: &str = include_str!("prompt/command_initialize.txt");
pub const REVIEW: &str = include_str!("prompt/command_review.txt");
pub const NAMES: &[&str] = &["init", "review"];

pub fn builtin(name: &str, directory: &Path) -> Option<CommandInfo> {
    let (template, description, subtask) = match name {
        "init" => (INITIALIZE, "guided AGENTS.md setup", None),
        "review" => (
            REVIEW,
            "review changes [commit|branch|pr], defaults to uncommitted",
            Some(true),
        ),
        _ => return None,
    };
    Some(CommandInfo {
        template: template.replace("${path}", &directory.to_string_lossy()),
        description: Some(description.to_string()),
        agent: None,
        model: None,
        variant: None,
        subtask,
    })
}
