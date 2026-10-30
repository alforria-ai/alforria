//! The transcript's render cache (T2): rendered lines per part, kept
//! across frames, and a segmented line list so a frame clones only the
//! rows it shows.
//!
//! OpenTUI keeps one retained renderable per part and re-lays it out only
//! when its content or width changes; the port renders immediate-mode,
//! so without this every frame re-parsed and re-wrapped the whole
//! session. A part's lines are reused while the part is equal to the
//! cached copy and nothing it reads changed: the frame-wide inputs
//! ([`FrameKey`]: width, theme, toggles…) clear the cache when they
//! change; the per-part ones (finished message, expanded, expanded
//! error) are checked per entry. Parts that animate or read other
//! sessions (running tools, unfinished reasoning, `task`) are never
//! cached.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use ratatui::text::Line;

use alforria_schema::session_v1::{V1Part, V1ToolState};

use super::Ctx;
use crate::ui::theme::Theme;

/// A shared block of rendered rows.
pub type Block = Rc<Vec<Line<'static>>>;

/// Rows held as shared blocks plus a tail of loose rows; `window`
/// clones only the visible ones.
#[derive(Default)]
pub struct Segmented {
    blocks: Vec<Block>,
    tail: Vec<Line<'static>>,
    len: usize,
}

impl Segmented {
    pub fn push(&mut self, line: Line<'static>) {
        self.tail.push(line);
        self.len += 1;
    }

    pub fn extend(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        for line in lines {
            self.push(line);
        }
    }

    pub fn extend_shared(&mut self, block: Block) {
        if block.is_empty() {
            return;
        }
        self.flush();
        self.len += block.len();
        self.blocks.push(block);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn flush(&mut self) {
        if !self.tail.is_empty() {
            self.blocks.push(Rc::new(std::mem::take(&mut self.tail)));
        }
    }

    /// The `height` rows from `top`.
    pub fn window(mut self, top: usize, height: usize) -> Vec<Line<'static>> {
        self.flush();
        let mut out = Vec::with_capacity(height);
        let mut skip = top;
        for block in &self.blocks {
            if out.len() >= height {
                break;
            }
            if skip >= block.len() {
                skip -= block.len();
                continue;
            }
            let take = (height - out.len()).min(block.len() - skip);
            out.extend(block[skip..skip + take].iter().cloned());
            skip = 0;
        }
        out
    }
}

/// Everything frame-wide a part's rendering reads; a change clears the
/// cache.
#[derive(Clone, PartialEq)]
pub struct FrameKey {
    session_id: String,
    width: u16,
    theme: Theme,
    thinking_mode: &'static str,
    show_details: bool,
    show_generic_output: bool,
    diff_wrap: crate::ui::diff::WrapMode,
    conceal: bool,
    diff_style: String,
    directory: Option<String>,
    /// The call the session's head permission asks about (inline tool
    /// rows turn `warning` for it).
    permission_call: Option<String>,
}

impl FrameKey {
    pub fn new(ctx: &Ctx) -> FrameKey {
        let app = ctx.app;
        FrameKey {
            session_id: ctx.session_id.to_string(),
            width: ctx.width,
            theme: *ctx.theme,
            thinking_mode: ctx.thinking_mode(),
            show_details: ctx.show_details(),
            show_generic_output: ctx.show_generic_tool_output(),
            diff_wrap: ctx.diff_wrap_mode(),
            conceal: ctx.conceal(),
            diff_style: app.config.diff_style.clone(),
            directory: app.state.project.instance_path.directory.clone(),
            permission_call: app
                .state
                .sync
                .permission
                .get(ctx.session_id)
                .and_then(|requests| requests.first())
                .and_then(|request| request.tool.as_ref())
                .map(|tool| tool.call_id.clone()),
        }
    }
}

struct Entry {
    part: V1Part,
    message_done: bool,
    expanded: bool,
    expanded_error: bool,
    lines: Block,
    generation: u64,
}

/// User-message text wrapped at a width.
struct TextEntry {
    text: String,
    width: u16,
    lines: Block,
    generation: u64,
}

