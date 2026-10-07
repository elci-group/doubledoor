//! Phase III exit criterion (§47): Doubledoor identifies meaningful
//! cross-file conflicts that ordinary Git line analysis cannot detect — and
//! the §52 per-surface classification (retry.rs independent,
//! RouterConfig.retry_limit dependency-relevant, dispatch() write/API).

#[path = "../integration/common/mod.rs"]
mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::*;
use doubledoor_core::DoorState;
use doubledoor_protocol::{GhostView, SessionSummary};

const CARGO: &str = r#"[package]
name = "router-app"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1"
legacy-http = "0.3"
"#;

const LIB: &str = "pub mod config;\npub mod handler;\npub mod outcome;\npub mod router;\n";

const ROUTER: &str = r#"use crate::config::RouterConfig;

pub struct Request;
pub struct Response;
pub struct Error;

pub fn dispatch(req: Request, cfg: &RouterConfig) -> Result<Response, Error> {
    let _ = (req, cfg.timeout);
    Ok(Response)
}

pub fn log_route(route: &str) -> usize {
    route.len()
}
"#;

const CONFIG: &str = "pub struct RouterConfig {\n    pub timeout: u32,\n}\n";

const HANDLER: &str = r#"use crate::config::RouterConfig;
use crate::router::{dispatch, Request};

pub fn handle(cfg: &RouterConfig) -> bool {
    dispatch(Request, cfg).is_ok()
}
"#;

const OUTCOME: &str = r#"pub enum Outcome {
    Done,
    Retry,
}
"#;

/// A Rust package at commit A.
fn rust_repo() -> Fixture {
    let fx = Fixture::new();
    let root = fx.root();
    fs::write(root.join(".git/info/exclude"), "notice-*.json\n").unwrap();
    for (p, c) in [
        ("Cargo.toml", CARGO),
        ("src/lib.rs", LIB),
        ("src/router.rs", ROUTER),
        ("src/config.rs", CONFIG),
        ("src/handler.rs", HANDLER),
        ("src/outcome.rs", OUTCOME),
    ] {
        write(root, p, c);
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "A: router app"]);
    fx
}

fn write(root: &Path, path: &str, content: &str) {
    let full = root.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

fn commit_all(dir: &Path, who: &str, msg: &str) -> String {
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

fn open_beta(repo: &Path, surface: &str) -> SessionSummary {
    open_session_with(repo, "agent-beta", surface, "agent-alpha", &[])
}

fn ghosts(repo: &Path, id: &str) -> Vec<GhostView> {
    serde_json::from_str(&dd(repo, &["ghosts", id, "--format", "json"])).unwrap()
}

fn class_of<'a>(g: &'a GhostView, surface: &str) -> &'a str {
    g.assessments
        .iter()
        .find(|a| a.surface == surface)
        .map(|a| a.classification.as_str())
        .unwrap_or_else(|| panic!("no assessment for {surface}: {:#?}", g.assessments))
}

fn state_of(repo: &Path, id: &str) -> DoorState {
    sessions(repo)
        .into_iter()
        .find(|s| s.id.to_string() == id)
        .unwrap()
        .state
}

