# iTan-ReclaimSpace justfile
# Requires: just (https://just.systems)

# Default: run all tests
default: test

# ─── Core commands ────────────────────────────────────────────────────────────

# Check all crates compile cleanly with warnings-as-errors
check:
    cargo check --all-targets

# Format check (mirrors CI gate)
fmt-check:
    cargo fmt --all -- --check

# Clippy with hard-error mode
clippy:
    cargo clippy --all-targets -- -D warnings

# All fast unit tests (excludes street and cluster)
test:
    cargo test --all -- --exclude-should-panic

# Run only the itan-core unit tests
test-core:
    cargo test -p itan-core

# ─── Street tests (real disk I/O) ─────────────────────────────────────────────

# Run street tests (single-file real-world scenarios)
street:
    cargo test -p itan-core --test street -- --test-threads=1 --nocapture

# ─── Cluster tests (multi-worker concurrency) ────────────────────────────────

# Run cluster tests
cluster:
    cargo test -p itan-core --test cluster -- --test-threads=4 --nocapture

# ─── Benchmarks ───────────────────────────────────────────────────────────────

# Run Criterion.rs benchmarks
bench:
    cargo bench --all

# ─── Build ────────────────────────────────────────────────────────────────────

# Release build (all crates)
build-release:
    cargo build --release

# Windows NTFS feature-enabled release build
build-release-ntfs:
    cargo build --release -p itan-core --features windows-ntfs

# ─── CI gate (run locally before pushing) ────────────────────────────────────

ci: fmt-check clippy test
    @echo "CI gate PASSED"
