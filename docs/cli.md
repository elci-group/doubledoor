# Doubledoor CLI — Phases I–VI

The `doubledoor` binary is a thin orchestration layer over the library
crates; all domain logic (state machine, policy, journal, Git isolation,
ghost observation) lives there. Implemented in Phase I: `status`,
`sessions`, `open`, `inspect`, `abandon`, `recover`, `doctor`. Implemented
in Phase II: `watch`, `events`, `ghosts`, `accept`, `defer`, `reject`.
Phase III adds `surfaces` and Rust-symbol/Cargo assessment of every ghost.
Phase IV implements `reconcile`, `validate`, and `promote`, and adds
`candidate`, `discard`, and `accept --symbol`. Every §30 command is now
implemented; exit 9 stays reserved for future declared-but-unimplemented
commands.

Global flags: `--repo <path>` (default: discover the enclosing git
repository upward from the current directory), `--format json|human`
(default human), `--verbose` (tracing to stderr), and `-v` / `--version`
(conventional `-V` also works), which print `doubledoor <version>` and are
legal before or after any subcommand.

Human output is styled when stdout is a terminal: section rules
(`── TITLE ───…`), cyan `KEY=VALUE` keys, tables for session listings and
`doctor` checks, ✓/✗/○ markers, and severity colors for states,
classifications, verdicts, and gate results. Piped or redirected output —
and any run under `NO_COLOR` or `TERM=dumb` — is plain text with the same
§31 shapes, so scripts and tests are unaffected; `--format json` never
carries styling.

## Commands

- `open --actor <id> --intent <text> --surface <s>... --notice <json-file>
  [--read <path>]... [--write <path>]... [--depends <path>]...` —
  validate the Locksmith contention notice, evaluate the live policy, then
  create the session layout `.doubledoor/sessions/dd_<uuid>/` (CBOR metadata
  in a schema-versioned envelope, append-only `journal/events.jsonl`,
  detached `git worktree` workspace at the canonical base commit). The §11
  sets are declared here: `--surface` values that name a tracked file join
  the write set, and `--read`/`--write`/`--depends` add `file:<path>`
  surfaces to the read, write, and dependency sets. The store directory gets
  its own `.gitignore` (`*`), so a canonical `git add -A` never captures
  Doubledoor state; the user's `.gitignore` is never touched. Human output
  is the §31 machine-readable shape: `SESSION=… STATE=… WORKSPACE=…`.
- `status` / `sessions` — scan session metadata into a `StatusReport` /
  list of `SessionSummary`.
- `inspect <id>` — full detail: state, base commit, journal event count,
  workspace existence.
- `abandon <id>` — from ACTIVE, GHOST_PENDING, or ASSESSING; removes the
  worktree, transitions to ABANDONED, journals it. Metadata, journal, and
  ghost records are retained.
- `recover` — replays every session journal, reconciles metadata with
  on-disk reality (ISOLATING without workspace → FAILED_ISOLATION;
  ISOLATING with workspace → ACTIVE; ACTIVE without workspace →
  FAILED_ISOLATION; torn journal tails are tolerated and reported), prunes
  stale worktree registrations, and never modifies canonical refs. An
  accept whose workspace commit landed but whose journal record did not
  (crash between the two) is adopted from the commit's `Dd-Ghost:` trailer
  (`adopted-ghost-accept`).
- `doctor` — checks git availability/version, repo discovery, metadata
  schema compatibility, journal readability, and worktree-registration
  consistency.

## Ghost observation (Phase II)

- `watch [--interval-ms N] [--once]` — the canonical commit watcher. Polls
  canonical `HEAD` (one `rev-parse` per tick) and, when it moves, observes
  every live session: each canonical commit past the session's ghost cursor
  becomes a classified GhostDiff under `sessions/<id>/ghosts/`, is journalled
  (`GhostRecorded`, then `GhostCursorAdvanced`), and is published as a
  `ghost.available` event. A session with a ghost that needs assessment
  (anything but `independent`; `unknown` always does) moves ACTIVE →
  GHOST_PENDING. **Workspaces are never touched.** One failing session never
  stops observation of the others. Output: one line (or JSON object) per
  observed session.
