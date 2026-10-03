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
fn option_rows_are_mouse_clickable() {
    // Hover moves the selection and release activates the row
    // (`dialog-select.tsx:640-676`).
    let mut app = new_app();
    app.ui.terminal_width = 80;
    app.ui.terminal_height = 30;
    open(&mut app, PendingDialog::CommandPalette);

    let titles = palette_titles(&app);
    let target = titles
        .iter()
        .position(|title| title.contains("New session"))
        .expect("New session is in the palette");

    // Locate a screen cell that renders the target option row.
    let mut position = None;
    'outer: for row in 0..30u16 {
        for column in 0..80u16 {
            if option_row(&app, column, row) == Some(target) {
                position = Some((column, row));
                break 'outer;
            }
        }
    }
    if position.is_none() {
        panic!("option {target} not on screen: {titles:?}");
    }
    let (column, row) = position.unwrap();

    // `onMouseOver` — hover moves the selection.
    update(
        &mut app,
        crate::state::Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert_eq!(
        app.ui.dialogs.top().expect("still open").select.selected,
        target
    );

    // `onMouseUp` — release activates: navigating home closes the
    // palette.
    update(
        &mut app,
        crate::state::Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert!(app.ui.dialogs.is_empty());
    assert!(matches!(
        app.state.route.data,
        crate::state::route::Route::Home { .. }
    ));
}

#[test]
fn option_row_matches_the_rendered_rows() {
    // 120x40 with a live session — "Switch session" is suggested and
    // renders at the top of the Suggested block.
    let mut app = new_app();
    app.ui.terminal_width = 120;
    app.ui.terminal_height = 40;
    app.state
        .sync
        .session
        .push(alforria_schema::session_v1::V1SessionInfo {
            id: "s1".into(),
            slug: "x".into(),
            project_id: "prj".into(),
            workspace_id: None,
            directory: "/repo".into(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "session".into(),
            agent: None,
            model: None,
            version: "1".into(),
            metadata: None,
            time: alforria_schema::session_v1::V1SessionTime {
                created: 0,
                updated: 0,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        });
    open(&mut app, PendingDialog::CommandPalette);
    let titles = palette_titles(&app);
    assert!(
        titles.iter().any(|t| t.contains("Switch session")),
        "suggested Switch session in palette: {titles:?}"
    );
    // Where does the render put "Switch session"?
    let lines = render_lines(&mut app, 120, 40);
    let row = lines
        .iter()
        .position(|line| line.contains("Switch session"))
        .expect("Switch session renders");
    // The click target maps back to the suggested option.
    let index = titles
        .iter()
        .position(|t| t.contains("Switch session"))
        .unwrap();
    assert_eq!(
        option_row(&app, 40, row as u16),
        Some(index),
        "click on the rendered row resolves to the option"
    );

    // Release activates it — the sessions dialog opens.
    update(
        &mut app,
        crate::state::Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 40,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert!(
        matches!(app.ui.dialogs.top_kind(), Some(PendingDialog::SessionList)),
        "sessions dialog opened, got {:?}",
        app.ui.dialogs.top_kind()
    );
}

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
    // The real terminal dimensions arrive via `Msg::Resize` before the
    // first draw — the dialog window height derives from them.
    app.ui.terminal_width = width;
    app.ui.terminal_height = height;
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
            text.contains("Session ID"),
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

#[test]
fn dialog_backdrop_dims_the_text_behind_it() {
    // OpenTUI composites the `RGBA.fromInts(0, 0, 0, 150)` backdrop
    // over the underlying cells — the text behind a dialog fades too,
    // it does not shine through at full brightness (`dialog.tsx:30-38`).
    let mut app = new_app();
    app.state.route.navigate(Route::Session {
        session_id: "ses_1".into(),
        prompt: None,
    });
    app.state.sync.session = vec![crate::ui::session::tests::session_info("ses_1", "X")];
    app.state.sync.message.insert(
        "ses_1".into(),
        vec![alforria_schema::session_v1::V1Message::User {
            id: "msg_u".into(),
            session_id: "ses_1".into(),
            time: alforria_schema::session_v1::UserTime { created: 1.0 },
            summary: None,
            format: None,
            agent: "build".into(),
            model: alforria_schema::session_v1::V1UserModel {
                model_id: "claude".into(),
                provider_id: "anthropic".into(),
                variant: None,
            },
            system: None,
            tools: None,
        }],
    );
    app.state.sync.part.insert(
        "msg_u".into(),
        vec![alforria_schema::session_v1::V1Part::Text {
            id: "prt_1".into(),
            session_id: "ses_1".into(),
            message_id: "msg_u".into(),
            text: "BEHIND THE DIALOG TEXT".into(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        }],
    );
    open(&mut app, PendingDialog::Model);
    app.ui.terminal_width = 80;
    app.ui.terminal_height = 24;
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            crate::ui::view(&mut app, frame);
        })
        .unwrap();

    // Row 2 holds the transcript text, behind the backdrop.
    let transcript_fg = terminal.backend().buffer()[(7, 2)].fg;
    let theme = app
        .ui
        .theme
        .resolve(&app.state.kv)
        .expect("builtin theme resolves");
    assert!(
        transcript_fg != theme.text.to_color(),
        "the backdrop must dim the text behind the dialog"
    );
    // The dialog panel itself stays opaque (`theme.backgroundPanel`).
    assert_eq!(
        terminal.backend().buffer()[(14, 7)].bg,
        theme.background_panel.to_color()
    );
}

// ------------------------------------------------------ provider oauth

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Effect> {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(code, modifiers)),
    )
}

fn with_libertai_methods(app: &mut App) {
    app.state.sync.provider_auth.insert(
        "libertai".to_string(),
        vec![
            serde_json::json!({"type": "oauth", "label": "Sign in with LibertAI"}),
            serde_json::json!({"type": "api", "label": "API key"}),
        ],
    );
}

const LONG_URL: &str = "https://console.libertai.io/cli?redirect_uri=http%3A%2F%2F127.0.0.1%3A41234%2Fcallback&state=AbCdEfGhIjKlMnOpQrStUv&challenge=0123456789abcdefghijklmnopqrstuvwxyzABCDEFG&client=Alforria";

fn oauth_dialog(auto: bool, rejected: bool) -> PendingDialog {
    PendingDialog::ProviderOauth {
        provider_id: "libertai".to_string(),
        method: 0,
        flow: 7,
        title: "Sign in with LibertAI".to_string(),
        url: LONG_URL.to_string(),
        instructions: "Finish signing in in the browser tab that opened. If your browser runs on another machine, paste the address it lands on.".to_string(),
        auto,
        rejected,
    }
}

#[test]
fn auth_method_selection_routes_oauth_and_api() {
    let mut app = new_app();
    with_libertai_methods(&mut app);

    // The oauth method authorizes (`dialog-provider.tsx:184-205`).
    open(
        &mut app,
        PendingDialog::ProviderAuthMethod {
            provider_id: "libertai".to_string(),
        },
    );
    let effects = key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(
        effects.as_slice(),
        [Effect::ProviderOauthAuthorize { provider_id, method: 0, title }]
            if provider_id == "libertai" && title == "Sign in with LibertAI"
    ));
    assert!(app.ui.dialogs.is_empty());

    // The api method opens the key prompt.
    open(
        &mut app,
        PendingDialog::ProviderAuthMethod {
            provider_id: "libertai".to_string(),
        },
    );
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let effects = key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(effects.is_empty());
    assert_eq!(
        app.ui.dialogs.top_kind(),
        Some(&PendingDialog::ProviderApiKey {
            provider_id: "libertai".to_string()
        })
    );
}

