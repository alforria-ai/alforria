//! `config/keybind.ts` — every default binding, the `CommandMap` name
//! mapping, and the binding-string parser (`keybind.ts:1-471` ported
//! mechanically; the file is the law).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub const LEADER_DEFAULT: &str = "ctrl+x";
/// `LeaderTimeoutDefault` (`config/index.tsx:21`).
pub const LEADER_TIMEOUT_DEFAULT: u64 = 2000;

/// One definition row: `keybind(default, description)` (`keybind.ts:36-43`).
pub struct Definition {
    pub name: &'static str,
    /// Verbatim default — `"none"` and `"false"` included.
    pub default: &'static str,
    pub description: &'static str,
}

/// `Definitions` (`keybind.ts:45-240`) — all defaults, verbatim.
pub static DEFINITIONS: &[Definition] = &[
    (Definition {
        name: "leader",
        default: "ctrl+x",
        description: "Leader key for keybind combinations",
    }),
    (Definition {
        name: "app_exit",
        default: "ctrl+c,ctrl+d,<leader>q",
        description: "Exit the application",
    }),
    (Definition {
        name: "app_debug",
        default: "none",
        description: "Toggle debug panel",
    }),
    (Definition {
        name: "app_console",
        default: "none",
        description: "Toggle console",
    }),
    (Definition {
        name: "app_heap_snapshot",
        default: "none",
        description: "Write heap snapshot",
    }),
    (Definition {
        name: "app_toggle_animations",
        default: "none",
        description: "Toggle animations",
    }),
    (Definition {
        name: "app_toggle_file_context",
        default: "none",
        description: "Toggle file context",
    }),
    (Definition {
        name: "app_toggle_diffwrap",
        default: "none",
        description: "Toggle diff wrapping",
    }),
    (Definition {
        name: "app_toggle_paste_summary",
        default: "none",
        description: "Toggle paste summary",
    }),
    (Definition {
        name: "app_toggle_session_directory_filter",
        default: "none",
        description: "Toggle session directory filtering",
    }),
    (Definition {
        name: "command_list",
        default: "ctrl+p",
        description: "List available commands",
    }),
    (Definition {
        name: "help_show",
        default: "none",
        description: "Open help dialog",
    }),
    (Definition {
        name: "docs_open",
        default: "none",
        description: "Open documentation",
    }),
    (Definition {
        name: "diff_open",
        default: "none",
        description: "Open diff viewer",
    }),
    (Definition {
        name: "diff_close",
        default: "escape,q",
        description: "Close diff viewer",
    }),
    (Definition {
        name: "diff_toggle",
        default: "enter,space",
        description: "Toggle diff viewer item",
    }),
    (Definition {
        name: "diff_expand",
        default: "right",
        description: "Expand diff viewer item",
    }),
    (Definition {
        name: "diff_expand_all",
        default: "E",
        description: "Expand all diff viewer folders",
    }),
    (Definition {
        name: "diff_collapse",
        default: "left",
        description: "Collapse diff viewer item",
    }),
    (Definition {
        name: "diff_switch_focus",
        default: "tab",
        description: "Switch diff viewer focus",
    }),
    (Definition {
        name: "diff_next_hunk",
        default: "]",
        description: "Jump to next diff hunk",
    }),
    (Definition {
        name: "diff_previous_hunk",
        default: "[",
        description: "Jump to previous diff hunk",
    }),
    (Definition {
        name: "diff_next_file",
        default: "n",
        description: "Jump to next diff file",
    }),
    (Definition {
        name: "diff_previous_file",
        default: "p",
        description: "Jump to previous diff file",
    }),
    (Definition {
        name: "diff_toggle_file_tree",
        default: "b",
        description: "Toggle diff viewer file tree",
    }),
    (Definition {
        name: "diff_single_patch",
        default: "s",
        description: "Toggle single patch view",
    }),
    (Definition {
        name: "diff_switch_source",
        default: "d",
        description: "Switch diff viewer source",
    }),
    (Definition {
        name: "diff_toggle_view",
        default: "v",
        description: "Toggle diff viewer split or unified view",
    }),
    (Definition {
        name: "diff_help",
        default: "?",
        description: "Show more diff viewer shortcuts",
    }),
    (Definition {
        name: "editor_open",
        default: "<leader>e",
        description: "Open external editor",
    }),
    (Definition {
        name: "theme_list",
        default: "<leader>t",
        description: "List available themes",
    }),
    (Definition {
        name: "theme_switch_mode",
        default: "none",
        description: "Switch between light and dark theme mode",
    }),
    (Definition {
        name: "theme_mode_lock",
        default: "none",
        description: "Lock or unlock theme mode",
    }),
    (Definition {
        name: "sidebar_toggle",
        default: "<leader>b",
        description: "Toggle sidebar",
    }),
    (Definition {
        name: "scrollbar_toggle",
        default: "none",
        description: "Toggle session scrollbar",
    }),
    (Definition {
        name: "status_view",
        default: "<leader>s",
        description: "View status",
    }),
    (Definition {
        name: "debug_view",
        default: "none",
        description: "View debug info",
    }),
    (Definition {
        name: "session_export",
        default: "<leader>x",
        description: "Export session to editor",
    }),
    (Definition {
        name: "session_copy",
        default: "none",
        description: "Copy session transcript",
    }),
    (Definition {
        name: "session_move",
        default: "none",
        description: "Move session",
    }),
    (Definition {
        name: "session_new",
        default: "<leader>n",
        description: "Create a new session",
    }),
    (Definition {
        name: "session_list",
        default: "<leader>l",
        description: "List all sessions",
    }),
    (Definition {
        name: "session_timeline",
        default: "<leader>g",
        description: "Show session timeline",
    }),
    (Definition {
        name: "session_fork",
        default: "none",
        description: "Fork session from message",
    }),
    (Definition {
        name: "session_rename",
        default: "ctrl+r",
        description: "Rename session",
    }),
    (Definition {
        name: "session_delete",
        default: "ctrl+d",
        description: "Delete session",
    }),
    (Definition {
        name: "session_share",
        default: "none",
        description: "Share current session",
    }),
    (Definition {
        name: "session_unshare",
        default: "none",
        description: "Unshare current session",
    }),
    (Definition {
        name: "session_interrupt",
        default: "escape",
        description: "Interrupt current session",
    }),
    (Definition {
        name: "session_background",
        default: "ctrl+b",
        description: "Background synchronous subagents",
    }),
    (Definition {
        name: "session_compact",
        default: "<leader>c",
        description: "Compact the session",
    }),
    (Definition {
        name: "session_toggle_timestamps",
        default: "none",
        description: "Toggle message timestamps",
    }),
    (Definition {
        name: "session_toggle_generic_tool_output",
        default: "none",
        description: "Toggle generic tool output",
    }),
    (Definition {
        name: "session_queued_prompts",
        default: "<leader>q",
        description: "Manage queued prompts",
    }),
    (Definition {
        name: "session_child_first",
        default: "<leader>down",
        description: "Go to first child session",
    }),
    (Definition {
        name: "session_child_cycle",
        default: "right",
        description: "Go to next child session",
    }),
    (Definition {
        name: "session_child_cycle_reverse",
        default: "left",
        description: "Go to previous child session",
    }),
    (Definition {
        name: "session_parent",
        default: "up",
        description: "Go to parent session",
    }),
    (Definition {
        name: "session_pin_toggle",
        default: "ctrl+f",
        description: "Pin or unpin session in the session list",
    }),
    (Definition {
        name: "session_quick_switch_1",
        default: "<leader>1",
        description: "Switch to session in quick slot 1",
    }),
    (Definition {
        name: "session_quick_switch_2",
        default: "<leader>2",
        description: "Switch to session in quick slot 2",
    }),
    (Definition {
        name: "session_quick_switch_3",
        default: "<leader>3",
        description: "Switch to session in quick slot 3",
    }),
    (Definition {
        name: "session_quick_switch_4",
        default: "<leader>4",
        description: "Switch to session in quick slot 4",
    }),
    (Definition {
        name: "session_quick_switch_5",
        default: "<leader>5",
        description: "Switch to session in quick slot 5",
    }),
    (Definition {
        name: "session_quick_switch_6",
        default: "<leader>6",
        description: "Switch to session in quick slot 6",
    }),
    (Definition {
        name: "session_quick_switch_7",
        default: "<leader>7",
        description: "Switch to session in quick slot 7",
    }),
    (Definition {
        name: "session_quick_switch_8",
        default: "<leader>8",
        description: "Switch to session in quick slot 8",
    }),
    (Definition {
        name: "session_quick_switch_9",
        default: "<leader>9",
        description: "Switch to session in quick slot 9",
    }),
    (Definition {
        name: "stash_delete",
        default: "ctrl+d",
        description: "Delete stash entry",
    }),
    (Definition {
        name: "model_provider_list",
        default: "ctrl+a",
        description: "Open provider list from model dialog",
    }),
    (Definition {
        name: "model_favorite_toggle",
        default: "ctrl+f",
        description: "Toggle model favorite status",
    }),
    (Definition {
        name: "model_list",
        default: "<leader>m",
        description: "List available models",
    }),
    (Definition {
        name: "model_cycle_recent",
        default: "f2",
        description: "Next recently used model",
    }),
    (Definition {
        name: "model_cycle_recent_reverse",
        default: "shift+f2",
        description: "Previous recently used model",
    }),
    (Definition {
        name: "model_cycle_favorite",
        default: "none",
        description: "Next favorite model",
    }),
    (Definition {
        name: "model_cycle_favorite_reverse",
        default: "none",
        description: "Previous favorite model",
    }),
    (Definition {
        name: "mcp_list",
        default: "none",
        description: "List MCP servers",
    }),
    (Definition {
        name: "provider_connect",
        default: "none",
        description: "Connect provider",
    }),
    (Definition {
        name: "console_org_switch",
        default: "none",
        description: "Switch console organization",
    }),
    (Definition {
        name: "agent_list",
        default: "<leader>a",
        description: "List agents",
    }),
    (Definition {
        name: "agent_cycle",
        default: "tab",
        description: "Next agent",
    }),
    (Definition {
        name: "agent_cycle_reverse",
        default: "shift+tab",
        description: "Previous agent",
    }),
    (Definition {
        name: "variant_cycle",
        default: "ctrl+t",
        description: "Cycle model variants",
    }),
    (Definition {
        name: "variant_list",
        default: "none",
        description: "List model variants",
    }),
    (Definition {
        name: "messages_page_up",
        default: "pageup,ctrl+alt+b",
        description: "Scroll messages up by one page",
    }),
    (Definition {
        name: "messages_page_down",
        default: "pagedown,ctrl+alt+f",
        description: "Scroll messages down by one page",
    }),
    (Definition {
        name: "messages_line_up",
        default: "ctrl+alt+y",
        description: "Scroll messages up by one line",
    }),
    (Definition {
        name: "messages_line_down",
        default: "ctrl+alt+e",
        description: "Scroll messages down by one line",
    }),
    (Definition {
        name: "messages_half_page_up",
        default: "ctrl+alt+u",
        description: "Scroll messages up by half page",
    }),
    (Definition {
        name: "messages_half_page_down",
        default: "ctrl+alt+d",
        description: "Scroll messages down by half page",
    }),
    (Definition {
        name: "messages_first",
        default: "ctrl+g,home",
        description: "Navigate to first message",
    }),
    (Definition {
        name: "messages_last",
        default: "ctrl+alt+g,end",
        description: "Navigate to last message",
    }),
    (Definition {
        name: "messages_next",
        default: "none",
        description: "Navigate to next message",
    }),
    (Definition {
        name: "messages_previous",
        default: "none",
        description: "Navigate to previous message",
    }),
    (Definition {
        name: "messages_last_user",
        default: "none",
        description: "Navigate to last user message",
    }),
    (Definition {
        name: "messages_copy",
        default: "<leader>y",
        description: "Copy message",
    }),
    (Definition {
        name: "messages_undo",
        default: "<leader>u",
        description: "Undo message",
    }),
    (Definition {
        name: "messages_redo",
        default: "<leader>r",
        description: "Redo message",
    }),
    (Definition {
        name: "messages_toggle_conceal",
        default: "<leader>h",
        description: "Toggle code block concealment in messages",
    }),
    (Definition {
        name: "tool_details",
        default: "none",
        description: "Toggle tool details visibility",
    }),
    (Definition {
        name: "display_thinking",
        default: "none",
        description: "Toggle thinking blocks visibility",
    }),
    (Definition {
        name: "prompt_submit",
        default: "none",
        description: "Submit prompt",
    }),
    (Definition {
        name: "prompt_editor_context_clear",
        default: "none",
        description: "Clear editor context",
    }),
    (Definition {
        name: "prompt_skills",
        default: "none",
        description: "Open skill selector",
    }),
    (Definition {
        name: "prompt_stash",
        default: "none",
        description: "Stash prompt",
    }),
    (Definition {
        name: "prompt_stash_pop",
        default: "none",
        description: "Pop stashed prompt",
    }),
    (Definition {
        name: "prompt_stash_list",
        default: "none",
        description: "List stashed prompts",
    }),
    (Definition {
        name: "workspace_set",
        default: "none",
        description: "Set workspace",
    }),
    (Definition {
        name: "input_clear",
        default: "ctrl+c",
        description: "Clear input field",
    }),
    // `keybind({ key: "ctrl+v", preventDefault: false }, …)` (`keybind.ts:162`).
    (Definition {
        name: "input_paste",
        default: "ctrl+v",
        description: "Paste from clipboard",
    }),
    (Definition {
        name: "input_submit",
        default: "return",
        description: "Submit input",
    }),
    (Definition {
        name: "input_newline",
        default: "shift+return,ctrl+return,alt+return,ctrl+j",
        description: "Insert newline in input",
    }),
    (Definition {
        name: "input_move_left",
        default: "left,ctrl+b",
        description: "Move cursor left in input",
    }),
    (Definition {
        name: "input_move_right",
        default: "right,ctrl+f",
        description: "Move cursor right in input",
    }),
    (Definition {
        name: "input_move_up",
        default: "up",
        description: "Move cursor up in input",
    }),
    (Definition {
        name: "input_move_down",
        default: "down",
        description: "Move cursor down in input",
    }),
    (Definition {
        name: "input_select_left",
        default: "shift+left",
        description: "Select left in input",
    }),
    (Definition {
        name: "input_select_right",
        default: "shift+right",
        description: "Select right in input",
    }),
    (Definition {
        name: "input_select_up",
        default: "shift+up",
        description: "Select up in input",
    }),
    (Definition {
        name: "input_select_down",
        default: "shift+down",
        description: "Select down in input",
    }),
    (Definition {
        name: "input_line_home",
        default: "ctrl+a",
        description: "Move to start of line in input",
    }),
    (Definition {
        name: "input_line_end",
        default: "ctrl+e",
        description: "Move to end of line in input",
    }),
    (Definition {
        name: "input_select_line_home",
        default: "ctrl+shift+a",
        description: "Select to start of line in input",
    }),
    (Definition {
        name: "input_select_line_end",
        default: "ctrl+shift+e",
        description: "Select to end of line in input",
    }),
    (Definition {
        name: "input_visual_line_home",
        default: "alt+a",
        description: "Move to start of visual line in input",
    }),
    (Definition {
        name: "input_visual_line_end",
        default: "alt+e",
        description: "Move to end of visual line in input",
    }),
    (Definition {
        name: "input_select_visual_line_home",
        default: "alt+shift+a",
        description: "Select to start of visual line in input",
    }),
    (Definition {
        name: "input_select_visual_line_end",
        default: "alt+shift+e",
        description: "Select to end of visual line in input",
    }),
    (Definition {
        name: "input_buffer_home",
        default: "home",
        description: "Move to start of buffer in input",
    }),
    (Definition {
        name: "input_buffer_end",
        default: "end",
        description: "Move to end of buffer in input",
    }),
    (Definition {
        name: "input_select_buffer_home",
        default: "shift+home",
        description: "Select to start of buffer in input",
    }),
    (Definition {
        name: "input_select_buffer_end",
        default: "shift+end",
        description: "Select to end of buffer in input",
    }),
    (Definition {
        name: "input_delete_line",
        default: "ctrl+shift+d",
        description: "Delete line in input",
    }),
    (Definition {
        name: "input_delete_to_line_end",
        default: "ctrl+k",
        description: "Delete to end of line in input",
    }),
    (Definition {
        name: "input_delete_to_line_start",
        default: "ctrl+u",
        description: "Delete to start of line in input",
    }),
    (Definition {
        name: "input_backspace",
        default: "backspace,shift+backspace",
        description: "Backspace in input",
    }),
    (Definition {
        name: "input_delete",
        default: "ctrl+d,delete,shift+delete",
        description: "Delete character in input",
    }),
    (Definition {
        name: "input_undo",
        default: "ctrl+-,super+z",
        description: "Undo in input",
    }),
    (Definition {
        name: "input_redo",
        default: "ctrl+.,super+shift+z",
        description: "Redo in input",
    }),
    (Definition {
        name: "input_word_forward",
        default: "alt+f,alt+right,ctrl+right",
        description: "Move word forward in input",
    }),
    (Definition {
        name: "input_word_backward",
        default: "alt+b,alt+left,ctrl+left",
        description: "Move word backward in input",
    }),
    (Definition {
        name: "input_select_word_forward",
        default: "alt+shift+f,alt+shift+right",
        description: "Select word forward in input",
    }),
    (Definition {
        name: "input_select_word_backward",
        default: "alt+shift+b,alt+shift+left",
        description: "Select word backward in input",
    }),
    (Definition {
        name: "input_delete_word_forward",
        default: "alt+d,alt+delete,ctrl+delete",
        description: "Delete word forward in input",
    }),
    (Definition {
        name: "input_delete_word_backward",
        default: "ctrl+w,ctrl+backspace,alt+backspace",
        description: "Delete word backward in input",
    }),
    (Definition {
        name: "input_select_all",
        default: "super+a",
        description: "Select all in input",
    }),
    (Definition {
        name: "history_previous",
        default: "up",
        description: "Previous history item",
    }),
    (Definition {
        name: "history_next",
        default: "down",
        description: "Next history item",
    }),
    (Definition {
        name: "dialog.select.prev",
        default: "up,ctrl+p",
        description: "Move to previous dialog item",
    }),
    (Definition {
        name: "dialog.select.next",
        default: "down,ctrl+n",
        description: "Move to next dialog item",
    }),
    (Definition {
        name: "dialog.select.page_up",
        default: "pageup",
        description: "Move up one page in dialog",
    }),
    (Definition {
        name: "dialog.select.page_down",
        default: "pagedown",
        description: "Move down one page in dialog",
    }),
    (Definition {
        name: "dialog.select.home",
        default: "home",
        description: "Move to first dialog item",
    }),
    (Definition {
        name: "dialog.select.end",
        default: "end",
        description: "Move to last dialog item",
    }),
    (Definition {
        name: "dialog.select.submit",
        default: "return",
        description: "Submit selected dialog item",
    }),
    (Definition {
        name: "dialog.prompt.submit",
        default: "return",
        description: "Submit dialog prompt",
    }),
    (Definition {
        name: "dialog.mcp.toggle",
        default: "space",
        description: "Toggle MCP in MCP dialog",
    }),
    (Definition {
        name: "dialog.move_session.new",
        default: "ctrl+m",
        description: "New project copy",
    }),
    (Definition {
        name: "dialog.move_session.delete",
        default: "ctrl+d",
        description: "Delete project copy",
    }),
    (Definition {
        name: "dialog.move_session.refresh",
        default: "ctrl+r",
        description: "Refresh project copies",
    }),
    (Definition {
        name: "prompt.autocomplete.prev",
        default: "up,ctrl+p",
        description: "Move to previous autocomplete item",
    }),
    (Definition {
        name: "prompt.autocomplete.next",
        default: "down,ctrl+n",
        description: "Move to next autocomplete item",
    }),
    (Definition {
        name: "prompt.autocomplete.hide",
        default: "escape",
        description: "Hide autocomplete",
    }),
    (Definition {
        name: "prompt.autocomplete.select",
        default: "return",
        description: "Select autocomplete item",
    }),
    (Definition {
        name: "prompt.autocomplete.complete",
        default: "tab",
        description: "Complete autocomplete item",
    }),
    (Definition {
        name: "permission.prompt.fullscreen",
        default: "ctrl+f",
        description: "Toggle permission prompt fullscreen",
    }),
    (Definition {
        name: "plugins.toggle",
        default: "space",
        description: "Toggle plugin",
    }),
    (Definition {
        name: "dialog.plugins.install",
        default: "shift+i",
        description: "Install plugin from plugin dialog",
    }),
    (Definition {
        name: "terminal_suspend",
        default: "ctrl+z",
        description: "Suspend terminal",
    }),
    (Definition {
        name: "terminal_title_toggle",
        default: "none",
        description: "Toggle terminal title",
    }),
    (Definition {
        name: "tips_toggle",
        default: "<leader>h",
        description: "Toggle tips on home screen",
    }),
    (Definition {
        name: "plugin_manager",
        default: "none",
        description: "Open plugin manager dialog",
    }),
    (Definition {
        name: "plugin_install",
        default: "none",
        description: "Install plugin",
    }),
    (Definition {
        name: "which_key_toggle",
        default: "ctrl+alt+k",
        description: "Toggle which-key panel",
    }),
    (Definition {
        name: "which_key_layout_toggle",
        default: "ctrl+alt+shift+k",
        description: "Switch which-key layout",
    }),
    (Definition {
        name: "which_key_pending_toggle",
        default: "ctrl+alt+shift+p",
        description: "Toggle which-key pending preview",
    }),
    (Definition {
        name: "which_key_group_previous",
        default: "ctrl+alt+left,ctrl+alt+[",
        description: "Previous which-key group",
    }),
    (Definition {
        name: "which_key_group_next",
        default: "ctrl+alt+right,ctrl+alt+]",
        description: "Next which-key group",
    }),
    (Definition {
        name: "which_key_scroll_up",
        default: "ctrl+alt+up,ctrl+alt+p",
        description: "Scroll which-key up",
    }),
    (Definition {
        name: "which_key_scroll_down",
        default: "ctrl+alt+down,ctrl+alt+n",
        description: "Scroll which-key down",
    }),
    (Definition {
        name: "which_key_page_up",
        default: "ctrl+alt+pageup",
        description: "Page which-key up",
    }),
    (Definition {
        name: "which_key_page_down",
        default: "ctrl+alt+pagedown",
        description: "Page which-key down",
    }),
    (Definition {
        name: "which_key_home",
        default: "ctrl+alt+home",
        description: "Jump to first which-key binding",
    }),
    (Definition {
        name: "which_key_end",
        default: "ctrl+alt+end",
        description: "Jump to last which-key binding",
    }),
];

