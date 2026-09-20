//! `command.rs` tests.

use crossterm::event::{KeyCode, KeyModifiers};

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

fn navigate_to_session(app: &mut App) -> String {
    app.state.route.navigate(Route::Session {
        session_id: "ses_1".into(),
        prompt: None,
    });
    app.state.sync.session = vec![session_info("ses_1", 1, None)];
    "ses_1".to_string()
}

fn session_info(id: &str, updated: i64, parent: Option<&str>) -> V1SessionInfo {
    V1SessionInfo {
        id: id.into(),
        slug: "x".into(),
        project_id: "prj".into(),
        workspace_id: None,
        directory: "/x".into(),
        path: None,
        parent_id: parent.map(str::to_string),
        summary: None,
        cost: None,
        tokens: None,
        share: None,
        title: "X".into(),
        agent: None,
        model: None,
        version: "1".into(),
        metadata: None,
        time: opencode_schema::session_v1::V1SessionTime {
            created: updated as u64,
            updated: updated as u64,
            compacting: None,
            archived: None,
        },
        permission: None,
        revert: None,
    }
}

#[test]
fn registry_names_match_the_ts_command_sets() {
    let app = new_app();
    let names: Vec<_> = registry(&app).into_iter().map(|c| c.name).collect();
    for expected in [
        "command.palette.show",
        "session.list",
        "session.new",
        "workspace.copy_path",
        "session.quick_switch.1",
        "session.quick_switch.9",
        "model.list",
        "model.cycle_recent",
        "model.cycle_recent_reverse",
        "model.cycle_favorite",
        "model.cycle_favorite_reverse",
        "agent.list",
        "mcp.list",
        "agent.cycle",
        "agent.cycle.reverse",
        "variant.cycle",
        "variant.list",
        "provider.connect",
        "opencode.status",
        "opencode.debug",
        "theme.switch",
        "theme.switch_mode",
        "theme.mode.lock",
        "help.show",
        "docs.open",
        "app.exit",
        "app.debug",
        "app.console",
        "app.heap_snapshot",
        "terminal.suspend",
        "terminal.title.toggle",
        "app.toggle.animations",
        "app.toggle.file_context",
        "app.toggle.diffwrap",
        "app.toggle.paste_summary",
        "app.toggle.session_directory_filter",
        "permission.mode",
        "tips.toggle",
        "prompt.clear",
        "prompt.submit",
        "prompt.editor_context.clear",
        "prompt.paste",
        "session.interrupt",
        "prompt.editor",
        "prompt.skills",
        "workspace.set",
        "session.move",
        "prompt.stash",
        "prompt.stash.pop",
        "prompt.stash.list",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    assert!(
        !names.contains(&"console.org.switch"),
        "console.org.switch needs switchableOrgCount > 1"
    );
    // No duplicate registrations.
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "duplicate command names");
}

