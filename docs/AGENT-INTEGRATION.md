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
