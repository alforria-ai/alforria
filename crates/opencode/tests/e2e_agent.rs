//! Shared full-agent E2E suite (spec E2E §2.1): drives the built
//! `opencode` binary through the real server + LLM wire protocol against
//! a scripted backend seam.
//!
//! Scenario index (spec E2E §2.4; mock fixtures in `fixtures/`, the CLI
//! stream golden in `golden/`):
//!
//! | Scenario | Fixture | What it proves |
//! |---|---|---|
//! | A1 file mutation | `a1_file_mutation.json` | `write` tool over the wire → disk + result feedback |
//! | A2 multi-step loop | `a2_multi_step.json` | 3 steps, parallel calls, request #2 carries results |
//! | A3 permission gate | `a3_permission_gate.json` | ask once / always / reject over HTTP |
//! | A4 subagent | `a4_subagent.json` | `task` tool spawns a child session, result XML |
//! | A5 doom-loop | `a5_doom_loop.json` | one `doom_loop` ask, `always=[tool]` unblocks |
//! | A6 compaction | `a6_compaction.json` | overflow → summary turn → compacted continuation |
//! | A7 revert | `a7_revert.json` | git snapshot rollback + diff summary + unrevert |
//! | A8 cancel | `a8_cancel_mid_stream.json` | abort mid-stream, interrupted part, re-prompt |
//! | A9 structured output | `a9_structured_output(_error).json` | json_schema prompt → `structured` on the message; error variant |
//! | A10 CLI stream golden | `a1_file_mutation.json` | `--format json` event-type golden (`golden/a10_cli_stream.json`) |
//! | CLI `--auto` reply | `cli_auto_reply.json` | `run --auto` answers asks; without it auto-rejects |

#[path = "e2e_agent/backend.rs"]
mod backend;
#[path = "e2e_agent/harness.rs"]
mod harness;
#[path = "e2e_agent/live.rs"]
mod live;
#[path = "e2e_agent/mock.rs"]
mod mock;
#[path = "e2e_agent/scenarios.rs"]
mod scenarios;
#[path = "e2e_agent/transcript.rs"]
mod transcript;
#[path = "e2e_agent/wire.rs"]
mod wire;
