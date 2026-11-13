#!/usr/bin/env bash
# Generate per-agent opencode profiles from the user's CURRENT global config
# without ever modifying that config.
#
# Single source: ~/.config/opencode/opencode.json (read only). Every generated
# profile is complete: the provider block and every other top-level key are
# copied from the global file, only "plugins" is replaced. opencode replaces
# array keys instead of merging them, so a partial file would silently drop
# providers.
#
# Path mapping: opencode appends "opencode" to XDG_CONFIG_HOME and reads
# opencode.json there. A profile named <name> is therefore written to
#   <profiles>/generated/<name>/opencode/opencode.json
# and used with
#   XDG_CONFIG_HOME=$HOME/.config/opencode/profiles/generated/<name>
# The ticket text wrote profiles/generated/<name>/opencode.json; the extra
# "opencode/" level is what makes the XDG mapping resolve unchanged.
#
# The profile named "inherit" is never written. It means "use the global config
# as-is": the board lists it, and the dispatcher sets no XDG_CONFIG_HOME at all.
#
# Safety:
#   * dry-run by default; --apply is required to write
#   * writes only under <profiles>/generated/ (unless --profile-root names
#     another root explicitly)
#   * every generated dir carries .bureau-generated.json with the generator
#     version and the sha256 of each emitted file. If a target exists and does
#     not match that fingerprint, the WHOLE run is refused and the file named.
#
# Usage:
#   scripts/opencode-profiles.sh                 # dry run: show planned writes
#   scripts/opencode-profiles.sh --apply         # write all named profiles
#   scripts/opencode-profiles.sh --check         # verify generated profiles
#   scripts/opencode-profiles.sh --agent <slug> --skills a,b --plugins x,y
#   scripts/opencode-profiles.sh --profile-root DIR --global-config FILE
set -euo pipefail

GENERATOR_VERSION="bureau-opencode-profiles/2"
OPENCODE_HOME=${OPENCODE_HOME:-$HOME/.config/opencode}
GLOBAL_CONFIG=${GLOBAL_CONFIG:-$OPENCODE_HOME/opencode.json}
PROFILE_ROOT=${PROFILE_ROOT:-$OPENCODE_HOME/profiles}
SKILLS_DIR=${SKILLS_DIR:-$OPENCODE_HOME/skills}
GENERATED_ROOT="$PROFILE_ROOT/generated"
MARKER=".bureau-generated.json"

APPLY=0
CHECK=0

die() { echo "$*" >&2; exit 1; }

require_tools() {
  command -v jq >/dev/null || die "jq is required"
  command -v sha256sum >/dev/null || die "sha256sum is required"
}

hash_file() { sha256sum "$1" | cut -d' ' -f1; }

# Discovery indexes ~/.agents/skills as well as the profile skill dir, so a
# selected catalog entry must resolve from either root.
skill_source() {
  local name=$1 root
  for root in "$SKILLS_DIR" "$HOME/.agents/skills"; do
    if [ -f "$root/$name/SKILL.md" ]; then
      echo "$root/$name"
      return 0
    fi
  done
  return 1
}

# One JSON object {"generator":..., "files":{rel:sha256}} for the given files.
fingerprint() {
  local base=$1; shift
  local rel
  {
    for rel in "$@"; do
      jq -nc --arg path "$rel" --arg sha "$(hash_file "$base/$rel")" \
        '{path: $path, sha256: $sha}'
    done
  } | jq -s --arg generator "$GENERATOR_VERSION" \
        '{generator: $generator,
          files: (map({key: .path, value: .sha256}) | from_entries)}'
}

# Comma-separated names -> compact JSON array. Empty means no names.
json_list() {
  if [ -z "$1" ]; then
    echo '[]'
  else
    printf '%s' "$1" | jq -Rc 'split(",") | map(select(length > 0))'
  fi
}

# The global config with "plugins" replaced. Everything else is copied as-is.
build_config() {
  local plugins_json=$1
  jq --argjson plugins "$plugins_json" '.plugins = $plugins' "$GLOBAL_CONFIG"
}

# Named profiles. "inherit" is deliberately absent: it is not a directory.
PROFILE_NAMES=(worker-lite worker-full manager-bare)

plugins_for() {
  case $1 in
    # The global plugin list, exactly as configured.
    worker-full) jq -c '.plugins // []' "$GLOBAL_CONFIG" ;;
    # The global list narrowed to ponytail, the cheapest worker bundle.
    worker-lite) jq -c '[.plugins[]? | select(test("ponytail"; "i"))]' "$GLOBAL_CONFIG" ;;
    # No plugins at all.
    manager-bare) echo '[]' ;;
    *) die "unknown profile '$1'" ;;
  esac
}

