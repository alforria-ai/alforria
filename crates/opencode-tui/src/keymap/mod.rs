//! `keymap.tsx` — the mode stack (`keymap.tsx:53-100`), the timed
//! `ctrl+x` leader (`registerTimedLeader`), pending-sequence state and
//! dispatch. Scopes port the `useBindings` gather sites: `gather`
//! collects exactly the listed commands (`@opentui/keymap/extras`
//! `gatherBindings`), so each gather site becomes a scope table.

pub mod bindings;

use std::collections::BTreeMap;

use bindings::{command_of, event_to_stroke, parse_binding_value, BindingValue, KeyStroke};
use crossterm::event::KeyEvent;

pub use bindings::LEADER_DEFAULT;
pub use bindings::LEADER_TIMEOUT_DEFAULT;

use crate::config::TuiConfig;

/// `OPENCODE_BASE_MODE` (`keymap.tsx:21`).
pub const BASE_MODE: &str = "base";
/// `COMMAND_PALETTE_COMMAND` (`keymap.tsx:22`).
pub const COMMAND_PALETTE_COMMAND: &str = "command.palette.show";
/// The mode pushed by the dialog stack (`ui/dialog.tsx` — M8.7).
pub const MODAL_MODE: &str = "modal";

/// The gather-site scopes, in dispatch priority order (highest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Pushed-mode bindings (`dialog.*`, `diff.*`, which-key, …).
    Modal,
    /// The prompt's managed-textarea layer (`keymap.tsx:136-173`).
    Input,
    /// `gather("session.global.unfocused", …)` (`session/index.tsx:1105`).
    SessionUnfocused,
    /// `gather("session.global", …)` (`session/index.tsx:1101`) —
    /// shares a layer with the prompt bindings.
    SessionGlobal,
    /// `gather("prompt.palette", …)` (`prompt/index.tsx:566`).
    Prompt,
    /// `gather("session", …)` (`session/index.tsx:1110`).
    Session,
    /// `gather("app.global", …)` (`app.tsx:974`).
    AppGlobal,
    /// `gather("app", …)` (`app.tsx:970`).
    App,
}

impl Scope {
    fn table(self) -> &'static [&'static str] {
        match self {
            Scope::Modal => MODAL_KEYBINDS,
            Scope::Input => INPUT_KEYBINDS,
            Scope::SessionUnfocused => SESSION_UNFOCUSED_KEYBINDS,
            Scope::SessionGlobal => SESSION_GLOBAL_KEYBINDS,
            Scope::Prompt => PROMPT_KEYBINDS,
            Scope::Session => SESSION_KEYBINDS,
            Scope::AppGlobal => APP_GLOBAL_KEYBINDS,
            Scope::App => APP_KEYBINDS,
        }
    }
}

/// `appBindingCommands` (`app.tsx:106-142`).
static APP_KEYBINDS: &[&str] = &[
    "command_list",
    "model_list",
    "model_cycle_recent",
    "model_cycle_recent_reverse",
    "model_cycle_favorite",
    "model_cycle_favorite_reverse",
    "agent_list",
    "mcp_list",
    "agent_cycle",
    "agent_cycle_reverse",
    "variant_cycle",
    "variant_list",
    "provider_connect",
    "console_org_switch",
    "status_view",
    "debug_view",
    "theme_list",
    "theme_switch_mode",
    "theme_mode_lock",
    "help_show",
    "docs_open",
    "diff_open",
    "workspace_list",
    "app_debug",
    "app_console",
    "app_heap_snapshot",
    "terminal_suspend",
    "terminal_title_toggle",
    "app_toggle_animations",
    "app_toggle_file_context",
    "app_toggle_diffwrap",
    "app_toggle_paste_summary",
    "app_toggle_session_directory_filter",
    "app_exit",
    // `tips.toggle` is registered by the home tips plugin
    // (`feature-plugins/home/tips.tsx:22-33`) — base mode.
    "tips_toggle",
];

