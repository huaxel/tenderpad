#!/usr/bin/env bash
set -euo pipefail

database=${1:-data/tenders-rust.duckdb}
enterprise=${2:-0404.484.060}
port=${3:-3301}
kbo_database=${4:-}
# The fixture notice the database was built from decides which lot and CPV the
# recommendation must carry. Defaults describe tests/fixtures/bda/notice.xml;
# a caller that ingests a different notice passes its own pair.
expected_lot=${5:-LOT-0001}
expected_cpv=${6:-33600000}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=lib/api-smoke-port.sh
source "$here/lib/api-smoke-port.sh"
for tool in curl jq; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "rust-api-smoke: '$tool' is required" >&2
    exit 1
  fi
done
# Cargo's own variable decides where the binaries are (CI keeps the target
# directory outside the checkout so `actions/checkout` cannot wipe the
# incremental build); relative values resolve from here, as cargo resolves
# them.
rust_api="${CARGO_TARGET_DIR:-rust/target}/debug/api"
log=$(mktemp)
# A scratch directory, not `mktemp ...-XXXXXXXX.duckdb`: `mktemp` only expands
# TRAILING X's, so that template produced one fixed path. The first run created
# it literally and every later run failed with "File exists" — which is what two
# concurrent gates (they share TMPDIR) hit. The directory also guarantees the
# copy and its WAL are removed together.
scratch=$(mktemp -d "${TMPDIR:-/tmp}/rust-api-smoke-XXXXXXXX")
runtime_database="$scratch/runtime.duckdb"
pid=''
cleanup() {
  local exit_code=$?
  if [[ -n "$pid" ]]; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  # Keep the server log visible when the run fails so crashes and panics can
  # be diagnosed instead of vanishing with the temporary files.
  if [[ "$exit_code" -ne 0 && -f "$log" ]]; then
    echo "smoke test failed; last server output from $log:" >&2
    tail -n 20 "$log" >&2
  fi
  rm -f "$log"
  rm -rf "$scratch"
}
trap cleanup EXIT

if [[ -f "$database" ]]; then
  cp "$database" "$runtime_database"
else
  rm -f "$runtime_database"
fi
database=$runtime_database

# When the API token is set, every request carries it (health ignores it);
# the script additionally asserts that a bare request is rejected below.
if [[ -n "${AANBESTEDINGEN_API_TOKEN:-}" ]]; then
  curl() { command curl -H "Authorization: Bearer $AANBESTEDINGEN_API_TOKEN" "$@"; }
fi

cargo build --manifest-path rust/Cargo.toml --bin api --quiet
kbo_args=()
if [[ -n "$kbo_database" ]]; then
  kbo_args=(--kbo-database "$kbo_database")
fi

# Starts the API on a free port and waits until it answers with *its own* health
# payload. A requested port that is already in use (or answered by something
# else) is retried elsewhere instead of failing much later inside an unrelated
# assertion. Sets the globals `port`, `pid` and `health`.
start_api() {
  local attempt response
  for attempt in 1 2 3; do
    if ! port=$(choose_port "$port"); then
      echo "rust-api-smoke: no free port after 50 attempts" >&2
      exit 1
    fi
    : >"$log"
    "$rust_api" \
      --address "127.0.0.1:$port" --tender-database "$database" "${kbo_args[@]}" >"$log" 2>&1 &
    pid=$!
    response=''
    for _ in $(seq 1 100); do
      if kill -0 "$pid" 2>/dev/null \
        && response=$(curl -fsS 2>/dev/null "http://127.0.0.1:$port/health") \
        && kill -0 "$pid" 2>/dev/null; then
        break
      fi
      sleep 0.2
    done
    if health_is_ours "$response"; then
      health=$response
      return 0
    fi
    if [[ -n "$response" ]]; then
      echo "rust-api-smoke: 127.0.0.1:$port answered /health with a foreign payload; trying another port" >&2
    elif grep -q 'Address already in use' "$log" 2>/dev/null; then
      # The availability check and the bind are not atomic: another process can take
      # the port in between. Relocate rather than reporting "did not start".
      echo "rust-api-smoke: 127.0.0.1:$port was taken between the check and the bind; trying another port" >&2
    fi
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    pid=''
    port=$(( 20000 + RANDOM % 20000 ))
  done
  echo "rust-api-smoke: API did not start on a free port; last server output:" >&2
  tail -n 20 "$log" >&2 || true
  exit 1
}