#[test]
fn oauth_dialog_takes_a_pasted_redirect() {
    let mut app = new_app();
    open(&mut app, oauth_dialog(true, true));

    // A bracketed paste lands in the dialog input, on one line.
    update(
        &mut app,
        crate::state::Msg::Paste("http://127.0.0.1:41234/callback?code=abc&state=xyz\n".into()),
    );
    assert_eq!(
        app.ui.dialogs.top().unwrap().input,
        "http://127.0.0.1:41234/callback?code=abc&state=xyz"
    );

    // Submit sends it; the dialog stays up (the flow settles it) with the
    // input cleared and the previous rejection reset.
    let effects = key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(
        effects.as_slice(),
        [Effect::ProviderOauthCallback { provider_id, method: 0, flow: 7, code }]
            if provider_id == "libertai"
                && code == "http://127.0.0.1:41234/callback?code=abc&state=xyz"
    ));
    let frame = app.ui.dialogs.top().unwrap();
    assert!(frame.input.is_empty());
    assert!(matches!(
        frame.kind,
        PendingDialog::ProviderOauth {
            rejected: false,
            ..
        }
    ));

    // Nothing to submit → nothing sent; typed text goes in too.
    assert!(key(&mut app, KeyCode::Enter, KeyModifiers::NONE).is_empty());
    key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    key(&mut app, KeyCode::Char('b'), KeyModifiers::NONE);
    key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.ui.dialogs.top().unwrap().input, "a");

    // ctrl+y copies the link; escape closes.
    let effects = key(&mut app, KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert!(matches!(
        effects.as_slice(),
        [Effect::ClipboardWrite { text, .. }] if text == LONG_URL
    ));
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.ui.dialogs.is_empty());
}

