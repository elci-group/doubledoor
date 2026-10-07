# Doubledoor — State Machine and Invariants

Doubledoor is an agent-aware speculative concurrency layer: when Locksmith
denies direct mutation, an actor gets an isolated DoorSession instead of
idleness. Phase I implements safe isolation (§45); Phase II adds ghost
observation (§46).

## Lifecycle (Phase I + Phase II subset of §8)

```text
REQUESTED
    │
    ▼ (IsolationBegun)
ISOLATING
    │              \
    ▼ (WorkspaceReady) \ (IsolationFailed)
ACTIVE                FAILED_ISOLATION
    │                  (terminal)
    ├─ (AbandonRequested) ──────► ABANDONED (terminal)
    ├─ (PromotionCompleted) ────► COMPLETED (terminal)
    │
    ▼ (GhostObserved)
GHOST_PENDING
    │
    ▼ (AssessmentBegun)
ASSESSING
    │
    ▼ (AssessmentConcluded)
ACTIVE        (accept applied / defer / reject all conclude assessment)

AbandonRequested is also legal from REQUESTED, ISOLATING, GHOST_PENDING,
and ASSESSING.

GHOST_PENDING → ACTIVE without ASSESSING is ILLEGAL: no silent world-state
entry (DD-I2). A pending ghost must pass through explicit assessment.
```

Every transition goes through `DoorState::successor` /
`DoorSession::transition`. Illegal combinations return a typed
`TransitionError` and leave the session untouched — no silent coercion.
Every applied transition is appended to the session's journal *before* it is
considered effective (write → flush → fsync → append-only).

Phase IV (§16, §22, §23):

```text
ACTIVE ─(ReconcileBegun)→ RECONCILING ─(CandidateBuilt)→ VALIDATING
VALIDATING ─(GatesPassed)→ READY ─(PromotionBegun)→ PROMOTING
PROMOTING ─(PromotionCompleted)→ COMPLETED
PROMOTING ─(PartialPromotionCompleted)→ ACTIVE   (contest stays isolated)
PROMOTING ─(PromotionAborted)→ READY             (canonical unchanged)
VALIDATING ─(GatesFailed)→ VALIDATION_FAILED
READY | VALIDATION_FAILED ─(ValidationBegun)→ VALIDATING
READY | VALIDATION_FAILED ─(CandidateDiscarded)→ ACTIVE
RECONCILING | VALIDATING ─(ReconcileAborted)→ ACTIVE   (candidate incomplete)
VALIDATING | READY | PROMOTING ─(CanonicalAdvanced)→ ACTIVE (candidate stale)
```

There is no ACTIVE → COMPLETED shortcut: completion requires a validated
candidate. `CanonicalAdvanced` returns to ACTIVE rather than GHOST_PENDING
because the new commits may need no assessment; they are observed at once,
and anything relevant moves the session on to GHOST_PENDING through the
normal path. Abandonment is legal from every non-terminal state except
PROMOTING, which must resolve first.

## Ghost observation (§46, Phase II)

While a session speculates, external commits to the canonical repository
become *ghosts*: `doubledoor-ghost` walks `ghost_cursor..HEAD`, builds a
schema-versioned `GhostDiff` per commit (parsed unified diff, touched file
surfaces, evidence), classifies it against the session's read/write/
dependency sets (`SurfaceSet`, §11), and persists it under
`.doubledoor/sessions/<id>/ghosts/`. Classification is file-level: W_m∩W_s →
WriteOverlap, W_m∩R_s → ReadRelevant, W_m∩D_s → DependencyRelevant, none →
Independent, analysis failure (oversized/malformed diff) → Unknown — and
Unknown never means safe (§12). Cursor advance is the caller's job; the
session transitions `ACTIVE → GHOST_PENDING` only when a ghost needs the
actor's assessment (every class except Independent).

Observation order per session (under the session lock): generate and
persist ghosts (idempotent: a commit that already has a ghost is skipped) →
journal `GhostRecorded` per ghost → publish `ghost.available` → journal
`GhostCursorAdvanced` → if ACTIVE with an unanswered ghost that needs
assessment, journal and apply `GhostObserved`. A crash anywhere in that
sequence leaves the cursor behind, so the next pass re-covers the range and
records any ghost that was persisted but not journalled.