/// The inverse of [`command_of`] — the keybind definition name for a
/// dotted command name (`SessionList` bindings resolve here).
pub fn keybind_for_command(command: &str) -> Option<&'static str> {
    DEFINITIONS
        .iter()
        .find(|definition| command_of(definition.name) == Some(command))
        .map(|definition| definition.name)
}

/// `CommandMap` (`keybind.ts:256-420`) — keybind name → command name.
/// `None` = the binding has no dotted command (inline dialog handlers).
pub fn command_of(keybind: &str) -> Option<&'static str> {
    Some(match keybind {
        "app_exit" => "app.exit",
        "app_debug" => "app.debug",
        "app_console" => "app.console",
        "app_heap_snapshot" => "app.heap_snapshot",
        "app_toggle_animations" => "app.toggle.animations",
        "app_toggle_file_context" => "app.toggle.file_context",
        "app_toggle_diffwrap" => "app.toggle.diffwrap",
        "app_toggle_paste_summary" => "app.toggle.paste_summary",
        "app_toggle_session_directory_filter" => "app.toggle.session_directory_filter",
        "command_list" => "command.palette.show",
        "help_show" => "help.show",
        "docs_open" => "docs.open",
        "diff_open" => "diff.open",
        "diff_close" => "diff.close",
        "diff_toggle" => "diff.toggle",
        "diff_expand" => "diff.expand",
        "diff_expand_all" => "diff.expand_all",
        "diff_collapse" => "diff.collapse",
        "diff_switch_focus" => "diff.switch_focus",
        "diff_next_hunk" => "diff.next_hunk",
        "diff_previous_hunk" => "diff.previous_hunk",
        "diff_next_file" => "diff.next_file",
        "diff_previous_file" => "diff.previous_file",
        "diff_toggle_file_tree" => "diff.toggle_file_tree",
        "diff_single_patch" => "diff.single_patch",
        "diff_switch_source" => "diff.switch_source",
        "diff_toggle_view" => "diff.toggle_view",
        "diff_help" => "diff.help",
        "editor_open" => "prompt.editor",
        "theme_list" => "theme.switch",
        "theme_switch_mode" => "theme.switch_mode",
        "theme_mode_lock" => "theme.mode.lock",
        "sidebar_toggle" => "session.sidebar.toggle",
        "scrollbar_toggle" => "session.toggle.scrollbar",
        "status_view" => "alforria.status",
        "debug_view" => "alforria.debug",
        "session_export" => "session.export",
        "session_copy" => "session.copy",
        "session_move" => "session.move",
        "session_new" => "session.new",
        "session_list" => "session.list",
        "session_timeline" => "session.timeline",
        "session_fork" => "session.fork",
        "session_rename" => "session.rename",
        "session_delete" => "session.delete",
        "session_share" => "session.share",
        "session_unshare" => "session.unshare",
        "session_interrupt" => "session.interrupt",
        "session_background" => "session.background",
        "session_compact" => "session.compact",
        "session_toggle_timestamps" => "session.toggle.timestamps",
        "session_toggle_generic_tool_output" => "session.toggle.generic_tool_output",
        "session_queued_prompts" => "session.queued_prompts",
        "session_child_first" => "session.child.first",
        "session_child_cycle" => "session.child.next",
        "session_child_cycle_reverse" => "session.child.previous",
        "session_parent" => "session.parent",
        "session_pin_toggle" => "session.pin.toggle",
        "session_quick_switch_1" => "session.quick_switch.1",
        "session_quick_switch_2" => "session.quick_switch.2",
        "session_quick_switch_3" => "session.quick_switch.3",
        "session_quick_switch_4" => "session.quick_switch.4",
        "session_quick_switch_5" => "session.quick_switch.5",
        "session_quick_switch_6" => "session.quick_switch.6",
        "session_quick_switch_7" => "session.quick_switch.7",
        "session_quick_switch_8" => "session.quick_switch.8",
        "session_quick_switch_9" => "session.quick_switch.9",
        "stash_delete" => "stash.delete",
        "model_provider_list" => "model.dialog.provider",
        "model_favorite_toggle" => "model.dialog.favorite",
        "model_list" => "model.list",
        "model_cycle_recent" => "model.cycle_recent",
        "model_cycle_recent_reverse" => "model.cycle_recent_reverse",
        "model_cycle_favorite" => "model.cycle_favorite",
        "model_cycle_favorite_reverse" => "model.cycle_favorite_reverse",
        "mcp_list" => "mcp.list",
        "provider_connect" => "provider.connect",
        "console_org_switch" => "console.org.switch",
        "agent_list" => "agent.list",
        "agent_cycle" => "agent.cycle",
        "agent_cycle_reverse" => "agent.cycle.reverse",
        "variant_cycle" => "variant.cycle",
        "variant_list" => "variant.list",
        "messages_page_up" => "session.page.up",
        "messages_page_down" => "session.page.down",
        "messages_line_up" => "session.line.up",
        "messages_line_down" => "session.line.down",
        "messages_half_page_up" => "session.half.page.up",
        "messages_half_page_down" => "session.half.page.down",
        "messages_first" => "session.first",
        "messages_last" => "session.last",
        "messages_next" => "session.message.next",
        "messages_previous" => "session.message.previous",
        "messages_last_user" => "session.messages_last_user",
        "messages_copy" => "messages.copy",
        "messages_undo" => "session.undo",
        "messages_redo" => "session.redo",
        "messages_toggle_conceal" => "session.toggle.conceal",
        "tool_details" => "session.toggle.actions",
        "display_thinking" => "session.toggle.thinking",
        "prompt_submit" => "prompt.submit",
        "prompt_editor_context_clear" => "prompt.editor_context.clear",
        "prompt_skills" => "prompt.skills",
        "prompt_stash" => "prompt.stash",
        "prompt_stash_pop" => "prompt.stash.pop",
        "prompt_stash_list" => "prompt.stash.list",
        "workspace_set" => "workspace.set",
        "input_clear" => "prompt.clear",
        "input_paste" => "prompt.paste",
        "input_submit" => "input.submit",
        "input_newline" => "input.newline",
        "input_move_left" => "input.move.left",
        "input_move_right" => "input.move.right",
        "input_move_up" => "input.move.up",
        "input_move_down" => "input.move.down",
        "input_select_left" => "input.select.left",
        "input_select_right" => "input.select.right",
        "input_select_up" => "input.select.up",
        "input_select_down" => "input.select.down",
        "input_line_home" => "input.line.home",
        "input_line_end" => "input.line.end",
        "input_select_line_home" => "input.select.line.home",
        "input_select_line_end" => "input.select.line.end",
        "input_visual_line_home" => "input.visual.line.home",
        "input_visual_line_end" => "input.visual.line.end",
        "input_select_visual_line_home" => "input.select.visual.line.home",
        "input_select_visual_line_end" => "input.select.visual.line.end",
        "input_buffer_home" => "input.buffer.home",
        "input_buffer_end" => "input.buffer.end",
        "input_select_buffer_home" => "input.select.buffer.home",
        "input_select_buffer_end" => "input.select.buffer.end",
        "input_delete_line" => "input.delete.line",
        "input_delete_to_line_end" => "input.delete.to.line.end",
        "input_delete_to_line_start" => "input.delete.to.line.start",
        "input_backspace" => "input.backspace",
        "input_delete" => "input.delete",
        "input_undo" => "input.undo",
        "input_redo" => "input.redo",
        "input_word_forward" => "input.word.forward",
        "input_word_backward" => "input.word.backward",
        "input_select_word_forward" => "input.select.word.forward",
        "input_select_word_backward" => "input.select.word.backward",
        "input_delete_word_forward" => "input.delete.word.forward",
        "input_delete_word_backward" => "input.delete.word.backward",
        "input_select_all" => "input.select.all",
        "history_previous" => "prompt.history.previous",
        "history_next" => "prompt.history.next",
        "terminal_suspend" => "terminal.suspend",
        "terminal_title_toggle" => "terminal.title.toggle",
        "tips_toggle" => "tips.toggle",
        "plugin_manager" => "plugins.list",
        "plugin_install" => "plugins.install",
        "which_key_toggle" => "which-key.toggle",
        "which_key_layout_toggle" => "which-key.layout.toggle",
        "which_key_pending_toggle" => "which-key.pending.toggle",
        "which_key_group_previous" => "which-key.group.previous",
        "which_key_group_next" => "which-key.group.next",
        "which_key_scroll_up" => "which-key.scroll.up",
        "which_key_scroll_down" => "which-key.scroll.down",
        "which_key_page_up" => "which-key.page.up",
        "which_key_page_down" => "which-key.page.down",
        "which_key_home" => "which-key.home",
        "which_key_end" => "which-key.end",
        _ => return None,
    })
}

