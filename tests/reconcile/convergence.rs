//! Phase IV exit criterion (§48): independent portions of competing
//! mutations converge automatically while contested surfaces remain
//! isolated — plus the §52 flow (E' constructed, validated, compatibility of
//! Beta's retained `dispatch()` reported), CAS promotion (§23, DD-I4),
//! Locksmith authorization (DD-I6), symbol-level selective import (§15), and
//! crash recovery of candidates (§24, §43).

#[path = "../integration/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use doubledoor_core::DoorState;
use doubledoor_protocol::{GhostView, SessionSummary};

const CARGO: &str =
    "[package]\nname = \"router-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n";
const LIB: &str = "pub mod config;\npub mod handler;\npub mod router;\n";
const CONFIG: &str = "pub struct RouterConfig {\n    pub timeout: u32,\n}\n";
const ROUTER: &str = r#"use crate::config::RouterConfig;

#[derive(Debug, PartialEq)]
pub struct Response(pub u32);

pub fn dispatch(cfg: &RouterConfig) -> Result<Response, String> {
    Ok(Response(cfg.timeout))
}

pub fn log_route(route: &str) -> usize {
    route.len()
}

#[cfg(test)]
mod tests {
    #[test]
    fn logs() {
        assert!(super::log_route("ab") >= 2);
    }
}
"#;
const HANDLER: &str = r#"use crate::config::RouterConfig;
use crate::router::dispatch;

pub fn handle(cfg: &RouterConfig) -> bool {
    dispatch(cfg).is_ok()
}
"#;

const BETA_DISPATCH: &str = "pub fn dispatch(cfg: &RouterConfig) -> Result<Response, String> {\n    if cfg.timeout == 0 {\n        return Err(\"zero timeout\".to_string());\n    }\n    Ok(Response(cfg.timeout * 2))\n}";
const TRACE: &str = "pub fn trace(route: &str) -> String {\n    format!(\"-> {route}\")\n}";
const HANDLE_TWICE: &str =
    "\npub fn handle_twice(cfg: &RouterConfig) -> bool {\n    handle(cfg) && handle(cfg)\n}\n";

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
            "--trailer",
            &format!("dd-actor: {who}"),
        ],
    );
    git(dir, &["rev-parse", "HEAD"])
}

/// A dependency-free Cargo package at commit A; validation runs offline.
fn repo() -> Fixture {
    let fx = Fixture::new();
    let root = fx.root();
    fs::write(
        root.join(".git/info/exclude"),
        "notice-*.json\nlease*.json\n",
    )
    .unwrap();
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
    fx
}

/// β's speculative work: rework dispatch (contested), add trace() in the
/// same file and handle_twice() elsewhere (independent). Committed.
fn beta_works(ws: &Path) {
    let router = ROUTER
        .replace(
            "pub fn dispatch(cfg: &RouterConfig) -> Result<Response, String> {\n    Ok(Response(cfg.timeout))\n}",
            BETA_DISPATCH,
        )
        .replace(
            "pub fn log_route(route: &str) -> usize {\n    route.len()\n}",
            &format!("pub fn log_route(route: &str) -> usize {{\n    route.len()\n}}\n\n{TRACE}"),
        );
    write(ws, "src/router.rs", &router);
    write(ws, "src/handler.rs", &format!("{HANDLER}{HANDLE_TWICE}"));
    commit(ws, "agent-beta", "D/E: speculative rework");
}

/// α's canonical commit B (§52): retry.rs, RouterConfig.retry_limit, a
/// log_route change, and a dispatch() signature change.
fn alpha_commits(repo: &Path, rename_timeout: bool) -> String {
    write(repo, "src/lib.rs", &format!("{LIB}pub mod retry;\n"));
    write(
        repo,
        "src/retry.rs",
        "pub fn backoff(attempt: u32) -> u64 {\n    2u64.pow(attempt)\n}\n",
    );
    let field = if rename_timeout {
        "timeout_ms"
    } else {
        "timeout"
    };
    write(
        repo,
        "src/config.rs",
        &format!(
            "pub struct RouterConfig {{\n    pub {field}: u32,\n    pub retry_limit: u32,\n}}\n"
        ),
    );
    let router = ROUTER
        .replace(
            "pub struct Response(pub u32);",
            "pub struct Response(pub u32);\n\n#[derive(Debug)]\npub struct RouteError;",
        )
        .replace(
            "pub fn dispatch(cfg: &RouterConfig) -> Result<Response, String> {\n    Ok(Response(cfg.timeout))\n}",
            &format!("pub fn dispatch(cfg: &RouterConfig) -> Result<Response, RouteError> {{\n    Ok(Response(cfg.{field} + cfg.retry_limit))\n}}"),
        )
        .replace("    route.len()\n", "    route.len() + 1\n");
    write(repo, "src/router.rs", &router);
    commit(repo, "agent-alpha", "B: retry semantics")
}

