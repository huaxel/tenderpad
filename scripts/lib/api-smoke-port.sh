#!/usr/bin/env bash
# Helpers for scripts/rust-api-smoke.sh, kept in their own file so the guards
# below can be tested without building or starting the API.
#
# Why they exist: hosts commonly keep long-lived services on high ports
# (llama.cpp answers GET /health with a bare {"status":"ok"}). The smoke script
# used to treat "some HTTP response arrived" as "our API is up", so a port
# collision surfaced as an unrelated jq assertion far away from the cause.

# A port is available when nothing accepts a connection on it.
port_is_available() {
  ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

# The API's health payload always reports this key, so a foreign service that
# happens to answer /health cannot be mistaken for ours.
health_is_ours() {
  jq -e 'has("tender_database_readable")' >/dev/null 2>&1 <<<"${1:-}"
}

# Returns $1 when it is free, otherwise the first free port among a bounded
# number of random high ports. Prints nothing and fails when none is free.
choose_port() {
  local candidate=$1 tries
  for ((tries = 0; tries < 50; tries++)); do
    if port_is_available "$candidate"; then
      printf '%s' "$candidate"
      return 0
    fi
    candidate=$(( 20000 + RANDOM % 20000 ))
  done
  return 1
}