// ------------------------------------------------------------- parsing

/// `KEY_ALIASES` (`keymap.tsx:112-126`): matched case-insensitively,
/// bounded by separators on both sides.
const KEY_ALIASES: &[(&str, &str)] = &[
    ("enter", "return"),
    ("esc", "escape"),
    ("pgdown", "pagedown"),
    ("pgup", "pageup"),
];

fn is_alias_separator(char: char) -> bool {
    matches!(char, '+' | ',' | ' ' | '\t')
}

/// `expandKeyAliases` (`keymap.tsx:119-126`) — the regex
/// `(^|[+,\s>])alias(?=$|[+,\s<])` with the "gi" flag.
pub fn expand_key_aliases(input: &str) -> Option<String> {
    let mut result = input.to_string();
    for (alias, replacement) in KEY_ALIASES {
        let alias: Vec<char> = alias.chars().collect();
        let mut next = String::with_capacity(result.len());
        let current: Vec<char> = result.chars().collect();
        let lower: Vec<char> = result.to_lowercase().chars().collect();
        let mut index = 0;
        while index < current.len() {
            let matches = index + alias.len() <= current.len()
                && alias
                    .iter()
                    .enumerate()
                    .all(|(offset, char)| lower[index + offset] == *char);
            if matches {
                let before_ok = index == 0
                    || is_alias_separator(current[index - 1])
                    || current[index - 1] == '>';
                let after = index + alias.len();
                let after_ok = after >= current.len()
                    || is_alias_separator(current[after])
                    || current[after] == '<';
                if before_ok && after_ok {
                    next.push_str(replacement);
                    index += alias.len();
                    continue;
                }
            }
            next.push(current[index]);
            index += 1;
        }
        result = next;
    }
    if result == input {
        None
    } else {
        Some(result)
    }
}