#[test]
fn session_commands_only_register_on_the_session_route() {
    let app = new_app();
    assert!(registry(&app).iter().all(|c| c.name != "session.share"));
    let mut app = new_app();
    navigate_to_session(&mut app);
    let names: Vec<_> = registry(&app).into_iter().map(|c| c.name).collect();
    for expected in [
        "session.share",
        "session.rename",
        "session.timeline",
        "session.fork",
        "session.compact",
        "session.unshare",
        "session.undo",
        "session.redo",
        "session.sidebar.toggle",
        "session.toggle.conceal",
        "session.toggle.timestamps",
        "session.toggle.thinking",
        "session.toggle.actions",
        "session.toggle.scrollbar",
        "session.toggle.generic_tool_output",
        "session.page.up",
        "session.page.down",
        "session.line.up",
        "session.line.down",
        "session.half.page.up",
        "session.half.page.down",
        "session.first",
        "session.last",
        "session.messages_last_user",
        "session.message.next",
        "session.message.previous",
        "messages.copy",
        "session.copy",
        "session.export",
        "session.background",
        "session.child.first",
        "session.parent",
        "session.child.next",
        "session.child.previous",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    assert!(
        !names.contains(&"session.queued_prompts"),
        "session.queued_prompts is bound but never registered (TS divergence)"
    );
}

#[test]
fn slash_command_names_and_aliases() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    let slashes = slash_commands(&app);
    let get = |name: &str| {
        slashes
            .iter()
            .find(|command| command.name == name)
            .unwrap_or_else(|| panic!("missing {name}"))
    };
    assert_eq!(get("session.list").slash_name, Some("sessions"));
    assert_eq!(get("session.list").slash_aliases, &["resume", "continue"]);
    assert_eq!(get("session.new").slash_aliases, &["clear"]);
    assert_eq!(get("model.list").slash_name, Some("models"));
    assert_eq!(get("model.list").slash_aliases, &["mo"]);
    assert_eq!(get("agent.list").slash_name, Some("agents"));
    assert_eq!(get("mcp.list").slash_name, Some("mcps"));
    assert_eq!(get("provider.connect").slash_name, Some("connect"));
    assert_eq!(get("session.rename").slash_name, Some("rename"));
    assert_eq!(get("session.share").slash_name, Some("share"));
    assert_eq!(get("session.timeline").slash_name, Some("timeline"));
    assert_eq!(get("session.fork").slash_name, Some("fork"));
    assert_eq!(get("session.compact").slash_name, Some("compact"));
    assert_eq!(get("session.compact").slash_aliases, &["summarize"]);
    assert_eq!(get("session.unshare").slash_name, Some("unshare"));
    assert_eq!(get("session.undo").slash_name, Some("undo"));
    assert_eq!(get("session.redo").slash_name, Some("redo"));
    assert_eq!(get("session.copy").slash_name, Some("copy"));
    assert_eq!(get("session.export").slash_name, Some("export"));
    assert_eq!(get("prompt.editor").slash_name, Some("editor"));
    assert_eq!(get("prompt.skills").slash_name, Some("skills"));
    assert_eq!(get("workspace.set").slash_name, Some("warp"));
    assert_eq!(get("session.move").slash_name, Some("move"));
    assert_eq!(get("variant.list").slash_name, Some("variants"));
    assert_eq!(get("app.exit").slash_name, Some("exit"));
    assert_eq!(get("app.exit").slash_aliases, &["quit", "q"]);
    assert_eq!(get("help.show").slash_name, Some("help"));
    assert_eq!(get("opencode.status").slash_name, Some("status"));
    assert_eq!(get("opencode.debug").slash_name, Some("debug"));
    assert_eq!(get("theme.switch").slash_name, Some("themes"));
}

#[test]
fn palette_hides_hidden_and_the_palette_command() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    let palette = palette(&app);
    assert!(palette.iter().all(|c| !c.hidden));
    assert!(!palette.is_empty());
    assert!(palette
        .iter()
        .all(|c| c.name != crate::keymap::COMMAND_PALETTE_COMMAND));
    assert!(palette.iter().any(|c| c.name == "session.share"));
}

#[test]
fn suggested_flags() {
    let mut app = new_app();
    app.state.sync.session = vec![session_info("ses_a", 1, None)];
    let suggested: Vec<_> = registry(&app)
        .iter()
        .filter(|c| c.suggested)
        .map(|c| c.name)
        .collect();
    assert!(suggested.contains(&"session.list"));
    assert!(suggested.contains(&"model.list"));
    assert!(suggested.contains(&"provider.connect"));
}

#[test]
fn kv_toggle_commands_flip_persisted_state() {
    let mut app = new_app();
    run(&mut app, "app.toggle.animations");
    assert!(!app.state.kv.get_bool("animations_enabled", true));
    run(&mut app, "app.toggle.animations");
    assert!(app.state.kv.get_bool("animations_enabled", false));
    run(&mut app, "app.toggle.diffwrap");
    assert_eq!(
        app.state.kv.get("diff_wrap_mode", json!("x")),
        json!("none")
    );
    run(&mut app, "app.toggle.diffwrap");
    assert_eq!(
        app.state.kv.get("diff_wrap_mode", json!("x")),
        json!("word")
    );
}

#[test]
fn terminal_title_toggle_flips() {
    let mut app = new_app();
    run(&mut app, "terminal.title.toggle");
    assert!(!app.state.kv.get_bool("terminal_title_enabled", true));
    run(&mut app, "terminal.title.toggle");
    assert!(app.state.kv.get_bool("terminal_title_enabled", false));
}

