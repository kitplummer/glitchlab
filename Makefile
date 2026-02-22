.PHONY: build check test coverage lint fmt clean

# Build all crates
build:
	cargo build

# Type-check without building
check:
	cargo check

# Run all tests
test:
	cargo test

# Run tests for a specific crate (usage: make test-crate CRATE=kernel)
test-crate:
	cargo test -p glitchlab-$(CRATE)

# Measure test coverage (excludes CLI binary)
coverage:
	cargo tarpaulin --exclude-files 'crates/cli/*' --skip-clean -o stdout

# Coverage with HTML report
coverage-html:
	cargo tarpaulin --exclude-files 'crates/cli/*' --skip-clean -o html
	@echo "Report: tarpaulin-report.html"

# Run clippy lints
lint:
	cargo clippy --all-targets -- -D warnings

# Format code
fmt:
	cargo fmt --all

# Check formatting without modifying
fmt-check:
	cargo fmt --all -- --check

# Clean build artifacts
clean:
	cargo clean

# Full CI check: fmt + lint + test + coverage
ci: fmt-check lint test coverage
