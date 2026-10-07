//! Pure (no Git) tests of Rust symbol extraction, symbol diffing, and the
//! reference relations that drive Phase III assessment.

use doubledoor_surface::rust::{
    analyze_source, RefTarget, RustParseError, SymbolKind, SymbolVisibility,
};
use doubledoor_surface::semantic::{
    diff_symbols, ApiImpact, ChangeKind, Relation, SessionReference, SessionSemantics, SymbolChange,
};

fn module(segs: &[&str]) -> Vec<String> {
    segs.iter().map(ToString::to_string).collect()
}

fn syms(src: &str, m: &[&str]) -> Vec<(String, doubledoor_surface::rust::Symbol)> {
    analyze_source(src, &module(m))
        .unwrap()
        .symbols
        .into_iter()
        .map(|s| ("f.rs".to_string(), s))
        .collect()
}

fn names(src: &str, m: &[&str]) -> Vec<String> {
    syms(src, m)
        .into_iter()
        .map(|(_, s)| s.qualified())
        .collect()
}

const ROUTER: &str = r#"
use crate::config::RouterConfig;

/// Dispatch a request.
pub fn dispatch(req: Request, cfg: &RouterConfig) -> Result<Response, Error> {
    let _ = cfg.timeout;
    Ok(Response)
}

pub struct Router { pub routes: Vec<String>, retries: u8 }

impl Router {
    pub fn new() -> Self { Router { routes: vec![], retries: 0 } }
}

impl Default for Router {
    fn default() -> Self { Self::new() }
}

pub enum Outcome { Done, Retry(u8) }

pub trait Handler { fn handle(&self); fn name(&self) -> &str { "h" } }

pub(crate) const LIMIT: u32 = 3;

mod inner { pub fn helper() {} }
"#;

#[test]
fn extracts_items_members_and_qualified_ids() {
    let n = names(ROUTER, &["app", "router"]);
    for want in [
        "app::router::dispatch",
        "app::router::Router",
        "app::router::Router.routes",
        "app::router::Router.retries",
        "app::router::Router::new",
        "app::router::<Router as Default>::default",
        "app::router::Outcome",
        "app::router::Outcome::Done",
        "app::router::Outcome::Retry",
        "app::router::Handler",
        "app::router::Handler::handle",
        "app::router::Handler::name",
        "app::router::LIMIT",
        "app::router::inner",
        "app::router::inner::helper",
    ] {
        assert!(n.contains(&want.to_string()), "missing {want} in {n:#?}");
    }
    let all = syms(ROUTER, &["app", "router"]);
    let dispatch = &all.iter().find(|(_, s)| s.name() == "dispatch").unwrap().1;
    assert_eq!(dispatch.kind, SymbolKind::Function);
    assert_eq!(dispatch.visibility, SymbolVisibility::Public);
    assert!(
        dispatch.signature.contains("Result < Response , Error >"),
        "{}",
        dispatch.signature
    );
    assert_eq!(dispatch.surface_id().as_str(), "rust:app::router::dispatch");
    let limit = &all.iter().find(|(_, s)| s.name() == "LIMIT").unwrap().1;
    assert_eq!(limit.visibility, SymbolVisibility::Restricted);
    let handle = &all.iter().find(|(_, s)| s.name() == "handle").unwrap().1;
    let name = &all.iter().find(|(_, s)| s.name() == "name").unwrap().1;
    assert!(!handle.has_default && name.has_default);
}

