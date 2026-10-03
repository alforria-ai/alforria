//! Dialog modality across every dialog family: key isolation, dismissal,
//! focus and selection, and layout at any terminal size
//! (`ui/dialog.tsx`, `ui/dialog-select.tsx`).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::*;
use crate::state::{update, Msg};

fn new_app() -> App {
    App::new(
        crate::config::TuiConfig::default(),
        crate::state::Args::default(),
        None,
    )
}

fn session(id: &str, title: &str) -> alforria_schema::session_v1::V1SessionInfo {
    crate::ui::session::tests::session_info(id, title)
}

fn permission(id: &str, session_id: &str) -> alforria_schema::permission_v1::PermissionV1Request {
    alforria_schema::permission_v1::PermissionV1Request {
        id: id.into(),
        session_id: session_id.into(),
        permission: "bash".into(),
        patterns: vec!["bun run db:migrate".into()],
        metadata: Default::default(),
        always: vec!["*".into()],
        tool: None,
    }
}

/// A busy session with a pending permission, a prompt draft and a
/// transcript scrolled off the bottom — everything a stray key could
/// change behind a dialog.
fn busy_app() -> App {
    let mut app = new_app();
    app.ui.terminal_width = 100;
    app.ui.terminal_height = 30;
    app.state.sync.session = vec![
        session("ses_1", "Apply pending migrations"),
        session("ses_2", "Fix the build"),
    ];
    app.state.sync.session_status.insert(
        "ses_1".into(),
        alforria_schema::session_status::SessionStatusInfo::Busy,
    );
    app.state
        .sync
        .permission
        .insert("ses_1".into(), vec![permission("per_1", "ses_1")]);
    app.state.route.navigate(Route::Session {
        session_id: "ses_1".into(),
        prompt: None,
    });
    app.ui.prompt.textarea.set_text("draft text");
    app.ui.session_scroll.content_height = 100;
    app.ui.session_scroll.viewport_height = 20;
    app.ui.session_scroll.y = 40;
    app.ui.session_scroll.sticky = false;
    crate::app::post_update(&mut app);
    app
}

/// What the screen behind a dialog holds.
#[derive(Debug, PartialEq)]
struct Behind {
    route: Route,
    prompt: String,
    permissions: usize,
    permission: String,
    scroll: (usize, bool),
    exit: bool,
    interrupt: u32,
    leader: bool,
}

fn behind(app: &App) -> Behind {
    let permission = &app.ui.permission;
    Behind {
        route: app.state.route.data.clone(),
        prompt: app.ui.prompt.input().to_string(),
        permissions: app.state.sync.permission.values().map(Vec::len).sum(),
        permission: format!(
            "{:?}/{}/{}/{}",
            permission.stage, permission.selected, permission.expanded, permission.reject_input
        ),
        scroll: (app.ui.session_scroll.y, app.ui.session_scroll.sticky),
        exit: app.ui.exit,
        interrupt: app.ui.interrupt,
        leader: app.keymap.leader_active(),
    }
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Effect> {
    update(app, Msg::Key(KeyEvent::new(code, modifiers)))
}

fn release(app: &mut App, code: KeyCode) -> Vec<Effect> {
    let mut key = KeyEvent::new(code, KeyModifiers::NONE);
    key.kind = KeyEventKind::Release;
    update(app, Msg::Key(key))
}

/// Effects only the screen behind a dialog produces.
fn leaked(effects: &[Effect]) -> Vec<&Effect> {
    effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::PermissionReply { .. }
                    | Effect::QuestionReply { .. }
                    | Effect::QuestionReject { .. }
                    | Effect::SessionAbort { .. }
                    | Effect::PromptSubmit { .. }
                    | Effect::SessionFork { .. }
            )
        })
        .collect()
}

