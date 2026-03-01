# Contributing to GLITCHLAB

First off, thank you for considering contributing to GLITCHLAB! It's people like you that make GLITCHLAB a better tool for everyone.

## Architectural Overview

GLITCHLAB is built as a **deterministic orchestrator** that manages a pipeline of **stateless agents**. The Rust workspace lives in `crates/` with six crates:

1. **`kernel`** — Core traits and types (Agent, Pipeline, Governance, Budget, Context, Tool). Provider-agnostic, no external service dependencies.

2. **`router`** — Vendor-agnostic LLM routing. Provider trait with Anthropic, Gemini, and OpenAI-compatible implementations. `ModelChooser` for cost-aware model selection.

3. **`memory`** — Persistence layer. JSONL history (fallback), Beads integration (graph-based issue tracking).

4. **`eng-org`** — Engineering org: 12 agents, workspace (git worktree isolation), repo indexer, config loading, TQM self-repair analyzer, orchestrator, pipeline.

5. **`dashboard`** — SSE-based live dashboard for monitoring batch runs.

6. **`cli`** — Binary crate. CLI commands via `clap`.

## Getting Started

### Prerequisites

* Rust nightly toolchain
* Git
* API keys for Gemini (Google) and/or Anthropic

### Local Setup

1. Fork the repository and clone it locally.
2. Build and run tests:

```bash
cargo build
cargo test --workspace
```

3. Enable pre-commit hooks (enforces `cargo fmt` + `cargo clippy`):

```bash
git config core.hooksPath .githooks
```

4. Configure your environment:

```bash
export GEMINI_API_KEY="..."
export ANTHROPIC_API_KEY="sk-ant-..."
```

## Running Tests

Before submitting a Pull Request, ensure all checks pass:

```bash
make ci          # fmt-check + clippy + test + coverage
```

Or run individually:

```bash
make test        # cargo test --workspace
make lint        # cargo clippy --all-targets -- -D warnings
make fmt-check   # cargo fmt --all -- --check
make coverage    # cargo tarpaulin (90% minimum)
```

## How to Contribute

### Adding a New Agent

1. Create a new module in `crates/eng-org/src/agents/`.
2. Define a struct implementing the `Agent` trait from `kernel`.
3. Implement `role()`, `persona()`, `system_prompt()`, and `execute()`.
4. Register the agent in the pipeline stages (`crates/eng-org/src/pipeline.rs`).
5. Write unit tests in the same file (inline `#[cfg(test)] mod tests`).

### Adding a New Tool

To give agents more capabilities:

1. Add the tool definition in `crates/eng-org/src/tools.rs`.
2. Implement the `ToolDefinition` schema and execution handler.
3. Register it in the tool executor's allowlist.
4. Ensure it does not allow arbitrary shell injection.

### Modifying the Pipeline

1. Edit the relevant stage in `crates/eng-org/src/pipeline.rs`.
2. Update `PipelineStatus` variants in `crates/kernel/src/pipeline.rs` if adding a new stage.
3. Add pipeline tests covering the new behavior.

## Code Standards

* Rust edition 2024, workspace resolver 2.
* All code must pass `cargo clippy -- -D warnings` with no warnings.
* All code must be formatted with `cargo fmt`.
* Use `thiserror` for library error types, `anyhow` only in the CLI binary.
* Use `tracing` for structured logging, never `println!` in library crates.
* Minimum 90% test coverage (measured with `cargo-tarpaulin`).
* Write tests in the same commit as the code they cover.

## Development Principles

* **Build Weird. Ship Clean.** — Agents can be chaotic, but the output must be surgical and high-quality.
* **Local-First** — We avoid cloud dependencies other than the model APIs.
* **Deterministic Orchestration** — The sequence of events should be explicit, not governed by "emergent behavior".
* **Provider-Agnostic** — LLM calls go through the Router, never directly to a provider API from agent code.
* **Type-Level Governance** — `ApprovedAction<T>` cannot be constructed outside the governance module. The compiler enforces safety.
