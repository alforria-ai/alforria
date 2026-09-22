//! `component/command-palette.tsx` — the ctrl+p palette. With an empty
//! filter the suggested commands are listed first under a "Suggested"
//! category (`command-palette.tsx:64-76`); a typed filter searches the
//! plain list.

use super::primitives::SelectOption;
use crate::state::App;

pub fn options(app: &App, frame: &crate::ui::dialogs::DialogFrame) -> Vec<SelectOption> {
    let commands = crate::command::palette(app);
    // The `footer: formatKeyBindings(entry.bindings, config)` of the
    // palette options (`command-palette.tsx:53`).
    let footer = |command: &crate::command::CommandInfo| {
        crate::keymap::bindings::keybind_for_command(command.name)
            .and_then(|keybind| crate::ui::dialogs::key_hint(app, keybind))
            .unwrap_or_default()
    };
    let all = || {
        commands
            .iter()
            .map(|command| {
                SelectOption::new(command.title.clone())
                    .with_value(command.name.to_string())
                    .with_category(command.category)
                    .with_footer(footer(command))
            })
            .collect::<Vec<_>>()
    };
    if !frame.select.filter.is_empty() {
        return all();
    }
    let mut options: Vec<SelectOption> = commands
        .iter()
        .filter(|command| command.suggested)
        .map(|command| {
            SelectOption::new(command.title.clone())
                .with_value(format!("suggested:{}", command.name))
                .with_category("Suggested")
                .with_footer(footer(command))
        })
        .collect();
    options.extend(all());
    options
}