/// One dialog of each family.
fn families() -> Vec<PendingDialog> {
    vec![
        // DialogSelect
        PendingDialog::CommandPalette,
        PendingDialog::Model,
        PendingDialog::Agent,
        PendingDialog::SessionList,
        PendingDialog::ThemeList,
        PendingDialog::Mcp,
        PendingDialog::Skill,
        PendingDialog::ProviderConnect,
        // DialogPrompt
        PendingDialog::SessionRename {
            session_id: "ses_1".into(),
        },
        PendingDialog::ProviderApiKey {
            provider_id: "libertai".into(),
        },
        PendingDialog::ProviderCustomId,
        // DialogConfirm-style
        PendingDialog::ShareConsent {
            session_id: "ses_1".into(),
        },
        PendingDialog::UpdateAvailable {
            version: "9.9.9".into(),
        },
        PendingDialog::WorkspaceUnavailable,
        PendingDialog::SessionDeleteFailed {
            session_id: "ses_1".into(),
            workspace: "wrk_1".into(),
        },
        PendingDialog::RetryAction {
            title: "Go".into(),
            message: "Upgrade".into(),
            label: "upgrade".into(),
            link: None,
            kv_key: None,
        },
        // DialogAlert / help / info
        PendingDialog::Alert {
            title: "Heads up".into(),
            message: "Something happened".into(),
            exit_on_confirm: false,
        },
        PendingDialog::Help,
        PendingDialog::Status,
        PendingDialog::Debug,
        PendingDialog::ConsoleOrg,
        // forms
        PendingDialog::ExportOptions,
        PendingDialog::ProviderOauth {
            provider_id: "libertai".into(),
            method: 0,
            flow: 1,
            title: "Sign in with LibertAI".into(),
            url: "https://console.libertai.io/cli".into(),
            instructions: "Finish in the browser.".into(),
            auto: true,
            rejected: false,
        },
    ]
}

/// The text of a dialog's input, wherever its kind keeps it.
fn typed(app: &App) -> String {
    let frame = app.ui.dialogs.top().expect("dialog open");
    if is_select_kind(&frame.kind) {
        frame.select.filter.clone()
    } else {
        frame.input.clone()
    }
}

#[test]
fn no_key_reaches_the_screen_behind_any_dialog() {
    for kind in families() {
        let mut app = busy_app();
        let before = behind(&app);
        let theme = app.ui.theme.active.clone();
        let _ = open(&mut app, kind.clone());
        crate::app::post_update(&mut app);
        let input_before = typed(&app);

        let mut effects = Vec::new();
        // Global shortcut letters and the permission's own keys (`h`/`l`,
        // left/right) type or stay inside the dialog.
        for char in "qhlyn1".chars() {
            effects.extend(press(&mut app, KeyCode::Char(char), KeyModifiers::NONE));
        }
        let accepts_text = matches!(
            kind,
            PendingDialog::SessionRename { .. }
                | PendingDialog::ProviderApiKey { .. }
                | PendingDialog::ProviderCustomId
                | PendingDialog::ProviderOauth { .. }
                | PendingDialog::ExportOptions
        ) || is_select_kind(&kind);
        if accepts_text {
            assert_eq!(
                typed(&app),
                format!("{input_before}qhlyn1"),
                "{kind:?}: typed text belongs to the dialog input"
            );
        }
        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Up,
            KeyCode::Down,
        ] {
            effects.extend(press(&mut app, code, KeyModifiers::NONE));
        }
        // The leader, the palette, the exit chord and a leader sequence.
        for (code, modifiers) in [
            (KeyCode::Char('x'), KeyModifiers::CONTROL),
            (KeyCode::Char('n'), KeyModifiers::NONE),
            (KeyCode::Char('x'), KeyModifiers::CONTROL),
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Char('p'), KeyModifiers::CONTROL),
            (KeyCode::Char('d'), KeyModifiers::CONTROL),
        ] {
            effects.extend(press(&mut app, code, modifiers));
        }
        assert!(
            leaked(&effects).is_empty(),
            "{kind:?} leaked {:?}",
            leaked(&effects)
        );
        assert_eq!(
            app.ui.dialogs.top_kind(),
            Some(&kind),
            "{kind:?}: still the open dialog"
        );
        assert_eq!(behind(&app), before, "{kind:?}: the screen behind changed");

        // Escape closes the dialog and nothing else: the permission is
        // not rejected, the busy session not interrupted. Its release
        // (Windows consoles report one) lands nowhere either.
        effects = press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        effects.extend(release(&mut app, KeyCode::Esc));
        assert!(app.ui.dialogs.is_empty(), "{kind:?}: escape closes");
        assert!(leaked(&effects).is_empty(), "{kind:?} leaked {effects:?}");
        assert_eq!(behind(&app), before, "{kind:?}: escape leaked");
        assert_eq!(app.ui.theme.active, theme, "{kind:?}: preview reverted");
        // Focus is back on the permission prompt, which stands in for
        // the prompt while the request is pending.
        assert!(
            crate::ui::session::permission::visible(&app).is_some() && !app.ui.prompt_focused,
            "{kind:?}: focus back on the permission prompt"
        );
    }
}

