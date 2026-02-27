.PHONY: build build-release check test test-crate test-functional coverage \
       coverage-html lint fmt fmt-check clean ci \
       dogfood run batch canary canary-dry-run init status history version \
       hooks

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

# Build all crates (debug)
build:
	cargo build

# Build all crates (release)
build-release:
	cargo build --release

# Type-check without building
check:
	cargo check

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

# Run all tests
test:
	cargo test --workspace

# Run tests for a specific crate (usage: make test-crate CRATE=kernel)
test-crate:
	cargo test -p glitchlab-$(CRATE)

# Run functional/E2E test suite (CLI binary + mock server + history verification)
test-functional:
	cargo test -p glitchlab-cli --test e2e
	cargo test -p glitchlab-eng-org --test memory_integration

# ---------------------------------------------------------------------------
# Coverage
# ---------------------------------------------------------------------------

# Measure test coverage (excludes CLI binary and Dolt backend)
coverage:
	cargo tarpaulin --exclude-files 'crates/cli/*' 'crates/memory/src/dolt.rs' --skip-clean -o stdout

# Coverage with HTML report
coverage-html:
	cargo tarpaulin --exclude-files 'crates/cli/*' 'crates/memory/src/dolt.rs' --skip-clean -o html
	@echo "Report: tarpaulin-report.html"

# ---------------------------------------------------------------------------
# Lint & Format
# ---------------------------------------------------------------------------

# Run clippy lints
lint:
	cargo clippy --all-targets -- -D warnings

# Format code
fmt:
	cargo fmt --all

# Check formatting without modifying
fmt-check:
	cargo fmt --all -- --check

# Full CI check: fmt + lint + test + coverage
ci: fmt-check lint test coverage

# ---------------------------------------------------------------------------
# Clean
# ---------------------------------------------------------------------------

clean:
	cargo clean

# ---------------------------------------------------------------------------
# Git hooks
# ---------------------------------------------------------------------------

# Enable pre-commit hooks (fmt + clippy gate)
hooks:
	git config core.hooksPath .githooks
	@echo "Pre-commit hooks enabled (.githooks/)"

# ---------------------------------------------------------------------------
# GLITCHLAB CLI — single task
# ---------------------------------------------------------------------------

# Run a single task from an objective string
# Usage: make run OBJ="Add Display impl for Foo"
# Usage: make run REPO=~/other-project OBJ="Fix the bug"
REPO ?= $(CURDIR)
OBJ ?= Add a 'version' subcommand to the CLI that prints the crate version.
run: build-release
	./target/release/glitchlab run --repo $(REPO) --auto-approve --objective "$(OBJ)"

# Self-dogfooding run (alias for run with RUST_LOG)
dogfood: build-release
	RUST_LOG=info ./target/release/glitchlab run --repo $(CURDIR) --auto-approve --objective "$(OBJ)"

# Interactive mode
# Usage: make interactive
# Usage: make interactive REPO=~/other-project
interactive: build-release
	./target/release/glitchlab interactive --repo $(REPO) --auto-approve

# ---------------------------------------------------------------------------
# GLITCHLAB CLI — batch
# ---------------------------------------------------------------------------

# Run the default backlog
# Usage: make batch BUDGET=50.0
# Usage: make batch BUDGET=20.0 TASKS=.glitchlab/tasks/backlog.yaml
BUDGET ?= 10.0
TASKS ?= .glitchlab/tasks/backlog.yaml
batch: build-release
	./target/release/glitchlab batch --repo $(REPO) --budget $(BUDGET) \
		--tasks-file $(TASKS) --auto-approve -v

# Dry-run a batch (show tasks without executing)
batch-dry-run: build-release
	./target/release/glitchlab batch --repo $(REPO) --budget $(BUDGET) \
		--tasks-file $(TASKS) --dry-run

# ---------------------------------------------------------------------------
# GLITCHLAB CLI — canary
# ---------------------------------------------------------------------------

CANARY_TASKS ?= .glitchlab/tasks/canary-sizing.yaml
CANARY_BUDGET ?= 10.0

# Run the canary sizing batch (S/M/L/XL test tasks)
canary: build-release
	./target/release/glitchlab batch --repo $(CURDIR) --budget $(CANARY_BUDGET) \
		--tasks-file $(CANARY_TASKS) --auto-approve -v

# Dry-run canary (preview tasks)
canary-dry-run: build-release
	./target/release/glitchlab batch --repo $(CURDIR) --budget $(CANARY_BUDGET) \
		--tasks-file $(CANARY_TASKS) --dry-run

# ---------------------------------------------------------------------------
# GLITCHLAB CLI — utilities
# ---------------------------------------------------------------------------

# Initialize GLITCHLAB in a repository
# Usage: make init REPO=~/your-project
init: build-release
	./target/release/glitchlab init $(REPO)

# Check configuration, API keys, and tools
# Usage: make status
# Usage: make status REPO=~/other-project
status: build-release
	./target/release/glitchlab status --repo $(REPO)

# View task history
# Usage: make history
# Usage: make history COUNT=20
# Usage: make history REPO=~/other-project
COUNT ?= 10
history: build-release
	./target/release/glitchlab history --repo $(REPO) --count $(COUNT)

# View task history with aggregate statistics
history-stats: build-release
	./target/release/glitchlab history --repo $(REPO) --stats

# Print version
version: build-release
	./target/release/glitchlab version