#[test]
fn permission_mode_command_toggles() {
    let mut app = new_app();
    assert_eq!(
        app.state.permission_mode,
        crate::state::PermissionMode::Normal
    );
    run(&mut app, "permission.mode");
    assert_eq!(
        app.state.permission_mode,
        crate::state::PermissionMode::Auto
    );
    run(&mut app, "permission.mode");
    assert_eq!(
        app.state.permission_mode,
        crate::state::PermissionMode::Normal
    );
}

#[test]
fn session_directory_filter_refreshes() {
    let mut app = new_app();
    let effects = run(&mut app, "app.toggle.session_directory_filter");
    assert_eq!(effects, vec![Effect::SessionRefresh]);
}

#[test]
fn docs_open_fires_an_open_url_effect() {
    let mut app = new_app();
    let effects = run(&mut app, "docs.open");
    assert_eq!(
        effects,
        vec![Effect::OpenUrl {
            url: "https://opencode.ai/docs".into(),
        }]
    );
}

#[test]
fn exit_command_exits() {
    let mut app = new_app();
    run(&mut app, "app.exit");
    assert!(app.ui.exit);
}

#[test]
fn unknown_commands_no_op() {
    let mut app = new_app();
    assert!(run(&mut app, "nope.command").is_empty());
    assert!(run(&mut app, "session.queued_prompts").is_empty());
    // Route-gated command outside its route:
    assert!(run(&mut app, "session.rename").is_empty());
}

#[test]
fn share_needs_consent_then_shares() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    let effects = run(&mut app, "session.share");
    assert!(effects.is_empty());
    assert_eq!(
        app.ui.dialog,
        Some(PendingDialog::ShareConsent {
            session_id: "ses_1".into(),
        })
    );
    app.state.kv.set(keys::SHARE_CONSENT, json!(true));
    let effects = run(&mut app, "session.share");
    assert_eq!(
        effects,
        vec![Effect::SessionShare {
            session_id: "ses_1".into(),
        }]
    );
}

#[test]
fn share_with_url_copies() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    app.state.sync.session[0].share = Some(opencode_schema::session_v1::V1SessionShare {
        url: "https://example.com/s/abc".into(),
    });
    let effects = run(&mut app, "session.share");
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects[0], Effect::ClipboardWrite { .. }));
}

#[test]
fn compact_without_model_warns() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    let effects = run(&mut app, "session.compact");
    assert!(effects.is_empty());
    assert_eq!(
        app.ui.toasts[0].message,
        "Connect a provider to summarize this session"
    );

    app.state.sync.agent = vec![json!({ "name": "build" })];
    app.state.sync.provider = vec![json!({
        "id": "anthropic",
        "models": {"claude": {"id": "claude"}},
    })];
    app.state.local.model_set(
        &app.state.sync,
        crate::state::local::ModelRef {
            provider_id: "anthropic".into(),
            model_id: "claude".into(),
        },
        false,
    );
    let effects = run(&mut app, "session.compact");
    assert_eq!(
        effects,
        vec![Effect::SessionSummarize {
            session_id: "ses_1".into(),
            provider_id: "anthropic".into(),
            model_id: "claude".into(),
        }]
    );
}

#[test]
fn quick_switch_navigates() {
    let mut app = new_app();
    app.state.local.session_toggle_pin("ses_a");
    app.state.sync.session = vec![
        session_info("ses_a", 1, None),
        session_info("ses_b", 2, None),
    ];
    let effects = run(&mut app, "session.quick_switch.1");
    assert!(effects.is_empty());
    assert_eq!(
        app.state.route.data,
        Route::Session {
            session_id: "ses_a".into(),
            prompt: None,
        }
    );
}

#[test]
fn undo_reverts_to_last_user_message_and_repopulates_prompt() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    app.state.sync.message.insert(
        "ses_1".to_string(),
        vec![
            user_message("msg_1"),
            assistant_message("msg_2"),
            user_message("msg_3"),
        ],
    );
    app.state.sync.part.insert(
        "msg_3".to_string(),
        vec![
            text_part("txt_3", "revert me", false),
            text_part("txt_4", "[synthetic]", true),
        ],
    );
    let effects = run(&mut app, "session.undo");
    assert_eq!(
        effects,
        vec![
            // The session status is unset, i.e. not idle (session/index.tsx:618-620).
            Effect::SessionAbort {
                session_id: "ses_1".into(),
            },
            Effect::SessionRevert {
                session_id: "ses_1".into(),
                message_id: "msg_3".into(),
            },
        ]
    );
    assert_eq!(app.ui.prompt.input(), "revert me");
    assert_eq!(app.ui.dialog, None);
}