#[test]
fn ctrl_c_closes_every_dialog_without_exiting() {
    for kind in families() {
        let mut app = busy_app();
        app.ui.prompt.textarea.set_text("");
        let _ = open(&mut app, kind.clone());
        let effects = press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.ui.dialogs.is_empty(), "{kind:?}: ctrl+c closes");
        assert!(!app.ui.exit, "{kind:?}: ctrl+c must not also quit");
        assert!(leaked(&effects).is_empty(), "{kind:?} leaked {effects:?}");
    }
}

#[test]
fn closing_restores_the_prompt_with_its_draft() {
    let mut app = new_app();
    app.ui.prompt.textarea.set_text("half a thought");
    let _ = open(&mut app, PendingDialog::CommandPalette);
    crate::app::post_update(&mut app);
    assert!(!app.ui.prompt_focused, "the dialog holds the focus");
    press(&mut app, KeyCode::Char('z'), KeyModifiers::NONE);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.ui.prompt_focused);
    assert_eq!(app.ui.prompt.input(), "half a thought");
    // The next key types into the prompt again.
    press(&mut app, KeyCode::Char('!'), KeyModifiers::NONE);
    assert_eq!(app.ui.prompt.input(), "half a thought!");
}

#[test]
fn the_update_complete_alert_exits_however_it_closes() {
    // `DialogAlert.show(...)` resolves on confirm and on close alike;
    // `app.tsx:1072-1078` exits after it.
    for code in [KeyCode::Enter, KeyCode::Esc] {
        let mut app = new_app();
        let _ = open(
            &mut app,
            PendingDialog::Alert {
                title: "Update Complete".into(),
                message: "Restart".into(),
                exit_on_confirm: true,
            },
        );
        press(&mut app, code, KeyModifiers::NONE);
        assert!(app.ui.exit, "{code:?}");
    }
}

