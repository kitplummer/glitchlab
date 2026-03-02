# Runbook: LowEndInsight (GLITCHLAB) — Manual Deployment

**Version:** 1.0
**Audience:** Engineers, SREs
**Last Updated:** 2026-03-02

---

## Overview

This runbook documents the manual steps required to deploy GLITCHLAB (internally referred to as LowEndInsight) from source. GLITCHLAB is a local-first, multi-agent development engine written in Rust. It runs on the operator's machine and requires no cloud infrastructure beyond LLM API credentials.

---

## Prerequisites

### System Requirements

| Requirement | Minimum Version | Notes |
|-------------|----------------|-------|
| Rust toolchain | 1.85+ (edition 2024) | Install via [rustup](https://rustup.rs/) |
| Git | 2.28+ | Required for worktree support |
| Docker + Docker Compose | Docker 24+ / Compose v2 | Required for Dolt database |
| `bd` (Beads CLI) | latest | Required for bead persistence; install separately |

### Required API Keys

At least one LLM provider credential is required. Gemini is the default for most agents.

| Environment Variable | Provider | Required? |
|---------------------|---------|-----------|
| `GEMINI_API_KEY` | Google Gemini | Required (default provider) |
| `ANTHROPIC_API_KEY` | Anthropic Claude | Optional (used for Debugger by default) |
| `OPENAI_API_KEY` | OpenAI / Ollama-compatible | Optional |
| `OPENAI_API_BASE` | OpenAI-compatible base URL | Optional (set for Ollama/local models) |

---

## Step 1: Clone the Repository

```bash
git clone https://github.com/adjective-llc/glitchlab.git
cd glitchlab
```

Enable the pre-commit hooks (enforces `cargo fmt` and `cargo clippy` before each commit):

```bash
git config core.hooksPath .githooks
```

---

## Step 2: Build the Application

Build all workspace crates in release mode:

```bash
cargo build --release
```

The compiled binaries are placed in `target/release/`:

| Binary | Description |
|--------|-------------|
| `target/release/glitchlab` | Main CLI — `run`, `batch`, `status`, `plan` |
| `target/release/glitchlab-dashboard` | SSE-based live monitoring dashboard |

> **Note:** The first build will take several minutes due to dependency compilation. Subsequent builds are incremental.

Optionally, install the binary into your PATH:

```bash
cargo install --path crates/cli
```

---

## Step 3: Configure API Keys

Export the required credentials in your shell (or add to `~/.bashrc` / `~/.zshrc`):

```bash
export GEMINI_API_KEY="your-gemini-api-key"
export ANTHROPIC_API_KEY="sk-ant-your-anthropic-key"
# Optional: OpenAI or Ollama-compatible endpoint
export OPENAI_API_KEY="your-openai-key"
export OPENAI_API_BASE="http://localhost:11434/v1/chat/completions"  # Ollama example
```

### Using a Local Model (Ollama)

1. Install Ollama and pull a tool-use-capable model:
   ```bash
   ollama pull llama3
   ```
2. Set the OpenAI-compatible endpoint:
   ```bash
   export OPENAI_API_BASE="http://localhost:11434/v1/chat/completions"
   export OPENAI_API_KEY="ollama"  # Any non-empty string
   ```

---

## Step 4: Start the Database (Dolt)

GLITCHLAB uses [Dolt](https://github.com/dolthub/dolt) as its SQL persistence backend. Start it via Docker Compose:

```bash
docker compose up -d
```

This starts the `glitchlab-dolt` container:
- **Image:** `dolthub/dolt-sql-server:latest`
- **Port:** `3306` (MySQL-compatible)
- **Volume:** `dolt-data` (persistent across restarts)
- **Credentials:** user `glitchlab`, password `glitchlab`
- **Database:** `glitchlab` (auto-initialized on first start)

Verify the database is running:

```bash
docker compose ps
# Expected: glitchlab-dolt  running  0.0.0.0:3306->3306/tcp
```

To stop the database:

```bash
docker compose down
```

To stop and destroy all data (destructive — use with caution):

```bash
docker compose down -v
```

---

## Step 5: Initialize a Target Repository

Before running tasks, initialize GLITCHLAB for the target project repository:

```bash
glitchlab init ~/path/to/your-project
```

This creates a `.glitchlab/` directory inside the target repo with:
- `config.yaml` — per-repo configuration overrides
- `tasks.yaml` — local task backlog (if using YAML task source)
- `history/` — JSONL task history (fallback when Dolt is unavailable)

---

## Step 6: Configure Per-Repo Settings (Optional)

Edit `.glitchlab/config.yaml` in the target repository to override defaults:

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

Key configuration options:

| Key | Default | Description |
|-----|---------|-------------|
| `limits.max_dollars_per_task` | `0.50` | Hard cap per task in USD |
| `limits.max_fix_attempts` | `3` | Max debugger retry attempts |
| `limits.repair_budget_fraction` | `0.20` | Fraction of batch budget reserved for TQM repair |
| `boundaries.protected_paths` | `[]` | Paths the implementer cannot modify |
| `tqm.remediation_enabled` | `true` | Enable self-repair loop |

---

## Step 7: Run Tasks

### Single Task — from GitHub Issue

```bash
glitchlab run --repo ~/your-project --issue 42
```

### Single Task — from Local YAML

Create a task in `.glitchlab/tasks.yaml`:

```yaml
tasks:
  - id: my-task-001
    title: "Fix the login bug"
    description: "Users cannot log in when session cookie is expired."
    priority: 1
```

Then run:

```bash
glitchlab run --repo ~/your-project --local-task
```

### Batch Run — Process Entire Backlog

```bash
glitchlab batch --repo ~/your-project --budget 50.0
```

The `--budget` flag sets the maximum spend in USD for the entire batch. The orchestrator will stop when budget is exhausted or the backlog is empty.

### Check Status

```bash
glitchlab status --repo ~/your-project
```

---

## Step 8: Run with Docker Compose (Ops Mode)

For headless / containerized operation, use `compose-ops.yaml`:

```bash
docker compose -f compose-ops.yaml up
```

This starts both Dolt and the GLITCHLAB binary. The `glitchlab` service mounts the current directory and reads API keys from the host environment.

> **Note:** Ensure the release binary exists at `target/release/glitchlab` before starting in ops mode — the container does not build from source.

---

## Step 9: Monitor a Batch Run

During a batch run, start the dashboard in a separate terminal:

```bash
glitchlab-dashboard
```

The dashboard listens on `http://localhost:3000` by default and provides a live SSE feed of task progress, agent outputs, and budget consumption.

---

## Verification

After deployment, verify the system is operational:

1. **Database connectivity:**
   ```bash
   mysql -h 127.0.0.1 -P 3306 -u glitchlab -pglitchlab glitchlab -e "SHOW TABLES;"
   ```

2. **Binary version:**
   ```bash
   glitchlab --version
   ```

3. **Smoke test — single S-sized task:**
   ```bash
   glitchlab run --repo ~/your-project --issue 1
   # Watch for: Plan → Triage → Implement → Test → PR opened
   ```

4. **Confirm bead persistence:**
   ```bash
   bd list --limit 10
   # Should show tasks created during the run
   ```

---

## Troubleshooting

### Dolt fails to start

```
Error: Cannot connect to database on port 3306
```

- Check Docker is running: `docker ps`
- Check port conflicts: `lsof -i :3306`
- Inspect logs: `docker compose logs dolt`

### Build fails with linker errors

Ensure you have the system C linker and OpenSSL dev headers:

```bash
# Debian/Ubuntu
sudo apt-get install build-essential pkg-config libssl-dev

# macOS
xcode-select --install
brew install openssl
```

### "Worktree already in use" error during batch

This occurs when a previous batch run exited uncleanly and left a worktree checked out on `main`. Clean up stale worktrees:

```bash
cd ~/your-project
git worktree list
git worktree remove --force .glitchlab/worktrees/<stale-worktree-name>
```

### Coverage drops below 90%

Run the coverage check manually and review the gap report:

```bash
make coverage
# Excluded: crates/cli/*, crates/dashboard/*, crates/memory/src/dolt.rs
```

Uncovered lines in `claude_code.rs` (process-spawning functions) are expected; they require a command-runner trait injection to test and are tracked as a known gap.

### Agent returns empty `result` field

This occurs when the implementer exhausts its tool-call turns. The fallback uses `detect_changes_from_worktree()` to recover changes from the git worktree. If the worktree is also empty, the task will be marked `error` and retried.

---

## Rollback

GLITCHLAB does not modify the host system beyond:
- Git worktrees created under `.glitchlab/worktrees/` in the target repo
- PRs opened against the target repo
- Rows written to the Dolt database

To roll back a deployment:

1. Close any open PRs created during the run.
2. Remove stale worktrees: `git worktree prune` in the target repo.
3. Stop and optionally wipe Dolt: `docker compose down -v`
4. Remove the `.glitchlab/` directory from the target repo if re-initializing from scratch.

---

## Reference

| Command | Description |
|---------|-------------|
| `cargo build --release` | Build all binaries |
| `make test` | `cargo test --workspace` |
| `make lint` | `cargo clippy --all-targets -- -D warnings` |
| `make fmt` | `cargo fmt` |
| `make coverage` | `cargo tarpaulin` (90% minimum) |
| `make ci` | `fmt + lint + test + coverage` |
| `docker compose up -d` | Start Dolt database |
| `docker compose down` | Stop Dolt database |
| `glitchlab run` | Run a single task |
| `glitchlab batch` | Run entire backlog with budget cap |
| `glitchlab status` | Show current run status |
| `glitchlab-dashboard` | Start live monitoring dashboard |
