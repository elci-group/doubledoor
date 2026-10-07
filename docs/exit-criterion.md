# Exit Criteria — Phases I–VI

# Phase I Exit Criterion — Two-Actor Isolation Scenario

This is the permanent integration fixture for Phase I (§45). It is
implemented as an integration test in `tests/integration/` and must pass
before any Phase I completion claim.

## Scenario

1. A git repository exists at commit **A** (`dispatch() -> Result<Response>`).
2. **Agent α** requests `src/router.rs` from Locksmith; the lease is granted
   (α proceeds in the canonical worktree — outside Doubledoor).
3. **Agent β** requests an intersecting surface; Locksmith denies and issues a
   `ContentionNotice`; β is redirected into Doubledoor:
   `doubledoor open --actor beta --intent "..." --surface src/router.rs --notice notice.json`.
4. β's DoorSession is created at canonical commit **A**, isolated in a
   detached `git worktree` under `.doubledoor/sessions/dd_<uuid>/workspace`.
5. α commits a change in the canonical worktree (commit **B**).
6. β edits a disjoint file in its workspace and commits (commit **D**).

## Assertions (the §45 exit criterion)

- **No cross-contamination**: β's workspace contains β's edit and *not* α's
  canonical commit; α's worktree contains α's commit and *not* β's edit.
- **Canonical isolation (DD-I1)**: the repository's HEAD and branches are
  exactly what α produced; Doubledoor created no branch and moved no ref.
- **User worktree untouched (§9)**: the main checkout contains only α's
  change — `.doubledoor/` and the worktree are additions, never mutations.
- **Stable base (DD-I3)**: β's session metadata still records commit **A**
  as its base after α's commit **B**.
- **Journalled**: β's session journal records `SessionCreated`,
  `WorkspaceCreated`, `SessionActivated` in order; `doubledoor inspect`
  reports `ACTIVE`.
- **Clean exit**: `doubledoor abandon dd_<uuid>` removes the worktree
  (`git worktree list` no longer shows it) and records `ABANDONED`.

## Crash variant

Killing Doubledoor between metadata write and workspace creation, then
running `doubledoor recover`, must leave: canonical refs untouched, the
session marked `FAILED_ISOLATION`, and no usable partial workspace.

## Note

This scenario deliberately stops before GhostDiffs: β receives *no*
notification of α's commit in Phase I (that is Phase II's exit criterion).
β learns about commit **B** only through its own Git operations, never
through silent workspace mutation (DD-I2).

# Phase II Exit Criterion — Notification Without Mutation (§46)

> A speculative actor receives structured notification of relevant canonical
> mutations without having its workspace altered automatically.

Implemented in `tests/integration/ghosts.rs`
(`exit_criterion_notified_without_workspace_mutation` plus the accept,
defer, reject, selective-accept, conflict, and recovery scenarios).

## Scenario

1. A repository at commit **A**; β opens a session writing `alpha.txt`
   (`--surface alpha.txt`) and reading `beta.txt` (`--read beta.txt`).
2. α commits **B** canonically with `git add -A`, rewriting `beta.txt` and
   adding `gamma.txt`, with `dd-actor:`/`dd-intent:` trailers.
3. `doubledoor watch --once` observes the session.

## Assertions

- **Store invisible to Git**: α's `git add -A` did not capture `.doubledoor/`.
- **Structured notification**: the event stream carries `ghost.available`
  for β with `source_commit` **B**, `source_actor` `agent-alpha`,
  classification `read_relevant`, intersections `["file:beta.txt"]`, and
  `needs_assessment: true`, followed by `session.state` ACTIVE →
  GHOST_PENDING.
- **No automatic mutation**: β's workspace HEAD, tracked contents, and
  status are byte-identical before and after observation and after
  `ghosts`; `gamma.txt` does not appear.
- **Explained**: `doubledoor ghosts` shows SOURCE, INTENT, INTERSECTION,
  SAFE IMPORTS, and ACTION REQUIRED.
