//! Phase VI exit criterion (§50): coding agents can use Doubledoor without
//! parsing human CLI prose — structured protocol, capability negotiation,
//! event subscription, automatic low-risk acceptance, and machine-readable
//! reports with next actions. Every message is checked against `schemas/`.

#[path = "../integration/common/mod.rs"]
mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// A minimal JSON Schema checker (type, required, properties, items, enum,
// $ref to sibling files) — enough to hold outputs to the published schemas.
// ---------------------------------------------------------------------------

fn schema(name: &str) -> Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .unwrap()
}

fn conforms(v: &Value, s: &Value, path: &str) -> Result<(), String> {
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        return conforms(v, &schema(r), path);
    }
    if let Some(t) = s.get("type") {
        let types: Vec<&str> = match t {
            Value::String(x) => vec![x.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let ok = types.iter().any(|t| match *t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            _ => true,
        });
        if !ok {
            return Err(format!("{path}: expected {types:?}, got {v}"));
        }
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            return Err(format!("{path}: {v} not in {e:?}"));
        }
    }
    if let (Some(obj), Some(req)) = (v.as_object(), s.get("required").and_then(Value::as_array)) {
        for r in req.iter().filter_map(Value::as_str) {
            if !obj.contains_key(r) {
                return Err(format!("{path}: missing `{r}`"));
            }
        }
    }
    if let (Some(obj), Some(props)) = (
        v.as_object(),
        s.get("properties").and_then(Value::as_object),
    ) {
        for (k, ps) in props {
            if let Some(x) = obj.get(k) {
                conforms(x, ps, &format!("{path}.{k}"))?;
            }
        }
    }
    if let (Some(arr), Some(items)) = (v.as_array(), s.get("items")) {
        for (i, x) in arr.iter().enumerate() {
            conforms(x, items, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

fn check(v: &Value, name: &str) {
    if let Err(e) = conforms(v, &schema(name), "$") {
        panic!("does not conform to {name}: {e}\n{v:#}");
    }
}

// ---------------------------------------------------------------------------
// A protocol client (the "agent"): JSON lines in, JSON lines out.
// ---------------------------------------------------------------------------

struct Client {
    tx: Box<dyn Write>,
    rx: Receiver<String>,
    next: u64,
    notes: Vec<Value>,
    lines: usize,
    _child: Option<Child>,
}

impl Client {
    fn rpc(repo: &Path) -> (Self, std::process::ChildStderr) {
        let mut child = Command::new(doubledoor_bin())
            .args(["rpc", "--repo", repo.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin: ChildStdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        (
            Self::new(Box::new(stdin), Box::new(stdout), Some(child)),
            stderr,
        )
    }

    fn socket(path: &Path) -> Self {
        let s = UnixStream::connect(path).unwrap();
        let r = s.try_clone().unwrap();
        Self::new(Box::new(s), Box::new(r), None)
    }

    fn new(tx: Box<dyn Write>, rd: Box<dyn std::io::Read + Send>, child: Option<Child>) -> Self {
        let (send, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(rd).lines() {
                let Ok(l) = line else { return };
                if send.send(l).is_err() {
                    return;
                }
            }
        });
        Client {
            tx,
            rx,
            next: 1,
            notes: Vec::new(),
            lines: 0,
            _child: child,
        }
    }

    /// Every line must be JSON: that is the exit criterion.
    fn read(&mut self, deadline: Instant) -> Option<Value> {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = self.rx.recv_timeout(left).ok()?;
        self.lines += 1;
        let v: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("non-JSON output line ({e}): {line}"));
        if v.get("notification").is_some() {
            check(&v, "protocol-notification.schema.json");
        } else {
            check(&v, "protocol-response.schema.json");
        }
        Some(v)
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let req = json!({ "id": id, "method": method, "params": params });
        check(&req, "protocol-request.schema.json");
        writeln!(self.tx, "{req}").unwrap();
        self.tx.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let v = self
                .read(deadline)
                .unwrap_or_else(|| panic!("no response to {method}"));
            if v.get("notification").is_some() {
                self.notes.push(v);
            } else if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params);
        assert_eq!(r["ok"], true, "{method} failed: {r:#}");
        r["result"].clone()
    }

    fn wait_note(&mut self, pred: impl Fn(&Value) -> bool, secs: u64) -> Value {
        if let Some(i) = self.notes.iter().position(|n| pred(&n["event"])) {
            return self.notes.remove(i);
        }
        let deadline = Instant::now() + Duration::from_secs(secs);
        while let Some(v) = self.read(deadline) {
            if v.get("notification").is_some() {
                if pred(&v["event"]) {
                    return v;
                }
                self.notes.push(v);
            }
        }
        panic!("no matching notification; saw {:#?}", self.notes);
    }
}

fn notice(repo: &Path, surface: &str) -> Value {
    json!({
        "holder": "agent-alpha",
        "requested": [surface],
        "contested": [surface],
        "lease_id": "550e8400-e29b-41d4-a716-446655440000",
        "base_commit": git(repo, &["rev-parse", "HEAD"]),
    })
}

fn commit(dir: &Path, who: &str, files: &[(&str, &str)]) -> String {
    for (f, c) in files {
        fs::write(dir.join(f), c).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(
        dir,
        &[
            "-c",
            &format!("user.name={who}"),
            "-c",
            "user.email=a@example.dev",
            "commit",
            "-qm",
            who,
        ],
    );
    git(dir, &["rev-parse", "HEAD"])
}

fn hello(c: &mut Client) -> Value {
    let h = c.ok(
        "hello",
        json!({
            "protocol_versions": ["doubledoor.protocol/v1"],
            "capabilities": ["events", "auto_accept", "reports", "reconcile", "promotion"],
            "agent": { "name": "test-agent", "version": "1" },
        }),
    );
    check(&h, "hello-result.schema.json");
    h
}

fn pick<'a>(report: &'a Value, action: &str) -> &'a Value {
    report["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["action"] == action)
        .unwrap_or_else(|| panic!("no next action `{action}` in {:#}", report["next_actions"]))
}

#[test]
fn exit_criterion_an_agent_drives_the_whole_lifecycle_over_the_protocol() {
    let fx = Fixture::new();
    let repo = fx.root();
    fs::write(repo.join(".git/info/exclude"), "notice-*.json\n").unwrap();
    let (mut c, mut stderr) = Client::rpc(repo);

    // Nothing works before negotiation; negotiation is explicit.
    let r = c.call("status", json!({}));
    assert_eq!(r["error"]["kind"], "handshake_required");
    let r = c.call(
        "hello",
        json!({ "protocol_versions": ["doubledoor.protocol/v9"] }),
    );
    assert_eq!(r["error"]["kind"], "unsupported_protocol");
    let r = c.call("hello", json!({ "require": ["teleport"] }));
    assert_eq!(r["error"]["kind"], "capability_unavailable");
    let h = hello(&mut c);
    assert_eq!(h["protocol"], "doubledoor.protocol/v1");
    assert_eq!(
        h["granted"],
        json!(["events", "auto_accept", "reports", "reconcile", "promotion"])
    );
    assert!(h["methods"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["name"] == "subscribe" && m["streams"] == true));
    assert_eq!(h["exit_codes"].as_array().unwrap().len(), 13);

    // Open with automatic acceptance of independent ghosts.
    let s = c.ok(
        "open",
        json!({
            "actor": "agent-beta", "intent": "tidy beta", "surfaces": ["alpha.txt"],
            "notice": notice(repo, "alpha.txt"), "auto_accept": "independent",
        }),
    );
    let id = s["id"].as_str().unwrap().to_string();
    let ws = PathBuf::from(s["workspace"].as_str().unwrap());
    let sub = c.ok("subscribe", json!({ "session": id }))["subscription"]
        .as_u64()
        .unwrap();
    assert!(sub >= 1);

    commit(&ws, "agent-beta", &[("gamma.txt", "beta's work\n")]);
    commit(repo, "agent-alpha", &[("NOTES.md", "independent\n")]);
    let overlap = commit(
        repo,
        "agent-alpha",
        &[("alpha.txt", "alpha rewrote this\n")],
    );

    let obs = c.ok("observe", json!({}));
    let mine = obs
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["session"] == json!(id))
        .unwrap();
    assert_eq!(
        mine["auto_accepted"].as_array().unwrap().len(),
        1,
        "{mine:#}"
    );
    assert!(
        ws.join("NOTES.md").exists(),
        "the independent ghost entered by policy"
    );
    assert!(
        !fs::read_to_string(ws.join("alpha.txt"))
            .unwrap()
            .contains("alpha rewrote"),
        "the overlapping one did not"
    );

    // Structured notifications, including who decided.
    let by_policy = c.wait_note(
        |e| e["event"] == "ghost.assessed" && e["by"] == "policy:independent",
        30,
    );
    assert_eq!(by_policy["subscription"], json!(sub));
    c.wait_note(
        |e| e["event"] == "session.state" && e["to"] == "GhostPending",
        30,
    );

    // The report says exactly what to do next; the agent just follows it.
    let rep = c.ok("report", json!({ "session": id }));
    check(&rep, "report.schema.json");
    assert_eq!(rep["state"], "GhostPending");
    assert_eq!(rep["ghosts"]["accepted"], 1);
    assert_eq!(
        rep["ghosts"]["pending"][0]["classification"],
        "write_overlap"
    );
    let defer = pick(&rep, "defer").clone();
    c.ok(defer["action"].as_str().unwrap(), defer["params"].clone());

    let rep = c.ok("report", json!({ "session": id }));
    let next = pick(&rep, "reconcile").clone();
    let cand = c.ok(next["action"].as_str().unwrap(), next["params"].clone());
    assert_eq!(cand["state"], "Ready", "{cand:#}");

    let rep = c.ok("report", json!({ "session": id }));
    check(&rep, "report.schema.json");
    let promote = pick(&rep, "promote").clone();
    assert_eq!(promote["requires"], json!(["lease"]));
    let mut params = promote["params"].clone();
    params["lease"] = json!({ "holder": "agent-beta", "lease_id": "550e8400-e29b-41d4-a716-446655440000", "surfaces": ["*"] });
    let done = c.ok("promote", params);
    assert_eq!(done["state"], "Completed");
    assert_eq!(git(repo, &["rev-parse", "HEAD^"]), overlap);
    assert_eq!(read(&repo.join("gamma.txt")), "beta's work\n");
    c.wait_note(|e| e["event"] == "candidate.promoted", 30);

    let rep = c.ok("report", json!({ "session": id }));
    assert!(
        rep["next_actions"].as_array().unwrap().is_empty(),
        "nothing left to do"
    );

    // Structured failures, never prose to parse.
    let e = c.call("promote", json!({ "session": id, "lease": {} }));
    assert_eq!(e["ok"], false);
    check(&e["error"], "protocol-error.schema.json");
    assert_eq!(e["error"]["kind"], "invalid_transition");
    let e = c.call("inspect", json!({ "session": "--repo=/" }));
    assert_eq!(
        e["error"]["kind"], "not_found",
        "parameters can never become flags"
    );
    let e = c.call("status", json!({ "verbose": true }));
    assert_eq!(e["error"]["kind"], "invalid_params");
    let e = c.call("no_such_method", json!({}));
    assert_eq!(e["error"]["kind"], "unknown_method");

    assert!(c.lines > 15);
    drop(c);
    let mut err = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut err).unwrap();
    assert!(
        err.trim().is_empty(),
        "the protocol never writes prose: {err}"
    );
}

