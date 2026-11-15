# Bureau ◈

**A ticket system for a company of AI agents.**

You open a ticket. The division manager reads it, splits it into a few
self-contained sub-tickets and hands each one to a worker. Workers claim their
ticket, do the work in the project directory, and leave a result for you to
review. Every hop is a row in SQLite and a card on the board, so you can always
see who did what and why.

One Rust binary, one SQLite file, one embedded web board. No accounts, no cloud,
no build step for the UI.

```
you ──ticket──▶ division manager ──sub-tickets──▶ workers
                     │                              │
                     └──────── review ◀─────────────┘
```

![The board](docs/board.png)

---

## Quick start

```bash
cargo build --release

./target/release/bureau init          # creates ./bureau.db + the seeded divisions
./target/release/bureau serve --seed  # http://127.0.0.1:8787
```

That is enough to try it from a checkout. To install it properly — system
service, packages, upgrades — see [Install](#install) below.

The board opens with a welcome ticket already flowing through the `demo`
adapter — no model is called, you are watching the workflow, not the
intelligence. Switch an agent to a real adapter and the same loop starts doing
real work.

---

## Dependencies

**To run a package:** nothing beyond libc and systemd. SQLite is compiled into the
binary (`rusqlite` with the `bundled` feature), so there is no `libsqlite3-dev` to
install and no database server to run. No OpenSSL either.

**To build from source:**

- Rust with `cargo`. The crate is `edition = "2024"`, so **rustc 1.85 or newer**.
  `rustup` is the easy path; distro packages work too.
  Fedora / Nobara: `sudo dnf install cargo rust` · Debian / Ubuntu: `sudo apt install cargo rustc`
- A C compiler and linker, for the bundled SQLite:
  `sudo dnf group install "Development Tools"` · `sudo apt install build-essential`
- `pkg-config` is **not** required.

## Install

Two ways to get a binary: **build from source** (fastest for development) or
**install a package** (runs as a system service, survives reboots).

### A. From source

```bash
git clone <repo> && cd bureau
cargo build --release
./target/release/bureau --version        # prints bureau <version>
```

Then either run it straight from `target/` — `./target/release/bureau init` and
`./target/release/bureau serve --seed` — or install that binary:

```bash
sudo install -m755 target/release/bureau /usr/bin/bureau
```

Building the release binary needs only `cargo` (Rust). No distro package
manager is involved, on any system.

### B. Package (deb / rpm / Arch)

```bash
./packaging/build-all.sh          # builds into dist/
```

**It only builds the formats this host can actually produce**, and prints the
reason for anything it skips. Building one distro's package on another is not
supported, and it never falls back to a foreign package manager:

| format | needs | typical host |
|---|---|---|
| `.rpm` | `rpmbuild` (`sudo dnf install rpm-build`) | Fedora / RHEL / Nobara / openSUSE |
| `.deb` | `ar`, `tar`, `xz` (usually already present) | Debian / Ubuntu |
| Arch | `makepkg` **and** an Arch-family `/etc/os-release` | Arch / CachyOS / Manjaro |

So on Fedora/Nobara you get an `.rpm` and a `.deb`; on Arch you get the Arch
package. Then:

```bash
sudo dnf install ./dist/bureau-<version>-1.x86_64.rpm        # Fedora / RHEL
sudo apt install ./dist/bureau_<version>_amd64.deb           # Debian / Ubuntu
sudo pacman -U dist/bureau-<version>-1-x86_64.pkg.tar.zst    # Arch / CachyOS

# then, once
sudo systemctl enable --now bureau@$USER
```

The unit is a template — the instance name is the user that owns the project
folders, so agents can write there. First start seeds the company into
`/var/lib/bureau/<user>/bureau.db`.

The installer writes `/etc/bureau/bureau.env` with `BUREAU_BIND=0.0.0.0:8787`
and a random `BUREAU_TOKEN`. Open the board once with
`http://<host>:8787/?token=<token>`; the browser keeps the token.

| file | purpose |
|---|---|
| `/usr/bin/bureau` | the binary |
| `/usr/lib/systemd/system/bureau@.service` | system service template |
| `/etc/bureau/bureau.env` | bind address + token (created on install, mode 600) |
| `/var/lib/bureau/<user>/bureau.db` | tickets |
| `/var/lib/bureau/<user>/logs/<ref>.log` | one transcript per agent run |

**Upgrading the binary:** `sudo install -m755 target/release/bureau /usr/bin/bureau && sudo systemctl restart bureau@$USER`.
The board HTML is embedded in the binary, so any UI change needs a rebuild and a
restart — reloading the browser is not enough.

> Packages are x86_64 and built for a recent glibc (2.39+). An older distro must
> rebuild from source.

Full per-distro notes, firewall commands and uninstall steps:
[`packaging/INSTALL.md`](packaging/INSTALL.md).

## ⚠ Read this before your first command

Almost every "it can't find my project" problem is one of these two.

### 1. `bureau` reads `./bureau.db` in the *current directory*

The CLI's default database is `bureau.db` relative to wherever you are standing.
Run it from your home directory and it opens (or creates) an **empty**
`~/bureau.db`, where every division, agent and ticket appears to be missing:

```
$ bureau division list
$ bureau division pause eduranus
Error: no division 'eduranus' in bureau.db — a fresh database has none; run
`bureau seed` to create the defaults, or pass --db / set BUREAU_DB
       ^^^^^^^^^^ that is not the server's database
```

Fix it once, in your shell profile:

```bash
echo 'export BUREAU_DB=/var/lib/bureau/$USER/bureau.db' >> ~/.zshrc   # or ~/.bashrc
```

…or pass `--db /var/lib/bureau/<user>/bureau.db` on every call. **The server's
database is the only one that matters** — the board shows what the service
holds, not what a stray file on disk says.

### 2. Division slugs are exact

`eduranus`, not `EdUranus` / `EduUranus` / `EdUranusz`. Never guess — ask:

```bash
bureau division list      # prints the real slugs and whether each is paused
```

---

## Add a project

A project is a **division** (with a charter and a directory) plus its **agents**.
Exactly one manager per division; without it, the division cannot triage.

```bash
# 1. the division — slug and name are POSITIONAL arguments
bureau division add myproject "My Project" \
  -d "What this repo is, and how to verify work (the build/test commands)." \
  -p /home/golddeniel/Projects/myproject

# 2. the manager — it triages every new ticket
bureau agent add head "Head" --division myproject -r manager --adapter hermes

# 3. workers — they do the edits
bureau agent add dev1 "Dev One" --division myproject \
  --adapter opencode --model opencode/deepseek-v4.1-flash-free
bureau agent add dev2 "Dev Two" --division myproject \
  --adapter opencode --model nvidia/minimaxai/minimax-m3.1-flash

# 4. check
bureau division list
bureau agent list
```

Field notes:

- **`-p` (project path) really matters.** Seeded divisions ship a literal
  `/home/you/Projects/...` placeholder; dispatching into one sends the agent to
  a directory that does not exist.
- **The charter (`-d`) is not decoration.** It is prepended to every agent prompt
  in that division. Put the real verification commands there
  (`cargo test`, `npm run typecheck`), not "work on this repo".
- **One model id per opencode worker — never a comma-separated list.** The
  adapter passes the field verbatim as a single `--model` argument, so a list is
  an unresolvable model id and **every** ticket fails with
  `{"name":"UnknownError","message":"Unexpected server error"}`. Switch the model
  by hand when a free tier is rate-limited.
- **Write an `AGENTS.md` in the repo.** A worker sees its ticket body and the
  project — nothing else. Your conventions must live in the repo or in the
  ticket, or they will be ignored.
- Agents are added or updated with the same command: `bureau agent add` on an
  existing slug updates it.

Existing project, different machine? Same steps — `-p` points at wherever the
repo lives there.

---

## Pause a project

Stop starting new work without throwing anything away:

```bash
bureau division pause eduranus      # no new triage, no new worker claims
bureau division resume eduranus
```

- Tickets already **`in_progress` finish normally** — their job was already
  spawned.
- **`queued` / `triaged` tickets wait** in priority order and pick up where they
  left off on resume.
- **`new` tickets stay untriaged**, so no new sub-tickets are created.
- `bureau division list` shows the state; the board shows a `paused` tag in the
  rail and a **Pause / Resume** button in the topbar.

This is the right tool for "let this land, then stop" — for example before
reviewing a batch of work, or before switching a worker's model.

---

## The model

**Divisions** are projects. Each has a slug, a name, a project path and a
charter (the text every agent in it reads), plus a `paused` flag.

**Agents** are either `manager` (exactly one per division) or `worker`.
Each agent has an *adapter* — how it actually runs:

| adapter | what it does |
|---|---|
| `demo` | deterministic, no model, no tokens. Exercises the full loop. |
| `ollama` | local Ollama at `127.0.0.1:11434/api/chat`. Chat only — it can answer and plan, but it has no file tools. Set the model on the agent. |
| `hermes` | `hermes -z <prompt> --in <project>`; full tool-calling agent, verifies its own work. Put `yolo` in the agent's hint to add `--yolo`. |
| `opencode` | `opencode run <prompt> --auto` in the project directory. One model id per agent. |
| `shell` | runs the agent's *model* field as a shell command; `{{prompt}}` is replaced, and the prompt is also on stdin. |
| `echo` | returns the prompt. Debugging only. |

**Tickets** have a ref (`BUR-42`), a status, a priority (P0–P4), an assignee and
an optional parent. Statuses: `new → triaged → queued → in_progress → review →
done`, plus `blocked` and `failed`.

**The loop** runs every 20 s (`--interval`) while the server is up:

1. `recover` — leases held by dead workers are requeued; three strikes → `failed`.
2. `triage` — every `new` ticket with no assignee goes to its division manager,
   which replies with strict JSON: delegate / answer / blocked.
   *(Skipped for paused divisions.)*
3. `delegate` — each sub-ticket becomes a real ticket assigned to a worker,
   status `queued`, parent moves to `triaged`. **Capped at `--max-subtasks`
   (default 3)** — a manager that proposes more is truncated rather than
   producing a long list of near-duplicate children.
4. `work` — idle workers claim queued tickets atomically, run their adapter in
   the project directory, and move the ticket to `review` with their output.
   *(Skipped for paused divisions.)*
5. `rollup` — when every child is finished the parent moves to `review`; a
   blocked or failed child pulls the parent into **Blocked** instead.

Managers only ever see tickets. They never see each other's output, so every
sub-ticket prompt carries its own context.

**While children are open, the parent reads `blocked`.** That is the waiting
state, not a fault — read the children before reporting a stuck parent.

### opencode worker profiles

An `opencode` agent inherits the global plugin set unless it gets its own
config directory. `scripts/opencode-profiles.sh` reads
`~/.config/opencode/opencode.json` as the single source and generates complete
profiles from it. It never writes the global config or the user's skills.

| profile | plugins |
|---|---|
| `worker-lite` | global plugins narrowed to ponytail |
| `worker-full` | the global plugin array, exactly as configured |
| `manager-bare` | none |
| `inherit` | not generated: use the global config as-is, no env override |

Every generated profile copies the provider block and every other top-level
key from the global config verbatim, because opencode replaces array keys
instead of merging them. Change providers or plugins in the global file, then
regenerate:

```bash
scripts/opencode-profiles.sh --apply   # write; dry run without --apply
scripts/opencode-profiles.sh --check   # validate files and fingerprints
```

Generated files live in a managed namespace. There is one extra `opencode/`
level because opencode appends it to `XDG_CONFIG_HOME`:

```text
~/.config/opencode/profiles/generated/<name>/opencode/opencode.json
XDG_CONFIG_HOME=~/.config/opencode/profiles/generated/<name> opencode run …
```

Each generated directory carries `.bureau-generated.json`, a generator version
plus the sha256 of every emitted file. If a file was edited after generation,
the next run refuses the whole write and names the file. `inherit` is selected
in the board like a profile but sets no `XDG_CONFIG_HOME`, so the agent uses the
global config exactly as before profiles existed. A blank profile or `none`
means the same thing; `inherit` just makes the choice explicit.

Inside another opencode session, add `--standalone` so the run gets its own
server and does not reuse the caller's config.

---

## Watching agents work

The board's **Live** strip sits right under the header. Each running job is a
chip: agent slug, ticket ref, elapsed time and a process count. Click it to see
the real process tree — the agent CLI plus every subagent/helper it spawned
(`opencode run`, `godot --headless`, …), with pid and runtime.

The list is built from `ps` restricted to the server's own process tree, so a
shell or watcher that merely mentions a ticket ref never shows up as an agent.
`GET /api/state` returns the same data under `live` — and `dispatching: false`
between passes means "nothing is running right now", not "the loop is off".

Every run also writes a transcript to `<db dir>/logs/<ref>.log`, served by
`GET /api/tickets/{ref}/log`. **A ticket can read `failed` while its work is on
disk** — read the log and check the project's `git log` before believing a
failure produced nothing.

---

## CLI

```
bureau init                       create db + seed divisions/agents
bureau serve [--bind ADDR]        HTTP + board (+ dispatcher unless --no-dispatch)
bureau tick                       one dispatch pass, then exit (cron-friendly)
bureau ticket new|list|show|status|assign|comment
bureau agent list|add|set         add/set --adapter --model --hint --enable/--disable
bureau division list|add|pause|resume
bureau events                     recent activity
```

Global `--db <path>` (env `BUREAU_DB`) — see the warning at the top.

Handy one-liners:

```bash
bureau ticket new "Add a save system" -d example-game -p 1 \
  -b "Three slots, autosave every 5 minutes, works from the pause menu."
bureau ticket list --division example-game
bureau ticket list --status review          # what is waiting on you
bureau ticket show BUR-7
bureau ticket status BUR-7 done             # close it
bureau agent set forge --adapter ollama --model qwen3:14b
```

Leave out `--agent` and the division manager triages the ticket. Assign an agent
directly and it skips triage.

---

## HTTP API

Everything the UI does is a public endpoint — agents can drive it directly.
When the service has a token, send it as `x-bureau-token`, `Authorization:
Bearer`, a cookie, or `?token=`. `/` and `/api/health` stay open.

| method | path | purpose |
|---|---|---|
| GET | `/api/health` | version + liveness (no auth) |
| GET | `/api/state` | everything the board needs in one call |
| GET/POST | `/api/tickets` | list (`?division=&status=`) / create |
| GET/PATCH | `/api/tickets/{ref}` | detail / status, priority, assignee, title, body |
| POST | `/api/tickets/{ref}/comment` | comment — **singular**, `/comments` is a 404 |
| POST | `/api/tickets/{ref}/assign` | assign to agent slug |
| POST | `/api/tickets/{ref}/claim` | claim with a 1 h lease |
| GET | `/api/tickets/{ref}/log` | tail of the agent transcript |
| GET | `/api/tickets/{ref}` | also returns `attempts[]` — every failed run, classified |
| GET/PUT | `/api/agents/{slug}/models` | the agent's fallback ladder, in try order |
| GET/POST | `/api/divisions` | list / create |
| POST / DELETE | `/api/divisions/{slug}/pause` | pause / resume a division |
| GET/POST | `/api/agents` | list / create |
| PATCH | `/api/agents/{slug}` | adapter, model, hint, enabled, role, plugin_profile |
| DELETE | `/api/agents/{slug}` | remove an agent |
| GET | `/api/agents` | agents plus `plugin_profiles` (valid names) |
| GET | `/api/events?limit=` | activity log |
| POST | `/api/dispatch/tick` | start a pass in the background |

`PATCH /api/agents/{slug}` also takes an optional `X-Bureau-Agent: <slug>`
header: when present, that agent must be a manager and may only patch agents
in its own division (403 otherwise). Unknown `plugin_profile` names are 400 and
the error lists the valid ones.

Things the API will not tell you politely:

- `POST /api/tickets` **may not echo the new ref** — re-read `/api/state`.
- There is **no endpoint that deletes a division** — only `DELETE /api/agents`
  exists. Retiring a division is SQL: checkpoint the WAL, back up, then delete
  `comments → events → tickets → agents → divisions` scoped by `division_id`,
  and clear `sqlite_sequence` so refs do not inherit the old numbering. Verify
  afterwards with `PRAGMA foreign_key_check` **and** a count of tickets whose
  `assignee_id` no longer resolves — that second check is what catches a partial
  delete (an over-broad `division_id IN (…)` list can take a healthy division's
  agents with it).
- Refs restart at `BUR-1` on a fresh database, so a ref in an old commit message
  may name a different ticket today.

External agent loop:

```bash
curl -s localhost:8787/api/tickets?status=queued\&division=example-game
curl -s -X POST localhost:8787/api/tickets/BUR-7/claim \
     -H 'content-type: application/json' -d '{"agent":"forge"}'
# ... do the work ...
curl -s -X POST localhost:8787/api/tickets/BUR-7/comment \
     -H 'content-type: application/json' -d '{"body":"Done: ...","author":"agent:forge"}'
curl -s -X PATCH localhost:8787/api/tickets/BUR-7 \
     -H 'content-type: application/json' -d '{"status":"review"}'
```

Agent integration details: `docs/AGENT-INTEGRATION.md`.

---

## Troubleshooting

| symptom | cause and fix |
|---|---|
| `no division 'X' in bureau.db` | Wrong database (see the warning at the top), or a misspelled slug. `bureau division list`. |
| Every child ticket `failed` with `UnknownError` | A comma-separated model list on an opencode agent. Set exactly one model id. |
| Board is blank / connection refused on the LAN | `ss -ltn \| grep 8787`; open `firewalld`/`ufw` for the port. |
| `inactive (dead)` after installing | The packaged unit ships disabled. `sudo systemctl enable --now bureau@$USER`, then `journalctl -u bureau@$USER -n 40`. |
| Agents "cannot spawn 'opencode'" | The unit's `PATH` is `/usr/local/bin:/usr/bin`. Install a drop-in with the real CLI directory, or symlink the CLI into a directory already on it. |
| `packaging/build-all.sh` fails on `makepkg: command not found` | Older versions called every builder unconditionally. It now detects the host: Fedora/Nobara builds `.rpm`+`.deb`, Arch builds the Arch package, and anything it cannot build is skipped with a reason. If you see this, you are on the old script. |
| Board shows no change after a UI edit | The HTML is embedded with `include_str!` — `cargo build --release` and restart the service. |
| Ticket stuck at `blocked` and no leave is moving | Check its children. All children open = the parent is waiting, which is correct. |
| Nothing is dispatching | Division is paused (`bureau division list`), or a pass is in flight (`dispatching: true`). |

Stopping the service: `pidof bureau` to find it — `pkill -f 'bureau serve'` also
matches the shell that runs it.

---

## Failures, tags and model rotation

Every failed run is **classified**, not just recorded. The kind is stored per
attempt and shown as a tag on the card and in the drawer, so a board full of
red says *what* is wrong instead of just *that* something is.

| kind | means | next move |
|---|---|---|
| `rate_limit` | 429 / provider throttling | retry once, then next model |
| `quota` | free tier or plan exhausted | next model immediately |
| `auth` | bad or missing credential | next model immediately |
| `provider_down` | 5xx, overloaded, socket closed | retry once, then next model |
| `content_safety` | the provider refused the request | next model immediately |
| `timeout` | hit the wall-clock budget | **stops** for a human |
| `model_output` | answered, but not to contract (bad JSON) | **stops** for a human |
| `tooling` | a local tool was missing (ripgrep, dotnet) | fails — another model cannot help |
| `unknown` | no signature matched | fails |

Detection looks at the **tail** of the agent output first, where CLIs write
their real errors, so a 429 buried under thousands of lines of narration is
still found, and the one-line reason on the card is the matching line rather
than the whole blob.

**Rotation is deliberately conservative.** Only provider-side failures walk the
ladder. A two-hour timeout or a half-finished run is parked for a human,
because the agent may already have committed — replaying that prompt on a
different model would be a second implementation, not a retry.

Give an agent a ladder from the board (Agents → agent → Fallback models) or via
the API. Rungs are tried top to bottom; a rung that fails for an infrastructure
reason gets a cooldown (`rate_limit` 60 s, `provider_down` 5 min, `quota` 6 h,
`auth` 24 h) so the next ticket skips it instead of rediscovering it.

```bash
curl -X PUT localhost:8787/api/agents/tamis/models -H 'content-type: application/json' -d '{
  "models": [
    {"provider": "nvidia",     "model": "nvidia/minimaxai/minimax-m3.1-flash", "adapter": "opencode"},
    {"provider": "openrouter", "model": "deepseek/v4.1",                       "adapter": "opencode"},
    {"provider": "ollama",     "model": "qwen3.8:27b",                          "adapter": "ollama"}
  ]}'
```

### Per-agent skills and plugins

Agents carry typed `skills` and `plugins` lists. The `hermes` adapter passes
them as `--skill` / `--plugin`; the other adapters store them and log a warning
that they cannot be applied, rather than pretending they took effect.

---

## Why not an existing tracker?

| tool | great at | missing for this |
|---|---|---|
| [Beads](https://github.com/gastownhall/beads) | dependency graph, hash IDs, agent CLI, Dolt sync | flat queue — no managers, no delegation tree, no division ownership |
| [Atlas Tasker](https://github.com/myrrazor/atlas-tasker) | tickets in the repo, leases, evidence, MCP | all agents are peers; no manager role or automatic triage |
| [Fleet Beacon](https://github.com/np6126/fleet-beacon) | pull-based fleet, isolated workspaces | one board, no org chart; runners are Python |
| [Kontora](https://github.com/worksonmyai/kontora) | pipelines, git worktrees, tmux | pipeline stages, not a persistent ticket company |
| Chorus / fanoutd | visual multi-project orchestration | heavyweight or model-coupled; not a local ticket store |

Bureau is deliberately the small piece those tools do not have: **a persistent
ticket board with a manager layer**. If you later want dependency graphs, use
Beads alongside it — Bureau tickets can reference `bd` IDs in their bodies.

Terminal / scripts: `scripts/smoke.sh` runs the whole loop on a throwaway database.

---

## Run it as a user service (no root)

```bash
mkdir -p ~/.config/systemd/user
cp contrib/bureau.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now bureau
```

Handy if you are not using the packaged system unit. Note it uses
`BUREAU_DB`/`--db` from the unit file, not your shell.

---

## Safety notes

- Binds to `127.0.0.1` by default (`--bind`, or `BUREAU_BIND`). The packaged
  service binds `0.0.0.0` **and** generates a token; if you set
  `BUREAU_TOKEN=` empty, the board and API are open to your LAN. Never expose it
  to the internet — agents run as the service user and there is no TLS.
- Adapters run real commands with your user's permissions. `hermes` without
  `--yolo` and `opencode` without `--auto` are safer; the seeded agents use
  `demo` for exactly this reason.
- `shell` adapter executes whatever is in the agent's model field. Treat that
  field as code.
- Workers get a 1 h claim lease; a crashed run is requeued, and after three
  attempts the ticket is marked `failed` instead of retrying forever.
- `/etc/bureau/bureau.env` is mode 600 and root-owned. If you need the token,
  read it from the running process rather than widening the file:
  `tr '\0' '\n' < /proc/$(pidof bureau)/environ | sed -n 's/^BUREAU_TOKEN=//p'`.

---

## Development

```bash
cargo test          # unit + HTTP integration tests, all demo adapters
cargo fmt && cargo clippy
cargo run -- serve --db /tmp/scratch.db --seed
```

Layout: `src/db.rs` schema/queries · `src/dispatch.rs` the loop ·
`src/adapters.rs` how agents run · `src/api.rs` HTTP · `assets/index.html` board.

Schema changes to an existing database: `CREATE TABLE IF NOT EXISTS` never
alters a table, so add a column with an explicit `pragma_table_info` check in
`db::open` (see `divisions.paused`) or an `ALTER TABLE` migration.
