# Doubledoor

**Agent-aware speculative concurrency for Git repositories.**

When several coding agents work in one repository, contention is inevitable:
two actors need the same file, a lease is denied, and one of them stalls.
Doubledoor turns that denial into progress. Instead of idling, the denied
actor receives an isolated, speculatively mutable session — a *DoorSession* —
in a detached Git worktree pinned to the canonical base commit. While it
works, Doubledoor watches canonical history, classifies every incoming commit
against the session's declared read/write/dependency surfaces (*ghost
observation*), helps the actor import or reconcile what changed, validates
the merged result, and promotes it back to canonical only when the evidence
is clear, the workspace is fresh, and a valid lease authorizes the mutation.

Nothing in a user's worktree, branch, or ref is ever mutated silently.
Every state change is journalled, every decision is attributable, and every
promotion is a compare-and-swap.

- **Crates:** 14 (workspace, `#![forbid(unsafe_code)]` throughout)
- **Protocol:** `doubledoor.protocol/v1` — line-delimited JSON over a Unix socket or stdio
- **License:** MIT (declared in `Cargo.toml`)
- **Status:** Phases I–VI implemented and tested (isolation, ghost observation, semantic surfaces, reconciliation, unified evidence, agent protocol); Phase VII (distributed remotes) in progress

---

## Table of contents