/// `appGlobalBindingCommands` (`app.tsx:92-104`).
static APP_GLOBAL_KEYBINDS: &[&str] = &[
    "session_list",
    "session_new",
    "session_quick_switch_1",
    "session_quick_switch_2",
    "session_quick_switch_3",
    "session_quick_switch_4",
    "session_quick_switch_5",
    "session_quick_switch_6",
    "session_quick_switch_7",
    "session_quick_switch_8",
    "session_quick_switch_9",
];

/// `sessionBindingCommands` (`session/index.tsx:115-143`).
static SESSION_KEYBINDS: &[&str] = &[
    "session_share",
    "session_rename",
    "session_timeline",
    "session_fork",
    "session_compact",
    "session_unshare",
    "messages_undo",
    "messages_redo",
    "sidebar_toggle",
    "session_toggle_timestamps",
    "session_toggle_generic_tool_output",
    "display_thinking",
    "tool_details",
    "scrollbar_toggle",
    "messages_first",
    "messages_last",
    "messages_last_user",
    "messages_next",
    "messages_previous",
    "messages_copy",
    "session_copy",
    "session_export",
    "session_background",
    "session_child_first",
    "session_parent",
    "session_child_cycle",
    "session_child_cycle_reverse",
];

/// `sessionGlobalBindingCommands` (`session/index.tsx:145-153`).
static SESSION_GLOBAL_KEYBINDS: &[&str] = &[
    "messages_page_up",
    "messages_page_down",
    "messages_line_up",
    "messages_line_down",
    "messages_half_page_up",
    "messages_half_page_down",
];

/// `sessionGlobalUnfocusedBindingCommands` (`session/index.tsx:154`).
static SESSION_UNFOCUSED_KEYBINDS: &[&str] = &["messages_first", "messages_last"];

/// `gather("prompt.palette", …)` (`prompt/index.tsx:568-579`).
static PROMPT_KEYBINDS: &[&str] = &[
    "prompt_submit",
    "editor_open",
    "prompt_editor_context_clear",
    "prompt_stash",
    "prompt_stash_pop",
    "prompt_stash_list",
    "prompt_skills",
    "session_interrupt",
    "workspace_set",
    "session_move",
];

/// `inputCommands` (`keymap.tsx:136-173`) plus `input.clear`
/// (`prompt/index.tsx:808-812`).
static INPUT_KEYBINDS: &[&str] = &[
    "input_clear",
    "input_move_left",
    "input_move_right",
    "input_move_up",
    "input_move_down",
    "input_select_left",
    "input_select_right",
    "input_select_up",
    "input_select_down",
    "input_line_home",
    "input_line_end",
    "input_select_line_home",
    "input_select_line_end",
    "input_visual_line_home",
    "input_visual_line_end",
    "input_select_visual_line_home",
    "input_select_visual_line_end",
    "input_buffer_home",
    "input_buffer_end",
    "input_select_buffer_home",
    "input_select_buffer_end",
    "input_delete_line",
    "input_delete_to_line_end",
    "input_delete_to_line_start",
    "input_backspace",
    "input_delete",
    "input_newline",
    "input_undo",
    "input_redo",
    "input_word_forward",
    "input_word_backward",
    "input_select_word_forward",
    "input_select_word_backward",
    "input_delete_word_forward",
    "input_delete_word_backward",
    "input_select_all",
    "input_submit",
];

