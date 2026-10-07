//! Agent-native operation (§31, §50 Phase VI): the `doubledoor.protocol/v1`
//! server (`serve` over a Unix socket, `rpc` over stdio), capability
//! negotiation (`hello`, `capabilities`), event subscription, and the
//! machine-readable session report with next actions (`report`).
//!
//! Every protocol request that maps to a CLI operation executes this very
//! binary as a child process with `--format json` against the served
//! repository. The protocol therefore inherits every guarantee of the CLI
//! unchanged — locking, journalling, exit-code contract — and a request
//! that crashes cannot take the server or other connections down (DD-I7).
//! Parameters become argument vectors (never a shell); flag values are
//! passed as `--flag=value` and positionals after `--`, so no parameter can
//! be read as a flag (a session id of `--repo=/` is just a bad id).
//!
//! Concurrency is one thread per connection plus one per subscription and
//! the watcher: a handful of local agents do not warrant an async runtime
//! (§42), and the core stays runtime-agnostic.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use doubledoor_core::{DoorSessionId, DoorState, GhostDecision};
use doubledoor_ghost::GhostStore;
use doubledoor_journal::{self as journal, EventCursor, EventLog};
use doubledoor_protocol::rpc::{
    exit_codes, ErrorObject, Hello, HelloResult, MethodSpec, NextAction, Notification, ParamSpec,
    Request, Response, MAX_REQUEST_BYTES, PROTOCOL, REPORT_SCHEMA,
};
use doubledoor_protocol::CURRENT_SCHEMA;

use crate::commands::{parse_session_id, resolve_git, session_dir, store_root, summary_of};
use crate::error::CliError;
use crate::Cli;

// ---------------------------------------------------------------------------
// Method catalogue
// ---------------------------------------------------------------------------

