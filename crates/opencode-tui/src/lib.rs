//! ratatui TUI — a stateless HTTP+SSE client of the opencode server (M8).
//!
//! TS reference: `packages/tui` (whole package is ground truth; see
//! `scratchpad/specs/M8.md`). Elm-style model: one `update()`, full redraw
//! per change. This crate is transport-first: everything above
//! `transport::api` is testable against a `FakeApi` + fake event source.

pub mod state;
pub mod transport;