#[test]
fn clicking_the_esc_hint_closes_the_dialog() {
    let mut app = busy_app();
    let _ = open(&mut app, PendingDialog::CommandPalette);
    let buffer = draw(&mut app, 100, 30);
    let rect = placed(&app).expect("placed").rect;
    let row = rect.y + 1;
    let column = (rect.x..rect.right())
        .find(|&x| {
            ["e", "s", "c"]
                .iter()
                .enumerate()
                .all(|(i, s)| buffer[(x + i as u16, row)].symbol() == *s)
        })
        .expect("esc hint on the header row");
    let click = |kind| {
        Msg::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    update(
        &mut app,
        click(crossterm::event::MouseEventKind::Down(
            crossterm::event::MouseButton::Left,
        )),
    );
    let effects = update(
        &mut app,
        click(crossterm::event::MouseEventKind::Up(
            crossterm::event::MouseButton::Left,
        )),
    );
    assert!(app.ui.dialogs.is_empty());
    assert!(leaked(&effects).is_empty());
}

// ------------------------------------------------- focus and selection

fn selected_title(app: &App) -> Option<String> {
    selected_option(app).map(|option| option.title)
}

fn with_models(app: &mut App) {
    app.state.sync.provider = vec![
        serde_json::json!({
            "id": "anthropic", "name": "Anthropic",
            "models": {
                "claude-a": {"id": "claude-a", "name": "Claude A", "release_date": "2025-01-01"},
                "claude-b": {"id": "claude-b", "name": "Claude B", "release_date": "2025-02-01"},
            },
        }),
        serde_json::json!({
            "id": "openai", "name": "OpenAI",
            "models": {
                "gpt-a": {"id": "gpt-a", "name": "GPT A", "release_date": "2025-03-01"},
                "gpt-b": {"id": "gpt-b", "name": "GPT B", "release_date": "2024-01-01"},
            },
        }),
    ];
    app.state.sync.agent = vec![
        serde_json::json!({"name": "build", "mode": "primary"}),
        serde_json::json!({"name": "plan", "mode": "primary"}),
    ];
}

#[test]
fn a_list_opens_on_its_current_option() {
    let mut app = busy_app();
    with_models(&mut app);
    let sync = std::mem::take(&mut app.state.sync);
    let _ = app.state.local.agent_set("plan", &sync);
    let _ = app.state.local.model_set(
        &sync,
        crate::state::local::ModelRef {
            provider_id: "openai".into(),
            model_id: "gpt-b".into(),
        },
        false,
    );
    app.state.sync = sync;

    let _ = open(&mut app, PendingDialog::Model);
    assert_eq!(selected_title(&app).as_deref(), Some("GPT B"));
    let _ = open(&mut app, PendingDialog::Agent);
    assert_eq!(selected_title(&app).as_deref(), Some("plan"));
    let _ = open(&mut app, PendingDialog::SessionList);
    assert_eq!(
        selected_title(&app).as_deref(),
        Some("Apply pending migrations")
    );
    let active = app.ui.theme.active.clone();
    let _ = open(&mut app, PendingDialog::ThemeList);
    assert_eq!(selected_title(&app), Some(active));
}

#[test]
fn the_model_list_keeps_each_provider_in_one_block() {
    // `dialog-model.tsx:63-105`: providers by name, each sorted on its
    // own (release date desc) — never interleaved under repeated headers.
    let mut app = busy_app();
    with_models(&mut app);
    let _ = open(&mut app, PendingDialog::Model);
    let titles: Vec<(String, String)> = filtered_options(&app)
        .into_iter()
        .map(|option| (option.category.unwrap_or_default(), option.title))
        .collect();
    assert_eq!(
        titles,
        [
            ("Anthropic", "Claude B"),
            ("Anthropic", "Claude A"),
            ("OpenAI", "GPT A"),
            ("OpenAI", "GPT B"),
        ]
        .map(|(category, title)| (category.to_string(), title.to_string()))
    );
    // Filtering flattens the list; the provider moves to the footer.
    for char in "gpt".chars() {
        press(&mut app, KeyCode::Char(char), KeyModifiers::NONE);
    }
    let options = filtered_options(&app);
    assert!(options.iter().all(|option| option.category.is_none()));
    assert_eq!(options[0].footer.as_deref(), Some("OpenAI"));
}

#[test]
fn the_filter_resets_the_selection_onto_a_real_row() {
    let mut app = busy_app();
    let _ = open(&mut app, PendingDialog::CommandPalette);
    for _ in 0..15 {
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    assert_eq!(app.ui.dialogs.top().unwrap().select.selected, 15);
    // Typing selects the first match — Enter acts on it.
    for char in "new se".chars() {
        press(&mut app, KeyCode::Char(char), KeyModifiers::NONE);
    }
    assert_eq!(app.ui.dialogs.top().unwrap().select.selected, 0);
    assert_eq!(selected_title(&app).as_deref(), Some("New session"));

    // No match: an empty list, "No results found", Enter does nothing.
    for char in "zzzz".chars() {
        press(&mut app, KeyCode::Char(char), KeyModifiers::NONE);
    }
    assert!(filtered_options(&app).is_empty());
    let text = render(&mut app, 100, 30).join("\n");
    assert!(text.contains("No results found"), "{text}");
    for code in [KeyCode::Down, KeyCode::End, KeyCode::PageDown] {
        press(&mut app, code, KeyModifiers::NONE);
    }
    let effects = press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(effects.is_empty());
    assert!(!app.ui.dialogs.is_empty());

    // Clearing the filter returns to the current option.
    for _ in 0.."new sezzzz".len() {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    assert_eq!(app.ui.dialogs.top().unwrap().select.selected, 0);
}

#[test]
fn a_shrinking_list_keeps_the_selection_valid() {
    let mut app = busy_app();
    let _ = open(&mut app, PendingDialog::SessionList);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(selected_title(&app).as_deref(), Some("Fix the build"));
    // The selected session disappears under the open list.
    app.state
        .sync
        .session
        .retain(|session| session.id == "ses_1");
    assert_eq!(
        selected_title(&app).as_deref(),
        Some("Apply pending migrations")
    );
    render(&mut app, 100, 30);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.ui.dialogs.is_empty());
}

#[test]
fn categories_group_and_the_window_follows_the_selection() {
    let mut app = busy_app();
    with_models(&mut app);
    let _ = open(&mut app, PendingDialog::CommandPalette);
    // Every category header shows once, however the options interleave.
    let options = filtered_options(&app);
    let mut seen: Vec<String> = Vec::new();
    for option in &options {
        let category = option.category.clone().unwrap_or_default();
        if seen.last() != Some(&category) {
            assert!(!seen.contains(&category), "{category} split: {seen:?}");
            seen.push(category);
        }
    }
    // Walk the whole list: the selection is always on screen.
    let count = options.len();
    for step in 0..count + 2 {
        let (lines, _) = panel(&mut app, 100, 24);
        let title = selected_title(&app).unwrap();
        assert!(
            lines.iter().any(|line| line.contains(&title)),
            "step {step}: {title} off screen:\n{}",
            lines.join("\n")
        );
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    let last = options.last().unwrap().title.clone();
    assert!(panel(&mut app, 100, 24)
        .0
        .iter()
        .any(|line| line.contains(&last)));
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    let (lines, _) = panel(&mut app, 100, 24);
    assert!(lines.iter().any(|line| line.contains("Suggested")));
}

// --------------------------------------------------------------- layout

/// Resize (the real `Msg::Resize` path) and draw the whole view.
fn draw(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    update(app, Msg::Resize(width, height));
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| crate::ui::view(app, frame)).unwrap();
    terminal.backend().buffer().clone()
}

fn rows_of(buffer: &ratatui::buffer::Buffer, area: Rect) -> Vec<String> {
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let buffer = draw(app, width, height);
    rows_of(&buffer, buffer.area)
}

/// Draw, then return the panel's own rows and its rectangle. Every cell
/// painted with the panel background lies inside the frame — the dialog
/// draws nothing outside it.
fn panel(app: &mut App, width: u16, height: u16) -> (Vec<String>, Rect) {
    let buffer = draw(app, width, height);
    let theme = app.ui.theme.resolve(&app.state.kv).unwrap();
    let panel_bg = theme.background_panel.to_color();
    let rect = placed(app).expect("dialog placed").rect;
    for y in 0..height {
        for x in 0..width {
            if buffer[(x, y)].bg == panel_bg {
                assert!(
                    rect.contains(ratatui::layout::Position { x, y }),
                    "panel cell ({x}, {y}) outside the frame {rect:?}"
                );
            }
        }
    }
    (rows_of(&buffer, rect), rect)
}

const SIZES: [(u16, u16); 6] = [(80, 24), (60, 20), (40, 12), (200, 50), (30, 8), (12, 4)];

#[test]
fn every_dialog_fits_any_terminal() {
    let long = "An exceptionally long session title that cannot possibly fit inside a medium dialog frame at all";
    for kind in families() {
        for (width, height) in SIZES {
            let mut app = busy_app();
            app.state.sync.session[0].title = long.into();
            let _ = open(&mut app, kind.clone());
            let (lines, rect) = panel(&mut app, width, height);
            assert!(
                rect.right() <= width && rect.bottom() <= height,
                "{kind:?} {rect:?}"
            );
            if width < 30 || height < 8 {
                continue;
            }
            // The header keeps its close hint.
            assert!(
                lines[1].trim_end().ends_with("esc") || lines[1].trim_end().ends_with("esc/enter"),
                "{kind:?} at {width}x{height}: no esc hint\n{}",
                lines.join("\n")
            );
        }
    }
}

#[test]
fn the_text_behind_never_shows_through_the_panel() {
    let mut app = busy_app();
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
            text: "#".repeat(400),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        }],
    );
    app.ui.session_scroll.sticky = true;
    for kind in families() {
        let _ = open(&mut app, kind.clone());
        let (lines, _) = panel(&mut app, 80, 24);
        for line in &lines {
            assert!(!line.contains('#'), "{kind:?}: bleed-through: {line}");
        }
        clear(&mut app);
    }
}

