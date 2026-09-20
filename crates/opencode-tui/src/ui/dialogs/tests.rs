//! Dialog stack + palette tests (`scratchpad/specs/M8.md` M8.7).

use crossterm::event::{KeyCode, KeyModifiers};

use super::primitives::filter_options;
use super::*;
use crate::state::update;

fn new_app() -> App {
    App::new(
        crate::config::TuiConfig::default(),
        crate::state::Args::default(),
        None,
    )
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(code, modifiers)),
    );
}

fn press_ctrl(app: &mut App, char: char) {
    press(app, KeyCode::Char(char), KeyModifiers::CONTROL);
}

// --------------------------------------------------------- stack machine

#[test]
fn open_replaces_and_escape_pops() {
    let mut app = new_app();
    open(&mut app, PendingDialog::CommandPalette);
    assert!(matches!(
        app.ui.dialogs.top_kind(),
        Some(PendingDialog::CommandPalette)
    ));

    // `replace` semantics — open closes everything first.
    open(&mut app, PendingDialog::Model);
    assert_eq!(app.ui.dialogs.stack.len(), 1);
    assert!(matches!(
        app.ui.dialogs.top_kind(),
        Some(PendingDialog::Model)
    ));

    // Escape pops the top (`dialog.tsx:105-137`).
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.ui.dialogs.is_empty());
}

#[test]
fn ctrl_c_pops_and_clear_closes_all() {
    let mut app = new_app();
    open(&mut app, PendingDialog::CommandPalette);
    press_ctrl(&mut app, 'c');
    assert!(app.ui.dialogs.is_empty());

    open(&mut app, PendingDialog::CommandPalette);
    clear(&mut app);
    assert!(app.ui.dialogs.is_empty());
}

#[test]
fn open_pushes_the_modal_mode() {
    let mut app = new_app();
    open(&mut app, PendingDialog::CommandPalette);
    crate::app::post_update(&mut app);
    assert_eq!(app.keymap.modes.current(), crate::keymap::MODAL_MODE);
    clear(&mut app);
    crate::app::post_update(&mut app);
    assert_eq!(app.keymap.modes.current(), crate::keymap::BASE_MODE);
}

#[test]
fn sizes_follow_the_dialog_kinds() {
    let mut app = new_app();
    for (kind, size) in [
        (PendingDialog::Model, DialogSize::Medium),
        (PendingDialog::Skill, DialogSize::Large),
        (PendingDialog::MoveSession, DialogSize::Xlarge),
    ] {
        open(&mut app, kind);
        assert_eq!(app.ui.dialogs.size, size);
        clear(&mut app);
    }
}

// ------------------------------------------------------------ palette

fn palette_titles(app: &App) -> Vec<String> {
    let Some(frame) = app.ui.dialogs.top() else {
        return Vec::new();
    };
    let filter = frame.select.filter.clone();
    let options = options(app, frame);
    filter_options(&filter, options)
        .into_iter()
        .map(|option| option.title)
        .collect()
}

#[test]
fn palette_filter_goldens() {
    let mut app = new_app();
    open(&mut app, PendingDialog::CommandPalette);

    // Unfiltered: every palette command, in registry order.
    let unfiltered = palette_titles(&app);
    assert!(
        unfiltered
            .iter()
            .any(|label| label.contains("Exit the app")),
        "exit is in the palette: {unfiltered:?}"
    );

    // A fuzzy filter narrows the list.
    for char in "exi".chars() {
        press(&mut app, KeyCode::Char(char), KeyModifiers::NONE);
    }
    let filtered = palette_titles(&app);
    assert!(
        filtered.iter().any(|label| label.contains("Exit the app")),
        "exit is reachable through the filter: {filtered:?}"
    );
    assert!(filtered.len() < unfiltered.len());
}

#[test]
fn palette_selection_moves_and_submits() {
    let mut app = new_app();
    open(&mut app, PendingDialog::CommandPalette);
    // Filter down to "New session" and submit — it navigates home and
    // closes the palette.
    for char in "new se".chars() {
        press(&mut app, KeyCode::Char(char), KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.ui.dialogs.is_empty());
    assert!(matches!(
        app.state.route.data,
        crate::state::route::Route::Home { .. }
    ));
}

// ---------------------------------------------------- rendering goldens

fn render_lines(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::view(app, frame);
        })
        .unwrap();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn rendered_text(lines: &[String]) -> String {
    lines.join("\n")
}

#[test]
fn dialog_snapshots_at_80_and_140_columns() {
    for width in [80u16, 140] {
        let mut app = new_app();

        // Command palette.
        open(&mut app, PendingDialog::CommandPalette);
        let text = rendered_text(&render_lines(&mut app, width, 30));
        assert!(
            text.contains("New session"),
            "palette renders its options at {width} columns"
        );
        clear(&mut app);

        // Help.
        open(&mut app, PendingDialog::Help);
        let text = rendered_text(&render_lines(&mut app, width, 30));
        assert!(
            text.contains("Help"),
            "help dialog renders at {width} columns"
        );
        clear(&mut app);

        // Debug.
        open(&mut app, PendingDialog::Debug);
        let text = rendered_text(&render_lines(&mut app, width, 30));
        assert!(
            text.contains("SessionID"),
            "debug dialog renders at {width} columns"
        );
        clear(&mut app);
    }
}

#[test]
fn select_dialogs_render_their_titles() {
    // Every select-style dialog renders its title + filter at both the
    // medium and large widths.
    for width in [80u16, 140] {
        let mut app = new_app();
        for (kind, title) in [
            (PendingDialog::Model, "Select model"),
            (PendingDialog::Agent, "Select agent"),
            (PendingDialog::Variant, "Select variant"),
            (PendingDialog::ProviderConnect, "Connect a provider"),
            (PendingDialog::Mcp, "MCPs"),
            (PendingDialog::ThemeList, "Themes"),
            (PendingDialog::SessionList, "Sessions"),
            (PendingDialog::Skill, "Skills"),
            (PendingDialog::StashList, "Stash"),
            (PendingDialog::MoveSession, "Move session"),
            (PendingDialog::WorkspaceList, "Workspaces"),
            (PendingDialog::WorkspaceSet, "Warp"),
            (PendingDialog::Tag, "Autocomplete"),
        ] {
            open(&mut app, kind.clone());
            let text = rendered_text(&render_lines(&mut app, width, 30));
            assert!(
                text.contains(title),
                "{title} renders at {width} columns:\n{text}"
            );
            clear(&mut app);
        }
    }
}
