//! Phase V exit criterion (§49): reconciliation reports combine source,
//! API, dependency, diagnostic, and behavioural evidence through a unified
//! evidence model — with the §17 trust root (deterministic evidence
//! decides; heuristic/model evidence is advisory), §18 separation of claims
//! from evidence, and §21 graceful operation without optional providers.
//!
//! Uses the real ELCI tools when installed (Kaptaind, Traci, Fract, ZOC,
//! Padagonia) and asserts only what holds either way when they are not.

#[path = "../integration/common/mod.rs"]
mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::*;
use serde_json::Value;

const CARGO: &str =
    "[package]\nname = \"router-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n";
const LIB: &str = "pub mod config;\npub mod handler;\npub mod router;\n";
const CONFIG: &str = "pub struct RouterConfig {\n    pub timeout: u32,\n}\n";
const ROUTER: &str = "use crate::config::RouterConfig;\n\npub fn dispatch(cfg: &RouterConfig) -> Result<u32, String> {\n    Ok(cfg.timeout)\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn dispatches() {\n        let c = crate::config::RouterConfig { timeout: 3 };\n        assert_eq!(super::dispatch(&c), Ok(3));\n    }\n}\n";
const HANDLER: &str = "use crate::config::RouterConfig;\nuse crate::router::dispatch;\n\npub fn handle(cfg: &RouterConfig) -> bool {\n    dispatch(cfg).is_ok()\n}\n";

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn commit(dir: &Path, who: &str, msg: &str) -> String {
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
            msg,
        ],
    );
    git(dir, &["rev-parse", "HEAD"])
}

fn repo(evidence_json: Option<&str>) -> Fixture {
    let fx = Fixture::new();
    let root = fx.root();
    fs::write(root.join(".git/info/exclude"), "notice-*.json\n").unwrap();
    for (p, c) in [
        ("Cargo.toml", CARGO),
        ("src/lib.rs", LIB),
        ("src/config.rs", CONFIG),
        ("src/router.rs", ROUTER),
        ("src/handler.rs", HANDLER),
    ] {
        write(root, p, c);
    }
    fs::write(root.join(".gitignore"), "/target\n").unwrap();
    commit(root, "setup", "A");
    fs::create_dir_all(root.join(".doubledoor")).unwrap();
    fs::write(
        root.join(".doubledoor/validation.json"),
        r#"{"cargo_offline": true}"#,
    )
    .unwrap();
    if let Some(e) = evidence_json {
        fs::write(root.join(".doubledoor/evidence.json"), e).unwrap();
    }
    fx
}

/// β widens dispatch()'s error type (a breaking public API change it will
/// promote) and adds a helper; α adds an unrelated file meanwhile.
fn scenario(fx: &Fixture) -> (String, PathBuf) {
    let repo = fx.root();
    let s = open_session_with(repo, "agent-beta", "src/router.rs", "agent-alpha", &[]);
    let ws = s.workspace.clone().unwrap();
    write(
        &ws,
        "src/router.rs",
        &ROUTER
            .replace("Result<u32, String>", "Result<u32, Box<str>>")
            .replace(
                "        assert_eq!(super::dispatch(&c), Ok(3));",
                "        assert_eq!(super::dispatch(&c).unwrap(), 3);",
            ),
    );
    write(&ws, "src/handler.rs", &format!("{HANDLER}\npub fn handle_all(cfgs: &[RouterConfig]) -> usize {{\n    cfgs.iter().filter(|c| handle(c)).count()\n}}\n"));
    commit(&ws, "agent-beta", "D");
    write(repo, "NOTES.md", "alpha\n");
    commit(repo, "agent-alpha", "B");
    (s.id.to_string(), ws)
}

fn reconcile(repo: &Path, id: &str, envs: &[(&str, &str)]) -> Value {
    let out = Command::new(doubledoor_bin())
        .args(["reconcile", id, "--format", "json"])
        .current_dir(repo)
        .envs(envs.iter().copied())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn installed(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

fn provider<'a>(e: &'a Value, name: &str) -> &'a Value {
    e["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == name)
        .unwrap_or_else(|| panic!("no provider run for {name}: {:#}", e["providers"]))
}

fn items_from<'a>(e: &'a Value, provider: &str) -> Vec<&'a Value> {
    e["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["provider"] == provider)
        .collect()
}