Assessment (`accept`/`defer`/`reject`): GHOST_PENDING →(AssessmentBegun)→
ASSESSING; the decision is journalled as `GhostAssessed`; when no ghost
needing assessment is unanswered, ASSESSING →(AssessmentConcluded)→ ACTIVE.
A deferral answers a ghost without closing it. Accepted/rejected ghosts are
final. A ghost may also be accepted or rejected from ACTIVE (a deferred or
independent ghost) without a lifecycle transition.

## Event stream (§13, Phase II)

`.doubledoor/events.jsonl` is append-only, one schema-versioned event per
line, written with a single `O_APPEND` write + `sync_data` so concurrent
writers never interleave. Sequence numbers are implicit (the n-th complete
line is event n); an unterminated final line is not yet an event. The
journal is the authority and is written first; the stream is delivery.

## Semantic surfaces (§47, Phase III)

`doubledoor-surface` parses Rust with `syn` (sources over 1 MiB, bracket
nesting over 256, or absurd unary chains are rejected before parsing: a
stack overflow would abort the process, §27). Module paths follow Cargo's
conventions from the manifests present (`src/a/b.rs` → `crate::a::b`, the
crate segment being the library name). Each item yields a symbol with a
normalized signature (formatting-, comment-, and doc-insensitive) and a body
hash; struct fields, enum variants, trait items, and impl methods are
member symbols. References (paths, method calls, field accesses, struct
literals, exhaustive matches, trait impls, macro bodies, derive lists) are
resolved through the file's `use` declarations and `crate`/`self`/`super`/
`Self`, and attributed to the enclosing item.

A commit's symbol changes are `Added`, `Removed`, `SignatureChanged`,
`VisibilityChanged`, or `BodyChanged`, with API impact `Breaking` (removed,
signature changed, visibility narrowed), `Additive`, or `Behavioral`. A
session's footprint is its workspace (including uncommitted and untracked
files) against its base: modified symbols, references, external crates,
touched packages. If the working copy does not parse, the workspace `HEAD`
(then the base) version is analyzed instead and the fallback is recorded.

Per changed symbol S:

| Session relation to S | Ghost change | Class |
|---|---|---|
| session also changed S | any | `write_overlap` |
| names S | breaking | `api_breaking` + assumption violation |
| struct literal of S's owner (no `..rest`) | field added / removed / retyped | `api_breaking` |
| exhaustive `match` on S's enum | variant added | `api_breaking` |
| `impl` of S's trait | required item added, item removed/changed | `api_breaking` |
| names S or its owner | otherwise | `dependency_relevant` |
| none | any | `independent` |

Cargo: a removed dependency, or a semver-incompatible change of one, that
the session's code uses (in a package it touches, or via
`workspace.dependencies`) is `api_breaking`; other changes to such manifests
are `dependency_relevant`; manifests of untouched packages are
`independent`. `unknown` is assigned when textual analysis failed, when a
changed file in a package the session touches could not be parsed, when the
session's own files could not be analyzed at all and the ghost changes
symbols there, or when the footprint is unavailable and the ghost has a
breaking change.

## Invariant mapping (§33)

| Invariant | Phase I mechanism |
|---|---|
| DD-I1 Canonical isolation | Speculative edits happen only in a `git worktree add --detach` workspace; nothing in Phase I writes to the user's worktree or branches |
| DD-I2 Explicit world-state transition | A session's workspace content changes only via `DoorSession::transition`, which requires a journal record; no external mutation enters silently. Phase II: observation never touches a workspace; a ghost enters only through `accept`, after `git apply --check`, as a new commit on the workspace's history |
| DD-I3 Stable base identity | `DoorSession::base` is set at creation and structurally immutable; tested across the lifecycle |
| DD-I5 Evidence preservation | Append-only per-session journal (JSONL, schema-versioned envelopes) + CBOR metadata; every transition carries prior/new state and timestamp. Phase II: persisted GhostDiffs with git-command evidence, `GhostAssessed` decisions, and the applied patch of every accept |
| DD-I6 Lock authority | `LockAuthority` trait; Phase I `NoticeAdapter` only *receives* `ContentionNotice`s — Doubledoor never fabricates grants or releases |
| DD-I7 Failure containment | Per-session journal and metadata directories; recovery operates per session. Phase II: per-session advisory locks; `watch` continues past a failing session |
| DD-I8 Unknown is not safe | `FAILED_ISOLATION` is terminal and recoverable only via explicit `recover`; schema-version rejection on read (§44). Phase II: an `unknown` ghost always needs assessment. Phase III: unanalyzable Rust or manifests in a package the session touches make the ghost `unknown` |