# Plan a named profile into $WORK/<name>/ and echo its base directory.
plan_named() {
  local name=$1 base="$WORK/$1" plugins
  plugins=$(plugins_for "$name")
  mkdir -p "$base/opencode"
  build_config "$plugins" > "$base/opencode/opencode.json"
  fingerprint "$base" opencode/opencode.json > "$base/$MARKER"
  echo "$base"
}

# Plan the explicit per-agent selection adapters.rs asks for.
plan_agent() {
  local slug=$1 skills_csv=$2 plugins_csv=$3
  [[ "$slug" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || die "invalid agent slug '$slug'"
  local base="$WORK/$slug"
  local skills_json plugins_json name
  skills_json=$(json_list "$skills_csv")
  plugins_json=$(json_list "$plugins_csv")

  mkdir -p "$base/opencode"
  build_config "$plugins_json" > "$base/opencode/opencode.json"
  jq -n --arg slug "$slug" --argjson skills "$skills_json" --argjson plugins "$plugins_json" \
    '{ generated_for: $slug, inherited_skills: true, skills: $skills, plugins: $plugins }' \
    > "$base/generated.json"
  fingerprint "$base" opencode/opencode.json generated.json > "$base/$MARKER"
  echo "$base"
}

# Refuse the whole run when a target was edited after generation.
guard() {
  local base=$1 dir rel recorded actual
  dir="$GENERATED_ROOT/$(basename "$base")"
  case "$dir" in
    "$GENERATED_ROOT"/*) ;;
    *) die "refusing to write outside $GENERATED_ROOT: $dir" ;;
  esac
  local -a targets=()
  [ -e "$dir/opencode/opencode.json" ] && targets+=("opencode/opencode.json")
  [ -e "$dir/generated.json" ] && targets+=("generated.json")
  [ "${#targets[@]}" -gt 0 ] || return 0

  if [ ! -f "$dir/$MARKER" ]; then
    echo "$dir/${targets[0]}: exists but has no $MARKER fingerprint; not written by this generator" >&2
    return 1
  fi
  for rel in "${targets[@]}"; do
    recorded=$(jq -r --arg p "$rel" '.files[$p] // empty' "$dir/$MARKER")
    actual=$(hash_file "$dir/$rel")
    if [ -z "$recorded" ]; then
      echo "$dir/$rel: is not covered by $dir/$MARKER" >&2
      return 1
    fi
    if [ "$recorded" != "$actual" ]; then
      echo "$dir/$rel: changed after generation (sha256 $actual, recorded $recorded); refusing to overwrite a hand-edited profile" >&2
      return 1
    fi
  done
}

# Copy one planned profile, printing each path. Byte-identical files stay put.
install() {
  local base=$1 name dir rel source target
  name=$(basename "$base")
  dir="$GENERATED_ROOT/$name"
  for rel in opencode/opencode.json generated.json "$MARKER"; do
    source="$base/$rel"
    target="$dir/$rel"
    [ -f "$source" ] || continue
    if [ -f "$target" ] && cmp -s "$source" "$target"; then
      echo "unchanged $target"
    elif [ "$APPLY" = 1 ]; then
      mkdir -p "$(dirname "$target")"
      cp "$source" "$target"
      echo "wrote     $target"
    else
      echo "would write $target"
    fi
  done

  # Skills are symlinks to the user's originals: never copied, never changed.
  if [ -f "$base/skills.list" ]; then
    [ "$APPLY" = 1 ] && rm -rf "$dir/opencode/skills"
    while IFS= read -r skill; do
      [ -n "$skill" ] || continue
      case "$skill" in
        */*|*..*) die "invalid skill name '$skill'" ;;
      esac
      source=$(skill_source "$skill") || die "skill '$skill' not found under $SKILLS_DIR or $HOME/.agents/skills"
      if [ "$APPLY" = 1 ]; then
        mkdir -p "$dir/opencode/skills"
        rm -rf "$dir/opencode/skills/$skill"
        ln -s "$source" "$dir/opencode/skills/$skill"
        echo "linked    $dir/opencode/skills/$skill"
      else
        echo "would link $dir/opencode/skills/$skill"
      fi
    done < "$base/skills.list"
  fi
}

plan_agent_skills() {
  local base=$1 skills_json=$2
  : > "$base/skills.list"
  local name
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    case "$name" in
      */*|*..*) die "invalid skill name '$name'" ;;
    esac
    skill_source "$name" >/dev/null || die "skill '$name' not found under $SKILLS_DIR or $HOME/.agents/skills"
    echo "$name" >> "$base/skills.list"
  done < <(echo "$skills_json" | jq -r '.[]')
}

# Static check: valid JSON, hashes match, providers identical to the global file.
check() {
  local rc=0 base name expected actual
  expected=$(jq -c '.providers // .provider // null' "$GLOBAL_CONFIG")
  [ -d "$GENERATED_ROOT" ] || { echo "check: no $GENERATED_ROOT"; return 0; }
  for base in "$GENERATED_ROOT"/*; do
    [ -f "$base/$MARKER" ] || continue
    name=$(basename "$base")
    if ! jq -e . "$base/opencode/opencode.json" >/dev/null 2>&1; then
      echo "generated/$name: opencode.json is not valid JSON" >&2
      rc=1
      continue
    fi
    actual=$(jq -c '.providers // .provider // null' "$base/opencode/opencode.json")
    if [ "$actual" != "$expected" ]; then
      echo "generated/$name: providers differ from $GLOBAL_CONFIG" >&2
      rc=1
      continue
    fi
    if ! ( cd "$base" && jq -r '.files | to_entries[] | "\(.value)  \(.key)"' "$MARKER" \
           | sha256sum --check --quiet - ); then
      echo "generated/$name: file hashes no longer match $MARKER" >&2
      rc=1
      continue
    fi
    echo "generated/$name: ok"
  done
  return "$rc"
}

usage() {
  cat <<'TXT'
Generate opencode profiles from the global config, without touching it.

  --apply              write the files (default is a dry run)
  --check              verify generated profiles and their fingerprints
  --agent <slug>       generate one explicit agent profile
      --skills a,b     skills to symlink (with --agent)
      --plugins x,y    plugins to enable (with --agent)
  --profile-root DIR   write under DIR/generated (default ~/.config/opencode/profiles)
  --global-config FILE read FILE as the single source (default ~/.config/opencode/opencode.json)

Profiles land in profiles/generated/<name>/opencode/opencode.json, used as
XDG_CONFIG_HOME=<profiles>/generated/<name>. "inherit" is a board-selectable
profile meaning the global config as-is; it writes no file and sets no env.
TXT
}

main() {
  local mode="" slug="" skills="" plugins=""
  while [ $# -gt 0 ]; do
    case $1 in
      --apply) APPLY=1 ;;
      --check) CHECK=1 ;;
      --agent) mode=agent; slug=${2:-}; shift ;;
      --skills) skills=${2:-}; shift ;;
      --plugins) plugins=${2:-}; shift ;;
      --profile-root) PROFILE_ROOT=${2:-}; shift ;;
      --global-config) GLOBAL_CONFIG=${2:-}; shift ;;
      -h|--help) usage; exit 0 ;;
      *) die "unknown option '$1' (try --help)" ;;
    esac
    shift
  done

  require_tools
  [ -f "$GLOBAL_CONFIG" ] || die "global config not found: $GLOBAL_CONFIG"
  GENERATED_ROOT="$PROFILE_ROOT/generated"

  if [ "$CHECK" = 1 ]; then
    check
    return
  fi

  WORK=$(mktemp -d "${TMPDIR:-/tmp}/bureau-profiles-XXXXXX")
  trap 'rm -rf "$WORK"' EXIT

  local -a planned=()
  if [ "$mode" = agent ]; then
    [ -n "$slug" ] || die "usage: $0 --agent <slug> [--skills a,b] [--plugins x,y]"
    local base
    base=$(plan_agent "$slug" "$skills" "$plugins")
    plan_agent_skills "$base" "$(json_list "$skills")"
    planned+=("$base")
  else
    local name
    for name in "${PROFILE_NAMES[@]}"; do
      planned+=("$(plan_named "$name")")
    done
  fi

  # Preflight every target before writing a single byte: a refusal must leave
  # the whole tree untouched.
  local base
  for base in "${planned[@]}"; do
    guard "$base" || die "refused: no files were written"
  done

  for base in "${planned[@]}"; do
    install "$base"
  done

  if [ "$APPLY" = 1 ]; then
    echo "applied ${#planned[@]} profile(s) under $GENERATED_ROOT"
  else
    echo "dry run: nothing written; re-run with --apply"
  fi
}

main "$@"