#[test]
fn exit_criterion_reports_combine_all_five_kinds_through_one_model() {
    let fx = repo(None);
    let repo = fx.root();
    let (id, ws) = scenario(&fx);
    let ws_before = git(&ws, &["status", "--porcelain"]);
    let r = reconcile(repo, &id, &[]);
    let e = &r["evidence"];
    assert_eq!(e["schema"], "doubledoor.evidence/v1");

    // Every required kind carries deterministic evidence.
    for kind in ["source", "api", "dependency", "diagnostic", "behavioural"] {
        assert!(
            e["assessment"]["coverage"][kind].as_u64().unwrap() > 0,
            "no deterministic {kind} evidence: {:#}",
            e["assessment"]
        );
        assert!(
            e["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["kind"] == kind && i["tier"] != "heuristic" && i["tier"] != "model"),
            "{kind}"
        );
    }
    assert!(e["assessment"]["unverified_kinds"]
        .as_array()
        .unwrap()
        .is_empty());
    // Specific evidence: the breaking public API change, the compiler, the tests.
    let api: Vec<&Value> = items_from(e, "doubledoor.ast");
    assert!(
        api.iter()
            .any(|i| i["verdict"] == "concern"
                && i["subject"] == "rust:router_app::router::dispatch"),
        "{api:#?}"
    );
    let tests = items_from(e, "doubledoor.tests");
    assert_eq!(tests[0]["verdict"], "supports");
    assert!(
        tests[0]["attributes"]["passed"]
            .as_str()
            .unwrap()
            .parse::<u32>()
            .unwrap()
            >= 1
    );
    assert!(items_from(e, "doubledoor.compiler")
        .iter()
        .any(|i| i["verdict"] == "supports"));
    // A breaking API change is a concern, not a block: tests and compiler pass.
    assert_eq!(e["assessment"]["verdict"], "concerns");
    assert_eq!(r["state"], "Ready");

    // Traci exits non-zero when it has findings; that is a run, not a
    // failure (regression: adapters judge output, not exit codes).
    if installed("traci") {
        let t = items_from(e, "traci");
        assert!(!t.is_empty());
        assert!(t.iter().all(|i| i["attributes"]["exit_code"].is_string()));
    }
    // Every provider is accounted for: ran, or reported unavailable.
    for (name, tier) in [
        ("kaptaind", "static_analysis"),
        ("traci", "static_analysis"),
        ("fract", "static_analysis"),
        ("zoc", "heuristic"),
    ] {
        let p = provider(e, name);
        assert_eq!(p["tier"], tier);
        if installed(name) {
            assert_eq!(p["status"], "ran", "{name}: {p:#}");
        } else {
            assert_eq!(p["status"], "unavailable");
        }
    }
    assert_eq!(provider(e, "sore")["status"], "unavailable");
    assert_eq!(provider(e, "sore")["tier"], "model");
    assert_eq!(provider(e, "padagonia")["status"], "disabled");

    // §17: heuristic relevance never enters the verdict; §18: the actor's
    // claim is kept apart from evidence.
    for z in items_from(e, "zoc") {
        assert_eq!(z["tier"], "heuristic");
        assert_eq!(z["verdict"], "info");
    }
    assert_eq!(e["explanation"]["actor_claim"], "speculative work");
    let det = e["explanation"]["deterministic_evidence"]
        .as_array()
        .unwrap()
        .len();
    let prob = e["explanation"]["probabilistic_evidence"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(det + prob, e["items"].as_array().unwrap().len());
    if installed("zoc") {
        assert!(prob > 0);
        assert!(e["explanation"]["inferred_intent"]
            .as_str()
            .unwrap()
            .contains("not evidence of safety"));
    }

    // Providers disagreeing on the same question is surfaced, not hidden.
    let kap_says_fine = items_from(e, "kaptaind")
        .iter()
        .any(|i| i["kind"] == "api" && i["verdict"] == "supports");
    if kap_says_fine {
        let c = e["conflicts"].as_array().unwrap();
        assert!(
            c.iter()
                .any(|c| c["kind"] == "api" && c["note"].as_str().unwrap().contains("kaptaind")),
            "{c:#?}"
        );
    }

    // Persisted, shown, and nothing leaked into the session or canonical.
    let shown: Value =
        serde_json::from_str(&dd(repo, &["candidate", &id, "--format", "json"])).unwrap();
    assert_eq!(shown["evidence"]["candidate"], e["candidate"]);
    let human = dd(repo, &["candidate", &id]);
    for block in [
        "EVIDENCE=CONCERNS",
        "COVERAGE",
        "PROVIDERS",
        "FINDINGS (deterministic)",
        "CLAIMS VS EVIDENCE",
    ] {
        assert!(human.contains(block), "missing {block}:\n{human}");
    }
    assert_eq!(git(&ws, &["status", "--porcelain"]), ws_before);
    assert!(
        !ws.join(".kaptaind").exists() && !repo.join(".kaptaind").exists(),
        "providers only ever see private trees"
    );
    assert!(
        !git(repo, &["worktree", "list"]).contains("evidence"),
        "evidence worktrees are removed"
    );
    let journal: Vec<String> = journal_lines(repo, &id).iter().map(journal_kind).collect();
    assert!(journal.contains(&"EvidenceRecorded".to_string()));
}

#[test]
fn without_optional_providers_doubledoor_is_fully_operational() {
    // Every ELCI tool resolves to a binary that does not exist.
    let cfg = serde_json::json!({
        "kaptaind": {"bin": "/nonexistent/kaptaind"},
        "traci": {"bin": "/nonexistent/traci"},
        "fract": {"bin": "/nonexistent/fract"},
        "zoc": {"bin": "/nonexistent/zoc"}
    });
    let fx = repo(Some(&cfg.to_string()));
    let repo = fx.root();
    let (id, _) = scenario(&fx);
    let r = reconcile(repo, &id, &[]);
    let e = &r["evidence"];
    for name in ["kaptaind", "traci", "fract", "zoc"] {
        let p = provider(e, name);
        assert_eq!(p["status"], "unavailable", "{name}: {p:#}");
        assert!(items_from(e, name).is_empty());
    }
    assert_eq!(provider(e, "sore")["status"], "unavailable");
    for kind in ["source", "api", "dependency", "diagnostic", "behavioural"] {
        assert!(
            e["assessment"]["coverage"][kind].as_u64().unwrap() > 0,
            "{kind}"
        );
    }
    assert_eq!(e["assessment"]["verdict"], "concerns");
    assert_eq!(r["state"], "Ready");
    // doctor reports availability without failing on optional tools.
    let doctor: Value = serde_json::from_str(&dd(repo, &["doctor", "--format", "json"])).unwrap();
    let check = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "evidence-providers")
        .unwrap()
        .clone();
    assert_eq!(check["ok"], true);
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .contains("kaptaind=unavailable"),
        "{check}"
    );
}