#[test]
fn exit_criterion_signature_change_breaks_an_untouched_callers_file() {
    let fx = rust_repo();
    let repo = fx.root();
    let beta = open_beta(repo, "src/handler.rs");
    let id = beta.id.to_string();
    let ws = beta.workspace.clone().unwrap();

    // β works only in handler.rs (and commits in its workspace).
    write(
        &ws,
        "src/handler.rs",
        &HANDLER.replace(
            "    dispatch(Request, cfg).is_ok()",
            "    let ok = dispatch(Request, cfg).is_ok();\n    ok",
        ),
    );
    let beta_head = commit_all(&ws, "agent-beta", "D: handler tweak");
    let ws_before = git(&ws, &["rev-parse", "HEAD"]);

    // α changes dispatch()'s return type in router.rs only.
    write(
        repo,
        "src/router.rs",
        &ROUTER
            .replace("pub struct Error;", "pub struct Error;\npub enum RetryOutcome { Ok(Response), GaveUp }")
            .replace(
                "-> Result<Response, Error> {\n    let _ = (req, cfg.timeout);\n    Ok(Response)",
                "-> Result<RetryOutcome, Error> {\n    let _ = (req, cfg.timeout);\n    Ok(RetryOutcome::Ok(Response))",
            ),
    );
    let alpha = commit_all(repo, "agent-alpha", "B: retry outcome");

    // Git's own line-level analysis: disjoint files, a clean merge.
    let merge = Command::new("git")
        .args(["merge-tree", "--write-tree", &alpha, &beta_head])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        merge.status.success(),
        "git sees no conflict between the two histories: {}",
        String::from_utf8_lossy(&merge.stdout)
    );
    let changed_by_alpha = git(repo, &["diff", "--name-only", "HEAD~1", "HEAD"]);
    assert_eq!(changed_by_alpha, "src/router.rs");

    // Doubledoor: the change breaks β's call in handler.rs.
    dd(repo, &["watch", "--once"]);
    assert_eq!(state_of(repo, &id), DoorState::GhostPending);
    let g = &ghosts(repo, &id)[0];
    assert_eq!(g.classification, "api_breaking", "{g:#?}");
    assert_eq!(
        class_of(g, "file:src/router.rs"),
        "independent",
        "file level sees nothing"
    );
    assert_eq!(
        class_of(g, "rust:router_app::router::dispatch"),
        "api_breaking"
    );
    assert!(g
        .api_breaking
        .contains(&"rust:router_app::router::dispatch".to_string()));
    let v = &g.invalidated_assumptions;
    assert_eq!(v.len(), 1, "{v:#?}");
    assert_eq!(v[0].surface, "rust:router_app::router::dispatch");
    for needle in [
        "fn dispatch(req: Request, cfg: &RouterConfig) -> Result<Response, Error>",
        "Result<RetryOutcome, Error>",
        "router_app::handler::handle",
        "src/handler.rs:",
    ] {
        assert!(
            v[0].description.contains(needle),
            "missing {needle}: {}",
            v[0].description
        );
    }
    // Importing router.rs would break β's code: not a safe import.
    assert_eq!(g.breaking_imports, ["file:src/router.rs"]);
    assert!(g.safe_imports.is_empty());
    // The additive RetryOutcome enum is irrelevant to β.
    assert_eq!(
        class_of(g, "rust:router_app::router::RetryOutcome"),
        "independent"
    );

    // Event stream carries the classification and the symbol intersection.
    let events = dd(repo, &["events", "--format", "json"]);
    let avail: serde_json::Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
    assert_eq!(avail["classification"], "api_breaking");
    assert!(avail["intersections"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("rust:router_app::router::dispatch")));

    // §31 human block.
    let human = dd(repo, &["ghosts", &id]);
    assert!(human.contains("ASSUMPTION INVALIDATED"), "{human}");
    assert!(human.contains("ASSESSMENT"), "{human}");

    // Still: nothing entered β's workspace.
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), ws_before);
    assert!(git(&ws, &["status", "--porcelain"]).is_empty());
}

#[test]
fn acceptance_scenario_per_surface_classification() {
    let fx = rust_repo();
    let repo = fx.root();
    let beta = open_beta(repo, "src/router.rs");
    let id = beta.id.to_string();
    let ws = beta.workspace.clone().unwrap();

    // β reworks dispatch()'s implementation (E).
    write(
        &ws,
        "src/router.rs",
        &ROUTER.replace("    Ok(Response)\n}", "    // beta: speculative rework\n    if cfg.timeout == 0 { return Err(Error); }\n    Ok(Response)\n}"),
    );
    commit_all(&ws, "agent-beta", "E: rework dispatch");

    // α: retry.rs, RouterConfig.retry_limit, dispatch() signature change.
    write(repo, "src/lib.rs", &format!("{LIB}pub mod retry;\n"));
    write(
        repo,
        "src/retry.rs",
        "pub fn backoff(attempt: u32) -> u64 {\n    2u64.pow(attempt)\n}\n",
    );
    write(
        repo,
        "src/config.rs",
        "pub struct RouterConfig {\n    pub timeout: u32,\n    pub retry_limit: u32,\n}\n",
    );
    write(
        repo,
        "src/router.rs",
        &ROUTER
            .replace(
                "-> Result<Response, Error>",
                "-> Result<Response, Vec<Error>>",
            )
            .replace("    Ok(Response)\n}", "    Ok(Response)\n}\n"),
    );
    commit_all(repo, "agent-alpha", "B: retry semantics");

    dd(repo, &["watch", "--once"]);
    let g = &ghosts(repo, &id)[0];
    // retry.rs: independent / safe candidate.
    assert_eq!(class_of(g, "file:src/retry.rs"), "independent");
    assert_eq!(
        class_of(g, "rust:router_app::retry::backoff"),
        "independent"
    );
    // RouterConfig.retry_limit: dependency relevant (β's router.rs uses
    // RouterConfig, without a struct literal).
    assert_eq!(
        class_of(g, "rust:router_app::config::RouterConfig.retry_limit"),
        "dependency_relevant"
    );
    // dispatch(): write intersection.
    assert_eq!(
        class_of(g, "rust:router_app::router::dispatch"),
        "write_overlap"
    );
    assert_eq!(class_of(g, "file:src/router.rs"), "write_overlap");
    assert_eq!(g.classification, "write_overlap");
    assert_eq!(g.held, ["file:src/router.rs"]);
    assert_eq!(
        g.safe_imports,
        ["file:src/config.rs", "file:src/lib.rs", "file:src/retry.rs"]
    );
    let why = &g
        .assessments
        .iter()
        .find(|a| a.surface == "rust:router_app::router::dispatch")
        .unwrap()
        .reason;
    assert!(why.contains("API-breaking"), "{why}");

    // β accepts the first two, keeps its dispatch() (selective import is
    // file-level in this phase).
    let gid = g.id.to_string();
    dd(
        repo,
        &[
            "accept",
            &gid,
            "--surface",
            "src/retry.rs",
            "--surface",
            "src/config.rs",
            "--surface",
            "src/lib.rs",
        ],
    );
    assert!(read(&ws.join("src/retry.rs")).contains("backoff"));
    assert!(read(&ws.join("src/config.rs")).contains("retry_limit"));
    assert!(read(&ws.join("src/router.rs")).contains("beta: speculative rework"));
    assert!(!read(&ws.join("src/router.rs")).contains("Vec<Error>"));
}