- `events [--since SEQ] [--follow] [--session ID] [--max N]` — the local
  event stream (§13) from `.doubledoor/events.jsonl`, one JSON object per
  line with a dense 1-based `seq`: `ghost.available`
  (`source_commit`, `source_actor`, `classification` in snake_case,
  `intersections`, `needs_assessment`), `ghost.assessed` (`decision`), and
  `session.state` (`from`, `to`). Agents follow this stream instead of
  polling Git, resuming with `--since <last seq>`.
- `ghosts <session> [--no-observe]` — observe, then list the session's
  ghosts oldest-first with status (`pending`, `deferred`, `accepted`,
  `rejected`). Human output is the §31 block: SOURCE, INTENT, CHANGED,
  INTERSECTION, SAFE IMPORTS (affected surfaces outside the write set), HELD
  (affected surfaces in the write set), and ACTION REQUIRED with the
  selective-accept command that keeps held surfaces.
- `accept <ghost> [--surface <path>]...` — import the ghost into the
  workspace as a **new** commit (authored by the ghost's author, trailers
  `Dd-Ghost:`/`Dd-Source-Commit:`), optionally only the named surfaces
  (§15). The patch is kept as evidence at
  `ghosts/<ghost>.accept.patch`. Refused with exit 10, changing nothing, if
  the imported paths have uncommitted workspace edits or the delta does not
  apply; combining both histories is reconciliation (Phase IV).
- `surfaces <session>` (Phase III) — the session's derived footprint from
  its workspace versus its base: changed files (committed, uncommitted, and
  new), Rust symbols it added/removed/modified, the names its changed code
  refers to, the external crates it uses, and files analyzed from the
  workspace `HEAD` because the working copy did not parse.
- `defer <ghost>` — acknowledge and continue; the ghost stays open and can
  be accepted or rejected later.
- `reject <ghost>` — decline for this session; canonical history is
  untouched.

A decision on a GHOST_PENDING session first moves it to ASSESSING; once no
ghost needing assessment is left unanswered (a deferral counts as an
answer), it returns to ACTIVE. Accepted and rejected ghosts are final
(exit 6 on a second decision). Every mutating command holds the session's
advisory lock (`sessions/<id>/lock`), so a running `watch` and a concurrent
`accept` never overwrite each other's state change.

Ids in all JSON output are the same `dd_<uuid>` / `gh_<uuid>` strings the
commands accept.

## Semantic assessment (Phase III)

Every ghost is assessed per surface — files, Rust symbols
(`rust:<crate>::<module>::<item>`, fields as `Type.field`), and manifest
entries (`cargo:<manifest>#<section>.<key>`) — against the session's
declared sets and its derived footprint (what `surfaces` shows). `ghosts`
reports, per ghost:

- `classification` — the strongest per-surface class: `independent`,
  `dependency_relevant`, `read_relevant`, `write_overlap`, `api_breaking`,
  or `unknown` (analysis could not rule a change out; never safe);
- `assessments` — every surface with its class and reason (human: the
  ASSESSMENT block lists the non-independent ones);
- `invalidated_assumptions` — what the ghost breaks in the session's code,
  with the referencing item and `file:line` (human: ASSUMPTION INVALIDATED);
- `api_breaking` / `api_additive` — public-API symbols changed;
- `safe_imports` / `breaking_imports` / `held` — the file-level import view
  for `accept --surface`: held files are the session's own; breaking imports
  carry a change that breaks the session's code; safe imports do neither.

Symbol evidence is syntactic (a `syn` AST with `use`-aware name
resolution), not compiler evidence; `accept --surface` imports whole files.
Compile/test validation of a candidate is Phase IV.

## Reconciliation (Phase IV)