#[test]
fn external_providers_are_capped_and_models_are_never_decisive() {
    let dir = tempfile::tempdir().unwrap();
    let script = |name: &str, body: &str| -> String {
        let p = dir.path().join(name);
        fs::write(&p, format!("#!/bin/sh\ncat <<'JSON'\n{body}\nJSON\n")).unwrap();
        Command::new("chmod")
            .args(["+x", p.to_str().unwrap()])
            .status()
            .unwrap();
        p.to_str().unwrap().to_string()
    };
    // A model-backed "Sore" that overclaims: git-object tier, and a block.
    let sore = script(
        "sore",
        r#"{"schema":"doubledoor.evidence/v1","items":[
        {"kind":"repair","tier":"git_object","verdict":"blocks","summary":"rewrite dispatch() to return Box<str>"},
        {"kind":"api","verdict":"supports","summary":"the change is safe, trust me"}]}"#,
    );
    // A deterministic linter with a concern about a promoted file.
    let lint = script(
        "lint",
        r#"{"schema":"doubledoor.evidence/v1","items":[
        {"kind":"diagnostic","tier":"compiler","verdict":"concern","subject":"file:src/handler.rs","file":"src/handler.rs","line":6,"summary":"prefer explicit iterator types"}]}"#,
    );
    let broken = script("broken", "this is not json");
    let cfg = serde_json::json!({
        "kaptaind": {"enabled": false}, "traci": {"enabled": false}, "fract": {"enabled": false}, "zoc": {"enabled": false},
        "external": {
            "sore": {"name": "sore", "argv": [sore], "probabilistic": true, "default_kind": "repair"},
            "lint": {"name": "lint", "argv": [lint], "max_tier": "static_analysis"},
            "broken": {"name": "broken", "argv": [broken]}
        }
    });
    let fx = repo(Some(&cfg.to_string()));
    let repo = fx.root();
    let (id, _) = scenario(&fx);
    let r = reconcile(repo, &id, &[]);
    let e = &r["evidence"];

    let s = items_from(e, "sore");
    assert_eq!(s.len(), 2);
    for i in &s {
        assert_eq!(
            i["tier"], "model",
            "probabilistic providers are model-tier, whatever they claim"
        );
    }
    assert!(
        s.iter().all(|i| i["verdict"] != "blocks"),
        "external providers cannot block"
    );
    assert_eq!(s[0]["attributes"]["claimed_tier"], "GitObject");
    // The model's "safe, trust me" does not count as API coverage, and its
    // concern is advisory only.
    assert!(e["assessment"]["advisory"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a.as_str().unwrap().contains("rewrite dispatch")));
    assert!(!e["explanation"]["deterministic_evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x.as_str().unwrap().starts_with("sore:")));

    let l = items_from(e, "lint");
    assert_eq!(
        l[0]["tier"], "static_analysis",
        "claimed compiler tier is capped"
    );
    assert_eq!(l[0]["line"], 6);
    assert!(e["assessment"]["rationale"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x
            .as_str()
            .unwrap()
            .contains("prefer explicit iterator types")));

    let b = provider(e, "broken");
    assert_eq!(b["status"], "failed");
    assert!(
        items_from(e, "broken").is_empty(),
        "unparseable output contributes nothing"
    );
    for disabled in ["kaptaind", "traci", "fract", "zoc"] {
        assert_eq!(provider(e, disabled)["status"], "disabled");
    }
    assert_eq!(e["assessment"]["verdict"], "concerns");
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn http(addr: &str, method: &str, path: &str, key: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {key}\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn evidence_graph_is_published_to_padagonia() {
    if !installed("padagonia") {
        eprintln!("padagonia not installed; publishing is covered by unit tests only");
        return;
    }
    let pdir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = format!("127.0.0.1:{port}");
    let key = "doubledoor-integration-key-0123";
    fs::create_dir_all(pdir.path().join("data")).unwrap();
    fs::write(
        pdir.path().join("p.toml"),
        format!("[server]\nlisten_addr = \"{addr}\"\napi_key = \"{key}\"\ndata_dir = \"data/store.pad\"\n[storage]\ndata_dir = \"data\"\ndefault_graph = \"graph.pad\"\n[logging]\nlevel = \"warn\"\n"),
    )
    .unwrap();
    let _server = Server(
        Command::new("padagonia")
            .args(["server", "--config", "p.toml"])
            .current_dir(pdir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut up = false;
    for _ in 0..50 {
        if TcpStream::connect(&addr).is_ok() && http(&addr, "GET", "/health", key).contains("ok") {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(up, "padagonia server did not start");

    let cfg = serde_json::json!({
        "kaptaind": {"enabled": false}, "traci": {"enabled": false}, "fract": {"enabled": false}, "zoc": {"enabled": false},
        "padagonia": {"enabled": true, "url": format!("http://{addr}"), "api_key_env": "DD_TEST_PADAGONIA_KEY"}
    });
    let fx = repo(Some(&cfg.to_string()));
    let repo = fx.root();
    let (id, _) = scenario(&fx);
    let r = reconcile(repo, &id, &[("DD_TEST_PADAGONIA_KEY", key)]);
    let p = provider(&r["evidence"], "padagonia");
    assert_eq!(p["status"], "ran", "{p:#}");
    assert!(p["tier"].is_null(), "a sink, not an evidence source");
    let stats = http(&addr, "GET", "/api/v1/stats", key);
    let body: Value = serde_json::from_str(stats.split("\r\n\r\n").nth(1).unwrap().trim()).unwrap();
    let items = r["evidence"]["items"].as_array().unwrap().len() as u64;
    assert!(body["nodes"].as_u64().unwrap() > items, "{body}");
    assert!(body["edges"].as_u64().unwrap() >= items, "{body}");

    // Idempotent: validating again republishes without duplicating nodes.
    dd(repo, &["validate", &id]);
    let again: Value = serde_json::from_str(
        http(&addr, "GET", "/api/v1/stats", key)
            .split("\r\n\r\n")
            .nth(1)
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(again["nodes"], body["nodes"]);

    // A wrong key: the sink fails, the report still completes.
    dd(repo, &["discard", &id]);
    let r = reconcile(
        repo,
        &id,
        &[("DD_TEST_PADAGONIA_KEY", "wrong-key-000000000000000")],
    );
    assert_eq!(provider(&r["evidence"], "padagonia")["status"], "failed");
    assert_eq!(r["state"], "Ready");
}