1. [Why Doubledoor](#why-doubledoor)
2. [How it works](#how-it-works)
3. [Feature highlights](#feature-highlights)
4. [Architecture](#architecture)
5. [Quick start](#quick-start)
6. [A full walkthrough](#a-full-walkthrough)
7. [Agent integration](#agent-integration)
8. [Distributed operation](#distributed-operation)
9. [Guarantees and invariants](#guarantees-and-invariants)
10. [The evidence model](#the-evidence-model)
11. [Exit codes](#exit-codes)
12. [On-disk layout and configuration](#on-disk-layout-and-configuration)
13. [Building and testing](#building-and-testing)
14. [Security posture](#security-posture)
15. [Documentation map](#documentation-map)

## Why Doubledoor

Agentic development breaks the assumptions classical concurrency control was
built on:

- **Actors are autonomous.** A queued agent cannot be "asked to wait" — it
  needs work it can do now.
- **Writes are declared, not discovered.** Sessions declare surfaces up
  front (`--read`, `--write`, `--depends`), so overlap is computable.
- **Canonical history moves underneath you.** While a session speculates,
  other actors commit. Every such commit that *could* matter must be
  observed, classified, and answered — never silently merged into a
  workspace.
- **Trust must be earned deterministically.** "It looks fine" is not a
  promotion criterion. A candidate is promoted only when deterministic
  evidence — Git integrity, AST analysis, the compiler, the test suite,
  configured gates — supports it.

Doubledoor integrates with [Locksmith](https://github.com/elci-group/locksmith),
which grants and denies leases and issues contention notices. Doubledoor
never fabricates a grant: it consumes notices, isolates the denied actor, and
requires a genuine lease before any canonical mutation.

## How it works

### 1. Isolation (Phase I)

`doubledoor open` validates the actor's Locksmith contention notice, evaluates
live policy, and creates a session:

- CBOR metadata in a schema-versioned envelope;
- an append-only JSONL journal (write → flush → fsync per event);
- a detached `git worktree` workspace at the canonical base commit.

The store lives under `.doubledoor/sessions/dd_<uuid>/` with its own
`.gitignore` (`*`), so a canonical `git add -A` never captures Doubledoor
state and the user's `.gitignore` is never touched.

### 2. Ghost observation (Phase II)

`doubledoor watch` polls canonical `HEAD`; every commit past a session's
ghost cursor becomes a persisted, classified **GhostDiff** and a
`ghost.available` event. Classes, strongest-first:

| Class | Meaning |
|---|---|
| `api_breaking` | breaks the session's code (API change it depends on) |
| `write_overlap` | changed files the session is writing |
| `read_relevant` | changed files the session reads |
| `dependency_relevant` | changed a dependency the session's code uses |
| `independent` | provably disjoint from the session |
| `unknown` | analysis could not rule it out — **never treated as safe** |

A session with a ghost needing assessment moves `ACTIVE → GHOST_PENDING`.
**Workspaces are never touched by observation.** The actor answers each ghost
with `accept` (import as a new commit, whole-file or `--symbol` selective),
`defer`, or `reject`; every decision is journalled and published on the local
event stream (`.doubledoor/events.jsonl`), which agents follow instead of
polling Git.

### 3. Semantic assessment (Phase III)

Classification is not just file-level. Doubledoor parses the session's Rust
with `syn` and computes:

- per-symbol changes (`Added`, `Removed`, `SignatureChanged`,
  `VisibilityChanged`, `BodyChanged`) and their API impact;
- the session's *footprint*: symbols it modified, names its changed code
  references, external crates it uses;
- **assumption invalidations**: the exact `file:line` where a ghost breaks a
  symbol the session's code relies on;
- Cargo-manifest analysis, including semver-incompatible dependency changes.

### 4. Reconciliation and promotion (Phase IV)

`doubledoor reconcile` builds a **candidate**: a private snapshot of the
workspace (including uncommitted files) merged with canonical changes —
one-sided changes taken, Rust files merged **item by item** at exact spans,
textual conflicts via `git merge-file`. Contested items are never blended:
one side is kept and recorded, and `--converge` flips the default so the
uncontested work can ship while the contest stays isolated in the session.

The candidate is validated in a throwaway worktree (Git integrity →
`cargo metadata` → `cargo check` → `cargo test` → configured gates, each with
a timeout) and reaches `READY`. `doubledoor promote` then requires:

- a `READY`, promotable candidate (every ghost contesting retained work answered);
- an unchanged workspace since the candidate was built;
- a Locksmith lease covering **every file the promotion changes**.

Promotion is a compare-and-swap: canonical `HEAD` must equal the candidate's
target, and the branch fast-forwards through Git's verified ref transaction.
If canonical advanced first, nothing changes, the candidate is marked stale,
and the new commits are observed (exit 12).

### 5. Unified evidence (Phase V)

Every validation collects a `doubledoor.evidence/v1` bundle: each item has a
kind, a tier (git object → AST → dependency → compiler → static analysis →
test → behavioural → heuristic → model), a verdict, a subject, and the exact
command that produced it. Optional external providers (Kaptaind, Traci, Fract,
ZOC, Sore, or any configured argv speaking the schema; Padagonia as an HTTP
sink) run against private throwaway worktrees. The unified verdict is
computed from **deterministic evidence only** — heuristic and model items are
advisory and never decide, and external providers can raise `concern` but
never `blocks`. A `BLOCKED` candidate cannot be promoted.

### 6. Agent-native operation (Phase VI)

Agents speak structured JSON, never prose: `doubledoor serve` (Unix socket
with an integrated watcher) or `doubledoor rpc` (stdio), speaking
`doubledoor.protocol/v1` — capability negotiation, typed methods, JSON
Schemas in [`schemas/`](schemas/), stable error objects, subscriptions, and a
machine-readable `report` whose `next_actions` let an agent drive the entire
lifecycle. Automatic low-risk acceptance is an explicit, journalled,
per-session policy (`policy --auto-accept off|independent|dependency_relevant`)
with every automatic decision attributed `by: policy:<level>`.

## Feature highlights

- **No silent mutation.** Observation never touches a workspace; a ghost
  enters only through an explicit, checked `accept` (`git apply --check`
  first) as a new commit on the workspace's history.
- **Canonical isolation.** Speculative work happens only in detached
  worktrees; Doubledoor creates no branches and moves no refs outside a
  verified promotion or recovery decision.
- **Crash-safe by construction.** Journal-before-effect for every
  transition; torn journal tails are tolerated and reported;
  `doubledoor recover` replays every journal and reconciles metadata with
  on-disk reality, never modifying canonical refs.
- **Concurrent-safe.** Per-session advisory locks; `watch` and a concurrent
  `accept` never overwrite each other; a failing session never stops
  observation of the others.
- **Human *and* machine output.** Styled tables and markers on a terminal;
  plain §31 shapes when piped or under `NO_COLOR`; `--format json` for every
  command, including errors (`{"error": {code, kind, retryable, message}}`
  on stderr).
- **Stable exit-code contract**, shipped in `--help` and in the protocol
  handshake.

## Architecture

The `doubledoor` binary is a thin orchestration layer; all domain logic lives
in library crates:

| Crate | Responsibility |
|---|---|
| `doubledoor-core` | contracts: states, transitions, evidence model (no I/O) |
| `doubledoor-git` | Git plumbing: worktrees, private indexes, refs |
| `doubledoor-locksmith` | contention-notice intake (`LockAuthority` trait) |
| `doubledoor-policy` | policy evaluation, auto-accept levels |
| `doubledoor-journal` | append-only journals and the event stream |
| `doubledoor-surface` | file/Rust-symbol/Cargo surface model and footprints |
| `doubledoor-ghost` | GhostDiff generation, classification, assessment, import |
| `doubledoor-reconcile` | candidate construction, item-level merge, conflict graph |
| `doubledoor-validate` | validation gates in throwaway worktrees |
| `doubledoor-evidence` | unified evidence bundles and external providers |
| `doubledoor-trust` | verdict computation from deterministic evidence |
| `doubledoor-protocol` | `doubledoor.protocol/v1`: envelopes, RPC, events |
| `doubledoor-remote` | remote Doubledoors over the protocol (Phase VII) |
| `doubledoor-cli` | the `doubledoor` binary |

## Quick start

Requirements: a recent stable Rust toolchain (the workspace uses edition
2021), a C compiler, Git, and Cargo. Building uses the sibling
[`form3`](https://github.com/elci-group/form3) library — check it out next to
this repository (`../3form`).

```sh
git clone git@github.com:elci-group/doubledoor.git
git clone git@github.com:elci-group/form3.git 3form  # required path dependency
cargo build --release -p doubledoor-cli              # target/release/doubledoor

target/release/doubledoor doctor                     # environment check
target/release/doubledoor --help                     # commands and exit codes
```

Then, from any repository where a lease was denied:

```sh
doubledoor open --actor beta --intent "Add rate limiting to the API" \
    --surface src/api.rs --notice notice.json
doubledoor watch --interval-ms 500                   # observe canonical commits
doubledoor ghosts dd_3f9c...                         # see and assess what moved
doubledoor status                                    # aggregate session status
```

## A full walkthrough

```sh
# 1. Contended: open an isolated session from a Locksmith contention notice.
doubledoor open --actor beta --intent "Refactor dispatch" \
    --surface src/router.rs --notice notice.json
# SESSION=dd_3f9c... STATE=ACTIVE WORKSPACE=.doubledoor/sessions/dd_3f9c.../workspace

# 2. Work speculatively inside the workspace (it is a normal Git worktree).
cd .doubledoor/sessions/dd_3f9c.../workspace
# ...edit, test, commit...
git commit -am "Refactor dispatch"

# 3. Meanwhile, canonical moved. Observe and assess the ghosts.
doubledoor watch --once
doubledoor ghosts dd_3f9c...
doubledoor accept gh_81b2... --surface src/api.rs   # import what you need
doubledoor defer  gh_90c3...                        # answer, keep it open
doubledoor reject gh_77d4...                        # decline for this session

# 4. Reconcile with canonical, validate, promote — lease in hand.
doubledoor reconcile dd_3f9c...
doubledoor candidate dd_3f9c...                     # conflict graph, gates, evidence
doubledoor promote dd_3f9c... --lease lease.json    # compare-and-swap against HEAD
```

At every step `--format json` gives the same story to scripts, and
`doubledoor report dd_3f9c...` gives an agent the exact next callable action.

## Agent integration

- **Transports:** `doubledoor serve` (Unix socket, default
  `.doubledoor/doubledoor.sock`, mode 0600, integrated watcher) or
  `doubledoor rpc` (stdio). Line-delimited JSON; one response per request.
- **Handshake:** the first request must be `hello` (`describe` is always
  allowed); the response carries the
  method catalogue, schema versions, live capabilities, the exit-code table,
  and limits. `doubledoor capabilities --format json` prints the same
  offline.
- **Schemas:** every request, response, notification, error, hello result,
  and report has a JSON Schema in [`schemas/`](schemas/).
- **Reports:** `report` returns `doubledoor.report/v1` with
  `next_actions` — ready-to-send params included — so an agent drives the
  lifecycle by repeatedly calling `report` and executing one action.
- **Protocol safety:** each CLI-backed method executes the `doubledoor`
  binary with `--format json`, so locking, journalling, and exit codes are
  identical to the CLI, parameters can never become flags, and a failing
  request cannot affect the server.
- **Auto-accept:** explicit, journalled policy levels (`off`,
  `independent`, `dependency_relevant`) applied oldest-ghost-first in
  canonical graph order, never ahead of an unanswered ghost touching the
  same files.

See [docs/agent-protocol.md](docs/agent-protocol.md) for the full protocol.

## Distributed operation

A **remote** (Phase VII) is another Doubledoor serving a clone of the same
canonical repository — on another host (via an SSH transport such as
`ssh -oBatchMode=yes host`) or locally. Clients speak the same Phase VI
protocol over the transport's stdin/stdout: the protocol is the wire, the
transport is just a pipe. Remote command lines contain only fixed tokens and
validated configuration values; everything a request carries travels as JSON
on stdin, so no shell ever concatenates untrusted values.

## Guarantees and invariants

The design is anchored by invariants tested in the suite
([docs/state-machine.md](docs/state-machine.md) maps each to its mechanism):

| Invariant | Guarantee |
|---|---|
| **DD-I1** Canonical isolation | Speculative edits happen only in detached worktrees; no branch is created, no ref moved |
| **DD-I2** Explicit transitions | Workspace content changes only through journalled state transitions; no silent world-state entry |
| **DD-I3** Stable base identity | A session's base commit is set at creation and structurally immutable |
| **DD-I4** Promotion freshness | A candidate's only parent is the `HEAD` it was built on; promotion compare-and-swaps against it, else exit 12 |
| **DD-I5** Evidence preservation | Append-only journals, persisted ghosts, and the applied patch of every accept |
| **DD-I6** Lock authority | Doubledoor only *receives* notices and *requires* leases; it never fabricates either |
| **DD-I7** Failure containment | Per-session journals, locks, and recovery; one failure never cascades |
| **DD-I8** Unknown is not safe | Unanalyzable changes classify as `unknown`, which always demands assessment |

Two structural rules worth knowing:

- `GHOST_PENDING → ACTIVE` without assessment is illegal — no silent entry
  back into the world.
- There is no `ACTIVE → COMPLETED` shortcut: completion requires a
  validated candidate.

## The evidence model

Each validation produces a unified bundle (`doubledoor.evidence/v1`):

- **Doubledoor's own evidence, always present:** candidate derivation and
  integrity; per-file merge method and contests; public-API changes; Cargo
  dependency changes; `cargo check` and per-item compatibility; `cargo test`
  counts.
- **Optional providers, capped:** Kaptaind (API break, semver projection),
  Traci (diagnostics), Fract (architecture health), ZOC (intent relevance),
  Sore or any external argv speaking the schema, Padagonia as an HTTP sink
  for the evidence graph. Providers analyse private throwaway worktrees —
  never a session's or the canonical worktree. Absent or failing providers
  are reported and change nothing.
- **Deterministic verdict:** `blocked` (demonstrated problem) >
  `concerns` > `unverified` (required coverage missing) > `clear`.
  Heuristic/model items are listed as advisory and never decide; external
  providers cap at `concern` and at their configured tier; disagreements
  between providers are reported, not resolved by fiat.

Configure providers in `.doubledoor/evidence.json`; gates and timeouts in
`.doubledoor/validation.json`. Both are optional.

## Exit codes

Stable contract (§30), identical for the CLI (`--format json` emits the same
code/kind on stderr) and the protocol:

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | usage error; also `doctor`'s diagnostic code when any check fails |
| 2 | policy denied the session-open request |
| 3 | request contends but no valid contention notice was supplied |
| 4 | workspace isolation failed (session is `FAILED_ISOLATION`) |
| 5 | requested session not found |
| 6 | requested state transition is not legal from the current state |
| 7 | session metadata or journal is corrupt or schema-incompatible |
| 8 | internal error |
| 9 | reserved: subcommand declared for a later phase |
| 10 | a change cannot be applied cleanly; nothing was changed |
| 11 | the Locksmith lease does not permit the canonical mutation |
| 12 | canonical advanced past the candidate's target; the candidate is stale |

## On-disk layout and configuration

All state lives under `.doubledoor/` in the repository — additions, never
mutations, and self-ignored so `git add -A` stays clean:

```text
.doubledoor/
├── doubledoor.sock                  # serve's socket (mode 0600)
├── events.jsonl                     # local event stream (append-only)
├── validation.json                  # optional: gates and timeouts
├── evidence.json                    # optional: evidence providers
├── remotes.json                     # optional: registered remote Doubledoors
└── sessions/
    └── dd_<uuid>/
        ├── metadata (CBOR envelope) # state, base, declared sets, policy
        ├── journal/events.jsonl     # append-only transition journal
        ├── ghosts/                  # persisted GhostDiffs and evidence
        ├── lock                     # per-session advisory lock
        └── workspace/               # detached git worktree at the base
```

## Building and testing

```sh
cargo fmt --check                     # formatting
cargo clippy --all-targets --all-features
cargo test --workspace                # unit + integration suites
cargo doc --no-deps --workspace
```

The repository carries a `deliver.toml` acceptance gate covering every
phase's deliverables and exit criteria:

```sh
deliver --spec deliver.toml --format json
```

Test layout: [`tests/integration/`](tests/integration/) (Phases I–II exit
criteria, isolation, ghosts), [`tests/semantic/`](tests/semantic/) (Phase
III symbol analysis), [`tests/reconcile/`](tests/reconcile/) (Phase IV
convergence and promotion), [`tests/evidence/`](tests/evidence/) (Phase V
unified evidence), [`tests/agent/`](tests/agent/) (Phase VI protocol),
[`tests/distributed/`](tests/distributed/) (Phase VII remotes).

## Security posture

- **`#![forbid(unsafe_code)]`** in every crate.
- **No shell interpolation:** subprocesses are fixed argv; no
  `sh -c` anywhere in the codebase (enforced by the deliver gate).
- **Protocol hardening:** parameters become `--flag=value` arguments or
  post-`--` positionals, so a value can never be interpreted as a flag;
  request size limits; handshake required before any other method.
- **Lease-gated mutation:** canonical changes require a Locksmith lease
  covering every changed file, verified against the actor's identity.
- **Compare-and-swap promotion:** ref updates verify their old value; a
  lost race is a reported stale candidate, never a silent overwrite.
- **Evidence integrity:** the trust root is deterministic; probabilistic
  tools can advise but cannot block, promote, or fill a coverage gap.
- **Provider isolation:** external analysers run in throwaway worktrees and
  can never write into a session workspace or the canonical worktree.

## Documentation map

| Document | Contents |
|---|---|
| [docs/cli.md](docs/cli.md) | every command, flag, and output shape (Phases I–VI) |
| [docs/state-machine.md](docs/state-machine.md) | lifecycle, ghost model, invariants, recovery semantics |
| [docs/agent-protocol.md](docs/agent-protocol.md) | `doubledoor.protocol/v1`, methods, errors, reports, auto-accept |
| [docs/exit-criterion.md](docs/exit-criterion.md) | per-phase acceptance scenarios and their tests |
| [schemas/](schemas/) | JSON Schemas for all protocol messages and reports |
| [deliver.toml](deliver.toml) | the acceptance gate: deliverables and commands |

## Contributing

1. Pick or raise an issue describing the behavioural change.
2. Write or extend the failing test first — each phase's exit criterion in
   [docs/exit-criterion.md](docs/exit-criterion.md) is a permanent fixture.
3. `cargo fmt --check && cargo clippy --all-targets --all-features && cargo
   test --workspace` must pass; the deliver gate must be green.
4. Keep the invariants: if a change touches isolation, journalling, or
   promotion, cite the invariant it preserves in the PR description.

## License

MIT — see the `license` field in [Cargo.toml](Cargo.toml).