/// How a method's parameter becomes CLI arguments.
#[derive(Clone, Copy)]
enum Arg {
    /// Positional (after `--`).
    Pos,
    /// `--flag=value`.
    Flag(&'static str),
    /// Repeated `--flag=v` per array element.
    Many(&'static str),
    /// `--flag` when true.
    Bool(&'static str),
    /// A JSON object written to a private temp file, passed as `--flag=path`.
    Json(&'static str),
}

struct Method {
    name: &'static str,
    doc: &'static str,
    /// The CLI subcommand (`None` for native methods).
    command: Option<&'static str>,
    params: &'static [(&'static str, &'static str, bool, Arg, &'static str)],
    result: &'static str,
    streams: bool,
    /// Output is JSON lines, returned as an array.
    lines: bool,
}

const METHODS: &[Method] = &[
    Method {
        name: "hello",
        doc: "negotiate protocol version and capabilities (must be first)",
        command: None,
        params: &[
            (
                "protocol_versions",
                "string[]",
                false,
                Arg::Pos,
                "versions the client speaks",
            ),
            (
                "capabilities",
                "string[]",
                false,
                Arg::Pos,
                "capabilities wanted",
            ),
            (
                "require",
                "string[]",
                false,
                Arg::Pos,
                "capabilities required",
            ),
            ("agent", "object", false, Arg::Pos, "client identification"),
        ],
        result: "HelloResult",
        streams: false,
        lines: false,
    },
    Method {
        name: "describe",
        doc: "the method catalogue",
        command: None,
        params: &[],
        result: "MethodSpec[]",
        streams: false,
        lines: false,
    },
    Method {
        name: "status",
        doc: "aggregate status",
        command: Some("status"),
        params: &[],
        result: "StatusReport",
        streams: false,
        lines: false,
    },
    Method {
        name: "sessions",
        doc: "every session",
        command: Some("sessions"),
        params: &[],
        result: "SessionSummary[]",
        streams: false,
        lines: false,
    },
    Method {
        name: "open",
        doc: "open a DoorSession (requires a Locksmith contention notice object)",
        command: Some("open"),
        params: &[
            ("actor", "string", true, Arg::Flag("--actor"), "actor id"),
            (
                "intent",
                "string",
                true,
                Arg::Flag("--intent"),
                "the actor's intent",
            ),
            (
                "surfaces",
                "string[]",
                true,
                Arg::Many("--surface"),
                "requested surfaces",
            ),
            (
                "read",
                "string[]",
                false,
                Arg::Many("--read"),
                "read set paths",
            ),
            (
                "write",
                "string[]",
                false,
                Arg::Many("--write"),
                "write set paths",
            ),
            (
                "depends",
                "string[]",
                false,
                Arg::Many("--depends"),
                "dependency set paths",
            ),
            (
                "notice",
                "object",
                true,
                Arg::Json("--notice"),
                "Locksmith ContentionNotice",
            ),
            (
                "auto_accept",
                "string",
                false,
                Arg::Flag("--auto-accept"),
                "off | independent | dependency_relevant",
            ),
        ],
        result: "SessionSummary",
        streams: false,
        lines: false,
    },
    Method {
        name: "inspect",
        doc: "one session in detail",
        command: Some("inspect"),
        params: &[("session", "string", true, Arg::Pos, "session id")],
        result: "SessionSummary+",
        streams: false,
        lines: false,
    },
    Method {
        name: "report",
        doc: "machine-readable session report with next actions",
        command: Some("report"),
        params: &[("session", "string", true, Arg::Pos, "session id")],
        result: REPORT_SCHEMA,
        streams: false,
        lines: false,
    },
    Method {
        name: "observe",
        doc: "one observation pass over every live session",
        command: Some("watch"),
        params: &[],
        result: "SessionObservation[]",
        streams: false,
        lines: true,
    },
    Method {
        name: "ghosts",
        doc: "observe, then list a session's ghosts",
        command: Some("ghosts"),
        params: &[
            ("session", "string", true, Arg::Pos, "session id"),
            (
                "no_observe",
                "boolean",
                false,
                Arg::Bool("--no-observe"),
                "skip observation",
            ),
        ],
        result: "GhostView[]",
        streams: false,
        lines: false,
    },
    Method {
        name: "surfaces",
        doc: "a session's derived footprint",
        command: Some("surfaces"),
        params: &[("session", "string", true, Arg::Pos, "session id")],
        result: "SurfacesView",
        streams: false,
        lines: false,
    },
    Method {
        name: "accept",
        doc: "accept a ghost (optionally only some files or Rust items)",
        command: Some("accept"),
        params: &[
            ("ghost", "string", true, Arg::Pos, "ghost id"),
            (
                "surfaces",
                "string[]",
                false,
                Arg::Many("--surface"),
                "files",
            ),
            (
                "symbols",
                "string[]",
                false,
                Arg::Many("--symbol"),
                "rust: symbols",
            ),
        ],
        result: "DecisionView",
        streams: false,
        lines: false,
    },
    Method {
        name: "defer",
        doc: "defer a ghost",
        command: Some("defer"),
        params: &[("ghost", "string", true, Arg::Pos, "ghost id")],
        result: "DecisionView",
        streams: false,
        lines: false,
    },
    Method {
        name: "reject",
        doc: "reject a ghost for this session",
        command: Some("reject"),
        params: &[("ghost", "string", true, Arg::Pos, "ghost id")],
        result: "DecisionView",
        streams: false,
        lines: false,
    },
    Method {
        name: "set_policy",
        doc: "set the session's automatic low-risk acceptance level",
        command: Some("policy"),
        params: &[
            ("session", "string", true, Arg::Pos, "session id"),
            (
                "auto_accept",
                "string",
                true,
                Arg::Flag("--auto-accept"),
                "off | independent | dependency_relevant",
            ),
        ],
        result: "PolicyResult",
        streams: false,
        lines: false,
    },
    Method {
        name: "reconcile",
        doc: "build and validate a candidate",
        command: Some("reconcile"),
        params: &[
            ("session", "string", true, Arg::Pos, "session id"),
            (
                "converge",
                "boolean",
                false,
                Arg::Bool("--converge"),
                "contested surfaces keep canonical",
            ),
        ],
        result: "CandidateReport",
        streams: false,
        lines: false,
    },
    Method {
        name: "validate",
        doc: "re-validate the current candidate",
        command: Some("validate"),
        params: &[("session", "string", true, Arg::Pos, "session id")],
        result: "CandidateReport",
        streams: false,
        lines: false,
    },
    Method {
        name: "candidate",
        doc: "the latest candidate, validation, and evidence",
        command: Some("candidate"),
        params: &[("session", "string", true, Arg::Pos, "session id")],
        result: "CandidateReport",
        streams: false,
        lines: false,
    },
    Method {
        name: "discard",
        doc: "set the candidate aside",
        command: Some("discard"),
        params: &[("session", "string", true, Arg::Pos, "session id")],
        result: "SessionSummary",
        streams: false,
        lines: false,
    },
    Method {
        name: "promote",
        doc: "CAS-promote a READY candidate (requires a Locksmith lease grant object)",
        command: Some("promote"),
        params: &[
            ("session", "string", true, Arg::Pos, "session id"),
            (
                "lease",
                "object",
                true,
                Arg::Json("--lease"),
                "Locksmith LeaseGrant",
            ),
        ],
        result: "PromotionResult",
        streams: false,
        lines: false,
    },
    Method {
        name: "abandon",
        doc: "abandon a session",
        command: Some("abandon"),
        params: &[("session", "string", true, Arg::Pos, "session id")],
        result: "SessionSummary",
        streams: false,
        lines: false,
    },
    Method {
        name: "events",
        doc: "read the event log",
        command: Some("events"),
        params: &[
            (
                "since",
                "integer",
                false,
                Arg::Flag("--since"),
                "after this seq",
            ),
            (
                "session",
                "string",
                false,
                Arg::Flag("--session"),
                "only this session",
            ),
            (
                "max",
                "integer",
                false,
                Arg::Flag("--max"),
                "at most this many",
            ),
        ],
        result: "SequencedEvent[]",
        streams: false,
        lines: true,
    },
    Method {
        name: "subscribe",
        doc: "stream events as notifications",
        command: None,
        params: &[
            ("since", "integer", false, Arg::Pos, "after this seq"),
            ("session", "string", false, Arg::Pos, "only this session"),
            (
                "events",
                "string[]",
                false,
                Arg::Pos,
                "only these event names",
            ),
        ],
        result: "{subscription}",
        streams: true,
        lines: false,
    },
    Method {
        name: "unsubscribe",
        doc: "stop a subscription",
        command: None,
        params: &[("subscription", "integer", true, Arg::Pos, "subscription id")],
        result: "{stopped}",
        streams: false,
        lines: false,
    },
    Method {
        name: "recover",
        doc: "crash recovery",
        command: Some("recover"),
        params: &[],
        result: "RecoveryReport",
        streams: false,
        lines: false,
    },
    Method {
        name: "doctor",
        doc: "environment diagnostics",
        command: Some("doctor"),
        params: &[],
        result: "DoctorReport",
        streams: false,
        lines: false,
    },
];

fn specs() -> Vec<MethodSpec> {
    METHODS
        .iter()
        .map(|m| MethodSpec {
            name: m.name.to_string(),
            doc: m.doc.to_string(),
            params: m
                .params
                .iter()
                .map(|(n, t, r, _, d)| ParamSpec {
                    name: n.to_string(),
                    ty: t.to_string(),
                    required: *r,
                    doc: d.to_string(),
                })
                .collect(),
            result: m.result.to_string(),
            streams: m.streams,
        })
        .collect()
}

/// Capabilities this server offers.
const CAPABILITIES: &[&str] = &[
    "sessions",
    "ghosts",
    "events",
    "auto_accept",
    "symbol_import",
    "semantic_surfaces",
    "reconcile",
    "evidence",
    "promotion",
    "reports",
];

fn capability_detail(repo: &Path) -> Value {
    let cfg =
        doubledoor_evidence::EvidenceConfig::load(&repo.join(".doubledoor")).unwrap_or_default();
    let avail = |name: &str, bin: Option<&Path>| {
        doubledoor_evidence::config::resolve_bin(name, bin).is_some()
    };
    json!({
        "offered": CAPABILITIES,
        "auto_accept_levels": ["off", "independent", "dependency_relevant"],
        "reconcile_modes": ["speculative", "converge"],
        "transports": ["unix_socket", "stdio"],
        "evidence_providers": {
            "kaptaind": avail("kaptaind", cfg.kaptaind.bin.as_deref()),
            "traci": avail("traci", cfg.traci.bin.as_deref()),
            "fract": avail("fract", cfg.fract.bin.as_deref()),
            "zoc": avail("zoc", cfg.zoc.tool.bin.as_deref()),
            "sore": cfg.external.contains_key("sore"),
            "padagonia": cfg.padagonia.enabled,
        },
        "cargo": avail("cargo", None),
    })
}

fn hello_result(hello: &Hello, repo: &Path) -> Result<HelloResult, ErrorObject> {
    if !hello.protocol_versions.is_empty() && !hello.protocol_versions.iter().any(|v| v == PROTOCOL)
    {
        return Err(ErrorObject::protocol(
            "unsupported_protocol",
            format!(
                "this server speaks {PROTOCOL} only; client offered {:?}",
                hello.protocol_versions
            ),
        ));
    }
    let granted: Vec<String> = hello
        .capabilities
        .iter()
        .filter(|c| CAPABILITIES.contains(&c.as_str()))
        .cloned()
        .collect();
    let unsupported: Vec<String> = hello
        .capabilities
        .iter()
        .chain(hello.require.iter())
        .filter(|c| !CAPABILITIES.contains(&c.as_str()))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if let Some(missing) = hello
        .require
        .iter()
        .find(|c| !CAPABILITIES.contains(&c.as_str()))
    {
        return Err(ErrorObject::protocol(
            "capability_unavailable",
            format!("required capability `{missing}` is not offered"),
        ));
    }
    Ok(HelloResult {
        protocol: PROTOCOL.to_string(),
        server: json!({ "name": "doubledoor", "version": env!("CARGO_PKG_VERSION"), "repo": repo }),
        schemas: json!({
            "envelope": CURRENT_SCHEMA.to_string(),
            "protocol": PROTOCOL,
            "report": REPORT_SCHEMA,
            "evidence": doubledoor_core::evidence::EVIDENCE_SCHEMA,
            "events": "SequencedEvent (tagged by `event`)",
        }),
        methods: specs(),
        capabilities: capability_detail(repo),
        granted,
        unsupported,
        exit_codes: exit_codes(),
        limits: json!({ "max_request_bytes": MAX_REQUEST_BYTES }),
    })
}

/// `doubledoor capabilities`: what `hello` would grant, without a session.
pub fn capabilities(cli: &Cli) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let r =
        hello_result(&Hello::default(), git.repo_root()).map_err(|e| CliError::Usage(e.message))?;
    crate::output::print_json(&r)
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// `doubledoor report <session>`: the `doubledoor.report/v1` document —
/// state, ghosts by status, the current candidate with validation and
/// evidence, and the next actions as callable methods (§50).
pub fn report(cli: &Cli, id: &str) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let sid = parse_session_id(id)?;
    let dir = session_dir(&git, &sid);
    if !dir.is_dir() {
        return Err(CliError::NotFound(sid.to_string()));
    }
    let session = journal::load_metadata(&dir)?;
    let records = journal::replay(&dir)?.records;
    let decisions = journal::ghost_decisions(&records);
    let ghosts = GhostStore::new(&git, &sid)?.list()?;
    let mut pending = Vec::new();
    let (mut deferred, mut accepted, mut rejected, mut independent) = (0, 0, 0, 0);
    for g in &ghosts {
        match decisions.get(&g.id) {
            Some(GhostDecision::Accepted { .. }) => accepted += 1,
            Some(GhostDecision::Rejected) => rejected += 1,
            Some(GhostDecision::Deferred) => deferred += 1,
            None if g.classification.needs_assessment() => pending.push(g),
            None => independent += 1,
        }
    }
    let candidate = crate::reconcile::latest_candidate(&dir)?;
    let evidence = candidate
        .as_ref()
        .and_then(|c| crate::reconcile::load_evidence(&dir, c));

    let s = json!(sid);
    let mut next: Vec<NextAction> = Vec::new();
    let act = |action: &str, params: Value, reason: String| NextAction {
        action: action.to_string(),
        params,
        requires: Vec::new(),
        reason,
    };
    match session.state {
        DoorState::GhostPending | DoorState::Assessing => {
            for g in &pending {
                let why = format!(
                    "ghost {} is {}{}",
                    g.id,
                    g.classification.wire_name(),
                    g.invalidated_assumptions
                        .first()
                        .map(|v| format!(": {}", v.description))
                        .unwrap_or_default()
                );
                for a in ["accept", "defer", "reject"] {
                    next.push(act(a, json!({ "ghost": g.id }), why.clone()));
                }
            }
        }
        DoorState::Active => {
            next.push(act(
                "reconcile",
                json!({ "session": s }),
                "build and validate a candidate".into(),
            ));
            if session.auto_accept == doubledoor_core::AutoAcceptLevel::Off && independent > 0 {
                next.push(act(
                    "set_policy",
                    json!({ "session": s, "auto_accept": "independent" }),
                    format!("{independent} independent ghost(s) could be imported automatically"),
                ));
            }
        }
        DoorState::Ready => {
            let c = candidate.as_ref();
            let blocked = evidence.as_ref().is_some_and(|e| {
                e.assessment.verdict == doubledoor_core::evidence::UnifiedVerdict::Blocked
            });
            match c {
                Some(c) if c.promotable && !blocked => next.push(NextAction {
                    action: "promote".into(),
                    params: json!({ "session": s }),
                    requires: vec!["lease".into()],
                    reason: format!(
                        "candidate {} is validated and promotable; a Locksmith lease must cover: {}",
                        c.id,
                        c.promotes_files.join(", ")
                    ),
                }),
                Some(c) => next.push(act(
                    "discard",
                    json!({ "session": s }),
                    format!("candidate {} is not promotable: {}", c.id, c.blocking.join("; ")),
                )),
                None => {}
            }
            if !next.iter().any(|n| n.action == "discard") {
                next.push(act(
                    "discard",
                    json!({ "session": s }),
                    "set the candidate aside".into(),
                ));
            }
        }
        DoorState::ValidationFailed => {
            next.push(act(
                "candidate",
                json!({ "session": s }),
                "inspect the failed gates and compatibility".into(),
            ));
            next.push(act(
                "discard",
                json!({ "session": s }),
                "resume speculative work".into(),
            ));
        }
        DoorState::Reconciling | DoorState::Validating | DoorState::Promoting => {
            next.push(act(
                "wait",
                json!({ "session": s }),
                format!("the session is {}", session.state),
            ));
        }
        DoorState::Requested | DoorState::Isolating => {
            next.push(act(
                "recover",
                json!({}),
                "the session never finished isolation".into(),
            ));
        }
        _ => {}
    }

    let rec_json = |dir: &Path, c: &doubledoor_reconcile::Candidate| -> Value {
        doubledoor_reconcile::store::load_json::<Value>(
            &doubledoor_reconcile::candidate::candidate_dir(dir),
            &format!("{}.validation", c.id),
        )
        .unwrap_or(Value::Null)
    };
    crate::output::print_json(&json!({
        "schema": REPORT_SCHEMA,
        "session": summary_of(&session),
        "state": session.state,
        "auto_accept": session.auto_accept,
        "ghosts": {
            "pending": pending.iter().map(|g| json!({
                "id": g.id,
                "classification": g.classification.wire_name(),
                "invalidated_assumptions": g.invalidated_assumptions,
            })).collect::<Vec<_>>(),
            "deferred": deferred,
            "accepted": accepted,
            "rejected": rejected,
            "independent_undecided": independent,
        },
        "candidate": candidate.as_ref().map(|c| json!({
            "record": c,
            "validation": rec_json(&dir, c),
            "evidence": evidence,
        })),
        "next_actions": next,
    }))
}

// ---------------------------------------------------------------------------
// Protocol server
// ---------------------------------------------------------------------------

type Sink = Arc<Mutex<Box<dyn Write + Send>>>;

fn send(out: &Sink, v: &impl serde::Serialize) -> bool {
    let Ok(line) = serde_json::to_string(v) else {
        return false;
    };
    let Ok(mut w) = out.lock() else { return false };
    writeln!(w, "{line}").and_then(|()| w.flush()).is_ok()
}

struct Connection {
    repo: PathBuf,
    exe: PathBuf,
    out: Sink,
    greeted: bool,
    next_sub: Arc<AtomicU64>,
    subs: Vec<(u64, Arc<AtomicBool>)>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        for (_, stop) in &self.subs {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

/// Serve one connection until EOF: requests in, responses and
/// notifications out.
fn handle<R: BufRead>(repo: PathBuf, mut input: R, out: Sink) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("doubledoor"));
    let mut conn = Connection {
        repo,
        exe,
        out,
        greeted: false,
        next_sub: Arc::new(AtomicU64::new(1)),
        subs: Vec::new(),
    };
    loop {
        let mut buf = Vec::new();
        let n = match std::io::Read::take(&mut input, MAX_REQUEST_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf)
        {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            return;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            send(
                &conn.out,
                &Response::err(
                    Value::Null,
                    ErrorObject::protocol(
                        "request_too_large",
                        format!("requests are limited to {MAX_REQUEST_BYTES} bytes"),
                    ),
                ),
            );
            return;
        }
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) => conn.dispatch(req),
            Err(e) => Some(Response::err(
                Value::Null,
                ErrorObject::protocol("parse_error", e.to_string()),
            )),
        };
        if let Some(resp) = resp {
            if !send(&conn.out, &resp) {
                return;
            }
        }
    }
}