#[test]
fn the_footer_stays_visible_in_a_small_terminal() {
    for (width, height) in [(80, 24), (60, 20), (40, 12)] {
        let mut app = busy_app();
        let _ = open(&mut app, PendingDialog::SessionList);
        let text = render(&mut app, width, height).join("\n");
        assert!(text.contains("pin/unpin"), "{width}x{height}:\n{text}");

        let _ = open(
            &mut app,
            PendingDialog::ShareConsent {
                session_id: "ses_1".into(),
            },
        );
        let text = render(&mut app, width, height).join("\n");
        assert!(text.contains("Confirm"), "{width}x{height}:\n{text}");

        let _ = open(
            &mut app,
            PendingDialog::ProviderApiKey {
                provider_id: "libertai".into(),
            },
        );
        let text = render(&mut app, width, height).join("\n");
        assert!(text.contains("submit"), "{width}x{height}:\n{text}");
    }
}

#[test]
fn a_dialog_taller_than_the_terminal_keeps_its_input() {
    // The sign-in dialog's URL, instructions and input outgrow 40x12: the
    // header, the input and the submit hint stay.
    let mut app = busy_app();
    let _ = open(
        &mut app,
        PendingDialog::ProviderOauth {
            provider_id: "libertai".into(),
            method: 0,
            flow: 1,
            title: "Sign in with LibertAI".into(),
            url: format!("https://console.libertai.io/cli?state={}", "x".repeat(150)),
            instructions: "Finish signing in in the browser tab that opened. If your browser runs on another machine, paste the address it lands on.".into(),
            auto: true,
            rejected: false,
        },
    );
    let (lines, _) = panel(&mut app, 40, 12);
    let text = lines.join("\n");
    assert!(text.contains("Sign in with"), "{text}");
    assert!(text.contains("Paste the address"), "{text}");
    assert!(text.contains("ctrl+y"), "{text}");
}