/// Bindings that only live while a pushed mode owns the keyboard: the
/// dialog primitives, the diff viewer, which-key, the permission
/// fullscreen toggle and the inline-only handlers registrations
/// (`tips.toggle` renders on home, kept global here). The which-key
/// panel is a non-goal (§6 N7) and the diff-viewer route a non-goal
// (§6 N8), but their bindings stay registered.
static MODAL_KEYBINDS: &[&str] = &[
    "stash_delete",
    "model_provider_list",
    "model_favorite_toggle",
    "session_pin_toggle",
    "session_delete",
    "diff_close",
    "diff_toggle",
    "diff_expand",
    "diff_expand_all",
    "diff_collapse",
    "diff_switch_focus",
    "diff_next_hunk",
    "diff_previous_hunk",
    "diff_next_file",
    "diff_previous_file",
    "diff_toggle_file_tree",
    "diff_single_patch",
    "diff_switch_source",
    "diff_toggle_view",
    "diff_help",
    "which_key_toggle",
    "which_key_layout_toggle",
    "which_key_pending_toggle",
    "which_key_group_previous",
    "which_key_group_next",
    "which_key_scroll_up",
    "which_key_scroll_down",
    "which_key_page_up",
    "which_key_page_down",
    "which_key_home",
    "which_key_end",
];

/// `createOpencodeModeStack` (`keymap.tsx:53-100`): a base mode with
/// pushed modes on top; pushes return a token that pops that entry.
#[derive(Debug, Default, Clone)]
pub struct ModeStack {
    entries: Vec<(u64, String)>,
    next: u64,
}

impl ModeStack {
    pub fn current(&self) -> &str {
        self.entries
            .last()
            .map(|(_, mode)| mode.as_str())
            .unwrap_or(BASE_MODE)
    }

    pub fn push(&mut self, mode: &str) -> u64 {
        let token = self.next;
        self.next += 1;
        self.entries.push((token, mode.to_string()));
        token
    }