impl Connection {
    /// `None` when the response was already sent (subscriptions send theirs
    /// before any notification can follow).
    fn dispatch(&mut self, req: Request) -> Option<Response> {
        let is_sub = req.method == "subscribe";
        let resp = self.respond(req);
        if is_sub && resp.ok {
            return None;
        }
        Some(resp)
    }

    fn respond(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        let Some(method) = METHODS.iter().find(|m| m.name == req.method) else {
            return Response::err(
                id,
                ErrorObject::protocol(
                    "unknown_method",
                    format!("no method `{}` (see `describe`)", req.method),
                ),
            );
        };
        if !self.greeted && !matches!(method.name, "hello" | "describe") {
            return Response::err(
                id,
                ErrorObject::protocol("handshake_required", "send `hello` first"),
            );
        }
        let params = if req.params.is_null() {
            json!({})
        } else {
            req.params.clone()
        };
        let Some(obj) = params.as_object() else {
            return Response::err(
                id,
                ErrorObject::protocol("invalid_params", "params must be an object"),
            );
        };
        if let Some(unknown) = obj
            .keys()
            .find(|k| !method.params.iter().any(|p| p.0 == k.as_str()))
        {
            return Response::err(
                id,
                ErrorObject::protocol(
                    "invalid_params",
                    format!("`{}` takes no parameter `{unknown}`", method.name),
                ),
            );
        }
        if let Some(missing) = method.params.iter().find(|p| p.2 && !obj.contains_key(p.0)) {
            return Response::err(
                id,
                ErrorObject::protocol(
                    "invalid_params",
                    format!("`{}` requires `{}`", method.name, missing.0),
                ),
            );
        }
        match method.name {
            "hello" => match serde_json::from_value::<Hello>(params) {
                Ok(h) => match hello_result(&h, &self.repo) {
                    Ok(r) => {
                        self.greeted = true;
                        Response::ok(id, serde_json::to_value(r).unwrap_or(Value::Null))
                    }
                    Err(e) => Response::err(id, e),
                },
                Err(e) => Response::err(id, ErrorObject::protocol("invalid_params", e.to_string())),
            },
            "describe" => Response::ok(id, serde_json::to_value(specs()).unwrap_or(Value::Null)),
            "subscribe" => self.subscribe(id, obj),
            "unsubscribe" => {
                let Some(sub) = obj.get("subscription").and_then(Value::as_u64) else {
                    return Response::err(
                        id,
                        ErrorObject::protocol(
                            "invalid_params",
                            "`subscription` must be an integer",
                        ),
                    );
                };
                let found = self.subs.iter().find(|(s, _)| *s == sub);
                match found {
                    Some((_, stop)) => {
                        stop.store(true, Ordering::SeqCst);
                        Response::ok(id, json!({ "stopped": sub }))
                    }
                    None => Response::err(
                        id,
                        ErrorObject::from_code(5, format!("no subscription {sub}")),
                    ),
                }
            }
            _ => self.run(id, method, obj),
        }
    }