#[test]
fn undo_aborts_when_session_busy() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    app.state.sync.session_status.insert(
        "ses_1".into(),
        opencode_schema::session_status::SessionStatusInfo::Busy,
    );
    app.state
        .sync
        .message
        .insert("ses_1".to_string(), vec![user_message("msg_1")]);
    let effects = run(&mut app, "session.undo");
    assert_eq!(
        effects,
        vec![
            Effect::SessionAbort {
                session_id: "ses_1".into(),
            },
            Effect::SessionRevert {
                session_id: "ses_1".into(),
                message_id: "msg_1".into(),
            },
        ]
    );
}

#[test]
fn redo_unreverts_when_no_later_user_message() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    app.state.sync.session[0].revert = Some(revert("msg_1"));
    app.state
        .sync
        .message
        .insert("ses_1".to_string(), vec![user_message("msg_1")]);
    let effects = run(&mut app, "session.redo");
    assert_eq!(
        effects,
        vec![Effect::SessionUnrevert {
            session_id: "ses_1".into(),
        }]
    );
    assert_eq!(app.ui.prompt.input(), "");
}

#[test]
fn messages_copy_finds_last_assistant_message() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    app.state.sync.message.insert(
        "ses_1".to_string(),
        vec![user_message("msg_1"), assistant_message("msg_2")],
    );
    app.state.sync.part.insert(
        "msg_2".to_string(),
        vec![
            text_part("txt_1", "answer ", false),
            text_part("txt_2", "here", false),
        ],
    );
    let effects = run(&mut app, "messages.copy");
    assert_eq!(
        effects,
        vec![Effect::ClipboardWrite {
            text: "answer \nhere".into(),
            success: Some(clipboard_toast(
                "Message copied to clipboard!",
                ToastVariant::Success
            )),
            failure: Some(clipboard_toast(
                "Failed to copy to clipboard",
                ToastVariant::Error
            )),
        }]
    );
}

#[test]
fn messages_copy_without_assistant_message_toasts() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    app.state
        .sync
        .message
        .insert("ses_1".to_string(), vec![user_message("msg_1")]);
    let effects = run(&mut app, "messages.copy");
    assert!(effects.is_empty());
    assert_eq!(app.ui.toasts[0].message, "No assistant messages found");
}

#[test]
fn interrupt_requires_two_presses() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    app.state.sync.session_status.insert(
        "ses_1".into(),
        opencode_schema::session_status::SessionStatusInfo::Busy,
    );
    assert!(run(&mut app, "session.interrupt").is_empty());
    assert_eq!(
        run(&mut app, "session.interrupt"),
        vec![Effect::SessionAbort {
            session_id: "ses_1".into(),
        }]
    );
}

#[test]
fn stash_round_trip() {
    let mut app = new_app();
    app.ui.prompt.textarea.set_text("draft");
    assert!(is_enabled(&app, "prompt.stash"));
    run(&mut app, "prompt.stash");
    assert_eq!(app.ui.prompt.input(), "");
    assert!(is_enabled(&app, "prompt.stash.pop"));
    run(&mut app, "prompt.stash.pop");
    assert_eq!(app.ui.prompt.input(), "draft");
}

#[test]
fn sidebar_toggle_flips_kv_and_open_flag() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    run(&mut app, "session.sidebar.toggle");
    assert_eq!(app.state.kv.get("sidebar", json!("x")), json!("auto"));
    assert!(app.ui.sidebar_open);
    run(&mut app, "session.sidebar.toggle");
    assert_eq!(app.state.kv.get("sidebar", json!("x")), json!("hide"));
    assert!(!app.ui.sidebar_open);
}

#[test]
fn thinking_toggle_cycles_kv() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    run(&mut app, "session.toggle.thinking");
    assert_eq!(app.state.kv.get("thinking_mode", json!("x")), json!("show"));
    run(&mut app, "session.toggle.thinking");
    assert_eq!(app.state.kv.get("thinking_mode", json!("x")), json!("hide"));
}