#[test]
fn contextual_breaks_struct_literals_and_exhaustive_matches() {
    let fx = rust_repo();
    let repo = fx.root();
    let beta = open_beta(repo, "src/handler.rs");
    let id = beta.id.to_string();
    let ws = beta.workspace.clone().unwrap();
    write(
        &ws,
        "src/handler.rs",
        &format!(
            "{HANDLER}\npub fn default_config() -> RouterConfig {{\n    RouterConfig {{ timeout: 30 }}\n}}\n\npub fn label(o: crate::outcome::Outcome) -> &'static str {{\n    match o {{\n        crate::outcome::Outcome::Done => \"done\",\n        crate::outcome::Outcome::Retry => \"retry\",\n    }}\n}}\n"
        ),
    );

    // Two separate α commits: a new field, then a new variant.
    write(
        repo,
        "src/config.rs",
        "pub struct RouterConfig {\n    pub timeout: u32,\n    pub retry_limit: u32,\n}\n",
    );
    commit_all(repo, "agent-alpha", "B: retry_limit");
    write(
        repo,
        "src/outcome.rs",
        "pub enum Outcome {\n    Done,\n    Retry,\n    GaveUp,\n}\n",
    );
    commit_all(repo, "agent-alpha", "C: GaveUp");

    let gs = ghosts(repo, &id);
    assert_eq!(gs.len(), 2);
    let field = gs
        .iter()
        .find(|g| {
            g.assessments
                .iter()
                .any(|a| a.surface.ends_with("retry_limit"))
        })
        .unwrap();
    assert_eq!(
        class_of(field, "rust:router_app::config::RouterConfig.retry_limit"),
        "api_breaking"
    );
    assert!(
        field.invalidated_assumptions[0]
            .description
            .contains("struct literals"),
        "{:#?}",
        field.invalidated_assumptions
    );
    let variant = gs
        .iter()
        .find(|g| g.assessments.iter().any(|a| a.surface.ends_with("GaveUp")))
        .unwrap();
    assert_eq!(
        class_of(variant, "rust:router_app::outcome::Outcome::GaveUp"),
        "api_breaking"
    );
    assert!(variant.invalidated_assumptions[0]
        .description
        .contains("exhaustive matches"));
}

#[test]
fn cargo_mutations_against_the_crates_the_session_uses() {
    let fx = rust_repo();
    let repo = fx.root();
    let beta = open_beta(repo, "src/handler.rs");
    let id = beta.id.to_string();
    let ws = beta.workspace.clone().unwrap();
    write(
        &ws,
        "src/handler.rs",
        &format!("{HANDLER}\npub fn fetch() -> String {{\n    legacy_http::get(\"/\")\n}}\n\n#[derive(serde::Serialize)]\npub struct Report;\n"),
    );

    write(
        repo,
        "Cargo.toml",
        &(CARGO.replace(
            "serde = \"1\"\nlegacy-http = \"0.3\"\n",
            "serde = \"2\"\nbackoff = \"0.4\"\n",
        ) + "\n[features]\nretry = []\n"),
    );
    commit_all(repo, "agent-alpha", "B: deps");

    let g = &ghosts(repo, &id)[0];
    assert_eq!(g.classification, "api_breaking");
    assert_eq!(
        class_of(g, "cargo:Cargo.toml#dependencies.legacy-http"),
        "api_breaking"
    );
    assert_eq!(
        class_of(g, "cargo:Cargo.toml#dependencies.serde"),
        "api_breaking"
    );
    assert_eq!(
        class_of(g, "cargo:Cargo.toml#dependencies.backoff"),
        "dependency_relevant"
    );
    assert_eq!(
        class_of(g, "cargo:Cargo.toml#features.retry"),
        "dependency_relevant"
    );
    let descriptions: Vec<&str> = g
        .invalidated_assumptions
        .iter()
        .map(|v| v.description.as_str())
        .collect();
    assert!(
        descriptions
            .iter()
            .any(|d| d.contains("legacy-http") && d.contains("removed")),
        "{descriptions:#?}"
    );
    assert!(
        descriptions.iter().any(|d| d.contains("semver")),
        "{descriptions:#?}"
    );
}