    fn subscribe(&mut self, id: Value, p: &serde_json::Map<String, Value>) -> Response {
        let since = p.get("since").and_then(Value::as_u64).unwrap_or(0);
        let session = match p
            .get("session")
            .and_then(Value::as_str)
            .map(DoorSessionId::parse)
        {
            None => None,
            Some(Ok(s)) => Some(s),
            Some(Err(_)) => {
                return Response::err(id, ErrorObject::from_code(5, "unknown session id"))
            }
        };
        let names: Option<Vec<String>> = p.get("events").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect()
        });
        let sub = self.next_sub.fetch_add(1, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        self.subs.push((sub, stop.clone()));
        let out = self.out.clone();
        let log = EventLog::new(&self.repo.join(".doubledoor"));
        // The response goes out first; the tailing thread starts after it.
        let resp = Response::ok(id, json!({ "subscription": sub }));
        send(&out, &resp);
        std::thread::spawn(move || {
            let mut cursor = EventCursor::default();
            while !stop.load(Ordering::SeqCst) {
                match log.read_new(&mut cursor) {
                    Ok(evs) => {
                        for ev in evs {
                            if ev.seq <= since || session.is_some_and(|s| ev.event.session() != s) {
                                continue;
                            }
                            if let Some(names) = &names {
                                let name = serde_json::to_value(&ev.event)
                                    .ok()
                                    .and_then(|v| v["event"].as_str().map(ToString::to_string))
                                    .unwrap_or_default();
                                if !names.contains(&name) {
                                    continue;
                                }
                            }
                            let n = Notification {
                                notification: "event".into(),
                                subscription: sub,
                                event: ev,
                            };
                            if !send(&out, &n) {
                                return;
                            }
                        }
                    }
                    Err(_) => return,
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        // Already sent; `dispatch` suppresses the ok response.
        Response {
            id: Value::Null,
            ok: true,
            result: None,
            error: None,
        }
    }

    fn run(&self, id: Value, m: &Method, p: &serde_json::Map<String, Value>) -> Response {
        let command = m.command.expect("CLI-backed method");
        let tmp_dir = self.repo.join(".doubledoor").join("tmp");
        let mut temps = Vec::new();
        let mut flags: Vec<String> = Vec::new();
        let mut positionals: Vec<String> = Vec::new();
        for (name, ty, _, arg, _) in m.params {
            let Some(v) = p.get(*name) else { continue };
            let bad = |why: &str| {
                Response::err(
                    id.clone(),
                    ErrorObject::protocol("invalid_params", format!("`{name}` {why}")),
                )
            };
            let scalar = |v: &Value| -> Option<String> {
                match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) if *ty == "integer" => n.as_u64().map(|n| n.to_string()),
                    _ => None,
                }
            };
            match arg {
                Arg::Pos => match scalar(v) {
                    Some(s) => positionals.push(s),
                    None => return bad(&format!("must be a {ty}")),
                },
                Arg::Flag(f) => match scalar(v) {
                    Some(s) => flags.push(format!("{f}={s}")),
                    None => return bad(&format!("must be a {ty}")),
                },
                Arg::Many(f) => match v.as_array() {
                    Some(a) => {
                        for x in a {
                            match x.as_str() {
                                Some(s) => flags.push(format!("{f}={s}")),
                                None => return bad("must be an array of strings"),
                            }
                        }
                    }
                    None => return bad("must be an array of strings"),
                },
                Arg::Bool(f) => match v.as_bool() {
                    Some(true) => flags.push(f.to_string()),
                    Some(false) => {}
                    None => return bad("must be a boolean"),
                },
                Arg::Json(f) => {
                    if !v.is_object() {
                        return bad("must be an object");
                    }
                    let _ = std::fs::create_dir_all(&tmp_dir);
                    let path = tmp_dir.join(format!("rpc-{}.json", uuid_like()));
                    if std::fs::write(&path, v.to_string()).is_err() {
                        return Response::err(
                            id,
                            ErrorObject::from_code(8, "cannot stage parameter file"),
                        );
                    }
                    flags.push(format!("{f}={}", path.display()));
                    temps.push(path);
                }
            }
        }
        let mut argv: Vec<String> = vec![
            format!("--repo={}", self.repo.display()),
            "--format=json".into(),
            command.into(),
        ];
        if command == "watch" {
            argv.push("--once".into());
        }
        argv.extend(flags);
        if !positionals.is_empty() {
            argv.push("--".into());
            argv.extend(positionals);
        }
        let out = Command::new(&self.exe)
            .args(&argv)
            .current_dir(&self.repo)
            .output();
        for t in temps {
            let _ = std::fs::remove_file(t);
        }
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                return Response::err(
                    id,
                    ErrorObject::from_code(8, format!("cannot run doubledoor: {e}")),
                )
            }
        };
        let code = out.status.code().unwrap_or(8) as u8;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: Option<Value> = if m.lines {
            stdout
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(serde_json::from_str::<Value>)
                .collect::<Result<Vec<_>, _>>()
                .ok()
                .map(Value::Array)
        } else if stdout.trim().is_empty() {
            None
        } else {
            serde_json::from_str(stdout.trim()).ok()
        };
        // doctor reports failed checks with exit 1 and a full report.
        if code == 0 || (command == "doctor" && parsed.is_some()) {
            return match parsed {
                Some(v) => Response::ok(id, v),
                None => Response::err(
                    id,
                    ErrorObject::from_code(8, "command produced no JSON result"),
                ),
            };
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        let error = stderr
            .lines()
            .rev()
            .find_map(|l| serde_json::from_str::<Value>(l).ok())
            .and_then(|v| serde_json::from_value::<ErrorObject>(v["error"].clone()).ok())
            .unwrap_or_else(|| ErrorObject::from_code(code, stderr.trim().to_string()));
        Response::err(id, error)
    }
}