- `reconcile <session> [--converge]` — observe canonical first (new ghosts
  needing assessment must be answered: the session must be ACTIVE), then,
  with the session frozen (its lock held):
  1. snapshot the workspace — including uncommitted and new files — through
     a private index (the workspace is untouched);
  2. merge base→canonical with base→snapshot per file: one-sided changes
     are taken; Rust files changed by both merge **item by item** (top-level
     items and `impl` members, exact spans), other files with
     `git merge-file`; an item both sides changed differently, a textual
     conflict, or delete-vs-modify is **contested** and never blended;
  3. write the candidate tree through a private index and commit it with
     the canonical `HEAD` as its only parent (no ref moves);
  4. validate it (below) → READY or VALIDATION_FAILED.

  Contested surfaces keep the session's version by default (the §52 `E'`)
  and the candidate is promotable only when every ghost touching them was
  accepted or rejected. `--converge` keeps canonical's version instead, so
  the uncontested work can be promoted while the contested work stays
  isolated in the session. The report lists files and how each was merged,
  contested surfaces and their resolution, the conflict graph (session code
  → canonical changes it depends on, marked breaking), blocking reasons,
  validation gates, and the **compatibility of the session's retained
  code**: each item it contributed is `compatible`, `incompatible` (with
  the compiler errors inside it), or `unverified`.
- `validate <session>` — re-run the gates for the current candidate.
- `candidate <session>` — show the latest candidate and its persisted
  validation.
- `discard <session>` — READY/VALIDATION_FAILED → ACTIVE.
- `promote <session> --lease <json>` — requires a READY, validated,
  promotable candidate; the workspace unchanged since the candidate was
  built; and a Locksmith lease grant held by the session's actor covering
  every file the promotion changes (paths, `file:` ids, `dir/` prefixes,
  or `*`). Canonical is then compare-and-swapped: `HEAD` must equal the
  candidate's target, and the checked-out branch fast-forwards to the
  candidate (`git merge --ff-only`, whose ref update verifies the old
  value). A full promotion completes the session and retires its workspace;
  a converge-mode promotion with contested surfaces returns it to ACTIVE
  with the contest still isolated. If canonical advanced, nothing changes,
  the candidate is marked stale, the session returns to ACTIVE, and the new
  commits are observed (exit 12).
- `accept <ghost> --symbol rust:<path>` — import only the named Rust items
  of a ghost into the workspace (a field or variant selects its struct or
  enum); items the session also changed are refused (exit 10).

Validation (`.doubledoor/validation.json`, all optional: `rust`,
`cargo_offline`, `cargo_test`, `cargo_timeout_secs`, `gates: [{name, argv,
timeout_secs, required}]`) runs in a throwaway worktree of the candidate:
git integrity → `cargo metadata --no-deps` → `cargo check --message-format
json` → `cargo test` → configured gates, each an argv with a timeout, within
the policy's `max_validation_runtime_secs`; after a failed required gate the
rest are recorded as skipped. The cargo target directory is kept per
session. Policy also caps `max_reconciliation_attempts`.

## Unified evidence (Phase V)

Every validation (in `reconcile` and `validate`) ends by collecting a
**unified evidence bundle** (`doubledoor.evidence/v1`) for the candidate,
persisted next to it and shown by `reconcile`/`candidate` (JSON field
`evidence`; human section `EVIDENCE=…`). Each item has a *kind* (source,
API, dependency, diagnostic, behavioural, architecture, relevance, repair),
a §17 *tier* (git_object, ast, dependency, compiler, static_analysis, test,
behavioural, heuristic, model), a *verdict* (info, supports, concern,
blocks), a subject surface, a location, and the exact command that
produced it.

- **Doubledoor's own evidence** (always present): candidate derivation and
  integrity, per-file merge method and contests (source); public API
  changes the promotion carries (API, syntactic); Cargo changes and
  retained code relying on canonical changes, settled by the compiler when
  it ran (dependency); `cargo check` and per-item compatibility
  (diagnostic); `cargo test` counts (behavioural).
- **Optional ELCI providers** (§21) run against private throwaway worktrees
  — never a session's or the canonical worktree (Kaptaind writes
  `.kaptaind/` into what it analyses):

  | provider | command | contributes | tier |
  |---|---|---|---|
  | Kaptaind | `kaptaind analyze -r <target+candidate staged>` | API break yes/no, projected semver bump, dependency/runtime scores | static_analysis |
  | Traci | `traci check . --format json` | diagnostics on promoted files | static_analysis |
  | Fract | `fract --format json index` | architecture health of promoted modules | static_analysis |
  | ZOC | `zoc --task <intent> . --dry-run` | where the actor's intent matters | heuristic (`model` in LLM mode) |
  | Sore / others | configured argv speaking `doubledoor.evidence/v1` | anything, capped | ≤ configured cap; model if probabilistic |
  | Padagonia | HTTP `POST /api/v1/nodes`, `/edges` | *sink*: publishes the evidence graph with provenance | — |

  A tool that is not installed is `unavailable`; a run whose output does
  not parse is `failed` and contributes nothing (a non-zero exit alone is
  not a failure — analysers exit non-zero when they find things). The
  exit code is kept on each item.
- **Unified verdict** — deterministic evidence only: `blocked` (a
  demonstrated problem: compile error, failing test, integrity) >
  `concerns` > `unverified` (a required kind has no deterministic check) >
  `clear`. Heuristic/model items are listed as ADVISORY and never change
  the verdict; external providers can never block (their `blocks` becomes
  `concern`) and cannot claim a tier above their configured cap. A BLOCKED
  candidate cannot be promoted.
- **Disagreements** between providers on the same API or dependency
  question are reported (PROVIDERS DISAGREE), not resolved by fiat.
- **§18**: the actor's stated intent (`actor_claim`) and any inferred
  intent are kept apart from the evidence, which is listed as
  deterministic vs probabilistic item ids.

`.doubledoor/evidence.json` (all optional): `kaptaind`/`traci`/`fract`
`{enabled, bin, timeout_secs}`; `zoc {…, mode: "dry-run"|"model"}`;
`padagonia {enabled, url, api_key_env, namespace}` (plain `http://`, key
read from the named environment variable); `external: {name: {name, argv,
max_tier, probabilistic, default_kind, timeout_secs}}` — an entry named
`sore` configures Sore. External providers receive
`DOUBLEDOOR_CANDIDATE_TREE`, `DOUBLEDOOR_REQUEST` (a JSON description of
the candidate), `DOUBLEDOOR_CANDIDATE`, `DOUBLEDOOR_TARGET`,
`DOUBLEDOOR_BASE`, and print `{"schema": "doubledoor.evidence/v1",
"items": [{kind, tier, verdict, subject, file, line, summary}]}`.

