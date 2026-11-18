# BUR-72 Tester Workflows Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add tester-role API coverage, prove worker-to-tester workflows with demo adapters on throwaway databases, and publish release `v0.5.4`.

**Architecture:** Keep existing API validation and database schema. Extend dispatch routing so completed worker tickets enter `testing` only when their division has an enabled tester. Use deterministic demo-adapter verdict control and one shared verdict handler for PASS, FAIL, and tooling outcomes. Pin HTTP behavior and end-to-end workflow behavior with integration tests before changing dispatch.

**Tech Stack:** Rust 2024, Axum, Tokio, Rusqlite, Tokio integration tests, Bash smoke script, RPM and Arch packaging.

**Spec:** BUR-72 acceptance criteria from task conversation; no separate spec file exists.

## Global Constraints

- Never use live board or `/var/lib/bureau/golddeniel/bureau.db`.
- Keep `testing` schema migration limited to expanding SQLite TEXT CHECK constraints; preserve existing rows and do not recreate `bureau.db`.
- Store failed tester verdicts in `attempts.kind` as exactly `verification`.
- Do not install Bureau, restart or reload `bureau@golddeniel`, or start a server on port 8787.
- Bump `Cargo.toml`, generated `Cargo.lock`, `packaging/bureau.spec`, and `packaging/PKGBUILD` together in one release commit.
- Preserve unrelated dirty work in `assets/index.html`.

## Review Focus

- A division with an enabled tester must route worker output through `testing`; a division without one must keep direct `review` routing.
- A tester FAIL must return work to `queued`, retain a verification comment, and create one attempt row whose kind is exactly `verification`.
- A tester PASS must create no failure attempt and move the ticket to `review`.
- Invalid create or update roles must return 4xx and leave every existing agent field unchanged.
- Schema upgrade must preserve existing agent and ticket rows when adding `testing` and `tester` to CHECK constraints.

---

### Task 1: Pin Tester-Role HTTP Behavior

**Files:**
- Modify: `tests/api.rs`

**Interfaces:**
- Consumes: `bureau::api::router`, `bureau::db::Db`, existing `setup`, `send`, and `send_with` helpers.
- Produces: API contract tests for POST `/api/agents` and PATCH `/api/agents/{slug}`.

- [ ] **Step 1: Add failing API characterization test**

Create a tester through `POST /api/agents` and assert a 2xx response, returned role `tester`, and persisted role `tester`.

- [ ] **Step 2: Add failing update characterization test**

Patch an existing worker with role `tester`; assert 2xx and persisted role `tester`.

- [ ] **Step 3: Add failing rejection tests**

Send create and update requests with role `wizard` or another unknown role; assert 4xx and assert the complete stored agent record remains unchanged.

- [ ] **Step 4: Run focused API tests**

Run: `cargo test --test api tester_role -- --nocapture`

Expected: PASS if existing production validation already satisfies the contract. If not, fix validation before continuing.

### Task 2: Prove Tester Workflow Routing

**Files:**
- Modify: `tests/api.rs`
- Modify: `src/db.rs`
- Modify: `src/dispatch.rs`
- Modify: `src/adapters.rs`

**Interfaces:**
- Consumes: `bureau::dispatch::tick`, `db::testers_for`, `db::testing_for_agent`, `db::attempts_for`, and `db::comments`.
- Produces: deterministic worker completion routing, tester PASS handling, tester FAIL handling, and no-tester fallback.

- [ ] **Step 1: Add PASS workflow test**

Create a throwaway database with one manager, one worker, and one tester. Assign a ticket directly to the worker, run `dispatch::tick`, and assert ticket status `review`, tester count `1`, and no failed attempt.

- [ ] **Step 2: Add FAIL workflow test**

Use deterministic demo FAIL control, run `dispatch::tick`, and assert ticket status `queued`, a verification comment exists, and the latest attempt kind equals `verification`.

