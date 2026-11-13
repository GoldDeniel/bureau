#!/usr/bin/env bash
# End-to-end smoke test on a throwaway database. No model calls: demo adapters only.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-./target/release/bureau}
TMPROOT=$(mktemp -d "${TMPDIR:-/tmp}/bureau-smoke-XXXXXX")
DB="$TMPROOT/bureau.db"
LOG="$TMPROOT/server.log"
MARKER="$TMPROOT/discovery-calls"
BODY="$TMPROOT/body.json"
SERVER_PID=""

fail() { echo "smoke: FAIL: $*" >&2; exit 1; }

stop_server() {
  if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  SERVER_PID=""
}

cleanup() {
  stop_server
  rm -rf "$TMPROOT"
}
trap cleanup EXIT

[ -x "$BIN" ] || fail "$BIN is missing; run cargo build --release first"
command -v curl >/dev/null || fail "curl is required"
command -v jq >/dev/null || fail "jq is required"

# Fixtures: a throwaway HOME keeps real opencode config untouched. The fake
# opencode records every discovery subprocess, so we can prove the board's
# page load and 4 s poll only read the cached result.
HOME_DIR="$TMPROOT/home"
mkdir -p "$HOME_DIR/.config/opencode/skills/smoke-skill"
cat >"$HOME_DIR/.config/opencode/skills/smoke-skill/SKILL.md" <<'MD'
---
name: smoke-skill
description: "Fixture skill for the smoke test"
---

body
MD
mkdir -p "$HOME_DIR/.config/opencode/profiles/smoke-profile"
printf '{"plugins":["smoke-plugin"]}\n' >"$HOME_DIR/.config/opencode/profiles/smoke-profile/opencode.json"
mkdir -p "$TMPROOT/bin"
cat >"$TMPROOT/bin/opencode" <<'SH'
#!/bin/sh
echo call >>"$SMOKE_DISCOVERY_MARKER"
exit 1
SH
chmod +x "$TMPROOT/bin/opencode"

start_server() {
  : >"$MARKER"
  : >"$LOG"
  HOME="$HOME_DIR" \
  BUREAU_PROFILES_DIR="$HOME_DIR/.config/opencode/profiles" \
  BUREAU_TOKEN="" \
  PATH="$TMPROOT/bin:/usr/bin:/bin" \
  SMOKE_DISCOVERY_MARKER="$MARKER" \
    "$BIN" --db "$DB" serve --bind 127.0.0.1:0 --no-dispatch >"$LOG" 2>&1 &
  SERVER_PID=$!
  local i
  for i in $(seq 1 100); do
    if grep -q 'http://127\.0\.0\.1:[0-9]' "$LOG"; then
      BASE=$(grep -o 'http://127\.0\.0\.1:[0-9]*' "$LOG" | head -1)
      if curl -fsS "$BASE/api/health" >/dev/null 2>&1; then return; fi
    fi
    kill -0 "$SERVER_PID" 2>/dev/null || { cat "$LOG" >&2; fail "server exited during startup"; }
    sleep 0.1
  done
  cat "$LOG" >&2
  fail "server did not become healthy"
}

# call METHOD PATH [JSON] -> HTTP status in $CODE, response in $BODY
call() {
  local method=$1 path=$2 data=${3:-}
  if [ -n "$data" ]; then
    CODE=$(curl -sS -o "$BODY" -w '%{http_code}' -X "$method" \
      -H 'content-type: application/json' --data "$data" "$BASE$path") \
      || fail "$method $path failed to connect"
  else
    CODE=$(curl -sS -o "$BODY" -w '%{http_code}' -X "$method" "$BASE$path") \
      || fail "$method $path failed to connect"
  fi
}

json_is() {
  local msg=$1
  shift
  jq -e "$@" "$BODY" >/dev/null || fail "$msg: $(cat "$BODY")"
}

