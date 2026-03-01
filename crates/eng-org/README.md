# glitchlab-eng-org

The engineering organization crate for GLITCHLAB. Contains the agent roster, pipeline stages, orchestrator, TQM self-repair system, and all supporting infrastructure for autonomous software development.

## Contents

- **Agents** (`src/agents/`) — 12 stateless agents: Planner, Implementer, Debugger, Security, CISO, Release, Archivist, ArchitectTriage, ArchitectReview, OpsDiagnosis, AdrDecomposer, BacklogReview.
- **Pipeline** (`src/pipeline.rs`) — 15+ stage engineering pipeline: index, plan, triage, implement, test, debug, security, CISO, review, release, archive, commit, PR, merge, TQM.
- **Orchestrator** (`src/orchestrator.rs`) — Task queue processing with cumulative budget tracking, attempt tracking, and restart intensity monitoring.
- **TQM** (`src/tqm.rs`) — Task Queue Manager detecting 9 anti-pattern types and generating remediation tasks.
- **Config** (`src/config.rs`) — YAML-based per-repo configuration loading.
- **Workspace** (`src/workspace.rs`) — Git worktree creation and cleanup for task isolation.
- **Indexer** (`src/indexer.rs`) — Repository structure indexing and codebase knowledge generation.
- **Tools** (`src/tools.rs`) — Tool definitions and execution: read_file, write_file, edit_file, list_files, run_command.
