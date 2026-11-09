#!/usr/bin/env bash
set -euo pipefail

DEST_DIR="${1:-target/fixtures}"
KBO_DB="$DEST_DIR/kbo.duckdb"
TENDERS_DB="$DEST_DIR/tenders.duckdb"

mkdir -p "$DEST_DIR"
rm -f "$KBO_DB" "$TENDERS_DB" "$KBO_DB.wal" "$TENDERS_DB.wal"

echo "Building KBO fixture DuckDB from tests/fixtures/kbo..."
RUSTUP_TOOLCHAIN=1.88.0 cargo run --manifest-path rust/Cargo.toml --bin kbo-rebuild -- \
  --csv-dir tests/fixtures/kbo --output "$KBO_DB" --force

echo "Building Tenders fixture DuckDB from tests/fixtures/bda/brugge-notice.xml..."
RUSTUP_TOOLCHAIN=1.88.0 cargo run --manifest-path rust/Cargo.toml --bin bda-ingest -- \
  tests/fixtures/bda/brugge-notice.xml --database "$TENDERS_DB"

echo "Fixture databases created successfully in $DEST_DIR."