#[test]
fn references_resolve_through_imports_and_prefixes() {
    let src = r#"
use crate::router::{dispatch, Outcome as O};
use serde::Serialize;

#[derive(tokio_derive::Wrap, Clone)]
pub struct Thing;

pub fn handle(req: Request) -> Response {
    let cfg = RouterConfig { timeout: 3 };
    match dispatch(req, &cfg) {
        O::Done => todo!(),
        O::Retry(n) => super::fallback(n),
    }
    x.route(1);
    println!("{}", self::helper(cfg.timeout));
}

impl crate::router::Handler for Thing { fn handle(&self) {} }
"#;
    let fs = analyze_source(src, &module(&["app", "handler"])).unwrap();
    let targets: Vec<&RefTarget> = fs.references.iter().map(|r| &r.target).collect();
    let has = |t: RefTarget| targets.contains(&&t);
    assert!(
        has(RefTarget::Path(module(&["app", "router", "dispatch"]))),
        "{targets:#?}"
    );
    assert!(has(RefTarget::Path(module(&[
        "app", "router", "Outcome", "Retry"
    ]))));
    assert!(has(RefTarget::Path(module(&["app", "fallback"]))));
    assert!(
        has(RefTarget::Path(module(&["app", "handler", "helper"]))),
        "macro bodies are analyzed"
    );
    assert!(has(RefTarget::Method("route".into())));
    assert!(has(RefTarget::Field("timeout".into())));
    assert!(has(RefTarget::StructLiteral {
        path: module(&["RouterConfig"]),
        has_rest: false
    }));
    assert!(has(RefTarget::ExhaustiveMatch {
        enum_path: module(&["app", "router", "Outcome"])
    }));
    assert!(has(RefTarget::TraitImpl {
        trait_path: module(&["app", "router", "Handler"])
    }));
    assert!(fs.external_roots.contains("serde"));
    assert!(
        fs.external_roots.contains("tokio_derive"),
        "derive paths are uses"
    );
    let from_handle = fs
        .references
        .iter()
        .filter(|r| r.from == "app::handler::handle")
        .count();
    assert!(
        from_handle > 5,
        "references are attributed to the enclosing item"
    );
}

#[test]
fn a_catch_all_arm_is_not_an_exhaustive_match() {
    let src = "fn f(o: Outcome) { match o { Outcome::Done => {}, _ => {} } }";
    let fs = analyze_source(src, &module(&["app"])).unwrap();
    assert!(!fs
        .references
        .iter()
        .any(|r| matches!(r.target, RefTarget::ExhaustiveMatch { .. })));
}

#[test]
fn diffs_classify_signature_body_visibility_and_docs() {
    let before = syms(ROUTER, &["app", "router"]);
    let after_src = ROUTER
        .replace(
            "-> Result<Response, Error> {",
            "-> Result<RetryOutcome, Error> {",
        )
        .replace(
            "Router { routes: vec![], retries: 0 }",
            "Router { routes: vec![], retries: 1 }",
        )
        .replace("pub(crate) const LIMIT", "const LIMIT")
        .replace(
            "/// Dispatch a request.",
            "/// Dispatch a request, now with docs.",
        )
        .replace(
            "pub enum Outcome { Done, Retry(u8) }",
            "pub enum Outcome { Done, Retry(u8), GiveUp }",
        )
        .replace(
            "pub struct Router { pub routes: Vec<String>, retries: u8 }",
            "pub struct Router { pub routes: Vec<String>, retries: u8, pub retry_limit: u32 }",
        );
    let after = syms(&after_src, &["app", "router"]);
    let changes = diff_symbols(&before, &after);
    let get = |q: &str| -> &SymbolChange {
        changes
            .iter()
            .find(|c| c.qualified() == q)
            .unwrap_or_else(|| panic!("no change for {q}: {changes:#?}"))
    };
    assert!(matches!(
        get("app::router::dispatch").change,
        ChangeKind::SignatureChanged { .. }
    ));
    assert_eq!(get("app::router::dispatch").impact, ApiImpact::Breaking);
    assert_eq!(
        get("app::router::Router::new").change,
        ChangeKind::BodyChanged
    );
    assert_eq!(
        get("app::router::Router::new").impact,
        ApiImpact::Behavioral
    );
    assert_eq!(
        get("app::router::LIMIT").impact,
        ApiImpact::Breaking,
        "visibility narrowed"
    );
    assert_eq!(
        get("app::router::Outcome::GiveUp").change,
        ChangeKind::Added
    );
    assert_eq!(
        get("app::router::Router.retry_limit").impact,
        ApiImpact::Additive
    );
    assert_eq!(
        changes.len(),
        5,
        "doc-only edits and untouched items are not changes: {changes:#?}"
    );
}

#[test]
fn formatting_only_changes_nothing() {
    let before = syms(ROUTER, &["app", "router"]);
    let reformatted = ROUTER.replace(
        "    Ok(Response)",
        "        Ok(\n            Response\n        )  // trailing comment",
    );
    assert!(diff_symbols(&before, &syms(&reformatted, &["app", "router"])).is_empty());
}