/// One key press: a key name plus its modifier flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyStroke {
    pub key: String,
    pub ctrl: bool,
    pub shift: bool,
    pub meta: bool,
    pub super_key: bool,
    pub hyper: bool,
}

/// One alternative of a binding value — a key sequence (`<leader>q`) is
/// a two-stroke sequence, a chord (`ctrl+shift+a`) a single stroke.
pub type Sequence = Vec<KeyStroke>;

/// A parsed binding value. Both `false` and `"none"` disable the
/// binding; `listed` records whether the binding still appears in
/// help/which-key listings (§5.2 — only `"none"` does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingValue {
    Disabled { listed: bool },
    Alternatives(Vec<Sequence>),
}

/// Parse a chord like `ctrl+alt+b` into a single stroke.
fn parse_chord(input: &str) -> Option<KeyStroke> {
    let mut key = None;
    let mut ctrl = false;
    let mut shift = false;
    let mut meta = false;
    let mut super_key = false;
    let mut hyper = false;
    for token in input.split('+') {
        match token.trim().to_ascii_lowercase().as_str() {
            "ctrl" => ctrl = true,
            "shift" => shift = true,
            "meta" | "alt" => meta = true,
            "super" => super_key = true,
            "hyper" => hyper = true,
            _ => key = Some(token.trim().to_string()),
        }
    }
    let key = key.filter(|key| !key.is_empty())?;
    // A bare capital letter is shift + the letter.
    let shift =
        shift || (key.chars().count() == 1 && key.chars().next().unwrap().is_ascii_uppercase());
    Some(KeyStroke {
        key: key.to_ascii_lowercase(),
        ctrl,
        shift,
        meta,
        super_key,
        hyper,
    })
}