fn open_beta(repo: &Path) -> (SessionSummary, String, PathBuf) {
    let s = open_session_with(repo, "agent-beta", "src/router.rs", "agent-alpha", &[]);
    let id = s.id.to_string();
    let ws = s.workspace.clone().unwrap();
    (s, id, ws)
}

fn ghosts(repo: &Path, id: &str) -> Vec<GhostView> {
    serde_json::from_str(&dd(repo, &["ghosts", id, "--format", "json"])).unwrap()
}

fn state_of(repo: &Path, id: &str) -> DoorState {
    sessions(repo)
        .into_iter()
        .find(|s| s.id.to_string() == id)
        .unwrap()
        .state
}

fn reconcile(repo: &Path, id: &str, extra: &[&str]) -> (Option<i32>, serde_json::Value) {
    let mut args = vec!["reconcile", id, "--format", "json"];
    args.extend_from_slice(extra);
    let out = doubledoor(repo, &args);
    let v = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    (out.status.code(), v)
}

fn lease(repo: &Path, holder: &str, surfaces: &[&str]) -> String {
    let p = repo.join(format!("lease-{holder}.json"));
    fs::write(
        &p,
        serde_json::json!({
            "holder": holder,
            "lease_id": "550e8400-e29b-41d4-a716-446655440000",
            "surfaces": surfaces,
        })
        .to_string(),
    )
    .unwrap();
    p.to_str().unwrap().to_string()
}

fn show(repo: &Path, commit: &str, path: &str) -> String {
    git(repo, &["show", &format!("{commit}:{path}")])
}

fn defer_all(repo: &Path, id: &str) {
    for g in ghosts(repo, id) {
        if g.needs_assessment {
            dd(repo, &["defer", &g.id.to_string()]);
        }
    }
}