#[test]
fn long_titles_are_cut_with_an_ellipsis() {
    let mut app = busy_app();
    app.state.sync.session[0].title = format!("{} END", "word ".repeat(30));
    let _ = open(&mut app, PendingDialog::SessionList);
    let (lines, _) = panel(&mut app, 80, 24);
    let row = lines
        .iter()
        .find(|line| line.contains("word word"))
        .expect("long title rendered");
    assert!(row.contains('…'), "{row}");
    assert!(!row.contains("END"), "{row}");
}

#[test]
fn a_long_api_key_wraps_inside_the_prompt() {
    let mut app = busy_app();
    let _ = open(
        &mut app,
        PendingDialog::ProviderApiKey {
            provider_id: "libertai".into(),
        },
    );
    let key = format!("LTAI-{}-TAIL", "0123456789".repeat(9));
    update(&mut app, Msg::Paste(key.clone()));
    let text = render(&mut app, 80, 24).join("\n");
    assert!(text.contains("LTAI-0123"), "{text}");
    assert!(text.contains("-TAIL"), "{text}");
}

#[test]
fn resizing_keeps_the_selection_in_view() {
    let mut app = busy_app();
    let _ = open(&mut app, PendingDialog::CommandPalette);
    render(&mut app, 200, 50);
    for _ in 0..20 {
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    let title = selected_title(&app).unwrap();
    for (width, height) in [(40, 12), (60, 20), (200, 50), (40, 12)] {
        let (lines, _) = panel(&mut app, width, height);
        assert!(
            lines.iter().any(|line| line.contains(&title)),
            "{width}x{height}: {title} off screen:\n{}",
            lines.join("\n")
        );
    }
    // The first option under its category header: a one-row window shows
    // the option, not the header.
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    let title = selected_title(&app).unwrap();
    for (width, height) in [(80, 24), (40, 12)] {
        let (lines, _) = panel(&mut app, width, height);
        assert!(
            lines.iter().any(|line| line.contains(&title)),
            "{width}x{height}: {title} off screen:\n{}",
            lines.join("\n")
        );
    }
}
