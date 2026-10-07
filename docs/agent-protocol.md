# Agent Protocol — `doubledoor.protocol/v1` (Phase VI, §50)

Coding agents use Doubledoor through structured JSON only: no human prose
needs to be parsed, ever. Schemas for every message live in `schemas/`.

## Transports

- `doubledoor serve [--socket PATH] [--watch-interval-ms N] [--no-watch]` —
  a Unix domain socket (default `.doubledoor/doubledoor.sock`, mode 0600)
  plus an integrated canonical-commit watcher, so subscribers receive
  events with no other process running. One server per socket; a second
  exits 6.
- `doubledoor rpc` — the same protocol on stdin/stdout, for agents that
  prefer to spawn a process.

Both speak line-delimited JSON: one request per line in; one response per
request (same `id`) plus subscription notifications out.

```json
{"id": 1, "method": "hello", "params": {"protocol_versions": ["doubledoor.protocol/v1"], "capabilities": ["events", "auto_accept"], "require": ["reconcile"]}}
{"id": 1, "ok": true, "result": {"protocol": "doubledoor.protocol/v1", "granted": ["events", "auto_accept"], "methods": [...], "capabilities": {...}, "exit_codes": [...], ...}}
{"id": 2, "method": "subscribe", "params": {"session": "dd_…", "events": ["ghost.available"]}}
{"id": 2, "ok": true, "result": {"subscription": 1}}
{"notification": "event", "subscription": 1, "event": {"seq": 7, "event": "ghost.available", "session": "dd_…", "classification": "write_overlap", ...}}
{"id": 3, "method": "inspect", "params": {"session": "dd_nope"}}
{"id": 3, "ok": false, "error": {"code": 5, "kind": "not_found", "retryable": false, "message": "not found: dd_nope"}}
```

## Negotiation (`hello`)

The first request on a connection must be `hello` (`describe` is always
allowed). The server answers with the negotiated protocol, the method
catalogue (names, typed parameters, result shape, streaming flag), schema
versions (`envelope`, `protocol`, `report`, `evidence`), capabilities with
live detail (auto-accept levels, reconcile modes, transports, which
evidence providers are installed, cargo availability), `granted` and
`unsupported` capabilities, the exit-code table, and limits. It fails with
`unsupported_protocol` if the client's versions do not include v1, and with
`capability_unavailable` if a `require`d capability is not offered.
`doubledoor capabilities --format json` prints the same without a
connection.

## Methods

`hello`, `describe`, `status`, `sessions`, `open` (the Locksmith notice is
passed as a JSON object), `inspect`, `report`, `observe`, `ghosts`,
`surfaces`, `accept` / `defer` / `reject`, `set_policy`, `reconcile`,
`validate`, `candidate`, `discard`, `promote` (the lease is a JSON object),
`abandon`, `events`, `subscribe` / `unsubscribe`, `recover`, `doctor`.
Parameters are named and typed; unknown or missing ones are
`invalid_params`. Each CLI-backed method runs the `doubledoor` binary as a
child process with `--format json`, so locking, journalling, and the
exit-code contract are identical to the CLI and a failing request cannot
affect the server. Parameter values become `--flag=value` arguments or
positionals after `--`: no value can be interpreted as a flag.

## Errors

`{"code", "kind", "retryable", "message"}`: `code` is the stable §30 exit
code, `kind` its stable name (`not_found`, `invalid_transition`,
`conflict`, `lock_not_permitted`, `canonical_advanced`, …) or a protocol
kind (`parse_error`, `unknown_method`, `invalid_params`,
`handshake_required`, `unsupported_protocol`, `capability_unavailable`,
`request_too_large`). The CLI emits the same object on stderr as
`{"error": {…}}` whenever `--format json` is given — including argument
parse failures.

## Reports and next actions

`report` returns `doubledoor.report/v1`: the session, its state and
auto-accept level, ghosts by status (pending ones with classification and
invalidated assumptions), the current candidate with validation and
evidence, and `next_actions` — each a method name, ready-to-send `params`,
the params the caller must still add (`requires`, e.g. `lease` for
`promote`), and a reason. An agent can drive the whole lifecycle by
calling `report` and executing one of its next actions.

## Automatic low-risk acceptance

A session's `auto_accept` level (`open --auto-accept`, `set_policy`) is an
explicit, journalled opt-in (`PolicyChanged`): `off` (default),
`independent` (every assessment independent, no invalidated assumption),
or `dependency_relevant` (additionally ghosts the session depends on, if
nothing they change is API-breaking for it and no dependency is removed or
moved across a semver boundary). It applies only while the session is
ACTIVE — never under a candidate — oldest ghost first in canonical graph
order, and never ahead of an earlier ghost the actor has not accepted that
touches the same files. Each acceptance is a normal file-level accept
(clean paths, `git apply --check` first), journalled as `GhostAssessed`
with `by: "policy:<level>"` and published as `ghost.assessed` with the same
`by`.
