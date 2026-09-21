//! Cross-parity harness (spec PARITY §2.1): drives the Rust binary and
//! the pinned TS reference side by side against the same scripted mock
//! LLM and diffs normalized captures. Every test is gated behind
//! `OPENCODE_PARITY=1` and early-returns in <1s without it.

#[path = "e2e_agent/backend.rs"]
mod backend;
#[path = "e2e_agent/harness.rs"]
mod harness;
#[path = "e2e_agent/transcript.rs"]
mod transcript;
#[path = "e2e_agent/wire.rs"]
mod wire;

#[path = "parity/catalog.rs"]
mod catalog;
#[path = "parity/differ.rs"]
mod differ;
#[path = "parity/normal.rs"]
mod normal;
#[path = "parity/harness.rs"]
mod parity_harness;
#[path = "parity/parity.rs"]
mod parity_scenarios;
#[path = "parity/report.rs"]
mod report;
#[path = "parity/scenarios.rs"]
mod scenarios;
#[path = "parity/static_parity.rs"]
mod static_parity;