- **Stable base (DD-I3)**: β's base commit is still **A**.
- **Journalled (DD-I5)**: `GhostRecorded`, `GhostCursorAdvanced`,
  `GhostObserved` are in β's journal.
- **Explicit entry only (DD-I2)**: the change reaches β's workspace only
  through `accept`, as a new commit on β's own history; a held surface
  keeps β's version under a selective accept; a non-applying accept exits 10
  and leaves the workspace unchanged.

# Phase III Exit Criterion — Cross-File Conflicts Git Cannot See (§47)

> Doubledoor identifies meaningful cross-file conflicts that ordinary Git
> line analysis cannot detect.

Implemented in `tests/semantic/cross_file.rs` against a real Cargo package
(`router-app`: `router.rs`, `config.rs`, `handler.rs`, `outcome.rs`).

## Scenario

1. β's session edits only `src/handler.rs`, whose `handle()` calls
   `crate::router::dispatch`, and commits in its workspace (**D**).
2. α changes only `src/router.rs`: `dispatch()` now returns
   `Result<RetryOutcome, Error>` (**B**).
3. `git merge-tree --write-tree B D` succeeds: the files are disjoint, so
   Git's line analysis reports no conflict.

## Assertions

- The ghost is `api_breaking`; `file:src/router.rs` alone is
  `independent` (the file layer sees nothing).
- `rust:router_app::router::dispatch` is `api_breaking`, listed in
  `api_breaking`, and carries one invalidated assumption naming the old and
  new signatures and the dependent site
  `router_app::handler::handle (src/handler.rs:N)`.
- `src/router.rs` is a *breaking* import, not a safe one.
- The event stream's `ghost.available` has `classification: api_breaking`
  and the symbol among its intersections; the human view shows ASSESSMENT
  and ASSUMPTION INVALIDATED.
- β's workspace is untouched.

## §52 acceptance classification

α adds `retry.rs`, `RouterConfig.retry_limit`, and changes `dispatch()`'s
signature while β reworks `dispatch()`: `retry.rs` and its symbols are
`independent`, `RouterConfig.retry_limit` is `dependency_relevant`,
`dispatch` is `write_overlap` (reason notes the API break), and a selective
accept of `retry.rs`, `config.rs`, `lib.rs` keeps β's `dispatch()`.
Validating the combined candidate is Phase IV.

Further fixtures in the same suite: new struct fields against struct
literals, new variants against exhaustive matches, removed and
semver-incompatible dependencies, behaviour-only changes, unused changes
that do not block, and a mid-edit working copy that falls back to the
committed version.

# Phase IV Exit Criterion — Convergence With Isolation (§48)

> Independent portions of competing mutations can converge automatically
> while contested surfaces remain isolated.

Implemented in `tests/reconcile/convergence.rs` against a dependency-free
Cargo package, with validation running `cargo check` and `cargo test`
offline.

## Scenario (§52)

α commits canonically: `retry.rs`, `RouterConfig.retry_limit`, a
`log_route` change, a new `RouteError`, and a `dispatch()` signature
change. β, in its session, reworks `dispatch()`, adds `trace()` to the same
file and `handle_twice()` to `handler.rs`, and defers the ghost.

## Assertions

- `reconcile` builds `E'` as a new commit whose parent is α's commit;
  canonical `HEAD`, every ref, and β's workspace are unchanged.
- Converged: α's `retry.rs`, `retry_limit`, `log_route`, `RouteError`; β's
  `trace()` (same file as the contest) and `handle_twice()`.
- Isolated: `dispatch()` is contested, kept whole from β, never blended
  with α's signature; its resolution is unresolved (deferred), so the
  candidate is not promotable and `promote` refuses (exit 6).
- Conflict graph: `handler::handle → router::dispatch` (breaking).
- Validation: git integrity, cargo metadata, cargo check, cargo test pass;
  β's retained `dispatch()` is reported `compatible`.
- After β rejects the ghost, a new candidate is promotable and promotes.