#[test]
fn child_navigation_walks_children() {
    let mut app = new_app();
    app.state.route.navigate(Route::Session {
        session_id: "child_b".into(),
        prompt: None,
    });
    app.state.sync.session = vec![
        session_info("parent", 1, None),
        session_info("child_a", 2, Some("parent")),
        session_info("child_b", 3, Some("parent")),
    ];
    run(&mut app, "session.child.next");
    assert_eq!(
        app.state.route.data,
        Route::Session {
            session_id: "child_a".into(),
            prompt: None,
        }
    );
    run(&mut app, "session.parent");
    assert_eq!(
        app.state.route.data,
        Route::Session {
            session_id: "parent".into(),
            prompt: None,
        }
    );
}

#[test]
fn tui_command_execute_dispatches_the_command() {
    let mut app = new_app();
    let effects = crate::app::on_bus_event(
        &mut app,
        crate::transport::events::BusEvent {
            event: opencode_schema::event_manifest::Event::TuiCommandExecute(
                opencode_schema::tui_event::TuiCommandExecuteData {
                    command: "app.exit".into(),
                },
            ),
            metadata: Default::default(),
        },
    );
    assert!(effects.is_empty());
    assert!(app.ui.exit);
}

#[test]
fn key_dispatch_runs_the_first_enabled_command() {
    let mut app = new_app();
    app.ui.prompt_focused = true;
    app.ui.prompt.textarea.set_text("typing");
    press_ctrl(&mut app, 'c');
    assert!(!app.ui.exit, "ctrl+c clears the prompt instead");
    assert_eq!(app.ui.prompt.input(), "");

    press_ctrl(&mut app, 'c');
    assert!(app.ui.exit, "empty input: ctrl+c exits");
}

#[test]
fn key_dispatch_palette_and_leader_compact() {
    let mut app = new_app();
    press_ctrl(&mut app, 'p');
    assert_eq!(app.ui.dialog, Some(PendingDialog::CommandPalette));

    let mut app = new_app();
    navigate_to_session(&mut app);
    app.ui.prompt_focused = false;
    press_ctrl(&mut app, 'x');
    press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
    assert_eq!(
        app.ui.toasts[0].message, "Connect a provider to summarize this session",
        "<leader>c compacts"
    );
}

#[test]
fn input_layer_swallows_ctrl_d_when_focused() {
    let mut app = new_app();
    navigate_to_session(&mut app);
    app.ui.prompt_focused = true;
    press_ctrl(&mut app, 'd');
    assert_eq!(
        app.ui.dialog, None,
        "input.delete runs (no-op until M8.6); session.delete must not"
    );
}

fn user_message(id: &str) -> V1Message {
    V1Message::User {
        id: id.into(),
        session_id: "ses_1".into(),
        time: opencode_schema::session_v1::UserTime { created: 1.0 },
        format: None,
        summary: None,
        agent: "build".into(),
        model: opencode_schema::session_v1::V1UserModel {
            provider_id: "anthropic".into(),
            model_id: "claude".into(),
            variant: None,
        },
        system: None,
        tools: None,
    }
}

fn assistant_message(id: &str) -> V1Message {
    V1Message::Assistant {
        id: id.into(),
        session_id: "ses_1".into(),
        time: opencode_schema::session_v1::AssistantTime {
            created: 1,
            completed: Some(2),
        },
        error: None,
        parent_id: "msg_0".into(),
        provider_id: "anthropic".into(),
        model_id: "claude".into(),
        mode: "primary".into(),
        agent: "build".into(),
        path: opencode_schema::session_v1::V1Path {
            cwd: "/x".into(),
            root: "/x".into(),
        },
        summary: None,
        cost: 0.0,
        tokens: opencode_schema::session_v1::V1StepTokens {
            total: None,
            input: 0.0,
            output: 0.0,
            reasoning: 0.0,
            cache: opencode_schema::session_v1::V1TokenCache {
                read: 0.0,
                write: 0.0,
            },
        },
        structured: None,
        variant: None,
        finish: None,
    }
}

fn text_part(id: &str, text: &str, synthetic: bool) -> V1Part {
    V1Part::Text {
        id: id.into(),
        session_id: "ses_1".into(),
        message_id: "msg_1".into(),
        text: text.into(),
        synthetic: Some(synthetic),
        ignored: None,
        time: None,
        metadata: None,
    }
}

fn revert(message_id: &str) -> opencode_schema::session_v1::V1SessionRevert {
    opencode_schema::session_v1::V1SessionRevert {
        message_id: message_id.into(),
        part_id: None,
        snapshot: None,
        diff: None,
    }
}