assert_page() {
  local html=$1
  for needle in \
    'Bureau' \
    'id="agent-drawer"' \
    'Skills &amp; plugins' \
    'Plugins' \
    'renderAgentDrawer' \
    '/api/discovery/refresh' \
    '/toggles' \
    'plugin_profile' \
    'warmTries' \
    'discovery is still loading' \
    'setInterval(() => { if (!document.hidden) load(); }, 4000)' \
  ; do
    grep -Fq "$needle" "$html" || fail "board page is missing: $needle"
  done
}

# ---------------------------------------------------------------- delegation
$BIN --db "$DB" init >/dev/null
REF=$($BIN --db "$DB" ticket new "Smoke test: delegation loop" -d bureau \
        -b "Exercise manager triage and worker execution." --json \
      | grep -o '"ref": *"[^"]*"' | head -1 | cut -d'"' -f4)

$BIN --db "$DB" tick --max-parallel 10 >/dev/null
$BIN --db "$DB" tick --max-parallel 10 >/dev/null

$BIN --db "$DB" ticket show "$REF"
$BIN --db "$DB" ticket show "$REF" | grep -q "\[review\]"
$BIN --db "$DB" ticket show "$REF" | grep -q "sub-tickets:"
echo "smoke: $REF is in review"

# ---------------------------------------------------------------- board page
start_server

curl -fsS "$BASE/" >"$TMPROOT/index.html"
assert_page "$TMPROOT/index.html"
curl -fsS "$BASE/api/state" >"$TMPROOT/state.json"
jq -e '.agents | length > 0' "$TMPROOT/state.json" >/dev/null || fail "state has no agents"

if [ -s "$MARKER" ]; then
  fail "page load or the 4 s poll triggered discovery ($(wc -l <"$MARKER") subprocess call(s))"
fi
echo "smoke: board serves the toggles markup; page load and poll stay on cached reads"

# The drawer's GET is the first thing allowed to warm discovery. Every seeded
# agent must answer it, and all but the first call must hit the cache.
curl -fsS "$BASE/api/agents" >"$TMPROOT/agents.json"
SLUGS=$(jq -r '.agents[].slug' "$TMPROOT/agents.json")
for slug in $SLUGS; do
  call GET "/api/agents/$slug"
  [ "$CODE" = 200 ] || fail "drawer GET /api/agents/$slug returned $CODE: $(cat "$BODY")"
  json_is "GET /api/agents/$slug has no toggles" '.toggles | type == "object"'
  json_is "GET /api/agents/$slug returned another agent" --arg slug "$slug" '.agent.slug == $slug'
done
for i in $(seq 1 50); do
  [ "$(wc -l <"$MARKER" 2>/dev/null || echo 0)" -ge 1 ] && break
  sleep 0.1
done
CALLS=$(wc -l <"$MARKER" 2>/dev/null || echo 0)
[ "$CALLS" -eq 1 ] || fail "expected exactly one discovery subprocess for all drawer reads, got $CALLS"

# The first uncached GET warms discovery in the background. Wait for the
# fixture before the positive round-trip; an empty set is tolerated below.
for i in $(seq 1 50); do
  call GET "/api/agents/spark"
  [ "$CODE" = 200 ] || fail "GET /api/agents/spark returned $CODE"
  if jq -e '((.toggles.skills | length) + (.toggles.plugins | length)) > 0' "$BODY" >/dev/null; then
    break
  fi
  sleep 0.1
done

# ---------------------------------------------------------------- toggles
json_is "seeded spark is not inherit on first read" '.toggles.mode == "inherit"'

KIND=skills
NAME=$(jq -r '.toggles.skills[0].name // empty' "$BODY")
if [ -z "$NAME" ]; then
  KIND=plugins
  NAME=$(jq -r '.toggles.plugins[0].name // empty' "$BODY")
fi