start_api
printf '%s\n' "$health"
jq -e '.status == "ok" and .kbo_database_readable == true and has("tenders_last_fetched") and has("awards") and has("awards_last_fetched") and (.award_lots | type == "number") and (.award_lots_with_winner | type == "number") and .analysis_model == null and .analysis_timeout_seconds == null' <<<"$health" >/dev/null

# This test drives real recommendations, so an empty store cannot exercise it.
# Say that plainly instead of failing later inside a jq length assertion.
jq -e '(.tenders | type == "number") and .tenders > 0' <<<"$health" >/dev/null || {
  echo "the smoke test needs a seeded store: '$database' has no tenders." >&2
  echo "Pass one explicitly, e.g. just rust-api-smoke <populated.duckdb>." >&2
  exit 1
}

# What a company can be matched against today, and whether we can vouch for it:
# a store can be large and still offer nothing, which is how a stale deployment
# silently produced irrelevant recommendations.
jq -e '(.tenders_open_notices | type == "number") and (.tenders_open_procurements | type == "number") and (.tenders_open_procurements <= .tenders_open_notices) and (.tenders_freshness | . == "fresh" or . == "stale" or . == "unknown") and (.award_history_companies | type == "number")' <<<"$health" >/dev/null
# The Brussels inventories were imported but read by nothing until this endpoint
# existed; it must answer with the documented shape even when (as in this fixture)
# no inventory row covers the company.
inventory=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/inventory-awards")
jq -e '(.engagements | type == "array") and (.accounting_rows | type == "number") and (.note | type == "string")' <<<"$inventory" >/dev/null
# TED/BOSA records have a separate endpoint so inventory accounting semantics stay unchanged.
award_records=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/awards?limit=10")
jq -e '(.awards | type == "array") and (.note | type == "string")' <<<"$award_records" >/dev/null
award_records_limit_status=$(curl -sS -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$port/companies/$enterprise/awards?limit=0")
[[ "$award_records_limit_status" == "422" ]]

recommendations=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/recommendations?limit=1&candidates=100")
jq -e --arg lot "$expected_lot" --arg cpv "$expected_cpv" \
  'length == 1 and (.[0].lot_cpv_alignment.lots | any(.lot_id == $lot and .cpv_codes == [$cpv]))' \
  <<<"$recommendations" >/dev/null
source=$(jq -er '.[0].source' <<<"$recommendations")
external_id=$(jq -er '.[0].external_id' <<<"$recommendations")
printf '%s\n' "$external_id"
if [[ -n "${AANBESTEDINGEN_API_TOKEN:-}" ]]; then
  unauthenticated_status=$(command curl -sS -o /dev/null -w '%{http_code}' \
    "http://127.0.0.1:$port/companies/$enterprise/recommendations?limit=1&candidates=100")
  [[ "$unauthenticated_status" == "401" ]]
  unauthenticated_awards_status=$(command curl -sS -o /dev/null -w '%{http_code}' \
    "http://127.0.0.1:$port/companies/$enterprise/awards?limit=1")
  [[ "$unauthenticated_awards_status" == "401" ]]
fi
feedback_status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H 'content-type: application/json' \
  -d "{\"source\":\" $source \",\"external_id\":\" $external_id \",\"relevant\":true}" \
  "http://127.0.0.1:$port/companies/$enterprise/feedback")