#[test]
fn exit_criterion_independent_parts_converge_contested_stay_isolated() {
    let fx = repo();
    let repo = fx.root();
    let (_, id, ws) = open_beta(repo);
    beta_works(&ws);
    let ws_head = git(&ws, &["rev-parse", "HEAD"]);
    let alpha = alpha_commits(repo, false);
    defer_all(repo, &id); // β defers the ghost that touches dispatch()
    assert_eq!(state_of(repo, &id), DoorState::Active);
    let refs_before = git(repo, &["for-each-ref"]);

    let (code, r) = reconcile(repo, &id, &[]);
    assert_eq!(code, Some(0), "{r:#}");
    let c = &r["candidate"];
    let commit = c["commit"].as_str().unwrap().to_string();

    // E' is a new commit on top of canonical; nothing canonical moved.
    assert_eq!(c["target"], alpha);
    assert_eq!(git(repo, &["rev-parse", &format!("{commit}^")]), alpha);
    assert_eq!(
        fx.head(),
        alpha,
        "reconciliation never touches canonical (§16)"
    );
    assert_eq!(
        git(repo, &["for-each-ref"]),
        refs_before,
        "no ref created or moved"
    );
    assert_eq!(
        git(&ws, &["rev-parse", "HEAD"]),
        ws_head,
        "β's workspace is not rewritten"
    );

    // Independent portions of both sides converged.
    let router = show(repo, &commit, "src/router.rs");
    assert!(
        show(repo, &commit, "src/retry.rs").contains("backoff"),
        "α's retry.rs"
    );
    assert!(
        show(repo, &commit, "src/config.rs").contains("retry_limit"),
        "α's RouterConfig.retry_limit"
    );
    assert!(router.contains("route.len() + 1"), "α's log_route");
    assert!(router.contains("pub struct RouteError"), "α's new type");
    assert!(
        router.contains("pub fn trace"),
        "β's trace() in the same file"
    );
    assert!(
        show(repo, &commit, "src/handler.rs").contains("handle_twice"),
        "β's handler"
    );
    // The contested dispatch() is β's, whole — never blended with α's.
    assert!(router.contains(BETA_DISPATCH), "{router}");
    assert!(!router.contains("Result<Response, RouteError>"), "{router}");
    let contested = c["contested"].as_array().unwrap();
    assert_eq!(contested.len(), 1, "{contested:#?}");
    assert_eq!(contested[0]["surface"], "rust:router_app::router::dispatch");
    assert_eq!(contested[0]["kept"], "Session");
    assert!(
        contested[0]["resolution"]["Unresolved"].is_object(),
        "the ghost was deferred"
    );
    assert_eq!(c["promotable"], false);
    assert!(c["blocking"][0].as_str().unwrap().contains("dispatch"));

    // Conflict graph: β's handler depends on α's changed dispatch().
    let edges = c["graph"]["edges"].as_array().unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e["from"] == "rust:router_app::handler::handle"
                && e["to"] == "rust:router_app::router::dispatch"
                && e["breaking"] == true),
        "{edges:#?}"
    );

    // §52: E' validated; β's retained dispatch() is compatible with the
    // accepted API/dependency changes (RouterConfig gained retry_limit).
    let gates: Vec<(&str, &str)> = r["validation"]["gates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["name"].as_str().unwrap(), g["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        gates,
        [
            ("git integrity", "passed"),
            ("cargo metadata", "passed"),
            ("cargo check", "passed"),
            ("cargo test", "passed")
        ],
        "{:#}",
        r["validation"]
    );
    let compat = r["compatibility"].as_array().unwrap();
    let dispatch = compat
        .iter()
        .find(|x| x["surface"] == "rust:router_app::router::dispatch")
        .unwrap();
    assert_eq!(dispatch["status"], "compatible");
    assert_eq!(r["state"], "Ready");

    // Promotion is refused while the contested surface is unresolved.
    let l = lease(repo, "agent-beta", &["*"]);
    let out = doubledoor(repo, &["promote", &id, "--lease", &l]);
    assert_eq!(
        out.status.code(),
        Some(6),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not promotable"));
    assert_eq!(fx.head(), alpha);

    // β answers the contest explicitly (reject: keep its dispatch()); a new
    // candidate is then promotable, and promoting carries β's dispatch.
    dd(repo, &["discard", &id]);
    let gid = ghosts(repo, &id)[0].id.to_string();
    dd(repo, &["reject", &gid]);
    let (code, r) = reconcile(repo, &id, &[]);
    assert_eq!(code, Some(0));
    assert_eq!(r["candidate"]["contested"][0]["resolution"], "Resolved");
    assert_eq!(r["candidate"]["promotable"], true);
    dd(repo, &["promote", &id, "--lease", &l]);
    assert_eq!(git(repo, &["rev-parse", "HEAD^"]), alpha);
    assert!(read(&repo.join("src/router.rs")).contains(BETA_DISPATCH));
    assert_eq!(state_of(repo, &id), DoorState::Completed);
}