#[test]
fn the_socket_server_pushes_watcher_events_to_subscribers() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new();
    let repo = fx.root();
    fs::write(repo.join(".git/info/exclude"), "notice-*.json\n").unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("dd.sock");
    let mut server = Command::new(doubledoor_bin())
        .args([
            "serve",
            "--repo",
            repo.to_str().unwrap(),
            "--socket",
            sock.to_str().unwrap(),
            "--watch-interval-ms",
            "200",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sock.exists());
    assert_eq!(
        fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
        0o600,
        "owner-only socket"
    );

    let second = Command::new(doubledoor_bin())
        .args([
            "serve",
            "--repo",
            repo.to_str().unwrap(),
            "--socket",
            sock.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(6));
    let err: Value = serde_json::from_slice(&second.stderr).unwrap();
    check(&err["error"], "protocol-error.schema.json");

    let mut c = Client::socket(&sock);
    hello(&mut c);
    let s = c.ok("open", json!({ "actor": "agent-beta", "intent": "x", "surfaces": ["alpha.txt"], "notice": notice(repo, "alpha.txt") }));
    let id = s["id"].as_str().unwrap().to_string();
    c.ok(
        "subscribe",
        json!({ "session": id, "events": ["ghost.available"] }),
    );
    commit(repo, "agent-alpha", &[("alpha.txt", "changed\n")]);
    // No `observe` call: the integrated watcher produces the event.
    let n = c.wait_note(|e| e["event"] == "ghost.available", 30);
    assert_eq!(n["event"]["classification"], "write_overlap");
    assert_eq!(n["event"]["session"], json!(id));
    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn auto_accept_never_imports_out_of_order_and_is_journalled() {
    let fx = Fixture::new();
    let repo = fx.root();
    fs::write(repo.join(".git/info/exclude"), "notice-*.json\n").unwrap();
    fs::write(repo.join("shared.txt"), "v0\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "shared"]);
    let s = open_session(repo, "agent-beta", "b", "alpha.txt", "agent-alpha");
    let id = s.id.to_string();
    let ws = s.workspace.unwrap();

    commit(
        repo,
        "agent-alpha",
        &[("alpha.txt", "a1\n"), ("shared.txt", "v1\n")],
    ); // write overlap
    commit(repo, "agent-alpha", &[("shared.txt", "v2\n")]); // independent, but after an unaccepted one
    commit(repo, "agent-alpha", &[("other.txt", "o\n")]); // independent
    let ghosts: Vec<Value> =
        serde_json::from_str(&dd(repo, &["ghosts", &id, "--format", "json"])).unwrap();
    let overlap = ghosts
        .iter()
        .find(|g| g["classification"] == "write_overlap")
        .unwrap();
    dd(repo, &["defer", overlap["id"].as_str().unwrap()]);

    let out: Value = serde_json::from_str(&dd(
        repo,
        &[
            "policy",
            &id,
            "--auto-accept",
            "independent",
            "--format",
            "json",
        ],
    ))
    .unwrap();
    assert_eq!(out["auto_accepted"].as_array().unwrap().len(), 1, "{out:#}");
    assert_eq!(out["skipped"].as_array().unwrap().len(), 1);
    assert!(out["skipped"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("earlier"));
    assert!(ws.join("other.txt").exists());
    assert_eq!(
        read(&ws.join("shared.txt")),
        "v0\n",
        "never applied ahead of the deferred ghost"
    );

    let kinds: Vec<String> = journal_lines(repo, &id).iter().map(journal_kind).collect();
    assert!(kinds.contains(&"PolicyChanged".to_string()));
    let assessed: Vec<Value> = journal_lines(repo, &id)
        .into_iter()
        .filter(|l| journal_kind(l) == "GhostAssessed")
        .collect();
    assert!(assessed
        .iter()
        .any(|l| l["payload"]["event"]["GhostAssessed"]["by"] == "policy:independent"));
    assert!(
        assessed
            .iter()
            .any(|l| l["payload"]["event"]["GhostAssessed"]["by"].is_null()),
        "the actor's own defer"
    );

    assert_eq!(
        doubledoor(repo, &["policy", &id, "--auto-accept", "reckless"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn every_cli_failure_is_json_under_format_json() {
    let fx = Fixture::new();
    let repo = fx.root();
    for (args, kind) in [
        (
            vec![
                "inspect",
                "dd_00000000-0000-0000-0000-000000000000",
                "--format",
                "json",
            ],
            "not_found",
        ),
        (vec!["inspect", "--bogus", "--format", "json"], "usage"),
        (
            vec![
                "--format=json",
                "open",
                "--actor",
                "x",
                "--intent",
                "y",
                "--surface",
                "alpha.txt",
            ],
            "contention_without_notice",
        ),
    ] {
        let out = doubledoor(repo, &args);
        assert!(out.stdout.is_empty(), "{args:?}");
        let v: Value = serde_json::from_slice(&out.stderr).unwrap_or_else(|e| {
            panic!(
                "{args:?}: not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        check(&v["error"], "protocol-error.schema.json");
        assert_eq!(v["error"]["kind"], kind, "{args:?}");
        assert_eq!(
            out.status.code(),
            Some(v["error"]["code"].as_u64().unwrap() as i32)
        );
    }
    let caps: Value =
        serde_json::from_str(&dd(repo, &["capabilities", "--format", "json"])).unwrap();
    check(&caps, "hello-result.schema.json");
}