[[ "$feedback_status" == "200" ]]
recommendations_after_feedback=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/recommendations?limit=100&candidates=100")
jq -e --arg source "$source" --arg external_id "$external_id" 'any(.[]; .source == $source and .external_id == $external_id and any(.reasons[]; contains("Previously marked relevant")))' <<<"$recommendations_after_feedback" >/dev/null
analysis=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/tenders/$source/$external_id/analysis")
jq -e '.provider == "deterministic-rust" and has("relevance") and has("prompt_version") and (.cached | type == "boolean")' <<<"$analysis" >/dev/null
legacy_analysis=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/tenders/$source/$external_id/jev")
jq -e '.provider == "deterministic-rust" and has("relevance")' <<<"$legacy_analysis" >/dev/null
cached_analysis=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/tenders/$source/$external_id/analysis")
jq -e '.provider == "deterministic-rust" and .cached == true' <<<"$cached_analysis" >/dev/null
health_after=$(curl -fsS "http://127.0.0.1:$port/health")
jq -e '.analysis_cache_entries >= 1 and .persistent_analysis_cache_entries >= 1 and .analysis_cache_memory_hits >= 1 and (.analysis_cache_misses + .analysis_cache_persistent_hits >= 1)' <<<"$health_after" >/dev/null
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
pid=''
# The restart proves the analysis cache is persistent, not in-memory state; a
# different free port is fine because the store path is unchanged.
start_api
health_restart=$health
restart_analysis=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/tenders/$source/$external_id/analysis")
jq -e '.provider == "deterministic-rust" and .cached == true' <<<"$restart_analysis" >/dev/null
health_restart_after=$(curl -fsS "http://127.0.0.1:$port/health")
jq -e '.analysis_cache_persistent_hits >= 1' <<<"$health_restart_after" >/dev/null
deep_recommendations=$(curl -fsS \
  "http://127.0.0.1:$port/companies/$enterprise/recommendations?limit=2&candidates=100&deep=2")
jq -e 'length >= 1 and (.[0].jev_relevance == "strong" or .[0].jev_relevance == "possible" or .[0].jev_relevance == "poor") and (.[0].jev_qualification_risk | type == "string") and any(.[0].reasons[]; startswith("Jev relevance: "))' <<<"$deep_recommendations" >/dev/null
invalid_deep_status=$(curl -sS -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$port/companies/$enterprise/recommendations?deep=11")
[[ "$invalid_deep_status" == "422" ]]
invalid_status=$(curl -sS -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$port/companies/$enterprise/recommendations?limit=0")
[[ "$invalid_status" == "422" ]]
invalid_body=$(curl -sS \
  "http://127.0.0.1:$port/companies/$enterprise/recommendations?limit=0")
jq -e 'has("detail")' <<<"$invalid_body" >/dev/null
invalid_candidates_status=$(curl -sS -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$port/companies/$enterprise/recommendations?candidates=0")
[[ "$invalid_candidates_status" == "422" ]]
invalid_preferences_status=$(curl -sS -o /dev/null -w '%{http_code}' -X PUT \
  -H 'content-type: application/json' \
  -d '{"minimum_value":"200","maximum_value":"100"}' \
  "http://127.0.0.1:$port/companies/$enterprise/preferences")
[[ "$invalid_preferences_status" == "422" ]]
invalid_enterprise_status=$(curl -sS -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$port/companies/not-an-enterprise")
[[ "$invalid_enterprise_status" == "422" ]]
invalid_feedback_status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H 'content-type: application/json' \
  -d '{"source":" ","external_id":"tender-1","relevant":true}' \
  "http://127.0.0.1:$port/companies/$enterprise/feedback")
[[ "$invalid_feedback_status" == "422" ]]
oversized_reason=$(printf '%70000s' '' | tr ' ' x)
oversized_feedback=$(jq -nc --arg reason "$oversized_reason" '{source:"ted",external_id:"tender-1",relevant:true,reason:$reason}')
unset oversized_reason
oversized_feedback_status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H 'content-type: application/json' \
  -d "$oversized_feedback" \
  "http://127.0.0.1:$port/companies/$enterprise/feedback")
[[ "$oversized_feedback_status" == "413" ]]
missing_feedback_status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H 'content-type: application/json' \
  -d '{"source":"ted","external_id":"missing-tender","relevant":true}' \
  "http://127.0.0.1:$port/companies/$enterprise/feedback")
[[ "$missing_feedback_status" == "404" ]]
unknown_company_feedback_status=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
  -H 'content-type: application/json' \
  -d "{\"source\":\"$source\",\"external_id\":\"$external_id\",\"relevant\":true}" \
  "http://127.0.0.1:$port/companies/0999.999.999/feedback")
[[ "$unknown_company_feedback_status" == "404" ]]
