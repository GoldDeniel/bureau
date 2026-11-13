#!/usr/bin/env bash
# Generate one complete opencode config per worker profile.
#
# Why: a worker inherits the global plugin set unless its config directory
# contains a complete opencode.json. opencode resolves that directory from
# XDG_CONFIG_HOME and appends "opencode", so a profile is
#   profiles/<name>/opencode/opencode.json
# and is used with XDG_CONFIG_HOME=$HOME/.config/opencode/profiles/<name>.
# Array keys replace instead of merging across layers, so a partial file
# silently drops providers. Every profile is therefore built from the same
# PROVIDERS fragment below.
#
# Usage:
#   scripts/opencode-profiles.sh           # write all three profiles
#   scripts/opencode-profiles.sh --check   # parse each profile, fail if bad
set -euo pipefail

PROFILE_ROOT=${PROFILE_ROOT:-$HOME/.config/opencode/profiles}

# One source of truth for provider wiring. Secrets stay as {env:...}/{file:...}
# references; never put a literal key here.
PROVIDERS=$(cat <<'JSON'
{
    "freellmapi": {
      "name": "FreeLLMAPI (local router)",
      "env": [
        "FREELLMAPI_KEY"
      ],
      "package": "@opencode/ai/providers/openai-compatible",
      "settings": {
        "baseURL": "http://127.0.0.1:3001/v1",
        "apiKey": "{file:~/.hermes/secrets/freellmapi.key}"
      },
      "models": {
        "glm-5.3:free": {
          "name": "GLM 5.3 (free)",
          "limit": {
            "context": 1000000,
            "output": 32768
          }
        },
        "glm-5.3-flash:free": {
          "name": "GLM 5.3 Flash (free)",
          "limit": {
            "context": 1000000,
            "output": 32768
          }
        },
        "qwen/qwen3.8-max:free": {
          "name": "Qwen3.8 Max (free)",
          "limit": {
            "context": 1000000,
            "output": 32768
          }
        },
        "qwen3.8-flash": {
          "name": "Qwen3.8 Flash",
          "limit": {
            "context": 1000000,
            "output": 32768
          }
        },
        "gemini-3.8-flash": {
          "name": "Gemini 3.8 Flash",
          "limit": {
            "context": 1048576,
            "output": 32768
          }
        },
        "gpt-5.6-luna": {
          "name": "GPT-5.6 Luna",
          "limit": {
            "context": 1000000,
            "output": 32768
          }
        }
      }
    }
  }
JSON
)

PONYTAIL='@dietrichgebert/ponytail'
SUPERPOWERS='superpowers@git+https://github.com/obra/superpowers.git'
CAVEMAN='@mumme-it/opencode-caveman'

PROFILE_NAMES=(worker-lite worker-full manager-bare)
PLUGINS_WORKER_LITE="[\"$PONYTAIL\"]"
PLUGINS_WORKER_FULL="[\"$PONYTAIL\", \"$SUPERPOWERS\", \"$CAVEMAN\"]"
PLUGINS_MANAGER_BARE='[]'

plugins_for() {
  case $1 in
    worker-lite)   echo "$PLUGINS_WORKER_LITE" ;;
    worker-full)   echo "$PLUGINS_WORKER_FULL" ;;
    manager-bare)  echo "$PLUGINS_MANAGER_BARE" ;;
  esac
}

generate() {
  local name=$1 dir="$PROFILE_ROOT/$1/opencode" plugins
  plugins=$(plugins_for "$name")
  mkdir -p "$dir"
  cat > "$dir/opencode.json" <<JSON
{
  "\$schema": "https://opencode.ai/config.json",
  "plugins": $plugins,
  "providers": $PROVIDERS
}
JSON
  echo "wrote $dir/opencode.json"
}

check() {
  command -v jq >/dev/null || { echo "check: jq not found" >&2; return 1; }
  local name base dir expected got rc=0
  for name in "${PROFILE_NAMES[@]}"; do
    base="$PROFILE_ROOT/$name"
    dir="$base/opencode"
    expected=$(plugins_for "$name" | jq -c .)
    if [ ! -f "$dir/opencode.json" ]; then
      echo "$name: missing $dir/opencode.json (run without --check)" >&2
      rc=1
      continue
    fi
    echo "== $name"
    # debug config talks to a running server; --standalone parses this profile.
    XDG_CONFIG_HOME="$base" opencode debug paths | grep '^config'
    got=$(XDG_CONFIG_HOME="$base" opencode debug config --standalone \
      | jq -c '[.[] | select(.type == "document") | .info.plugins][0] // []') || { rc=1; continue; }
    if [ "$got" != "$expected" ]; then
      echo "$name: plugins $got, expected $expected" >&2
      rc=1
      continue
    fi
    echo "$name: plugins $got"
  done
  return "$rc"
}

if [ "${1:-}" = "--check" ]; then
  check
else
  for name in "${PROFILE_NAMES[@]}"; do generate "$name"; done
fi
