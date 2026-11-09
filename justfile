# Tenderpad Portfolio Showcase Justfile
# Local-only, history-free, reproducible development & verification gate.

export RUSTUP_TOOLCHAIN := "1.88.0"

default:
    @just --list

# Complete deterministic CI gate
ci: safety-check fmt-check test api-smoke dashboard-test
    @echo "\n🎉 ALL SHOWCASE GATES PASSED DETERMINISTICALLY."

# One-stop runnable showcase demonstration
demo: api-fixtures
    @echo "\n=== 1. Ranking open tenders for the fixture company (CLI) ==="
    cargo run --manifest-path rust/Cargo.toml --bin recommend -- \
      0404.484.060 --database target/fixtures/tenders.duckdb \
      --kbo-database target/fixtures/kbo.duckdb --limit 5
    @echo "\n=== 2. Running API & Dashboard Smoke Verification ==="
    just api-smoke
    @echo "\n=== 3. Ready to explore ==="
    @echo "To start the interactive web dashboard on loopback, run:"
    @echo "  just serve"
    @echo "Then open: http://127.0.0.1:18000 in your browser."

# Start local loopback API server and serve dashboard
serve host='127.0.0.1' port='18000' database='target/fixtures/tenders.duckdb' kbo='target/fixtures/kbo.duckdb': api-fixtures
    cargo run --manifest-path rust/Cargo.toml --bin api -- \
      --address {{host}}:{{port}} --tender-database {{database}} --kbo-database {{kbo}}

# Run Rust unit and integration tests
test:
    cargo test --manifest-path rust/Cargo.toml --lib

# Check Rust code formatting
fmt-check:
    cargo fmt --manifest-path rust/Cargo.toml -- --check

# Build the fixture DuckDB databases (KBO and Tenders). Inlined rather than
# delegated to a script: this recipe used to call scripts/generate-fixtures.sh,
# which does not exist in the product and is not published, so `just demo` and
# `just ci` failed at their first step in the published tree.
api-fixtures:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target/fixtures
    rm -f target/fixtures/kbo.duckdb target/fixtures/tenders.duckdb
    cargo run --manifest-path rust/Cargo.toml --quiet --bin kbo-rebuild -- --csv-dir tests/fixtures/kbo --output target/fixtures/kbo.duckdb --force >/dev/null
    # The overlay ships the real Bruges notice, and api-smoke expects its lot
    # (LOT-0001) and CPV (72210000). The product's synthetic notice is CPV
    # 33600000, so ingesting that one makes the smoke test fail.
    cargo run --manifest-path rust/Cargo.toml --quiet --bin bda-ingest -- tests/fixtures/bda/brugge-notice.xml --database target/fixtures/tenders.duckdb >/dev/null

# End-to-end API smoke test against synthetic fixtures
api-smoke: api-fixtures
    #!/usr/bin/env bash
    set -euo pipefail
    port=$(( 20000 + RANDOM % 20000 ))
    # The published fixture notice is the real Bruges one, so the smoke test
    # expects its lot and CPV instead of the product fixture's defaults.
    env -u TYPESAFE_API_KEY scripts/rust-api-smoke.sh target/fixtures/tenders.duckdb 0404.484.060 "$port" target/fixtures/kbo.duckdb LOT-0001 72210000

# Run dashboard regression test suite (Node 22+)
dashboard-test:
    node --test scripts/dashboard-bootstrap.test.cjs scripts/dashboard-workspace.test.cjs scripts/dashboard-example.test.cjs scripts/dashboard-landing.test.cjs

# Deterministic safety, secret, and publication allowlist check
safety-check:
    node tests/safety_check.cjs

