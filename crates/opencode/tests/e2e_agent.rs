//! Shared full-agent E2E suite (spec E2E §2.1): drives the built
//! `opencode` binary through the real server + LLM wire protocol against
//! a scripted backend seam.

#[path = "e2e_agent/backend.rs"]
mod backend;
#[path = "e2e_agent/harness.rs"]
mod harness;
#[path = "e2e_agent/mock.rs"]
mod mock;
#[path = "e2e_agent/scenarios.rs"]
mod scenarios;
#[path = "e2e_agent/transcript.rs"]
mod transcript;
