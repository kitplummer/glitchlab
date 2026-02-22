# GLITCHLAB Self-Dogfooding Status

## Current State (2026-02-22)

The pipeline is functionally complete and has been tested against itself.
All fixes are committed and pushed to `main`.

## What Works

- Full 15-stage pipeline executes end-to-end with real Anthropic API calls
- Tool-use loop (implementer/debugger agents read/write files via tools)
- Anthropic content block serialization (tool_use, tool_result)
- `apply_changes` handles dual write paths (tool-written + JSON output)
- Auto-format (`cargo fmt`) runs before commit stage
- 529 overloaded errors are retried (5 attempts, 5s default backoff)
- Pipeline timeout (default 3600s)
- History recording to composite backend
- `make dogfood` target for easy self-dogfooding runs

## What Needs Testing

- **A successful end-to-end self-dogfooding run has not yet completed.**
  - Closest: attempt #4 got through all 15 stages (tests passed, security,
    archive) but failed at commit due to `cargo fmt` — now fixed.
  - Subsequent attempts hit API rate limiting (429) and overloading (529).
  - The 529 retry fix is in place but untested against the real API.

## Next Steps

1. **Run `make dogfood` from a terminal with `ANTHROPIC_API_KEY` set.**
   If it succeeds, GLITCHLAB will have built itself.

2. **If it fails**, likely causes and fixes:
   - Budget exceeded → bump `max_tokens_per_task` in `.glitchlab/config.yaml`
   - Rate limiting → wait and retry (the pipeline now handles 429/529)
   - `apply_changes` error → check if another dual-path edge case

3. **After first successful run**, consider:
   - Running on a non-trivial objective (not just `version` subcommand)
   - Adding `make dogfood` to CI as a smoke test
   - Tracking cost/token metrics across runs

## Key Files Modified This Session

| File | Change |
|------|--------|
| `crates/router/src/provider/anthropic.rs` | Content block serialization, 529 retry |
| `crates/router/src/provider/openai.rs` | 529 retry |
| `crates/router/src/router.rs` | 5 retries (up from 3) |
| `crates/eng-org/src/pipeline.rs` | Timeout, tail-truncation, auto-fmt, apply_changes skip |
| `crates/eng-org/src/config.rs` | `max_pipeline_duration_secs` |
| `crates/kernel/src/pipeline.rs` | `PipelineStatus::TimedOut` |
| `.glitchlab/config.yaml` | Protected paths, limits |
| `Makefile` | `make dogfood` target |

## Test Coverage

338+ tests pass. Clippy clean. Coverage >=90%.