#[test]
fn retained_code_that_no_longer_compiles_is_reported_incompatible() {
    let fx = repo();
    let repo = fx.root();
    let (_, id, ws) = open_beta(repo);
    beta_works(&ws);
    alpha_commits(repo, true); // α renames RouterConfig.timeout → timeout_ms
    defer_all(repo, &id);

    let (code, r) = reconcile(repo, &id, &[]);
    assert_eq!(code, Some(0));
    assert_eq!(r["state"], "ValidationFailed");
    assert_eq!(r["validation"]["passed"], false);
    let check = r["validation"]["gates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "cargo check")
        .unwrap()
        .clone();
    assert_eq!(check["status"], "failed");
    let test = r["validation"]["gates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "cargo test")
        .unwrap()
        .clone();
    assert_eq!(
        test["status"], "skipped",
        "later gates are skipped, never passed"
    );
    let dispatch = r["compatibility"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["surface"] == "rust:router_app::router::dispatch")
        .unwrap()
        .clone();
    assert_eq!(dispatch["status"], "incompatible", "{dispatch:#}");
    assert!(
        dispatch["diagnostics"][0]
            .as_str()
            .unwrap()
            .contains("timeout"),
        "{dispatch:#}"
    );
    // β's other contribution is fine.
    let trace = r["compatibility"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["surface"] == "rust:router_app::router::trace")
        .unwrap()
        .clone();
    assert_eq!(trace["status"], "compatible");

    // The persisted report survives, and discarding resumes speculation.
    let shown: serde_json::Value =
        serde_json::from_str(&dd(repo, &["candidate", &id, "--format", "json"])).unwrap();
    assert_eq!(shown["validation"]["passed"], false);
    dd(repo, &["discard", &id]);
    assert_eq!(state_of(repo, &id), DoorState::Active);
}

#[test]
fn converge_mode_promotes_the_independent_part_and_keeps_the_contest_isolated() {
    let fx = repo();
    let repo = fx.root();
    let (_, id, ws) = open_beta(repo);
    beta_works(&ws);
    let alpha = alpha_commits(repo, false);
    defer_all(repo, &id);

    let (code, r) = reconcile(repo, &id, &["--converge"]);
    assert_eq!(code, Some(0), "{r:#}");
    assert_eq!(r["candidate"]["promotable"], true);
    assert_eq!(r["candidate"]["contested"][0]["kept"], "Canonical");
    assert_eq!(r["state"], "Ready");
    let commit = r["candidate"]["commit"].as_str().unwrap().to_string();

    // Locksmith must authorize every file the promotion changes (DD-I6).
    let files: Vec<String> = r["candidate"]["promotes_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap().to_string())
        .collect();
    assert_eq!(files, ["src/handler.rs", "src/router.rs"]);
    let partial_lease = lease(repo, "agent-beta", &["src/handler.rs"]);
    let out = doubledoor(repo, &["promote", &id, "--lease", &partial_lease]);
    assert_eq!(
        out.status.code(),
        Some(11),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let other = lease(repo, "agent-alpha", &["*"]);
    assert_eq!(
        doubledoor(repo, &["promote", &id, "--lease", &other])
            .status
            .code(),
        Some(11)
    );
    assert_eq!(
        doubledoor(repo, &["promote", &id, "--lease", "missing.json"])
            .status
            .code(),
        Some(11)
    );
    assert_eq!(fx.head(), alpha, "refused promotions change nothing");

    let l = lease(repo, "agent-beta", &["src/"]);
    let out: serde_json::Value = serde_json::from_str(&dd(
        repo,
        &["promote", &id, "--lease", &l, "--format", "json"],
    ))
    .unwrap();
    assert_eq!(out["partial"], true);
    assert_eq!(
        out["isolated"],
        serde_json::json!(["rust:router_app::router::dispatch"])
    );
    // CAS promotion: canonical fast-forwarded from exactly the target.
    assert_eq!(fx.head(), commit);
    assert_eq!(git(repo, &["rev-parse", "HEAD^"]), alpha);
    let canonical_router = read(&repo.join("src/router.rs"));
    assert!(
        canonical_router.contains("pub fn trace"),
        "β's independent work is canonical"
    );
    assert!(
        canonical_router.contains("Result<Response, RouteError>"),
        "α's dispatch stays canonical"
    );
    assert!(read(&repo.join("src/handler.rs")).contains("handle_twice"));
    assert!(
        git(repo, &["status", "--porcelain", "--untracked-files=no"]).is_empty(),
        "canonical worktree is clean"
    );
    // The contest stays isolated: β keeps working on its own dispatch().
    assert_eq!(state_of(repo, &id), DoorState::Active);
    assert!(read(&ws.join("src/router.rs")).contains(BETA_DISPATCH));
    // β's own promoted work does not come back to it as a ghost.
    dd(repo, &["watch", "--once"]);
    let gs = ghosts(repo, &id);
    assert_eq!(gs.len(), 1, "only α's commit is a ghost");
    let evs = dd(repo, &["events", "--format", "json"]);
    assert!(evs.contains("candidate.promoted"));
}

#[test]
fn full_promotion_completes_the_session() {
    let fx = repo();
    let repo = fx.root();
    let (_, id, ws) = open_beta(repo);
    write(&ws, "src/handler.rs", &format!("{HANDLER}{HANDLE_TWICE}"));
    commit(&ws, "agent-beta", "D");
    // An independent canonical commit meanwhile.
    write(repo, "src/retry.rs", "pub fn backoff() {}\n");
    write(repo, "src/lib.rs", &format!("{LIB}pub mod retry;\n"));
    let alpha = commit(repo, "agent-alpha", "B");

    let (code, r) = reconcile(repo, &id, &[]);
    assert_eq!(code, Some(0), "{r:#}");
    assert!(r["candidate"]["contested"].as_array().unwrap().is_empty());
    let l = lease(repo, "agent-beta", &["*"]);
    dd(repo, &["promote", &id, "--lease", &l]);
    assert_eq!(state_of(repo, &id), DoorState::Completed);
    assert_eq!(git(repo, &["rev-parse", "HEAD^"]), alpha);
    assert!(read(&repo.join("src/handler.rs")).contains("handle_twice"));
    assert!(repo.join("src/retry.rs").exists());
    assert!(!ws.exists(), "the speculative workspace is retired");
    assert!(!git(repo, &["worktree", "list"]).contains("workspace"));
}

#[test]
fn a_stale_candidate_is_never_promoted() {
    let fx = repo();
    let repo = fx.root();
    let (_, id, ws) = open_beta(repo);
    write(&ws, "src/handler.rs", &format!("{HANDLER}{HANDLE_TWICE}"));
    commit(&ws, "agent-beta", "D");
    let (code, _) = reconcile(repo, &id, &[]);
    assert_eq!(code, Some(0));
    assert_eq!(state_of(repo, &id), DoorState::Ready);

    // Canonical advances after validation (DD-I4).
    write(repo, "NOTES.md", "alpha was here\n");
    let alpha = commit(repo, "agent-alpha", "C");
    let l = lease(repo, "agent-beta", &["*"]);
    let out = doubledoor(repo, &["promote", &id, "--lease", &l]);
    assert_eq!(
        out.status.code(),
        Some(12),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fx.head(), alpha, "canonical untouched");
    assert_eq!(state_of(repo, &id), DoorState::Active, "back to assessment");
    let shown: serde_json::Value =
        serde_json::from_str(&dd(repo, &["candidate", &id, "--format", "json"])).unwrap();
    assert_eq!(shown["candidate"]["status"], "Stale");
    assert_eq!(
        ghosts(repo, &id).len(),
        1,
        "the advancing commit was observed"
    );

    // A fresh candidate targets the new canonical HEAD and promotes.
    let (code, r) = reconcile(repo, &id, &[]);
    assert_eq!(code, Some(0));
    assert_eq!(r["candidate"]["target"], alpha);
    dd(repo, &["promote", &id, "--lease", &l]);
    assert_eq!(git(repo, &["rev-parse", "HEAD^"]), alpha);
}

#[test]
fn watcher_marks_ready_candidates_stale_and_workspace_edits_block_promotion() {
    let fx = repo();
    let repo = fx.root();
    let (_, id, ws) = open_beta(repo);
    write(&ws, "src/handler.rs", &format!("{HANDLER}{HANDLE_TWICE}"));
    commit(&ws, "agent-beta", "D");
    reconcile(repo, &id, &[]);
    // β keeps editing after the candidate was built.
    write(
        &ws,
        "src/handler.rs",
        &format!("{HANDLER}{HANDLE_TWICE}// more\n"),
    );
    let l = lease(repo, "agent-beta", &["*"]);
    let out = doubledoor(repo, &["promote", &id, "--lease", &l]);
    assert_eq!(out.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&out.stderr).contains("workspace changed"));

    write(repo, "NOTES.md", "x\n");
    commit(repo, "agent-alpha", "C");
    dd(repo, &["watch", "--once"]);
    assert_eq!(state_of(repo, &id), DoorState::Active);
}

#[test]
fn symbol_level_selective_import() {
    let fx = repo();
    let repo = fx.root();
    let (_, id, ws) = open_beta(repo);
    beta_works(&ws);
    alpha_commits(repo, false);
    let g = &ghosts(repo, &id)[0];
    let gid = g.id.to_string();

    // dispatch() was also changed by β: importing it is refused, untouched.
    let before = read(&ws.join("src/router.rs"));
    let out = doubledoor(
        repo,
        &[
            "accept",
            &gid,
            "--symbol",
            "rust:router_app::router::dispatch",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(10),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read(&ws.join("src/router.rs")), before);
    // Not a symbol of this ghost: usage error.
    assert_eq!(
        doubledoor(repo, &["accept", &gid, "--symbol", "rust:nope::x"])
            .status
            .code(),
        Some(1)
    );

    // log_route and RouteError are imported; β's dispatch() and trace() stay.
    dd(
        repo,
        &[
            "accept",
            &gid,
            "--symbol",
            "rust:router_app::router::log_route",
            "--symbol",
            "rust:router_app::router::RouteError",
        ],
    );
    let router = read(&ws.join("src/router.rs"));
    assert!(router.contains("route.len() + 1"));
    assert!(router.contains("pub struct RouteError"));
    assert!(router.contains(BETA_DISPATCH));
    assert!(router.contains("pub fn trace"));
    assert!(git(&ws, &["log", "-1", "--format=%B"]).contains("rust:router_app::router::log_route"));
    assert!(git(&ws, &["status", "--porcelain"]).is_empty());
}

#[test]
fn recovery_resolves_interrupted_validation_and_promotion() {
    use doubledoor_core::DoorSessionId;
    use doubledoor_journal::{
        load_metadata, save_metadata, Journal, JournalEventKind, JournalRecord,
    };

    let fx = repo();
    let repo = fx.root();
    let (_, id, ws) = open_beta(repo);
    write(&ws, "src/handler.rs", &format!("{HANDLER}{HANDLE_TWICE}"));
    commit(&ws, "agent-beta", "D");
    let (_, r) = reconcile(repo, &id, &[]);
    let commit_c = r["candidate"]["commit"].as_str().unwrap().to_string();
    let dir = fx.session_dir(&id);
    let sid = DoorSessionId::parse(&id).unwrap();

    // Simulate a crash mid-promotion, after canonical moved: PROMOTING in
    // the journal, canonical already at the candidate.
    let force_state = |to: DoorState| {
        let mut s = load_metadata(&dir).unwrap();
        Journal::open(&dir)
            .unwrap()
            .append(JournalRecord {
                session: sid,
                at: std::time::SystemTime::now(),
                from: s.state,
                to,
                event: JournalEventKind::RecoveryPerformed {
                    action: "test: simulated crash".into(),
                },
            })
            .unwrap();
        s.state = to;
        save_metadata(&dir, &s).unwrap();
    };
    force_state(DoorState::Promoting);
    git(repo, &["merge", "--ff-only", "-q", &commit_c]);
    let rep: serde_json::Value =
        serde_json::from_str(&dd(repo, &["recover", "--format", "json"])).unwrap();
    assert!(
        rep["actions"].to_string().contains("promotion completed"),
        "{rep:#}"
    );
    assert_eq!(state_of(repo, &id), DoorState::Completed);

    // Interrupted validation: back to ACTIVE, candidate marked incomplete.
    let (_, id2, ws2) = open_beta(repo);
    write(
        &ws2,
        "src/config.rs",
        "pub struct RouterConfig {\n    pub timeout: u32,\n    pub extra: u8,\n}\n",
    );
    commit(&ws2, "agent-beta", "D2");
    reconcile(repo, &id2, &[]);
    let dir2 = fx.session_dir(&id2);
    let sid2 = DoorSessionId::parse(&id2).unwrap();
    {
        let mut s = load_metadata(&dir2).unwrap();
        Journal::open(&dir2)
            .unwrap()
            .append(JournalRecord {
                session: sid2,
                at: std::time::SystemTime::now(),
                from: s.state,
                to: DoorState::Validating,
                event: JournalEventKind::RecoveryPerformed {
                    action: "test: simulated crash".into(),
                },
            })
            .unwrap();
        s.state = DoorState::Validating;
        save_metadata(&dir2, &s).unwrap();
    }
    fs::create_dir_all(dir2.join("validate/tree")).unwrap();
    dd(repo, &["recover"]);
    assert_eq!(state_of(repo, &id2), DoorState::Active);
    assert!(!dir2.join("validate").exists());
    let shown: serde_json::Value =
        serde_json::from_str(&dd(repo, &["candidate", &id2, "--format", "json"])).unwrap();
    assert!(
        shown["candidate"]["status"]["Incomplete"].is_object(),
        "{:#}",
        shown["candidate"]["status"]
    );
}
