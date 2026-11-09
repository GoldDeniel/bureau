#!/usr/bin/env bash
# End-to-end smoke test on a throwaway database. No model calls: demo adapters only.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-./target/release/bureau}
DB=$(mktemp -u /tmp/bureau-smoke-XXXXXX.db)
trap 'rm -f "$DB" "$DB"-wal "$DB"-shm' EXIT

$BIN --db "$DB" init >/dev/null
REF=$($BIN --db "$DB" ticket new "Smoke test: delegation loop" -d bureau \
        -b "Exercise manager triage and worker execution." --json \
      | grep -o '"ref": *"[^"]*"' | head -1 | cut -d'"' -f4)

$BIN --db "$DB" tick --max-parallel 10 >/dev/null
$BIN --db "$DB" tick --max-parallel 10 >/dev/null

$BIN --db "$DB" ticket show "$REF"
$BIN --db "$DB" ticket show "$REF" | grep -q "\[review\]"
$BIN --db "$DB" ticket show "$REF" | grep -q "sub-tickets:"
echo "smoke: $REF is in review ✨"