Also covered: a rename that breaks β's retained `dispatch()` is reported
`incompatible` with the compiler error and VALIDATION_FAILED (later gates
skipped); `--converge` promotes only β's independent work while
`dispatch()` stays isolated in β's workspace (partial promotion, session
stays ACTIVE, its own promoted commit is not a ghost); a full promotion
completes the session and retires its workspace; lease refusals (partial
coverage, other holder, missing) change nothing (exit 11); a canonical
commit after validation makes the candidate stale and promotion exit 12
with canonical untouched; workspace edits after the candidate block
promotion; `accept --symbol` imports single items and refuses items β also
changed; recovery completes a promotion interrupted after the ref moved and
aborts an interrupted validation.

# Phase V Exit Criterion — One Evidence Model (§49)

> Reconciliation reports combine source, API, dependency, diagnostic and
> behavioural evidence through a unified evidence model.

Implemented in `tests/evidence/unified.rs` with the real ELCI tools where
installed (Kaptaind, Traci, Fract, ZOC, Padagonia).

## Assertions

- The candidate report's `evidence` bundle (`doubledoor.evidence/v1`) has
  deterministic items of all five required kinds — source (derivation,
  merges), API (β's breaking `dispatch()` signature change), dependency,
  diagnostic (`cargo check`, compatibility), behavioural (`cargo test`
  counts) — and the unified verdict is `concerns` (a breaking API change,
  with compiler and tests passing).
- Every provider is accounted for: Kaptaind/Traci/Fract (static analysis)
  and ZOC (heuristic) ran when installed, else `unavailable`; Sore
  `unavailable`; Padagonia `disabled`. Traci's non-zero "findings" exit is
  a run, not a failure.
- ZOC items are heuristic `info` only; the actor's intent is `actor_claim`;
  deterministic and probabilistic item ids partition the evidence.
- When Kaptaind reports no API break against Doubledoor's AST finding, the
  disagreement is recorded.
- Nothing leaks: β's workspace unchanged, no `.kaptaind/` in the session or
  canonical tree, evidence worktrees removed; `EvidenceRecorded` journalled.

Also covered: with every provider unavailable, all five kinds are still
covered and `doctor` reports availability without failing; a probabilistic
external provider overclaiming `git_object`/`blocks` is forced to `model`,
demoted to `concern`, advisory only; a deterministic external concern is
capped at its tier and counts; unparseable output is `failed` with no
items; the evidence graph is published to a real Padagonia server,
idempotently, and a wrong API key fails only the sink.

# Phase VI Exit Criterion — No Prose Required (§50)

> Coding agents can use Doubledoor without parsing human CLI prose.

Implemented in `tests/agent/protocol.rs`; every message is checked against
`schemas/`.

## Scenario (over `doubledoor rpc`)

1. Requests before `hello` fail `handshake_required`; an unknown protocol
   version fails `unsupported_protocol`; a required unknown capability
   fails `capability_unavailable`; `hello` grants the requested
   capabilities and returns 25 methods and the 13-row exit-code table.
2. The agent opens a session (notice as a JSON object) with
   `auto_accept: independent` and subscribes to its events.
3. α commits an independent file and a write-overlapping change;
   `observe` auto-accepts the independent ghost (it appears in the
   workspace), the overlapping one is not imported; notifications carry
   `ghost.assessed` with `by: policy:independent` and `session.state →
   GhostPending`.
4. The agent follows `report.next_actions` verbatim: `defer`, then
   `reconcile`, then `promote` (adding the `lease` object it `requires`);
   the session completes and canonical fast-forwards over α's commit.
5. Failures are structured (`invalid_transition`, `not_found` for a
   `--repo=/` "session id", `invalid_params`, `unknown_method`).
6. Every stdout line parsed as JSON; stderr stayed empty.

Also covered: `serve` on a 0600 socket pushes watcher-produced
`ghost.available` notifications without any `observe` call, and refuses a
second server (exit 6, JSON error); automatic acceptance never imports a
ghost ahead of an earlier deferred one touching the same file, and both
the policy and the attribution are journalled; every CLI failure under
`--format json` is a schema-conforming JSON error.