`doctor` includes an informational `evidence-providers` check listing which
providers are available; missing optional tools never fail it.

## Agent-native operation (Phase VI)

- `serve` / `rpc` — the `doubledoor.protocol/v1` agent protocol over a Unix
  socket (with an integrated watcher) or stdio; see
  [agent-protocol.md](agent-protocol.md).
- `capabilities` — the negotiated contract `hello` would return.
- `report <session>` — `doubledoor.report/v1` with `next_actions`.
- `policy <session> --auto-accept off|independent|dependency_relevant` and
  `open --auto-accept …` — automatic low-risk acceptance (journalled,
  decisions attributed `by: policy:<level>`).
- With `--format json`, every failure is a JSON error object on stderr
  (`{"error": {code, kind, retryable, message}}`), argument errors
  included.

## Exit codes (stable contract, §30)

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | usage error (bad flags, or no git repository resolvable); also `doctor`'s diagnostic code when any check fails |
| 2 | policy denied the session-open request |
| 3 | request contends but no valid contention notice was supplied |
| 4 | workspace isolation failed (session is FAILED_ISOLATION) |
| 5 | requested session not found |
| 6 | requested state transition is not legal from the current state |
| 7 | session metadata or journal is corrupt or schema-incompatible |
| 8 | internal error |
| 9 | subcommand is declared for a later phase (reserved; none as of Phase IV) |
| 10 | a change cannot be applied cleanly (uncommitted edits in the way, the delta does not apply, or the canonical worktree refuses the fast-forward); nothing was changed |
| 11 | the Locksmith lease does not permit the canonical mutation (missing, another holder, or not covering every changed file) |
| 12 | canonical advanced past the candidate's target (DD-I4); the candidate is stale and nothing was changed |

The same table ships in `doubledoor --help` (`after_long_help`).
