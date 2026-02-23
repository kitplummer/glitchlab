# GLITCHLAB — Claude Code Instructions

## Project

GLITCHLAB is an agentic development engine being ported from Python to Rust. The Rust workspace lives in `crates/` with five crates: `kernel`, `memory`, `router`, `eng-org`, and `cli`.

## Test Coverage

**Minimum test coverage: 90%.**

- Every module must have unit tests.
- Every public function and trait implementation must be tested.
- Use `cargo-tarpaulin` for coverage measurement: `cargo tarpaulin --exclude-files 'crates/cli/*' 'crates/memory/src/dolt.rs'`
- The CLI binary and Dolt backend are excluded from coverage (thin orchestration / external DB integration layers, tested via integration).
- Do not merge code that drops coverage below 90%.
- When writing new code, write tests in the same commit.
- Prefer testing behavior over implementation details.

## Development Setup

After cloning, enable the pre-commit hook:

```sh
git config core.hooksPath .githooks
```

This runs `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` before each commit. Commits that fail either check are rejected. The same checks run in CI (`.github/workflows/ci.yml`).

Use the Makefile for common tasks: `make test`, `make lint`, `make fmt`, `make coverage`, `make ci`.

## Code Standards

- Rust edition 2024, workspace resolver 2.
- All code must pass `cargo clippy -- -D warnings` with no warnings.
- All code must be formatted with `cargo fmt`.
- Use `thiserror` for library error types, `anyhow` only in the CLI binary.
- Use `tracing` for structured logging, never `println!` in library crates.
- Agents are stateless — all state lives in the repo, worktree, or history file.
- The framework is provider-agnostic. No code in `kernel` may depend on a specific LLM provider.

## Architecture

- `kernel` — Core traits and types (Agent, Pipeline, Org, Governance, Budget, Context, Tool). No external service dependencies.
- `router` — Vendor-agnostic LLM routing. Provider trait with Anthropic and OpenAI-compatible implementations.
- `memory` — Persistence layer. JSONL history (fallback), Dolt and Beads integration (future).
- `eng-org` — Engineering org: 6 agents (Planner, Implementer, Debugger, Security, Release, Archivist), workspace (git worktree), repo indexer, config loading.
- `cli` — Binary crate. CLI commands via clap.

## Key Design Decisions

- **Provider-agnostic**: LLM calls go through the Router, never directly to a provider API from agent code.
- **Type-level governance**: `ApprovedAction<T>` cannot be constructed outside the governance module. The compiler enforces governance.
- **Context assembly is first-class**: The `ContextAssembler` in kernel manages what goes into the LLM context window with priority-based segment selection and truncation.
- **Sessions are ephemeral, state is not**: Inspired by Gastown. Agent sessions may die; all state persists in git, history files, and beads.
- **Graceful degradation**: JSON parse failures produce fallback outputs with `parse_error: true`, never panics.
