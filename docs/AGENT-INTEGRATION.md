# Integrating agents with Bureau

Bureau's HTTP API is the integration surface. Agents never touch SQLite.

## External worker loop

```bash
BASE=http://127.0.0.1:8787
DIV=example-game
ME=forge

# 1. find work
curl -s "$BASE/api/tickets?status=queued&division=$DIV"

# 2. claim it (atomic, 1 hour lease; a second claim fails cleanly)
curl -s -X POST "$BASE/api/tickets/BUR-7/claim" \
  -H 'content-type: application/json' -d "{\"agent\":\"$ME\"}"

# 3. do the work in the directory from GET /api/divisions
curl -s "$BASE/api/divisions" | jq -r ".divisions[] | select(.slug==\"$DIV\") | .project_path"

# 4. report
curl -s -X POST "$BASE/api/tickets/BUR-7/comment" \
  -H 'content-type: application/json' \
  -d "{\"author\":\"agent:$ME\",\"body\":\"Changed src/save.gd; tests pass.\"}"

curl -s -X PATCH "$BASE/api/tickets/BUR-7" \
  -H 'content-type: application/json' -d '{"status":"review"}'
```

If the process dies after claiming, the lease expires and the ticket returns to
`queued` (after three attempts it becomes `failed` instead of looping).

## Manager contract

Managers are ordinary agents with `role = manager`. They receive a prompt with
the ticket, the division charter and the worker list, and must reply with one
JSON object:

```json
{"action":"delegate","summary":"...","subtasks":[
  {"title":"...","body":"...","priority":2,"agent":"forge"}
]}
```

or `{"action":"answer","body":"..."}` or
`{"action":"blocked","reason":"..."}`.

The parser tolerates prose and code fences around the object. If no JSON object
is found the ticket is marked `blocked` and the raw output is attached, so a
misbehaving model never silently disappears.

## Rules for a worker prompt

A worker cannot see the parent ticket, the conversation, or other sub-tickets.
Its sub-ticket body must therefore contain: the goal, the paths involved, and
the acceptance criteria. Keep them small enough to finish in one run.

## Skill and plugin toggles

Relevance is per project and per agent, not security: "a web development
frontend skill is not required for a 3D game".

- `GET /api/catalog` lists every discovered skill and plugin with `name`,
  `source`, `description` (from `SKILL.md` frontmatter) and `default_enabled`.
- `GET /api/agents/{slug}/skills` rounds that catalog to one agent: every
  entry keeps its effective `enabled` value and an `origin` of `default`,
  `override` or `unavailable`.
- `PUT` or `PATCH /api/agents/{slug}/skills` accepts
  `{"skills": [{"name": "gamma", "enabled": false}], "plugins": [{"name": "p-one", "enabled": false}]}`.
  A plain `"gamma"` string means on. An unknown name returns 400 with the
  allowed names in `valid`. The whole batch is one transaction, and it writes
  a `skills` event that `bureau events` shows.

No explicit row means the discovered default is on. Turning a toggle back on
deletes the override, so the agent keeps following discovery. Skills are read
from `~/.agents/skills`, `~/.config/opencode/skills`, project `.opencode/skills`
and profile `skills` directories, including
`profiles/generated/<slug>/skills`.

These tables are the per-name store. `GET`/`PUT`/`PATCH
/api/agents/{slug}/toggles` remains the whole-selection API that the dispatcher
reads today.