- [ ] **Step 3: Add no-tester workflow test**

Remove or omit the tester, assign a ticket to the worker, run `dispatch::tick`, and assert ticket status `review`.

- [ ] **Step 4: Run focused workflow tests and confirm expected failure**

Run: `cargo test --test api tester_workflow -- --nocapture`

Expected: FAIL because worker output currently moves directly to `review`, tester collection requires the wrong assignee, and demo tester output has no verdict.

- [ ] **Step 5: Implement minimum worker routing**

After worker success, query enabled testers for the ticket division. Move ticket to `testing` and clear its worker lease when a tester exists; otherwise move it to `review`.

- [ ] **Step 6: Fix tester collection and claim ownership**

Collect idle enabled testers against division-wide `testing` work. Ensure claimed testing tickets can be claimed by testers but remain associated with the original worker for FAIL requeue behavior.

- [ ] **Step 7: Add deterministic demo verdict control**

Make demo tester output contain `TESTER_VERDICT: PASS` by default and support a narrow FAIL control used by tests and smoke runs.

- [ ] **Step 8: Implement PASS and FAIL transitions**

PASS moves ticket to `review`. FAIL adds a verification comment, records one attempt with kind `verification`, and moves ticket to `queued` for worker retry.

- [ ] **Step 9: Run focused workflow tests**

Run: `cargo test --test api tester_workflow -- --nocapture`

Expected: PASS.

### Task 3: Extend Release Smoke Coverage

**Files:**
- Modify: `scripts/smoke.sh`

**Interfaces:**
- Consumes: release binary CLI, demo adapters, throwaway database created by `mktemp`.
- Produces: smoke evidence for tester PASS, tester FAIL, and no-tester fallback without model calls.

- [ ] **Step 1: Add PASS smoke flow**

Create a tester through CLI or API, run demo dispatch, and assert the ticket reaches `review` after tester verification.

- [ ] **Step 2: Add FAIL smoke flow**

Run deterministic demo FAIL control and assert ticket reaches `queued` with a verification attempt.

- [ ] **Step 3: Add no-tester smoke flow**

Use a division without a tester and assert worker output reaches `review`.

- [ ] **Step 4: Run smoke script**

Run: `bash scripts/smoke.sh`

Expected: PASS with no model calls and no live database access.

### Task 4: Verify and Publish `v0.5.4`

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `packaging/bureau.spec`
- Modify: `packaging/PKGBUILD`

**Interfaces:**
- Consumes: verified implementation and packaging metadata.
- Produces: release commit and tag `v0.5.4`.

- [ ] **Step 1: Run full verification**

Run: `cargo test --all-targets && cargo build --release && cargo test && bash scripts/smoke.sh`

Expected: all commands exit 0.

- [ ] **Step 2: Bump package version**

Set package version to `0.5.4` in all four release files. Regenerate `Cargo.lock` with `cargo check` or `cargo generate-lockfile`.

- [ ] **Step 3: Verify release metadata and tag absence**

Run: `grep -n '0.5.[34]' Cargo.toml Cargo.lock packaging/bureau.spec packaging/PKGBUILD && git tag --list v0.5.4`

Expected: all release files use `0.5.4`; tag list is empty.

- [ ] **Step 4: Create release commit**

Stage only implementation, tests, smoke, plan, and four release files. Do not stage unrelated `assets/index.html` work. Commit all release-file bumps together with message `release: v0.5.4`.

- [ ] **Step 5: Tag and verify release commit**

Run: `git tag -a v0.5.4 -m 'v0.5.4' && git show-ref --tags v0.5.4 && git rev-parse v0.5.4^{commit}`

Expected: annotated tag points to release commit.

- [ ] **Step 6: Report install and schema guidance**

Provide exact command `sudo install -m755 target/release/bureau /usr/bin/bureau`. Explain that `testing` expands a TEXT CHECK constraint, requires no row-data migration, and existing `bureau.db` must not be recreated.
