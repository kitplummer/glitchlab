## Codebase Knowledge

Use this context to orient yourself. Do NOT re-explore files described here.
Do NOT call `list_files` on paths already listed below.

**Project:** GLITCHLAB — agentic development engine (Rust workspace)
**Test:** `cargo test --workspace`
**Lint:** `cargo clippy --all-targets -- -D warnings`
**Format:** `cargo fmt --check`

### Workspace crates (dependency order: kernel → router → memory → eng-org → cli)

- **kernel** (`crates/kernel/src/`) — Core traits. No external deps. All other crates depend on this.
  - `agent.rs` — `Agent` trait, `AgentContext`, `AgentOutput`, `Message`, `MessageRole`
  - `pipeline.rs` — `Pipeline` trait, `PipelineStatus` enum (19 variants), `PipelineResult`
  - `outcome.rs` — `OutcomeContext`, `ObstacleKind` enum (8 variants) for structured failure info
  - `budget.rs` — `BudgetTracker` (token + dollar limits per task)
  - `context.rs` — `ContextAssembler` with priority-based segment selection
  - `tool.rs` — `ToolDefinition`, `ToolCall`, `ToolResult` types
  - `governance.rs` — `ApprovedAction<T>` (type-level governance, cannot be constructed outside module)
  - `error.rs` — `Error` enum via `thiserror`, `Result<T>` alias
  - `org.rs` — `Org` trait for organizational structures

- **router** (`crates/router/src/`) — LLM routing, provider-agnostic.
  - `router.rs` — `Router` struct with `complete()` and `complete_with_tools()` methods
  - `provider.rs` — `Provider` trait + implementations (anthropic, gemini, openai subdirs)
  - `chooser.rs` — `ModelChooser`, `ModelTier` enum, `ModelProfile`, `RolePreference`
  - `budget.rs` — Router-level budget tracking (tokens + cost)
  - `provider_diagnostics.rs` — `ProviderDiagnostic`, `ErrorCategory`, `classify_error()`

- **memory** (`crates/memory/src/`) — Persistence layer.
  - `history.rs` — `HistoryBackend` trait, JSONL implementation
  - `beads.rs` — Beads integration (bd CLI wrapper)
  - `dolt.rs` — Dolt database backend

- **eng-org** (`crates/eng-org/src/`) — Engineering org: agents + pipeline + orchestrator.
  - `pipeline.rs` — `EngineeringPipeline` (~6500 lines), stages: index → plan → triage → implement → test → security → release → archive
  - `orchestrator.rs` — `Orchestrator` runs tasks from queue, tracks budget, handles retries, `CumulativeBudget`, `AttemptTracker`
  - `agents/` — 9 agents, all implement `Agent` trait:
    - `planner.rs` — Plans implementation steps, decides decomposition
    - `implementer.rs` — Writes code via tool-use loop (read/write/edit/run)
    - `debugger.rs` — Fixes test failures
    - `security.rs` — Reviews changes for vulnerabilities
    - `architect.rs` — Two agents: `ArchitectTriageAgent` (sizes tasks S/M/L/XL) and `ArchitectReviewAgent`
    - `release.rs` — Assesses release readiness
    - `archivist.rs` — Summarizes changes for history
    - `ciso.rs` — Strategic risk analysis
  - `agents/mod.rs` — `build_user_message()` assembles context for all agents
  - `agents/parse.rs` — `parse_json_response()` with regex fallback
  - `config.rs` — `EngConfig` with YAML loading, `RoutingConfig`, `LimitsConfig`
  - `taskqueue.rs` — `TaskQueue`, `Task`, `TaskStatus`, YAML + Beads loading
  - `indexer.rs` — `RepoIndex`, `build_index()`, `build_codebase_knowledge()`
  - `workspace.rs` — Git worktree creation/cleanup per task
  - `tqm.rs` — `TQMAnalyzer`, 9 `PatternKind` variants for failure pattern detection
  - `tools/` — Tool implementations: `read_file`, `write_file`, `edit_file`, `list_files`, `run_command`

- **cli** (`crates/cli/src/`) — Binary. Commands: `run`, `batch`, `status`, `plan`.

### Key patterns

- **All agents are stateless** — state lives in git worktree, history files, and beads.
- **Provider-agnostic** — LLM calls go through `Router`, never directly to provider APIs.
- **Error types** — Use `thiserror` in libraries, `anyhow` only in CLI.
- **Logging** — Use `tracing` (structured), never `println!` in library crates.
- **Tests** — Inline `#[cfg(test)] mod tests` in every module. 90% coverage minimum.
- **Graceful degradation** — JSON parse failures produce fallback outputs with `parse_error: true`, never panics.
- **Config precedence** — Built-in defaults → `.glitchlab/config.yaml` overrides → CLI flags.
- **Pipeline stages** — Each stage gets `AgentContext` via `build_user_message()`. Context flows forward via `ctx.stage_outputs`.
- **Budget enforcement** — `BudgetTracker` in router enforces per-task limits. `CumulativeBudget` in orchestrator enforces batch limits.

### Common edit patterns

- Adding a new enum variant: add to the enum, update `Display` impl, update tests, update any `match` arms.
- Adding a new agent: implement `Agent` trait, register in `pipeline.rs` stage flow, add to agent roster in config.
- Adding a field to a struct: update struct def, update `Default` impl if applicable, update serialization, update tests.
- Modifying pipeline behavior: edit `pipeline.rs` `run()` method, update relevant stage, add pipeline test.