#[test]
fn paste_lands_in_prompt_dialogs_not_behind_them() {
    let mut app = new_app();
    open(
        &mut app,
        PendingDialog::ProviderApiKey {
            provider_id: "libertai".to_string(),
        },
    );
    update(&mut app, crate::state::Msg::Paste("LTAI_secret".into()));
    assert_eq!(app.ui.dialogs.top().unwrap().input, "LTAI_secret");

    // A select dialog doesn't take text: the paste goes to the prompt.
    open(&mut app, PendingDialog::Model);
    assert!(!paste(&mut app, "text"));
}

#[test]
fn oauth_dialog_renders_the_link_instructions_and_state() {
    for width in [80u16, 140] {
        let mut app = new_app();
        open(&mut app, oauth_dialog(true, true));
        let text = rendered_text(&render_lines(&mut app, width, 40));
        assert!(text.contains("Sign in with LibertAI"), "{text}");
        assert!(text.contains("it lands on."), "{text}");
        assert!(text.contains("Waiting for authorization…"), "{text}");
        assert!(text.contains("Paste the address or code here"), "{text}");
        assert!(text.contains("Invalid code"), "{text}");
        assert!(text.contains("ctrl+y"), "{text}");
        // The URL wraps rather than clipping: its head and tail both show.
        assert!(text.contains("https://console.libertai.io/cli?"), "{text}");
        assert!(text.contains("client=Alforria"), "{text}");

        // The code method has no waiting line.
        open(&mut app, oauth_dialog(false, false));
        let text = rendered_text(&render_lines(&mut app, width, 40));
        assert!(!text.contains("Waiting for authorization…"), "{text}");
        assert!(!text.contains("Invalid code"), "{text}");
    }
}

fn with_connected(app: &mut App) {
    app.state.sync.provider_next = serde_json::json!({
        "all": [
            {"id": "libertai", "name": "LibertAI", "source": "api"},
            {"id": "anthropic", "name": "Anthropic", "source": "api"},
            {"id": "openai", "name": "OpenAI", "source": "env"},
        ],
        "connected": ["libertai", "anthropic", "openai"],
    });
}

#[test]
fn stored_credentials_offer_sign_out() {
    let mut app = new_app();
    with_libertai_methods(&mut app);
    with_connected(&mut app);
    let titles = |app: &App, id: &str| -> Vec<String> {
        model::auth_method_options(app, id)
            .into_iter()
            .map(|option| option.title)
            .collect()
    };
    assert_eq!(
        titles(&app, "libertai"),
        ["Sign in with LibertAI", "API key", "Sign out"]
    );
    // A key-only provider removes its key; an env key isn't ours to drop.
    assert_eq!(titles(&app, "anthropic"), ["API key", "Remove API key"]);
    assert_eq!(titles(&app, "openai"), ["API key"]);

    open(
        &mut app,
        PendingDialog::ProviderAuthMethod {
            provider_id: "libertai".to_string(),
        },
    );
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let effects = key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(
        effects.as_slice(),
        [Effect::AuthRemove { provider_id }] if provider_id == "libertai"
    ));
    assert!(app.ui.dialogs.is_empty());
}