fn uuid_like() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// `doubledoor rpc`: the protocol over stdin/stdout for this repository.
pub fn rpc(cli: &Cli) -> Result<(), CliError> {
    let git = resolve_git(cli)?;
    let out: Sink = Arc::new(Mutex::new(Box::new(std::io::stdout())));
    handle(
        git.repo_root().to_path_buf(),
        BufReader::new(std::io::stdin()),
        out.clone(),
    );
    Ok(())
}

/// `doubledoor serve`: the protocol on a Unix domain socket (mode 0600),
/// with an integrated watcher so subscribers receive events without any
/// other process running.
pub fn serve(
    cli: &Cli,
    socket: Option<&Path>,
    watch_interval_ms: u64,
    no_watch: bool,
) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    let git = resolve_git(cli)?;
    let repo = git.repo_root().to_path_buf();
    crate::commands::ensure_store(&git)?;
    let path = socket
        .map(Path::to_path_buf)
        .unwrap_or_else(|| store_root(&git).join("doubledoor.sock"));
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(CliError::InvalidTransition(format!(
                "a server is already listening on {}",
                path.display()
            )));
        }
        std::fs::remove_file(&path)
            .map_err(|e| CliError::Internal(format!("stale socket {}: {e}", path.display())))?;
    }
    let listener = UnixListener::bind(&path).map_err(|e| {
        CliError::Usage(format!(
            "cannot listen on {} ({e}); socket paths are limited to ~100 bytes, use --socket",
            path.display()
        ))
    })?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| CliError::Internal(e.to_string()))?;
    eprintln!("{}", json!({ "listening": path, "protocol": PROTOCOL }));

    if !no_watch {
        let exe = std::env::current_exe().map_err(|e| CliError::Internal(e.to_string()))?;
        let repo = repo.clone();
        std::thread::spawn(move || loop {
            let _ = Command::new(&exe)
                .args([
                    format!("--repo={}", repo.display()),
                    "--format=json".into(),
                    "watch".into(),
                    "--once".into(),
                ])
                .output();
            std::thread::sleep(Duration::from_millis(watch_interval_ms));
        });
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let repo = repo.clone();
        std::thread::spawn(move || {
            let Ok(reader) = stream.try_clone() else {
                return;
            };
            let out: Sink = Arc::new(Mutex::new(Box::new(stream)));
            handle(repo, BufReader::new(reader), out);
        });
    }
    Ok(())
}