#[derive(Default)]
struct Cache {
    key: Option<FrameKey>,
    parts: HashMap<String, Entry>,
    texts: HashMap<String, TextEntry>,
    generation: u64,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Start a frame: drop everything when a frame-wide input changed.
pub fn begin(key: FrameKey) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.key.as_ref() != Some(&key) {
            cache.parts.clear();
            cache.texts.clear();
            cache.key = Some(key);
        }
        cache.generation += 1;
    });
}

/// End a frame: forget the parts this frame didn't show (deleted,
/// reverted, another session's).
pub fn end() {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let generation = cache.generation;
        cache
            .parts
            .retain(|_, entry| entry.generation == generation);
        cache
            .texts
            .retain(|_, entry| entry.generation == generation);
    });
}

fn part_id(part: &V1Part) -> Option<&str> {
    match part {
        V1Part::Text { id, .. } | V1Part::Reasoning { id, .. } | V1Part::Tool { id, .. } => {
            Some(id)
        }
        _ => None,
    }
}

/// Whether the part's rows are a pure function of the part and the
/// keyed inputs (no spinner, no other session's state).
fn cacheable(part: &V1Part, ctx: &Ctx) -> bool {
    match part {
        V1Part::Text { .. } => true,
        V1Part::Reasoning { time, .. } => time.end.is_some() || ctx.message_done,
        V1Part::Tool { tool, state, .. } => {
            tool != "task"
                && tool != "workflow"
                && matches!(
                    state,
                    V1ToolState::Completed { .. } | V1ToolState::Error { .. }
                )
        }
        _ => false,
    }
}

/// `parts::render_part`, through the cache.
pub fn part_lines(part: &V1Part, ctx: &Ctx) -> Block {
    let Some(id) = part_id(part).filter(|_| cacheable(part, ctx)) else {
        return Rc::new(super::parts::render_part(part, ctx));
    };
    let expanded = ctx.app.ui.expanded.contains(id);
    let expanded_error = ctx.app.ui.expanded_errors.contains(id);
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let generation = cache.generation;
        if let Some(entry) = cache.parts.get_mut(id) {
            if entry.message_done == ctx.message_done
                && entry.expanded == expanded
                && entry.expanded_error == expanded_error
                && entry.part == *part
            {
                entry.generation = generation;
                return entry.lines.clone();
            }
        }
        let lines: Block = Rc::new(super::parts::render_part(part, ctx));
        cache.parts.insert(
            id.to_string(),
            Entry {
                part: part.clone(),
                message_done: ctx.message_done,
                expanded,
                expanded_error,
                lines: lines.clone(),
                generation,
            },
        );
        lines
    })
}

/// `render(text, width)` memoized per message id.
pub fn text_lines(
    message_id: &str,
    text: &str,
    width: u16,
    render: impl FnOnce() -> Vec<Line<'static>>,
) -> Block {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let generation = cache.generation;
        if let Some(entry) = cache.texts.get_mut(message_id) {
            if entry.width == width && entry.text == text {
                entry.generation = generation;
                return entry.lines.clone();
            }
        }
        let lines: Block = Rc::new(render());
        cache.texts.insert(
            message_id.to_string(),
            TextEntry {
                text: text.to_string(),
                width,
                lines: lines.clone(),
                generation,
            },
        );
        lines
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(count: usize, tag: &str) -> Vec<Line<'static>> {
        (0..count)
            .map(|index| Line::from(format!("{tag}{index}")))
            .collect()
    }

    #[test]
    fn segmented_windows_cross_blocks() {
        let mut lines = Segmented::default();
        lines.push(Line::from("top"));
        lines.extend_shared(Rc::new(rows(3, "a")));
        lines.extend(rows(2, "b"));
        lines.extend_shared(Rc::new(Vec::new()));
        lines.extend_shared(Rc::new(rows(2, "c")));
        assert_eq!(lines.len(), 8);
        let window: Vec<String> = lines
            .window(2, 4)
            .iter()
            .map(|line| line.to_string())
            .collect();
        assert_eq!(window, vec!["a1", "a2", "b0", "b1"]);
    }

    #[test]
    fn segmented_window_past_the_end_is_short() {
        let mut lines = Segmented::default();
        lines.extend(rows(3, "a"));
        assert_eq!(lines.window(2, 10).len(), 1);
    }
}