if [ -n "$NAME" ]; then
  SUBSET=$(jq -nc --arg kind "$KIND" --arg name "$NAME" \
    '{mode: "explicit", skills: [], plugins: []} | .[$kind] = [$name]')

  call PUT "/api/agents/spark/toggles" "$SUBSET"
  [ "$CODE" = 200 ] || fail "PUT toggles returned $CODE: $(cat "$BODY")"
  echo "smoke: PUT explicit $KIND=$NAME returned 200"
  json_is "PUT did not echo the explicit subset" --arg kind "$KIND" --arg name "$NAME" \
    ".toggles.mode == \"explicit\" and (.toggles[\$kind] | map(.name)) == [\$name]"

  call PATCH "/api/agents/spark/toggles" "$SUBSET"
  [ "$CODE" = 200 ] || fail "PATCH toggles returned $CODE: $(cat "$BODY")"
  echo "smoke: PATCH explicit $KIND=$NAME returned 200"
  json_is "PATCH did not echo the explicit subset" --arg kind "$KIND" --arg name "$NAME" \
    ".toggles.mode == \"explicit\" and (.toggles[\$kind] | map(.name)) == [\$name]"

  call GET "/api/agents/spark"
  [ "$CODE" = 200 ] || fail "GET after save returned $CODE"
  json_is "saved subset did not survive a re-read" --arg kind "$KIND" --arg name "$NAME" \
    ".toggles.mode == \"explicit\" and (.toggles[\$kind] | map(.name)) == [\$name]"
else
  echo "smoke: discovery is empty; skipping the positive toggles round-trip"
fi

# Never skipped: an unknown skill is rejected with the valid list.
call PUT "/api/agents/spark/toggles" \
  '{"mode":"explicit","skills":["definitely-not-a-skill"],"plugins":[]}'
[ "$CODE" = 400 ] || fail "unknown skill returned $CODE, expected 400: $(cat "$BODY")"
json_is "unknown skill 400 did not carry error plus valid list" \
  '(.error // "" | test("unknown skill")) and (.valid | type == "array")'
echo "smoke: unknown skill rejected with 400 and the valid list"

# The rejection must not overwrite the previous selection.
call GET "/api/agents/spark"
if [ -n "$NAME" ]; then
  json_is "a rejected selection changed stored toggles" --arg kind "$KIND" --arg name "$NAME" \
    ".toggles.mode == \"explicit\" and (.toggles[\$kind] | map(.name)) == [\$name]"
fi

# Persistence across a restart on the same database file.
stop_server
start_server
call GET "/api/agents/spark"
[ "$CODE" = 200 ] || fail "GET after restart returned $CODE"
if [ -n "$NAME" ]; then
  json_is "saved subset did not survive a server restart" --arg kind "$KIND" --arg name "$NAME" \
    ".toggles.mode == \"explicit\" and (.toggles[\$kind] | map(.name)) == [\$name]"
  echo "smoke: saved subset survived a restart"
fi
call PUT "/api/agents/spark/toggles" \
  '{"mode":"explicit","skills":["definitely-not-a-skill"],"plugins":[]}'
[ "$CODE" = 400 ] || fail "unknown skill after restart returned $CODE, expected 400"
echo "smoke: unknown skill still rejected with 400 after restart"

# ---------------------------------------------------------------- plugin_profile
call PATCH "/api/agents/spark" '{"plugin_profile":"smoke-profile"}'
[ "$CODE" = 200 ] || fail "PATCH plugin_profile returned $CODE: $(cat "$BODY")"
json_is "PATCH plugin_profile did not stick" '.agent.plugin_profile == "smoke-profile"'
call GET "/api/agents"
[ "$CODE" = 200 ] || fail "GET /api/agents returned $CODE"
json_is "plugin_profiles omits the fixture profile" '.plugin_profiles | index("smoke-profile") != null'
json_is "agent list lost plugin_profile" \
  '.agents[] | select(.slug == "spark") | .plugin_profile == "smoke-profile"'
call GET "/api/state"
[ "$CODE" = 200 ] || fail "GET /api/state returned $CODE"
json_is "board snapshot lost plugin_profile" \
  '.agents[] | select(.slug == "spark") | .plugin_profile == "smoke-profile"'
echo "smoke: old plugin_profile field still works end to end"

echo "smoke: all checks passed ✨"
