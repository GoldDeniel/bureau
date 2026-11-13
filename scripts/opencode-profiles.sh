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
#   scripts/opencode-profiles.sh --agent <slug> --skills a,b --plugins x,y
#
# The --agent form writes profiles/generated/<slug>/ only. It symlinks the
# selected skills from the user's skill directory, so the originals stay
# byte-identical, and marks the generated dir as inherited. It never writes
# into a user-authored profile directory.
set -euo pipefail

PROFILE_ROOT=${PROFILE_ROOT:-$HOME/.config/opencode/profiles}
SKILLS_DIR=${SKILLS_DIR:-$HOME/.config/opencode/skills}
GENERATED_ROOT="$PROFILE_ROOT/generated"

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

die() { echo "$*" >&2; exit 1; }

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

# Comma-separated names -> compact JSON array. Empty means no names.
json_list() {
  command -v jq >/dev/null || die "jq is required to generate an agent profile"
  if [ -z "$1" ]; then
    echo '[]'
  else
    printf '%s' "$1" | jq -Rc 'split(",") | map(select(length > 0))'
  fi
}

# Explicit per-agent selection. Writes only under profiles/generated/<slug>/.
generate_agent() {
  local slug=$1 skills_csv=$2 plugins_csv=$3
  [[ "$slug" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || die "invalid agent slug '$slug'"
  local base="$GENERATED_ROOT/$slug"
  local dir="$base/opencode"
  local skills_json plugins_json name

  skills_json=$(json_list "$skills_csv")
  plugins_json=$(json_list "$plugins_csv")

  # Never write into a directory we did not generate.
  if [ -e "$base" ] && [ ! -f "$base/generated.json" ]; then
    die "$base exists but is not a generated profile; refusing to write there"
  fi
  mkdir -p "$dir"

  # Skills: symlink the originals. The user's files stay byte-identical; the
  # generated.json marker labels the whole set as inherited.
  rm -rf "$dir/skills"
  if [ "$(echo "$skills_json" | jq 'length')" -gt 0 ]; then
    mkdir -p "$dir/skills"
    while IFS= read -r name; do
      case "$name" in
        ""|*/*|*..*) die "invalid skill name '$name'" ;;
      esac
      [ -f "$SKILLS_DIR/$name/SKILL.md" ] || die "skill '$name' not found under $SKILLS_DIR"
      ln -s "$SKILLS_DIR/$name" "$dir/skills/$name"
    done < <(echo "$skills_json" | jq -r '.[]')
  fi

  # Same PROVIDERS fragment as every hand-written profile: array keys replace,
  # so a missing fragment would silently drop the provider.
  jq -n \
    --argjson plugins "$plugins_json" \
    --argjson providers "$PROVIDERS" \
    --arg slug "$slug" \
    '{ "$schema": "https://opencode.ai/config.json",
       generated_for: $slug,
       inherited_skills: true,
       plugins: $plugins,
       providers: $providers }' > "$dir/opencode.json"

  jq -n --arg slug "$slug" --argjson skills "$skills_json" --argjson plugins "$plugins_json" \
    '{ generated_for: $slug, inherited_skills: true, skills: $skills, plugins: $plugins }' \
    > "$base/generated.json"
  echo "wrote $dir/opencode.json"
}

# Parse every generated profile with opencode itself, so --check still proves
# the file is valid and carries exactly the selected plugin set.
check_generated() {
  local rc=0 base name dir expected got
  [ -d "$GENERATED_ROOT" ] || return 0
  for base in "$GENERATED_ROOT"/*; do
    [ -f "$base/generated.json" ] || continue
    name=$(basename "$base")
    dir="$base/opencode"
    expected=$(jq -c '.plugins' "$base/generated.json") || { rc=1; continue; }
    echo "== generated/$name"
    got=$(XDG_CONFIG_HOME="$base" opencode debug config --standalone \
      | jq -c '[.[] | select(.type == "document") | .info.plugins][0] // []') || { rc=1; continue; }
    if [ "$got" != "$expected" ]; then
      echo "generated/$name: plugins $got, expected $expected" >&2
      rc=1
      continue
    fi
    echo "generated/$name: plugins $got"
  done
  return "$rc"
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
  check_generated || rc=1
  return "$rc"
}

case "${1:-}" in
  "") for name in "${PROFILE_NAMES[@]}"; do generate "$name"; done ;;
  --check) check ;;
  --agent)
    slug=${2:-}
    shift 2 || die "usage: $0 --agent <slug> [--skills a,b] [--plugins x,y]"
    skills=""
    plugins=""
    while [ $# -gt 0 ]; do
      case $1 in
        --skills) skills=${2:-}; shift 2 ;;
        --plugins) plugins=${2:-}; shift 2 ;;
        *) die "unknown option '$1' (expected --skills or --plugins)" ;;
      esac
    done
    generate_agent "$slug" "$skills" "$plugins"
    ;;
  *) die "usage: $0 [--check | --agent <slug> --skills a,b --plugins x,y]" ;;
esac