| DD-I4 Promotion freshness | Phase IV: a candidate's only parent is the canonical `HEAD` it was built on; promotion requires `HEAD` to still be that commit and fast-forwards from it (Git's ref transaction verifies the old value); anything else is exit 12 and a stale candidate |

## Reconciliation invariants (Phase IV)

- Candidates are built privately: snapshot and tree construction use
  temporary index files; no worktree, index, or ref of the session or the
  canonical repository changes (DD-I1, DD-I2).
- Contested items are never blended from both sides; which side is kept is
  recorded, and promotion of a session-kept contest requires the actor's
  final decision on every ghost touching it.
- Validation runs in a throwaway worktree and its report is persisted
  before the state change that depends on it; skipped or timed-out gates
  never count as passed (§43).
- Promotion requires validation, resolution, workspace freshness (same
  snapshot tree), and a Locksmith lease covering every changed file
  (DD-I6).

## Evidence invariants (Phase V, §17/§18/§21)

- The trust root is deterministic: the unified verdict is computed from
  git-object, AST, dependency, compiler, static-analysis, test, and
  behavioural evidence only. Heuristic and model-derived evidence is kept
  and shown but never decides, never fills a coverage gap, and never
  blocks.
- Only Doubledoor's own compiler, test, and integrity evidence can
  demonstrate a problem (`blocks`); external providers are capped at
  `concern` and at their configured tier.
- An actor's claim about its mutation is recorded separately from
  evidence (`MutationExplanation`).
- Providers analyse private throwaway worktrees; nothing they write reaches
  a session workspace or the canonical worktree. Their absence or failure
  is reported and changes nothing else.
- The evidence bundle is persisted before the lifecycle transition that
  follows validation, and journalled (`EvidenceRecorded`).

## Agent-native invariants (Phase VI, §50)

- Automatic acceptance is an explicit, journalled policy of the session
  (`PolicyChanged`), and every automatic decision is attributed
  (`by: policy:<level>`): DD-I2's "explicit world-state transition" holds
  per ghost, with the actor's consent given once.
- It runs only in ACTIVE, in canonical graph order, never ahead of an
  earlier unaccepted ghost touching the same files, and through the same
  checked file-level import as a manual accept.
- Ghost order everywhere is `git rev-list --topo-order` over the session's
  range — never timestamps.
- Protocol requests execute the CLI itself, so they inherit every lock,
  journal, and exit-code guarantee; parameters can never become flags.

## Recovery semantics (§24)

`doubledoor recover` replays each session's journal and reconciles metadata
with filesystem/Git reality:

- journal ends in `ISOLATING` but no workspace exists → `FAILED_ISOLATION`
  (workspace creation was interrupted; the temp-dir cleanup in
  `create_workspace` already removed partial state);
- `GHOST_PENDING` or `ASSESSING` without a workspace → `FAILED_ISOLATION`
  (the session still holds a workspace in those states; losing it is the
  same failure as an `ACTIVE` session losing it, DD-I8);
- torn final journal line (crash mid-append) → tolerated, tail logically
  truncated, reported;
- metadata unreadable or schema-incompatible → session reported, never
  silently assumed compatible;
- `RECONCILING` / `VALIDATING` → `ACTIVE`, candidate marked incomplete,
  validation worktree removed (§43);
- `PROMOTING` → decided from canonical `HEAD`: equal to the candidate →
  promotion completed (full or partial, as recorded); equal to the target →
  `READY`; anything else → candidate stale, `ACTIVE`. Git's ref update is
  atomic, so the ref is never half-moved;
- `COMPLETED` with a leftover workspace → workspace retired.

Canonical Git references are never modified by recovery.