#[test]
fn behaviour_changes_are_relevant_and_unused_changes_do_not_block() {
    let fx = rust_repo();
    let repo = fx.root();
    let beta = open_beta(repo, "src/handler.rs");
    let id = beta.id.to_string();
    let ws = beta.workspace.clone().unwrap();
    write(
        &ws,
        "src/handler.rs",
        &HANDLER.replace("is_ok()", "is_ok() // beta"),
    );
    commit_all(&ws, "agent-beta", "D");

    // Unused private-ish helper: independent; the session keeps working.
    write(
        repo,
        "src/router.rs",
        &ROUTER.replace("route.len()", "route.len() + 1"),
    );
    commit_all(repo, "agent-alpha", "B: log_route");
    dd(repo, &["watch", "--once"]);
    let g = &ghosts(repo, &id)[0];
    assert_eq!(
        class_of(g, "rust:router_app::router::log_route"),
        "independent"
    );
    assert_eq!(g.classification, "independent");
    assert_eq!(state_of(repo, &id), DoorState::Active);

    // Body-only change of something β calls: relevant, not breaking.
    let current = read(&repo.join("src/router.rs"));
    write(
        repo,
        "src/router.rs",
        &current.replace(
            "    Ok(Response)\n",
            "    if cfg.timeout > 99 { return Err(Error); }\n    Ok(Response)\n",
        ),
    );
    commit_all(repo, "agent-alpha", "C: dispatch behaviour");
    dd(repo, &["watch", "--once"]);
    let gs = ghosts(repo, &id);
    let g = gs
        .iter()
        .find(|g| g.classification != "independent")
        .unwrap();
    assert_eq!(
        class_of(g, "rust:router_app::router::dispatch"),
        "dependency_relevant"
    );
    assert!(g.invalidated_assumptions.is_empty());
    assert_eq!(state_of(repo, &id), DoorState::GhostPending);
}

#[test]
fn mid_edit_working_copy_falls_back_and_surfaces_reports_the_footprint() {
    let fx = rust_repo();
    let repo = fx.root();
    let beta = open_beta(repo, "src/handler.rs");
    let id = beta.id.to_string();
    let ws = beta.workspace.clone().unwrap();
    write(
        &ws,
        "src/handler.rs",
        &HANDLER.replace("is_ok()", "is_ok() && cfg.timeout > 0"),
    );
    commit_all(&ws, "agent-beta", "D");
    // β is mid-edit: the working copy does not parse.
    write(
        &ws,
        "src/handler.rs",
        "use crate::router::dispatch;\npub fn handle( {\n",
    );

    let view: serde_json::Value =
        serde_json::from_str(&dd(repo, &["surfaces", &id, "--format", "json"])).unwrap();
    assert_eq!(view["changed_files"], serde_json::json!(["src/handler.rs"]));
    assert_eq!(
        view["fallback_files"],
        serde_json::json!(["src/handler.rs"])
    );
    assert!(view["modified_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["surface"] == "rust:router_app::handler::handle"));
    assert!(view["references"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("router_app::router::dispatch")));
    assert!(dd(repo, &["surfaces", &id]).contains("MODIFIED SYMBOLS"));

    write(
        repo,
        "src/router.rs",
        &ROUTER
            .replace("-> Result<Response, Error>", "-> Option<Response>")
            .replace("Ok(Response)", "Some(Response)"),
    );
    commit_all(repo, "agent-alpha", "B");
    let g = &ghosts(repo, &id)[0];
    assert_eq!(
        g.classification, "api_breaking",
        "committed version still shows the call"
    );
    assert!(
        g.evidence.iter().any(|e| e.contains("did not parse")),
        "{:#?}",
        g.evidence
    );
}