    /// The `off` function a TS push returns: removes exactly that entry.
    pub fn pop(&mut self, token: u64) {
        if let Some(index) = self.entries.iter().position(|(id, _)| *id == token) {
            self.entries.remove(index);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// What [`Keymap::dispatch`] needs to know about the app beyond the
/// keymap itself.
#[derive(Debug, Clone)]
pub struct DispatchContext {
    pub route_is_session: bool,
    pub prompt_focused: bool,
    pub foreground_tasks: bool,
}

/// The resolved keymap: definitions + overrides after
/// `config/index.tsx:95-111` `resolve()`.
#[derive(Debug, Clone)]
pub struct Keymap {
    pub modes: ModeStack,
    pub bindings: BTreeMap<String, BindingValue>,
    pub leader: KeyStroke,
    pub leader_timeout_ms: u64,
    pending: Vec<KeyStroke>,
    pending_since: Option<u64>,
}

impl Keymap {
    /// `resolve()` (`config/index.tsx:95-127`): overrides, the
    /// terminal-suspend rewrite, the leader and its timeout.
    pub fn resolve(config: &TuiConfig) -> Keymap {
        let mut values: BTreeMap<String, BindingValue> = BTreeMap::new();
        let leader_binding = config
            .keybinds
            .get("leader")
            .cloned()
            .unwrap_or_else(|| LEADER_DEFAULT.to_string());
        let leader = match parse_binding_value(&leader_binding, LEADER_DEFAULT) {
            BindingValue::Alternatives(mut alternatives) => alternatives
                .pop()
                .and_then(|mut sequence| sequence.pop())
                .unwrap_or_else(parse_chord_or_default),
            _ => parse_chord_or_default(),
        };
        for definition in bindings::DEFINITIONS {
            let value = config
                .keybinds
                .get(definition.name)
                .cloned()
                .unwrap_or_else(|| definition.default.to_string());
            values.insert(
                definition.name.to_string(),
                parse_binding_value(&value, &leader_binding),
            );
        }
        // `resolve()` (`config/index.tsx:101-111`): without suspend
        // support, `ctrl+z` joins `input_undo` and the suspend binding
        // is `none`.
        if !config.terminal_suspend_supported {
            values.insert(
                "terminal_suspend".to_string(),
                BindingValue::Disabled { listed: true },
            );
            if !config.keybinds.contains_key("input_undo") {
                let input_undo = bindings::DEFINITIONS
                    .iter()
                    .find(|d| d.name == "input_undo")
                    .map(|d| d.default)
                    .unwrap_or_default();
                let mut alternatives: Vec<String> = vec!["ctrl+z".to_string()];
                for alternative in input_undo.split(',') {
                    if !alternatives.iter().any(|item| item == alternative) {
                        alternatives.push(alternative.to_string());
                    }
                }
                values.insert(
                    "input_undo".to_string(),
                    parse_binding_value(&alternatives.join(","), &leader_binding),
                );
            }
        }
        Keymap {
            modes: ModeStack::default(),
            bindings: values,
            leader,
            leader_timeout_ms: config.leader_timeout_ms.unwrap_or(LEADER_TIMEOUT_DEFAULT),
            pending: Vec::new(),
            pending_since: None,
        }
    }

    /// `useLeaderActive()` (`keymap.tsx:246-248`).
    pub fn leader_active(&self) -> bool {
        !self.pending.is_empty() && self.pending.first() == Some(&self.leader)
    }

    pub fn pending_sequence(&self) -> &[KeyStroke] {
        &self.pending
    }

    /// The timed-leader `setTimeout` (`registerTimedLeader`) — clears
    /// the pending sequence once the timeout elapses.
    pub fn poll(&mut self, now_ms: u64) {
        if let Some(since) = self.pending_since {
            if now_ms.saturating_sub(since) >= self.leader_timeout_ms {
                self.clear_pending();
            }
        }
    }

    /// Whether `key` triggers the (single-stroke) binding `keybind`.
    pub fn matches(&self, keybind: &str, key: &KeyEvent) -> bool {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return false;
        }
        let Some(BindingValue::Alternatives(alternatives)) = self.bindings.get(keybind) else {
            return false;
        };
        let Some(stroke) = event_to_stroke(key) else {
            return false;
        };
        alternatives
            .iter()
            .any(|sequence| sequence.len() == 1 && sequence[0] == stroke)
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_since = None;
    }

    fn scope_active(&self, ctx: &DispatchContext, scope: Scope) -> bool {
        let mode = self.modes.current();
        match scope {
            Scope::Modal => mode != BASE_MODE,
            Scope::Input => ctx.prompt_focused,
            Scope::SessionUnfocused => ctx.route_is_session && !ctx.prompt_focused,
            Scope::SessionGlobal => ctx.route_is_session,
            Scope::Session => mode == BASE_MODE && ctx.route_is_session,
            Scope::Prompt => mode == BASE_MODE,
            Scope::AppGlobal => true,
            Scope::App => mode == BASE_MODE,
        }
    }

    /// Run one key event through the active bindings. Returns the
    /// matching commands in scope-priority order (the runner executes
    /// the first whose command is enabled) and tracks the pending
    /// leader sequence with its 2000 ms timeout.
    pub fn dispatch(
        &mut self,
        ctx: &DispatchContext,
        key: &KeyEvent,
        now_ms: u64,
    ) -> Vec<&'static str> {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return Vec::new();
        }
        self.poll(now_ms);
        // `registerEscapeClearsPendingSequence` / `registerBackspacePopsPendingSequence`.
        if !self.pending.is_empty() {
            match key.code {
                crossterm::event::KeyCode::Esc => {
                    self.clear_pending();
                    return Vec::new();
                }
                crossterm::event::KeyCode::Backspace => {
                    self.pending.pop();
                    if self.pending.is_empty() {
                        self.pending_since = None;
                    }
                    return Vec::new();
                }
                _ => {}
            }
        }
        let Some(stroke) = event_to_stroke(key) else {
            self.clear_pending();
            return Vec::new();
        };
        let mut commands: Vec<(Scope, &'static str)> = Vec::new();
        let mut extends = false;
        for scope in [
            Scope::Modal,
            Scope::Input,
            Scope::SessionUnfocused,
            Scope::SessionGlobal,
            Scope::Prompt,
            Scope::Session,
            Scope::AppGlobal,
            Scope::App,
        ] {
            if !self.scope_active(ctx, scope) {
                continue;
            }
            for keybind in scope.table() {
                let Some(value) = self.bindings.get(*keybind) else {
                    continue;
                };
                let BindingValue::Alternatives(alternatives) = value else {
                    continue;
                };
                for sequence in alternatives {
                    let Some(command) = command_of(keybind) else {
                        continue;
                    };
                    // The pending sequence must prefix-match, and the
                    // new stroke must extend it.
                    if sequence.len() <= self.pending.len()
                        || sequence[..self.pending.len()] != self.pending[..]
                        || sequence[self.pending.len()] != stroke
                    {
                        continue;
                    }
                    if sequence.len() == self.pending.len() + 1 {
                        commands.push((scope, command));
                    } else {
                        extends = true;
                    }
                }
            }
        }
        if !commands.is_empty() {
            self.clear_pending();
        } else if extends {
            self.pending.push(stroke);
            self.pending_since = Some(now_ms);
        } else {
            self.clear_pending();
        }
        commands.sort_by_key(|&(scope, _)| std::cmp::Reverse(scope_priority(scope)));
        commands.into_iter().map(|(_, command)| command).collect()
    }
}

fn scope_priority(scope: Scope) -> u8 {
    match scope {
        Scope::Modal => 7,
        Scope::Input => 6,
        Scope::SessionUnfocused => 5,
        Scope::SessionGlobal => 4,
        Scope::Prompt => 3,
        Scope::Session => 2,
        Scope::AppGlobal => 1,
        Scope::App => 0,
    }
}

fn parse_chord_or_default() -> KeyStroke {
    // Unreachable in practice — "ctrl+x" always parses.
    KeyStroke {
        key: "x".to_string(),
        ctrl: true,
        shift: false,
        meta: false,
        super_key: false,
        hyper: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn keymap() -> Keymap {
        Keymap::resolve(&TuiConfig::default())
    }

    fn ctx() -> DispatchContext {
        DispatchContext {
            route_is_session: true,
            prompt_focused: false,
            foreground_tasks: false,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn ctrl(char: char) -> KeyEvent {
        key(KeyCode::Char(char), KeyModifiers::CONTROL)
    }

    #[test]
    fn base_mode_dispatches_simple_bindings() {
        let mut keymap = keymap();
        let commands = keymap.dispatch(&ctx(), &ctrl('p'), 0);
        assert_eq!(commands, vec!["command.palette.show"]);
    }

    #[test]
    fn leader_sequence_completes_within_the_timeout() {
        let mut keymap = keymap();
        // ctrl+x arms the leader at t=0.
        keymap.dispatch(&ctx(), &ctrl('x'), 0);
        assert!(keymap.leader_active());
        // At t=1999 the sequence is still pending and completes.
        let commands = keymap.dispatch(&ctx(), &key(KeyCode::Char('q'), KeyModifiers::NONE), 1999);
        assert_eq!(commands, vec!["app.exit"]);
        assert!(!keymap.leader_active());
    }

    #[test]
    fn leader_sequence_expires_at_the_timeout() {
        let mut keymap = keymap();
        keymap.dispatch(&ctx(), &ctrl('x'), 0);
        // The next key arrives at t=2000 — the pending sequence died.
        let commands = keymap.dispatch(&ctx(), &key(KeyCode::Char('q'), KeyModifiers::NONE), 2000);
        assert!(commands.is_empty());
        assert!(!keymap.leader_active());
    }

    #[test]
    fn poll_expires_and_escape_and_backspace_clear() {
        let mut keymap = keymap();
        keymap.dispatch(&ctx(), &ctrl('x'), 0);
        keymap.poll(2000);
        assert!(keymap.pending_sequence().is_empty());

        keymap.dispatch(&ctx(), &ctrl('x'), 3000);
        assert!(keymap.leader_active());
        keymap.dispatch(&ctx(), &key(KeyCode::Esc, KeyModifiers::NONE), 3100);
        assert!(!keymap.leader_active());

        keymap.dispatch(&ctx(), &ctrl('x'), 4000);
        assert!(keymap.leader_active());
        keymap.dispatch(&ctx(), &key(KeyCode::Backspace, KeyModifiers::NONE), 4100);
        assert!(!keymap.leader_active());
    }

    #[test]
    fn ctrl_c_returns_both_clear_and_exit() {
        let mut keymap = keymap();
        let mut context = ctx();
        context.prompt_focused = true;
        context.route_is_session = false;
        let commands = keymap.dispatch(&context, &ctrl('c'), 0);
        assert_eq!(commands, vec!["prompt.clear", "app.exit"]);
    }

    #[test]
    fn session_scope_requires_the_session_route() {
        let mut keymap = keymap();
        let mut context = ctx();
        context.route_is_session = false;
        let commands = keymap.dispatch(&context, &ctrl('r'), 0);
        assert!(commands.is_empty());
        let commands = keymap.dispatch(&ctx(), &ctrl('r'), 0);
        assert_eq!(commands, vec!["session.rename"]);
    }

    #[test]
    fn input_layer_needs_prompt_focus() {
        let mut keymap = keymap();
        let mut context = ctx();
        context.prompt_focused = false;
        let commands = keymap.dispatch(&context, &ctrl('d'), 0);
        assert_eq!(commands, vec!["app.exit"]);

        let mut context = ctx();
        context.prompt_focused = true;
        let commands = keymap.dispatch(&context, &ctrl('d'), 0);
        assert_eq!(commands, vec!["input.delete", "app.exit"]);
    }

    #[test]
    fn pushed_modes_move_dispatch_to_the_modal_layer() {
        let mut keymap = keymap();
        let token = keymap.modes.push(MODAL_MODE);
        // ctrl+p is dialog.select.prev while modal — a dialog key, not a
        // registered command, so nothing dispatches.
        let commands = keymap.dispatch(&ctx(), &ctrl('p'), 0);
        assert!(commands.is_empty());
        keymap.modes.pop(token);
        let commands = keymap.dispatch(&ctx(), &ctrl('p'), 0);
        assert_eq!(commands, vec!["command.palette.show"]);
    }

    #[test]
    fn mode_stack_push_pop_semantics() {
        let mut modes = ModeStack::default();
        assert_eq!(modes.current(), BASE_MODE);
        let first = modes.push("modal");
        let second = modes.push("question");
        assert_eq!(modes.current(), "question");
        modes.pop(first);
        assert_eq!(modes.current(), "question", "popping a buried entry");
        modes.pop(second);
        assert_eq!(modes.current(), BASE_MODE);
    }

    #[test]
    fn leader_q_fires_app_exit_once() {
        let mut keymap = keymap();
        keymap.dispatch(&ctx(), &ctrl('x'), 0);
        let commands = keymap.dispatch(&ctx(), &key(KeyCode::Char('q'), KeyModifiers::NONE), 50);
        assert_eq!(commands, vec!["app.exit"]);
        // The sequence must not stay pending.
        assert!(!keymap.leader_active());
    }

    #[test]
    fn key_aliases_dispatch() {
        let mut keymap = keymap();
        let mut context = ctx();
        context.prompt_focused = true;
        // "return" and "enter" are the same key.
        let commands = keymap.dispatch(&context, &key(KeyCode::Enter, KeyModifiers::NONE), 0);
        assert_eq!(commands, vec!["input.submit"]);
        let commands = keymap.dispatch(&ctx(), &key(KeyCode::BackTab, KeyModifiers::NONE), 0);
        assert_eq!(commands, vec!["agent.cycle.reverse"]);
    }
}
