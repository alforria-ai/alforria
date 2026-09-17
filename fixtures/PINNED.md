# Frozen fixtures

All fixtures in this directory are frozen copies extracted from the pinned
TypeScript reference commit:

- Repository: anomalyco/opencode (branch `dev`)
- Commit: 88c6c7abc7f320b6aabed2634ac0b2d6e6ecea67
- Version: 1.18.31

## Contents

- `openapi/openapi.json` — v1+v2 HTTP API contract (from `packages/sdk/openapi.json`).
- `llm-recordings/` — recorded LLM protocol exchanges (SSE streams + responses) per
  protocol: openai-chat, openai-compatible-chat, openai-responses, anthropic-messages,
  bedrock, gemini. Golden tests in `opencode-llm` replay these.
- `schema-src/` — `@opencode-ai/schema` TypeScript sources (wire-visible DTO definitions).
- `protocol-src/` — `@opencode-ai/protocol` TypeScript sources (v2 API group definitions).

## Provenance

These are reference material for deriving Rust types and wire-conformance tests.
Do NOT regenerate or hand-edit; refresh only by re-extracting from a new pinned commit.
