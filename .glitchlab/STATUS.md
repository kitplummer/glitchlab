# GLITCHLAB Project Status

## Metrics

- **Rust LOC:** ~41,000
- **Crates:** 6 (kernel, router, memory, eng-org, dashboard, cli)
- **Agent Count:** 12 (Planner, Implementer, Debugger, Security, CISO, Release, Archivist, ArchitectTriage, ArchitectReview, OpsDiagnosis, AdrDecomposer, BacklogReview)
- **Pipeline Stage Count:** 15+
- **TQM Pattern Count:** 9
- **Test Count:** 1027
- **Coverage:** ~87% (target: 90%)

## What It Does

GLITCHLAB takes a development task (GitHub issue, local YAML, or interactive prompt), breaks it into an execution plan, implements the changes, runs tests, fixes failures, scans for security issues, and opens a PR — all orchestrated locally with deterministic control.

When things go wrong, the system detects anti-patterns (decomposition loops, stuck agents, budget pressure), diagnoses root causes via its ops agent, and generates remediation tasks that feed back into the queue. It's a self-repairing pipeline.