#[test]
fn hostile_input_fails_typed() {
    assert!(matches!(
        analyze_source("fn (", &module(&["x"])),
        Err(RustParseError::Syntax { .. })
    ));
    let huge = "a".repeat(doubledoor_surface::rust::MAX_RUST_SOURCE_BYTES + 1);
    assert!(matches!(
        analyze_source(&huge, &module(&["x"])),
        Err(RustParseError::Oversized { .. })
    ));
    // Deep nesting is rejected before the recursive parser runs: a stack
    // overflow would abort the whole process.
    let deep = format!(
        "fn f() {{ let x = {}1{}; }}",
        "(".repeat(20_000),
        ")".repeat(20_000)
    );
    assert!(matches!(
        analyze_source(&deep, &module(&["x"])),
        Err(RustParseError::TooDeep { .. })
    ));
    let unary = format!("fn f() {{ let x = {}true; }}", "!".repeat(20_000));
    assert!(matches!(
        analyze_source(&unary, &module(&["x"])),
        Err(RustParseError::TooDeep { .. })
    ));
    // Brackets inside strings, chars, and comments do not count.
    let quoted = format!(
        "fn f() {{ let s = \"{}\"; let r = r#\"{}\"#; let c = '('; /* {} */ }} // {}",
        "(".repeat(5_000),
        "[".repeat(5_000),
        "{".repeat(5_000),
        "(".repeat(5_000)
    );
    assert!(analyze_source(&quoted, &module(&["x"])).is_ok());
    // Ordinary deep-ish code is fine.
    let ok = format!(
        "fn f() {{ let x = {}1{}; }}",
        "(".repeat(100),
        ")".repeat(100)
    );
    assert!(analyze_source(&ok, &module(&["x"])).is_ok());
}

fn session_with(refs: Vec<RefTarget>) -> SessionSemantics {
    SessionSemantics {
        references: refs
            .into_iter()
            .map(|t| SessionReference {
                file: "src/handler.rs".into(),
                reference: doubledoor_surface::rust::Reference {
                    from: "app::handler::handle".into(),
                    target: t,
                    line: 1,
                },
            })
            .collect(),
        ..Default::default()
    }
}

fn change_of(src_before: &str, src_after: &str, q: &str) -> SymbolChange {
    diff_symbols(
        &syms(src_before, &["app", "config"]),
        &syms(src_after, &["app", "config"]),
    )
    .into_iter()
    .find(|c| c.qualified() == q)
    .unwrap()
}

#[test]
fn relations_distinguish_direct_owner_and_contextual_uses() {
    let field_added = change_of(
        "pub struct RouterConfig { pub timeout: u32 }",
        "pub struct RouterConfig { pub timeout: u32, pub retry_limit: u32 }",
        "app::config::RouterConfig.retry_limit",
    );
    let owner_only = session_with(vec![RefTarget::Path(module(&[
        "app",
        "config",
        "RouterConfig",
    ]))]);
    let rels: Vec<Relation> = owner_only
        .relations(&field_added)
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(rels, [Relation::Owner]);

    let literal = session_with(vec![RefTarget::StructLiteral {
        path: module(&["RouterConfig"]),
        has_rest: false,
    }]);
    assert_eq!(
        literal.relations(&field_added)[0].0,
        Relation::StructLiteral
    );
    let with_rest = session_with(vec![RefTarget::StructLiteral {
        path: module(&["RouterConfig"]),
        has_rest: true,
    }]);
    assert_eq!(with_rest.relations(&field_added)[0].0, Relation::Owner);

    let variant_added = change_of(
        "pub enum E { A }",
        "pub enum E { A, B }",
        "app::config::E::B",
    );
    let matcher = session_with(vec![RefTarget::ExhaustiveMatch {
        enum_path: module(&["E"]),
    }]);
    assert_eq!(
        matcher.relations(&variant_added)[0].0,
        Relation::ExhaustiveMatch
    );

    let fn_changed = change_of(
        "pub fn load() -> u8 { 1 }",
        "pub fn load() -> u16 { 1 }",
        "app::config::load",
    );
    let caller = session_with(vec![RefTarget::Path(module(&["app", "config", "load"]))]);
    assert_eq!(caller.relations(&fn_changed)[0].0, Relation::Direct);
    let unrelated = session_with(vec![RefTarget::Path(module(&["other", "load_all"]))]);
    assert!(unrelated.relations(&fn_changed).is_empty());
}

#[test]
fn pretty_tokens_is_presentation_only() {
    use doubledoor_surface::semantic::pretty_tokens;
    assert_eq!(
        pretty_tokens(
            "fn dispatch (req : Request , cfg : & RouterConfig) -> Result < Response , Error >"
        ),
        "fn dispatch(req: Request, cfg: &RouterConfig) -> Result<Response, Error>"
    );
    assert_eq!(
        pretty_tokens("fn f (& mut self) -> std :: io :: Result < () >"),
        "fn f(&mut self) -> std::io::Result<()>"
    );
}
