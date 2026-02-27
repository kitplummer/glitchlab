# GLITCHLAB

**The Agentic Dev Engine — Build Weird. Ship Clean.**

A local-first, repo-agnostic, multi-agent development engine that evolves codebases under strict governance. Forked from [adjective-rob/glitchlab](https://github.com/adjective-rob/glitchlab) and rewritten in Rust.

## What It Does

GLITCHLAB takes a development task (GitHub issue, local YAML, or interactive prompt), breaks it into an execution plan, implements the changes, runs tests, fixes failures, scans for security issues, and opens a PR — all orchestrated locally with deterministic control.

When things go wrong, the system detects anti-patterns (decomposition loops, stuck agents, budget pressure), diagnoses root causes via its ops agent, and generates remediation tasks that feed back into the queue. It's a self-repairing pipeline.

## Agent Roster

Nine agents, each with a persona and a job:

| Agent | Persona | Role | Default Model |
|-------|---------|------|---------------|
| Planner | Professor Zap | Decompose objectives into execution plans | gemini-2.5-flash |
| Implementer | Patch | Write code and tests (tool-use loop, 15 turns) | gemini-2.5-pro |
| Debugger | Reroute | Fix failing tests and builds | claude-sonnet-4 |
| Security | Firewall Frankie | Review changes for vulnerabilities | gemini-2.5-flash |
| Release | Semver Sam | Determine semantic version bump | gemini-2.5-flash |
| Archivist | Nova | Generate ADRs and documentation | gemini-2.5-flash |
| Architect (Triage) | Blueprint | Pre-impl: check if work is already done | gemini-2.5-flash |
| Architect (Review) | Blueprint | Post-impl: review diff before merge | gemini-2.5-flash |
| Ops Diagnosis | Circuit | Diagnose TQM patterns, generate remediation | gemini-2.5-flash-lite |

Model assignments are configurable per-repo. The `ModelChooser` selects models based on tier, capabilities, and cost.

## Quick Start

### 1. Build

```bash
git clone https://github.com/kitplummer/glitchlab.git
cd glitchlab
cargo build --release
```

### 2. Configure API Keys

```bash
export GEMINI_API_KEY="..."
export ANTHROPIC_API_KEY="sk-ant-..."
```

### 3. Initialize a Repository

```bash
glitchlab init ~/your-project
```

### 4. Run

**From GitHub issue:**
```bash
glitchlab run --repo ~/your-project --issue 42
```

**From local task file:**
```bash
glitchlab run --repo ~/your-project --local-task
```

**Batch run (process entire backlog):**
```bash
glitchlab batch --repo ~/your-project --budget 100.0
```

### 5. Check Status

```bash
glitchlab status --repo ~/your-project
```

## Pipeline

The engineering pipeline runs 15+ stages per task:

1. **Task pickup** — Priority queue with remediation-first scheduling
2. **Architect triage** — Skip work that's already done
3. **Workspace creation** — Isolated git worktree per task
4. **Boundary check** — Protected-path enforcement (kernel, .github, etc.)
5. **Planning** — LLM decomposes the objective into steps
6. **Implementation** — Tool-use loop: write code, read files, run commands
7. **Test execution** — Run the project's test suite
8. **Debug loop** — Up to N attempts to fix failures
9. **Security review** — Scan for vulnerabilities before PR
10. **Architect review** — Diff review with approval/rejection
11. **Release assessment** — Semantic version bump determination
12. **Documentation** — ADR generation
13. **Commit + PR** — Open PR with structured description
14. **Auto-merge** — Merge on approval (configurable)
15. **TQM analysis** — Detect anti-patterns, feed back into queue

## Self-Repair Loop

The Task Queue Manager (TQM) analyzes pipeline outcomes and detects 9 anti-pattern types:

- **Decomposition loops** — Task keeps splitting without progress
- **Scope creep** — Too many deferred tasks
- **Model degradation** — Repeated failures from same model class
- **Stuck agents** — Same task fails repeatedly
- **Test flakiness** — Intermittent test failures
- **Architect rejection rate** — High review rejection rate
- **Budget pressure** — Approaching spending limit
- **Provider failures** — Infrastructure/setup errors
- **Boundary violations** — Protected-path hits

When patterns are detected, the **Circuit** agent (Ops Diagnosis) generates scoped remediation tasks that are injected at high priority. The pipeline repairs itself.

## Architecture

```
                    Backlog Source
              (GitHub / YAML / Interactive)
                         │
                         ▼
               ┌─────────────────────┐
               │   Orchestrator      │  CumulativeBudget, AttemptTracker,
               │   (deterministic)   │  RestartIntensityMonitor
               └────────┬────────────┘
                        │
          ┌─────────────┼─────────────────┐
          ▼             ▼                 ▼
    ArchitectTriage   Plan → Impl → Debug → Test
                        │
                        ▼
              Security → Release → Archivist
                        │
                        ▼
                   Commit → PR → Merge
                        │
                        ▼
               ┌────────────────┐
               │   TQM Analyzer │──→ Circuit (Ops) ──→ Remediation Tasks
               └────────────────┘         │
                        ▲                 │
                        └─────────────────┘
```

### Crate Layout

```
crates/
├── kernel/       Core traits: Agent, Pipeline, Org, Governance, Budget, Context, Tool
│                 Provider-agnostic. No external service dependencies.
│
├── router/       Vendor-agnostic LLM routing. Provider trait with Anthropic and
│                 OpenAI-compatible implementations. ModelChooser for cost-aware selection.
│
├── memory/       Persistence: JSONL history (fallback), Dolt (SQL), Beads (graph).
│
├── eng-org/      Engineering org: 9 agents, workspace (git worktree), repo indexer,
│                 config loading, TQM analyzer, orchestrator, pipeline.
│
└── cli/          Binary crate. CLI commands via clap.
```

## Configuration

Per-repo overrides in `.glitchlab/config.yaml`:

```yaml
routing:
  implementer: "anthropic/claude-sonnet-4-20250514"
  models:
    - model: "gemini/gemini-2.5-flash"
      tier: standard
      capabilities: [tool_use, code, long_context]
  roles:
    implementer:
      min_tier: standard
      requires: [tool_use]

boundaries:
  protected_paths:
    - "crates/kernel"
    - ".github"

limits:
  max_fix_attempts: 3
  max_dollars_per_task: 0.50
  max_total_tasks: 200
  repair_budget_fraction: 0.20

tqm:
  remediation_enabled: true
  remediation_priority: 2
```

## Cost Model

You only pay for LLM API tokens. The orchestrator runs on your machine.

| Run Type | Typical Cost |
|----------|-------------|
| Single task (Display impl, small fix) | $0.10 - $0.50 |
| Batch run (30 tasks, self-improvement) | $10 - $20 |
| Full batch with repair budget | $15 - $100 |

Budget governance is enforced at every stage. The `CumulativeBudget` tracker splits spending into feature work and repair allocations. Tasks that exceed their budget are halted, not retried.

## Human Intervention Points

GLITCHLAB is autonomous between checkpoints, but you stay in control:

1. **Plan Review** — Approve before implementation begins
2. **Core Boundary** — Protected paths block unauthorized changes
3. **Fix Loop** — Halts after N failed attempts
4. **Architect Review** — Diff review before merge
5. **Budget Cap** — Halts if dollar limit exceeded
6. **TQM Escalation** — Circuit escalates when confidence is low

## Development

```bash
# Enable pre-commit hooks (fmt + clippy)
git config core.hooksPath .githooks

# Common tasks
make test       # cargo test --workspace
make lint       # cargo clippy --all-targets -- -D warnings
make fmt        # cargo fmt
make coverage   # cargo tarpaulin (90% minimum)
make ci         # all of the above
```

Rust edition 2024, workspace resolver 2. All code must pass `clippy -D warnings` and `cargo fmt --check`.

## Design Principles

- **Local-first** — Runs on your machine, no cloud infra required
- **Provider-agnostic** — LLM calls go through the Router, never directly to a provider
- **Type-level governance** — `ApprovedAction<T>` can't be constructed outside the governance module
- **Deterministic orchestration** — Controller logic is explicit, not ML
- **Graceful degradation** — Parse failures produce fallback outputs, never panics
- **Sessions are ephemeral, state is not** — Agent sessions may die; all state persists in git

## License

MIT — [adjective-rob](https://github.com/adjective-rob) / Adjective LLC

See [LICENSE](LICENSE) for details.