/// Parse one binding alternative, expanding `<leader>` into the leader
/// stroke and key aliases.
fn parse_alternative(input: &str, leader: &str) -> Option<Sequence> {
    let input = expand_key_aliases(input).unwrap_or_else(|| input.to_string());
    if input.trim() == "<leader>" {
        return parse_chord(leader).map(|stroke| vec![stroke]);
    }
    if let Some(rest) = input.strip_prefix("<leader>") {
        let mut sequence = parse_alternative(rest, leader)?;
        let leader_stroke = parse_chord(leader)?;
        sequence.insert(0, leader_stroke);
        return Some(sequence);
    }
    parse_chord(&input).map(|stroke| vec![stroke])
}

/// Parse a binding value string: `"none"`, comma alternatives, `+`
/// chords, `<leader>` tokens (`keybind.ts:439-457`).
pub fn parse_binding_value(value: &str, leader: &str) -> BindingValue {
    if value == "none" {
        return BindingValue::Disabled { listed: true };
    }
    if value == "false" || value.is_empty() {
        return BindingValue::Disabled { listed: false };
    }
    let mut alternatives = Vec::new();
    for alternative in value.split(',') {
        if let Some(sequence) = parse_alternative(alternative, leader) {
            alternatives.push(sequence);
        }
    }
    if alternatives.is_empty() {
        return BindingValue::Disabled { listed: false };
    }
    BindingValue::Alternatives(alternatives)
}

