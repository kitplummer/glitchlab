# GLITCHLAB

**The Agentic Dev Engine — Build Weird. Ship Clean.**

A local-first, repo-agnostic, multi-agent development engine that evolves codebases under strict governance. Forked from [adjective-rob/glitchlab](https://github.com/adjective-rob/glitchlab) and rewritten in Rust.

## What It Does

GLITCHLAB takes a development task (GitHub issue, local YAML, or interactive prompt), breaks it into an execution plan, implements the changes, runs tests, fixes failures, scans for security issues, and opens a PR — all orchestrated locally with deterministic control.

When things go wrong, the system detects anti-patterns (decomposition loops, stuck agents, budget pressure), diagnoses root causes via its ops agent, and generates remediation tasks that feed back into the queue. It's a self-repairing pipeline.

## Agent Roster

Ten agents, each with a persona and a job:

| Agent | Persona | Role | Default Model |
|-------|---------|------|---------------|
| Planner | Professor Zap | Decompose objectives into execution plans | gemini-2.5-flash |
| Implementer | Patch | Write code and tests (tool-use loop, 9 turns) | gemini-2.5-pro |
| Debugger | Reroute | Fix failing tests and builds | claude-sonnet-4 |
| Security | Firewall Frankie | Code-level vulnerability scanning | gemini-2.5-flash |
| CISO | Sentinel | Strategic risk analysis: blast radius, trust boundaries, compliance | gemini-2.5-flash |
| Release | Semver Sam | Determine semantic version bump | gemini-2.5-flash |
| Archivist | Nova | Generate ADRs and documentation | gemini-2.5-flash |
| Architect (Triage) | Blueprint | Pre-impl: check if work is already done | gemini-2.5-flash |
| Architect (Review) | Blueprint | Post-impl: review diff before merge | gemini-2.5-flash |
| Ops Diagnosis | Circuit | Diagnose TQM patterns, generate remediation | gemini-2.5-flash-lite |

Model assignments are configurable per-repo. The `ModelChooser` selects models based on tier, capabilities, and cost.

## Tool-Calling Models

The `Implementer` agent relies on tool-calling (function-calling) capabilities from the underlying model. Not all models support this feature, and performance varies. Here is a list of known-compatible models:

| Provider  | Model Families Supporting Tool Use | Notes |
|-----------|------------------------------------|-------|
| Google    | Gemini 1.5 Pro, 1.5 Flash, 1.0 Pro | Gemini 1.5 Pro is recommended for complex tasks. |
| Anthropic | Claude 3 family (Opus, Sonnet, Haiku) | All Claude 3 models have strong tool-use capabilities. |
| OpenAI    | GPT-4, GPT-4o, GPT-3.5 Turbo | Newer models generally have better JSON formatting for tool arguments. |
| Ollama    | Llama 3, other fine-tunes | Requires a model fine-tuned for function calling. `llama3` (8b and 70b) has shown good results. |

When configuring the `Implementer` agent, ensure you select a model from this list for reliable performance.

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
export OPENAI_API_KEY="..." # Can be a placeholder for Ollama/other compatible endpoints
```

#### Using Ollama with a Local Model

To use a local model via Ollama, ensure Ollama is running and you have pulled a model that supports OpenAI-compatible function calling (e.g., Llama 3).

1.  **Pull a model:**
    ```bash
    ollama pull llama3
    ```

2.  **Set the API base and a dummy API key:**
    GLITCHLAB uses the `OPENAI_API_BASE` environment variable to connect to OpenAI-compatible endpoints.
    ```bash
    export OPENAI_API_BASE="http://localhost:11434/v1/chat/completions"
    export OPENAI_API_KEY="ollama" # The key can be any non-empty string
    ```
    > **Note:** You must provide the full chat completions endpoint URL to `OPENAI_API_BASE`.

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

## What Is a "Task"?

A **task** in GLITCHLAB is the complete lifecycle of turning an objective into delivered code. It is not just implementation — it is the full flow:

```
Objective → Plan → Triage → [Decompose] → Implement → Test → Debug → Review → Deliver
```

Every task passes through these phases:

| Phase | What Happens | Token Budget Share |
|-------|-------------|-------------------|
| **Plan** | Planner decomposes objective into steps, identifies files, assesses complexity | ~5% |
| **Triage** | Architect checks if work exists, assigns t-shirt size (S/M/L), gates XL tasks | ~3% |
| **Decompose** | If needed: split into sub-tasks, each a full task with its own lifecycle | (parent ends here) |
| **Implement** | Tool-use loop: read files, write code, run commands (bulk of the budget) | ~65% |
| **Test + Debug** | Run test suite, fix failures (up to N attempts via debugger agent) | ~15% |
| **Review** | Security scan, CISO risk analysis, architect diff review, release assessment, documentation | ~10% |
| **Deliver** | Commit, open PR, auto-merge (configurable) | ~2% |

For a **Medium** task (the most common size), this budget is approximately **60K tokens across 9 tool turns** for the implementer, plus overhead for the surrounding agents. A typical M task consumes **$0.10–$0.30** end to end.

When a task is **decomposed**, the parent task ends at the Decompose phase and spawns child tasks. Each child is a full task with its own Plan→Deliver lifecycle. Decomposition adds overhead (workspace setup, context assembly, planner calls per sub-task), so it is only triggered when:
- Complexity is medium or large
- The task touches **4+ files AND requires 5+ steps**
- No single implementer pass could complete it within 9 tool turns

### Task Sizes

| Size | Token Budget | Tool Turns | Typical Scope |
|------|-------------|------------|---------------|
| **S** | 20K | 4 | 1 file, ≤2 edits, mechanical change |
| **M** | 60K | 9 | 1–3 files, requires understanding |
| **L** | 90K | 12 | 2–3 files, requires design decisions |
| **XL** | — | — | Must decompose into S/M/L sub-tasks |

## Pipeline Stages

The engineering pipeline runs 15+ stages per task:

1. **Task pickup** — Priority queue with remediation-first scheduling
2. **Architect triage** — Skip work that's already done, assign size
3. **Workspace creation** — Isolated git worktree per task
4. **Boundary check** — Protected-path enforcement (kernel, .github, etc.)
5. **Planning** — LLM decomposes the objective into steps
6. **Decomposition guard** — Strip spurious decomposition from small/medium tasks
7. **Implementation** — Tool-use loop: write code, read files, run commands
8. **Test execution** — Run the project's test suite
9. **Debug loop** — Up to N attempts to fix failures
10. **Security review** — Scan for vulnerabilities before PR
11. **CISO risk analysis** — Strategic risk: blast radius, trust boundaries, compliance
12. **Architect review** — Diff review with approval/rejection
13. **Release assessment** — Semantic version bump determination
14. **Documentation** — ADR generation
15. **Commit + PR** — Open PR with structured description
16. **Auto-merge** — Merge on approval (configurable)
17. **TQM analysis** — Detect anti-patterns, feed back into queue

When a task fails, the pipeline captures structured **OutcomeContext** (what approach was tried, what obstacle was hit, what was discovered). This context is fed into the AttemptTracker so that retries start with knowledge of prior failures instead of repeating blind.

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
              Security → CISO → Release → Archivist
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
├── eng-org/      Engineering org: 10 agents, workspace (git worktree), repo indexer,
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

| Run Type | Typical Cost | Notes |
|----------|-------------|-------|
| Single task — S (rename, config tweak) | $0.02 - $0.10 | 1 agent pass, minimal overhead |
| Single task — M (bug fix, add function) | $0.10 - $0.30 | Full lifecycle: plan → deliver |
| Single task — L (new module, refactor) | $0.20 - $0.50 | Design decisions + review |
| Decomposed task (XL split into 3 sub-tasks) | $0.40 - $1.50 | Parent + 3 child lifecycles |
| Batch run (30 tasks, self-improvement) | $10 - $20 | Includes TQM + remediation |
| Full batch with repair budget | $15 - $100 | 20% budget reserved for repair |

Budget governance is enforced at every stage. The `CumulativeBudget` tracker splits spending into feature work and repair allocations (default 80/20 split). Tasks that exceed their budget are halted, not retried.

## Human Intervention Points

GLITCHLAB is autonomous between checkpoints, but you stay in control:

1. **Plan Review** — Approve before implementation begins
2. **Core Boundary** — Protected paths block unauthorized changes
3. **Fix Loop** — Halts after N failed attempts
4. **CISO Escalation** — Sentinel flags high-risk changes for human security review
5. **Architect Review** — Diff review before merge
6. **Budget Cap** — Halts if dollar limit exceeded
7. **TQM Escalation** — Circuit escalates when confidence is low

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