// ------------------------------------------------------ event matching

/// Map a crossterm event to the binding-space stroke it names, `None`
/// for events no binding can express. Uppercase chars fold to their
/// lowercase key plus SHIFT.
pub fn event_to_stroke(event: &KeyEvent) -> Option<KeyStroke> {
    let mut shift = false;
    let key = match event.code {
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() {
                shift = true;
            }
            c.to_ascii_lowercase().to_string()
        }
        KeyCode::Enter => "return".to_string(),
        KeyCode::Esc => "escape".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => {
            shift = true;
            "tab".to_string()
        }
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::F(n @ 1..=12) => return key_function(n, event.modifiers),
        _ => return None,
    };
    let modifiers = event.modifiers;
    Some(KeyStroke {
        key,
        ctrl: modifiers.contains(KeyModifiers::CONTROL),
        shift: modifiers.contains(KeyModifiers::SHIFT) || shift,
        meta: modifiers.contains(KeyModifiers::ALT),
        super_key: modifiers.contains(KeyModifiers::SUPER),
        hyper: modifiers.contains(KeyModifiers::HYPER),
    })
}

fn key_function(n: u8, modifiers: KeyModifiers) -> Option<KeyStroke> {
    Some(KeyStroke {
        key: format!("f{n}"),
        ctrl: modifiers.contains(KeyModifiers::CONTROL),
        shift: modifiers.contains(KeyModifiers::SHIFT),
        meta: modifiers.contains(KeyModifiers::ALT),
        super_key: modifiers.contains(KeyModifiers::SUPER),
        hyper: modifiers.contains(KeyModifiers::HYPER),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_table_is_unique() {
        let mut names: Vec<_> = DEFINITIONS.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(count, names.len(), "duplicate definitions");
    }

    #[test]
    fn parses_comma_alternatives_and_chords() {
        let value = parse_binding_value("ctrl+c,ctrl+d,<leader>q", "ctrl+x");
        let BindingValue::Alternatives(alternatives) = value else {
            panic!("not alternatives");
        };
        assert_eq!(alternatives.len(), 3);
        assert_eq!(
            alternatives[0],
            vec![KeyStroke {
                key: "c".into(),
                ctrl: true,
                shift: false,
                meta: false,
                super_key: false,
                hyper: false,
            }]
        );
        let leader = parse_chord("ctrl+x").unwrap();
        assert_eq!(alternatives[2], vec![leader, parse_chord("q").unwrap()]);
    }

    #[test]
    fn parses_none_and_false() {
        assert_eq!(
            parse_binding_value("none", "ctrl+x"),
            BindingValue::Disabled { listed: true }
        );
        assert_eq!(
            parse_binding_value("false", "ctrl+x"),
            BindingValue::Disabled { listed: false }
        );
    }

    #[test]
    fn expands_key_aliases() {
        assert_eq!(
            expand_key_aliases("ctrl+return"),
            None,
            "return is already canonical"
        );
        assert_eq!(expand_key_aliases("Enter"), Some("return".to_string()));
        assert_eq!(expand_key_aliases("esc"), Some("escape".to_string()));
        assert_eq!(expand_key_aliases("pgdown"), Some("pagedown".to_string()));
        assert_eq!(expand_key_aliases("pgup"), Some("pageup".to_string()));
        // Bounded: no match inside words (`entering` stays).
        assert_eq!(expand_key_aliases("entering"), None);
        // `<enter>` — the `<` bound blocks the alias.
        assert_eq!(expand_key_aliases("<enter>"), None);
        assert_eq!(
            expand_key_aliases("shift+ESC"),
            Some("shift+escape".to_string())
        );
    }

    #[test]
    fn capital_letters_become_shift() {
        let stroke = parse_chord("E").unwrap();
        assert_eq!(stroke.key, "e");
        assert!(stroke.shift);
        assert!(!stroke.ctrl);
    }

    #[test]
    fn events_match_bindings() {
        let parsed = parse_binding_value("ctrl+alt+b", "ctrl+x");
        let BindingValue::Alternatives(alternatives) = parsed else {
            panic!();
        };
        let event = KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(event_to_stroke(&event), Some(alternatives[0][0].clone()));
        // Extra modifier disqualifies.
        let event = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_ne!(event_to_stroke(&event), Some(alternatives[0][0].clone()));
    }

    #[test]
    fn shift_tab_and_f_keys() {
        let value = parse_binding_value("shift+tab", "ctrl+x");
        let BindingValue::Alternatives(alternatives) = value else {
            panic!();
        };
        let event = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(event_to_stroke(&event), Some(alternatives[0][0].clone()));
        let value = parse_binding_value("f2", "ctrl+x");
        let BindingValue::Alternatives(alternatives) = value else {
            panic!();
        };
        let event = KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE);
        assert_eq!(event_to_stroke(&event), Some(alternatives[0][0].clone()));
    }
}
